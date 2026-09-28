//! `GET /defaults/snapshot` through the real router: authentication, headers, shape, and the real
//! writers (`POST /management-commands` and household registration) racing a read that has been
//! paused between its two halves, on both engines. The deterministic schedule for every core
//! writer is `atlas-core`'s `households/snapshot`; these cases show the same guarantee holds for
//! the routes a client actually uses.
use anyhow::Result;
use atlas_core::households::ManagementCommand as M;
use atlas_server::{hash_password, now};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::Duration;
use uuid::Uuid;

use crate::support::http::{command_request, request};
use crate::support::retirement::{
    PASSWORD, Reply, World, bearer, cookie_login, id, login, postgres, spawn_request_with, world,
};

const POINT: &str = "sharing_snapshot.between_reads";

struct Scene {
    w: World,
    alice: String,
    bob: String,
    carol: String,
    home: String,
    /// Alice's pending invitation of carol to HOME.
    invitation: String,
    token: String,
}

/// Alice manages HOME; bob is a member of it; carol holds a pending invitation to it.
async fn scene() -> Result<Scene> {
    let w = world(true).await?;
    let hash = hash_password(PASSWORD.into()).await?;
    let carol = id();
    w.store.add_account(&carol, "snapshot-carol", &hash).await?;
    let bob: String = sqlx::query_scalar("SELECT id FROM accounts WHERE username='device-bob'")
        .fetch_one(&w.store.pool)
        .await?;
    let (home, invitation) = (id(), id());
    let alice = w.alice.clone();
    w.store
        .management(
            &alice,
            &id(),
            &[M::CreateHousehold {
                id: home.clone(),
                name: "Home".into(),
            }],
            now(),
        )
        .await?;
    let invite = |recipient: &str, invitation: &str, version| M::InviteToHousehold {
        id: invitation.into(),
        household_id: home.clone(),
        recipient_id: recipient.into(),
        expected_version: version,
    };
    let bobs_invitation = id();
    w.store
        .management(&alice, &id(), &[invite(&bob, &bobs_invitation, 1)], now())
        .await?;
    w.store
        .management(
            &bob,
            &id(),
            &[M::RespondToHouseholdInvitation {
                id: bobs_invitation,
                expected_version: 1,
                accept: true,
            }],
            now(),
        )
        .await?;
    // Two more household versions have been consumed: the invitation and its acceptance.
    w.store
        .management(&alice, &id(), &[invite(&carol, &invitation, 3)], now())
        .await?;
    let token = login(&w.app, "device-alice", "phone").await;
    Ok(Scene {
        w,
        alice,
        bob,
        carol,
        home,
        invitation,
        token,
    })
}

async fn snapshot(scene: &Scene, token: &str) -> Reply {
    request(
        &scene.w.app,
        "GET",
        "defaults/snapshot",
        &[("authorization", &bearer(token))],
        Value::Null,
    )
    .await
}

async fn get_with(scene: &Scene, headers: &[(&str, &str)]) -> Reply {
    request(
        &scene.w.app,
        "GET",
        "defaults/snapshot",
        headers,
        Value::Null,
    )
    .await
}

async fn body_of(scene: &Scene, token: &str) -> Value {
    let (status, _, body) = snapshot(scene, token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

fn revision(body: &Value) -> &str {
    body["defaults"]["revision"].as_str().unwrap()
}

fn home_of<'a>(body: &'a Value, home: &str) -> &'a Value {
    body["households"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == home)
        .unwrap_or_else(|| panic!("{home} is not listed in {body}"))
}

fn member_ids(body: &Value, home: &str) -> BTreeSet<String> {
    home_of(body, home)["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["account_id"].as_str().unwrap().to_owned())
        .collect()
}

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().unwrap().keys().cloned().collect()
}

fn contract() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../api/openapi.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn properties(contract: &Value, schema: &str) -> BTreeSet<String> {
    keys(&contract["components"]["schemas"][schema]["properties"])
}

