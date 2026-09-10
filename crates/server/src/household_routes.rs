use crate::*;
use atlas_core::error::ErrorCode;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagementRequest {
    commands: Vec<atlas_core::households::ManagementCommand>,
}
pub(crate) async fn management(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<ManagementRequest>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    let operation = headers
        .get("idempotency-key")
        .and_then(|s| s.to_str().ok())
        .ok_or_else(|| anyhow!(ErrorCode::MalformedRequest))?;
    let revision = app
        .store
        .management(&actor, operation, &input.commands, now())
        .await?;
    Ok(Json(json!({"revision":revision})))
}
pub(crate) async fn households(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Json<Vec<atlas_core::households::Household>>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    Ok(Json(app.store.households(&actor).await?))
}
pub(crate) async fn invitations(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Json<Vec<atlas_core::households::Invitation>>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    Ok(Json(app.store.invitations(&actor).await?))
}
pub(crate) async fn defaults(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Json<atlas_core::policy::Defaults>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    Ok(Json(app.store.defaults(&actor).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TemplateQuery {
    household_id: Option<String>,
}
pub(crate) async fn default_templates(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<TemplateQuery>, QueryRejection>,
) -> Result<Json<atlas_core::policy::TemplateScope>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    let Query(input) = query.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    Ok(Json(
        app.store
            .default_templates(&actor, input.household_id.as_deref())
            .await?,
    ))
}
pub(crate) async fn resource_policy(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<atlas_core::policy::PolicyState>, ApiError> {
    let (actor, _) = identity(&app, &headers).await?;
    Ok(Json(app.store.resource_policy(&actor, &id).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectoryQuery {
    username: Option<String>,
}
pub(crate) async fn directory(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<DirectoryQuery>, QueryRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    identity(&app, &headers).await?;
    let Query(input) = query.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(
        app.directory_enabled || input.username.is_some(),
        ErrorCode::Forbidden,
    )?;
    ensure_api(
        input.username.as_ref().is_none_or(|s| s.len() <= 100),
        ErrorCode::InvalidValue,
    )?;
    let rows = if let Some(username) = input.username {
        sqlx::query("SELECT id,username FROM accounts WHERE username=$1")
            .bind(username)
            .fetch_all(&app.store.pool)
            .await?
    } else {
        sqlx::query("SELECT id,username FROM accounts ORDER BY username LIMIT 100")
            .fetch_all(&app.store.pool)
            .await?
    };
    Ok(Json(json!(
        rows.into_iter()
            .map(|r| json!({"id":r.get::<String,_>(0),"username":r.get::<String,_>(1)}))
            .collect::<Vec<_>>()
    )))
}
