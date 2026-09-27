use crate::support::http::session_request as call;
use crate::support::retirement::activate;
use atlas_core::Store;
use atlas_server::{App, hash_password, reset_password};
use axum::Router;
use axum::http::StatusCode;
use openidconnect::PkceCodeChallenge;
use serde_json::{Value, json};
use uuid::Uuid;

/// The grant, and whether it is still `issued`, for `account`/`device`.
async fn grant_state(store: &Store, account: &str, device: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT state FROM activation_grants WHERE account_id=$1 AND device_id=$2 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(account)
    .bind(device)
    .fetch_optional(&store.pool)
    .await
    .unwrap()
}

/// A BE-Q19 grant for `session-alice`/`device` under her current `password`, and the verifier
/// that redeems it.
async fn issue(app: &Router, device: &str, password: &str) -> (String, String) {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (status, _, body) = crate::support::http::request(
        app,
        "POST",
        "browser-sessions",
        &[("origin", "https://atlas.example")],
        json!({"username":"session-alice","password":password,"device_id":device,"attempt_challenge":challenge.as_str()}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    (
        body["grant"].as_str().unwrap().to_owned(),
        verifier.secret().to_owned(),
    )
}
async fn scenario(store: Store) -> anyhow::Result<()> {
    store.migrate().await?;
    let hash = hash_password("initial-password-123".into()).await?;
    let alice = Uuid::new_v4().to_string();
    store.add_account(&alice, "session-alice", &hash).await?;
    store
        .add_account(&Uuid::new_v4().to_string(), "session-bob", &hash)
        .await?;
    let app = App::new(store.clone())
        .await?
        .public_origin("https://atlas.example")?
        .router();
    let mut tokens = Vec::new();
    for (user, device) in [
        ("session-alice", "phone"),
        ("session-alice", "browser"),
        ("session-bob", "phone"),
    ] {
        let (status, body) = call(
            &app,
            "POST",
            "sessions",
            None,
            json!({"username":user,"password":"initial-password-123","device_id":device}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        tokens.push(body["access_token"].as_str().unwrap().to_owned());
    }
    let (status, devices) = call(&app, "GET", "devices", Some(&tokens[0]), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(devices["devices"].as_array().unwrap().len(), 2);
    assert_eq!(
        devices["devices"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["current"] == true)
            .count(),
        1
    );
    // Bob's same-named device is independent of Alice's device and sessions.
    let (_, bobs) = call(&app, "GET", "devices", Some(&tokens[2]), Value::Null).await;
    let state = bobs["devices"][0]["state_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let bearer = format!("Bearer {}", tokens[2]);
    let operation = Uuid::new_v4().to_string();
    let (status, headers, outcome) = crate::support::http::request(
        &app,
        "DELETE",
        "devices/phone",
        &[
            ("authorization", &bearer),
            ("idempotency-key", &operation),
            ("atlas-device-state", &state),
        ],
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(outcome["outcome"], "confirmed_applied");
    assert_eq!(outcome["operation_id"], operation);
    assert!(!headers.contains_key("set-cookie"));
    assert_eq!(
        call(&app, "GET", "devices", Some(&tokens[2]), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, "GET", "devices", Some(&tokens[0]), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    let (_, replacement) = call(
        &app,
        "POST",
        "sessions",
        None,
        json!({"username":"session-bob","password":"initial-password-123","device_id":"phone"}),
    )
    .await;
    tokens[2] = replacement["access_token"].as_str().unwrap().to_owned();
    let (status, list) = call(&app, "GET", "sessions", Some(&tokens[0]), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let entries = list["sessions"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries.iter().filter(|s| s["current"] == true).count(), 1);
    assert!(!list.to_string().contains("token"));
    let other = entries.iter().find(|s| s["current"] == false).unwrap()["id"]
        .as_str()
        .unwrap();
    assert_eq!(
        call(
            &app,
            "DELETE",
            &format!("sessions/{other}"),
            Some(&tokens[2]),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &app,
            "DELETE",
            &format!("sessions/{other}"),
            Some(&tokens[0]),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&app, "GET", "sessions", Some(&tokens[1]), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "password",
            Some(&tokens[0]),
            json!({"current_password":"wrong","new_password":"changed-password-456"})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    // An `issued` grant for the account is cancelled by the change; a `redeemed` one survives
    // untouched (BE-Q19 item 7).
    issue(&app, "grant-only", "initial-password-123").await;
    let (redeem_grant, redeem_verifier) =
        issue(&app, "grant-redeemed", "initial-password-123").await;
    let (status, _, redeemed) = activate(&app, &redeem_grant, &redeem_verifier).await;
    assert_eq!(status, StatusCode::OK, "{redeemed}");
    assert_eq!(
        call(
            &app,
            "POST",
            "password",
            Some(&tokens[0]),
            json!({"current_password":"initial-password-123","new_password":"changed-password-456"})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        grant_state(&store, &alice, "grant-only").await.as_deref(),
        Some("cancelled"),
        "change_password cancels an issued grant"
    );
    assert_eq!(
        grant_state(&store, &alice, "grant-redeemed")
            .await
            .as_deref(),
        Some("redeemed"),
        "change_password leaves a redeemed grant untouched"
    );
    assert_eq!(
        call(&app, "GET", "sessions", Some(&tokens[0]), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, "GET", "sessions", Some(&tokens[2]), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(call(&app,"POST","sessions",None,json!({"username":"session-alice","password":"initial-password-123","device_id":"phone"})).await.0,StatusCode::UNAUTHORIZED);
    let (status, session) = call(
        &app,
        "POST",
        "sessions",
        None,
        json!({"username":"session-alice","password":"changed-password-456","device_id":"phone"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    issue(&app, "grant-only", "changed-password-456").await;
    reset_password(&store, "session-alice", &hash).await?;
    assert_eq!(
        grant_state(&store, &alice, "grant-only").await.as_deref(),
        Some("cancelled"),
        "the operator reset_password also cancels an issued grant"
    );
    assert_eq!(
        grant_state(&store, &alice, "grant-redeemed")
            .await
            .as_deref(),
        Some("redeemed"),
        "reset_password leaves a redeemed grant untouched"
    );
    assert_eq!(
        call(
            &app,
            "GET",
            "sessions",
            Some(session["access_token"].as_str().unwrap()),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(call(&app,"POST","sessions",None,json!({"username":"session-alice","password":"initial-password-123","device_id":"phone"})).await.0,StatusCode::OK);
    Ok(())
}
#[tokio::test]
async fn sessions_and_password_recovery() -> anyhow::Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    scenario(Store::connect(&url).await?).await
}
