//! Over the HTTP router, a content write carries the sharing revision its author saw.
use anyhow::Result;
use atlas_core::Store;
use atlas_server::{App, hash_password};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

fn id() -> String {
    Uuid::new_v4().to_string()
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    operation: Option<&str>,
    body: Option<&Value>,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/api/experimental/v1/{path}"))
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if let Some(operation) = operation {
        request = request.header("idempotency-key", operation);
    }
    let body = body.map(Value::to_string).unwrap_or_default();
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

/// A rejected or successful command, with the guarantee that it never touches cookies.
async fn send(
    app: &Router,
    path: &str,
    token: &str,
    operation: &str,
    body: Value,
) -> (StatusCode, Value) {
    let (status, headers, text) =
        call(app, "POST", path, Some(token), Some(operation), Some(&body)).await;
    assert!(
        !headers.contains_key("set-cookie"),
        "a content write must not change cookies"
    );
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn changes(page: &Value) -> Vec<&Value> {
    page["batches"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|b| b["changes"].as_array().unwrap())
        .collect()
}

async fn login(app: &Router, user: &str, password: &str) -> String {
    let (status, _, text) = call(
        app,
        "POST",
        "sessions",
        None,
        None,
        Some(&json!({"username":user,"password":password,"device_id":"phone"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_str::<Value>(&text).unwrap()["access_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The field as `token`'s holder sees it in a fresh sync snapshot, if it can see it at all.
async fn field_of(app: &Router, token: &str, field: &str) -> Option<Value> {
    let (status, _, text) = call(app, "GET", "sync", Some(token), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let page: Value = serde_json::from_str(&text).unwrap();
    changes(&page)
        .into_iter()
        .map(|c| &c["resource"])
        .find(|r| r["id"] == json!(field))
        .cloned()
}

/// Three accounts on one in-process router. The `TempDir` keeps a SQLite file alive.
struct World {
    _dir: tempfile::TempDir,
    store: Store,
    app: Router,
    alice: String,
    bob: String,
    carol: String,
    a: String,
    b: String,
    c: String,
}

async fn world() -> Result<World> {
    let (dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let password = "policy-precondition-password";
    let hash = hash_password(password.into()).await?;
    let (alice, bob, carol) = (id(), id(), id());
    store.add_account(&alice, "alice", &hash).await?;
    store.add_account(&bob, "bob", &hash).await?;
    store.add_account(&carol, "carol", &hash).await?;
    let app = App::new(store.clone()).await?.router();
    let (a, b, c) = (
        login(&app, "alice", password).await,
        login(&app, "bob", password).await,
        login(&app, "carol", password).await,
    );
    Ok(World {
        _dir: dir,
        store,
        app,
        alice,
        bob,
        carol,
        a,
        b,
        c,
    })
}

#[tokio::test]
async fn stale_and_omitted_policy_versions_are_rejected_over_http() -> Result<()> {
    // `_dir` is named so that the SQLite file lives as long as the test does.
    let World {
        _dir,
        store,
        app,
        alice,
        bob,
        carol,
        a,
        b,
        c,
    } = world().await?;
    let (person, field) = (id(), id());
    let created = json!({"commands":[
        {"kind":"create_person","id":person,"name":"Morgan"},
        {"kind":"create_field","id":field,"person_id":person,"label":"Hobby","value":"Music"}
    ]});
    assert_eq!(
        send(&app, "commands", &a, &id(), created).await.0,
        StatusCode::OK
    );
    // Bob and carol both read the person and both may edit the field.
    let shared = json!({"commands":[
        {"kind":"grant","id":person,"expected_version":1,"account_id":bob,"edit":false},
        {"kind":"grant","id":person,"expected_version":2,"account_id":carol,"edit":false},
        {"kind":"grant","id":field,"expected_version":1,"account_id":bob,"edit":true},
        {"kind":"grant","id":field,"expected_version":2,"account_id":carol,"edit":true}
    ]});
    assert_eq!(
        send(&app, "access-commands", &a, &id(), shared).await.0,
        StatusCode::OK
    );

    // The collaborator reads the sharing revision from their own sync page, and learns
    // nothing about who else can see the field.
    let (status, _, text) = call(&app, "GET", "sync", Some(&b), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let page: Value = serde_json::from_str(&text)?;
    let seen = changes(&page)
        .into_iter()
        .map(|c| &c["resource"])
        .find(|r| r["id"] == json!(field))
        .expect("bob sees the shared field")
        .clone();
    let (version, revision) = (
        seen["version"].as_i64().unwrap(),
        seen["policy_version"].as_i64().unwrap(),
    );
    assert_eq!((version, revision), (1, 3));
    assert!(!text.contains(&alice) && !text.contains("grants"));
    // Carol holds the field too, and does not learn of bob either.
    assert!(field_of(&app, &c, &field).await.is_some());
    // The ACL stays with the owner.
    assert_eq!(
        call(
            &app,
            "GET",
            &format!("policies/{field}"),
            Some(&b),
            None,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &app,
            "GET",
            &format!("policies/{field}"),
            Some(&a),
            None,
            None
        )
        .await
        .0,
        StatusCode::OK
    );

    let edit = |version: i64, policy: Option<Value>, label: &str| {
        let mut command = json!({"kind":"edit","id":field,"expected_version":version,"label":label,"value":"Surfing"});
        if let Some(policy) = policy {
            command["expected_policy_version"] = policy;
        }
        json!({"commands":[command]})
    };
    // The current revision succeeds.
    assert_eq!(
        send(
            &app,
            "commands",
            &b,
            &id(),
            edit(1, Some(json!(revision)), "One")
        )
        .await
        .0,
        StatusCode::OK
    );

    // The owner narrows sharing from another session: carol is removed and bob keeps edit
    // access, so the audience really shrinks while the collaborator can still write.
    let narrow = json!({"commands":[{"kind":"replace_policy","id":field,"expected_version":revision,
        "policy":{"grants":[{"kind":"account","id":bob,"edit":true}],"exclude_accounts":[]}}]});
    assert_eq!(
        send(&app, "management-commands", &a, &id(), narrow).await.0,
        StatusCode::OK
    );
    assert!(
        field_of(&app, &c, &field).await.is_none(),
        "the narrowing removed carol"
    );
    let bobs = field_of(&app, &b, &field)
        .await
        .expect("bob keeps the field");
    assert_eq!(
        (bobs["can_edit"].as_bool(), bobs["policy_version"].as_i64()),
        (Some(true), Some(revision + 1))
    );

    // Stale, omitted, null and impossible revisions are all refused as conflicts.
    for (label, policy) in [
        ("stale", Some(json!(revision))),
        ("omitted", None),
        ("null", Some(Value::Null)),
        ("zero", Some(json!(0))),
    ] {
        let (status, body) = send(&app, "commands", &b, &id(), edit(2, policy, label)).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::CONFLICT, Some("conflict")),
            "{label}"
        );
        assert!(!body.to_string().contains(&alice) && !body.to_string().contains("grants"));
    }
    // A value that is not a 64-bit integer never reaches the domain.
    for bad in [json!("3"), json!(9_223_372_036_854_775_808_u64), json!(1.5)] {
        let (status, body) = send(&app, "commands", &b, &id(), edit(2, Some(bad), "bad")).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::BAD_REQUEST, Some("malformed_request"))
        );
    }
    let stored: (String, i64) = sqlx::query_as("SELECT label,version FROM resources WHERE id=$1")
        .bind(&field)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(
        stored,
        ("One".to_owned(), 2),
        "rejected writes changed nothing"
    );

    // After refetching, the same operation ID succeeds with the new revision.
    let (_, _, text) = call(&app, "GET", "sync", Some(&b), None, None).await;
    let refreshed: Value = serde_json::from_str(&text)?;
    let now = changes(&refreshed)
        .into_iter()
        .map(|c| &c["resource"])
        .find(|r| r["id"] == json!(field))
        .unwrap()
        .clone();
    let (version, revision) = (
        now["version"].as_i64().unwrap(),
        now["policy_version"].as_i64().unwrap(),
    );
    let narrowed = revision;
    assert_eq!((version, narrowed), (2, 4));
    let operation = id();
    assert_eq!(
        send(
            &app,
            "commands",
            &b,
            &operation,
            edit(2, Some(json!(narrowed - 1)), "Again")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        send(
            &app,
            "commands",
            &b,
            &operation,
            edit(2, Some(json!(narrowed)), "Again")
        )
        .await
        .0,
        StatusCode::OK
    );

    // The typed field command follows the same rule on its own route.
    let put = |policy: Option<Value>, text: &str| {
        let mut command = json!({"kind":"put_field","id":field,"parent_id":person,"expected_version":3,
            "label":"Hobby","value":{"kind":"text","text":text},"initial_policy":null});
        if let Some(policy) = policy {
            command["expected_policy_version"] = policy;
        }
        json!({"command":command})
    };
    for policy in [Some(json!(narrowed - 1)), None, Some(Value::Null)] {
        let (status, body) = send(&app, "task-commands", &b, &id(), put(policy, "Stale")).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::CONFLICT, Some("conflict"))
        );
    }
    assert_eq!(
        send(
            &app,
            "task-commands",
            &b,
            &id(),
            put(Some(json!(narrowed)), "Typed")
        )
        .await
        .0,
        StatusCode::OK
    );
    // Creating a field carries no revision, and offering one is refused.
    let create = |policy: Option<i64>| {
        let mut command = json!({"kind":"put_field","id":id(),"parent_id":person,"expected_version":null,
            "label":"New","value":{"kind":"text","text":"New"},"initial_policy":null});
        if let Some(policy) = policy {
            command["expected_policy_version"] = json!(policy);
        }
        json!({"command":command})
    };
    assert_eq!(
        send(&app, "task-commands", &a, &id(), create(Some(1)))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        send(&app, "task-commands", &a, &id(), create(None)).await.0,
        StatusCode::OK
    );
    Ok(())
}

/// Bob's typed save of an existing field and the owner's narrowing overlap on separate
/// connections of the in-process router, so the gate alone orders them. Which one commits
/// first is not fixed, and both orders are valid; what must hold in either is that bob's
/// answer describes what was stored. The deterministic ordering evidence is the held-gate
/// case in the core `storage::contention` tests; this checks the same rule end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_narrowing_and_field_save_give_a_consistent_answer_over_http() -> Result<()> {
    let w = world().await?;
    let (mut saved, mut rejected) = (0, 0);
    for round in 0..10 {
        let (person, field) = (id(), id());
        let created = json!({"commands":[
            {"kind":"create_person","id":person,"name":"Morgan"},
            {"kind":"create_field","id":field,"person_id":person,"label":"Hobby","value":"Original"}
        ]});
        assert_eq!(
            send(&w.app, "commands", &w.a, &id(), created).await.0,
            StatusCode::OK
        );
        let shared = json!({"commands":[
            {"kind":"grant","id":person,"expected_version":1,"account_id":w.bob,"edit":false},
            {"kind":"grant","id":person,"expected_version":2,"account_id":w.carol,"edit":false},
            {"kind":"grant","id":field,"expected_version":1,"account_id":w.bob,"edit":true},
            {"kind":"grant","id":field,"expected_version":2,"account_id":w.carol,"edit":true}
        ]});
        assert_eq!(
            send(&w.app, "access-commands", &w.a, &id(), shared).await.0,
            StatusCode::OK
        );
        let seen = field_of(&w.app, &w.b, &field).await.expect("bob holds it");
        let (version, revision) = (
            seen["version"].as_i64().unwrap(),
            seen["policy_version"].as_i64().unwrap(),
        );
        assert_eq!((version, revision), (1, 3));
        assert!(field_of(&w.app, &w.c, &field).await.is_some());

        let narrow = json!({"commands":[{"kind":"replace_policy","id":field,"expected_version":revision,
            "policy":{"grants":[{"kind":"account","id":w.bob,"edit":true}],"exclude_accounts":[]}}]});
        let save_id = id();
        let save = json!({"command":{"kind":"put_field","id":field,"parent_id":person,
            "expected_version":version,"expected_policy_version":revision,"label":"Hobby",
            "value":{"kind":"text","text":"Saved"},"initial_policy":null}});
        let owner = {
            let (app, token) = (w.app.clone(), w.a.clone());
            tokio::spawn(
                async move { send(&app, "management-commands", &token, &id(), narrow).await },
            )
        };
        let collaborator = {
            let (app, token, operation) = (w.app.clone(), w.b.clone(), save_id.clone());
            tokio::spawn(async move { send(&app, "task-commands", &token, &operation, save).await })
        };
        let ((narrowed, _), (answer, body)) = (owner.await?, collaborator.await?);
        // The narrowing never depends on the content version, so it always applies.
        assert_eq!(narrowed, StatusCode::OK, "round {round}");
        let now = field_of(&w.app, &w.b, &field).await.expect("bob keeps it");
        assert_eq!(now["policy_version"].as_i64(), Some(revision + 1));
        assert_eq!(now["can_edit"].as_bool(), Some(true));
        assert!(field_of(&w.app, &w.c, &field).await.is_none());
        let receipts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM receipts WHERE operation_id=$1")
                .bind(&save_id)
                .fetch_one(&w.store.pool)
                .await?;
        match answer {
            StatusCode::OK => {
                saved += 1;
                assert_eq!(
                    (
                        now["version"].as_i64(),
                        now["value"]["text"].as_str(),
                        receipts
                    ),
                    (Some(2), Some("Saved"), 1)
                );
            }
            StatusCode::CONFLICT => {
                rejected += 1;
                assert_eq!(body["code"].as_str(), Some("conflict"));
                assert_eq!(
                    (
                        now["version"].as_i64(),
                        now["value"]["text"].as_str(),
                        receipts
                    ),
                    (Some(1), Some("Original"), 0),
                    "a rejected save leaves no content and no receipt"
                );
            }
            other => panic!("round {round}: unexpected status {other}"),
        }
    }
    println!("saved before the narrowing: {saved}, rejected after it: {rejected}");
    Ok(())
}
