//! A world, a request vocabulary and paused-retirement schedules shared by the device-retirement
//! tests (`cases/devices.rs`, `cases/writers.rs`). Everything runs the real router in process on
//! the real engine; only the named hooks, which are compiled out of release builds, hold a request.
use anyhow::Result;
use atlas_core::{Store, hooks::Gate};
use atlas_server::{App, hash_password};
use axum::{Router, http::HeaderMap, http::StatusCode};
use openidconnect::PkceCodeChallenge;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::http::request;
use super::oidc::Provider;

pub(crate) const PASSWORD: &str = "device-password-123";
pub(crate) const ORIGIN: &str = "https://atlas.example";
pub(crate) const NATIVE_REDIRECT: &str = "dev.atlas.app:/oauth/callback";

pub(crate) struct World {
    pub(crate) app: Router,
    pub(crate) store: Store,
    pub(crate) alice: String,
    pub(crate) provider: Option<Provider>,
    server: Option<JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(server) = &self.server {
            server.abort();
        }
    }
}

impl World {
    /// PostgreSQL check (ai): run every retirement at REPEATABLE READ instead of READ COMMITTED.
    pub(crate) fn raise_isolation(&self) {
        self.store.hooks().raise_isolation();
    }
}

pub(crate) fn postgres() -> bool {
    std::env::var_os("ATLAS_TEST_POSTGRES_URL").is_some()
}

