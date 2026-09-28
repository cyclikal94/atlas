//! `GET /timer-sessions` (BE-B6) through the real router: authentication, headers, shape against the
//! contract, parameter validation, and the independent-timer commands a client actually sends.
use anyhow::Result;
use atlas_core::{Command, tasks::*};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::collections::BTreeSet;

use crate::support::http::{command_request as call, request};
use crate::support::retirement::{World, bearer, cookie_login, id, login, world};

const PATH: &str = "timer-sessions";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn contract() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../api/openapi.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn properties(contract: &Value, schema: &str) -> BTreeSet<String> {
    contract["components"]["schemas"][schema]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().unwrap().keys().cloned().collect()
}

fn seconds_task(task: &str) -> Value {
    json!({"command":{"kind":"create_task","id":task,"execution_id":id(),"title":"Timed","definition":{
        "schedule":{"start_date":null,"time":null,"timezone":"Europe/Vienna","repeat":null},
        "goal":{"kind":"numeric","minimum":"60","maximum":null,"unit":"seconds"},
        "carry":"retain_one","participation":"personal","open_days_before":0,"close_days_after":1}}})
}

/// A task with a seconds goal, over HTTP; returns its single occurrence.
async fn occurrence(w: &World, token: &str) -> String {
    let task = id();
    let reply = call(
        &w.app,
        "task-commands",
        Some(token),
        Some(&id()),
        Some(seconds_task(&task)),
    )
    .await;
    assert_eq!(reply.0, StatusCode::OK, "{:?}", reply.1);
    occurrence_id(&task, "once").unwrap()
}

async fn start_at(w: &World, token: &str, o: &str, session: &str, at: i64) -> (StatusCode, Value) {
    call(
        &w.app,
        "task-commands",
        Some(token),
        Some(&id()),
        Some(json!({"command":{"kind":"start_timer","occurrence_id":o,"session_id":session,"started_at":at}})),
    )
    .await
}

async fn list(w: &World, token: &str, query: &str) -> (StatusCode, axum::http::HeaderMap, Value) {
    request(
        &w.app,
        "GET",
        &format!("{PATH}{query}"),
        &[("authorization", &bearer(token))],
        Value::Null,
    )
    .await
}

#[tokio::test]
async fn the_route_is_authenticated_like_every_other_protected_read() -> Result<()> {
    let w = world(true).await?;
    let token = login(&w.app, "device-alice", "phone").await;

    let ok = list(&w, &token, "").await;
    assert_eq!(ok.0, StatusCode::OK, "{}", ok.2);
    assert_eq!(ok.1["cache-control"], "private, no-store");
    assert!(!ok.1.contains_key("set-cookie"));
    assert_eq!(ok.2, json!({"items":[],"next_after":null}));

    let (status, headers, body) = request(&w.app, "GET", PATH, &[], Value::Null).await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::UNAUTHORIZED, &json!("unauthenticated"))
    );
    assert!(!headers.contains_key("set-cookie"));
    assert_eq!(headers["cache-control"], "private, no-store");
    assert_eq!(
        list(&w, "not-a-session", "").await.0,
        StatusCode::UNAUTHORIZED
    );

    // A cookie caller presents its CSRF token: absent is `forbidden`, wrong is
    // `credential_mismatch`, right reads. Reading never sets a cookie.
    let (cookie, csrf) = cookie_login(&w.app, "browser").await;
    let wrong = "0".repeat(64);
    let get = |headers: Vec<(&'static str, String)>| {
        let app = w.app.clone();
        async move {
            let headers: Vec<(&str, &str)> =
                headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
            request(&app, "GET", PATH, &headers, Value::Null).await
        }
    };
    let (status, _, body) = get(vec![("cookie", cookie.clone())]).await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::FORBIDDEN, &json!("forbidden"))
    );
    let (status, _, body) = get(vec![("cookie", cookie.clone()), ("x-csrf-token", wrong)]).await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::FORBIDDEN, &json!("credential_mismatch"))
    );
    let (status, headers, body) = get(vec![("cookie", cookie), ("x-csrf-token", csrf)]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!headers.contains_key("set-cookie"));

    // A revoked credential is refused, never answered with an empty list.
    let logout = request(
        &w.app,
        "DELETE",
        "sessions/current",
        &[("authorization", &bearer(&token))],
        Value::Null,
    )
    .await;
    assert_eq!(logout.0, StatusCode::NO_CONTENT);
    let (status, _, body) = list(&w, &token, "").await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::UNAUTHORIZED, &json!("unauthenticated"))
    );
    Ok(())
}

