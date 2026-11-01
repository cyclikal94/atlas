//! `GET /occurrences/{id}/dependency-graph` (BE-DEPENDENCY-GRAPH) through the real router:
//! authentication, CSRF, headers, parameter validation, indistinguishable `not_found`, every
//! `200` body validated against `api/openapi.json`, and the editor flow through
//! `/task-commands`. Traversal, privacy and snapshot semantics are `atlas-core`'s
//! `tasks::dependency_graph` cases.
use anyhow::Result;
use atlas_core::{
    Store,
    households::ManagementCommand,
    policy::{Policy, PrincipalGrant},
    tasks::*,
};
use atlas_server::now;
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::collections::BTreeSet;

use crate::support::contract::{conforms, contract};
use crate::support::http::request;
use crate::support::retirement::{
    Reply, World, assert_error, bearer, cookie_login, id, login, world,
};

fn checkbox() -> Definition {
    Definition {
        schedule: Schedule {
            start_date: None,
            time: None,
            timezone: "Europe/London".into(),
            repeat: None,
        },
        goal: Goal::Checkbox,
        carry: Carry::RetainOne,
        participation: Participation::Personal,
        open_days_before: 0,
        close_days_after: 1,
        allow_streak_exclusions: false,
    }
}

async fn make(s: &Store, a: &str, title: &str, policy: Option<Policy>) -> Result<String> {
    let t = id();
    s.task_command(
        a,
        &id(),
        &TaskCommand::CreateTask {
            id: t.clone(),
            execution_id: id(),
            title: title.into(),
            anchor: None,
            initial_policy: policy,
            definition: checkbox(),
        },
        None,
        now(),
    )
    .await?;
    occurrence_id(&t, "once")
}

async fn version(s: &Store, resource: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT version FROM resources WHERE id=$1")
            .bind(resource)
            .fetch_one(&s.pool)
            .await?,
    )
}

async fn set(s: &Store, a: &str, o: &str, prerequisites: Vec<String>) -> Result<()> {
    s.task_command(
        a,
        &id(),
        &TaskCommand::SetDependencies {
            occurrence_id: o.into(),
            expected_version: version(s, o).await?,
            prerequisites,
            strict: false,
        },
        None,
        now(),
    )
    .await?;
    Ok(())
}

fn to(account: &str) -> Option<Policy> {
    Some(Policy {
        grants: vec![PrincipalGrant::Account {
            id: account.into(),
            edit: true,
        }],
        exclude_accounts: vec![],
    })
}

struct Scene {
    w: World,
    alice_token: String,
    bob_token: String,
    /// root → {cook, drinks} → table: a shared prerequisite.
    root: String,
    cook: String,
    drinks: String,
    table: String,
    /// Alice's, shared with Bob, with one prerequisite private to Alice.
    shared: String,
    private: String,
    empty: String,
}

async fn scene() -> Result<Scene> {
    let w = world(true).await?;
    let s = w.store.clone();
    let alice = w.alice.clone();
    let bob: String = sqlx::query_scalar("SELECT id FROM accounts WHERE username='device-bob'")
        .fetch_one(&s.pool)
        .await?;
    let table = make(&s, &alice, "Set the table", None).await?;
    let cook = make(&s, &alice, "Cook", None).await?;
    let drinks = make(&s, &alice, "Pour drinks", None).await?;
    let root = make(&s, &alice, "Serve dinner", None).await?;
    set(&s, &alice, &cook, vec![table.clone()]).await?;
    set(&s, &alice, &drinks, vec![table.clone()]).await?;
    set(&s, &alice, &root, vec![cook.clone(), drinks.clone()]).await?;
    let private = make(&s, &alice, "Surprise", None).await?;
    let shared = make(&s, &alice, "Shared plan", to(&bob)).await?;
    set(&s, &alice, &shared, vec![private.clone()]).await?;
    let empty = make(&s, &alice, "Nothing first", None).await?;
    Ok(Scene {
        alice_token: bearer(&login(&w.app, "device-alice", "phone").await),
        bob_token: bearer(&login(&w.app, "device-bob", "laptop").await),
        w,
        root,
        cook,
        drinks,
        table,
        shared,
        private,
        empty,
    })
}

