//! Command batch-size schema/implementation reconciliation (BE-CH1).
//!
//! `api/openapi.json`'s `maxItems` and each domain's `commands.len() <= N` check must be
//! numerically identical across `ContentCommands`, `AccessCommands` and `ManagementCommands`;
//! the reconciled maximum must round-trip; one command over it must be rejected atomically,
//! with no partial write and no corruption of subsequent account state.
use anyhow::Result;
use atlas_core::Store;
use atlas_server::{App, hash_password};
use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::support::http::command_request as call;

fn id() -> String {
    Uuid::new_v4().to_string()
}

const CONTRACT: &str = include_str!("../../../../api/openapi.json");
const RESOURCES_SOURCE: &str = include_str!("../../../core/src/resources/mod.rs");
const HOUSEHOLDS_SOURCE: &str = include_str!("../../../core/src/households.rs");

/// `maxItems` for a named schema's `commands` array, read directly from the contract text
/// (same `include_str!` + `serde_json` pattern as `compatibility.rs`'s `CONTRACT` constant).
fn schema_max_items(schema: &str) -> i64 {
    let document: Value = serde_json::from_str(CONTRACT).expect("api/openapi.json is JSON");
    document["components"]["schemas"][schema]["properties"]["commands"]["maxItems"]
        .as_i64()
        .unwrap_or_else(|| panic!("{schema}.commands.maxItems is present and numeric"))
}

