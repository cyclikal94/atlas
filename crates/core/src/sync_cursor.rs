//! Authenticated delta positions. Short-lived row snapshots retain legacy UUID tokens.
use super::*;
use crate::error::ErrorCode;
use hmac::{Hmac, KeyInit, Mac};

pub(super) fn new_key() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
/// Delta cursors and continuations use distinct domains, so neither verifies as the other.
const DELTA: &str = "atlas-delta-v1";
const CONTINUATION: &str = "atlas-delta-v2";

fn mac(key: &str, domain: &str, actor: &str, device: &str, payload: &str) -> Result<Hmac<Sha256>> {
    ensure!(key.len() == 64, ErrorCode::ResyncRequired);
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes())
        .map_err(|_| anyhow!(ErrorCode::ResyncRequired))?;
    // Length-prefix the variable device ID, so its punctuation cannot alter framing.
    mac.update(format!("{domain}\n{actor}\n{}:{device}\n{payload}", device.len()).as_bytes());
    Ok(mac)
}
fn signature(mac: Hmac<Sha256>) -> String {
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
/// Check `token`'s trailing 64-hex MAC over everything before it, in `domain`.
fn authentic(key: &str, domain: &str, actor: &str, device: &str, token: &str) -> Result<()> {
    let (payload, hex) = token
        .rsplit_once('.')
        .ok_or_else(|| anyhow!(ErrorCode::ResyncRequired))?;
    ensure!(
        hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        ErrorCode::ResyncRequired
    );
    let signature = (0..64)
        .step_by(2)
        .map(|n| {
            u8::from_str_radix(&hex[n..n + 2], 16).map_err(|_| anyhow!(ErrorCode::ResyncRequired))
        })
        .collect::<Result<Vec<_>>>()?;
    mac(key, domain, actor, device, payload)?
        .verify_slice(&signature)
        .map_err(|_| anyhow!(ErrorCode::ResyncRequired))
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
    let signature = signature(mac(key, DELTA, actor, device, &payload)?);
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
    authentic(key, DELTA, actor, device, token)?;
    Ok((
        i64::from_str_radix(parts[1], 16).map_err(|_| anyhow!(ErrorCode::ResyncRequired))?,
        i64::from_str_radix(parts[2], 16).map_err(|_| anyhow!(ErrorCode::ResyncRequired))?,
    ))
}

/// Which part of a split revision a continuation resumes: its upserts, then its removals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pass {
    Upsert,
    Remove,
}

/// The rest of one revision after a piece (BE-Q23). It names the revision and a position in its
/// immutable, deterministically ordered notice list, never a resource identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Continuation {
    /// The delta boundary of the request that started the revision.
    pub boundary: i64,
    pub expiry: i64,
    pub revision: i64,
    /// The index of the piece this continuation returns.
    pub index: u32,
    pub pass: Pass,
    pub ordinal: usize,
}

/// `d2.{boundary}.{expiry}.{revision}.{index}.{pass}{ordinal}.{mac}`, in the continuation domain.
pub(super) fn sign_continuation(
    key: &str,
    actor: &str,
    device: &str,
    c: &Continuation,
) -> Result<String> {
    ensure!(
        c.boundary >= 0 && c.expiry >= 0 && c.revision > c.boundary && c.ordinal <= 0xffff,
        ErrorCode::InvalidValue
    );
    let pass = match c.pass {
        Pass::Upsert => 'u',
        Pass::Remove => 'r',
    };
    let payload = format!(
        "d2.{:016x}.{:016x}.{:016x}.{:08x}.{pass}{:04x}",
        c.boundary, c.expiry, c.revision, c.index, c.ordinal
    );
    let signature = signature(mac(key, CONTINUATION, actor, device, &payload)?);
    Ok(format!("{payload}.{signature}"))
}

/// Every parse or authentication failure is `resync_required`, as for a delta cursor.
pub(super) fn verify_continuation(
    key: &str,
    actor: &str,
    device: &str,
    token: &str,
) -> Result<Continuation> {
    let parts: Vec<_> = token.split('.').collect();
    ensure!(
        parts.len() == 7
            && parts[0] == "d2"
            && [16, 16, 16, 8, 5, 64]
                .iter()
                .zip(&parts[1..])
                .all(|(len, part)| part.len() == *len),
        ErrorCode::ResyncRequired
    );
    let (pass, ordinal) = parts[5].split_at(1);
    let pass = match pass {
        "u" => Pass::Upsert,
        "r" => Pass::Remove,
        _ => return Err(anyhow!(ErrorCode::ResyncRequired)),
    };
    ensure!(
        [parts[1], parts[2], parts[3], parts[4], ordinal]
            .iter()
            .all(|s| s.bytes().all(|b| b.is_ascii_hexdigit())),
        ErrorCode::ResyncRequired
    );
    authentic(key, CONTINUATION, actor, device, token)?;
    let number =
        |s: &str| i64::from_str_radix(s, 16).map_err(|_| anyhow!(ErrorCode::ResyncRequired));
    let c = Continuation {
        boundary: number(parts[1])?,
        expiry: number(parts[2])?,
        revision: number(parts[3])?,
        index: u32::from_str_radix(parts[4], 16).map_err(|_| anyhow!(ErrorCode::ResyncRequired))?,
        pass,
        ordinal: usize::from_str_radix(ordinal, 16)
            .map_err(|_| anyhow!(ErrorCode::ResyncRequired))?,
    };
    ensure!(c.revision > c.boundary, ErrorCode::ResyncRequired);
    Ok(c)
}
