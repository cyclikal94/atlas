use super::ics::{Event, EventStatus, EventTime};
use crate::error::ErrorCode;
use crate::{
    Store,
    policy::Policy,
    tasks::{Carry, Definition, FieldValue, OccurrenceData, Outcome, Participation, Slot},
};
use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Any, Row, Transaction};
use std::collections::BTreeSet;
use uuid::Uuid;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reference {
    EventSeries { source_id: String, uid: String },
    PersonDate { field_id: String, annual: bool },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Offset {
    CalendarDays { days: i32, time: Option<String> },
    ElapsedSeconds { seconds: i64 },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Anchor {
    pub reference: Reference,
    pub offset: Offset,
    #[serde(default)]
    pub weekdays: Vec<u32>,
    pub title_contains: Option<String>,
}
impl Anchor {
    pub fn reference_id(&self) -> &str {
        match &self.reference {
            Reference::EventSeries { source_id, .. } => source_id,
            Reference::PersonDate { field_id, .. } => field_id,
        }
    }
    fn validate(&self) -> Result<()> {
        crate::identifier(self.reference_id())?;
        ensure!(
            self.weekdays.len() <= 7 && self.weekdays.iter().all(|v| (1..=7).contains(v)),
            ErrorCode::InvalidValue
        );
        ensure!(
            self.title_contains
                .as_ref()
                .is_none_or(|v| !v.is_empty() && v.len() <= 100),
            ErrorCode::InvalidValue
        );
        match &self.offset {
            Offset::CalendarDays { days, time } => {
                ensure!((-366..=366).contains(days), ErrorCode::InvalidValue);
                if let Some(t) = time {
                    ensure!(
                        NaiveTime::parse_from_str(t, "%H:%M:%S").is_ok() && t.len() == 8,
                        ErrorCode::InvalidValue
                    );
                }
            }
            Offset::ElapsedSeconds { seconds } => {
                ensure!(
                    (-31622400..=31622400).contains(seconds),
                    ErrorCode::InvalidValue
                )
            }
        };
        if let Reference::EventSeries { uid, .. } = &self.reference {
            ensure!(!uid.is_empty() && uid.len() <= 512, ErrorCode::InvalidValue);
        }
        Ok(())
    }
}
fn shifted(time: &EventTime, anchor: &Anchor, definition: &Definition, key: &str) -> Result<Slot> {
    let zone = definition
        .schedule
        .timezone
        .parse::<chrono_tz::Tz>()
        .map_err(|_| anyhow!(ErrorCode::InvalidValue))?;
    let (date, clock) = match &anchor.offset {
        Offset::CalendarDays { days, time: clock } => {
            let local = time
                .instant
                .map(|v| {
                    DateTime::<Utc>::from_timestamp(v, 0)
                        .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))
                })
                .transpose()?
                .map(|v| v.with_timezone(&zone));
            let date = if let Some(v) = local {
                v.date_naive()
            } else {
                NaiveDate::parse_from_str(&time.date, "%Y-%m-%d")?
            };
            let date = date
                .checked_add_signed(Duration::days(i64::from(*days)))
                .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
            let clock = clock
                .clone()
                .or_else(|| local.map(|v| v.time().format("%H:%M:%S").to_string()));
            (date, clock)
        }
        Offset::ElapsedSeconds { seconds } => {
            let instant = time
                .instant
                .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?
                .checked_add(*seconds)
                .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
            let local = DateTime::<Utc>::from_timestamp(instant, 0)
                .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?
                .with_timezone(&zone);
            (
                local.date_naive(),
                Some(local.time().format("%H:%M:%S").to_string()),
            )
        }
    };
    let instant = clock
        .as_ref()
        .map(|v| {
            crate::tasks::resolve(
                zone,
                date.and_time(NaiveTime::parse_from_str(v, "%H:%M:%S")?),
            )
        })
        .transpose()?;
    Ok(Slot {
        key: key.into(),
        date: Some(date.to_string()),
        intended_time: clock,
        timezone: zone.to_string(),
        instant,
    })
}
impl Store {
    pub(crate) async fn set_task_anchor(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        task: &str,
        anchor: &Anchor,
    ) -> Result<()> {
        anchor.validate()?;
        let execution = Self::execution(tx, actor, task, true).await?;
        let definition: Definition = serde_json::from_value(execution.value.clone())?;
        ensure!(
            definition.schedule.repeat.is_none(),
            ErrorCode::InvalidValue
        );
        let reference = Self::task_resource(tx, actor, anchor.reference_id(), "", false).await?;
        match anchor.reference {
            Reference::EventSeries { .. } => {
                ensure!(reference.kind == "calendar_source", ErrorCode::InvalidValue)
            }
            Reference::PersonDate { .. } => {
                ensure!(
                    reference.kind == "field"
                        && matches!(
                            serde_json::from_value::<FieldValue>(reference.value.clone())?,
                            FieldValue::Date { .. }
                        ),
                    ErrorCode::InvalidValue
                );
            }
        }
        for reader in Self::audience(tx, &BTreeSet::from([execution.id.clone()])).await? {
            if Self::subset(tx, &reader, &BTreeSet::from([execution.id.clone()]))
                .await?
                .is_empty()
            {
                continue;
            }
            Self::task_resource(tx, &reader, anchor.reference_id(), "", false)
                .await
                .map_err(|_| anyhow!(ErrorCode::Forbidden))?;
        }
        sqlx::query("INSERT INTO task_anchors(task_id,owner_id,spec) VALUES ($1,$2,$3) ON CONFLICT(task_id) DO UPDATE SET owner_id=excluded.owner_id,spec=excluded.spec,version=task_anchors.version+1,active=1").bind(task).bind(actor).bind(serde_json::to_string(anchor)?).execute(&mut **tx).await?;
        // Anchored schedules are generated only from their references, never clock polling.
        sqlx::query("UPDATE tasks SET once_created=1 WHERE task_id=$1")
            .bind(task)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
    pub async fn task_anchor(&self, actor: &str, task: &str) -> Result<Option<serde_json::Value>> {
        let mut tx = self.pool.begin().await?;
        if !self.sqlite {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
        }
        Self::execution(&mut tx, actor, task, false).await?;
        let row =
            sqlx::query("SELECT spec,version,active,owner_id FROM task_anchors WHERE task_id=$1")
                .bind(task)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(row) = row {
            let anchor: Anchor = serde_json::from_str(&row.get::<String, _>(0))?;
            match Self::task_resource(&mut tx, actor, anchor.reference_id(), "", false).await {
                Ok(_) => {}
                Err(e) if e.downcast_ref::<ErrorCode>() == Some(&ErrorCode::NotFound) => {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
            if row.get::<String, _>(3) != actor
                && let Reference::EventSeries { source_id, uid } = &anchor.reference
            {
                // Source access alone does not disclose identifiers of independently
                // private events. The binding author already supplied this UID.
                let events: Vec<String> = sqlx::query_scalar(
                    "SELECT id FROM calendar_events WHERE source_id=$1 AND uid=$2",
                )
                .bind(source_id)
                .bind(uid)
                .fetch_all(&mut *tx)
                .await?;
                let mut known = false;
                for event in events {
                    match Self::task_resource(&mut tx, actor, &event, "event", false).await {
                        Ok(_) => {
                            known = true;
                            break;
                        }
                        Err(e) if e.downcast_ref::<ErrorCode>() == Some(&ErrorCode::NotFound) => {}
                        Err(e) => return Err(e),
                    }
                }
                if !known {
                    return Ok(None);
                }
            }
            Ok(Some(
                serde_json::json!({"anchor":anchor,"version":row.get::<i64,_>(1),"active":row.get::<i64,_>(2)==1}),
            ))
        } else {
            Ok(None)
        }
    }
    pub(crate) async fn reconcile_task_anchor(
        tx: &mut Transaction<'_, Any>,
        task: &str,
        now: i64,
        touched: &mut BTreeSet<String>,
    ) -> Result<()> {
        let row =
            sqlx::query("SELECT owner_id,spec FROM task_anchors WHERE task_id=$1 AND active=1")
                .bind(task)
                .fetch_optional(&mut **tx)
                .await?;
        let Some(row) = row else {
            return Ok(());
        };
        let actor: String = row.get(0);
        let anchor: Anchor = serde_json::from_str(&row.get::<String, _>(1))?;
        let execution = match Self::execution(tx, &actor, task, true).await {
            Ok(v) => v,
            Err(e) if matches!(e.to_string().as_str(), "not_found" | "forbidden") => return Ok(()),
            Err(e) => return Err(e),
        };
        let root = Self::task_resource(tx, &actor, task, "task", false).await?;
        if root.archived {
            return Ok(());
        }
        let definition: Definition = serde_json::from_value(execution.value.clone())?;
        let reference =
            match Self::task_resource(tx, &actor, anchor.reference_id(), "", false).await {
                Ok(v) => Some(v),
                Err(e) if e.downcast_ref::<ErrorCode>() == Some(&ErrorCode::NotFound) => None,
                Err(e) => return Err(e),
            };
        let mut candidates = Vec::<(String, String, i64, EventTime, EventStatus, String)>::new();
        if let Some(reference) = reference.filter(|v| !v.archived) {
            match &anchor.reference {
                Reference::EventSeries { source_id, uid } => {
                    let events = sqlx::query("SELECT id,data FROM calendar_events WHERE source_id=$1 AND uid=$2 ORDER BY original")
                        .bind(source_id).bind(uid).fetch_all(&mut **tx).await?;
                    for row in events {
                        let id: String = row.get(0);
                        let resource =
                            match Self::task_resource(tx, &actor, &id, "event", false).await {
                                Ok(resource) => resource,
                                Err(error)
                                    if error.downcast_ref::<ErrorCode>()
                                        == Some(&ErrorCode::NotFound) =>
                                {
                                    continue;
                                }
                                Err(error) => return Err(error),
                            };
                        let event: Event = serde_json::from_str(&row.get::<String, _>(1))?;
                        candidates.push((
                            id.clone(),
                            id,
                            resource.version,
                            event.start,
                            event.status,
                            event.title,
                        ));
                    }
                }
                Reference::PersonDate { field_id, annual } => {
                    if let FieldValue::Date { year, month, day } =
                        serde_json::from_value(reference.value.clone())?
                    {
                        let today = DateTime::<Utc>::from_timestamp(now, 0)
                            .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?
                            .date_naive();
                        let mut years = if *annual {
                            BTreeSet::from([today.year() - 1, today.year(), today.year() + 1])
                        } else {
                            BTreeSet::from([year.ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?])
                        };
                        if *annual {
                            let keys: Vec<String> = sqlx::query_scalar("SELECT anchor_key FROM anchored_occurrences WHERE task_id=$1 AND reference_id=$2")
                                .bind(task).bind(field_id).fetch_all(&mut **tx).await?;
                            for key in keys {
                                if let Some((_, year)) = key.rsplit_once(':') {
                                    years.insert(year.parse::<i32>()?);
                                }
                            }
                        }
                        for year in years {
                            let date = NaiveDate::from_ymd_opt(year, month, day).or_else(|| {
                                if *annual && month == 2 && day == 29 {
                                    NaiveDate::from_ymd_opt(year, 2, 28)
                                } else {
                                    None
                                }
                            });
                            if let Some(date) = date {
                                candidates.push((
                                    format!("{field_id}:{year}"),
                                    field_id.clone(),
                                    reference.version,
                                    EventTime {
                                        date: date.to_string(),
                                        time: None,
                                        timezone: definition.schedule.timezone.clone(),
                                        instant: None,
                                    },
                                    EventStatus::Present,
                                    reference.label.clone(),
                                ));
                            }
                        }
                    }
                }
            }
        }
        let existing=sqlx::query("SELECT occurrence_id,anchor_key,reference_id,anchor_revision FROM anchored_occurrences WHERE task_id=$1").bind(task).fetch_all(&mut **tx).await?;
        let available = candidates
            .iter()
            .map(|v| v.0.clone())
            .collect::<BTreeSet<_>>();
        for row in existing {
            let key: String = row.get(1);
            if !available.contains(&key) {
                let id: String = row.get(0);
                Self::anchor_review(
                    tx,
                    &actor,
                    &id,
                    "context_unavailable",
                    None,
                    row.get::<i64, _>(3) + 1,
                    None,
                    now,
                    touched,
                )
                .await?;
            }
        }
        for (key, reference_id, revision, start, status, title) in candidates {
            let old: Option<String> = sqlx::query_scalar(
                "SELECT occurrence_id FROM anchored_occurrences WHERE task_id=$1 AND anchor_key=$2",
            )
            .bind(task)
            .bind(&key)
            .fetch_optional(&mut **tx)
            .await?;
            let slot = shifted(&start, &anchor, &definition, &format!("anchor:{key}"))?;
            let date = NaiveDate::parse_from_str(slot.date.as_deref().unwrap_or(""), "%Y-%m-%d")?;
            let eligible = (anchor.weekdays.is_empty()
                || anchor
                    .weekdays
                    .contains(&date.weekday().number_from_monday()))
                && anchor
                    .title_contains
                    .as_ref()
                    .is_none_or(|v| title.to_lowercase().contains(&v.to_lowercase()));
            if let Some(id) = old {
                let p = Self::task_resource(tx, &actor, &id, "occurrence", false).await?;
                let mut data: OccurrenceData = serde_json::from_value(p.value.clone())?;
                if data.resolved
                    || (data.definition.carry == Carry::CloseIncomplete
                        && data.closes_at.is_some_and(|v| v <= now))
                {
                    continue;
                }
                let readers = Self::audience(tx, &BTreeSet::from([id.clone()])).await?;
                let mut common = true;
                let mut completed = true;
                let streams=sqlx::query("SELECT account_id,progress_id,aggregate_consent FROM occurrence_participants WHERE occurrence_id=$1").bind(&id).fetch_all(&mut **tx).await?;
                for reader in readers {
                    let Ok(visible) =
                        Self::task_resource(tx, &reader, &id, "occurrence", false).await
                    else {
                        continue;
                    };
                    if Self::task_resource(tx, &reader, &reference_id, "", false)
                        .await
                        .is_err()
                    {
                        common = false;
                        break;
                    }
                    if data.definition.participation == Participation::Anyone
                        && visible.can_edit
                        && !streams.iter().any(|r| r.get::<String, _>(0) == reader)
                    {
                        common = false;
                        break;
                    }
                    for stream in &streams {
                        if stream.get::<i64, _>(2) != 1
                            || Self::task_resource(
                                tx,
                                &reader,
                                &stream.get::<String, _>(1),
                                "progress",
                                false,
                            )
                            .await
                            .is_err()
                        {
                            common = false;
                            break;
                        }
                    }
                    completed &= Self::outcome_for(tx, &reader, &id, &data, now, false).await?
                        == Outcome::Complete;
                }
                if !common {
                    Self::anchor_review(
                        tx,
                        &actor,
                        &id,
                        "context_unavailable",
                        None,
                        revision,
                        None,
                        now,
                        touched,
                    )
                    .await?;
                    continue;
                }
                if completed {
                    continue;
                }
                let reason = if status == EventStatus::Cancelled {
                    Some("cancelled")
                } else if status == EventStatus::Missing {
                    Some("missing")
                } else if !eligible {
                    Some("no_longer_eligible")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    Self::anchor_review(
                        tx,
                        &actor,
                        &id,
                        reason,
                        Some(&reference_id),
                        revision,
                        None,
                        now,
                        touched,
                    )
                    .await?;
                    continue;
                }
                if data.slot.date == slot.date
                    && data.slot.intended_time == slot.intended_time
                    && data.slot.instant == slot.instant
                {
                    continue;
                }
                // Explicitly recorded work freezes its period; offer review instead of
                // silently changing which evidence belongs to a historical success.
                let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM progress_entries e JOIN occurrence_participants p ON p.progress_id=e.progress_id WHERE p.occurrence_id=$1").bind(&id).fetch_one(&mut **tx).await?;
                if count > 0 {
                    Self::anchor_review(
                        tx,
                        &actor,
                        &id,
                        "work_started",
                        Some(&reference_id),
                        revision,
                        Some(&slot),
                        now,
                        touched,
                    )
                    .await?;
                    continue;
                }
                let previous = data.slot.clone();
                data.slot = Slot {
                    key: data.slot.key.clone(),
                    ..slot
                };
                let zone = data.slot.timezone.parse::<chrono_tz::Tz>()?;
                data.opens_at = Some(crate::tasks::resolve(
                    zone,
                    (date - Duration::days(i64::from(data.definition.open_days_before)))
                        .and_time(NaiveTime::MIN),
                )?);
                data.closes_at = Some(crate::tasks::resolve(
                    zone,
                    (date + Duration::days(i64::from(data.definition.close_days_after)))
                        .and_time(NaiveTime::MIN),
                )?);
                Self::value(tx, &id, "Occurrence", &serde_json::to_string(&data)?).await?;
                sqlx::query(
                    "UPDATE task_occurrences SET slot=$1,opens_at=$2,closes_at=$3 WHERE id=$4",
                )
                .bind(serde_json::to_string(&data.slot)?)
                .bind(data.opens_at)
                .bind(data.closes_at)
                .bind(&id)
                .execute(&mut **tx)
                .await?;
                sqlx::query(
                    "UPDATE anchored_occurrences SET anchor_revision=$1 WHERE occurrence_id=$2",
                )
                .bind(revision)
                .bind(&id)
                .execute(&mut **tx)
                .await?;
                for stream in &streams {
                    Self::refresh_stream(
                        tx,
                        &stream.get::<String, _>(1),
                        &data.definition.goal,
                        data.closes_at,
                    )
                    .await?;
                }
                Self::anchor_review(
                    tx,
                    &actor,
                    &id,
                    "moved",
                    Some(&reference_id),
                    revision,
                    Some(&previous),
                    now,
                    touched,
                )
                .await?;
            } else if status == EventStatus::Present && eligible {
                let today = DateTime::<Utc>::from_timestamp(now, 0)
                    .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?
                    .date_naive();
                if date < today - Duration::days(30) || date > today + Duration::days(400) {
                    continue;
                }
                let mut allowed = true;
                for reader in Self::audience(tx, &BTreeSet::from([execution.id.clone()])).await? {
                    if Self::task_resource(tx, &reader, &execution.id, "execution", false)
                        .await
                        .is_ok()
                        && Self::task_resource(tx, &reader, &reference_id, "", false)
                            .await
                            .is_err()
                    {
                        allowed = false;
                        break;
                    }
                }
                if !allowed {
                    continue;
                }
                let id = crate::tasks::occurrence_id(task, &slot.key)?;
                Self::make_occurrence(tx, task, &execution, &definition, slot, now, touched)
                    .await?;
                sqlx::query("INSERT INTO anchored_occurrences VALUES ($1,$2,$3,$4,$5)")
                    .bind(id)
                    .bind(task)
                    .bind(key)
                    .bind(reference_id)
                    .bind(revision)
                    .execute(&mut **tx)
                    .await?;
            }
        }
        touched.extend(Self::task_touched(tx, task).await?);
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    async fn anchor_review(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        occurrence: &str,
        reason: &str,
        reference: Option<&str>,
        revision: i64,
        slot: Option<&Slot>,
        now: i64,
        touched: &mut BTreeSet<String>,
    ) -> Result<()> {
        let p = Self::task_resource(tx, actor, occurrence, "occurrence", false).await?;
        let data: OccurrenceData = serde_json::from_value(p.value.clone())?;
        if data.resolved
            || (data.definition.carry == Carry::CloseIncomplete
                && data.closes_at.is_some_and(|v| v <= now))
        {
            return Ok(());
        }
        if Self::outcome_for(tx, actor, occurrence, &data, now, false).await? == Outcome::Complete {
            return Ok(());
        }
        let id = Uuid::new_v5(
            &Uuid::parse_str(occurrence)?,
            format!("atlas-review-v1:{reason}:{revision}").as_bytes(),
        )
        .to_string();
        let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM calendar_reviews WHERE id=$1")
            .bind(&id)
            .fetch_one(&mut **tx)
            .await?;
        if exists > 0 {
            return Ok(());
        }
        Self::new_resource(
            tx,
            actor,
            &id,
            Some(occurrence),
            "review",
            "Review task",
            &serde_json::json!({"reason":reason,"resolved":false,"slot":slot}).to_string(),
            &Policy::default(),
        )
        .await?;
        sqlx::query("INSERT INTO calendar_reviews(id,occurrence_id,reason,reference_id,source_revision) VALUES ($1,$2,$3,$4,$5)").bind(&id).bind(occurrence).bind(reason).bind(reference).bind(revision).execute(&mut **tx).await?;
        touched.insert(id);
        Ok(())
    }
    pub async fn reconcile_calendar_tasks(&self, now: i64) -> Result<()> {
        let mut tx = self.begin_serial().await?;
        let tasks: Vec<String> =
            sqlx::query_scalar("SELECT task_id FROM task_anchors WHERE active=1 ORDER BY task_id")
                .fetch_all(&mut *tx)
                .await?;
        let mut touched = BTreeSet::new();
        for task in &tasks {
            touched.extend(Self::task_touched(&mut tx, task).await?);
        }
        let before = Self::calendar_before(&mut tx, &touched).await?;
        for task in tasks {
            Self::reconcile_task_anchor(&mut tx, &task, now, &mut touched).await?;
        }
        if !touched.is_empty() {
            Self::calendar_publish(&mut tx, &touched, before).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
