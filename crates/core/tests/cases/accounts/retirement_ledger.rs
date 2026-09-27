//! Outcomes, replay and operation-ID reuse for device retirement and keyed session revocation
//! (checks a, b, d, e, h, i, j, l, p, q, s, t, u, v, w, ag, af and N5, N6).
//! Interleavings that need a paused retirement are in `retirement_coordination`.
use anyhow::Result;
use atlas_core::{
    Store,
    operations::{Operation, Outcome},
};

use crate::support::devices::{
    FAR, add_handoff, add_registration, add_session, add_subscription, clear, expected_token, id,
    ledger_rows, populate, snapshot,
};
use crate::support::resource_commands::{account, fixture};
use crate::support::schedule::{assert_isolation, postgres};

const NOW: i64 = 1000;
const PHONE: &str = "phone";

async fn retire_with(s: &Store, a: &str, op: &str, token: &str) -> Result<Operation> {
    s.retire_device(a, PHONE, op, token, NOW).await
}

fn outcomes(rows: &[(String, String, String)]) -> Vec<&str> {
    rows.iter().map(|r| r.2.as_str()).collect()
}

/// (a) Two concurrent retirements: one applies, the other finds nothing left; each ID has exactly
/// one durable row. Ten rounds, each on a freshly created device.
#[tokio::test]
async fn concurrent_retirements_record_one_outcome_each() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    for round in 0..10 {
        populate(&s, &a, PHONE).await?;
        let token = expected_token(&s, &a, PHONE, NOW).await?;
        let (first, second) = (id(), id());
        let (r1, r2) = tokio::join!(
            retire_with(&s, &a, &first, &token),
            retire_with(&s, &a, &second, &token)
        );
        let mut got = [r1?.outcome.as_str(), r2?.outcome.as_str()];
        got.sort();
        assert_eq!(got, ["confirmed_applied", "superseded"], "round {round}");
        let rows = ledger_rows(&s, &a).await?;
        for op in [&first, &second] {
            assert_eq!(rows.iter().filter(|r| &r.0 == op).count(), 1);
        }
        assert_eq!(snapshot(&s, &a, PHONE).await?.sessions.len(), 0);
    }
    Ok(())
}

/// (a) The same ID sent twice at once is evaluated once and replayed once.
#[tokio::test]
async fn concurrent_duplicates_are_evaluated_once() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    for _ in 0..10 {
        populate(&s, &a, PHONE).await?;
        let token = expected_token(&s, &a, PHONE, NOW).await?;
        let op = id();
        let (r1, r2) = tokio::join!(
            retire_with(&s, &a, &op, &token),
            retire_with(&s, &a, &op, &token)
        );
        let (r1, r2) = (r1?, r2?);
        assert_eq!(r1, r2);
        assert_eq!(r1.outcome, Outcome::ConfirmedApplied);
        let rows = ledger_rows(&s, &a).await?;
        assert_eq!(rows.iter().filter(|r| r.0 == op).count(), 1);
        assert_eq!(
            rows.iter().find(|r| r.0 == op).unwrap().2,
            "confirmed_applied"
        );
    }
    Ok(())
}

