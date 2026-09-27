//! HTTP contract of BE-Q19's activation-grant lifecycle: issue (`POST /browser-sessions`, whose
//! twins `browser-registration` and the OIDC browser callback share the same `issue_grant`),
//! redeem (`POST /browser-sessions/activate`) and abandon (`.../activate/cancel`). Checks are
//! lettered to match `.agtx/plan.md` §6.2's `crates/server/tests/cases/activation.rs` bullet;
//! letters it also covers elsewhere are not duplicated here: (k) is
//! `cases::devices::self_revoking_responses_never_set_a_cookie`, and the `forget_device` half of
//! (f)/(r) is `cases::devices::a_grant_only_device_stays_listed_and_forget_device_cancels_its_grant`.
use anyhow::Result;
use atlas_server::now;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support::http::request;
use crate::support::retirement::{
    ORIGIN, PASSWORD, activate, activate_cancel, attempt, bearer, cookie_login, id, issue_grant,
    login, world, world_with_oidc,
};

/// (a) No response other than `POST /browser-sessions/activate` carries `Set-Cookie`: the login
/// route and `activate/cancel` carry none.
#[tokio::test]
async fn a_only_activate_sets_a_cookie_among_the_grant_routes() -> Result<()> {
    let w = world(true).await?;
    let (status, headers, granted) = issue_grant(&w.app, "device-alice", "browser").await;
    assert_eq!(status, StatusCode::OK, "{granted}");
    assert!(
        !headers.contains_key("set-cookie"),
        "login issues no cookie"
    );
    let (status, headers, activated) = activate(
        &w.app,
        granted["grant"].as_str().unwrap(),
        granted["verifier"].as_str().unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{activated}");
    assert!(
        headers.contains_key("set-cookie"),
        "activate is the only cookie-setter"
    );

    let (status, _, granted) = issue_grant(&w.app, "device-alice", "browser2").await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, cancelled) =
        activate_cancel(&w.app, granted["verifier"].as_str().unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert!(!headers.contains_key("set-cookie"), "cancel sets no cookie");
    Ok(())
}

/// (b) Two concurrent `activate` calls against the same grant: exactly one session results, the
/// other gets `401`. `begin_serial()`'s existing global ordering point is what makes this hold;
/// no new locking primitive is introduced for it.
#[tokio::test]
async fn b_two_concurrent_activations_of_one_grant_produce_exactly_one_session() -> Result<()> {
    let w = world(true).await?;
    let (status, _, granted) = issue_grant(&w.app, "device-alice", "race").await;
    assert_eq!(status, StatusCode::OK);
    let grant = granted["grant"].as_str().unwrap().to_owned();
    let verifier = granted["verifier"].as_str().unwrap().to_owned();
    let (app1, app2) = (w.app.clone(), w.app.clone());
    let (g1, v1) = (grant.clone(), verifier.clone());
    let (a, b) = tokio::join!(
        tokio::spawn(async move { activate(&app1, &g1, &v1).await }),
        tokio::spawn(async move { activate(&app2, &grant, &verifier).await }),
    );
    let (a, b) = (a?, b?);
    let statuses = [a.0, b.0];
    assert_eq!(
        statuses.iter().filter(|s| **s == StatusCode::OK).count(),
        1,
        "{:?} {:?}",
        a.2,
        b.2
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::UNAUTHORIZED)
            .count(),
        1
    );
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE device_id='race'")
        .fetch_one(&w.store.pool)
        .await?;
    assert_eq!(sessions, 1);
    Ok(())
}

