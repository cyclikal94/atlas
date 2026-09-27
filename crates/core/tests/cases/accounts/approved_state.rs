//! The approved device state, its token and the listing that offers it (check x, N4).
use anyhow::Result;
use atlas_core::{devices::valid_state_token, operations::Outcome};

use crate::support::devices::{
    FAR, add_grant, add_handoff, add_registration, add_session, add_subscription, expected_token,
    id, ledger_rows, populate, snapshot,
};
use crate::support::resource_commands::{account, create, fixture};

const NOW: i64 = 1000;
const PHONE: &str = "phone";

fn outcomes(rows: &[(String, String, String)]) -> Vec<&str> {
    rows.iter().map(|r| r.2.as_str()).collect()
}

#[tokio::test]
async fn listing_offers_the_independently_computed_token() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    populate(&s, &a, "phone").await?;
    add_session(&s, &a, "tablet", FAR).await?;
    add_registration(&s, &a, "watch").await?;
    let listed = s.devices(&a, NOW).await?;
    assert_eq!(
        listed.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
        ["phone", "tablet", "watch"]
    );
    for device in &listed {
        // The oracle reads the tables and follows the specification; it never calls the listing.
        assert_eq!(
            device.state_token,
            expected_token(&s, &a, &device.id, NOW).await?
        );
        assert!(valid_state_token(&device.state_token));
        assert_eq!(device.state_token.len(), 3 + 43);
        assert_eq!(device.active_sessions, device.summary.sessions);
        assert_eq!(device.summary.pending_sign_ins, 0);
    }
    let by = |name: &str| listed.iter().find(|d| d.id == name).unwrap();
    let phone = &by("phone").summary;
    assert_eq!(
        (
            phone.sessions,
            phone.native_handoffs,
            phone.notification_subscriptions,
            phone.sync_registered
        ),
        (2, 1, 1, true)
    );
    // A registration alone is listed, tokenised and retirable.
    let watch = by("watch");
    assert_eq!(
        (watch.summary.sessions, watch.summary.sync_registered),
        (0, true)
    );
    assert!(!by("tablet").summary.sync_registered);
    assert_ne!(by("phone").state_token, by("tablet").state_token);
    Ok(())
}

async fn listed_token(s: &atlas_core::Store, a: &str, device: &str) -> Result<String> {
    let token = s
        .devices(a, NOW)
        .await?
        .into_iter()
        .find(|d| d.id == device)
        .expect("device is listed")
        .state_token;
    assert_eq!(token, expected_token(s, a, device, NOW).await?);
    Ok(token)
}

