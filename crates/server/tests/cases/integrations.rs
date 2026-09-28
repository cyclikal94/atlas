use anyhow::Result;
use atlas_core::calendars::Notification;
use atlas_server::integrations::{
    IntegrationConfig, Link, Subscription, public_ip, web_push_payload,
};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;
use url::Url;
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
        config.deliver(&ntfy, &notification, i64::MAX, None).await?,
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
        config.deliver(&web, &notification, i64::MAX, None).await?,
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
                    i64::MAX,
                    None
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
        .calendar_resources(
            &actor,
            "calendar_source",
            None,
            None,
            200,
            atlas_server::now(),
        )
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

const SCOPE: &str = "https://atlas.example/";

fn notification() -> Notification {
    Notification {
        id: Uuid::new_v4().to_string(),
        reminder_id: Uuid::new_v4().to_string(),
        occurrence_id: Uuid::new_v4().to_string(),
    }
}

/// What a user agent would display for a parsed declarative push message.
struct Declarative {
    title: String,
    body: Option<String>,
    lang: Option<String>,
    dir: Option<String>,
    tag: Option<String>,
    navigate: Url,
    mutable: bool,
}

/// Test-only restatement of the W3C Push API "declarative push message parser" (Editor's Draft,
/// read from its source on 28 September 2026) plus the two `create a notification` TypeError
/// rules of the WHATWG Notifications Standard. It shows that a payload satisfies the published
/// algorithm. It is not a browser and says nothing about display or clicks.
fn parse_declarative(bytes: &[u8], scope: &str) -> Option<Declarative> {
    let message: Value = serde_json::from_slice(bytes).ok()?;
    let message = message.as_object()?;
    if message.get("web_push")?.as_f64()? != 8030.0 {
        return None;
    }
    let input = message.get("notification")?.as_object()?;
    let title = input.get("title")?.as_str()?;
    let navigate = input.get("navigate")?.as_str()?;
    let text = |name: &str| input.get(name).and_then(Value::as_str).map(str::to_owned);
    let flag = |name: &str| input.get(name).and_then(Value::as_bool);
    let vibrates = input
        .get("vibrate")
        .and_then(Value::as_array)
        .is_some_and(|list| {
            list.iter()
                .all(|v| v.as_u64().is_some_and(|n| u32::try_from(n).is_ok()))
        });
    if (flag("silent") == Some(true) && vibrates)
        || (flag("renotify") == Some(true) && text("tag").unwrap_or_default().is_empty())
    {
        return None;
    }
    let navigate = Url::parse(scope).ok()?.join(navigate).ok()?;
    Some(Declarative {
        title: title.to_owned(),
        body: text("body"),
        lang: text("lang"),
        dir: text("dir").filter(|dir| matches!(dir.as_str(), "auto" | "ltr" | "rtl")),
        tag: text("tag"),
        navigate,
        mutable: message
            .get("mutable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn query(url: &Url) -> Vec<(String, String)> {
    url.query_pairs()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

#[test]
fn declarative_payload_matches_the_contract() -> Result<()> {
    let n = notification();
    let bytes = web_push_payload(&n, Some("https://atlas.example"))?;
    let navigate = format!(
        "https://atlas.example/?delivery_id={}&reminder_id={}&occurrence_id={}",
        n.id, n.reminder_id, n.occurrence_id
    );
    // The exact serialised form, including member order.
    assert_eq!(
        String::from_utf8(bytes.clone())?,
        format!(
            r#"{{"web_push":8030,"notification":{{"title":"Atlas reminder","body":"Open Atlas to view your reminder.","lang":"en-GB","dir":"ltr","tag":"{id}","navigate":"{navigate}"}},"id":"{id}","reminder_id":"{reminder}","occurrence_id":"{occurrence}"}}"#,
            id = n.id,
            reminder = n.reminder_id,
            occurrence = n.occurrence_id,
        )
    );
    let value: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(value["web_push"].as_u64(), Some(8030));
    let parsed = parse_declarative(&bytes, SCOPE).expect("the payload must parse as declarative");
    assert_eq!(parsed.title, "Atlas reminder");
    assert_eq!(
        parsed.body.as_deref(),
        Some("Open Atlas to view your reminder.")
    );
    assert_eq!(parsed.lang.as_deref(), Some("en-GB"));
    assert_eq!(parsed.dir.as_deref(), Some("ltr"));
    assert_eq!(parsed.tag.as_deref(), Some(n.id.as_str()));
    assert_eq!(parsed.navigate.as_str(), navigate);
    assert!(!parsed.mutable, "no service worker code may be required");

    // The mirror has teeth: each of these must fail declarative parsing.
    let edits: [fn(&mut Value); 9] = [
        |v| {
            v["notification"].as_object_mut().unwrap().remove("title");
        },
        |v| v["notification"]["title"] = json!(7),
        |v| v["notification"]["navigate"] = json!(5),
        |v| v["notification"]["navigate"] = json!("https://[bad"),
        |v| v["web_push"] = json!("8030"),
        |v| v["web_push"] = json!(8031),
        |v| {
            v.as_object_mut().unwrap().remove("web_push");
        },
        |v| {
            v.as_object_mut().unwrap().remove("notification");
        },
        |v| v["notification"] = json!("text"),
    ];
    for edit in edits {
        let mut broken = value.clone();
        edit(&mut broken);
        assert!(
            parse_declarative(&serde_json::to_vec(&broken)?, SCOPE).is_none(),
            "{broken}"
        );
    }
    for raw in [&b"not json"[..], b"[]", b"8030", b""] {
        assert!(parse_declarative(raw, SCOPE).is_none());
    }
    // ...and it accepts the message published in the specification, with its unknown members ignored.
    let published = br#"{"web_push":8030,"notification":{"title":"Ada emailed","lang":"en-US","dir":"ltr","body":"Did you hear?","navigate":"https://email.example/message/12"}}"#;
    assert!(parse_declarative(published, "https://email.example/").is_some());
    let webkit = br#"{"web_push":8030,"notification":{"title":"T","navigate":"https://webkit.org/","silent":false,"app_badge":"1"}}"#;
    assert!(parse_declarative(webkit, "https://webkit.org/").is_some());

    // Only the members every current source agrees on are emitted.
    let mut members: Vec<_> = value["notification"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    members.sort();
    assert_eq!(members, ["body", "dir", "lang", "navigate", "tag", "title"]);
    let mut top: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
    top.sort();
    assert_eq!(
        top,
        [
            "id",
            "notification",
            "occurrence_id",
            "reminder_id",
            "web_push"
        ]
    );
    Ok(())
}

#[test]
fn declarative_navigation_and_legacy_readers() -> Result<()> {
    let n = notification();
    let bytes = web_push_payload(&n, Some("https://atlas.example"))?;
    let value: Value = serde_json::from_slice(&bytes)?;
    let parsed = parse_declarative(&bytes, SCOPE).unwrap();
    // The deep link is absolute, on the configured origin, and names the three identifiers.
    assert_eq!(parsed.navigate.origin(), Url::parse(SCOPE)?.origin());
    assert_eq!(parsed.navigate.path(), "/");
    assert_eq!(
        query(&parsed.navigate),
        [
            ("delivery_id".into(), n.id.clone()),
            ("reminder_id".into(), n.reminder_id.clone()),
            ("occurrence_id".into(), n.occurrence_id.clone()),
        ]
    );

    // Hostile identifier text cannot add parameters, a fragment or a path.
    let hostile = Notification {
        id: "a&b=c d".into(),
        reminder_id: "#frag?x=1".into(),
        occurrence_id: "%41é/../".into(),
    };
    let bytes = web_push_payload(&hostile, Some("https://atlas.example"))?;
    let parsed = parse_declarative(&bytes, SCOPE).unwrap();
    assert_eq!(parsed.navigate.origin(), Url::parse(SCOPE)?.origin());
    assert_eq!(parsed.navigate.path(), "/");
    assert_eq!(parsed.navigate.fragment(), None);
    assert_eq!(
        query(&parsed.navigate),
        [
            ("delivery_id".into(), hostile.id.clone()),
            ("reminder_id".into(), hostile.reminder_id.clone()),
            ("occurrence_id".into(), hostile.occurrence_id.clone()),
        ]
    );
    assert_eq!(parsed.tag.as_deref(), Some("a&b=c d"));

    // Legacy: without an origin the bytes are exactly today's payload.
    assert_eq!(web_push_payload(&n, None)?, serde_json::to_vec(&n)?);
    assert!(parse_declarative(&web_push_payload(&n, None)?, SCOPE).is_none());
    // The top-level identifiers of the envelope are the legacy payload, so a tolerant reader of
    // the imperative `push` handler is unaffected...
    let mut top = value.as_object().unwrap().clone();
    top.remove("web_push");
    top.remove("notification");
    assert_eq!(Value::Object(top), serde_json::to_value(&n)?);
    #[derive(serde::Deserialize)]
    struct Tolerant {
        id: String,
        reminder_id: String,
        occurrence_id: String,
    }
    let tolerant: Tolerant = serde_json::from_slice(&web_push_payload(&n, Some(SCOPE))?)?;
    assert_eq!(
        (tolerant.id, tolerant.reminder_id, tolerant.occurrence_id),
        (n.id.clone(), n.reminder_id.clone(), n.occurrence_id.clone())
    );
    // ...while a validator that forbids unknown members rejects it. That is why the contract
    // advances by a MINOR version rather than a patch.
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Strict {
        id: String,
        reminder_id: String,
        occurrence_id: String,
    }
    assert!(serde_json::from_slice::<Strict>(&web_push_payload(&n, Some(SCOPE))?).is_err());
    assert!(serde_json::from_slice::<Strict>(&web_push_payload(&n, None)?).is_ok());

    // Deterministic per delivery, distinct between deliveries.
    assert_eq!(
        web_push_payload(&n, Some(SCOPE))?,
        web_push_payload(&n, Some(SCOPE))?
    );
    let other = notification();
    let first = parse_declarative(&web_push_payload(&n, Some(SCOPE))?, SCOPE).unwrap();
    let second = parse_declarative(&web_push_payload(&other, Some(SCOPE))?, SCOPE).unwrap();
    assert_ne!(first.tag, second.tag);
    assert_ne!(first.navigate, second.navigate);

    // Nothing that could alter display policy, hold data or clear anything is emitted.
    let text = String::from_utf8(web_push_payload(&n, Some(SCOPE))?)?;
    for absent in [
        "mutable",
        "silent",
        "renotify",
        "requireInteraction",
        "app_badge",
        "actions",
        "\"data\"",
        "icon",
        "badge",
        "image",
        "vibrate",
        "timestamp",
    ] {
        assert!(!text.contains(absent), "{absent}");
    }
    Ok(())
}

fn host(length: usize) -> String {
    (0..length)
        .map(|i| {
            if i % 61 == 60 && i + 1 < length {
                '.'
            } else {
                'a'
            }
        })
        .collect()
}

#[test]
fn origin_variants_and_size_fallback() -> Result<()> {
    let n = notification();
    let legacy = serde_json::to_vec(&n)?;
    for (origin, expected) in [
        ("http://localhost:3000", "http://localhost:3000/?"),
        ("http://[::1]:3000", "http://[::1]:3000/?"),
        ("https://atlas.example:443", "https://atlas.example/?"),
        ("https://atlas.example/", "https://atlas.example/?"),
        (
            "https://atlas.example/app/x?y=1#z",
            "https://atlas.example/?",
        ),
        ("HTTPS://ATLAS.EXAMPLE", "https://atlas.example/?"),
    ] {
        let bytes = web_push_payload(&n, Some(origin))?;
        let parsed = parse_declarative(&bytes, expected)
            .unwrap_or_else(|| panic!("{origin} must give a declarative payload"));
        assert!(parsed.navigate.as_str().starts_with(expected), "{origin}");
        assert_eq!(parsed.navigate.origin(), Url::parse(origin)?.origin());
        assert_eq!(parsed.navigate.fragment(), None);
    }
    // An unusable origin is never an error: the legacy payload is sent instead.
    for origin in [
        "",
        "not a url",
        "atlas.example",
        "atlas.example:3000",
        "ftp://atlas.example",
        "file:///tmp/x",
        "mailto:someone@atlas.example",
        "javascript:alert(1)",
        "https://user@atlas.example",
        "https://user:secret@atlas.example",
    ] {
        assert_eq!(web_push_payload(&n, Some(origin))?, legacy, "{origin:?}");
    }

    // A maximum-length (253-octet) host still fits comfortably.
    let longest = web_push_payload(&n, Some(&format!("https://{}", host(253))))?;
    assert!(parse_declarative(&longest, "https://example.invalid/").is_some());
    assert!(longest.len() <= 3052);
    // The fallback switches exactly at the 3052-byte plaintext limit `web-push` enforces. The host
    // is written once, one byte per character, so the expected envelope size is plain arithmetic.
    let base = web_push_payload(&n, Some(&format!("https://{}", host(10))))?.len();
    let largest_fitting_host = 3052 - (base - 10);
    for length in largest_fitting_host - 2..=largest_fitting_host + 2 {
        let origin = format!("https://{}", host(length));
        assert!(Url::parse(&origin).is_ok(), "the host must be parseable");
        let bytes = web_push_payload(&n, Some(&origin))?;
        if length <= largest_fitting_host {
            assert!(parse_declarative(&bytes, "https://example.invalid/").is_some());
            assert!(bytes.len() <= 3052, "{length}");
        } else {
            assert_eq!(bytes, legacy, "{length}");
        }
    }
    Ok(())
}

async fn serve(router: Router) -> Result<(String, tokio::task::JoinHandle<std::io::Result<()>>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    Ok((
        origin,
        tokio::spawn(async move { axum::serve(listener, router).await }),
    ))
}

fn push_config(origin: &str) -> Result<IntegrationConfig> {
    IntegrationConfig::new(
        Some(&"01".repeat(32)),
        vec![origin.into()],
        Some(URL_SAFE_NO_PAD.encode([1_u8; 32])),
        Some("mailto:test@example.invalid".into()),
    )
}

/// The browser's side of a subscription: decrypts what the push service received.
type Decrypt = Box<dyn Fn(&[u8]) -> Result<Vec<u8>>>;

/// A real subscription key pair for `endpoint`, and its decryption.
fn web_push_device(endpoint: String) -> Result<(Subscription, Decrypt)> {
    let (key, auth) = ece::generate_keypair_and_auth_secret()?;
    let subscription = Subscription::WebPush {
        endpoint,
        p256dh: URL_SAFE_NO_PAD.encode(key.pub_as_raw()?),
        auth: URL_SAFE_NO_PAD.encode(auth),
    };
    Ok((
        subscription,
        Box::new(move |body: &[u8]| Ok(ece::decrypt(&key.raw_components()?, &auth, body)?)),
    ))
}

fn secrets(subscription: &Subscription) -> Vec<String> {
    match subscription {
        Subscription::WebPush {
            endpoint,
            p256dh,
            auth,
        } => vec![endpoint.clone(), p256dh.clone(), auth.clone()],
        Subscription::Ntfy { .. } => unreachable!(),
    }
}

#[tokio::test]
async fn web_push_provider_path_carries_the_declarative_envelope() -> Result<()> {
    let received = Received::default();
    let router = Router::new()
        .route("/push", post(capture))
        .route("/gone", post(|| async { StatusCode::GONE }))
        .route("/missing", post(|| async { StatusCode::NOT_FOUND }))
        .route(
            "/too-large",
            post(|| async { StatusCode::PAYLOAD_TOO_LARGE }),
        )
        .route("/retry", post(|| async { StatusCode::SERVICE_UNAVAILABLE }))
        .with_state(received.clone());
    let (origin, server) = serve(router).await?;
    let config = push_config(&origin)?;
    let (device, decrypt) = web_push_device(format!("{origin}/push"))?;
    config.validate_subscription(&device).await?;
    let n = notification();
    let public = "https://atlas.example";

    assert_eq!(
        config.deliver(&device, &n, i64::MAX, Some(public)).await?,
        (true, false)
    );
    // The same delivery again, as a retry would send it.
    assert_eq!(
        config.deliver(&device, &n, i64::MAX, Some(public)).await?,
        (true, false)
    );
    // Without a public origin the message is the legacy payload.
    assert_eq!(
        config.deliver(&device, &n, i64::MAX, None).await?,
        (true, false)
    );
    let messages = received.lock().await;
    assert_eq!(messages.len(), 3);
    for (headers, _) in messages.iter() {
        assert_eq!(headers["content-encoding"], "aes128gcm");
        assert!(headers["authorization"].to_str()?.starts_with("vapid "));
        assert_eq!(headers["topic"], n.id.replace('-', ""));
        assert_eq!(headers["ttl"], "604800");
    }
    let first = decrypt(&messages[0].1)?;
    let second = decrypt(&messages[1].1)?;
    assert_ne!(
        messages[0].1, messages[1].1,
        "each message is freshly encrypted"
    );
    assert_eq!(first, second, "a retry carries identical plaintext");
    assert_eq!(first, web_push_payload(&n, Some(public))?);
    let parsed = parse_declarative(&first, SCOPE).expect("the delivered bytes must parse");
    assert_eq!(parsed.tag.as_deref(), Some(n.id.as_str()));
    assert_eq!(decrypt(&messages[2].1)?, serde_json::to_vec(&n)?);
    // No subscription material or credential reaches the push service, or the user's screen.
    let text = String::from_utf8(first)?;
    for private in
        secrets(&device)
            .into_iter()
            .chain(["vapid".into(), "mailto:".into(), origin.clone()])
    {
        assert!(!text.contains(&private), "{private}");
    }
    drop(messages);

    for (path, expected) in [
        ("gone", (false, true)),
        ("missing", (false, true)),
        ("too-large", (false, true)),
        ("retry", (false, false)),
    ] {
        let (device, _) = web_push_device(format!("{origin}/{path}"))?;
        assert_eq!(
            config.deliver(&device, &n, i64::MAX, Some(public)).await?,
            expected,
            "{path}"
        );
    }
    server.abort();
    Ok(())
}

/// A due reminder with one sealed subscription, ready for `integration_tick`.
struct Due {
    _dir: tempfile::TempDir,
    store: atlas_core::Store,
    actor: String,
    subscription: String,
}

async fn due_reminder(config: &IntegrationConfig, subscription: &Subscription) -> Result<Due> {
    use atlas_core::{Store, calendars::*, tasks::*};
    let (dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let id = || Uuid::new_v4().to_string();
    let actor = id();
    store
        .add_account(&actor, "declarative-owner", "unused")
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
    let secret = config.seal(
        &format!("subscription:{sub}"),
        &serde_json::to_string(subscription)?,
    )?;
    store
        .reminder_command(
            &actor,
            &id(),
            &ReminderCommand::SetSubscription {
                id: sub.clone(),
                expected_version: 0,
                device_id: "phone-declarative".into(),
                transport: "web_push".into(),
                secret,
                enabled: true,
            },
        )
        .await?;
    Ok(Due {
        _dir: dir,
        store,
        actor,
        subscription: sub,
    })
}

/// Answers the first request with a temporary failure and every later one with success.
async fn flaky(State(received): State<Received>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let mut all = received.lock().await;
    all.push((headers, body));
    if all.len() == 1 {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::CREATED
    }
}

#[tokio::test]
async fn worker_sends_one_declarative_message_for_the_owner_only() -> Result<()> {
    use atlas_server::App;
    let received = Received::default();
    let router = Router::new()
        .route("/push", post(capture))
        .with_state(received.clone());
    let (origin, server) = serve(router).await?;
    let config = push_config(&origin)?;
    let (device, decrypt) = web_push_device(format!("{origin}/push"))?;
    let due = due_reminder(&config, &device).await?;
    let app = App::new(due.store.clone())
        .await?
        .public_origin("http://localhost:3000")?
        .integration_config(config);
    app.integration_tick().await?;
    app.integration_tick().await?;

    let messages = received.lock().await;
    assert_eq!(messages.len(), 1, "two ticks must send one message");
    let history = due
        .store
        .reminder_delivery_history(&due.actor, None, 200)
        .await?;
    assert_eq!(history["items"].as_array().unwrap().len(), 1);
    let delivery = history["items"][0]["id"].as_str().unwrap();
    assert_eq!(history["items"][0]["state"], "delivered");
    assert_eq!(messages[0].0["topic"], delivery.replace('-', ""));
    let plain = decrypt(&messages[0].1)?;
    let value: Value = serde_json::from_slice(&plain)?;
    let parsed = parse_declarative(&plain, "http://localhost:3000/")
        .expect("the delivered bytes must parse as declarative");
    // The notification names the delivery that the owner's authorised history returns...
    assert_eq!(value["id"], delivery);
    assert_eq!(parsed.tag.as_deref(), Some(delivery));
    assert_eq!(
        parsed.navigate.origin(),
        Url::parse("http://localhost:3000")?.origin()
    );
    assert_eq!(
        query(&parsed.navigate),
        [
            ("delivery_id".into(), delivery.to_owned()),
            (
                "reminder_id".into(),
                value["reminder_id"].as_str().unwrap().to_owned()
            ),
            (
                "occurrence_id".into(),
                value["occurrence_id"].as_str().unwrap().to_owned()
            ),
        ]
    );
    // ...and another account cannot read that delivery.
    let other = Uuid::new_v4().to_string();
    due.store
        .add_account(&other, "someone-else", "unused")
        .await?;
    let hidden = due
        .store
        .reminder_delivery_history(&other, None, 200)
        .await?;
    assert!(hidden["items"].as_array().unwrap().is_empty());
    // Nothing private is in the message.
    let text = String::from_utf8(plain)?;
    for private in secrets(&device).into_iter().chain([
        "Private reminder text".into(),
        "declarative-owner".into(),
        "phone-declarative".into(),
        due.actor.clone(),
        due.subscription.clone(),
        origin.clone(),
    ]) {
        assert!(!text.contains(&private), "{private}");
    }
    drop(messages);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn worker_retry_repeats_the_same_declarative_message() -> Result<()> {
    use atlas_server::App;
    let received = Received::default();
    let router = Router::new()
        .route("/push", post(flaky))
        .with_state(received.clone());
    let (origin, server) = serve(router).await?;
    let config = push_config(&origin)?;
    let (device, decrypt) = web_push_device(format!("{origin}/push"))?;
    let due = due_reminder(&config, &device).await?;
    let app = App::new(due.store.clone())
        .await?
        .public_origin("http://localhost:3000")?
        .integration_config(config);
    app.integration_tick().await?;
    assert_eq!(received.lock().await.len(), 1);
    // The temporary failure backs off; an immediate tick sends nothing more.
    app.integration_tick().await?;
    assert_eq!(received.lock().await.len(), 1);
    sqlx::query("UPDATE reminder_deliveries SET next_attempt=0")
        .execute(&due.store.pool)
        .await?;
    app.integration_tick().await?;
    app.integration_tick().await?;

    let messages = received.lock().await;
    assert_eq!(messages.len(), 2);
    assert_ne!(messages[0].1, messages[1].1);
    assert_eq!(messages[0].0["topic"], messages[1].0["topic"]);
    let first = decrypt(&messages[0].1)?;
    assert_eq!(first, decrypt(&messages[1].1)?);
    assert!(parse_declarative(&first, "http://localhost:3000/").is_some());
    let history = due
        .store
        .reminder_delivery_history(&due.actor, None, 200)
        .await?;
    assert_eq!(history["items"].as_array().unwrap().len(), 1);
    assert_eq!(history["items"][0]["state"], "delivered");
    assert_eq!(history["items"][0]["attempts"], 2);
    assert_eq!(
        serde_json::from_slice::<Value>(&first)?["id"],
        history["items"][0]["id"]
    );
    drop(messages);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn worker_sends_nothing_after_the_subscription_is_removed() -> Result<()> {
    use atlas_core::calendars::ReminderCommand;
    use atlas_server::App;
    let received = Received::default();
    let router = Router::new()
        .route("/push", post(capture))
        .with_state(received.clone());
    let (origin, server) = serve(router).await?;
    let config = push_config(&origin)?;
    let (device, _) = web_push_device(format!("{origin}/push"))?;
    let due = due_reminder(&config, &device).await?;
    let app = App::new(due.store.clone())
        .await?
        .public_origin("http://localhost:3000")?
        .integration_config(config);
    // The delivery exists and is pending, then the subscription is removed before dispatch.
    due.store.schedule_reminders(atlas_server::now()).await?;
    let before = due
        .store
        .reminder_delivery_history(&due.actor, None, 200)
        .await?;
    assert_eq!(before["items"][0]["state"], "pending");
    let version =
        due.store.notification_subscriptions(&due.actor).await?["subscriptions"][0]["version"]
            .as_i64()
            .unwrap();
    due.store
        .reminder_command(
            &due.actor,
            &Uuid::new_v4().to_string(),
            &ReminderCommand::RemoveSubscription {
                id: due.subscription.clone(),
                expected_version: version,
            },
        )
        .await?;
    app.integration_tick().await?;
    assert!(received.lock().await.is_empty());
    let after = due
        .store
        .reminder_delivery_history(&due.actor, None, 200)
        .await?;
    assert_eq!(after["items"][0]["state"], "cancelled");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn worker_without_a_public_origin_sends_the_legacy_payload() -> Result<()> {
    use atlas_server::App;
    let received = Received::default();
    let router = Router::new()
        .route("/push", post(capture))
        .with_state(received.clone());
    let (origin, server) = serve(router).await?;
    let config = push_config(&origin)?;
    let (device, decrypt) = web_push_device(format!("{origin}/push"))?;
    let due = due_reminder(&config, &device).await?;
    let app = App::new(due.store.clone())
        .await?
        .integration_config(config);
    app.integration_tick().await?;

    let messages = received.lock().await;
    assert_eq!(messages.len(), 1);
    let plain = decrypt(&messages[0].1)?;
    assert!(parse_declarative(&plain, SCOPE).is_none());
    let value: Value = serde_json::from_slice(&plain)?;
    let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["id", "occurrence_id", "reminder_id"]);
    drop(messages);
    server.abort();
    Ok(())
}
