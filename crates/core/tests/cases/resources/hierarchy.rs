use anyhow::Result;
use atlas_core::{Change, Command, Store};
use uuid::Uuid;
fn id() -> String {
    Uuid::new_v4().to_string()
}
async fn scenario(store: Store) -> Result<()> {
    store.migrate().await?;
    let owner = id();
    let viewer = id();
    for (account, name) in [(&owner, "owner"), (&viewer, "viewer")] {
        store
            .add_account(
                account,
                &format!("{name}-{}", Uuid::new_v4().simple()),
                "test-only",
            )
            .await?;
    }
    let task = id();
    let execution = id();
    let occurrence = id();
    let progress = id();
    let mut tx = store.begin_serial().await?;
    for (resource, parent, kind) in [
        (&task, None, "task"),
        (&execution, Some(&task), "execution"),
        (&occurrence, Some(&execution), "occurrence"),
        (&progress, Some(&occurrence), "progress"),
    ] {
        sqlx::query("INSERT INTO resources(id,owner_id,parent_id,kind,label,value) VALUES ($1,$2,$3,$4,$4,'')").bind(resource).bind(&owner).bind(parent).bind(kind).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO resource_grants VALUES ($1,$2,0)")
            .bind(resource)
            .bind(&viewer)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE sync_clock SET resource_count=resource_count+4 WHERE id=1")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let snapshot = store.sync(&viewer, "phone", None, 200, 1000).await?;
    let kinds = snapshot.batches[0]
        .changes
        .iter()
        .filter_map(|c| {
            if let Change::Upsert { resource } = c {
                Some(resource.kind.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(kinds, vec!["task", "execution", "occurrence", "progress"]);
    // Removing the middle ancestor must revoke all deeper descendants, even
    // though their own grants remain. Removal order is child-before-parent.
    store
        .apply(
            &owner,
            &id(),
            &[Command::Revoke {
                id: execution.clone(),
                expected_version: 1,
                account_id: viewer.clone(),
            }],
        )
        .await?;
    let delta = store
        .sync(&viewer, "phone", Some(&snapshot.next_cursor), 200, 1001)
        .await?;
    let removed = delta
        .batches
        .iter()
        .flat_map(|b| &b.changes)
        .filter_map(|c| {
            if let Change::Remove { id } = c {
                Some(id)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(removed, vec![&progress, &occurrence, &execution]);
    let fresh = store.sync(&viewer, "fresh", None, 200, 1001).await?;
    assert_eq!(fresh.batches[0].changes.len(), 1);
    // Granting access to a child cannot circumvent the hidden ancestor.
    assert_eq!(
        store
            .apply(
                &owner,
                &id(),
                &[Command::Grant {
                    id: progress.clone(),
                    expected_version: 1,
                    account_id: viewer.clone(),
                    edit: false
                }]
            )
            .await
            .unwrap_err()
            .to_string(),
        "identity_grant_required"
    );
    store
        .apply(
            &owner,
            &id(),
            &[Command::Grant {
                id: execution.clone(),
                expected_version: 2,
                account_id: viewer.clone(),
                edit: false,
            }],
        )
        .await?;
    let delta = store
        .sync(&viewer, "phone", Some(&delta.next_cursor), 200, 1002)
        .await?;
    let upserts = delta
        .batches
        .iter()
        .flat_map(|b| &b.changes)
        .filter_map(|c| {
            if let Change::Upsert { resource } = c {
                Some(&resource.id)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(upserts, vec![&task, &execution, &occurrence, &progress]);
    // Legacy generic edits cannot replace validated task/progress data.
    assert_eq!(
        store
            .apply(
                &owner,
                &id(),
                &[Command::Edit {
                    id: progress.clone(),
                    expected_version: 1,
                    label: "bypass".into(),
                    value: "{}".into()
                }]
            )
            .await
            .unwrap_err()
            .to_string(),
        "invalid_value"
    );
    assert!(
        sqlx::query("UPDATE resources SET parent_id=$1 WHERE id=$2")
            .bind(&task)
            .bind(&progress)
            .execute(&store.pool)
            .await
            .is_err()
    );
    let broken = id();
    assert!(sqlx::query("INSERT INTO resources(id,owner_id,parent_id,kind,label,value) VALUES ($1,$2,$3,'progress','invalid','')").bind(broken).bind(&owner).bind(&task).execute(&store.pool).await.is_err());
    store.migrate().await?;
    Ok(())
}
#[tokio::test]
async fn transitive_privacy_and_ordering() -> Result<()> {
    let (_dir, store) = crate::support::database::fixture().await?;
    scenario(store).await
}

#[tokio::test]
async fn sqlite_initialisation_keeps_foreign_keys_enabled() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::connect(&format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("foreign-keys.sqlite").display()
    ))
    .await?;
    store.migrate().await?;
    let mut connections = Vec::new();
    for _ in 0..10 {
        let mut connection = store.pool.acquire().await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
                .fetch_one(&mut *connection)
                .await?,
            1
        );
        connections.push(connection);
    }
    Ok(())
}
