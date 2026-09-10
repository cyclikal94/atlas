//! Atlas HTTP API with local and OIDC authentication.
use atlas_core::error::ErrorCode;
mod household_routes;
use household_routes::{
    default_templates, defaults, directory, households, invitations, management, resource_policy,
};
mod resource_routes;
use resource_routes::{access_commands, commands, sync};
mod app;
mod error;
pub use error::ApiError;
mod browser;
mod sessions;
pub use sessions::reset_password;
mod oidc;
mod onboarding;
pub use onboarding::{operator_invitation, revoke_operator_invitation};
mod calendars;
pub mod integrations;
mod people;
mod tasks;
mod throttle;
use anyhow::{Result, anyhow, ensure};
use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use atlas_core::{Command, Store};
use axum::{
    Extension, Json, Router,
    extract::Request,
    extract::{
        ConnectInfo, DefaultBodyLimit, MatchedPath, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;
use uuid::Uuid;

#[derive(Clone)]
pub struct App {
    pub store: Store,
    integrations: integrations::IntegrationConfig,
    calendar_slots: Arc<Semaphore>,
    password_slots: Arc<Semaphore>,
    password_admission: Arc<Semaphore>,
    unknown_principal_budget: Arc<Mutex<throttle::Budget>>,
    principal_budget: Arc<Mutex<throttle::Budget>>,
    source_budget: Arc<Mutex<throttle::Budget>>,
    started: Instant,
    trusted_proxies: Arc<Vec<IpAddr>>,
    dummy_hash: Arc<String>,
    directory_enabled: bool,
    browser: Option<browser::Config>,
    oidc: Option<Arc<oidc::Config>>,
    native_redirects: Arc<Vec<String>>,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn digest(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub async fn hash_password(password: String) -> Result<String> {
    ensure!(
        (12..=1024).contains(&password.len()),
        ErrorCode::PasswordLength
    );
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|h| h.to_string())
            .map_err(|_| anyhow!(ErrorCode::PasswordHashFailed))
    })
    .await?
}

struct PasswordPermit {
    _admission: tokio::sync::OwnedSemaphorePermit,
    _work: tokio::sync::OwnedSemaphorePermit,
}

async fn no_store(request: Request, next: Next) -> Response {
    let id = Uuid::new_v4().to_string();
    let method = request.method().to_string();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("<unmatched>", |p| p.as_str())
        .to_owned();
    let started = Instant::now();
    let mut response = REQUEST_ID.scope(id.clone(), next.run(request)).await;
    response
        .headers_mut()
        .insert("cache-control", "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert("x-request-id", id.parse().unwrap());
    response
        .headers_mut()
        .insert("referrer-policy", "no-referrer".parse().unwrap());
    eprintln!(
        "{}",
        json!({"event":"http_request","request_id":id,"method":method,"route":route,"status":response.status().as_u16(),"duration_ms":started.elapsed().as_millis()})
    );
    response
}

tokio::task_local! {static REQUEST_ID:String;}

async fn ready(State(app): State<App>) -> Result<Json<serde_json::Value>, ApiError> {
    sqlx::query("SELECT 1")
        .execute(&app.store.pool)
        .await
        .map_err(|_| anyhow!(ErrorCode::NotReady))?;
    Ok(Json(json!({"status":"ready"})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Login {
    username: String,
    password: String,
    device_id: String,
}
#[derive(Serialize, Deserialize)]
pub struct SessionResponse {
    pub access_token: String,
    pub account_id: String,
    pub expires_at: String,
}

async fn login(
    State(app): State<App>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Result<Json<Login>, JsonRejection>,
) -> Result<Json<SessionResponse>, ApiError> {
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(
        !input.device_id.is_empty()
            && input.device_id.len() <= 100
            && input.username.len() <= 100
            && input.password.len() <= 1024,
        ErrorCode::InvalidValue,
    )?;
    source_attempt(&app, peer, &headers)?;
    // Source admission precedes this indexed lookup. Unknown names never occupy
    // the registered-account budget, nor evict an existing account's attempts.
    let row = sqlx::query("SELECT id,password_hash FROM accounts WHERE username=$1")
        .bind(&input.username)
        .fetch_optional(&app.store.pool)
        .await?;
    let seconds = app.started.elapsed().as_secs();
    let (budget, key) = if let Some(row) = &row {
        (&app.principal_budget, row.get::<String, _>(0))
    } else {
        // A separate fixed bucket space preserves throttling for repeated unknown
        // names without making their cardinality a registered-user lockout vector.
        (
            &app.unknown_principal_budget,
            digest(&input.username)[..3].to_owned(),
        )
    };
    ensure_api(
        budget
            .lock()
            .map_err(|_| anyhow!(ErrorCode::InternalError))?
            .allow(&key, seconds),
        ErrorCode::RateLimited,
    )?;
    let permit = app.password_permit().await?;
    let hash = row
        .as_ref()
        .map(|r| r.get::<String, _>(1))
        .unwrap_or_else(|| (*app.dummy_hash).clone());
    let expected_hash = hash.clone();
    let valid = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        PasswordHash::new(&hash).is_ok_and(|h| {
            Argon2::default()
                .verify_password(input.password.as_bytes(), &h)
                .is_ok()
        })
    })
    .await
    .map_err(|_| anyhow!(ErrorCode::InternalError))?;
    ensure_api(valid && row.is_some(), ErrorCode::Unauthenticated)?;
    app.principal_budget
        .lock()
        .map_err(|_| anyhow!(ErrorCode::InternalError))?
        .reset(&key);
    let account_id: String = row.unwrap().get(0);
    let mut tx = app.store.begin_serial().await?;
    let unchanged: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE id=$1 AND password_hash=$2")
            .bind(&account_id)
            .bind(expected_hash)
            .fetch_one(&mut *tx)
            .await?;
    ensure_api(unchanged == 1, ErrorCode::Unauthenticated)?;
    let session = sessions::issue(&mut tx, &account_id, &input.device_id, "local").await?;
    tx.commit().await?;
    Ok(Json(session))
}

fn ensure_api(condition: bool, code: ErrorCode) -> Result<(), ApiError> {
    if !condition {
        return Err(anyhow!(code).into());
    }
    Ok(())
}
fn bearer(headers: &HeaderMap) -> Result<&str, ApiError> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| anyhow!(ErrorCode::Unauthenticated))?;
    ensure_api(
        token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()),
        ErrorCode::Unauthenticated,
    )?;
    Ok(token)
}
async fn identity(app: &App, headers: &HeaderMap) -> Result<(String, String), ApiError> {
    let hash = digest(&browser::credential(app, headers)?);
    let row = sqlx::query(
        "SELECT account_id,device_id FROM sessions WHERE token_hash=$1 AND expires_at>$2",
    )
    .bind(hash)
    .bind(now())
    .fetch_optional(&app.store.pool)
    .await?
    .ok_or_else(|| anyhow!(ErrorCode::Unauthenticated))?;
    Ok((row.get(0), row.get(1)))
}

