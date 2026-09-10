use anyhow::Result;
use atlas_core::{Command, Store};

use crate::support::resource_commands::{account, fixture, id};

async fn atomicity(store: &Store, rollback: bool) -> Result<()> {
    let owner = account(store).await?;
    let page = store.sync(&owner, "phone", None, 200, 1000).await?;
    let op = id();
    let commands = [
        Command::CreatePerson {
            initial_policy: None,
            id: id(),
            name: "One".into(),
        },
        Command::CreatePerson {
            initial_policy: None,
            id: id(),
            name: "Two".into(),
        },
    ];
    if rollback {
        let mut tx = store.begin_serial().await?;
        Store::apply_in(&mut tx, &owner, &op, &commands).await?;
        tx.rollback().await?;
        let page = store
            .sync(&owner, "phone", Some(&page.next_cursor), 1, 1001)
            .await?;
        assert!(page.batches.is_empty());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM receipts WHERE account_id=$1")
            .bind(&owner)
            .fetch_one(&store.pool)
            .await?;
        assert_eq!(count, 0);
    }
    store.apply(&owner, &op, &commands).await?;
    let page = store
        .sync(&owner, "phone", Some(&page.next_cursor), 1, 1001)
        .await?;
    assert_eq!(page.batches.len(), 1);
    assert_eq!(page.batches[0].changes.len(), 2);
    Ok(())
}

#[tokio::test]
async fn publication_batches_are_indivisible() -> Result<()> {
    let (_d, s) = fixture().await?;
    atomicity(&s, false).await
}

#[tokio::test]
async fn rollback_includes_receipt_and_publication() -> Result<()> {
    let (_d, s) = fixture().await?;
    atomicity(&s, true).await
}
