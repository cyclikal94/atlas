//! API version signal and compatibility baseline (docs/api.md, "API version and compatibility").
//!
//! `api/compatibility.json` records the contract version, the SHA-256 of `api/openapi.json` and a
//! deterministic transcript of probed behaviour. A contract or probed-behaviour change must be
//! accompanied by a numerically greater `info.version` and a re-recorded baseline. The probes are
//! a floor, not a proof: unprobed behaviour is not detected.
use anyhow::Result;
use atlas_core::{Store, error::ErrorCode};
use atlas_server::{API_VERSION, ApiError, App};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderValue, Request, StatusCode},
    middleware,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tower::ServiceExt;
use uuid::Uuid;

const CONTRACT: &str = include_str!("../../../../api/openapi.json");
const ERROR_SOURCE: &str = include_str!("../../../core/src/error.rs");

/// Every application error code. `error_codes_are_all_probed` compares this list with the enum.
const ERROR_CODES: &[ErrorCode] = &[
    ErrorCode::AccessChanged,
    ErrorCode::BatchTooLarge,
    ErrorCode::CalendarLimit,
    ErrorCode::Conflict,
    ErrorCode::CredentialMismatch,
    ErrorCode::DefaultsChanged,
    ErrorCode::DeliveryFailed,
    ErrorCode::DeviceCapacity,
    ErrorCode::ExpansionLimit,
    ErrorCode::FetchFailed,
    ErrorCode::Forbidden,
    ErrorCode::IdentityGrantRequired,
    ErrorCode::IntegrationUnconfigured,
    ErrorCode::InternalError,
    ErrorCode::InvalidEndpoint,
    ErrorCode::InvalidIcs,
    ErrorCode::InvalidSecret,
    ErrorCode::InvalidSubscription,
    ErrorCode::InvalidValue,
    ErrorCode::InvitationExpired,
    ErrorCode::LastManager,
    ErrorCode::MalformedRequest,
    ErrorCode::MaterialisationRequired,
    ErrorCode::NotFound,
    ErrorCode::NotReady,
    ErrorCode::OidcUnavailable,
    ErrorCode::OperationConflict,
    ErrorCode::OutboundDenied,
    ErrorCode::PasswordHashFailed,
    ErrorCode::PasswordLength,
    ErrorCode::RateLimited,
    ErrorCode::RefreshInProgress,
    ErrorCode::ReminderTimeRequired,
    ErrorCode::ResyncRequired,
    ErrorCode::SecretEncryptionFailed,
    ErrorCode::SliceCapacity,
    ErrorCode::SourceConnectionRequired,
    ErrorCode::StaleDelivery,
    ErrorCode::StaleRefresh,
    ErrorCode::TemporarilyUnavailable,
    ErrorCode::Unauthenticated,
    ErrorCode::UnsupportedCalendarRule,
    ErrorCode::UnsupportedCalendarTimezone,
    ErrorCode::UnsupportedReceipt,
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Baseline {
    api_version: String,
    contract_sha256: String,
    behaviour: Value,
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The recorded baseline describes this tree.
    Current,
    /// Something changed (or the version moved backwards) without a greater `info.version`.
    NotAdvanced {
        contract_changed: bool,
        behaviour_changed: bool,
    },
    /// The version advanced: the change is permitted, but the baseline must be re-recorded.
    NotRecorded,
}

/// Strict `MAJOR.MINOR.PATCH`: numeric parts, no prefix, prerelease, metadata or leading zeros.
fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
        })
    {
        return None;
    }
    Some((
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ))
}

fn version(text: &str) -> (u64, u64, u64) {
    parse_version(text).unwrap_or_else(|| panic!("{text:?} is not MAJOR.MINOR.PATCH"))
}

fn next_minor(text: &str) -> String {
    let (major, minor, _) = version(text);
    format!("{major}.{}.0", minor + 1)
}

fn evaluate(recorded: &Baseline, current: &Baseline) -> Verdict {
    if recorded == current {
        return Verdict::Current;
    }
    if version(&current.api_version) > version(&recorded.api_version) {
        return Verdict::NotRecorded;
    }
    Verdict::NotAdvanced {
        contract_changed: recorded.contract_sha256 != current.contract_sha256,
        behaviour_changed: recorded.behaviour != current.behaviour,
    }
}

