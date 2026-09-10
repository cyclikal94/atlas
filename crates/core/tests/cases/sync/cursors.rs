use anyhow::Result;
use atlas_core::{Command, Store};

use crate::support::resources::{account, id, local};

async fn cursor_scenario(store: Store) -> Result<()> {
    store.migrate().await?;
    let owner = account(&store).await?;
    let person = id();
    let operation = id();
    let create = Command::CreatePerson {
        id: person.clone(),
        name: "Initial".into(),
        initial_policy: None,
    };
    let receipt = store
        .apply(&owner, &operation, std::slice::from_ref(&create))
        .await?;
    let first = store.sync(&owner, "phone", None, 200, 1000).await?;
    assert!(first.next_cursor.starts_with("d1."));
    let mut cursor = first.next_cursor.clone();
    for version in 1..=100 {
        store
            .apply(
                &owner,
                &id(),
                &[Command::Edit {
                    id: person.clone(),
                    expected_version: version,
                    label: format!("Version {version}"),
                    value: String::new(),
                }],
            )
            .await?;
        cursor = store
            .sync(&owner, "phone", Some(&cursor), 200, 1000 + version)
            .await?
            .next_cursor;
    }
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sync_cursors WHERE account_id=$1")
        .bind(&owner)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(rows, 0, "delta activity must not create stored cursor rows");
    assert_eq!(
        store
            .sync(
                &owner,
                "phone",
                Some(&first.next_cursor),
                200,
                1000 + 7 * 86400
            )
            .await?
            .batches
            .len(),
        100,
        "offline and lost-response cursors retain their horizon"
    );
    let mut forged = cursor.clone();
    let last = forged.pop().unwrap();
    forged.push(if last == 'a' { 'b' } else { 'a' });
    assert_eq!(
        store
            .sync(&owner, "phone", Some(&forged), 200, 1200)
            .await
            .unwrap_err()
            .to_string(),
        "resync_required"
    );
    store.sync(&owner, "tablet", None, 200, 1200).await?;
    assert_eq!(
        store
            .sync(&owner, "tablet", Some(&cursor), 200, 1200)
            .await
            .unwrap_err()
            .to_string(),
        "resync_required"
    );
    store.forget_device(&owner, "phone").await?;
    store.sync(&owner, "phone", None, 200, 1201).await?;
    for old in [&cursor] {
        assert_eq!(
            store
                .sync(&owner, "phone", Some(old), 200, 1201)
                .await
                .unwrap_err()
                .to_string(),
            "resync_required"
        );
    }
    assert_eq!(
        store.apply(&owner, &operation, &[create]).await?,
        receipt,
        "device retirement must not discard operation identity"
    );
    Ok(())
}

#[tokio::test]
async fn delta_cursors_stay_bounded_and_revocation_preserves_receipts() -> Result<()> {
    let (_dir, store) = local().await?;
    cursor_scenario(store).await
}
