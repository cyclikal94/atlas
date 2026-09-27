use super::*;
use atlas_core::error::ErrorCode;
use sqlx::{Any, Transaction};

/// Returns the issued session and its `session_id` (BE-Q19): every caller but
/// `activation::activate` discards the ID, which exists so `activate` can report it without a
/// second read.
pub(super) async fn issue(
    tx: &mut Transaction<'_, Any>,
    account: &str,
    device: &str,
    kind: &str,
) -> Result<(SessionResponse, String), ApiError> {
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
    let session_id = Uuid::new_v4().to_string();
    let expires_at = created + 86400;
    sqlx::query("INSERT INTO sessions(token_hash,account_id,device_id,expires_at,session_id,created_at,auth_kind) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(digest(&token)).bind(account).bind(device).bind(expires_at).bind(&session_id).bind(created).bind(kind).execute(&mut **tx).await?;
    Ok((
        SessionResponse {
            access_token: token,
            account_id: account.to_owned(),
            expires_at: timestamp(expires_at)?,
        },
        session_id,
    ))
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
/// Exactly one header of this name, or `None` when absent. Repeats are ambiguous, so refused.
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, ApiError> {
    let mut values = headers.get_all(name).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    ensure_api(values.next().is_none(), ErrorCode::MalformedRequest)?;
    Ok(Some(
        first
            .to_str()
            .map_err(|_| anyhow!(ErrorCode::MalformedRequest))?,
    ))
}

/// The operation ID a retirement or keyed revocation is recorded under: one `Idempotency-Key`
/// holding a canonical lower-case UUID, so an ID has a single spelling and a single ledger key.
fn operation_id(headers: &HeaderMap, required: bool) -> Result<Option<String>, ApiError> {
    let Some(value) = single_header(headers, "idempotency-key")? else {
        ensure_api(!required, ErrorCode::MalformedRequest)?;
        return Ok(None);
    };
    ensure_api(
        Uuid::parse_str(value).is_ok_and(|v| v.to_string() == value),
        ErrorCode::InvalidValue,
    )?;
    Ok(Some(value.to_owned()))
}

/// `200` with the recorded outcome, or the `409` carrying it when the approved state had changed.
fn operation_response(operation: atlas_core::operations::Operation) -> Response {
    if operation.outcome == atlas_core::operations::Outcome::RejectedStale {
        return crate::error::operation_rejected(&operation);
    }
    Json(json!({"operation_id":operation.operation_id,"account_id":operation.account_id,"outcome":operation.outcome})).into_response()
}

/// Without an operation ID this is the plain revocation of another session: one `DELETE`,
/// `404` when nothing matched. With one it is recorded, and repeatable, in the operation ledger.
pub(super) async fn revoke(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let (account, _) = identity(&app, &headers).await?;
    let operation = operation_id(&headers, false)?;
    ensure_api(
        Uuid::parse_str(&id).is_ok_and(|v| v.to_string() == id),
        ErrorCode::InvalidValue,
    )?;
    if let Some(operation) = operation {
        return Ok(operation_response(
            app.store
                .revoke_session(&account, &id, &operation, now())
                .await?,
        ));
    }
    hook!(app.store, "session_delete.after_identity");
    let removed: Option<String> = sqlx::query_scalar(
        "DELETE FROM sessions WHERE session_id=$1 AND account_id=$2 RETURNING token_hash",
    )
    .bind(id)
    .bind(account)
    .fetch_optional(&app.store.pool)
    .await?;
    ensure_api(removed.is_some(), ErrorCode::NotFound)?;
    Ok(StatusCode::NO_CONTENT.into_response())
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
        .bind(&account)
        .execute(&mut *tx)
        .await?;
    // A grant minted with a password that has since changed must not still be activatable
    // (BE-Q19 item 7); `redeemed` rows are left alone as evidence for `activate/cancel`.
    sqlx::query(
        "UPDATE activation_grants SET state='cancelled',cancelled_at=$1 WHERE account_id=$2 AND state='issued'",
    )
    .bind(now())
    .bind(&account)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
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
        .bind(&account)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE activation_grants SET state='cancelled',cancelled_at=$1 WHERE account_id=$2 AND state='issued'",
    )
    .bind(now())
    .bind(&account)
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
    sqlx::query(
        "UPDATE activation_grants SET state='cancelled',cancelled_at=$1 WHERE account_id=$2 AND state='issued'",
    )
    .bind(now())
    .bind(&account)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(super) async fn devices(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (account, current) = identity(&app, &headers).await?;
    let mut devices = Vec::new();
    for device in app.store.devices(&account, now()).await? {
        devices.push(json!({"id":device.id,"last_synced_at":device.last_synced_at.map(timestamp).transpose()?,"active_sessions":device.active_sessions,"current":device.id==current,"state_token":device.state_token,"summary":device.summary}));
    }
    Ok(Json(json!({"devices":devices})))
}
/// Retire a device only if its approved state still matches the token the user confirmed.
/// The credential's own session may be among the members; the response carries no `Set-Cookie`
/// either way, and the caller recovers the outcome by repeating this call.
pub(super) async fn forget_device(
    State(app): State<App>,
    headers: HeaderMap,
    Path(device): Path<String>,
) -> Result<Response, ApiError> {
    let (account, _) = identity(&app, &headers).await?;
    let operation =
        operation_id(&headers, true)?.ok_or_else(|| anyhow!(ErrorCode::MalformedRequest))?;
    let token = single_header(&headers, "atlas-device-state")?
        .ok_or_else(|| anyhow!(ErrorCode::MalformedRequest))?;
    Ok(operation_response(
        app.store
            .retire_device(&account, &device, &operation, token, now())
            .await?,
    ))
}
