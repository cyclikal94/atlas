//! A fake OpenID provider on a loopback listener, shared by the OIDC scenario and by the
//! retirement-coordination tests that drive the real callback and native exchange routes.
use atlas_server::now;
use axum::{
    Form, Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use openidconnect::{
    AccessToken, Audience, EmptyAdditionalClaims, IssuerUrl, JsonWebKeyId, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, PrivateSigningKey, SigningError, StandardClaims,
    SubjectIdentifier,
    core::{
        CoreIdToken, CoreIdTokenClaims, CoreJsonWebKey, CoreJwsSigningAlgorithm,
        CoreRsaPrivateSigningKey,
    },
};
use ring::{
    rand::SystemRandom,
    signature::{
        ECDSA_P256_SHA256_FIXED_SIGNING, ECDSA_P384_SHA384_FIXED_SIGNING, EcdsaKeyPair, KeyPair,
    },
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

/// A test-only ECDSA signer backed by ring, so the provider's EC signatures come from a different
/// implementation than the RustCrypto verifier inside openidconnect.
pub(crate) struct EcKey {
    pair: EcdsaKeyPair,
    alg: CoreJwsSigningAlgorithm,
    kid: String,
}

impl EcKey {
    /// A fresh disposable key for `alg` (ES256 or ES384).
    pub(crate) fn generate(alg: CoreJwsSigningAlgorithm, kid: &str) -> Self {
        let signing = match alg {
            CoreJwsSigningAlgorithm::EcdsaP256Sha256 => &ECDSA_P256_SHA256_FIXED_SIGNING,
            CoreJwsSigningAlgorithm::EcdsaP384Sha384 => &ECDSA_P384_SHA384_FIXED_SIGNING,
            _ => panic!("not an EC test algorithm: {alg:?}"),
        };
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(signing, &rng).unwrap();
        Self {
            pair: EcdsaKeyPair::from_pkcs8(signing, pkcs8.as_ref(), &rng).unwrap(),
            alg,
            kid: kid.into(),
        }
    }

    /// The public point as a JWK, built from ring's uncompressed SEC1 bytes.
    pub(crate) fn jwk(&self) -> Value {
        let point = self.pair.public_key().as_ref();
        let half = (point.len() - 1) / 2;
        let crv = match self.alg {
            CoreJwsSigningAlgorithm::EcdsaP256Sha256 => "P-256",
            _ => "P-384",
        };
        json!({"kty":"EC","use":"sig","crv":crv,"kid":self.kid,
            "x":URL_SAFE_NO_PAD.encode(&point[1..1 + half]),
            "y":URL_SAFE_NO_PAD.encode(&point[1 + half..])})
    }
}

impl PrivateSigningKey for EcKey {
    type VerificationKey = CoreJsonWebKey;

    fn sign(
        &self,
        signature_alg: &CoreJwsSigningAlgorithm,
        message: &[u8],
    ) -> Result<Vec<u8>, SigningError> {
        if *signature_alg != self.alg {
            return Err(SigningError::UnsupportedAlg(format!("{signature_alg:?}")));
        }
        let signature = self
            .pair
            .sign(&SystemRandom::new(), message)
            .map_err(|_| SigningError::CryptoError)?;
        Ok(signature.as_ref().to_vec())
    }

    fn as_verification_key(&self) -> CoreJsonWebKey {
        serde_json::from_value(self.jwk()).unwrap()
    }
}

/// How the provider signs ID tokens: the RS256 fixture key, or a fresh ECDSA key. An EC provider
/// also holds a second, unpublished key of the same curve for the `wrong_key` mode.
pub(crate) enum Signing {
    Rs256,
    Ec {
        published: Box<EcKey>,
        unpublished: Box<EcKey>,
    },
}

impl Signing {
    pub(crate) fn ec(alg: CoreJwsSigningAlgorithm) -> Self {
        Signing::Ec {
            published: Box::new(EcKey::generate(alg.clone(), "test-ec")),
            unpublished: Box::new(EcKey::generate(alg, "test-ec")),
        }
    }

    fn alg(&self) -> CoreJwsSigningAlgorithm {
        match self {
            Signing::Rs256 => CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
            Signing::Ec { published, .. } => published.alg.clone(),
        }
    }

    fn jwk(&self) -> Value {
        match self {
            Signing::Rs256 => serde_json::to_value(key().as_verification_key()).unwrap(),
            Signing::Ec { published, .. } => published.jwk(),
        }
    }
}

/// Authorisation codes the provider will redeem: code -> (nonce, PKCE challenge, mode).
pub(crate) type PendingCodes = Arc<Mutex<HashMap<String, (String, String, String)>>>;

#[derive(Clone)]
pub(crate) struct Provider {
    pub(crate) issuer: String,
    pub(crate) pending: PendingCodes,
    signing: Arc<Signing>,
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
    let alg = provider.signing.alg();
    let signed = match &*provider.signing {
        Signing::Rs256 => CoreIdToken::new(claims, &key(), alg, Some(&access), None),
        Signing::Ec {
            published,
            unpublished,
        } => CoreIdToken::new(
            claims,
            if mode == "wrong_key" {
                &**unpublished
            } else {
                &**published
            },
            alg,
            Some(&access),
            None,
        ),
    }
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

/// Start the RS256 provider on an ephemeral loopback port. Abort the handle to stop it.
pub(crate) async fn serve() -> anyhow::Result<(Provider, JoinHandle<()>)> {
    serve_signed(Signing::Rs256).await
}

/// Start a provider that signs, and advertises only, `signing`'s algorithm.
pub(crate) async fn serve_signed(signing: Signing) -> anyhow::Result<(Provider, JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let provider = Provider {
        issuer: format!("http://{}", listener.local_addr()?),
        pending: Arc::new(Mutex::new(HashMap::new())),
        signing: Arc::new(signing),
    };
    let alg = serde_json::to_value(provider.signing.alg())?;
    let metadata = json!({"issuer":provider.issuer,"authorization_endpoint":format!("{}/authorize",provider.issuer),"token_endpoint":format!("{}/token",provider.issuer),"jwks_uri":format!("{}/jwks",provider.issuer),"response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":[alg]});
    let jwks = json!({"keys":[provider.signing.jwk()]});
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
