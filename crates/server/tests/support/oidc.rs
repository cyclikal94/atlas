//! A fake OpenID provider on a loopback listener, shared by the OIDC scenario and by the
//! retirement-coordination tests that drive the real callback and native exchange routes.
use atlas_server::now;
use axum::{
    Form, Json, Router,
    extract::State,
    http::StatusCode,
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
use tokio::task::JoinHandle;
use uuid::Uuid;

// Public fixture, generated solely for these tests. Never a deployment key.
fn key() -> CoreRsaPrivateSigningKey {
    CoreRsaPrivateSigningKey::from_pem(
        include_str!("../fixtures/oidc-test-only.pem"),
        Some(JsonWebKeyId::new("test".into())),
    )
    .unwrap()
}

/// Authorisation codes the provider will redeem: code -> (nonce, PKCE challenge, mode).
pub(crate) type PendingCodes = Arc<Mutex<HashMap<String, (String, String, String)>>>;

#[derive(Clone)]
pub(crate) struct Provider {
    pub(crate) issuer: String,
    pub(crate) pending: PendingCodes,
}

impl Provider {
    /// Approve the authorisation request at `location` (the provider URL a start route redirected
    /// to) in `mode`, and return the code the provider will now redeem for it.
    pub(crate) fn approve(&self, location: &str, mode: &str) -> String {
        let url = url::Url::parse(location).unwrap();
        let params: HashMap<_, _> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let code = Uuid::new_v4().to_string();
        self.pending.lock().unwrap().insert(
            code.clone(),
            (
                params["nonce"].clone(),
                params["code_challenge"].clone(),
                mode.into(),
            ),
        );
        code
    }
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

/// Start the provider on an ephemeral loopback port. Abort the handle to stop it.
pub(crate) async fn serve() -> anyhow::Result<(Provider, JoinHandle<()>)> {
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
    Ok((provider, server))
}
