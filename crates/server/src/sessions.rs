use super::*;
use atlas_core::error::ErrorCode;
use sqlx::{Any, Transaction};

pub(super) async fn issue(
    tx: &mut Transaction<'_, Any>,
    account: &str,
    device: &str,
    kind: &str,
) -> Result<SessionResponse, ApiError> {
    let created = now();
    sqlx::query("DELETE FROM sessions WHERE account_id=$1 AND expires_at<=$2")
        .bind(account)
        .bind(created)
        .execute(&mut **tx)
        .await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE account_id=$1")
        .bind(account)
        .fetch_one(&mut **tx)
        .await?;
    ensure_api(count < 32, ErrorCode::DeviceCapacity)?;
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let expires_at = created + 86400;
    sqlx::query("INSERT INTO sessions(token_hash,account_id,device_id,expires_at,session_id,created_at,auth_kind) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(digest(&token)).bind(account).bind(device).bind(expires_at).bind(Uuid::new_v4().to_string()).bind(created).bind(kind).execute(&mut **tx).await?;
    Ok(SessionResponse {
        access_token: token,
        account_id: account.to_owned(),
        expires_at: timestamp(expires_at)?,
    })
}
fn timestamp(seconds: i64) -> Result<String, ApiError> {
    Ok(chrono::DateTime::from_timestamp(seconds, 0)
        .ok_or_else(|| anyhow!(ErrorCode::InternalError))?
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

pub(super) async fn list(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (account, _) = identity(&app, &headers).await?;
    let current = digest(&browser::credential(&app, &headers)?);
    let rows = sqlx::query("SELECT session_id,device_id,created_at,expires_at,auth_kind,token_hash FROM sessions WHERE account_id=$1 AND expires_at>$2 ORDER BY created_at,session_id").bind(account).bind(now()).fetch_all(&app.store.pool).await?;
    let mut sessions = Vec::new();
    for row in rows {
        sessions.push(json!({"id":row.get::<String,_>(0),"device_id":row.get::<String,_>(1),"created_at":timestamp(row.get(2))?,"expires_at":timestamp(row.get(3))?,"authentication":row.get::<String,_>(4),"current":row.get::<String,_>(5)==current}));
    }
    Ok(Json(json!({"sessions":sessions})))
}
pub(super) async fn revoke(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let (account, _) = identity(&app, &headers).await?;
    ensure_api(
        Uuid::parse_str(&id).is_ok_and(|v| v.to_string() == id),
        ErrorCode::InvalidValue,
    )?;
    let removed: Option<String> = sqlx::query_scalar(
        "DELETE FROM sessions WHERE session_id=$1 AND account_id=$2 RETURNING token_hash",
    )
    .bind(id)
    .bind(account)
    .fetch_optional(&app.store.pool)
    .await?;
    let Some(removed) = removed else {
        return Err(anyhow!(ErrorCode::NotFound).into());
    };
    let response = StatusCode::NO_CONTENT.into_response();
    Ok(
        if removed == digest(&browser::credential(&app, &headers)?) {
            browser::clear_cookie(&app, response)
        } else {
            response
        },
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PasswordChange {
    current_password: String,
    new_password: String,
}

pub(super) async fn change_password(
    State(app): State<App>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Result<Json<PasswordChange>, JsonRejection>,
) -> Result<Response, ApiError> {
    let (account, _) = identity(&app, &headers).await?;
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(
        input.current_password.len() <= 1024 && (12..=1024).contains(&input.new_password.len()),
        ErrorCode::InvalidValue,
    )?;
    source_attempt(&app, peer, &headers)?;
    let permit = app.password_permit().await?;
    let expected: String = sqlx::query_scalar("SELECT password_hash FROM accounts WHERE id=$1")
        .bind(&account)
        .fetch_one(&app.store.pool)
        .await?;
    let old = expected.clone();
    let new = tokio::task::spawn_blocking(move || -> Result<String> {
        let _permit = permit;
        ensure!(
            PasswordHash::new(&old).is_ok_and(|h| Argon2::default()
                .verify_password(input.current_password.as_bytes(), &h)
                .is_ok()),
            ErrorCode::Unauthenticated
        );
        Argon2::default()
            .hash_password(input.new_password.as_bytes())
            .map(|h| h.to_string())
            .map_err(|_| anyhow!(ErrorCode::InternalError))
    })
    .await
    .map_err(|_| anyhow!(ErrorCode::InternalError))??;
    let mut tx = app.store.begin_serial().await?;
    let session = digest(&browser::credential(&app, &headers)?);
    let valid: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions WHERE token_hash=$1 AND account_id=$2 AND expires_at>$3",
    )
    .bind(session)
    .bind(&account)
    .bind(now())
    .fetch_one(&mut *tx)
    .await?;
    ensure_api(valid == 1, ErrorCode::Unauthenticated)?;
    let result =
        sqlx::query("UPDATE accounts SET password_hash=$1 WHERE id=$2 AND password_hash=$3")
            .bind(new)
            .bind(&account)
            .bind(expected)
            .execute(&mut *tx)
            .await?;
    ensure_api(result.rows_affected() == 1, ErrorCode::Conflict)?;
    sqlx::query("DELETE FROM native_handoffs WHERE account_id=$1")
        .bind(&account)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE account_id=$1")
        .bind(account)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(browser::clear_cookie(
        &app,
        StatusCode::NO_CONTENT.into_response(),
    ))
}

/// Operator recovery. Changing credentials revokes all sessions atomically.
pub async fn reset_password(store: &Store, username: &str, password_hash: &str) -> Result<()> {
    let mut tx = store.begin_serial().await?;
    let account: String =
        sqlx::query_scalar("UPDATE accounts SET password_hash=$1 WHERE username=$2 RETURNING id")
            .bind(password_hash)
            .bind(username)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| anyhow!("account not found"))?;
    sqlx::query("DELETE FROM native_handoffs WHERE account_id=$1")
        .bind(&account)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE account_id=$1")
        .bind(account)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub(super) async fn me(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (account, _) = identity(&app, &headers).await?;
    let row = sqlx::query("SELECT username,password_hash FROM accounts WHERE id=$1")
        .bind(&account)
        .fetch_one(&app.store.pool)
        .await?;
    let issuers: Vec<String> = sqlx::query_scalar(
        "SELECT issuer FROM external_identities WHERE account_id=$1 ORDER BY issuer",
    )
    .bind(&account)
    .fetch_all(&app.store.pool)
    .await?;
    Ok(Json(
        json!({"id":account,"username":row.get::<String,_>(0),"has_password":row.get::<String,_>(1).starts_with("$argon2id$"),"oidc_issuers":issuers}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Unlink {
    issuer: String,
}
pub(super) async fn unlink_oidc(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Unlink>, JsonRejection>,
) -> Result<Response, ApiError> {
    let (account, _) = identity(&app, &headers).await?;
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(input.issuer.len() <= 2048, ErrorCode::InvalidValue)?;
    let mut tx = app.store.begin_serial().await?;
    let recent:i64=sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE token_hash=$1 AND account_id=$2 AND created_at>=$3 AND expires_at>$4").bind(digest(&browser::credential(&app,&headers)?)).bind(&account).bind(now()-300).bind(now()).fetch_one(&mut *tx).await?;
    ensure_api(recent == 1, ErrorCode::Unauthenticated)?;
    let password: String = sqlx::query_scalar("SELECT password_hash FROM accounts WHERE id=$1")
        .bind(&account)
        .fetch_one(&mut *tx)
        .await?;
    ensure_api(password.starts_with("$argon2id$"), ErrorCode::Conflict)?;
    let result = sqlx::query("DELETE FROM external_identities WHERE account_id=$1 AND issuer=$2")
        .bind(&account)
        .bind(input.issuer)
        .execute(&mut *tx)
        .await?;
    ensure_api(result.rows_affected() == 1, ErrorCode::NotFound)?;
    sqlx::query("DELETE FROM native_handoffs WHERE account_id=$1")
        .bind(&account)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE account_id=$1")
        .bind(&account)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(browser::clear_cookie(
        &app,
        StatusCode::NO_CONTENT.into_response(),
    ))
}

pub(super) async fn devices(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (account, current) = identity(&app, &headers).await?;
    let mut devices = Vec::new();
    for device in app.store.devices(&account, now()).await? {
        devices.push(json!({"id":device.id,"last_synced_at":device.last_synced_at.map(timestamp).transpose()?,"active_sessions":device.active_sessions,"current":device.id==current}));
    }
    Ok(Json(json!({"devices":devices})))
}
pub(super) async fn forget_device(
    State(app): State<App>,
    headers: HeaderMap,
    Path(device): Path<String>,
) -> Result<Response, ApiError> {
    let (account, current) = identity(&app, &headers).await?;
    app.store.forget_device(&account, &device).await?;
    let response = StatusCode::NO_CONTENT.into_response();
    Ok(if current == device {
        browser::clear_cookie(&app, response)
    } else {
        response
    })
}
