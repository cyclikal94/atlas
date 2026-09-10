use super::*;
use atlas_core::calendars::{CalendarCommand, ReminderCommand};
use atlas_core::error::ErrorCode;
use integrations::{Link, Subscription};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    command: serde_json::Value,
}
fn operation(headers: &HeaderMap) -> Result<String, ApiError> {
    let value = headers
        .get("idempotency-key")
        .and_then(|s| s.to_str().ok())
        .ok_or_else(|| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(Uuid::parse_str(value).is_ok(), ErrorCode::InvalidValue)?;
    Ok(value.into())
}
pub(super) async fn commands(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Input>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Json(mut input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    let op = operation(&headers)?;
    if matches!(
        input.command["kind"].as_str(),
        Some("create_source" | "configure_source")
    ) && !input.command["connection"].is_null()
    {
        let id = input.command["id"]
            .as_str()
            .ok_or_else(|| anyhow!(ErrorCode::MalformedRequest))?;
        let connection: Link = serde_json::from_value(input.command["connection"].clone())
            .map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
        let _permit = app
            .calendar_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow!(ErrorCode::TemporarilyUnavailable))?;
        ensure_api(
            connection.bearer.as_ref().is_none_or(|v| v.len() <= 4096),
            ErrorCode::InvalidValue,
        )?;
        app.integrations.client(&connection.url).await?;
        input.command["connection"] = app
            .integrations
            .seal(
                &format!("source:{id}"),
                &serde_json::to_string(&connection)?,
            )?
            .into();
    }
    let command: CalendarCommand =
        serde_json::from_value(input.command).map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    Ok(Json(
        json!({"revision":app.store.calendar_command(&actor,&op,&command).await?}),
    ))
}
pub(crate) use crate::integrations::worker::Import;
pub(super) async fn import(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<Import>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    let op = operation(&headers)?;
    Ok(Json(
        json!({"revision":app.import_text(&actor,&id,&op,input).await?}),
    ))
}
pub(super) async fn refresh(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let op = operation(&headers)?;
    Ok(Json(
        json!({"revision":app.refresh_link(&actor,&id,Some(&op)).await?}),
    ))
}
pub(super) async fn anchor(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    Ok(Json(
        json!({"binding":app.store.task_anchor(&actor,&id).await?}),
    ))
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct Filter {
    parent_id: Option<String>,
    after: Option<String>,
    limit: Option<u16>,
}
async fn resources(
    app: App,
    headers: HeaderMap,
    query: Result<Query<Filter>, QueryRejection>,
    kind: &str,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Query(filter) = query.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    let limit = filter.limit.unwrap_or(50);
    let items = app
        .store
        .calendar_resources(
            &actor,
            kind,
            filter.parent_id.as_deref(),
            filter.after.as_deref(),
            limit,
        )
        .await?;
    let after = if items.len() == usize::from(limit) {
        items.last().map(|v| v.id.clone())
    } else {
        None
    };
    Ok(Json(json!({"items":items,"next_after":after})))
}
pub(super) async fn sources(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<Filter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    resources(app, headers, query, "calendar_source").await
}
pub(super) async fn events(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<Filter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    resources(app, headers, query, "event").await
}
pub(super) async fn reviews(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<Filter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    resources(app, headers, query, "review").await
}
pub(super) async fn reminders(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<Filter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    resources(app, headers, query, "reminder").await
}
pub(super) async fn reminder_commands(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Input>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Json(mut input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    let op = operation(&headers)?;
    if input.command["kind"] == "set_subscription" {
        let id = input.command["id"]
            .as_str()
            .ok_or_else(|| anyhow!(ErrorCode::MalformedRequest))?;
        let subscription: Subscription = serde_json::from_value(input.command["secret"].clone())
            .map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
        let _permit = app
            .calendar_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow!(ErrorCode::TemporarilyUnavailable))?;
        app.integrations
            .validate_subscription(&subscription)
            .await?;
        let transport = match subscription {
            Subscription::Ntfy { .. } => "ntfy",
            Subscription::WebPush { .. } => "web_push",
        };
        ensure_api(
            input.command["transport"] == transport,
            ErrorCode::InvalidValue,
        )?;
        input.command["secret"] = app
            .integrations
            .seal(
                &format!("subscription:{id}"),
                &serde_json::to_string(&subscription)?,
            )?
            .into();
    }
    let command: ReminderCommand =
        serde_json::from_value(input.command).map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    Ok(Json(
        json!({"revision":app.store.reminder_command(&actor,&op,&command).await?}),
    ))
}
pub(super) async fn subscriptions(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    Ok(Json(app.store.notification_subscriptions(&actor).await?))
}
pub(super) async fn capabilities(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    identity(&app, &headers).await?;
    Ok(Json(app.integrations.capabilities()?))
}
pub(super) async fn deliveries(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<Filter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Query(filter) = query.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(filter.parent_id.is_none(), ErrorCode::InvalidValue)?;
    Ok(Json(
        app.store
            .reminder_delivery_history(&actor, filter.after.as_deref(), filter.limit.unwrap_or(50))
            .await?,
    ))
}
