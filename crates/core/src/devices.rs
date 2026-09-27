//! Device listing and atomic, recoverable device retirement.
//!
//! A device is not a table: it is the set of rows sharing an `(account, device_id)`. The
//! *approved device state* is the members a user could have confirmed retiring, and its opaque
//! `state_token` is what a retirement must still match when it commits. See the retirement
//! protocol on [`Store::retire_device`] and `docs/authentication.md`.
use super::*;
use crate::error::ErrorCode;
use crate::operations::{Operation, OperationKind, Outcome, request_digest};
use crate::sync::retry_unit;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

const TOKEN_PREFIX: &str = "v1.";

#[derive(Serialize)]
pub struct DeviceSummary {
    pub sessions: i64,
    pub native_handoffs: i64,
    pub notification_subscriptions: i64,
    pub pending_sign_ins: i64,
    pub sync_registered: bool,
}

#[derive(Serialize)]
pub struct Device {
    pub id: String,
    pub last_synced_at: Option<i64>,
    pub active_sessions: i64,
    pub state_token: String,
    pub summary: DeviceSummary,
}

/// Whether `token` is a well-formed version-1 state token: `v1.` and the unpadded base64url of a
/// SHA-256 digest in its one canonical spelling. Anything else is refused, not compared.
pub fn valid_state_token(token: &str) -> bool {
    token.strip_prefix(TOKEN_PREFIX).is_some_and(|rest| {
        rest.len() == 43
            && URL_SAFE_NO_PAD
                .decode(rest)
                .is_ok_and(|bytes| bytes.len() == 32)
    })
}

/// The members of one device's approved state, each read from a single statement.
#[derive(Default)]
struct State {
    /// Component 1: live `session_id`s.
    sessions: Vec<String>,
    /// Component 2: live `code_hash`es.
    handoffs: Vec<String>,
    /// Component 3: active `(id, version)` pairs.
    subscriptions: Vec<(String, i64)>,
    /// Component 4: live `grant_id`s (BE-Q19).
    grants: Vec<String>,
    /// Component 5: the registration's `cursor_key`, when a registration exists.
    registration: Option<String>,
}

impl State {
    fn is_empty(&self) -> bool {
        self.sessions.is_empty()
            && self.handoffs.is_empty()
            && self.subscriptions.is_empty()
            && self.grants.is_empty()
            && self.registration.is_none()
    }

    /// `v1.` + base64url(SHA-256(canonical)). The canonical form length-prefixes the device ID so
    /// no two devices share a spelling, then lists one `<component> <identity>` line per member,
    /// sorted by component and then by identity *bytes* in Rust, so a database collation can never
    /// change a token. An empty state has no member lines.
    fn token(&self, device: &str) -> String {
        let mut lines: Vec<(u8, String)> = Vec::new();
        lines.extend(self.sessions.iter().map(|id| (1, id.clone())));
        lines.extend(self.handoffs.iter().map(|hash| (2, hash.clone())));
        lines.extend(
            self.subscriptions
                .iter()
                .map(|(id, version)| (3, format!("{id}@{version}"))),
        );
        lines.extend(self.grants.iter().map(|id| (4, id.clone())));
        if let Some(key) = &self.registration {
            lines.push((5, hex(&Sha256::digest(key.as_bytes()))));
        }
        lines.sort();
        let mut canonical = format!("atlas-device-state-v1\n{}:{device}\n", device.len());
        for (component, identity) in lines {
            canonical.push_str(&format!("{component} {identity}\n"));
        }
        format!(
            "{TOKEN_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
        )
    }

