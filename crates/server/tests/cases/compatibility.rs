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
    ErrorCode::ConnectionUnavailable,
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

/// BE-B8: safe, distinguishable calendar-refresh error codes, reached through the real
/// `refresh_link` call path, not merely present in the OpenAPI enumeration. Covers
/// `connection_unavailable` (cause 1, no connection at all), `integration_unconfigured` (cause
/// 3: a connection sealed while the server had a key, refreshed once the key becomes unset —
/// two `App` instances sharing one store, matching a real server restart with a changed
/// `ATLAS_SECRET_KEY`) and `stale_refresh` with both a `generation_changed` reason (cause 4: a
/// real fetch held in flight by a delayed loopback feed, raced by a concurrent settings edit)
/// and a `lease_expired` reason (cause 5: the same in-flight fetch, but only the lease itself
/// is forced past expiry, with no settings edit and no generation change).
///
/// Also records the read-time `refresh_in_progress` field on `GET /calendar-sources`, each
/// observation asserted as well as recorded, at these points:
/// - cause 4's source: `false` before any refresh; `true` while its fetch is held in flight;
///   `false` immediately after the configuration edit, *while the superseded provider response is
///   still held*; and `false` once the rejected attempt has completed;
/// - cause 5's source: `true` while its fetch is held, then `false` once only `lease_until` has
///   been forced into the past — again read before the provider response is released, so the
///   expired read is not inferred from the later `lease_expired` rejection.
///
/// Before this coverage none of these read-time transitions was part of the enforced behaviour
/// floor, so the field's semantics (including which clock the handler compares the lease with)
/// could change without the version-bump discipline this baseline exists to enforce. Records
/// only deterministic status/code/reason/`refresh_in_progress` shape, never a sealed ciphertext
/// or random ID.
async fn calendar_refresh_errors_probes() -> Result<Value> {
    use crate::support::http::{delayed_feed, request};
    use atlas_server::{hash_password, integrations::IntegrationConfig};

    fn shape(reply: &(StatusCode, axum::http::HeaderMap, Value)) -> Value {
        json!({"status": reply.0.as_u16(), "code": reply.2.get("code"), "reason": reply.2.get("reason")})
    }
    /// Reads `refresh_in_progress` for one source off `GET /calendar-sources` and returns its
    /// recorded shape, asserting the expected value so a wrong observation fails the probe itself
    /// as well as changing the recorded transcript.
    async fn progress(app: &Router, auth: &str, id: &str, expected: bool) -> Value {
        let list = request(
            app,
            "GET",
            "calendar-sources",
            &[("authorization", auth)],
            Value::Null,
        )
        .await;
        assert_eq!(list.0, StatusCode::OK, "{}", list.2);
        let observed = list.2["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == id)
            .and_then(|v| v["value"]["refresh_in_progress"].as_bool());
        assert_eq!(
            observed,
            Some(expected),
            "refresh_in_progress observation for {id}"
        );
        json!({"status": list.0.as_u16(), "refresh_in_progress": observed})
    }

    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = Uuid::new_v4().to_string();
    store
        .add_account(
            &actor,
            "calendar-refresh-errors-probe",
            &hash_password("calendar-refresh-errors-probe-123".into()).await?,
        )
        .await?;
    let origin = "http://127.0.0.1:59997".to_string();
    let key = "0d".repeat(32);
    let app = App::new(store.clone())
        .await?
        .integration_config(IntegrationConfig::new(
            Some(&key),
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
        json!({"username":"calendar-refresh-errors-probe","password":"calendar-refresh-errors-probe-123","device_id":"probe"}),
    )
    .await;
    assert_eq!(login.0, StatusCode::OK, "{}", login.2);
    let token = login.2["access_token"].as_str().unwrap().to_owned();
    let auth = format!("Bearer {token}");

    let mut results = serde_json::Map::new();

    // Cause 1: no connection at all.
    let source = Uuid::new_v4().to_string();
    let created = request(
        &app,
        "POST",
        "calendar-commands",
        &[
            ("authorization", &auth),
            ("idempotency-key", &Uuid::new_v4().to_string()),
        ],
        json!({"command":{"kind":"create_source","id":source,"label":"Calendar","timezone":"UTC"}}),
    )
    .await;
    assert_eq!(created.0, StatusCode::OK, "{}", created.2);
    let unavailable = request(
        &app,
        "POST",
        &format!("calendar-sources/{source}/refresh"),
        &[
            ("authorization", &auth),
            ("idempotency-key", &Uuid::new_v4().to_string()),
        ],
        json!({}),
    )
    .await;
    assert_eq!(
        unavailable.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        unavailable.2
    );
    assert_eq!(unavailable.2["code"], "connection_unavailable");
    results.insert("connection_unavailable".into(), shape(&unavailable));

    // Cause 3: sealed while this server had a key, refreshed through a second `App` sharing
    // the same store but with no key configured.
    let unconfigured_source = Uuid::new_v4().to_string();
    let create_unconfigured = request(
        &app,
        "POST",
        "calendar-commands",
        &[
            ("authorization", &auth),
            ("idempotency-key", &Uuid::new_v4().to_string()),
        ],
        json!({"command":{"kind":"create_source","id":unconfigured_source,"label":"Calendar","timezone":"UTC",
            "connection":{"url":format!("{origin}/feed"),"bearer":"probe-secret"}}}),
    )
    .await;
    assert_eq!(
        create_unconfigured.0,
        StatusCode::OK,
        "{}",
        create_unconfigured.2
    );
    let unkeyed_app = App::new(store.clone())
        .await?
        .integration_config(IntegrationConfig::new(
            None,
            vec![origin.clone()],
            None,
            None,
        )?)
        .router();
    let unconfigured = request(
        &unkeyed_app,
        "POST",
        &format!("calendar-sources/{unconfigured_source}/refresh"),
        &[
            ("authorization", &auth),
            ("idempotency-key", &Uuid::new_v4().to_string()),
        ],
        json!({}),
    )
    .await;
    assert_eq!(
        unconfigured.0,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        unconfigured.2
    );
    assert_eq!(unconfigured.2["code"], "integration_unconfigured");
    results.insert("integration_unconfigured".into(), shape(&unconfigured));

    // Cause 4: a real fetch held in flight by a delayed loopback feed, raced by a concurrent
    // settings edit that bumps `generation` before the response is released.
    let (feed_origin, mut arrived_rx, release_tx, feed_server) = delayed_feed().await?;
    let stale_app = App::new(store.clone())
        .await?
        .integration_config(IntegrationConfig::new(
            Some(&key),
            vec![feed_origin.clone()],
            None,
            None,
        )?)
        .router();
    let stale_source = Uuid::new_v4().to_string();
    let create_stale = request(
        &stale_app,
        "POST",
        "calendar-commands",
        &[
            ("authorization", &auth),
            ("idempotency-key", &Uuid::new_v4().to_string()),
        ],
        json!({"command":{"kind":"create_source","id":stale_source,"label":"Calendar","timezone":"UTC",
            "connection":{"url":format!("{feed_origin}/feed"),"bearer":"probe-secret"}}}),
    )
    .await;
    assert_eq!(create_stale.0, StatusCode::OK, "{}", create_stale.2);
    results.insert(
        "refresh_in_progress_before".into(),
        progress(&stale_app, &auth, &stale_source, false).await,
    );
    let refresh_task = {
        let stale_app = stale_app.clone();
        let auth = auth.clone();
        let stale_source = stale_source.clone();
        tokio::spawn(async move {
            request(
                &stale_app,
                "POST",
                &format!("calendar-sources/{stale_source}/refresh"),
                &[
                    ("authorization", &auth),
                    ("idempotency-key", &Uuid::new_v4().to_string()),
                ],
                json!({}),
            )
            .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), arrived_rx.recv())
        .await?
        .expect("feed connection");
    results.insert(
        "refresh_in_progress_during".into(),
        progress(&stale_app, &auth, &stale_source, true).await,
    );
    let version: i64 = sqlx::query_scalar("SELECT version FROM resources WHERE id=$1")
        .bind(&stale_source)
        .fetch_one(&store.pool)
        .await?;
    let edit = request(
        &stale_app,
        "POST",
        "calendar-commands",
        &[
            ("authorization", &auth),
            ("idempotency-key", &Uuid::new_v4().to_string()),
        ],
        json!({"command":{"kind":"configure_source","id":stale_source,"expected_version":version,
            "timezone":"Europe/London","enabled":true}}),
    )
    .await;
    assert_eq!(edit.0, StatusCode::OK, "{}", edit.2);
    // The edit cleared the lease, so the flag is already false while the superseded provider
    // response is still held — read before that response is released.
    results.insert(
        "refresh_in_progress_configuration_changed_held".into(),
        progress(&stale_app, &auth, &stale_source, false).await,
    );
    release_tx.send(()).ok();
    let stale = tokio::time::timeout(std::time::Duration::from_secs(5), refresh_task)
        .await?
        .expect("refresh task panicked");
    assert_eq!(stale.0, StatusCode::CONFLICT, "{}", stale.2);
    assert_eq!(stale.2["code"], "stale_refresh");
    assert_eq!(stale.2["reason"], "generation_changed");
    results.insert("stale_refresh_generation_changed".into(), shape(&stale));
    results.insert(
        "refresh_in_progress_after".into(),
        progress(&stale_app, &auth, &stale_source, false).await,
    );
    feed_server.abort();

    // Cause 5: the identical in-flight fetch, but only the lease itself is forced past expiry
    // (direct SQL, for test speed — a real 60-second wait would prove nothing a controlled
    // value does not), with no settings edit and no generation change.
    let (lease_origin, mut lease_arrived_rx, lease_release_tx, lease_feed_server) =
        delayed_feed().await?;
    let lease_app = App::new(store.clone())
        .await?
        .integration_config(IntegrationConfig::new(
            Some(&key),
            vec![lease_origin.clone()],
            None,
            None,
        )?)
        .router();
    let lease_source = Uuid::new_v4().to_string();
    let create_lease = request(
        &lease_app,
        "POST",
        "calendar-commands",
        &[
            ("authorization", &auth),
            ("idempotency-key", &Uuid::new_v4().to_string()),
        ],
        json!({"command":{"kind":"create_source","id":lease_source,"label":"Calendar","timezone":"UTC",
            "connection":{"url":format!("{lease_origin}/feed"),"bearer":"probe-secret"}}}),
    )
    .await;
    assert_eq!(create_lease.0, StatusCode::OK, "{}", create_lease.2);
    let lease_refresh_task = {
        let lease_app = lease_app.clone();
        let auth = auth.clone();
        let lease_source = lease_source.clone();
        tokio::spawn(async move {
            request(
                &lease_app,
                "POST",
                &format!("calendar-sources/{lease_source}/refresh"),
                &[
                    ("authorization", &auth),
                    ("idempotency-key", &Uuid::new_v4().to_string()),
                ],
                json!({}),
            )
            .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), lease_arrived_rx.recv())
        .await?
        .expect("feed connection");
    // Same source, same held fetch: live first, then — once only the lease timestamp has been
    // forced into the past — not live, both read before the provider response is released. A
    // read that treated any positive `lease_until` as live, or passed the wrong clock, would
    // record `true` for the second observation.
    results.insert(
        "lease_refresh_in_progress_live".into(),
        progress(&lease_app, &auth, &lease_source, true).await,
    );
    sqlx::query("UPDATE calendar_sources SET lease_until=1 WHERE id=$1")
        .bind(&lease_source)
        .execute(&store.pool)
        .await?;
    results.insert(
        "lease_refresh_in_progress_expired_held".into(),
        progress(&lease_app, &auth, &lease_source, false).await,
    );
    lease_release_tx.send(()).ok();
    let lease_expired = tokio::time::timeout(std::time::Duration::from_secs(5), lease_refresh_task)
        .await?
        .expect("refresh task panicked");
    assert_eq!(lease_expired.0, StatusCode::CONFLICT, "{}", lease_expired.2);
    assert_eq!(lease_expired.2["code"], "stale_refresh");
    assert_eq!(lease_expired.2["reason"], "lease_expired");
    results.insert("stale_refresh_lease_expired".into(), shape(&lease_expired));
    lease_feed_server.abort();

    Ok(Value::Object(results))
}
/// BE-Q16 probes: the recipient-safe merge-preview read (one side hidden, staleness, the
/// 410/404 distinction between an expired and a withdrawn/handled request), the `409` on a
/// stale/missing `recipient_preview_token` at merge acceptance, and one page each of the
/// people-request and household-invitation durable sent-history reads including an `"expired"`
/// row produced after the operational cleanup/expiry rules would otherwise have hidden it.
/// Random identities are kept out of the recorded transcript; only status/code/state shapes are.
async fn merge_preview_and_sent_history_probes() -> Result<Value> {
    use crate::support::http::request;
    use atlas_core::{
        Command, households::ManagementCommand as M, people::PeopleCommand, policy::Policy,
        policy::PrincipalGrant,
    };
    use atlas_server::hash_password;

    fn shape(reply: &(StatusCode, axum::http::HeaderMap, Value)) -> Value {
        json!({"status": reply.0.as_u16(), "code": reply.2.get("code")})
    }
    fn share(id: &str) -> Policy {
        Policy {
            grants: vec![PrincipalGrant::Account {
                id: id.into(),
                edit: true,
            }],
            exclude_accounts: vec![],
        }
    }

    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let app = App::new(store.clone()).await?.router();

    let a = Uuid::new_v4().to_string();
    let b = Uuid::new_v4().to_string();
    store
        .add_account(
            &a,
            "q16-sender",
            &hash_password("q16-sender-pw-123".into()).await?,
        )
        .await?;
    store
        .add_account(
            &b,
            "q16-recipient",
            &hash_password("q16-recipient-pw-123".into()).await?,
        )
        .await?;
    let login = |username: &'static str, password: &'static str| {
        let app = app.clone();
        async move {
            let reply = request(
                &app,
                "POST",
                "sessions",
                &[],
                json!({"username":username,"password":password,"device_id":"probe"}),
            )
            .await;
            assert_eq!(reply.0, StatusCode::OK, "{}", reply.2);
            format!("Bearer {}", reply.2["access_token"].as_str().unwrap())
        }
    };
    let auth_a = login("q16-sender", "q16-sender-pw-123").await;
    let auth_b = login("q16-recipient", "q16-recipient-pw-123").await;
    let now = atlas_server::now();

    let mut results = serde_json::Map::new();

    // Pair 1: a fresh-then-stale-then-withdrawn recipient preview. `source` is private to `a`
    // (never shared with `b`); `target` is owned by `b` and shared only with `a`, so `a` (the
    // initiator) can see both sides but `b` (the recipient) can never see `source`.
    let source1 = Uuid::new_v4().to_string();
    let target1 = Uuid::new_v4().to_string();
    store
        .apply(
            &a,
            &Uuid::new_v4().to_string(),
            &[Command::CreatePerson {
                id: source1.clone(),
                name: "Probe Source".into(),
                initial_policy: Some(Policy::default()),
            }],
        )
        .await?;
    store
        .apply(
            &b,
            &Uuid::new_v4().to_string(),
            &[Command::CreatePerson {
                id: target1.clone(),
                name: "Probe Target".into(),
                initial_policy: Some(share(&a)),
            }],
        )
        .await?;
    let preview1 = store.merge_preview(&a, &source1, &target1).await?;
    let request1 = Uuid::new_v4().to_string();
    store
        .people_command(
            &a,
            &Uuid::new_v4().to_string(),
            &PeopleCommand::RequestMerge {
                id: request1.clone(),
                source_id: source1.clone(),
                target_id: target1.clone(),
                preview_token: preview1.token,
                name: "Probe Merged".into(),
            },
            now,
        )
        .await?;
    let fresh = request(
        &app,
        "GET",
        &format!("people/requests/{request1}/merge-preview"),
        &[("authorization", &auth_b)],
        json!({}),
    )
    .await;
    assert_eq!(fresh.0, StatusCode::OK, "{}", fresh.2);
    assert!(fresh.2["source"].is_null(), "{}", fresh.2);
    assert!(!fresh.2["target"].is_null(), "{}", fresh.2);
    assert_eq!(fresh.2["stale"], json!(false));
    results.insert(
        "recipient_preview_fresh_one_side_hidden".into(),
        json!({"status": fresh.0.as_u16(), "source_omitted": fresh.2["source"].is_null(),
            "target_omitted": fresh.2["target"].is_null(), "stale": fresh.2["stale"]}),
    );

    // `b` revokes `a`'s access to `target1`: `a` can no longer reproduce their own original
    // preview, so the recipient read is now `stale`, but still 200 (not an error).
    let target1_version = store.resource_policy(&b, &target1).await?.version;
    store
        .management(
            &b,
            &Uuid::new_v4().to_string(),
            &[M::ReplacePolicy {
                id: target1.clone(),
                expected_version: target1_version,
                policy: Policy::default(),
            }],
            now,
        )
        .await?;
    let stale = request(
        &app,
        "GET",
        &format!("people/requests/{request1}/merge-preview"),
        &[("authorization", &auth_b)],
        json!({}),
    )
    .await;
    assert_eq!(stale.0, StatusCode::OK, "{}", stale.2);
    assert_eq!(stale.2["stale"], json!(true));
    results.insert(
        "recipient_preview_stale_after_sender_visibility_change".into(),
        json!({"status": stale.0.as_u16(), "stale": stale.2["stale"]}),
    );

    // The sender withdraws the request: the recipient's preview read now reports 404, not 410 —
    // distinct from the expired case below.
    let cancelled = request(
        &app,
        "POST",
        "people-commands",
        &[
            ("authorization", &auth_a),
            ("idempotency-key", &Uuid::new_v4().to_string()),
        ],
        json!({"command":{"kind":"cancel_request","id":request1}}),
    )
    .await;
    assert_eq!(cancelled.0, StatusCode::OK, "{}", cancelled.2);
    let withdrawn = request(
        &app,
        "GET",
        &format!("people/requests/{request1}/merge-preview"),
        &[("authorization", &auth_b)],
        json!({}),
    )
    .await;
    results.insert("recipient_preview_withdrawn".into(), shape(&withdrawn));

    // Pair 2: an expired-but-unswept request. Distinguishing 410 from 404 is the one case the
    // brief requires to be told apart from "withdrawn".
    let source2 = Uuid::new_v4().to_string();
    let target2 = Uuid::new_v4().to_string();
    store
        .apply(
            &a,
            &Uuid::new_v4().to_string(),
            &[Command::CreatePerson {
                id: source2.clone(),
                name: "Probe Source Two".into(),
                initial_policy: Some(Policy::default()),
            }],
        )
        .await?;
    store
        .apply(
            &b,
            &Uuid::new_v4().to_string(),
            &[Command::CreatePerson {
                id: target2.clone(),
                name: "Probe Target Two".into(),
                initial_policy: Some(share(&a)),
            }],
        )
        .await?;
    let preview2 = store.merge_preview(&a, &source2, &target2).await?;
    let request2 = Uuid::new_v4().to_string();
    store
        .people_command(
            &a,
            &Uuid::new_v4().to_string(),
            &PeopleCommand::RequestMerge {
                id: request2.clone(),
                source_id: source2.clone(),
                target_id: target2.clone(),
                preview_token: preview2.token,
                name: "Probe Merged Two".into(),
            },
            now,
        )
        .await?;
    // Mirrors what real time passing would do to both the operational row and its durable
    // history mirror — a sent-history read must still show this as "expired" (below).
    for table in ["people_requests", "people_request_history"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET expires_at=1 WHERE id=$1"
        )))
        .bind(&request2)
        .execute(&store.pool)
        .await?;
    }
    let expired = request(
        &app,
        "GET",
        &format!("people/requests/{request2}/merge-preview"),
        &[("authorization", &auth_b)],
        json!({}),
    )
    .await;
    results.insert("recipient_preview_expired".into(), shape(&expired));

    // Pair 3: `b` accepts with a stale/missing `recipient_preview_token`: `409`, not committed.
    let source3 = Uuid::new_v4().to_string();
    let target3 = Uuid::new_v4().to_string();
    store
        .apply(
            &a,
            &Uuid::new_v4().to_string(),
            &[Command::CreatePerson {
                id: source3.clone(),
                name: "Probe Source Three".into(),
                initial_policy: Some(Policy::default()),
            }],
        )
        .await?;
    store
        .apply(
            &b,
            &Uuid::new_v4().to_string(),
            &[Command::CreatePerson {
                id: target3.clone(),
                name: "Probe Target Three".into(),
                initial_policy: Some(share(&a)),
            }],
        )
        .await?;
    let preview3 = store.merge_preview(&a, &source3, &target3).await?;
    let request3 = Uuid::new_v4().to_string();
    store
        .people_command(
            &a,
            &Uuid::new_v4().to_string(),
            &PeopleCommand::RequestMerge {
                id: request3.clone(),
                source_id: source3.clone(),
                target_id: target3.clone(),
                preview_token: preview3.token,
                name: "Probe Merged Three".into(),
            },
            now,
        )
        .await?;
    let missing_token = request(
        &app,
        "POST",
        "people-commands",
        &[
            ("authorization", &auth_b),
            ("idempotency-key", &Uuid::new_v4().to_string()),
        ],
        json!({"command":{"kind":"respond_request","id":request3,"accept":true}}),
    )
    .await;
    assert_eq!(missing_token.0, StatusCode::CONFLICT, "{}", missing_token.2);
    assert_eq!(missing_token.2["code"], "conflict");
    results.insert(
        "merge_accept_missing_recipient_token".into(),
        shape(&missing_token),
    );

    // Sent-history: `a` sent three people-requests above (withdrawn, expired, still pending).
    let sent_requests = request(
        &app,
        "GET",
        "people/requests/sent",
        &[("authorization", &auth_a)],
        json!({}),
    )
    .await;
    assert_eq!(sent_requests.0, StatusCode::OK, "{}", sent_requests.2);
    let mut request_states: Vec<String> = sent_requests.2["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["state"].as_str().unwrap().to_owned())
        .collect();
    request_states.sort();
    results.insert(
        "sent_people_requests".into(),
        json!({"status": sent_requests.0.as_u16(), "count": request_states.len(),
            "states": request_states, "next_after": sent_requests.2["next_after"]}),
    );

    // Sent-history for household invitations: one invitation ages into "expired" the same way,
    // and one is revoked *before* it would have gone stale by time — its terminal state must
    // not be overwritten by the "pending && past expiry" rule.
    let c = Uuid::new_v4().to_string();
    store
        .add_account(
            &c,
            "q16-third",
            &hash_password("q16-third-pw-123".into()).await?,
        )
        .await?;
    let household = Uuid::new_v4().to_string();
    store
        .management(
            &a,
            &Uuid::new_v4().to_string(),
            &[M::CreateHousehold {
                id: household.clone(),
                name: "Probe Household".into(),
            }],
            now,
        )
        .await?;
    let expiring_invitation = Uuid::new_v4().to_string();
    store
        .management(
            &a,
            &Uuid::new_v4().to_string(),
            &[M::InviteToHousehold {
                id: expiring_invitation.clone(),
                household_id: household.clone(),
                recipient_id: b.clone(),
                expected_version: store.households(&a).await?[0].version,
            }],
            now,
        )
        .await?;
    for table in ["household_invitations", "household_invitation_history"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET expires_at=1 WHERE id=$1"
        )))
        .bind(&expiring_invitation)
        .execute(&store.pool)
        .await?;
    }
    let revoked_invitation = Uuid::new_v4().to_string();
    store
        .management(
            &a,
            &Uuid::new_v4().to_string(),
            &[M::InviteToHousehold {
                id: revoked_invitation.clone(),
                household_id: household.clone(),
                recipient_id: c.clone(),
                expected_version: store.households(&a).await?[0].version,
            }],
            now,
        )
        .await?;
    store
        .management(
            &a,
            &Uuid::new_v4().to_string(),
            &[M::RevokeHouseholdInvitation {
                id: revoked_invitation.clone(),
                expected_version: 1,
            }],
            now,
        )
        .await?;
    for table in ["household_invitations", "household_invitation_history"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET expires_at=1 WHERE id=$1"
        )))
        .bind(&revoked_invitation)
        .execute(&store.pool)
        .await?;
    }
    let sent_invitations = request(
        &app,
        "GET",
        "invitations/sent",
        &[("authorization", &auth_a)],
        json!({}),
    )
    .await;
    assert_eq!(sent_invitations.0, StatusCode::OK, "{}", sent_invitations.2);
    let mut invitation_states: Vec<String> = sent_invitations.2["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["status"].as_str().unwrap().to_owned())
        .collect();
    invitation_states.sort();
    assert_eq!(
        invitation_states,
        vec!["expired".to_owned(), "revoked".to_owned()],
        "{}",
        sent_invitations.2
    );
    results.insert(
        "sent_invitations".into(),
        json!({"status": sent_invitations.0.as_u16(), "count": invitation_states.len(),
            "states": invitation_states, "next_after": sent_invitations.2["next_after"]}),
    );

    Ok(Value::Object(results))
}