/// (b), (ag) A duplicate after the state changed returns the original outcome and evaluates
/// nothing. Repeated for each of the three recorded outcomes, after the device is recreated.
/// With `raise` (PostgreSQL only) the retirement runs at REPEATABLE READ (check ai).
async fn replay_after_recreation(raise: bool) -> Result<()> {
    for wanted in [
        Outcome::ConfirmedApplied,
        Outcome::RejectedStale,
        Outcome::Superseded,
    ] {
        let (_d, s) = fixture().await?;
        if raise {
            s.hooks().raise_isolation();
        }
        let a = account(&s).await?;
        populate(&s, &a, PHONE).await?;
        let token = expected_token(&s, &a, PHONE, NOW).await?;
        match wanted {
            Outcome::ConfirmedApplied => {}
            Outcome::RejectedStale => {
                add_session(&s, &a, PHONE, FAR).await?;
            }
            Outcome::Superseded => clear(&s, &a, PHONE).await?,
        }
        let op = id();
        assert_eq!(retire_with(&s, &a, &op, &token).await?.outcome, wanted);
        let row = ledger_rows(&s, &a).await?;
        assert_eq!(outcomes(&row), [wanted.as_str()]);
        let digest: String =
            sqlx::query_scalar("SELECT digest FROM operation_outcomes WHERE operation_id=$1")
                .bind(&op)
                .fetch_one(&s.pool)
                .await?;

        // Recreate the device: new sessions, a new registration key.
        clear(&s, &a, PHONE).await?;
        populate(&s, &a, PHONE).await?;
        let recreated = snapshot(&s, &a, PHONE).await?;

        // The identical request replays the recorded outcome and leaves the newcomer alone.
        let replay = retire_with(&s, &a, &op, &token).await?;
        assert_eq!(
            (replay.outcome, replay.operation_id.as_str()),
            (wanted, op.as_str())
        );
        assert_eq!(replay.account_id, a);
        assert_eq!(snapshot(&s, &a, PHONE).await?, recreated);

        // (w) Reusing the ID for a different approved state is refused; the row is untouched.
        let fresh = expected_token(&s, &a, PHONE, NOW).await?;
        assert_ne!(fresh, token);
        assert_eq!(
            retire_with(&s, &a, &op, &fresh)
                .await
                .unwrap_err()
                .to_string(),
            "invalid_value"
        );
        assert_eq!(
            s.retire_device(&a, "other", &op, &token, NOW)
                .await
                .unwrap_err()
                .to_string(),
            "invalid_value"
        );
        let after: (String, String) =
            sqlx::query_as("SELECT digest,outcome FROM operation_outcomes WHERE operation_id=$1")
                .bind(&op)
                .fetch_one(&s.pool)
                .await?;
        assert_eq!(after, (digest, wanted.as_str().to_owned()));
        assert_eq!(snapshot(&s, &a, PHONE).await?, recreated);
        assert_isolation(&s, raise);
    }
    Ok(())
}

#[tokio::test]
async fn replay_survives_device_recreation_for_each_outcome() -> Result<()> {
    replay_after_recreation(false).await
}

/// (ai) The same, with the retirement raised to REPEATABLE READ. PostgreSQL only.
#[tokio::test]
async fn replay_survives_device_recreation_at_repeatable_read() -> Result<()> {
    if !postgres() {
        return Ok(());
    }
    replay_after_recreation(true).await
}

/// (d) A device recreated since approval is refused with nothing deleted.
#[tokio::test]
async fn a_recreated_device_is_stale() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    populate(&s, &a, PHONE).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    clear(&s, &a, PHONE).await?; // retired elsewhere ...
    populate(&s, &a, PHONE).await?; // ... and recreated: new IDs, new cursor key
    let before = snapshot(&s, &a, PHONE).await?;
    let op = id();
    assert_eq!(
        retire_with(&s, &a, &op, &token).await?.outcome,
        Outcome::RejectedStale
    );
    assert_eq!(snapshot(&s, &a, PHONE).await?, before);
    Ok(())
}

/// (e), (af), (h) A session added after the token was formed makes the retirement stale: neither
/// session is deleted, and the rejection is recorded for its own ID and distinguishable from an
/// ID with no row.
#[tokio::test]
async fn a_second_session_makes_the_attempt_stale() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    let first = add_session(&s, &a, PHONE, FAR).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let second = add_session(&s, &a, PHONE, FAR).await?;
    let before = snapshot(&s, &a, PHONE).await?;
    let (rejected, pending) = (id(), id());
    assert_eq!(s.operation_outcome(&a, &rejected).await?, None);
    assert_eq!(
        retire_with(&s, &a, &rejected, &token).await?.outcome,
        Outcome::RejectedStale
    );
    assert_eq!(snapshot(&s, &a, PHONE).await?, before);
    assert!(before.sessions.contains(&first) && before.sessions.contains(&second));
    assert_eq!(
        s.operation_outcome(&a, &rejected).await?,
        Some(Outcome::RejectedStale)
    );
    assert_eq!(
        s.operation_outcome(&a, &pending).await?,
        None,
        "unresolved is not rejected"
    );
    // Another account's view of the same ID is unresolved.
    let other = account(&s).await?;
    assert_eq!(s.operation_outcome(&other, &rejected).await?, None);
    // The rejection replays.
    assert_eq!(
        retire_with(&s, &a, &rejected, &token).await?.outcome,
        Outcome::RejectedStale
    );
    Ok(())
}

