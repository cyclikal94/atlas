use super::*;
use atlas_core::error::ErrorCode;
use atlas_core::people::PeopleCommand;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    command: PeopleCommand,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreviewInput {
    source_id: String,
    target_id: String,
}
pub(super) async fn command(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Input>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    let operation = headers
        .get("idempotency-key")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| anyhow!(ErrorCode::MalformedRequest))?;
    Ok(Json(serde_json::to_value(
        app.store
            .people_command(&actor, operation, &input.command, now())
            .await?,
    )?))
}
pub(super) async fn preview(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<PreviewInput>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    Ok(Json(serde_json::to_value(
        app.store
            .merge_preview(&actor, &input.source_id, &input.target_id)
            .await?,
    )?))
}
pub(super) async fn requests(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    Ok(Json(
        json!({"items":app.store.people_requests(&actor,now()).await?}),
    ))
}
pub(super) async fn detail(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    Ok(Json(serde_json::to_value(
        app.store.person_detail(&actor, &id).await?,
    )?))
}

use crate::tasks::{ResourceFilter, TimerFilter, list};
pub(super) async fn people(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<ResourceFilter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    list(app, headers, query, "person").await
}

pub(super) async fn duplicates(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<TimerFilter>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Query(filter) = query.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    Ok(Json(serde_json::to_value(
        app.store
            .person_duplicates(
                &actor,
                &id,
                filter.after.as_deref(),
                filter.limit.unwrap_or(50),
            )
            .await?,
    )?))
}