/// BE-B4: the plaintext of a reminder Web Push message (Declarative Web Push envelope and the
/// identifiers-only fallbacks) and the real encrypted delivery path that carries it, through
/// `IntegrationConfig::deliver` to a loopback push-service double. Fixed identifiers keep the
/// transcript deterministic; only facts that do not vary between runs are recorded.
async fn web_push_payload_probes() -> Result<Value> {
    use atlas_core::calendars::Notification;
    use atlas_server::integrations::{IntegrationConfig, Subscription, web_push_payload};
    use axum::{body::Bytes, extract::State, http::HeaderMap, routing::post};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    type Received = Arc<Mutex<Vec<(HeaderMap, Bytes)>>>;
    async fn capture(State(received): State<Received>, headers: HeaderMap, body: Bytes) {
        received.lock().await.push((headers, body));
    }

    let notification = Notification {
        id: "3f2b8c1e-5a7d-4e90-b1c4-8d6e2a9f0b73".into(),
        reminder_id: "a1d4e7f2-9c35-4b68-8e1a-5f3c7d2b9a40".into(),
        occurrence_id: "c8e5b3a9-2d71-4f06-9a84-1b7e6c0d3f52".into(),
    };
    let public = Some("https://atlas.example");
    let declarative = web_push_payload(&notification, public)?;
    let identifiers = web_push_payload(&notification, None)?;
    assert_eq!(identifiers, serde_json::to_vec(&notification)?);
    let unusable = web_push_payload(&notification, Some("ftp://atlas.example"))?;
    let very_long_host: String = (0..3100)
        .map(|i| if i % 61 == 60 { '.' } else { 'a' })
        .collect();
    let oversize = web_push_payload(&notification, Some(&format!("https://{very_long_host}")))?;
    assert!(oversize.len() <= 3052);

    let received = Received::default();
    let router = Router::new()
        .route("/web", post(capture))
        .route("/ntfy", post(capture))
        .route("/gone", post(|| async { StatusCode::GONE }))
        .route(
            "/unavailable",
            post(|| async { StatusCode::SERVICE_UNAVAILABLE }),
        )
        .with_state(received.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let config = IntegrationConfig::new(
        Some(&"01".repeat(32)),
        vec![origin.clone()],
        Some(URL_SAFE_NO_PAD.encode([1_u8; 32])),
        Some("mailto:test@example.invalid".into()),
    )?;
    let (key, auth) = ece::generate_keypair_and_auth_secret()?;
    let web = |path: &str| Subscription::WebPush {
        endpoint: format!("{origin}/{path}"),
        p256dh: URL_SAFE_NO_PAD.encode(key.pub_as_raw().unwrap()),
        auth: URL_SAFE_NO_PAD.encode(auth),
    };
    let ntfy = Subscription::Ntfy {
        url: format!("{origin}/ntfy"),
        bearer: None,
    };
    let outcome = |(success, permanent): (bool, bool)| json!([success, permanent]);
    let accepted = outcome(
        config
            .deliver(&web("web"), &notification, i64::MAX, public)
            .await?,
    );
    let without_origin = outcome(
        config
            .deliver(&web("web"), &notification, i64::MAX, None)
            .await?,
    );
    let gone = outcome(
        config
            .deliver(&web("gone"), &notification, i64::MAX, public)
            .await?,
    );
    let unavailable = outcome(
        config
            .deliver(&web("unavailable"), &notification, i64::MAX, public)
            .await?,
    );
    let ntfy_accepted = outcome(
        config
            .deliver(&ntfy, &notification, i64::MAX, public)
            .await?,
    );

    let messages = received.lock().await;
    assert_eq!(messages.len(), 3);
    let header = |headers: &HeaderMap, name: &str| headers[name].to_str().unwrap().to_owned();
    let delivered = |index: usize, expected: &[u8]| -> Result<Value> {
        let (headers, body) = &messages[index];
        let plaintext = ece::decrypt(&key.raw_components()?, &auth, body)?;
        Ok(json!({
            "authorization_scheme": header(headers, "authorization").split(' ').next(),
            "content_encoding": header(headers, "content-encoding"),
            "plaintext_is_builder_output": plaintext == expected,
            "topic": header(headers, "topic"),
            "ttl": header(headers, "ttl"),
        }))
    };
    let transcript = json!({
        "declarative": {
            "bytes": declarative.len(),
            "payload": serde_json::from_slice::<Value>(&declarative)?,
        },
        "delivery": {
            "declarative": {"message": delivered(0, &declarative)?, "result": accepted},
            "identifiers_only": {"message": delivered(1, &identifiers)?, "result": without_origin},
            "gone": gone,
            "unavailable": unavailable,
        },
        "identifiers_only": {
            "bytes": identifiers.len(),
            "payload": serde_json::from_slice::<Value>(&identifiers)?,
        },
        "ntfy": {
            "body": String::from_utf8(messages[2].1.to_vec())?,
            "cache": header(&messages[2].0, "cache"),
            "result": ntfy_accepted,
            "title": header(&messages[2].0, "title"),
        },
        "oversize_origin_falls_back": oversize == identifiers,
        "unusable_origin_falls_back": unusable == identifiers,
    });
    drop(messages);
    server.abort();
    Ok(transcript)
}

/// BE-B5: `GET /defaults/snapshot`. Records only deterministic shape and outcome, never a random
/// identity: authentication, the headers a client relies on, the body's keys, that its `defaults`
/// are what `GET /defaults` returns for the same state, the households listed for a manager, a
/// removed member and a member whose household-layer template names a household they do not belong
/// to (which must be omitted and must not move their revision).
async fn sharing_snapshot_probes() -> Result<Value> {
    use crate::support::http::request;
    use atlas_core::households::ManagementCommand as M;
    use atlas_server::{hash_password, now};

    const PASSWORD: &str = "sharing-snapshot-probe-123";
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let hash = hash_password(PASSWORD.into()).await?;
    let app = App::new(store.clone()).await?.router();
    let new_id = || Uuid::new_v4().to_string();
    let mut people = BTreeMap::new();
    for name in ["alice", "bob", "carol"] {
        let id = new_id();
        store
            .add_account(&id, &format!("snapshot-probe-{name}"), &hash)
            .await?;
        let login = request(
            &app,
            "POST",
            "sessions",
            &[],
            json!({"username":format!("snapshot-probe-{name}"),"password":PASSWORD,"device_id":"probe"}),
        )
        .await;
        assert_eq!(login.0, StatusCode::OK, "{}", login.2);
        people.insert(
            name,
            (
                id,
                format!("Bearer {}", login.2["access_token"].as_str().unwrap()),
            ),
        );
    }
    let manage = |who: &str, command: M| {
        let store = store.clone();
        let actor = people[who].0.clone();
        async move {
            store
                .management(&actor, &Uuid::new_v4().to_string(), &[command], now())
                .await
        }
    };
    let read = |who: &str, path: &'static str| {
        let app = app.clone();
        let auth = people[who].1.clone();
        async move { request(&app, "GET", path, &[("authorization", &auth)], Value::Null).await }
    };
    let snapshot = |who: &str| read(who, "defaults/snapshot");
    let join = |owner: &str, other: &str, household: String, version: i64| {
        let (owner, other) = (owner.to_owned(), other.to_owned());
        let (people, manage) = (&people, &manage);
        async move {
            let invitation = Uuid::new_v4().to_string();
            manage(
                &owner,
                M::InviteToHousehold {
                    id: invitation.clone(),
                    household_id: household,
                    recipient_id: people[other.as_str()].0.clone(),
                    expected_version: version,
                },
            )
            .await?;
            manage(
                &other,
                M::RespondToHouseholdInvitation {
                    id: invitation,
                    expected_version: 1,
                    accept: true,
                },
            )
            .await
        }
    };
    let summary = |reply: &(StatusCode, axum::http::HeaderMap, Value)| {
        let body = &reply.2;
        let households: Vec<Value> = body["households"]
            .as_array()
            .map(|list| {
                list.iter()
                    .map(|h| {
                        let mut roles: Vec<&str> = h["members"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|m| m["role"].as_str().unwrap())
                            .collect();
                        roles.sort_unstable();
                        json!({"role": h["role"], "version": h["version"], "member_roles": roles})
                    })
                    .collect()
            })
            .unwrap_or_default();
        json!({
            "status": reply.0.as_u16(),
            "code": body.get("code"),
            "households": households,
            "primary_household_set": body["defaults"]["primary_household_id"].is_string(),
        })
    };
    let equals_defaults = |who: &'static str, reply: (StatusCode, axum::http::HeaderMap, Value)| {
        let defaults = read(who, "defaults");
        async move { defaults.await.2 == reply.2["defaults"] }
    };

    let mut results = serde_json::Map::new();

    // Unauthenticated: the same answer every protected read gives, with the same headers.
    let unauthenticated = request(&app, "GET", "defaults/snapshot", &[], Value::Null).await;
    assert_eq!(unauthenticated.0, StatusCode::UNAUTHORIZED);
    results.insert(
        "unauthenticated".into(),
        json!({
            "status": unauthenticated.0.as_u16(),
            "code": unauthenticated.2["code"],
            "cache_control": unauthenticated.1["cache-control"].to_str()?,
            "set_cookie": unauthenticated.1.contains_key("set-cookie"),
        }),
    );

    // No household: defaults, an empty list, and exactly the contract's two members.
    let none = snapshot("alice").await;
    assert_eq!(none.0, StatusCode::OK, "{}", none.2);
    assert!(equals_defaults("alice", none.clone()).await);
    Uuid::parse_str(none.1["x-request-id"].to_str()?)?;
    let revision = none.2["defaults"]["revision"].as_str().unwrap();
    assert!(revision.len() == 64 && revision.bytes().all(|b| b.is_ascii_hexdigit()));
    let mut keys: Vec<&String> = none.2.as_object().unwrap().keys().collect();
    keys.sort();
    results.insert(
        "no_household".into(),
        json!({
            "summary": summary(&none),
            "keys": keys,
            "cache_control": none.1["cache-control"].to_str()?,
            "content_type": none.1["content-type"].to_str()?,
            "set_cookie": none.1.contains_key("set-cookie"),
        }),
    );

    // A manager with one other member: the household, both roles, and the defaults GET returns.
    let home = new_id();
    manage(
        "alice",
        M::CreateHousehold {
            id: home.clone(),
            name: "Home".into(),
        },
    )
    .await?;
    join("alice", "bob", home.clone(), 1).await?;
    let manager = snapshot("alice").await;
    assert_eq!(manager.0, StatusCode::OK, "{}", manager.2);
    assert!(equals_defaults("alice", manager.clone()).await);
    results.insert("manager".into(), summary(&manager));
    let member = snapshot("bob").await;
    assert_eq!(member.2["households"][0]["role"], "member");
    results.insert("member".into(), summary(&member));

    // A household-layer template names a household carol cannot see: it is omitted, and activity in
    // it neither lists it nor moves her revision, although it moves the manager's.
    let flat = new_id();
    manage(
        "bob",
        M::CreateHousehold {
            id: flat.clone(),
            name: "Flat".into(),
        },
    )
    .await?;
    join("bob", "alice", flat.clone(), 1).await?;
    let invitation = new_id();
    manage(
        "alice",
        M::InviteToHousehold {
            id: invitation.clone(),
            household_id: home.clone(),
            recipient_id: people["carol"].0.clone(),
            expected_version: 3,
        },
    )
    .await?;
    manage(
        "carol",
        M::RespondToHouseholdInvitation {
            id: invitation,
            expected_version: 1,
            accept: true,
        },
    )
    .await?;
    manage(
        "alice",
        M::SetDefaults {
            household_id: Some(home.clone()),
            resource_kind: "list".into(),
            expected_version: 0,
            template: Some(atlas_core::policy::DefaultTemplate::Explicit {
                policy: atlas_core::policy::Policy {
                    grants: vec![atlas_core::policy::PrincipalGrant::Household {
                        id: flat.clone(),
                        edit: false,
                    }],
                    exclude_accounts: vec![],
                },
            }),
        },
    )
    .await?;
    let carol = snapshot("carol").await;
    assert_eq!(
        carol.2["defaults"]["list"]["policy"]["grants"][0]["id"],
        json!(flat)
    );
    assert_eq!(carol.2["households"].as_array().unwrap().len(), 1);
    let alice_before = snapshot("alice").await.2["defaults"]["revision"].clone();
    manage(
        "bob",
        M::RenameHousehold {
            id: flat.clone(),
            name: "Renamed".into(),
            expected_version: 3,
        },
    )
    .await?;
    let carol_after = snapshot("carol").await;
    let alice_after = snapshot("alice").await;
    assert_eq!(carol_after.2, carol.2, "carol's whole body is unchanged");
    assert_ne!(alice_after.2["defaults"]["revision"], alice_before);
    results.insert(
        "template_names_an_unjoined_household".into(),
        json!({
            "summary": summary(&carol),
            "template_still_names_it": true,
            "unchanged_by_activity_in_it": true,
            "moves_for_a_member_of_it": true,
        }),
    );

    // A removed member: an empty list, no primary household, and a revision that no longer moves
    // with what happens in the household they left.
    let version = alice_after.2["households"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == json!(home))
        .unwrap()["version"]
        .as_i64()
        .unwrap();
    manage(
        "alice",
        M::RemoveHouseholdMember {
            household_id: home.clone(),
            account_id: people["bob"].0.clone(),
            expected_version: version,
        },
    )
    .await?;
    let removed = snapshot("bob").await;
    assert!(equals_defaults("bob", removed.clone()).await);
    manage(
        "alice",
        M::RenameHousehold {
            id: home.clone(),
            name: "Private".into(),
            expected_version: version + 1,
        },
    )
    .await?;
    let still = snapshot("bob").await;
    assert_eq!(
        still.2, removed.2,
        "a former member's whole body is unchanged"
    );
    results.insert(
        "removed_member".into(),
        json!({
            "summary": summary(&removed),
            "unchanged_by_activity_in_it": true,
        }),
    );

    Ok(Value::Object(results))
}

/// The combined probe transcript includes activation, calendar connection preservation and
/// disconnection, inactive-subscription retirement/version changes, command batch-size
/// acceptance/rejection, and the Declarative Web Push payload and its encrypted delivery.
/// `/health` omits `api_version`, which is asserted separately.
async fn observe(app: &Router) -> Value {
    json!({
        "activation": activation_probes().await.expect("activation-grant protocol probes"),
        "calendar_configure": calendar_configure_probes().await.expect("BE-Q22 connection preserve/disconnect probes"),
        "calendar_refresh_errors": calendar_refresh_errors_probes().await.expect("BE-B8 safe calendar-refresh error-code probes"),
        "command_batch": command_batch_probes().await.expect("BE-CH1 command batch-size probes"),
        "merge_preview_and_sent_history": merge_preview_and_sent_history_probes()
            .await
            .expect("BE-Q16 recipient-safe preview and sent-history probes"),
        "sharing_snapshot": sharing_snapshot_probes().await.expect("BE-B5 sharing-snapshot probes"),
        "error_mapping": error_mapping().await,
        "web_push_payload": web_push_payload_probes().await.expect("BE-B4 Declarative Web Push payload probes"),
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
