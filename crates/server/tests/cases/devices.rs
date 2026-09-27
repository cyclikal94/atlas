//! HTTP contract of device retirement and keyed session revocation, on both engines
//! (checks c, e, f, g, h, i, j, k, m, n, v, w, y, z and N7). Schedules that pause a request use the
//! test-only hooks; the release-shaped binary is exercised by `scripts/smoke/devices.py`. The
//! real writers of an approved-state member are in `writers.rs`.
use anyhow::Result;
use atlas_core::calendars::ReminderCommand;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support::http::request;
use crate::support::retirement::{
    PASSWORD, RETIREMENT_FIRST, Route, activate, assert_blocked, assert_error, assert_isolation,
    bearer, cookie_login, device_state, id, issue_grant, ledger, listed, login, native_handoff,
    postgres, retire, retirement_headers, revoke, scene, session_of, spawn_native_exchange,
    spawn_request, world, world_with_oidc, writer_first_point,
};

/// A header-validation case: name, request headers, expected status and error code.
type Case<'a> = (&'a str, Vec<(&'a str, &'a str)>, StatusCode, &'a str);

// ------------------------------------------------------------------------------- the contract

/// (N7), (w), (m) Header handling; refusal leaves everything alone and records nothing.
#[tokio::test]
async fn retirement_headers_are_validated_before_anything_changes() -> Result<()> {
    let w = world(false).await?;
    let phone = login(&w.app, "device-alice", "phone").await;
    let state = device_state(&w.app, &phone, "phone").await;
    let (key, other) = (id(), id());
    let auth = bearer(&phone);
    let short_state = "v1.short".to_owned();
    let wrong_prefix = format!("v2.{}", "A".repeat(43));
    let upper = key.to_uppercase();
    let matrix: Vec<Case> = vec![
        (
            "no Idempotency-Key",
            vec![("authorization", &auth), ("atlas-device-state", &state)],
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
        (
            "no state header",
            vec![("authorization", &auth), ("idempotency-key", &key)],
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
        (
            "repeated Idempotency-Key",
            vec![
                ("authorization", &auth),
                ("idempotency-key", &key),
                ("idempotency-key", &other),
                ("atlas-device-state", &state),
            ],
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
        (
            "repeated state header",
            vec![
                ("authorization", &auth),
                ("idempotency-key", &key),
                ("atlas-device-state", &state),
                ("atlas-device-state", &state),
            ],
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
        (
            "upper-case ID",
            vec![
                ("authorization", &auth),
                ("idempotency-key", &upper),
                ("atlas-device-state", &state),
            ],
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        (
            "non-UUID ID",
            vec![
                ("authorization", &auth),
                ("idempotency-key", "abc"),
                ("atlas-device-state", &state),
            ],
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        (
            "unknown state prefix",
            vec![
                ("authorization", &auth),
                ("idempotency-key", &key),
                ("atlas-device-state", &wrong_prefix),
            ],
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        (
            "malformed state",
            vec![
                ("authorization", &auth),
                ("idempotency-key", &key),
                ("atlas-device-state", &short_state),
            ],
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        (
            "no credential",
            vec![("idempotency-key", &key), ("atlas-device-state", &state)],
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
        ),
    ];
    for (name, headers, status, code) in matrix {
        let reply = request(&w.app, "DELETE", "devices/phone", &headers, Value::Null).await;
        assert_eq!(reply.2["code"], code, "{name}");
        assert_error(&reply, status, code);
    }
    // The same header rules apply to a keyed revocation.
    let session = session_of(&w.app, &phone, "phone").await.remove(0);
    for (headers, status) in [
        (
            vec![
                ("authorization", auth.as_str()),
                ("idempotency-key", key.as_str()),
                ("idempotency-key", other.as_str()),
            ],
            StatusCode::BAD_REQUEST,
        ),
        (
            vec![
                ("authorization", auth.as_str()),
                ("idempotency-key", upper.as_str()),
            ],
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let reply = request(
            &w.app,
            "DELETE",
            &format!("sessions/{session}"),
            &headers,
            Value::Null,
        )
        .await;
        assert_eq!(reply.0, status);
    }
    assert_eq!(ledger(&w.store).await?, []);
    assert_eq!(
        session_of(&w.app, &phone, "phone").await,
        [session],
        "nothing was revoked"
    );
    Ok(())
}

/// (c), (k), (m), (w) A self-retirement revokes the caller's own session; the outcome is recovered
/// by repeating the identical call under a different session of the account, and a reused ID with
/// another state is refused.
#[tokio::test]
async fn self_retirement_is_recoverable_by_another_session() -> Result<()> {
    let w = world(false).await?;
    let phone = login(&w.app, "device-alice", "phone").await;
    let laptop = login(&w.app, "device-alice", "laptop").await;
    let state = device_state(&w.app, &phone, "phone").await;
    let op = id();

    let first = retire(&w.app, &phone, "phone", &op, &state).await;
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(
        first.2,
        json!({"operation_id":op,"account_id":w.alice,"outcome":"confirmed_applied"})
    );
    assert!(
        !first.1.contains_key("set-cookie"),
        "the response must not touch any cookie"
    );

    // The retired session is dead: a retry under it is `401` and records nothing new.
    let dead = retire(&w.app, &phone, "phone", &op, &state).await;
    assert_error(&dead, StatusCode::UNAUTHORIZED, "unauthenticated");
    assert_eq!(
        ledger(&w.store).await?,
        [(op.clone(), "confirmed_applied".into())]
    );

    // Recovery: the identical call under another session replays the recorded outcome.
    let replay = retire(&w.app, &laptop, "phone", &op, &state).await;
    assert_eq!((replay.0, &replay.2), (StatusCode::OK, &first.2));
    assert!(!replay.1.contains_key("set-cookie"));
    assert_eq!(ledger(&w.store).await?.len(), 1);

    // (w) The ID cannot be reused for another state or another device.
    let laptop_state = device_state(&w.app, &laptop, "laptop").await;
    assert_error(
        &retire(&w.app, &laptop, "phone", &op, &laptop_state).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_value",
    );
    assert_error(
        &retire(&w.app, &laptop, "laptop", &op, &state).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_value",
    );
    assert_eq!(
        session_of(&w.app, &laptop, "laptop").await.len(),
        1,
        "the laptop was not retired"
    );
    assert_eq!(ledger(&w.store).await?, [(op, "confirmed_applied".into())]);
    Ok(())
}

/// (e), (h), (v) A session added after the token was read makes the attempt stale: `409` carrying
/// the recorded outcome, neither session deleted, the rejection replayable, and a new read plus a
/// new ID succeeding.
#[tokio::test]
async fn a_changed_device_is_a_409_carrying_the_outcome() -> Result<()> {
    let w = world(false).await?;
    let first = login(&w.app, "device-alice", "tablet").await;
    let state = device_state(&w.app, &first, "tablet").await;
    let second = login(&w.app, "device-alice", "tablet").await;
    let op = id();

    let rejected = retire(&w.app, &first, "tablet", &op, &state).await;
    assert_eq!(rejected.0, StatusCode::CONFLICT);
    assert_eq!(rejected.2["code"], "operation_conflict");
    assert_eq!(rejected.2["outcome"], "rejected_stale");
    assert_eq!(rejected.2["operation_id"], op);
    assert_eq!(rejected.2["account_id"], w.alice);
    assert_eq!(rejected.2["details"], json!([]));
    assert_eq!(
        rejected.2["request_id"],
        rejected.1["x-request-id"].to_str().unwrap()
    );
    assert!(!rejected.1.contains_key("set-cookie"));
    assert_eq!(
        session_of(&w.app, &second, "tablet").await.len(),
        2,
        "neither session deleted"
    );
    assert_eq!(
        ledger(&w.store).await?,
        [(op.clone(), "rejected_stale".into())]
    );
    // The rejection is the recorded outcome of its own ID, not an ID with no row.
    assert_eq!(
        w.store
            .operation_outcome(&w.alice, &op)
            .await?
            .map(|o| o.as_str()),
        Some("rejected_stale")
    );
    assert_eq!(w.store.operation_outcome(&w.alice, &id()).await?, None);
    let replay = retire(&w.app, &second, "tablet", &op, &state).await;
    assert_eq!(
        (replay.0, &replay.2["outcome"]),
        (StatusCode::CONFLICT, &json!("rejected_stale"))
    );

    // A fresh read, a fresh confirmation and a new ID succeed; the old ID still replays.
    let fresh = device_state(&w.app, &second, "tablet").await;
    assert_ne!(fresh, state);
    let again = id();
    let done = retire(&w.app, &second, "tablet", &again, &fresh).await;
    assert_eq!(
        (done.0, &done.2["outcome"]),
        (StatusCode::OK, &json!("confirmed_applied"))
    );
    assert_eq!(
        retire(&w.app, &first, "tablet", &op, &state).await.0,
        StatusCode::UNAUTHORIZED
    );
    let laptop = login(&w.app, "device-alice", "laptop").await;
    assert_eq!(
        retire(&w.app, &laptop, "tablet", &op, &state).await.2["outcome"],
        "rejected_stale"
    );
    Ok(())
}

/// The listing offers what the retirement needs, and only the caller's own account's devices.
#[tokio::test]
async fn the_listing_carries_the_state_token_and_summary() -> Result<()> {
    let w = world(false).await?;
    let phone = login(&w.app, "device-alice", "phone").await;
    login(&w.app, "device-alice", "phone").await;
    let bob = login(&w.app, "device-bob", "phone").await;
    let (_, _, body) = request(
        &w.app,
        "GET",
        "devices",
        &[("authorization", &bearer(&phone))],
        Value::Null,
    )
    .await;
    let devices = body["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 1);
    let phone_device = &devices[0];
    assert_eq!(phone_device["current"], true);
    assert_eq!(phone_device["active_sessions"], 2);
    assert_eq!(
        phone_device["summary"],
        json!({"sessions":2,"native_handoffs":0,"notification_subscriptions":0,"pending_sign_ins":0,"sync_registered":false})
    );
    let token = phone_device["state_token"].as_str().unwrap();
    assert!(token.starts_with("v1.") && token.len() == 46);
    // Bob's same-named device has another token, and Alice cannot retire it.
    assert_ne!(device_state(&w.app, &bob, "phone").await, token);
    let op = id();
    let reply = retire(&w.app, &bob, "phone", &op, token).await;
    assert_eq!(
        reply.2["outcome"], "rejected_stale",
        "not the state Bob's device is in"
    );
    assert_eq!(session_of(&w.app, &phone, "phone").await.len(), 2);
    Ok(())
}

/// R1 over HTTP. A device whose last session has gone but that still holds a live native handoff
/// (created by the real OIDC callback) or an active subscription (set by the real reminder
/// command) stays in `GET /devices`, so the fresh token the stale rejection asks for can be read.
/// The token offered before the logout is refused with the member intact; the fresh one retires
/// it; both outcomes replay; and another account can neither see nor affect the device.
#[tokio::test]
async fn a_device_left_with_only_a_handoff_or_subscription_stays_listed_and_retirable() -> Result<()>
{
    let w = world_with_oidc().await?;
    let laptop = login(&w.app, "device-alice", "laptop").await;
    let bob = login(&w.app, "device-bob", "watch").await;

    // `watch`: a session and a handoff. `tablet`: a session and a subscription.
    let watch = login(&w.app, "device-alice", "watch").await;
    let (code, verifier) = native_handoff(&w, "watch").await;
    let tablet = login(&w.app, "device-alice", "tablet").await;
    let subscription = id();
    w.store
        .reminder_command(
            &w.alice,
            &id(),
            &ReminderCommand::SetSubscription {
                id: subscription.clone(),
                expected_version: 0,
                device_id: "tablet".into(),
                transport: "ntfy".into(),
                secret: "not-used-by-this-test".into(),
                enabled: true,
            },
        )
        .await?;

    for (device, session) in [("watch", &watch), ("tablet", &tablet)] {
        let offered = device_state(&w.app, &laptop, device).await;
        // Bob has no such device: Alice's token is not his state, and nothing of Alice's changes.
        assert!(listed(&w.app, &bob, "tablet").await.is_none());
        let foreign = retire(&w.app, &bob, "tablet", &id(), &offered).await;
        assert_eq!(foreign.2["outcome"], "superseded", "{device}");
        assert!(listed(&w.app, &laptop, device).await.is_some());

        // The device's only session signs out.
        let out = request(
            &w.app,
            "DELETE",
            "sessions/current",
            &[("authorization", &bearer(session))],
            Value::Null,
        )
        .await;
        assert_eq!(out.0, StatusCode::NO_CONTENT);

        let entry = listed(&w.app, &laptop, device)
            .await
            .unwrap_or_else(|| panic!("{device} vanished from the listing after its last logout"));
        assert_eq!(entry["active_sessions"], 0);
        assert_eq!(entry["summary"]["sessions"], 0);
        let held = match device {
            "watch" => ("native_handoffs", "notification_subscriptions"),
            _ => ("notification_subscriptions", "native_handoffs"),
        };
        assert_eq!(entry["summary"][held.0], 1, "{device}");
        assert_eq!(entry["summary"][held.1], 0, "{device}");
        let fresh = entry["state_token"].as_str().unwrap().to_owned();
        assert_ne!(fresh, offered);

        // The token offered before the logout is refused, and the member survives.
        let stale_op = id();
        let stale = retire(&w.app, &laptop, device, &stale_op, &offered).await;
        assert_eq!(
            (stale.0, &stale.2["outcome"]),
            (StatusCode::CONFLICT, &json!("rejected_stale")),
            "{device}"
        );
        assert!(listed(&w.app, &laptop, device).await.is_some());

        // The fresh token retires it, and both operations replay.
        let op = id();
        let done = retire(&w.app, &laptop, device, &op, &fresh).await;
        assert_eq!(
            (done.0, &done.2["outcome"]),
            (StatusCode::OK, &json!("confirmed_applied")),
            "{device}"
        );
        assert!(!done.1.contains_key("set-cookie"));
        assert!(listed(&w.app, &laptop, device).await.is_none());
        let again = retire(&w.app, &laptop, device, &op, &fresh).await;
        assert_eq!(again.2["outcome"], "confirmed_applied");
        let again = retire(&w.app, &laptop, device, &stale_op, &offered).await;
        assert_eq!(
            (again.0, &again.2["outcome"]),
            (StatusCode::CONFLICT, &json!("rejected_stale"))
        );
    }

    // What retirement did to each member.
    let exchange = spawn_native_exchange(&w, &code, &verifier).await?;
    assert_error(&exchange, StatusCode::UNAUTHORIZED, "unauthenticated");
    let (active, secret): (i64, String) =
        sqlx::query_as("SELECT active,secret FROM notification_subscriptions WHERE id=$1")
            .bind(&subscription)
            .fetch_one(&w.store.pool)
            .await?;
    assert_eq!((active, secret.as_str()), (0, ""));
    // Bob's only ledger row is his own, and Alice's rows carry Alice's account.
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT account_id,outcome FROM operation_outcomes WHERE account_id<>$1")
            .bind(&w.alice)
            .fetch_all(&w.store.pool)
            .await?;
    assert_eq!(rows.len(), 2, "one `superseded` foreign attempt per device");
    assert!(rows.iter().all(|(_, outcome)| outcome == "superseded"));
    Ok(())
}

// ------------------------------------------------------------------------ keyed revocation

/// (g), (i), (j), (k), (m) A keyed revocation replays after a lost response, by another session,
/// after the target is gone, and never reports a stale outcome or a cookie.
/// Component 4 over HTTP (BE-Q19). A device with no session, handoff or subscription — only a
/// pending activation grant — still appears in `GET /devices`, and `forget_device` cancels it:
/// the grant no longer activates, but the row survives as evidence for `activate/cancel`.
#[tokio::test]
async fn a_grant_only_device_stays_listed_and_forget_device_cancels_its_grant() -> Result<()> {
    let w = world(true).await?;
    let laptop = login(&w.app, "device-alice", "laptop").await;
    let (status, _, granted) = issue_grant(&w.app, "device-alice", "phone").await;
    assert_eq!(status, StatusCode::OK, "{granted}");
    let grant = granted["grant"].as_str().unwrap().to_owned();
    let verifier = granted["verifier"].as_str().unwrap().to_owned();

    let entry = listed(&w.app, &laptop, "phone")
        .await
        .expect("a grant-only device is listed");
    assert_eq!(entry["active_sessions"], 0);
    assert_eq!(
        entry["summary"],
        json!({"sessions":0,"native_handoffs":0,"notification_subscriptions":0,"pending_sign_ins":1,"sync_registered":false})
    );
    let token = entry["state_token"].as_str().unwrap().to_owned();

    let op = id();
    let done = retire(&w.app, &laptop, "phone", &op, &token).await;
    assert_eq!(
        (done.0, &done.2["outcome"]),
        (StatusCode::OK, &json!("confirmed_applied"))
    );
    assert!(!done.1.contains_key("set-cookie"));
    assert!(listed(&w.app, &laptop, "phone").await.is_none());

    let (status, _, activated) = activate(&w.app, &grant, &verifier).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{activated}");
    let (state, cancelled_at): (String, Option<i64>) = sqlx::query_as(
        "SELECT state,cancelled_at FROM activation_grants WHERE account_id=$1 AND device_id='phone'",
    )
    .bind(&w.alice)
    .fetch_one(&w.store.pool)
    .await?;
    assert_eq!(state, "cancelled");
    assert!(cancelled_at.is_some());
    Ok(())
}

#[tokio::test]
async fn keyed_revocation_is_recoverable_and_never_stale() -> Result<()> {
    let w = world(false).await?;
    let phone = login(&w.app, "device-alice", "phone").await;
    let laptop = login(&w.app, "device-alice", "laptop").await;
    let tablet = login(&w.app, "device-alice", "tablet").await;
    let laptop_session = session_of(&w.app, &phone, "laptop").await.remove(0);
    let phone_session = session_of(&w.app, &phone, "phone").await.remove(0);
    let op = id();
    let expected = json!({"operation_id":op,"account_id":w.alice,"outcome":"confirmed_applied"});

    let first = revoke(&w.app, &phone, &laptop_session, Some(&op)).await;
    assert_eq!((first.0, &first.2), (StatusCode::OK, &expected));
    assert!(!first.1.contains_key("set-cookie"));
    // Lost response: retry, by the same session and by another, after the target is gone.
    for token in [&phone, &tablet] {
        let again = revoke(&w.app, token, &laptop_session, Some(&op)).await;
        assert_eq!((again.0, &again.2), (StatusCode::OK, &expected));
    }
    assert_eq!(
        revoke(&w.app, &laptop, &laptop_session, None).await.0,
        StatusCode::UNAUTHORIZED
    );

    // (j) Two calls against an already-deleted session: the first records `superseded`, the
    // second replays it. Asserting the reported outcome, not the status.
    let already = id();
    let superseded = revoke(&w.app, &phone, &laptop_session, Some(&already)).await;
    assert_eq!(
        (superseded.0, &superseded.2["outcome"]),
        (StatusCode::OK, &json!("superseded"))
    );
    assert_eq!(
        revoke(&w.app, &tablet, &laptop_session, Some(&already))
            .await
            .2["outcome"],
        "superseded"
    );
    // The ID is bound to its target.
    assert_error(
        &revoke(&w.app, &phone, &phone_session, Some(&already)).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_value",
    );

    // Self-revocation: the caller's own session; the outcome is read back under another.
    let own = id();
    let gone = revoke(&w.app, &phone, &phone_session, Some(&own)).await;
    assert_eq!(
        (gone.0, &gone.2["outcome"]),
        (StatusCode::OK, &json!("confirmed_applied"))
    );
    assert!(!gone.1.contains_key("set-cookie"));
    // (m) The revoked credential is a `401` that records nothing; the recovery is under another.
    assert_error(
        &revoke(&w.app, &phone, &phone_session, Some(&own)).await,
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
    );
    assert_eq!(
        revoke(&w.app, &tablet, &phone_session, Some(&own)).await.2["outcome"],
        "confirmed_applied"
    );

    let rows = ledger(&w.store).await?;
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|(_, outcome)| outcome != "rejected_stale"));

    // Without a key the route is the plain revocation it always was.
    let extra = login(&w.app, "device-alice", "extra").await;
    let extra_session = session_of(&w.app, &tablet, "extra").await.remove(0);
    let plain = revoke(&w.app, &tablet, &extra_session, None).await;
    assert_eq!((plain.0, plain.2), (StatusCode::NO_CONTENT, Value::Null));
    assert!(!plain.1.contains_key("set-cookie"));
    assert_eq!(
        revoke(&w.app, &tablet, &extra_session, None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        revoke(&w.app, &extra, &extra_session, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_error(
        &revoke(&w.app, &tablet, "not-a-uuid", None).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_value",
    );
    assert_eq!(
        ledger(&w.store).await?.len(),
        3,
        "the plain route records nothing"
    );
    Ok(())
}

// --------------------------------------------------------------------------------- cookies

/// (n) None of the five self-revoking responses emits a `Set-Cookie`; each session row is gone and
/// the dead cookie then gets `401`. (`DELETE /oidc/identities` is asserted in the OIDC scenario.)
/// (f) A retirement, then a new login on the same device: nothing the retirement's response says
/// can touch the new cookie, and the new session works.
#[tokio::test]
async fn self_revoking_responses_never_set_a_cookie() -> Result<()> {
    let w = world(true).await?;
    let as_cookie = |cookie: &str, csrf: &str| {
        vec![
            ("cookie", cookie.to_owned()),
            ("x-csrf-token", csrf.to_owned()),
        ]
    };
    let call =
        |method: &'static str, path: String, headers: Vec<(&'static str, String)>, body: Value| {
            let app = w.app.clone();
            async move {
                let refs: Vec<(&str, &str)> =
                    headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
                request(&app, method, &path, &refs, body).await
            }
        };
    let alive = |cookie: &str, csrf: &str| {
        let headers = as_cookie(cookie, csrf);
        call("GET", "sync".into(), headers, Value::Null)
    };

    // 1. DELETE /sessions/current
    let (cookie, csrf) = cookie_login(&w.app, "browser").await;
    assert_eq!(alive(&cookie, &csrf).await.0, StatusCode::OK);
    let reply = call(
        "DELETE",
        "sessions/current".into(),
        as_cookie(&cookie, &csrf),
        Value::Null,
    )
    .await;
    assert_eq!(reply.0, StatusCode::NO_CONTENT);
    assert!(!reply.1.contains_key("set-cookie"));
    assert_eq!(alive(&cookie, &csrf).await.0, StatusCode::UNAUTHORIZED);

    // 2. DELETE /sessions/{id}, of the caller's own session, plain and keyed
    for keyed in [false, true] {
        let (cookie, csrf) = cookie_login(&w.app, "browser").await;
        let (_, _, list) = call(
            "GET",
            "sessions".into(),
            as_cookie(&cookie, &csrf),
            Value::Null,
        )
        .await;
        let own = list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["current"] == true)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut headers = as_cookie(&cookie, &csrf);
        if keyed {
            headers.push(("idempotency-key", id()));
        }
        let reply = call("DELETE", format!("sessions/{own}"), headers, Value::Null).await;
        assert_eq!(
            reply.0,
            if keyed {
                StatusCode::OK
            } else {
                StatusCode::NO_CONTENT
            }
        );
        assert!(!reply.1.contains_key("set-cookie"));
        assert_eq!(alive(&cookie, &csrf).await.0, StatusCode::UNAUTHORIZED);
    }

    // 3. DELETE /devices/{id}, retiring the caller's own device, then (f) a new login on it
    let (cookie, csrf) = cookie_login(&w.app, "shared").await;
    let (_, _, list) = call(
        "GET",
        "devices".into(),
        as_cookie(&cookie, &csrf),
        Value::Null,
    )
    .await;
    let state = list["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == "shared")
        .unwrap()["state_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut headers = as_cookie(&cookie, &csrf);
    headers.push(("idempotency-key", id()));
    headers.push(("atlas-device-state", state));
    let reply = call("DELETE", "devices/shared".into(), headers, Value::Null).await;
    assert_eq!(
        (reply.0, &reply.2["outcome"]),
        (StatusCode::OK, &json!("confirmed_applied"))
    );
    assert!(!reply.1.contains_key("set-cookie"));
    assert_eq!(alive(&cookie, &csrf).await.0, StatusCode::UNAUTHORIZED);
    let (newer_cookie, newer_csrf) = cookie_login(&w.app, "shared").await;
    assert_ne!(newer_cookie, cookie);
    // The retirement's response, however late, carried nothing that could clear this cookie.
    assert!(!reply.1.contains_key("set-cookie"));
    assert_eq!(alive(&newer_cookie, &newer_csrf).await.0, StatusCode::OK);

    // 4. POST /password (last: it revokes every session of the account)
    let (cookie, csrf) = cookie_login(&w.app, "browser").await;
    let reply = call(
        "POST",
        "password".into(),
        as_cookie(&cookie, &csrf),
        json!({"current_password":PASSWORD,"new_password":"changed-password-456"}),
    )
    .await;
    assert_eq!(reply.0, StatusCode::NO_CONTENT);
    assert!(!reply.1.contains_key("set-cookie"));
    assert_eq!(alive(&cookie, &csrf).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        alive(&newer_cookie, &newer_csrf).await.0,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

// --------------------------------------------------------------------- route-by-route timing

/// (y), (z) Writer first: the revoke or logout completes before the retirement takes its locks,
/// answers `204`, and the recompute is empty: `superseded`.
async fn writer_first(route: Route, raise: bool) -> Result<()> {
    let s = scene(raise).await?;
    let op = id();
    let (gate, retirement) = s.hold_retirement(writer_first_point(), &op).await;
    let done = s.spawn_writer(route, &s.phone).await?;
    assert_eq!(done.0, StatusCode::NO_CONTENT, "{route:?}");
    assert!(!done.1.contains_key("set-cookie"));
    gate.release();
    let answer = retirement.await?;
    assert_eq!(
        (answer.0, &answer.2["outcome"]),
        (StatusCode::OK, &json!("superseded"))
    );
    assert_eq!(ledger(&s.w.store).await?, [(op, "superseded".into())]);
    assert_isolation(&s.w.store, raise);
    Ok(())
}

/// (y), (z) Retirement first, the request having authenticated before the commit: the ordinary
/// revoke finds its row gone (`404`); `DELETE /sessions/current` deletes nothing and still answers
/// `204`. Neither is credited anything: the retirement's ledger row says `confirmed_applied`.
async fn retirement_first_authenticated_before(route: Route, raise: bool) -> Result<()> {
    let s = scene(raise).await?;
    let op = id();
    // The writer authenticates, then is held between `identity()` and its `DELETE`.
    let mut after_identity = s.w.store.hooks().arm("session_delete.after_identity");
    let mut writer = s.spawn_writer(route, &s.phone);
    after_identity.reached().await;
    let (gate, retirement) = s.hold_retirement(RETIREMENT_FIRST, &op).await;
    after_identity.release();
    assert_blocked(&s.w.store, RETIREMENT_FIRST, &mut writer).await?;
    gate.release();
    let answer = retirement.await?;
    assert_eq!(
        (answer.0, &answer.2["outcome"]),
        (StatusCode::OK, &json!("confirmed_applied"))
    );
    let done = writer.await?;
    let expected = match route {
        Route::Ordinary => StatusCode::NOT_FOUND,
        Route::Current => StatusCode::NO_CONTENT,
    };
    assert_eq!(done.0, expected, "{route:?}: {}", done.2);
    assert!(!done.1.contains_key("set-cookie"));
    assert_eq!(
        ledger(&s.w.store).await?,
        [(op, "confirmed_applied".into())]
    );
    assert_isolation(&s.w.store, raise);
    Ok(())
}

/// (y), (z) Retirement first, the request begun after the commit: authentication fails first.
async fn retirement_first_begun_after(route: Route, raise: bool) -> Result<()> {
    let s = scene(raise).await?;
    let done = retire(&s.w.app, &s.laptop, "phone", &id(), &s.state).await;
    assert_eq!(done.2["outcome"], "confirmed_applied");
    let late = s.spawn_writer(route, &s.phone).await?;
    assert_error(&late, StatusCode::UNAUTHORIZED, "unauthenticated");
    if matches!(route, Route::Ordinary) {
        // Revoking the retired session's ID, authenticated by a session of a surviving device.
        let reply = revoke(&s.w.app, &s.laptop, &s.phone_session, None).await;
        assert_error(&reply, StatusCode::NOT_FOUND, "not_found");
    }
    assert_isolation(&s.w.store, raise);
    Ok(())
}

#[tokio::test]
async fn ordinary_revoke_first_answers_204_and_the_retirement_is_superseded() -> Result<()> {
    writer_first(Route::Ordinary, false).await
}

#[tokio::test]
async fn logout_first_answers_204_and_the_retirement_is_superseded() -> Result<()> {
    writer_first(Route::Current, false).await
}

#[tokio::test]
async fn ordinary_revoke_after_a_winning_retirement_is_404() -> Result<()> {
    retirement_first_authenticated_before(Route::Ordinary, false).await
}

#[tokio::test]
async fn logout_after_a_winning_retirement_is_204_and_credited_nothing() -> Result<()> {
    retirement_first_authenticated_before(Route::Current, false).await
}

#[tokio::test]
async fn requests_begun_after_the_retirement_are_401_or_404() -> Result<()> {
    retirement_first_begun_after(Route::Ordinary, false).await?;
    retirement_first_begun_after(Route::Current, false).await
}

/// (z) `D` has `S1` and `S2` and `S1` is the caller: logout first is `204`, the retirement is
/// `rejected_stale`, and `S2` is untouched.
async fn logout_of_one_of_two_sessions(raise: bool) -> Result<()> {
    let w = world(false).await?;
    if raise {
        w.raise_isolation();
    }
    let first = login(&w.app, "device-alice", "phone").await;
    let second = login(&w.app, "device-alice", "phone").await;
    let laptop = login(&w.app, "device-alice", "laptop").await;
    let state = device_state(&w.app, &laptop, "phone").await;
    let op = id();
    let mut gate = w.store.hooks().arm(writer_first_point());
    let retirement = spawn_request(
        &w.app,
        "DELETE",
        "devices/phone".into(),
        retirement_headers(&laptop, &op, &state),
    );
    gate.reached().await;
    let done = request(
        &w.app,
        "DELETE",
        "sessions/current",
        &[("authorization", &bearer(&first))],
        Value::Null,
    )
    .await;
    assert_eq!(done.0, StatusCode::NO_CONTENT);
    gate.release();
    let answer = retirement.await?;
    assert_eq!(
        (answer.0, &answer.2["outcome"]),
        (StatusCode::CONFLICT, &json!("rejected_stale"))
    );
    assert_eq!(
        session_of(&w.app, &second, "phone").await.len(),
        1,
        "S2 intact"
    );
    assert_eq!(ledger(&w.store).await?, [(op, "rejected_stale".into())]);
    assert_isolation(&w.store, raise);
    Ok(())
}

#[tokio::test]
async fn logout_of_one_of_two_sessions_makes_the_retirement_stale() -> Result<()> {
    logout_of_one_of_two_sessions(false).await
}

/// A keyed revocation runs inside `begin_serial()`: begun while a retirement holds its locks it
/// waits, and after the retirement commits it finds nothing to remove. It is never credited with
/// the retirement's removal.
async fn keyed_revoke_after_a_winning_retirement(raise: bool) -> Result<()> {
    let s = scene(raise).await?;
    let (retire_op, revoke_op) = (id(), id());
    let (gate, retirement) = s.hold_retirement(RETIREMENT_FIRST, &retire_op).await;
    let mut keyed = spawn_request(
        &s.w.app,
        "DELETE",
        format!("sessions/{}", s.phone_session),
        vec![
            ("authorization".into(), bearer(&s.phone)),
            ("idempotency-key".into(), revoke_op.clone()),
        ],
    );
    assert_blocked(&s.w.store, RETIREMENT_FIRST, &mut keyed).await?;
    gate.release();
    assert_eq!(retirement.await?.2["outcome"], "confirmed_applied");
    let answer = keyed.await?;
    assert_eq!(
        (answer.0, &answer.2["outcome"]),
        (StatusCode::OK, &json!("superseded"))
    );
    let mut rows = ledger(&s.w.store).await?;
    rows.sort();
    let mut expected = vec![
        (retire_op, "confirmed_applied".to_owned()),
        (revoke_op, "superseded".to_owned()),
    ];
    expected.sort();
    assert_eq!(rows, expected);
    assert_isolation(&s.w.store, raise);
    Ok(())
}

#[tokio::test]
async fn a_keyed_revoke_after_a_winning_retirement_is_superseded() -> Result<()> {
    keyed_revoke_after_a_winning_retirement(false).await
}

/// (ai) Every route schedule above, and the keyed one, again with the retirement raised to
/// REPEATABLE READ (PostgreSQL only; SQLite has no isolation level to raise). The difference is
/// retries, not outcomes: each answer, ledger row and remaining row is asserted as before.
#[tokio::test]
async fn every_route_schedule_also_passes_at_repeatable_read() -> Result<()> {
    if !postgres() {
        return Ok(());
    }
    for route in [Route::Ordinary, Route::Current] {
        writer_first(route, true).await?;
        retirement_first_authenticated_before(route, true).await?;
        retirement_first_begun_after(route, true).await?;
    }
    logout_of_one_of_two_sessions(true).await?;
    keyed_revoke_after_a_winning_retirement(true).await?;
    Ok(())
}