async fn logout(State(app): State<App>, headers: HeaderMap) -> Result<Response, ApiError> {
    identity(&app, &headers).await?;
    sqlx::query("DELETE FROM sessions WHERE token_hash=$1")
        .bind(digest(&browser::credential(&app, &headers)?))
        .execute(&app.store.pool)
        .await?;
    Ok(browser::clear_cookie(
        &app,
        StatusCode::NO_CONTENT.into_response(),
    ))
}

fn client_source(
    peer: IpAddr,
    headers: &HeaderMap,
    trusted_proxies: &[IpAddr],
) -> Result<IpAddr, ApiError> {
    let mut source = peer;
    if trusted_proxies.contains(&source)
        && let Some(header) = headers.get("x-forwarded-for")
    {
        let text = header
            .to_str()
            .map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
        ensure_api(text.len() <= 1024, ErrorCode::MalformedRequest)?;
        let hops: Vec<_> = text.split(',').collect();
        ensure_api(hops.len() <= 16, ErrorCode::MalformedRequest)?;
        for hop in hops.iter().rev() {
            if !trusted_proxies.contains(&source) {
                break;
            }
            source = hop
                .trim()
                .parse()
                .map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
        }
    }
    Ok(source)
}

#[cfg(test)]
mod proxy_tests {
    use super::*;
    #[test]
    fn auth_005_forwarding_requires_a_trusted_peer_and_strips_only_trusted_hops() {
        let proxy: IpAddr = "10.0.0.1".parse().unwrap();
        let client: IpAddr = "192.0.2.10".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "203.0.113.9, 192.0.2.10, 10.0.0.1".parse().unwrap(),
        );
        assert_eq!(client_source(proxy, &headers, &[]).ok(), Some(proxy));
        assert_eq!(client_source(proxy, &headers, &[proxy]).ok(), Some(client));
        headers.insert("x-forwarded-for", "bad header".parse().unwrap());
        assert!(client_source(proxy, &headers, &[proxy]).is_err());
        assert_eq!(client_source(client, &headers, &[proxy]).ok(), Some(client));
    }
}