/// (c) A wrong verifier is `401` without redeeming; the right verifier still works after a few
/// failures; the fifth failure cancels the grant, after which even the right verifier is `401`.
#[tokio::test]
async fn c_five_wrong_verifiers_cancel_the_grant() -> Result<()> {
    let w = world(true).await?;
    let (_, wrong) = attempt();

    let (status, _, granted) = issue_grant(&w.app, "device-alice", "phone-a").await;
    assert_eq!(status, StatusCode::OK);
    let grant_a = granted["grant"].as_str().unwrap().to_owned();
    let verifier_a = granted["verifier"].as_str().unwrap().to_owned();
    for attempt_number in 0..4 {
        let (status, _, body) = activate(&w.app, &grant_a, &wrong).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "attempt {attempt_number}: {body}"
        );
    }
    let (status, _, activated) = activate(&w.app, &grant_a, &verifier_a).await;
    assert_eq!(status, StatusCode::OK, "{activated}");

    let (status, _, granted) = issue_grant(&w.app, "device-alice", "phone-b").await;
    assert_eq!(status, StatusCode::OK);
    let grant_b = granted["grant"].as_str().unwrap().to_owned();
    let verifier_b = granted["verifier"].as_str().unwrap().to_owned();
    let account = granted["account_id"].as_str().unwrap().to_owned();
    for attempt_number in 0..5 {
        let (status, _, body) = activate(&w.app, &grant_b, &wrong).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "attempt {attempt_number}: {body}"
        );
    }
    let (state, failed): (String, i64) = sqlx::query_as(
        "SELECT state,failed_verifiers FROM activation_grants WHERE account_id=$1 AND device_id='phone-b'",
    )
    .bind(&account)
    .fetch_one(&w.store.pool)
    .await?;
    assert_eq!((state.as_str(), failed), ("cancelled", 5));
    let (status, _, activated) = activate(&w.app, &grant_b, &verifier_b).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{activated}");
    Ok(())
}

/// (d) Activation past `expires_at`: `401`; an expired unredeemed row reports `not_activated`
/// from `activate/cancel`; the 48h sweep leaves rows alone before that and removes them after;
/// the 1000-row non-terminal cap refuses new issuance with `429`.
#[tokio::test]
async fn d_expiry_sweep_and_issuance_cap() -> Result<()> {
    let w = world(true).await?;
    let (status, _, granted) = issue_grant(&w.app, "device-alice", "phone").await;
    assert_eq!(status, StatusCode::OK);
    let grant = granted["grant"].as_str().unwrap().to_owned();
    let verifier = granted["verifier"].as_str().unwrap().to_owned();
    let account = granted["account_id"].as_str().unwrap().to_owned();
    sqlx::query(
        "UPDATE activation_grants SET expires_at=0 WHERE account_id=$1 AND device_id='phone'",
    )
    .bind(&account)
    .execute(&w.store.pool)
    .await?;
    let (status, _, activated) = activate(&w.app, &grant, &verifier).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{activated}");
    let (status, _, cancelled) = activate_cancel(&w.app, &verifier).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["result"], "not_activated");

    // The row is `cancelled` now (durably terminal even though it was already expired): the
    // sweep keys off `COALESCE(redeemed_at,cancelled_at,expires_at)`, so backdating it for this
    // test must move `cancelled_at`, not `expires_at`, which no longer governs retention here.
    // The sweep leaves rows alone before 48h past that, and removes them after.
    sqlx::query(
        "UPDATE activation_grants SET created_at=$1,cancelled_at=$1 WHERE account_id=$2 AND device_id='phone'",
    )
    .bind(now() - 48 * 3600 + 60)
    .bind(&account)
    .execute(&w.store.pool)
    .await?;
    issue_grant(&w.app, "device-alice", "sweep-trigger-1").await;
    let still: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM activation_grants WHERE account_id=$1 AND device_id='phone'",
    )
    .bind(&account)
    .fetch_one(&w.store.pool)
    .await?;
    assert_eq!(still, 1, "not yet past the 48h retention window");
    sqlx::query(
        "UPDATE activation_grants SET created_at=$1,cancelled_at=$1 WHERE account_id=$2 AND device_id='phone'",
    )
    .bind(now() - 48 * 3600 - 60)
    .bind(&account)
    .execute(&w.store.pool)
    .await?;
    issue_grant(&w.app, "device-alice", "sweep-trigger-2").await;
    let swept: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM activation_grants WHERE account_id=$1 AND device_id='phone'",
    )
    .bind(&account)
    .fetch_one(&w.store.pool)
    .await?;
    assert_eq!(swept, 0, "past the 48h retention window");

    for index in 0..1000 {
        sqlx::query("INSERT INTO activation_grants(grant_hash,grant_id,account_id,device_id,auth_kind,challenge_hash,state,failed_verifiers,created_at,expires_at) VALUES ($1,$2,$3,$4,'local',$5,'issued',0,$6,$7)")
            .bind(format!("cap-{index}"))
            .bind(format!("cap-id-{index}"))
            .bind(&account)
            .bind(format!("cap-device-{index}"))
            .bind(format!("cap-challenge-{index}"))
            .bind(now())
            .bind(now() + 60)
            .execute(&w.store.pool)
            .await?;
    }
    let (status, _, capped) = issue_grant(&w.app, "device-alice", "capped").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{capped}");
    assert_eq!(capped["code"], "rate_limited");
    Ok(())
}