#[tokio::test]
async fn token_tracks_each_component_and_ignores_churn() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    add_session(&s, &a, "phone", FAR).await?;
    let t0 = listed_token(&s, &a, "phone").await?;
    assert_eq!(
        t0,
        listed_token(&s, &a, "phone").await?,
        "stable across reads"
    );

    // Component 1: a session.
    let extra = add_session(&s, &a, "phone", FAR).await?;
    let t1 = listed_token(&s, &a, "phone").await?;
    assert_ne!(t1, t0);
    sqlx::query("DELETE FROM sessions WHERE session_id=$1")
        .bind(&extra)
        .execute(&s.pool)
        .await?;
    assert_eq!(
        listed_token(&s, &a, "phone").await?,
        t0,
        "the state, not the history"
    );

    // Component 2: a native handoff.
    add_handoff(&s, &a, "phone", FAR).await?;
    let t2 = listed_token(&s, &a, "phone").await?;
    assert_ne!(t2, t0);

    // Component 3: a subscription, and its version (a re-subscription or secret rotation).
    let subscription = add_subscription(&s, &a, "phone").await?;
    let t3 = listed_token(&s, &a, "phone").await?;
    assert_ne!(t3, t2);
    sqlx::query("UPDATE notification_subscriptions SET version=version+1 WHERE id=$1")
        .bind(&subscription)
        .execute(&s.pool)
        .await?;
    let t3b = listed_token(&s, &a, "phone").await?;
    assert_ne!(t3b, t3);

    // Component 4 (BE-Q19): an activation grant, then its redemption or cancellation — both
    // simulated with raw SQL, since no server writer for either exists in this crate.
    let grant = add_grant(&s, &a, "phone").await?;
    let t3c = listed_token(&s, &a, "phone").await?;
    assert_ne!(t3c, t3b);
    sqlx::query("UPDATE activation_grants SET state='redeemed' WHERE grant_id=$1")
        .bind(&grant)
        .execute(&s.pool)
        .await?;
    assert_eq!(
        listed_token(&s, &a, "phone").await?,
        t3b,
        "a redeemed grant leaves component 4"
    );
    let grant = add_grant(&s, &a, "phone").await?;
    assert_ne!(
        listed_token(&s, &a, "phone").await?,
        t3b,
        "a fresh grant is a member again"
    );
    sqlx::query("UPDATE activation_grants SET state='cancelled' WHERE grant_id=$1")
        .bind(&grant)
        .execute(&s.pool)
        .await?;
    assert_eq!(
        listed_token(&s, &a, "phone").await?,
        t3b,
        "a cancelled grant also leaves component 4"
    );
    // An expired-but-still-`issued` grant is churn, not state.
    add_grant(&s, &a, "phone").await?;
    sqlx::query("UPDATE activation_grants SET expires_at=$1 WHERE account_id=$2 AND device_id='phone' AND state='issued'")
        .bind(NOW)
        .bind(&a)
        .execute(&s.pool)
        .await?;
    assert_eq!(listed_token(&s, &a, "phone").await?, t3b);

    // Component 5: the registration, and a re-created registration with a new key.
    add_registration(&s, &a, "phone").await?;
    let t4 = listed_token(&s, &a, "phone").await?;
    assert_ne!(t4, t3b);
    sqlx::query("UPDATE sync_devices SET cursor_key=$1 WHERE account_id=$2 AND device_id='phone'")
        .bind(id())
        .bind(&a)
        .execute(&s.pool)
        .await?;
    let t5 = listed_token(&s, &a, "phone").await?;
    assert_ne!(t5, t4, "a recreated device always has a different token");

    // Churn is not state: last_seen, expired members. Neither changes the token.
    sqlx::query("UPDATE sync_devices SET last_seen=last_seen+99999 WHERE account_id=$1")
        .bind(&a)
        .execute(&s.pool)
        .await?;
    add_session(&s, &a, "phone", NOW).await?; // expires_at = now: not live
    add_handoff(&s, &a, "phone", NOW - 1).await?;
    assert_eq!(listed_token(&s, &a, "phone").await?, t5);

    // An inactive subscription (its secret already erased) is not a member; reactivating it
    // restores the earlier state exactly.
    sqlx::query("UPDATE notification_subscriptions SET active=0,secret='' WHERE id=$1")
        .bind(&subscription)
        .execute(&s.pool)
        .await?;
    assert_ne!(listed_token(&s, &a, "phone").await?, t5);
    sqlx::query("UPDATE notification_subscriptions SET active=1 WHERE id=$1")
        .bind(&subscription)
        .execute(&s.pool)
        .await?;
    assert_eq!(listed_token(&s, &a, "phone").await?, t5);
    Ok(())
}

#[tokio::test]
async fn sync_progress_does_not_change_the_token() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    create(&s, &a).await?;
    add_session(&s, &a, "phone", FAR).await?;
    let page = s.sync(&a, "phone", None, 200, NOW).await?;
    let before = listed_token(&s, &a, "phone").await?;
    // A later delta at a much later time refreshes last_seen and cursor state.
    s.sync(&a, "phone", Some(&page.next_cursor), 200, NOW + 200_000)
        .await?;
    assert_eq!(listed_token(&s, &a, "phone").await?, before);
    Ok(())
}

