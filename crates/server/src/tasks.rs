use super::*;
use atlas_core::error::ErrorCode;
use atlas_core::tasks::{TaskCommand, ViewFilter};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    command: TaskCommand,
    defaults_revision: Option<String>,
}
async fn execute(
    app: App,
    headers: HeaderMap,
    body: Result<Json<Input>, JsonRejection>,
    online: bool,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(input.command.online() == online, ErrorCode::InvalidValue)?;
    ensure_api(
        input
            .defaults_revision
            .as_ref()
            .is_none_or(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
        ErrorCode::InvalidValue,
    )?;
    let operation = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow!(ErrorCode::MalformedRequest))?;
    Ok(Json(serde_json::to_value(
        app.store
            .task_command(
                &actor,
                operation,
                &input.command,
                input.defaults_revision.as_deref(),
                now(),
            )
            .await?,
    )?))
}
pub(super) async fn commands(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Input>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    execute(app, headers, body, false).await
}
pub(super) async fn access(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Input>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    execute(app, headers, body, true).await
}
pub(super) async fn detail(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    Ok(Json(app.store.task_detail(&actor, &id, now()).await?))
}
pub(super) async fn enrolment(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    Ok(Json(app.store.task_enrolment(&actor, &id).await?))
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct StreakFilter {
    scope: Option<String>,
}
pub(super) async fn streak(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<StreakFilter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Query(filter) = query.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    Ok(Json(serde_json::to_value(
        app.store
            .task_streak_scoped(
                &actor,
                &id,
                filter.scope.as_deref().unwrap_or("joint"),
                now(),
            )
            .await?,
    )?))
}
pub(super) async fn occurrences(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<ViewFilter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Query(filter) = query.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    Ok(Json(serde_json::to_value(
        app.store.occurrence_view(&actor, &filter, now()).await?,
    )?))
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct ResourceFilter {
    parent_id: Option<String>,
    #[serde(default)]
    archived: bool,
    pub(crate) after: Option<String>,
    pub(crate) limit: Option<u16>,
}
pub(crate) async fn list(
    app: App,
    headers: HeaderMap,
    query: Result<Query<ResourceFilter>, QueryRejection>,
    kind: &str,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Query(filter) = query.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    let limit = filter.limit.unwrap_or(50);
    let items = app
        .store
        .resources(
            &actor,
            kind,
            filter.parent_id.as_deref(),
            filter.archived,
            filter.after.as_deref(),
            limit,
        )
        .await?;
    let next_after = if items.len() == usize::from(limit) {
        items.last().map(|p| p.id.clone())
    } else {
        None
    };
    Ok(Json(json!({"items":items,"next_after":next_after})))
}
pub(super) async fn tasks(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<ResourceFilter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    list(app, headers, query, "task").await
}
pub(super) async fn lists(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<ResourceFilter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    list(app, headers, query, "list").await
}

pub(super) async fn fields(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<ResourceFilter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    list(app, headers, query, "field").await
}

pub(super) async fn progress(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<ResourceFilter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    list(app, headers, query, "progress").await
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct JournalFilter {
    after: Option<i64>,
    pub(crate) limit: Option<u16>,
}
pub(super) async fn journal(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<JournalFilter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Query(filter) = query.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    Ok(Json(
        app.store
            .progress_journal(
                &actor,
                &id,
                filter.after.unwrap_or(0),
                filter.limit.unwrap_or(50),
            )
            .await?,
    ))
}
pub(super) async fn daily(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<ViewFilter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Query(mut filter) = query.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    // A day and display zone are explicit: server or device timezone changes must
    // never silently change the client's requested planning date.
    ensure_api(
        filter.day.is_some() && filter.timezone.is_some(),
        ErrorCode::InvalidValue,
    )?;
    if filter.state.is_none() {
        filter.state = Some("all".into());
    }
    Ok(Json(serde_json::to_value(
        app.store.occurrence_view(&actor, &filter, now()).await?,
    )?))
}

pub(super) async fn dependencies(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    Ok(Json(serde_json::to_value(
        app.store.dependency_preview(&actor, &id, now()).await?,
    )?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TimerFilter {
    pub(crate) after: Option<String>,
    pub(crate) limit: Option<u16>,
}
pub(super) async fn timers(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<TimerFilter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Query(filter) = query.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    let limit = filter.limit.unwrap_or(50);
    let items = app
        .store
        .timer_sessions(&actor, &id, filter.after.as_deref(), limit)
        .await?;
    let next_after = if items.len() == usize::from(limit) {
        items.last().map(|v| v.id.clone())
    } else {
        None
    };
    Ok(Json(json!({"items":items,"next_after":next_after})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PresetFilter {
    start_date: String,
    timezone: String,
}
pub(super) async fn presets(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<PresetFilter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    identity(&app, &headers).await?;
    let Query(filter) = query.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    Ok(Json(
        json!({"items":atlas_core::tasks::presets(&filter.start_date, &filter.timezone)?}),
    ))
}

pub(super) async fn rota(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    Ok(Json(serde_json::to_value(
        app.store.task_rota(&actor, &id).await?,
    )?))
}
