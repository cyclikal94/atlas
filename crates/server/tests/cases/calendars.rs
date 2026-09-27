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
/// BE-Q22: `configure_source` against the real router, a real store and real encryption —
/// omitting `connection` preserves the sealed value, `disconnect: true` clears it, an
/// explicit `connection` still replaces it, and sending both is rejected. The allow-listed
/// loopback origin resolves via DNS with no listener needed: `configure_source` never calls
/// `fetch()`, only `refresh_link`/`import_text` do.
#[tokio::test]
async fn http_calendar_configure_source_connection_semantics() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = id();
    store
        .add_account(
            &actor,
            "calendar-configure-user",
            &hash_password("calendar-configure-password-123".into()).await?,
        )
        .await?;
    let key = "04".repeat(32);
    let origin = "http://127.0.0.1:59999".to_string();
    let app = App::new(store.clone())
        .await?
        .integration_config(IntegrationConfig::new(
            Some(&key),
            vec![origin.clone()],
            None,
            None,
        )?)
        .router();
    let (status, login) = call(
        &app,
        "sessions",
        None,
        None,
        Some(
            json!({"username":"calendar-configure-user","password":"calendar-configure-password-123","device_id":"phone"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = login["access_token"].as_str().unwrap();
    let bearer_secret = "super-secret-bearer-token";

    let source = id();
    let create = json!({"command":{"kind":"create_source","id":source,"label":"Calendar","timezone":"UTC",
        "connection":{"url":format!("{origin}/feed"),"bearer":bearer_secret}}});
    let created = call(
        &app,
        "calendar-commands",
        Some(token),
        Some(&id()),
        Some(create),
    )
    .await;
    assert_eq!(created.0, StatusCode::OK, "{:?}", created.1);
    assert!(!created.1.to_string().contains(bearer_secret));

    let stored_after_create: Option<String> =
        sqlx::query_scalar("SELECT connection FROM calendar_sources WHERE id=$1")
            .bind(&source)
            .fetch_one(&store.pool)
            .await?;
    let sealed = stored_after_create.expect("connection sealed at creation");

    // Omitting `connection` on a credential-free edit (timezone-only) preserves it exactly.
    let omitted = json!({"command":{"kind":"configure_source","id":source,"expected_version":2,
        "timezone":"Europe/London","enabled":true}});
    let omitted_reply = call(
        &app,
        "calendar-commands",
        Some(token),
        Some(&id()),
        Some(omitted),
    )
    .await;
    assert_eq!(omitted_reply.0, StatusCode::OK, "{:?}", omitted_reply.1);
    assert!(!omitted_reply.1.to_string().contains(bearer_secret));
    let stored_after_omit: Option<String> =
        sqlx::query_scalar("SELECT connection FROM calendar_sources WHERE id=$1")
            .bind(&source)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(
        stored_after_omit,
        Some(sealed.clone()),
        "omission must preserve the sealed connection unchanged"
    );

    // Sending both `connection` and `disconnect: true` is rejected, with no state change.
    let contradiction = json!({"command":{"kind":"configure_source","id":source,"expected_version":3,
        "timezone":"UTC","connection":{"url":format!("{origin}/feed"),"bearer":"other-secret"},
        "disconnect":true,"enabled":true}});
    let contradiction_reply = call(
        &app,
        "calendar-commands",
        Some(token),
        Some(&id()),
        Some(contradiction),
    )
    .await;
    assert_eq!(
        contradiction_reply.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{:?}",
        contradiction_reply.1
    );
    assert_eq!(contradiction_reply.1["code"], "invalid_value");
    assert!(!contradiction_reply.1.to_string().contains("other-secret"));
    let stored_after_contradiction: Option<String> =
        sqlx::query_scalar("SELECT connection FROM calendar_sources WHERE id=$1")
            .bind(&source)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(stored_after_contradiction, Some(sealed));

    // Explicit `disconnect: true` clears the stored connection.
    let disconnect = json!({"command":{"kind":"configure_source","id":source,"expected_version":3,
        "timezone":"UTC","disconnect":true,"enabled":true}});
    let disconnect_reply = call(
        &app,
        "calendar-commands",
        Some(token),
        Some(&id()),
        Some(disconnect),
    )
    .await;
    assert_eq!(
        disconnect_reply.0,
        StatusCode::OK,
        "{:?}",
        disconnect_reply.1
    );
    let stored_after_disconnect: Option<String> =
        sqlx::query_scalar("SELECT connection FROM calendar_sources WHERE id=$1")
            .bind(&source)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(stored_after_disconnect, None);

    Ok(())
}

/// BE-CH3: exhausting the two real `calendar_slots` permits (a genuine `Semaphore`, reached
/// only through real HTTP requests — no test-only accessor is added) via two in-flight
/// `refresh_link` requests, each blocked awaiting a response from a loopback origin that never
/// completes, makes a third, concurrent `refresh_link` on the same router return `503
/// temporarily_unavailable` immediately. This is the fixed `Some(ErrorCode::TemporarilyUnavailable)`
/// arm, not the unrelated `_ if busy` database-busy guard: this test never induces a database
/// lock (single writer, no concurrent conflicting transaction), so a 503 here can only have come
/// from the application-error arm.
#[tokio::test]
async fn http_calendar_refresh_capacity_exhaustion_is_service_unavailable() -> Result<()> {
    use axum::{Router as FeedRouter, routing::get as feed_get};
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};

    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = id();
    store
        .add_account(
            &actor,
            "calendar-refresh-user",
            &hash_password("calendar-refresh-password-123".into()).await?,
        )
        .await?;

    // A loopback origin that accepts the connection, signals arrival, then never responds —
    // holding the `reqwest` client (and therefore the `calendar_slots` permit) open until the
    // test aborts this server.
    let (arrived_tx, mut arrived_rx) = mpsc::channel::<()>(2);
    let feed = FeedRouter::new().route(
        "/feed",
        feed_get(move || {
            let arrived_tx = arrived_tx.clone();
            async move {
                arrived_tx.send(()).await.ok();
                std::future::pending::<StatusCode>().await
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let feed_server = tokio::spawn(async move { axum::serve(listener, feed).await });

    let key = "05".repeat(32);
    let app = App::new(store.clone())
        .await?
        .integration_config(IntegrationConfig::new(
            Some(&key),
            vec![origin.clone()],
            None,
            None,
        )?)
        .router();
    let (status, login) = call(
        &app,
        "sessions",
        None,
        None,
        Some(
            json!({"username":"calendar-refresh-user","password":"calendar-refresh-password-123","device_id":"phone"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = login["access_token"].as_str().unwrap().to_string();

    let mut sources = Vec::new();
    for label in ["a", "b", "c"] {
        let source = id();
        let create = json!({"command":{"kind":"create_source","id":source,"label":label,"timezone":"UTC",
            "connection":{"url":format!("{origin}/feed"),"bearer":null}}});
        let created = call(
            &app,
            "calendar-commands",
            Some(&token),
            Some(&id()),
            Some(create),
        )
        .await;
        assert_eq!(created.0, StatusCode::OK, "{:?}", created.1);
        sources.push(source);
    }
    let (a, b, c) = (sources[0].clone(), sources[1].clone(), sources[2].clone());

    // Two concurrent, real refresh requests: each must pass `try_acquire_owned()` and then
    // block inside `fetch()`, holding its permit for as long as the stalled origin holds the
    // connection open.
    let blocked: Vec<_> = [a, b]
        .into_iter()
        .map(|source| {
            let app = app.clone();
            let token = token.clone();
            tokio::spawn(async move {
                call(
                    &app,
                    &format!("calendar-sources/{source}/refresh"),
                    Some(&token),
                    Some(&id()),
                    Some(json!({})),
                )
                .await
            })
        })
        .collect();

    // Bounded, non-sleep synchronisation: only proceed once both spawned requests have reached
    // the stalled feed, i.e. both permits are genuinely held.
    for _ in 0..2 {
        timeout(Duration::from_secs(5), arrived_rx.recv())
            .await?
            .expect("feed connection");
    }

    // The third refresh, on the same real router, must return immediately — bounded so a
    // regression that reintroduces blocking fails loudly instead of hanging the test.
    let third = timeout(
        Duration::from_secs(2),
        call(
            &app,
            &format!("calendar-sources/{c}/refresh"),
            Some(&token),
            Some(&id()),
            Some(json!({})),
        ),
    )
    .await
    .expect("third refresh must not hang — permits are exhausted, not deadlocked");
    assert_eq!(third.0, StatusCode::SERVICE_UNAVAILABLE, "{:?}", third.1);
    assert_eq!(third.1["code"], "temporarily_unavailable");

    // Unblock and clean up: stop the stalled feed so the two spawned refreshes fail (irrelevant
    // to this test) and finish, then join them with a bounded wait so nothing leaks past return.
    feed_server.abort();
    for task in blocked {
        let _ = timeout(Duration::from_secs(5), task).await;
    }

    Ok(())
}