#[tokio::test]
async fn malformed_and_unknown_prefix_tokens_are_refused() -> Result<()> {
    let zero = "A".repeat(43);
    assert!(valid_state_token(&format!("v1.{zero}")));
    for bad in [
        String::new(),
        "v1.".into(),
        format!("v2.{zero}"),
        format!("V1.{zero}"),
        zero.clone(),
        format!("v1.{}", "A".repeat(42)),
        format!("v1.{}", "A".repeat(44)),
        format!("v1.{}B", "A".repeat(42)), // non-canonical trailing bits
        format!("v1.{}+", "A".repeat(42)),
        format!("v1.{}/", "A".repeat(42)),
        format!("v1.{}=", "A".repeat(42)),
    ] {
        assert!(!valid_state_token(&bad), "{bad}");
    }
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    populate(&s, &a, "phone").await?;
    let before = snapshot(&s, &a, "phone").await?;
    for bad in [format!("v2.{zero}"), "v1.short".to_owned(), String::new()] {
        let error = s
            .retire_device(&a, "phone", &id(), &bad, NOW)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "invalid_value");
    }
    assert_eq!(snapshot(&s, &a, "phone").await?, before);
    assert!(ledger_rows(&s, &a).await?.is_empty());
    Ok(())
}

/// R1: each live member, alone, lists its device with the token a retirement needs; members that
/// are not live (expired, deactivated) list nothing.
#[tokio::test]
async fn any_live_member_alone_lists_the_device_with_its_token() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    add_handoff(&s, &a, "handoff-only", FAR).await?;
    add_subscription(&s, &a, "subscription-only").await?;
    add_registration(&s, &a, "registration-only").await?;
    add_session(&s, &a, "session-only", FAR).await?;
    add_grant(&s, &a, "grant-only").await?;
    add_handoff(&s, &a, "expired-handoff", NOW).await?;
    add_session(&s, &a, "expired-session", NOW).await?;
    let inactive = add_subscription(&s, &a, "inactive-subscription").await?;
    sqlx::query("UPDATE notification_subscriptions SET active=0,secret='' WHERE id=$1")
        .bind(inactive)
        .execute(&s.pool)
        .await?;
    let expired_grant = add_grant(&s, &a, "expired-grant").await?;
    sqlx::query("UPDATE activation_grants SET expires_at=$1 WHERE grant_id=$2")
        .bind(NOW)
        .bind(expired_grant)
        .execute(&s.pool)
        .await?;
    let cancelled_grant = add_grant(&s, &a, "cancelled-grant").await?;
    sqlx::query("UPDATE activation_grants SET state='cancelled' WHERE grant_id=$1")
        .bind(cancelled_grant)
        .execute(&s.pool)
        .await?;

    let listed = s.devices(&a, NOW).await?;
    assert_eq!(
        listed.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
        [
            "grant-only",
            "handoff-only",
            "registration-only",
            "session-only",
            "subscription-only"
        ]
    );
    for device in &listed {
        assert_eq!(
            device.state_token,
            expected_token(&s, &a, &device.id, NOW).await?,
            "{}",
            device.id
        );
        let summary = &device.summary;
        let counts = (
            summary.sessions,
            summary.native_handoffs,
            summary.notification_subscriptions,
            summary.pending_sign_ins,
            summary.sync_registered,
        );
        let expected = match device.id.as_str() {
            "grant-only" => (0, 0, 0, 1, false),
            "handoff-only" => (0, 1, 0, 0, false),
            "registration-only" => (0, 0, 0, 0, true),
            "session-only" => (1, 0, 0, 0, false),
            "subscription-only" => (0, 0, 1, 0, false),
            other => panic!("unexpected device {other}"),
        };
        assert_eq!(counts, expected, "{}", device.id);
        assert_eq!(device.active_sessions, summary.sessions);
    }
    let by = |name: &str| listed.iter().find(|d| d.id == name).unwrap();
    assert_eq!(by("grant-only").last_synced_at, None);
    assert_eq!(by("handoff-only").last_synced_at, None);
    assert_eq!(by("registration-only").last_synced_at, Some(1));
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum Left {
    Handoff,
    Subscription,
}

