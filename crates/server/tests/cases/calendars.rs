use anyhow::Result;
use atlas_core::Store;
use atlas_server::{App, hash_password, integrations::IntegrationConfig};
use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;
fn id() -> String {
    Uuid::new_v4().to_string()
}
use crate::support::http::command_request as call;
#[tokio::test]
async fn http_calendar_import_replay_and_privacy() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = id();
    store
        .add_account(
            &actor,
            "calendar-user",
            &hash_password("calendar-password-123".into()).await?,
        )
        .await?;
    let app = App::new(store.clone())
        .await?
        .integration_config(IntegrationConfig::new(None, vec![], None, None)?)
        .router();
    let (status,login)=call(&app,"sessions",None,None,Some(json!({"username":"calendar-user","password":"calendar-password-123","device_id":"phone"}))).await;
    assert_eq!(status, StatusCode::OK);
    let token = login["access_token"].as_str().unwrap();
    assert_eq!(
        call(&app, "events", None, None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let source = id();
    let op = id();
    let create =
        json!({"command":{"kind":"create_source","id":source,"label":"Calendar","timezone":"UTC"}});
    let first = call(
        &app,
        "calendar-commands",
        Some(token),
        Some(&op),
        Some(create.clone()),
    )
    .await;
    assert_eq!(first.0, StatusCode::OK, "{:?}", first.1);
    assert_eq!(
        first,
        call(
            &app,
            "calendar-commands",
            Some(token),
            Some(&op),
            Some(create)
        )
        .await
    );
    let path = format!("calendar-sources/{source}/import");
    let text = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:trip\r\nDTSTART:20260910T090000Z\r\nDESCRIPTION:{}\r\nSUMMARY:Private trip\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        "x".repeat(70000)
    );
    let import = json!({"ics":text,"from":"2026-09-01","through":"2026-12-31"});
    let op = id();
    let first = call(&app, &path, Some(token), Some(&op), Some(import.clone())).await;
    assert_eq!(first.0, StatusCode::OK, "{:?}", first.1);
    assert_eq!(
        first,
        call(&app, &path, Some(token), Some(&op), Some(import.clone())).await
    );
    let mut changed = import;
    changed["through"] = json!("2026-12-30");
    assert_eq!(
        call(&app, &path, Some(token), Some(&op), Some(changed))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let events = call(&app, "events", Some(token), None, None).await;
    assert_eq!(events.1["items"].as_array().unwrap().len(), 1);
    let invalid = json!({"ics":"invalid","from":"2026-09-01","through":"2026-12-31"});
    assert_eq!(
        call(&app, &path, Some(token), Some(&id()), Some(invalid))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(events, call(&app, "events", Some(token), None, None).await);
    let sources = call(&app, "calendar-sources", Some(token), None, None).await;
    let value = &sources.1["items"][0]["value"];
    assert_eq!(value["health"], "invalid_ics");
    assert!(value.get("connection").is_none());
    let caps = call(&app, "notification-capabilities", Some(token), None, None).await;
    assert_eq!(caps.1["web_push"], false);
    assert_eq!(caps.1["native_local"], true);
    Ok(())
}
