use super::ics::{Event, EventStatus, Feed};
use crate::error::ErrorCode;
use crate::{Projection, Store, identifier, policy::Policy, receipt_digest};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{Any, Row, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CalendarCommand {
    CreateSource {
        id: String,
        label: String,
        timezone: String,
        connection: Option<String>,
        initial_policy: Option<Policy>,
    },
    ConfigureSource {
        id: String,
        expected_version: i64,
        timezone: String,
        connection: Option<String>,
        enabled: bool,
    },
    SetAnchor {
        id: String,
        expected_version: i64,
        anchor: Option<super::Anchor>,
    },
    ResolveReview {
        id: String,
        expected_version: i64,
        resolved: bool,
    },
}
#[derive(Clone, Debug, Serialize)]
pub struct Refresh {
    pub generation: i64,
    pub timezone: String,
    pub connection: Option<String>,
    pub etag: Option<String>,
    pub modified: Option<String>,
}
pub(super) type Before = (
    BTreeSet<String>,
    BTreeMap<String, BTreeMap<String, Projection>>,
);
impl Store {
    pub(super) async fn calendar_before(
        tx: &mut Transaction<'_, Any>,
        touched: &BTreeSet<String>,
    ) -> Result<Before> {
        let accounts = Self::audience(tx, touched).await?;
        let mut before = BTreeMap::new();
        for a in &accounts {
            before.insert(a.clone(), Self::subset(tx, a, touched).await?);
        }
        Ok((accounts, before))
    }
    pub(super) async fn calendar_publish(
        tx: &mut Transaction<'_, Any>,
        touched: &BTreeSet<String>,
        (mut accounts, mut before): Before,
    ) -> Result<i64> {
        accounts.extend(Self::audience(tx, touched).await?);
        for a in &accounts {
            before.entry(a.clone()).or_default();
        }
        let mut changed = false;
        for account in &accounts {
            if Self::subset(tx, account, touched).await? != before[account] {
                changed = true;
                break;
            }
        }
        if !changed {
            return Ok(
                sqlx::query_scalar("SELECT revision FROM sync_clock WHERE id=1")
                    .fetch_one(&mut **tx)
                    .await?,
            );
        }
        Self::publish(tx, &accounts, touched, &before, true).await
    }
    pub async fn calendar_command(
        &self,
        actor: &str,
        operation: &str,
        command: &CalendarCommand,
    ) -> Result<i64> {
        identifier(operation)?;
        let mut tx = self.begin_serial().await?;
        Self::epoch(&mut tx, actor).await?;
        let mut receipt_command = command.clone();
        match &mut receipt_command {
            CalendarCommand::CreateSource { connection, .. }
            | CalendarCommand::ConfigureSource { connection, .. } => {
                *connection = connection.as_ref().map(|v| super::secret_identity(v));
            }
            _ => {}
        }
        let hash = receipt_digest(&format!(
            "calendar-command-v1:{}",
            serde_json::to_string(&receipt_command)?
        ));
        if let Some(row) = sqlx::query(
            "SELECT payload,revision FROM receipts WHERE account_id=$1 AND operation_id=$2",
        )
        .bind(actor)
        .bind(operation)
        .fetch_optional(&mut *tx)
        .await?
        {
            ensure!(
                row.get::<String, _>(0) == hash,
                ErrorCode::OperationConflict
            );
            return Ok(row.get(1));
        }
        let id = match command {
            CalendarCommand::CreateSource { id, .. }
            | CalendarCommand::ConfigureSource { id, .. }
            | CalendarCommand::ResolveReview { id, .. }
            | CalendarCommand::SetAnchor { id, .. } => id,
        };
        let mut touched = Self::task_touched(&mut tx, id).await?;
        let before = Self::calendar_before(&mut tx, &touched).await?;
        match command {
            CalendarCommand::CreateSource {
                id,
                label,
                timezone,
                connection,
                initial_policy,
            } => {
                timezone
                    .parse::<chrono_tz::Tz>()
                    .map_err(|_| anyhow!(ErrorCode::InvalidValue))?;
                ensure!(
                    connection.as_ref().is_none_or(|v| v.len() <= 8192),
                    ErrorCode::InvalidValue
                );
                Self::new_resource(
                    &mut tx,
                    actor,
                    id,
                    None,
                    "calendar_source",
                    label,
                    "{}",
                    &initial_policy.clone().unwrap_or_default(),
                )
                .await?;
                sqlx::query(
                    "INSERT INTO calendar_sources(id,connection,timezone) VALUES ($1,$2,$3)",
                )
                .bind(id)
                .bind(connection)
                .bind(timezone)
                .execute(&mut *tx)
                .await?;
                Self::source_value(&mut tx, id).await?;
            }
            CalendarCommand::ConfigureSource {
                id,
                expected_version,
                timezone,
                connection,
                enabled,
            } => {
                let p = Self::task_resource(&mut tx, actor, id, "calendar_source", true).await?;
                ensure!(p.version == *expected_version, ErrorCode::Conflict);
                // Connection settings are owner-only even when imported events are collaborative.
                let owner: String =
                    sqlx::query_scalar("SELECT owner_id FROM resources WHERE id=$1")
                        .bind(id)
                        .fetch_one(&mut *tx)
                        .await?;
                ensure!(owner == actor, ErrorCode::Forbidden);
                timezone
                    .parse::<chrono_tz::Tz>()
                    .map_err(|_| anyhow!(ErrorCode::InvalidValue))?;
                ensure!(
                    connection.as_ref().is_none_or(|v| v.len() <= 8192),
                    ErrorCode::InvalidValue
                );
                sqlx::query("UPDATE calendar_sources SET connection=$1,timezone=$2,generation=generation+1,lease_until=0,next_refresh=0,health='pending',etag=NULL,modified=NULL WHERE id=$3").bind(connection).bind(timezone).bind(id).execute(&mut *tx).await?;
                sqlx::query("UPDATE resources SET archived=$1 WHERE id=$2")
                    .bind(i64::from(!enabled))
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                Self::source_value(&mut tx, id).await?;
            }
            CalendarCommand::SetAnchor {
                id,
                expected_version,
                anchor,
            } => {
                let p = Self::task_resource(&mut tx, actor, id, "task", true).await?;
                ensure!(p.version == *expected_version, ErrorCode::Conflict);
                if let Some(anchor) = anchor {
                    Self::set_task_anchor(&mut tx, actor, id, anchor).await?;
                    Self::reconcile_task_anchor(
                        &mut tx,
                        id,
                        crate::calendars::unix_now(),
                        &mut touched,
                    )
                    .await?;
                } else {
                    sqlx::query(
                        "UPDATE task_anchors SET active=0,version=version+1 WHERE task_id=$1",
                    )
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                }
                Self::value(&mut tx, id, &p.label, &p.value.to_string()).await?;
            }
            CalendarCommand::ResolveReview {
                id,
                expected_version,
                resolved,
            } => {
                let p = Self::task_resource(&mut tx, actor, id, "review", true).await?;
                ensure!(p.version == *expected_version, ErrorCode::Conflict);
                sqlx::query("UPDATE calendar_reviews SET resolved=$1 WHERE id=$2")
                    .bind(i64::from(*resolved))
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                let mut data: serde_json::Value = serde_json::from_value(p.value.clone())?;
                data["resolved"] = (*resolved).into();
                Self::value(&mut tx, id, &p.label, &data.to_string()).await?;
            }
        }
        touched.extend(Self::task_touched(&mut tx, id).await?);
        let revision = Self::calendar_publish(&mut tx, &touched, before).await?;
        sqlx::query("INSERT INTO receipts(account_id,operation_id,payload,revision,digest_version) VALUES ($1,$2,$3,$4,1)").bind(actor).bind(operation).bind(hash).bind(revision).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(revision)
    }
    async fn source_value(tx: &mut Transaction<'_, Any>, id: &str) -> Result<()> {
        let row=sqlx::query("SELECT health,last_success,coverage_start,coverage_end,timezone FROM calendar_sources WHERE id=$1").bind(id).fetch_one(&mut **tx).await?;
        let label: String = sqlx::query_scalar("SELECT label FROM resources WHERE id=$1")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
        Self::value(tx,id,&label,&serde_json::json!({"health":row.get::<String,_>(0),"last_success":row.get::<Option<i64>,_>(1),"coverage_start":row.get::<Option<String>,_>(2),"coverage_end":row.get::<Option<String>,_>(3),"timezone":row.get::<String,_>(4)}).to_string()).await
    }
    pub async fn begin_calendar_refresh(&self, actor: &str, id: &str, now: i64) -> Result<Refresh> {
        let mut tx = self.begin_serial().await?;
        let p = Self::task_resource(&mut tx, actor, id, "calendar_source", true).await?;
        ensure!(!p.archived, ErrorCode::InvalidValue);
        let row=sqlx::query("SELECT generation,lease_until,timezone,connection,etag,modified FROM calendar_sources WHERE id=$1").bind(id).fetch_one(&mut *tx).await?;
        ensure!(row.get::<i64, _>(1) <= now, ErrorCode::RefreshInProgress);
        let generation = row.get::<i64, _>(0) + 1;
        sqlx::query("UPDATE calendar_sources SET generation=$1,lease_until=$2 WHERE id=$3")
            .bind(generation)
            .bind(now + 60)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // Only the owner/worker may read the encrypted connection; other editors can import files.
        let owner: String = sqlx::query_scalar("SELECT owner_id FROM resources WHERE id=$1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        let result = Refresh {
            generation,
            timezone: row.get(2),
            connection: if owner == actor { row.get(3) } else { None },
            etag: row.get(4),
            modified: row.get(5),
        };
        tx.commit().await?;
        Ok(result)
    }
    #[allow(clippy::too_many_arguments)] // Fenced import result and provider validators.
    pub async fn finish_calendar_refresh(
        &self,
        actor: &str,
        id: &str,
        generation: i64,
        feed: Option<&Feed>,
        validators: (Option<&str>, Option<&str>),
        failure: Option<&str>,
        now: i64,
    ) -> Result<i64> {
        self.finish_calendar_refresh_receipt(
            actor, id, generation, feed, validators, failure, now, None,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn finish_calendar_refresh_receipt(
        &self,
        actor: &str,
        id: &str,
        generation: i64,
        feed: Option<&Feed>,
        validators: (Option<&str>, Option<&str>),
        failure: Option<&str>,
        now: i64,
        receipt: Option<(&str, &str)>,
    ) -> Result<i64> {
        let mut tx = self.begin_serial().await?;
        if let Some((operation, hash)) = receipt {
            identifier(operation)?;
            if let Some(row) = sqlx::query(
                "SELECT payload,revision FROM receipts WHERE account_id=$1 AND operation_id=$2",
            )
            .bind(actor)
            .bind(operation)
            .fetch_optional(&mut *tx)
            .await?
            {
                ensure!(
                    row.get::<String, _>(0) == hash,
                    ErrorCode::OperationConflict
                );
                return Ok(row.get(1));
            }
        }
        let source = Self::task_resource(&mut tx, actor, id, "calendar_source", true).await?;
        ensure!(!source.archived, ErrorCode::InvalidValue);
        let row = sqlx::query(
            "SELECT generation,committed_generation,lease_until FROM calendar_sources WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(
            row.get::<i64, _>(0) == generation && row.get::<i64, _>(2) > now,
            ErrorCode::StaleRefresh
        );
        ensure!(
            failure.is_none_or(|v| matches!(
                v,
                "fetch_failed"
                    | "invalid_ics"
                    | "unsupported_calendar_rule"
                    | "unsupported_calendar_timezone"
                    | "calendar_limit"
            )),
            ErrorCode::InvalidValue
        );
        let mut touched = Self::task_touched(&mut tx, id).await?;
        // Anchored task updates are captured in the same publication transaction.
        let tasks: Vec<String> =
            sqlx::query_scalar("SELECT task_id FROM task_anchors WHERE active=1")
                .fetch_all(&mut *tx)
                .await?;
        for task in &tasks {
            touched.extend(Self::task_touched(&mut tx, task).await?);
        }
        let before = Self::calendar_before(&mut tx, &touched).await?;
        if let Some(feed) = feed {
            ensure!(
                failure.is_none() && feed.events.len() <= 2048,
                ErrorCode::InvalidValue
            );
            let existing = sqlx::query("SELECT id,data FROM calendar_events WHERE source_id=$1")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
            let mut seen = BTreeSet::new();
            let policy = Self::copied_policy(&mut tx, id).await?;
            let owner: String = sqlx::query_scalar("SELECT owner_id FROM resources WHERE id=$1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
            for event in &feed.events {
                let event_id = Uuid::new_v5(
                    &Uuid::parse_str(id)?,
                    format!(
                        "atlas-event-v1:{}:{}:{}",
                        event.uid.len(),
                        event.uid,
                        event.original
                    )
                    .as_bytes(),
                )
                .to_string();
                seen.insert(event_id.clone());
                let old: Option<String> =
                    sqlx::query_scalar("SELECT data FROM calendar_events WHERE id=$1")
                        .bind(&event_id)
                        .fetch_optional(&mut *tx)
                        .await?;
                if let Some(old) = old {
                    let previous: Event = serde_json::from_str(&old)?;
                    if event.sequence < previous.sequence {
                        continue;
                    }
                    if previous == *event {
                        continue;
                    }
                    Self::value(
                        &mut tx,
                        &event_id,
                        &event.title,
                        &serde_json::to_string(event)?,
                    )
                    .await?;
                    sqlx::query("UPDATE calendar_events SET data=$1,generation=$2 WHERE id=$3")
                        .bind(serde_json::to_string(event)?)
                        .bind(generation)
                        .bind(&event_id)
                        .execute(&mut *tx)
                        .await?;
                } else {
                    Self::new_resource(
                        &mut tx,
                        &owner,
                        &event_id,
                        Some(id),
                        "event",
                        &event.title,
                        &serde_json::to_string(event)?,
                        &policy,
                    )
                    .await?;
                    sqlx::query("INSERT INTO calendar_events(id,source_id,uid,original,data,generation) VALUES ($1,$2,$3,$4,$5,$6)").bind(&event_id).bind(id).bind(&event.uid).bind(&event.original).bind(serde_json::to_string(event)?).bind(generation).execute(&mut *tx).await?;
                }
                touched.insert(event_id);
            }
            for old in existing {
                let event_id: String = old.get(0);
                let mut event: Event = serde_json::from_str(&old.get::<String, _>(1))?;
                let cancellation = feed
                    .cancelled_series
                    .get(&event.uid)
                    .into_iter()
                    .chain(
                        feed.cancelled_instances
                            .get(&(event.uid.clone(), event.original.clone())),
                    )
                    .max()
                    .copied();
                if let Some(sequence) = cancellation {
                    if sequence < event.sequence {
                        continue;
                    }
                    event.status = EventStatus::Cancelled;
                    event.sequence = sequence;
                } else if !feed.cancellations_only
                    && !seen.contains(&event_id)
                    && event.start.date >= feed.from
                    && event.start.date <= feed.through
                    && event.status != EventStatus::Cancelled
                {
                    event.status = EventStatus::Missing;
                } else {
                    continue;
                }
                let data = serde_json::to_string(&event)?;
                if data == old.get::<String, _>(1) {
                    continue;
                }
                Self::value(&mut tx, &event_id, &event.title, &data).await?;
                sqlx::query("UPDATE calendar_events SET data=$1,generation=$2 WHERE id=$3")
                    .bind(data)
                    .bind(generation)
                    .bind(&event_id)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query("UPDATE calendar_sources SET coverage_start=$1,coverage_end=$2,committed_generation=$3 WHERE id=$4").bind(&feed.from).bind(&feed.through).bind(generation).bind(id).execute(&mut *tx).await?;
        }
        if failure.is_none() {
            sqlx::query("UPDATE calendar_sources SET health='healthy',last_success=$1,etag=$2,modified=$3 WHERE id=$4").bind(now).bind(validators.0).bind(validators.1).bind(id).execute(&mut *tx).await?;
        } else {
            sqlx::query("UPDATE calendar_sources SET health=$1 WHERE id=$2")
                .bind(failure)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE calendar_sources SET lease_until=0,next_refresh=$1 WHERE id=$2")
            .bind(now + 900)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        Self::source_value(&mut tx, id).await?;
        for task in tasks {
            Self::reconcile_task_anchor(&mut tx, &task, now, &mut touched).await?;
        }
        touched.extend(Self::task_touched(&mut tx, id).await?);
        let revision = Self::calendar_publish(&mut tx, &touched, before).await?;
        if let Some((operation, hash)) = receipt {
            sqlx::query("INSERT INTO receipts(account_id,operation_id,payload,revision,digest_version) VALUES ($1,$2,$3,$4,1)").bind(actor).bind(operation).bind(hash).bind(revision).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(revision)
    }
    pub async fn calendar_resources(
        &self,
        actor: &str,
        kind: &str,
        parent: Option<&str>,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<Projection>> {
        ensure!(
            matches!(kind, "calendar_source" | "event" | "review" | "reminder")
                && (1..=200).contains(&limit),
            ErrorCode::InvalidValue
        );
        let mut tx = self.pool.begin().await?;
        if !self.sqlite {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
        }
        Self::epoch(&mut tx, actor).await?;
        let ids:Vec<String>=sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT r.id FROM resources r LEFT JOIN resource_grants g ON g.resource_id=r.id AND g.account_id=$1 WHERE r.kind=$2 AND ($3 IS NULL OR r.parent_id=$3) AND ($4 IS NULL OR r.id>$4) AND {} ORDER BY r.id LIMIT $5",crate::policy::VISIBLE))).bind(actor).bind(kind).bind(parent).bind(after).bind(i64::from(limit)).fetch_all(&mut *tx).await?;
        let values = Self::subset(&mut tx, actor, &ids.into_iter().collect())
            .await?
            .into_values()
            .collect();
        tx.commit().await?;
        Ok(values)
    }
}
