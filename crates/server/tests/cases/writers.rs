//! Check (ae): the real `begin_serial()` writers of an approved-state member, each run against a
//! paused retirement in both orders, on both engines: session issue (native login and the browser
//! OIDC callback), native-handoff creation (the native OIDC callback) and consumption (the native
//! exchange), and password change. Subscription set is a core function and is in
//! `atlas-core`'s `retirement_coordination`.
//!
//! Every case asserts (i) the ledger row, (ii) the writer's own HTTP result and (iii) the rows that
//! remain. A writer can commit first only at `retire.before_begin` (on both engines a
//! `begin_serial()` writer blocks on `sync_clock` or the write lock from the retirement's first
//! statement); "retirement first" holds the retirement after its locking reads and observes the
//! writer waiting. On PostgreSQL the same cases are run again with the retirement raised to
//! REPEATABLE READ (check ai). Activation-grant writers (issue, redeem, cancel) are not here: the
//! table they write is BE-Q19's and is not in this task's base.
use anyhow::Result;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::support::http::request;
use crate::support::retirement::{
    BEFORE_BEGIN, PASSWORD, RETIREMENT_FIRST, Reply, Scene, World, assert_blocked,
    assert_isolation, bearer, browser_start, handoff_code, id, ledger, native_handoff,
    native_start, postgres, spawn_native_callback, spawn_native_exchange, spawn_request_with,
    world_with_oidc,
};
use atlas_core::calendars::ReminderCommand;

const NEW_PASSWORD: &str = "changed-password-456";

#[derive(Clone, Copy, Debug)]
enum Order {
    /// The writer commits before the retirement begins.
    WriterFirst,
    /// The retirement holds its locks; the writer starts, waits, and runs after the commit.
    RetirementFirst,
}

const ORDERS: [Order; 2] = [Order::WriterFirst, Order::RetirementFirst];

async fn scene(raise: bool) -> Result<Scene> {
    Scene::over(world_with_oidc().await?, raise).await
}

struct Raced {
    writer: Reply,
    retirement: Reply,
}

/// Retire `phone` (authenticated by the surviving laptop) and run one writer against it in `order`.
async fn race(
    s: &Scene,
    order: Order,
    operation: &str,
    spawn: impl FnOnce(&Scene) -> JoinHandle<Reply>,
) -> Result<Raced> {
    let raced = race_unchecked(s, order, operation, spawn).await?;
    assert_isolation(&s.w.store, s.raised);
    Ok(raced)
}

async fn race_unchecked(
    s: &Scene,
    order: Order,
    operation: &str,
    spawn: impl FnOnce(&Scene) -> JoinHandle<Reply>,
) -> Result<Raced> {
    match order {
        Order::WriterFirst => {
            let (gate, retirement) = s.hold_retirement(BEFORE_BEGIN, operation).await;
            let writer = spawn(s).await?;
            gate.release();
            Ok(Raced {
                writer,
                retirement: retirement.await?,
            })
        }
        Order::RetirementFirst => {
            let (gate, retirement) = s.hold_retirement(RETIREMENT_FIRST, operation).await;
            let mut writer = spawn(s);
            assert_blocked(&s.w.store, RETIREMENT_FIRST, &mut writer).await?;
            gate.release();
            let retirement = retirement.await?;
            Ok(Raced {
                writer: writer.await?,
                retirement,
            })
        }
    }
}

/// The phone's rows, whatever their liveness (every fixture row is live).
#[derive(Debug)]
struct Rows {
    sessions: Vec<String>,
    handoffs: i64,
    /// `(id, active, version, secret)`
    subscriptions: Vec<(String, i64, i64, String)>,
}

async fn rows(w: &World) -> Result<Rows> {
    Ok(Rows {
        sessions: sqlx::query_scalar("SELECT session_id FROM sessions WHERE account_id=$1 AND device_id='phone' ORDER BY session_id")
            .bind(&w.alice)
            .fetch_all(&w.store.pool)
            .await?,
        handoffs: sqlx::query_scalar("SELECT COUNT(*) FROM native_handoffs WHERE account_id=$1 AND device_id='phone'")
            .bind(&w.alice)
            .fetch_one(&w.store.pool)
            .await?,
        subscriptions: sqlx::query_as("SELECT id,active,version,secret FROM notification_subscriptions WHERE account_id=$1 AND device_id='phone' ORDER BY id")
            .bind(&w.alice)
            .fetch_all(&w.store.pool)
            .await?,
    })
}

