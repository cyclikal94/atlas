use super::*;
use crate::error::ErrorCode;

impl Store {
    pub(crate) async fn task_read(&self) -> Result<Transaction<'static, Any>> {
        let mut tx = self.pool.begin().await?;
        if !self.sqlite {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
        }
        Ok(tx)
    }
    pub(crate) async fn needs_materialisation(
        tx: &mut Transaction<'_, Any>,
        task: &str,
        now: i64,
    ) -> Result<bool> {
        let archived: i64 = sqlx::query_scalar("SELECT archived FROM resources WHERE id=$1")
            .bind(task)
            .fetch_one(&mut **tx)
            .await?;
        if archived == 1 {
            return Ok(false);
        }
        let row=sqlx::query("SELECT t.last_date,t.once_created,r.value FROM tasks t JOIN resources r ON r.id=t.execution_id WHERE t.task_id=$1").bind(task).fetch_one(&mut **tx).await?;
        let definition: Definition = serde_json::from_str(&row.get::<String, _>(2))?;
        if definition.schedule.repeat.is_none() && row.get::<i64, _>(1) == 1 {
            return Ok(false);
        }
        let zone: chrono_tz::Tz = definition.schedule.timezone.parse()?;
        let today = DateTime::<Utc>::from_timestamp(now, 0)
            .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?
            .with_timezone(&zone)
            .date_naive()
            .to_string();
        Ok(!definition
            .schedule
            .slots(row.get::<Option<String>, _>(0).as_deref(), &today, 1)?
            .0
            .is_empty())
    }
    pub(crate) async fn pending_tasks(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        filter: &ViewFilter,
        now: i64,
    ) -> Result<Vec<String>> {
        let ids:Vec<String>=sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT t.task_id FROM tasks t JOIN resources r ON r.id=t.execution_id JOIN resources root ON root.id=t.task_id LEFT JOIN resource_grants g ON g.resource_id=r.id AND g.account_id=$1 WHERE root.archived=0 AND ($2 IS NULL OR t.task_id=$2) AND ($3 IS NULL OR EXISTS(SELECT 1 FROM list_items li WHERE li.list_id=$3 AND li.task_id=t.task_id)) AND {} ORDER BY t.task_id",crate::policy::VISIBLE))).bind(actor).bind(&filter.task_id).bind(&filter.list_id).fetch_all(&mut **tx).await?;
        let mut pending = Vec::new();
        for id in ids {
            if Self::needs_materialisation(tx, &id, now).await? {
                pending.push(id);
            }
        }
        Ok(pending)
    }
    pub async fn task_detail(&self, actor: &str, id: &str, now: i64) -> Result<serde_json::Value> {
        let mut tx = self.task_read().await?;
        let task = Self::task_resource(&mut tx, actor, id, "task", false).await?;
        let execution = match Self::execution(&mut tx, actor, id, false).await {
            Ok(p) => Some(p),
            Err(e) if e.to_string() == "not_found" => None,
            Err(e) => return Err(e),
        };
        let state = if execution.is_some() {
            let last: Option<String> =
                sqlx::query_scalar("SELECT last_date FROM tasks WHERE task_id=$1")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await?;
            Some(
                serde_json::json!({"last_date":last,"pending":Self::needs_materialisation(&mut tx,id,now).await?}),
            )
        } else {
            None
        };
        tx.commit().await?;
        Ok(serde_json::json!({"task":task,"execution":execution,"materialisation":state}))
    }
    pub(crate) async fn outcome_for(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        occurrence: &str,
        data: &OccurrenceData,
        now: i64,
        period: bool,
    ) -> Result<Outcome> {
        let closed = data.closes_at.is_some_and(|v| now >= v);
        if data.covered_by.is_some() {
            return Ok(if closed {
                Outcome::Missed
            } else {
                Outcome::Incomplete
            });
        }
        let rows=sqlx::query("SELECT account_id,progress_id,aggregate_consent FROM occurrence_participants WHERE occurrence_id=$1 ORDER BY account_id").bind(occurrence).fetch_all(&mut **tx).await?;
        let mut streams = Vec::new();
        let mut excluded = Vec::new();
        for row in rows {
            let subject: String = row.get(0);
            let id: String = row.get(1);
            let visible = Self::subset(tx, actor, &BTreeSet::from([id.clone()])).await?;
            if !visible.contains_key(&id) || (actor != subject && row.get::<i64, _>(2) != 1) {
                streams.push(None);
                excluded.push(false);
                continue;
            }
            let stream = Self::stream_data(tx, &id).await?;
            excluded.push(stream.excluded);
            // Anyone has no fixed required roster. Neither an excluded stream nor
            // private on-demand participation may prove a shared skipped period.
            if data.definition.participation == Participation::Anyone && stream.excluded {
                streams.push(None);
                continue;
            }

            streams.push(Some(
                stream
                    .entries
                    .into_iter()
                    .filter(|e| {
                        !period
                            || (data.opens_at.is_none_or(|v| e.happened_at >= v)
                                && data.closes_at.is_none_or(|v| e.happened_at < v))
                    })
                    .map(|e| e.evidence)
                    .collect::<Vec<_>>(),
            ));
        }
        if data.definition.participation != Participation::Anyone
            && !streams.is_empty()
            && streams.iter().all(Option::is_some)
            && excluded.iter().all(|v| *v)
            && data.definition.allow_streak_exclusions
        {
            return Ok(Outcome::Excluded);
        }
        // Late work may resolve a retained/accumulating chore, but cannot repair
        // missed streak windows. Upper/range results still close on the window.
        aggregate(
            &data.definition.goal,
            data.definition.participation,
            &streams,
            closed,
        )
    }
    pub(crate) async fn personal_outcome(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        id: &str,
        data: &OccurrenceData,
        now: i64,
        period: bool,
    ) -> Result<Option<Outcome>> {
        if data.covered_by.is_some() && data.participants.iter().any(|id| id == actor) {
            return Ok(Some(if data.closes_at.is_some_and(|v| now >= v) {
                Outcome::Missed
            } else {
                Outcome::Incomplete
            }));
        }
        let row:Option<String>=sqlx::query_scalar("SELECT progress_id FROM occurrence_participants WHERE occurrence_id=$1 AND account_id=$2").bind(id).bind(actor).fetch_optional(&mut **tx).await?;
        let Some(progress) = row else {
            return Ok(None);
        };
        Self::task_resource(tx, actor, &progress, "progress", false).await?;
        let stream = Self::stream_data(tx, &progress).await?;
        if stream.excluded && data.definition.allow_streak_exclusions {
            return Ok(Some(Outcome::Excluded));
        }
        let evidence = stream
            .entries
            .into_iter()
            .filter(|e| {
                !period
                    || (data.opens_at.is_none_or(|v| e.happened_at >= v)
                        && data.closes_at.is_none_or(|v| e.happened_at < v))
            })
            .map(|e| e.evidence)
            .collect::<Vec<_>>();
        Ok(Some(
            evaluate(
                &data.definition.goal,
                &evidence,
                data.closes_at.is_some_and(|v| now >= v),
            )?
            .outcome,
        ))
    }
    pub async fn progress_journal(
        &self,
        actor: &str,
        id: &str,
        after: i64,
        limit: u16,
    ) -> Result<serde_json::Value> {
        ensure!(
            after >= 0 && (1..=200).contains(&limit),
            ErrorCode::InvalidValue
        );
        let mut tx = self.task_read().await?;
        Self::task_resource(&mut tx, actor, id, "progress", false).await?;
        let rows=sqlx::query("SELECT id,sequence,logical_order,evidence,replaces,happened_at,recorded_by FROM progress_entries WHERE progress_id=$1 AND sequence>$2 ORDER BY sequence LIMIT $3").bind(id).bind(after).bind(i64::from(limit)+1).fetch_all(&mut *tx).await?;
        let more = rows.len() > usize::from(limit);
        let mut entries = Vec::new();
        for row in rows.into_iter().take(usize::from(limit)) {
            entries.push(serde_json::json!({"id":row.get::<String,_>(0),"sequence":row.get::<i64,_>(1),"logical_order":row.get::<i64,_>(2),"evidence":serde_json::from_str::<Evidence>(&row.get::<String,_>(3))?,"replaces":row.get::<Option<String>,_>(4),"happened_at":row.get::<i64,_>(5),"recorded_by":row.get::<String,_>(6)}));
        }
        let next_after = if more {
            entries.last().map(|e| e["sequence"].clone())
        } else {
            None
        };
        tx.commit().await?;
        Ok(serde_json::json!({"entries":entries,"next_after":next_after}))
    }
    pub async fn task_enrolment(&self, actor: &str, task: &str) -> Result<serde_json::Value> {
        let mut tx = self.task_read().await?;
        Self::execution(&mut tx, actor, task, false).await?;
        let row=sqlx::query("SELECT version,active,aggregate_consent,policy FROM task_enrolments WHERE task_id=$1 AND account_id=$2").bind(task).bind(actor).fetch_optional(&mut *tx).await?;
        Ok(if let Some(r) = row {
            serde_json::json!({"version":r.get::<i64,_>(0),"active":r.get::<i64,_>(1)==1,"aggregate_consent":r.get::<i64,_>(2)==1,"policy":serde_json::from_str::<Policy>(&r.get::<String,_>(3))?})
        } else {
            serde_json::json!({"version":0,"active":false,"aggregate_consent":false,"policy":Policy::default()})
        })
    }
    pub async fn resources(
        &self,
        actor: &str,
        kind: &str,
        parent: Option<&str>,
        archived: bool,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<Projection>> {
        ensure!(
            matches!(kind, "task" | "person" | "field" | "list" | "progress")
                && (1..=200).contains(&limit),
            ErrorCode::InvalidValue
        );
        let mut tx = self.task_read().await?;
        let ids:Vec<String>=sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT r.id FROM resources r LEFT JOIN resource_grants g ON g.resource_id=r.id AND g.account_id=$1 WHERE r.kind=$2 AND ($3 IS NULL OR r.parent_id=$3) AND r.archived=$4 AND ($5 IS NULL OR r.id>$5) AND {} ORDER BY r.id LIMIT $6",crate::policy::VISIBLE))).bind(actor).bind(kind).bind(parent).bind(i64::from(archived)).bind(after).bind(i64::from(limit)).fetch_all(&mut *tx).await?;
        let values = Self::subset(&mut tx, actor, &ids.into_iter().collect())
            .await?
            .into_values()
            .collect();
        tx.commit().await?;
        Ok(values)
    }
    pub async fn occurrence_view(
        &self,
        actor: &str,
        filter: &ViewFilter,
        now: i64,
    ) -> Result<ViewPage> {
        let limit = filter.limit.unwrap_or(50);
        ensure!((1..=200).contains(&limit), ErrorCode::InvalidValue);
        ensure!(
            filter.state.as_deref().is_none_or(|s| matches!(
                s,
                "actionable" | "complete" | "missed" | "unknown" | "all"
            )),
            ErrorCode::InvalidValue
        );
        ensure!(
            filter.after.is_none() || filter.revision.is_some(),
            ErrorCode::InvalidValue
        );
        ensure!(
            filter
                .scope
                .as_deref()
                .is_none_or(|s| matches!(s, "personal" | "joint")),
            ErrorCode::InvalidValue
        );
        let day = filter.day.as_deref().map(schedule::date).transpose()?;
        let zone = filter
            .timezone
            .as_deref()
            .unwrap_or("UTC")
            .parse::<chrono_tz::Tz>()
            .map_err(|_| anyhow!(ErrorCode::InvalidValue))?;
        let mut tx = self.task_read().await?;
        if !self.sqlite {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
        }
        Self::epoch(&mut tx, actor).await?;
        let revision: i64 = sqlx::query_scalar("SELECT revision FROM sync_clock WHERE id=1")
            .fetch_one(&mut *tx)
            .await?;
        if let Some(expected) = filter.revision {
            ensure!(expected == revision, ErrorCode::Conflict);
        }
        if let Some(list) = &filter.list_id {
            Self::task_resource(&mut tx, actor, list, "list", false).await?;
        }
        if let Some(task) = &filter.task_id {
            Self::execution(&mut tx, actor, task, false).await?;
        }
        // The installation ceiling bounds candidate evaluation; authority and
        // business filters precede pagination. Never page first and filter later.
        let ids:Vec<String>=sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT r.id FROM task_occurrences o JOIN resources r ON r.id=o.id JOIN resources t ON t.id=o.task_id LEFT JOIN resource_grants g ON g.resource_id=r.id AND g.account_id=$1 WHERE ($2 IS NULL OR o.task_id=$2) AND ($3 IS NULL OR EXISTS(SELECT 1 FROM list_items li WHERE li.list_id=$3 AND li.task_id=o.task_id)) AND ($4 IS NULL OR r.id>$4) AND (t.archived=0 OR $2 IS NOT NULL) AND {} ORDER BY r.id",crate::policy::VISIBLE))).bind(actor).bind(&filter.task_id).bind(&filter.list_id).bind(&filter.after).fetch_all(&mut *tx).await?;
        let mut items = Vec::new();
        let mut next_after = None;
        for id in ids {
            let resource = Self::task_resource(&mut tx, actor, &id, "occurrence", false).await?;
            let data: OccurrenceData = serde_json::from_value(resource.value.clone())?;
            let (outcome, period_outcome) = if filter.scope.as_deref() == Some("personal") {
                let Some(outcome) =
                    Self::personal_outcome(&mut tx, actor, &id, &data, now, false).await?
                else {
                    continue;
                };
                (
                    outcome,
                    Self::personal_outcome(&mut tx, actor, &id, &data, now, true)
                        .await?
                        .unwrap_or(Outcome::Unknown),
                )
            } else {
                (
                    Self::outcome_for(&mut tx, actor, &id, &data, now, false).await?,
                    Self::outcome_for(&mut tx, actor, &id, &data, now, true).await?,
                )
            };
            let participant=sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM occurrence_participants WHERE occurrence_id=$1 AND account_id=$2").bind(&id).bind(actor).fetch_one(&mut *tx).await?==1;
            let eligible = participant
                || (data.definition.participation == Participation::Anyone && resource.can_edit);
            let actionable = eligible
                && !data.resolved
                && data.covered_by.is_none()
                && data.opens_at.is_none_or(|v| v <= now)
                && (data.definition.carry != Carry::CloseIncomplete
                    || data.closes_at.is_none_or(|v| now < v))
                && outcome != Outcome::Complete
                && outcome != Outcome::Excluded;
            if let Some(day) = day {
                let scheduled = if data.slot.instant.is_some() {
                    data.slot
                        .instant
                        .and_then(|v| DateTime::<Utc>::from_timestamp(v, 0))
                        .is_some_and(|v| v.with_timezone(&zone).date_naive() == day)
                } else {
                    data.slot
                        .date
                        .as_deref()
                        .is_some_and(|v| v == day.format("%Y-%m-%d").to_string())
                };
                let day_end = resolve(
                    zone,
                    day.succ_opt()
                        .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?
                        .and_time(NaiveTime::MIN),
                )?;
                let open_for_day = data.opens_at.is_none_or(|v| v < day_end) && actionable;
                if !scheduled && !open_for_day {
                    continue;
                }
            }
            let matches = match filter.state.as_deref().unwrap_or("all") {
                "actionable" => actionable,
                "complete" => outcome == Outcome::Complete,
                "missed" => period_outcome == Outcome::Missed,
                "unknown" => outcome == Outcome::Unknown,
                _ => true,
            };
            if !matches {
                continue;
            }
            if items.len() == usize::from(limit) {
                next_after = items.last().map(|v: &OccurrenceView| v.id.clone());
                break;
            }
            items.push(OccurrenceView {
                id: resource.id,
                version: resource.version,
                can_edit: resource.can_edit,
                data,
                outcome,
                period_outcome,
                actionable,
            });
        }
        let pending_tasks = Self::pending_tasks(&mut tx, actor, filter, now).await?;
        tx.commit().await?;
        Ok(ViewPage {
            items,
            next_after,
            revision,
            pending_tasks,
        })
    }
    pub async fn task_streak(&self, actor: &str, task: &str, now: i64) -> Result<Streak> {
        self.task_streak_scoped(actor, task, "joint", now).await
    }
    pub async fn task_streak_scoped(
        &self,
        actor: &str,
        task: &str,
        scope: &str,
        now: i64,
    ) -> Result<Streak> {
        ensure!(
            matches!(scope, "personal" | "joint"),
            ErrorCode::InvalidValue
        );
        let mut tx = self.task_read().await?;
        if !self.sqlite {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
        }
        Self::execution(&mut tx, actor, task, false).await?;
        ensure!(
            !Self::needs_materialisation(&mut tx, task, now).await?,
            ErrorCode::MaterialisationRequired
        );
        let ids = Self::chronological_occurrences(&mut tx, task).await?;
        let mut periods = Vec::new();
        for id in ids {
            let p = match Self::task_resource(&mut tx, actor, &id, "occurrence", false).await {
                Ok(p) => p,
                Err(e) if e.to_string() == "not_found" => {
                    return Ok(Streak {
                        current: None,
                        longest: None,
                    });
                }
                Err(e) => return Err(e),
            };
            let data: OccurrenceData = serde_json::from_value(p.value.clone())?;
            if data.opens_at.is_some_and(|v| v > now) {
                continue;
            }
            let outcome = if scope == "personal" {
                let Some(outcome) =
                    Self::personal_outcome(&mut tx, actor, &id, &data, now, true).await?
                else {
                    continue;
                };
                outcome
            } else {
                Self::outcome_for(&mut tx, actor, &id, &data, now, true).await?
            };
            // An open, not-yet-observed period cannot erase an established streak.
            if data.closes_at.is_none_or(|v| now < v)
                && matches!(
                    outcome,
                    Outcome::Unknown | Outcome::Incomplete | Outcome::Provisional
                )
            {
                continue;
            }
            periods.push((outcome, data.definition.allow_streak_exclusions));
        }
        tx.commit().await?;
        Ok(streak(&periods))
    }
}
