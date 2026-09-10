use anyhow::Result;
use atlas_core::Store;

use crate::support::resources::{account, local};

async fn quota_scenario(store: Store) -> Result<()> {
    store.migrate().await?;
    let owner = account(&store).await?;
    for n in 0..32 {
        store
            .sync(&owner, &format!("device-{n}"), None, 200, 1000)
            .await?;
    }
    assert_eq!(
        store
            .sync(&owner, "replacement", None, 200, 1000)
            .await
            .unwrap_err()
            .to_string(),
        "device_capacity"
    );
    assert_eq!(store.devices(&owner, 1000).await?.len(), 32);
    store.forget_device(&owner, "device-0").await?;
    store.sync(&owner, "replacement", None, 200, 1001).await?;
    let devices = store.devices(&owner, 1001).await?;
    assert_eq!(devices.len(), 32);
    assert!(devices.iter().any(|d| d.id == "replacement"));
    Ok(())
}

#[tokio::test]
async fn device_quota_is_recoverable() -> Result<()> {
    let (_dir, store) = local().await?;
    quota_scenario(store).await
}
