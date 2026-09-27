//! Browser sign-in grants (BE-Q19): the bearer credential login, registration and the OIDC
//! callback return in place of a cookie, and the only two routes that redeem or abandon one.
//! See `docs/authentication.md` and `account-lifecycle-matrix.md` §3.5 for the protocol this
//! implements; `atlas_core::devices` owns the grant's membership in the approved device state.
use super::*;
use atlas_core::error::ErrorCode;
use openidconnect::{PkceCodeChallenge, PkceCodeVerifier};
use sqlx::{Any, Transaction};

/// Shape of an S256 PKCE challenge and of a grant's `attempt_challenge`: 43-character unpadded
/// base64url, the charset `devices::valid_state_token` checks minus its `v1.` prefix. Shared by
/// every issuer (`browser::login`, `onboarding::browser_register`, `oidc::start`) rather than
/// duplicated three times.
pub(super) fn valid_challenge(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// RFC 7636 `code_verifier` shape, identical to `oidc::Exchange.code_verifier`'s validation.
fn valid_verifier(value: &str) -> bool {
    (43..=128).contains(&value.len())
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c))
}

fn timestamp(seconds: i64) -> Result<String, ApiError> {
    Ok(chrono::DateTime::from_timestamp(seconds, 0)
        .ok_or_else(|| anyhow!(ErrorCode::InternalError))?
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

#[derive(Serialize)]
pub(super) struct BrowserGrant {
    pub(super) grant: String,
    pub(super) grant_expires_at: String,
    pub(super) account_id: String,
}

/// Issue a grant for `account`/`device`/`auth_kind`, storing the caller-supplied
/// `attempt_challenge` verbatim as `challenge_hash` (it is already `S256(verifier)`). Must run
/// inside the caller's own `begin_serial()` transaction so the grant commits atomically with
/// whatever authenticated or created the account — mirrors `sessions::issue`'s contract exactly.
pub(super) async fn issue_grant(
    tx: &mut Transaction<'_, Any>,
    account: &str,
    device: &str,
    auth_kind: &str,
    challenge_hash: &str,
    now: i64,
) -> Result<BrowserGrant, ApiError> {
    // Terminal and unredeemed-expired rows are retained 48h as activate/cancel evidence, then
    // swept, mirroring the `oidc_flows` pattern; an `issued`, unexpired row never matches.
    sqlx::query(
        "DELETE FROM activation_grants WHERE COALESCE(redeemed_at,cancelled_at,expires_at)<=$1",
    )
    .bind(now - 48 * 3600)
    .execute(&mut **tx)
    .await?;
    // The cap is on still-activatable rows only (approved Q19 item 1: "1000 non-terminal
    // rows"): a retained-but-expired `issued` row is evidence, not an open attempt, and must not
    // itself block new issuance for the rest of its 48h retention.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM activation_grants WHERE state='issued' AND expires_at>$1",
    )
    .bind(now)
    .fetch_one(&mut **tx)
    .await?;
    ensure_api(count < 1000, ErrorCode::RateLimited)?;
    let grant = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let grant_id = Uuid::new_v4().to_string();
    let expires_at = now + 60;
    sqlx::query("INSERT INTO activation_grants(grant_hash,grant_id,account_id,device_id,auth_kind,challenge_hash,state,failed_verifiers,created_at,expires_at) VALUES ($1,$2,$3,$4,$5,$6,'issued',0,$7,$8)")
        .bind(digest(&grant))
        .bind(&grant_id)
        .bind(account)
        .bind(device)
        .bind(auth_kind)
        .bind(challenge_hash)
        .bind(now)
        .bind(expires_at)
        .execute(&mut **tx)
        .await?;
    Ok(BrowserGrant {
        grant,
        grant_expires_at: timestamp(expires_at)?,
        account_id: account.to_owned(),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Activate {
    grant: String,
    verifier: String,
}

/// `POST /browser-sessions/activate`. Origin required, throttled by source. A verifier mismatch
/// increments `failed_verifiers` (a real, committed write) and returns `401` without redeeming;
/// the fifth failure also cancels the grant. A `DeviceCapacity` failure from `sessions::issue`
/// rolls the whole transaction back, leaving the grant `issued` and still redeemable until it
/// expires — `begin_serial()`'s existing global serialisation (the same `sync_clock` ordering
/// point every other approved-state writer uses) is what makes two concurrent calls against the
/// same grant resolve to exactly one winner, with no extra locking needed here. `now` is read
/// only after that serialisation is acquired, never before: a request queued behind a
/// connection-pool or lock wait must judge expiry by the time it actually runs, not by a value
/// captured while still waiting, or a request that sat queued past the grant's expiry could still
/// redeem it. The redemption write re-asserts the same expiry condition (`activation_grants`'s
/// `state='issued'` alone is not enough), so the guarantee holds even if a future caller ever
/// reads `now` earlier again.
pub(super) async fn activate(
    State(app): State<App>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Result<Json<Activate>, JsonRejection>,
) -> Result<Response, ApiError> {
    let config = app
        .browser
        .as_ref()
        .ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
    config.origin(&headers, true)?;
    source_attempt(&app, peer, &headers)?;
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(
        input.grant.len() == 64
            && input.grant.bytes().all(|c| c.is_ascii_hexdigit())
            && valid_verifier(&input.verifier),
        ErrorCode::InvalidValue,
    )?;
    let grant_hash = digest(&input.grant);
    hook!(app.store, "activate.before_begin");
    let mut tx = app.store.begin_serial().await?;
    let now = now();
    let row = sqlx::query(
        "SELECT account_id,device_id,auth_kind,challenge_hash,state,failed_verifiers,expires_at FROM activation_grants WHERE grant_hash=$1",
    )
    .bind(&grant_hash)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| anyhow!(ErrorCode::Unauthenticated))?;
    let state: String = row.get(4);
    let expires_at: i64 = row.get(6);
    ensure_api(
        state == "issued" && expires_at > now,
        ErrorCode::Unauthenticated,
    )?;
    let challenge_hash: String = row.get(3);
    let presented =
        PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(input.verifier));
    if presented.as_str() != challenge_hash {
        let failed: i64 = row.get(5);
        let failed = failed + 1;
        if failed >= 5 {
            sqlx::query("UPDATE activation_grants SET failed_verifiers=$1,state='cancelled',cancelled_at=$2 WHERE grant_hash=$3 AND state='issued'")
                .bind(failed).bind(now).bind(&grant_hash).execute(&mut *tx).await?;
        } else {
            sqlx::query("UPDATE activation_grants SET failed_verifiers=$1 WHERE grant_hash=$2 AND state='issued'")
                .bind(failed).bind(&grant_hash).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        return Err(anyhow!(ErrorCode::Unauthenticated).into());
    }
    let account: String = row.get(0);
    let device: String = row.get(1);
    let auth_kind: String = row.get(2);
    let (session, session_id) = sessions::issue(&mut tx, &account, &device, &auth_kind).await?;
    // Re-check `expires_at>now` here as well as above: this is the write that actually creates a
    // session, so it — not only the earlier read — must enforce the approved expiry condition.
    let redeemed = sqlx::query("UPDATE activation_grants SET state='redeemed',session_id=$1,redeemed_at=$2 WHERE grant_hash=$3 AND state='issued' AND expires_at>$4")
        .bind(&session_id).bind(now).bind(&grant_hash).bind(now).execute(&mut *tx).await?;
    ensure_api(redeemed.rows_affected() == 1, ErrorCode::InternalError)?;
    tx.commit().await?;
    Ok(browser::establish(config, session, &session_id))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ActivateCancel {
    verifier: String,
}

/// `POST /browser-sessions/activate/cancel`. Origin required, throttled by source, no cookie
/// required or set. Resolved purely by `S256(verifier)=challenge_hash`, which is `UNIQUE`, so a
/// tab that only remembers its verifier can abandon an attempt with no reference to the grant.
/// Cancellation is durably terminal for every `issued` row, expired or not: it always writes
/// `state='cancelled'` rather than leaving an expired row `issued` for the sweep to find later,
/// so a concurrent `activate` that is still mid-flight (queued behind a pool or lock wait) cannot
/// find the row `issued` and redeem it after this call has already reported `not_activated`.
pub(super) async fn activate_cancel(
    State(app): State<App>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Result<Json<ActivateCancel>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let config = app
        .browser
        .as_ref()
        .ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
    config.origin(&headers, true)?;
    source_attempt(&app, peer, &headers)?;
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(valid_verifier(&input.verifier), ErrorCode::InvalidValue)?;
    let challenge_hash =
        PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(input.verifier))
            .as_str()
            .to_owned();
    let mut tx = app.store.begin_serial().await?;
    let now = now();
    let row = sqlx::query(
        "SELECT account_id,state,session_id FROM activation_grants WHERE challenge_hash=$1",
    )
    .bind(&challenge_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let result = match row {
        None => "unknown",
        Some(row) => match row.get::<String, _>(1).as_str() {
            "issued" => {
                sqlx::query("UPDATE activation_grants SET state='cancelled',cancelled_at=$1 WHERE challenge_hash=$2 AND state='issued'")
                    .bind(now).bind(&challenge_hash).execute(&mut *tx).await?;
                "not_activated"
            }
            "cancelled" => "not_activated",
            _ => {
                // redeemed: the row stays as evidence; only the live session is revoked.
                let account: String = row.get(0);
                if let Some(session_id) = row.get::<Option<String>, _>(2) {
                    sqlx::query("DELETE FROM sessions WHERE session_id=$1 AND account_id=$2")
                        .bind(session_id)
                        .bind(account)
                        .execute(&mut *tx)
                        .await?;
                }
                "session_revoked"
            }
        },
    };
    tx.commit().await?;
    Ok(Json(json!({"result": result})))
}
