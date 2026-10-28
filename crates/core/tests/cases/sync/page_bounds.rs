//! BE-B1: `/sync` page counts agree with the contract, the `limit` ceiling round-trips without
//! truncation, and a rejected `limit` has no side effect.
use anyhow::Result;
use atlas_core::{Change, Command, Page, SYNC_LIMIT_DEFAULT, SYNC_LIMIT_MAX as CEILING, Store};
use std::collections::BTreeSet;

use crate::support::resources::{account, id, local};

/// The page bounds `api/openapi.json` states, read from the contract itself.
struct Contract {
    limit_min: u64,
    limit_max: u64,
    limit_default: u64,
    page_batches: usize,
    batch_changes: usize,
}

fn contract() -> Result<Contract> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../api/openapi.json");
    let document: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let limit = document["paths"]["/sync"]["get"]["parameters"]
        .as_array()
        .and_then(|p| p.iter().find(|p| p["name"] == "limit"))
        .map(|p| &p["schema"])
        .ok_or_else(|| anyhow::anyhow!("/sync has no limit parameter"))?;
    let schemas = &document["components"]["schemas"];
    let number = |value: &serde_json::Value, name: &str| {
        value
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("{name} is not a number"))
    };
    Ok(Contract {
        limit_min: number(&limit["minimum"], "limit.minimum")?,
        limit_max: number(&limit["maximum"], "limit.maximum")?,
        limit_default: number(&limit["default"], "limit.default")?,
        page_batches: number(
            &schemas["SyncPage"]["properties"]["batches"]["maxItems"],
            "SyncPage.batches.maxItems",
        )? as usize,
        batch_changes: number(
            &schemas["Batch"]["properties"]["changes"]["maxItems"],
            "Batch.changes.maxItems",
        )? as usize,
    })
}

/// Every returned page must be valid against the contract the server reports.
fn within_contract(contract: &Contract, page: &Page) {
    assert!(
        page.batches.len() <= contract.page_batches,
        "{} batches exceed SyncPage.batches.maxItems {}",
        page.batches.len(),
        contract.page_batches
    );
    for batch in &page.batches {
        assert!(
            batch.changes.len() <= contract.batch_changes,
            "{} changes exceed Batch.changes.maxItems {}",
            batch.changes.len(),
            contract.batch_changes
        );
    }
}

/// One person, then `count` single edits; each edit publishes exactly one one-change batch for
/// the owner. Returns the published revisions in order.
async fn publish(store: &Store, owner: &str, person: &str, count: i64) -> Result<Vec<i64>> {
    let mut revisions = Vec::new();
    for version in 1..=count {
        let revision = store
            .apply(
                owner,
                &id(),
                &[Command::Edit {
                    id: person.to_owned(),
                    expected_version: version,
                    expected_policy_version: Some(1),
                    label: format!("Version {version}"),
                    value: String::new(),
                }],
            )
            .await?;
        revisions.push(revision);
    }
    Ok(revisions)
}

async fn create_person(store: &Store, owner: &str) -> Result<String> {
    let person = id();
    store
        .apply(
            owner,
            &id(),
            &[Command::CreatePerson {
                id: person.clone(),
                name: "Paged".into(),
                initial_policy: None,
            }],
        )
        .await?;
    Ok(person)
}

/// `CEILING + 20` one-change batches pending for every returned device's delta cursor.
struct Pending {
    owner: String,
    person: String,
    cursors: Vec<String>,
    published: Vec<i64>,
}

async fn pending(store: &Store, devices: &[&str]) -> Result<Pending> {
    let owner = account(store).await?;
    let person = create_person(store, &owner).await?;
    let mut cursors = Vec::new();
    for device in devices {
        let page = store.sync(&owner, device, None, CEILING, 1000).await?;
        assert!(!page.has_more && page.next_cursor.starts_with("d1."));
        cursors.push(page.next_cursor);
    }
    let published = publish(store, &owner, &person, (CEILING + 20) as i64).await?;
    Ok(Pending {
        owner,
        person,
        cursors,
        published,
    })
}

