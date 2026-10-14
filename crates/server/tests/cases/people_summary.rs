//! `GET /people/summaries`, `GET /people/{id}/summary` and `GET /people/{id}/field-audiences`
//! (BE-PEOPLE-SUMMARY) through the real router: authentication, headers, routing, parameter
//! validation, indistinguishable `not_found`, no owner-only `403`, alias resolution, the unchanged
//! `listPeople`/`getPerson` bodies, and every `200` body validated against `api/openapi.json`. The
//! disclosure and filter semantics themselves are `atlas-core`'s `people/summary` cases.
use anyhow::Result;
use atlas_core::{
    Command,
    households::ManagementCommand as M,
    people::PeopleCommand,
    policy::{Policy, PrincipalGrant},
};
use atlas_server::{hash_password, now};
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support::http::{command_request, request, session_request};
use crate::support::retirement::{
    ORIGIN, PASSWORD, Reply, World, attempt, bearer, id, login, world,
};

fn contract() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../api/openapi.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

const UUID_PATTERN: &str = "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$";

/// A JSON Schema check for the keywords these responses use. An unsupported keyword that applies
/// to a value fails loudly rather than passing silently.
fn check(contract: &Value, schema: &Value, value: &Value, at: &str) -> Result<(), String> {
    let fail = |why: String| Err(format!("{at}: {why} in {value}"));
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let name = reference.trim_start_matches("#/components/schemas/");
        return check(
            contract,
            &contract["components"]["schemas"][name],
            value,
            at,
        );
    }
    if let Some(kind) = schema.get("type") {
        let kinds: Vec<&str> = match kind {
            Value::String(k) => vec![k.as_str()],
            Value::Array(ks) => ks.iter().filter_map(Value::as_str).collect(),
            _ => return fail("bad type".into()),
        };
        let matches = kinds.iter().any(|k| match *k {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => false,
        });
        if !matches {
            return fail(format!("not {kinds:?}"));
        }
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.contains(value)
    {
        return fail(format!("not one of {options:?}"));
    }
    if let Some(constant) = schema.get("const")
        && constant != value
    {
        return fail(format!("not {constant}"));
    }
    if let Some(s) = value.as_str() {
        if schema.get("format").and_then(Value::as_str) == Some("uuid")
            && uuid::Uuid::parse_str(s).map(|u| u.to_string()) != Ok(s.to_owned())
        {
            return fail("not a canonical uuid".into());
        }
        if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
            assert_eq!(pattern, UUID_PATTERN, "unsupported pattern at {at}");
        }
        let length = s.chars().count() as u64;
        if schema
            .get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|m| length < m)
            || schema
                .get("maxLength")
                .and_then(Value::as_u64)
                .is_some_and(|m| length > m)
        {
            return fail("length out of range".into());
        }
    }
    if let Some(n) = value.as_i64()
        && (schema
            .get("minimum")
            .and_then(Value::as_i64)
            .is_some_and(|m| n < m)
            || schema
                .get("maximum")
                .and_then(Value::as_i64)
                .is_some_and(|m| n > m))
    {
        return fail("out of range".into());
    }
    if let Some(items) = value.as_array() {
        if schema
            .get("maxItems")
            .and_then(Value::as_u64)
            .is_some_and(|m| items.len() as u64 > m)
            || schema
                .get("minItems")
                .and_then(Value::as_u64)
                .is_some_and(|m| (items.len() as u64) < m)
        {
            return fail("item count out of range".into());
        }
        if let Some(item) = schema.get("items") {
            for (i, v) in items.iter().enumerate() {
                check(contract, item, v, &format!("{at}[{i}]"))?;
            }
        }
    }
    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    return fail(format!("missing {name}"));
                }
            }
        }
        for (name, member) in object {
            match properties.and_then(|p| p.get(name)) {
                Some(s) => check(contract, s, member, &format!("{at}.{name}"))?,
                None if schema.get("additionalProperties") == Some(&Value::Bool(false)) => {
                    return fail(format!("unexpected member {name}"));
                }
                None => {}
            }
        }
    }
    for every in schema
        .get("allOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        check(contract, every, value, at)?;
    }
    if let Some(options) = schema.get("anyOf").and_then(Value::as_array)
        && !options
            .iter()
            .any(|o| check(contract, o, value, at).is_ok())
    {
        return fail("matches no anyOf".into());
    }
    if let Some(options) = schema.get("oneOf").and_then(Value::as_array)
        && options
            .iter()
            .filter(|o| check(contract, o, value, at).is_ok())
            .count()
            != 1
    {
        return fail("matches not exactly one oneOf".into());
    }
    if let Some(condition) = schema.get("if") {
        let branch = if check(contract, condition, value, at).is_ok() {
            schema.get("then")
        } else {
            schema.get("else")
        };
        if let Some(branch) = branch {
            check(contract, branch, value, at)?;
        }
    }
    Ok(())
}

