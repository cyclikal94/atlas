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
use crate::support::http::{delayed_feed, switchable_feed};
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
async fn login(app: &axum::Router, username: &str, password: &str) -> Result<String> {
    let (status, login) = call(
        app,
        "sessions",
        None,
        None,
        Some(json!({"username":username,"password":password,"device_id":"phone"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{:?}", login);
    Ok(login["access_token"].as_str().unwrap().to_owned())
}
/// A router whose integration config holds `key` and allows fetching only from `origin`.
async fn app_allowing(store: &Store, key: &str, origin: &str) -> Result<axum::Router> {
    Ok(App::new(store.clone())
        .await?
        .integration_config(IntegrationConfig::new(
            Some(key),
            vec![origin.to_owned()],
            None,
            None,
        )?)
        .router())
}
/// Creates a source with a sealed provider connection through the real command route.
async fn create_linked_source(app: &axum::Router, token: &str, origin: &str) -> Result<String> {
    let source = id();
    let create = json!({"command":{"kind":"create_source","id":source,"label":"Calendar","timezone":"UTC",
        "connection":{"url":format!("{origin}/feed"),"bearer":"probe-secret"}}});
    let created = call(
        app,
        "calendar-commands",
        Some(token),
        Some(&id()),
        Some(create),
    )
    .await;
    assert_eq!(created.0, StatusCode::OK, "{:?}", created.1);
    Ok(source)
}
/// Starts a real linked refresh in the background, so a test can act while its fetch is held.
fn spawn_refresh(
    app: &axum::Router,
    token: &str,
    source: &str,
) -> tokio::task::JoinHandle<(StatusCode, serde_json::Value)> {
    let (app, token, source) = (app.clone(), token.to_owned(), source.to_owned());
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
}
/// The `refresh_in_progress` flag for one source, read through `GET /calendar-sources`.
async fn refresh_in_progress(app: &axum::Router, token: &str, source: &str) -> Option<bool> {
    let list = call(app, "calendar-sources", Some(token), None, None).await;
    assert_eq!(list.0, StatusCode::OK, "{:?}", list.1);
    list.1["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == source)
        .and_then(|v| v["value"]["refresh_in_progress"].as_bool())
}
/// Recorded health, refresh lease and next scheduled refresh of one source.
async fn source_state(store: &Store, source: &str) -> Result<(String, i64, i64)> {
    Ok(sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT health,lease_until,next_refresh FROM calendar_sources WHERE id=$1",
    )
    .bind(source)
    .fetch_one(&store.pool)
    .await?)
}
/// The account's events, read through the real route.
async fn events(app: &axum::Router, token: &str) -> serde_json::Value {
    let list = call(app, "events", Some(token), None, None).await;
    assert_eq!(list.0, StatusCode::OK, "{:?}", list.1);
    list.1
}
/// Asserts a rejected attempt settled rather than stranding the source: the failure is recorded as
/// health, the lease is released (so an immediate corrected request is not blocked behind
/// `refresh_in_progress`) and `next_refresh` was rewritten to about `now + 900` — a value only the
/// finish stage's fallback write can have produced once the test has reset it to a sentinel.
/// `started`/`ended` bracket the attempt in whole seconds.
async fn assert_settled(
    store: &Store,
    source: &str,
    health: &str,
    started: i64,
    ended: i64,
) -> Result<()> {
    let (recorded, lease_until, next_refresh) = source_state(store, source).await?;
    assert_eq!(
        recorded, health,
        "the fallback health write must succeed, not silently fail"
    );
    assert_eq!(lease_until, 0, "the lease must be released, not left live");
    assert!(
        (started + 900..=ended + 900).contains(&next_refresh),
        "next_refresh {next_refresh} must have advanced to about now+900 ({started}..={ended})"
    );
    Ok(())
}
/// BE-B8, cause 1: missing or undecryptable connection details never reach the provider and
/// never leak credentials in the error body. The health-publication allowlist extension (the
/// finish-function's failure-string allowlist, item 3 of the approved brief) is what keeps the
/// fallback health write from silently failing once `calendar_error()` stops collapsing this to
/// `fetch_failed` (item 2) — both are asserted together here, on the real fallback call path.
#[tokio::test]
async fn http_calendar_refresh_connection_unavailable_records_health() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = id();
    store
        .add_account(
            &actor,
            "calendar-connection-user",
            &hash_password("calendar-connection-password-123".into()).await?,
        )
        .await?;
    let origin = "http://127.0.0.1:59980".to_string();
    let key = "06".repeat(32);
    let app = App::new(store.clone())
        .await?
        .integration_config(IntegrationConfig::new(
            Some(&key),
            vec![origin.clone()],
            None,
            None,
        )?)
        .router();
    let token = login(
        &app,
        "calendar-connection-user",
        "calendar-connection-password-123",
    )
    .await?;

    // (a) no connection at all.
    let source_a = id();
    let create_a = json!({"command":{"kind":"create_source","id":source_a,"label":"Calendar","timezone":"UTC"}});
    let created_a = call(
        &app,
        "calendar-commands",
        Some(&token),
        Some(&id()),
        Some(create_a),
    )
    .await;
    assert_eq!(created_a.0, StatusCode::OK, "{:?}", created_a.1);
    let reply_a = call(
        &app,
        &format!("calendar-sources/{source_a}/refresh"),
        Some(&token),
        Some(&id()),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        reply_a.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{:?}",
        reply_a.1
    );
    assert_eq!(reply_a.1["code"], "connection_unavailable");
    let health_a: String = sqlx::query_scalar("SELECT health FROM calendar_sources WHERE id=$1")
        .bind(&source_a)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(
        health_a, "connection_unavailable",
        "the fallback health write must succeed once the allowlist accepts this reason"
    );

    // (b) a stored connection overwritten with a non-decryptable string (bypassing `seal()`).
    let source_b = id();
    let bearer_secret = "super-secret-bearer-must-never-leak";
    let create_b = json!({"command":{"kind":"create_source","id":source_b,"label":"Calendar","timezone":"UTC",
        "connection":{"url":format!("{origin}/feed"),"bearer":bearer_secret}}});
    let created_b = call(
        &app,
        "calendar-commands",
        Some(&token),
        Some(&id()),
        Some(create_b),
    )
    .await;
    assert_eq!(created_b.0, StatusCode::OK, "{:?}", created_b.1);
    assert!(!created_b.1.to_string().contains(bearer_secret));
    sqlx::query("UPDATE calendar_sources SET connection=$1 WHERE id=$2")
        .bind("not-a-valid-sealed-value")
        .bind(&source_b)
        .execute(&store.pool)
        .await?;
    let reply_b = call(
        &app,
        &format!("calendar-sources/{source_b}/refresh"),
        Some(&token),
        Some(&id()),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        reply_b.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{:?}",
        reply_b.1
    );
    assert_eq!(reply_b.1["code"], "connection_unavailable");
    assert!(!reply_b.1.to_string().contains(bearer_secret));
    let health_b: String = sqlx::query_scalar("SELECT health FROM calendar_sources WHERE id=$1")
        .bind(&source_b)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(health_b, "connection_unavailable");

    Ok(())
}
/// BE-B8, cause 2: a genuine provider fetch failure is the one cause `fetch_failed` is already
/// correct for — unchanged by the `calendar_error()` fix. Health still records it (unchanged
/// behaviour: `"fetch_failed"` was already in the finish-function's allowlist).
#[tokio::test]
async fn http_calendar_refresh_fetch_failure_unchanged_at_502() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = id();
    store
        .add_account(
            &actor,
            "calendar-fetch-failure-user",
            &hash_password("calendar-fetch-failure-password-123".into()).await?,
        )
        .await?;
    // Bind then immediately drop: the port is free but nothing is listening, so connecting
    // fails with a genuine connection-refused error, not a test double.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    drop(listener);
    let key = "08".repeat(32);
    let app = App::new(store.clone())
        .await?
        .integration_config(IntegrationConfig::new(
            Some(&key),
            vec![origin.clone()],
            None,
            None,
        )?)
        .router();
    let token = login(
        &app,
        "calendar-fetch-failure-user",
        "calendar-fetch-failure-password-123",
    )
    .await?;
    let source = id();
    let create = json!({"command":{"kind":"create_source","id":source,"label":"Calendar","timezone":"UTC",
        "connection":{"url":format!("{origin}/feed"),"bearer":"probe-secret"}}});
    let created = call(
        &app,
        "calendar-commands",
        Some(&token),
        Some(&id()),
        Some(create),
    )
    .await;
    assert_eq!(created.0, StatusCode::OK, "{:?}", created.1);
    let reply = call(
        &app,
        &format!("calendar-sources/{source}/refresh"),
        Some(&token),
        Some(&id()),
        Some(json!({})),
    )
    .await;
    assert_eq!(reply.0, StatusCode::BAD_GATEWAY, "{:?}", reply.1);
    assert_eq!(reply.1["code"], "fetch_failed");
    let health: String = sqlx::query_scalar("SELECT health FROM calendar_sources WHERE id=$1")
        .bind(&source)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(health, "fetch_failed");
    Ok(())
}
/// BE-B8 revision, R1: `ics::parse` raises `ErrorCode::InvalidValue` for a malformed
/// request-supplied date and for an oversized (>300 char) event `SUMMARY` — a different stage
/// from the finish-stage archived-check's `InvalidValue` (cause 6a). Before this fix,
/// `calendar_error()`'s blanket `InvalidValue` pass-through fed the literal string
/// `"invalid_value"` into `finish_calendar_refresh`'s failure-string allowlist, which does not
/// contain it, so the fallback health/lease-release write itself failed its own `ensure!` and
/// was silently discarded (`let _ = ...` at the `refresh_link` call site; a hard error at the
/// `import_text` call site, since that one propagates the fallback's own `?`). Health stayed
/// `"healthy"`, the lease was never released and `next_refresh` was never advanced, leaving an
/// immediate corrected retry blocked behind `409 refresh_in_progress`. This restores the pre-B8
/// outcome for these two parser triggers — `502 fetch_failed`, exactly as `calendar_error()`
/// mapped them before this card, since B8 does not name parser/content validation as one of its
/// six causes — and confirms the fallback health/lease/`next_refresh` write actually succeeds.
///
/// Every payload, on both call paths, starts from a source that already has healthy recorded
/// state and a published event, and asserts the same outcome: `502 fetch_failed`, health,
/// released lease and rewritten `next_refresh` (each reset to a sentinel first, so an untouched
/// value cannot pass), the prior events unchanged, and an immediate corrected request that
/// succeeds. `next_refresh` only steers the scheduler for a source with a connection, so for
/// the local-import half it shows that the fallback write ran to completion.
#[tokio::test]
async fn http_calendar_parse_stage_invalid_value_settles_as_fetch_failed() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = id();
    store
        .add_account(
            &actor,
            "calendar-parse-invalid-user",
            &hash_password("calendar-parse-invalid-password-123".into()).await?,
        )
        .await?;
    let app = App::new(store.clone())
        .await?
        .integration_config(IntegrationConfig::new(None, vec![], None, None)?)
        .router();
    let token = login(
        &app,
        "calendar-parse-invalid-user",
        "calendar-parse-invalid-password-123",
    )
    .await?;
    let reset_schedule = |source: String| {
        let pool = store.pool.clone();
        async move {
            sqlx::query("UPDATE calendar_sources SET next_refresh=1 WHERE id=$1")
                .bind(source)
                .execute(&pool)
                .await
        }
    };

    // Local import: a healthy source with one already-imported event, so a rejected attempt's
    // effect on both prior events and recorded state is checked, not just the wire code.
    let source = id();
    let create =
        json!({"command":{"kind":"create_source","id":source,"label":"Calendar","timezone":"UTC"}});
    let created = call(
        &app,
        "calendar-commands",
        Some(&token),
        Some(&id()),
        Some(create),
    )
    .await;
    assert_eq!(created.0, StatusCode::OK, "{:?}", created.1);
    let good_ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:trip\r\nDTSTART:20260910T090000Z\r\nSUMMARY:Trip\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let path = format!("calendar-sources/{source}/import");
    let corrected = json!({"ics":good_ics,"from":"2026-09-01","through":"2026-12-31"});
    let first = call(
        &app,
        &path,
        Some(&token),
        Some(&id()),
        Some(corrected.clone()),
    )
    .await;
    assert_eq!(first.0, StatusCode::OK, "{:?}", first.1);
    let events_before = events(&app, &token).await;
    assert_eq!(events_before["items"].as_array().unwrap().len(), 1);
    assert_eq!(source_state(&store, &source).await?.0, "healthy");

    let long_summary_ics = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:trip2\r\nDTSTART:20260911T090000Z\r\nSUMMARY:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        "x".repeat(301)
    );
    for (name, rejected) in [
        (
            "malformed from",
            json!({"ics":good_ics,"from":"not-a-date","through":"2026-12-31"}),
        ),
        (
            "oversized summary",
            json!({"ics":long_summary_ics,"from":"2026-09-01","through":"2026-12-31"}),
        ),
    ] {
        reset_schedule(source.clone()).await?;
        let started = atlas_server::now();
        let reply = call(&app, &path, Some(&token), Some(&id()), Some(rejected)).await;
        let ended = atlas_server::now();
        assert_eq!(reply.0, StatusCode::BAD_GATEWAY, "{name}: {:?}", reply.1);
        assert_eq!(reply.1["code"], "fetch_failed", "{name}");
        assert_settled(&store, &source, "fetch_failed", started, ended).await?;
        assert_eq!(
            events_before,
            events(&app, &token).await,
            "{name}: a rejected import must not disturb prior events"
        );
        // An immediate corrected import is not blocked behind a stale lease and restores health.
        let retry = call(
            &app,
            &path,
            Some(&token),
            Some(&id()),
            Some(corrected.clone()),
        )
        .await;
        assert_eq!(retry.0, StatusCode::OK, "{name}: {:?}", retry.1);
        assert_eq!(source_state(&store, &source).await?.0, "healthy", "{name}");
        assert_eq!(events_before, events(&app, &token).await, "{name}");
    }

    // Linked refresh: the same parser trigger reached through `refresh_link` instead of
    // `import_text`, again from healthy data. The provider first serves valid content (a
    // successful refresh publishes an event), then oversized content, then corrected content
    // that changes the event so the retry's publication is observable. The event date is
    // relative to today because a linked refresh only covers a window around it.
    let date = (chrono::Utc::now().date_naive() + chrono::Duration::days(7))
        .format("%Y%m%d")
        .to_string();
    let linked_ics = |summary: &str| {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:linked\r\nDTSTART:{date}T090000Z\r\nSUMMARY:{summary}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    };
    let (origin, feed_body, feed_server) = switchable_feed(linked_ics("Original trip")).await?;
    let key = "0e".repeat(32);
    let linked_app = app_allowing(&store, &key, &origin).await?;
    let linked_source = create_linked_source(&linked_app, &token, &origin).await?;
    let linked_path = format!("calendar-sources/{linked_source}/refresh");
    let healthy = call(
        &linked_app,
        &linked_path,
        Some(&token),
        Some(&id()),
        Some(json!({})),
    )
    .await;
    assert_eq!(healthy.0, StatusCode::OK, "{:?}", healthy.1);
    assert_eq!(source_state(&store, &linked_source).await?.0, "healthy");
    let linked_before = events(&linked_app, &token).await;
    let listed = linked_before.to_string();
    assert!(listed.contains("Original trip"), "{listed}");
    assert_eq!(linked_before["items"].as_array().unwrap().len(), 2);

    *feed_body.lock().unwrap() = linked_ics(&"x".repeat(301));
    reset_schedule(linked_source.clone()).await?;
    let started = atlas_server::now();
    let reply = call(
        &linked_app,
        &linked_path,
        Some(&token),
        Some(&id()),
        Some(json!({})),
    )
    .await;
    let ended = atlas_server::now();
    assert_eq!(reply.0, StatusCode::BAD_GATEWAY, "{:?}", reply.1);
    assert_eq!(reply.1["code"], "fetch_failed");
    assert_settled(&store, &linked_source, "fetch_failed", started, ended).await?;
    assert_eq!(
        linked_before,
        events(&linked_app, &token).await,
        "a rejected linked refresh must not disturb prior events"
    );

    *feed_body.lock().unwrap() = linked_ics("Corrected trip");
    let retry = call(
        &linked_app,
        &linked_path,
        Some(&token),
        Some(&id()),
        Some(json!({})),
    )
    .await;
    assert_eq!(retry.0, StatusCode::OK, "{:?}", retry.1);
    assert_eq!(source_state(&store, &linked_source).await?.0, "healthy");
    let listed = events(&linked_app, &token).await.to_string();
    assert!(listed.contains("Corrected trip"), "{listed}");
    assert!(!listed.contains("Original trip"), "{listed}");
    feed_server.abort();

    Ok(())
}
/// BE-B8, cause 3: a connection sealed while the server had a key, refreshed after the server's
/// encryption key becomes unset/misconfigured — two `App` instances sharing one store, matching
/// how a real server restart with a changed `ATLAS_SECRET_KEY` would look, not a fabricated
/// state. `calendar_error()` stops discarding `IntegrationUnconfigured`, so the wire code and
/// health both surface it instead of the previous generic `fetch_failed`.
#[tokio::test]
async fn http_calendar_refresh_integration_unconfigured_records_health() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = id();
    store
        .add_account(
            &actor,
            "calendar-unconfigured-user",
            &hash_password("calendar-unconfigured-password-123".into()).await?,
        )
        .await?;
    let origin = "http://127.0.0.1:59981".to_string();
    let key = "07".repeat(32);
    let keyed_app = App::new(store.clone())
        .await?
        .integration_config(IntegrationConfig::new(
            Some(&key),
            vec![origin.clone()],
            None,
            None,
        )?)
        .router();
    let token = login(
        &keyed_app,
        "calendar-unconfigured-user",
        "calendar-unconfigured-password-123",
    )
    .await?;
    let source = id();
    let create = json!({"command":{"kind":"create_source","id":source,"label":"Calendar","timezone":"UTC",
        "connection":{"url":format!("{origin}/feed"),"bearer":"probe-secret"}}});
    let created = call(
        &keyed_app,
        "calendar-commands",
        Some(&token),
        Some(&id()),
        Some(create),
    )
    .await;
    assert_eq!(created.0, StatusCode::OK, "{:?}", created.1);

    let unkeyed_app = App::new(store.clone())
        .await?
        .integration_config(IntegrationConfig::new(None, vec![origin], None, None)?)
        .router();
    let reply = call(
        &unkeyed_app,
        &format!("calendar-sources/{source}/refresh"),
        Some(&token),
        Some(&id()),
        Some(json!({})),
    )
    .await;
    assert_eq!(reply.0, StatusCode::SERVICE_UNAVAILABLE, "{:?}", reply.1);
    assert_eq!(reply.1["code"], "integration_unconfigured");
    let health: String = sqlx::query_scalar("SELECT health FROM calendar_sources WHERE id=$1")
        .bind(&source)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(health, "integration_unconfigured");
    Ok(())
}
/// BE-B8, causes 4 and 5: both raise the identical `ErrorCode::StaleRefresh` (`409
/// stale_refresh`) but must carry a distinguishing `reason`. Each sub-case uses `delayed_feed`
/// to hold a real fetch in flight, makes the mid-flight change directly against the state
/// `finish_calendar_refresh_receipt` will check, then releases the response — a real refresh
/// attempt genuinely loses the race, not a fabricated error.
#[tokio::test]
async fn http_calendar_refresh_stale_refresh_reason_distinguishes_generation_and_lease()
-> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = id();
    store
        .add_account(
            &actor,
            "calendar-stale-user",
            &hash_password("calendar-stale-password-123".into()).await?,
        )
        .await?;

    // Cause 4: a concurrent settings edit bumps `generation` while the fetch is still in
    // flight — proof of an external event, not a newer refresh attempt in this case.
    {
        let (origin, mut arrived_rx, release_tx, feed_server) = delayed_feed().await?;
        let key = "09".repeat(32);
        let app = App::new(store.clone())
            .await?
            .integration_config(IntegrationConfig::new(
                Some(&key),
                vec![origin.clone()],
                None,
                None,
            )?)
            .router();
        let token = login(&app, "calendar-stale-user", "calendar-stale-password-123").await?;
        let source = id();
        let create = json!({"command":{"kind":"create_source","id":source,"label":"Calendar","timezone":"UTC",
            "connection":{"url":format!("{origin}/feed"),"bearer":"probe-secret"}}});
        let created = call(
            &app,
            "calendar-commands",
            Some(&token),
            Some(&id()),
            Some(create),
        )
        .await;
        assert_eq!(created.0, StatusCode::OK, "{:?}", created.1);

        let refresh_task = {
            let app = app.clone();
            let token = token.clone();
            let source = source.clone();
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
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), arrived_rx.recv())
            .await?
            .expect("feed connection");
        let version: i64 = sqlx::query_scalar("SELECT version FROM resources WHERE id=$1")
            .bind(&source)
            .fetch_one(&store.pool)
            .await?;
        let edit = json!({"command":{"kind":"configure_source","id":source,"expected_version":version,
            "timezone":"Europe/London","enabled":true}});
        let edited = call(
            &app,
            "calendar-commands",
            Some(&token),
            Some(&id()),
            Some(edit),
        )
        .await;
        assert_eq!(edited.0, StatusCode::OK, "{:?}", edited.1);
        release_tx.send(()).ok();
        let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(5), refresh_task)
            .await?
            .expect("refresh task panicked");
        assert_eq!(status, StatusCode::CONFLICT, "{:?}", body);
        assert_eq!(body["code"], "stale_refresh");
        assert_eq!(body["reason"], "generation_changed");
        let health: String = sqlx::query_scalar("SELECT health FROM calendar_sources WHERE id=$1")
            .bind(&source)
            .fetch_one(&store.pool)
            .await?;
        assert_eq!(
            health, "pending",
            "a rejected stale attempt must not change recorded health"
        );
        feed_server.abort();
    }

    // Cause 5: only the lease has expired (forced short via direct SQL, for test speed — a
    // real 60-second wait would prove nothing a controlled clock does not), with no generation
    // change and no settings edit.
    {
        let (origin, mut arrived_rx, release_tx, feed_server) = delayed_feed().await?;
        let key = "0a".repeat(32);
        let app = App::new(store.clone())
            .await?
            .integration_config(IntegrationConfig::new(
                Some(&key),
                vec![origin.clone()],
                None,
                None,
            )?)
            .router();
        let token = login(&app, "calendar-stale-user", "calendar-stale-password-123").await?;
        let source = id();
        let create = json!({"command":{"kind":"create_source","id":source,"label":"Calendar","timezone":"UTC",
            "connection":{"url":format!("{origin}/feed"),"bearer":"probe-secret"}}});
        let created = call(
            &app,
            "calendar-commands",
            Some(&token),
            Some(&id()),
            Some(create),
        )
        .await;
        assert_eq!(created.0, StatusCode::OK, "{:?}", created.1);

        let refresh_task = {
            let app = app.clone();
            let token = token.clone();
            let source = source.clone();
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
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), arrived_rx.recv())
            .await?
            .expect("feed connection");
        sqlx::query("UPDATE calendar_sources SET lease_until=1 WHERE id=$1")
            .bind(&source)
            .execute(&store.pool)
            .await?;
        release_tx.send(()).ok();
        let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(5), refresh_task)
            .await?
            .expect("refresh task panicked");
        assert_eq!(status, StatusCode::CONFLICT, "{:?}", body);
        assert_eq!(body["code"], "stale_refresh");
        assert_eq!(body["reason"], "lease_expired");
        let health: String = sqlx::query_scalar("SELECT health FROM calendar_sources WHERE id=$1")
            .bind(&source)
            .fetch_one(&store.pool)
            .await?;
        assert_eq!(
            health, "pending",
            "a rejected stale attempt must not change recorded health"
        );
        feed_server.abort();
    }

    Ok(())
}
/// BE-B8, cause 6a: the source is archived (the user-facing "enabled" toggle) while a fetch is
/// in flight — still visible/editable, so this is `invalid_value`, distinct from 6b's lost
/// access. (Cause 6b's own `NotFound`/`Forbidden` pass-through through the real HTTP call path
/// and `calendar_error()` wrapper is exercised next, by
/// `http_calendar_refresh_access_lost_mid_flight_is_fault_injected` — a genuine collaborator
/// refresh attempt cannot reach it naturally, since `begin_calendar_refresh` only returns a
/// usable connection to the owner, so a collaborator fails earlier, at cause 1. The `Store`-level
/// coverage in `crates/core/tests/cases/calendars/refresh_errors.rs` remains as well.)
#[tokio::test]
async fn http_calendar_refresh_archived_mid_flight_is_invalid_value() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = id();
    store
        .add_account(
            &actor,
            "calendar-archived-user",
            &hash_password("calendar-archived-password-123".into()).await?,
        )
        .await?;
    let (origin, mut arrived_rx, release_tx, feed_server) = delayed_feed().await?;
    let key = "0b".repeat(32);
    let app = App::new(store.clone())
        .await?
        .integration_config(IntegrationConfig::new(
            Some(&key),
            vec![origin.clone()],
            None,
            None,
        )?)
        .router();
    let token = login(
        &app,
        "calendar-archived-user",
        "calendar-archived-password-123",
    )
    .await?;
    let source = id();
    let create = json!({"command":{"kind":"create_source","id":source,"label":"Calendar","timezone":"UTC",
        "connection":{"url":format!("{origin}/feed"),"bearer":"probe-secret"}}});
    let created = call(
        &app,
        "calendar-commands",
        Some(&token),
        Some(&id()),
        Some(create),
    )
    .await;
    assert_eq!(created.0, StatusCode::OK, "{:?}", created.1);

    let refresh_task = {
        let app = app.clone();
        let token = token.clone();
        let source = source.clone();
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
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), arrived_rx.recv())
        .await?
        .expect("feed connection");
    let version: i64 = sqlx::query_scalar("SELECT version FROM resources WHERE id=$1")
        .bind(&source)
        .fetch_one(&store.pool)
        .await?;
    let archive = json!({"command":{"kind":"configure_source","id":source,"expected_version":version,
        "timezone":"UTC","enabled":false}});
    let archived = call(
        &app,
        "calendar-commands",
        Some(&token),
        Some(&id()),
        Some(archive),
    )
    .await;
    assert_eq!(archived.0, StatusCode::OK, "{:?}", archived.1);
    release_tx.send(()).ok();
    let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(5), refresh_task)
        .await?
        .expect("refresh task panicked");
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{:?}", body);
    assert_eq!(body["code"], "invalid_value");
    let health: String = sqlx::query_scalar("SELECT health FROM calendar_sources WHERE id=$1")
        .bind(&source)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(
        health, "pending",
        "an archived source's rejected attempt must not change recorded health"
    );
    feed_server.abort();
    Ok(())
}
/// BE-B8 revision, R3: cause 6b (access lost or the source deleted, mid-refresh) exercised
/// through the real HTTP `refresh_link` call path and the `calendar_error()` wrapper, not only
/// at the `Store` level. Neither half of this cause is reachable through a supported product
/// journey: a collaborator's own refresh attempt never receives a usable `connection` from
/// `begin_calendar_refresh` (so it fails immediately at cause 1, before ever reaching the
/// finish-stage access check), and the owner's own access to a source they own cannot be
/// revoked through any command in this codebase — `frozen_owner_visibility`, the one
/// visibility-freezing mechanism that exists, is written only by `people/merging.rs` (person
/// merges) and is never referenced anywhere in the calendar code. This test therefore uses a
/// directly-labelled fault injection — reassigning the resource's `owner_id` via raw SQL
/// mid-flight, a state no product command produces — solely to exercise the defensive
/// `NotFound`/`Forbidden` propagation through the real wrapper and wire layer. It does not
/// claim this is a reachable product journey; that limitation is unchanged and remains recorded
/// in `.agtx/execute.md`.
#[tokio::test]
async fn http_calendar_refresh_access_lost_mid_flight_is_fault_injected() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = id();
    store
        .add_account(
            &actor,
            "calendar-access-lost-user",
            &hash_password("calendar-access-lost-password-123".into()).await?,
        )
        .await?;
    let other = id();
    store
        .add_account(
            &other,
            "calendar-access-lost-other",
            &hash_password("calendar-access-lost-other-123".into()).await?,
        )
        .await?;

    // `NotFound` half: ownership reassigned away with no grant left behind for the actor.
    {
        let (origin, mut arrived_rx, release_tx, feed_server) = delayed_feed().await?;
        let key = "11".repeat(32);
        let app = App::new(store.clone())
            .await?
            .integration_config(IntegrationConfig::new(
                Some(&key),
                vec![origin.clone()],
                None,
                None,
            )?)
            .router();
        let token = login(
            &app,
            "calendar-access-lost-user",
            "calendar-access-lost-password-123",
        )
        .await?;
        let source = id();
        let create = json!({"command":{"kind":"create_source","id":source,"label":"Calendar","timezone":"UTC",
            "connection":{"url":format!("{origin}/feed"),"bearer":"probe-secret"}}});
        let created = call(
            &app,
            "calendar-commands",
            Some(&token),
            Some(&id()),
            Some(create),
        )
        .await;
        assert_eq!(created.0, StatusCode::OK, "{:?}", created.1);

        let refresh_task = {
            let app = app.clone();
            let token = token.clone();
            let source = source.clone();
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
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), arrived_rx.recv())
            .await?
            .expect("feed connection");
        // Fault injection: no product command reassigns a resource's owner. Direct SQL only.
        sqlx::query("UPDATE resources SET owner_id=$1 WHERE id=$2")
            .bind(&other)
            .bind(&source)
            .execute(&store.pool)
            .await?;
        release_tx.send(()).ok();
        let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(5), refresh_task)
            .await?
            .expect("refresh task panicked");
        assert_eq!(status, StatusCode::NOT_FOUND, "{:?}", body);
        assert_eq!(body["code"], "not_found");
        let health: String = sqlx::query_scalar("SELECT health FROM calendar_sources WHERE id=$1")
            .bind(&source)
            .fetch_one(&store.pool)
            .await?;
        assert_eq!(
            health, "pending",
            "an access-lost rejected attempt must not change recorded health"
        );
        feed_server.abort();
    }

    // `Forbidden` half: ownership reassigned away, but a view-only grant is left for the
    // original actor — still visible, but no longer editable.
    {
        let (origin, mut arrived_rx, release_tx, feed_server) = delayed_feed().await?;
        let key = "12".repeat(32);
        let app = App::new(store.clone())
            .await?
            .integration_config(IntegrationConfig::new(
                Some(&key),
                vec![origin.clone()],
                None,
                None,
            )?)
            .router();
        let token = login(
            &app,
            "calendar-access-lost-user",
            "calendar-access-lost-password-123",
        )
        .await?;
        let source = id();
        let create = json!({"command":{"kind":"create_source","id":source,"label":"Calendar","timezone":"UTC",
            "connection":{"url":format!("{origin}/feed"),"bearer":"probe-secret"}}});
        let created = call(
            &app,
            "calendar-commands",
            Some(&token),
            Some(&id()),
            Some(create),
        )
        .await;
        assert_eq!(created.0, StatusCode::OK, "{:?}", created.1);

        let refresh_task = {
            let app = app.clone();
            let token = token.clone();
            let source = source.clone();
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
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), arrived_rx.recv())
            .await?
            .expect("feed connection");
        // Fault injection: no product command reassigns ownership while leaving a view-only
        // grant behind for the previous owner. Direct SQL only.
        sqlx::query("UPDATE resources SET owner_id=$1 WHERE id=$2")
            .bind(&other)
            .bind(&source)
            .execute(&store.pool)
            .await?;
        sqlx::query(
            "INSERT INTO resource_grants(resource_id,account_id,can_edit) VALUES ($1,$2,0)",
        )
        .bind(&source)
        .bind(&actor)
        .execute(&store.pool)
        .await?;
        release_tx.send(()).ok();
        let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(5), refresh_task)
            .await?
            .expect("refresh task panicked");
        assert_eq!(status, StatusCode::FORBIDDEN, "{:?}", body);
        assert_eq!(body["code"], "forbidden");
        let health: String = sqlx::query_scalar("SELECT health FROM calendar_sources WHERE id=$1")
            .bind(&source)
            .fetch_one(&store.pool)
            .await?;
        assert_eq!(
            health, "pending",
            "an access-lost rejected attempt must not change recorded health"
        );
        feed_server.abort();
    }

    Ok(())
}
/// BE-B8, item 4: `refresh_in_progress` over the real router, in three sub-cases that each hold
/// a genuine provider fetch in flight with `delayed_feed`:
/// 1. false before any refresh, live while the lease is held, and false again once the request
///    completes successfully;
/// 2. a configuration change while the fetch is held clears the flag at once — read before the
///    superseded provider response is released — and the released response is then rejected with
///    `generation_changed`;
/// 3. with only `lease_until` forced into the past (direct SQL, for test speed) the flag is false
///    while the response is still held — again read before release, so it is the read that
///    observes the expiry, not the later `lease_expired` rejection. This exercises the handler's
///    own clock argument over the real router; the pure `Store`-level comparison (advancing an
///    explicit `now` with no completing write at all) is covered in
///    `crates/core/tests/cases/calendars/refresh_errors.rs`.
#[tokio::test]
async fn http_calendar_sources_refresh_in_progress_reflects_live_lease() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = id();
    store
        .add_account(
            &actor,
            "calendar-progress-user",
            &hash_password("calendar-progress-password-123".into()).await?,
        )
        .await?;
    let key = "0c".repeat(32);
    let (origin, mut arrived_rx, release_tx, feed_server) = delayed_feed().await?;
    let app = app_allowing(&store, &key, &origin).await?;
    let token = login(
        &app,
        "calendar-progress-user",
        "calendar-progress-password-123",
    )
    .await?;
    let source = create_linked_source(&app, &token, &origin).await?;

    // 1. Before any refresh.
    assert_eq!(
        refresh_in_progress(&app, &token, &source).await,
        Some(false)
    );

    // A real refresh begins and holds its lease while the loopback feed has received the
    // request but not yet answered — live during the lease, over the real router.
    let refresh_task = spawn_refresh(&app, &token, &source);
    tokio::time::timeout(std::time::Duration::from_secs(5), arrived_rx.recv())
        .await?
        .expect("feed connection");
    assert_eq!(refresh_in_progress(&app, &token, &source).await, Some(true));

    // Once the fetch completes and the refresh commits, the lease clears and a fresh list
    // is false again — the property `refresh_in_progress` exists to obtain, per cause 4/5's
    // client recovery in the approved brief.
    release_tx.send(()).ok();
    let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(5), refresh_task)
        .await?
        .expect("refresh task panicked");
    assert_eq!(status, StatusCode::OK, "{:?}", body);
    assert_eq!(
        refresh_in_progress(&app, &token, &source).await,
        Some(false)
    );
    feed_server.abort();

    // 2. A configuration change while the fetch is held.
    let (origin, mut arrived_rx, release_tx, feed_server) = delayed_feed().await?;
    let app = app_allowing(&store, &key, &origin).await?;
    let source = create_linked_source(&app, &token, &origin).await?;
    let refresh_task = spawn_refresh(&app, &token, &source);
    tokio::time::timeout(std::time::Duration::from_secs(5), arrived_rx.recv())
        .await?
        .expect("feed connection");
    assert_eq!(refresh_in_progress(&app, &token, &source).await, Some(true));
    let version: i64 = sqlx::query_scalar("SELECT version FROM resources WHERE id=$1")
        .bind(&source)
        .fetch_one(&store.pool)
        .await?;
    let edit = json!({"command":{"kind":"configure_source","id":source,"expected_version":version,
        "timezone":"Europe/London","enabled":true}});
    let edited = call(
        &app,
        "calendar-commands",
        Some(&token),
        Some(&id()),
        Some(edit),
    )
    .await;
    assert_eq!(edited.0, StatusCode::OK, "{:?}", edited.1);
    assert_eq!(
        refresh_in_progress(&app, &token, &source).await,
        Some(false),
        "the edit cleared the lease, so the flag is false while the superseded fetch is still held"
    );
    release_tx.send(()).ok();
    let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(5), refresh_task)
        .await?
        .expect("refresh task panicked");
    assert_eq!(status, StatusCode::CONFLICT, "{:?}", body);
    assert_eq!(body["reason"], "generation_changed");
    assert_eq!(
        refresh_in_progress(&app, &token, &source).await,
        Some(false)
    );
    feed_server.abort();

    // 3. Only the lease expires while the fetch is held.
    let (origin, mut arrived_rx, release_tx, feed_server) = delayed_feed().await?;
    let app = app_allowing(&store, &key, &origin).await?;
    let source = create_linked_source(&app, &token, &origin).await?;
    let refresh_task = spawn_refresh(&app, &token, &source);
    tokio::time::timeout(std::time::Duration::from_secs(5), arrived_rx.recv())
        .await?
        .expect("feed connection");
    assert_eq!(
        refresh_in_progress(&app, &token, &source).await,
        Some(true),
        "the same held fetch is live until its lease is forced into the past"
    );
    sqlx::query("UPDATE calendar_sources SET lease_until=1 WHERE id=$1")
        .bind(&source)
        .execute(&store.pool)
        .await?;
    assert_eq!(
        refresh_in_progress(&app, &token, &source).await,
        Some(false),
        "an expired lease must read false while its provider response is still held"
    );
    release_tx.send(()).ok();
    let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(5), refresh_task)
        .await?
        .expect("refresh task panicked");
    assert_eq!(status, StatusCode::CONFLICT, "{:?}", body);
    assert_eq!(body["reason"], "lease_expired");
    feed_server.abort();

    Ok(())
}
