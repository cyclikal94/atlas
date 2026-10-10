//! BE-Q25: device names and client labels in the core store. A profile is display metadata: it
//! is visible exactly while its device is listed, never changes a `state_token`, is deleted with a
//! confirmed retirement and on restore, and a device that is forgotten starts unnamed whichever
//! transition lists it again. The server routes and real session, grant and handoff issue are
//! covered in the server crate.
use anyhow::Result;
use atlas_core::Store;
use atlas_core::calendars::ReminderCommand;
use atlas_core::devices::{
    Browser, Channel, ClientLabel, Continuity, Platform, forget_unlisted, order_membership,
    record_client,
};
use atlas_core::operations::Outcome;

use crate::support::database::postgres;
use crate::support::devices::{
    FAR, add_handoff, add_registration, add_session, expected_token, id, populate,
};
use crate::support::resource_commands::{account, fixture};
use crate::support::schedule::assert_blocked;
use sqlx::{Any, Transaction};
use std::time::Duration;
use tokio::task::JoinHandle;

const NOW: i64 = 1000;

const FIREFOX_LINUX: ClientLabel = ClientLabel {
    channel: Channel::Browser,
    browser: Some(Browser::Firefox),
    platform: Platform::Linux,
};
const NATIVE_IOS: ClientLabel = ClientLabel {
    channel: Channel::Native,
    browser: None,
    platform: Platform::Ios,
};

/// What `sessions::issue` does in the server for a sign-in that consumes no member (password and
/// registration): inside one `begin_serial()` transaction holding the account's membership
/// ordering point, record the label (pruning orphans first), then insert the session.
async fn sign_in(s: &Store, a: &str, device: &str, client: &ClientLabel, now: i64) -> Result<()> {
    let mut tx = s.begin_serial().await?;
    let ordered = order_membership(&mut tx, a, &now).await?;
    record_client(&mut tx, &ordered, device, client, &Continuity::Unknown).await?;
    sqlx::query("INSERT INTO sessions(token_hash,account_id,device_id,expires_at,session_id,created_at,auth_kind) VALUES ($1,$2,$3,$4,$5,$6,'local')")
        .bind(id()).bind(a).bind(device).bind(now + 86400).bind(id()).bind(now)
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn profile(
    s: &Store,
    a: &str,
    device: &str,
) -> Result<Option<(Option<String>, Option<String>)>> {
    Ok(sqlx::query_as(
        "SELECT name,client_platform FROM device_profiles WHERE account_id=$1 AND device_id=$2",
    )
    .bind(a)
    .bind(device)
    .fetch_optional(&s.pool)
    .await?)
}

async fn listed(
    s: &Store,
    a: &str,
    device: &str,
    now: i64,
) -> Result<Option<(Option<String>, Option<ClientLabel>, String)>> {
    Ok(s.devices(a, now)
        .await?
        .devices
        .into_iter()
        .find(|d| d.id == device)
        .map(|d| (d.name, d.client, d.state_token)))
}

#[tokio::test]
async fn a_name_and_label_are_listed_and_never_change_the_token() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    populate(&s, &a, "phone").await?;
    let (name, client, before) = listed(&s, &a, "phone", NOW).await?.unwrap();
    assert_eq!((name, client), (None, None), "no profile yet");

    sign_in(&s, &a, "phone", &NATIVE_IOS, NOW).await?;
    let token = expected_token(&s, &a, "phone", NOW).await?;
    s.set_device_name(&a, "phone", Some("Kieran's phone"), NOW)
        .await?;
    let (name, client, after) = listed(&s, &a, "phone", NOW).await?.unwrap();
    assert_eq!(name.as_deref(), Some("Kieran's phone"));
    assert_eq!(client, Some(NATIVE_IOS));
    assert_eq!(after, token, "the name is not part of the approved state");
    assert_ne!(after, before, "the new session is");

    // The latest sign-in's label replaces the device's; the name is untouched. Clearing is
    // idempotent.
    sign_in(&s, &a, "phone", &FIREFOX_LINUX, NOW + 1).await?;
    let (name, client, _) = listed(&s, &a, "phone", NOW + 1).await?.unwrap();
    assert_eq!(
        (name.as_deref(), client),
        (Some("Kieran's phone"), Some(FIREFOX_LINUX))
    );
    for _ in 0..2 {
        s.set_device_name(&a, "phone", None, NOW + 1).await?;
        let (name, client, _) = listed(&s, &a, "phone", NOW + 1).await?.unwrap();
        assert_eq!((name, client), (None, Some(FIREFOX_LINUX)));
    }
    // Naming a device that has never signed in since 1005 creates a name-only profile.
    add_registration(&s, &a, "watch").await?;
    s.set_device_name(&a, "watch", Some("Watch"), NOW).await?;
    let (name, client, _) = listed(&s, &a, "watch", NOW).await?.unwrap();
    assert_eq!((name.as_deref(), client), (Some("Watch"), None));
    Ok(())
}