// H1 -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_route_is_authenticated_like_every_other_protected_read() -> Result<()> {
    let s = scene().await?;
    assert_eq!(snapshot(&s, &s.token).await.0, StatusCode::OK);

    let (status, _, body) = request(&s.w.app, "GET", "defaults/snapshot", &[], Value::Null).await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::UNAUTHORIZED, &json!("unauthenticated"))
    );
    let (status, ..) = snapshot(&s, "not-a-session").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A cookie caller must present its CSRF token: absent is `forbidden`, wrong-but-well-formed is
    // `credential_mismatch`, and the right one reads.
    let (cookie, csrf) = cookie_login(&s.w.app, "browser").await;
    let wrong = "0".repeat(64);
    let (status, _, body) = get_with(&s, &[("cookie", &cookie)]).await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::FORBIDDEN, &json!("forbidden"))
    );
    let (status, _, body) = get_with(&s, &[("cookie", &cookie), ("x-csrf-token", &wrong)]).await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::FORBIDDEN, &json!("credential_mismatch"))
    );
    let good = [("cookie", cookie.as_str()), ("x-csrf-token", csrf.as_str())];
    let (status, headers, body) = get_with(&s, &good).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // H2: reading with a cookie never sets one; only activation does.
    assert!(!headers.contains_key("set-cookie"));

    // A revoked session reads nothing.
    let (status, ..) = request(&s.w.app, "DELETE", "sessions/current", &good, Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, body) = get_with(&s, &good).await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::UNAUTHORIZED, &json!("unauthenticated"))
    );
    Ok(())
}

// H2 -------------------------------------------------------------------------------------------

#[tokio::test]
async fn responses_are_private_correlated_and_never_set_a_cookie() -> Result<()> {
    let s = scene().await?;
    for (label, reply) in [
        ("200", snapshot(&s, &s.token).await),
        ("401", snapshot(&s, "not-a-session").await),
    ] {
        let (_, headers, _) = reply;
        assert_eq!(headers["cache-control"], "private, no-store", "{label}");
        Uuid::parse_str(headers["x-request-id"].to_str()?)?;
        assert!(!headers.contains_key("set-cookie"), "{label}");
    }
    Ok(())
}

// H3 -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_body_has_exactly_the_contracts_members_and_the_defaults_body() -> Result<()> {
    let s = scene().await?;
    let contract = contract();
    let body = body_of(&s, &s.token).await;
    assert_eq!(keys(&body), properties(&contract, "SharingSnapshot"));
    assert_eq!(keys(&body["defaults"]), properties(&contract, "Defaults"));
    let home = home_of(&body, &s.home);
    assert_eq!(keys(home), properties(&contract, "Household"));
    for member in home["members"].as_array().unwrap() {
        assert_eq!(keys(member), properties(&contract, "HouseholdMember"));
    }
    assert_eq!(home["role"], "manager");
    assert_eq!(
        member_ids(&body, &s.home),
        BTreeSet::from([s.alice.clone(), s.bob.clone()])
    );

    // Quiescent, the same state is what the two existing reads say.
    let (_, defaults) = command_request(&s.w.app, "defaults", Some(&s.token), None, None).await;
    assert_eq!(body["defaults"], defaults);
    let (_, households) = command_request(&s.w.app, "households", Some(&s.token), None, None).await;
    assert_eq!(body["households"], households);

    // The operation is in the contract with the schema the body follows, and is a safe read.
    let operation = &contract["paths"]["/defaults/snapshot"]["get"];
    assert_eq!(operation["operationId"], "readSharingSnapshot");
    assert_eq!(
        operation["x-error-codes"],
        contract["paths"]["/defaults"]["get"]["x-error-codes"]
    );
    assert!(
        contract["paths"]["/defaults/snapshot"]
            .get("post")
            .is_none()
    );
    Ok(())
}

// H4, H5 ---------------------------------------------------------------------------------------

/// A snapshot read over HTTP, held between its two halves while `write` runs to completion. The
/// reply is the paused read's, which must be the state it began with.
async fn paused_over_http<F>(s: &Scene, write: F) -> Result<Reply>
where
    F: std::future::Future<Output = ()>,
{
    let mut gate = s.w.store.hooks().arm(POINT);
    let read = spawn_request_with(
        &s.w.app,
        "GET",
        "defaults/snapshot".into(),
        vec![("authorization".into(), bearer(&s.token))],
        Value::Null,
    );
    gate.reached().await;
    // The writer must finish while the read is still paused: a read that blocked writers would
    // hang here, so the wait is bounded.
    tokio::time::timeout(Duration::from_secs(20), write)
        .await
        .expect("the writer was blocked by the paused read");
    gate.release();
    Ok(read.await?)
}

