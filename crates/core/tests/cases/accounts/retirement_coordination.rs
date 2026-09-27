//! Coordination of the retirement with every writer of an approved-state member, on both engines,
//! by forcing named interleavings (checks y, aa–al). These are deterministic in-process schedules
//! that use test-only hooks; the release-shaped binary is exercised by `scripts/`.
//!
//! "Writer first" and "retirement first" are engine-specific schedules (see `support::schedule`);
//! the ledger and row assertions are identical on both engines.
use anyhow::{Context, Result};
use atlas_core::{Store, calendars::ReminderCommand, operations::Outcome};
use std::time::Duration;
use tokio::task::JoinHandle;

use crate::support::devices::{
    FAR, add_registration, add_session, add_session_named, add_subscription, expected_token, id,
    ledger_rows, populate, snapshot,
};
use crate::support::resource_commands::{account, fixture};
use crate::support::schedule::{
    BEFORE_BEGIN, RETIREMENT_FIRST, assert_blocked, assert_isolation, pause, postgres,
    writer_first_point,
};

const NOW: i64 = 1000;
const PHONE: &str = "phone";
/// Past the retention cutoff of a registration last seen at 1 (90 days of retention).
const CLEANUP_NOW: i64 = 1 + 90 * 86400 + 1;

async fn setup(raise: bool) -> Result<(tempfile::TempDir, Store, String)> {
    let (dir, s) = fixture().await?;
    if raise {
        s.hooks().raise_isolation();
    }
    let a = account(&s).await?;
    Ok((dir, s, a))
}

/// The ordinary session revoke's own statement (`sessions.rs`): one autocommit `DELETE`.
async fn revoke(s: &Store, a: &str, session: &str) -> Result<bool> {
    let removed: Option<String> = sqlx::query_scalar(
        "DELETE FROM sessions WHERE session_id=$1 AND account_id=$2 RETURNING token_hash",
    )
    .bind(session)
    .bind(a)
    .fetch_optional(&s.pool)
    .await?;
    Ok(removed.is_some())
}

fn spawn_revoke(s: &Store, a: &str, session: &str) -> JoinHandle<Result<bool>> {
    let (s, a, session) = (s.clone(), a.to_owned(), session.to_owned());
    tokio::spawn(async move { revoke(&s, &a, &session).await })
}

fn outcomes(rows: &[(String, String, String)]) -> Vec<&str> {
    rows.iter().map(|r| r.2.as_str()).collect()
}

// ---------------------------------------------------------------- (y) ordinary revoke

/// (y) `D` has exactly one live session; the revoke is authenticated by it.
async fn revoke_writer_first(raise: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    let s1 = add_session(&s, &a, PHONE, FAR).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let paused = pause(&s, &a, PHONE, &id(), &token, NOW, writer_first_point()).await;
    assert!(
        revoke(&s, &a, &s1).await?,
        "the revoke removed the row: 204"
    );
    let done = paused.finish().await?;
    assert_eq!(done.outcome, Outcome::Superseded, "the recompute is empty");
    assert_eq!(outcomes(&ledger_rows(&s, &a).await?), ["superseded"]);
    assert_isolation(&s, raise);
    Ok(())
}

async fn revoke_retirement_first(raise: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    let s1 = add_session(&s, &a, PHONE, FAR).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let paused = pause(&s, &a, PHONE, &id(), &token, NOW, RETIREMENT_FIRST).await;
    let mut writer = spawn_revoke(&s, &a, &s1);
    assert_blocked(&s, paused.point, &mut writer).await?;
    let done = paused.finish().await?;
    assert_eq!(done.outcome, Outcome::ConfirmedApplied);
    assert!(
        !writer.await??,
        "the revoke found nothing: 404, and is credited nothing"
    );
    assert_eq!(outcomes(&ledger_rows(&s, &a).await?), ["confirmed_applied"]);
    assert!(snapshot(&s, &a, PHONE).await?.sessions.is_empty());
    assert_isolation(&s, raise);
    Ok(())
}

#[tokio::test]
async fn revoke_first_is_superseded() -> Result<()> {
    revoke_writer_first(false).await
}

#[tokio::test]
async fn retirement_first_beats_the_revoke() -> Result<()> {
    revoke_retirement_first(false).await
}

