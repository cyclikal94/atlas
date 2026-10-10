//! BE-Q25 through the real router: session client labels captured by every session-issuing route,
//! device names (`PUT`/`DELETE /devices/{id}/name`), and session/sync capacity reported with the
//! same semantics the two `device_capacity` checks enforce. Runs on both engines.
use anyhow::Result;
use atlas_server::{now, operator_invitation};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::collections::BTreeSet;

use crate::support::http::request;
use crate::support::retirement::{
    ORIGIN, PASSWORD, Reply, World, activate, assert_error, attempt, bearer, cookie_login,
    device_state, id, issue_grant, ledger, listed, login, native_handoff, postgres, retire,
    session_of, world, world_with_oidc,
};
use std::time::Duration;
use tokio::task::JoinHandle;

const CHROME_WINDOWS: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36";
const FIREFOX_LINUX: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0";
const SAFARI_IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1";
const EDGE_WINDOWS: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36 Edg/129.0.0.0";
const NATIVE_ANDROID: &str = "Atlas/1.0 (Linux; Android 14)";

fn contract() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../api/openapi.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn properties(schema: &str) -> BTreeSet<String> {
    contract()["components"]["schemas"][schema]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().unwrap().keys().cloned().collect()
}

fn label(channel: &str, browser: Option<&str>, platform: &str) -> Value {
    json!({"channel":channel,"browser":browser,"platform":platform})
}

fn capacity(used: i64) -> Value {
    json!({"limit":32,"used":used,"remaining":32 - used})
}

