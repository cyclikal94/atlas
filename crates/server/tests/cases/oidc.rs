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

/// BE-Q25: a session from the browser OIDC flow is labelled by the `activate` request that redeems
/// its grant (the cookie holder), not by the start or callback requests.
#[tokio::test]
async fn a_browser_oidc_session_is_labelled_by_its_activation() -> anyhow::Result<()> {
    let (provider, server) = crate::support::oidc::serve().await?;
    let (_dir, database) = crate::support::database::database_url().await?;
    let store = Store::connect(&database).await?;
    store.migrate().await?;
    let app = App::new(store.clone())
        .await?
        .public_origin("https://atlas.example")?
        .oidc(&provider.issuer, "atlas-test", None, true)?
        .router();
    let (path, cookie, _, verifier) = begin(&app, &provider, "new_subject", None, None).await;
    let (status, headers, _) = call(
        &app,
        "GET",
        &path,
        &[
            ("cookie", &cookie),
            (
                "user-agent",
                "Mozilla/5.0 (Windows NT 10.0) Chrome/129.0.0.0 Safari/537.36",
            ),
        ],
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let sent = fragment(headers["location"].to_str()?);
    let (status, headers, activated) = call(
        &app,
        "POST",
        "browser-sessions/activate",
        &[
            ("origin", "https://atlas.example"),
            (
                "user-agent",
                "Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0",
            ),
        ],
        json!({"grant":sent["atlas_grant"],"verifier":verifier}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{activated}");
    let cookie = headers["set-cookie"]
        .to_str()?
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let csrf = activated["csrf_token"].as_str().unwrap();
    let (status, _, sessions) = call(
        &app,
        "GET",
        "sessions",
        &[
            ("cookie", &cookie),
            ("x-csrf-token", csrf),
            ("origin", "https://atlas.example"),
        ],
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{sessions}");
    let session = &sessions["sessions"][0];
    assert_eq!(session["authentication"], "oidc");
    assert_eq!(
        session["client"],
        json!({"channel":"browser","browser":"firefox","platform":"linux"})
    );
    server.abort();
    let _ = server.await;
    Ok(())
}

/// BE-Q23 dependency compatibility: an ES256 or ES384 provider (signed by ring, verified by the
/// vendored openidconnect on the 0.14 curve stack) gets the same claim checks and linking flow as
/// the RS256 scenario, and a token signed by an unpublished key of the same curve is refused.
#[tokio::test]
async fn oidc_verifies_ec_signed_id_tokens() -> anyhow::Result<()> {
    use crate::support::oidc::{Signing, serve_signed};
    use openidconnect::core::CoreJwsSigningAlgorithm;
    for alg in [
        CoreJwsSigningAlgorithm::EcdsaP256Sha256,
        CoreJwsSigningAlgorithm::EcdsaP384Sha384,
    ] {
        let (provider, server) = serve_signed(Signing::ec(alg.clone())).await?;
        let (_dir, database) = crate::support::database::database_url().await?;
        let store = Store::connect(&database).await?;
        store.migrate().await?;
        let account = Uuid::new_v4().to_string();
        store
            .add_account(
                &account,
                "alice",
                &hash_password("oidc-ec-password".into()).await?,
            )
            .await?;
        let app = App::new(store.clone())
            .await?
            .public_origin("https://atlas.example")?
            .oidc(&provider.issuer, "atlas-test", None, false)?
            .router();
        let (_, _, session) = call(
            &app,
            "POST",
            "sessions",
            &[],
            json!({"username":"alice","password":"oidc-ec-password","device_id":"native"}),
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
            "wrong_key",
        ] {
            let (path, cookie, _, _) = begin(&app, &provider, mode, Some(bearer), None).await;
            assert_eq!(
                call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null)
                    .await
                    .0,
                StatusCode::UNAUTHORIZED,
                "{alg:?} {mode}"
            );
        }
        let (path, cookie, attempt_id, verifier) =
            begin(&app, &provider, "valid", Some(bearer), None).await;
        let (status, headers, _) =
            call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{alg:?}");
        let sent = fragment(headers["location"].to_str()?);
        assert_eq!(sent["atlas_account"], account, "{alg:?}");
        assert_eq!(sent["atlas_attempt"], attempt_id, "{alg:?}");
        let (status, _, activated) = call(
            &app,
            "POST",
            "browser-sessions/activate",
            &[("origin", "https://atlas.example")],
            json!({"grant":sent["atlas_grant"],"verifier":verifier}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{alg:?}: {activated}");
        assert_eq!(activated["account_id"], account, "{alg:?}");
        assert_eq!(
            call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED,
            "{alg:?}: the flow row is consumed exactly once"
        );
        server.abort();
        let _ = server.await;
    }
    Ok(())
}

/// Rows that carry or create sign-in authority, by table, for before/after comparisons.
async fn authority(store: &Store) -> anyhow::Result<Vec<(&'static str, i64)>> {
    let mut rows = Vec::new();
    for table in [
        "accounts",
        "sessions",
        "activation_grants",
        "native_handoffs",
        "external_identities",
        "device_profiles",
        "oidc_flows",
    ] {
        let count: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                .fetch_one(&store.pool)
                .await?;
        rows.push((table, count));
    }
    Ok(rows)
}

/// `before` with one browser flow consumed: the only effect a refused callback may have.
fn consumed_one(before: &[(&'static str, i64)]) -> Vec<(&'static str, i64)> {
    before
        .iter()
        .map(|(t, n)| (*t, if *t == "oidc_flows" { n - 1 } else { *n }))
        .collect()
}

/// BE-Q23 H1 (Review 12; authority BE-Q23-HMAC-TOKEN-SCOPE-20261103): a confidential-client
/// provider that signs ID tokens with HS256, HS384 or HS512 (ring, independently of the RustCrypto
/// verifier) reaches the real browser and native callbacks. Each malformed MAC (empty, one byte,
/// N - 1, N + 1, the other HS lengths), an altered or wrong-secret MAC, an unadvertised HS
/// algorithm and each claim failure is `401` without a panic. Its only durable effect is consuming
/// its own flow: no account, identity, grant, handoff, session or session cookie, the existing
/// session survives, and a replay is refused. The valid controls link, provision, activate and
/// exchange. A second callback for a flow whose exchange is held at the token endpoint is refused,
/// for a malformed and a valid MAC alike; a flow bound to another tab's cookie is not consumed.
#[tokio::test]
async fn oidc_hmac_callbacks_refuse_malformed_signatures() -> anyhow::Result<()> {
    use crate::support::oidc::{Signing, serve_signed};
    use openidconnect::core::CoreJwsSigningAlgorithm as A;
    const SECRET: &str = "atlas-test-only-confidential-client-secret-0123456789";
    for (alg, size, others) in [
        (A::HmacSha256, 32, ["HS384", "HS512"]),
        (A::HmacSha384, 48, ["HS256", "HS512"]),
        (A::HmacSha512, 64, ["HS256", "HS384"]),
    ] {
        let (provider, server) = serve_signed(Signing::hmac(alg.clone(), SECRET)).await?;
        let (_dir, database) = crate::support::database::database_url().await?;
        let store = Store::connect(&database).await?;
        store.migrate().await?;
        let account = Uuid::new_v4().to_string();
        store
            .add_account(
                &account,
                "alice",
                &hash_password("oidc-hmac-password".into()).await?,
            )
            .await?;
        let app = App::new(store.clone())
            .await?
            .public_origin("https://atlas.example")?
            .oidc(&provider.issuer, "atlas-test", Some(SECRET.into()), false)?
            .oidc_native_redirects(vec!["dev.atlas.app:/oauth/callback".into()])?
            .router();
        let (_, _, session) = call(
            &app,
            "POST",
            "sessions",
            &[],
            json!({"username":"alice","password":"oidc-hmac-password","device_id":"native"}),
        )
        .await;
        let bearer = session["access_token"].as_str().unwrap().to_owned();
        let me = |app: Router, bearer: String| async move {
            call(
                &app,
                "GET",
                "me",
                &[("authorization", &format!("Bearer {bearer}"))],
                Value::Null,
            )
            .await
            .0
        };
        let mut refusals: Vec<String> = [0, 1, size - 1, size + 1, 32, 48, 64]
            .into_iter()
            .filter(|n| *n != size)
            .map(|n| format!("mac_len:{n}"))
            .collect();
        refusals.extend(
            [
                "signature",
                "wrong_key",
                "nonce",
                "audience",
                "issuer",
                "expired",
                // Open finding H1-AT (characterised, not accepted): the callback resolves the
                // `at_hash` key from the published JWKS, which never holds a shared secret, so a
                // valid HS token that carries `at_hash` is refused. Recorded for decision.
                "valid_at_hash",
            ]
            .map(String::from),
        );
        refusals.extend(others.map(|o| format!("alg:{o}")));
        for mode in &refusals {
            let (path, cookie, _, _) = begin(&app, &provider, mode, Some(&bearer), None).await;
            let before = authority(&store).await?;
            let issued = provider.issued();
            let (status, headers, _) =
                call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{alg:?} {mode}");
            assert_eq!(
                provider.issued(),
                issued + 1,
                "{alg:?} {mode}: refused after the provider returned its ID token"
            );
            assert!(!headers.contains_key("location"), "{alg:?} {mode}");
            assert!(
                headers
                    .get_all("set-cookie")
                    .iter()
                    .all(|c| !c.to_str().unwrap().contains("atlas_session")),
                "{alg:?} {mode}: no session cookie"
            );
            assert_eq!(
                authority(&store).await?,
                consumed_one(&before),
                "{alg:?} {mode}: only the flow is consumed"
            );
            assert_eq!(
                call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null)
                    .await
                    .0,
                StatusCode::UNAUTHORIZED,
                "{alg:?} {mode}: a replay is refused"
            );
            assert_eq!(
                me(app.clone(), bearer.clone()).await,
                StatusCode::OK,
                "{alg:?} {mode}: the existing session survives"
            );
        }

        // A second callback while the first one's exchange is held: refused at once, before any
        // network I/O; the held one then completes with its own outcome.
        for (mode, expected) in [
            (
                format!("hold+mac_len:{}", size - 1),
                StatusCode::UNAUTHORIZED,
            ),
            ("hold+valid".to_owned(), StatusCode::SEE_OTHER),
        ] {
            let (path, cookie, _, _) = begin(&app, &provider, &mode, Some(&bearer), None).await;
            let before = authority(&store).await?;
            let held = tokio::spawn({
                let (app, path, cookie) = (app.clone(), path.clone(), cookie.clone());
                async move { call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null).await }
            });
            provider.arrived.notified().await;
            assert_eq!(
                call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null)
                    .await
                    .0,
                StatusCode::UNAUTHORIZED,
                "{alg:?} {mode}: the second callback finds the flow consumed"
            );
            assert_eq!(authority(&store).await?, consumed_one(&before));
            provider.release.notify_one();
            let (status, headers, _) = held.await?;
            assert_eq!(status, expected, "{alg:?} {mode}");
            // The valid one links the subject (its first link) and issues one grant.
            let linked = status == StatusCode::SEE_OTHER;
            let expected_rows = consumed_one(&before)
                .iter()
                .map(|(t, n)| {
                    let added = matches!(*t, "activation_grants" | "external_identities");
                    (*t, *n + i64::from(linked && added))
                })
                .collect::<Vec<_>>();
            assert_eq!(authority(&store).await?, expected_rows, "{alg:?} {mode}");
            if status == StatusCode::SEE_OTHER {
                assert_eq!(
                    fragment(headers["location"].to_str()?)["atlas_account"],
                    account
                );
            }
        }

        // Two tabs: a later flow replaces the binding cookie; the earlier flow's callback with it
        // is refused and consumes nothing. The earlier tab's own cookie still completes its flow.
        let (path_a, cookie_a, attempt_a, verifier_a) =
            begin(&app, &provider, "valid", Some(&bearer), None).await;
        let (path_b, cookie_b, _, _) = begin(
            &app,
            &provider,
            &format!("mac_len:{}", size + 1),
            Some(&bearer),
            None,
        )
        .await;
        let before = authority(&store).await?;
        assert_eq!(
            call(&app, "GET", &path_a, &[("cookie", &cookie_b)], Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            authority(&store).await?,
            before,
            "{alg:?}: nothing consumed"
        );
        assert_eq!(
            call(&app, "GET", &path_b, &[("cookie", &cookie_b)], Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        let (status, headers, _) =
            call(&app, "GET", &path_a, &[("cookie", &cookie_a)], Value::Null).await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "{alg:?}: healthy link control"
        );
        let sent = fragment(headers["location"].to_str()?);
        assert_eq!(sent["atlas_account"], account);
        assert_eq!(sent["atlas_attempt"], attempt_a);
        let (status, headers, activated) = call(
            &app,
            "POST",
            "browser-sessions/activate",
            &[("origin", "https://atlas.example")],
            json!({"grant":sent["atlas_grant"],"verifier":verifier_a}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{alg:?}: {activated}");
        assert_eq!(activated["account_id"], account);
        assert!(headers["set-cookie"].to_str()?.contains("atlas_session"));
        let mapped: String = sqlx::query_scalar("SELECT account_id FROM external_identities")
            .fetch_one(&store.pool)
            .await?;
        assert_eq!(mapped, account, "{alg:?}");

        // Native: a malformed MAC creates no handoff; the valid control exchanges once.
        for mode in [format!("mac_len:{}", size - 1), "valid".to_owned()] {
            let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
            let mut start = url::Url::parse("https://atlas.example/oidc/native/start")?;
            start
                .query_pairs_mut()
                .append_pair("device_id", "native-oidc")
                .append_pair("redirect_uri", "dev.atlas.app:/oauth/callback")
                .append_pair("code_challenge", challenge.as_str())
                .append_pair("state", "client-state");
            let path = format!("oidc/native/start?{}", start.query().unwrap());
            let (status, headers, _) = call(&app, "GET", &path, &[], Value::Null).await;
            assert_eq!(status, StatusCode::SEE_OTHER);
            let code = provider.approve(headers["location"].to_str()?, &mode);
            let auth = url::Url::parse(headers["location"].to_str()?)?;
            let state = auth
                .query_pairs()
                .find(|(k, _)| k == "state")
                .unwrap()
                .1
                .into_owned();
            let cookie = headers["set-cookie"]
                .to_str()?
                .split(';')
                .next()
                .unwrap()
                .to_owned();
            let before = authority(&store).await?;
            let issued = provider.issued();
            let (status, headers, _) = call(
                &app,
                "GET",
                &format!("oidc/callback?state={state}&code={code}"),
                &[("cookie", &cookie)],
                Value::Null,
            )
            .await;
            assert_eq!(provider.issued(), issued + 1, "{alg:?} native {mode}");
            if mode != "valid" {
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{alg:?} native {mode}");
                assert_eq!(authority(&store).await?, consumed_one(&before));
                continue;
            }
            assert_eq!(status, StatusCode::SEE_OTHER, "{alg:?} native");
            let destination = url::Url::parse(headers["location"].to_str()?)?;
            let result: HashMap<_, _> = destination
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            let exchange = json!({"code":result["code"],"code_verifier":verifier.secret()});
            let (status, _, native) =
                call(&app, "POST", "oidc/native/exchange", &[], exchange.clone()).await;
            assert_eq!(status, StatusCode::OK, "{alg:?}: {native}");
            assert_eq!(native["account_id"], account);
            assert_eq!(
                call(&app, "POST", "oidc/native/exchange", &[], exchange)
                    .await
                    .0,
                StatusCode::UNAUTHORIZED
            );
        }

        // Provisioning: a malformed MAC for an unknown subject creates no account; the valid
        // control provisions one.
        let auto = App::new(store.clone())
            .await?
            .public_origin("https://atlas.example")?
            .oidc(&provider.issuer, "atlas-test", Some(SECRET.into()), true)?
            .router();
        let (path, cookie, _, _) =
            begin(&auto, &provider, "mac_len:0+new_subject", None, None).await;
        let before = authority(&store).await?;
        let issued = provider.issued();
        assert_eq!(
            call(&auto, "GET", &path, &[("cookie", &cookie)], Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(provider.issued(), issued + 1, "{alg:?}: provisioning");
        assert_eq!(authority(&store).await?, consumed_one(&before));
        let (path, cookie, _, verifier) = begin(&auto, &provider, "new_subject", None, None).await;
        let (status, headers, _) =
            call(&auto, "GET", &path, &[("cookie", &cookie)], Value::Null).await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "{alg:?}: provisioning control"
        );
        let sent = fragment(headers["location"].to_str()?);
        assert_ne!(sent["atlas_account"], account);
        let (status, _, activated) = call(
            &auto,
            "POST",
            "browser-sessions/activate",
            &[("origin", "https://atlas.example")],
            json!({"grant":sent["atlas_grant"],"verifier":verifier}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{alg:?}: {activated}");

        // A public client (no secret) refuses even a valid shared-secret signature.
        let public = App::new(store.clone())
            .await?
            .public_origin("https://atlas.example")?
            .oidc(&provider.issuer, "atlas-test", None, true)?
            .router();
        let (path, cookie, _, _) = begin(&public, &provider, "new_subject", None, None).await;
        let before = authority(&store).await?;
        let issued = provider.issued();
        assert_eq!(
            call(&public, "GET", &path, &[("cookie", &cookie)], Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED,
            "{alg:?}: public client"
        );
        assert_eq!(provider.issued(), issued + 1, "{alg:?}: public client");
        assert_eq!(authority(&store).await?, consumed_one(&before));
        server.abort();
        let _ = server.await;
    }
    Ok(())
}

/// Signs a JWS signing input with an independent (ring) implementation.
type Signer = Box<dyn Fn(&[u8]) -> Vec<u8>>;

/// One retained JWS algorithm for the signature matrix: the verification key, a same-type key that
/// must not verify, and a signer from ring (independent of the RustCrypto code openidconnect uses).
struct Jws {
    alg: openidconnect::core::CoreJwsSigningAlgorithm,
    key: openidconnect::core::CoreJsonWebKey,
    other: openidconnect::core::CoreJsonWebKey,
    sign: Signer,
}

fn jwk(value: Value) -> openidconnect::core::CoreJsonWebKey {
    serde_json::from_value(value).unwrap()
}

fn b64(bytes: &[u8]) -> String {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Every algorithm openidconnect 4.0.1 verifies, each with an independent ring signer.
fn retained_algorithms() -> Vec<Jws> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use openidconnect::{
        JsonWebKeyId, PrivateSigningKey,
        core::{CoreJwsSigningAlgorithm as A, CoreRsaPrivateSigningKey},
    };
    use ring::{hmac, rand::SystemRandom, signature as sig, signature::KeyPair};
    use std::sync::Arc;
    let pem = include_str!("../fixtures/oidc-test-only.pem");
    let rsa_jwk = serde_json::to_value(
        CoreRsaPrivateSigningKey::from_pem(pem, Some(JsonWebKeyId::new("test".into())))
            .unwrap()
            .as_verification_key(),
    )
    .unwrap();
    // A same-size modulus that is not the signing key's.
    let mut other_rsa = rsa_jwk.clone();
    let mut n = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(rsa_jwk["n"].as_str().unwrap())
        .unwrap();
    let middle = n.len() / 2;
    n[middle] ^= 0x10;
    other_rsa["n"] = json!(b64(&n));
    let der: Vec<u8> = STANDARD
        .decode(
            pem.lines()
                .filter(|l| !l.starts_with("-----"))
                .collect::<String>(),
        )
        .unwrap();
    let rsa = Arc::new(sig::RsaKeyPair::from_der(&der).unwrap());
    let rsa_signer = |encoding: &'static dyn sig::RsaEncoding| -> Signer {
        let rsa = rsa.clone();
        Box::new(move |message: &[u8]| {
            let mut out = vec![0; rsa.public().modulus_len()];
            rsa.sign(encoding, &SystemRandom::new(), message, &mut out)
                .unwrap();
            out
        })
    };
    let ec = |signing: &'static sig::EcdsaSigningAlgorithm, crv: &str| -> (Value, Signer) {
        let rng = SystemRandom::new();
        let pkcs8 = sig::EcdsaKeyPair::generate_pkcs8(signing, &rng).unwrap();
        let pair = sig::EcdsaKeyPair::from_pkcs8(signing, pkcs8.as_ref(), &rng).unwrap();
        let point = pair.public_key().as_ref().to_vec();
        let half = (point.len() - 1) / 2;
        (
            json!({"kty":"EC","use":"sig","crv":crv,"x":b64(&point[1..1 + half]),"y":b64(&point[1 + half..])}),
            Box::new(move |message: &[u8]| {
                pair.sign(&SystemRandom::new(), message)
                    .unwrap()
                    .as_ref()
                    .to_vec()
            }),
        )
    };
    let ed = || -> (Value, Signer) {
        let pkcs8 = sig::Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = sig::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        (
            json!({"kty":"OKP","use":"sig","crv":"Ed25519","x":b64(pair.public_key().as_ref())}),
            Box::new(move |message: &[u8]| pair.sign(message).as_ref().to_vec()),
        )
    };
    let mac = |algorithm: hmac::Algorithm, secret: &[u8]| -> (Value, Signer) {
        let key = hmac::Key::new(algorithm, secret);
        (
            json!({"kty":"oct","use":"sig","k":b64(secret)}),
            Box::new(move |message: &[u8]| hmac::sign(&key, message).as_ref().to_vec()),
        )
    };
    let mut cases = vec![
        (
            A::RsaSsaPkcs1V15Sha256,
            &sig::RSA_PKCS1_SHA256 as &'static dyn sig::RsaEncoding,
        ),
        (A::RsaSsaPkcs1V15Sha384, &sig::RSA_PKCS1_SHA384),
        (A::RsaSsaPkcs1V15Sha512, &sig::RSA_PKCS1_SHA512),
        (A::RsaSsaPssSha256, &sig::RSA_PSS_SHA256),
        (A::RsaSsaPssSha384, &sig::RSA_PSS_SHA384),
        (A::RsaSsaPssSha512, &sig::RSA_PSS_SHA512),
    ]
    .into_iter()
    .map(|(alg, encoding)| Jws {
        alg,
        key: jwk(rsa_jwk.clone()),
        other: jwk(other_rsa.clone()),
        sign: rsa_signer(encoding),
    })
    .collect::<Vec<_>>();
    for (alg, signing, crv) in [
        (
            A::EcdsaP256Sha256,
            &sig::ECDSA_P256_SHA256_FIXED_SIGNING,
            "P-256",
        ),
        (
            A::EcdsaP384Sha384,
            &sig::ECDSA_P384_SHA384_FIXED_SIGNING,
            "P-384",
        ),
    ] {
        let (key, sign) = ec(signing, crv);
        let (other, _) = ec(signing, crv);
        cases.push(Jws {
            alg,
            key: jwk(key),
            other: jwk(other),
            sign,
        });
    }
    let (key, sign) = ed();
    let (other, _) = ed();
    cases.push(Jws {
        alg: A::EdDsa,
        key: jwk(key),
        other: jwk(other),
        sign,
    });
    for (alg, algorithm, size) in [
        (A::HmacSha256, hmac::HMAC_SHA256, 32),
        (A::HmacSha384, hmac::HMAC_SHA384, 48),
        (A::HmacSha512, hmac::HMAC_SHA512, 64),
    ] {
        let (key, sign) = mac(algorithm, &vec![7; size]);
        let (other, _) = mac(algorithm, &vec![8; size]);
        cases.push(Jws {
            alg,
            key: jwk(key),
            other: jwk(other),
            sign,
        });
    }
    cases
}

#[derive(Debug, PartialEq)]
enum Verified {
    Accepted,
    Refused,
    Panicked,
}

fn verify(
    key: &openidconnect::core::CoreJsonWebKey,
    alg: &openidconnect::core::CoreJwsSigningAlgorithm,
    message: &[u8],
    signature: &[u8],
) -> Verified {
    use openidconnect::JsonWebKey;
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        key.verify_signature(alg, message, signature)
    })) {
        Ok(Ok(())) => Verified::Accepted,
        Ok(Err(_)) => Verified::Refused,
        Err(_) => Verified::Panicked,
    }
}

/// Every malformed or mismatched signature is refused with an ordinary verification error.
///
/// BE-Q23 H1 (Review 12, 3 November 2026): upstream openidconnect 4.0.1 converted an HMAC
/// signature with `GenericArray::from(&[u8])`, which panicked on a wrong length before its error
/// mapping, and this matrix used to expect `Panicked` there as characterisation. The vendored copy
/// now verifies with `verify_slice` under authority BE-Q23-HMAC-TOKEN-SCOPE-20261103, so any panic
/// is a failure here; the historical panic evidence is kept under `.agtx/evidence/q23`.
fn expected_refusal() -> Verified {
    Verified::Refused
}

/// BE-Q23 dependency compatibility: every retained algorithm accepts its own signature and refuses
/// a changed message, a changed, truncated, one-byte, overlong or empty signature, a different key,
/// and every other algorithm (including `none` and the unsupported ES512) for the same key and
/// signature. None of them may panic (H1).
#[test]
fn oidc_signature_verification_matrix() {
    use openidconnect::core::CoreJwsSigningAlgorithm as A;
    let message = b"eyJhbGciOiJ0ZXN0In0.eyJzdWIiOiJhdGxhcyJ9";
    let cases = retained_algorithms();
    assert_eq!(cases.len(), 12);
    let mut algs: Vec<A> = cases.iter().map(|c| c.alg.clone()).collect();
    algs.extend([A::EcdsaP521Sha512, A::None]);
    for case in &cases {
        let alg = &case.alg;
        let signature = (case.sign)(message);
        assert_eq!(
            verify(&case.key, alg, message, &signature),
            Verified::Accepted,
            "{alg:?} must accept its own signature"
        );
        let mut changed = message.to_vec();
        changed[5] ^= 1;
        let mut flipped = signature.clone();
        let middle = flipped.len() / 2;
        flipped[middle] ^= 1;
        let truncated = &signature[..signature.len() - 1];
        let mut overlong = signature.clone();
        overlong.push(signature[0]);
        for (label, key, alg, message, signature) in [
            ("message", &case.key, alg, &changed[..], &signature[..]),
            ("signature", &case.key, alg, &message[..], &flipped[..]),
            ("truncated", &case.key, alg, &message[..], truncated),
            ("one byte", &case.key, alg, &message[..], &signature[..1]),
            ("overlong", &case.key, alg, &message[..], &overlong[..]),
            ("empty", &case.key, alg, &message[..], &[][..]),
            ("other key", &case.other, alg, &message[..], &signature[..]),
        ] {
            assert_eq!(
                verify(key, alg, message, signature),
                expected_refusal(),
                "{alg:?} {label}"
            );
        }
        for mismatch in algs.iter().filter(|a| *a != alg) {
            assert_eq!(
                verify(&case.key, mismatch, message, &signature),
                expected_refusal(),
                "{alg:?} key and signature verified as {mismatch:?}"
            );
        }
    }
}

/// BE-Q23 H1: fixed RFC 4231 test case 2 MACs (key `Jefe`), computed independently of both
/// RustCrypto and ring, are accepted by HS256/384/512 at their exact length. A same-length
/// alteration, and every signature of length 0, 1, N - 1 and N + 1, is refused without a panic.
#[test]
fn oidc_hmac_known_answers_and_malformed_lengths() {
    use openidconnect::core::CoreJwsSigningAlgorithm as A;
    let message = b"what do ya want for nothing?";
    let key = jwk(json!({"kty":"oct","use":"sig","k":b64(b"Jefe")}));
    for (alg, hex) in [
        (
            A::HmacSha256,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
        ),
        (
            A::HmacSha384,
            "af45d2e376484031617f78d2b58a6b1b9c7ef464f5a01b47e42ec3736322445e\
             8e2240ca5e69e2c78b3239ecfab21649",
        ),
        (
            A::HmacSha512,
            "164b7a7bfcf819e2e395fbe73b56e0a387bd64222e831fd610270cd7ea250554\
             9758bf75c05a994a6d034f65f8f0e6fdcaeab1a34d4a6b4b636e070a38bce737",
        ),
    ] {
        let mac: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let n = mac.len();
        assert_eq!(
            verify(&key, &alg, message, &mac),
            Verified::Accepted,
            "{alg:?}"
        );
        let mut altered = mac.clone();
        altered[n - 1] ^= 1;
        assert_eq!(
            verify(&key, &alg, message, &altered),
            Verified::Refused,
            "{alg:?} altered"
        );
        let mut extended = mac.clone();
        extended.push(0);
        for (label, signature) in [
            ("empty", &[][..]),
            ("one byte", &mac[..1]),
            ("short", &mac[..n - 1]),
            ("overlong", &extended[..]),
        ] {
            assert_eq!(
                verify(&key, &alg, message, signature),
                Verified::Refused,
                "{alg:?} {label} ({} bytes)",
                signature.len()
            );
        }
    }
}

/// BE-Q23 dependency compatibility: malformed EC verification keys are refused rather than
/// accepted or panicking, while the unaltered key still verifies.
#[test]
fn oidc_rejects_malformed_ec_keys() {
    use openidconnect::{JsonWebKey, core::CoreJwsSigningAlgorithm as A};
    let message = b"header.payload";
    for case in retained_algorithms()
        .into_iter()
        .filter(|c| matches!(c.alg, A::EcdsaP256Sha256 | A::EcdsaP384Sha384))
    {
        let alg = &case.alg;
        let signature = (case.sign)(message);
        let good = serde_json::to_value(&case.key).unwrap();
        assert!(
            case.key.verify_signature(alg, message, &signature).is_ok(),
            "{alg:?}"
        );
        let coordinate = |name: &str| {
            base64::Engine::decode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                good[name].as_str().unwrap(),
            )
            .unwrap()
        };
        let mut off_curve = good.clone();
        let mut y = coordinate("y");
        *y.last_mut().unwrap() ^= 1;
        off_curve["y"] = json!(b64(&y));
        let mut short = good.clone();
        short["x"] = json!(b64(&coordinate("x")[1..]));
        let mut zero = good.clone();
        zero["x"] = json!(b64(&vec![0; coordinate("x").len()]));
        zero["y"] = json!(b64(&vec![0; coordinate("y").len()]));
        let mut wrong_curve = good.clone();
        wrong_curve["crv"] = json!(if *alg == A::EcdsaP256Sha256 {
            "P-384"
        } else {
            "P-256"
        });
        let mut p521 = good.clone();
        p521["crv"] = json!("P-521");
        for (label, key) in [
            ("off-curve", off_curve),
            ("short", short),
            ("zero", zero),
            ("wrong curve", wrong_curve),
            ("P-521", p521),
        ] {
            let key = jwk(key);
            assert!(
                key.verify_signature(alg, message, &signature).is_err(),
                "{alg:?} {label}"
            );
        }
    }
}

/// BE-Q23 dependency compatibility: `at_hash` for every retained algorithm is the left half of the
/// algorithm's digest of the access token (OIDC Core 3.1.3.6), computed independently with ring;
/// PKCE S256 matches RFC 7636 Appendix B.
#[test]
fn oidc_access_token_hash_and_pkce_known_answers() {
    use openidconnect::{
        AccessToken, AccessTokenHash, PkceCodeChallenge, PkceCodeVerifier,
        core::CoreJwsSigningAlgorithm as A,
    };
    use ring::digest;
    let token = AccessToken::new("test-access".into());
    for case in retained_algorithms() {
        let algorithm = match case.alg {
            A::RsaSsaPkcs1V15Sha256 | A::RsaSsaPssSha256 | A::EcdsaP256Sha256 | A::HmacSha256 => {
                &digest::SHA256
            }
            A::RsaSsaPkcs1V15Sha384 | A::RsaSsaPssSha384 | A::EcdsaP384Sha384 | A::HmacSha384 => {
                &digest::SHA384
            }
            _ => &digest::SHA512,
        };
        let full = digest::digest(algorithm, b"test-access");
        let expected = b64(&full.as_ref()[..full.as_ref().len() / 2]);
        let actual = AccessTokenHash::from_token(&token, &case.alg, &case.key).unwrap();
        assert_eq!(actual.to_string(), expected, "{:?}", case.alg);
    }
    assert_eq!(
        PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into()
        ))
        .as_str(),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}