/// (y) PostgreSQL interleaving negative control. Without the row locks an unlocked autocommit
/// `DELETE` commits while the retirement is paused after its compare; the retirement's own
/// `DELETE` then affects 0 of 1 locked rows, and its assertion rolls it back: no ledger row and
/// an internal error. With the locks, the same schedule blocks the writer instead.
#[tokio::test]
async fn negative_control_shows_the_assertion_and_the_locks_matter() -> Result<()> {
    if !postgres() {
        return Ok(()); // SQLite's engine, not row locks, serialises writers: see (ak) and (al).
    }
    let (_d, s, a) = setup(false).await?;
    s.hooks().row_locking(false);
    let s1 = add_session(&s, &a, PHONE, FAR).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let paused = pause(&s, &a, PHONE, &id(), &token, NOW, "retire.before_effects").await;
    assert!(
        revoke(&s, &a, &s1).await?,
        "unlocked, the revoke commits at once"
    );
    let error = paused.finish().await.unwrap_err();
    assert_eq!(error.to_string(), "internal_error");
    assert!(
        ledger_rows(&s, &a).await?.is_empty(),
        "no ledger row on a discrepancy"
    );

    // The same schedule with the locks restored blocks the writer.
    s.hooks().row_locking(true);
    let s2 = add_session(&s, &a, PHONE, FAR).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let paused = pause(&s, &a, PHONE, &id(), &token, NOW, "retire.before_effects").await;
    let mut writer = spawn_revoke(&s, &a, &s2);
    assert_blocked(&s, paused.point, &mut writer).await?;
    assert_eq!(paused.finish().await?.outcome, Outcome::ConfirmedApplied);
    assert!(!writer.await??);
    Ok(())
}

// ---------------------------------------------------------------- (aa) partial removal

async fn partial_removal_writer_first(raise: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    let members = populate(&s, &a, PHONE).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let paused = pause(&s, &a, PHONE, &id(), &token, NOW, writer_first_point()).await;
    assert!(revoke(&s, &a, &members.sessions[0]).await?);
    let after_writer = snapshot(&s, &a, PHONE).await?;
    let done = paused.finish().await?;
    assert_eq!(done.outcome, Outcome::RejectedStale);
    let after = snapshot(&s, &a, PHONE).await?;
    assert_eq!(after, after_writer, "(af) nothing else changed");
    assert_eq!(after.sessions, [members.sessions[1].clone()]);
    assert_eq!(after.handoffs, std::slice::from_ref(&members.handoff));
    assert_eq!(after.subscriptions.len(), 1);
    assert_eq!(after.registrations, std::slice::from_ref(&members.key));
    assert_isolation(&s, raise);
    Ok(())
}

async fn partial_removal_retirement_first(raise: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    let members = populate(&s, &a, PHONE).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let paused = pause(&s, &a, PHONE, &id(), &token, NOW, RETIREMENT_FIRST).await;
    let mut writer = spawn_revoke(&s, &a, &members.sessions[0]);
    assert_blocked(&s, paused.point, &mut writer).await?;
    assert_eq!(paused.finish().await?.outcome, Outcome::ConfirmedApplied);
    assert!(!writer.await??, "the revoke gets 404");
    let after = snapshot(&s, &a, PHONE).await?;
    assert!(after.sessions.is_empty() && after.handoffs.is_empty());
    assert!(after.registrations.is_empty());
    // The subscription is deactivated by the retirement's own effect, secret erased.
    assert_eq!(after.subscriptions.len(), 1);
    assert_eq!(after.subscriptions[0].0, members.subscription);
    assert_eq!(
        (after.subscriptions[0].2, after.subscriptions[0].3.as_str()),
        (0, "")
    );
    assert_isolation(&s, raise);
    Ok(())
}

#[tokio::test]
async fn partial_removal_first_is_stale() -> Result<()> {
    partial_removal_writer_first(false).await
}

#[tokio::test]
async fn partial_removal_second_removes_everything() -> Result<()> {
    partial_removal_retirement_first(false).await
}

// ---------------------------------------------------------------- (ab) retention cleanup

async fn cleanup_first_supersedes(raise: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    add_registration(&s, &a, PHONE).await?; // a native client that syncs but holds no session
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let paused = pause(&s, &a, PHONE, &id(), &token, NOW, writer_first_point()).await;
    s.collect_expired(CLEANUP_NOW).await?;
    assert!(
        snapshot(&s, &a, PHONE).await?.registrations.is_empty(),
        "the cleanup removed K1"
    );
    assert_eq!(paused.finish().await?.outcome, Outcome::Superseded);
    assert_isolation(&s, raise);
    Ok(())
}

