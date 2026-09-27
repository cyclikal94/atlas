//! BE-Q22: `configure_source`'s `connection` field distinguishes omission (preserve),
//! `Some(value)` (replace) and `disconnect: true` (clear) — `answers.md` Q22 "other
//! calendar commands" relies on a credential-free settings edit (timezone/enabled only)
//! not clearing a stored connection. Also covers the review's R1 correction: the default
//! (`disconnect: false`) receipt hash must stay byte-identical to the pre-change format so
//! an existing receipt still replays, while an explicit `disconnect: true` remains its own
//! distinct hashed operation.
use crate::support::sharing::policy;
use anyhow::Result;
use atlas_core::{Store, calendars::*, policy::Policy};
use sha2::{Digest, Sha256};
use uuid::Uuid;
fn id() -> String {
    Uuid::new_v4().to_string()
}
async fn account(s: &Store) -> Result<String> {
    let a = id();
    s.add_account(&a, &format!("cal-cfg-{}", Uuid::new_v4().simple()), "test")
        .await?;
    Ok(a)
}
async fn new_source(
    s: &Store,
    a: &str,
    connection: Option<&str>,
    initial_policy: Option<Policy>,
) -> Result<String> {
    let source = id();
    s.calendar_command(
        a,
        &id(),
        &CalendarCommand::CreateSource {
            id: source.clone(),
            label: "Calendar".into(),
            timezone: "UTC".into(),
            connection: connection.map(str::to_owned),
            initial_policy,
        },
    )
    .await?;
    Ok(source)
}
async fn connection(s: &Store, id: &str) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT connection FROM calendar_sources WHERE id=$1")
            .bind(id)
            .fetch_one(&s.pool)
            .await?,
    )
}
async fn version(s: &Store, id: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT version FROM resources WHERE id=$1")
            .bind(id)
            .fetch_one(&s.pool)
            .await?,
    )
}
/// Independently reproduces the pre-`disconnect` receipt digest for a `ConfigureSource`
/// command whose `connection` is `None` (masking is a no-op on `None`, so this needs no
/// `secret_identity` replica): the exact JSON field order/content the struct serialised to
/// before this field existed, hashed the same way `receipt_digest` does. Kept independent of
/// `atlas_core`'s own `Serialize` impl, rather than calling it, so this test still catches a
/// regression instead of trivially matching whatever the implementation currently emits.
fn pre_change_digest(id: &str, expected_version: i64, timezone: &str, enabled: bool) -> String {
    let json = format!(
        r#"{{"kind":"configure_source","id":"{id}","expected_version":{expected_version},"timezone":"{timezone}","connection":null,"enabled":{enabled}}}"#
    );
    let payload = format!("calendar-command-v1:{json}");
    Sha256::digest(format!("atlas-command-v1\n{payload}"))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn configure(
    id: &str,
    expected_version: i64,
    timezone: &str,
    connection: Option<&str>,
    disconnect: bool,
    enabled: bool,
) -> CalendarCommand {
    CalendarCommand::ConfigureSource {
        id: id.into(),
        expected_version,
        timezone: timezone.into(),
        connection: connection.map(str::to_owned),
        disconnect,
        enabled,
    }
}
async fn scenario(s: Store) -> Result<()> {
    let a = account(&s).await?;

    // Case 1: omission preserves the stored connection, both for a timezone-only and an
    // enabled-only credential-free edit — the two offline-queued shapes `answers.md` names.
    let source = new_source(&s, &a, Some("v1:fp1:sealed-initial"), None).await?;
    s.calendar_command(
        &a,
        &id(),
        &configure(
            &source,
            version(&s, &source).await?,
            "Europe/London",
            None,
            false,
            true,
        ),
    )
    .await?;
    assert_eq!(
        connection(&s, &source).await?.as_deref(),
        Some("v1:fp1:sealed-initial"),
        "timezone-only edit must not clear the stored connection"
    );
    s.calendar_command(
        &a,
        &id(),
        &configure(
            &source,
            version(&s, &source).await?,
            "Europe/London",
            None,
            false,
            false,
        ),
    )
    .await?;
    assert_eq!(
        connection(&s, &source).await?.as_deref(),
        Some("v1:fp1:sealed-initial"),
        "enabled-only edit must not clear the stored connection"
    );

    // Case 2: explicit disconnect clears it.
    s.calendar_command(
        &a,
        &id(),
        &configure(
            &source,
            version(&s, &source).await?,
            "Europe/London",
            None,
            true,
            false,
        ),
    )
    .await?;
    assert_eq!(connection(&s, &source).await?, None);

    // Case 3: explicit replacement still works, and is not swallowed by (1)'s COALESCE.
    s.calendar_command(
        &a,
        &id(),
        &configure(
            &source,
            version(&s, &source).await?,
            "UTC",
            Some("v1:fp2:sealed-replacement"),
            false,
            true,
        ),
    )
    .await?;
    assert_eq!(
        connection(&s, &source).await?.as_deref(),
        Some("v1:fp2:sealed-replacement")
    );

    // Case 4: connection + disconnect together is contradictory and rejected, with no
    // partial effect (the transaction rolls back before the update runs).
    let before = version(&s, &source).await?;
    assert_eq!(
        s.calendar_command(
            &a,
            &id(),
            &configure(&source, before, "UTC", Some("v1:fp3:ignored"), true, true),
        )
        .await
        .unwrap_err()
        .to_string(),
        "invalid_value"
    );
    assert_eq!(
        connection(&s, &source).await?.as_deref(),
        Some("v1:fp2:sealed-replacement"),
        "a rejected contradictory command must not change stored state"
    );
    assert_eq!(
        version(&s, &source).await?,
        before,
        "a rejected contradictory command must not change the resource version"
    );

    // Case 5: idempotent replay of the identical command returns the same receipt with no
    // second effect; replaying the same operation ID with different content conflicts.
    let op = id();
    let cmd = configure(
        &source,
        version(&s, &source).await?,
        "UTC",
        None,
        true,
        false,
    );
    let first = s.calendar_command(&a, &op, &cmd).await?;
    let replay = s.calendar_command(&a, &op, &cmd).await?;
    assert_eq!(first, replay);
    assert_eq!(
        connection(&s, &source).await?,
        None,
        "the replayed disconnect applied exactly once"
    );
    let conflicting = configure(
        &source,
        version(&s, &source).await?,
        "UTC",
        None,
        false,
        false,
    );
    assert_eq!(
        s.calendar_command(&a, &op, &conflicting)
            .await
            .unwrap_err()
            .to_string(),
        "operation_conflict"
    );

    // Case 6: ownership is unchanged — a collaborator with edit access still cannot touch
    // connection settings, whichever combination of connection/disconnect is sent.
    let b = account(&s).await?;
    let shared = new_source(
        &s,
        &a,
        Some("v1:fp4:owner-only"),
        Some(policy(&[(&b, true)])),
    )
    .await?;
    assert_eq!(
        s.calendar_command(
            &b,
            &id(),
            &configure(
                &shared,
                version(&s, &shared).await?,
                "UTC",
                None,
                false,
                true
            ),
        )
        .await
        .unwrap_err()
        .to_string(),
        "forbidden"
    );
    assert_eq!(
        connection(&s, &shared).await?.as_deref(),
        Some("v1:fp4:owner-only")
    );

    // Case 7: isolate `disconnect` itself in the conflict check. Same operation ID, same
    // expected_version/timezone/connection/enabled — only `disconnect` differs — so a hash
    // that accidentally dropped the field from both sides could not masquerade this as a
    // real replay (unlike a check that also lets `expected_version` drift between the two
    // calls, which would conflict for that reason regardless of `disconnect`).
    let iso_source = new_source(&s, &a, Some("v1:fp6:isolated"), None).await?;
    let iso_version = version(&s, &iso_source).await?;
    let iso_op = id();
    let iso_first = configure(&iso_source, iso_version, "UTC", None, false, true);
    s.calendar_command(&a, &iso_op, &iso_first).await?;
    let iso_flipped = configure(&iso_source, iso_version, "UTC", None, true, true);
    assert_eq!(
        s.calendar_command(&a, &iso_op, &iso_flipped)
            .await
            .unwrap_err()
            .to_string(),
        "operation_conflict",
        "the same operation ID with only `disconnect` flipped must conflict, not replay"
    );
    assert_eq!(
        connection(&s, &iso_source).await?.as_deref(),
        Some("v1:fp6:isolated"),
        "a rejected conflicting replay must not itself clear the connection"
    );

    // Case 8: a receipt computed before `disconnect` existed (no such field in its stored
    // hash) must still replay to its stored revision when the equivalent default/false
    // command arrives today, with no further write — the default case's serialised shape is
    // required to stay byte-identical to the pre-change format.
    let legacy_source = new_source(&s, &a, Some("v1:fp7:pre-change"), None).await?;
    let legacy_version = version(&s, &legacy_source).await?;
    let digest = pre_change_digest(&legacy_source, legacy_version, "Europe/London", true);
    let seeded_revision: i64 = sqlx::query_scalar("SELECT revision FROM sync_clock WHERE id=1")
        .fetch_one(&s.pool)
        .await?;
    let legacy_op = id();
    sqlx::query(
        "INSERT INTO receipts(account_id,operation_id,payload,revision,digest_version) VALUES ($1,$2,$3,$4,1)",
    )
    .bind(&a)
    .bind(&legacy_op)
    .bind(&digest)
    .bind(seeded_revision)
    .execute(&s.pool)
    .await?;
    let replayed = s
        .calendar_command(
            &a,
            &legacy_op,
            &configure(
                &legacy_source,
                legacy_version,
                "Europe/London",
                None,
                false,
                true,
            ),
        )
        .await?;
    assert_eq!(
        replayed, seeded_revision,
        "a pre-change receipt must replay to its stored revision"
    );
    assert_eq!(
        connection(&s, &legacy_source).await?.as_deref(),
        Some("v1:fp7:pre-change"),
        "replaying a matched pre-change receipt must not perform a second write"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT revision FROM sync_clock WHERE id=1")
            .fetch_one(&s.pool)
            .await?,
        seeded_revision,
        "replaying a matched receipt must not advance the sync clock"
    );

    Ok(())
}
#[tokio::test]
async fn calendars_configure_source() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let s = Store::connect(&url).await?;
    s.migrate().await?;
    scenario(s).await
}