#[tokio::test]
async fn only_a_listed_device_of_the_caller_can_be_named() -> Result<()> {
    let (_d, s) = fixture().await?;
    let (a, b) = (account(&s).await?, account(&s).await?);
    add_session(&s, &a, "phone", FAR).await?;
    add_session(&s, &a, "expired", NOW).await?;
    add_session(&s, &b, "bobs-phone", FAR).await?;
    for (device, expected) in [
        ("unknown", "not_found"),
        ("bobs-phone", "not_found"),
        ("expired", "not_found"),
        ("", "invalid_value"),
    ] {
        let error = s
            .set_device_name(&a, device, Some("Name"), NOW)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), expected, "{device:?}");
        let error = s.set_device_name(&a, device, None, NOW).await.unwrap_err();
        assert_eq!(error.to_string(), expected, "clear {device:?}");
    }
    let long = "d".repeat(101);
    assert_eq!(
        s.set_device_name(&a, &long, Some("Name"), NOW)
            .await
            .unwrap_err()
            .to_string(),
        "invalid_value"
    );
    for bad in ["", " padded", "rtl\u{202E}", &"x".repeat(65)] {
        assert_eq!(
            s.set_device_name(&a, "phone", Some(bad), NOW)
                .await
                .unwrap_err()
                .to_string(),
            "invalid_value",
            "{bad:?}"
        );
    }
    // Nothing was written for either account.
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM device_profiles")
        .fetch_one(&s.pool)
        .await?;
    assert_eq!(rows, 0);
    // The same device ID under another account is an independent profile.
    add_session(&s, &b, "phone", FAR).await?;
    s.set_device_name(&a, "phone", Some("Alice's"), NOW).await?;
    s.set_device_name(&b, "phone", Some("Bob's"), NOW).await?;
    assert_eq!(
        listed(&s, &a, "phone", NOW).await?.unwrap().0.as_deref(),
        Some("Alice's")
    );
    assert_eq!(
        listed(&s, &b, "phone", NOW).await?.unwrap().0.as_deref(),
        Some("Bob's")
    );
    Ok(())
}

/// B5: a stale retirement keeps the name; a confirmed one deletes the profile, so the device's
/// next incarnation starts unnamed.
#[tokio::test]
async fn retirement_deletes_the_profile_only_when_it_applies() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    sign_in(&s, &a, "phone", &NATIVE_IOS, NOW).await?;
    s.set_device_name(&a, "phone", Some("Phone"), NOW).await?;
    let stale = expected_token(&s, &a, "phone", NOW).await?;
    sign_in(&s, &a, "phone", &NATIVE_IOS, NOW).await?;
    let rejected = s.retire_device(&a, "phone", &id(), &stale, NOW).await?;
    assert_eq!(rejected.outcome, Outcome::RejectedStale);
    assert_eq!(
        profile(&s, &a, "phone").await?,
        Some((Some("Phone".into()), Some("ios".into())))
    );

    // A rename between the read and the retirement does not make it stale.
    let token = expected_token(&s, &a, "phone", NOW).await?;
    s.set_device_name(&a, "phone", Some("Renamed"), NOW).await?;
    let applied = s.retire_device(&a, "phone", &id(), &token, NOW).await?;
    assert_eq!(applied.outcome, Outcome::ConfirmedApplied);
    assert_eq!(profile(&s, &a, "phone").await?, None);
    assert_eq!(listed(&s, &a, "phone", NOW).await?, None);
    assert_eq!(
        s.set_device_name(&a, "phone", Some("Ghost"), NOW)
            .await
            .unwrap_err()
            .to_string(),
        "not_found"
    );

    sign_in(&s, &a, "phone", &FIREFOX_LINUX, NOW + 1).await?;
    let (name, client, _) = listed(&s, &a, "phone", NOW + 1).await?.unwrap();
    assert_eq!((name, client), (None, Some(FIREFOX_LINUX)));
    Ok(())
}

