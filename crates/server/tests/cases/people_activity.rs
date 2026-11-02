//! `GET /people/{id}/activity` (BE-PEOPLE-ACTIVITY) through the real router: authentication and
//! headers, the indistinguishable `not_found` shared with `GET /people/{id}`, parameter validation,
//! every `200` body validated against `api/openapi.json`, cross-device reads, and an HTTP retry
//! with the same `Idempotency-Key` that adds no event. The recording and disclosure semantics are
//! `atlas-core`'s `people/activity` cases.
use anyhow::Result;
use atlas_core::{
    Command,
    people::PeopleCommand,
    policy::{Policy, PrincipalGrant},
};
use atlas_server::{hash_password, now};
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::cases::people_summary::{check, contract};

/// `people_summary::conforms`, refusing a schema name the contract does not define (which that
/// checker would otherwise accept vacuously).
fn conforms(schema: &str, body: &Value) {
    assert!(
        contract()["components"]["schemas"].get(schema).is_some(),
        "{schema} is not in api/openapi.json"
    );
    crate::cases::people_summary::conforms(schema, body);
}
use crate::support::http::{command_request, request};
use crate::support::retirement::{PASSWORD, Reply, World, bearer, id, login, world};

struct Scene {
    w: World,
    alice: String,
    bob: String,
    /// Alice's, shared with Bob; one shared field, one private field.
    shared: String,
    field: String,
    private_field: String,
    /// Alice's, shared with nobody.
    private: String,
    alice_phone: String,
    alice_laptop: String,
    bob_token: String,
    carol_token: String,
}

async fn scene() -> Result<Scene> {
    let w = world(false).await?;
    let alice = w.alice.clone();
    let bob: String = sqlx::query_scalar("SELECT id FROM accounts WHERE username='device-bob'")
        .fetch_one(&w.store.pool)
        .await?;
    w.store
        .add_account(
            &id(),
            "activity-carol",
            &hash_password(PASSWORD.into()).await?,
        )
        .await?;
    let to_bob = Policy {
        grants: vec![PrincipalGrant::Account {
            id: bob.clone(),
            edit: true,
        }],
        exclude_accounts: vec![],
    };
    let (shared, field, private_field, private) = (id(), id(), id(), id());
    w.store
        .apply(
            &alice,
            &id(),
            &[
                Command::CreatePerson {
                    id: shared.clone(),
                    name: "Mo".into(),
                    initial_policy: Some(to_bob.clone()),
                },
                Command::CreateField {
                    id: field.clone(),
                    person_id: shared.clone(),
                    label: "Birthday".into(),
                    value: "1 May".into(),
                    initial_policy: Some(to_bob),
                },
                Command::CreateField {
                    id: private_field.clone(),
                    person_id: shared.clone(),
                    label: "Medical note".into(),
                    value: "secret".into(),
                    initial_policy: Some(Policy::default()),
                },
                Command::CreatePerson {
                    id: private.clone(),
                    name: "Private".into(),
                    initial_policy: Some(Policy::default()),
                },
            ],
        )
        .await?;
    let alice_phone = login(&w.app, "device-alice", "phone").await;
    let alice_laptop = login(&w.app, "device-alice", "laptop").await;
    let bob_token = login(&w.app, "device-bob", "laptop").await;
    let carol_token = login(&w.app, "activity-carol", "tablet").await;
    Ok(Scene {
        w,
        alice,
        bob,
        shared,
        field,
        private_field,
        private,
        alice_phone,
        alice_laptop,
        bob_token,
        carol_token,
    })
}

async fn get(s: &Scene, token: Option<&str>, path: &str) -> Reply {
    let auth = token.map(bearer);
    let headers: Vec<(&str, &str)> = auth
        .as_deref()
        .map(|a| vec![("authorization", a)])
        .unwrap_or_default();
    request(&s.w.app, "GET", path, &headers, Value::Null).await
}

fn assert_error(reply: &Reply, status: StatusCode, code: &str) {
    assert_eq!(reply.0, status, "{}", reply.2);
    assert_eq!(reply.2["code"], code, "{}", reply.2);
    conforms("Error", &reply.2);
}

fn strip(mut body: Value) -> Value {
    body.as_object_mut().unwrap().remove("request_id");
    body
}

