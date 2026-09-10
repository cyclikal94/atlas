use super::*;
use atlas_core::error::ErrorCode;
use sqlx::{Any, Transaction};

#[derive(Serialize)]
pub struct AccountInvitation {
    pub id: String,
    pub token: String,
    pub expires_at: String,
}
fn timestamp(value: i64) -> Result<String> {
    Ok(chrono::DateTime::from_timestamp(value, 0)
        .ok_or_else(|| anyhow!(ErrorCode::InternalError))?
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}
async fn manager(tx: &mut Transaction<'_, Any>, household: &str, account: &str) -> Result<()> {
    let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM household_memberships WHERE household_id=$1 AND account_id=$2 AND role='manager'").bind(household).bind(account).fetch_one(&mut **tx).await?;
    ensure!(count == 1, ErrorCode::Forbidden);
    Ok(())
}
async fn issue(
    tx: &mut Transaction<'_, Any>,
    issuer: Option<&str>,
    household: Option<&str>,
) -> Result<AccountInvitation> {
    if let Some(household) = household {
        manager(
            tx,
            household,
            issuer.ok_or_else(|| anyhow!(ErrorCode::Forbidden))?,
        )
        .await?;
    }
    let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM account_invitations WHERE (issuer_id=$1 OR (issuer_id IS NULL AND $1 IS NULL)) AND revoked=0 AND redeemed_by IS NULL AND expires_at>$2").bind(issuer).bind(now()).fetch_one(&mut **tx).await?;
    ensure!(count < 20, ErrorCode::RateLimited);
    let id = Uuid::new_v4().to_string();
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let expiry = now() + 7 * 86400;
    sqlx::query("INSERT INTO account_invitations(id,token_hash,issuer_id,household_id,created_at,expires_at) VALUES ($1,$2,$3,$4,$5,$6)").bind(&id).bind(digest(&token)).bind(issuer).bind(household).bind(now()).bind(expiry).execute(&mut **tx).await?;
    Ok(AccountInvitation {
        id,
        token,
        expires_at: timestamp(expiry)?,
    })
}
/// Operator-issued standalone signup; the plaintext token is returned only once.
pub async fn operator_invitation(store: &Store) -> Result<AccountInvitation> {
    let mut tx = store.begin_serial().await?;
    let invitation = issue(&mut tx, None, None).await?;
    tx.commit().await?;
    Ok(invitation)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Invite {
    household_id: String,
}
pub(super) async fn invite(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Invite>, JsonRejection>,
) -> Result<Json<AccountInvitation>, ApiError> {
    let (account, _) = identity(&app, &headers).await?;
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    let mut tx = app.store.begin_serial().await?;
    let result = issue(&mut tx, Some(&account), Some(&input.household_id)).await?;
    tx.commit().await?;
    Ok(Json(result))
}
pub(super) async fn list(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (account, _) = identity(&app, &headers).await?;
    let rows=sqlx::query("SELECT i.id,i.household_id,i.expires_at FROM account_invitations i WHERE (i.issuer_id=$1 OR EXISTS (SELECT 1 FROM household_memberships m WHERE m.household_id=i.household_id AND m.account_id=$1 AND m.role='manager')) AND i.revoked=0 AND i.redeemed_by IS NULL AND i.expires_at>$2 ORDER BY i.expires_at,i.id").bind(account).bind(now()).fetch_all(&app.store.pool).await?;
    let mut invitations = Vec::new();
    for row in rows {
        invitations.push(json!({"id":row.get::<String,_>(0),"household_id":row.get::<Option<String>,_>(1),"expires_at":timestamp(row.get(2))?}));
    }
    Ok(Json(json!({"invitations":invitations})))
}
pub(super) async fn revoke(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let (account, _) = identity(&app, &headers).await?;
    let mut tx = app.store.begin_serial().await?;
    let result=sqlx::query("UPDATE account_invitations SET revoked=1 WHERE id=$1 AND (issuer_id=$2 OR EXISTS (SELECT 1 FROM household_memberships m WHERE m.household_id=account_invitations.household_id AND m.account_id=$2 AND m.role='manager'))").bind(id).bind(account).execute(&mut *tx).await?;
    ensure_api(result.rows_affected() == 1, ErrorCode::NotFound)?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Registration {
    token: String,
    username: String,
    password: String,
    device_id: String,
}
pub(super) async fn register(
    State(app): State<App>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Result<Json<Registration>, JsonRejection>,
) -> Result<Json<SessionResponse>, ApiError> {
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(
        input.token.len() == 64
            && input.token.bytes().all(|c| c.is_ascii_hexdigit())
            && !input.username.is_empty()
            && input.username.len() <= 100
            && (12..=1024).contains(&input.password.len())
            && !input.device_id.is_empty()
            && input.device_id.len() <= 100,
        ErrorCode::InvalidValue,
    )?;
    source_attempt(&app, peer, &headers)?;
    let hash = digest(&input.token);
    let valid:i64=sqlx::query_scalar("SELECT COUNT(*) FROM account_invitations WHERE token_hash=$1 AND expires_at>$2 AND redeemed_by IS NULL AND revoked=0").bind(&hash).bind(now()).fetch_one(&app.store.pool).await?;
    ensure_api(valid == 1, ErrorCode::Unauthenticated)?;
    let permit = app.password_permit().await?;
    let password = tokio::task::spawn_blocking(move || -> Result<String> {
        let _permit = permit;
        Argon2::default()
            .hash_password(input.password.as_bytes())
            .map(|h| h.to_string())
            .map_err(|_| anyhow!(ErrorCode::InternalError))
    })
    .await
    .map_err(|_| anyhow!(ErrorCode::InternalError))??;
    let mut tx = app.store.begin_serial().await?;
    let invitation=sqlx::query("SELECT id,issuer_id,household_id FROM account_invitations WHERE token_hash=$1 AND expires_at>$2 AND redeemed_by IS NULL AND revoked=0").bind(hash).bind(now()).fetch_optional(&mut *tx).await?.ok_or_else(||anyhow!(ErrorCode::Unauthenticated))?;
    let household: Option<String> = invitation.get(2);
    if let Some(household) = &household {
        manager(
            &mut tx,
            household,
            &invitation
                .get::<Option<String>, _>(1)
                .ok_or_else(|| anyhow!(ErrorCode::Forbidden))?,
        )
        .await?;
    }
    let account = Uuid::new_v4().to_string();
    Store::add_account_in(&mut tx, &account, &input.username, &password).await?;
    if let Some(household) = household {
        sqlx::query("INSERT INTO household_memberships(household_id,account_id,role) VALUES ($1,$2,'member')").bind(&household).bind(&account).execute(&mut *tx).await?;
        sqlx::query("UPDATE accounts SET primary_household_id=$1 WHERE id=$2")
            .bind(&household)
            .bind(&account)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE households SET version=version+1 WHERE id=$1")
            .bind(household)
            .execute(&mut *tx)
            .await?;
        // This account has never had a client projection. Its first snapshot
        // reads all current grants, including existing household-shared records.
        sqlx::query("UPDATE sync_clock SET revision=revision+1 WHERE id=1")
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE account_invitations SET redeemed_by=$1 WHERE id=$2")
        .bind(&account)
        .bind(invitation.get::<String, _>(0))
        .execute(&mut *tx)
        .await?;
    let session = sessions::issue(&mut tx, &account, &input.device_id, "local").await?;
    tx.commit().await?;
    Ok(Json(session))
}

/// Operator-only revocation, including standalone invitations.
pub async fn revoke_operator_invitation(store: &Store, id: &str) -> Result<()> {
    let mut tx = store.begin_serial().await?;
    let result = sqlx::query("UPDATE account_invitations SET revoked=1 WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    ensure!(result.rows_affected() == 1, "invitation not found");
    tx.commit().await?;
    Ok(())
}
pub(super) async fn browser_register(
    State(app): State<App>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Result<Json<Registration>, JsonRejection>,
) -> Result<Response, ApiError> {
    let config = app
        .browser
        .as_ref()
        .ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
    config.origin(&headers, true)?;
    let Json(session) = register(State(app.clone()), peer, headers, body).await?;
    Ok(browser::establish(config, session))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Preview {
    token: String,
}
pub(super) async fn preview(
    State(app): State<App>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Result<Json<Preview>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(
        input.token.len() == 64 && input.token.bytes().all(|c| c.is_ascii_hexdigit()),
        ErrorCode::InvalidValue,
    )?;
    source_attempt(&app, peer, &headers)?;
    let row=sqlx::query("SELECT i.household_id,h.name,i.expires_at FROM account_invitations i LEFT JOIN households h ON h.id=i.household_id WHERE i.token_hash=$1 AND i.expires_at>$2 AND i.redeemed_by IS NULL AND i.revoked=0 AND (i.household_id IS NULL OR EXISTS (SELECT 1 FROM household_memberships m WHERE m.household_id=i.household_id AND m.account_id=i.issuer_id AND m.role='manager'))").bind(digest(&input.token)).bind(now()).fetch_optional(&app.store.pool).await?.ok_or_else(||anyhow!(ErrorCode::Unauthenticated))?;
    let household = row
        .get::<Option<String>, _>(0)
        .map(|id| json!({"id":id,"name":row.get::<String,_>(1)}));
    Ok(Json(
        json!({"household":household,"expires_at":timestamp(row.get(2))?}),
    ))
}