/// (d) revision fix: the 1000-row cap counts still-activatable rows only. A thousand `issued`
/// rows that have already expired (retained inside the 48h window as `activate/cancel` evidence)
/// must not themselves refuse a fresh, unrelated issuance.
#[tokio::test]
async fn d_expired_issued_rows_do_not_count_against_the_issuance_cap() -> Result<()> {
    let w = world(true).await?;
    for index in 0..1000 {
        sqlx::query("INSERT INTO activation_grants(grant_hash,grant_id,account_id,device_id,auth_kind,challenge_hash,state,failed_verifiers,created_at,expires_at) VALUES ($1,$2,$3,$4,'local',$5,'issued',0,$6,$7)")
            .bind(format!("expired-cap-{index}"))
            .bind(format!("expired-cap-id-{index}"))
            .bind(&w.alice)
            .bind(format!("expired-cap-device-{index}"))
            .bind(format!("expired-cap-challenge-{index}"))
            .bind(now() - 120)
            .bind(now() - 60)
            .execute(&w.store.pool)
            .await?;
    }
    let (status, _, granted) = issue_grant(&w.app, "device-alice", "not-capped").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "expired issued rows must not consume the cap: {granted}"
    );
    Ok(())
}

/// (d)/(n) revision fix: `activate` must judge expiry by the time it actually acquires
/// `begin_serial()`'s serialisation, not by a value read before a connection-pool or lock wait.
/// A grant that is still valid when the held call starts, but expires in real wall-clock time
/// while it is paused (mirroring a real pool or lock wait, not a DB-only edit), must still fail
/// once released — reading `now` before the wait would find the grant valid throughout and let
/// it redeem regardless of how long the wait turned out to be.
#[tokio::test]
async fn d_activate_evaluates_expiry_after_its_own_wait_not_before_it() -> Result<()> {
    let w = world(true).await?;
    let (status, _, granted) = issue_grant(&w.app, "device-alice", "held-expiry").await;
    assert_eq!(status, StatusCode::OK);
    let grant = granted["grant"].as_str().unwrap().to_owned();
    let verifier = granted["verifier"].as_str().unwrap().to_owned();
    let account = granted["account_id"].as_str().unwrap().to_owned();
    // Still valid when the hold begins; the sleep below crosses it in real time, exactly as a
    // connection-pool or lock wait would.
    sqlx::query(
        "UPDATE activation_grants SET expires_at=$1 WHERE account_id=$2 AND device_id='held-expiry'",
    )
    .bind(now() + 1)
    .bind(&account)
    .execute(&w.store.pool)
    .await?;

    let mut gate = w.store.hooks().arm("activate.before_begin");
    let app = w.app.clone();
    let held = tokio::spawn(async move { activate(&app, &grant, &verifier).await });
    gate.reached().await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    gate.release();
    let (status, _, activated) = tokio::time::timeout(std::time::Duration::from_secs(10), held)
        .await
        .expect("held activate did not complete")?;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "must judge expiry by the time it actually runs, not by a value read before the wait: {activated}"
    );
    Ok(())
}

