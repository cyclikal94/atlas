use crate::error::ErrorCode;
use crate::{
    Store, identifier,
    policy::Policy,
    receipt_digest,
    tasks::{OccurrenceData, Outcome},
};
use anyhow::{Result, anyhow, ensure};
use chrono::{Duration, NaiveDate, NaiveTime, Timelike};
use serde::{Deserialize, Serialize};
use sqlx::{Any, Row, Transaction};
use std::collections::BTreeSet;
use uuid::Uuid;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryMode {
    Server,
    Device { device_id: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReminderRule {
    pub offset_seconds: i64,
    pub time: Option<String>,
    pub late_seconds: u32,
    pub enabled: bool,
    pub delivery: DeliveryMode,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReminderCommand {
    SetRule {
        id: String,
        occurrence_id: String,
        expected_version: Option<i64>,
        rule: ReminderRule,
    },
    SetSubscription {
        id: String,
        expected_version: i64,
        device_id: String,
        transport: String,
        secret: String,
        enabled: bool,
    },
    RemoveSubscription {
        id: String,
        expected_version: i64,
    },
}
#[derive(Clone, Serialize)]
pub struct Notification {
    pub id: String,
    pub reminder_id: String,
    pub occurrence_id: String,
}
// This type is worker-only: encrypted connection material never enters sync.
pub struct Delivery {
    pub id: String,
    pub token: String,
    pub transport: String,
    pub secret: String,
    pub notification: Notification,
    pub expires_at: i64,
}
fn due(data: &OccurrenceData, rule: &ReminderRule) -> Result<i64> {
    ensure!(
        (-31622400..=31622400).contains(&rule.offset_seconds) && rule.late_seconds <= 604800,
        ErrorCode::InvalidValue
    );
    let instant = if let Some(time) = &rule.time {
        ensure!(time.len() == 8, ErrorCode::InvalidValue);
        let clock = NaiveTime::parse_from_str(time, "%H:%M:%S")
            .map_err(|_| anyhow!(ErrorCode::InvalidValue))?;
        ensure!(clock.nanosecond() == 0, ErrorCode::InvalidValue);
        let date = NaiveDate::parse_from_str(
            data.slot
                .date
                .as_deref()
                .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?,
            "%Y-%m-%d",
        )?;
        crate::tasks::resolve(
            data.slot
                .timezone
                .parse()
                .map_err(|_| anyhow!(ErrorCode::InvalidValue))?,
            date.and_time(clock),
        )?
    } else {
        data.slot
            .instant
            .ok_or_else(|| anyhow!(ErrorCode::ReminderTimeRequired))?
    };
    instant
        .checked_add(rule.offset_seconds)
        .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))
}
impl Store {
    pub async fn reminder_command(
        &self,
        actor: &str,
        operation: &str,
        command: &ReminderCommand,
    ) -> Result<i64> {
        identifier(operation)?;
        let mut tx = self.begin_serial().await?;
        Self::epoch(&mut tx, actor).await?;
        let mut receipt_command = command.clone();
        if let ReminderCommand::SetSubscription { secret, .. } = &mut receipt_command {
            *secret = super::secret_identity(secret);
        }
        let hash = receipt_digest(&format!(
            "reminder-command-v1:{}",
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
        let mut touched = BTreeSet::new();
        if let ReminderCommand::SetRule { id, .. } = command {
            touched.insert(id.clone());
        }
        let before = Self::calendar_before(&mut tx, &touched).await?;
        match command {
            ReminderCommand::SetRule {
                id,
                occurrence_id,
                expected_version,
                rule,
            } => {
                identifier(id)?;
                let occurrence =
                    Self::task_resource(&mut tx, actor, occurrence_id, "occurrence", false).await?;
                let data: OccurrenceData = serde_json::from_value(occurrence.value.clone())?;
                let instant = due(&data, rule)?;
                if let DeliveryMode::Device { device_id } = &rule.delivery {
                    ensure!(
                        !device_id.is_empty() && device_id.len() <= 100,
                        ErrorCode::InvalidValue
                    );
                }
                let value=serde_json::json!({"rule":rule,"due_at":instant,"occurrence_version":occurrence.version}).to_string();
                if let Some(version) = expected_version {
                    let p = Self::task_resource(&mut tx, actor, id, "reminder", true).await?;
                    ensure!(
                        p.version == *version && p.parent_id.as_deref() == Some(occurrence_id),
                        ErrorCode::Conflict
                    );
                    let owner: String =
                        sqlx::query_scalar("SELECT owner_id FROM reminder_rules WHERE id=$1")
                            .bind(id)
                            .fetch_one(&mut *tx)
                            .await?;
                    ensure!(owner == actor, ErrorCode::Forbidden);
                    Self::value(&mut tx, id, "Reminder", &value).await?;
                    sqlx::query("UPDATE reminder_rules SET data=$1 WHERE id=$2")
                        .bind(serde_json::to_string(rule)?)
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                } else {
                    let count: i64 =
                        sqlx::query_scalar("SELECT COUNT(*) FROM reminder_rules WHERE owner_id=$1")
                            .bind(actor)
                            .fetch_one(&mut *tx)
                            .await?;
                    ensure!(count < 1000, ErrorCode::SliceCapacity);
                    Self::new_resource(
                        &mut tx,
                        actor,
                        id,
                        Some(occurrence_id),
                        "reminder",
                        "Reminder",
                        &value,
                        &Policy::default(),
                    )
                    .await?;
                    sqlx::query("INSERT INTO reminder_rules VALUES ($1,$2,$3,$4)")
                        .bind(id)
                        .bind(occurrence_id)
                        .bind(actor)
                        .bind(serde_json::to_string(rule)?)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            ReminderCommand::SetSubscription {
                id,
                expected_version,
                device_id,
                transport,
                secret,
                enabled,
            } => {
                identifier(id)?;
                ensure!(
                    matches!(transport.as_str(), "web_push" | "ntfy")
                        && secret.len() <= 16384
                        && !device_id.is_empty()
                        && device_id.len() <= 100,
                    ErrorCode::InvalidValue
                );
                if *enabled {
                    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notification_subscriptions WHERE account_id=$1 AND active=1 AND id<>$2").bind(actor).bind(id).fetch_one(&mut *tx).await?;
                    ensure!(count < 16, ErrorCode::SliceCapacity);
                }
                let existing = sqlx::query(
                    "SELECT account_id,version FROM notification_subscriptions WHERE id=$1",
                )
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
                if let Some(row) = existing {
                    ensure!(row.get::<String, _>(0) == actor, ErrorCode::NotFound);
                    ensure!(
                        row.get::<i64, _>(1) == *expected_version,
                        ErrorCode::Conflict
                    );
                    sqlx::query("UPDATE notification_subscriptions SET device_id=$1,transport=$2,secret=$3,active=$4,version=version+1 WHERE id=$5").bind(device_id).bind(transport).bind(secret).bind(i64::from(*enabled)).bind(id).execute(&mut *tx).await?;
                } else {
                    ensure!(*expected_version == 0, ErrorCode::Conflict);

                    sqlx::query("INSERT INTO notification_subscriptions(id,account_id,device_id,transport,secret,active) VALUES ($1,$2,$3,$4,$5,$6)").bind(id).bind(actor).bind(device_id).bind(transport).bind(secret).bind(i64::from(*enabled)).execute(&mut *tx).await?;
                }
            }
            ReminderCommand::RemoveSubscription {
                id,
                expected_version,
            } => {
                let n=sqlx::query("UPDATE notification_subscriptions SET active=0,secret='',version=version+1 WHERE id=$1 AND account_id=$2 AND version=$3").bind(id).bind(actor).bind(expected_version).execute(&mut *tx).await?.rows_affected();
                ensure!(n == 1, ErrorCode::Conflict);
            }
        }
        let revision = Self::calendar_publish(&mut tx, &touched, before).await?;
        sqlx::query("INSERT INTO receipts(account_id,operation_id,payload,revision,digest_version) VALUES ($1,$2,$3,$4,1)").bind(actor).bind(operation).bind(hash).bind(revision).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(revision)
    }
    pub async fn notification_subscriptions(&self, actor: &str) -> Result<serde_json::Value> {
        let rows=sqlx::query("SELECT id,device_id,transport,version,active FROM notification_subscriptions WHERE account_id=$1 ORDER BY id").bind(actor).fetch_all(&self.pool).await?;
        Ok(
            serde_json::json!({"subscriptions":rows.into_iter().map(|r|serde_json::json!({"id":r.get::<String,_>(0),"device_id":r.get::<String,_>(1),"transport":r.get::<String,_>(2),"version":r.get::<i64,_>(3),"enabled":r.get::<i64,_>(4)==1})).collect::<Vec<_>>()}),
        )
    }
    async fn reminder_archived(tx: &mut Transaction<'_, Any>, occurrence: &str) -> Result<bool> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resources WHERE archived=1 AND (id=$1 OR id IN (SELECT ancestor_id FROM resource_ancestors WHERE resource_id=$1))")
            .bind(occurrence).fetch_one(&mut **tx).await?;
        Ok(count > 0)
    }
    pub async fn schedule_reminders(&self, now: i64) -> Result<()> {
        let mut tx = self.begin_serial().await?;
        let rules =
            sqlx::query("SELECT id,occurrence_id,owner_id,data FROM reminder_rules ORDER BY id")
                .fetch_all(&mut *tx)
                .await?;
        let touched = rules
            .iter()
            .map(|r| r.get::<String, _>(0))
            .collect::<BTreeSet<_>>();
        let before = Self::calendar_before(&mut tx, &touched).await?;
        for row in rules {
            let id: String = row.get(0);
            let occurrence: String = row.get(1);
            let owner: String = row.get(2);
            let rule: ReminderRule = serde_json::from_str(&row.get::<String, _>(3))?;
            let p = match Self::task_resource(&mut tx, &owner, &occurrence, "occurrence", false)
                .await
            {
                Ok(v) => Some(v),
                Err(e) if e.to_string() == "not_found" => None,
                Err(e) => return Err(e),
            };
            let mut active = rule.enabled && matches!(rule.delivery, DeliveryMode::Server);
            let mut at = 0;
            let mut occurrence_version = 0;
            if let Some(p) = p {
                let data: OccurrenceData = serde_json::from_value(p.value.clone())?;
                at = due(&data, &rule)?;
                occurrence_version = p.version;
                active &= !Self::reminder_archived(&mut tx, &occurrence).await?
                    && !data.resolved
                    && data.covered_by.is_none()
                    && Self::outcome_for(&mut tx, &owner, &occurrence, &data, now, false).await?
                        != Outcome::Complete;
                let desired =
                    serde_json::json!({"rule":rule,"due_at":at,"occurrence_version":p.version})
                        .to_string();
                let old: String = sqlx::query_scalar("SELECT value FROM resources WHERE id=$1")
                    .bind(&id)
                    .fetch_one(&mut *tx)
                    .await?;
                if old != desired {
                    Self::value(&mut tx, &id, "Reminder", &desired).await?;
                }
            } else {
                active = false;
            }
            let rv: i64 = sqlx::query_scalar("SELECT version FROM resources WHERE id=$1")
                .bind(&id)
                .fetch_one(&mut *tx)
                .await?;
            let subscriptions = sqlx::query(
                "SELECT id,version,active FROM notification_subscriptions WHERE account_id=$1",
            )
            .bind(&owner)
            .fetch_all(&mut *tx)
            .await?;
            for subscription in subscriptions {
                let sub: String = subscription.get(0);
                let revision = receipt_digest(&format!(
                    "{rv}:{occurrence_version}:{}",
                    subscription.get::<i64, _>(1)
                ));
                sqlx::query("UPDATE reminder_deliveries SET state='cancelled',lease_token=NULL WHERE reminder_id=$1 AND subscription_id=$2 AND state IN ('pending','sending') AND (revision<>$3 OR $4=0)").bind(&id).bind(&sub).bind(&revision).bind(i64::from(active&&subscription.get::<i64,_>(2)==1)).execute(&mut *tx).await?;
                if !active || subscription.get::<i64, _>(2) != 1 {
                    continue;
                }
                let expires = at
                    .checked_add(i64::from(rule.late_seconds))
                    .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
                let delivery_id = Uuid::new_v5(
                    &Uuid::parse_str(&id)?,
                    format!("atlas-delivery-v1:{sub}:{revision}").as_bytes(),
                )
                .to_string();
                sqlx::query("INSERT INTO reminder_deliveries(id,reminder_id,subscription_id,revision,due_at,expires_at,next_attempt,state) VALUES ($1,$2,$3,$4,$5,$6,$5,$7) ON CONFLICT DO NOTHING").bind(delivery_id).bind(&id).bind(sub).bind(revision).bind(at).bind(expires).bind(if expires<now{"expired"}else{"pending"}).execute(&mut *tx).await?;
            }
        }
        if !touched.is_empty() {
            Self::calendar_publish(&mut tx, &touched, before).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn claim_reminder_delivery(&self, now: i64) -> Result<Option<Delivery>> {
        let mut tx = self.begin_serial().await?;
        sqlx::query("UPDATE reminder_deliveries SET state='expired',lease_token=NULL WHERE state IN ('pending','sending') AND expires_at<$1").bind(now).execute(&mut *tx).await?;
        sqlx::query("UPDATE reminder_deliveries SET state='failed',lease_token=NULL WHERE state='sending' AND lease_until<=$1 AND attempts>=8").bind(now).execute(&mut *tx).await?;
        let ids:Vec<String>=sqlx::query_scalar("SELECT id FROM reminder_deliveries WHERE (state='pending' OR (state='sending' AND lease_until<=$1)) AND next_attempt<=$1 AND attempts<8 ORDER BY next_attempt,id LIMIT 16").bind(now).fetch_all(&mut *tx).await?;
        for id in ids {
            let token = Uuid::new_v4().to_string();
            sqlx::query("UPDATE reminder_deliveries SET state='sending',lease_token=$1,lease_until=$2,attempts=attempts+1 WHERE id=$3").bind(&token).bind(now+60).bind(&id).execute(&mut *tx).await?;
            if let Some(delivery) = Self::delivery_in(&mut tx, &id, &token, now).await? {
                tx.commit().await?;
                return Ok(Some(delivery));
            }
            sqlx::query(
                "UPDATE reminder_deliveries SET state='cancelled',lease_token=NULL WHERE id=$1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(None)
    }
    async fn delivery_in(
        tx: &mut Transaction<'_, Any>,
        id: &str,
        token: &str,
        now: i64,
    ) -> Result<Option<Delivery>> {
        let row=sqlx::query("SELECT d.reminder_id,d.revision,d.expires_at,r.owner_id,r.occurrence_id,r.data,s.transport,s.secret,s.version,p.version FROM reminder_deliveries d JOIN reminder_rules r ON r.id=d.reminder_id JOIN resources p ON p.id=r.id JOIN notification_subscriptions s ON s.id=d.subscription_id WHERE d.id=$1 AND d.lease_token=$2 AND d.state='sending' AND d.lease_until>$3 AND d.expires_at>=$3 AND s.active=1").bind(id).bind(token).bind(now).fetch_optional(&mut **tx).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let owner: String = row.get(3);
        let occurrence: String = row.get(4);
        let rule: ReminderRule = serde_json::from_str(&row.get::<String, _>(5))?;
        if !rule.enabled || !matches!(rule.delivery, DeliveryMode::Server) {
            return Ok(None);
        }
        let p = match Self::task_resource(tx, &owner, &occurrence, "occurrence", false).await {
            Ok(p) => p,
            Err(e) if e.to_string() == "not_found" => return Ok(None),
            Err(e) => return Err(e),
        };
        let revision = receipt_digest(&format!(
            "{}:{}:{}",
            row.get::<i64, _>(9),
            p.version,
            row.get::<i64, _>(8)
        ));
        if revision != row.get::<String, _>(1) {
            return Ok(None);
        }
        let data: OccurrenceData = serde_json::from_value(p.value.clone())?;
        if Self::reminder_archived(tx, &occurrence).await?
            || data.resolved
            || data.covered_by.is_some()
            || Self::outcome_for(tx, &owner, &occurrence, &data, now, false).await?
                == Outcome::Complete
        {
            return Ok(None);
        }
        Ok(Some(Delivery {
            id: id.into(),
            token: token.into(),
            transport: row.get(6),
            secret: row.get(7),
            notification: Notification {
                id: id.into(),
                reminder_id: row.get(0),
                occurrence_id: occurrence,
            },
            expires_at: row.get(2),
        }))
    }
    pub async fn validate_reminder_delivery(
        &self,
        id: &str,
        token: &str,
        now: i64,
    ) -> Result<Option<Delivery>> {
        let mut tx = self.begin_serial().await?;
        let result = Self::delivery_in(&mut tx, id, token, now).await?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn finish_reminder_delivery(
        &self,
        id: &str,
        token: &str,
        success: bool,
        permanent: bool,
        now: i64,
    ) -> Result<()> {
        let mut tx = self.begin_serial().await?;
        let row=sqlx::query("SELECT attempts FROM reminder_deliveries WHERE id=$1 AND lease_token=$2 AND state='sending' AND lease_until>$3").bind(id).bind(token).bind(now).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            return Err(anyhow!(ErrorCode::StaleDelivery));
        };
        let attempts: i64 = row.get(0);
        let state = if success {
            "delivered"
        } else if permanent || attempts >= 8 {
            "failed"
        } else {
            "pending"
        };
        let wait = Duration::seconds((15_i64 * 2_i64.pow(attempts.min(8) as u32)).min(3600));
        sqlx::query("UPDATE reminder_deliveries SET state=$1,lease_token=NULL,lease_until=0,next_attempt=$2 WHERE id=$3").bind(state).bind(now+wait.num_seconds()).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn reminder_delivery_history(
        &self,
        actor: &str,
        after: Option<&str>,
        limit: u16,
    ) -> Result<serde_json::Value> {
        ensure!((1..=200).contains(&limit), ErrorCode::InvalidValue);
        let mut tx = self.pool.begin().await?;
        let rows=sqlx::query(sqlx::AssertSqlSafe(format!("SELECT d.id,d.reminder_id,d.state,d.attempts,d.due_at FROM reminder_deliveries d JOIN reminder_rules rr ON rr.id=d.reminder_id JOIN resources r ON r.id=rr.id LEFT JOIN resource_grants g ON g.resource_id=r.id AND g.account_id=$1 WHERE rr.owner_id=$1 AND ($2 IS NULL OR d.id>$2) AND {} ORDER BY d.id LIMIT $3",crate::policy::VISIBLE))).bind(actor).bind(after).bind(i64::from(limit)).fetch_all(&mut *tx).await?;
        let mut items = Vec::new();
        for r in rows {
            let reminder: String = r.get(1);
            items.push(serde_json::json!({"id":r.get::<String,_>(0),"reminder_id":reminder,"state":r.get::<String,_>(2),"attempts":r.get::<i64,_>(3),"due_at":r.get::<i64,_>(4)}));
        }
        let next_after = if items.len() == usize::from(limit) {
            items.last().map(|v| v["id"].clone())
        } else {
            None
        };
        Ok(serde_json::json!({"items":items,"next_after":next_after}))
    }
}