/// (l) A device with no member left is `superseded`, distinguishable from a changed state.
#[tokio::test]
async fn an_empty_device_is_superseded_not_stale() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    populate(&s, &a, PHONE).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let first = id();
    assert_eq!(
        retire_with(&s, &a, &first, &token).await?.outcome,
        Outcome::ConfirmedApplied
    );
    let second = id();
    assert_eq!(
        retire_with(&s, &a, &second, &token).await?.outcome,
        Outcome::Superseded
    );
    assert_eq!(
        s.operation_outcome(&a, &second).await?,
        Some(Outcome::Superseded)
    );
    // Expired members alone are not members either.
    add_session(&s, &a, PHONE, NOW).await?;
    add_handoff(&s, &a, PHONE, NOW - 5).await?;
    let third = id();
    assert_eq!(
        retire_with(&s, &a, &third, &token).await?.outcome,
        Outcome::Superseded
    );
    Ok(())
}

/// (p), (q), (s), (t), (u), (af) Every change to any component after approval is `rejected_stale`
/// with no deletion, deactivation or cancellation of anything.
#[tokio::test]
async fn any_component_change_is_stale_and_changes_nothing() -> Result<()> {
    type Change = for<'a> fn(
        &'a Store,
        &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<()>> + 'a>,
    >;
    let changes: Vec<(&str, Change)> = vec![
        ("re-subscription", |s, a| {
            Box::pin(async move {
                sqlx::query("UPDATE notification_subscriptions SET version=version+1 WHERE account_id=$1 AND device_id='phone'")
                .bind(a).execute(&s.pool).await?;
                Ok(())
            })
        }),
        ("handoff created", |s, a| {
            Box::pin(async move {
                add_handoff(s, a, PHONE, FAR).await?;
                Ok(())
            })
        }),
        ("handoff consumed and a session added", |s, a| {
            Box::pin(async move {
                sqlx::query(
                    "DELETE FROM native_handoffs WHERE account_id=$1 AND device_id='phone'",
                )
                .bind(a)
                .execute(&s.pool)
                .await?;
                add_session(s, a, PHONE, FAR).await?;
                Ok(())
            })
        }),
        ("subscription added", |s, a| {
            Box::pin(async move {
                add_subscription(s, a, PHONE).await?;
                Ok(())
            })
        }),
        ("registration replaced", |s, a| {
            Box::pin(async move {
                sqlx::query("UPDATE sync_devices SET cursor_key='replacement' WHERE account_id=$1 AND device_id='phone'")
                .bind(a).execute(&s.pool).await?;
                Ok(())
            })
        }),
        (
            "registration removed by retention cleanup while others remain",
            |s, a| {
                Box::pin(async move {
                    let _ = a;
                    s.collect_expired(1 + 90 * 86400 + 1).await?;
                    Ok(())
                })
            },
        ),
        ("one session removed independently", |s, a| {
            Box::pin(async move {
                sqlx::query("DELETE FROM sessions WHERE session_id=(SELECT MIN(session_id) FROM sessions WHERE account_id=$1 AND device_id='phone')")
                .bind(a).execute(&s.pool).await?;
                Ok(())
            })
        }),
        ("a whole component removed independently", |s, a| {
            Box::pin(async move {
                sqlx::query("DELETE FROM sessions WHERE account_id=$1 AND device_id='phone'")
                    .bind(a)
                    .execute(&s.pool)
                    .await?;
                Ok(())
            })
        }),
        ("retired elsewhere and recreated", |s, a| {
            Box::pin(async move {
                clear(s, a, PHONE).await?;
                populate(s, a, PHONE).await?;
                Ok(())
            })
        }),
    ];
    for (name, change) in changes {
        let (_d, s) = fixture().await?;
        let a = account(&s).await?;
        populate(&s, &a, PHONE).await?;
        let token = expected_token(&s, &a, PHONE, NOW).await?;
        change(&s, &a).await?;
        let before = snapshot(&s, &a, PHONE).await?;
        let op = id();
        assert_eq!(
            retire_with(&s, &a, &op, &token).await?.outcome,
            Outcome::RejectedStale,
            "{name}"
        );
        assert_eq!(
            snapshot(&s, &a, PHONE).await?,
            before,
            "{name}: nothing may change"
        );
        assert_eq!(
            outcomes(&ledger_rows(&s, &a).await?),
            ["rejected_stale"],
            "{name}"
        );
    }
    Ok(())
}

