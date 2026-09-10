use super::*;
use crate::error::ErrorCode;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimerSession {
    pub id: String,
    pub occurrence_id: String,
    pub started_at: i64,
    pub stopped_at: Option<i64>,
    pub version: i64,
}
impl Store {
    pub async fn timer_sessions(
        &self,
        actor: &str,
        occurrence: &str,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<TimerSession>> {
        ensure!((1..=200).contains(&limit), ErrorCode::InvalidValue);
        if let Some(after) = after {
            identifier(after)?;
        }
        let mut tx = self.task_read().await?;
        let stream = Self::participant_stream(&mut tx, actor, occurrence, actor, false).await?;
        let rows = sqlx::query("SELECT id,started_at,stopped_at,version FROM timer_sessions WHERE progress_id=$1 AND cancelled=0 AND ($2 IS NULL OR id>$2) ORDER BY id LIMIT $3").bind(&stream.id).bind(after).bind(i64::from(limit)).fetch_all(&mut *tx).await?;
        let items = rows
            .into_iter()
            .map(|r| TimerSession {
                id: r.get(0),
                occurrence_id: occurrence.into(),
                started_at: r.get(1),
                stopped_at: r.get(2),
                version: r.get(3),
            })
            .collect();
        tx.commit().await?;
        Ok(items)
    }
    pub(super) async fn timer_command(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        command: &TaskCommand,
        defaults: &crate::policy::Defaults,
        now: i64,
        touched: &mut BTreeSet<String>,
    ) -> Result<()> {
        let (occurrence, session) = match command {
            TaskCommand::CancelTimer {
                occurrence_id,
                session_id,
                ..
            }
            | TaskCommand::StartTimer {
                occurrence_id,
                session_id,
                ..
            }
            | TaskCommand::StopTimer {
                occurrence_id,
                session_id,
                ..
            } => (occurrence_id, session_id),
            _ => return Err(anyhow!(ErrorCode::InvalidValue)),
        };
        identifier(session)?;
        let p = Self::task_resource(tx, actor, occurrence, "occurrence", false).await?;
        let data: OccurrenceData = serde_json::from_value(p.value.clone())?;
        ensure!(
            matches!(&data.definition.goal, Goal::Numeric {unit,..} if unit == "seconds")
                && data.covered_by.is_none(),
            ErrorCode::InvalidValue
        );
        if matches!(command, TaskCommand::StartTimer { .. }) {
            Self::ensure_own_participation(tx, actor, defaults, &p, &data, touched).await?;
        }
        let stream = Self::participant_stream(tx, actor, occurrence, actor, true).await?;
        match command {
            TaskCommand::CancelTimer {
                expected_version, ..
            } => {
                let result = sqlx::query("UPDATE timer_sessions SET cancelled=1,version=version+1 WHERE id=$1 AND progress_id=$2 AND account_id=$3 AND version=$4 AND stopped_at IS NULL AND cancelled=0").bind(session).bind(&stream.id).bind(actor).bind(expected_version).execute(&mut **tx).await?;
                ensure!(result.rows_affected() == 1, ErrorCode::Conflict);
                Self::value(tx, &stream.id, &stream.label, &stream.value.to_string()).await?;
            }
            TaskCommand::StartTimer { started_at, .. } => {
                ensure!(
                    *started_at <= now + 300
                        && DateTime::<Utc>::from_timestamp(*started_at, 0).is_some()
                        && data.opens_at.is_none_or(|v| *started_at >= v),
                    ErrorCode::InvalidValue
                );
                // One active session per account, across all tasks and devices.
                let overlapping: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM timer_sessions WHERE account_id=$1 AND cancelled=0 AND (stopped_at IS NULL OR (started_at <= $2 AND stopped_at > $2))").bind(actor).bind(started_at).fetch_one(&mut **tx).await?;
                ensure!(overlapping == 0, ErrorCode::Conflict);
                sqlx::query("INSERT INTO timer_sessions(id,progress_id,account_id,started_at,version) VALUES ($1,$2,$3,$4,1)").bind(session).bind(&stream.id).bind(actor).bind(started_at).execute(&mut **tx).await?;
                // Ordinary sync invalidates the stream; session details are fetched
                // through the authorised paged endpoint, not a global activity feed.
                Self::value(tx, &stream.id, &stream.label, &stream.value.to_string()).await?;
            }
            TaskCommand::StopTimer {
                stopped_at,
                expected_version,
                ..
            } => {
                let row = sqlx::query("SELECT started_at,version,stopped_at FROM timer_sessions WHERE id=$1 AND progress_id=$2 AND account_id=$3 AND cancelled=0").bind(session).bind(&stream.id).bind(actor).fetch_optional(&mut **tx).await?.ok_or_else(||anyhow!(ErrorCode::NotFound))?;
                let started: i64 = row.get(0);
                ensure!(
                    row.get::<i64, _>(1) == *expected_version
                        && row.get::<Option<i64>, _>(2).is_none(),
                    ErrorCode::Conflict
                );
                ensure!(
                    *stopped_at > started
                        && *stopped_at <= now + 300
                        && stopped_at.checked_sub(started).is_some_and(|d| d <= 604800),
                    ErrorCode::InvalidValue
                );
                let overlapping: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM timer_sessions WHERE account_id=$1 AND cancelled=0 AND id<>$2 AND started_at<$3 AND (stopped_at IS NULL OR stopped_at>$4)").bind(actor).bind(session).bind(stopped_at).bind(started).fetch_one(&mut **tx).await?;
                ensure!(overlapping == 0, ErrorCode::Conflict);
                sqlx::query(
                    "UPDATE timer_sessions SET stopped_at=$1,version=version+1 WHERE id=$2",
                )
                .bind(stopped_at)
                .bind(session)
                .execute(&mut **tx)
                .await?;
                Self::record_entry(
                    tx,
                    actor,
                    defaults,
                    &TaskCommand::Record {
                        occurrence_id: occurrence.clone(),
                        subject_account_id: actor.into(),
                        entry_id: Uuid::new_v5(&Uuid::parse_str(session)?, b"timer-duration-v1")
                            .to_string(),
                        evidence: Evidence::Quantity {
                            amount: Decimal((stopped_at - started).to_string()),
                        },
                        replaces: None,
                        expected_version: None,
                        happened_at: *stopped_at,
                    },
                    now,
                    touched,
                )
                .await?;
                let updated = Self::participant_stream(tx, actor, occurrence, actor, false).await?;
                let state: ProgressState = serde_json::from_value(updated.value.clone())?;
                if state.action.outcome == Outcome::Complete {
                    Self::require_dependencies(tx, actor, occurrence, now).await?;
                }
            }
            _ => unreachable!(),
        }
        touched.insert(stream.id);
        Ok(())
    }
}