async fn get(s: &Scene, auth: Option<&str>, path: &str) -> Reply {
    let headers: Vec<(&str, &str)> = auth.map(|a| vec![("authorization", a)]).unwrap_or_default();
    request(&s.w.app, "GET", path, &headers, Value::Null).await
}

fn graph_path(occurrence: &str, query: &str) -> String {
    format!("occurrences/{occurrence}/dependency-graph{query}")
}

#[tokio::test]
async fn every_graph_body_conforms_to_the_contract() -> Result<()> {
    let s = scene().await?;
    let a = s.alice_token.as_str();
    let diamond = get(&s, Some(a), &graph_path(&s.root, "")).await;
    assert_eq!(diamond.0, StatusCode::OK, "{}", diamond.2);
    conforms("DependencyGraph", &diamond.2);
    assert_eq!(diamond.2["nodes"].as_array().unwrap().len(), 4);
    assert_eq!(diamond.2["coverage"], "complete");
    let edges: BTreeSet<(String, String)> = diamond.2["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["prerequisite_id"].as_str().unwrap().to_owned(),
                e["dependant_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        edges,
        BTreeSet::from([
            (s.table.clone(), s.cook.clone()),
            (s.table.clone(), s.drinks.clone()),
            (s.cook.clone(), s.root.clone()),
            (s.drinks.clone(), s.root.clone()),
        ])
    );
    let partial = get(&s, Some(a), &graph_path(&s.root, "?max_depth=1")).await;
    assert_eq!(partial.0, StatusCode::OK);
    conforms("DependencyGraph", &partial.2);
    assert_eq!(partial.2["coverage"], "partial");
    assert_eq!(partial.2["max_depth"], 1);
    let empty = get(&s, Some(a), &graph_path(&s.empty, "")).await;
    conforms("DependencyGraph", &empty.2);
    assert_eq!(empty.2["edges"], json!([]));
    // Bob's restricted view discloses nothing about Alice's private prerequisite.
    let restricted = get(&s, Some(&s.bob_token), &graph_path(&s.shared, "")).await;
    assert_eq!(restricted.0, StatusCode::OK, "{}", restricted.2);
    conforms("DependencyGraph", &restricted.2);
    assert_eq!(restricted.2["restricted"], true);
    assert_eq!(restricted.2["nodes"][0]["dependency_state"], "restricted");
    assert_eq!(restricted.2["nodes"][0]["restricted_prerequisites"], true);
    let body = restricted.2.to_string();
    assert!(!body.contains(&s.private) && !body.contains("Surprise"));
    // Alice's own view of the same graph.
    let full = get(&s, Some(a), &graph_path(&s.shared, "")).await;
    conforms("DependencyGraph", &full.2);
    assert_eq!(full.2["restricted"], false);
    Ok(())
}

#[tokio::test]
async fn statuses_parameters_and_headers() -> Result<()> {
    let s = scene().await?;
    let a = s.alice_token.as_str();
    let ok = get(&s, Some(a), &graph_path(&s.root, "?max_depth=200")).await;
    assert_eq!(ok.0, StatusCode::OK);
    assert_eq!(ok.1["cache-control"], "private, no-store");
    assert!(!ok.1.contains_key("set-cookie"));
    assert_error(
        &get(&s, None, &graph_path(&s.root, "")).await,
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
    );
    assert_error(
        &get(&s, Some(&bearer(&"0".repeat(64))), &graph_path(&s.root, "")).await,
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
    );
    for query in [
        "?max_depth=0",
        "?max_depth=201",
        "?max_depth=-1",
        "?max_depth=70000",
    ] {
        assert_error(
            &get(&s, Some(a), &graph_path(&s.root, query)).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        );
    }
    for query in [
        "?max_depth=deep",
        "?max_depth=1.5",
        "?depth=1",
        "?max_depth=1&limit=2",
    ] {
        assert_error(
            &get(&s, Some(a), &graph_path(&s.root, query)).await,
            StatusCode::BAD_REQUEST,
            "malformed_request",
        );
    }
    // A real occurrence the reader cannot see and an id that names nothing are the same
    // `not_found`, apart from the request id.
    let invisible = get(&s, Some(&s.bob_token), &graph_path(&s.root, "")).await;
    let missing = get(&s, Some(&s.bob_token), &graph_path(&id(), "")).await;
    assert_error(&invisible, StatusCode::NOT_FOUND, "not_found");
    assert_error(&missing, StatusCode::NOT_FOUND, "not_found");
    let strip = |v: &Value| {
        let mut v = v.clone();
        v.as_object_mut().unwrap().remove("request_id");
        v
    };
    assert_eq!(strip(&invisible.2), strip(&missing.2));
    // A task id is not an occurrence.
    let task: String = sqlx::query_scalar(
        "SELECT parent_id FROM resources WHERE id=(SELECT parent_id FROM resources WHERE id=$1)",
    )
    .bind(&s.root)
    .fetch_one(&s.w.store.pool)
    .await?;
    assert_error(
        &get(&s, Some(a), &graph_path(&task, "")).await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    // A cookie caller presents its CSRF token like every other protected read.
    let (cookie, csrf) = cookie_login(&s.w.app, "browser").await;
    let path = graph_path(&s.root, "");
    let with = |headers: Vec<(&'static str, String)>| {
        let (app, path) = (s.w.app.clone(), path.clone());
        async move {
            let headers: Vec<(&str, &str)> =
                headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
            request(&app, "GET", &path, &headers, Value::Null).await
        }
    };
    assert_error(
        &with(vec![("cookie", cookie.clone())]).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    );
    assert_error(
        &with(vec![
            ("cookie", cookie.clone()),
            ("x-csrf-token", "0".repeat(64)),
        ])
        .await,
        StatusCode::FORBIDDEN,
        "credential_mismatch",
    );
    let read = with(vec![("cookie", cookie), ("x-csrf-token", csrf)]).await;
    assert_eq!(read.0, StatusCode::OK, "{}", read.2);
    assert!(!read.1.contains_key("set-cookie"));
    conforms("DependencyGraph", &read.2);
    Ok(())
}

#[tokio::test]
async fn the_editor_flow_uses_graph_versions_and_rejects_stale_edits() -> Result<()> {
    let s = scene().await?;
    let a = s.alice_token.as_str();
    let edit = |version: &Value, prerequisites: Vec<&str>| {
        let body = json!({"command": {
            "kind": "set_dependencies",
            "occurrence_id": s.root,
            "expected_version": version,
            "prerequisites": prerequisites,
            "strict": false,
        }});
        let (app, op) = (s.w.app.clone(), id());
        async move {
            request(
                &app,
                "POST",
                "task-commands",
                &[("authorization", a), ("idempotency-key", &op)],
                body,
            )
            .await
        }
    };
    let first = get(&s, Some(a), &graph_path(&s.root, "?max_depth=1"))
        .await
        .2;
    let root_version = first["nodes"][0]["version"].clone();
    let applied = edit(&root_version, vec![s.cook.as_str()]).await;
    assert_eq!(applied.0, StatusCode::OK, "{}", applied.2);
    let second = get(&s, Some(a), &graph_path(&s.root, "?max_depth=1"))
        .await
        .2;
    assert_eq!(second["direct_prerequisites"], json!([s.cook]));
    assert!(second["nodes"][0]["version"].as_i64() > root_version.as_i64());
    // The first read's version is stale now: refused, and nothing changes.
    assert_error(
        &edit(&root_version, vec![s.drinks.as_str()]).await,
        StatusCode::CONFLICT,
        "conflict",
    );
    assert_eq!(
        get(&s, Some(a), &graph_path(&s.root, "?max_depth=1"))
            .await
            .2,
        second
    );
    // A self-reference stays invalid.
    assert_error(
        &edit(&second["nodes"][0]["version"], vec![s.root.as_str()]).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_value",
    );
    Ok(())
}

#[test]
fn the_contract_describes_a_safe_authenticated_read_and_leaves_the_preview_alone() {
    let contract = contract();
    let path = &contract["paths"]["/occurrences/{id}/dependency-graph"];
    assert_eq!(
        path.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["get"],
        "a safe read only"
    );
    let operation = &path["get"];
    assert_eq!(operation["operationId"], "getDependencyGraph");
    let preview = &contract["paths"]["/occurrences/{id}/dependencies"]["get"];
    assert_eq!(operation["x-error-codes"], preview["x-error-codes"]);
    let statuses = |op: &Value| {
        op["responses"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(statuses(operation), statuses(preview));
    for (status, codes) in operation["x-error-codes"].as_object().unwrap() {
        let described = operation["responses"][status]["description"]
            .as_str()
            .unwrap();
        for code in codes.as_array().unwrap() {
            let code = code.as_str().unwrap();
            assert!(
                described.contains(code) || code == "credential_mismatch",
                "{status} {code}"
            );
        }
    }
    let parameters: Vec<&str> = operation["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(parameters, ["id", "max_depth"]);
    assert_eq!(
        operation["parameters"][1]["schema"],
        json!({"type": "integer", "minimum": 1, "maximum": 200, "default": 200})
    );
    assert_eq!(
        operation["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/DependencyGraph"
    );
    // The completion preview and its schemas are unchanged.
    assert_eq!(preview["operationId"], "previewDependencyCompletion");
    let schemas = &contract["components"]["schemas"];
    assert_eq!(
        schemas["DependencyPreview"]["required"],
        json!(["token", "items", "unavailable", "strict"])
    );
    assert_eq!(
        schemas["DependencyItem"]["required"],
        json!([
            "occurrence_id",
            "task_id",
            "complete",
            "can_complete",
            "progress_version"
        ])
    );
    for name in [
        "DependencyGraph",
        "DependencyGraphNode",
        "DependencyEdge",
        "DependencyGraphTask",
    ] {
        assert_eq!(
            schemas[name]["additionalProperties"],
            json!(false),
            "{name} is strict"
        );
        let required: BTreeSet<&str> = schemas[name]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let properties: BTreeSet<&str> = schemas[name]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(required, properties, "{name}: every member is required");
        for example in schemas[name]["examples"].as_array().unwrap() {
            conforms(name, example);
        }
    }
}

/// The serialised core types are exactly the contract's members (no drift between the two).
#[tokio::test]
async fn the_core_types_serialise_exactly_the_contract_members() -> Result<()> {
    let s = scene().await?;
    let body = get(&s, Some(&s.alice_token), &graph_path(&s.root, ""))
        .await
        .2;
    let contract = contract();
    let schemas = &contract["components"]["schemas"];
    let keys = |v: &Value| {
        v.as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
    };
    let properties = |name: &str| keys(&schemas[name]["properties"]);
    assert_eq!(keys(&body), properties("DependencyGraph"));
    assert_eq!(keys(&body["nodes"][0]), properties("DependencyGraphNode"));
    assert_eq!(keys(&body["edges"][0]), properties("DependencyEdge"));
    assert_eq!(keys(&body["tasks"][0]), properties("DependencyGraphTask"));
    Ok(())
}

/// Revoking Bob's access makes the shared root `not_found` for him at once.
#[tokio::test]
async fn revocation_is_immediate_over_http() -> Result<()> {
    let s = scene().await?;
    let before = get(&s, Some(&s.bob_token), &graph_path(&s.shared, "")).await;
    assert_eq!(before.0, StatusCode::OK);
    let task: String = sqlx::query_scalar(
        "SELECT parent_id FROM resources WHERE id=(SELECT parent_id FROM resources WHERE id=$1)",
    )
    .bind(&s.shared)
    .fetch_one(&s.w.store.pool)
    .await?;
    s.w.store
        .management(
            &s.w.alice,
            &id(),
            &[ManagementCommand::ReplacePolicy {
                id: task.clone(),
                expected_version: version(&s.w.store, &task).await?,
                policy: Policy::default(),
            }],
            now(),
        )
        .await?;
    assert_error(
        &get(&s, Some(&s.bob_token), &graph_path(&s.shared, "")).await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    Ok(())
}