async fn add_member(s: &atlas_core::Store, a: &str, device: &str, left: Left) -> Result<String> {
    match left {
        Left::Handoff => add_handoff(s, a, device, FAR).await,
        Left::Subscription => add_subscription(s, a, device).await,
    }
}

/// R1: a device whose last session has gone, leaving only a live native handoff or an active
/// subscription, is still listed, so the fresh token the stale-state protocol asks for exists.
/// Then: the old token is refused with the member intact, the fresh one retires it, both
/// operations replay, and everything is scoped to the account (a same-named device of another
/// account, and the same operation ID under it, are unaffected and independent).
async fn a_device_left_with_one_member(left: Left) -> Result<()> {
    let (_d, s) = fixture().await?;
    let (a, b) = (account(&s).await?, account(&s).await?);
    let session = add_session(&s, &a, PHONE, FAR).await?;
    let member = add_member(&s, &a, PHONE, left).await?;
    // Bob's same-named device is in the same shape: a session and the same kind of member.
    let bob_session = add_session(&s, &b, PHONE, FAR).await?;
    add_member(&s, &b, PHONE, left).await?;
    let offered = listed_token(&s, &a, PHONE).await?;

    // Ordinary logout removes the last session; the device must stay listed.
    sqlx::query("DELETE FROM sessions WHERE session_id=$1")
        .bind(&session)
        .execute(&s.pool)
        .await?;
    let listed = s.devices(&a, NOW).await?;
    assert_eq!(
        listed.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
        [PHONE],
        "{left:?}: the device is still listed"
    );
    let phone = &listed[0];
    assert_eq!(phone.active_sessions, 0);
    let (handoffs, subscriptions) = match left {
        Left::Handoff => (1, 0),
        Left::Subscription => (0, 1),
    };
    assert_eq!(
        (
            phone.summary.sessions,
            phone.summary.native_handoffs,
            phone.summary.notification_subscriptions,
            phone.summary.sync_registered
        ),
        (0, handoffs, subscriptions, false)
    );
    let fresh = phone.state_token.clone();
    assert_ne!(fresh, offered);
    assert_eq!(fresh, expected_token(&s, &a, PHONE, NOW).await?);

    // The token offered before the logout is refused: nothing is deleted, the outcome recorded.
    let before = snapshot(&s, &a, PHONE).await?;
    let (stale_op, op) = (id(), id());
    let rejected = s.retire_device(&a, PHONE, &stale_op, &offered, NOW).await?;
    assert_eq!(rejected.outcome, Outcome::RejectedStale, "{left:?}");
    assert_eq!(snapshot(&s, &a, PHONE).await?, before);

    // Bob's device is another account's: not in Alice's listing, and Alice's token does not
    // match it (a rejection, with Bob's rows untouched and nothing in Bob's ledger for it).
    sqlx::query("DELETE FROM sessions WHERE session_id=$1")
        .bind(&bob_session)
        .execute(&s.pool)
        .await?;
    let bob_before = snapshot(&s, &b, PHONE).await?;
    assert_eq!(
        s.retire_device(&b, PHONE, &id(), &fresh, NOW)
            .await?
            .outcome,
        Outcome::RejectedStale,
        "{left:?}: Alice's token is not Bob's state"
    );
    assert_eq!(snapshot(&s, &b, PHONE).await?, bob_before);
    let bob_listed = s.devices(&b, NOW).await?;
    assert_eq!(bob_listed.len(), 1);
    assert_eq!(
        bob_listed[0].state_token,
        expected_token(&s, &b, PHONE, NOW).await?
    );
    assert_ne!(bob_listed[0].state_token, fresh);

    // The fresh token retires exactly the member that outlived the session.
    let done = s.retire_device(&a, PHONE, &op, &fresh, NOW).await?;
    assert_eq!(done.outcome, Outcome::ConfirmedApplied, "{left:?}");
    let after = snapshot(&s, &a, PHONE).await?;
    assert!(after.sessions.is_empty() && after.handoffs.is_empty());
    match left {
        Left::Handoff => assert!(after.subscriptions.is_empty()),
        Left::Subscription => {
            assert_eq!(after.subscriptions.len(), 1);
            assert_eq!(after.subscriptions[0].0, member);
            assert_eq!(
                (after.subscriptions[0].2, after.subscriptions[0].3.as_str()),
                (0, "")
            );
        }
    }
    assert!(
        s.devices(&a, NOW).await?.is_empty(),
        "nothing live remains, so nothing is listed"
    );
    assert_eq!(
        outcomes(&ledger_rows(&s, &a).await?)
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        ["confirmed_applied", "rejected_stale"].into()
    );

    // Replay: the identical calls answer with what was recorded, even though the device has since
    // been recreated, and leave the newcomer alone.
    let newcomer = add_member(&s, &a, PHONE, left).await?;
    let recreated = snapshot(&s, &a, PHONE).await?;
    assert_eq!(
        s.retire_device(&a, PHONE, &op, &fresh, NOW).await?.outcome,
        Outcome::ConfirmedApplied
    );
    assert_eq!(
        s.retire_device(&a, PHONE, &stale_op, &offered, NOW)
            .await?
            .outcome,
        Outcome::RejectedStale
    );
    assert_eq!(snapshot(&s, &a, PHONE).await?, recreated);
    assert_ne!(newcomer, member);

    // Isolation: Bob is untouched by everything Alice did; the same operation ID under Bob is
    // evaluated on Bob's own state, not replayed from Alice's ledger row.
    assert_eq!(snapshot(&s, &b, PHONE).await?, bob_before);
    assert!(
        ledger_rows(&s, &b)
            .await?
            .iter()
            .all(|r| r.2 == "rejected_stale")
    );
    let bob_token = listed_token(&s, &b, PHONE).await?;
    let bob_done = s.retire_device(&b, PHONE, &op, &bob_token, NOW).await?;
    assert_eq!(bob_done.outcome, Outcome::ConfirmedApplied);
    assert_eq!(bob_done.account_id, b);
    assert_eq!(
        snapshot(&s, &a, PHONE).await?,
        recreated,
        "Alice unaffected"
    );
    Ok(())
}