/// B8: profiles of devices with no live member are pruned at the account's next sign-in, before
/// the new profile is written; listed devices and other accounts are untouched.
#[tokio::test]
async fn orphan_profiles_are_pruned_at_the_next_sign_in() -> Result<()> {
    let (_d, s) = fixture().await?;
    let (a, b) = (account(&s).await?, account(&s).await?);
    let later = NOW + 2 * 86400;
    for device in ["phone", "laptop"] {
        sign_in(&s, &a, device, &NATIVE_IOS, NOW).await?;
        s.set_device_name(&a, device, Some(device), NOW).await?;
    }
    add_registration(&s, &a, "laptop").await?;
    sign_in(&s, &b, "phone", &NATIVE_IOS, NOW).await?;
    s.set_device_name(&b, "phone", Some("Bob"), NOW).await?;

    // Every session has expired: the phone has no member left, the laptop keeps its registration.
    assert_eq!(listed(&s, &a, "phone", later).await?, None, "not listed");
    assert!(profile(&s, &a, "phone").await?.is_some(), "not yet pruned");
    sign_in(&s, &a, "tablet", &FIREFOX_LINUX, later).await?;
    assert_eq!(profile(&s, &a, "phone").await?, None, "pruned");
    assert_eq!(
        listed(&s, &a, "laptop", later).await?.unwrap().0.as_deref(),
        Some("laptop"),
        "a listed device keeps its name"
    );
    assert_eq!(
        profile(&s, &b, "phone").await?.unwrap().0.as_deref(),
        Some("Bob"),
        "another account's orphan is that account's to prune"
    );

    // The same device returning after it was forgotten starts unnamed: the prune runs before
    // its own new profile is written.
    sign_in(&s, &b, "phone", &FIREFOX_LINUX, later).await?;
    let (name, client, _) = listed(&s, &b, "phone", later).await?.unwrap();
    assert_eq!((name, client), (None, Some(FIREFOX_LINUX)));
    Ok(())
}

/// B10: restore forgets every device, and with it every name and label.
#[tokio::test]
async fn restore_preparation_forgets_every_profile() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    sign_in(&s, &a, "phone", &NATIVE_IOS, NOW).await?;
    s.set_device_name(&a, "phone", Some("Phone"), NOW).await?;
    s.prepare_restored_database().await?;
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM device_profiles")
        .fetch_one(&s.pool)
        .await?;
    assert_eq!(rows, 0);
    Ok(())
}

fn real_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn subscribe(id: &str, expected_version: i64, device: &str) -> ReminderCommand {
    ReminderCommand::SetSubscription {
        id: id.to_owned(),
        expected_version,
        device_id: device.to_owned(),
        transport: "ntfy".to_owned(),
        secret: "secret".to_owned(),
        enabled: true,
    }
}

