use crate::*;
use atlas_core::error::ErrorCode;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Commands {
    commands: Vec<Command>,
    defaults_revision: Option<String>,
}
pub(crate) async fn execute(
    app: App,
    headers: HeaderMap,
    body: Result<Json<Commands>, JsonRejection>,
    access: bool,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (account, _) = identity(&app, &headers).await?;
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(
        input
            .commands
            .iter()
            .all(|c| requires_online_sharing(c) == access),
        ErrorCode::InvalidValue,
    )?;
    let operation = headers
        .get("idempotency-key")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(
        input
            .defaults_revision
            .as_ref()
            .is_none_or(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())),
        ErrorCode::InvalidValue,
    )?;
    let revision = app
        .store
        .apply_with_defaults(
            &account,
            operation,
            &input.commands,
            input.defaults_revision.as_deref(),
        )
        .await?;
    // Minimal receipt; never replay protected content after a permission change.
    Ok(Json(json!({"revision":revision})))
}
pub(crate) async fn commands(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Commands>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    execute(app, headers, body, false).await
}
pub(crate) async fn access_commands(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Commands>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    execute(app, headers, body, true).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SyncQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}
pub(crate) async fn sync(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<SyncQuery>, QueryRejection>,
) -> Result<Json<atlas_core::Page>, ApiError> {
    let (account, device) = identity(&app, &headers).await?;
    let Query(input) = query.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(
        input.cursor.as_ref().is_none_or(|c| c.len() <= 512),
        ErrorCode::InvalidValue,
    )?;
    Ok(Json(
        app.store
            .sync(
                &account,
                &device,
                input.cursor.as_deref(),
                input.limit.unwrap_or(50),
                now(),
            )
            .await?,
    ))
}