/// (e) A `DeviceCapacity`-triggered rollback at `activate` leaves the grant `issued` and still
/// redeemable once room exists.
#[tokio::test]
async fn e_device_capacity_rollback_leaves_the_grant_redeemable() -> Result<()> {
    let w = world(true).await?;
    for index in 0..32 {
        sqlx::query("INSERT INTO sessions(token_hash,account_id,device_id,expires_at,session_id,created_at,auth_kind) VALUES ($1,$2,'phone',$3,$4,$5,'local')")
            .bind(format!("filler-{index}"))
            .bind(&w.alice)
            .bind(now() + 86400)
            .bind(format!("filler-session-{index}"))
            .bind(now())
            .execute(&w.store.pool)
            .await?;
    }
    let (status, _, granted) = issue_grant(&w.app, "device-alice", "phone").await;
    assert_eq!(status, StatusCode::OK);
    let grant = granted["grant"].as_str().unwrap().to_owned();
    let verifier = granted["verifier"].as_str().unwrap().to_owned();
    let (status, _, refused) = activate(&w.app, &grant, &verifier).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{refused}");
    assert_eq!(refused["code"], "device_capacity");
    let state: String = sqlx::query_scalar(
        "SELECT state FROM activation_grants WHERE account_id=$1 AND device_id='phone'",
    )
    .bind(&w.alice)
    .fetch_one(&w.store.pool)
    .await?;
    assert_eq!(state, "issued");
    sqlx::query("DELETE FROM sessions WHERE token_hash='filler-0'")
        .execute(&w.store.pool)
        .await?;
    let (status, _, activated) = activate(&w.app, &grant, &verifier).await;
    assert_eq!(status, StatusCode::OK, "{activated}");
    Ok(())
}