/// R1 (revision 1): an active subscription lists its device, and its `device_id` is chosen by the
/// client, so a subscription change is a transition that can list a forgotten device again. It
/// prunes first: the forgotten device returns unnamed, while a device that stayed listed keeps its
/// name. Moving a subscription to another device also drops the moved-to device's stale profile.
#[tokio::test]
async fn a_subscription_lists_a_forgotten_device_unnamed() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    let now = real_now();
    let past = now - 2 * 86400;
    // Named while their sessions were live; by `now` every session has expired, except the
    // laptop's. `reminder_command` runs at the real time, so this fixture is anchored to it.
    for device in ["phone", "tablet", "laptop"] {
        sign_in(&s, &a, device, &NATIVE_IOS, past).await?;
        s.set_device_name(&a, device, Some(device), past).await?;
    }
    add_session(&s, &a, "laptop", FAR).await?;
    assert_eq!(listed(&s, &a, "phone", now).await?, None);
    assert!(profile(&s, &a, "phone").await?.is_some(), "not yet pruned");

    let subscription = id();
    s.reminder_command(&a, &id(), &subscribe(&subscription, 0, "phone"))
        .await?;
    let (name, client, _) = listed(&s, &a, "phone", now).await?.expect("listed again");
    assert_eq!(
        (name, client),
        (None, None),
        "the forgotten phone returns unnamed"
    );
    assert_eq!(
        profile(&s, &a, "tablet").await?,
        None,
        "every orphan is pruned"
    );
    assert_eq!(
        listed(&s, &a, "laptop", now).await?.unwrap().0.as_deref(),
        Some("laptop"),
        "a device that stayed listed keeps its name"
    );

    // Name the phone now that it is listed, forget the tablet again, then move the subscription:
    // the tablet is listed unnamed, and the phone (no longer listed) keeps nothing visible.
    s.set_device_name(&a, "phone", Some("Phone"), now).await?;
    sign_in(&s, &a, "tablet", &NATIVE_IOS, past).await?;
    s.set_device_name(&a, "tablet", Some("Old tablet"), past)
        .await?;
    s.reminder_command(&a, &id(), &subscribe(&subscription, 1, "tablet"))
        .await?;
    let (name, _, _) = listed(&s, &a, "tablet", now).await?.expect("listed again");
    assert_eq!(name, None, "the moved-to tablet returns unnamed");
    assert_eq!(listed(&s, &a, "phone", now).await?, None);
    Ok(())
}

/// R2 (revision 1): a transition that consumes a live member of a device and replaces it keeps
/// that device's profile even though, between the consumption and the new member, the device has
/// no member at all; every other orphan of the account is still pruned. With `Unknown` the same
/// intermediate state drops the profile, which is right for a sign-in that consumes nothing.
/// Revision 2: the replacement counts only if the consumed member was live at the transaction's
/// own decision time (`Ordered::replacing`); one that had already expired proves nothing.
#[tokio::test]
async fn replacing_a_live_member_keeps_only_that_devices_profile() -> Result<()> {
    let (_d, s) = fixture().await?;
    let (a, b) = (account(&s).await?, account(&s).await?);
    let later = NOW + 2 * 86400;
    for device in ["tablet", "phone"] {
        sign_in(&s, &a, device, &NATIVE_IOS, NOW).await?;
        s.set_device_name(&a, device, Some(device), NOW).await?;
    }
    sign_in(&s, &b, "tablet", &NATIVE_IOS, NOW).await?;
    s.set_device_name(&b, "tablet", Some("Bob"), NOW).await?;

    // At `later` neither of the account's devices has a member: the state an exchange is in after
    // deleting a handoff-only device's handoff and before inserting its session.
    // A consumed member that had expired by the decision time is no replacement: the profile goes.
    let mut tx = s.begin_serial().await?;
    let ordered = order_membership(&mut tx, &a, &later).await?;
    let expired = ordered.replacing("tablet", later);
    assert_eq!(expired, Continuity::Unknown);
    record_client(&mut tx, &ordered, "tablet", &FIREFOX_LINUX, &expired).await?;
    let named: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM device_profiles WHERE account_id=$1 AND device_id='tablet' AND name IS NOT NULL",
    )
    .bind(&a)
    .fetch_one(&mut *tx)
    .await?;
    assert_eq!(named, 0, "an expired member does not keep the name");
    tx.rollback().await?;
    // The consumed member was live at `later` (it would have expired a minute later).
    let mut tx = s.begin_serial().await?;
    let ordered = order_membership(&mut tx, &a, &later).await?;
    let replacing = ordered.replacing("tablet", later + 60);
    assert!(matches!(replacing, Continuity::Replaces(_)));
    // The proof is for the tablet only.
    assert!(
        record_client(&mut tx, &ordered, "phone", &FIREFOX_LINUX, &replacing)
            .await
            .is_err()
    );
    tx.rollback().await?;
    let mut tx = s.begin_serial().await?;
    let ordered = order_membership(&mut tx, &a, &later).await?;
    let replacing = ordered.replacing("tablet", later + 60);
    record_client(&mut tx, &ordered, "tablet", &FIREFOX_LINUX, &replacing).await?;
    tx.commit().await?;
    assert_eq!(
        profile(&s, &a, "tablet").await?,
        Some((Some("tablet".to_owned()), Some("linux".to_owned()))),
        "the replaced device keeps its name and takes the new label"
    );
    assert_eq!(profile(&s, &a, "phone").await?, None, "other orphans go");
    assert_eq!(
        profile(&s, &b, "tablet").await?.unwrap().0.as_deref(),
        Some("Bob"),
        "another account is untouched"
    );

    let mut tx = s.begin_serial().await?;
    let ordered = order_membership(&mut tx, &a, &later).await?;
    forget_unlisted(&mut tx, &ordered, &Continuity::Unknown).await?;
    tx.commit().await?;
    assert_eq!(
        profile(&s, &a, "tablet").await?,
        None,
        "no keep, no exception"
    );
    Ok(())
}