fn conforms(schema: &str, body: &Value) {
    let contract = contract();
    if let Err(why) = check(
        &contract,
        &json!({"$ref": format!("#/components/schemas/{schema}")}),
        body,
        schema,
    ) {
        panic!("{why}");
    }
}

/// The validator itself rejects what the contract forbids (negative controls).
#[test]
fn the_contract_check_rejects_extra_missing_and_mistyped_members() {
    let contract = contract();
    let schema = json!({"$ref": "#/components/schemas/HouseholdAudience"});
    let good = json!({"id": id(), "can_edit": true, "you_are_member": false, "name": null, "version": null, "member_count": null});
    assert!(check(&contract, &schema, &good, "h").is_ok());
    let mut extra = good.clone();
    extra["members_hidden"] = json!(2);
    let mut missing = good.clone();
    missing.as_object_mut().unwrap().remove("member_count");
    let mut mistyped = good.clone();
    mistyped["can_edit"] = json!("yes");
    let mut bad_id = good.clone();
    bad_id["id"] = json!("not-a-uuid");
    for bad in [extra, missing, mistyped, bad_id] {
        assert!(check(&contract, &schema, &bad, "h").is_err(), "{bad}");
    }
    let partial = json!({"disclosure": "hidden", "in_home": false, "households": [], "your_direct_grant": null, "direct_grant_count": null, "excluded_count": null});
    assert!(
        check(
            &contract,
            &json!({"$ref": "#/components/schemas/AudienceSummary"}),
            &partial,
            "a"
        )
        .is_err()
    );
}

struct Scene {
    w: World,
    alice: String,
    bob: String,
    home: String,
    /// Alice's, shared with Home (bob is a member).
    shared: String,
    /// Alice's, shared with nobody.
    private: String,
    field: String,
    alice_token: String,
    bob_token: String,
    carol_token: String,
}

