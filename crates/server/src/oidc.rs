use super::*;
use atlas_core::error::ErrorCode;
use axum::response::Redirect;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use openidconnect::{
    AccessTokenHash, AsyncHttpClient, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
    EndpointMaybeSet, EndpointNotSet, EndpointSet, HttpRequest, HttpResponse, IssuerUrl, Nonce,
    OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
    reqwest,
};
use std::{future::Future, pin::Pin};

type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

pub(super) struct Config {
    issuer: IssuerUrl,
    client_id: ClientId,
    secret: Option<ClientSecret>,
    callback: RedirectUrl,
    provision: bool,
    http: BoundedHttp,
    slots: Semaphore,
}

impl Config {
    pub(super) fn new(
        browser: &browser::Config,
        issuer: &str,
        client_id: &str,
        secret: Option<String>,
        provision: bool,
    ) -> Result<Self> {
        let issuer = IssuerUrl::new(issuer.to_owned())?;
        let local_http = issuer.url().scheme() == "http" && loopback(issuer.url());
        ensure!(
            safe_url(issuer.url(), local_http)
                && issuer.url().query().is_none()
                && !client_id.is_empty()
                && client_id.len() <= 512,
            "invalid OIDC configuration"
        );
        Ok(Self {
            issuer,
            client_id: ClientId::new(client_id.to_owned()),
            secret: secret.map(ClientSecret::new),
            callback: RedirectUrl::new(format!(
                "{}/api/experimental/v1/oidc/callback",
                browser.origin
            ))?,
            provision,
            http: BoundedHttp {
                client: reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(std::time::Duration::from_secs(10))
                    .build()?,
                local_http,
            },
            slots: Semaphore::new(4),
        })
    }
    fn fingerprint(&self) -> String {
        digest(&format!(
            "{}\n{}\n{}",
            self.issuer.as_str(),
            self.client_id.as_str(),
            self.callback.as_str()
        ))
    }
    async fn client(&self) -> Result<Client, ApiError> {
        let metadata = CoreProviderMetadata::discover_async(self.issuer.clone(), &self.http)
            .await
            .map_err(|_| anyhow!(ErrorCode::OidcUnavailable))?;
        ensure_api(
            safe_url(
                metadata.authorization_endpoint().url(),
                self.http.local_http,
            ),
            ErrorCode::OidcUnavailable,
        )?;
        Ok(CoreClient::from_provider_metadata(
            metadata,
            self.client_id.clone(),
            self.secret.clone(),
        )
        .set_redirect_uri(self.callback.clone()))
    }
}

fn loopback(url: &url::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}
fn safe_url(url: &url::Url, local_http: bool) -> bool {
    (url.scheme() == "https" || (local_http && url.scheme() == "http" && loopback(url)))
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}
struct BoundedHttp {
    client: reqwest::Client,
    local_http: bool,
}
impl<'a> AsyncHttpClient<'a> for BoundedHttp {
    type Error = std::io::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<HttpResponse, Self::Error>> + Send + Sync + 'a>>;
    fn call(&'a self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            let fail = || std::io::Error::other("OIDC HTTP request failed");
            let url = url::Url::parse(&request.uri().to_string()).map_err(|_| fail())?;
            if !safe_url(&url, self.local_http) {
                return Err(fail());
            }
            let mut response = self
                .client
                .execute(request.try_into().map_err(|_| fail())?)
                .await
                .map_err(|_| fail())?;
            let mut builder = axum::http::Response::builder().status(response.status());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| fail())? {
                if body.len() + chunk.len() > 512 * 1024 {
                    return Err(fail());
                }
                body.extend_from_slice(&chunk);
            }
            builder.body(body).map_err(|_| fail())
        })
    }
}

fn binding_name(browser: &browser::Config) -> &'static str {
    if browser.secure {
        "__Host-atlas_oidc"
    } else {
        "atlas_oidc"
    }
}
fn binding_cookie(browser: &browser::Config, value: String) -> Cookie<'static> {
    Cookie::build((binding_name(browser), value))
        .path("/")
        .http_only(true)
        .secure(browser.secure)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::minutes(10))
        .build()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Start {
    device_id: String,
    #[serde(default)]
    link: bool,
}

