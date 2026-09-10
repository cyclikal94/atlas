use super::*;
use atlas_core::error::ErrorCode;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};

#[derive(Clone)]
pub(super) struct Config {
    pub(super) origin: String,
    pub(super) secure: bool,
}

impl Config {
    pub(super) fn new(value: &str) -> Result<Self> {
        let url = url::Url::parse(value)?;
        let local = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        ensure!(
            (url.scheme() == "https" || (url.scheme() == "http" && local))
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "ATLAS_PUBLIC_ORIGIN must be an HTTPS origin (HTTP is allowed only on loopback)"
        );
        Ok(Self {
            origin: url.origin().ascii_serialization(),
            secure: url.scheme() == "https",
        })
    }

    fn name(&self) -> &'static str {
        if self.secure {
            "__Host-atlas_session"
        } else {
            "atlas_session"
        }
    }

    pub(super) fn cookie(&self, token: String) -> Cookie<'static> {
        Cookie::build((self.name(), token))
            .path("/")
            .secure(self.secure)
            .http_only(true)
            .same_site(SameSite::Lax)
            .max_age(time::Duration::days(1))
            .build()
    }

    pub(super) fn origin(&self, headers: &HeaderMap, required: bool) -> Result<(), ApiError> {
        match headers.get("origin") {
            Some(origin) => ensure_api(
                origin.to_str().ok() == Some(&self.origin),
                ErrorCode::Forbidden,
            ),
            None => ensure_api(!required, ErrorCode::Forbidden),
        }
    }

    fn token(&self, headers: &HeaderMap) -> Result<String, ApiError> {
        // Reject ambiguous duplicate cookies rather than relying on parser order.
        let mut tokens = headers
            .get_all("cookie")
            .iter()
            .filter_map(|h| h.to_str().ok())
            .flat_map(|h| h.split(';'))
            .filter_map(|part| part.trim().split_once('='))
            .filter(|(name, _)| *name == self.name())
            .map(|(_, value)| value);
        let token = tokens
            .next()
            .ok_or_else(|| anyhow!(ErrorCode::Unauthenticated))?;
        ensure_api(
            tokens.next().is_none()
                && token.len() == 64
                && token.bytes().all(|b| b.is_ascii_hexdigit()),
            ErrorCode::Unauthenticated,
        )?;
        Ok(token.to_owned())
    }
}

pub(super) fn csrf(token: &str) -> String {
    // A domain-separated one-way derivation from 244 random session bits. Exposing
    // this value never exposes the HttpOnly authentication credential.
    digest(&format!("atlas-browser-csrf-v1\n{token}"))
}

pub(super) fn credential(app: &App, headers: &HeaderMap) -> Result<String, ApiError> {
    if headers.contains_key("authorization") {
        return bearer(headers).map(str::to_owned);
    }
    let config = app
        .browser
        .as_ref()
        .ok_or_else(|| anyhow!(ErrorCode::Unauthenticated))?;
    config.origin(headers, false)?;
    let token = config.token(headers)?;
    let supplied = headers
        .get("x-csrf-token")
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    ensure_api(supplied.len() == 64, ErrorCode::Forbidden)?;
    // The dependency's timing-resistant-secret-traits feature uses constant-time equality.
    ensure_api(
        openidconnect::CsrfToken::new(csrf(&token))
            == openidconnect::CsrfToken::new(supplied.to_owned()),
        ErrorCode::Forbidden,
    )?;
    Ok(token)
}

pub(super) async fn login(
    State(app): State<App>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Result<Json<Login>, JsonRejection>,
) -> Result<Response, ApiError> {
    let config = app
        .browser
        .as_ref()
        .ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
    config.origin(&headers, true)?;
    let Json(session) = super::login(State(app.clone()), peer, headers, body).await?;
    Ok(establish(config, session))
}

pub(super) fn establish(config: &Config, session: SessionResponse) -> Response {
    (CookieJar::new().add(config.cookie(session.access_token.clone())), Json(json!({"account_id":session.account_id,"expires_at":session.expires_at,"csrf_token":csrf(&session.access_token)}))).into_response()
}

pub(super) async fn current(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let config = app
        .browser
        .as_ref()
        .ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
    config.origin(&headers, false)?;
    // Reload bootstrap requires a non-simple header. Cross-origin JS cannot send
    // it without a successful CORS preflight; Atlas deliberately grants no CORS.
    ensure_api(
        headers.get("x-atlas-session").is_some_and(|v| v == "1"),
        ErrorCode::Forbidden,
    )?;
    let token = config.token(&headers)?;
    let row = sqlx::query(
        "SELECT account_id,expires_at FROM sessions WHERE token_hash=$1 AND expires_at>$2",
    )
    .bind(digest(&token))
    .bind(now())
    .fetch_optional(&app.store.pool)
    .await?
    .ok_or_else(|| anyhow!(ErrorCode::Unauthenticated))?;
    let expires_at: i64 = row.get(1);
    Ok(Json(
        json!({"account_id": row.get::<String,_>(0), "expires_at": chrono::DateTime::from_timestamp(expires_at,0).ok_or_else(|| anyhow!(ErrorCode::InternalError))?.to_rfc3339_opts(chrono::SecondsFormat::Secs,true), "csrf_token": csrf(&token)}),
    ))
}

pub(super) fn clear_cookie(app: &App, mut response: Response) -> Response {
    if let Some(config) = &app.browser {
        let mut cookie = config.cookie(String::new());
        cookie.make_removal();
        response
            .headers_mut()
            .append("set-cookie", cookie.to_string().parse().unwrap());
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_origin_validation() {
        for origin in [
            "https://atlas.example",
            "http://localhost:3000",
            "http://127.0.0.1:3000",
            "http://[::1]:3000",
        ] {
            assert!(Config::new(origin).is_ok(), "{origin}");
        }
        for origin in [
            "http://atlas.example",
            "https://user@atlas.example",
            "https://atlas.example/path",
            "https://atlas.example?x=1",
            "https://atlas.example#x",
            "file:///tmp",
        ] {
            assert!(Config::new(origin).is_err(), "{origin}");
        }
    }
}