/// PB1: a delta page at the ceiling, then the following page, without truncation.
async fn ceiling_round_trip(store: Store) -> Result<()> {
    let contract = contract()?;
    let seeded = pending(&store, &["ceiling"]).await?;
    let (owner, cursor) = (&seeded.owner, &seeded.cursors[0]);
    let first = store
        .sync(owner, "ceiling", Some(cursor), CEILING, 1001)
        .await?;
    assert_eq!(first.phase, "delta");
    assert_eq!(first.batches.len(), CEILING);
    assert!(first.has_more);
    let second = store
        .sync(owner, "ceiling", Some(&first.next_cursor), CEILING, 1002)
        .await?;
    assert_eq!(second.batches.len(), seeded.published.len() - CEILING);
    assert!(!second.has_more);
    let delivered: Vec<i64> = first
        .batches
        .iter()
        .chain(&second.batches)
        .map(|b| b.revision)
        .collect();
    assert_eq!(
        delivered, seeded.published,
        "no gap, duplicate or reordering"
    );
    for batch in first.batches.iter().chain(&second.batches) {
        assert!(
            matches!(&batch.changes[..], [Change::Upsert { resource }] if resource.id == seeded.person),
            "each publication batch arrives whole"
        );
    }
    // Checked last, so a page-bound failure shows the counts and order above already held.
    within_contract(&contract, &first);
    within_contract(&contract, &second);
    Ok(())
}

#[tokio::test]
async fn ceiling_delta_page_round_trips_without_truncation() -> Result<()> {
    let (_dir, store) = local().await?;
    ceiling_round_trip(store).await
}