async fn build(browser: bool, oidc: bool) -> Result<World> {
    let (dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let hash = hash_password(PASSWORD.into()).await?;
    let alice = Uuid::new_v4().to_string();
    store.add_account(&alice, "device-alice", &hash).await?;
    store
        .add_account(&Uuid::new_v4().to_string(), "device-bob", &hash)
        .await?;
    let mut app = App::new(store.clone()).await?;
    if browser || oidc {
        app = app.public_origin(ORIGIN)?;
    }
    let (mut provider, mut server) = (None, None);
    if oidc {
        let (started, handle) = crate::support::oidc::serve().await?;
        app = app
            .oidc(&started.issuer, "atlas-test", None, false)?
            .oidc_native_redirects(vec![NATIVE_REDIRECT.into()])?;
        // Alice is already linked to the provider's subject: linking is not the writer under test.
        sqlx::query("INSERT INTO external_identities(issuer,subject,account_id) VALUES ($1,'provider-subject',$2)")
            .bind(&started.issuer)
            .bind(&alice)
            .execute(&store.pool)
            .await?;
        (provider, server) = (Some(started), Some(handle));
    }
    Ok(World {
        app: app.router(),
        store,
        alice,
        provider,
        server,
        _dir: dir,
    })
}

pub(crate) async fn world(browser: bool) -> Result<World> {
    build(browser, false).await
}

/// A world whose provider is a fake OpenID Connect server and whose native redirect is
/// [`NATIVE_REDIRECT`]; Alice is linked to it.
pub(crate) async fn world_with_oidc() -> Result<World> {
    build(false, true).await
}

/// PostgreSQL check (ai): every attempt of every retirement in the test ran at the level the test
/// asked for, and at least one reached its locking reads, so a raise that silently did nothing
/// (or a retirement that never got as far as the schedule) fails here. SQLite has no level.
pub(crate) fn assert_isolation(store: &Store, raised: bool) {
    if !postgres() {
        return;
    }
    let expected = if raised {
        "repeatable read"
    } else {
        "read committed"
    };
    let seen = store.hooks().isolation_seen();
    assert!(!seen.is_empty(), "no retirement reached its locking reads");
    assert!(seen.iter().all(|level| level == expected), "{seen:?}");
}

pub(crate) fn id() -> String {
    Uuid::new_v4().to_string()
}

pub(crate) fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

pub(crate) type Reply = (StatusCode, HeaderMap, Value);

pub(crate) async fn login(app: &Router, user: &str, device: &str) -> String {
    let (status, _, body) = request(
        app,
        "POST",
        "sessions",
        &[],
        json!({"username":user,"password":PASSWORD,"device_id":device}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body["access_token"].as_str().unwrap().to_owned()
}

/// A browser login: the cookie pair to send, and the CSRF token that goes with it.
pub(crate) async fn cookie_login(app: &Router, device: &str) -> (String, String) {
    let (status, headers, body) = request(
        app,
        "POST",
        "browser-sessions",
        &[("origin", ORIGIN)],
        json!({"username":"device-alice","password":PASSWORD,"device_id":device}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cookie = headers["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    (
        cookie.to_owned(),
        body["csrf_token"].as_str().unwrap().to_owned(),
    )
}

/// The listing entry of `device` as the caller `token` sees it, or `None` when it is not listed.
pub(crate) async fn listed(app: &Router, token: &str, device: &str) -> Option<Value> {
    let (status, _, body) = request(
        app,
        "GET",
        "devices",
        &[("authorization", &bearer(token))],
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == device)
        .cloned()
}

pub(crate) async fn device_state(app: &Router, token: &str, device: &str) -> String {
    listed(app, token, device)
        .await
        .unwrap_or_else(|| panic!("{device} is not listed"))["state_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

pub(crate) async fn session_of(app: &Router, token: &str, device: &str) -> Vec<String> {
    let (_, _, body) = request(
        app,
        "GET",
        "sessions",
        &[("authorization", &bearer(token))],
        Value::Null,
    )
    .await;
    body["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["device_id"] == device)
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect()
}

pub(crate) async fn retire(
    app: &Router,
    token: &str,
    device: &str,
    operation: &str,
    state: &str,
) -> Reply {
    request(
        app,
        "DELETE",
        &format!("devices/{device}"),
        &[
            ("authorization", &bearer(token)),
            ("idempotency-key", operation),
            ("atlas-device-state", state),
        ],
        Value::Null,
    )
    .await
}

pub(crate) async fn revoke(
    app: &Router,
    token: &str,
    session: &str,
    operation: Option<&str>,
) -> Reply {
    let auth = bearer(token);
    let mut headers = vec![("authorization", auth.as_str())];
    if let Some(operation) = operation {
        headers.push(("idempotency-key", operation));
    }
    request(
        app,
        "DELETE",
        &format!("sessions/{session}"),
        &headers,
        Value::Null,
    )
    .await
}

pub(crate) fn assert_error(reply: &Reply, status: StatusCode, code: &str) {
    assert_eq!(reply.0, status, "{}", reply.2);
    assert_eq!(reply.2["code"], code);
    assert_eq!(reply.2["details"], json!([]));
    assert_eq!(
        reply.2["request_id"],
        reply.1["x-request-id"].to_str().unwrap()
    );
    assert!(!reply.1.contains_key("set-cookie"));
}

/// Every ledger row of every account, `(operation_id, outcome)`, in operation-ID order.
pub(crate) async fn ledger(store: &Store) -> Result<Vec<(String, String)>> {
    Ok(sqlx::query_as::<_, (String, String)>(
        "SELECT operation_id,outcome FROM operation_outcomes ORDER BY operation_id",
    )
    .fetch_all(&store.pool)
    .await?)
}

// ------------------------------------------------------------------------ native and browser OIDC

/// A native OIDC flow that has been started and approved by the provider: only the callback, which
/// is the writer of the native handoff, remains to be sent.
pub(crate) struct NativeFlow {
    pub(crate) callback: String,
    cookie: String,
    pub(crate) verifier: String,
}

pub(crate) async fn native_start(w: &World, device: &str) -> NativeFlow {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut query = url::Url::parse("https://atlas.example/").unwrap();
    query
        .query_pairs_mut()
        .append_pair("device_id", device)
        .append_pair("redirect_uri", NATIVE_REDIRECT)
        .append_pair("code_challenge", challenge.as_str())
        .append_pair("state", "client-state");
    let (status, headers, _) = request(
        &w.app,
        "GET",
        &format!("oidc/native/start?{}", query.query().unwrap()),
        &[],
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = headers["location"].to_str().unwrap();
    let code = w.provider.as_ref().unwrap().approve(location, "valid");
    let state = url::Url::parse(location)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    NativeFlow {
        callback: format!("oidc/callback?state={state}&code={code}"),
        cookie: headers["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned(),
        verifier: verifier.secret().to_owned(),
    }
}

/// The native callback: the real writer of a handoff for the flow's device.
pub(crate) fn spawn_native_callback(w: &World, flow: &NativeFlow) -> JoinHandle<Reply> {
    spawn_request(
        &w.app,
        "GET",
        flow.callback.clone(),
        vec![("cookie".into(), flow.cookie.clone())],
    )
}

/// The handoff code a native callback redirected the client with.
pub(crate) fn handoff_code(reply: &Reply) -> String {
    assert_eq!(reply.0, StatusCode::SEE_OTHER, "{}", reply.2);
    let destination = url::Url::parse(reply.1["location"].to_str().unwrap()).unwrap();
    assert_eq!(destination.scheme(), "dev.atlas.app");
    destination
        .query_pairs()
        .find(|(k, _)| k == "code")
        .unwrap()
        .1
        .into_owned()
}

/// A complete native start and callback, returning the handoff code and its PKCE verifier.
pub(crate) async fn native_handoff(w: &World, device: &str) -> (String, String) {
    let flow = native_start(w, device).await;
    let reply = spawn_native_callback(w, &flow).await.unwrap();
    (handoff_code(&reply), flow.verifier)
}

/// `POST /oidc/native/exchange`: the real writer that consumes a handoff and issues a session.
pub(crate) fn spawn_native_exchange(w: &World, code: &str, verifier: &str) -> JoinHandle<Reply> {
    let app = w.app.clone();
    let body = json!({"code":code,"code_verifier":verifier});
    tokio::spawn(async move { request(&app, "POST", "oidc/native/exchange", &[], body).await })
}

/// A browser OIDC flow started and approved: the callback path and the binding cookie to send.
pub(crate) async fn browser_start(w: &World, device: &str) -> (String, String) {
    let (status, headers, body) = request(
        &w.app,
        "POST",
        "oidc/start",
        &[("origin", ORIGIN)],
        json!({"device_id":device,"link":false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let url = body["authorization_url"].as_str().unwrap();
    let code = w.provider.as_ref().unwrap().approve(url, "valid");
    let state = url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    (
        format!("oidc/callback?state={state}&code={code}"),
        headers["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned(),
    )
}

// ------------------------------------------------------------------------------- held requests

/// A request running in its own task, so a test can hold it at a hook and release it later.
pub(crate) fn spawn_request(
    app: &Router,
    method: &'static str,
    path: String,
    headers: Vec<(String, String)>,
) -> JoinHandle<Reply> {
    spawn_request_with(app, method, path, headers, Value::Null)
}

pub(crate) fn spawn_request_with(
    app: &Router,
    method: &'static str,
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
) -> JoinHandle<Reply> {
    let app = app.clone();
    tokio::spawn(async move {
        let refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        request(&app, method, &path, &refs, body).await
    })
}

pub(crate) fn retirement_headers(
    token: &str,
    operation: &str,
    state: &str,
) -> Vec<(String, String)> {
    vec![
        ("authorization".into(), bearer(token)),
        ("idempotency-key".into(), operation.into()),
        ("atlas-device-state".into(), state.into()),
    ]
}

/// Where a retirement is held so that a writer that does not take `sync_clock` commits first.
pub(crate) fn writer_first_point() -> &'static str {
    if postgres() {
        "retire.after_ledger_read"
    } else {
        "retire.before_begin"
    }
}

/// Held here, no writer of a member row can commit until the retirement does.
pub(crate) const RETIREMENT_FIRST: &str = "retire.after_locking_reads";
/// A `begin_serial()` writer needs this on both engines.
pub(crate) const BEFORE_BEGIN: &str = "retire.before_begin";

/// Prove `task` is waiting on the paused retirement (see the core tests' `assert_blocked`).
pub(crate) async fn assert_blocked<T>(
    store: &Store,
    point: &str,
    task: &mut JoinHandle<T>,
) -> Result<()> {
    if postgres() {
        let pid = store
            .hooks()
            .backend_pid(point)
            .expect("armed in-transaction point");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waits: Vec<Option<String>> = sqlx::query_scalar(
                    "SELECT wait_event_type FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))",
                )
                .bind(pid)
                .fetch_all(&store.pool)
                .await?;
                if waits.iter().any(|w| w.as_deref() == Some("Lock")) {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        assert!(!task.is_finished());
    } else {
        // The brief's bound is at most one second, well inside the 10 s busy timeout. Writers that
        // hash a password first need most of it before they reach the lock they wait on.
        assert!(
            tokio::time::timeout(Duration::from_millis(1000), &mut *task)
                .await
                .is_err(),
            "the writer must still be waiting for the retirement's write lock"
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Route {
    /// `DELETE /sessions/{id}` without an operation ID, authenticated by the retired session.
    Ordinary,
    /// `DELETE /sessions/current`.
    Current,
}

/// Alice on two devices: the `phone` to be retired and a surviving `laptop` that authenticates
/// the retirement.
pub(crate) struct Scene {
    pub(crate) w: World,
    /// Bearer tokens of the phone's first session and of the laptop's.
    pub(crate) phone: String,
    pub(crate) laptop: String,
    pub(crate) phone_session: String,
    /// The approved-state token the laptop last read for the phone.
    pub(crate) state: String,
    /// Whether retirements run at REPEATABLE READ (PostgreSQL check ai).
    pub(crate) raised: bool,
}

pub(crate) async fn scene(raise: bool) -> Result<Scene> {
    Scene::over(world(false).await?, raise).await
}

impl Scene {
    pub(crate) async fn over(w: World, raise: bool) -> Result<Scene> {
        if raise {
            w.raise_isolation();
        }
        let phone = login(&w.app, "device-alice", "phone").await;
        let laptop = login(&w.app, "device-alice", "laptop").await;
        let phone_session = session_of(&w.app, &phone, "phone").await.remove(0);
        let state = device_state(&w.app, &laptop, "phone").await;
        Ok(Scene {
            w,
            phone,
            laptop,
            phone_session,
            state,
            raised: raise,
        })
    }

    /// Re-read the phone's approved-state token after the test changed the device.
    pub(crate) async fn refresh(&mut self) {
        self.state = device_state(&self.w.app, &self.laptop, "phone").await;
    }

    pub(crate) fn writer(
        &self,
        route: Route,
        token: &str,
    ) -> (&'static str, String, Vec<(String, String)>) {
        let path = match route {
            Route::Ordinary => format!("sessions/{}", self.phone_session),
            Route::Current => "sessions/current".to_owned(),
        };
        (
            "DELETE",
            path,
            vec![("authorization".into(), bearer(token))],
        )
    }

    pub(crate) fn spawn_writer(&self, route: Route, token: &str) -> JoinHandle<Reply> {
        let (method, path, headers) = self.writer(route, token);
        spawn_request(&self.w.app, method, path, headers)
    }

    /// The retirement, authenticated by the surviving laptop, held at `point` until released.
    pub(crate) async fn hold_retirement(
        &self,
        point: &'static str,
        operation: &str,
    ) -> (Gate, JoinHandle<Reply>) {
        let mut gate = self.w.store.hooks().arm(point);
        let handle = spawn_request(
            &self.w.app,
            "DELETE",
            "devices/phone".into(),
            retirement_headers(&self.laptop, operation, &self.state),
        );
        gate.reached().await;
        (gate, handle)
    }
}
