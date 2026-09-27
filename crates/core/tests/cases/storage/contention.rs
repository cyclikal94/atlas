use anyhow::Result;
use atlas_core::{Change, Command, Projection, Store, households::ManagementCommand, tasks::*};

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
            expected_policy_version: Some(1),
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
                    expected_policy_version: Some(1),
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

/// Waits until `writer` is demonstrably queued behind `held`'s write gate. PostgreSQL's own
/// lock graph is the evidence there; SQLite has no equivalent view, so the bounded poll
/// only confirms the writer has not finished. The assertions that follow are the proof.
async fn queued_behind<T>(
    store: &Store,
    held: &mut sqlx::Transaction<'static, sqlx::Any>,
    writer: &tokio::task::JoinHandle<Result<T>>,
) -> Result<()> {
    if crate::support::database::postgres() {
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut **held)
            .await?;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let blocked: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))",
                )
                .bind(pid)
                .fetch_one(&store.pool)
                .await?;
                if blocked > 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
    } else {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(!writer.is_finished());
    Ok(())
}

/// Both orderings of a sharing change and a content write are decided under the one gate:
/// whichever commits first is what the other is checked against.
async fn policy_and_content_ordering(store: &Store) -> Result<()> {
    use crate::support::projection::projection;
    use crate::support::sharing::{ledger, shared};
    let w = shared(store).await?;
    let seen = projection(store, &w.bob, &w.field).await?;
    let edit = |label: &str, policy_version| Command::Edit {
        id: w.field.clone(),
        expected_version: seen.version,
        expected_policy_version: policy_version,
        label: label.into(),
        value: "Climbing".into(),
    };
    let revoke_carol = Command::Revoke {
        id: w.field.clone(),
        expected_version: 1,
        account_id: w.carol.clone(),
    };

    // 1. The sharing change commits first: the queued write, built against the old
    // sharing, waits for the gate and is then rejected.
    let mut held = store.begin_serial().await?;
    Store::apply_in(
        &mut held,
        &w.alice,
        &id(),
        std::slice::from_ref(&revoke_carol),
    )
    .await?;
    let (writer_store, bob, operation) = (store.clone(), w.bob.clone(), id());
    let stale = edit("Stale", seen.policy_version);
    let writer = tokio::spawn(async move { writer_store.apply(&bob, &operation, &[stale]).await });
    queued_behind(store, &mut held, &writer).await?;
    held.commit().await?;
    assert_eq!(writer.await?.unwrap_err().to_string(), "conflict");
    let after_policy = ledger(store, &w.field).await?;
    assert_eq!(
        after_policy.content,
        (1, 2, "Hobby".into(), after_policy.content.3.clone())
    );

    // 2. The content write commits first: the queued sharing change is still valid, since
    // a content edit does not move the sharing revision, and both end up applied.
    let mut held = store.begin_serial().await?;
    Store::apply_in(&mut held, &w.bob, &id(), &[edit("First", Some(2))]).await?;
    let (writer_store, alice, carol, field) = (
        store.clone(),
        w.alice.clone(),
        w.carol.clone(),
        w.field.clone(),
    );
    let writer = tokio::spawn(async move {
        writer_store
            .apply(
                &alice,
                &id(),
                &[Command::Grant {
                    id: field,
                    expected_version: 2,
                    account_id: carol,
                    edit: false,
                }],
            )
            .await
    });
    queued_behind(store, &mut held, &writer).await?;
    held.commit().await?;
    writer.await??;
    let end = ledger(store, &w.field).await?;
    assert_eq!(
        (end.content.0, end.content.1, end.content.2.as_str()),
        (2, 3, "First")
    );
    Ok(())
}

#[tokio::test]
async fn sharing_change_and_content_write_are_ordered_by_the_gate() -> Result<()> {
    let (_d, s) = fixture().await?;
    policy_and_content_ordering(&s).await
}

