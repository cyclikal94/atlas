use anyhow::Result;
use atlas_core::{Command, Store};

use crate::support::resources::{account, id, local};

async fn cleanup_scenario(store: Store, postgres: bool) -> Result<()> {
    store.migrate().await?;
    let owner = account(&store).await?;
    let person = id();
    store
        .apply(
            &owner,
            &id(),
            &[Command::CreatePerson {
                id: person.clone(),
                name: "Retain content".into(),
                initial_policy: None,
            }],
        )
        .await?;
    let start: i64 = sqlx::query_scalar("SELECT revision FROM sync_clock WHERE id=1")
        .fetch_one(&store.pool)
        .await?;
    let mut tx = store.begin_serial().await?;
    for revision in start + 1..=start + 1500 {
        sqlx::query("INSERT INTO sync_batches(account_id,revision,payload,created_at) VALUES ($1,$2,'[]',0)").bind(&owner).bind(revision).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE sync_clock SET revision=$1 WHERE id=1")
        .bind(start + 1500)
        .execute(&mut *tx)
        .await?;
    let snapshot = id();
    sqlx::query("INSERT INTO sync_snapshots(id,expires_at) VALUES ($1,0)")
        .bind(&snapshot)
        .execute(&mut *tx)
        .await?;
    for position in 1_i64..=1250 {
        sqlx::query(
            "INSERT INTO snapshot_items(snapshot_id,position,id,kind,parent_id,label,value,version,policy_version,can_edit) VALUES ($1,$2,$3,'person',NULL,'Old snapshot','',1,1,1)",
        )
        .bind(&snapshot)
        .bind(position)
        .bind(id())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let page = store
        .sync(&owner, "active", None, 200, 1_000_000_000)
        .await?;
    let gate = if postgres {
        Some(store.begin_serial().await?)
    } else {
        None
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.collect_expired(1_000_000_000),
    )
    .await??;
    if let Some(gate) = gate {
        gate.rollback().await?;
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sync_batches WHERE account_id=$1 AND created_at=0",
    )
    .bind(&owner)
    .fetch_one(&store.pool)
    .await?;
    assert_eq!(count, 0);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM snapshot_items WHERE snapshot_id=$1")
        .bind(&snapshot)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(count, 0);
    let floor: i64 = sqlx::query_scalar("SELECT sync_floor FROM accounts WHERE id=$1")
        .bind(&owner)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(floor, start + 1500);
    assert!(
        store
            .sync(
                &owner,
                "active",
                Some(&page.next_cursor),
                200,
                1_000_000_001
            )
            .await?
            .batches
            .is_empty()
    );
    let label: String = sqlx::query_scalar("SELECT label FROM resources WHERE id=$1")
        .bind(person)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(label, "Retain content");
    Ok(())
}

#[tokio::test]
async fn cleanup_removes_large_expired_sets_without_content_loss() -> Result<()> {
    let (_dir, store) = local().await?;
    cleanup_scenario(store, crate::support::database::postgres()).await
}