fn requires_online_sharing(command: &Command) -> bool {
    match command {
        Command::Grant { .. } | Command::Revoke { .. } => true,
        Command::CreatePerson {
            initial_policy: Some(policy),
            ..
        }
        | Command::CreateField {
            initial_policy: Some(policy),
            ..
        } => !policy.grants.is_empty(),
        _ => false,
    }
}

fn source_attempt(
    app: &App,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    let source = client_source(
        peer.map_or(
            IpAddr::from([127, 0, 0, 1]),
            |Extension(ConnectInfo(peer))| peer.ip(),
        ),
        headers,
        &app.trusted_proxies,
    )?;
    let seconds = app.started.elapsed().as_secs();
    ensure_api(
        app.source_budget
            .lock()
            .map_err(|_| anyhow!(ErrorCode::InternalError))?
            .allow(&source.to_string(), seconds),
        ErrorCode::RateLimited,
    )?;
    Ok(())
}

#[cfg(test)]
mod auth_capacity_tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    #[tokio::test]
    async fn unknown_budget_saturation_does_not_lock_out_registered_accounts() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("capacity.sqlite").display()
        ))
        .await?;
        store.migrate().await?;
        store
            .add_account(
                &Uuid::new_v4().to_string(),
                "alice",
                &hash_password("capacity-password-123".into()).await?,
            )
            .await?;
        let app = App::new(store).await?;
        for key in 0..4096 {
            {
                assert!(
                    app.unknown_principal_budget
                        .lock()
                        .unwrap()
                        .allow(&format!("{key:03x}"), 0)
                );
            }
        }
        let response=app.router().oneshot(HttpRequest::builder().method("POST").uri("/api/experimental/v1/sessions").header("content-type","application/json").body(Body::from(json!({"username":"alice","password":"capacity-password-123","device_id":"phone"}).to_string()))?).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 10000).await?;
        assert!(serde_json::from_slice::<serde_json::Value>(&body)?["access_token"].is_string());
        Ok(())
    }

    #[tokio::test]
    async fn password_queue_smooths_bursts_without_unbounded_waiters() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("queue.sqlite").display()
        ))
        .await?;
        store.migrate().await?;
        let app = App::new(store).await?;
        let first = app.password_permit().await.map_err(|e| e.0)?;
        let second = app.password_permit().await.map_err(|e| e.0)?;
        let mut waiting = Vec::new();
        for _ in 0..8 {
            let app = app.clone();
            waiting.push(tokio::spawn(async move {
                let permit = app.password_permit().await;
                assert!(permit.is_ok());
            }));
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while app.password_admission.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(waiting.iter().all(|task| !task.is_finished()));
        assert!(app.password_permit().await.is_err());
        drop((first, second));
        for task in waiting {
            task.await?;
        }
        assert_eq!(app.password_slots.available_permits(), 2);
        assert_eq!(app.password_admission.available_permits(), 10);
        Ok(())
    }
}
pub mod config;
