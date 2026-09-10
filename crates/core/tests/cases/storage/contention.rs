use anyhow::Result;
use atlas_core::{Change, Command, Store};

use crate::support::resource_commands::{account, create, fixture, id};

async fn read_during_write(store: &Store) -> Result<()> {
    let owner = account(store).await?;
    let person = create(store, &owner).await?;
    let page = store.sync(&owner, "phone", None, 200, 1000).await?;
    let mut pending = store.begin_serial().await?;
    Store::apply_in(
        &mut pending,
        &owner,
        &id(),
        &[Command::Edit {
            id: person,
            expected_version: 1,
            label: "Not committed".into(),
            value: String::new(),
        }],
    )
    .await?;
    // The write gate is already held: this isn't a task-scheduling race probe.
    let idle = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        store.sync(&owner, "phone", Some(&page.next_cursor), 200, 1001),
    )
    .await??;
    assert!(idle.batches.is_empty());
    assert_eq!(idle.next_cursor, page.next_cursor);
    pending.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn idle_read_does_not_wait_for_publication() -> Result<()> {
    let (_dir, s) = fixture().await?;
    read_during_write(&s).await
}

#[tokio::test]
async fn reports_actual_gate_contention() -> Result<()> {
    let (_d, store) = fixture().await?;
    if crate::support::database::postgres() {
        return postgres_contention(&store).await;
    }
    let held = store.begin_serial().await?;
    let mut connection = store.pool.acquire().await?;
    sqlx::query("PRAGMA busy_timeout=0")
        .execute(&mut *connection)
        .await?;
    let error = sqlx::query("UPDATE sync_clock SET revision=revision WHERE id=1")
        .execute(&mut *connection)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("5")
    );
    held.rollback().await?;
    sqlx::query("UPDATE sync_clock SET revision=revision WHERE id=1")
        .execute(&mut *connection)
        .await?;
    Ok(())
}

async fn postgres_contention(store: &Store) -> Result<()> {
    let held = store.begin_serial().await?;
    let mut later = store.pool.begin().await?;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *later)
        .await?;
    let writer = tokio::spawn(async move {
        sqlx::query("UPDATE sync_clock SET revision=revision WHERE id=1")
            .execute(&mut *later)
            .await?;
        later.commit().await
    });
    // Observe PostgreSQL's actual wait state, rather than a signal before polling SQL.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let wait: Option<String> =
                sqlx::query_scalar("SELECT wait_event_type FROM pg_stat_activity WHERE pid=$1")
                    .bind(pid)
                    .fetch_one(&store.pool)
                    .await?;
            if wait.as_deref() == Some("Lock") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert!(!writer.is_finished());
    held.commit().await?;
    writer.await??;
    Ok(())
}

async fn concurrent_clients(store: &Store) -> Result<()> {
    let owner = account(store).await?;
    let person = create(store, &owner).await?;
    let mut jobs = tokio::task::JoinSet::new();
    for device in 0..10 {
        let s = store.clone();
        let a = owner.clone();
        jobs.spawn(async move {
            let device = format!("device-{device}");
            let mut page = s.sync(&a, &device, None, 200, 1000).await?;
            let mut cached = String::new();
            for batch in &page.batches {
                for change in &batch.changes {
                    if let Change::Upsert { resource } = change {
                        cached = resource.label.clone();
                    }
                }
            }
            for tick in 1..=10 {
                page = s
                    .sync(&a, &device, Some(&page.next_cursor), 200, 1000 + tick)
                    .await?;
                for batch in &page.batches {
                    for change in &batch.changes {
                        if let Change::Upsert { resource } = change {
                            cached = resource.label.clone();
                        }
                    }
                }
            }
            Ok::<_, anyhow::Error>((device, page.next_cursor, cached))
        });
    }
    for version in 1..=10 {
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
    }
    while let Some(result) = jobs.join_next().await {
        let (device, cursor, mut cached) = result??;
        // Regardless of interleaving, the final cache state can be recovered.
        let page = store
            .sync(&owner, &device, Some(&cursor), 200, 1011)
            .await?;
        for batch in &page.batches {
            for change in &batch.changes {
                if let Change::Upsert { resource } = change {
                    cached = resource.label.clone();
                }
            }
        }
        assert_eq!(cached, "Version 10");
    }
    Ok(())
}

#[tokio::test]
async fn ten_clients_and_writer_make_progress() -> Result<()> {
    let (_d, s) = fixture().await?;
    concurrent_clients(&s).await
}
