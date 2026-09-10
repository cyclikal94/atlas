use anyhow::Result;
use atlas_core::{Command, Store};
use uuid::Uuid;

#[tokio::test]
async fn restore_invalidates_both_cursor_types_and_preserves_receipts() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let actor = Uuid::new_v4().to_string();
    store.add_account(&actor, &actor, "fixture").await?;
    let commands = vec![
        Command::CreatePerson {
            id: Uuid::new_v4().to_string(),
            name: "One".into(),
            initial_policy: None,
        },
        Command::CreatePerson {
            id: Uuid::new_v4().to_string(),
            name: "Two".into(),
            initial_policy: None,
        },
    ];
    let operation = Uuid::new_v4().to_string();
    let receipt = store.apply(&actor, &operation, &commands).await?;
    let snapshot = store.sync(&actor, "snapshot", None, 1, 1000).await?;
    assert!(snapshot.has_more);
    let delta = store.sync(&actor, "delta", None, 100, 1000).await?;
    assert!(!delta.has_more);
    store.prepare_restored_database().await?;
    for (device, cursor) in [
        ("snapshot", snapshot.next_cursor),
        ("delta", delta.next_cursor),
    ] {
        assert!(
            store
                .sync(&actor, device, Some(&cursor), 100, 1001)
                .await
                .is_err()
        );
    }
    assert_eq!(
        serde_json::to_value(receipt)?,
        serde_json::to_value(store.apply(&actor, &operation, &commands).await?)?
    );
    let fresh = store.sync(&actor, "delta", None, 100, 1001).await?;
    assert_eq!(
        fresh.batches.iter().map(|b| b.changes.len()).sum::<usize>(),
        2
    );
    Ok(())
}
