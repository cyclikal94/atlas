use crate::support::http::session_request as call;
use atlas_core::Store;
use atlas_server::{App, hash_password, reset_password};
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;
async fn scenario(store: Store) -> anyhow::Result<()> {
    store.migrate().await?;
    let hash = hash_password("initial-password-123".into()).await?;
    store
        .add_account(&Uuid::new_v4().to_string(), "session-alice", &hash)
        .await?;
    store
        .add_account(&Uuid::new_v4().to_string(), "session-bob", &hash)
        .await?;
    let app = App::new(store.clone()).await?.router();
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
    assert_eq!(
        call(
            &app,
            "DELETE",
            "devices/phone",
            Some(&tokens[2]),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
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
    reset_password(&store, "session-alice", &hash).await?;
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
