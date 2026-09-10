use anyhow::Result;
use atlas_core::{Command, Store};

use crate::support::resource_commands::{account, fixture, id};

async fn retention(store: &Store) -> Result<()> {
    let owner = account(store).await?;
    let person = id();
    let op = id();
    let command = Command::CreatePerson {
        initial_policy: None,
        id: person.clone(),
        name: "Retain this content".into(),
    };
    let revision = store
        .apply(&owner, &op, std::slice::from_ref(&command))
        .await?;
    let old = store.sync(&owner, "phone", None, 200, 1000).await?;
    // Delta recovery lasts well beyond a one-hour snapshot session.
    store
        .sync(&owner, "phone", Some(&old.next_cursor), 200, 86400)
        .await?;
    sqlx::query("UPDATE sync_batches SET created_at=1000 WHERE account_id=$1")
        .bind(&owner)
        .execute(&store.pool)
        .await?;
    store.collect_expired(8_000_000).await?;
    assert!(
        store
            .sync(&owner, "phone", Some(&old.next_cursor), 200, 8_000_000)
            .await
            .is_err()
    );
    assert_eq!(store.apply(&owner, &op, &[command]).await?, revision);
    let fresh = store.sync(&owner, "phone", None, 200, 8_000_000).await?;
    assert!(serde_json::to_string(&fresh)?.contains("Retain this content"));
    let receipts: Vec<String> =
        sqlx::query_scalar("SELECT payload FROM receipts WHERE account_id=$1")
            .bind(&owner)
            .fetch_all(&store.pool)
            .await?;
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].len(), 64);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sync_batches WHERE account_id=$1")
        .bind(&owner)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn retention_preserves_content_and_operation_identity() -> Result<()> {
    let (_dir, s) = fixture().await?;
    retention(&s).await
}