/// Every `commands.len() <= N` literal in `source`, in order — the same textual cross-check
/// technique as `compatibility.rs::error_codes_are_all_probed`, needing no shared
/// production-code symbol between the two independent call sites (plan.md §2.3).
fn implementation_max_items(source: &str) -> Vec<i64> {
    const NEEDLE: &str = "commands.len() <= ";
    source
        .match_indices(NEEDLE)
        .map(|(index, _)| {
            let rest = &source[index + NEEDLE.len()..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits
                .parse()
                .unwrap_or_else(|_| panic!("{NEEDLE} followed by a numeric literal"))
        })
        .collect()
}

/// Acceptance check 1: schema `maxItems` and implementation `<= N` checks numerically agree
/// across all three command batch types.
#[test]
fn schema_and_implementation_batch_limits_agree() {
    let resources_limits = implementation_max_items(RESOURCES_SOURCE);
    let households_limits = implementation_max_items(HOUSEHOLDS_SOURCE);
    assert_eq!(
        resources_limits.len(),
        1,
        "expected exactly one commands.len() <= N check in resources/mod.rs, found {resources_limits:?}"
    );
    assert_eq!(
        households_limits.len(),
        1,
        "expected exactly one commands.len() <= N check in households.rs, found {households_limits:?}"
    );
    let limits = [
        (
            "ContentCommands schema",
            schema_max_items("ContentCommands"),
        ),
        ("AccessCommands schema", schema_max_items("AccessCommands")),
        (
            "ManagementCommands schema",
            schema_max_items("ManagementCommands"),
        ),
        ("resources/mod.rs apply_guarded", resources_limits[0]),
        ("households.rs management", households_limits[0]),
    ];
    let reference = limits[0].1;
    for (name, value) in limits {
        assert_eq!(
            value, reference,
            "batch-size limits must be numerically identical; {name} is {value}, expected {reference}"
        );
    }
}

struct World {
    _dir: tempfile::TempDir,
    store: Store,
    app: Router,
    primary: String,
    grantee: String,
    token: String,
}

async fn world() -> Result<World> {
    let (dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let password = "commands-batch-password-1";
    let hash = hash_password(password.into()).await?;
    let (primary, grantee) = (id(), id());
    store
        .add_account(&primary, "commands-batch-primary", &hash)
        .await?;
    store
        .add_account(&grantee, "commands-batch-grantee", &hash)
        .await?;
    let app = App::new(store.clone()).await?.router();
    let (status, login) = call(
        &app,
        "sessions",
        None,
        None,
        Some(json!({"username":"commands-batch-primary","password":password,"device_id":"phone"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = login["access_token"].as_str().unwrap().to_owned();
    Ok(World {
        _dir: dir,
        store,
        app,
        primary,
        grantee,
        token,
    })
}

/// `count` freshly identified `create_person` commands with no `initial_policy`, so
/// `requires_online_sharing` is `false` for every command (the content-only gate,
/// `resource_routes.rs:17-23`).
fn content_batch(count: usize) -> Value {
    json!({
        "commands": (0..count)
            .map(|_| json!({"kind":"create_person","id":id(),"name":"Batch person"}))
            .collect::<Vec<_>>()
    })
}

/// `count` freshly identified `create_person` commands, each granting `grantee` non-empty
/// access, so `requires_online_sharing` is `true` for every command (the access-only gate).
fn access_batch(count: usize, grantee: &str) -> Value {
    json!({
        "commands": (0..count)
            .map(|_| json!({
                "kind":"create_person",
                "id":id(),
                "name":"Batch person",
                "initial_policy":{"grants":[{"kind":"account","id":grantee,"edit":false}],"exclude_accounts":[]}
            }))
            .collect::<Vec<_>>()
    })
}

/// `count` freshly identified `create_household` commands.
fn management_batch(count: usize) -> Value {
    json!({
        "commands": (0..count)
            .map(|_| json!({"kind":"create_household","id":id(),"name":"Batch household"}))
            .collect::<Vec<_>>()
    })
}

async fn person_count(store: &Store, owner: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM resources WHERE owner_id=$1 AND kind='person'")
            .bind(owner)
            .fetch_one(&store.pool)
            .await?,
    )
}

async fn household_count(store: &Store, account: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM household_memberships WHERE account_id=$1")
            .bind(account)
            .fetch_one(&store.pool)
            .await?,
    )
}

/// Acceptance check 2, one case per endpoint: a batch of exactly the reconciled maximum size
/// round-trips successfully and every command's resource is actually stored.
#[tokio::test]
async fn maximum_size_batch_round_trips_on_every_endpoint() -> Result<()> {
    let max = schema_max_items("ContentCommands") as usize;

    let content = world().await?;
    let reply = call(
        &content.app,
        "commands",
        Some(&content.token),
        Some(&id()),
        Some(content_batch(max)),
    )
    .await;
    assert_eq!(reply.0, StatusCode::OK, "{:?}", reply.1);
    assert_eq!(
        person_count(&content.store, &content.primary).await?,
        max as i64
    );

    let access = world().await?;
    let reply = call(
        &access.app,
        "access-commands",
        Some(&access.token),
        Some(&id()),
        Some(access_batch(max, &access.grantee)),
    )
    .await;
    assert_eq!(reply.0, StatusCode::OK, "{:?}", reply.1);
    assert_eq!(
        person_count(&access.store, &access.primary).await?,
        max as i64
    );

    let management = world().await?;
    let reply = call(
        &management.app,
        "management-commands",
        Some(&management.token),
        Some(&id()),
        Some(management_batch(max)),
    )
    .await;
    assert_eq!(reply.0, StatusCode::OK, "{:?}", reply.1);
    assert_eq!(
        household_count(&management.store, &management.primary).await?,
        max as i64
    );
    Ok(())
}

/// Acceptance check 3, one case per endpoint: a batch one over the maximum is rejected with
/// `invalid_value`, atomically (no partial write), and a subsequent batch at exactly the
/// maximum on the same account still succeeds (the rejection did not corrupt account state).
#[tokio::test]
async fn over_limit_batch_is_rejected_atomically_on_every_endpoint() -> Result<()> {
    let max = schema_max_items("ContentCommands") as usize;
    let over = max + 1;

    let content = world().await?;
    let rejected = call(
        &content.app,
        "commands",
        Some(&content.token),
        Some(&id()),
        Some(content_batch(over)),
    )
    .await;
    assert_eq!(
        rejected.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{:?}",
        rejected.1
    );
    assert_eq!(rejected.1["code"], "invalid_value");
    assert_eq!(
        person_count(&content.store, &content.primary).await?,
        0,
        "a rejected over-limit batch must not write any of its commands"
    );
    let recovered = call(
        &content.app,
        "commands",
        Some(&content.token),
        Some(&id()),
        Some(content_batch(max)),
    )
    .await;
    assert_eq!(recovered.0, StatusCode::OK, "{:?}", recovered.1);
    assert_eq!(
        person_count(&content.store, &content.primary).await?,
        max as i64,
        "the account must accept a maximum-size batch after an unrelated rejection"
    );

    let access = world().await?;
    let rejected = call(
        &access.app,
        "access-commands",
        Some(&access.token),
        Some(&id()),
        Some(access_batch(over, &access.grantee)),
    )
    .await;
    assert_eq!(
        rejected.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{:?}",
        rejected.1
    );
    assert_eq!(rejected.1["code"], "invalid_value");
    assert_eq!(person_count(&access.store, &access.primary).await?, 0);
    let recovered = call(
        &access.app,
        "access-commands",
        Some(&access.token),
        Some(&id()),
        Some(access_batch(max, &access.grantee)),
    )
    .await;
    assert_eq!(recovered.0, StatusCode::OK, "{:?}", recovered.1);
    assert_eq!(
        person_count(&access.store, &access.primary).await?,
        max as i64
    );

    let management = world().await?;
    let rejected = call(
        &management.app,
        "management-commands",
        Some(&management.token),
        Some(&id()),
        Some(management_batch(over)),
    )
    .await;
    assert_eq!(
        rejected.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{:?}",
        rejected.1
    );
    assert_eq!(rejected.1["code"], "invalid_value");
    assert_eq!(
        household_count(&management.store, &management.primary).await?,
        0
    );
    let recovered = call(
        &management.app,
        "management-commands",
        Some(&management.token),
        Some(&id()),
        Some(management_batch(max)),
    )
    .await;
    assert_eq!(recovered.0, StatusCode::OK, "{:?}", recovered.1);
    assert_eq!(
        household_count(&management.store, &management.primary).await?,
        max as i64
    );
    Ok(())
}

/// Edge cases carried over from existing behaviour, not new but must not regress: an empty
/// batch is still rejected, and a single-command batch still succeeds, on all three endpoints.
#[tokio::test]
async fn empty_and_single_command_batches_are_unaffected_on_every_endpoint() -> Result<()> {
    let content = world().await?;
    let empty = call(
        &content.app,
        "commands",
        Some(&content.token),
        Some(&id()),
        Some(json!({"commands": []})),
    )
    .await;
    assert_eq!(empty.0, StatusCode::UNPROCESSABLE_ENTITY, "{:?}", empty.1);
    assert_eq!(empty.1["code"], "invalid_value");
    let single = call(
        &content.app,
        "commands",
        Some(&content.token),
        Some(&id()),
        Some(content_batch(1)),
    )
    .await;
    assert_eq!(single.0, StatusCode::OK, "{:?}", single.1);
    assert_eq!(person_count(&content.store, &content.primary).await?, 1);

    let access = world().await?;
    let empty = call(
        &access.app,
        "access-commands",
        Some(&access.token),
        Some(&id()),
        Some(json!({"commands": []})),
    )
    .await;
    assert_eq!(empty.0, StatusCode::UNPROCESSABLE_ENTITY, "{:?}", empty.1);
    assert_eq!(empty.1["code"], "invalid_value");
    let single = call(
        &access.app,
        "access-commands",
        Some(&access.token),
        Some(&id()),
        Some(access_batch(1, &access.grantee)),
    )
    .await;
    assert_eq!(single.0, StatusCode::OK, "{:?}", single.1);
    assert_eq!(person_count(&access.store, &access.primary).await?, 1);

    let management = world().await?;
    let empty = call(
        &management.app,
        "management-commands",
        Some(&management.token),
        Some(&id()),
        Some(json!({"commands": []})),
    )
    .await;
    assert_eq!(empty.0, StatusCode::UNPROCESSABLE_ENTITY, "{:?}", empty.1);
    assert_eq!(empty.1["code"], "invalid_value");
    let single = call(
        &management.app,
        "management-commands",
        Some(&management.token),
        Some(&id()),
        Some(management_batch(1)),
    )
    .await;
    assert_eq!(single.0, StatusCode::OK, "{:?}", single.1);
    assert_eq!(
        household_count(&management.store, &management.primary).await?,
        1
    );
    Ok(())
}
