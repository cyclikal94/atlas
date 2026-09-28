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
/// Which of the caller's sessions the account-wide listing returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerState {
    Running,
    Stopped,
    All,
}

/// One row of the caller's own account-wide timer list. A session whose task the caller can no
/// longer read is `Restricted`: it is still listed (it is the caller's own data) but carries no
/// task detail at all, so nothing about the resource leaks through the row.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "access", rename_all = "snake_case")]
pub enum AccountTimerSession {
    Available {
        id: String,
        occurrence_id: String,
        task_id: String,
        task_title: String,
        slot_date: Option<String>,
        started_at: i64,
        stopped_at: Option<i64>,
        version: i64,
        /// Advisory: the caller could stop or discard this running session now. The command
        /// remains the authority (version, goal-unit and covered-occurrence rules still apply).
        can_modify: bool,
    },
    Restricted {
        id: String,
        started_at: i64,
        stopped_at: Option<i64>,
        version: i64,
    },
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AccountTimerPage {
    pub items: Vec<AccountTimerSession>,
    /// Sort key of the last returned row; present only when a further row exists.
    pub next_after: Option<String>,
}

/// What the caller may see and do for one progress stream, resolved once per stream per page.
struct TimerAccess {
    occurrence_id: String,
    task_id: String,
    task_title: String,
    slot_date: Option<String>,
    can_modify: bool,
}

/// Listing cursor: the sort key of the last returned row, running rows first.
#[derive(Clone, Debug, PartialEq, Eq)]
enum TimerCursor {
    Running(i64, String),
    Stopped(i64, String),
}

impl TimerCursor {
    fn parse(after: &str) -> Result<Self> {
        ensure!(after.len() <= 64, ErrorCode::InvalidValue);
        let (kind, rest) = after
            .split_once('.')
            .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
        let (time, id) = rest
            .split_once('.')
            .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
        let digits = time.strip_prefix('-').unwrap_or(time);
        ensure!(
            (1..=18).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()),
            ErrorCode::InvalidValue
        );
        let time: i64 = time.parse().map_err(|_| anyhow!(ErrorCode::InvalidValue))?;
        identifier(id)?;
        match kind {
            "r" => Ok(Self::Running(time, id.into())),
            "s" => Ok(Self::Stopped(time, id.into())),
            _ => Err(anyhow!(ErrorCode::InvalidValue)),
        }
    }
}

