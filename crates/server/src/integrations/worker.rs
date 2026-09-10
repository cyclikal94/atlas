use super::{IntegrationConfig, Link, Subscription};
use crate::*;
use atlas_core::calendars::ics;
use atlas_core::error::ErrorCode;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Import {
    ics: String,
    from: String,
    through: String,
}
impl App {
    pub fn integration_config(mut self, config: IntegrationConfig) -> Self {
        self.integrations = config;
        self
    }
    async fn calendar_receipt(
        &self,
        actor: &str,
        operation: &str,
        hash: &str,
    ) -> Result<Option<i64>> {
        let row = sqlx::query(
            "SELECT payload,revision FROM receipts WHERE account_id=$1 AND operation_id=$2",
        )
        .bind(actor)
        .bind(operation)
        .fetch_optional(&self.store.pool)
        .await?;
        if let Some(row) = row {
            ensure!(
                row.get::<String, _>(0) == hash,
                ErrorCode::OperationConflict
            );
            Ok(Some(row.get(1)))
        } else {
            Ok(None)
        }
    }
    pub(crate) async fn import_text(
        &self,
        actor: &str,
        id: &str,
        operation: &str,
        input: Import,
    ) -> Result<i64> {
        let _permit = self
            .calendar_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow!(ErrorCode::TemporarilyUnavailable))?;
        ensure!(input.ics.len() <= 1024 * 1024, ErrorCode::CalendarLimit);
        let hash = digest(&format!(
            "calendar-import-v1:{}",
            serde_json::to_string(&(id, &input.from, &input.through, &input.ics))?
        ));
        if let Some(revision) = self.calendar_receipt(actor, operation, &hash).await? {
            return Ok(revision);
        }
        let refresh = self.store.begin_calendar_refresh(actor, id, now()).await?;
        let zone = refresh.timezone;
        let parsed = tokio::task::spawn_blocking(move || {
            ics::parse(&input.ics, &zone, &input.from, &input.through)
        })
        .await?;
        match parsed {
            Ok(feed) => {
                self.store
                    .finish_calendar_refresh_receipt(
                        actor,
                        id,
                        refresh.generation,
                        Some(&feed),
                        (None, None),
                        None,
                        now(),
                        Some((operation, &hash)),
                    )
                    .await
            }
            Err(error) => {
                let reason = calendar_error(&error);
                self.store
                    .finish_calendar_refresh(
                        actor,
                        id,
                        refresh.generation,
                        None,
                        (None, None),
                        Some(reason.as_str()),
                        now(),
                    )
                    .await?;
                Err(anyhow!(reason))
            }
        }
    }
    pub(crate) async fn refresh_link(
        &self,
        actor: &str,
        id: &str,
        operation: Option<&str>,
    ) -> Result<i64> {
        let _permit = self
            .calendar_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow!(ErrorCode::TemporarilyUnavailable))?;
        let hash = digest(&format!("calendar-refresh-v1:{id}"));
        if let Some(operation) = operation
            && let Some(revision) = self.calendar_receipt(actor, operation, &hash).await?
        {
            return Ok(revision);
        }
        let refresh = self.store.begin_calendar_refresh(actor, id, now()).await?;
        let result = async {
            let sealed = refresh
                .connection
                .as_deref()
                .ok_or_else(|| anyhow!(ErrorCode::SourceConnectionRequired))?;
            let link: Link =
                serde_json::from_str(&self.integrations.open(&format!("source:{id}"), sealed)?)?;
            let (text, etag, modified) = self
                .integrations
                .fetch(&link, refresh.etag.as_deref(), refresh.modified.as_deref())
                .await?;
            let feed = if let Some(text) = text {
                let zone = refresh.timezone;
                let date = chrono::Utc::now().date_naive();
                Some(
                    tokio::task::spawn_blocking(move || {
                        ics::parse(
                            &text,
                            &zone,
                            &(date - chrono::Duration::days(30)).to_string(),
                            &(date + chrono::Duration::days(400)).to_string(),
                        )
                    })
                    .await??,
                )
            } else {
                None
            };
            self.store
                .finish_calendar_refresh_receipt(
                    actor,
                    id,
                    refresh.generation,
                    feed.as_ref(),
                    (etag.as_deref(), modified.as_deref()),
                    None,
                    now(),
                    operation.map(|v| (v, hash.as_str())),
                )
                .await
        }
        .await;
        if let Err(error) = result {
            let reason = calendar_error(&error);
            let _ = self
                .store
                .finish_calendar_refresh(
                    actor,
                    id,
                    refresh.generation,
                    None,
                    (None, None),
                    Some(reason.as_str()),
                    now(),
                )
                .await;
            return Err(anyhow!(reason));
        }
        result
    }
    pub async fn integration_tick(&self) -> Result<()> {
        let rows=sqlx::query("SELECT s.id,r.owner_id FROM calendar_sources s JOIN resources r ON r.id=s.id WHERE s.connection IS NOT NULL AND r.archived=0 AND s.next_refresh<=$1 AND s.lease_until<=$1 ORDER BY s.next_refresh,s.id LIMIT 4").bind(now()).fetch_all(&self.store.pool).await?;
        for row in rows {
            let id: String = row.get(0);
            let actor: String = row.get(1);
            if self.refresh_link(&actor, &id, None).await.is_err() {
                eprintln!(
                    "{}",
                    json!({"event":"integration_error","operation":"calendar_refresh"})
                );
            }
        }
        self.store.reconcile_completion_tasks(now()).await?;
        self.store.reconcile_calendar_tasks(now()).await?;
        self.store.schedule_reminders(now()).await?;
        for _ in 0..8 {
            let Some(delivery) = self.store.claim_reminder_delivery(now()).await? else {
                break;
            };
            let result = async {
                let subscription: Subscription = serde_json::from_str(&self.integrations.open(
                    &format!("subscription:{}", self.subscription_id(&delivery.id).await?),
                    &delivery.secret,
                )?)?;
                let Some(current) = self
                    .store
                    .validate_reminder_delivery(&delivery.id, &delivery.token, now())
                    .await?
                else {
                    return Ok((false, true));
                };
                self.integrations
                    .deliver(&subscription, &current.notification, current.expires_at)
                    .await
            }
            .await;
            let (success, permanent) = result.unwrap_or((false, false));
            let _ = self
                .store
                .finish_reminder_delivery(&delivery.id, &delivery.token, success, permanent, now())
                .await;
        }
        Ok(())
    }
    async fn subscription_id(&self, id: &str) -> Result<String> {
        Ok(
            sqlx::query_scalar("SELECT subscription_id FROM reminder_deliveries WHERE id=$1")
                .bind(id)
                .fetch_one(&self.store.pool)
                .await?,
        )
    }
}
fn calendar_error(error: &anyhow::Error) -> ErrorCode {
    match error.downcast_ref::<ErrorCode>().copied() {
        Some(
            code @ (ErrorCode::UnsupportedCalendarTimezone
            | ErrorCode::UnsupportedCalendarRule
            | ErrorCode::CalendarLimit
            | ErrorCode::InvalidIcs),
        ) => code,
        _ => ErrorCode::FetchFailed,
    }
}