async fn management(s: &Scene, token: &str, command: Value) {
    let (status, body) = command_request(
        &s.w.app,
        "management-commands",
        Some(token),
        Some(&id()),
        Some(json!({"commands":[command]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn home_version(s: &Scene) -> i64 {
    home_of(&body_of(s, &s.token).await, &s.home)["version"]
        .as_i64()
        .unwrap()
}

fn assert_isolation(s: &Scene) {
    if postgres() {
        let seen = s.w.store.hooks().isolation_seen();
        assert_eq!(seen.len() as u32, s.w.store.hooks().count(POINT));
        assert!(
            seen.iter().all(|level| level == "repeatable read"),
            "{seen:?}"
        );
    }
}

#[tokio::test]
async fn a_member_removed_by_the_real_route_is_invisible_to_a_paused_read() -> Result<()> {
    let s = scene().await?;
    let pre = body_of(&s, &s.token).await;
    let version = home_of(&pre, &s.home)["version"].as_i64().unwrap();
    let removal = management(
        &s,
        &s.token,
        json!({"kind":"remove_household_member","household_id":s.home,
               "account_id":s.bob,"expected_version":version}),
    );
    let (status, _, paused) = paused_over_http(&s, removal).await?;
    assert_eq!(status, StatusCode::OK, "{paused}");
    assert_eq!(
        paused, pre,
        "the paused read returns the state it began with"
    );
    let post = body_of(&s, &s.token).await;
    assert_ne!(revision(&post), revision(&pre));
    assert_eq!(
        member_ids(&post, &s.home),
        BTreeSet::from([s.alice.clone()])
    );
    assert_isolation(&s);
    Ok(())
}

#[tokio::test]
async fn an_invitation_accepted_by_the_real_route_is_invisible_to_a_paused_read() -> Result<()> {
    let s = scene().await?;
    let carol = login(&s.w.app, "snapshot-carol", "phone").await;
    let pre = body_of(&s, &s.token).await;
    assert!(!member_ids(&pre, &s.home).contains(&s.carol));
    let acceptance = management(
        &s,
        &carol,
        json!({"kind":"respond_to_household_invitation","id":s.invitation,
               "expected_version":1,"accept":true}),
    );
    let (status, _, paused) = paused_over_http(&s, acceptance).await?;
    assert_eq!(status, StatusCode::OK, "{paused}");
    assert_eq!(paused, pre);
    let post = body_of(&s, &s.token).await;
    assert_ne!(revision(&post), revision(&pre));
    assert!(member_ids(&post, &s.home).contains(&s.carol));
    assert_eq!(
        home_of(&post, &s.home)["version"].as_i64().unwrap(),
        home_of(&pre, &s.home)["version"].as_i64().unwrap() + 1
    );
    assert_isolation(&s);
    Ok(())
}

/// The writer outside `management`: a household signup invitation redeemed at registration adds
/// the new account as a member and must raise the household version like any other membership
/// change, or the revision would not move when the audience did.
#[tokio::test]
async fn a_household_signup_by_the_real_route_is_invisible_to_a_paused_read_then_appears()
-> Result<()> {
    let s = scene().await?;
    let (status, invitation) = command_request(
        &s.w.app,
        "account-invitations",
        Some(&s.token),
        None,
        Some(json!({"household_id": s.home})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{invitation}");
    let signup_token = invitation["token"].as_str().unwrap().to_owned();

    let pre = body_of(&s, &s.token).await;
    let before = home_version(&s).await;
    let register = {
        let app = s.w.app.clone();
        async move {
            let (status, _, body) = request(
                &app,
                "POST",
                "registration",
                &[],
                json!({"token":signup_token,"username":"snapshot-newcomer",
                       "password":PASSWORD,"device_id":"phone"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    };
    let (status, _, paused) = paused_over_http(&s, register).await?;
    assert_eq!(status, StatusCode::OK, "{paused}");
    assert_eq!(paused, pre, "the paused read must not see the signup");

    let post = body_of(&s, &s.token).await;
    assert_ne!(revision(&post), revision(&pre));
    let newcomer: String =
        sqlx::query_scalar("SELECT id FROM accounts WHERE username='snapshot-newcomer'")
            .fetch_one(&s.w.store.pool)
            .await?;
    assert!(member_ids(&post, &s.home).contains(&newcomer));
    assert_eq!(
        home_of(&post, &s.home)["version"].as_i64().unwrap(),
        before + 1
    );
    // The newcomer's own snapshot lists the household they joined, with themself as a member.
    let token = login(&s.w.app, "snapshot-newcomer", "phone").await;
    let theirs = body_of(&s, &token).await;
    assert_eq!(theirs["defaults"]["primary_household_id"], s.home.as_str());
    assert_eq!(home_of(&theirs, &s.home)["role"], "member");
    assert_isolation(&s);
    Ok(())
}
