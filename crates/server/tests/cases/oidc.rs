use crate::support::oidc::Provider;
use atlas_core::Store;
use atlas_server::{App, hash_password};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use openidconnect::PkceCodeChallenge;
use serde_json::{Value, json};
use std::collections::HashMap;
use tower::ServiceExt;
use uuid::Uuid;

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("/api/experimental/v1/{path}"))
        .header("content-type", "application/json");
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
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
/// `(callback_path, binding_cookie, attempt_id, verifier)`: `attempt_id`/`verifier` are the
/// caller's own BE-Q19 activation-grant attempt, threaded through so a test can redeem the grant
/// the callback eventually issues. `attempt_id` is a fresh UUID unless the caller supplies one
/// (to exercise the accepted-but-unusual-character shape, e.g. reserved fragment bytes).
async fn begin(
    app: &Router,
    provider: &Provider,
    mode: &str,
    link: Option<&str>,
    attempt_id: Option<&str>,
) -> (String, String, String, String) {
    let mut headers = vec![("origin", "https://atlas.example")];
    let auth = link.map(|t| format!("Bearer {t}"));
    if let Some(auth) = &auth {
        headers.push(("authorization", auth));
    }
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let attempt_id = attempt_id
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let (status, headers, body) = call(
        app,
        "POST",
        "oidc/start",
        &headers,
        json!({"device_id":"browser","link":link.is_some(),"attempt_id":attempt_id,"attempt_challenge":challenge.as_str()}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let url = url::Url::parse(body["authorization_url"].as_str().unwrap()).unwrap();
    let params: HashMap<_, _> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(params["code_challenge_method"], "S256");
    let code = Uuid::new_v4().to_string();
    provider.pending.lock().unwrap().insert(
        code.clone(),
        (
            params["nonce"].clone(),
            params["code_challenge"].clone(),
            mode.into(),
        ),
    );
    (
        format!("oidc/callback?state={}&code={code}", params["state"]),
        headers["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned(),
        attempt_id,
        verifier.secret().to_owned(),
    )
}

/// The `#key=value&...` pairs of a redirect's fragment (BE-Q19's grant/attempt/account carrier),
/// decoded as `application/x-www-form-urlencoded` to match how the server writes it.
fn fragment(location: &str) -> HashMap<String, String> {
    let (_, frag) = location
        .split_once('#')
        .unwrap_or_else(|| panic!("no fragment in {location}"));
    url::form_urlencoded::parse(frag.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

async fn scenario() -> anyhow::Result<()> {
    let (provider, server) = crate::support::oidc::serve().await?;
    let (_dir, database) = crate::support::database::database_url().await?;
    let store = Store::connect(&database).await?;
    store.migrate().await?;
    let account = Uuid::new_v4().to_string();
    store
        .add_account(
            &account,
            "alice",
            &hash_password("oidc-link-password".into()).await?,
        )
        .await?;
    let app = App::new(store.clone())
        .await?
        .public_origin("https://atlas.example")?
        .oidc(&provider.issuer, "atlas-test", None, false)?
        .oidc_native_redirects(vec!["dev.atlas.app:/oauth/callback".into()])?
        .router();
    let (_, _, session) = call(
        &app,
        "POST",
        "sessions",
        &[],
        json!({"username":"alice","password":"oidc-link-password","device_id":"native"}),
    )
    .await;
    let bearer = session["access_token"].as_str().unwrap();
    for mode in [
        "nonce",
        "audience",
        "issuer",
        "expired",
        "signature",
        "access_hash",
    ] {
        let (path, cookie, _, _) = begin(&app, &provider, mode, Some(bearer), None).await;
        assert_eq!(
            call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED,
            "{mode}"
        );
    }
    let (path, cookie, _, _) = begin(&app, &provider, "valid", None, None).await;
    assert_eq!(
        call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN,
        "unknown subjects must not auto-link"
    );
    let (path, cookie, attempt_id, verifier) =
        begin(&app, &provider, "valid", Some(bearer), None).await;
    assert_eq!(
        call(&app, "GET", &path, &[], Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, headers, _) = call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    // BE-Q19: a browser callback issues a grant and sets no session cookie; only the OIDC
    // binding cookie is cleared (matching the native flow's own removal-only assertion below).
    assert!(!headers["set-cookie"].to_str()?.contains("atlas_session"));
    let redirect = headers["location"].to_str()?.to_owned();
    assert!(redirect.starts_with("https://atlas.example/#"));
    let sent = fragment(&redirect);
    assert_eq!(
        sent["atlas_account"], account,
        "check (g): the linked account"
    );
    assert_eq!(sent["atlas_attempt"], attempt_id);
    let (status, _, activated) = call(
        &app,
        "POST",
        "browser-sessions/activate",
        &[("origin", "https://atlas.example")],
        json!({"grant":sent["atlas_grant"],"verifier":verifier}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{activated}");
    assert_eq!(activated["account_id"], account);
    assert_eq!(
        call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED,
        "the flow row is consumed exactly once"
    );
    let mapped: String = sqlx::query_scalar("SELECT account_id FROM external_identities")
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(mapped, account);
    let (path, cookie, _, _) = begin(&app, &provider, "valid", None, None).await;
    assert_eq!(
        call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null)
            .await
            .0,
        StatusCode::SEE_OTHER
    );
    let (path, cookie, _, _) = begin(&app, &provider, "valid", Some(bearer), None).await;
    call(
        &app,
        "DELETE",
        "sessions/current",
        &[("authorization", &format!("Bearer {bearer}"))],
        Value::Null,
    )
    .await;
    assert_eq!(
        call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED,
        "logout must invalidate pending linking"
    );

    for expired in [false, true] {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut start_url = url::Url::parse("https://atlas.example/oidc/native/start")?;
        start_url
            .query_pairs_mut()
            .append_pair("device_id", "native-oidc")
            .append_pair("redirect_uri", "dev.atlas.app:/oauth/callback")
            .append_pair("code_challenge", challenge.as_str())
            .append_pair("state", "client-state");
        let path = format!("oidc/native/start?{}", start_url.query().unwrap());
        let (status, headers, _) = call(&app, "GET", &path, &[], Value::Null).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let auth_url = url::Url::parse(headers["location"].to_str()?)?;
        let params: HashMap<_, _> = auth_url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let code = Uuid::new_v4().to_string();
        provider.pending.lock().unwrap().insert(
            code.clone(),
            (
                params["nonce"].clone(),
                params["code_challenge"].clone(),
                "valid".into(),
            ),
        );
        let cookie = headers["set-cookie"].to_str()?.split(';').next().unwrap();
        let (status, headers, _) = call(
            &app,
            "GET",
            &format!("oidc/callback?state={}&code={code}", params["state"]),
            &[("cookie", cookie)],
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(!headers["set-cookie"].to_str()?.contains("atlas_session"));
        let destination = url::Url::parse(headers["location"].to_str()?)?;
        assert_eq!(destination.scheme(), "dev.atlas.app");
        let result: HashMap<_, _> = destination
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(result["state"], "client-state");
        let handoff = &result["code"];
        if expired {
            sqlx::query("UPDATE native_handoffs SET expires_at=0")
                .execute(&store.pool)
                .await?;
        }
        let (status, _) = {
            let (s, _, b) = call(
                &app,
                "POST",
                "oidc/native/exchange",
                &[],
                json!({"code":handoff,"code_verifier":"x".repeat(43)}),
            )
            .await;
            (s, b)
        };
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let exchange = json!({"code":handoff,"code_verifier":verifier.secret()});
        let (status, _, native) =
            call(&app, "POST", "oidc/native/exchange", &[], exchange.clone()).await;
        assert_eq!(
            status,
            if expired {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::OK
            },
            "{native}"
        );
        if !expired {
            assert_eq!(native["account_id"], account);
        }
        assert_eq!(
            call(&app, "POST", "oidc/native/exchange", &[], exchange)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }

    let auto = App::new(store.clone())
        .await?
        .public_origin("https://atlas.example")?
        .oidc(&provider.issuer, "atlas-test", None, true)?
        .router();
    let (path, cookie, attempt_id, verifier) =
        begin(&auto, &provider, "new_subject", None, None).await;
    let (status, headers, _) = call(&auto, "GET", &path, &[("cookie", &cookie)], Value::Null).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(!headers["set-cookie"].to_str()?.contains("atlas_session"));
    let sent = fragment(headers["location"].to_str()?);
    assert_eq!(sent["atlas_attempt"], attempt_id);
    let (status, headers, activated) = call(
        &auto,
        "POST",
        "browser-sessions/activate",
        &[("origin", "https://atlas.example")],
        json!({"grant":sent["atlas_grant"],"verifier":verifier}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{activated}");
    assert_eq!(activated["account_id"], sent["atlas_account"]);
    let session_cookie = headers["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let (_, _, bootstrap) = call(
        &auto,
        "GET",
        "browser-sessions/current",
        &[("cookie", session_cookie), ("x-atlas-session", "1")],
        Value::Null,
    )
    .await;
    let auth = [
        ("cookie", session_cookie),
        ("x-csrf-token", bootstrap["csrf_token"].as_str().unwrap()),
    ];
    let (_, _, me) = call(&auto, "GET", "me", &auth, Value::Null).await;
    assert_eq!(me["has_password"], false);
    assert_ne!(me["id"], account);
    assert_eq!(
        call(
            &auto,
            "DELETE",
            "oidc/identities",
            &auth,
            json!({"issuer":provider.issuer})
        )
        .await
        .0,
        StatusCode::CONFLICT,
        "OIDC-only accounts must retain a login method"
    );
    let (_, _, fresh) = call(
        &app,
        "POST",
        "sessions",
        &[],
        json!({"username":"alice","password":"oidc-link-password","device_id":"unlink"}),
    )
    .await;
    let bearer = format!("Bearer {}", fresh["access_token"].as_str().unwrap());
    // An `issued` grant for the account is cancelled by unlink, same as any other account-wide
    // session revocation (BE-Q19 item 7).
    let (challenge, _) = PkceCodeChallenge::new_random_sha256();
    let (status, _, granted) = call(
        &app,
        "POST",
        "browser-sessions",
        &[("origin", "https://atlas.example")],
        json!({"username":"alice","password":"oidc-link-password","device_id":"unlink-grant","attempt_challenge":challenge.as_str()}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{granted}");
    let (status, headers, _) = call(
        &app,
        "DELETE",
        "oidc/identities",
        &[("authorization", &bearer)],
        json!({"issuer":provider.issuer}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // A self-revoking response never pushes a cookie decision made from state read earlier.
    assert!(!headers.contains_key("set-cookie"));
    let state: String = sqlx::query_scalar(
        "SELECT state FROM activation_grants WHERE account_id=$1 AND device_id='unlink-grant'",
    )
    .bind(&account)
    .fetch_one(&store.pool)
    .await?;
    assert_eq!(state, "cancelled", "unlink_oidc cancels an issued grant");
    assert_eq!(
        call(
            &app,
            "GET",
            "me",
            &[("authorization", &bearer)],
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM external_identities WHERE account_id=$1")
            .bind(&account)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(remaining, 0);
    server.abort();
    let _ = server.await;
    Ok(())
}

#[tokio::test]
async fn oidc_validates_tokens_binding_replay_and_explicit_linking() -> anyhow::Result<()> {
    scenario().await
}

/// `attempt_id` is caller-supplied and only length-checked (non-empty, <=64 bytes), so it may
/// contain reserved `application/x-www-form-urlencoded` bytes. Round-trip through the real start
/// and callback routes, not just a unit check on the encoder, for reserved characters and the
/// documented upper limit.
#[tokio::test]
async fn oidc_attempt_id_round_trips_reserved_characters() -> anyhow::Result<()> {
    let (provider, server) = crate::support::oidc::serve().await?;
    let (_dir, database) = crate::support::database::database_url().await?;
    let store = Store::connect(&database).await?;
    store.migrate().await?;
    let app = App::new(store.clone())
        .await?
        .public_origin("https://atlas.example")?
        .oidc(&provider.issuer, "atlas-test", None, true)?
        .router();
    for attempt_id in [
        "alpha&extra=beta+gamma",
        "100% done",
        "x".repeat(64).as_str(),
    ] {
        let (path, cookie, _, verifier) =
            begin(&app, &provider, "new_subject", None, Some(attempt_id)).await;
        let (status, headers, _) =
            call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let sent = fragment(headers["location"].to_str()?);
        assert_eq!(sent["atlas_attempt"], attempt_id, "{attempt_id:?}");
        let (status, _, activated) = call(
            &app,
            "POST",
            "browser-sessions/activate",
            &[("origin", "https://atlas.example")],
            json!({"grant":sent["atlas_grant"],"verifier":verifier}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{attempt_id:?}: {activated}");
    }
    server.abort();
    let _ = server.await;
    Ok(())
}