// ------------------------- revision 2: decisions, removals, expiry and listings at the same time
//
// Review 2 R1 and design-assessment decision P1 (Kieran, 10 October 2026: public listings take
// part in the account's membership ordering). Time is a fixed logical clock here, so the expiry
// boundary is exact and nothing waits out a real timer.

#[derive(Clone, Copy, Debug)]
enum Timed {
    Session,
    Handoff,
    Grant,
}
const TIMED: [Timed; 3] = [Timed::Session, Timed::Handoff, Timed::Grant];
/// When the tablet's only member expires.
const EXPIRES: i64 = NOW + 10;

/// The tablet's only member, of `kind`, live until [`EXPIRES`]; the tablet is named "Tablet".
async fn tablet_with_only(s: &Store, a: &str, kind: Timed) -> Result<()> {
    match kind {
        Timed::Session => {
            add_session(s, a, "tablet", EXPIRES).await?;
        }
        Timed::Handoff => {
            add_handoff(s, a, "tablet", EXPIRES).await?;
        }
        Timed::Grant => {
            sqlx::query("INSERT INTO activation_grants(grant_hash,grant_id,account_id,device_id,auth_kind,challenge_hash,state,failed_verifiers,created_at,expires_at) VALUES ($1,$2,$3,'tablet','local',$4,'issued',0,$5,$6)")
                .bind(id()).bind(id()).bind(a).bind(id()).bind(NOW).bind(EXPIRES)
                .execute(&s.pool).await?;
        }
    }
    s.set_device_name(a, "tablet", Some("Tablet"), NOW).await
}

/// A session-issuing transition for the tablet as the server makes one, decided at `decided`:
/// `begin_serial()`, the account's ordering point, the label (with `replacing`, the continuity of
/// consuming a member of the tablet that expires then) and the new session. Returned uncommitted,
/// still holding the point, with its PostgreSQL backend.
async fn transition(
    s: &Store,
    a: &str,
    decided: i64,
    replacing: Option<i64>,
) -> Result<(Transaction<'static, Any>, Option<i32>)> {
    let mut tx = s.begin_serial().await?;
    let pid = if postgres() {
        Some(
            sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await?,
        )
    } else {
        None
    };
    let ordered = order_membership(&mut tx, a, &decided).await?;
    let continuity = match replacing {
        Some(expires_at) => ordered.replacing("tablet", expires_at),
        None => Continuity::Unknown,
    };
    record_client(&mut tx, &ordered, "tablet", &FIREFOX_LINUX, &continuity).await?;
    sqlx::query("INSERT INTO sessions(token_hash,account_id,device_id,expires_at,session_id,created_at,auth_kind) VALUES ($1,$2,'tablet',$3,$4,$5,'local')")
        .bind(id()).bind(a).bind(decided + 86400).bind(id()).bind(decided)
        .execute(&mut *tx).await?;
    Ok((tx, pid))
}

