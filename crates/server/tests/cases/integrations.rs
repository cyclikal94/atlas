use anyhow::Result;
use atlas_core::calendars::Notification;
use atlas_server::integrations::{IntegrationConfig, Link, Subscription, public_ip};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

#[test]
fn secret_scope_replay_and_address_boundaries() -> Result<()> {
    let config = IntegrationConfig::new(Some(&"01".repeat(32)), vec![], None, None)?;
    let first = config.seal("calendar:a", "private-token")?;
    let second = config.seal("calendar:a", "private-token")?;
    assert_ne!(first, second);
    assert_eq!(first.split(':').nth(1), second.split(':').nth(1));
    assert!(!first.contains("private-token"));
    assert_eq!(config.open("calendar:a", &first)?, "private-token");
    assert!(config.open("calendar:b", &first).is_err());
    let tampered = first.replacen("v1:", "v1:x", 1);
    assert!(config.open("calendar:a", &tampered).is_err());
    assert!(
        IntegrationConfig::new(Some(&"02".repeat(32)), vec![], None, None)?
            .open("calendar:a", &first)
            .is_err()
    );
    for address in [
        "127.0.0.1",
        "10.2.3.4",
        "169.254.169.254",
        "100.64.0.1",
        "198.18.0.1",
        "192.0.2.1",
        "240.0.0.1",
        "::1",
        "::ffff:127.0.0.1",
        "fc00::1",
        "fe80::1",
        "2002:7f00:1::",
        "2001:db8::1",
    ] {
        assert!(!public_ip(address.parse()?), "{address}");
    }
    for address in ["8.8.8.8", "2606:4700:4700::1111"] {
        assert!(public_ip(address.parse()?), "{address}");
    }
    Ok(())
}
type Received = Arc<Mutex<Vec<(HeaderMap, Bytes)>>>;
async fn capture(State(received): State<Received>, headers: HeaderMap, body: Bytes) -> StatusCode {
    received.lock().await.push((headers, body));
    StatusCode::CREATED
}
#[tokio::test]
async fn bounded_fetch_and_real_push_payloads() -> Result<()> {
    let received = Received::default();
    let router = Router::new()
        .route("/push", post(capture))
        .route("/gone", post(|| async { StatusCode::GONE }))
        .route("/retry", post(|| async { StatusCode::SERVICE_UNAVAILABLE }))
        .route(
            "/redirect",
            get(|| async {
                (
                    StatusCode::FOUND,
                    [("location", "http://127.0.0.1:1/private")],
                )
            }),
        )
        .route("/large", get(|| async { "x".repeat(1024 * 1024 + 1) }))
        .route(
            "/feed",
            get(|headers: HeaderMap| async move {
                if headers.get("if-none-match").is_some_and(|v| v == "\"v1\"") {
                    (StatusCode::NOT_MODIFIED, [("etag", "\"v1\"")], "")
                } else {
                    (
                        StatusCode::OK,
                        [("etag", "\"v1\"")],
                        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n",
                    )
                }
            }),
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
    let default = IntegrationConfig::new(None, vec![], None, None)?;
    assert_eq!(
        default
            .client("https://127.0.0.1/private")
            .await
            .err()
            .unwrap()
            .to_string(),
        "outbound_denied"
    );
    assert!(default.client(&origin).await.is_err());
    let link = |path: &str| Link {
        url: format!("{origin}/{path}"),
        bearer: None,
    };
    let (text, etag, modified) = config.fetch(&link("feed"), None, None).await?;
    assert!(text.unwrap().contains("VCALENDAR"));
    assert_eq!(
        config
            .fetch(&link("feed"), etag.as_deref(), modified.as_deref())
            .await?
            .0,
        None
    );
    assert_eq!(
        config
            .fetch(&link("redirect"), None, None)
            .await
            .unwrap_err()
            .to_string(),
        "fetch_failed"
    );
    assert_eq!(
        config
            .fetch(&link("large"), None, None)
            .await
            .unwrap_err()
            .to_string(),
        "calendar_limit"
    );
    let notification = Notification {
        id: Uuid::new_v4().to_string(),
        reminder_id: Uuid::new_v4().to_string(),
        occurrence_id: Uuid::new_v4().to_string(),
    };
    let ntfy = Subscription::Ntfy {
        url: format!("{origin}/push"),
        bearer: Some("test-token".into()),
    };
    config.validate_subscription(&ntfy).await?;
    assert_eq!(
        config.deliver(&ntfy, &notification, i64::MAX).await?,
        (true, false)
    );
    let (key, auth) = ece::generate_keypair_and_auth_secret()?;
    let web = Subscription::WebPush {
        endpoint: format!("{origin}/push"),
        p256dh: URL_SAFE_NO_PAD.encode(key.pub_as_raw()?),
        auth: URL_SAFE_NO_PAD.encode(auth),
    };
    config.validate_subscription(&web).await?;
    assert_eq!(
        config.deliver(&web, &notification, i64::MAX).await?,
        (true, false)
    );
    let messages = received.lock().await;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].0["authorization"], "Bearer test-token");
    assert!(String::from_utf8(messages[0].1.to_vec())?.contains(&notification.id));
    assert_eq!(messages[1].0["content-encoding"], "aes128gcm");
    assert!(
        messages[1].0["authorization"]
            .to_str()?
            .starts_with("vapid ")
    );
    assert_eq!(messages[1].0["topic"], notification.id.replace('-', ""));
    let plain = ece::decrypt(&key.raw_components()?, &auth, &messages[1].1)?;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&plain)?,
        serde_json::to_value(&notification)?
    );
    drop(messages);
    for (path, expected) in [("gone", (false, true)), ("retry", (false, false))] {
        assert_eq!(
            config
                .deliver(
                    &Subscription::Ntfy {
                        url: format!("{origin}/{path}"),
                        bearer: None
                    },
                    &notification,
                    i64::MAX
                )
                .await?,
            expected
        );
    }
    server.abort();
    Ok(())
}