/// (t) A registration created between the read and the retirement.
#[tokio::test]
async fn a_registration_created_after_approval_is_stale() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    add_session(&s, &a, PHONE, FAR).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    add_registration(&s, &a, PHONE).await?;
    let before = snapshot(&s, &a, PHONE).await?;
    assert_eq!(
        retire_with(&s, &a, &id(), &token).await?.outcome,
        Outcome::RejectedStale
    );
    assert_eq!(snapshot(&s, &a, PHONE).await?, before);
    Ok(())
}

/// (u) Removal of every member is `superseded`; a partial removal never is.
#[tokio::test]
async fn partial_removal_is_stale_and_total_removal_is_superseded() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    populate(&s, &a, PHONE).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    sqlx::query("DELETE FROM sessions WHERE account_id=$1 AND device_id=$2")
        .bind(&a)
        .bind(PHONE)
        .execute(&s.pool)
        .await?;
    assert_eq!(
        retire_with(&s, &a, &id(), &token).await?.outcome,
        Outcome::RejectedStale
    );
    clear(&s, &a, PHONE).await?;
    assert_eq!(
        retire_with(&s, &a, &id(), &token).await?.outcome,
        Outcome::Superseded
    );
    Ok(())
}

/// (v) After a rejection a fresh read and a new ID succeed; the old ID still replays the rejection.
#[tokio::test]
async fn a_fresh_token_and_id_succeed_after_a_rejection() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    populate(&s, &a, PHONE).await?;
    let stale = expected_token(&s, &a, PHONE, NOW).await?;
    add_session(&s, &a, PHONE, FAR).await?;
    let old = id();
    assert_eq!(
        retire_with(&s, &a, &old, &stale).await?.outcome,
        Outcome::RejectedStale
    );
    let fresh = expected_token(&s, &a, PHONE, NOW).await?;
    let new = id();
    assert_eq!(
        retire_with(&s, &a, &new, &fresh).await?.outcome,
        Outcome::ConfirmedApplied
    );
    assert!(snapshot(&s, &a, PHONE).await?.sessions.is_empty());
    assert_eq!(
        retire_with(&s, &a, &old, &stale).await?.outcome,
        Outcome::RejectedStale
    );
    assert_eq!(outcomes(&ledger_rows(&s, &a).await?).len(), 2);
    Ok(())
}

/// (i), (j), N5 Keyed revocation records `confirmed_applied` or `superseded`, never
/// `rejected_stale` (in code and in the schema), and never credits an expiry or another account.
#[tokio::test]
async fn keyed_revocation_outcomes() -> Result<()> {
    let (_d, s) = fixture().await?;
    let (a, b) = (account(&s).await?, account(&s).await?);
    let live = add_session(&s, &a, "one", FAR).await?;
    let expired = add_session(&s, &a, "two", NOW).await?;
    let theirs = add_session(&s, &b, "one", FAR).await?;

    let op = id();
    assert_eq!(
        s.revoke_session(&a, &live, &op, NOW).await?.outcome,
        Outcome::ConfirmedApplied
    );
    // Replay after the target is gone, and by construction independent of any session.
    assert_eq!(
        s.revoke_session(&a, &live, &op, NOW + 5).await?.outcome,
        Outcome::ConfirmedApplied
    );
    // (j) A second ID against the already-deleted session, then its replay.
    let again = id();
    assert_eq!(
        s.revoke_session(&a, &live, &again, NOW).await?.outcome,
        Outcome::Superseded
    );
    assert_eq!(
        s.revoke_session(&a, &live, &again, NOW).await?.outcome,
        Outcome::Superseded
    );
    // N5 An expired-but-unswept row is not credited to the call, and is left to the sweeps.
    assert_eq!(
        s.revoke_session(&a, &expired, &id(), NOW).await?.outcome,
        Outcome::Superseded
    );
    let still: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE session_id=$1")
        .bind(&expired)
        .fetch_one(&s.pool)
        .await?;
    assert_eq!(still, 1);
    // Another account's session, and an unknown one, are not this account's to revoke.
    assert_eq!(
        s.revoke_session(&a, &theirs, &id(), NOW).await?.outcome,
        Outcome::Superseded
    );
    assert_eq!(
        s.revoke_session(&a, &id(), &id(), NOW).await?.outcome,
        Outcome::Superseded
    );
    let alive: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE session_id=$1")
        .bind(&theirs)
        .fetch_one(&s.pool)
        .await?;
    assert_eq!(alive, 1);
    assert!(
        ledger_rows(&s, &b).await?.is_empty(),
        "nothing is recorded for the other account"
    );
    let rows = ledger_rows(&s, &a).await?;
    assert_eq!(rows.len(), 5);
    assert!(
        rows.iter()
            .all(|r| r.1 == "revoke_session" && r.2 != "rejected_stale")
    );
    assert_eq!(rows.iter().filter(|r| r.0 == again).count(), 1);

    // The schema itself forbids it.
    let forbidden = sqlx::query("INSERT INTO operation_outcomes(account_id,operation_id,kind,digest,outcome,created_at) VALUES ($1,$2,'revoke_session','d','rejected_stale',1)")
        .bind(&a).bind(id()).execute(&s.pool).await;
    assert!(forbidden.is_err());
    let allowed = sqlx::query("INSERT INTO operation_outcomes(account_id,operation_id,kind,digest,outcome,created_at) VALUES ($1,$2,'retire_device','d','rejected_stale',1)")
        .bind(&a).bind(id()).execute(&s.pool).await;
    assert!(allowed.is_ok());
    Ok(())
}

