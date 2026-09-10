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

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/api/experimental/v1/{path}"))
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        headers,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        },
    )
}

#[tokio::test]
async fn cookies_require_csrf_and_revocation_survives_reload() -> anyhow::Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let account = Uuid::new_v4().to_string();
    store
        .add_account(
            &account,
            "alice",
            &hash_password("browser-password-123".into()).await?,
        )
        .await?;
    let app = App::new(store.clone())
        .await?
        .public_origin("https://atlas.example")?
        .router();
    let input =
        json!({"username":"alice", "password":"browser-password-123", "device_id":"browser"});
    for headers in [
        vec![],
        vec![("origin", "https://attacker.example")],
        vec![("origin", "null")],
    ] {
        assert_eq!(
            call(&app, "POST", "browser-sessions", &headers, input.clone())
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    let (status, headers, session) = call(
        &app,
        "POST",
        "browser-sessions",
        &[("origin", "https://atlas.example")],
        input,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(session["account_id"], account);
    assert!(session.get("access_token").is_none());
    let set_cookie = headers["set-cookie"].to_str()?;
    for flag in [
        "__Host-atlas_session=",
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
        "Path=/",
    ] {
        assert!(set_cookie.contains(flag), "{set_cookie}");
    }
    assert!(!set_cookie.contains("Domain="));
    let cookie = set_cookie.split(';').next().unwrap();
    let csrf = session["csrf_token"].as_str().unwrap();
    let stored: String = sqlx::query_scalar("SELECT token_hash FROM sessions")
        .fetch_one(&store.pool)
        .await?;
    assert_ne!(stored, cookie.split_once('=').unwrap().1);
    assert_eq!(
        call(&app, "GET", "sync", &[("cookie", cookie)], Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &app,
            "GET",
            "browser-sessions/current",
            &[("cookie", cookie)],
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, _, restored) = call(
        &app,
        "GET",
        "browser-sessions/current",
        &[("cookie", cookie), ("x-atlas-session", "1")],
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(restored, session);
    let good = [("cookie", cookie), ("x-csrf-token", csrf)];
    assert_eq!(
        call(&app, "GET", "sync", &good, Value::Null).await.0,
        StatusCode::OK
    );
    let mut cross_origin = good.to_vec();
    cross_origin.push(("origin", "https://attacker.example"));
    assert_eq!(
        call(
            &app,
            "DELETE",
            "sessions/current",
            &cross_origin,
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let operation = Uuid::new_v4().to_string();
    let mut mutation_headers = good.to_vec();
    mutation_headers.push(("idempotency-key", &operation));
    assert_eq!(
        call(
            &app,
            "POST",
            "commands",
            &mutation_headers,
            json!({"commands":[{"kind":"create_person", "id":Uuid::new_v4().to_string(), "name":"A friend"}]})
        )
        .await
        .0,
        StatusCode::OK
    );
    let duplicate = format!("{cookie}; {cookie}");
    assert_eq!(
        call(
            &app,
            "GET",
            "sync",
            &[("cookie", &duplicate), ("x-csrf-token", csrf)],
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, headers, _) = call(&app, "DELETE", "sessions/current", &good, Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(headers["set-cookie"].to_str()?.contains("Max-Age=0"));
    assert_eq!(
        call(&app, "GET", "sync", &good, Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}
