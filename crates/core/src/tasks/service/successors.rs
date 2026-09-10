use super::*;
use crate::error::ErrorCode;
impl Store {
    /// The transaction claims a bounded due page and publishes successors before
    /// committing. Concurrent workers therefore cannot issue duplicate work.
    pub async fn reconcile_completion_tasks(&self, now: i64) -> Result<()> {
        // Idle ticks need no publication lock; a racing insert is handled next tick.
        let due: Option<String> = sqlx::query_scalar(
            "SELECT occurrence_id FROM completion_pending WHERE next_check<=$1 LIMIT 1",
        )
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        if due.is_none() {
            return Ok(());
        }
        let mut tx = self.begin_serial().await?;
        let ids:Vec<String>=sqlx::query_scalar("SELECT occurrence_id FROM completion_pending WHERE next_check<=$1 ORDER BY next_check,occurrence_id LIMIT 32").bind(now).fetch_all(&mut *tx).await?;
        let mut touched = ids.iter().cloned().collect::<BTreeSet<_>>();
        let mut accounts = Self::audience(&mut tx, &touched).await?;
        let mut before = BTreeMap::new();
        for account in &accounts {
            before.insert(
                account.clone(),
                Self::subset(&mut tx, account, &touched).await?,
            );
        }
        for id in &ids {
            sqlx::query("UPDATE completion_pending SET next_check=$1 WHERE occurrence_id=$2")
                .bind(now + 60)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        Self::completion_successors(&mut tx, &mut touched, now).await?;
        // Existing resources stay intact; only actual new successors need a delta.
        if touched.len() > ids.len() {
            accounts.extend(Self::audience(&mut tx, &touched).await?);
            for account in &accounts {
                before.entry(account.clone()).or_default();
            }
            Self::publish(&mut tx, &accounts, &touched, &before, false).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Completion creates one durable successor. Corrections can reopen the
    /// predecessor but never erase or move work already issued to devices.
    pub(super) async fn completion_successors(
        tx: &mut Transaction<'_, Any>,
        touched: &mut BTreeSet<String>,
        now: i64,
    ) -> Result<()> {
        let mut occurrences = BTreeSet::new();
        for id in touched.iter() {
            occurrences.extend(sqlx::query_scalar::<_,String>("SELECT id FROM task_occurrences WHERE id=$1 UNION SELECT occurrence_id FROM occurrence_participants WHERE progress_id=$1").bind(id).fetch_all(&mut **tx).await?);
        }
        for occurrence in occurrences {
            if sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM completion_successors WHERE predecessor_id=$1",
            )
            .bind(&occurrence)
            .fetch_one(&mut **tx)
            .await?
                > 0
            {
                continue;
            }
            let data: OccurrenceData = serde_json::from_str(
                &sqlx::query_scalar::<_, String>("SELECT value FROM resources WHERE id=$1")
                    .bind(&occurrence)
                    .fetch_one(&mut **tx)
                    .await?,
            )?;
            let Some(cadence) = &data.definition.schedule.repeat else {
                continue;
            };
            if cadence.frequency != Frequency::AfterCompletion || data.covered_by.is_some() {
                continue;
            }
            sqlx::query("INSERT INTO completion_pending(occurrence_id,next_check) VALUES ($1,$2) ON CONFLICT(occurrence_id) DO NOTHING")
                .bind(&occurrence).bind(now).execute(&mut **tx).await?;
            let row = sqlx::query("SELECT t.execution_id,r.owner_id,r.archived FROM tasks t JOIN resources r ON r.id=t.task_id WHERE t.task_id=$1").bind(&data.task_id).fetch_one(&mut **tx).await?;
            if row.get::<i64, _>(2) != 0 {
                sqlx::query("DELETE FROM completion_pending WHERE occurrence_id=$1")
                    .bind(&occurrence)
                    .execute(&mut **tx)
                    .await?;
                continue;
            }
            let execution_id: String = row.get(0);
            let owner: String = row.get(1);
            let execution = Self::execution(tx, &owner, &data.task_id, false).await?;
            let audience = Self::audience(tx, &BTreeSet::from([execution_id.clone()])).await?;
            let mut readers = Vec::new();
            for account in audience {
                if !Self::subset(tx, &account, &BTreeSet::from([execution_id.clone()]))
                    .await?
                    .is_empty()
                {
                    readers.push(account);
                }
            }
            let rows = sqlx::query("SELECT progress_id,account_id,aggregate_consent FROM occurrence_participants WHERE occurrence_id=$1 ORDER BY account_id").bind(&occurrence).fetch_all(&mut **tx).await?;
            let mut streams = Vec::new();
            let mut latest: Option<i64> = None;
            for row in rows {
                let progress: String = row.get(0);
                let subject: String = row.get(1);
                let mut common = !readers.is_empty();
                for reader in &readers {
                    if (reader != &subject && row.get::<i64, _>(2) != 1)
                        || Self::subset(tx, reader, &BTreeSet::from([progress.clone()]))
                            .await?
                            .is_empty()
                    {
                        common = false;
                        break;
                    }
                }
                if !common {
                    streams.push(None);
                    continue;
                }
                let stream = Self::stream_data(tx, &progress).await?;
                if stream.excluded {
                    streams.push(None);
                    continue;
                }
                latest = latest
                    .into_iter()
                    .chain(stream.entries.iter().map(|e| e.happened_at))
                    .max();
                streams.push(Some(
                    stream.entries.into_iter().map(|e| e.evidence).collect(),
                ));
            }
            // Use only evidence visible to every schedule reader, including the
            // timestamp. Even Anyone's hidden extra contribution stays private.
            let outcome = aggregate(
                &data.definition.goal,
                data.definition.participation,
                &streams,
                data.closes_at.is_some_and(|v| now >= v),
            )?;
            if outcome != Outcome::Complete {
                // A missed completion-relative period waits for explicit evidence
                // or correction, not another minute of wall-clock passage. Never
                // turn a missed deadline into fabricated completion or a new date.
                if outcome == Outcome::Missed && data.definition.carry == Carry::CloseIncomplete {
                    sqlx::query("DELETE FROM completion_pending WHERE occurrence_id=$1")
                        .bind(&occurrence)
                        .execute(&mut **tx)
                        .await?;
                }
                continue;
            }
            let Some(mut completion) = latest else {
                continue;
            };
            if matches!(
                data.definition.goal,
                Goal::Numeric {
                    maximum: Some(_),
                    ..
                }
            ) {
                completion = completion.max(data.closes_at.unwrap_or(completion));
            }
            let definition: Definition = serde_json::from_value(execution.value.clone())?;
            let zone: chrono_tz::Tz = definition.schedule.timezone.parse()?;
            let completed_date = DateTime::<Utc>::from_timestamp(completion, 0)
                .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?
                .with_timezone(&zone)
                .date_naive();
            let date = completed_date
                .checked_add_signed(Duration::days(i64::from(cadence.interval)))
                .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?
                .to_string();
            schedule::date(&date)?;
            let instant = definition
                .schedule
                .time
                .as_ref()
                .map(|t| -> Result<i64> {
                    resolve(
                        zone,
                        schedule::date(&date)?.and_time(NaiveTime::parse_from_str(t, "%H:%M:%S")?),
                    )
                })
                .transpose()?;
            let slot = Slot {
                key: format!("after:{occurrence}"),
                date: Some(date),
                intended_time: definition.schedule.time.clone(),
                timezone: definition.schedule.timezone.clone(),
                instant,
            };
            let successor = occurrence_id(&data.task_id, &slot.key)?;
            Self::make_occurrence(
                tx,
                &data.task_id,
                &execution,
                &definition,
                slot,
                now,
                touched,
            )
            .await?;
            sqlx::query("DELETE FROM completion_pending WHERE occurrence_id=$1")
                .bind(&occurrence)
                .execute(&mut **tx)
                .await?;
            sqlx::query("INSERT INTO completion_successors VALUES ($1,$2)")
                .bind(&occurrence)
                .bind(successor)
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }
}