/// An edit through `POST /commands`, retried with the same key when `retries` > 0.
async fn edit_over_http(
    s: &Scene,
    token: &str,
    resource: &str,
    label: &str,
    retries: usize,
) -> Value {
    let (version, policy_version): (i64, i64) =
        sqlx::query_as("SELECT version,policy_version FROM resources WHERE id=$1")
            .bind(resource)
            .fetch_one(&s.w.store.pool)
            .await
            .unwrap();
    let body = json!({"commands":[{"kind":"edit","id":resource,"expected_version":version,
        "expected_policy_version":policy_version,"label":label,"value":"2 May"}]});
    let key = id();
    let first = command_request(
        &s.w.app,
        "commands",
        Some(token),
        Some(&key),
        Some(body.clone()),
    )
    .await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    for _ in 0..retries {
        let again = command_request(
            &s.w.app,
            "commands",
            Some(token),
            Some(&key),
            Some(body.clone()),
        )
        .await;
        assert_eq!(again, first, "a retry returns the same receipt");
    }
    first.1
}

#[test]
fn the_contract_rejects_hidden_or_historical_members_in_an_event() {
    let contract = contract();
    let schema = json!({"$ref": "#/components/schemas/PersonActivityEvent"});
    let good = json!({"id": id(), "occurred_at": 1, "type": "field_edited", "summary": "Edited a field",
        "actor": {"kind": "someone_else"}, "changed": ["value"],
        "target": {"kind": "field", "id": id(), "label": "Hobby", "archived": false, "state": "current", "field_kind": "text"}});
    assert!(check(&contract, &schema, &good, "e").is_ok());
    let mut old_value = good.clone();
    old_value["previous_label"] = json!("Old");
    let mut hidden_actor = good.clone();
    hidden_actor["actor"] = json!({"kind": "someone_else", "account_id": id()});
    let mut sharing = good.clone();
    sharing["type"] = json!("field_shared");
    let mut revision = good.clone();
    revision["revision"] = json!(3);
    let mut bad_part = good.clone();
    bad_part["changed"] = json!(["policy"]);
    for bad in [old_value, hidden_actor, sharing, revision, bad_part] {
        assert!(check(&contract, &schema, &bad, "e").is_err(), "{bad}");
    }
}

#[tokio::test]
async fn the_route_is_authenticated_private_and_never_sets_a_cookie() -> Result<()> {
    let s = scene().await?;
    let path = format!("people/{}/activity", s.shared);
    for token in [None, Some("0".repeat(64))] {
        assert_error(
            &get(&s, token.as_deref(), &path).await,
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
        );
    }
    let reply = get(&s, Some(&s.bob_token), &path).await;
    assert_eq!(reply.0, StatusCode::OK, "{}", reply.2);
    assert_eq!(reply.1["cache-control"], "private, no-store");
    assert!(!reply.1.contains_key("set-cookie"));
    Ok(())
}