#[tokio::test]
async fn a_device_left_with_only_a_handoff_can_still_be_retired() -> Result<()> {
    a_device_left_with_one_member(Left::Handoff).await
}

#[tokio::test]
async fn a_device_left_with_only_a_subscription_can_still_be_retired() -> Result<()> {
    a_device_left_with_one_member(Left::Subscription).await
}

/// N4: nothing committed after the listing's first read reaches its summary or its token.
#[tokio::test]
async fn listing_is_one_snapshot() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    add_session(&s, &a, "phone", FAR).await?;
    add_registration(&s, &a, "phone").await?;
    let before = listed_token(&s, &a, "phone").await?;
    let mut gate = s.hooks().arm("devices.between_reads");
    let reader = {
        let (s, a) = (s.clone(), a.clone());
        tokio::spawn(async move { s.devices(&a, NOW).await })
    };
    gate.reached().await;
    // Committed while the listing is between its reads.
    add_subscription(&s, &a, "phone").await?;
    gate.release();
    let listed = reader.await??;
    let phone = &listed[0];
    assert_eq!(phone.summary.notification_subscriptions, 0);
    assert_eq!(phone.state_token, before);
    // The next listing sees it.
    let after = s.devices(&a, NOW).await?;
    assert_eq!(after[0].summary.notification_subscriptions, 1);
    assert_ne!(after[0].state_token, before);
    Ok(())
}
