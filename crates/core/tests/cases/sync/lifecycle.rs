use anyhow::Result;
use atlas_core::{Change, Command, Page, Store};
use std::collections::BTreeMap;
use uuid::Uuid;

fn id() -> String {
    Uuid::new_v4().to_string()
}
fn edit(id: &str, version: i64, label: &str) -> Command {
    Command::Edit {
        id: id.into(),
        expected_version: version,
        label: label.into(),
        value: String::new(),
    }
}
fn grant(id: &str, version: i64, account: &str, edit: bool) -> Command {
    Command::Grant {
        id: id.into(),
        expected_version: version,
        account_id: account.into(),
        edit,
    }
}
fn resources(page: &Page) -> BTreeMap<String, atlas_core::Projection> {
    page.batches
        .iter()
        .flat_map(|b| &b.changes)
        .filter_map(|c| match c {
            Change::Upsert { resource } => Some((resource.id.clone(), resource.clone())),
            _ => None,
        })
        .collect()
}

async fn scenario(url: &str) -> Result<()> {
    let store = Store::connect(url).await?;
    store.migrate().await?;
    let alice = id();
    let bob = id();
    let eve = id();
    for (user, name) in [(&alice, "alice"), (&bob, "bob"), (&eve, "eve")] {
        store
            .add_account(user, name, "test-only-not-a-login-hash")
            .await?;
    }
    let person = id();
    let private = id();
    let shared = id();
    store
        .apply(
            &alice,
            &id(),
            &[
                Command::CreatePerson {
                    initial_policy: None,
                    id: person.clone(),
                    name: "Morgan".into(),
                },
                Command::CreateField {
                    initial_policy: None,
                    id: private.clone(),
                    person_id: person.clone(),
                    label: "Secret gift".into(),
                    value: "Surprise".into(),
                },
                Command::CreateField {
                    initial_policy: None,
                    id: shared.clone(),
                    person_id: person.clone(),
                    label: "Hobby".into(),
                    value: "Surfing".into(),
                },
            ],
        )
        .await?;
    // PRIV-001: no identity means no child, even if an attempted grant targets it.
    assert!(
        store
            .apply(&alice, &id(), &[grant(&shared, 1, &bob, true)])
            .await
            .is_err()
    );
    assert!(resources(&store.sync(&bob, "phone", None, 200, 1000).await?).is_empty());
    // PRIV-002: identity + selected field are shared atomically, siblings stay private.
    store
        .apply(
            &alice,
            &id(),
            &[
                grant(&person, 1, &bob, false),
                grant(&shared, 1, &bob, true),
            ],
        )
        .await?;
    let b = store.sync(&bob, "phone", None, 200, 1000).await?;
    let visible = resources(&b);
    assert_eq!(visible.len(), 2);
    assert!(!visible.contains_key(&private));
    assert_eq!(visible[&shared].parent_id.as_deref(), Some(person.as_str()));
    assert!(!visible[&person].can_edit);
    assert!(visible[&shared].can_edit);
    assert!(
        store
            .apply(&bob, &id(), &[edit(&private, 1, "steal")])
            .await
            .is_err()
    );
    assert!(
        store
            .apply(&bob, &id(), &[edit(&person, 2, "rename")])
            .await
            .is_err()
    );
    assert!(
        store
            .apply(&bob, &id(), &[grant(&shared, 2, &eve, true)])
            .await
            .is_err()
    );

    // SYNC-001: a transaction is one indivisible delta batch even with page limit 1.
    store
        .apply(
            &alice,
            &id(),
            &[
                edit(&person, 1, "Morgan Smith"),
                Command::Edit {
                    id: shared.clone(),
                    expected_version: 1,
                    label: "Hobbies".into(),
                    value: "Surfing and music".into(),
                },
            ],
        )
        .await?;
    let delta = store
        .sync(&bob, "phone", Some(&b.next_cursor), 1, 1001)
        .await?;
    assert_eq!(delta.phase, "delta");
    assert_eq!(delta.batches.len(), 1);
    assert_eq!(delta.batches[0].changes.len(), 2);
    assert_eq!(resources(&delta)[&person].label, "Morgan Smith");
    assert!(!serde_json::to_string(&delta)?.contains("Surprise"));

    // SYNC-002: rollback leaves no evidence, batch, or receipt; retry can apply once.
    let operation = id();
    assert!(
        store
            .apply(
                &alice,
                "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA",
                &[edit(&person, 2, "Non-canonical")]
            )
            .await
            .is_err()
    );
    let command = edit(&person, 2, "After rollback");
    let mut failed = store.begin_serial().await?;
    Store::apply_in(
        &mut failed,
        &alice,
        &operation,
        std::slice::from_ref(&command),
    )
    .await?;
    failed.rollback().await?;
    let empty = store
        .sync(&bob, "phone", Some(&delta.next_cursor), 1, 1002)
        .await?;
    assert!(empty.batches.is_empty());
    let revision = store
        .apply(&alice, &operation, std::slice::from_ref(&command))
        .await?;
    assert_eq!(
        revision,
        store
            .apply(&alice, &operation, std::slice::from_ref(&command))
            .await?
    );
    assert!(
        store
            .apply(&alice, &operation, &[edit(&person, 3, "different")])
            .await
            .is_err()
    );

    // SYNC-003: a later writer cannot publish past an in-flight earlier writer.
    let mut first = store.begin_serial().await?;
    let first_revision =
        Store::apply_in(&mut first, &alice, &id(), &[edit(&person, 3, "First")]).await?;
    let other = store.clone();
    let who = alice.clone();
    let target = person.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let writer = tokio::spawn(async move {
        entered_tx.send(()).unwrap();
        other
            .apply(&who, &id(), &[edit(&target, 4, "Second")])
            .await
    });
    entered_rx.await?;
    assert!(!writer.is_finished());
    first.commit().await?;
    assert!(writer.await?? > first_revision);
    let page = store
        .sync(&bob, "phone", Some(&empty.next_cursor), 1, 1003)
        .await?;
    assert!(page.has_more);
    assert_eq!(page.batches[0].revision, revision);
    let page2 = store
        .sync(&bob, "phone", Some(&page.next_cursor), 1, 1003)
        .await?;
    assert_eq!(page2.batches[0].revision, first_revision);
    let page3 = store
        .sync(&bob, "phone", Some(&page2.next_cursor), 1, 1003)
        .await?;
    assert_eq!(resources(&page3)[&person].label, "Second");

    // SYNC-004: snapshots are stable under ordinary writes; revocation invalidates snapshots.
    let snapshot = store.sync(&bob, "phone", None, 1, 1004).await?;
    assert!(snapshot.has_more);
    store
        .apply(
            &alice,
            &id(),
            &[Command::Edit {
                id: shared.clone(),
                expected_version: 2,
                label: "Later".into(),
                value: "New".into(),
            }],
        )
        .await?;
    let tail = store
        .sync(&bob, "phone", Some(&snapshot.next_cursor), 1, 1005)
        .await?;
    assert_eq!(resources(&tail)[&shared].value["text"], "Surfing and music");
    let latest = store
        .sync(&bob, "phone", Some(&tail.next_cursor), 1, 1005)
        .await?;
    assert_eq!(resources(&latest)[&shared].value["text"], "New");
    let stale_snapshot = store.sync(&bob, "phone", None, 1, 1005).await?;
    store
        .apply(
            &alice,
            &id(),
            &[Command::Revoke {
                id: person.clone(),
                expected_version: 2,
                account_id: bob.clone(),
            }],
        )
        .await?;
    assert_eq!(
        store
            .sync(&bob, "phone", Some(&stale_snapshot.next_cursor), 1, 1006)
            .await
            .unwrap_err()
            .to_string(),
        "access_changed"
    );
    let removed = store
        .sync(&bob, "phone", Some(&latest.next_cursor), 200, 1006)
        .await?;
    assert_eq!(
        removed.batches.last().unwrap().changes,
        [
            Change::Remove { id: shared.clone() },
            Change::Remove { id: person.clone() }
        ]
    );
    assert!(resources(&store.sync(&bob, "phone", None, 200, 1006).await?).is_empty());
    let payload: String = sqlx::query_scalar(
        "SELECT payload FROM sync_batches WHERE account_id=$1 ORDER BY revision DESC LIMIT 1",
    )
    .bind(&bob)
    .fetch_one(&store.pool)
    .await?;
    assert!(!payload.contains("Surprise"));
    // Regrant reveals unchanged child data, without sharing the secret sibling.
    store
        .apply(&alice, &id(), &[grant(&person, 3, &bob, false)])
        .await?;
    let restored = store.sync(&bob, "phone", None, 200, 1007).await?;
    assert_eq!(resources(&restored)[&shared].value["text"], "New");
    assert!(!resources(&restored).contains_key(&private));
    assert!(
        store
            .sync(&eve, "phone", Some(&restored.next_cursor), 1, 1007)
            .await
            .is_err()
    );
    assert!(
        store
            .sync(&bob, "another-device", Some(&restored.next_cursor), 1, 1007)
            .await
            .is_err()
    );
    assert!(
        store
            .sync(&bob, "phone", Some(&restored.next_cursor), 1, 8000000)
            .await
            .is_err()
    );

    // SYNC-005: concurrent retries and restart retain account-wide operation identity.
    let replay = id();
    let cmd = edit(&person, 5, "One logical edit");
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..6 {
        let s = store.clone();
        let a = alice.clone();
        let op = replay.clone();
        let c = cmd.clone();
        jobs.spawn(async move { s.apply(&a, &op, &[c]).await });
    }
    let mut revisions = std::collections::BTreeSet::new();
    while let Some(r) = jobs.join_next().await {
        revisions.insert(r??);
    }
    assert_eq!(revisions.len(), 1);
    store.pool.close().await;
    let reopened = Store::connect(url).await?;
    assert_eq!(
        reopened.apply(&alice, &replay, &[cmd]).await?,
        *revisions.first().unwrap()
    );
    let persisted = reopened
        .sync(&bob, "phone", Some(&restored.next_cursor), 200, 1008)
        .await?;
    assert_eq!(resources(&persisted)[&person].label, "One logical edit");
    // SYNC-006: process exit without destructors rolls back batch + receipt + content.
    let crash_id = id();
    let crash_operation = id();
    let output = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--ignored",
            "--exact",
            "cases::sync::lifecycle::crash_writer",
        ])
        .env("ATLAS_CRASH_URL", url)
        .env("ATLAS_CRASH_ACTOR", &alice)
        .env("ATLAS_CRASH_RESOURCE", &crash_id)
        .env("ATLAS_CRASH_OPERATION", &crash_operation)
        .output()?;
    assert_eq!(
        output.status.code(),
        Some(77),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM receipts WHERE operation_id=$1")
        .bind(&crash_operation)
        .fetch_one(&reopened.pool)
        .await?;
    assert_eq!(receipts, 0);
    assert!(
        !resources(&reopened.sync(&alice, "laptop", None, 200, 1009).await?)
            .contains_key(&crash_id)
    );
    reopened
        .apply(
            &alice,
            &crash_operation,
            &[Command::CreatePerson {
                initial_policy: None,
                id: crash_id,
                name: "Crash retry".into(),
            }],
        )
        .await?;
    reopened.pool.close().await;
    Ok(())
}

#[tokio::test]
async fn sync_invariants() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    scenario(&url).await
}

#[tokio::test]
#[ignore = "subprocess helper; invoked by shared sync scenario"]
async fn crash_writer() -> Result<()> {
    let store = Store::connect(&std::env::var("ATLAS_CRASH_URL")?).await?;
    let mut tx = store.begin_serial().await?;
    Store::apply_in(
        &mut tx,
        &std::env::var("ATLAS_CRASH_ACTOR")?,
        &std::env::var("ATLAS_CRASH_OPERATION")?,
        &[Command::CreatePerson {
            initial_policy: None,
            id: std::env::var("ATLAS_CRASH_RESOURCE")?,
            name: "Crash retry".into(),
        }],
    )
    .await?;
    std::process::exit(77);
}
