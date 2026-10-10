use anyhow::Result;
use atlas_core::Store;
use atlas_core::devices::{Capacity, SYNC_DEVICE_LIMIT};

use crate::support::devices::retire;
use crate::support::resources::{account, local};

fn sync_capacity(used: i64) -> Capacity {
    Capacity {
        limit: Some(SYNC_DEVICE_LIMIT),
        used,
        remaining: Some(SYNC_DEVICE_LIMIT - used),
    }
}

async fn quota_scenario(store: Store) -> Result<()> {
    store.migrate().await?;
    let owner = account(&store).await?;
    let other = account(&store).await?;
    assert_eq!(SYNC_DEVICE_LIMIT, 32, "the enforced limit is unchanged");
    assert_eq!(
        store.devices(&owner, 1000).await?.sync_capacity,
        sync_capacity(0)
    );
    for n in 0..32 {
        store
            .sync(&owner, &format!("device-{n}"), None, 200, 1000)
            .await?;
        // BE-Q25 B7: the reported capacity is the enforced count after every registration.
        assert_eq!(
            store.devices(&owner, 1000).await?.sync_capacity,
            sync_capacity(n + 1)
        );
    }
    assert_eq!(
        store
            .sync(&owner, "replacement", None, 200, 1000)
            .await
            .unwrap_err()
            .to_string(),
        "device_capacity"
    );
    // A registered device needs no new slot, and another account's capacity is its own.
    store.sync(&owner, "device-5", None, 200, 1000).await?;
    assert_eq!(
        store.devices(&owner, 1000).await?.sync_capacity,
        sync_capacity(32)
    );
    assert_eq!(
        store.devices(&other, 1000).await?.sync_capacity,
        sync_capacity(0)
    );
    let listing = store.devices(&owner, 1000).await?;
    assert_eq!(listing.devices.len(), 32);
    assert_eq!(listing.sync_capacity.remaining, Some(0));
    retire(&store, &owner, "device-0", 1000).await?;
    assert_eq!(
        store.devices(&owner, 1000).await?.sync_capacity,
        sync_capacity(31),
        "the retirement freed its slot on commit"
    );
    store.sync(&owner, "replacement", None, 200, 1001).await?;
    let devices = store.devices(&owner, 1001).await?;
    assert_eq!(devices.devices.len(), 32);
    assert_eq!(devices.sync_capacity, sync_capacity(32));
    assert!(devices.devices.iter().any(|d| d.id == "replacement"));
    Ok(())
}

#[tokio::test]
async fn device_quota_is_recoverable() -> Result<()> {
    let (_dir, store) = local().await?;
    quota_scenario(store).await
}

/// BE-Q25 B7: a registration past the retention cutoff that the sweep has not yet removed still
/// holds a slot, for enforcement and for the reported capacity alike; the sweep frees it in both.
#[tokio::test]
async fn an_aged_registration_counts_until_it_is_swept() -> Result<()> {
    let (_dir, store) = local().await?;
    let store = store.with_retention_days(1)?;
    store.migrate().await?;
    let owner = account(&store).await?;
    for n in 0..32 {
        store
            .sync(&owner, &format!("device-{n}"), None, 200, 1000)
            .await?;
    }
    let later = 1000 + 2 * 86400;
    // Every registration is now aged past the one-day retention, but none has been swept.
    assert_eq!(
        store.devices(&owner, later).await?.sync_capacity,
        sync_capacity(32)
    );
    assert_eq!(
        store
            .sync(&owner, "replacement", None, 200, later)
            .await
            .unwrap_err()
            .to_string(),
        "device_capacity",
        "enforcement counts the aged rows too"
    );
    store.collect_expired(later).await?;
    assert_eq!(
        store.devices(&owner, later).await?.sync_capacity,
        sync_capacity(0)
    );
    store.sync(&owner, "replacement", None, 200, later).await?;
    assert_eq!(
        store.devices(&owner, later).await?.sync_capacity,
        sync_capacity(1)
    );
    Ok(())
}