/// PB2: every accepted `limit` returns exactly `min(limit, pending)` whole batches, in order.
async fn accepted_limits(store: Store) -> Result<()> {
    let contract = contract()?;
    let limits = [1, 80, 81, 199, 200];
    let devices: Vec<String> = limits.iter().map(|l| format!("limit-{l}")).collect();
    let names: Vec<&str> = devices.iter().map(String::as_str).collect();
    let seeded = pending(&store, &names).await?;
    let pending = seeded.published.len();
    for ((limit, device), cursor) in limits.iter().zip(&names).zip(&seeded.cursors) {
        let page = store
            .sync(&seeded.owner, device, Some(cursor), *limit, 1003)
            .await?;
        within_contract(&contract, &page);
        assert_eq!(page.batches.len(), (*limit).min(pending), "limit={limit}");
        assert_eq!(page.has_more, *limit < pending, "limit={limit}");
        let revisions: Vec<i64> = page.batches.iter().map(|b| b.revision).collect();
        assert_eq!(
            revisions,
            seeded.published[..revisions.len()],
            "limit={limit}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn every_accepted_limit_returns_exactly_that_many_batches() -> Result<()> {
    let (_dir, store) = local().await?;
    accepted_limits(store).await
}

async fn count(store: &Store, table: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
            .fetch_one(&store.pool)
            .await?,
    )
}

/// PB3: limits outside 1..=ceiling are rejected before any write; the bounds are accepted.
async fn rejected_limits(store: Store) -> Result<()> {
    let owner = account(&store).await?;
    create_person(&store, &owner).await?;
    let held = store.sync(&owner, "held", None, 1, 1000).await?;
    let tables = [
        "sync_devices",
        "sync_snapshots",
        "snapshot_items",
        "sync_cursors",
        "sync_deliveries",
    ];
    let mut before = Vec::new();
    for table in tables {
        before.push(count(&store, table).await?);
    }
    let last_seen = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT last_seen FROM sync_devices WHERE account_id=$1 AND device_id='held'",
        )
        .bind(&owner)
        .fetch_one(&store.pool)
        .await
    };
    let seen = last_seen().await?;
    for limit in [0, CEILING + 1, usize::MAX] {
        for (device, cursor) in [("fresh", None), ("held", Some(held.next_cursor.as_str()))] {
            let error = store
                .sync(&owner, device, cursor, limit, 5000)
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), "invalid_value", "limit={limit}");
            // Checked after every request, so the first value that writes is the one reported.
            for (table, before) in tables.iter().zip(&before) {
                assert_eq!(
                    count(&store, table).await?,
                    *before,
                    "{table} changed (limit={limit}, {device})"
                );
            }
            assert_eq!(
                last_seen().await?,
                seen,
                "a rejected limit touched the device (limit={limit}, {device})"
            );
        }
    }

    // Both bounds are accepted, with and without a cursor.
    for limit in [1, CEILING] {
        let page = store
            .sync(&owner, &format!("bound-{limit}"), None, limit, 5001)
            .await?;
        store
            .sync(
                &owner,
                &format!("bound-{limit}"),
                Some(&page.next_cursor),
                limit,
                5002,
            )
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn out_of_range_limits_are_rejected_without_side_effects() -> Result<()> {
    let (_dir, store) = local().await?;
    rejected_limits(store).await
}

/// PB4: snapshot pages hold exactly one batch of at most `limit` resources and reach every
/// authorised resource.
async fn snapshot_pages(store: Store) -> Result<()> {
    let contract = contract()?;
    let owner = account(&store).await?;
    let baseline = store.sync(&owner, "baseline", None, CEILING, 1000).await?;
    assert!(!baseline.has_more);
    let mut expected: BTreeSet<String> = ids(&baseline).collect();
    for chunk in 0..21 {
        let commands: Vec<Command> = (0..if chunk == 20 { 3 } else { 20 })
            .map(|_| Command::CreatePerson {
                id: id(),
                name: "Snapshot".into(),
                initial_policy: None,
            })
            .collect();
        for command in &commands {
            if let Command::CreatePerson { id, .. } = command {
                expected.insert(id.clone());
            }
        }
        store.apply(&owner, &id(), &commands).await?;
    }
    assert!(expected.len() >= 403);

    let mut seen = BTreeSet::new();
    let mut page = store.sync(&owner, "ceiling", None, CEILING, 1001).await?;
    let mut sizes = Vec::new();
    loop {
        within_contract(&contract, &page);
        assert_eq!(page.phase, "snapshot");
        assert_eq!(page.batches.len(), 1, "a snapshot page holds one batch");
        sizes.push(page.batches[0].changes.len());
        for resource in ids(&page) {
            assert!(seen.insert(resource), "duplicate snapshot item");
        }
        if !page.has_more {
            break;
        }
        page = store
            .sync(&owner, "ceiling", Some(&page.next_cursor), CEILING, 1001)
            .await?;
    }
    assert!(page.next_cursor.starts_with("d1."));
    let total = expected.len();
    assert_eq!(sizes, [CEILING, CEILING, total - 2 * CEILING]);
    assert_eq!(
        seen, expected,
        "the snapshot reaches every authorised resource"
    );

    let mut cursor = None;
    for _ in 0..3 {
        let page = store
            .sync(&owner, "single", cursor.as_deref(), 1, 1001)
            .await?;
        assert_eq!(page.batches.len(), 1);
        assert_eq!(page.batches[0].changes.len(), 1);
        assert!(page.has_more);
        cursor = Some(page.next_cursor);
    }
    Ok(())
}

fn ids(page: &Page) -> impl Iterator<Item = String> + '_ {
    page.batches
        .iter()
        .flat_map(|b| &b.changes)
        .filter_map(|change| match change {
            Change::Upsert { resource } => Some(resource.id.clone()),
            Change::Remove { .. } => None,
        })
}

#[tokio::test]
async fn snapshot_pages_hold_one_batch_of_at_most_limit_resources() -> Result<()> {
    let (_dir, store) = local().await?;
    snapshot_pages(store).await
}

/// PB5: the core constants are the contract's numbers, and the page bound is the ceiling.
#[test]
fn core_limits_agree_with_the_contract() -> Result<()> {
    let contract = contract()?;
    assert_eq!(contract.limit_min, 1);
    assert_eq!(contract.limit_max as usize, CEILING, "limit.maximum");
    assert_eq!(
        contract.limit_default as usize, SYNC_LIMIT_DEFAULT,
        "limit.default"
    );
    assert_eq!(contract.page_batches, CEILING, "SyncPage.batches.maxItems");
    assert!(
        contract.batch_changes >= CEILING,
        "a full snapshot page fits its one batch"
    );
    Ok(())
}
