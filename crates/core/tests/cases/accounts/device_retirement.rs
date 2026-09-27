use anyhow::Result;
use atlas_core::Store;

use crate::support::devices::{add_inactive_subscription_with_secret, add_subscription, retire};
use crate::support::task_fixtures::{account, ledger, person, setup};

async fn device_cleanup(s: &Store) -> Result<()> {
    let a = account(s).await?;
    person(s, &a).await?;
    let page = s.sync(&a, "phone", None, 200, 1000).await?;
    s.sync(&a, "tablet", None, 200, 1000).await?;
    assert_eq!(ledger(s, &a, "phone").await?, 1);
    retire(s, &a, "phone", 1000).await?;
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

async fn mixed_active_and_inactive_subscriptions(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let active = add_subscription(s, &a, "phone").await?;
    let leaked_one = add_inactive_subscription_with_secret(s, &a, "phone", "leaked-1").await?;
    let leaked_two = add_inactive_subscription_with_secret(s, &a, "phone", "leaked-2").await?;
    retire(s, &a, "phone", 1000).await?;
    let rows: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT id,active,secret FROM notification_subscriptions WHERE account_id=$1 AND device_id='phone' ORDER BY id",
    )
    .bind(&a)
    .fetch_all(&s.pool)
    .await?;
    assert_eq!(
        rows.len(),
        3,
        "all three rows for the device must survive as rows, deactivated"
    );
    for (id, active_flag, secret) in &rows {
        assert_eq!(*active_flag, 0, "{id} must be inactive after retirement");
        assert_eq!(
            secret, "",
            "{id} must have its secret erased after retirement"
        );
    }
    for expected in [&active, &leaked_one, &leaked_two] {
        assert!(
            rows.iter().any(|(id, ..)| id == expected),
            "{expected} missing"
        );
    }
    Ok(())
}

#[tokio::test]
async fn retiring_a_device_erases_every_subscription_secret() -> Result<()> {
    let (s, _d) = setup().await?;
    mixed_active_and_inactive_subscriptions(&s).await
}
