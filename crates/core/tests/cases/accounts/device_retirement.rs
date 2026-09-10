use anyhow::Result;
use atlas_core::Store;

use crate::support::task_fixtures::{account, ledger, person, setup};

async fn device_cleanup(s: &Store) -> Result<()> {
    let a = account(s).await?;
    person(s, &a).await?;
    let page = s.sync(&a, "phone", None, 200, 1000).await?;
    s.sync(&a, "tablet", None, 200, 1000).await?;
    assert_eq!(ledger(s, &a, "phone").await?, 1);
    s.forget_device(&a, "phone").await?;
    assert_eq!(ledger(s, &a, "phone").await?, 0);
    assert_eq!(ledger(s, &a, "tablet").await?, 1);
    assert!(
        s.sync(&a, "phone", Some(&page.next_cursor), 200, 1001)
            .await
            .is_err()
    );
    s.sync(&a, "phone", None, 200, 1001).await?;
    // A retained resource need not be redelivered after reinstall until snapshot.
    assert_eq!(ledger(s, &a, "phone").await?, 1);
    s.sync(&a, "active", None, 200, 1_000_000_000).await?;
    s.collect_expired(1_000_000_000).await?;
    for device in ["phone", "tablet"] {
        assert_eq!(ledger(s, &a, device).await?, 0);
    }
    assert_eq!(ledger(s, &a, "active").await?, 1);
    assert_eq!(
        s.resources(&a, "person", None, false, None, 200)
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn retired_devices_release_ledgers() -> Result<()> {
    let (s, _d) = setup().await?;
    device_cleanup(&s).await
}