#[tokio::test]
async fn worker_refreshes_encrypted_link_and_dispatches_persisted_rule() -> Result<()> {
    use atlas_core::{Store, calendars::*, tasks::*};
    use atlas_server::App;
    let received = Received::default();
    let router = Router::new()
        .route("/push", post(capture))
        .route(
            "/feed",
            get(|| async { "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n" }),
        )
        .with_state(received.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let config = IntegrationConfig::new(Some(&"01".repeat(32)), vec![origin.clone()], None, None)?;
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let id = || Uuid::new_v4().to_string();
    let actor = id();
    store.add_account(&actor, "worker", "unused").await?;
    let source = id();
    let connection = config.seal(
        &format!("source:{source}"),
        &serde_json::to_string(&Link {
            url: format!("{origin}/feed"),
            bearer: None,
        })?,
    )?;
    store
        .calendar_command(
            &actor,
            &id(),
            &CalendarCommand::CreateSource {
                id: source.clone(),
                label: "Feed".into(),
                timezone: "UTC".into(),
                connection: Some(connection),
                initial_policy: None,
            },
        )
        .await?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let today = chrono::DateTime::from_timestamp(now, 0)
        .unwrap()
        .date_naive()
        .to_string();
    let task = id();
    store
        .task_command(
            &actor,
            &id(),
            &TaskCommand::CreateTask {
                id: task.clone(),
                execution_id: id(),
                title: "Private reminder text".into(),
                definition: Definition {
                    schedule: Schedule {
                        start_date: Some(today),
                        time: Some("00:00:00".into()),
                        timezone: "UTC".into(),
                        repeat: None,
                    },
                    goal: Goal::Checkbox,
                    carry: Carry::RetainOne,
                    participation: Participation::Personal,
                    open_days_before: 0,
                    close_days_after: 1,
                    allow_streak_exclusions: false,
                },
                initial_policy: None,
                anchor: None,
            },
            None,
            now,
        )
        .await?;
    let occurrence = store
        .occurrence_view(
            &actor,
            &ViewFilter {
                task_id: Some(task),
                ..Default::default()
            },
            now,
        )
        .await?
        .items
        .remove(0)
        .id;
    store
        .reminder_command(
            &actor,
            &id(),
            &ReminderCommand::SetRule {
                id: id(),
                occurrence_id: occurrence,
                expected_version: None,
                rule: ReminderRule {
                    offset_seconds: 0,
                    time: None,
                    late_seconds: 86400,
                    enabled: true,
                    delivery: DeliveryMode::Server,
                },
            },
        )
        .await?;
    let sub = id();
    let subscription = Subscription::Ntfy {
        url: format!("{origin}/push"),
        bearer: Some("worker-token".into()),
    };
    let secret = config.seal(
        &format!("subscription:{sub}"),
        &serde_json::to_string(&subscription)?,
    )?;
    store
        .reminder_command(
            &actor,
            &id(),
            &ReminderCommand::SetSubscription {
                id: sub,
                expected_version: 0,
                device_id: "phone".into(),
                transport: "ntfy".into(),
                secret,
                enabled: true,
            },
        )
        .await?;
    let app = App::new(store.clone()).await?.integration_config(config);
    app.integration_tick().await?;
    app.integration_tick().await?;
    let messages = received.lock().await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0["authorization"], "Bearer worker-token");
    assert!(!String::from_utf8(messages[0].1.to_vec())?.contains("Private reminder text"));
    let source = store
        .calendar_resources(&actor, "calendar_source", None, None, 200)
        .await?
        .remove(0);
    assert_eq!(
        serde_json::from_value::<serde_json::Value>(source.value.clone())?["health"],
        "healthy"
    );
    let history = store.reminder_delivery_history(&actor, None, 200).await?;
    assert_eq!(history["items"][0]["state"], "delivered");
    server.abort();
    Ok(())
}