#[tokio::test]
async fn hidden_missing_and_aliased_people_answer_exactly_as_get_person() -> Result<()> {
    let s = scene().await?;
    for subject in [s.private.clone(), id()] {
        let activity = get(
            &s,
            Some(&s.bob_token),
            &format!("people/{subject}/activity"),
        )
        .await;
        let detail = get(&s, Some(&s.bob_token), &format!("people/{subject}")).await;
        assert_error(&activity, StatusCode::NOT_FOUND, "not_found");
        assert_eq!((activity.0, strip(activity.2)), (detail.0, strip(detail.2)));
    }
    assert_error(
        &get(
            &s,
            Some(&s.carol_token),
            &format!("people/{}/activity", s.shared),
        )
        .await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    // A merged ID resolves to the canonical timeline for a reader of it.
    let preview =
        s.w.store
            .merge_preview(&s.alice, &s.private, &s.shared)
            .await?;
    s.w.store
        .people_command(
            &s.alice,
            &id(),
            &PeopleCommand::Merge {
                source_id: s.private.clone(),
                target_id: s.shared.clone(),
                preview_token: preview.token,
                name: "Mo".into(),
            },
            now(),
        )
        .await?;
    let via_alias = get(
        &s,
        Some(&s.alice_phone),
        &format!("people/{}/activity", s.private),
    )
    .await;
    let direct = get(
        &s,
        Some(&s.alice_phone),
        &format!("people/{}/activity", s.shared),
    )
    .await;
    assert_eq!(via_alias.0, StatusCode::OK);
    assert_eq!(via_alias.2, direct.2);
    assert_eq!(via_alias.2["person_id"], s.shared);
    conforms("PersonActivityPage", &via_alias.2);
    assert_eq!(via_alias.2["items"][0]["type"], "person_merged");
    // Bob never saw the private person: the merge without its source.
    let for_bob = get(
        &s,
        Some(&s.bob_token),
        &format!("people/{}/activity", s.shared),
    )
    .await;
    conforms("PersonActivityPage", &for_bob.2);
    assert_eq!(for_bob.2["items"][0]["type"], "person_merged");
    assert!(for_bob.2["items"][0].get("merged_from").is_none());
    assert!(!for_bob.2.to_string().contains(&s.private));
    Ok(())
}

#[tokio::test]
async fn parameters_are_validated() -> Result<()> {
    let s = scene().await?;
    let t = Some(s.alice_phone.as_str());
    let base = format!("people/{}/activity", s.shared);
    for query in [
        "limit=many",
        "limit=-1",
        "limit=70000",
        "unknown=1",
        "archived=true",
    ] {
        assert_error(
            &get(&s, t, &format!("{base}?{query}")).await,
            StatusCode::BAD_REQUEST,
            "malformed_request",
        );
    }
    let foreign =
        s.w.store
            .person_activity(&s.alice, &s.private, None, 1)
            .await?
            .items[0]
            .id
            .clone();
    for query in [
        "limit=0".to_owned(),
        "limit=101".to_owned(),
        "after=not-a-uuid".to_owned(),
        format!("after={}", id()),
        format!("after={foreign}"),
    ] {
        assert_error(
            &get(&s, t, &format!("{base}?{query}")).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        );
    }
    // A missing and a foreign cursor are byte-identical apart from the request id.
    let missing = get(&s, t, &format!("{base}?after={}", id())).await;
    let other = get(&s, t, &format!("{base}?after={foreign}")).await;
    assert_eq!(strip(missing.2), strip(other.2));
    assert_error(
        &get(&s, t, "people/not-a-uuid/activity").await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_value",
    );
    for query in ["", "?limit=1", "?limit=100"] {
        let reply = get(&s, t, &format!("{base}{query}")).await;
        assert_eq!(reply.0, StatusCode::OK, "{query}: {}", reply.2);
        conforms("PersonActivityPage", &reply.2);
    }
    Ok(())
}

#[tokio::test]
async fn every_body_conforms_and_hidden_fields_never_appear() -> Result<()> {
    let s = scene().await?;
    edit_over_http(&s, &s.bob_token, &s.field, "Birthday!", 0).await;
    edit_over_http(&s, &s.alice_phone, &s.private_field, "Medical", 0).await;
    let base = format!("people/{}/activity", s.shared);
    let owner = get(&s, Some(&s.alice_phone), &base).await;
    conforms("PersonActivityPage", &owner.2);
    let types: Vec<&str> = owner.2["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "field_edited",
            "field_edited",
            "field_created",
            "field_created",
            "person_created"
        ]
    );
    assert_eq!(
        owner.2["items"][1]["actor"],
        json!({"kind": "someone_else"}),
        "Bob is a grantee"
    );
    assert_eq!(owner.2["items"][4]["actor"], json!({"kind": "you"}));
    assert!(owner.2["coverage_started_at"].as_i64().unwrap() > 0);

    let reader = get(&s, Some(&s.bob_token), &base).await;
    conforms("PersonActivityPage", &reader.2);
    let items = reader.2["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["actor"], json!({"kind": "you"}));
    assert_eq!(items[0]["changed"], json!(["label", "value"]));
    assert_eq!(items[0]["target"]["label"], "Birthday!");
    assert_eq!(items[2]["actor"]["kind"], "account");
    assert_eq!(items[2]["actor"]["username"], "device-alice");
    let text = reader.2.to_string();
    for hidden in [
        s.private_field.as_str(),
        "Medical",
        "secret",
        "1 May",
        "2 May",
    ] {
        assert!(!text.contains(hidden), "{hidden} leaked");
    }
    // Pages of one walk the reader can follow to the end.
    let first = get(&s, Some(&s.bob_token), &format!("{base}?limit=2")).await;
    conforms("PersonActivityPage", &first.2);
    let after = first.2["next_after"].as_str().unwrap().to_owned();
    assert_eq!(after, first.2["items"][1]["id"]);
    let second = get(
        &s,
        Some(&s.bob_token),
        &format!("{base}?limit=2&after={after}"),
    )
    .await;
    assert_eq!(second.2["next_after"], Value::Null);
    assert_eq!(second.2["items"].as_array().unwrap().len(), 1);
    Ok(())
}

#[tokio::test]
async fn devices_of_one_account_read_one_timeline_and_a_retry_adds_nothing() -> Result<()> {
    let s = scene().await?;
    let base = format!("people/{}/activity", s.shared);
    // A write on one device, retried twice with the same key, is one event on every device.
    edit_over_http(&s, &s.alice_phone, &s.field, "Birthday (edited)", 2).await;
    let phone = get(&s, Some(&s.alice_phone), &base).await;
    let laptop = get(&s, Some(&s.alice_laptop), &base).await;
    assert_eq!(phone.0, StatusCode::OK);
    assert_eq!(phone.2, laptop.2, "identical pages on both devices");
    let edits = phone.2["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "field_edited")
        .count();
    assert_eq!(edits, 1);
    // People commands over HTTP retried with the same key: one event.
    let key = id();
    let body =
        json!({"command": {"kind": "reference_account", "account_id": s.bob, "person_id": id()}});
    let first = command_request(
        &s.w.app,
        "people-commands",
        Some(&s.alice_laptop),
        Some(&key),
        Some(body.clone()),
    )
    .await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    let again = command_request(
        &s.w.app,
        "people-commands",
        Some(&s.alice_laptop),
        Some(&key),
        Some(body),
    )
    .await;
    assert_eq!(again, first);
    let person = first.1["person_id"].as_str().unwrap();
    let bobs = get(&s, Some(&s.bob_token), &format!("people/{person}/activity")).await;
    conforms("PersonActivityPage", &bobs.2);
    let types: Vec<&str> = bobs.2["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["person_linked", "person_created"]);
    // Alice referenced Bob: Bob owns the person and is linked; Alice is someone else to Bob.
    assert_eq!(bobs.2["items"][0]["account"], json!({"kind": "you"}));
    assert_eq!(bobs.2["items"][0]["actor"], json!({"kind": "someone_else"}));
    Ok(())
}

/// One activity read of `s.shared` with `after` as the cursor.
async fn cursor(s: &Scene, token: &str, after: &str) -> Reply {
    get(
        s,
        Some(token),
        &format!("people/{}/activity?after={after}", s.shared),
    )
    .await
}

/// The owner replaces the shared field's policy.
async fn replace_field_policy(s: &Scene, policy: Policy) -> Result<i64> {
    let version = s.w.store.resource_policy(&s.alice, &s.field).await?.version;
    s.w.store
        .management(
            &s.alice,
            &id(),
            &[atlas_core::households::ManagementCommand::ReplacePolicy {
                id: s.field.clone(),
                expected_version: version,
                policy,
            }],
            now(),
        )
        .await
}

/// R1 (review 2 November 2026): over HTTP, an event the reader cannot currently see is the same
/// `422 invalid_value` as a missing cursor, byte for byte apart from the request id, whether it
/// was never visible or became hidden after the reader was given it.
#[tokio::test]
async fn a_hidden_event_is_the_same_invalid_cursor_as_a_missing_one() -> Result<()> {
    let s = scene().await?;
    let base = format!("people/{}/activity", s.shared);
    let owner =
        s.w.store
            .person_activity(&s.alice, &s.shared, None, 100)
            .await?;
    let event_of = |target: &str| {
        owner
            .items
            .iter()
            .find(|e| e.target.id == target)
            .map(|e| e.id.clone())
            .expect("an event about the target")
    };
    let (field_event, private_event) = (event_of(&s.field), event_of(&s.private_field));
    let missing = cursor(&s, &s.bob_token, &id()).await;
    assert_error(&missing, StatusCode::UNPROCESSABLE_ENTITY, "invalid_value");
    let missing = strip(missing.2);
    // Positive control: Bob was given the field's event and can page after it.
    let visible = cursor(&s, &s.bob_token, &field_event).await;
    assert_eq!(visible.0, StatusCode::OK, "{}", visible.2);
    conforms("PersonActivityPage", &visible.2);
    // An event Bob never could see.
    let never = cursor(&s, &s.bob_token, &private_event).await;
    assert_error(&never, StatusCode::UNPROCESSABLE_ENTITY, "invalid_value");
    assert_eq!(strip(never.2), missing);
    // The owner can use both.
    for event in [&field_event, &private_event] {
        assert_eq!(cursor(&s, &s.alice_phone, event).await.0, StatusCode::OK);
    }
    // Revoking only the field: Bob keeps the person, and the event Bob was given is now invalid.
    replace_field_policy(&s, Policy::default()).await?;
    let page = get(&s, Some(&s.bob_token), &base).await;
    assert_eq!(page.0, StatusCode::OK);
    assert_eq!(page.2["items"].as_array().unwrap().len(), 1, "{}", page.2);
    let revoked = cursor(&s, &s.bob_token, &field_event).await;
    assert_error(&revoked, StatusCode::UNPROCESSABLE_ENTITY, "invalid_value");
    assert_eq!(strip(revoked.2), missing);
    // Regaining it restores the cursor.
    replace_field_policy(
        &s,
        Policy {
            grants: vec![PrincipalGrant::Account {
                id: s.bob.clone(),
                edit: true,
            }],
            exclude_accounts: vec![],
        },
    )
    .await?;
    let regained = cursor(&s, &s.bob_token, &field_event).await;
    assert_eq!(regained.0, StatusCode::OK);
    assert_eq!(strip(regained.2), strip(visible.2));
    Ok(())
}