/// (w) An ID cannot be reused for a different kind of operation or a different target.
#[tokio::test]
async fn an_operation_id_cannot_change_meaning() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    let session = add_session(&s, &a, PHONE, FAR).await?;
    populate(&s, &a, "tablet").await?;
    let op = id();
    assert_eq!(
        s.revoke_session(&a, &session, &op, NOW).await?.outcome,
        Outcome::ConfirmedApplied
    );
    let token = expected_token(&s, &a, "tablet", NOW).await?;
    let error = s
        .retire_device(&a, "tablet", &op, &token, NOW)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "invalid_value");
    assert_eq!(
        s.revoke_session(&a, &id(), &op, NOW)
            .await
            .unwrap_err()
            .to_string(),
        "invalid_value"
    );
    assert_eq!(
        snapshot(&s, &a, "tablet").await?.sessions.len(),
        2,
        "the retirement did not run"
    );
    assert_eq!(outcomes(&ledger_rows(&s, &a).await?), ["confirmed_applied"]);
    Ok(())
}

#[tokio::test]
async fn inputs_are_validated_before_anything_is_written() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    populate(&s, &a, PHONE).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let good = id();
    let too_long = "x".repeat(101);
    for (device, op) in [
        ("", good.as_str()),
        (too_long.as_str(), good.as_str()),
        (PHONE, "not-a-uuid"),
        (PHONE, good.to_uppercase().as_str()),
        (PHONE, ""),
    ] {
        assert_eq!(
            s.retire_device(&a, device, op, &token, NOW)
                .await
                .unwrap_err()
                .to_string(),
            "invalid_value",
            "{device:?} {op:?}"
        );
    }
    for (session, op) in [("nope", good.as_str()), (good.as_str(), "nope")] {
        assert_eq!(
            s.revoke_session(&a, session, op, NOW)
                .await
                .unwrap_err()
                .to_string(),
            "invalid_value"
        );
    }
    assert!(ledger_rows(&s, &a).await?.is_empty());
    // An unknown account authenticates nothing and records nothing.
    let unknown = id();
    assert_eq!(
        s.retire_device(&unknown, PHONE, &id(), &token, NOW)
            .await
            .unwrap_err()
            .to_string(),
        "unauthenticated"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operation_outcomes")
        .fetch_one(&s.pool)
        .await?;
    assert_eq!(count, 0);
    Ok(())
}

/// N6 Restore preparation resets sessions and registrations but keeps the ledger, so an ID still
/// replays its recorded outcome afterwards.
#[tokio::test]
async fn restore_preparation_keeps_the_ledger() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    populate(&s, &a, PHONE).await?;
    let token = expected_token(&s, &a, PHONE, NOW).await?;
    let op = id();
    assert_eq!(
        retire_with(&s, &a, &op, &token).await?.outcome,
        Outcome::ConfirmedApplied
    );
    populate(&s, &a, "tablet").await?;
    s.prepare_restored_database().await?;
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(&s.pool)
        .await?;
    assert_eq!(sessions, 0);
    assert_eq!(outcomes(&ledger_rows(&s, &a).await?), ["confirmed_applied"]);
    assert_eq!(
        retire_with(&s, &a, &op, &token).await?.outcome,
        Outcome::ConfirmedApplied
    );
    Ok(())
}