async fn scene() -> Result<Scene> {
    let w = world(false).await?;
    let alice = w.alice.clone();
    let bob: String = sqlx::query_scalar("SELECT id FROM accounts WHERE username='device-bob'")
        .fetch_one(&w.store.pool)
        .await?;
    let carol = id();
    w.store
        .add_account(
            &carol,
            "summary-carol",
            &hash_password(PASSWORD.into()).await?,
        )
        .await?;
    let (home, invitation, shared, private, field) = (id(), id(), id(), id(), id());
    w.store
        .management(
            &alice,
            &id(),
            &[M::CreateHousehold {
                id: home.clone(),
                name: "Atlas Home".into(),
            }],
            now(),
        )
        .await?;
    w.store
        .management(
            &alice,
            &id(),
            &[M::InviteToHousehold {
                id: invitation.clone(),
                household_id: home.clone(),
                recipient_id: bob.clone(),
                expected_version: 1,
            }],
            now(),
        )
        .await?;
    w.store
        .management(
            &bob,
            &id(),
            &[M::RespondToHouseholdInvitation {
                id: invitation,
                expected_version: 1,
                accept: true,
            }],
            now(),
        )
        .await?;
    let household = Policy {
        grants: vec![PrincipalGrant::Household {
            id: home.clone(),
            edit: false,
        }],
        exclude_accounts: vec![],
    };
    w.store
        .apply(
            &alice,
            &id(),
            &[
                Command::CreatePerson {
                    id: shared.clone(),
                    name: "Mo".into(),
                    initial_policy: Some(household.clone()),
                },
                Command::CreateField {
                    id: field.clone(),
                    person_id: shared.clone(),
                    label: "Birthday".into(),
                    value: "1 May".into(),
                    initial_policy: Some(household),
                },
                Command::CreatePerson {
                    id: private.clone(),
                    name: "Private".into(),
                    initial_policy: Some(Policy::default()),
                },
            ],
        )
        .await?;
    let alice_token = login(&w.app, "device-alice", "phone").await;
    let bob_token = login(&w.app, "device-bob", "laptop").await;
    let carol_token = login(&w.app, "summary-carol", "tablet").await;
    Ok(Scene {
        w,
        alice,
        bob,
        home,
        shared,
        private,
        field,
        alice_token,
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

fn routes(s: &Scene) -> [String; 3] {
    [
        "people/summaries".to_owned(),
        format!("people/{}/summary", s.shared),
        format!("people/{}/field-audiences", s.shared),
    ]
}

#[tokio::test]
async fn the_routes_are_authenticated_private_and_never_set_a_cookie() -> Result<()> {
    let s = scene().await?;
    for path in routes(&s) {
        assert_error(
            &get(&s, None, &path).await,
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
        );
        assert_error(
            &get(&s, Some(&"0".repeat(64)), &path).await,
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
        );
        let reply = get(&s, Some(&s.bob_token), &path).await;
        assert_eq!(reply.0, StatusCode::OK, "{path}: {}", reply.2);
        assert_eq!(reply.1["cache-control"], "private, no-store");
        assert!(!reply.1.contains_key("set-cookie"));
    }
    // A revoked session is unauthenticated on every route.
    let revoked = login(&s.w.app, "device-bob", "tablet").await;
    let (status, _, _) = request(
        &s.w.app,
        "DELETE",
        "sessions/current",
        &[("authorization", &bearer(&revoked))],
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for path in routes(&s) {
        assert_error(
            &get(&s, Some(&revoked), &path).await,
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
        );
    }
    Ok(())
}

#[tokio::test]
async fn every_body_conforms_to_the_contract_and_the_static_route_wins() -> Result<()> {
    let s = scene().await?;
    for (token, scope) in [
        (&s.alice_token, "all"),
        (&s.bob_token, "all"),
        (&s.bob_token, "in_home"),
        (&s.bob_token, "shared_with_me"),
        (&s.bob_token, "not_linked"),
        (&s.carol_token, "in_home"),
    ] {
        let reply = get(&s, Some(token), &format!("people/summaries?scope={scope}")).await;
        assert_eq!(reply.0, StatusCode::OK, "{}", reply.2);
        conforms("PersonSummaryPage", &reply.2);
    }
    // `/people/summaries` is the list, not `getPerson` with the id "summaries" (a 422).
    let list = get(&s, Some(&s.bob_token), "people/summaries").await;
    assert!(list.2["items"].is_array());
    let bob_row = list.2["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == s.shared)
        .unwrap();
    assert_eq!(bob_row["audience"]["disclosure"], "partial");
    assert_eq!(bob_row["audience"]["in_home"], true);
    assert_eq!(list.2["home"]["id"], s.home);
    let carol = get(&s, Some(&s.carol_token), "people/summaries?scope=in_home").await;
    assert_eq!(
        carol.2,
        json!({"items": [], "next_after": null, "home": null, "preferences_version": 1})
    );

    for token in [&s.alice_token, &s.bob_token] {
        let detail = get(&s, Some(token), &format!("people/{}/summary", s.shared)).await;
        assert_eq!(detail.0, StatusCode::OK, "{}", detail.2);
        conforms("PersonSummaryDetail", &detail.2);
        let fields = get(
            &s,
            Some(token),
            &format!("people/{}/field-audiences", s.shared),
        )
        .await;
        assert_eq!(fields.0, StatusCode::OK, "{}", fields.2);
        conforms("FieldAudiencePage", &fields.2);
        assert_eq!(fields.2["items"][0]["id"], s.field);
    }
    let owner = get(
        &s,
        Some(&s.alice_token),
        &format!("people/{}/summary", s.shared),
    )
    .await;
    assert_eq!(owner.2["shared_with"]["disclosure"], "complete");
    assert_eq!(
        owner.2["shared_with"]["households"][0]["members"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let member = get(
        &s,
        Some(&s.bob_token),
        &format!("people/{}/summary", s.shared),
    )
    .await;
    assert_eq!(member.2["shared_with"]["disclosure"], "partial");
    assert_eq!(
        member.2["shared_with"]["households"][0]["members"],
        Value::Null
    );
    assert_eq!(member.2["shared_with"]["excluded_accounts"], Value::Null);
    Ok(())
}

#[tokio::test]
async fn parameters_are_validated() -> Result<()> {
    let s = scene().await?;
    let t = Some(s.alice_token.as_str());
    for query in [
        "scope=everyone",
        "scope=",
        "archived=maybe",
        "limit=many",
        "limit=-1",
        "unknown=1",
    ] {
        assert_error(
            &get(&s, t, &format!("people/summaries?{query}")).await,
            StatusCode::BAD_REQUEST,
            "malformed_request",
        );
    }
    for query in ["limit=0", "limit=101", "after=not-a-uuid"] {
        assert_error(
            &get(&s, t, &format!("people/summaries?{query}")).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        );
        assert_error(
            &get(
                &s,
                t,
                &format!("people/{}/field-audiences?{query}", s.shared),
            )
            .await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        );
    }
    assert_error(
        &get(
            &s,
            t,
            &format!("people/{}/field-audiences?scope=all", s.shared),
        )
        .await,
        StatusCode::BAD_REQUEST,
        "malformed_request",
    );
    let full = get(&s, t, "people/summaries?limit=100&archived=false&scope=all").await;
    assert_eq!(full.0, StatusCode::OK);
    // A malformed id is invalid exactly as on `getPerson`.
    for path in [
        "people/not-a-uuid/summary",
        "people/not-a-uuid/field-audiences",
        "people/not-a-uuid",
    ] {
        assert_error(
            &get(&s, t, path).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
        );
    }
    Ok(())
}

#[tokio::test]
async fn hidden_and_missing_people_are_the_same_not_found_and_no_route_answers_403() -> Result<()> {
    let s = scene().await?;
    let strip = |mut body: Value| {
        body.as_object_mut().unwrap().remove("request_id");
        body
    };
    for suffix in ["summary", "field-audiences"] {
        let hidden = get(
            &s,
            Some(&s.bob_token),
            &format!("people/{}/{suffix}", s.private),
        )
        .await;
        let missing = get(&s, Some(&s.bob_token), &format!("people/{}/{suffix}", id())).await;
        assert_error(&hidden, StatusCode::NOT_FOUND, "not_found");
        assert_eq!(
            (hidden.0, strip(hidden.2.clone())),
            (missing.0, strip(missing.2))
        );
    }
    // A reader of a person the reader does not own: the owner-only policy read refuses, the
    // summaries do not.
    let policy = get(&s, Some(&s.bob_token), &format!("policies/{}", s.shared)).await;
    assert_error(&policy, StatusCode::FORBIDDEN, "forbidden");
    for path in routes(&s) {
        assert_eq!(get(&s, Some(&s.bob_token), &path).await.0, StatusCode::OK);
    }
    Ok(())
}

#[tokio::test]
async fn a_merged_id_resolves_to_the_canonical_person() -> Result<()> {
    let s = scene().await?;
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
    let detail = get(
        &s,
        Some(&s.alice_token),
        &format!("people/{}/summary", s.private),
    )
    .await;
    assert_eq!(detail.0, StatusCode::OK, "{}", detail.2);
    assert_eq!(detail.2["summary"]["id"], s.shared);
    let fields = get(
        &s,
        Some(&s.alice_token),
        &format!("people/{}/field-audiences", s.private),
    )
    .await;
    assert_eq!(fields.2["items"][0]["id"], s.field);
    let archived = get(&s, Some(&s.alice_token), "people/summaries?archived=true").await;
    conforms("PersonSummaryPage", &archived.2);
    assert_eq!(archived.2["items"][0]["id"], s.private);
    Ok(())
}

#[tokio::test]
async fn list_people_and_get_person_are_unchanged() -> Result<()> {
    let s = scene().await?;
    for (token, account) in [(&s.alice_token, &s.alice), (&s.bob_token, &s.bob)] {
        let list = get(&s, Some(token), "people").await;
        assert_eq!(list.0, StatusCode::OK);
        conforms("ResourcePage", &list.2);
        let expected =
            s.w.store
                .resources(account, "person", None, false, None, 50)
                .await?;
        assert_eq!(
            list.2,
            json!({"items": expected, "next_after": null}),
            "the existing list has exactly its old members"
        );
        let detail = get(&s, Some(token), &format!("people/{}", s.shared)).await;
        assert_eq!(detail.0, StatusCode::OK);
        conforms("PersonDetail", &detail.2);
        assert_eq!(
            detail.2,
            serde_json::to_value(s.w.store.person_detail(account, &s.shared).await?)?
        );
        assert_eq!(
            detail
                .2
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["linked_account_id", "person"]
        );
    }
    Ok(())
}

/// What the first device can read without the summaries: its households, `/me` and the person.
async fn synced_reads(s: &Scene, token: &str) -> Vec<Value> {
    let mut bodies = Vec::new();
    for path in [
        "households".to_owned(),
        "me".to_owned(),
        format!("people/{}", s.shared),
    ] {
        bodies.push(get(s, Some(token), &path).await.2);
    }
    bodies
}

/// Revision 1, R3: a primary household change made through another device's session publishes no
/// resource change, so nothing the first device has synced changes; every summary response it
/// reads afterwards carries the higher `preferences_version` (equal to `GET /defaults`) and the new
/// `home`, which is how a client recognises its held summaries as out of date.
#[tokio::test]
async fn a_primary_household_change_on_another_device_is_visible_in_every_summary() -> Result<()> {
    let s = scene().await?;
    let second = id();
    s.w.store
        .management(
            &s.bob,
            &id(),
            &[M::CreateHousehold {
                id: second.clone(),
                name: "Second".into(),
            }],
            now(),
        )
        .await?;
    let first = Some(s.bob_token.as_str());
    let summary_paths = routes(&s);
    let mut before = Vec::new();
    for path in &summary_paths {
        let reply = get(&s, first, path).await;
        assert_eq!(reply.2["home"]["id"], s.home, "{path}");
        before.push(reply.2);
    }
    let defaults = get(&s, first, "defaults").await;
    let version = defaults.2["preferences_version"].as_i64().unwrap();
    assert!(before.iter().all(|b| b["preferences_version"] == version));
    let synced = synced_reads(&s, &s.bob_token).await;

    let other = login(&s.w.app, "device-bob", "tablet").await;
    let (status, body) = command_request(
        &s.w.app,
        "management-commands",
        Some(&other),
        Some(&id()),
        Some(json!({"commands": [{"kind": "set_primary_household", "household_id": second, "expected_version": version}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(
        synced_reads(&s, &s.bob_token).await,
        synced,
        "nothing synced or listed changes"
    );
    let after = get(&s, first, "defaults").await.2["preferences_version"]
        .as_i64()
        .unwrap();
    assert!(after > version);
    for (path, schema) in summary_paths.iter().zip([
        "PersonSummaryPage",
        "PersonSummaryDetail",
        "FieldAudiencePage",
    ]) {
        let reply = get(&s, first, path).await;
        conforms(schema, &reply.2);
        assert_eq!(reply.2["preferences_version"], after, "{path}");
        assert_eq!(reply.2["home"]["id"], second, "{path}");
    }
    let rail = get(&s, first, &format!("people/{}/summary", s.shared)).await;
    assert_eq!(rail.2["summary"]["audience"]["in_home"], false);
    let fields = get(&s, first, &format!("people/{}/field-audiences", s.shared)).await;
    assert_eq!(fields.2["items"][0]["audience"]["in_home"], false);
    assert_eq!(
        fields.2["items"][0]["effective"],
        json!({"disclosure": "partial", "account_count": null, "limited_count": null})
    );
    Ok(())
}

/// Revision 2, R4: owning a person and its field does not authorise reading the membership of a
/// household they are shared with once the owner has left it. Through the real router, while bob
/// is a member a native registration into Home changes his exact count (positive control); after
/// alice removes him his effective access is `partial`, and native and browser registrations into
/// Home change none of his responses. Registration is the onboarding membership writer.
#[tokio::test]
async fn a_removed_owner_learns_nothing_from_registrations_into_its_old_household() -> Result<()> {
    let w = world(true).await?;
    let alice = w.alice.clone();
    let bob: String = sqlx::query_scalar("SELECT id FROM accounts WHERE username='device-bob'")
        .fetch_one(&w.store.pool)
        .await?;
    let (home, invitation, person, field) = (id(), id(), id(), id());
    w.store
        .management(
            &alice,
            &id(),
            &[M::CreateHousehold {
                id: home.clone(),
                name: "Atlas Home".into(),
            }],
            now(),
        )
        .await?;
    w.store
        .management(
            &alice,
            &id(),
            &[M::InviteToHousehold {
                id: invitation.clone(),
                household_id: home.clone(),
                recipient_id: bob.clone(),
                expected_version: 1,
            }],
            now(),
        )
        .await?;
    w.store
        .management(
            &bob,
            &id(),
            &[M::RespondToHouseholdInvitation {
                id: invitation,
                expected_version: 1,
                accept: true,
            }],
            now(),
        )
        .await?;
    let household = Policy {
        grants: vec![PrincipalGrant::Household {
            id: home.clone(),
            edit: false,
        }],
        exclude_accounts: vec![],
    };
    w.store
        .apply(
            &bob,
            &id(),
            &[
                Command::CreatePerson {
                    id: person.clone(),
                    name: "Mo".into(),
                    initial_policy: Some(household.clone()),
                },
                Command::CreateField {
                    id: field.clone(),
                    person_id: person.clone(),
                    label: "Birthday".into(),
                    value: "1 May".into(),
                    initial_policy: Some(household),
                },
            ],
        )
        .await?;
    let alice_token = login(&w.app, "device-alice", "phone").await;
    let bob_token = login(&w.app, "device-bob", "laptop").await;
    let audiences = format!("people/{person}/field-audiences");
    let read = |path: String| {
        let (app, auth) = (w.app.clone(), bearer(&bob_token));
        async move {
            let reply = request(&app, "GET", &path, &[("authorization", &auth)], Value::Null).await;
            assert_eq!(reply.0, StatusCode::OK, "{}", reply.2);
            reply.2
        }
    };
    let members = || {
        let (app, auth, home) = (w.app.clone(), bearer(&alice_token), home.clone());
        async move {
            let reply = request(
                &app,
                "GET",
                "households",
                &[("authorization", &auth)],
                Value::Null,
            )
            .await;
            let home = reply
                .2
                .as_array()
                .unwrap()
                .iter()
                .find(|h| h["id"] == home)
                .unwrap()
                .clone();
            (
                home["members"].as_array().unwrap().len(),
                home["version"].as_i64().unwrap(),
            )
        }
    };
    let invite = || {
        let (app, token, home) = (w.app.clone(), alice_token.clone(), home.clone());
        async move {
            let (status, body) = session_request(
                &app,
                "POST",
                "account-invitations",
                Some(&token),
                json!({"household_id": home}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body["token"].as_str().unwrap().to_owned()
        }
    };
    let register = |token: String, username: &'static str| {
        let app = w.app.clone();
        async move {
            let (status, body) = session_request(
                &app,
                "POST",
                "registration",
                None,
                json!({"token": token, "username": username, "password": "onboarding-password-123", "device_id": "phone"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    };
    let complete =
        |seen: i64| json!({"disclosure": "complete", "account_count": seen, "limited_count": 0});
    let body = read(audiences.clone()).await;
    conforms("FieldAudiencePage", &body);
    assert_eq!(body["items"][0]["effective"], complete(1));

    // Positive control: bob is a member, so a registration into Home is his to see.
    register(invite().await, "summary-erin").await;
    assert_eq!(members().await.0, 3);
    assert_eq!(
        read(audiences.clone()).await["items"][0]["effective"],
        complete(2)
    );

    let (_, version) = members().await;
    let (status, removed) = command_request(
        &w.app,
        "management-commands",
        Some(&alice_token),
        Some(&id()),
        Some(json!({"commands": [{"kind": "remove_household_member", "household_id": home, "account_id": bob, "expected_version": version}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    let before = read(audiences.clone()).await;
    conforms("FieldAudiencePage", &before);
    assert_eq!(
        before["items"][0]["effective"],
        json!({"disclosure": "partial", "account_count": null, "limited_count": null})
    );
    assert_eq!(before["items"][0]["audience"]["disclosure"], "complete");
    assert_eq!(
        before["items"][0]["audience"]["households"][0]["member_count"],
        Value::Null
    );
    let rail = read(format!("people/{person}/summary")).await;

    // Native registration into Home after bob's removal.
    register(invite().await, "summary-frank").await;
    assert_eq!(members().await.0, 3);
    assert_eq!(read(audiences.clone()).await, before);
    // Browser registration into Home: the same onboarding writer through its other entry point.
    let (_verifier, challenge) = attempt();
    let reply = request(
        &w.app,
        "POST",
        "browser-registration",
        &[("origin", ORIGIN)],
        json!({"token": invite().await, "username": "summary-grace", "password": "onboarding-password-123", "device_id": "phone", "attempt_challenge": challenge}),
    )
    .await;
    assert_eq!(reply.0, StatusCode::OK, "{}", reply.2);
    assert_eq!(members().await.0, 4);
    assert_eq!(read(audiences.clone()).await, before);
    assert_eq!(read(format!("people/{person}/summary")).await, rail);
    Ok(())
}
