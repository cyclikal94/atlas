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
/// The pad that hides a continuation's position, keyed by that token's own MAC.
const POSITION: &str = "atlas-delta-v2-siv";

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

/// The position counts every notice of the revision, including ones the reader can no longer
/// see, so in clear it would reveal how many of those a piece skipped. A pad shared by two
/// tokens would reveal the same through the XOR of their fields, so the token is sealed
/// synthetic-IV style: the MAC covers the plain position, and the pad is derived from that MAC.
/// Distinct positions get unrelated pads; an identical continuation gets an identical token.
fn pad(key: &str, actor: &str, device: &str, tag: &str) -> Result<u32> {
    let pad = mac(key, POSITION, actor, device, tag)?
        .finalize()
        .into_bytes();
    Ok(u32::from_be_bytes([pad[0], pad[1], pad[2], pad[3]]))
}

/// `d2.{boundary}.{expiry}.{revision}.{index}.{sealed position}.{mac}`, in the continuation
/// domain. The position is the pass bit above a 16-bit ordinal; the MAC is over the header and
/// the plain position.
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
        Pass::Upsert => 0,
        Pass::Remove => 1 << 16,
    };
    let position = pass | c.ordinal as u32;
    let header = format!(
        "d2.{:016x}.{:016x}.{:016x}.{:08x}",
        c.boundary, c.expiry, c.revision, c.index
    );
    let tag = signature(mac(
        key,
        CONTINUATION,
        actor,
        device,
        &format!("{header}.{position:08x}"),
    )?);
    let sealed = position ^ pad(key, actor, device, &tag)?;
    Ok(format!("{header}.{sealed:08x}.{tag}"))
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
            && [16, 16, 16, 8, 8, 64]
                .iter()
                .zip(&parts[1..])
                .all(|(len, part)| part.len() == *len),
        ErrorCode::ResyncRequired
    );
    ensure!(
        parts[1..6]
            .iter()
            .all(|s| s.bytes().all(|b| b.is_ascii_hexdigit())),
        ErrorCode::ResyncRequired
    );
    let hex32 =
        |s: &str| u32::from_str_radix(s, 16).map_err(|_| anyhow!(ErrorCode::ResyncRequired));
    // Unseal with the pad of the presented MAC, then authenticate the plain position: an altered
    // field or MAC yields a position the MAC does not cover.
    let position = hex32(parts[5])? ^ pad(key, actor, device, parts[6])?;
    let header = parts[..5].join(".");
    authentic(
        key,
        CONTINUATION,
        actor,
        device,
        &format!("{header}.{position:08x}.{}", parts[6]),
    )?;
    let number =
        |s: &str| i64::from_str_radix(s, 16).map_err(|_| anyhow!(ErrorCode::ResyncRequired));
    let mut c = Continuation {
        boundary: number(parts[1])?,
        expiry: number(parts[2])?,
        revision: number(parts[3])?,
        index: hex32(parts[4])?,
        pass: Pass::Upsert,
        ordinal: 0,
    };
    c.pass = match position >> 16 {
        0 => Pass::Upsert,
        1 => Pass::Remove,
        _ => return Err(anyhow!(ErrorCode::ResyncRequired)),
    };
    c.ordinal = (position & 0xffff) as usize;
    ensure!(c.revision > c.boundary, ErrorCode::ResyncRequired);
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn continuation(pass: Pass, ordinal: usize) -> Continuation {
        Continuation {
            boundary: 3,
            expiry: 1_800_000_000,
            revision: 9,
            index: 2,
            pass,
            ordinal,
        }
    }

    /// The position round-trips and is never carried in clear.
    #[test]
    fn positions_round_trip_masked() -> Result<()> {
        for (pass, ordinal, plain) in [
            (Pass::Upsert, 0, 0),
            (Pass::Upsert, 5, 5),
            (Pass::Remove, 0, 1 << 16),
            (Pass::Remove, 0xffff, (1 << 16) | 0xffff),
        ] {
            let c = continuation(pass, ordinal);
            let token = sign_continuation(KEY, "actor", "device", &c)?;
            assert_eq!(token.len(), 136);
            assert_eq!(verify_continuation(KEY, "actor", "device", &token)?, c);
            let field: u32 = u32::from_str_radix(token.split('.').nth(5).expect("position"), 16)?;
            assert_ne!(field, plain, "the position is masked");
        }
        assert!(
            sign_continuation(KEY, "actor", "device", &continuation(Pass::Upsert, 0x10000))
                .is_err()
        );
        Ok(())
    }

    fn field(token: &str) -> u32 {
        u32::from_str_radix(token.split('.').nth(5).expect("position"), 16).expect("hex")
    }

    /// R1 (review 28 October 2026): continuations sharing every public field but the position, as
    /// different bounds, a repeat after a visibility change or another chain to the same index
    /// produce, must not cancel to the XOR of their positions, which counts hidden notices. A
    /// retry of the same continuation still gets the same token.
    #[test]
    fn positions_do_not_cancel_across_tokens() -> Result<()> {
        let mut sealed = Vec::new();
        for pass in [Pass::Upsert, Pass::Remove] {
            for ordinal in (0..=40).chain([0xfffe, 0xffff]) {
                let c = continuation(pass, ordinal);
                let token = sign_continuation(KEY, "actor", "device", &c)?;
                assert_eq!(token, sign_continuation(KEY, "actor", "device", &c)?);
                assert_eq!(verify_continuation(KEY, "actor", "device", &token)?, c);
                let plain = if pass == Pass::Remove { 1 << 16 } else { 0 } | ordinal as u32;
                sealed.push((plain, field(&token)));
            }
        }
        for (i, (a, x)) in sealed.iter().enumerate() {
            for (b, y) in &sealed[i + 1..] {
                assert_ne!(x ^ y, a ^ b, "positions {a:#x} and {b:#x} cancel");
            }
        }
        Ok(())
    }
}