/// (f) After `POST /password`, a previously issued grant no longer activates (`401`) and
/// `activate/cancel` reports `not_activated`.
#[tokio::test]
async fn f_password_change_leaves_a_grant_unredeemable() -> Result<()> {
    let w = world(true).await?;
    let (status, _, granted) = issue_grant(&w.app, "device-alice", "phone").await;
    assert_eq!(status, StatusCode::OK);
    let grant = granted["grant"].as_str().unwrap().to_owned();
    let verifier = granted["verifier"].as_str().unwrap().to_owned();
    let authenticator = login(&w.app, "device-alice", "authenticator").await;
    let (status, _, changed) = request(
        &w.app,
        "POST",
        "password",
        &[("authorization", &bearer(&authenticator))],
        json!({"current_password":PASSWORD,"new_password":"changed-activation-456"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{changed}");
    let (status, _, activated) = activate(&w.app, &grant, &verifier).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{activated}");
    let (status, _, cancelled) = activate_cancel(&w.app, &verifier).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["result"], "not_activated");
    Ok(())
}

/// (f) The operator's `reset_password` has the same effect on an outstanding grant.
#[tokio::test]
async fn f_operator_reset_password_leaves_a_grant_unredeemable() -> Result<()> {
    let w = world(true).await?;
    let (status, _, granted) = issue_grant(&w.app, "device-alice", "phone").await;
    assert_eq!(status, StatusCode::OK);
    let grant = granted["grant"].as_str().unwrap().to_owned();
    let verifier = granted["verifier"].as_str().unwrap().to_owned();
    let hash = atlas_server::hash_password("reset-activation-password-123".into()).await?;
    atlas_server::reset_password(&w.store, "device-alice", &hash).await?;
    let (status, _, activated) = activate(&w.app, &grant, &verifier).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{activated}");
    let (status, _, cancelled) = activate_cancel(&w.app, &verifier).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["result"], "not_activated");
    Ok(())
}

/// (f) `DELETE /oidc/identities` (`unlink_oidc`) has the same effect on an outstanding grant.
#[tokio::test]
async fn f_unlink_oidc_leaves_a_grant_unredeemable() -> Result<()> {
    let w = world_with_oidc().await?;
    let (status, _, granted) = issue_grant(&w.app, "device-alice", "phone").await;
    assert_eq!(status, StatusCode::OK);
    let grant = granted["grant"].as_str().unwrap().to_owned();
    let verifier = granted["verifier"].as_str().unwrap().to_owned();
    let recent = login(&w.app, "device-alice", "unlink-caller").await;
    let issuer = w.provider.as_ref().unwrap().issuer.clone();
    let (status, _, unlinked) = request(
        &w.app,
        "DELETE",
        "oidc/identities",
        &[("authorization", &bearer(&recent))],
        json!({"issuer":issuer}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{unlinked}");
    let (status, _, activated) = activate(&w.app, &grant, &verifier).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{activated}");
    let (status, _, cancelled) = activate_cancel(&w.app, &verifier).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["result"], "not_activated");
    Ok(())
}

/// (i) Session A's cookie with session B's own valid CSRF token: `403 credential_mismatch`, and
/// the write attempted with it changes nothing. A wrong-length token: `403 forbidden`. No
/// cookie: `401`.
#[tokio::test]
async fn i_credential_mismatch_is_distinct_from_forbidden_and_unauthenticated() -> Result<()> {
    let w = world(true).await?;
    let (cookie_a, _) = cookie_login(&w.app, "session-a").await;
    let (_cookie_b, csrf_b) = cookie_login(&w.app, "session-b").await;
    let write =
        json!({"commands":[{"kind":"create_person","id":id(),"name":"Should not be created"}]});

    let (status, _, mismatch) = request(
        &w.app,
        "POST",
        "commands",
        &[("cookie", &cookie_a), ("x-csrf-token", &csrf_b)],
        write.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{mismatch}");
    assert_eq!(mismatch["code"], "credential_mismatch");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resources WHERE kind='person'")
        .fetch_one(&w.store.pool)
        .await?;
    assert_eq!(count, 0, "the mismatched write never applied");

    let (status, _, wrong_length) = request(
        &w.app,
        "POST",
        "commands",
        &[("cookie", &cookie_a), ("x-csrf-token", "too-short")],
        write.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(wrong_length["code"], "forbidden");

    let (status, _, no_cookie) = request(&w.app, "POST", "commands", &[], write).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(no_cookie["code"], "unauthenticated");
    Ok(())
}

/// (j) `activate`'s `session_id`, `browser-sessions/current`'s and `GET /sessions`' own ID agree.
#[tokio::test]
async fn j_session_id_matches_between_activate_current_and_the_sessions_list() -> Result<()> {
    let w = world(true).await?;
    let (status, _, granted) = issue_grant(&w.app, "device-alice", "phone").await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, activated) = activate(
        &w.app,
        granted["grant"].as_str().unwrap(),
        granted["verifier"].as_str().unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{activated}");
    let session_id = activated["session_id"].as_str().unwrap().to_owned();
    let cookie = headers["set-cookie"]
        .to_str()?
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let csrf = activated["csrf_token"].as_str().unwrap().to_owned();

    let (status, _, current) = request(
        &w.app,
        "GET",
        "browser-sessions/current",
        &[("cookie", &cookie), ("x-atlas-session", "1")],
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{current}");
    assert_eq!(current["session_id"], session_id);

    let (status, _, list) = request(
        &w.app,
        "GET",
        "sessions",
        &[("cookie", &cookie), ("x-csrf-token", &csrf)],
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let ids: Vec<String> = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(ids.contains(&session_id), "{ids:?}");
    Ok(())
}

/// (m) `activate/cancel`'s three outcomes, each idempotent under a repeat.
#[tokio::test]
async fn m_cancel_outcomes_and_idempotency() -> Result<()> {
    let w = world(true).await?;

    let (status, _, granted) = issue_grant(&w.app, "device-alice", "unredeemed").await;
    assert_eq!(status, StatusCode::OK);
    let grant = granted["grant"].as_str().unwrap().to_owned();
    let verifier = granted["verifier"].as_str().unwrap().to_owned();
    let (status, _, cancelled) = activate_cancel(&w.app, &verifier).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["result"], "not_activated");
    let (status, _, activated) = activate(&w.app, &grant, &verifier).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{activated}");
    let (status, _, repeat) = activate_cancel(&w.app, &verifier).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(repeat["result"], "not_activated");

    let (status, _, granted) = issue_grant(&w.app, "device-alice", "to-redeem").await;
    assert_eq!(status, StatusCode::OK);
    let grant = granted["grant"].as_str().unwrap().to_owned();
    let verifier = granted["verifier"].as_str().unwrap().to_owned();
    let (status, _, activated) = activate(&w.app, &grant, &verifier).await;
    assert_eq!(status, StatusCode::OK, "{activated}");
    let session_id = activated["session_id"].as_str().unwrap().to_owned();
    let (status, _, cancelled) = activate_cancel(&w.app, &verifier).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["result"], "session_revoked");
    let gone: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE session_id=$1")
        .bind(&session_id)
        .fetch_one(&w.store.pool)
        .await?;
    assert_eq!(gone, 0);
    let (status, _, repeat) = activate_cancel(&w.app, &verifier).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(repeat["result"], "session_revoked");

    let (_, unknown_verifier) = attempt();
    let (status, _, unknown) = activate_cancel(&w.app, &unknown_verifier).await;
    assert_eq!(status, StatusCode::OK, "{unknown}");
    assert_eq!(unknown["result"], "unknown");
    Ok(())
}

/// (n) Concurrent `activate`/`activate/cancel` on one record: exactly one order occurs and the
/// post-state (no live session, no redeemable grant either way) is consistent with whichever
/// won. Many iterations, relying on `begin_serial()`'s ordering and Tokio's own scheduling
/// variance rather than a dedicated hook (see plan §8).
#[tokio::test]
async fn n_concurrent_activate_and_cancel_resolve_to_exactly_one_order() -> Result<()> {
    let w = world(true).await?;
    for round in 0..20 {
        let device = format!("race-n-{round}");
        let (status, _, granted) = issue_grant(&w.app, "device-alice", &device).await;
        assert_eq!(status, StatusCode::OK);
        let grant = granted["grant"].as_str().unwrap().to_owned();
        let verifier = granted["verifier"].as_str().unwrap().to_owned();
        let account = granted["account_id"].as_str().unwrap().to_owned();
        let (app1, app2) = (w.app.clone(), w.app.clone());
        let (g, v1, v2) = (grant.clone(), verifier.clone(), verifier.clone());
        let (a, b) = tokio::join!(
            tokio::spawn(async move { activate(&app1, &g, &v1).await }),
            tokio::spawn(async move { activate_cancel(&app2, &v2).await }),
        );
        let (a, b) = (a?, b?);
        let live_sessions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE device_id=$1")
                .bind(&device)
                .fetch_one(&w.store.pool)
                .await?;
        let state: String = sqlx::query_scalar(
            "SELECT state FROM activation_grants WHERE account_id=$1 AND device_id=$2",
        )
        .bind(&account)
        .bind(&device)
        .fetch_one(&w.store.pool)
        .await?;
        if a.0 == StatusCode::OK {
            // activate won the redemption race; the cancel that followed found the grant
            // already redeemed and revoked the session it had just created.
            assert_eq!(b.2["result"], "session_revoked", "{:?} {:?}", a.2, b.2);
            assert_eq!(state, "redeemed");
        } else {
            // cancel won first, cancelling the grant before activate could redeem it.
            assert_eq!(b.2["result"], "not_activated", "{:?} {:?}", a.2, b.2);
            assert_eq!(state, "cancelled");
        }
        // Either way, no live session survives the pair.
        assert_eq!(live_sessions, 0, "{:?} {:?}", a.2, b.2);
    }
    Ok(())
}

/// (d)/(m)/(n) revision fix: the exact reproduction from the review — an `activate` held
/// mid-flight (queued behind a pool/lock wait) must not be able to win after a concurrent
/// `activate/cancel` on the same, now-expired grant has already reported `not_activated`.
/// Cancellation must be durably terminal for an expired `issued` row, not a no-op that leaves it
/// `issued` for a still-in-flight `activate` to redeem regardless.
#[tokio::test]
async fn n_activate_cannot_win_after_a_concurrent_cancel_on_an_expired_grant() -> Result<()> {
    let w = world(true).await?;
    let (status, _, granted) = issue_grant(&w.app, "device-alice", "race-n-expiry").await;
    assert_eq!(status, StatusCode::OK);
    let grant = granted["grant"].as_str().unwrap().to_owned();
    let verifier = granted["verifier"].as_str().unwrap().to_owned();
    let account = granted["account_id"].as_str().unwrap().to_owned();

    let mut gate = w.store.hooks().arm("activate.before_begin");
    let app = w.app.clone();
    let (g, v) = (grant.clone(), verifier.clone());
    let held = tokio::spawn(async move { activate(&app, &g, &v).await });
    gate.reached().await;
    sqlx::query(
        "UPDATE activation_grants SET expires_at=0 WHERE account_id=$1 AND device_id='race-n-expiry'",
    )
    .bind(&account)
    .execute(&w.store.pool)
    .await?;
    let (status, _, cancelled) = activate_cancel(&w.app, &verifier).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["result"], "not_activated");
    gate.release();
    let (status, _, activated) = tokio::time::timeout(std::time::Duration::from_secs(10), held)
        .await
        .expect("held activate did not complete")?;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "activation must not win after cancellation already reported not_activated: {activated}"
    );
    let live_sessions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE device_id='race-n-expiry'")
            .fetch_one(&w.store.pool)
            .await?;
    assert_eq!(live_sessions, 0);
    Ok(())
}

/// (o) `activate/cancel` sets no cookie and reports `unknown` regardless of whether the request
/// carries no cookie, a dead cookie, or a valid cookie of a different account — and never
/// touches that other account's sessions.
#[tokio::test]
async fn o_cancel_ignores_any_cookie_and_touches_no_other_account() -> Result<()> {
    let w = world(true).await?;
    let (status, _, bob_granted) = issue_grant(&w.app, "device-bob", "bob-device").await;
    assert_eq!(status, StatusCode::OK, "{bob_granted}");
    let (status, bob_headers, bob_activated) = activate(
        &w.app,
        bob_granted["grant"].as_str().unwrap(),
        bob_granted["verifier"].as_str().unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{bob_activated}");
    let bob_cookie = bob_headers["set-cookie"]
        .to_str()?
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let bob_sessions_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE device_id='bob-device'")
            .fetch_one(&w.store.pool)
            .await?;

    let (_, verifier) = attempt();
    let dead_cookie = format!("__Host-atlas_session={}", "0".repeat(64));
    for (label, headers) in [
        ("no cookie", vec![("origin", ORIGIN)]),
        (
            "a dead cookie",
            vec![("origin", ORIGIN), ("cookie", dead_cookie.as_str())],
        ),
        (
            "a valid cookie of a different account",
            vec![("origin", ORIGIN), ("cookie", bob_cookie.as_str())],
        ),
    ] {
        let (status, headers_out, body) = request(
            &w.app,
            "POST",
            "browser-sessions/activate/cancel",
            &headers,
            json!({"verifier":verifier}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{label}: {body}");
        assert_eq!(body["result"], "unknown", "{label}");
        assert!(!headers_out.contains_key("set-cookie"), "{label}");
    }
    let bob_sessions_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE device_id='bob-device'")
            .fetch_one(&w.store.pool)
            .await?;
    assert_eq!(bob_sessions_before, bob_sessions_after);
    Ok(())
}

/// (p) A cancel deletes only the session recorded on its own record; another session of the
/// same account and device is untouched.
#[tokio::test]
async fn p_cancel_deletes_only_its_own_recorded_session() -> Result<()> {
    let w = world(true).await?;
    let other = login(&w.app, "device-alice", "phone").await;
    let (status, _, granted) = issue_grant(&w.app, "device-alice", "phone").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, activated) = activate(
        &w.app,
        granted["grant"].as_str().unwrap(),
        granted["verifier"].as_str().unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{activated}");
    let (status, _, cancelled) =
        activate_cancel(&w.app, granted["verifier"].as_str().unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["result"], "session_revoked");
    assert_eq!(
        request(
            &w.app,
            "GET",
            "sessions",
            &[("authorization", &bearer(&other))],
            Value::Null,
        )
        .await
        .0,
        StatusCode::OK,
        "the pre-existing session on the same device survives"
    );
    Ok(())
}

/// (q) After the retention sweep, `activate/cancel` reports `unknown` byte-identically for a
/// genuinely unknown verifier and a swept one.
#[tokio::test]
async fn q_swept_and_genuinely_unknown_verifiers_report_identically() -> Result<()> {
    let w = world(true).await?;
    let (status, _, granted) = issue_grant(&w.app, "device-alice", "to-sweep").await;
    assert_eq!(status, StatusCode::OK);
    let verifier = granted["verifier"].as_str().unwrap().to_owned();
    sqlx::query(
        "UPDATE activation_grants SET created_at=$1,expires_at=$1 WHERE account_id=$2 AND device_id='to-sweep'",
    )
    .bind(now() - 48 * 3600 - 60)
    .bind(granted["account_id"].as_str().unwrap())
    .execute(&w.store.pool)
    .await?;
    issue_grant(&w.app, "device-alice", "sweep-trigger").await;
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM activation_grants WHERE device_id='to-sweep'")
            .fetch_one(&w.store.pool)
            .await?;
    assert_eq!(remaining, 0);
    let (_, never_existed) = attempt();
    let (status_a, _, swept) = activate_cancel(&w.app, &verifier).await;
    let (status_b, _, unknown) = activate_cancel(&w.app, &never_existed).await;
    assert_eq!((status_a, status_b), (StatusCode::OK, StatusCode::OK));
    assert_eq!(swept, unknown);
    Ok(())
}

/// (s) Every `activate` refusal is an Atlas JSON error body, never a bare `204`/`404`, and sets
/// no cookie — checked across the distinct refusal causes named by this file's other checks
/// (unknown, malformed, expired, wrong verifier, cancelled), not only an unknown grant.
#[tokio::test]
async fn s_every_activate_refusal_is_a_json_error_with_no_cookie() -> Result<()> {
    let w = world(true).await?;

    fn assert_refusal(reply: &(StatusCode, axum::http::HeaderMap, Value), status: StatusCode) {
        assert_eq!(reply.0, status, "{}", reply.2);
        assert!(reply.2["code"].is_string(), "{}", reply.2);
        assert!(reply.2["request_id"].is_string(), "{}", reply.2);
        assert!(!reply.1.contains_key("set-cookie"));
    }

    let (_, verifier) = attempt();
    let unknown_grant = activate(&w.app, &"0".repeat(64), &verifier).await;
    assert_refusal(&unknown_grant, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown_grant.2["code"], "unauthenticated");

    let malformed_grant = activate(&w.app, "not-a-hex-grant", &verifier).await;
    assert_refusal(&malformed_grant, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(malformed_grant.2["code"], "invalid_value");

    let (status, _, granted) = issue_grant(&w.app, "device-alice", "refusal-expired").await;
    assert_eq!(status, StatusCode::OK);
    let expired_grant = granted["grant"].as_str().unwrap().to_owned();
    let expired_verifier = granted["verifier"].as_str().unwrap().to_owned();
    let expired_account = granted["account_id"].as_str().unwrap().to_owned();
    sqlx::query(
        "UPDATE activation_grants SET expires_at=0 WHERE account_id=$1 AND device_id='refusal-expired'",
    )
    .bind(&expired_account)
    .execute(&w.store.pool)
    .await?;
    let expired = activate(&w.app, &expired_grant, &expired_verifier).await;
    assert_refusal(&expired, StatusCode::UNAUTHORIZED);
    assert_eq!(expired.2["code"], "unauthenticated");

    let (status, _, granted) = issue_grant(&w.app, "device-alice", "refusal-wrong-verifier").await;
    assert_eq!(status, StatusCode::OK);
    let (_, wrong) = attempt();
    let wrong_verifier = activate(&w.app, granted["grant"].as_str().unwrap(), &wrong).await;
    assert_refusal(&wrong_verifier, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong_verifier.2["code"], "unauthenticated");

    let (status, _, granted) = issue_grant(&w.app, "device-alice", "refusal-cancelled").await;
    assert_eq!(status, StatusCode::OK);
    let cancelled_grant = granted["grant"].as_str().unwrap().to_owned();
    let cancelled_verifier = granted["verifier"].as_str().unwrap().to_owned();
    let (status, _, _) = activate_cancel(&w.app, &cancelled_verifier).await;
    assert_eq!(status, StatusCode::OK);
    let cancelled = activate(&w.app, &cancelled_grant, &cancelled_verifier).await;
    assert_refusal(&cancelled, StatusCode::UNAUTHORIZED);
    assert_eq!(cancelled.2["code"], "unauthenticated");
    Ok(())
}