pub(super) async fn start(
    State(app): State<App>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Result<Json<Start>, JsonRejection>,
) -> Result<Response, ApiError> {
    let browser = app
        .browser
        .as_ref()
        .ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
    browser.origin(&headers, true)?;
    source_attempt(&app, peer, &headers)?;
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(
        !input.device_id.is_empty() && input.device_id.len() <= 100,
        ErrorCode::InvalidValue,
    )?;
    let link_hash = if input.link {
        identity(&app, &headers).await?;
        let hash = digest(&browser::credential(&app, &headers)?);
        let recent: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sessions WHERE token_hash=$1 AND created_at>=$2",
        )
        .bind(&hash)
        .bind(now() - 300)
        .fetch_one(&app.store.pool)
        .await?;
        ensure_api(recent == 1, ErrorCode::Unauthenticated)?;
        Some(hash)
    } else {
        None
    };
    let (jar, url) = create_flow(&app, &input.device_id, link_hash, None).await?;
    Ok((jar, Json(json!({"authorization_url":url.as_str()}))).into_response())
}
async fn create_flow(
    app: &App,
    device: &str,
    link_hash: Option<String>,
    native: Option<&NativeStart>,
) -> Result<(CookieJar, url::Url), ApiError> {
    let config = app
        .oidc
        .as_ref()
        .ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
    let browser = app
        .browser
        .as_ref()
        .ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
    ensure_api(
        !device.is_empty() && device.len() <= 100,
        ErrorCode::InvalidValue,
    )?;
    let _permit = config
        .slots
        .try_acquire()
        .map_err(|_| anyhow!(ErrorCode::RateLimited))?;
    let client = config.client().await?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .set_pkce_challenge(challenge)
        .url();
    let binding = CsrfToken::new_random();
    let mut tx = app.store.begin_serial().await?;
    sqlx::query("DELETE FROM oidc_flows WHERE expires_at<=$1")
        .bind(now())
        .execute(&mut *tx)
        .await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oidc_flows")
        .fetch_one(&mut *tx)
        .await?;
    ensure_api(count < 1000, ErrorCode::RateLimited)?;
    sqlx::query("INSERT INTO oidc_flows(state_hash,binding_hash,nonce,verifier,device_id,link_session_hash,expires_at,configuration_hash,native_redirect,native_challenge,native_state) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
        .bind(digest(state.secret())).bind(digest(binding.secret())).bind(nonce.secret()).bind(verifier.secret()).bind(device).bind(link_hash).bind(now()+600).bind(config.fingerprint()).bind(native.map(|n| &n.redirect_uri)).bind(native.map(|n| &n.code_challenge)).bind(native.map(|n| &n.state)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok((
        CookieJar::new().add(binding_cookie(browser, binding.secret().clone())),
        url,
    ))
}

#[derive(Deserialize)]
pub(super) struct Callback {
    state: String,
    code: Option<String>,
    error: Option<String>,
}

pub(super) async fn callback(
    State(app): State<App>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    query: Result<Query<Callback>, QueryRejection>,
) -> Result<Response, ApiError> {
    let config = app
        .oidc
        .as_ref()
        .ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
    let browser = app
        .browser
        .as_ref()
        .ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
    source_attempt(&app, peer, &headers)?;
    let Query(input) = query.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(
        input.state.len() <= 512 && input.code.as_ref().is_none_or(|c| c.len() <= 8192),
        ErrorCode::InvalidValue,
    )?;
    let jar = CookieJar::from_headers(&headers);
    let binding = jar
        .get(binding_name(browser))
        .ok_or_else(|| anyhow!(ErrorCode::Unauthenticated))?;
    let _permit = config
        .slots
        .try_acquire()
        .map_err(|_| anyhow!(ErrorCode::RateLimited))?;
    // Consuming a browser-bound state precedes network I/O; concurrent callbacks
    // cannot exchange the same code twice, including across server replicas.
    let row = sqlx::query("DELETE FROM oidc_flows WHERE state_hash=$1 AND binding_hash=$2 AND expires_at>$3 AND configuration_hash=$4 RETURNING nonce,verifier,device_id,link_session_hash,native_redirect,native_challenge,native_state")
        .bind(digest(&input.state)).bind(digest(binding.value())).bind(now()).bind(config.fingerprint()).fetch_optional(&app.store.pool).await?.ok_or_else(|| anyhow!(ErrorCode::Unauthenticated))?;
    ensure_api(input.error.is_none(), ErrorCode::Unauthenticated)?;
    let native: Option<String> = row.get(4);
    if let Some(uri) = &native {
        ensure_api(app.native_redirects.contains(uri), ErrorCode::Forbidden)?;
    }

    let client = config.client().await?;
    let token = client
        .exchange_code(AuthorizationCode::new(
            input
                .code
                .ok_or_else(|| anyhow!(ErrorCode::MalformedRequest))?,
        ))
        .map_err(|_| anyhow!(ErrorCode::OidcUnavailable))?
        .set_pkce_verifier(PkceCodeVerifier::new(row.get(1)))
        .request_async(&config.http)
        .await
        .map_err(|_| anyhow!(ErrorCode::Unauthenticated))?;
    let id_token = token
        .id_token()
        .ok_or_else(|| anyhow!(ErrorCode::Unauthenticated))?;
    let verifier = client.id_token_verifier();
    let nonce = Nonce::new(row.get(0));
    let claims = id_token
        .claims(&verifier, &nonce)
        .map_err(|_| anyhow!(ErrorCode::Unauthenticated))?;
    if let Some(expected) = claims.access_token_hash() {
        let actual = AccessTokenHash::from_token(
            token.access_token(),
            id_token
                .signing_alg()
                .map_err(|_| anyhow!(ErrorCode::Unauthenticated))?,
            id_token
                .signing_key(&verifier)
                .map_err(|_| anyhow!(ErrorCode::Unauthenticated))?,
        )
        .map_err(|_| anyhow!(ErrorCode::Unauthenticated))?;
        ensure_api(actual == *expected, ErrorCode::Unauthenticated)?;
    }
    let subject = claims.subject().as_str();
    ensure_api(
        !subject.is_empty() && subject.len() <= 1024,
        ErrorCode::Unauthenticated,
    )?;
    let mut tx = app.store.begin_serial().await?;
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT account_id FROM external_identities WHERE issuer=$1 AND subject=$2",
    )
    .bind(config.issuer.as_str())
    .bind(subject)
    .fetch_optional(&mut *tx)
    .await?;
    let linking: Option<String> = row.get(3);
    let account = if let Some(hash) = linking {
        let account: String = sqlx::query_scalar(
            "SELECT account_id FROM sessions WHERE token_hash=$1 AND expires_at>$2",
        )
        .bind(hash)
        .bind(now())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow!(ErrorCode::Unauthenticated))?;
        ensure_api(
            existing.as_ref().is_none_or(|id| id == &account),
            ErrorCode::Conflict,
        )?;
        account
    } else if let Some(account) = &existing {
        account.clone()
    } else {
        ensure_api(config.provision, ErrorCode::Forbidden)?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
            .fetch_one(&mut *tx)
            .await?;
        ensure_api(count < 100, ErrorCode::SliceCapacity)?;
        let id = Uuid::new_v4().to_string();
        // No email/name matching. An OIDC-only account has no usable local hash.
        sqlx::query("INSERT INTO accounts(id,username,password_hash) VALUES ($1,$2,'!oidc-only')")
            .bind(&id)
            .bind(format!("oidc-{}", Uuid::new_v4().simple()))
            .execute(&mut *tx)
            .await?;
        id
    };
    if existing.is_none() {
        sqlx::query("INSERT INTO external_identities(issuer,subject,account_id) VALUES ($1,$2,$3)")
            .bind(config.issuer.as_str())
            .bind(subject)
            .bind(&account)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(uri) = native {
        sqlx::query("DELETE FROM native_handoffs WHERE expires_at<=$1")
            .bind(now())
            .execute(&mut *tx)
            .await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM native_handoffs")
            .fetch_one(&mut *tx)
            .await?;
        ensure_api(count < 1000, ErrorCode::RateLimited)?;
        let code = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        sqlx::query("INSERT INTO native_handoffs(code_hash,account_id,device_id,challenge,expires_at,configuration_hash,redirect_uri) VALUES ($1,$2,$3,$4,$5,$6,$7)").bind(digest(&code)).bind(account).bind(row.get::<String,_>(2)).bind(row.get::<String,_>(5)).bind(now()+60).bind(config.fingerprint()).bind(&uri).execute(&mut *tx).await?;
        tx.commit().await?;
        let mut destination =
            url::Url::parse(&uri).map_err(|_| anyhow!(ErrorCode::InternalError))?;
        destination
            .query_pairs_mut()
            .append_pair("code", &code)
            .append_pair("state", &row.get::<String, _>(6));
        let mut remove = binding_cookie(browser, String::new());
        remove.make_removal();
        return Ok((
            CookieJar::new().add(remove),
            Redirect::to(destination.as_str()),
        )
            .into_response());
    }
    let session = sessions::issue(&mut tx, &account, &row.get::<String, _>(2), "oidc").await?;
    tx.commit().await?;
    let mut remove = binding_cookie(browser, String::new());
    remove.make_removal();
    // Fixed same-origin destination; no caller-selected open redirect. The API
    // client obtains its CSRF token through the protected reload endpoint.
    Ok((
        CookieJar::new()
            .add(browser.cookie(session.access_token))
            .add(remove),
        Redirect::to(&browser.origin),
    )
        .into_response())
}

pub(super) fn native_redirect(value: &str) -> Result<String> {
    let url = url::Url::parse(value)?;
    ensure!(
        (url.scheme() == "https"
            || (url.scheme() == "http" && loopback(&url))
            || url.scheme().contains('.'))
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "native redirect must be an HTTPS, loopback HTTP or reverse-domain application URI without user information, query or fragment"
    );
    Ok(url.to_string())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeStart {
    device_id: String,
    redirect_uri: String,
    code_challenge: String,
    state: String,
}
pub(super) async fn native_start(
    State(app): State<App>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    query: Result<Query<NativeStart>, QueryRejection>,
) -> Result<Response, ApiError> {
    ensure_api(!app.native_redirects.is_empty(), ErrorCode::NotFound)?;
    let Query(input) = query.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(
        app.native_redirects.contains(&input.redirect_uri)
            && input.code_challenge.len() == 43
            && input
                .code_challenge
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
            && !input.state.is_empty()
            && input.state.len() <= 512,
        ErrorCode::InvalidValue,
    )?;
    source_attempt(&app, peer, &headers)?;
    let (jar, url) = create_flow(&app, &input.device_id, None, Some(&input)).await?;
    Ok((jar, Redirect::to(url.as_str())).into_response())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Exchange {
    code: String,
    code_verifier: String,
}
pub(super) async fn native_exchange(
    State(app): State<App>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Result<Json<Exchange>, JsonRejection>,
) -> Result<Json<SessionResponse>, ApiError> {
    ensure_api(
        app.oidc.is_some() && !app.native_redirects.is_empty(),
        ErrorCode::NotFound,
    )?;
    let Json(input) = body.map_err(|_| anyhow!(ErrorCode::MalformedRequest))?;
    ensure_api(
        input.code.len() == 64
            && input.code.bytes().all(|c| c.is_ascii_hexdigit())
            && (43..=128).contains(&input.code_verifier.len())
            && input
                .code_verifier
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c)),
        ErrorCode::InvalidValue,
    )?;
    source_attempt(&app, peer, &headers)?;
    let challenge =
        PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(input.code_verifier));
    let mut tx = app.store.begin_serial().await?;
    let row=sqlx::query("DELETE FROM native_handoffs WHERE code_hash=$1 AND challenge=$2 AND expires_at>$3 AND configuration_hash=$4 RETURNING account_id,device_id,redirect_uri").bind(digest(&input.code)).bind(challenge.as_str()).bind(now()).bind(app.oidc.as_ref().unwrap().fingerprint()).fetch_optional(&mut *tx).await?.ok_or_else(||anyhow!(ErrorCode::Unauthenticated))?;
    ensure_api(
        app.native_redirects.contains(&row.get::<String, _>(2)),
        ErrorCode::Forbidden,
    )?;
    let session = sessions::issue(
        &mut tx,
        &row.get::<String, _>(0),
        &row.get::<String, _>(1),
        "oidc",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(session))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_redirect_configuration_rejects_unsafe_or_ambiguous_targets() {
        for uri in [
            "https://app.example/callback",
            "http://127.0.0.1:3456/callback",
            "dev.atlas.app:/oauth/callback",
        ] {
            assert!(native_redirect(uri).is_ok(), "{uri}");
        }
        for uri in [
            "javascript:alert(1)",
            "data:text/html,test",
            "file:///tmp",
            "http://app.example/callback",
            "https://user@app.example",
            "https://app.example/callback?code=old",
            "https://app.example/#callback",
        ] {
            assert!(native_redirect(uri).is_err(), "{uri}");
        }
    }
}
