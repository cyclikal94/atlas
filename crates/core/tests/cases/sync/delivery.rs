use anyhow::Result;
use atlas_core::{Change, Command, Store};

use crate::support::resource_commands::{account, create, fixture, id, share};

async fn delivery_recovery(store: &Store) -> Result<()> {
    let owner = account(store).await?;
    let reader = account(store).await?;
    // Both devices establish a cursor before the grant. Only one fetches the grant.
    let seen = store.sync(&reader, "seen", None, 200, 1000).await?;
    let unseen = store.sync(&reader, "unseen", None, 200, 1000).await?;
    let person = create(store, &owner).await?;
    share(store, &owner, &person, &reader, 1).await?;
    let delivered = store
        .sync(&reader, "seen", Some(&seen.next_cursor), 200, 1001)
        .await?;
    assert!(serde_json::to_string(&delivered)?.contains("Private name"));
    store
        .apply(
            &owner,
            &id(),
            &[Command::Revoke {
                id: person.clone(),
                expected_version: 2,
                account_id: reader.clone(),
            }],
        )
        .await?;
    for _ in 0..2 {
        // Lost responses remain retryable; revocation is delivered without a snapshot.
        let page = store
            .sync(&reader, "seen", Some(&delivered.next_cursor), 200, 1002)
            .await?;
        assert_eq!(
            page.batches[0].changes,
            [Change::Remove { id: person.clone() }]
        );
    }
    let page = store
        .sync(&reader, "unseen", Some(&unseen.next_cursor), 200, 1002)
        .await?;
    assert!(page.batches.iter().all(|b| b.changes.is_empty()));
    assert!(!serde_json::to_string(&page)?.contains(&person));
    let payloads: Vec<String> =
        sqlx::query_scalar("SELECT payload FROM sync_batches WHERE account_id=$1")
            .bind(&reader)
            .fetch_all(&store.pool)
            .await?;
    assert!(payloads.iter().all(|s| !s.contains("Private name")));
    Ok(())
}

#[tokio::test]
async fn only_delivered_ids_are_removed_and_retries_survive() -> Result<()> {
    let (_dir, s) = fixture().await?;
    delivery_recovery(&s).await
}