    fn summary(&self) -> DeviceSummary {
        DeviceSummary {
            sessions: self.sessions.len() as i64,
            native_handoffs: self.handoffs.len() as i64,
            notification_subscriptions: self.subscriptions.len() as i64,
            pending_sign_ins: self.grants.len() as i64,
            sync_registered: self.registration.is_some(),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl Store {
    /// The account's devices with the approved state of each, all read from one snapshot so what
    /// a confirmation dialog shows is exactly what its token covers. A device is listed when any
    /// member of its approved state exists (a sync registration, a live session, a live native
    /// handoff, an active subscription or a pending activation grant), so every device a
    /// retirement could act on offers a token, including one whose only remaining state is a
    /// pending grant.
    pub async fn devices(&self, actor: &str, now: i64) -> Result<Vec<Device>> {
        // PostgreSQL: a repeatable-read, read-only snapshot. SQLite: a deferred BEGIN whose first
        // read fixes the WAL snapshot; readers never block the writers.
        let mut tx = self.pool.begin().await?;
        if !self.sqlite {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await?;
        }
        // The listing set is the union of the same five member predicates the components below
        // read, so it can never disagree with a token.
        let listed = sqlx::query("SELECT d.device_id,s.last_seen FROM (SELECT device_id FROM sync_devices WHERE account_id=$1 UNION SELECT device_id FROM sessions WHERE account_id=$1 AND expires_at>$2 UNION SELECT device_id FROM native_handoffs WHERE account_id=$1 AND expires_at>$2 UNION SELECT device_id FROM notification_subscriptions WHERE account_id=$1 AND active=1 UNION SELECT device_id FROM activation_grants WHERE account_id=$1 AND state='issued' AND expires_at>$2) d LEFT JOIN sync_devices s ON s.account_id=$1 AND s.device_id=d.device_id ORDER BY d.device_id")
            .bind(actor)
            .bind(now)
            .fetch_all(&mut *tx)
            .await?;
        let mut states = BTreeMap::<String, State>::new();
        for row in sqlx::query(
            "SELECT device_id,session_id FROM sessions WHERE account_id=$1 AND expires_at>$2",
        )
        .bind(actor)
        .bind(now)
        .fetch_all(&mut *tx)
        .await?
        {
            states
                .entry(row.get(0))
                .or_default()
                .sessions
                .push(row.get(1));
        }
        for row in sqlx::query(
            "SELECT device_id,code_hash FROM native_handoffs WHERE account_id=$1 AND expires_at>$2",
        )
        .bind(actor)
        .bind(now)
        .fetch_all(&mut *tx)
        .await?
        {
            states
                .entry(row.get(0))
                .or_default()
                .handoffs
                .push(row.get(1));
        }
        hook!(self, "devices.between_reads");
        for row in sqlx::query("SELECT device_id,id,version FROM notification_subscriptions WHERE account_id=$1 AND active=1")
            .bind(actor)
            .fetch_all(&mut *tx)
            .await?
        {
            states
                .entry(row.get(0))
                .or_default()
                .subscriptions
                .push((row.get(1), row.get(2)));
        }
        for row in sqlx::query(
            "SELECT device_id,grant_id FROM activation_grants WHERE account_id=$1 AND state='issued' AND expires_at>$2",
        )
        .bind(actor)
        .bind(now)
        .fetch_all(&mut *tx)
        .await?
        {
            states.entry(row.get(0)).or_default().grants.push(row.get(1));
        }
        for row in sqlx::query("SELECT device_id,cursor_key FROM sync_devices WHERE account_id=$1")
            .bind(actor)
            .fetch_all(&mut *tx)
            .await?
        {
            states.entry(row.get(0)).or_default().registration = Some(row.get(1));
        }
        tx.commit().await?;
        Ok(listed
            .iter()
            .map(|row| {
                let id: String = row.get(0);
                let state = states.remove(&id).unwrap_or_default();
                Device {
                    last_synced_at: row.get(1),
                    active_sessions: state.sessions.len() as i64,
                    state_token: state.token(&id),
                    summary: state.summary(),
                    id,
                }
            })
            .collect())
    }

    /// Retire `device` if, and only if, its approved state still equals the one `state_token`
    /// names, and record the outcome durably under `operation_id`.
    ///
    /// One serialised transaction, retried as a unit on a retryable conflict:
    /// 1. ledger first: a recorded outcome for this ID is replayed without evaluating anything;
    /// 2. a no-op update of the account row orders this against sync-registration creation;
    /// 3. the members are read again, `FOR UPDATE` on PostgreSQL, so every row compared is locked;
    /// 4. no member is `Superseded`; a different state is `RejectedStale` (nothing changes);
    /// 5. otherwise exactly the locked identities are deleted or deactivated, each statement's
    ///    rows-affected asserted, and any discrepancy rolls everything back without a ledger row;
    /// 6. the ledger row commits with the effects or with the rejection.
    ///
    /// `RejectedStale` is a committed answer, not an error: it needs its row to survive.
    pub async fn retire_device(
        &self,
        actor: &str,
        device: &str,
        operation_id: &str,
        state_token: &str,
        now: i64,
    ) -> Result<Operation> {
        ensure!(
            !device.is_empty() && device.len() <= 100,
            ErrorCode::InvalidValue
        );
        identifier(operation_id)?;
        ensure!(valid_state_token(state_token), ErrorCode::InvalidValue);
        let digest = request_digest(OperationKind::RetireDevice, device, Some(state_token));
        hook!(self, "retire.before_begin");
        retry_unit(device, || {
            self.retire_once(actor, device, operation_id, state_token, &digest, now)
        })
        .await
    }

    async fn retire_once(
        &self,
        actor: &str,
        device: &str,
        operation_id: &str,
        state_token: &str,
        digest: &str,
        now: i64,
    ) -> Result<Operation> {
        hook!(self, "retire.attempt");
        let mut tx = self.begin_retirement().await?;
        if let Some(recorded) = Self::ledger_read(&mut tx, actor, operation_id).await? {
            return Self::replay(actor, operation_id, digest, recorded);
        }
        hook!(self, "retire.after_ledger_read", &mut tx);
        let account = sqlx::query("UPDATE accounts SET access_epoch=access_epoch WHERE id=$1")
            .bind(actor)
            .execute(&mut *tx)
            .await?;
        ensure!(account.rows_affected() == 1, ErrorCode::Unauthenticated);
        let state = self.locked_state(&mut tx, actor, device, now).await?;
        hook!(self, "retire.after_locking_reads", &mut tx);
        let outcome = if state.is_empty() {
            Outcome::Superseded
        } else if state.token(device) != state_token {
            Outcome::RejectedStale
        } else {
            Outcome::ConfirmedApplied
        };
        hook!(self, "retire.before_effects", &mut tx);
        if outcome == Outcome::ConfirmedApplied {
            Self::retire_members(&mut tx, actor, device, &state, now).await?;
        }
        hook!(self, "retire.before_commit", &mut tx);
        Self::ledger_write(
            &mut tx,
            actor,
            operation_id,
            OperationKind::RetireDevice,
            digest,
            outcome,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(Operation {
            operation_id: operation_id.to_owned(),
            account_id: actor.to_owned(),
            outcome,
        })
    }

    /// Read every member in component order and, by identity within a component, `FOR UPDATE` on
    /// PostgreSQL, holding the locks to commit. On SQLite the write lock taken by `begin_serial`
    /// already excludes every other writer, and the clause is not supported.
    async fn locked_state(
        &self,
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        device: &str,
        now: i64,
    ) -> Result<State> {
        let lock = if self.locks_rows() { " FOR UPDATE" } else { "" };
        // Every statement is repository-owned; only static text is interpolated.
        let sessions = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT session_id FROM sessions WHERE account_id=$1 AND device_id=$2 AND expires_at>$3 ORDER BY session_id{lock}")))
            .bind(actor).bind(device).bind(now).fetch_all(&mut **tx).await?;
        let handoffs = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT code_hash FROM native_handoffs WHERE account_id=$1 AND device_id=$2 AND expires_at>$3 ORDER BY code_hash{lock}")))
            .bind(actor).bind(device).bind(now).fetch_all(&mut **tx).await?;
        let subscriptions = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT id,version FROM notification_subscriptions WHERE account_id=$1 AND device_id=$2 AND active=1 ORDER BY id{lock}")))
            .bind(actor).bind(device).fetch_all(&mut **tx).await?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        let grants = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT grant_id FROM activation_grants WHERE account_id=$1 AND device_id=$2 AND state='issued' AND expires_at>$3 ORDER BY grant_id{lock}")))
            .bind(actor).bind(device).bind(now).fetch_all(&mut **tx).await?;
        let registration = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT cursor_key FROM sync_devices WHERE account_id=$1 AND device_id=$2{lock}"
        )))
        .bind(actor)
        .bind(device)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(State {
            sessions,
            handoffs,
            subscriptions,
            grants,
            registration,
        })
    }

    /// Delete, deactivate and cancel exactly the locked identities. Each statement's
    /// rows-affected must equal the number of members it locked; a mismatch means a writer
    /// escaped the protocol, so this fails and the caller's transaction rolls back unrecorded.
    async fn retire_members(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        device: &str,
        state: &State,
        now: i64,
    ) -> Result<()> {
        Self::delete_identities(tx, "sessions", "session_id", actor, &state.sessions).await?;
        Self::delete_identities(tx, "native_handoffs", "code_hash", actor, &state.handoffs).await?;
        for (id, version) in &state.subscriptions {
            let changed = sqlx::query("UPDATE notification_subscriptions SET active=0,secret='',version=version+1 WHERE id=$1 AND account_id=$2 AND active=1 AND version=$3")
                .bind(id).bind(actor).bind(version).execute(&mut **tx).await?;
            ensure!(changed.rows_affected() == 1, ErrorCode::InternalError);
        }
        // Already-inactive subscriptions are not members (`locked_state` never reads them), so
        // this is uncounted cleanup, exactly like the `sync_cursors` deletion below: it closes
        // CH2 — a subscription `SetSubscription` deactivated for an unrelated reason could still
        // be holding a secret indefinitely — so every subscription row tied to this device ends
        // up secret-erased, matching docs/notifications.md's existing claim.
        sqlx::query("UPDATE notification_subscriptions SET secret='',version=version+1 WHERE account_id=$1 AND device_id=$2 AND active=0 AND secret<>''")
            .bind(actor)
            .bind(device)
            .execute(&mut **tx)
            .await?;
        Self::cancel_grants(tx, actor, device, &state.grants, now).await?;
        if let Some(key) = &state.registration {
            let removed = sqlx::query(
                "DELETE FROM sync_devices WHERE account_id=$1 AND device_id=$2 AND cursor_key=$3",
            )
            .bind(actor)
            .bind(device)
            .bind(key)
            .execute(&mut **tx)
            .await?;
            ensure!(removed.rows_affected() == 1, ErrorCode::InternalError);
        }
        // Cursor churn is deleted with the device but is not a member, so it is not counted.
        sqlx::query("DELETE FROM sync_cursors WHERE account_id=$1 AND device_id=$2")
            .bind(actor)
            .bind(device)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    async fn delete_identities(
        tx: &mut Transaction<'_, Any>,
        table: &'static str,
        column: &'static str,
        actor: &str,
        identities: &[String],
    ) -> Result<()> {
        let mut removed = 0;
        for chunk in identities.chunks(500) {
            let placeholders = (0..chunk.len())
                .map(|i| format!("${}", i + 2))
                .collect::<Vec<_>>()
                .join(",");
            // Table and column are static names; every identity is bound.
            let statement =
                format!("DELETE FROM {table} WHERE account_id=$1 AND {column} IN ({placeholders})");
            let mut delete = sqlx::query(sqlx::AssertSqlSafe(statement)).bind(actor);
            for identity in chunk {
                delete = delete.bind(identity);
            }
            removed += delete.execute(&mut **tx).await?.rows_affected() as usize;
        }
        ensure!(removed == identities.len(), ErrorCode::InternalError);
        Ok(())
    }

    /// Cancel exactly the locked `issued` grants. An `UPDATE`, not a `DELETE`, like
    /// `delete_identities`: the row stays as evidence for a later `activate`/`activate/cancel`.
    async fn cancel_grants(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        device: &str,
        grants: &[String],
        now: i64,
    ) -> Result<()> {
        let mut cancelled = 0;
        for chunk in grants.chunks(500) {
            let placeholders = (0..chunk.len())
                .map(|i| format!("${}", i + 4))
                .collect::<Vec<_>>()
                .join(",");
            let statement = format!(
                "UPDATE activation_grants SET state='cancelled',cancelled_at=$3 WHERE account_id=$1 AND device_id=$2 AND state='issued' AND grant_id IN ({placeholders})"
            );
            let mut update = sqlx::query(sqlx::AssertSqlSafe(statement))
                .bind(actor)
                .bind(device)
                .bind(now);
            for grant in chunk {
                update = update.bind(grant);
            }
            cancelled += update.execute(&mut **tx).await?.rows_affected() as usize;
        }
        ensure!(cancelled == grants.len(), ErrorCode::InternalError);
        Ok(())
    }

    /// Revoke one session by ID and record the outcome under `operation_id`, ordered against a
    /// retirement by the same serialisation point. Never `RejectedStale`: logout has no
    /// precondition. An expired-but-unswept row is not credited to this call.
    pub async fn revoke_session(
        &self,
        actor: &str,
        session_id: &str,
        operation_id: &str,
        now: i64,
    ) -> Result<Operation> {
        identifier(session_id)?;
        identifier(operation_id)?;
        let digest = request_digest(OperationKind::RevokeSession, session_id, None);
        retry_unit(session_id, || {
            self.revoke_once(actor, session_id, operation_id, &digest, now)
        })
        .await
    }

    async fn revoke_once(
        &self,
        actor: &str,
        session_id: &str,
        operation_id: &str,
        digest: &str,
        now: i64,
    ) -> Result<Operation> {
        hook!(self, "revoke.attempt");
        let mut tx = self.begin_serial().await?;
        // Replay precedes any target lookup, so it survives the target's absence.
        if let Some(recorded) = Self::ledger_read(&mut tx, actor, operation_id).await? {
            return Self::replay(actor, operation_id, digest, recorded);
        }
        let removed = sqlx::query(
            "DELETE FROM sessions WHERE session_id=$1 AND account_id=$2 AND expires_at>$3",
        )
        .bind(session_id)
        .bind(actor)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let outcome = if removed.rows_affected() == 1 {
            Outcome::ConfirmedApplied
        } else {
            Outcome::Superseded
        };
        Self::ledger_write(
            &mut tx,
            actor,
            operation_id,
            OperationKind::RevokeSession,
            digest,
            outcome,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(Operation {
            operation_id: operation_id.to_owned(),
            account_id: actor.to_owned(),
            outcome,
        })
    }
}