fn contract_version() -> String {
    let document: Value = serde_json::from_str(CONTRACT).expect("api/openapi.json is JSON");
    document["info"]["version"]
        .as_str()
        .expect("info.version is a string")
        .to_owned()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn baseline_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../api/compatibility.json")
}

async fn service() -> Result<(tempfile::TempDir, Store, Router)> {
    let (dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let router = App::new(store.clone()).await?.router();
    Ok((dir, store, router))
}

/// Status, the observable header subset and the body. The request ID is random, so it is checked
/// (a UUID, and equal to the error body's `request_id`) and left out of the transcript.
async fn describe(response: Response, without: &[&str]) -> Value {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let request_id = headers["x-request-id"].to_str().unwrap().to_owned();
    Uuid::parse_str(&request_id).expect("x-request-id is a UUID");
    let header = |name: &str| headers.get(name).map(|v| v.to_str().unwrap().to_owned());
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let mut body: Value = serde_json::from_slice(&bytes).unwrap();
    if let Some(id) = body.get("request_id") {
        assert_eq!(id, &json!(request_id));
    }
    for key in without {
        body.as_object_mut().unwrap().remove(*key);
    }
    json!({
        "body": body,
        "cache_control": header("cache-control"),
        "content_type": header("content-type"),
        "referrer_policy": header("referrer-policy"),
        "set_cookie": headers.contains_key("set-cookie"),
        "status": status,
    })
}

async fn probe(app: &Router, path: &str, without: &[&str]) -> Value {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .unwrap();
    describe(app.clone().oneshot(request).await.unwrap(), without).await
}

/// Every `ErrorCode` variant as the HTTP layer would render it: `[status, code]`.
/// A mapping that contradicts the contract is recorded as it is, not corrected here.
async fn error_mapping() -> Value {
    let mut mapping = BTreeMap::new();
    for code in ERROR_CODES {
        let response = ApiError::from(anyhow::anyhow!(*code)).into_response();
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        mapping.insert(code.as_str(), json!([status, body["code"]]));
    }
    serde_json::to_value(mapping).unwrap()
}

/// Exercise the combined content-write and retirement contract in one account.
/// Keep random identities out of the baseline; assertions check the actual stored
/// state and durable replay before recording observable status/code/outcome headers.
async fn content_and_retirement() -> Result<Value> {
    use crate::support::http::request;
    use crate::support::retirement::{Reply, bearer, device_state, id, login, retire, world};

    fn observed(reply: &Reply) -> Value {
        json!({
            "status": reply.0.as_u16(),
            "code": reply.2.get("code"),
            "outcome": reply.2.get("outcome"),
            "set_cookie": reply.1.contains_key("set-cookie"),
            "cache_control": reply.1.get("cache-control").map(|value| value.to_str().unwrap()),
        })
    }

    let w = world(false).await?;
    let phone = login(&w.app, "device-alice", "phone").await;
    let backup = login(&w.app, "device-alice", "backup").await;
    let auth = bearer(&phone);
    let (person, field) = (id(), id());
    let created = request(
        &w.app,
        "POST",
        "commands",
        &[("authorization", &auth), ("idempotency-key", &id())],
        json!({"commands":[
            {"kind":"create_person","id":person,"name":"Morgan"},
            {"kind":"create_field","id":field,"person_id":person,"label":"Hobby","value":"Music"}
        ]}),
    )
    .await;
    assert_eq!(created.0, StatusCode::OK);
    let mut results = serde_json::Map::new();
    for (name, policy) in [
        ("omitted_policy", None),
        ("stale_policy", Some(0)),
        ("current_policy", Some(1)),
    ] {
        let mut edit = json!({"kind":"edit","id":field,"expected_version":1,"label":"Updated","value":"Surfing"});
        if let Some(version) = policy {
            edit["expected_policy_version"] = json!(version);
        }
        let reply = request(
            &w.app,
            "POST",
            "commands",
            &[("authorization", &auth), ("idempotency-key", &id())],
            json!({"commands":[edit]}),
        )
        .await;
        let expected = if policy == Some(1) {
            StatusCode::OK
        } else {
            StatusCode::CONFLICT
        };
        assert_eq!(reply.0, expected, "{name}");
        if expected == StatusCode::CONFLICT {
            assert_eq!(reply.2["code"], "conflict");
        }
        assert!(!reply.1.contains_key("set-cookie"));
        let stored: (String, i64) =
            sqlx::query_as("SELECT label,version FROM resources WHERE id=$1")
                .bind(&field)
                .fetch_one(&w.store.pool)
                .await?;
        assert_eq!(
            stored,
            if expected == StatusCode::OK {
                ("Updated".into(), 2)
            } else {
                ("Hobby".into(), 1)
            }
        );
        results.insert(name.into(), observed(&reply));
    }

    // R1: a subscription that was already inactive before the retirement is not itself part of
    // the approved state, but `retire_members`'s cleanup statement still erases its secret and
    // advances its version. That version is externally visible through
    // `GET /notification-subscriptions` and gates `expected_version` acceptance on the row, so it
    // belongs in this floor, not only in a core-level test.
    let leaked_subscription = id();
    sqlx::query(
        "INSERT INTO notification_subscriptions(id,account_id,device_id,transport,secret,version,active) VALUES ($1,$2,$3,'webpush','leaked',1,0)",
    )
    .bind(&leaked_subscription)
    .bind(&w.alice)
    .bind("phone")
    .execute(&w.store.pool)
    .await?;

    let stale = device_state(&w.app, &phone, "phone").await;
    let _second = login(&w.app, "device-alice", "phone").await;
    let rejected = retire(&w.app, &backup, "phone", &id(), &stale).await;
    assert_eq!(rejected.0, StatusCode::CONFLICT);
    assert_eq!(rejected.2["outcome"], "rejected_stale");
    results.insert("stale_retirement".into(), observed(&rejected));

    let fresh = device_state(&w.app, &phone, "phone").await;
    let operation = id();
    let applied = retire(&w.app, &phone, "phone", &operation, &fresh).await;
    assert_eq!(applied.0, StatusCode::OK);
    assert_eq!(applied.2["outcome"], "confirmed_applied");
    assert!(!applied.1.contains_key("set-cookie"));
    let replay = retire(&w.app, &backup, "phone", &operation, &fresh).await;
    assert_eq!(replay.0, applied.0);
    assert_eq!(
        replay.2, applied.2,
        "another session recovers the same durable outcome"
    );
    results.insert("self_retirement".into(), observed(&applied));
    results.insert("retirement_replay".into(), observed(&replay));

    // R1 continued: confirm the erasure and version advance are observable through the real
    // router, not only by direct query, and that the new version now gates command acceptance.
    // `phone`'s own session was just revoked by its self-retirement, so read as `backup`, still
    // live on the same account.
    let backup_auth = bearer(&backup);
    let leaked_after = request(
        &w.app,
        "GET",
        "notification-subscriptions",
        &[("authorization", &backup_auth)],
        Value::Null,
    )
    .await;
    assert_eq!(leaked_after.0, StatusCode::OK);
    let leaked_row = leaked_after.2["subscriptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == leaked_subscription)
        .expect("the already-inactive, secret-bearing subscription must still be listed")
        .clone();
    assert_eq!(leaked_row["enabled"], false);
    assert_eq!(
        leaked_row["version"], 2,
        "R1: retirement must advance the version of an already-inactive subscription whose \
         secret it erases"
    );
    results.insert(
        "inactive_subscription_retirement".into(),
        json!({
            "version_before": 1,
            "version_after": leaked_row["version"].clone(),
            "enabled": leaked_row["enabled"].clone(),
        }),
    );

    let stale_subscription_command = request(
        &w.app,
        "POST",
        "reminder-commands",
        &[("authorization", &backup_auth), ("idempotency-key", &id())],
        json!({"command":{"kind":"remove_subscription","id":leaked_subscription,"expected_version":1}}),
    )
    .await;
    assert_eq!(stale_subscription_command.0, StatusCode::CONFLICT);
    assert_eq!(stale_subscription_command.2["code"], "conflict");
    results.insert(
        "inactive_subscription_stale_command".into(),
        json!({
            "status": stale_subscription_command.0.as_u16(),
            "code": stale_subscription_command.2["code"],
        }),
    );

    let after_retirement = request(
        &w.app,
        "POST",
        "commands",
        &[("authorization", &auth), ("idempotency-key", &id())],
        json!({"commands":[{"kind":"edit","id":field,"expected_version":2,
            "expected_policy_version":1,"label":"Forbidden change","value":"Changed"}]}),
    )
    .await;
    assert_eq!(after_retirement.0, StatusCode::UNAUTHORIZED);
    let stored: (String, i64) = sqlx::query_as("SELECT label,version FROM resources WHERE id=$1")
        .bind(&field)
        .fetch_one(&w.store.pool)
        .await?;
    assert_eq!(stored, ("Updated".into(), 2));
    results.insert("write_after_retirement".into(), observed(&after_retirement));
    Ok(Value::Object(results))
}

/// BE-Q19 activation-grant probes (plan §5.4): grant-only browser login, activation,
/// cancellation and a real `credential_mismatch` response. Records cookie presence/absence and
/// the normalised response shape (status, error code, sorted field names) — never the grant,
/// verifier, account ID or a timestamp, all of which are random or real-clock and would make the
/// recorded transcript non-deterministic.
async fn activation_probes() -> Result<Value> {
    use crate::support::http::request;
    use crate::support::retirement::{
        ORIGIN, PASSWORD, activate, activate_cancel, attempt, cookie_login, id, world,
    };

    fn shape(reply: &(StatusCode, axum::http::HeaderMap, Value)) -> Value {
        let mut fields: Vec<String> = reply
            .2
            .as_object()
            .map(|object| object.keys().cloned().collect())
            .unwrap_or_default();
        fields.sort();
        json!({
            "status": reply.0.as_u16(),
            "code": reply.2.get("code"),
            "set_cookie": reply.1.contains_key("set-cookie"),
            "fields": fields,
        })
    }

    let w = world(true).await?;
    let mut results = serde_json::Map::new();

    let (verifier, challenge) = attempt();
    let granted = request(
        &w.app,
        "POST",
        "browser-sessions",
        &[("origin", ORIGIN)],
        json!({"username":"device-alice","password":PASSWORD,"device_id":"compat-probe",
               "attempt_challenge":challenge}),
    )
    .await;
    assert_eq!(granted.0, StatusCode::OK, "{}", granted.2);
    assert!(
        !granted.1.contains_key("set-cookie"),
        "login issues no cookie"
    );
    results.insert("grant_only_login".into(), shape(&granted));

    let grant = granted.2["grant"].as_str().unwrap().to_owned();
    let activated = activate(&w.app, &grant, &verifier).await;
    assert_eq!(activated.0, StatusCode::OK, "{}", activated.2);
    assert!(
        activated.1.contains_key("set-cookie"),
        "activate is the only cookie writer"
    );
    results.insert("activate".into(), shape(&activated));

    let (cancel_verifier, cancel_challenge) = attempt();
    let cancel_granted = request(
        &w.app,
        "POST",
        "browser-sessions",
        &[("origin", ORIGIN)],
        json!({"username":"device-alice","password":PASSWORD,"device_id":"compat-probe-cancel",
               "attempt_challenge":cancel_challenge}),
    )
    .await;
    assert_eq!(cancel_granted.0, StatusCode::OK, "{}", cancel_granted.2);
    let cancelled = activate_cancel(&w.app, &cancel_verifier).await;
    assert_eq!(cancelled.0, StatusCode::OK, "{}", cancelled.2);
    assert!(!cancelled.1.contains_key("set-cookie"));
    assert_eq!(cancelled.2["result"], "not_activated");
    results.insert("activate_cancel".into(), shape(&cancelled));

    let (cookie_a, _) = cookie_login(&w.app, "compat-probe-a").await;
    let (_cookie_b, csrf_b) = cookie_login(&w.app, "compat-probe-b").await;
    let mismatch = request(
        &w.app,
        "POST",
        "commands",
        &[("cookie", &cookie_a), ("x-csrf-token", &csrf_b)],
        json!({"commands":[{"kind":"create_person","id":id(),"name":"Compat probe"}]}),
    )
    .await;
    assert_eq!(mismatch.0, StatusCode::FORBIDDEN, "{}", mismatch.2);
    assert_eq!(mismatch.2["code"], "credential_mismatch");
    results.insert("credential_mismatch".into(), shape(&mismatch));

    Ok(Value::Object(results))
}

/// BE-Q22 probes: `configure_source`'s `connection` field distinguishes omission (preserve),
/// `Some(value)` (replace) and `disconnect: true` (clear) (`docs/api.md` `0.17.0` entry).
/// Uses a real, allow-listed loopback origin and a real integration key so the stored
/// `connection` column holds a genuinely sealed value, not a placeholder string — the DNS
/// lookup on `127.0.0.1` resolves with no live listener required, since `configure_source`
/// never calls `fetch()`. Only status/code are recorded; the sealed ciphertext is randomised
/// per call and would make the transcript non-deterministic.
async fn calendar_configure_probes() -> Result<Value> {
    use crate::support::http::request;
    use atlas_server::{hash_password, integrations::IntegrationConfig};

    fn shape(reply: &(StatusCode, axum::http::HeaderMap, Value)) -> Value {
        json!({"status": reply.0.as_u16(), "code": reply.2.get("code")})
    }

    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = Uuid::new_v4().to_string();
    store
        .add_account(
            &actor,
            "calendar-configure-probe",
            &hash_password("calendar-configure-probe-123".into()).await?,
        )
        .await?;
    let origin = "http://127.0.0.1:59998".to_string();
    let app = App::new(store.clone())
        .await?
        .integration_config(IntegrationConfig::new(
            Some(&"05".repeat(32)),
            vec![origin.clone()],
            None,
            None,
        )?)
        .router();
    let login = request(
        &app,
        "POST",
        "sessions",
        &[],
        json!({"username":"calendar-configure-probe","password":"calendar-configure-probe-123","device_id":"probe"}),
    )
    .await;
    assert_eq!(login.0, StatusCode::OK, "{}", login.2);
    let token = login.2["access_token"].as_str().unwrap().to_owned();
    let auth = format!("Bearer {token}");

    let mut results = serde_json::Map::new();
    let source = Uuid::new_v4().to_string();
    let created = request(
        &app,
        "POST",
        "calendar-commands",
        &[
            ("authorization", &auth),
            ("idempotency-key", &Uuid::new_v4().to_string()),
        ],
        json!({"command":{"kind":"create_source","id":source,"label":"Calendar","timezone":"UTC",
            "connection":{"url":format!("{origin}/feed"),"bearer":"probe-secret-token"}}}),
    )
    .await;
    assert_eq!(created.0, StatusCode::OK, "{}", created.2);
    results.insert("create_with_connection".into(), shape(&created));

    let sealed: Option<String> =
        sqlx::query_scalar("SELECT connection FROM calendar_sources WHERE id=$1")
            .bind(&source)
            .fetch_one(&store.pool)
            .await?;
    let sealed = sealed.expect("connection sealed at creation");

    let omitted = request(
        &app,
        "POST",
        "calendar-commands",
        &[
            ("authorization", &auth),
            ("idempotency-key", &Uuid::new_v4().to_string()),
        ],
        json!({"command":{"kind":"configure_source","id":source,"expected_version":2,
            "timezone":"Europe/London","enabled":true}}),
    )
    .await;
    assert_eq!(omitted.0, StatusCode::OK, "{}", omitted.2);
    let stored: Option<String> =
        sqlx::query_scalar("SELECT connection FROM calendar_sources WHERE id=$1")
            .bind(&source)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(
        stored,
        Some(sealed.clone()),
        "omission preserves the stored connection"
    );
    results.insert("omitted_preserves".into(), shape(&omitted));

    let contradiction = request(
        &app,
        "POST",
        "calendar-commands",
        &[
            ("authorization", &auth),
            ("idempotency-key", &Uuid::new_v4().to_string()),
        ],
        json!({"command":{"kind":"configure_source","id":source,"expected_version":3,
            "timezone":"UTC","connection":{"url":format!("{origin}/feed"),"bearer":"other-secret"},
            "disconnect":true,"enabled":true}}),
    )
    .await;
    assert_eq!(
        contradiction.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        contradiction.2
    );
    assert_eq!(contradiction.2["code"], "invalid_value");
    let stored: Option<String> =
        sqlx::query_scalar("SELECT connection FROM calendar_sources WHERE id=$1")
            .bind(&source)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(
        stored,
        Some(sealed),
        "rejected combination leaves state untouched"
    );
    results.insert(
        "connection_and_disconnect_rejected".into(),
        shape(&contradiction),
    );

    let disconnected = request(
        &app,
        "POST",
        "calendar-commands",
        &[
            ("authorization", &auth),
            ("idempotency-key", &Uuid::new_v4().to_string()),
        ],
        json!({"command":{"kind":"configure_source","id":source,"expected_version":3,
            "timezone":"UTC","disconnect":true,"enabled":true}}),
    )
    .await;
    assert_eq!(disconnected.0, StatusCode::OK, "{}", disconnected.2);
    let stored: Option<String> =
        sqlx::query_scalar("SELECT connection FROM calendar_sources WHERE id=$1")
            .bind(&source)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(
        stored, None,
        "explicit disconnect clears the stored connection"
    );
    results.insert("disconnect_clears".into(), shape(&disconnected));

    Ok(Value::Object(results))
}

/// BE-CH1 command-batch probes (plan.md, "Command batch schema and implementation
/// agreement"): the reconciled `/commands` ceiling accepts exactly the schema's `maxItems`
/// commands and rejects one over it. One endpoint (content) is the baseline floor;
/// `commands.rs` is the exhaustive, non-probe coverage across all three batch endpoints.
/// Records only the deterministic status/code shape; the created person IDs are randomised
/// and would make the transcript non-deterministic.
async fn command_batch_probes() -> Result<Value> {
    use crate::support::http::request;
    use atlas_server::hash_password;

    fn shape(reply: &(StatusCode, axum::http::HeaderMap, Value)) -> Value {
        json!({"status": reply.0.as_u16(), "code": reply.2.get("code")})
    }

    fn batch(count: usize) -> Value {
        json!({
            "commands": (0..count)
                .map(|_| json!({"kind":"create_person","id":Uuid::new_v4().to_string(),"name":"Probe"}))
                .collect::<Vec<_>>()
        })
    }

    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = Uuid::new_v4().to_string();
    store
        .add_account(
            &actor,
            "command-batch-probe",
            &hash_password("command-batch-probe-123".into()).await?,
        )
        .await?;
    let app = App::new(store.clone()).await?.router();
    let login = request(
        &app,
        "POST",
        "sessions",
        &[],
        json!({"username":"command-batch-probe","password":"command-batch-probe-123","device_id":"probe"}),
    )
    .await;
    assert_eq!(login.0, StatusCode::OK, "{}", login.2);
    let token = login.2["access_token"].as_str().unwrap().to_owned();
    let auth = format!("Bearer {token}");

    let mut results = serde_json::Map::new();
    let accepted = request(
        &app,
        "POST",
        "commands",
        &[
            ("authorization", &auth),
            ("idempotency-key", &Uuid::new_v4().to_string()),
        ],
        batch(20),
    )
    .await;
    assert_eq!(accepted.0, StatusCode::OK, "{}", accepted.2);
    results.insert("maximum_batch".into(), shape(&accepted));

    let rejected = request(
        &app,
        "POST",
        "commands",
        &[
            ("authorization", &auth),
            ("idempotency-key", &Uuid::new_v4().to_string()),
        ],
        batch(21),
    )
    .await;
    assert_eq!(
        rejected.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        rejected.2
    );
    assert_eq!(rejected.2["code"], "invalid_value");
    results.insert("over_limit_batch".into(), shape(&rejected));

    Ok(Value::Object(results))
}

/// The combined probe transcript includes activation, calendar connection preservation and
/// disconnection, inactive-subscription retirement/version changes, and command batch-size
/// acceptance/rejection. `/health` omits `api_version`, which is asserted separately.
async fn observe(app: &Router) -> Value {
    json!({
        "activation": activation_probes().await.expect("activation-grant protocol probes"),
        "calendar_configure": calendar_configure_probes().await.expect("BE-Q22 connection preserve/disconnect probes"),
        "command_batch": command_batch_probes().await.expect("BE-CH1 command batch-size probes"),
        "error_mapping": error_mapping().await,
        "content_and_retirement": content_and_retirement().await.expect("combined protocol probes"),
        "health": probe(app, "/health", &["api_version"]).await,
        "me_unauthenticated": probe(app, "/api/experimental/v1/me", &["request_id"]).await,
        "ready": probe(app, "/ready", &[]).await,
    })
}

async fn current(app: &Router) -> Baseline {
    Baseline {
        api_version: contract_version(),
        contract_sha256: hex(&Sha256::digest(CONTRACT.as_bytes())),
        behaviour: observe(app).await,
    }
}

fn read_recorded() -> Option<Baseline> {
    let text = std::fs::read_to_string(baseline_path()).ok()?;
    Some(serde_json::from_str(&text).expect("api/compatibility.json is a baseline"))
}

fn record(baseline: &Baseline) {
    let mut text = serde_json::to_string_pretty(baseline).unwrap();
    text.push('\n');
    std::fs::write(baseline_path(), text).unwrap();
}

/// The live gate, and record mode (`ATLAS_RECORD_COMPATIBILITY=1`), which never overwrites a
/// baseline that has changed without a greater `info.version`.
#[tokio::test]
async fn baseline_matches_recorded_contract_and_behaviour() -> Result<()> {
    let (_dir, _store, router) = service().await?;
    let current = current(&router).await;
    let recording = std::env::var("ATLAS_RECORD_COMPATIBILITY").is_ok_and(|value| value == "1");
    let hint = "Record with: ATLAS_RECORD_COMPATIBILITY=1 cargo test --locked -p atlas-server --test api compatibility";
    match read_recorded() {
        None if recording => record(&current),
        None => panic!("api/compatibility.json is missing. {hint}"),
        Some(recorded) => match evaluate(&recorded, &current) {
            Verdict::Current => {}
            Verdict::NotRecorded if recording => record(&current),
            Verdict::NotRecorded => panic!(
                "info.version advanced from {} to {} but the baseline is stale. {hint}",
                recorded.api_version, current.api_version
            ),
            Verdict::NotAdvanced {
                contract_changed,
                behaviour_changed,
            } => panic!(
                "api/openapi.json or probed behaviour changed without a greater info.version \
                 (recorded {}, current {}; contract changed: {contract_changed}, behaviour \
                 changed: {behaviour_changed}). Advance info.version as described in docs/api.md, \
                 then record. Record mode refuses to overwrite this state.",
                recorded.api_version, current.api_version
            ),
        },
    }
    Ok(())
}

fn baseline(api_version: &str, contract: &str, behaviour: Value) -> Baseline {
    Baseline {
        api_version: api_version.into(),
        contract_sha256: contract.into(),
        behaviour,
    }
}

#[test]
fn version_advance_rules() {
    let base = baseline("0.13.0", "aa", json!({"probe": 1}));
    let not_advanced = |contract_changed, behaviour_changed| Verdict::NotAdvanced {
        contract_changed,
        behaviour_changed,
    };
    // Identical.
    assert_eq!(evaluate(&base, &base.clone()), Verdict::Current);
    // Behaviour, contract, or both changed at the same version.
    let behaviour = baseline("0.13.0", "aa", json!({"probe": 2}));
    assert_eq!(evaluate(&base, &behaviour), not_advanced(false, true));
    let contract = baseline("0.13.0", "bb", json!({"probe": 1}));
    assert_eq!(evaluate(&base, &contract), not_advanced(true, false));
    let both = baseline("0.13.0", "bb", json!({"probe": 2}));
    assert_eq!(evaluate(&base, &both), not_advanced(true, true));
    // A lower version is never an advance, with or without other changes.
    let lower = baseline("0.12.0", "bb", json!({"probe": 2}));
    assert_eq!(evaluate(&base, &lower), not_advanced(true, true));
    let lower_alone = baseline("0.12.9", "aa", json!({"probe": 1}));
    assert_eq!(evaluate(&base, &lower_alone), not_advanced(false, false));
    // A greater version permits the change but requires re-recording, including a bump alone.
    let advanced = baseline("0.13.1", "bb", json!({"probe": 2}));
    assert_eq!(evaluate(&base, &advanced), Verdict::NotRecorded);
    let bump_alone = baseline("0.14.0", "aa", json!({"probe": 1}));
    assert_eq!(evaluate(&base, &bump_alone), Verdict::NotRecorded);
    // Components compare numerically, never as text.
    let ten = baseline("0.10.0", "aa", json!({"probe": 1}));
    let nine = baseline("0.9.0", "aa", json!({"probe": 1}));
    assert_eq!(evaluate(&nine, &ten), Verdict::NotRecorded);
    assert_eq!(evaluate(&ten, &nine), not_advanced(false, false));
    // The version grammar.
    assert_eq!(parse_version("0.13.0"), Some((0, 13, 0)));
    for malformed in [
        "",
        "0.13",
        "v0.13.0",
        "0.13.0-rc.1",
        "0.13.0+b",
        "0.013.0",
        "00.1.0",
        "1.2.3.4",
        " 0.1.0",
        "0.1.0 ",
        "0.-1.0",
        "0..0",
        "a.b.c",
    ] {
        assert_eq!(parse_version(malformed), None, "{malformed:?}");
    }
}

/// The card's acceptance check: an externally observable behaviour changes with the contract
/// untouched, and the documented rule demands a version advance.
#[tokio::test]
async fn behaviour_only_change_needs_a_version_bump() -> Result<()> {
    let (_dir, _store, router) = service().await?;
    let recorded = current(&router).await;
    // The response policy header is observable to every client and is not in the contract.
    let mutated_router = router.clone().layer(middleware::map_response(
        |mut response: Response| async move {
            response
                .headers_mut()
                .insert("referrer-policy", HeaderValue::from_static("origin"));
            response
        },
    ));
    let mutated = current(&mutated_router).await;
    assert_eq!(mutated.contract_sha256, recorded.contract_sha256);
    assert_eq!(mutated.api_version, recorded.api_version);
    assert_ne!(mutated.behaviour, recorded.behaviour);
    // Same version: rejected, and the verdict names behaviour (not the contract) as the cause.
    assert_eq!(
        evaluate(&recorded, &mutated),
        Verdict::NotAdvanced {
            contract_changed: false,
            behaviour_changed: true
        }
    );
    // A greater version satisfies the rule; the baseline must then be re-recorded.
    let bumped = Baseline {
        api_version: next_minor(&recorded.api_version),
        ..mutated
    };
    assert_eq!(evaluate(&recorded, &bumped), Verdict::NotRecorded);
    // The unmodified router is still described by the baseline.
    assert_eq!(
        evaluate(&recorded, &current(&router).await),
        Verdict::Current
    );
    Ok(())
}

#[test]
fn error_codes_are_all_probed() {
    let declared: Vec<String> = ERROR_SOURCE
        .split("pub enum ErrorCode {")
        .nth(1)
        .expect("ErrorCode enum")
        .split("\n}")
        .next()
        .unwrap()
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_suffix(','))
        .filter(|name| !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(str::to_owned)
        .collect();
    let mut probed: Vec<String> = ERROR_CODES.iter().map(|code| format!("{code:?}")).collect();
    let mut declared_sorted = declared.clone();
    declared_sorted.sort();
    probed.sort();
    assert!(!declared.is_empty());
    assert_eq!(
        probed, declared_sorted,
        "list every ErrorCode variant in ERROR_CODES so its HTTP mapping is part of the baseline"
    );
}

#[tokio::test]
async fn health_reports_the_contract_version() -> Result<()> {
    let (_dir, store, router) = service().await?;
    // No credentials, cookie or CSRF header: this must work before any sign-in.
    let request = || {
        Request::builder()
            .method("GET")
            .uri("/health")
            .body(Body::empty())
            .unwrap()
    };
    let response = router.clone().oneshot(request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key("set-cookie"));
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    assert_eq!(response.headers()["content-type"], "application/json");
    Uuid::parse_str(response.headers()["x-request-id"].to_str()?)?;
    let bytes = to_bytes(response.into_body(), 1024).await?;
    let body: Value = serde_json::from_slice(&bytes)?;
    let object = body.as_object().unwrap();
    assert_eq!(object.len(), 2);
    assert_eq!(body["status"], "ok");
    // Exactly the contract's own version, in the strict grammar.
    assert_eq!(body["api_version"], API_VERSION);
    assert_eq!(body["api_version"], contract_version());
    assert!(parse_version(API_VERSION).is_some());
    // Liveness is independent of the database; readiness is not.
    store.pool.close().await;
    let response = router.clone().oneshot(request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let after: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1024).await?)?;
    assert_eq!(after, body);
    let ready = probe(&router, "/ready", &["request_id"]).await;
    assert_eq!(ready["status"], StatusCode::SERVICE_UNAVAILABLE.as_u16());
    Ok(())
}
