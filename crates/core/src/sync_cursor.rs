//! Authenticated delta positions. Short-lived row snapshots retain legacy UUID tokens.
use super::*;
use crate::error::ErrorCode;
use hmac::{Hmac, KeyInit, Mac};

pub(super) fn new_key() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
fn mac(key: &str, actor: &str, device: &str, payload: &str) -> Result<Hmac<Sha256>> {
    ensure!(key.len() == 64, ErrorCode::ResyncRequired);
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes())
        .map_err(|_| anyhow!(ErrorCode::ResyncRequired))?;
    // Length-prefix the variable device ID, so its punctuation cannot alter framing.
    mac.update(
        format!(
            "atlas-delta-v1\n{actor}\n{}:{device}\n{payload}",
            device.len()
        )
        .as_bytes(),
    );
    Ok(mac)
}
pub(super) fn sign(
    key: &str,
    actor: &str,
    device: &str,
    boundary: i64,
    expiry: i64,
) -> Result<String> {
    ensure!(boundary >= 0 && expiry >= 0, ErrorCode::InvalidValue);
    let payload = format!("d1.{boundary:016x}.{expiry:016x}");
    let signature = mac(key, actor, device, &payload)?
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    Ok(format!("{payload}.{signature}"))
}
pub(super) fn verify(key: &str, actor: &str, device: &str, token: &str) -> Result<(i64, i64)> {
    let parts: Vec<_> = token.split('.').collect();
    ensure!(
        parts.len() == 4
            && parts[0] == "d1"
            && parts[1].len() == 16
            && parts[2].len() == 16
            && parts[3].len() == 64,
        ErrorCode::ResyncRequired
    );
    ensure!(
        parts[1..]
            .iter()
            .all(|s| s.bytes().all(|b| b.is_ascii_hexdigit())),
        ErrorCode::ResyncRequired
    );
    let signature = (0..64)
        .step_by(2)
        .map(|n| {
            u8::from_str_radix(&parts[3][n..n + 2], 16)
                .map_err(|_| anyhow!(ErrorCode::ResyncRequired))
        })
        .collect::<Result<Vec<_>>>()?;
    let payload = token
        .rsplit_once('.')
        .ok_or_else(|| anyhow!(ErrorCode::ResyncRequired))?
        .0;
    mac(key, actor, device, payload)?
        .verify_slice(&signature)
        .map_err(|_| anyhow!(ErrorCode::ResyncRequired))?;
    Ok((
        i64::from_str_radix(parts[1], 16).map_err(|_| anyhow!(ErrorCode::ResyncRequired))?,
        i64::from_str_radix(parts[2], 16).map_err(|_| anyhow!(ErrorCode::ResyncRequired))?,
    ))
}