async fn retirement_first_beats_cleanup(raise: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    add_registration(&s, &a, PHONE).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let paused = pause(&s, &a, PHONE, &id(), &token, NOW, RETIREMENT_FIRST).await;
    let mut cleanup = {
        let s = s.clone();
        tokio::spawn(async move { s.collect_expired(CLEANUP_NOW).await })
    };
    assert_blocked(&s, paused.point, &mut cleanup).await?;
    assert_eq!(paused.finish().await?.outcome, Outcome::ConfirmedApplied);
    cleanup.await??; // its candidate query finds no row and raises no error
    assert!(snapshot(&s, &a, PHONE).await?.registrations.is_empty());
    assert_isolation(&s, raise);
    Ok(())
}

async fn cleanup_beside_a_session_is_stale(raise: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    let s1 = add_session(&s, &a, PHONE, FAR).await?;
    add_registration(&s, &a, PHONE).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let paused = pause(&s, &a, PHONE, &id(), &token, NOW, writer_first_point()).await;
    s.collect_expired(CLEANUP_NOW).await?;
    let after_cleanup = snapshot(&s, &a, PHONE).await?;
    assert_eq!(paused.finish().await?.outcome, Outcome::RejectedStale);
    let after = snapshot(&s, &a, PHONE).await?;
    assert_eq!(after, after_cleanup);
    assert_eq!(after.sessions, [s1], "S1 intact");
    assert_isolation(&s, raise);
    Ok(())
}

#[tokio::test]
async fn cleanup_first_is_superseded() -> Result<()> {
    cleanup_first_supersedes(false).await
}

#[tokio::test]
async fn retirement_first_makes_the_cleanup_a_no_op() -> Result<()> {
    retirement_first_beats_cleanup(false).await
}

#[tokio::test]
async fn cleanup_of_one_member_is_stale() -> Result<()> {
    cleanup_beside_a_session_is_stale(false).await
}

// ---------------------------------------------------------------- (ac) expiry sweeps

/// One chunk statement covers both members of `D` (both expire at 1500, after the retirement's
/// `now` of 1000 and before the sweep's 2000).
async fn expired_pair(s: &Store, a: &str, with_registration: bool) -> Result<[String; 2]> {
    let mut ids = [id(), id()];
    ids.sort();
    for session in &ids {
        add_session_named(s, a, PHONE, session, 1500).await?;
    }
    if with_registration {
        add_registration(s, a, PHONE).await?;
    }
    Ok(ids)
}

async fn sweep_first(raise: bool) -> Result<()> {
    for (with_registration, expected) in
        [(false, Outcome::Superseded), (true, Outcome::RejectedStale)]
    {
        let (_d, s, a) = setup(raise).await?;
        expired_pair(&s, &a, with_registration).await?;
        let token = expected_token(&s, &a, PHONE, NOW).await?;
        let paused = pause(&s, &a, PHONE, &id(), &token, NOW, writer_first_point()).await;
        s.collect_expired(2000).await?;
        let after_sweep = snapshot(&s, &a, PHONE).await?;
        assert!(after_sweep.sessions.is_empty());
        assert_eq!(paused.finish().await?.outcome, expected);
        assert_eq!(snapshot(&s, &a, PHONE).await?, after_sweep);
        assert_isolation(&s, raise);
    }
    Ok(())
}

async fn retirement_first_beats_sweep(raise: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    expired_pair(&s, &a, true).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let paused = pause(&s, &a, PHONE, &id(), &token, NOW, RETIREMENT_FIRST).await;
    let mut sweep = {
        let s = s.clone();
        tokio::spawn(async move { s.collect_expired(2000).await })
    };
    assert_blocked(&s, paused.point, &mut sweep).await?;
    assert_eq!(paused.finish().await?.outcome, Outcome::ConfirmedApplied);
    sweep.await??;
    assert!(snapshot(&s, &a, PHONE).await?.sessions.is_empty());
    assert_isolation(&s, raise);
    Ok(())
}

#[tokio::test]
async fn a_sweep_first_leaves_nothing_or_something_else() -> Result<()> {
    sweep_first(false).await
}

#[tokio::test]
async fn retirement_first_makes_the_sweep_a_no_op() -> Result<()> {
    retirement_first_beats_sweep(false).await
}

