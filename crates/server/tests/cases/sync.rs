use anyhow::Result;
use atlas_core::Store;
use atlas_server::{App, hash_password};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    operation: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if let Some(operation) = operation {
        request = request.header("idempotency-key", operation);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        if bytes.is_empty() {
            Value::Null
        } else {
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            if status.is_client_error() || status.is_server_error() {
                assert_eq!(body["request_id"], request_id);
            }
            body
        },
    )
}

#[tokio::test]
async fn authenticated_two_device_sharing_and_sync() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let alice = Uuid::new_v4().to_string();
    let bob = Uuid::new_v4().to_string();
    let password = "local-test-password-123";
    let hash = hash_password(password.into()).await?;
    assert!(hash.starts_with("$argon2id$"));
    store.add_account(&alice, "alice", &hash).await?;
    store.add_account(&bob, "bob", &hash).await?;
    let app = App::new(store.clone()).await?.router();
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/experimental/v1/sync",
            None,
            None,
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    for user in ["alice", "unknown"] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/experimental/v1/sessions",
                None,
                None,
                json!({"username":user,"password":"wrong","device_id":"phone"})
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let mut tokens = Vec::new();
    for (user, device) in [("alice", "phone"), ("alice", "laptop"), ("bob", "phone")] {
        let (status, body) = request(
            &app,
            "POST",
            "/api/experimental/v1/sessions",
            None,
            None,
            json!({"username":user,"password":password,"device_id":device}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        tokens.push(body["access_token"].as_str().unwrap().to_owned());
    }
    let stored: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM sessions")
        .fetch_all(&store.pool)
        .await?;
    assert!(tokens.iter().all(|t| !stored.contains(t)));
    let person = Uuid::new_v4().to_string();
    let field = Uuid::new_v4().to_string();
    let secret = Uuid::new_v4().to_string();
    let operation = Uuid::new_v4().to_string();
    let body = json!({"commands":[
        {"kind":"create_person","id":person,"name":"Morgan"},
        {"kind":"create_field","id":field,"person_id":person,"label":"Hobby","value":"Music"},
        {"kind":"create_field","id":secret,"person_id":person,"label":"Secret gift","value":"Surprise"}
    ]});
    let first = request(
        &app,
        "POST",
        "/api/experimental/v1/commands",
        Some(&tokens[0]),
        Some(&operation),
        body.clone(),
    )
    .await;
    assert_eq!(first.0, StatusCode::OK);
    // AUTH-002: a different device replays the same logical operation exactly once.
    let second = request(
        &app,
        "POST",
        "/api/experimental/v1/commands",
        Some(&tokens[1]),
        Some(&operation),
        body,
    )
    .await;
    assert_eq!(first, second);
    let commands = json!({"commands":[{"kind":"grant","id":person,"expected_version":1,"account_id":bob,"edit":false},{"kind":"grant","id":field,"expected_version":1,"account_id":bob,"edit":true}]});
    let share_op = Uuid::new_v4().to_string();
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/experimental/v1/commands",
            Some(&tokens[0]),
            Some(&share_op),
            commands.clone()
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/experimental/v1/access-commands",
            Some(&tokens[0]),
            Some(&share_op),
            commands
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, b) = request(
        &app,
        "GET",
        "/api/experimental/v1/sync",
        Some(&tokens[2]),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!b.to_string().contains("Surprise"));
    assert!(!b.to_string().contains("Secret gift"));
    assert_eq!(b["batches"][0]["changes"].as_array().unwrap().len(), 2);
    // Bob sends the sharing revision he read from his own page; it is not owner-only.
    let revision = b["batches"][0]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["resource"]["id"] == json!(field))
        .unwrap()["resource"]["policy_version"]
        .as_i64()
        .unwrap();
    let edit = json!({"commands":[{"kind":"edit","id":field,"expected_version":1,"expected_policy_version":revision,"label":"Hobby","value":"Surfing"}]});
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/experimental/v1/commands",
            Some(&tokens[2]),
            Some(&Uuid::new_v4().to_string()),
            edit
        )
        .await
        .0,
        StatusCode::OK
    );
    let revoke =
        json!({"commands":[{"kind":"revoke","id":person,"expected_version":2,"account_id":bob}]});
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/experimental/v1/access-commands",
            Some(&tokens[0]),
            Some(&Uuid::new_v4().to_string()),
            revoke
        )
        .await
        .0,
        StatusCode::OK
    );
    let path = format!(
        "/api/experimental/v1/sync?cursor={}",
        b["next_cursor"].as_str().unwrap()
    );
    let denied = request(&app, "GET", &path, Some(&tokens[2]), None, Value::Null).await;
    assert_eq!(denied.0, StatusCode::OK);
    assert!(!denied.1.to_string().contains("Surfing"));
    assert!(denied.1.to_string().contains("remove"));
    let fresh = request(
        &app,
        "GET",
        "/api/experimental/v1/sync",
        Some(&tokens[2]),
        None,
        Value::Null,
    )
    .await;
    assert!(
        fresh.1["batches"][0]["changes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // AUTH-003: logout immediately prevents subsequent authenticated requests.
    assert_eq!(
        request(
            &app,
            "DELETE",
            "/api/experimental/v1/sessions/current",
            Some(&tokens[2]),
            None,
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/experimental/v1/sync",
            Some(&tokens[2]),
            None,
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    // Unknown request properties and unbounded limits are rejected.
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/experimental/v1/sync?limit=999",
            Some(&tokens[0]),
            None,
            Value::Null
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let restricted = App::new(store.clone())
        .await?
        .directory_enabled(false)
        .router();
    assert_eq!(
        request(
            &restricted,
            "GET",
            "/api/experimental/v1/directory",
            Some(&tokens[0]),
            None,
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let found = request(
        &restricted,
        "GET",
        "/api/experimental/v1/directory?username=bob",
        Some(&tokens[0]),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(found.0, StatusCode::OK);
    assert_eq!(found.1[0]["id"], bob);
    assert_eq!(found.1[0].as_object().unwrap().len(), 2);
    store.pool.close().await;
    Ok(())
}

#[tokio::test]
async fn unrelated_failed_logins_do_not_lock_out_valid_accounts() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    store
        .add_account(
            &Uuid::new_v4().to_string(),
            "valid",
            &hash_password("local-test-password-123".into()).await?,
        )
        .await?;
    let app = App::new(store.clone()).await?.router();
    for attempt in 0..25 {
        let (status, _) = request(
            &app,
            "POST",
            "/api/experimental/v1/sessions",
            None,
            None,
            json!({"username":"unknown","password":"wrong","device_id":"phone"}),
        )
        .await;
        assert_eq!(
            status,
            if attempt < 10 {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::TOO_MANY_REQUESTS
            }
        );
    }
    let (status, session) = request(
        &app,
        "POST",
        "/api/experimental/v1/sessions",
        None,
        None,
        json!({"username":"valid","password":"local-test-password-123","device_id":"phone"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    chrono::DateTime::parse_from_rfc3339(session["expires_at"].as_str().unwrap())?;
    assert_eq!(
        request(&app, "GET", "/ready", None, None, Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    store.pool.close().await;
    assert_eq!(
        request(&app, "GET", "/ready", None, None, Value::Null)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        request(&app, "GET", "/health", None, None, Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    Ok(())
}

/// BE-B1: `limit` over HTTP. The ceiling round-trips, an absent `limit` is the default, and
/// out-of-range, malformed and repeated values keep their existing answers.
#[tokio::test]
async fn sync_limit_ceiling_default_and_rejections_over_http() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let owner = Uuid::new_v4().to_string();
    let password = "local-test-password-123";
    store
        .add_account(&owner, "pages", &hash_password(password.into()).await?)
        .await?;
    let app = App::new(store.clone()).await?.router();
    let mut tokens = Vec::new();
    let mut cursors = Vec::new();
    for device in ["ceiling", "default"] {
        let (status, body) = request(
            &app,
            "POST",
            "/api/experimental/v1/sessions",
            None,
            None,
            json!({"username":"pages","password":password,"device_id":device}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let token = body["access_token"].as_str().unwrap().to_owned();
        let (status, page) = request(
            &app,
            "GET",
            "/api/experimental/v1/sync?limit=200",
            Some(&token),
            None,
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page["has_more"], false);
        cursors.push(page["next_cursor"].as_str().unwrap().to_owned());
        tokens.push(token);
    }
    // 201 pending batches for the account: one person, then single edits seeded in the store.
    let person = Uuid::new_v4().to_string();
    store
        .apply(
            &owner,
            &Uuid::new_v4().to_string(),
            &[atlas_core::Command::CreatePerson {
                id: person.clone(),
                name: "Paged".into(),
                initial_policy: None,
            }],
        )
        .await?;
    for version in 1..=200 {
        store
            .apply(
                &owner,
                &Uuid::new_v4().to_string(),
                &[atlas_core::Command::Edit {
                    id: person.clone(),
                    expected_version: version,
                    expected_policy_version: Some(1),
                    label: format!("Version {version}"),
                    value: String::new(),
                }],
            )
            .await?;
    }
    let get = |path: String, token: String| {
        let app = app.clone();
        async move { request(&app, "GET", &path, Some(&token), None, Value::Null).await }
    };
    let delta =
        |cursor: &str, limit: &str| format!("/api/experimental/v1/sync?cursor={cursor}{limit}");

    // H1: the ceiling, then the following page.
    let (status, first) = get(delta(&cursors[0], "&limit=200"), tokens[0].clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["phase"], "delta");
    assert_eq!(first["batches"].as_array().unwrap().len(), 200);
    assert_eq!(first["has_more"], true);
    let next = first["next_cursor"].as_str().unwrap();
    let (status, second) = get(delta(next, "&limit=200"), tokens[0].clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["batches"].as_array().unwrap().len(), 1);
    assert_eq!(second["has_more"], false);

    // H2: no limit is the default of 50.
    let (status, page) = get(delta(&cursors[1], ""), tokens[1].clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["batches"].as_array().unwrap().len(), 50);
    assert_eq!(page["has_more"], true);

    // H3 and H4: the existing answers for rejected values.
    for (query, status, code) in [
        (
            "limit=201",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        ("limit=0", StatusCode::UNPROCESSABLE_ENTITY, "invalid_value"),
        (
            "limit=999",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        ("limit=-1", StatusCode::BAD_REQUEST, "malformed_request"),
        (
            "limit=18446744073709551616",
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
        ("limit=abc", StatusCode::BAD_REQUEST, "malformed_request"),
        ("limit=", StatusCode::BAD_REQUEST, "malformed_request"),
        (
            "limit=1&limit=2",
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
        ("unknown=1", StatusCode::BAD_REQUEST, "malformed_request"),
    ] {
        let (actual, body) = get(
            format!("/api/experimental/v1/sync?{query}"),
            tokens[0].clone(),
        )
        .await;
        assert_eq!(
            (actual, body["code"].as_str()),
            (status, Some(code)),
            "{query}"
        );
    }
    store.pool.close().await;
    Ok(())
}
