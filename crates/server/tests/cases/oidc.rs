use atlas_core::Store;
use atlas_server::{App, hash_password, now};
use axum::{
    Form, Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    routing::{get, post},
};
use openidconnect::{
    AccessToken, Audience, EmptyAdditionalClaims, IssuerUrl, JsonWebKeyId, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, PrivateSigningKey, StandardClaims, SubjectIdentifier,
    core::{CoreIdToken, CoreIdTokenClaims, CoreJwsSigningAlgorithm, CoreRsaPrivateSigningKey},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;
use uuid::Uuid;

// Public fixture, generated solely for these tests. Never a deployment key.
fn key() -> CoreRsaPrivateSigningKey {
    CoreRsaPrivateSigningKey::from_pem(
        include_str!("../fixtures/oidc-test-only.pem"),
        Some(JsonWebKeyId::new("test".into())),
    )
    .unwrap()
}
type PendingCodes = Arc<Mutex<HashMap<String, (String, String, String)>>>;

#[derive(Clone)]
struct Provider {
    issuer: String,
    pending: PendingCodes,
}
async fn token(
    State(provider): State<Provider>,
    Form(form): Form<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let Some((nonce, challenge, mode)) = form
        .get("code")
        .and_then(|code| provider.pending.lock().unwrap().remove(code))
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_grant"})),
        );
    };
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["client_id"], "atlas-test");
    assert_eq!(
        form["redirect_uri"],
        "https://atlas.example/api/experimental/v1/oidc/callback"
    );
    assert_eq!(
        PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(
            form["code_verifier"].clone()
        ))
        .as_str(),
        challenge
    );
    let instant = chrono::DateTime::from_timestamp(now(), 0).unwrap();
    let access = AccessToken::new("test-access".into());
    let claims = CoreIdTokenClaims::new(
        IssuerUrl::new(if mode == "issuer" {
            "https://wrong.example".into()
        } else {
            provider.issuer
        })
        .unwrap(),
        vec![Audience::new(
            if mode == "audience" {
                "wrong"
            } else {
                "atlas-test"
            }
            .into(),
        )],
        instant + chrono::Duration::seconds(if mode == "expired" { -60 } else { 300 }),
        instant,
        StandardClaims::new(SubjectIdentifier::new(
            if mode == "new_subject" {
                "new-subject"
            } else {
                "provider-subject"
            }
            .into(),
        )),
        EmptyAdditionalClaims {},
    )
    .set_nonce(Some(Nonce::new(if mode == "nonce" {
        "wrong".into()
    } else {
        nonce
    })));
    let signed = CoreIdToken::new(
        claims,
        &key(),
        CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
        Some(&access),
        None,
    )
    .unwrap();
    let mut token = signed.to_string();
    if mode == "signature" {
        let last = token.len() - 8;
        token.replace_range(
            last..last + 1,
            if &token[last..last + 1] == "A" {
                "B"
            } else {
                "A"
            },
        );
    }
    (
        StatusCode::OK,
        Json(
            json!({"access_token":if mode=="access_hash" {"swapped"} else {"test-access"}, "token_type":"Bearer", "id_token":token}),
        ),
    )
}
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
async fn begin(
    app: &Router,
    provider: &Provider,
    mode: &str,
    link: Option<&str>,
) -> (String, String) {
    let mut headers = vec![("origin", "https://atlas.example")];
    let auth = link.map(|t| format!("Bearer {t}"));
    if let Some(auth) = &auth {
        headers.push(("authorization", auth));
    }
    let (status, headers, body) = call(
        app,
        "POST",
        "oidc/start",
        &headers,
        json!({"device_id":"browser","link":link.is_some()}),
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
    )
}

async fn scenario() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let provider = Provider {
        issuer: format!("http://{}", listener.local_addr()?),
        pending: Arc::new(Mutex::new(HashMap::new())),
    };
    let metadata = json!({"issuer":provider.issuer,"authorization_endpoint":format!("{}/authorize",provider.issuer),"token_endpoint":format!("{}/token",provider.issuer),"jwks_uri":format!("{}/jwks",provider.issuer),"response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"]});
    let jwks = json!({"keys":[key().as_verification_key()]});
    let routes = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || async move { Json(metadata) }),
        )
        .route("/jwks", get(move || async move { Json(jwks) }))
        .route("/token", post(token))
        .with_state(provider.clone());
    let server = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
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
        let (path, cookie) = begin(&app, &provider, mode, Some(bearer)).await;
        assert_eq!(
            call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED,
            "{mode}"
        );
    }
    let (path, cookie) = begin(&app, &provider, "valid", None).await;
    assert_eq!(
        call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN,
        "unknown subjects must not auto-link"
    );
    let (path, cookie) = begin(&app, &provider, "valid", Some(bearer)).await;
    assert_eq!(
        call(&app, "GET", &path, &[], Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, headers, _) = call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "https://atlas.example");
    assert_eq!(
        call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let mapped: String = sqlx::query_scalar("SELECT account_id FROM external_identities")
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(mapped, account);
    let (path, cookie) = begin(&app, &provider, "valid", None).await;
    assert_eq!(
        call(&app, "GET", &path, &[("cookie", &cookie)], Value::Null)
            .await
            .0,
        StatusCode::SEE_OTHER
    );
    let (path, cookie) = begin(&app, &provider, "valid", Some(bearer)).await;
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
    let (path, cookie) = begin(&auto, &provider, "new_subject", None).await;
    let (status, headers, _) = call(&auto, "GET", &path, &[("cookie", &cookie)], Value::Null).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let session_cookie = headers
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|v| v.starts_with("__Host-atlas_session="))
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
    assert_eq!(
        call(
            &app,
            "DELETE",
            "oidc/identities",
            &[("authorization", &bearer)],
            json!({"issuer":provider.issuer})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
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
