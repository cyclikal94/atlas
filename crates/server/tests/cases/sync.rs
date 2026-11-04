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

/// BE-Q23 N1: `max_changes` through the HTTP route. Out-of-range integers are `422
/// invalid_value`; anything that is not a non-negative integer, or a repeated parameter, is
/// `400 malformed_request`, exactly as `limit` already behaves; a split revision carries
/// `segment` and a `d2` continuation; a tampered continuation is `410 resync_required`.
#[tokio::test]
async fn bounded_sync_parameters() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let alice = Uuid::new_v4().to_string();
    let password = "local-test-password-123";
    store
        .add_account(&alice, "alice", &hash_password(password.into()).await?)
        .await?;
    let app = App::new(store.clone()).await?.router();
    let (status, body) = request(
        &app,
        "POST",
        "/api/experimental/v1/sessions",
        None,
        None,
        json!({"username":"alice","password":password,"device_id":"phone"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = body["access_token"].as_str().unwrap().to_owned();
    let sync = |query: String| {
        let (app, token) = (app.clone(), token.clone());
        async move {
            request(
                &app,
                "GET",
                &format!("/api/experimental/v1/sync{query}"),
                Some(&token),
                None,
                Value::Null,
            )
            .await
        }
    };
    let (status, head) = sync(String::new()).await;
    assert_eq!(status, StatusCode::OK);
    let cursor = head["next_cursor"].as_str().unwrap().to_owned();
    let person = Uuid::new_v4().to_string();
    let mut commands = vec![json!({"kind":"create_person","id":person,"name":"Bounded"})];
    commands.extend((0..12).map(|_| {
        json!({"kind":"create_field","id":Uuid::new_v4().to_string(),"person_id":person,
            "label":"Field","value":"Value"})
    }));
    let (status, _) = request(
        &app,
        "POST",
        "/api/experimental/v1/commands",
        Some(&token),
        Some(&Uuid::new_v4().to_string()),
        json!({ "commands": commands }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for (query, expected, code) in [
        (
            "max_changes=3",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        (
            "max_changes=801",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        (
            "max_changes=0",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        (
            "max_changes=-1",
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
        (
            "max_changes=abc",
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
        (
            "max_changes=4.5",
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
        ("max_changes=", StatusCode::BAD_REQUEST, "malformed_request"),
        (
            "max_changes=4&max_changes=5",
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
        (
            "limit=4&limit=5",
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
        ("max_change=4", StatusCode::BAD_REQUEST, "malformed_request"),
    ] {
        let (status, body) = sync(format!("?cursor={cursor}&{query}")).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (expected, Some(code)),
            "{query}"
        );
    }
    // Without the bound: one whole batch, no segment member.
    let (status, whole) = sync(format!("?cursor={cursor}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(whole["batches"].as_array().unwrap().len(), 1);
    assert!(whole["batches"][0].get("segment").is_none());
    assert!(whole["next_cursor"].as_str().unwrap().starts_with("d1."));
    // With it: pieces of 4 (the person and three fields), each with its continuation.
    let mut next = cursor.clone();
    let mut pieces = Vec::new();
    loop {
        let (status, page) = sync(format!("?cursor={next}&max_changes=4")).await;
        assert_eq!(status, StatusCode::OK);
        let batch = &page["batches"][0];
        let mut members: Vec<_> = batch.as_object().unwrap().keys().cloned().collect();
        members.sort();
        assert_eq!(members, ["changes", "revision", "segment"]);
        let mut members: Vec<_> = batch["segment"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        members.sort();
        assert_eq!(members, ["index", "last"]);
        assert!(batch["changes"].as_array().unwrap().len() <= 4);
        next = page["next_cursor"].as_str().unwrap().to_owned();
        pieces.push(batch["segment"].clone());
        if batch["segment"]["last"] == true {
            assert!(next.starts_with("d1.") && page["has_more"] == false);
            break;
        }
        assert!(next.starts_with("d2.") && page["has_more"] == true);
    }
    assert_eq!(pieces.len(), 4, "12 fields, three per piece");
    let (status, first) = sync(format!("?cursor={cursor}&max_changes=4")).await;
    assert_eq!(status, StatusCode::OK);
    let continuation = first["next_cursor"].as_str().unwrap().to_owned();
    let mut forged = continuation.clone();
    let last = forged.pop().unwrap();
    forged.push(if last == '0' { '1' } else { '0' });
    let (status, body) = sync(format!("?cursor={forged}&max_changes=4")).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::GONE, Some("resync_required"))
    );
    // A continuation without the bound returns the rest of the revision at once.
    let (status, rest) = sync(format!("?cursor={continuation}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rest["batches"][0]["segment"],
        json!({"index":1,"last":true})
    );
    assert_eq!(rest["batches"][0]["changes"].as_array().unwrap().len(), 10);
    store.pool.close().await;
    Ok(())
}

/// DA-H1/Q6 (Review 12, 3 November 2026; authority BE-Q23-HMAC-TOKEN-SCOPE-20261103): fixed
/// continuation vectors through the real `/sync` route and bearer authentication. Every signing
/// input is declared: accounts, device `phone` and its key (installed through test-only database
/// access before any cursor is issued), the sync time (this App's test-only fixed time; sessions
/// keep the system clock), operation and resource IDs, notice order, policies and the delivery
/// ledger. The expected tokens come from `crates/core/tests/fixtures/q23_continuation_vectors.py`
/// (Python's standard library), not from the Rust signer. Two histories, without and with a
/// never-sent hidden notice, reach the same public revision and index in both passes; their plain
/// positions differ, their sealed fields do not cancel, and their bodies are identical. Owner
/// writes go through `Store::apply`, the command path the commands route uses.
#[tokio::test]
async fn continuation_vectors_over_http() -> Result<()> {
    const KEY: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";
    const T: i64 = 1_788_868_800;
    const EXPIRY: i64 = T + 90 * 86400;
    // `python3 -I crates/core/tests/fixtures/q23_continuation_vectors.py`, device `phone`.
    const START: &str = "d1.0000000000000002.000000006b169fc0.092f65b24b87c6ac7b835bcd175d45915e67ed65d48fa4f86a55158d2b9bbf22";
    const DONE: &str = "d1.0000000000000003.000000006b169fc0.456837dbac423857eb73aa49c94689fb616b5993c31c2a16d39a9ee475a40be9";
    const I1_U4: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000001.3d0fa71d.c185d642f23fd76ea76abd0bc4636d8a29df1bd6634c3fcebbd4edc491b0b7ab";
    const I1_U5: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000001.d9afe540.62f66aaeb7fc094c12b1a2451723711e8ca01b97d814578d6ae96042dc672fba";
    const I2_U7: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000002.1b3c6041.cf9b205a72038563f56b370aae5316a858f33c16ecb3dad5a91ba330250728ac";
    const I2_U8: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000002.90a993c0.77d91caec603a59cdf280c23f292cdb280a6b84bb4adc1d12df30c1b7010c9da";
    const I3_R4: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000003.2565a480.b547e049defeec4cb501b866c0ed91a8400c5950e77cbf1a1d5b5bb1ec092e2e";
    const I3_R5: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000003.8189feda.b2695eedcdc4a1cd4590039e5d7de3bc1c1250ee2090122fb59997398d8ed949";
    use atlas_core::{
        Command,
        policy::{Policy, PrincipalGrant},
    };
    let fixed = |n: u128| Uuid::from_u128(n).to_string();
    let (owner, reader) = (fixed(0xA0), fixed(0xB0));
    // The plain position of `token`, with ring's HMAC (independent of the server's `hmac`).
    let open = |token: &str| -> (String, u32) {
        let mac = |domain: &str, payload: &str| {
            let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, KEY.as_bytes());
            let message = format!("{domain}\n{reader}\n5:phone\n{payload}");
            ring::hmac::sign(&key, message.as_bytes()).as_ref().to_vec()
        };
        let parts: Vec<_> = token.split('.').collect();
        assert_eq!(parts.len(), 7, "a continuation: {token}");
        let pad = mac("atlas-delta-v2-siv", parts[6]);
        let sealed = u32::from_str_radix(parts[5], 16).unwrap();
        (
            parts[..5].join("."),
            sealed ^ u32::from_be_bytes([pad[0], pad[1], pad[2], pad[3]]),
        )
    };
    let sealed = |token: &str| u32::from_str_radix(token.split('.').nth(5).unwrap(), 16).unwrap();
    let share = || Policy {
        grants: vec![PrincipalGrant::Account {
            id: reader.clone(),
            edit: false,
        }],
        exclude_accounts: vec![],
    };
    let mut seen = Vec::new();
    let mut bodies = Vec::new();
    for (h, hidden) in [false, true].into_iter().enumerate() {
        let (_dir, url) = crate::support::database::database_url().await?;
        let store = Store::connect(&url).await?;
        store.migrate().await?;
        let password = "local-test-password-123";
        store.add_account(&owner, "q23-owner", "test-only").await?;
        store
            .add_account(
                &reader,
                "q23-reader",
                &hash_password(password.into()).await?,
            )
            .await?;
        let app = App::new(store.clone())
            .await?
            .sync_time_for_tests(T)
            .router();
        let wall = App::new(store.clone()).await?.router();
        let login = |device: &'static str| {
            let app = app.clone();
            async move {
                let (status, body) = request(
                    &app,
                    "POST",
                    "/api/experimental/v1/sessions",
                    None,
                    None,
                    json!({"username":"q23-reader","password":password,"device_id":device}),
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{body}");
                body["access_token"].as_str().unwrap().to_owned()
            }
        };
        let phone = login("phone").await;
        let sync = |app: Router, token: String, query: String| async move {
            request(
                &app,
                "GET",
                &format!("/api/experimental/v1/sync{query}"),
                Some(&token),
                None,
                Value::Null,
            )
            .await
        };
        // Registration (its random key signs only this snapshot's stored UUID), then the key.
        let (status, _) = sync(app.clone(), phone.clone(), String::new()).await;
        assert_eq!(status, StatusCode::OK);
        sqlx::query(
            "UPDATE sync_devices SET cursor_key=$1 WHERE account_id=$2 AND device_id='phone'",
        )
        .bind(KEY)
        .bind(&reader)
        .execute(&store.pool)
        .await?;
        // Sessions keep the system clock, whatever this App's sync time.
        let expires: i64 = sqlx::query_scalar(
            "SELECT expires_at FROM sessions WHERE account_id=$1 AND device_id='phone'",
        )
        .bind(&reader)
        .fetch_one(&store.pool)
        .await?;
        assert!(
            expires > atlas_server::now(),
            "a session from the wall clock"
        );
        let person = fixed(1);
        let fields: Vec<String> = (1..=12).map(|i| fixed(i * 100)).collect();
        let never_sent = fixed(350);
        let mut commands = vec![Command::CreatePerson {
            id: person.clone(),
            name: "Vector".into(),
            initial_policy: Some(share()),
        }];
        for f in fields.iter().chain(hidden.then_some(&never_sent)) {
            commands.push(Command::CreateField {
                id: f.clone(),
                person_id: person.clone(),
                label: "Shared".into(),
                value: "Shared value".into(),
                initial_policy: Some(share()),
            });
        }
        let policy = |id: String| {
            let store = store.clone();
            async move {
                sqlx::query_scalar::<_, i64>("SELECT policy_version FROM resources WHERE id=$1")
                    .bind(id)
                    .fetch_one(&store.pool)
                    .await
            }
        };
        assert_eq!(store.apply(&owner, &fixed(0xF001), &commands).await?, 1);
        let revoke = |id: &str, version: i64| Command::Revoke {
            id: id.into(),
            expected_version: version,
            account_id: reader.clone(),
        };
        let version = policy(person.clone()).await?;
        store
            .apply(&owner, &fixed(0xF002), &[revoke(&person, version)])
            .await?;
        // The anchor: drained from no cursor at the fixed time.
        let mut cursor = String::new();
        loop {
            let (status, page) = sync(app.clone(), phone.clone(), cursor.clone()).await;
            assert_eq!(status, StatusCode::OK);
            cursor = format!("?cursor={}", page["next_cursor"].as_str().unwrap());
            if page["has_more"] == false {
                break;
            }
        }
        assert_eq!(cursor, format!("?cursor={START}"), "history {h}: anchor");
        let grant = Command::Grant {
            id: person.clone(),
            expected_version: policy(person.clone()).await?,
            account_id: reader.clone(),
            edit: false,
        };
        assert_eq!(store.apply(&owner, &fixed(0xF003), &[grant]).await?, 3);
        if hidden {
            let version = policy(never_sent.clone()).await?;
            store
                .apply(&owner, &fixed(0xF004), &[revoke(&never_sent, version)])
                .await?;
        }
        let piece = |token: &str| {
            sync(
                app.clone(),
                phone.clone(),
                format!("?cursor={token}&max_changes=4&limit=1"),
            )
        };
        let mut pages = Vec::new();
        let mut expect = |page: &Value, index: u32, plain: u32, golden: &str| {
            let next = page["next_cursor"].as_str().unwrap().to_owned();
            let (header, position) = open(&next);
            assert_eq!(
                header,
                format!("d2.{:016x}.{EXPIRY:016x}.{:016x}.{index:08x}", 2, 3),
                "history {h} index {index}: header"
            );
            assert_eq!(position, plain, "history {h} index {index}: position");
            assert_eq!(next, golden, "history {h} index {index}: golden token");
            seen.push((next.clone(), plain));
            pages.push(page["batches"].clone());
            next
        };
        let (status, first) = piece(START).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        let one = expect(&first, 1, [4, 5][h], [I1_U4, I1_U5][h]);
        // A retry at the same time: the same response.
        assert_eq!(piece(START).await, (StatusCode::OK, first.clone()));
        let (_, second) = piece(&one).await;
        let two = expect(&second, 2, [7, 8][h], [I2_U7, I2_U8][h]);
        let version = policy(person.clone()).await?;
        store
            .apply(&owner, &fixed(0xF005), &[revoke(&person, version)])
            .await?;
        let (_, third) = piece(&two).await;
        let three = expect(&third, 3, (1 << 16) | [4, 5][h], [I3_R4, I3_R5][h]);
        let removed: Vec<Value> = fields[..4]
            .iter()
            .map(|id| json!({"kind":"remove","id":id}))
            .collect();
        assert_eq!(third["batches"][0]["changes"], json!(removed));
        let (status, last) = piece(&three).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            last["next_cursor"], DONE,
            "history {h}: the fixed d1 cursor"
        );
        assert_eq!(
            last["batches"][0]["segment"],
            json!({"index":3,"last":true})
        );
        let text = [&first, &second, &third, &last]
            .iter()
            .map(|p| p.to_string())
            .collect::<String>();
        assert!(!text.contains(&never_sent), "history {h}: never disclosed");
        pages.push(last["batches"].clone());
        // Isolation: an App without the fixed time signs with the system clock. This also
        // registers a second device of the same account, with its own key.
        let tablet = login("tablet").await;
        let before = atlas_server::now();
        let mut query = "?limit=200".to_owned();
        let next = loop {
            let (status, page) = sync(wall.clone(), tablet.clone(), query).await;
            assert_eq!(status, StatusCode::OK);
            let next = page["next_cursor"].as_str().unwrap().to_owned();
            if page["has_more"] == false {
                break next;
            }
            query = format!("?cursor={next}&limit=200");
        };
        let expiry = i64::from_str_radix(next.split('.').nth(2).unwrap(), 16)?;
        assert!(
            (before + 90 * 86400..=atlas_server::now() + 90 * 86400).contains(&expiry),
            "the system clock, not {T}"
        );
        // A changed token, and the phone's token presented by the tablet even with the phone's
        // key installed there (test-only): refused, the MAC binds the device.
        sqlx::query(
            "UPDATE sync_devices SET cursor_key=$1 WHERE account_id=$2 AND device_id='tablet'",
        )
        .bind(KEY)
        .bind(&reader)
        .execute(&store.pool)
        .await?;
        let mut forged = one.clone();
        let tail = forged.pop().unwrap();
        forged.push(if tail == '0' { '1' } else { '0' });
        for (token, device) in [(forged, phone.clone()), (one.clone(), tablet)] {
            let (status, body) = sync(
                app.clone(),
                device,
                format!("?cursor={token}&max_changes=4"),
            )
            .await;
            assert_eq!(
                (status, body["code"].as_str()),
                (StatusCode::GONE, Some("resync_required"))
            );
        }
        bodies.push(pages);
    }
    assert_eq!(bodies[0], bodies[1], "the hidden notice changes no body");
    // Same public header at each index, different plain positions: no cancellation.
    for index in 0..3 {
        let (x, a) = &seen[index];
        let (y, b) = &seen[index + 3];
        assert_eq!(open(x).0, open(y).0, "same public header");
        assert_ne!(sealed(x) ^ sealed(y), a ^ b, "privacy: positions cancel");
    }
    Ok(())
}