#[tokio::test]
async fn parameters_are_validated_before_anything_is_read() -> Result<()> {
    let w = world(false).await?;
    let token = login(&w.app, "device-alice", "phone").await;
    let cursor = |kind: &str| format!("{kind}.1.{}", id());
    for (query, status, code) in [
        (
            "?limit=0".to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        (
            "?limit=201".into(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        (
            "?limit=x".into(),
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
        (
            "?limit=-1".into(),
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
        (
            "?limit=70000".into(),
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
        (
            "?state=bogus".into(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        (
            "?state=".into(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        (
            "?after=".into(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        (
            "?after=garbage".into(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        (
            format!("?state=stopped&after={}", cursor("r")),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        (
            format!("?state=running&after={}", cursor("s")),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        ),
        (
            "?unknown=1".into(),
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
        (
            "?limit=1&limit=2".into(),
            StatusCode::BAD_REQUEST,
            "malformed_request",
        ),
    ] {
        let (got, headers, body) = list(&w, &token, &query).await;
        assert_eq!((got, &body["code"]), (status, &json!(code)), "{query}");
        assert!(!headers.contains_key("set-cookie"));
    }
    for query in [
        "?state=running".to_string(),
        "?state=stopped&limit=200".into(),
        "?state=all&limit=1".into(),
        format!("?after={}", cursor("r")),
        format!("?state=all&after={}", cursor("s")),
    ] {
        assert_eq!(list(&w, &token, &query).await.0, StatusCode::OK, "{query}");
    }
    Ok(())
}

#[tokio::test]
async fn independent_timers_run_concurrently_and_are_listed_against_the_contract() -> Result<()> {
    let w = world(false).await?;
    let token = login(&w.app, "device-alice", "phone").await;
    let contract = contract();
    let (one, two) = (occurrence(&w, &token).await, occurrence(&w, &token).await);
    let base = now();
    let (first, second) = (id(), id());

    // Two occurrences, sent at once: both are accepted, neither is a conflict.
    let (a, b) = tokio::join!(
        start_at(&w, &token, &one, &first, base - 600),
        start_at(&w, &token, &two, &second, base - 500)
    );
    assert_eq!(
        (a.0, b.0),
        (StatusCode::OK, StatusCode::OK),
        "{:?} {:?}",
        a.1,
        b.1
    );

    // The occurrence that already runs a timer refuses another, over HTTP and on replay.
    let operation = id();
    let body = json!({"command":{"kind":"start_timer","occurrence_id":one,"session_id":id(),"started_at":base - 400}});
    let refused = call(
        &w.app,
        "task-commands",
        Some(&token),
        Some(&operation),
        Some(body.clone()),
    )
    .await;
    assert_eq!(
        (refused.0, &refused.1["code"]),
        (StatusCode::CONFLICT, &json!("conflict"))
    );
    // Replaying the same key and body returns the original refusal; it is not re-evaluated.
    let replay = call(
        &w.app,
        "task-commands",
        Some(&token),
        Some(&operation),
        Some(body),
    )
    .await;
    assert_eq!(
        (replay.0, &replay.1["code"]),
        (StatusCode::CONFLICT, &json!("conflict"))
    );

    // The same key with a different body is `operation_conflict` (unchanged).
    let key = id();
    let accepted_body = json!({"command":{"kind":"start_timer","occurrence_id":occurrence(&w, &token).await,"session_id":id(),"started_at":base - 300}});
    let accepted = call(
        &w.app,
        "task-commands",
        Some(&token),
        Some(&key),
        Some(accepted_body.clone()),
    )
    .await;
    assert_eq!(accepted.0, StatusCode::OK);
    assert_eq!(
        call(
            &w.app,
            "task-commands",
            Some(&token),
            Some(&key),
            Some(accepted_body)
        )
        .await,
        accepted
    );
    let different = json!({"command":{"kind":"start_timer","occurrence_id":one,"session_id":id(),"started_at":base - 299}});
    let changed = call(
        &w.app,
        "task-commands",
        Some(&token),
        Some(&key),
        Some(different),
    )
    .await;
    assert_eq!(
        (changed.0, &changed.1["code"]),
        (StatusCode::CONFLICT, &json!("operation_conflict"))
    );

    // All three are listed as running, newest start first, with exactly the contract's members.
    let (status, headers, page) = list(&w, &token, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(headers["cache-control"], "private, no-store");
    assert_eq!(keys(&page), properties(&contract, "AccountTimerPage"));
    assert_eq!(page["next_after"], Value::Null);
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    for item in items {
        assert_eq!(item["access"], "available");
        assert_eq!(keys(item), properties(&contract, "AccountTimerSession"));
        assert_eq!(item["stopped_at"], Value::Null);
        assert_eq!(item["can_modify"], true);
        assert_eq!(item["task_title"], "Timed");
    }
    let started: Vec<i64> = items
        .iter()
        .map(|i| i["started_at"].as_i64().unwrap())
        .collect();
    assert_eq!(started, [base - 300, base - 500, base - 600]);
    assert_eq!(items[2]["id"], json!(first));
    assert_eq!(items[2]["occurrence_id"], json!(one));

    // Stopping one records its own duration; the others keep running.
    let stopped = call(
        &w.app,
        "task-commands",
        Some(&token),
        Some(&id()),
        Some(json!({"command":{"kind":"stop_timer","occurrence_id":two,"session_id":second,"expected_version":1,"stopped_at":base - 200}})),
    )
    .await;
    assert_eq!(stopped.0, StatusCode::OK, "{:?}", stopped.1);
    let (_, _, all) = list(&w, &token, "").await;
    let states: Vec<bool> = all["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["stopped_at"].is_null())
        .collect();
    assert_eq!(states, [true, true, false], "running first, then history");
    let (_, _, history) = list(&w, &token, "?state=stopped").await;
    assert_eq!(history["items"].as_array().unwrap().len(), 1);
    assert_eq!(history["items"][0]["stopped_at"], base - 200);
    assert_eq!(history["items"][0]["can_modify"], false);
    let (_, _, running) = list(&w, &token, "?state=running&limit=1").await;
    assert_eq!(running["items"].as_array().unwrap().len(), 1);
    assert!(running["next_after"].as_str().unwrap().starts_with("r."));
    Ok(())
}

#[tokio::test]
async fn a_restricted_row_carries_exactly_the_documented_members() -> Result<()> {
    let w = world(false).await?;
    let alice = login(&w.app, "device-alice", "phone").await;
    let bob = login(&w.app, "device-bob", "phone").await;
    let bob_id: String = sqlx::query_scalar("SELECT id FROM accounts WHERE username='device-bob'")
        .fetch_one(&w.store.pool)
        .await?;
    let contract = contract();

    // Alice's shared task, timed by bob; bob also owns a private task he times.
    let task = id();
    let mut body = seconds_task(&task);
    body["command"]["definition"]["participation"] = json!("anyone");
    body["command"]["initial_policy"] =
        json!({"grants":[{"kind":"account","id":bob_id,"edit":true}],"exclude_accounts":[]});
    // An explicit share is an access change, so it goes to the online-only endpoint.
    let created = call(
        &w.app,
        "task-access-commands",
        Some(&alice),
        Some(&id()),
        Some(body),
    )
    .await;
    assert_eq!(created.0, StatusCode::OK, "{:?}", created.1);
    let shared = occurrence_id(&task, "once")?;
    let mine = occurrence(&w, &bob).await;
    let base = now();
    for (o, at) in [(&shared, base - 400), (&mine, base - 300)] {
        let started = start_at(&w, &bob, o, &id(), at).await;
        assert_eq!(started.0, StatusCode::OK, "{:?}", started.1);
    }
    let (_, _, before) = list(&w, &bob, "").await;
    assert_eq!(before["items"].as_array().unwrap().len(), 2);
    assert!(
        before["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["access"] == "available")
    );

    // Alice withdraws bob's access: the row stays, with four members and no task detail.
    let version: i64 = sqlx::query_scalar("SELECT policy_version FROM resources WHERE id=$1")
        .bind(&task)
        .fetch_one(&w.store.pool)
        .await?;
    w.store
        .apply(
            &w.alice,
            &id(),
            &[Command::Revoke {
                id: task.clone(),
                expected_version: version,
                account_id: bob_id.clone(),
            }],
        )
        .await?;
    let (status, _, after) = list(&w, &bob, "").await;
    assert_eq!(status, StatusCode::OK, "{after}");
    let items = after["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "restricted rows are not omitted");
    let restricted: Vec<&Value> = items
        .iter()
        .filter(|i| i["access"] == "restricted")
        .collect();
    assert_eq!(restricted.len(), 1);
    assert_eq!(
        keys(restricted[0]),
        properties(&contract, "RestrictedTimerSession")
    );
    let text = restricted[0].to_string();
    for leaked in [
        task.as_str(),
        shared.as_str(),
        "Timed",
        "task_title",
        "occurrence_id",
        "slot_date",
        "can_modify",
    ] {
        assert!(!text.contains(leaked), "{text} leaks {leaked}");
    }
    let available = items.iter().find(|i| i["access"] == "available").unwrap();
    assert_eq!(available["occurrence_id"], json!(mine));
    // A restricted running timer cannot be stopped by its owner, and nothing was stopped for them.
    let stop = call(
        &w.app,
        "task-commands",
        Some(&bob),
        Some(&id()),
        Some(json!({"command":{"kind":"stop_timer","occurrence_id":shared,"session_id":restricted[0]["id"],"expected_version":1,"stopped_at":base - 100}})),
    )
    .await;
    assert_eq!(
        (stop.0, &stop.1["code"]),
        (StatusCode::NOT_FOUND, &json!("not_found"))
    );
    let (_, _, unchanged) = list(&w, &bob, "?state=running").await;
    assert_eq!(unchanged["items"].as_array().unwrap().len(), 2);
    // Alice sees none of bob's timers.
    assert!(
        list(&w, &alice, "").await.2["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    Ok(())
}

#[test]
fn the_contract_describes_a_safe_authenticated_read_with_the_documented_errors() {
    let contract = contract();
    let path = &contract["paths"]["/timer-sessions"];
    let operation = &path["get"];
    assert_eq!(operation["operationId"], "listAccountTimerSessions");
    assert_eq!(
        operation["x-error-codes"],
        contract["paths"]["/invitations/sent"]["get"]["x-error-codes"]
    );
    let statuses: BTreeSet<&str> = operation["responses"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        statuses,
        BTreeSet::from(["200", "400", "401", "403", "404", "422", "500", "503"])
    );
    assert_eq!(
        path.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["get"],
        "a safe read only"
    );
    let parameters: BTreeSet<&str> = operation["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(parameters, BTreeSet::from(["state", "after", "limit"]));
    // The per-occurrence read is untouched.
    assert_eq!(
        contract["paths"]["/occurrences/{id}/timers"]["get"]["operationId"],
        "listOwnTimerSessions"
    );
    // Both row schemas are closed, and the two shapes are told apart by `access` alone.
    for name in ["AccountTimerSession", "RestrictedTimerSession"] {
        let schema = &contract["components"]["schemas"][name];
        assert_eq!(schema["additionalProperties"], false);
        let required: BTreeSet<String> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            required,
            properties(&contract, name),
            "{name}: every member is required"
        );
    }
    assert_eq!(
        contract["components"]["schemas"]["RestrictedTimerSession"]["properties"]["access"]["const"],
        "restricted"
    );
    assert_eq!(
        contract["components"]["schemas"]["AccountTimerSession"]["properties"]["access"]["const"],
        "available"
    );
}