impl Store {
    /// The caller's own timers across every occurrence: running ones first (newest start first),
    /// then history by finish time (newest first), ties by id. Cancelled sessions are not listed.
    /// Access is applied per row to *redact*, never to omit or to page-then-filter, so a page is
    /// always `limit` rows (or the last page) and a keyset cursor names a row that exists.
    pub async fn account_timer_sessions(
        &self,
        actor: &str,
        state: TimerState,
        after: Option<&str>,
        limit: u16,
    ) -> Result<AccountTimerPage> {
        ensure!((1..=200).contains(&limit), ErrorCode::InvalidValue);
        let cursor = after.map(TimerCursor::parse).transpose()?;
        // A cursor names a row of one segment; it cannot resume a listing that excludes it.
        ensure!(
            !matches!(
                (state, &cursor),
                (TimerState::Stopped, Some(TimerCursor::Running(..)))
                    | (TimerState::Running, Some(TimerCursor::Stopped(..)))
            ),
            ErrorCode::InvalidValue
        );
        let mut tx = self.task_read().await?;
        Self::epoch(&mut tx, actor).await?;
        // One extra row tells whether a further page exists without a trailing empty page.
        let wanted = i64::from(limit) + 1;
        let mut rows = Vec::new();
        if state != TimerState::Stopped && !matches!(cursor, Some(TimerCursor::Stopped(..))) {
            // A person's running timers are bounded by the occurrences they have open, not by their
            // history, and a partial index holds exactly them. They are read whole and ordered and
            // paged here: an SQL `ORDER BY started_at` would be served by a history index and read
            // the whole account (measured in `examples/timer_list_load.rs`).
            let mut live = self.running_timers(&mut tx, actor).await?;
            live.sort_by(|a, b| {
                (b.get::<i64, _>(2), b.get::<String, _>(0))
                    .cmp(&(a.get::<i64, _>(2), a.get::<String, _>(0)))
            });
            if let Some(TimerCursor::Running(at, id)) = &cursor {
                live.retain(|r| (r.get::<i64, _>(2), r.get::<String, _>(0)) < (*at, id.clone()));
            }
            live.truncate(wanted as usize);
            rows.extend(live);
        }
        let running = rows.len();
        if state != TimerState::Running && (rows.len() as i64) < wanted {
            let resume = match &cursor {
                Some(TimerCursor::Stopped(at, id)) => Some((*at, id.as_str())),
                _ => None,
            };
            let room = wanted - rows.len() as i64;
            rows.extend(Self::finished_timers(&mut tx, actor, resume, room).await?);
        }
        let more = rows.len() > usize::from(limit);
        rows.truncate(usize::from(limit));
        let streams: BTreeSet<String> = rows.iter().map(|r| r.get::<String, _>(1)).collect();
        let access = Self::timer_accesses(&mut tx, actor, &streams).await?;
        let mut items = Vec::with_capacity(rows.len());
        let mut next_after = None;
        for (index, row) in rows.iter().enumerate() {
            let id: String = row.get(0);
            let progress: String = row.get(1);
            let started_at: i64 = row.get(2);
            let stopped_at: Option<i64> = row.get(3);
            let version: i64 = row.get(4);
            items.push(match access.get(&progress) {
                Some(a) => AccountTimerSession::Available {
                    id: id.clone(),
                    occurrence_id: a.occurrence_id.clone(),
                    task_id: a.task_id.clone(),
                    task_title: a.task_title.clone(),
                    slot_date: a.slot_date.clone(),
                    started_at,
                    stopped_at,
                    version,
                    can_modify: stopped_at.is_none() && a.can_modify,
                },
                None => AccountTimerSession::Restricted {
                    id: id.clone(),
                    started_at,
                    stopped_at,
                    version,
                },
            });
            if more && index + 1 == rows.len() {
                next_after = Some(if index < running {
                    format!("r.{started_at}.{id}")
                } else {
                    format!("s.{}.{id}", stopped_at.unwrap_or_default())
                });
            }
        }
        tx.commit().await?;
        Ok(AccountTimerPage { items, next_after })
    }

    /// The caller's running sessions, unordered: `(id, progress_id, started_at, stopped_at, version)`.
    /// SQLite has no statistics here and would pick `timer_account_times`, reading the whole
    /// account; the partial index holds exactly the running rows, so it is named.
    async fn running_timers(
        &self,
        tx: &mut Transaction<'_, Any>,
        actor: &str,
    ) -> Result<Vec<sqlx::any::AnyRow>> {
        let sql = if self.sqlite {
            "SELECT id,progress_id,started_at,stopped_at,version FROM timer_sessions INDEXED BY timer_active_account_occurrence WHERE account_id=$1 AND stopped_at IS NULL AND cancelled=0"
        } else {
            "SELECT id,progress_id,started_at,stopped_at,version FROM timer_sessions WHERE account_id=$1 AND stopped_at IS NULL AND cancelled=0"
        };
        Ok(sqlx::query(sql).bind(actor).fetch_all(&mut **tx).await?)
    }

    /// Finished sessions, latest finish first, after `resume` when given. Separate statements with
    /// and without the cursor clause keep the range usable by `timer_account_stopped`; the
    /// redundant `<=` bound gives the planner a tight start key.
    async fn finished_timers(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        resume: Option<(i64, &str)>,
        limit: i64,
    ) -> Result<Vec<sqlx::any::AnyRow>> {
        let sql = if resume.is_some() {
            "SELECT id,progress_id,started_at,stopped_at,version FROM timer_sessions WHERE account_id=$1 AND stopped_at IS NOT NULL AND cancelled=0 AND stopped_at<=$3 AND (stopped_at<$3 OR id<$4) ORDER BY stopped_at DESC,id DESC LIMIT $2"
        } else {
            "SELECT id,progress_id,started_at,stopped_at,version FROM timer_sessions WHERE account_id=$1 AND stopped_at IS NOT NULL AND cancelled=0 ORDER BY stopped_at DESC,id DESC LIMIT $2"
        };
        let mut query = sqlx::query(sql).bind(actor).bind(limit);
        if let Some((at, id)) = resume {
            query = query.bind(at).bind(id);
        }
        Ok(query.fetch_all(&mut **tx).await?)
    }