/// (ac), (ah) PostgreSQL: a real `40P01`. A test transaction holds the later member while the
/// retirement locks the earlier one and waits; the test transaction then asks for the earlier
/// member. PostgreSQL aborts the transaction that has waited longest (the retirement), which is
/// retried as a unit and writes exactly one ledger row.
async fn retry_after_a_deadlock(raise: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    let [first, second] = expired_pair_with_far(&s, &a).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let mut holder = s.pool.begin().await?;
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await?;
    sqlx::query("SELECT session_id FROM sessions WHERE session_id=$1 FOR UPDATE")
        .bind(&second)
        .fetch_all(&mut *holder)
        .await?;
    let retirement = {
        let (s, a, token) = (s.clone(), a.clone(), token.clone());
        tokio::spawn(async move { s.retire_device(&a, PHONE, &id(), &token, NOW).await })
    };
    // The retirement has locked `first` and waits for `second`.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))",
            )
            .bind(holder_pid)
            .fetch_one(&s.pool)
            .await?;
            if waiting > 0 {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await??;
    let closing = tokio::spawn(async move {
        sqlx::query("SELECT session_id FROM sessions WHERE session_id=$1 FOR UPDATE")
            .bind(first)
            .fetch_all(&mut *holder)
            .await?;
        holder.commit().await?;
        Ok::<_, anyhow::Error>(())
    });
    let done = tokio::time::timeout(Duration::from_secs(30), retirement).await???;
    closing
        .await?
        .context("the test transaction must not be the deadlock victim")?;
    assert_eq!(done.outcome, Outcome::ConfirmedApplied);
    assert!(
        s.hooks().attempts("retire") >= 2,
        "the deadlock forced a retry"
    );
    assert_eq!(outcomes(&ledger_rows(&s, &a).await?), ["confirmed_applied"]);
    assert!(snapshot(&s, &a, PHONE).await?.sessions.is_empty());
    assert_isolation(&s, raise);
    Ok(())
}

/// Two live members with known relative order (the retirement locks by ascending `session_id`).
async fn expired_pair_with_far(s: &Store, a: &str) -> Result<[String; 2]> {
    let mut ids = [id(), id()];
    ids.sort();
    for session in &ids {
        add_session_named(s, a, PHONE, session, FAR).await?;
    }
    Ok(ids)
}

#[tokio::test]
async fn a_deadlock_is_retried_as_a_unit() -> Result<()> {
    if !postgres() {
        return Ok(());
    }
    retry_after_a_deadlock(false).await
}

