//! A fake OpenID provider on a loopback listener, shared by the OIDC scenario and by the
//! retirement-coordination tests that drive the real callback and native exchange routes.
use atlas_server::now;
use axum::{
    Form, Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use openidconnect::{
    AccessToken, AccessTokenHash, Audience, EmptyAdditionalClaims, IssuerUrl, JsonWebKeyId, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, PrivateSigningKey, SigningError, StandardClaims,
    SubjectIdentifier,
    core::{
        CoreIdToken, CoreIdTokenClaims, CoreJsonWebKey, CoreJwsSigningAlgorithm,
        CoreRsaPrivateSigningKey,
    },
};
use ring::{
    digest, hmac,
    rand::SystemRandom,
    signature::{
        ECDSA_P256_SHA256_FIXED_SIGNING, ECDSA_P384_SHA384_FIXED_SIGNING, EcdsaKeyPair, KeyPair,
    },
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{sync::Notify, task::JoinHandle};
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

/// A test-only HS256/384/512 signer backed by ring, so the provider's MACs come from a different
/// implementation than the RustCrypto `hmac` verifier inside openidconnect (BE-Q23 H1).
pub(crate) struct HmacKey {
    alg: CoreJwsSigningAlgorithm,
    secret: Vec<u8>,
}

impl HmacKey {
    pub(crate) fn new(alg: CoreJwsSigningAlgorithm, secret: &[u8]) -> Self {
        Self {
            alg,
            secret: secret.to_vec(),
        }
    }
}

impl PrivateSigningKey for HmacKey {
    type VerificationKey = CoreJsonWebKey;

    fn sign(
        &self,
        signature_alg: &CoreJwsSigningAlgorithm,
        message: &[u8],
    ) -> Result<Vec<u8>, SigningError> {
        if *signature_alg != self.alg {
            return Err(SigningError::UnsupportedAlg(format!("{signature_alg:?}")));
        }
        let algorithm = match signature_alg {
            CoreJwsSigningAlgorithm::HmacSha256 => hmac::HMAC_SHA256,
            CoreJwsSigningAlgorithm::HmacSha384 => hmac::HMAC_SHA384,
            CoreJwsSigningAlgorithm::HmacSha512 => hmac::HMAC_SHA512,
            _ => return Err(SigningError::UnsupportedAlg(format!("{signature_alg:?}"))),
        };
        let key = hmac::Key::new(algorithm, &self.secret);
        Ok(hmac::sign(&key, message).as_ref().to_vec())
    }

    fn as_verification_key(&self) -> CoreJsonWebKey {
        serde_json::from_value(
            json!({"kty":"oct","use":"sig","k":URL_SAFE_NO_PAD.encode(&self.secret)}),
        )
        .unwrap()
    }
}

/// How the provider signs ID tokens: the RS256 fixture key, a fresh ECDSA key, or a shared client
/// secret. An EC provider also holds a second, unpublished key of the same curve for the
/// `wrong_key` mode; an HMAC provider publishes no key and signs `wrong_key` with another secret.
pub(crate) enum Signing {
    Rs256,
    Ec {
        published: Box<EcKey>,
        unpublished: Box<EcKey>,
    },
    Hmac {
        alg: CoreJwsSigningAlgorithm,
        secret: String,
    },
}

impl Signing {
    pub(crate) fn ec(alg: CoreJwsSigningAlgorithm) -> Self {
        Signing::Ec {
            published: Box::new(EcKey::generate(alg.clone(), "test-ec")),
            unpublished: Box::new(EcKey::generate(alg, "test-ec")),
        }
    }

    /// A confidential-client provider that signs ID tokens with `secret` under `alg`.
    pub(crate) fn hmac(alg: CoreJwsSigningAlgorithm, secret: &str) -> Self {
        Signing::Hmac {
            alg,
            secret: secret.into(),
        }
    }

    fn alg(&self) -> CoreJwsSigningAlgorithm {
        match self {
            Signing::Rs256 => CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
            Signing::Ec { published, .. } => published.alg.clone(),
            Signing::Hmac { alg, .. } => alg.clone(),
        }
    }

    /// The published verification keys: none for a shared secret.
    fn jwks(&self) -> Vec<Value> {
        match self {
            Signing::Rs256 => vec![serde_json::to_value(key().as_verification_key()).unwrap()],
            Signing::Ec { published, .. } => vec![published.jwk()],
            Signing::Hmac { .. } => vec![],
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
    /// For a `hold+` mode: the token endpoint signals `arrived` once it has the code, then waits
    /// for `release` before answering, so a test can order a second callback deterministically.
    pub(crate) arrived: Arc<Notify>,
    pub(crate) release: Arc<Notify>,
    /// ID tokens the token endpoint has answered with, so a test can tell a refused token from a
    /// failed exchange.
    issued: Arc<AtomicUsize>,
    /// The confidential client secret an RS/EC provider expects in HTTP Basic authentication (an
    /// HMAC provider expects its signing secret); `None` for a public client.
    client_secret: Option<String>,
}

impl Provider {
    pub(crate) fn issued(&self) -> usize {
        self.issued.load(Ordering::SeqCst)
    }
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
    headers: HeaderMap,
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
    let mode = match mode.strip_prefix("hold+") {
        Some(rest) => {
            provider.arrived.notify_one();
            provider.release.notified().await;
            rest.to_owned()
        }
        None => mode,
    };
    // `…+new_subject` / `…+hash_subject`: the same mode for one of two subjects no account is
    // linked to.
    let (mode, subject) = if let Some(rest) = mode.strip_suffix("+new_subject") {
        (rest.to_owned(), "new-subject")
    } else if let Some(rest) = mode.strip_suffix("+hash_subject") {
        (rest.to_owned(), "hash-subject")
    } else if mode == "new_subject" {
        (mode, "new-subject")
    } else {
        (mode, "provider-subject")
    };
    assert_eq!(form["grant_type"], "authorization_code");
    // A public client names itself in the form; a confidential one authenticates with HTTP Basic
    // (`client_secret_basic`, the library default), which must carry the shared secret.
    let confidential = match &*provider.signing {
        Signing::Hmac { secret, .. } => Some(secret.as_str()),
        _ => provider.client_secret.as_deref(),
    };
    match (confidential, headers.get("authorization")) {
        (Some(secret), Some(basic)) => {
            let encoded = basic.to_str().unwrap().strip_prefix("Basic ").unwrap();
            let decoded = String::from_utf8(STANDARD.decode(encoded).unwrap()).unwrap();
            assert_eq!(decoded, format!("atlas-test:{secret}"));
        }
        (_, None) => assert_eq!(form["client_id"], "atlas-test"),
        (None, Some(_)) => panic!("unexpected client authentication"),
    }
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
        StandardClaims::new(SubjectIdentifier::new(subject.into())),
        EmptyAdditionalClaims {},
    )
    .set_nonce(Some(Nonce::new(if mode == "nonce" {
        "wrong".into()
    } else {
        nonce
    })));
    let alg = provider.signing.alg();
    // `at_hash:KIND`: a validly signed token whose `at_hash` claim is not the hash of the returned
    // access token under the signing algorithm. Otherwise the library computes the claim from
    // the access token (always for RS/EC; for HS only in `valid_at_hash` and `access_hash`).
    let forged = mode
        .strip_prefix("at_hash:")
        .map(|kind| AccessTokenHash::new(forged_access_hash(kind, &alg, access.secret())));
    let hashed = forged.is_none().then_some(&access);
    let claims = match forged {
        Some(hash) => claims.set_access_token_hash(Some(hash)),
        None => claims,
    };
    let signed = match &*provider.signing {
        Signing::Rs256 => CoreIdToken::new(claims, &key(), alg, hashed, None),
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
            hashed,
            None,
        ),
        // `at_hash` is optional in the code flow; `valid_at_hash` and `access_hash` ask for one,
        // `at_hash:KIND` sets a wrong one. `alg:HS…` signs under an algorithm the provider
        // metadata does not advertise.
        Signing::Hmac { alg, secret } => {
            let alg = match mode.strip_prefix("alg:") {
                Some(name) => serde_json::from_value(json!(name)).unwrap(),
                None => alg.clone(),
            };
            let secret = if mode == "wrong_key" {
                format!("{secret}-other")
            } else {
                secret.clone()
            };
            CoreIdToken::new(
                claims,
                &HmacKey::new(alg.clone(), secret.as_bytes()),
                alg,
                matches!(mode.as_str(), "valid_at_hash" | "access_hash").then_some(&access),
                None,
            )
        }
    }
    .unwrap();
    let mut token = signed.to_string();
    // `mac_len:N`: the real signature cut or extended to N bytes (a prefix of the valid MAC when
    // shorter), the signing input unchanged.
    if let Some(length) = mode.strip_prefix("mac_len:") {
        let length: usize = length.parse().unwrap();
        let (input, signature) = token.rsplit_once('.').unwrap();
        let mut bytes = URL_SAFE_NO_PAD.decode(signature).unwrap();
        bytes.resize(length, 0xA5);
        token = format!("{input}.{}", URL_SAFE_NO_PAD.encode(&bytes));
    }
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
    provider.issued.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::OK,
        Json(
            json!({"access_token":if mode=="access_hash" {"swapped"} else {"test-access"}, "token_type":"Bearer", "id_token":token}),
        ),
    )
}

/// The `at_hash` claim of an `at_hash:KIND` mode, computed with ring (independently of
/// openidconnect) for `access` under `alg`: `other_digest` is the left half of a digest the
/// algorithm does not use (SHA-384 for a 256-bit algorithm, otherwise SHA-256), `truncated` the
/// correct value without its last character, `empty` and `garbage` not a hash at all.
fn forged_access_hash(kind: &str, alg: &CoreJwsSigningAlgorithm, access: &str) -> String {
    use CoreJwsSigningAlgorithm as A;
    let (own, other) = match alg {
        A::HmacSha256 | A::RsaSsaPkcs1V15Sha256 | A::RsaSsaPssSha256 | A::EcdsaP256Sha256 => {
            (&digest::SHA256, &digest::SHA384)
        }
        A::HmacSha384 | A::RsaSsaPkcs1V15Sha384 | A::RsaSsaPssSha384 | A::EcdsaP384Sha384 => {
            (&digest::SHA384, &digest::SHA256)
        }
        _ => (&digest::SHA512, &digest::SHA256),
    };
    let left_half = |algorithm| {
        let full = digest::digest(algorithm, access.as_bytes());
        URL_SAFE_NO_PAD.encode(&full.as_ref()[..full.as_ref().len() / 2])
    };
    match kind {
        "other_digest" => left_half(other),
        "truncated" => {
            let mut correct = left_half(own);
            correct.pop();
            correct
        }
        "empty" => String::new(),
        "garbage" => "not-an-access-token-hash".into(),
        _ => panic!("unknown at_hash mode {kind}"),
    }
}

/// Start the RS256 provider on an ephemeral loopback port. Abort the handle to stop it.
pub(crate) async fn serve() -> anyhow::Result<(Provider, JoinHandle<()>)> {
    serve_signed(Signing::Rs256).await
}

/// Start a provider that signs, and advertises only, `signing`'s algorithm.
pub(crate) async fn serve_signed(signing: Signing) -> anyhow::Result<(Provider, JoinHandle<()>)> {
    serve_provider(signing, None).await
}

/// Start an RS/EC provider for a confidential client that authenticates with `client_secret`.
pub(crate) async fn serve_confidential(
    signing: Signing,
    client_secret: &str,
) -> anyhow::Result<(Provider, JoinHandle<()>)> {
    assert!(!matches!(signing, Signing::Hmac { .. }), "use serve_signed");
    serve_provider(signing, Some(client_secret.into())).await
}

async fn serve_provider(
    signing: Signing,
    client_secret: Option<String>,
) -> anyhow::Result<(Provider, JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let provider = Provider {
        issuer: format!("http://{}", listener.local_addr()?),
        pending: Arc::new(Mutex::new(HashMap::new())),
        signing: Arc::new(signing),
        arrived: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
        issued: Arc::new(AtomicUsize::new(0)),
        client_secret,
    };
    let alg = serde_json::to_value(provider.signing.alg())?;
    let metadata = json!({"issuer":provider.issuer,"authorization_endpoint":format!("{}/authorize",provider.issuer),"token_endpoint":format!("{}/token",provider.issuer),"jwks_uri":format!("{}/jwks",provider.issuer),"response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":[alg]});
    let jwks = json!({"keys":provider.signing.jwks()});
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