fn assert_retirement(reply: &Reply, status: StatusCode, outcome: &str, context: &str) {
    assert_eq!(
        (reply.0, reply.2["outcome"].as_str()),
        (status, Some(outcome)),
        "{context}: {}",
        reply.2
    );
    assert!(!reply.1.contains_key("set-cookie"), "{context}");
}

async fn ledger_is(w: &World, op: &str, outcome: &str, context: &str) -> Result<()> {
    assert_eq!(
        ledger(&w.store).await?,
        [(op.to_owned(), outcome.to_owned())],
        "{context}"
    );
    Ok(())
}

/// The status of an authenticated read, to tell a live credential from a dead one.
async fn credential_status(w: &World, token: &str) -> StatusCode {
    request(
        &w.app,
        "GET",
        "sessions",
        &[("authorization", &bearer(token))],
        Value::Null,
    )
    .await
    .0
}

// ------------------------------------------------------------------------------ session issue

/// Native login (`POST /sessions`) issues a session for the device.
/// Writer first: the new session is a member the retirement never saw, so it is `rejected_stale`
/// and neither session is deleted. Retirement first: the retirement removes the old session and the
/// login then creates the device's *new incarnation*, which the retirement never touched.
async fn session_issue(order: Order, raise: bool) -> Result<()> {
    let s = scene(raise).await?;
    let op = id();
    let body = json!({"username":"device-alice","password":PASSWORD,"device_id":"phone"});
    let raced = race(&s, order, &op, |s| {
        spawn_request_with(&s.w.app, "POST", "sessions".into(), vec![], body)
    })
    .await?;
    let context = format!("session issue, {order:?}");
    assert_eq!(
        raced.writer.0,
        StatusCode::OK,
        "{context}: {}",
        raced.writer.2
    );
    let issued = raced.writer.2["access_token"].as_str().unwrap();
    let phone = rows(&s.w).await?;
    match order {
        Order::WriterFirst => {
            assert_retirement(
                &raced.retirement,
                StatusCode::CONFLICT,
                "rejected_stale",
                &context,
            );
            ledger_is(&s.w, &op, "rejected_stale", &context).await?;
            assert_eq!(
                phone.sessions.len(),
                2,
                "{context}: neither session deleted"
            );
            assert!(phone.sessions.contains(&s.phone_session));
            assert_eq!(credential_status(&s.w, &s.phone).await, StatusCode::OK);
            assert_eq!(credential_status(&s.w, issued).await, StatusCode::OK);
        }
        Order::RetirementFirst => {
            assert_retirement(
                &raced.retirement,
                StatusCode::OK,
                "confirmed_applied",
                &context,
            );
            ledger_is(&s.w, &op, "confirmed_applied", &context).await?;
            assert_eq!(
                phone.sessions.len(),
                1,
                "{context}: only the new incarnation"
            );
            assert!(!phone.sessions.contains(&s.phone_session));
            assert_eq!(
                credential_status(&s.w, &s.phone).await,
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(credential_status(&s.w, issued).await, StatusCode::OK);
        }
    }
    Ok(())
}

/// The browser OIDC callback (`GET /oidc/callback`) issues a session for the device through the
/// same `sessions::issue`, from its own route. Same outcomes as the native login.
async fn browser_callback_issue(order: Order, raise: bool) -> Result<()> {
    let s = scene(raise).await?;
    let op = id();
    let (path, cookie) = browser_start(&s.w, "phone").await;
    let raced = race(&s, order, &op, |s| {
        spawn_request_with(
            &s.w.app,
            "GET",
            path,
            vec![("cookie".into(), cookie)],
            Value::Null,
        )
    })
    .await?;
    let context = format!("browser callback, {order:?}");
    assert_eq!(
        raced.writer.0,
        StatusCode::SEE_OTHER,
        "{context}: {}",
        raced.writer.2
    );
    let phone = rows(&s.w).await?;
    match order {
        Order::WriterFirst => {
            assert_retirement(
                &raced.retirement,
                StatusCode::CONFLICT,
                "rejected_stale",
                &context,
            );
            ledger_is(&s.w, &op, "rejected_stale", &context).await?;
            assert_eq!(phone.sessions.len(), 2, "{context}");
            assert!(phone.sessions.contains(&s.phone_session));
        }
        Order::RetirementFirst => {
            assert_retirement(
                &raced.retirement,
                StatusCode::OK,
                "confirmed_applied",
                &context,
            );
            ledger_is(&s.w, &op, "confirmed_applied", &context).await?;
            assert_eq!(phone.sessions.len(), 1, "{context}");
            assert!(!phone.sessions.contains(&s.phone_session));
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------- native handoff create

/// The native OIDC callback creates a handoff for the device. Writer first: a new member, so
/// `rejected_stale`, and the handoff still exchanges. Retirement first: the handoff belongs to the
/// device's new incarnation, survives, and exchanges into a session on it.
async fn handoff_create(order: Order, raise: bool) -> Result<()> {
    let s = scene(raise).await?;
    let op = id();
    let flow = native_start(&s.w, "phone").await;
    let raced = race(&s, order, &op, |s| spawn_native_callback(&s.w, &flow)).await?;
    let context = format!("handoff create, {order:?}");
    let code = handoff_code(&raced.writer);
    let phone = rows(&s.w).await?;
    assert_eq!(phone.handoffs, 1, "{context}");
    match order {
        Order::WriterFirst => {
            assert_retirement(
                &raced.retirement,
                StatusCode::CONFLICT,
                "rejected_stale",
                &context,
            );
            ledger_is(&s.w, &op, "rejected_stale", &context).await?;
            assert_eq!(
                phone.sessions,
                std::slice::from_ref(&s.phone_session),
                "{context}"
            );
        }
        Order::RetirementFirst => {
            assert_retirement(
                &raced.retirement,
                StatusCode::OK,
                "confirmed_applied",
                &context,
            );
            ledger_is(&s.w, &op, "confirmed_applied", &context).await?;
            assert!(phone.sessions.is_empty(), "{context}");
        }
    }
    // Either way the handoff is intact and consumable.
    let exchanged = spawn_native_exchange(&s.w, &code, &flow.verifier).await?;
    assert_eq!(exchanged.0, StatusCode::OK, "{context}: {}", exchanged.2);
    assert_eq!(rows(&s.w).await?.handoffs, 0, "{context}: handoff consumed");
    Ok(())
}

// --------------------------------------------------------------------- native handoff consume

/// The native exchange consumes the device's handoff and issues a session for it. Writer first:
/// a session is added and the handoff leaves (components 1 and 2 change), so `rejected_stale` with
/// both sessions intact. Retirement first: the retirement deletes the handoff, and the exchange
/// finds nothing: `401`, no session created.
async fn handoff_consume(order: Order, raise: bool) -> Result<()> {
    let mut s = scene(raise).await?;
    let (code, verifier) = native_handoff(&s.w, "phone").await;
    s.refresh().await;
    let op = id();
    let raced = race(&s, order, &op, |s| {
        spawn_native_exchange(&s.w, &code, &verifier)
    })
    .await?;
    let context = format!("handoff consume, {order:?}");
    let phone = rows(&s.w).await?;
    assert_eq!(
        phone.handoffs, 0,
        "{context}: the handoff is gone either way"
    );
    match order {
        Order::WriterFirst => {
            assert_eq!(
                raced.writer.0,
                StatusCode::OK,
                "{context}: {}",
                raced.writer.2
            );
            assert_eq!(raced.writer.2["account_id"], s.w.alice.as_str());
            assert_retirement(
                &raced.retirement,
                StatusCode::CONFLICT,
                "rejected_stale",
                &context,
            );
            ledger_is(&s.w, &op, "rejected_stale", &context).await?;
            assert_eq!(
                phone.sessions.len(),
                2,
                "{context}: neither session deleted"
            );
            assert!(phone.sessions.contains(&s.phone_session));
        }
        Order::RetirementFirst => {
            assert_eq!(
                (raced.writer.0, raced.writer.2["code"].as_str()),
                (StatusCode::UNAUTHORIZED, Some("unauthenticated")),
                "{context}: {}",
                raced.writer.2
            );
            assert!(raced.writer.2["access_token"].is_null());
            assert_retirement(
                &raced.retirement,
                StatusCode::OK,
                "confirmed_applied",
                &context,
            );
            ledger_is(&s.w, &op, "confirmed_applied", &context).await?;
            assert!(
                phone.sessions.is_empty(),
                "{context}: no session was created"
            );
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------ password change

#[derive(Clone, Copy, Debug)]
enum Caller {
    /// Authenticated by the surviving laptop, which the retirement does not touch.
    Laptop,
    /// Authenticated by the retired device's own session.
    Phone,
}

#[derive(Clone, Copy, Debug)]
enum Beside {
    /// The phone holds only its session, which the password change also removes.
    NothingElse,
    /// The phone also holds an active subscription, which a password change does not remove.
    ASubscription,
}

/// `POST /password` deletes every session and native handoff of the account in one transaction.
/// Which outcome the retirement records, and what the change itself answers, depends on what is
/// left of the device and on whether the caller's session is the one retired.
async fn password_change(order: Order, caller: Caller, beside: Beside, raise: bool) -> Result<()> {
    let mut s = scene(raise).await?;
    if matches!(beside, Beside::ASubscription) {
        s.w.store
            .reminder_command(
                &s.w.alice,
                &id(),
                &ReminderCommand::SetSubscription {
                    id: id(),
                    expected_version: 0,
                    device_id: "phone".into(),
                    transport: "ntfy".into(),
                    secret: "not-used-by-this-test".into(),
                    enabled: true,
                },
            )
            .await?;
        s.refresh().await;
    }
    let before = rows(&s.w).await?;
    let op = id();
    let token = match caller {
        Caller::Laptop => s.laptop.clone(),
        Caller::Phone => s.phone.clone(),
    };
    let body = json!({"current_password":PASSWORD,"new_password":NEW_PASSWORD});
    let raced = race(&s, order, &op, |s| {
        spawn_request_with(
            &s.w.app,
            "POST",
            "password".into(),
            vec![("authorization".into(), bearer(&token))],
            body,
        )
    })
    .await?;
    let context = format!("password change, {order:?}, {caller:?}, {beside:?}");
    let after = rows(&s.w).await?;
    assert!(!raced.writer.1.contains_key("set-cookie"), "{context}");

    // What the retirement recorded, and whether the change took effect.
    let (status, outcome, changed) = match (order, caller, beside) {
        // The change removed every session first: nothing is left, or only the subscription is.
        (Order::WriterFirst, _, Beside::NothingElse) => (StatusCode::OK, "superseded", true),
        (Order::WriterFirst, _, Beside::ASubscription) => {
            (StatusCode::CONFLICT, "rejected_stale", true)
        }
        // The retirement commits first. A caller on the laptop is untouched and still succeeds;
        // a caller whose own session was retired has lost its credential: `401`, nothing changed.
        (Order::RetirementFirst, Caller::Laptop, _) => (StatusCode::OK, "confirmed_applied", true),
        (Order::RetirementFirst, Caller::Phone, _) => (StatusCode::OK, "confirmed_applied", false),
    };
    assert_retirement(&raced.retirement, status, outcome, &context);
    ledger_is(&s.w, &op, outcome, &context).await?;
    if changed {
        assert_eq!(
            raced.writer.0,
            StatusCode::NO_CONTENT,
            "{context}: {}",
            raced.writer.2
        );
    } else {
        assert_eq!(
            (raced.writer.0, raced.writer.2["code"].as_str()),
            (StatusCode::UNAUTHORIZED, Some("unauthenticated")),
            "{context}: {}",
            raced.writer.2
        );
    }

    // The rows that remain.
    match (order, changed, beside) {
        // The change took every session (and handoff) of the account; the retirement, if it ran
        // first, took the phone's rows and deactivated the subscription.
        (Order::WriterFirst, _, Beside::ASubscription) => {
            assert!(after.sessions.is_empty(), "{context}");
            assert_eq!(
                after.subscriptions, before.subscriptions,
                "{context}: untouched"
            );
            assert_eq!(after.subscriptions[0].1, 1, "{context}: still active");
        }
        (Order::RetirementFirst, _, Beside::ASubscription) => {
            assert!(after.sessions.is_empty(), "{context}");
            assert_eq!(after.subscriptions.len(), 1);
            assert_eq!(
                (after.subscriptions[0].1, after.subscriptions[0].3.as_str()),
                (0, ""),
                "{context}: deactivated by the retirement's own effects"
            );
        }
        _ => assert!(after.sessions.is_empty(), "{context}"),
    }
    // Every session of the account is gone after a successful change; after a refused one only
    // the laptop's survives.
    let account_sessions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE account_id=$1")
            .bind(&s.w.alice)
            .fetch_one(&s.w.store.pool)
            .await?;
    assert_eq!(account_sessions, if changed { 0 } else { 1 }, "{context}");

    // The password itself: the new one works exactly when the change succeeded.
    let attempt = |password: &'static str| {
        let app = s.w.app.clone();
        async move {
            request(
                &app,
                "POST",
                "sessions",
                &[],
                json!({"username":"device-alice","password":password,"device_id":"probe"}),
            )
            .await
            .0
        }
    };
    let (new_status, old_status) = (attempt(NEW_PASSWORD).await, attempt(PASSWORD).await);
    if changed {
        assert_eq!(
            (new_status, old_status),
            (StatusCode::OK, StatusCode::UNAUTHORIZED),
            "{context}"
        );
    } else {
        assert_eq!(
            (new_status, old_status),
            (StatusCode::UNAUTHORIZED, StatusCode::OK),
            "{context}"
        );
    }
    Ok(())
}

// ------------------------------------------------------------------------------------ tests

macro_rules! both_orders {
    ($writer_first:ident, $retirement_first:ident, $run:expr) => {
        #[tokio::test]
        async fn $writer_first() -> Result<()> {
            ($run)(Order::WriterFirst, false).await
        }
        #[tokio::test]
        async fn $retirement_first() -> Result<()> {
            ($run)(Order::RetirementFirst, false).await
        }
    };
}

both_orders!(
    a_login_first_makes_the_retirement_stale,
    a_login_after_the_retirement_creates_a_new_incarnation,
    session_issue
);
both_orders!(
    a_browser_callback_first_makes_the_retirement_stale,
    a_browser_callback_after_the_retirement_creates_a_new_incarnation,
    browser_callback_issue
);
both_orders!(
    a_handoff_created_first_makes_the_retirement_stale,
    a_handoff_created_after_the_retirement_survives_it,
    handoff_create
);
both_orders!(
    a_handoff_consumed_first_makes_the_retirement_stale,
    a_handoff_consumed_after_the_retirement_finds_nothing,
    handoff_consume
);
both_orders!(
    a_password_change_first_supersedes_a_session_only_device,
    a_password_change_by_another_device_after_the_retirement_succeeds,
    |order, raise| password_change(order, Caller::Laptop, Beside::NothingElse, raise)
);
both_orders!(
    a_password_change_first_leaves_a_subscription_so_the_retirement_is_stale,
    a_password_change_after_the_retirement_leaves_the_subscription_deactivated,
    |order, raise| password_change(order, Caller::Laptop, Beside::ASubscription, raise)
);
both_orders!(
    a_password_change_by_the_device_itself_first_supersedes_the_retirement,
    a_password_change_by_the_retired_device_is_refused_and_changes_nothing,
    |order, raise| password_change(order, Caller::Phone, Beside::NothingElse, raise)
);

/// (ai) Every real writer above, in both orders, again with the retirement raised to REPEATABLE
/// READ (PostgreSQL only). The difference is retries, not outcomes.
#[tokio::test]
async fn every_real_writer_also_passes_at_repeatable_read() -> Result<()> {
    if !postgres() {
        return Ok(());
    }
    for order in ORDERS {
        session_issue(order, true).await?;
        browser_callback_issue(order, true).await?;
        handoff_create(order, true).await?;
        handoff_consume(order, true).await?;
        for (caller, beside) in [
            (Caller::Laptop, Beside::NothingElse),
            (Caller::Laptop, Beside::ASubscription),
            (Caller::Phone, Beside::NothingElse),
        ] {
            password_change(order, caller, beside, true).await?;
        }
    }
    Ok(())
}