/// `POST /sessions` with an optional `User-Agent`.
async fn native_login(w: &World, user: &str, device: &str, agent: Option<&str>) -> String {
    let mut headers = Vec::new();
    if let Some(agent) = agent {
        headers.push(("user-agent", agent));
    }
    let (status, _, body) = request(
        &w.app,
        "POST",
        "sessions",
        &headers,
        json!({"username":user,"password":PASSWORD,"device_id":device}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["access_token"].as_str().unwrap().to_owned()
}

/// A browser sign-in whose grant is issued under `grant_agent` and redeemed under
/// `activate_agent`. Returns the cookie pair and its CSRF token.
async fn browser_login(
    w: &World,
    device: &str,
    grant_agent: &str,
    activate_agent: &str,
) -> (String, String) {
    let (verifier, challenge) = crate::support::retirement::attempt();
    let (status, _, granted) = request(
        &w.app,
        "POST",
        "browser-sessions",
        &[("origin", ORIGIN), ("user-agent", grant_agent)],
        json!({"username":"device-alice","password":PASSWORD,"device_id":device,"attempt_challenge":challenge}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{granted}");
    redeem(
        w,
        granted["grant"].as_str().unwrap(),
        &verifier,
        activate_agent,
    )
    .await
}

async fn redeem(w: &World, grant: &str, verifier: &str, agent: &str) -> (String, String) {
    let (status, headers, body) = request(
        &w.app,
        "POST",
        "browser-sessions/activate",
        &[("origin", ORIGIN), ("user-agent", agent)],
        json!({"grant":grant,"verifier":verifier}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cookie = headers["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    (cookie, body["csrf_token"].as_str().unwrap().to_owned())
}

async fn get(w: &World, path: &str, token: &str) -> Value {
    let (status, _, body) = request(
        &w.app,
        "GET",
        path,
        &[("authorization", &bearer(token))],
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// The caller's own session row from `GET /sessions`.
async fn own_session(w: &World, token: &str) -> Value {
    get(w, "sessions", token).await["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["current"] == true)
        .cloned()
        .expect("the caller's session is listed")
}

async fn name(w: &World, token: &str, device: &str, body: Value) -> Reply {
    request(
        &w.app,
        "PUT",
        &format!("devices/{device}/name"),
        &[("authorization", &bearer(token))],
        body,
    )
    .await
}

async fn unname(w: &World, token: &str, device: &str) -> Reply {
    request(
        &w.app,
        "DELETE",
        &format!("devices/{device}/name"),
        &[("authorization", &bearer(token))],
        Value::Null,
    )
    .await
}

fn no_content(reply: &Reply) {
    assert_eq!(reply.0, StatusCode::NO_CONTENT, "{}", reply.2);
    assert_eq!(reply.2, Value::Null);
    assert!(!reply.1.contains_key("set-cookie"));
}

async fn profiles(w: &World) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM device_profiles")
        .fetch_one(&w.store.pool)
        .await
        .unwrap()
}

// ------------------------------------------------------------------------------- B1: labels

/// Every session-issuing route records a label; the channel is the route's, never the header's,
/// and a browser session's label follows the `activate` request that receives the cookie.
#[tokio::test]
async fn every_session_issuing_route_records_its_label() -> Result<()> {
    let w = world(true).await?;
    let native = native_login(&w, "device-alice", "phone", Some(CHROME_WINDOWS)).await;
    let mine = own_session(&w, &native).await;
    assert_eq!(keys(&mine), properties("SessionSummary"));
    assert_eq!(mine["client"], label("native", None, "windows"));

    // Browser login: the grant request says Firefox on Linux, the activation Safari on iPhone.
    let (cookie, csrf) = browser_login(&w, "laptop", FIREFOX_LINUX, SAFARI_IPHONE).await;
    let (status, _, sessions) = request(
        &w.app,
        "GET",
        "sessions",
        &[
            ("cookie", &cookie),
            ("x-csrf-token", &csrf),
            ("origin", ORIGIN),
        ],
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sessions}");
    let browser = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["current"] == true)
        .unwrap();
    assert_eq!(browser["client"], label("browser", Some("safari"), "ios"));
    assert_eq!(browser["device_id"], "laptop");

    // No User-Agent: still signed in, and unknown.
    let bare = native_login(&w, "device-alice", "bare", None).await;
    assert_eq!(
        own_session(&w, &bare).await["client"],
        label("native", None, "unknown")
    );
    let (bare_cookie, bare_csrf) = browser_login(&w, "kiosk", FIREFOX_LINUX, "").await;
    let (_, _, kiosk) = request(
        &w.app,
        "GET",
        "sessions",
        &[
            ("cookie", &bare_cookie),
            ("x-csrf-token", &bare_csrf),
            ("origin", ORIGIN),
        ],
        Value::Null,
    )
    .await;
    let kiosk = kiosk["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["current"] == true)
        .unwrap()
        .clone();
    assert_eq!(
        kiosk["client"],
        label("browser", Some("unknown"), "unknown")
    );

    // Native and browser registration.
    let invitation = operator_invitation(&w.store).await?;
    let (status, _, registered) = request(
        &w.app,
        "POST",
        "registration",
        &[("user-agent", NATIVE_ANDROID)],
        json!({"token":invitation.token,"username":"label-native","password":PASSWORD,"device_id":"pixel"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{registered}");
    assert_eq!(
        own_session(&w, registered["access_token"].as_str().unwrap()).await["client"],
        label("native", None, "android")
    );
    let invitation = operator_invitation(&w.store).await?;
    let (verifier, challenge) = crate::support::retirement::attempt();
    let (status, _, granted) = request(
        &w.app,
        "POST",
        "browser-registration",
        &[("origin", ORIGIN), ("user-agent", SAFARI_IPHONE)],
        json!({"token":invitation.token,"username":"label-browser","password":PASSWORD,"device_id":"pc","attempt_challenge":challenge}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{granted}");
    redeem(
        &w,
        granted["grant"].as_str().unwrap(),
        &verifier,
        EDGE_WINDOWS,
    )
    .await;
    let token = native_login(&w, "label-browser", "other", None).await;
    let pc = get(&w, "sessions", &token).await["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["device_id"] == "pc")
        .unwrap()
        .clone();
    assert_eq!(pc["client"], label("browser", Some("edge"), "windows"));

    // No raw header is stored anywhere.
    for agent in [CHROME_WINDOWS, SAFARI_IPHONE, EDGE_WINDOWS, NATIVE_ANDROID] {
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE client_channel=$1 OR client_browser=$1 OR client_platform=$1")
            .bind(agent)
            .fetch_one(&w.store.pool)
            .await?;
        assert_eq!(stored, 0);
    }
    Ok(())
}

/// The native OIDC exchange is a native route even when the request looks like a browser.
#[tokio::test]
async fn the_native_oidc_exchange_records_a_native_label() -> Result<()> {
    let w = crate::support::retirement::world_with_oidc().await?;
    let (code, verifier) = crate::support::retirement::native_handoff(&w, "tablet").await;
    let (status, _, body) = request(
        &w.app,
        "POST",
        "oidc/native/exchange",
        &[("user-agent", SAFARI_IPHONE)],
        json!({"code":code,"code_verifier":verifier}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let session = own_session(&w, body["access_token"].as_str().unwrap()).await;
    assert_eq!(session["authentication"], "oidc");
    assert_eq!(session["client"], label("native", None, "ios"));
    Ok(())
}

// ---------------------------------------------------------- B2/B3: several sessions, legacy rows

#[tokio::test]
async fn each_session_keeps_its_own_label_and_the_device_shows_the_latest() -> Result<()> {
    let w = world(false).await?;
    let first = native_login(&w, "device-alice", "phone", Some(CHROME_WINDOWS)).await;
    let second = native_login(&w, "device-alice", "phone", Some(NATIVE_ANDROID)).await;
    no_content(&name(&w, &first, "phone", json!({"name":"Kieran's phone"})).await);
    assert_eq!(
        own_session(&w, &first).await["client"],
        label("native", None, "windows")
    );
    assert_eq!(
        own_session(&w, &second).await["client"],
        label("native", None, "android")
    );
    let sessions = get(&w, "sessions", &first).await;
    for session in sessions["sessions"].as_array().unwrap() {
        assert_eq!(session["device_name"], "Kieran's phone");
    }
    let phone = listed(&w.app, &first, "phone").await.unwrap();
    assert_eq!(keys(&phone), properties("Device"));
    assert_eq!(phone["name"], "Kieran's phone");
    assert_eq!(phone["client"], label("native", None, "android"));
    assert_eq!(phone["active_sessions"], 2);
    Ok(())
}

#[tokio::test]
async fn legacy_sessions_and_devices_have_no_label_or_name() -> Result<()> {
    let w = world(false).await?;
    let token = native_login(&w, "device-alice", "phone", Some(CHROME_WINDOWS)).await;
    // A session issued before 1005: the label columns are NULL and the device has no profile.
    sqlx::query("INSERT INTO sessions(token_hash,account_id,device_id,expires_at,session_id,created_at,auth_kind) VALUES ($1,$2,'legacy',$3,$4,1,'local')")
        .bind(id()).bind(&w.alice).bind(now() + 3600).bind(id())
        .execute(&w.store.pool).await?;
    let sessions = get(&w, "sessions", &token).await;
    let legacy = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["device_id"] == "legacy")
        .unwrap();
    assert_eq!(legacy["client"], Value::Null);
    assert_eq!(legacy["device_name"], Value::Null);
    let device = listed(&w.app, &token, "legacy").await.unwrap();
    assert_eq!(
        (&device["client"], &device["name"]),
        (&Value::Null, &Value::Null)
    );
    // It can still be named.
    no_content(&name(&w, &token, "legacy", json!({"name":"Old laptop"})).await);
    let device = listed(&w.app, &token, "legacy").await.unwrap();
    assert_eq!(
        (&device["client"], &device["name"]),
        (&Value::Null, &json!("Old laptop"))
    );
    Ok(())
}

// ---------------------------------------------------------------------------- B4: naming

#[tokio::test]
async fn naming_is_validated_account_scoped_and_returned_verbatim() -> Result<()> {
    let w = world(true).await?;
    let alice = native_login(&w, "device-alice", "phone", None).await;
    let bob = native_login(&w, "device-bob", "phone", None).await;
    login(&w.app, "device-bob", "bobs-laptop").await;

    // Accepted exactly as sent, including text that looks like markup.
    for accepted in [
        "\u{1F600}".repeat(64),
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} tablet".into(),
        "\u{5BB6}\u{65CF}\u{306E}iPad".into(),
        "<script>alert(1)</script>".into(),
        "x".repeat(64),
    ] {
        for _ in 0..2 {
            no_content(&name(&w, &alice, "phone", json!({"name":accepted})).await);
        }
        assert_eq!(
            listed(&w.app, &alice, "phone").await.unwrap()["name"],
            accepted
        );
    }
    // Bob's device with the same ID is his own, and Alice's naming never reached it.
    assert_eq!(
        listed(&w.app, &bob, "phone").await.unwrap()["name"],
        Value::Null
    );
    no_content(&name(&w, &bob, "phone", json!({"name":"Bob's"})).await);
    assert_eq!(
        listed(&w.app, &alice, "phone").await.unwrap()["name"],
        "x".repeat(64)
    );

    // Not listed for the caller: unknown, another account's, never existed.
    for device in ["bobs-laptop", "unknown"] {
        assert_error(
            &name(&w, &alice, device, json!({"name":"Mine"})).await,
            StatusCode::NOT_FOUND,
            "not_found",
        );
        assert_error(
            &unname(&w, &alice, device).await,
            StatusCode::NOT_FOUND,
            "not_found",
        );
    }
    assert_error(
        &name(&w, &alice, &"d".repeat(101), json!({"name":"Mine"})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_value",
    );

    for refused in [
        String::new(),
        "x".repeat(65),
        " leading".into(),
        "trailing ".into(),
        "line\nbreak".into(),
        "nul\u{0}".into(),
        "rtl \u{202E}override".into(),
        "isolate \u{2066}x".into(),
        "zero\u{200B}width".into(),
        "bom\u{FEFF}".into(),
        "line\u{2028}separator".into(),
        "non\u{FFFF}char".into(),
    ] {
        assert_error(
            &name(&w, &alice, "phone", json!({"name":refused})).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        );
    }
    for malformed in [
        json!({"name":null}),
        json!({"name":1}),
        json!({"name":"ok","extra":true}),
        json!({}),
        json!("just a string"),
    ] {
        assert_error(
            &name(&w, &alice, "phone", malformed.clone()).await,
            StatusCode::BAD_REQUEST,
            "malformed_request",
        );
    }
    // Nothing above changed Alice's name.
    assert_eq!(
        listed(&w.app, &alice, "phone").await.unwrap()["name"],
        "x".repeat(64)
    );

    // Unauthenticated, and a cookie without its CSRF token or from another origin.
    let (status, _, _) = request(
        &w.app,
        "PUT",
        "devices/phone/name",
        &[],
        json!({"name":"Anon"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (cookie, csrf) = cookie_login(&w.app, "browser").await;
    for headers in [
        vec![("cookie", cookie.as_str()), ("origin", ORIGIN)],
        vec![
            ("cookie", cookie.as_str()),
            ("x-csrf-token", csrf.as_str()),
            ("origin", "https://evil.example"),
        ],
    ] {
        let (status, _, body) = request(
            &w.app,
            "PUT",
            "devices/browser/name",
            &headers,
            json!({"name":"x"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    let cookie_headers = [
        ("cookie", cookie.as_str()),
        ("x-csrf-token", csrf.as_str()),
        ("origin", ORIGIN),
    ];
    let reply = request(
        &w.app,
        "PUT",
        "devices/browser/name",
        &cookie_headers,
        json!({"name":"This browser"}),
    )
    .await;
    no_content(&reply);
    no_content(
        &request(
            &w.app,
            "DELETE",
            "devices/browser/name",
            &cookie_headers,
            Value::Null,
        )
        .await,
    );

    // Clearing is idempotent, also when no name is set.
    for _ in 0..2 {
        no_content(&unname(&w, &alice, "phone").await);
        assert_eq!(
            listed(&w.app, &alice, "phone").await.unwrap()["name"],
            Value::Null
        );
    }
    Ok(())
}

// -------------------------------------------------------------------- B5/B8: lifetime of a name

/// A rename between reading the token and retiring does not make the retirement stale; the
/// retirement deletes the name, and the device's next sign-in starts unnamed.
#[tokio::test]
async fn retirement_forgets_the_name_and_a_rename_never_stales_it() -> Result<()> {
    let w = world(true).await?;
    let laptop = native_login(&w, "device-alice", "laptop", None).await;
    native_login(&w, "device-alice", "phone", Some(CHROME_WINDOWS)).await;
    no_content(&name(&w, &laptop, "phone", json!({"name":"Phone"})).await);
    let state = device_state(&w.app, &laptop, "phone").await;
    no_content(&name(&w, &laptop, "phone", json!({"name":"Renamed"})).await);
    assert_eq!(device_state(&w.app, &laptop, "phone").await, state);
    let operation = id();
    let reply = retire(&w.app, &laptop, "phone", &operation, &state).await;
    assert_eq!(reply.0, StatusCode::OK, "{}", reply.2);
    assert_eq!(
        ledger(&w.store).await?,
        [(operation, String::from("confirmed_applied"))]
    );
    assert_error(
        &name(&w, &laptop, "phone", json!({"name":"Ghost"})).await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    // The retirement itself deleted the profile: a pending browser sign-in, which lists the
    // device again without issuing a session (so without the sign-in's orphan prune), shows it
    // unnamed and unlabelled rather than resurrecting the retired device's name.
    let profile: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM device_profiles WHERE account_id=$1 AND device_id='phone'",
    )
    .bind(&w.alice)
    .fetch_one(&w.store.pool)
    .await?;
    assert_eq!(profile, 0);
    let (status, _, granted) = issue_grant(&w.app, "device-alice", "phone").await;
    assert_eq!(status, StatusCode::OK, "{granted}");
    let pending = listed(&w.app, &laptop, "phone").await.unwrap();
    assert_eq!(pending["summary"]["pending_sign_ins"], 1);
    assert_eq!(
        (&pending["name"], &pending["client"]),
        (&Value::Null, &Value::Null)
    );
    let again = native_login(&w, "device-alice", "phone", Some(FIREFOX_LINUX)).await;
    let phone = listed(&w.app, &again, "phone").await.unwrap();
    assert_eq!(phone["name"], Value::Null);
    assert_eq!(phone["client"], label("native", None, "linux"));
    Ok(())
}

/// When every member of a named device has expired, the device's own next sign-in starts unnamed:
/// `issue` prunes orphan profiles before it records the new label and inserts the session.
#[tokio::test]
async fn a_forgotten_device_signing_in_again_starts_unnamed() -> Result<()> {
    let w = world(false).await?;
    let laptop = native_login(&w, "device-alice", "laptop", None).await;
    let phone = native_login(&w, "device-alice", "phone", None).await;
    no_content(&name(&w, &phone, "phone", json!({"name":"Phone"})).await);
    no_content(&name(&w, &phone, "laptop", json!({"name":"Laptop"})).await);
    sqlx::query("UPDATE sessions SET expires_at=1 WHERE account_id=$1 AND device_id='phone'")
        .bind(&w.alice)
        .execute(&w.store.pool)
        .await?;
    assert!(listed(&w.app, &laptop, "phone").await.is_none());
    let again = native_login(&w, "device-alice", "phone", None).await;
    assert_eq!(
        listed(&w.app, &again, "phone").await.unwrap()["name"],
        Value::Null
    );
    assert_eq!(
        listed(&w.app, &again, "laptop").await.unwrap()["name"],
        "Laptop"
    );
    Ok(())
}

/// How the regression below lists a forgotten device again.
#[derive(Clone, Copy, Debug)]
enum Relisting {
    NativePassword,
    BrowserPassword,
    BrowserOidc,
    NativeOidc,
}

/// A browser OpenID Connect sign-in for Alice's linked subject on `device`, up to its callback:
/// returns the grant the callback issued and the verifier that activates it.
async fn browser_oidc_grant(w: &World, device: &str) -> (String, String) {
    let (verifier, challenge) = attempt();
    let (status, headers, body) = request(
        &w.app,
        "POST",
        "oidc/start",
        &[("origin", ORIGIN)],
        json!({"device_id":device,"link":false,"attempt_id":id(),"attempt_challenge":challenge}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let location = body["authorization_url"].as_str().unwrap();
    let code = w.provider.as_ref().unwrap().approve(location, "valid");
    let state = url::Url::parse(location)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    let cookie = headers["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let (status, headers, body) = request(
        &w.app,
        "GET",
        &format!("oidc/callback?state={state}&code={code}"),
        &[("cookie", cookie)],
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    let location = headers["location"].to_str().unwrap();
    let (_, fragment) = location.split_once('#').expect("a fragment");
    let grant = url::form_urlencoded::parse(fragment.as_bytes())
        .find(|(k, _)| k == "atlas_grant")
        .expect("a grant")
        .1
        .into_owned();
    (grant, verifier)
}

/// R1 (revision 1, family `device-profile-membership-boundary`): a device signed out of its last
/// session is forgotten, and returns unnamed whichever sign-in route lists it again — including
/// the pending grant or handoff that lists it before any session exists. Native password sign-in
/// is the positive control that passed before the correction. A device that stayed listed keeps
/// its name throughout.
async fn forgotten_device_returns_unnamed(route: Relisting) -> Result<()> {
    let w = world_with_oidc().await?;
    let admin = login(&w.app, "device-alice", "admin").await;
    let tablet = login(&w.app, "device-alice", "tablet").await;
    no_content(&name(&w, &admin, "tablet", json!({"name":"Recognisable tablet"})).await);
    no_content(&name(&w, &admin, "admin", json!({"name":"Admin"})).await);
    let (status, _, body) = request(
        &w.app,
        "DELETE",
        "sessions/current",
        &[("authorization", &bearer(&tablet))],
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(
        listed(&w.app, &admin, "tablet").await.is_none(),
        "{route:?}"
    );

    let pending = |kind: &'static str| {
        let (app, admin) = (w.app.clone(), admin.clone());
        async move {
            let pending = listed(&app, &admin, "tablet")
                .await
                .expect("listed while pending");
            assert_eq!(pending["summary"][kind], 1, "{route:?}");
            assert_eq!(pending["name"], Value::Null, "{route:?}: pending listing");
        }
    };
    match route {
        Relisting::NativePassword => {
            login(&w.app, "device-alice", "tablet").await;
        }
        Relisting::BrowserPassword => {
            let (status, _, granted) = issue_grant(&w.app, "device-alice", "tablet").await;
            assert_eq!(status, StatusCode::OK, "{granted}");
            pending("pending_sign_ins").await;
            let reply = activate(
                &w.app,
                granted["grant"].as_str().unwrap(),
                granted["verifier"].as_str().unwrap(),
            )
            .await;
            assert_eq!(reply.0, StatusCode::OK, "{}", reply.2);
        }
        Relisting::BrowserOidc => {
            let (grant, verifier) = browser_oidc_grant(&w, "tablet").await;
            pending("pending_sign_ins").await;
            let reply = activate(&w.app, &grant, &verifier).await;
            assert_eq!(reply.0, StatusCode::OK, "{}", reply.2);
        }
        Relisting::NativeOidc => {
            let (code, verifier) = native_handoff(&w, "tablet").await;
            pending("native_handoffs").await;
            let (status, _, body) = request(
                &w.app,
                "POST",
                "oidc/native/exchange",
                &[],
                json!({"code":code,"code_verifier":verifier}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    }
    let after = listed(&w.app, &admin, "tablet").await.unwrap();
    assert_eq!(after["active_sessions"], 1, "{route:?}");
    assert_eq!(
        after["name"],
        Value::Null,
        "{route:?}: after the session is issued"
    );
    assert_eq!(
        listed(&w.app, &admin, "admin").await.unwrap()["name"],
        "Admin",
        "{route:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_forgotten_device_returns_unnamed_by_native_password_sign_in() -> Result<()> {
    forgotten_device_returns_unnamed(Relisting::NativePassword).await
}

#[tokio::test]
async fn a_forgotten_device_returns_unnamed_by_browser_password_sign_in() -> Result<()> {
    forgotten_device_returns_unnamed(Relisting::BrowserPassword).await
}

#[tokio::test]
async fn a_forgotten_device_returns_unnamed_by_browser_oidc_sign_in() -> Result<()> {
    forgotten_device_returns_unnamed(Relisting::BrowserOidc).await
}

#[tokio::test]
async fn a_forgotten_device_returns_unnamed_by_native_oidc_sign_in() -> Result<()> {
    forgotten_device_returns_unnamed(Relisting::NativeOidc).await
}

async fn phone_profiles(w: &World) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM device_profiles WHERE account_id=$1 AND device_id='phone'",
    )
    .bind(&w.alice)
    .fetch_one(&w.store.pool)
    .await
    .unwrap()
}

/// R2 (revision 1, same family): exchanging a native handoff or activating a grant replaces a live
/// member of the device with its session, so a device listed only by that member keeps its name
/// across the atomic transition. The variant with another live session on the device is the
/// control that passed before the correction. The transition still prunes the account's other
/// orphans: a named device signed out after the grant or handoff was issued loses its profile.
async fn replacing_the_only_member_keeps_the_name(browser: bool, keep_session: bool) -> Result<()> {
    let w = world_with_oidc().await?;
    let admin = login(&w.app, "device-alice", "admin").await;
    if keep_session {
        login(&w.app, "device-alice", "tablet").await;
    }
    let phone = login(&w.app, "device-alice", "phone").await;
    no_content(&name(&w, &admin, "phone", json!({"name":"Old phone"})).await);
    let case = format!("browser={browser} keep_session={keep_session}");
    let (member, redeem): (&str, Box<dyn FnOnce() -> _>) = if browser {
        let (status, _, granted) = issue_grant(&w.app, "device-alice", "tablet").await;
        assert_eq!(status, StatusCode::OK, "{granted}");
        let app = w.app.clone();
        (
            "pending_sign_ins",
            Box::new(move || {
                Box::pin(async move {
                    activate(
                        &app,
                        granted["grant"].as_str().unwrap(),
                        granted["verifier"].as_str().unwrap(),
                    )
                    .await
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = Reply> + Send>>
            }),
        )
    } else {
        let (code, verifier) = native_handoff(&w, "tablet").await;
        let app = w.app.clone();
        (
            "native_handoffs",
            Box::new(move || {
                Box::pin(async move {
                    request(
                        &app,
                        "POST",
                        "oidc/native/exchange",
                        &[],
                        json!({"code":code,"code_verifier":verifier}),
                    )
                    .await
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = Reply> + Send>>
            }),
        )
    };
    // Signed out only now, after the grant or handoff was issued (whose own prune would otherwise
    // have dropped it), so it is the redemption's prune that must drop the phone's profile.
    let (status, _, _) = request(
        &w.app,
        "DELETE",
        "sessions/current",
        &[("authorization", &bearer(&phone))],
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    no_content(&name(&w, &admin, "tablet", json!({"name":"Recognisable tablet"})).await);
    let before = listed(&w.app, &admin, "tablet").await.unwrap();
    assert_eq!(before["summary"][member], 1, "{case}");
    assert_eq!(before["name"], "Recognisable tablet", "{case}");
    assert_eq!(
        phone_profiles(&w).await,
        1,
        "{case}: the phone's orphan awaits a prune"
    );
    let reply = redeem().await;
    assert_eq!(reply.0, StatusCode::OK, "{case}: {}", reply.2);
    let after = listed(&w.app, &admin, "tablet").await.unwrap();
    assert_eq!(after["summary"][member], 0, "{case}");
    assert_eq!(
        after["active_sessions"],
        if keep_session { 2 } else { 1 },
        "{case}"
    );
    assert_eq!(
        after["name"], "Recognisable tablet",
        "{case}: the name survives"
    );
    assert_eq!(
        phone_profiles(&w).await,
        0,
        "{case}: the signed-out phone's profile is pruned by the redemption"
    );
    Ok(())
}

#[tokio::test]
async fn exchanging_a_devices_only_handoff_keeps_its_name() -> Result<()> {
    replacing_the_only_member_keeps_the_name(false, false).await
}

#[tokio::test]
async fn exchanging_a_handoff_beside_a_session_keeps_its_name() -> Result<()> {
    replacing_the_only_member_keeps_the_name(false, true).await
}

#[tokio::test]
async fn activating_a_devices_only_grant_keeps_its_name() -> Result<()> {
    replacing_the_only_member_keeps_the_name(true, false).await
}

#[tokio::test]
async fn activating_a_grant_beside_a_session_keeps_its_name() -> Result<()> {
    replacing_the_only_member_keeps_the_name(true, true).await
}

/// A password change revokes every session; a device kept listed by its sync registration keeps
/// its name, a session-only device is no longer listed and its profile goes at the next sign-in.
#[tokio::test]
async fn a_password_change_keeps_names_of_devices_that_remain_listed() -> Result<()> {
    let w = world(false).await?;
    let phone = native_login(&w, "device-alice", "phone", None).await;
    native_login(&w, "device-alice", "laptop", None).await;
    w.store.sync(&w.alice, "phone", None, 200, now()).await?;
    no_content(&name(&w, &phone, "phone", json!({"name":"Phone"})).await);
    no_content(&name(&w, &phone, "laptop", json!({"name":"Laptop"})).await);
    let (status, _, body) = request(
        &w.app,
        "POST",
        "password",
        &[("authorization", &bearer(&phone))],
        json!({"current_password":PASSWORD,"new_password":"a-brand-new-password"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _, body) = request(
        &w.app,
        "POST",
        "sessions",
        &[],
        json!({"username":"device-alice","password":"a-brand-new-password","device_id":"tablet"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let tablet = body["access_token"].as_str().unwrap();
    assert_eq!(
        listed(&w.app, tablet, "phone").await.unwrap()["name"],
        "Phone"
    );
    assert!(listed(&w.app, tablet, "laptop").await.is_none());
    let laptop_profile: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM device_profiles WHERE account_id=$1 AND device_id='laptop'",
    )
    .bind(&w.alice)
    .fetch_one(&w.store.pool)
    .await?;
    assert_eq!(laptop_profile, 0, "pruned by the tablet's sign-in");
    assert_eq!(
        get(&w, "sessions", tablet).await["session_capacity"],
        capacity(1)
    );
    Ok(())
}

// --------------------------------------------------------------------------- B7: capacity

/// `session_capacity` counts exactly what `issue` counts: live sessions of the account, after
/// discarding expired rows; the 33rd sign-in is refused while the read says nothing remains.
#[tokio::test]
async fn session_capacity_matches_enforcement() -> Result<()> {
    let w = world(false).await?;
    for n in 0..30 {
        sqlx::query("INSERT INTO sessions(token_hash,account_id,device_id,expires_at,session_id,created_at,auth_kind) VALUES ($1,$2,$3,$4,$5,1,'local')")
            .bind(id()).bind(&w.alice).bind(format!("seeded-{n}")).bind(now() + 3600).bind(id())
            .execute(&w.store.pool).await?;
    }
    let first = native_login(&w, "device-alice", "phone", None).await;
    // Expired but not yet swept (inserted after the sign-in, whose own sweep would remove it):
    // counted by neither the read nor the next sign-in's check.
    sqlx::query("INSERT INTO sessions(token_hash,account_id,device_id,expires_at,session_id,created_at,auth_kind) VALUES ($1,$2,'stale',1,$3,1,'local')")
        .bind(id()).bind(&w.alice).bind(id())
        .execute(&w.store.pool).await?;
    let sessions = get(&w, "sessions", &first).await;
    assert_eq!(keys(&sessions), properties("Sessions"));
    assert_eq!(keys(&sessions["session_capacity"]), properties("Capacity"));
    assert_eq!(sessions["session_capacity"], capacity(31));
    let second = native_login(&w, "device-alice", "laptop", None).await;
    assert_eq!(
        get(&w, "sessions", &second).await["session_capacity"],
        capacity(32)
    );
    let (status, _, refused) = request(
        &w.app,
        "POST",
        "sessions",
        &[],
        json!({"username":"device-alice","password":PASSWORD,"device_id":"tablet"}),
    )
    .await;
    assert_eq!(
        (status, refused["code"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("device_capacity"))
    );
    assert_eq!(
        get(&w, "sessions", &first).await["session_capacity"],
        capacity(32)
    );
    // Bob's capacity is his own.
    let bob = native_login(&w, "device-bob", "phone", None).await;
    assert_eq!(
        get(&w, "sessions", &bob).await["session_capacity"],
        capacity(1)
    );
    // Signing out frees the slot on commit.
    let (status, _, _) = request(
        &w.app,
        "DELETE",
        "sessions/current",
        &[("authorization", &bearer(&second))],
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        get(&w, "sessions", &first).await["session_capacity"],
        capacity(31)
    );
    native_login(&w, "device-alice", "tablet", None).await;
    Ok(())
}

#[tokio::test]
async fn sync_capacity_is_reported_with_the_device_list() -> Result<()> {
    let w = world(false).await?;
    let token = native_login(&w, "device-alice", "phone", None).await;
    let devices = get(&w, "devices", &token).await;
    assert_eq!(keys(&devices), properties("Devices"));
    assert_eq!(
        devices["sync_capacity"],
        capacity(0),
        "a session is not a sync slot"
    );
    w.store.sync(&w.alice, "phone", None, 200, now()).await?;
    w.store.sync(&w.alice, "watch", None, 200, now()).await?;
    assert_eq!(
        get(&w, "devices", &token).await["sync_capacity"],
        capacity(2)
    );
    let bob = native_login(&w, "device-bob", "phone", None).await;
    assert_eq!(get(&w, "devices", &bob).await["sync_capacity"], capacity(0));
    assert_eq!(profiles(&w).await, 2, "one profile per signed-in device");
    Ok(())
}

// ------------------------------------- B8, revision 2: membership changes that overlap a decision
//
// Review 2 R1 (family `device-profile-membership-boundary`): a transition that decides a device
// is still listed (and keeps its name) must not be undone by a removal, or seen undone by a
// listing, before it commits. Each case runs both orders through the real router: removal first
// (the device is seen absent, so the new member starts it unnamed) and transition first (the
// removal and the listings wait for it, and the device stays listed and named). The variant with
// another live session on the device is the control in which the name always survives.

const TABLET_NAME: &str = "Recognisable tablet";

async fn sign_out(app: &axum::Router, token: &str) -> Reply {
    request(
        app,
        "DELETE",
        "sessions/current",
        &[("authorization", &bearer(token))],
        Value::Null,
    )
    .await
}

async fn sync_as(app: &axum::Router, token: &str) -> Reply {
    request(
        app,
        "GET",
        "sync",
        &[("authorization", &bearer(token))],
        Value::Null,
    )
    .await
}

fn spawn_reply<F>(future: F) -> JoinHandle<Reply>
where
    F: std::future::Future<Output = Reply> + Send + 'static,
{
    tokio::spawn(future)
}

/// Prove every task waits for the paused transition. PostgreSQL: as many backends in a `Lock` wait
/// whose `pg_blocking_pids` chain leads to `pid` (a second waiter for the account row queues
/// behind the first, which holds the row's tuple lock). SQLite: none returns within a second
/// (inside the 10 s busy timeout), as `retirement::assert_blocked` does.
async fn assert_waiting(
    w: &World,
    pid: Option<i32>,
    tasks: &mut [&mut JoinHandle<Reply>],
) -> Result<()> {
    if let Some(pid) = pid {
        let count = tasks.len() as i64;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting: i64 = sqlx::query_scalar(
                    "WITH RECURSIVE blocked(pid) AS (SELECT pid FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)) UNION SELECT a.pid FROM pg_stat_activity a JOIN blocked b ON b.pid = ANY(pg_blocking_pids(a.pid))) SELECT COUNT(*) FROM blocked JOIN pg_stat_activity s USING (pid) WHERE s.wait_event_type='Lock'",
                )
                .bind(pid)
                .fetch_one(&w.store.pool)
                .await?;
                if waiting >= count {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        for task in tasks.iter() {
            assert!(
                !task.is_finished(),
                "a waiting request must not have finished"
            );
        }
    } else {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        for task in tasks.iter() {
            assert!(
                !task.is_finished(),
                "the request must still be waiting for the transition's writer slot"
            );
        }
    }
    Ok(())
}

/// Admin and tablet sessions; with `keep_session` a second tablet session that is never signed
/// out. The tablet is named. Returns the admin and the tablet's first session.
async fn named_tablet(w: &World, keep_session: bool) -> (String, String) {
    let admin = login(&w.app, "device-alice", "admin").await;
    let tablet = login(&w.app, "device-alice", "tablet").await;
    if keep_session {
        login(&w.app, "device-alice", "tablet").await;
    }
    no_content(&name(w, &admin, "tablet", json!({"name":TABLET_NAME})).await);
    (admin, tablet)
}

fn expected_name(keep: bool) -> Value {
    if keep {
        json!(TABLET_NAME)
    } else {
        Value::Null
    }
}

/// Review 2 reproduction A, removal first: the tablet's last session is signed out, and a listing
/// sees the tablet absent, while a browser sign-in for it has authenticated and holds
/// `begin_serial()` but not yet the account's ordering point. The grant then lists the tablet
/// unnamed, pending and after activation. On SQLite the paused sign-in would hold the single
/// writer slot, so the sign-out cannot commit first; the same order is run sequentially there.
async fn grant_after_a_committed_sign_out(keep_session: bool) -> Result<()> {
    let w = world(true).await?;
    let (admin, tablet) = named_tablet(&w, keep_session).await;
    let case = format!("keep_session={keep_session}");
    let grant = if postgres() {
        let mut gate = w.store.hooks().arm("browser_login.before_order");
        let app = w.app.clone();
        let task = spawn_reply(async move { issue_grant(&app, "device-alice", "tablet").await });
        gate.reached().await;
        assert_eq!(sign_out(&w.app, &tablet).await.0, StatusCode::NO_CONTENT);
        let between = listed(&w.app, &admin, "tablet").await;
        assert_eq!(between.is_some(), keep_session, "{case}: {between:?}");
        gate.release();
        task.await?
    } else {
        assert_eq!(sign_out(&w.app, &tablet).await.0, StatusCode::NO_CONTENT);
        let between = listed(&w.app, &admin, "tablet").await;
        assert_eq!(between.is_some(), keep_session, "{case}: {between:?}");
        issue_grant(&w.app, "device-alice", "tablet").await
    };
    assert_eq!(grant.0, StatusCode::OK, "{case}: {}", grant.2);
    let pending = listed(&w.app, &admin, "tablet").await.unwrap();
    assert_eq!(pending["summary"]["pending_sign_ins"], 1, "{case}");
    assert_eq!(
        pending["name"],
        expected_name(keep_session),
        "{case}: {pending}"
    );
    let active = activate(
        &w.app,
        grant.2["grant"].as_str().unwrap(),
        grant.2["verifier"].as_str().unwrap(),
    )
    .await;
    assert_eq!(active.0, StatusCode::OK, "{case}: {}", active.2);
    let after = listed(&w.app, &admin, "tablet").await.unwrap();
    assert_eq!(
        after["name"],
        expected_name(keep_session),
        "{case}: {after}"
    );
    Ok(())
}

#[tokio::test]
async fn a_grant_after_the_last_sign_out_lists_the_device_unnamed() -> Result<()> {
    grant_after_a_committed_sign_out(false).await
}

#[tokio::test]
async fn a_grant_after_a_sign_out_beside_another_session_keeps_the_name() -> Result<()> {
    grant_after_a_committed_sign_out(true).await
}

/// The two session removals outside `begin_serial()`: the session's own sign-out, and the
/// unkeyed `DELETE /sessions/{id}` by another session of the account.
#[derive(Clone, Copy, Debug)]
enum Removal {
    SignOut,
    Revoke,
}

/// Review 2 reproduction A, transition first: the browser sign-in has issued its grant (deciding
/// the tablet is listed) and is paused before commit, holding the account's ordering point. The
/// removal of the tablet's (first) session, `GET /devices` and `GET /sessions` all wait for it;
/// the tablet then stays listed by the grant throughout, so it keeps its name, also after
/// activation.
async fn grant_holding_the_point_through_a_sign_out(
    keep_session: bool,
    removal: Removal,
) -> Result<()> {
    let w = world(true).await?;
    let (admin, tablet) = named_tablet(&w, keep_session).await;
    let case = format!("keep_session={keep_session} removal={removal:?}");
    let first = session_of(&w.app, &admin, "tablet").await[0].clone();
    // The paused sign-in's own backend, recorded by the in-transaction hook: the cases share one
    // PostgreSQL database (a schema each), so nothing found by searching it would be specific.
    let point = "browser_login.before_commit";
    let mut gate = w.store.hooks().arm(point);
    let app = w.app.clone();
    let grant = spawn_reply(async move { issue_grant(&app, "device-alice", "tablet").await });
    gate.reached().await;
    let pid = w.store.hooks().backend_pid(point);
    assert_eq!(pid.is_some(), postgres());
    let mut out = {
        let (app, tablet, admin) = (w.app.clone(), tablet.clone(), admin.clone());
        spawn_reply(async move {
            match removal {
                Removal::SignOut => sign_out(&app, &tablet).await,
                Removal::Revoke => {
                    request(
                        &app,
                        "DELETE",
                        &format!("sessions/{first}"),
                        &[("authorization", &bearer(&admin))],
                        Value::Null,
                    )
                    .await
                }
            }
        })
    };
    let mut devices = {
        let (app, admin) = (w.app.clone(), admin.clone());
        spawn_reply(async move {
            request(
                &app,
                "GET",
                "devices",
                &[("authorization", &bearer(&admin))],
                Value::Null,
            )
            .await
        })
    };
    let mut sessions = {
        let (app, admin) = (w.app.clone(), admin.clone());
        spawn_reply(async move {
            request(
                &app,
                "GET",
                "sessions",
                &[("authorization", &bearer(&admin))],
                Value::Null,
            )
            .await
        })
    };
    assert_waiting(&w, pid, &mut [&mut out, &mut devices, &mut sessions]).await?;
    gate.release();
    let grant = grant.await?;
    assert_eq!(grant.0, StatusCode::OK, "{case}: {}", grant.2);
    assert_eq!(out.await?.0, StatusCode::NO_CONTENT, "{case}");
    // Whether each listing ran before or after the sign-out, the tablet is listed and named.
    let devices = devices.await?;
    assert_eq!(devices.0, StatusCode::OK);
    let seen = devices.2["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == "tablet")
        .cloned()
        .unwrap_or_else(|| panic!("{case}: the tablet is never absent: {}", devices.2));
    assert_eq!(seen["name"], TABLET_NAME, "{case}");
    assert_eq!(seen["summary"]["pending_sign_ins"], 1, "{case}");
    let sessions = sessions.await?;
    assert_eq!(sessions.0, StatusCode::OK);
    for row in sessions.2["sessions"].as_array().unwrap() {
        if row["device_id"] == "tablet" {
            assert_eq!(row["device_name"], TABLET_NAME, "{case}");
        }
    }
    let after = listed(&w.app, &admin, "tablet").await.unwrap();
    assert_eq!(after["name"], TABLET_NAME, "{case}");
    assert_eq!(
        after["active_sessions"],
        if keep_session { 1 } else { 0 },
        "{case}"
    );
    let active = activate(
        &w.app,
        grant.2["grant"].as_str().unwrap(),
        grant.2["verifier"].as_str().unwrap(),
    )
    .await;
    assert_eq!(active.0, StatusCode::OK, "{case}: {}", active.2);
    let after = listed(&w.app, &admin, "tablet").await.unwrap();
    assert_eq!(after["name"], TABLET_NAME, "{case}");
    Ok(())
}

#[tokio::test]
async fn a_sign_out_waits_for_a_grant_that_kept_the_device_listed() -> Result<()> {
    grant_holding_the_point_through_a_sign_out(false, Removal::SignOut).await
}

#[tokio::test]
async fn a_sign_out_waits_for_a_grant_beside_another_session() -> Result<()> {
    grant_holding_the_point_through_a_sign_out(true, Removal::SignOut).await
}

#[tokio::test]
async fn an_unkeyed_revocation_waits_for_a_grant_that_kept_the_device_listed() -> Result<()> {
    grant_holding_the_point_through_a_sign_out(false, Removal::Revoke).await
}

/// Review 2 reproduction B, removal first: the tablet's `GET /sync` authenticated with its last
/// session, which is then signed out, and a listing sees the tablet absent, before the sync
/// registers it. The sync still succeeds (its authentication preceded the sign-out) and lists the
/// tablet unnamed. PostgreSQL holds the sync after its REPEATABLE READ snapshot, which still shows
/// the session: the sign-out's ordering point fails the sync's own, and the attempt is retried on
/// a new snapshot (two arrivals). SQLite holds it after authentication, before its transaction.
async fn sync_after_a_committed_sign_out(keep_session: bool) -> Result<()> {
    let w = world(false).await?;
    let (admin, tablet) = named_tablet(&w, keep_session).await;
    let case = format!("keep_session={keep_session}");
    let point = if postgres() {
        "sync.before_registration_order"
    } else {
        "sync_route.after_identity"
    };
    let mut gate = w.store.hooks().arm(point);
    let sync = {
        let (app, tablet) = (w.app.clone(), tablet.clone());
        spawn_reply(async move { sync_as(&app, &tablet).await })
    };
    gate.reached().await;
    assert_eq!(sign_out(&w.app, &tablet).await.0, StatusCode::NO_CONTENT);
    let between = listed(&w.app, &admin, "tablet").await;
    assert_eq!(between.is_some(), keep_session, "{case}: {between:?}");
    gate.release();
    let sync = sync.await?;
    assert_eq!(sync.0, StatusCode::OK, "{case}: {}", sync.2);
    if postgres() {
        assert_eq!(
            w.store.hooks().count(point),
            2,
            "{case}: one retried attempt"
        );
    }
    let after = listed(&w.app, &admin, "tablet").await.unwrap();
    assert_eq!(after["summary"]["sync_registered"], true, "{case}");
    assert_eq!(
        after["name"],
        expected_name(keep_session),
        "{case}: {after}"
    );
    Ok(())
}

#[tokio::test]
async fn a_sync_registration_after_the_last_sign_out_lists_the_device_unnamed() -> Result<()> {
    sync_after_a_committed_sign_out(false).await
}

#[tokio::test]
async fn a_sync_registration_after_a_sign_out_beside_another_session_keeps_the_name() -> Result<()>
{
    sync_after_a_committed_sign_out(true).await
}

/// Review 2 reproduction B, transition first: the sync has decided (holding the account's
/// ordering point, after its prune) and is paused before inserting the registration. The sign-out
/// and the listing wait; the tablet is then listed throughout by the registration and keeps its
/// name.
async fn sync_holding_the_point_through_a_sign_out(keep_session: bool) -> Result<()> {
    let w = world(false).await?;
    let (admin, tablet) = named_tablet(&w, keep_session).await;
    let case = format!("keep_session={keep_session}");
    let point = "sync.before_registration";
    let mut gate = w.store.hooks().arm(point);
    let sync = {
        let (app, tablet) = (w.app.clone(), tablet.clone());
        spawn_reply(async move { sync_as(&app, &tablet).await })
    };
    gate.reached().await;
    let pid = w.store.hooks().backend_pid(point);
    assert_eq!(pid.is_some(), postgres());
    let mut out = {
        let (app, tablet) = (w.app.clone(), tablet.clone());
        spawn_reply(async move { sign_out(&app, &tablet).await })
    };
    let mut devices = {
        let (app, admin) = (w.app.clone(), admin.clone());
        spawn_reply(async move {
            request(
                &app,
                "GET",
                "devices",
                &[("authorization", &bearer(&admin))],
                Value::Null,
            )
            .await
        })
    };
    assert_waiting(&w, pid, &mut [&mut out, &mut devices]).await?;
    gate.release();
    let sync = sync.await?;
    assert_eq!(sync.0, StatusCode::OK, "{case}: {}", sync.2);
    assert_eq!(out.await?.0, StatusCode::NO_CONTENT, "{case}");
    let devices = devices.await?;
    let seen = devices.2["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == "tablet")
        .cloned()
        .unwrap_or_else(|| panic!("{case}: the tablet is never absent: {}", devices.2));
    assert_eq!(seen["name"], TABLET_NAME, "{case}");
    let after = listed(&w.app, &admin, "tablet").await.unwrap();
    assert_eq!(after["summary"]["sync_registered"], true, "{case}");
    assert_eq!(
        after["active_sessions"],
        if keep_session { 1 } else { 0 },
        "{case}"
    );
    assert_eq!(after["name"], TABLET_NAME, "{case}");
    Ok(())
}

#[tokio::test]
async fn a_sign_out_waits_for_a_sync_registration_that_kept_the_device_listed() -> Result<()> {
    sync_holding_the_point_through_a_sign_out(false).await
}

#[tokio::test]
async fn a_sign_out_waits_for_a_sync_registration_beside_another_session() -> Result<()> {
    sync_holding_the_point_through_a_sign_out(true).await
}