/// The retained-field race through its two real code paths. The owner narrows a field's
/// sharing with `ReplacePolicy`, removing carol and keeping bob as an editor, while bob's
/// saved draft arrives as the existing-field branch of `PutField`. Each side runs on its own
/// connection; the gate decides which commits first and the other is checked against it.
async fn narrowing_and_put_field_ordering(store: &Store, narrowing_first: bool) -> Result<()> {
    use crate::support::projection::projection;
    use crate::support::sharing::{NOW, ledger, ledger_in, policy, shared};
    let w = shared(store).await?;
    let seen = projection(store, &w.bob, &w.field).await?;
    assert_eq!(seen.policy_version, Some(1));
    assert!(projection(store, &w.carol, &w.field).await?.can_edit);
    let narrow = ManagementCommand::ReplacePolicy {
        id: w.field.clone(),
        expected_version: 1,
        policy: policy(&[(&w.bob, true)]),
    };
    let save = |seen: &Projection, text: &str| TaskCommand::PutField {
        id: w.field.clone(),
        parent_id: w.person.clone(),
        expected_version: Some(seen.version),
        expected_policy_version: seen.policy_version,
        label: "Hobby".into(),
        value: FieldValue::Text { text: text.into() },
        initial_policy: None,
    };
    let (writer_store, alice, bob) = (store.clone(), w.alice.clone(), w.bob.clone());
    let operation = id();
    let held = store.begin_serial().await?;

    let committed = if narrowing_first {
        // The owner's narrowing holds the gate. Bob's draft, built against the old sharing,
        // queues behind it on another connection and must be rejected.
        let (_, mut held_now) = Store::management_on(held, &w.alice, &id(), &[narrow], NOW).await?;
        let committed = ledger_in(&mut held_now, &w.field).await?;
        let (stale, operation) = (save(&seen, "Stale"), operation.clone());
        let writer = tokio::spawn(async move {
            let result = writer_store
                .task_command(&bob, &operation, &stale, None, NOW)
                .await;
            result.map(|r| r.revision)
        });
        queued_behind(store, &mut held_now, &writer).await?;
        held_now.commit().await?;
        assert_eq!(writer.await?.unwrap_err().to_string(), "conflict");
        committed
    } else {
        // Bob's write holds the gate against the sharing he read. The owner's narrowing,
        // whose own revision a content write does not move, queues behind it.
        let (_, mut held_now) =
            Store::task_command_on(held, &w.bob, &id(), &save(&seen, "First"), None, NOW).await?;
        let committed = ledger_in(&mut held_now, &w.field).await?;
        let writer =
            tokio::spawn(
                async move { writer_store.management(&alice, &id(), &[narrow], NOW).await },
            );
        queued_behind(store, &mut held_now, &writer).await?;
        held_now.commit().await?;
        writer.await??;
        committed
    };

    let end = ledger(store, &w.field).await?;
    // Carol lost the field and bob kept it as an editor, whichever write went first.
    assert!(projection(store, &w.carol, &w.field).await.is_err());
    let now = projection(store, &w.bob, &w.field).await?;
    assert!(now.can_edit);
    assert_eq!(now.policy_version, Some(2));
    if narrowing_first {
        // The rejected write changed no content, receipt or sync state: everything the
        // gate holder had committed is exactly what is left, and bob's operation has no
        // receipt to replay.
        assert_eq!(end, committed);
        assert_eq!(
            (now.version, now.value["text"].as_str()),
            (1, Some("Surfing"))
        );
        // The control: after refetching, the same operation ID commits with the new sharing.
        let fresh = save(&now, "Fresh");
        store
            .task_command(&w.bob, &operation, &fresh, None, NOW)
            .await?;
        let after = projection(store, &w.bob, &w.field).await?;
        assert_eq!(
            (
                after.version,
                after.policy_version,
                after.value["text"].as_str()
            ),
            (2, Some(2), Some("Fresh"))
        );
    } else {
        // The write was valid against the sharing it read and is kept; the narrowing that
        // followed was also valid, because a content write never moves the sharing revision.
        assert_eq!(
            (now.version, now.value["text"].as_str()),
            (2, Some("First"))
        );
        assert_eq!(end.receipts, committed.receipts + 1);
        assert!(end.revision > committed.revision);
    }
    Ok(())
}

#[tokio::test]
async fn narrowing_committed_first_rejects_the_queued_put_field() -> Result<()> {
    let (_d, s) = fixture().await?;
    narrowing_and_put_field_ordering(&s, true).await
}

#[tokio::test]
async fn put_field_committed_first_is_kept_when_the_narrowing_follows() -> Result<()> {
    let (_d, s) = fixture().await?;
    narrowing_and_put_field_ordering(&s, false).await
}