/// (ac), (ah) SQLite: `SQLITE_BUSY` from `begin_serial()`'s first statement (busy timeout 0 and a
/// competing write transaction), retried as a unit with exactly one ledger row; and, when the
/// competitor outlasts all 16 attempts, exhaustion leaves no row and no effect.
#[tokio::test]
async fn busy_is_retried_then_exhausted() -> Result<()> {
    if postgres() {
        return Ok(());
    }
    let (_dir, url) = crate::support::database::database_url().await?;
    let s = Store::connect(&url).await?;
    s.migrate().await?;
    let fast = Store::connect_with_busy_timeout(&url, 0).await?;
    let a = account(&s).await?;
    populate(&s, &a, PHONE).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;

    // Retried: the competitor releases while the retirement is backing off.
    let held = s.begin_serial().await?;
    let retirement = {
        let (fast, a, token) = (fast.clone(), a.clone(), token.clone());
        tokio::spawn(async move { fast.retire_device(&a, PHONE, &id(), &token, NOW).await })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while fast.hooks().attempts("retire") < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    held.rollback().await?;
    assert_eq!(retirement.await??.outcome, Outcome::ConfirmedApplied);
    assert!(fast.hooks().attempts("retire") >= 3);
    assert_eq!(outcomes(&ledger_rows(&s, &a).await?), ["confirmed_applied"]);

    // Exhausted: the competitor outlasts every attempt.
    populate(&s, &a, PHONE).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let before = snapshot(&s, &a, PHONE).await?;
    let attempts_before = fast.hooks().attempts("retire");
    let held = s.begin_serial().await?;
    let error = fast
        .retire_device(&a, PHONE, &id(), &token, NOW)
        .await
        .unwrap_err();
    held.rollback().await?;
    let code = error
        .downcast_ref::<sqlx::Error>()
        .and_then(|e| e.as_database_error())
        .and_then(|e| e.code().map(|c| c.to_string()));
    assert_eq!(
        code.as_deref(),
        Some("5"),
        "SQLITE_BUSY surfaces (HTTP maps it to 503)"
    );
    assert_eq!(fast.hooks().attempts("retire") - attempts_before, 16);
    assert_eq!(
        ledger_rows(&s, &a).await?.len(),
        1,
        "no row for the exhausted attempt"
    );
    assert_eq!(snapshot(&s, &a, PHONE).await?, before, "no effect");
    // The identical call then succeeds: the client keeps its envelope and repeats it.
    let done = fast.retire_device(&a, PHONE, &id(), &token, NOW).await?;
    assert_eq!(done.outcome, Outcome::ConfirmedApplied);
    Ok(())
}

/// (ah) PostgreSQL: a real `40001` raised inside the transaction is retried as a unit; a
/// persistent one exhausts after 16 attempts with no ledger row and no effect.
async fn serialisation_failures(raise: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    const FAIL: &str = "DO $$ BEGIN RAISE EXCEPTION 'induced' USING ERRCODE = '40001'; END $$";
    populate(&s, &a, PHONE).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;

    s.hooks().inject_sql_times("retire.before_effects", FAIL, 1);
    let done = s.retire_device(&a, PHONE, &id(), &token, NOW).await?;
    assert_eq!(done.outcome, Outcome::ConfirmedApplied);
    assert_eq!(s.hooks().attempts("retire"), 2);
    assert_eq!(outcomes(&ledger_rows(&s, &a).await?), ["confirmed_applied"]);

    populate(&s, &a, PHONE).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let before = snapshot(&s, &a, PHONE).await?;
    let attempts_before = s.hooks().attempts("retire");
    s.hooks().inject_sql("retire.before_effects", FAIL);
    let error = s
        .retire_device(&a, PHONE, &id(), &token, NOW)
        .await
        .unwrap_err();
    let code = error
        .downcast_ref::<sqlx::Error>()
        .and_then(|e| e.as_database_error())
        .and_then(|e| e.code().map(|c| c.to_string()));
    assert_eq!(code.as_deref(), Some("40001"));
    assert_eq!(s.hooks().attempts("retire") - attempts_before, 16);
    assert_eq!(ledger_rows(&s, &a).await?.len(), 1);
    assert_eq!(snapshot(&s, &a, PHONE).await?, before);
    s.hooks().inject_sql_times("retire.before_effects", FAIL, 0);
    assert_eq!(
        s.retire_device(&a, PHONE, &id(), &token, NOW)
            .await?
            .outcome,
        Outcome::ConfirmedApplied
    );
    assert_isolation(&s, raise);
    Ok(())
}

#[tokio::test]
async fn serialisation_failures_are_retried_then_exhausted() -> Result<()> {
    if !postgres() {
        return Ok(());
    }
    serialisation_failures(false).await
}

// ---------------------------------------------------------------- (ad) registration writers

async fn registration_creation_writer_first(raise: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    add_session(&s, &a, PHONE, FAR).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let paused = pause(&s, &a, PHONE, &id(), &token, NOW, writer_first_point()).await;
    s.sync(&a, PHONE, None, 200, NOW).await?; // creates the registration
    let after_writer = snapshot(&s, &a, PHONE).await?;
    assert_eq!(after_writer.registrations.len(), 1);
    assert_eq!(paused.finish().await?.outcome, Outcome::RejectedStale);
    assert_eq!(snapshot(&s, &a, PHONE).await?, after_writer);
    assert_isolation(&s, raise);
    Ok(())
}

async fn registration_creation_retirement_first(raise: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    let session = add_session(&s, &a, PHONE, FAR).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let paused = pause(&s, &a, PHONE, &id(), &token, NOW, RETIREMENT_FIRST).await;
    let mut writer = {
        let (s, a) = (s.clone(), a.clone());
        tokio::spawn(async move { s.sync(&a, PHONE, None, 200, NOW).await })
    };
    assert_blocked(&s, paused.point, &mut writer).await?;
    assert_eq!(paused.finish().await?.outcome, Outcome::ConfirmedApplied);
    writer.await??;
    let after = snapshot(&s, &a, PHONE).await?;
    assert!(!after.sessions.contains(&session));
    // The registration is the new incarnation's, created after the retirement committed.
    assert_eq!(after.registrations.len(), 1);
    assert_isolation(&s, raise);
    Ok(())
}

/// The `last_seen` update of an existing registration, as a real delta `sync`.
async fn last_seen_setup(s: &Store, a: &str) -> Result<(String, String)> {
    add_session(s, a, PHONE, FAR).await?;
    let page = s.sync(a, PHONE, None, 200, NOW).await?;
    let key = snapshot(s, a, PHONE).await?.registrations.remove(0);
    Ok((key, page.next_cursor))
}
const LATER: i64 = NOW + 200_000; // more than a day after `last_seen`: the update runs

async fn last_seen_writer_first(raise: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    let (_, cursor) = last_seen_setup(&s, &a).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let paused = pause(&s, &a, PHONE, &id(), &token, NOW, writer_first_point()).await;
    s.sync(&a, PHONE, Some(&cursor), 200, LATER).await?;
    // `last_seen` is not state, so the token still matches.
    assert_eq!(paused.finish().await?.outcome, Outcome::ConfirmedApplied);
    assert!(snapshot(&s, &a, PHONE).await?.registrations.is_empty());
    assert_isolation(&s, raise);
    Ok(())
}

async fn last_seen_retirement_first(raise: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    let (key, cursor) = last_seen_setup(&s, &a).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let paused = pause(&s, &a, PHONE, &id(), &token, NOW, RETIREMENT_FIRST).await;
    let mut writer = {
        let (s, a) = (s.clone(), a.clone());
        tokio::spawn(async move { s.sync(&a, PHONE, Some(&cursor), 200, LATER).await })
    };
    assert_blocked(&s, paused.point, &mut writer).await?;
    assert_eq!(paused.finish().await?.outcome, Outcome::ConfirmedApplied);
    // The old cursor is dead with the old registration: recovery, not an error of the protocol.
    let _ = writer.await?;
    let after = snapshot(&s, &a, PHONE).await?;
    assert!(
        after.registrations.iter().all(|k| *k != key),
        "the retired registration is gone"
    );
    assert_isolation(&s, raise);
    Ok(())
}

#[tokio::test]
async fn registration_creation_first_is_stale() -> Result<()> {
    registration_creation_writer_first(false).await
}

#[tokio::test]
async fn retirement_first_orders_registration_creation_after_it() -> Result<()> {
    registration_creation_retirement_first(false).await
}

#[tokio::test]
async fn last_seen_update_first_does_not_change_the_state() -> Result<()> {
    last_seen_writer_first(false).await
}

#[tokio::test]
async fn retirement_first_retires_the_registration_the_update_targeted() -> Result<()> {
    last_seen_retirement_first(false).await
}

// ---------------------------------------------------------------- (ae) begin_serial writers

/// (ae) Subscription set: the real `reminder_command` writer, which runs inside `begin_serial()`.
/// The other `begin_serial()` writers of a member (session issue, handoff create and consume,
/// password change) live in the server crate and are run from there, against the real routes:
/// `crates/server/tests/cases/writers.rs`. Activation-grant writers wait for BE-Q19.
fn set_subscription(id: &str, expected_version: i64, secret: &str) -> ReminderCommand {
    ReminderCommand::SetSubscription {
        id: id.to_owned(),
        expected_version,
        device_id: PHONE.to_owned(),
        transport: "ntfy".to_owned(),
        secret: secret.to_owned(),
        enabled: true,
    }
}

/// Writer first (before the retirement begins, on both engines): the subscription's version, or a
/// new subscription, is a changed member, so the retirement is `rejected_stale` and nothing else
/// changes. `resubscribe` re-sets the existing subscription (its version moves 1 → 2); otherwise a
/// new subscription is added.
async fn subscription_set_writer_first(raise: bool, resubscribe: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    let session = add_session(&s, &a, PHONE, FAR).await?;
    let existing = add_subscription(&s, &a, PHONE).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let op = id();
    let paused = pause(&s, &a, PHONE, &op, &token, NOW, BEFORE_BEGIN).await;
    let command = if resubscribe {
        set_subscription(&existing, 1, "rotated")
    } else {
        set_subscription(&id(), 0, "added")
    };
    s.reminder_command(&a, &id(), &command).await?;
    let after_writer = snapshot(&s, &a, PHONE).await?;
    let done = paused.finish().await?;
    assert_eq!(
        done.outcome,
        Outcome::RejectedStale,
        "resubscribe={resubscribe}"
    );
    let after = snapshot(&s, &a, PHONE).await?;
    assert_eq!(after, after_writer, "(af) nothing else changed");
    assert_eq!(after.sessions, [session]);
    if resubscribe {
        assert_eq!(after.subscriptions.len(), 1);
        // `(id, version, active, secret)`: version moved 1 → 2, still active, secret rotated.
        assert_eq!(
            (
                after.subscriptions[0].1,
                after.subscriptions[0].2,
                after.subscriptions[0].3.as_str()
            ),
            (2, 1, "rotated")
        );
    } else {
        assert_eq!(after.subscriptions.len(), 2);
        assert!(after.subscriptions.iter().all(|row| row.2 == 1));
    }
    assert_eq!(outcomes(&ledger_rows(&s, &a).await?), ["rejected_stale"]);
    assert_isolation(&s, raise);
    Ok(())
}

/// Retirement first (held after its locking reads): the writer waits for the commit. A re-set of
/// the retired subscription then carries a version the retirement has already moved on, so it is
/// refused with a conflict and reactivates nothing; a new subscription is accepted and belongs to
/// the device's new incarnation. Either way the retirement is `confirmed_applied` for exactly the
/// members it locked.
async fn subscription_set_retirement_first(raise: bool, resubscribe: bool) -> Result<()> {
    let (_d, s, a) = setup(raise).await?;
    add_session(&s, &a, PHONE, FAR).await?;
    let existing = add_subscription(&s, &a, PHONE).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let op = id();
    let paused = pause(&s, &a, PHONE, &op, &token, NOW, RETIREMENT_FIRST).await;
    let command = if resubscribe {
        set_subscription(&existing, 1, "rotated")
    } else {
        set_subscription(&id(), 0, "added")
    };
    let mut writer = {
        let (s, a) = (s.clone(), a.clone());
        tokio::spawn(async move { s.reminder_command(&a, &id(), &command).await })
    };
    assert_blocked(&s, paused.point, &mut writer).await?;
    assert_eq!(paused.finish().await?.outcome, Outcome::ConfirmedApplied);
    let result = writer.await?;
    let after = snapshot(&s, &a, PHONE).await?;
    assert!(
        after.sessions.is_empty(),
        "the retirement removed the session"
    );
    // Rows are `(id, version, active, secret)`.
    let old = after
        .subscriptions
        .iter()
        .find(|row| row.0 == existing)
        .unwrap();
    assert_eq!(
        (old.1, old.2, old.3.as_str()),
        (2, 0, ""),
        "deactivated by the retirement, which moved its version on"
    );
    if resubscribe {
        assert_eq!(result.unwrap_err().to_string(), "conflict");
        assert_eq!(
            after.subscriptions.len(),
            1,
            "nothing was reactivated or added"
        );
    } else {
        result?;
        assert_eq!(after.subscriptions.len(), 2);
        let added = after
            .subscriptions
            .iter()
            .find(|row| row.0 != existing)
            .unwrap();
        assert_eq!(
            (added.1, added.2, added.3.as_str()),
            (1, 1, "added"),
            "the new incarnation's subscription"
        );
    }
    assert_eq!(outcomes(&ledger_rows(&s, &a).await?), ["confirmed_applied"]);
    assert_isolation(&s, raise);
    Ok(())
}

#[tokio::test]
async fn a_resubscription_first_makes_the_retirement_stale() -> Result<()> {
    subscription_set_writer_first(false, true).await
}

#[tokio::test]
async fn a_new_subscription_first_makes_the_retirement_stale() -> Result<()> {
    subscription_set_writer_first(false, false).await
}

#[tokio::test]
async fn a_resubscription_after_the_retirement_is_refused() -> Result<()> {
    subscription_set_retirement_first(false, true).await
}

#[tokio::test]
async fn a_new_subscription_after_the_retirement_belongs_to_the_new_incarnation() -> Result<()> {
    subscription_set_retirement_first(false, false).await
}

// ---------------------------------------------------------------- (al) rows-affected assertion

/// (al) A statement injected between the locking reads and the effects deletes a locked member.
/// Each effect statement's rows-affected assertion fails, the transaction rolls back, no ledger
/// row is written and the retirement fails with an internal error. Both engines.
#[tokio::test]
async fn a_discrepancy_rolls_back_without_a_ledger_row() -> Result<()> {
    for (name, interfering) in [
        (
            "session",
            "DELETE FROM sessions WHERE account_id='{a}' AND device_id='phone' AND session_id=(SELECT MIN(session_id) FROM sessions WHERE account_id='{a}' AND device_id='phone')",
        ),
        (
            "handoff",
            "DELETE FROM native_handoffs WHERE account_id='{a}' AND device_id='phone'",
        ),
        (
            "subscription",
            "UPDATE notification_subscriptions SET version=version+1 WHERE account_id='{a}' AND device_id='phone'",
        ),
        (
            "registration",
            "DELETE FROM sync_devices WHERE account_id='{a}' AND device_id='phone'",
        ),
    ] {
        let (_d, s, a) = setup(false).await?;
        populate(&s, &a, PHONE).await?;
        let token = expected_token(&s, &a, PHONE, NOW).await?;
        let before = snapshot(&s, &a, PHONE).await?;
        s.hooks()
            .inject_sql("retire.before_effects", &interfering.replace("{a}", &a));
        let error = s
            .retire_device(&a, PHONE, &id(), &token, NOW)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "internal_error", "{name}");
        assert!(
            ledger_rows(&s, &a).await?.is_empty(),
            "{name}: no ledger row"
        );
        assert_eq!(
            snapshot(&s, &a, PHONE).await?,
            before,
            "{name}: rolled back, injected statement too"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- (ak) SQLite premise

/// (ak) After `begin_serial()` returns, a second connection with `busy_timeout=0` cannot write
/// `sessions` (SQLite code 5) but can still read the last committed snapshot. Every SQLite
/// schedule rests on this.
#[tokio::test]
async fn sqlite_serialisation_premise() -> Result<()> {
    if postgres() {
        return Ok(());
    }
    let (_d, s, a) = setup(false).await?;
    let session = add_session(&s, &a, PHONE, FAR).await?;
    let held = s.begin_serial().await?;
    let mut other = s.pool.acquire().await?;
    sqlx::query("PRAGMA busy_timeout=0")
        .execute(&mut *other)
        .await?;
    let error = sqlx::query("DELETE FROM sessions WHERE session_id=$1")
        .bind(&session)
        .execute(&mut *other)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("5")
    );
    let visible: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE session_id=$1")
        .bind(&session)
        .fetch_one(&mut *other)
        .await?;
    assert_eq!(visible, 1);
    held.rollback().await?;
    Ok(())
}

// ---------------------------------------------------------------- (ai) isolation

/// (ai) The retirement runs at READ COMMITTED; and every schedule above also passes with it raised
/// to REPEATABLE READ (the difference is retries, not outcomes). PostgreSQL only.
#[tokio::test]
async fn runs_at_read_committed() -> Result<()> {
    if !postgres() {
        return Ok(());
    }
    let (_d, s, a) = setup(false).await?;
    add_session(&s, &a, PHONE, FAR).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    s.retire_device(&a, PHONE, &id(), &token, NOW).await?;
    assert_eq!(s.hooks().isolation_seen(), ["read committed"]);
    Ok(())
}

#[tokio::test]
async fn every_schedule_also_passes_at_repeatable_read() -> Result<()> {
    if !postgres() {
        return Ok(());
    }
    {
        let (_d, s, a) = setup(true).await?;
        add_session(&s, &a, PHONE, FAR).await?;
        let token = expected_token(&s, &a, PHONE, NOW).await?;
        s.retire_device(&a, PHONE, &id(), &token, NOW).await?;
        assert_eq!(s.hooks().isolation_seen(), ["repeatable read"]);
    }
    revoke_writer_first(true)
        .await
        .context("(y) writer first")?;
    revoke_retirement_first(true)
        .await
        .context("(y) retirement first")?;
    partial_removal_writer_first(true)
        .await
        .context("(aa) writer first")?;
    partial_removal_retirement_first(true)
        .await
        .context("(aa) retirement first")?;
    cleanup_first_supersedes(true)
        .await
        .context("(ab) cleanup first")?;
    retirement_first_beats_cleanup(true)
        .await
        .context("(ab) retirement first")?;
    cleanup_beside_a_session_is_stale(true)
        .await
        .context("(ab) beside a session")?;
    sweep_first(true).await.context("(ac) sweep first")?;
    retirement_first_beats_sweep(true)
        .await
        .context("(ac) retirement first")?;
    retry_after_a_deadlock(true)
        .await
        .context("(ac) deadlock")?;
    registration_creation_writer_first(true)
        .await
        .context("(ad) creation, writer first")?;
    registration_creation_retirement_first(true)
        .await
        .context("(ad) creation, retirement first")?;
    last_seen_writer_first(true)
        .await
        .context("(ad) last_seen, writer first")?;
    last_seen_retirement_first(true)
        .await
        .context("(ad) last_seen, retirement first")?;
    for resubscribe in [true, false] {
        subscription_set_writer_first(true, resubscribe)
            .await
            .context("(ae) subscription set, writer first")?;
        subscription_set_retirement_first(true, resubscribe)
            .await
            .context("(ae) subscription set, retirement first")?;
    }
    serialisation_failures(true)
        .await
        .context("(ah) serialisation failures")?;
    Ok(())
}