    /// The occurrence and task behind each of the caller's progress streams. A stream is absent from
    /// the result when any link is missing or not currently readable by the caller. Visibility is
    /// the repository's own rule (`subset_batch`: ancestors, exclusions and frozen owners all
    /// apply), evaluated for the whole page in a few statements, not one round trip per row.
    async fn timer_accesses(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        streams: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, TimerAccess>> {
        let mut occurrence_of = BTreeMap::new();
        let ids: Vec<&String> = streams.iter().collect();
        for chunk in ids.chunks(100) {
            let placeholders = (0..chunk.len())
                .map(|i| format!("${}", i + 2))
                .collect::<Vec<_>>()
                .join(",");
            let mut query = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT progress_id,occurrence_id FROM occurrence_participants WHERE account_id=$1 AND progress_id IN ({placeholders})"))).bind(actor);
            for id in chunk {
                query = query.bind(id.as_str());
            }
            for row in query.fetch_all(&mut **tx).await? {
                occurrence_of.insert(row.get::<String, _>(0), row.get::<String, _>(1));
            }
        }
        let wanted: BTreeSet<String> = occurrence_of
            .iter()
            .flat_map(|(progress, occurrence)| [progress.clone(), occurrence.clone()])
            .collect();
        let mut visible = Self::subset_batch(tx, actor, &wanted).await?;
        let mut resolved = Vec::new();
        for (progress, occurrence) in &occurrence_of {
            let (Some(stream), Some(target)) = (visible.get(progress), visible.get(occurrence))
            else {
                continue;
            };
            if stream.kind != "progress" || target.kind != "occurrence" {
                continue;
            }
            let Ok(data) = serde_json::from_value::<OccurrenceData>(target.value.clone()) else {
                continue;
            };
            resolved.push((progress.clone(), occurrence.clone(), stream.can_edit, data));
        }
        visible.clear();
        let task_ids: BTreeSet<String> = resolved.iter().map(|r| r.3.task_id.clone()).collect();
        let tasks = Self::subset_batch(tx, actor, &task_ids).await?;
        let mut access = BTreeMap::new();
        for (progress, occurrence, can_edit, data) in resolved {
            let Some(task) = tasks.get(&data.task_id) else {
                continue;
            };
            // What `timer_command` requires before it stops or discards a session.
            let can_modify = can_edit
                && data.covered_by.is_none()
                && matches!(&data.definition.goal, Goal::Numeric { unit, .. } if unit == "seconds");
            access.insert(
                progress,
                TimerAccess {
                    occurrence_id: occurrence,
                    task_id: data.task_id,
                    task_title: task.label.clone(),
                    slot_date: data.slot.date,
                    can_modify,
                },
            );
        }
        Ok(access)
    }

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
                // One active session per person per occurrence, across devices. Timers on other
                // occurrences are independent and may run at the same time (BE-B6); the unique
                // index `timer_active_account_occurrence` enforces the same scope in the database.
                let overlapping: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM timer_sessions WHERE account_id=$1 AND progress_id=$2 AND cancelled=0 AND (stopped_at IS NULL OR (started_at <= $3 AND stopped_at > $3))").bind(actor).bind(&stream.id).bind(started_at).fetch_one(&mut **tx).await?;
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
                // Stopping must not make this occurrence's own sessions overlap; sessions of the
                // person's other occurrences are independent.
                let overlapping: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM timer_sessions WHERE account_id=$1 AND progress_id=$2 AND cancelled=0 AND id<>$3 AND started_at<$4 AND (stopped_at IS NULL OR stopped_at>$5)").bind(actor).bind(&stream.id).bind(session).bind(stopped_at).bind(started).fetch_one(&mut **tx).await?;
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
