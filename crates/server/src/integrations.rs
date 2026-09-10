//! Outbound integrations: encrypted settings, pinned DNS and real push payloads.
use atlas_core::error::ErrorCode;
pub(crate) mod worker;
use anyhow::{Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    aead, hmac,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use url::Url;
#[derive(Clone)]
pub struct IntegrationConfig {
    key: Option<Arc<aead::LessSafeKey>>,
    mac: Option<Arc<hmac::Key>>,
    origins: Arc<BTreeSet<String>>,
    vapid: Option<Arc<String>>,
    contact: Option<Arc<String>>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub url: String,
    pub bearer: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Subscription {
    Ntfy {
        url: String,
        bearer: Option<String>,
    },
    WebPush {
        endpoint: String,
        p256dh: String,
        auth: String,
    },
}
impl IntegrationConfig {
    pub fn new(
        key: Option<&str>,
        origins: Vec<String>,
        vapid: Option<String>,
        contact: Option<String>,
    ) -> Result<Self> {
        let key = key
            .map(|s| -> Result<Vec<u8>> {
                ensure!(s.len() == 64 && s.is_ascii(), "invalid integration key");
                (0..32)
                    .map(|n| {
                        u8::from_str_radix(&s[n * 2..n * 2 + 2], 16)
                            .map_err(|_| anyhow!("invalid integration key"))
                    })
                    .collect()
            })
            .transpose()?;
        let mac = key
            .as_ref()
            .map(|v| Arc::new(hmac::Key::new(hmac::HMAC_SHA256, v)));
        let key = key
            .map(|v| {
                aead::UnboundKey::new(&aead::AES_256_GCM, &v)
                    .map(|v| Arc::new(aead::LessSafeKey::new(v)))
                    .map_err(|_| anyhow!("invalid integration key"))
            })
            .transpose()?;
        let origins = origins
            .into_iter()
            .map(|v| -> Result<String> {
                let u = Url::parse(&v)?;
                ensure!(
                    matches!(u.scheme(), "http" | "https")
                        && u.host_str().is_some()
                        && u.username().is_empty()
                        && u.password().is_none()
                        && u.query().is_none()
                        && u.fragment().is_none()
                        && u.path() == "/",
                    "invalid outbound origin"
                );
                Ok(u.origin().ascii_serialization())
            })
            .collect::<Result<BTreeSet<_>>>()?;
        if let Some(v) = &vapid {
            web_push::VapidSignatureBuilder::from_base64_no_sub(v)
                .map_err(|_| anyhow!("invalid VAPID key"))?;
            let c = contact
                .as_deref()
                .ok_or_else(|| anyhow!("VAPID contact required"))?;
            ensure!(
                c.starts_with("mailto:") || c.starts_with("https://"),
                "invalid VAPID contact"
            );
        }
        Ok(Self {
            key,
            mac,
            origins: Arc::new(origins),
            vapid: vapid.map(Arc::new),
            contact: contact.map(Arc::new),
        })
    }
    pub fn from_env() -> Result<Self> {
        Self::new(
            std::env::var("ATLAS_SECRET_KEY").ok().as_deref(),
            std::env::var("ATLAS_OUTBOUND_ALLOW_ORIGINS")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            std::env::var("ATLAS_VAPID_PRIVATE_KEY").ok(),
            std::env::var("ATLAS_VAPID_SUBJECT").ok(),
        )
    }
    pub fn capabilities(&self) -> Result<serde_json::Value> {
        let public = self
            .vapid
            .as_ref()
            .map(|v| {
                web_push::VapidSignatureBuilder::from_base64_no_sub(v)
                    .map(|v| URL_SAFE_NO_PAD.encode(v.get_public_key()))
            })
            .transpose()?;
        Ok(
            serde_json::json!({"calendar_links":self.key.is_some(),"ntfy":self.key.is_some(),"web_push":self.key.is_some()&&public.is_some(),"vapid_public_key":public,"native_local":true,"apns":false,"fcm":false}),
        )
    }
    pub fn seal(&self, scope: &str, plain: &str) -> Result<String> {
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| anyhow!(ErrorCode::IntegrationUnconfigured))?;
        let mac = self
            .mac
            .as_ref()
            .ok_or_else(|| anyhow!(ErrorCode::IntegrationUnconfigured))?;
        let fingerprint = URL_SAFE_NO_PAD
            .encode(hmac::sign(mac, format!("{scope}\n{plain}").as_bytes()).as_ref());
        let mut nonce = [0_u8; 12];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| anyhow!(ErrorCode::SecretEncryptionFailed))?;
        let mut body = plain.as_bytes().to_vec();
        key.seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(format!("{scope}:{fingerprint}").as_bytes()),
            &mut body,
        )
        .map_err(|_| anyhow!(ErrorCode::SecretEncryptionFailed))?;
        Ok(format!(
            "v1:{fingerprint}:{}",
            URL_SAFE_NO_PAD.encode([nonce.as_slice(), body.as_slice()].concat())
        ))
    }
    pub fn open(&self, scope: &str, sealed: &str) -> Result<String> {
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| anyhow!(ErrorCode::IntegrationUnconfigured))?;
        let parts = sealed.splitn(3, ':').collect::<Vec<_>>();
        ensure!(
            parts.len() == 3 && parts[0] == "v1",
            ErrorCode::InvalidSecret
        );
        let mut body = URL_SAFE_NO_PAD
            .decode(parts[2])
            .map_err(|_| anyhow!(ErrorCode::InvalidSecret))?;
        ensure!(body.len() >= 28, ErrorCode::InvalidSecret);
        let nonce: [u8; 12] = body[..12].try_into()?;
        let plain = key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(format!("{scope}:{}", parts[1]).as_bytes()),
                &mut body[12..],
            )
            .map_err(|_| anyhow!(ErrorCode::InvalidSecret))?;
        String::from_utf8(plain.to_vec()).map_err(|_| anyhow!(ErrorCode::InvalidSecret))
    }
    fn url(&self, value: &str) -> Result<Url> {
        let url = Url::parse(value).map_err(|_| anyhow!(ErrorCode::InvalidEndpoint))?;
        ensure!(
            url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none(),
            ErrorCode::InvalidEndpoint
        );
        let allowed = self.origins.contains(&url.origin().ascii_serialization());
        ensure!(
            url.scheme() == "https" || (allowed && url.scheme() == "http"),
            ErrorCode::InvalidEndpoint
        );
        Ok(url)
    }
    pub async fn client(&self, value: &str) -> Result<(reqwest::Client, Url)> {
        let url = self.url(value)?;
        let host = url
            .host_str()
            .ok_or_else(|| anyhow!(ErrorCode::InvalidEndpoint))?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let port = url
            .port_or_known_default()
            .ok_or_else(|| anyhow!(ErrorCode::InvalidEndpoint))?;
        let ips = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::lookup_host((host, port)),
        )
        .await
        .map_err(|_| anyhow!(ErrorCode::FetchFailed))?
        .map_err(|_| anyhow!(ErrorCode::FetchFailed))?
        .collect::<Vec<SocketAddr>>();
        ensure!(
            !ips.is_empty() && ips.len() <= 32,
            ErrorCode::InvalidEndpoint
        );
        let allowed = self.origins.contains(&url.origin().ascii_serialization());
        ensure!(
            allowed || ips.iter().all(|s| public_ip(s.ip())),
            ErrorCode::OutboundDenied
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .resolve_to_addrs(host, &ips)
            .build()
            .map_err(|_| anyhow!(ErrorCode::FetchFailed))?;
        Ok((client, url))
    }
    pub async fn fetch(
        &self,
        link: &Link,
        etag: Option<&str>,
        modified: Option<&str>,
    ) -> Result<(Option<String>, Option<String>, Option<String>)> {
        let (client, url) = self.client(&link.url).await?;
        let mut request = client.get(url).header("Accept", "text/calendar");
        if let Some(token) = &link.bearer {
            request = request.bearer_auth(token);
        }
        if let Some(v) = etag {
            request = request.header("If-None-Match", v);
        }
        if let Some(v) = modified {
            request = request.header("If-Modified-Since", v);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| anyhow!(ErrorCode::FetchFailed))?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            ensure!(etag.is_some() || modified.is_some(), ErrorCode::FetchFailed);
            return Ok((None, etag.map(str::to_owned), modified.map(str::to_owned)));
        }
        ensure!(response.status().is_success(), ErrorCode::FetchFailed);
        ensure!(
            response.content_length().is_none_or(|n| n <= 1024 * 1024),
            ErrorCode::CalendarLimit
        );
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .filter(|v| v.len() <= 1024)
                .map(str::to_owned)
        };
        let etag = header("etag");
        let modified = header("last-modified");
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!(ErrorCode::FetchFailed))?
        {
            ensure!(
                bytes.len() + chunk.len() <= 1024 * 1024,
                ErrorCode::CalendarLimit
            );
            bytes.extend(chunk);
        }
        Ok((
            Some(String::from_utf8(bytes).map_err(|_| anyhow!(ErrorCode::InvalidIcs))?),
            etag,
            modified,
        ))
    }
    pub async fn validate_subscription(&self, subscription: &Subscription) -> Result<()> {
        match subscription {
            Subscription::Ntfy { url, bearer } => {
                self.client(url).await?;
                ensure!(
                    bearer.as_ref().is_none_or(|s| s.len() <= 4096),
                    ErrorCode::InvalidValue
                );
            }
            Subscription::WebPush {
                endpoint,
                p256dh,
                auth,
            } => {
                self.client(endpoint).await?;
                ensure!(self.vapid.is_some(), ErrorCode::IntegrationUnconfigured);
                let info = web_push::SubscriptionInfo::new(endpoint, p256dh, auth);
                let mut builder = web_push::WebPushMessageBuilder::new(&info);
                builder.set_payload(web_push::ContentEncoding::Aes128Gcm, b"validation");
                builder
                    .build()
                    .map_err(|_| anyhow!(ErrorCode::InvalidSubscription))?;
            }
        }
        Ok(())
    }
    pub async fn deliver(
        &self,
        subscription: &Subscription,
        notification: &atlas_core::calendars::Notification,
        expires: i64,
    ) -> Result<(bool, bool)> {
        let response = match subscription {
            Subscription::Ntfy { url, bearer } => {
                let (client, url) = self.client(url).await?;
                let mut request = client
                    .post(url)
                    .header("Title", "Atlas reminder")
                    .header("Cache", "no")
                    .body(format!(
                        "Open Atlas to view your reminder.\nNotification: {}",
                        notification.id
                    ));
                if let Some(token) = bearer {
                    request = request.bearer_auth(token);
                }
                request
                    .send()
                    .await
                    .map_err(|_| anyhow!(ErrorCode::DeliveryFailed))?
            }
            Subscription::WebPush {
                endpoint,
                p256dh,
                auth,
            } => {
                let (client, url) = self.client(endpoint).await?;
                let info = web_push::SubscriptionInfo::new(endpoint, p256dh, auth);
                let mut signature = web_push::VapidSignatureBuilder::from_base64(
                    self.vapid
                        .as_ref()
                        .ok_or_else(|| anyhow!(ErrorCode::IntegrationUnconfigured))?,
                    &info,
                )?;
                signature.add_claim(
                    "sub",
                    self.contact
                        .as_deref()
                        .map(|v| v.as_str())
                        .ok_or_else(|| anyhow!(ErrorCode::IntegrationUnconfigured))?,
                );
                let mut builder = web_push::WebPushMessageBuilder::new(&info);
                builder.set_vapid_signature(signature.build()?);
                builder.set_ttl((expires - crate::now()).clamp(0, 604800) as u32);
                builder.set_topic(notification.id.replace('-', ""));
                let payload = serde_json::to_vec(notification)?;
                builder.set_payload(web_push::ContentEncoding::Aes128Gcm, &payload);
                let message = builder.build()?;
                let payload = message
                    .payload
                    .ok_or_else(|| anyhow!(ErrorCode::DeliveryFailed))?;
                let mut request = client
                    .post(url)
                    .header("TTL", message.ttl)
                    .header("Content-Encoding", payload.content_encoding.to_str())
                    .header("Content-Type", "application/octet-stream")
                    .header("Topic", message.topic.unwrap_or_default());
                for (name, value) in payload.crypto_headers {
                    request = request.header(name, value);
                }
                request
                    .body(payload.content)
                    .send()
                    .await
                    .map_err(|_| anyhow!(ErrorCode::DeliveryFailed))?
            }
        };
        let code = response.status();
        Ok((
            code.is_success(),
            matches!(code.as_u16(), 400 | 401 | 403 | 404 | 410 | 413),
        ))
    }
}
pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            !v.is_private()
                && !v.is_loopback()
                && !v.is_link_local()
                && !v.is_broadcast()
                && !v.is_documentation()
                && !v.is_unspecified()
                && !v.is_multicast()
                && o[0] != 0
                && o[0] < 224
                && !(o[0] == 100 && (64..=127).contains(&o[1]))
                && !(o[0] == 198 && (o[1] == 18 || o[1] == 19))
                && !(o[0] == 192 && o[1] == 0 && o[2] == 0)
        }
        IpAddr::V6(v) => {
            if let Some(v) = v.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v));
            }
            let s = v.segments();
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
                && s[0] != 0x2002
        }
    }
}