/// Wait until `count` requests are waiting for the transaction on backend `pid`, which the caller
/// then proves have not answered. PostgreSQL: that many backends in a `Lock` wait whose
/// `pg_blocking_pids` chain leads to it. SQLite (`pid` is `None`): half a second, well inside the
/// busy timeout, for a request that is not waiting to answer.
async fn await_waiting(s: &Store, pid: Option<i32>, count: i64) -> Result<()> {
    if let Some(pid) = pid {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting: i64 = sqlx::query_scalar(
                    "WITH RECURSIVE blocked(pid) AS (SELECT pid FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)) UNION SELECT a.pid FROM pg_stat_activity a JOIN blocked b ON b.pid = ANY(pg_blocking_pids(a.pid))) SELECT COUNT(*) FROM blocked JOIN pg_stat_activity s USING (pid) WHERE s.wait_event_type='Lock'",
                )
                .bind(pid)
                .fetch_one(&s.pool)
                .await?;
                if waiting >= count {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
    } else {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(())
}

fn assert_pending<T>(task: &JoinHandle<T>, case: &str) {
    assert!(
        !task.is_finished(),
        "{case}: a listing answered around a transition in flight"
    );
}

/// P1, transition first: a transition decided while the tablet's only member was live is still in
/// flight when that member expires. `devices` and `sessions` at a time past the expiry wait for it
/// and then find the tablet listed by the new session and still named; neither can report it
/// absent in between. For each timed member, for a plain sign-in and for a replacement of that
/// member. On PostgreSQL another account's listing does not wait (its own account row).
#[tokio::test]
async fn a_listing_after_the_last_member_expires_waits_for_a_transition_decided_before()
-> Result<()> {
    for kind in TIMED {
        for replace in [false, true] {
            let case = format!("{kind:?} replace={replace}");
            let (_d, s) = fixture().await?;
            let (a, b) = (account(&s).await?, account(&s).await?);
            add_session(&s, &b, "tablet", FAR).await?;
            tablet_with_only(&s, &a, kind).await?;
            let (tx, pid) = transition(&s, &a, EXPIRES - 1, replace.then_some(EXPIRES)).await?;
            let devices = {
                let (s, a) = (s.clone(), a.clone());
                tokio::spawn(async move { s.devices(&a, EXPIRES).await })
            };
            let sessions = {
                let (s, a) = (s.clone(), a.clone());
                tokio::spawn(async move { s.sessions(&a, EXPIRES).await })
            };
            await_waiting(&s, pid, 2).await?;
            assert_pending(&devices, &case);
            assert_pending(&sessions, &case);
            if postgres() {
                let other =
                    tokio::time::timeout(Duration::from_secs(5), s.devices(&b, EXPIRES)).await??;
                assert_eq!(
                    other.devices.len(),
                    1,
                    "{case}: another account is not held up"
                );
            }
            tx.commit().await?;
            let tablet = devices
                .await??
                .devices
                .into_iter()
                .find(|d| d.id == "tablet")
                .unwrap_or_else(|| panic!("{case}: the tablet was reported absent"));
            assert_eq!(tablet.name.as_deref(), Some("Tablet"), "{case}");
            assert_eq!(tablet.client, Some(FIREFOX_LINUX), "{case}");
            assert_eq!(tablet.active_sessions, 1, "{case}");
            let rows = sessions.await??;
            assert_eq!(rows.len(), 1, "{case}");
            assert_eq!(rows[0].device_name.as_deref(), Some("Tablet"), "{case}");
        }
    }
    Ok(())
}

/// P1, expiry observed first: once a listing at the expiry time has seen the tablet absent, a
/// transition starts it unnamed, decided at that time or later, also when it replaces the member
/// that has just expired (a member expiring exactly at the decision time is not live). Decided one
/// second earlier (nothing observed absent in between), the same transition keeps the name.
#[tokio::test]
async fn a_transition_after_the_last_member_expired_starts_the_device_unnamed() -> Result<()> {
    for kind in TIMED {
        for replace in [false, true] {
            for decided in [EXPIRES - 1, EXPIRES, EXPIRES + 1] {
                let case = format!("{kind:?} replace={replace} decided={decided}");
                let (_d, s) = fixture().await?;
                let a = account(&s).await?;
                tablet_with_only(&s, &a, kind).await?;
                if decided >= EXPIRES {
                    assert_eq!(listed(&s, &a, "tablet", EXPIRES).await?, None, "{case}");
                    assert!(
                        s.sessions(&a, EXPIRES).await?.is_empty(),
                        "{case}: no session row"
                    );
                }
                let (tx, _) = transition(&s, &a, decided, replace.then_some(EXPIRES)).await?;
                tx.commit().await?;
                let (name, client, _) = listed(&s, &a, "tablet", decided).await?.unwrap();
                let expected = (decided < EXPIRES).then(|| "Tablet".to_owned());
                assert_eq!(name, expected, "{case}");
                assert_eq!(client, Some(FIREFOX_LINUX), "{case}");
                assert_eq!(
                    s.sessions(&a, decided).await?[0].device_name,
                    expected,
                    "{case}"
                );
            }
        }
    }
    Ok(())
}

/// Review 2 R1B at the store, with an exact expiry: the tablet's only session expires at
/// [`EXPIRES`]. A first sync registration decided one second earlier is paused after its prune;
/// a listing at the expiry waits for it and finds the tablet registered and named. In the other
/// order the listing sees the tablet absent and the registration then lists it unnamed.
#[tokio::test]
async fn a_first_sync_registration_follows_the_same_lifetime() -> Result<()> {
    // Registration first.
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    tablet_with_only(&s, &a, Timed::Session).await?;
    let point = "sync.before_registration";
    let mut gate = s.hooks().arm(point);
    let sync = {
        let (s, a) = (s.clone(), a.clone());
        tokio::spawn(async move { s.sync(&a, "tablet", None, 50, EXPIRES - 1).await })
    };
    gate.reached().await;
    let mut reader = {
        let (s, a) = (s.clone(), a.clone());
        tokio::spawn(async move { s.devices(&a, EXPIRES).await })
    };
    assert_blocked(&s, point, &mut reader).await?;
    gate.release();
    sync.await??;
    let tablet = reader
        .await??
        .devices
        .into_iter()
        .find(|d| d.id == "tablet")
        .expect("never reported absent");
    assert!(tablet.summary.sync_registered);
    assert_eq!(tablet.name.as_deref(), Some("Tablet"));

    // Expiry observed first.
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    tablet_with_only(&s, &a, Timed::Session).await?;
    assert_eq!(listed(&s, &a, "tablet", EXPIRES).await?, None);
    s.sync(&a, "tablet", None, 50, EXPIRES).await?;
    let (name, _, _) = listed(&s, &a, "tablet", EXPIRES).await?.unwrap();
    assert_eq!(name, None, "registered after it was seen absent: unnamed");
    Ok(())
}

/// A transition queued behind a listing decides at its own time, after the listing: the account's
/// ordering point is taken before the decision's clock is read. Here a first sync registration is
/// queued while a listing at [`EXPIRES`] holds the point (paused between its reads) and has found
/// the tablet absent; the registration's clock moves on to [`EXPIRES`] before the listing lets it
/// go, so it lists the tablet unnamed. A registration that judged membership by a time read before
/// it waited would keep the name the listing has already seen go.
#[tokio::test]
async fn a_transition_queued_behind_a_listing_decides_at_its_own_time() -> Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering::SeqCst};
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    tablet_with_only(&s, &a, Timed::Session).await?;
    let mut gate = s.hooks().arm("devices.between_reads");
    let listing = {
        let (s, a) = (s.clone(), a.clone());
        tokio::spawn(async move { s.devices(&a, EXPIRES).await })
    };
    gate.reached().await;
    let time = Arc::new(AtomicI64::new(EXPIRES - 1));
    // PostgreSQL: the registration's own backend, from the hook point just before its ordering
    // point (the cases share one database, a schema each, so a search would not be specific).
    let order_point = "sync.before_registration_order";
    let mut ordering = postgres().then(|| s.hooks().arm(order_point));
    let sync = {
        let (s, a, time) = (s.clone(), a.clone(), time.clone());
        tokio::spawn(async move {
            s.sync(&a, "tablet", None, 50, move || time.load(SeqCst))
                .await
        })
    };
    if let Some(mut ordering) = ordering.take() {
        ordering.reached().await;
        let pid = s
            .hooks()
            .backend_pid(order_point)
            .expect("armed in-transaction point");
        ordering.release();
        // The registration is now waiting for the account row the listing holds.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let wait: Option<String> =
                    sqlx::query_scalar("SELECT wait_event_type FROM pg_stat_activity WHERE pid=$1")
                        .bind(pid)
                        .fetch_one(&s.pool)
                        .await?;
                if wait.as_deref() == Some("Lock") {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
    } else {
        // Waiting for the writer slot the listing holds.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_pending(&sync, "queued registration");
    time.store(EXPIRES, SeqCst);
    gate.release();
    let seen = listing.await??;
    assert!(
        seen.devices.iter().all(|d| d.id != "tablet"),
        "the listing saw the tablet absent"
    );
    sync.await??;
    let (name, _, _) = listed(&s, &a, "tablet", EXPIRES).await?.unwrap();
    assert_eq!(name, None, "seen absent, so registered unnamed");
    Ok(())
}

/// The aged-registration sweep removes a member, so it takes the account's ordering point too.
/// Transition first: a sign-in decided while the tablet was listed only by its aged registration
/// holds the point; the sweep waits, then removes the registration, and the tablet, listed by the
/// new session throughout, keeps its name. Sweep first: the tablet is seen absent and the sign-in
/// then lists it unnamed.
#[tokio::test]
async fn the_aged_registration_sweep_follows_the_same_lifetime() -> Result<()> {
    // Older than the 90-day retention of the fixture's registration (`last_seen` 1).
    const SWEPT: i64 = 1 + 90 * 86400 + 1;
    for transition_first in [true, false] {
        let case = format!("transition_first={transition_first}");
        let (_d, s) = fixture().await?;
        let a = account(&s).await?;
        add_registration(&s, &a, "tablet").await?;
        s.set_device_name(&a, "tablet", Some("Tablet"), SWEPT)
            .await?;
        if transition_first {
            let (tx, pid) = transition(&s, &a, SWEPT, None).await?;
            let sweep = {
                let s = s.clone();
                tokio::spawn(async move { s.collect_expired(SWEPT).await })
            };
            await_waiting(&s, pid, 1).await?;
            assert_pending(&sweep, &case);
            tx.commit().await?;
            sweep.await??;
        } else {
            s.collect_expired(SWEPT).await?;
            assert_eq!(listed(&s, &a, "tablet", SWEPT).await?, None, "{case}");
            let (tx, _) = transition(&s, &a, SWEPT, None).await?;
            tx.commit().await?;
        }
        let tablet = s
            .devices(&a, SWEPT)
            .await?
            .devices
            .into_iter()
            .find(|d| d.id == "tablet")
            .expect("listed by the new session");
        assert!(!tablet.summary.sync_registered, "{case}: swept");
        let expected = transition_first.then(|| "Tablet".to_owned());
        assert_eq!(tablet.name, expected, "{case}");
    }
    Ok(())
}
