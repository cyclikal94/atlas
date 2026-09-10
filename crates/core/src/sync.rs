//! Stable row snapshots and authority-filtered, device-aware delta recovery.
use super::*;
use crate::error::ErrorCode;

impl Store {
    /// Retry SQLite snapshot-to-writer conflicts without changing the read boundary
    /// inside a transaction. PostgreSQL uses a repeatable-read snapshot.
    pub async fn sync(
        &self,
        actor: &str,
        device: &str,
        cursor: Option<&str>,
        limit: usize,
        now: i64,
    ) -> Result<Page> {
        ensure!(
            (1..=200).contains(&limit)
                && !device.is_empty()
                && device.len() <= 100
                && cursor.is_none_or(|c| c.len() <= 512),
            ErrorCode::InvalidValue
        );
        for attempt in 0..16 {
            match self.sync_once(actor, device, cursor, limit, now).await {
                Err(error) if attempt < 15 && retryable(&error) => {
                    let jitter = (device.bytes().map(u64::from).sum::<u64>() + attempt * 7) % 17;
                    tokio::time::sleep(std::time::Duration::from_millis(
                        (5_u64 << attempt).min(50) + jitter,
                    ))
                    .await;
                }
                result => return result,
            }
        }
        unreachable!()
    }

    async fn sync_once(
        &self,
        actor: &str,
        device: &str,
        cursor: Option<&str>,
        limit: usize,
        now: i64,
    ) -> Result<Page> {
        let mut tx = if self.sqlite && cursor.is_none() {
            // A new snapshot necessarily writes. Reserve SQLite's writer slot before
            // reading to avoid simultaneous snapshot-to-writer upgrade starvation.
            self.pool.begin_with("BEGIN IMMEDIATE").await?
        } else {
            self.pool.begin().await?
        };
        if !self.sqlite {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
        }
        let epoch = Self::epoch(&mut tx, actor).await?;
        let clock: i64 = sqlx::query_scalar("SELECT revision FROM sync_clock WHERE id=1")
            .fetch_one(&mut *tx)
            .await?;
        let (boundary, snapshot, position, expires) = if let Some(token) =
            cursor.filter(|token| token.starts_with("d1."))
        {
            let key: String = sqlx::query_scalar(
                "SELECT cursor_key FROM sync_devices WHERE account_id=$1 AND device_id=$2",
            )
            .bind(actor)
            .bind(device)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| anyhow!(ErrorCode::ResyncRequired))?;
            let (boundary, expiry) = sync_cursor::verify(&key, actor, device, token)?;
            ensure!(expiry > now, ErrorCode::ResyncRequired);
            (boundary, None, 0, expiry)
        } else if let Some(token) = cursor {
            let r = sqlx::query("SELECT account_id,device_id,epoch,boundary,snapshot_id,position,expires_at FROM sync_cursors WHERE token=$1")
                .bind(token).fetch_optional(&mut *tx).await?.ok_or_else(|| anyhow!(ErrorCode::ResyncRequired))?;
            ensure!(
                r.get::<String, _>(0) == actor
                    && r.get::<String, _>(1) == device
                    && r.get::<i64, _>(6) > now,
                ErrorCode::ResyncRequired
            );
            let snapshot: String = r.get(4);
            ensure!(r.get::<i64, _>(2) == epoch, ErrorCode::AccessChanged);
            (
                r.get::<i64, _>(3),
                Some(snapshot),
                r.get::<i64, _>(5),
                r.get::<i64, _>(6),
            )
        } else {
            let id = Uuid::new_v4().to_string();
            let expires = now
                .checked_add(3600)
                .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
            sqlx::query("INSERT INTO sync_snapshots(id,expires_at) VALUES ($1,$2)")
                .bind(&id)
                .bind(expires)
                .execute(&mut *tx)
                .await?;
            // Indexed union finds owned/directly granted resources. SQL materialises
            // immutable rows; neither creation nor pagination builds a giant JSON blob.
            sqlx::query(sqlx::AssertSqlSafe(format!("INSERT INTO snapshot_items(snapshot_id,position,id,kind,parent_id,label,value,version,policy_version,can_edit,archived) SELECT $2,ROW_NUMBER() OVER(ORDER BY CASE r.kind WHEN 'calendar_source' THEN -2 WHEN 'event' THEN -1 WHEN 'person' THEN 0 WHEN 'task' THEN 1 WHEN 'list' THEN 2 WHEN 'field' THEN 3 WHEN 'execution' THEN 4 WHEN 'occurrence' THEN 5 ELSE 6 END,r.id),{} FROM resources r JOIN (SELECT id FROM resources WHERE owner_id=$1 UNION SELECT resource_id AS id FROM resource_grants WHERE account_id=$1 UNION SELECT hg.resource_id AS id FROM resource_household_grants hg JOIN household_memberships hm ON hm.household_id=hg.household_id WHERE hm.account_id=$1) visible ON visible.id=r.id LEFT JOIN resource_grants g ON g.resource_id=r.id AND g.account_id=$1 WHERE {}", policy::COLUMNS, policy::VISIBLE)))
                .bind(actor).bind(&id).execute(&mut *tx).await?;
            let lists: Vec<String> = sqlx::query_scalar(
                "SELECT id FROM snapshot_items WHERE snapshot_id=$1 AND kind='list'",
            )
            .bind(&id)
            .fetch_all(&mut *tx)
            .await?;
            for list in lists {
                let value = Self::list_value(&mut tx, actor, &list).await?;
                sqlx::query("UPDATE snapshot_items SET value=$1 WHERE snapshot_id=$2 AND id=$3")
                    .bind(value)
                    .bind(&id)
                    .bind(list)
                    .execute(&mut *tx)
                    .await?;
            }
            (clock, Some(id), 0, expires)
        };
        let floor: i64 = sqlx::query_scalar("SELECT sync_floor FROM accounts WHERE id=$1")
            .bind(actor)
            .fetch_one(&mut *tx)
            .await?;
        ensure!(
            boundary >= floor && boundary <= clock,
            ErrorCode::ResyncRequired
        );
        let (phase, batches, more, next_boundary, next_snapshot, next_position) = if let Some(id) =
            snapshot
        {
            let rows = sqlx::query("SELECT id,kind,parent_id,label,value,version,policy_version,can_edit,archived FROM snapshot_items WHERE snapshot_id=$1 AND position>$2 ORDER BY position LIMIT $3")
                .bind(&id).bind(position).bind((limit + 1) as i64).fetch_all(&mut *tx).await?;
            let more = rows.len() > limit;
            let changes: Vec<_> = rows
                .iter()
                .take(limit)
                .map(|r| {
                    Ok(Change::Upsert {
                        resource: projection_row(r)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let next_position = position + changes.len() as i64;
            (
                "snapshot",
                vec![Batch {
                    revision: boundary,
                    changes,
                }],
                more,
                boundary,
                if more { Some(id) } else { None },
                if more { next_position } else { 0 },
            )
        } else {
            let rows = sqlx::query("SELECT revision,payload FROM sync_batches WHERE account_id=$1 AND revision>$2 AND revision<=$3 ORDER BY revision LIMIT $4")
                .bind(actor).bind(boundary).bind(clock).bind((limit + 1) as i64).fetch_all(&mut *tx).await?;
            let more = rows.len() > limit;
            let mut batches = Vec::new();
            for row in rows.iter().take(limit) {
                let notices: Vec<Notice> = serde_json::from_str(&row.get::<String, _>(1))?;
                let ids = notices.iter().map(|n| n.id.clone()).collect();
                let mut current = Self::subset(&mut tx, actor, &ids).await?;
                let mut parents = BTreeSet::new();
                for id in current.keys() {
                    parents.extend(
                        sqlx::query_scalar::<_, String>(
                            "SELECT ancestor_id FROM resource_ancestors WHERE resource_id=$1",
                        )
                        .bind(id)
                        .fetch_all(&mut *tx)
                        .await?,
                    );
                }
                current.extend(Self::subset(&mut tx, actor, &parents).await?);
                let mut changes = Vec::new();
                for kind in RESOURCE_ORDER {
                    changes.extend(
                        current
                            .values()
                            .filter(|p| p.kind == kind)
                            .cloned()
                            .map(|resource| Change::Upsert { resource }),
                    );
                }
                let mut removed = Vec::new();
                for notice in notices {
                    if current.contains_key(&notice.id) {
                        continue;
                    }
                    let delivered: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sync_deliveries WHERE account_id=$1 AND device_id=$2 AND resource_id=$3")
                        .bind(actor).bind(device).bind(&notice.id).fetch_one(&mut *tx).await?;
                    if delivered == 0 {
                        continue;
                    }
                    let kind: Option<String> =
                        sqlx::query_scalar("SELECT kind FROM resources WHERE id=$1")
                            .bind(&notice.id)
                            .fetch_optional(&mut *tx)
                            .await?;
                    let kind = kind.unwrap_or(notice.resource_kind);
                    let rank = RESOURCE_ORDER.iter().position(|k| *k == kind).unwrap_or(0);
                    removed.push((std::cmp::Reverse(rank), notice.id));
                }
                removed.sort();
                changes.extend(removed.into_iter().map(|(_, id)| Change::Remove { id }));
                batches.push(Batch {
                    revision: row.get(0),
                    changes,
                });
            }
            let next = batches.last().map_or(boundary, |b| b.revision);
            ("delta", batches, more, next, None, 0)
        };
        // Keep old delivery entries after removal: a response can be lost and retried.
        // Device expiry, not sending a response, is the safe point to reclaim them.
        let last_seen: Option<i64> = sqlx::query_scalar(
            "SELECT last_seen FROM sync_devices WHERE account_id=$1 AND device_id=$2",
        )
        .bind(actor)
        .bind(device)
        .fetch_optional(&mut *tx)
        .await?;
        if last_seen.is_none() {
            // Serialise the per-account device quota, not every sync request.
            // A competing REPEATABLE READ transaction retries against the new state.
            sqlx::query("UPDATE accounts SET access_epoch=access_epoch WHERE id=$1")
                .bind(actor)
                .execute(&mut *tx)
                .await?;
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM sync_devices WHERE account_id=$1")
                    .bind(actor)
                    .fetch_one(&mut *tx)
                    .await?;
            ensure!(count < 32, ErrorCode::DeviceCapacity);
            sqlx::query("INSERT INTO sync_devices(account_id,device_id,last_seen,cursor_key) VALUES ($1,$2,$3,$4) ON CONFLICT(account_id,device_id) DO UPDATE SET last_seen=excluded.last_seen")
                .bind(actor).bind(device).bind(now).bind(sync_cursor::new_key()).execute(&mut *tx).await?;
        } else if last_seen.is_some_and(|last| {
            now.saturating_sub(last) >= (self.retention_seconds / 10).min(86400)
        }) {
            sqlx::query(
                "UPDATE sync_devices SET last_seen=$1 WHERE account_id=$2 AND device_id=$3",
            )
            .bind(now)
            .bind(actor)
            .bind(device)
            .execute(&mut *tx)
            .await?;
        }
        for batch in &batches {
            for change in &batch.changes {
                if let Change::Upsert { resource } = change {
                    sqlx::query(
                        "INSERT INTO sync_deliveries VALUES ($1,$2,$3) ON CONFLICT DO NOTHING",
                    )
                    .bind(actor)
                    .bind(device)
                    .bind(&resource.id)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        let reuse = phase == "delta"
            && batches.is_empty()
            && expires.saturating_sub(now) > (self.retention_seconds / 9).min(10 * 86400);
        let next_cursor = if reuse {
            cursor.expect("delta has cursor").to_owned()
        } else if let Some(snapshot) = next_snapshot {
            let token = Uuid::new_v4().to_string();
            sqlx::query("INSERT INTO sync_cursors VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
                .bind(&token)
                .bind(actor)
                .bind(device)
                .bind(epoch)
                .bind(next_boundary)
                .bind(snapshot)
                .bind(next_position)
                .bind(expires)
                .execute(&mut *tx)
                .await?;
            token
        } else {
            let expiry = now
                .checked_add(self.retention_seconds)
                .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
            let key: String = sqlx::query_scalar(
                "SELECT cursor_key FROM sync_devices WHERE account_id=$1 AND device_id=$2",
            )
            .bind(actor)
            .bind(device)
            .fetch_one(&mut *tx)
            .await?;
            sync_cursor::sign(&key, actor, device, next_boundary, expiry)?
        };
        tx.commit().await?;
        Ok(Page {
            phase: phase.into(),
            batches,
            next_cursor,
            has_more: more,
        })
    }

    /// Reclaim transport state in bounded transactions, without the publication gate.
    /// A deleted history batch and its account recovery floor commit atomically.
    pub async fn collect_expired(&self, now: i64) -> Result<()> {
        for statement in [
            "DELETE FROM native_handoffs WHERE code_hash IN (SELECT code_hash FROM native_handoffs WHERE expires_at<=$1 LIMIT 500)",
            "DELETE FROM oidc_flows WHERE state_hash IN (SELECT state_hash FROM oidc_flows WHERE expires_at<=$1 LIMIT 500)",
            "DELETE FROM sessions WHERE token_hash IN (SELECT token_hash FROM sessions WHERE expires_at<=$1 LIMIT 500)",
            "DELETE FROM sync_cursors WHERE token IN (SELECT token FROM sync_cursors WHERE expires_at<=$1 LIMIT 500)",
            "DELETE FROM people_requests WHERE id IN (SELECT id FROM people_requests WHERE expires_at<=$1 LIMIT 500)",
        ] {
            self.delete_expired_chunks(statement, now).await?;
        }
        self.delete_expired_chunks("DELETE FROM account_invitations WHERE id IN (SELECT id FROM account_invitations WHERE expires_at<=$1 LIMIT 500)",now.saturating_sub(7*86400)).await?;
        // Delete immutable snapshot children in chunks before their parent. A
        // single expired snapshot can contain the full 10,000-resource projection.
        self.delete_expired_chunks("DELETE FROM snapshot_items WHERE (snapshot_id,position) IN (SELECT i.snapshot_id,i.position FROM snapshot_items i JOIN sync_snapshots s ON s.id=i.snapshot_id WHERE s.expires_at<=$1 AND NOT EXISTS(SELECT 1 FROM sync_cursors c WHERE c.snapshot_id=s.id) LIMIT 500)",now).await?;
        self.delete_expired_chunks("DELETE FROM sync_snapshots WHERE id IN (SELECT s.id FROM sync_snapshots s WHERE s.expires_at<=$1 AND NOT EXISTS(SELECT 1 FROM sync_cursors c WHERE c.snapshot_id=s.id) AND NOT EXISTS(SELECT 1 FROM snapshot_items i WHERE i.snapshot_id=s.id) LIMIT 500)",now).await?;
        let cutoff = now.saturating_sub(self.retention_seconds);
        loop {
            let mut tx = self.begin_cleanup().await?;
            let rows=sqlx::query("SELECT account_id,revision FROM sync_batches WHERE created_at<=$1 ORDER BY created_at,revision,account_id LIMIT 256").bind(cutoff).fetch_all(&mut *tx).await?;
            if rows.is_empty() {
                tx.commit().await?;
                break;
            }
            let mut floors = BTreeMap::<String, i64>::new();
            for row in &rows {
                let revision = row.get::<i64, _>(1);
                floors
                    .entry(row.get(0))
                    .and_modify(|r| *r = (*r).max(revision))
                    .or_insert(revision);
            }
            for (account, floor) in floors {
                sqlx::query("UPDATE accounts SET sync_floor=CASE WHEN sync_floor<$1 THEN $1 ELSE sync_floor END WHERE id=$2").bind(floor).bind(account).execute(&mut *tx).await?;
            }
            // Numbered placeholders work with both backends; Any's query builder
            // otherwise emits question marks, which PostgreSQL does not accept.
            let placeholders = (0..rows.len())
                .map(|i| format!("(${},${})", i * 2 + 1, i * 2 + 2))
                .collect::<Vec<_>>()
                .join(",");
            let statement =
                format!("DELETE FROM sync_batches WHERE (account_id,revision) IN ({placeholders})");
            let mut delete = sqlx::query(sqlx::AssertSqlSafe(statement));
            for row in &rows {
                delete = delete
                    .bind(row.get::<String, _>(0))
                    .bind(row.get::<i64, _>(1));
            }
            delete.execute(&mut *tx).await?;
            tx.commit().await?;
            tokio::task::yield_now().await;
        }
        loop {
            let mut tx = self.begin_cleanup().await?;
            let statement = if self.sqlite {
                "SELECT account_id,device_id FROM sync_devices WHERE last_seen<=$1 ORDER BY last_seen LIMIT 1"
            } else {
                "SELECT account_id,device_id FROM sync_devices WHERE last_seen<=$1 ORDER BY last_seen LIMIT 1 FOR UPDATE"
            };
            let Some(device) = sqlx::query(sqlx::AssertSqlSafe(statement))
                .bind(cutoff)
                .fetch_optional(&mut *tx)
                .await?
            else {
                tx.commit().await?;
                break;
            };
            let account: String = device.get(0);
            let id: String = device.get(1);
            // One device's cursor/ledger retirement is atomic, so it cannot recover
            // from an old cursor against a partly deleted delivery ledger. The
            // ledger is bounded by the installation's resource ceiling.
            sqlx::query("DELETE FROM sync_cursors WHERE account_id=$1 AND device_id=$2")
                .bind(&account)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "DELETE FROM sync_devices WHERE account_id=$1 AND device_id=$2 AND last_seen<=$3",
            )
            .bind(account)
            .bind(id)
            .bind(cutoff)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            tokio::task::yield_now().await;
        }
        Ok(())
    }
    async fn begin_cleanup(&self) -> Result<Transaction<'static, Any>> {
        Ok(if self.sqlite {
            self.pool.begin_with("BEGIN IMMEDIATE").await?
        } else {
            self.pool.begin().await?
        })
    }
    async fn delete_expired_chunks(&self, statement: &'static str, cutoff: i64) -> Result<()> {
        loop {
            // Every statement is repository-owned; the timestamp is always bound.
            let result = sqlx::query(sqlx::AssertSqlSafe(statement))
                .bind(cutoff)
                .execute(&self.pool)
                .await?;
            if result.rows_affected() < 500 {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok(())
    }
}

fn retryable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<sqlx::Error>()
        .and_then(|e| e.as_database_error())
        .and_then(|e| e.code())
        .is_some_and(|c| matches!(c.as_ref(), "5" | "517" | "40001" | "40P01"))
}
