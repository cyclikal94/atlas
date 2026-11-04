//! Bounded `/sync` delivery (BE-Q23, contract v1): opt-in `max_changes`, revision pieces and
//! `d2` continuations. Each piece sequence is compared with the whole batch the same server
//! returns without the bound, at the same state.
use anyhow::Result;
use atlas_core::{
    Batch, Change, Command, Page, Projection, Store,
    households::ManagementCommand as M,
    people::PeopleCommand,
    policy::{Policy, PrincipalGrant},
    tasks::*,
};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

use crate::support::database::fixture;
use crate::support::devices::retire;
use crate::support::task_fixtures::definition;

const T: i64 = 1_788_868_800;
const ORDER: [&str; 11] = [
    "calendar_source",
    "event",
    "person",
    "task",
    "list",
    "field",
    "execution",
    "occurrence",
    "progress",
    "review",
    "reminder",
];

fn id() -> String {
    Uuid::new_v4().to_string()
}
fn rank(kind: &str) -> usize {
    ORDER.iter().position(|k| *k == kind).expect("known kind")
}
async fn account(store: &Store) -> Result<String> {
    let id = id();
    store
        .add_account(&id, &format!("q23-{}", id.replace('-', "")), "test-only")
        .await?;
    Ok(id)
}
fn share(reader: &str) -> Policy {
    Policy {
        grants: vec![PrincipalGrant::Account {
            id: reader.into(),
            edit: false,
        }],
        exclude_accounts: vec![],
    }
}
async fn versions(store: &Store, id: &str) -> Result<(i64, i64)> {
    Ok(
        sqlx::query_as("SELECT version,policy_version FROM resources WHERE id=$1")
            .bind(id)
            .fetch_one(&store.pool)
            .await?,
    )
}
async fn ledger(store: &Store, actor: &str, device: &str) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM sync_deliveries WHERE account_id=$1 AND device_id=$2",
    )
    .bind(actor)
    .bind(device)
    .fetch_one(&store.pool)
    .await?)
}

/// The densest valid checklist: 100 one-character items, 40 checked (BE-Q23 W-real).
fn checklist() -> FieldValue {
    let items: Vec<_> = (0..100)
        .map(|i| ChecklistItem {
            id: Uuid::from_u128(0xC000 + i).to_string(),
            label: "x".into(),
        })
        .collect();
    let checked = items.iter().take(40).map(|i| i.id.clone()).collect();
    FieldValue::Checklist { items, checked }
}

/// `count` dense checklist fields under `person`, each shared with `reader` when given, written
/// by the real field writer in one transaction (one revision per field).
async fn checklists(
    store: &Store,
    owner: &str,
    person: &str,
    reader: Option<&str>,
    count: usize,
) -> Result<Vec<String>> {
    let mut tx = store.begin_serial().await?;
    let mut fields = Vec::new();
    for i in 0..count {
        let field = id();
        let command = TaskCommand::PutField {
            id: field.clone(),
            parent_id: person.into(),
            expected_version: None,
            expected_policy_version: None,
            label: format!("Dense {i}"),
            value: checklist(),
            initial_policy: Some(reader.map_or_else(Policy::default, share)),
        };
        tx = Store::task_command_on(tx, owner, &id(), &command, None, T)
            .await?
            .1;
        fields.push(field);
    }
    tx.commit().await?;
    Ok(fields)
}

async fn apply(store: &Store, actor: &str, commands: &[Command]) -> Result<i64> {
    store.apply(actor, &id(), commands).await
}
async fn grant(store: &Store, owner: &str, id: &str, reader: &str) -> Result<i64> {
    let command = Command::Grant {
        id: id.into(),
        expected_version: versions(store, id).await?.1,
        account_id: reader.into(),
        edit: false,
    };
    apply(store, owner, &[command]).await
}
fn revoke_command(id: &str, policy_version: i64, reader: &str) -> Command {
    Command::Revoke {
        id: id.into(),
        expected_version: policy_version,
        account_id: reader.into(),
    }
}
async fn revoke(store: &Store, owner: &str, id: &str, reader: &str) -> Result<i64> {
    let command = revoke_command(id, versions(store, id).await?.1, reader);
    apply(store, owner, &[command]).await
}
async fn edit(store: &Store, owner: &str, field: &str, value: &str) -> Result<i64> {
    let (version, policy) = versions(store, field).await?;
    let command = Command::Edit {
        id: field.into(),
        expected_version: version,
        expected_policy_version: Some(policy),
        label: "Edited".into(),
        value: value.into(),
    };
    apply(store, owner, &[command]).await
}

/// A person shared with `reader` and `count` text fields shared with them, then hidden by a
/// revocation of the person, so granting the person again publishes `1 + count` upserts.
async fn hidden_person(
    store: &Store,
    owner: &str,
    reader: &str,
    count: usize,
) -> Result<(String, Vec<String>)> {
    let person = id();
    let fields: Vec<String> = (0..count).map(|_| id()).collect();
    let mut commands = vec![Command::CreatePerson {
        id: person.clone(),
        name: "Segmented".into(),
        initial_policy: Some(share(reader)),
    }];
    commands.extend(fields.iter().map(|f| Command::CreateField {
        id: f.clone(),
        person_id: person.clone(),
        label: "Shared".into(),
        value: "Shared value".into(),
        initial_policy: Some(share(reader)),
    }));
    // A command batch holds at most 20 commands.
    for chunk in commands.chunks(20) {
        apply(store, owner, chunk).await?;
    }
    revoke(store, owner, &person, reader).await?;
    let mut fields = fields;
    // In id order, which is also the order pieces deliver them.
    fields.sort();
    Ok((person, fields))
}

/// Every page from `cursor` until `has_more` is false.
async fn drain(
    store: &Store,
    actor: &str,
    device: &str,
    cursor: Option<String>,
    limit: usize,
    m: Option<usize>,
    now: i64,
) -> Result<(Vec<Page>, String)> {
    let mut cursor = cursor;
    let mut pages = Vec::new();
    loop {
        let page = store
            .sync_bounded(actor, device, cursor.as_deref(), limit, m, now)
            .await?;
        cursor = Some(page.next_cursor.clone());
        let more = page.has_more;
        pages.push(page);
        if !more {
            return Ok((pages, cursor.expect("drained")));
        }
        assert!(pages.len() < 10_000, "a drain terminates");
    }
}
async fn head(store: &Store, actor: &str, device: &str, now: i64) -> Result<String> {
    Ok(drain(store, actor, device, None, 200, None, now).await?.1)
}

fn upserts(changes: &[Change]) -> Vec<&Projection> {
    changes
        .iter()
        .filter_map(|c| match c {
            Change::Upsert { resource } => Some(resource),
            Change::Remove { .. } => None,
        })
        .collect()
}
fn removals(changes: &[Change]) -> Vec<&String> {
    changes
        .iter()
        .filter_map(|c| match c {
            Change::Remove { id } => Some(id),
            Change::Upsert { .. } => None,
        })
        .collect()
}
fn count(page: &Page) -> usize {
    page.batches.iter().map(|b| b.changes.len()).sum()
}

/// Within one piece: upserts before removals, upserts in strictly increasing (rank, id), and
/// every ancestor that the whole batch upserts placed before its descendant.
fn piece_is_ordered(changes: &[Change], whole: &BTreeMap<String, Projection>) {
    let first_removal = changes
        .iter()
        .position(|c| matches!(c, Change::Remove { .. }))
        .unwrap_or(changes.len());
    assert!(
        changes[first_removal..]
            .iter()
            .all(|c| matches!(c, Change::Remove { .. })),
        "upserts precede removals"
    );
    let keys: Vec<_> = upserts(changes)
        .iter()
        .map(|p| (rank(&p.kind), p.id.clone()))
        .collect();
    assert!(
        keys.windows(2).all(|w| w[0] < w[1]),
        "rank then id, no duplicates"
    );
    let positions: BTreeMap<&String, usize> = upserts(changes)
        .iter()
        .enumerate()
        .map(|(i, p)| (&p.id, i))
        .collect();
    for p in upserts(changes) {
        let mut parent = p.parent_id.as_ref();
        while let Some(id) = parent.filter(|id| whole.contains_key(*id)) {
            let at = positions
                .get(id)
                .unwrap_or_else(|| panic!("ancestor {id} of {} missing from its piece", p.id));
            assert!(*at < positions[&p.id], "ancestor {id} precedes {}", p.id);
            parent = whole[id].parent_id.as_ref();
        }
    }
}

/// I1, I2 and I10: `bounded` (drained with `max_changes = m`) delivers exactly the batches of
/// `whole` (drained without it, at the same state), splitting only those larger than `m`.
fn reassembles(whole: &[Page], bounded: &[Page], m: usize) {
    let mut pieces = BTreeMap::<i64, Vec<&Batch>>::new();
    let mut revisions = Vec::new();
    for page in bounded {
        assert!(count(page) <= m, "a response holds at most m changes");
        if page.batches.iter().any(|b| b.segment.is_some()) {
            assert_eq!(page.batches.len(), 1, "a piece is alone in its response");
        }
        for batch in &page.batches {
            if revisions.last() != Some(&batch.revision) {
                revisions.push(batch.revision);
            }
            pieces.entry(batch.revision).or_default().push(batch);
        }
    }
    let whole: Vec<&Batch> = whole.iter().flat_map(|p| &p.batches).collect();
    assert_eq!(
        revisions,
        whole.iter().map(|b| b.revision).collect::<Vec<_>>(),
        "same revisions, in order"
    );
    for batch in whole {
        let parts = &pieces[&batch.revision];
        if batch.changes.len() <= m {
            assert_eq!(parts.len(), 1, "a batch that fits is not split");
            assert!(parts[0].segment.is_none());
            assert_eq!(parts[0].changes, batch.changes);
            continue;
        }
        let segments: Vec<_> = parts.iter().map(|b| b.segment.expect("a piece")).collect();
        for (i, s) in segments.iter().enumerate() {
            assert_eq!(s.index as usize, i, "contiguous indices from 0");
            assert_eq!(
                s.last,
                i + 1 == segments.len(),
                "exactly one last piece, at the end"
            );
        }
        let expected: BTreeMap<String, Projection> = upserts(&batch.changes)
            .into_iter()
            .map(|p| (p.id.clone(), p.clone()))
            .collect();
        let mut union = BTreeMap::new();
        let mut removed = Vec::new();
        for part in parts {
            piece_is_ordered(&part.changes, &expected);
            for p in upserts(&part.changes) {
                union.insert(p.id.clone(), p.clone());
            }
            removed.extend(removals(&part.changes));
        }
        assert_eq!(union, expected, "the pieces' upserts are the whole batch's");
        assert_eq!(
            removed,
            removals(&batch.changes),
            "removals in whole-batch order"
        );
    }
}

/// Snapshot cursors for one unbounded device and one bounded device per `m`, all at the head.
async fn devices(store: &Store, reader: &str, ms: &[usize]) -> Result<Vec<(String, String)>> {
    let mut out = vec![("whole".to_owned(), head(store, reader, "whole", T).await?)];
    for m in ms {
        let device = format!("m{m}");
        out.push((device.clone(), head(store, reader, &device, T).await?));
    }
    Ok(out)
}
/// Drain every device to the head at the current state and compare each bounded drain with the
/// unbounded one; returns the new cursors.
async fn compare(
    store: &Store,
    reader: &str,
    ms: &[usize],
    cursors: Vec<(String, String)>,
    now: i64,
) -> Result<Vec<(String, String)>> {
    let mut next = Vec::new();
    let (device, cursor) = &cursors[0];
    let (whole, end) = drain(store, reader, device, Some(cursor.clone()), 200, None, now).await?;
    next.push((device.clone(), end));
    for (m, (device, cursor)) in ms.iter().zip(&cursors[1..]) {
        let (pages, end) = drain(
            store,
            reader,
            device,
            Some(cursor.clone()),
            200,
            Some(*m),
            now,
        )
        .await?;
        reassembles(&whole, &pages, *m);
        next.push((device.clone(), end));
    }
    Ok(next)
}

/// A1: the densest revision real writers can publish for a reader is 200 changes; one more
/// resource either advances the recovery floor (an access change) or is rejected.
#[tokio::test]
async fn reachable_dense_batch() -> Result<()> {
    let (_d, store) = fixture().await?;
    let (a, b) = (account(&store).await?, account(&store).await?);
    let person = id();
    apply(
        &store,
        &a,
        &[Command::CreatePerson {
            id: person.clone(),
            name: "Dense".into(),
            initial_policy: Some(share(&b)),
        }],
    )
    .await?;
    checklists(&store, &a, &person, Some(&b), 199).await?;
    revoke(&store, &a, &person, &b).await?;
    let cursor = head(&store, &b, "reader", T).await?;
    grant(&store, &a, &person, &b).await?;
    let (pages, _) = drain(&store, &b, "reader", Some(cursor), 200, None, T).await?;
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].batches.len(), 1);
    let batch = &pages[0].batches[0];
    assert_eq!(batch.changes.len(), 200, "1 person + 199 dense fields");
    assert!(
        serde_json::to_vec(batch)?.len() > 199 * 6000,
        "the fields are dense"
    );
    // The deepest ancestor chain is task → execution → occurrence → progress.
    store
        .task_command(
            &a,
            &id(),
            &TaskCommand::CreateTask {
                id: id(),
                execution_id: id(),
                title: "Chain".into(),
                definition: definition(Goal::Checkbox),
                anchor: None,
                initial_policy: None,
            },
            None,
            T,
        )
        .await?;
    let depth: i64 = sqlx::query_scalar("SELECT MAX(depth) FROM resource_ancestors")
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(depth, 3, "a single resource needs at most three ancestors");

    // 200 fields: the grant would publish 201 changes, so it advances the floor instead.
    let (_d, store) = fixture().await?;
    let (a, b) = (account(&store).await?, account(&store).await?);
    let person = id();
    apply(
        &store,
        &a,
        &[Command::CreatePerson {
            id: person.clone(),
            name: "Denser".into(),
            initial_policy: Some(share(&b)),
        }],
    )
    .await?;
    checklists(&store, &a, &person, Some(&b), 200).await?;
    let cursor = head(&store, &b, "reader", T).await?;
    revoke(&store, &a, &person, &b).await?;
    let error = store
        .sync_bounded(&b, "reader", Some(&cursor), 200, Some(4), T)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "resync_required");
    // A non-access revision cannot reach the cap: a command batch holds at most 20 commands,
    // and only access changes (grants, revocations, households) touch descendants.
    let commands: Vec<_> = (0..21)
        .map(|_| Command::CreatePerson {
            id: id(),
            name: "Too many".into(),
            initial_policy: None,
        })
        .collect();
    let error = apply(&store, &a, &commands).await.unwrap_err();
    assert_eq!(error.to_string(), "invalid_value");
    Ok(())
}

/// F1: mixed shapes (ancestor closure, removals of delivered content, mixed upsert/removal
/// revisions, small packed revisions) reassemble exactly at every bound.
#[tokio::test]
async fn pieces_reassemble_the_whole_batch() -> Result<()> {
    let (_d, store) = fixture().await?;
    let (a, b) = (account(&store).await?, account(&store).await?);
    let (p1, f1) = hidden_person(&store, &a, &b, 12).await?;
    let task = id();
    store
        .task_command(
            &a,
            &id(),
            &TaskCommand::CreateTask {
                id: task.clone(),
                execution_id: id(),
                title: "Shared task".into(),
                definition: definition(Goal::Checkbox),
                anchor: None,
                initial_policy: Some(share(&b)),
            },
            None,
            T,
        )
        .await?;
    revoke(&store, &a, &task, &b).await?;
    // The largest whole batch below has 1 + 12 + 3 = 16 changes.
    let n = 16;
    let ms = [4, 5, 7, n - 1, n, n + 1, 800];
    let mut cursors = devices(&store, &b, &ms).await?;
    let grants = [
        Command::Grant {
            id: p1.clone(),
            expected_version: versions(&store, &p1).await?.1,
            account_id: b.clone(),
            edit: false,
        },
        Command::Grant {
            id: task.clone(),
            expected_version: versions(&store, &task).await?.1,
            account_id: b.clone(),
            edit: false,
        },
    ];
    apply(&store, &a, &grants).await?;
    cursors = compare(&store, &b, &ms, cursors, T + 1).await?;
    // Mixed: delivered content removed and new content added in one revision.
    let p2 = id();
    let mut mixed = vec![
        revoke_command(&p1, versions(&store, &p1).await?.1, &b),
        Command::CreatePerson {
            id: p2.clone(),
            name: "Second".into(),
            initial_policy: Some(share(&b)),
        },
    ];
    mixed.extend((0..3).map(|_| Command::CreateField {
        id: id(),
        person_id: p2.clone(),
        label: "Second field".into(),
        value: "Visible".into(),
        initial_policy: Some(share(&b)),
    }));
    apply(&store, &a, &mixed).await?;
    let ledgers = [
        ledger(&store, &b, "whole").await?,
        ledger(&store, &b, "m4").await?,
    ];
    assert_eq!(
        ledgers[0], ledgers[1],
        "the pieces delivered what the whole batch did"
    );
    cursors = compare(&store, &b, &ms, cursors, T + 2).await?;
    // Small revisions pack, then a revoke of the task chain.
    for (i, field) in f1.iter().take(3).enumerate() {
        edit(&store, &a, field, &format!("Small {i}")).await?;
    }
    grant(&store, &a, &p1, &b).await?;
    revoke(&store, &a, &task, &b).await?;
    compare(&store, &b, &ms, cursors, T + 3).await?;
    Ok(())
}

/// F1 (synthetic, labelled): two progress records in one revision need their three ancestors
/// each, so `m = 4` gives exactly one progress chain per piece.
#[tokio::test]
async fn deepest_chains_fit_the_minimum() -> Result<()> {
    let (_d, store) = fixture().await?;
    let a = account(&store).await?;
    for _ in 0..2 {
        store
            .task_command(
                &a,
                &id(),
                &TaskCommand::CreateTask {
                    id: id(),
                    execution_id: id(),
                    title: "Chain".into(),
                    definition: definition(Goal::Checkbox),
                    anchor: None,
                    initial_policy: None,
                },
                None,
                T,
            )
            .await?;
    }
    let progress: Vec<String> =
        sqlx::query_scalar("SELECT id FROM resources WHERE kind='progress' ORDER BY id")
            .fetch_all(&store.pool)
            .await?;
    assert_eq!(progress.len(), 2);
    let cursors = devices(&store, &a, &[4]).await?;
    synthetic(
        &store,
        &a,
        &progress
            .iter()
            .map(|p| (p.as_str(), "progress"))
            .collect::<Vec<_>>(),
    )
    .await?;
    let whole = drain(
        &store,
        &a,
        "whole",
        Some(cursors[0].1.clone()),
        200,
        None,
        T,
    )
    .await?
    .0;
    assert_eq!(count(&whole[0]), 8);
    let pieces = drain(
        &store,
        &a,
        "m4",
        Some(cursors[1].1.clone()),
        200,
        Some(4),
        T,
    )
    .await?
    .0;
    reassembles(&whole, &pieces, 4);
    assert_eq!(pieces.len(), 2);
    for page in &pieces {
        let kinds: Vec<_> = upserts(&page.batches[0].changes)
            .iter()
            .map(|p| p.kind.clone())
            .collect();
        assert_eq!(kinds, ["task", "execution", "occurrence", "progress"]);
    }
    Ok(())
}

/// Insert a stored batch with these notices directly (a synthetic revision, as in the brief's
/// 800-change control). Real publication caps a revision at 200 changes.
async fn synthetic(store: &Store, account: &str, notices: &[(&str, &str)]) -> Result<i64> {
    let payload = serde_json::to_string(
        &notices
            .iter()
            .map(|(id, kind)| serde_json::json!({"id": id, "resource_kind": kind}))
            .collect::<Vec<_>>(),
    )?;
    let mut tx = store.begin_serial().await?;
    sqlx::query("UPDATE sync_clock SET revision=revision+1 WHERE id=1")
        .execute(&mut *tx)
        .await?;
    let revision: i64 = sqlx::query_scalar("SELECT revision FROM sync_clock WHERE id=1")
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO sync_batches(account_id,revision,payload,created_at) VALUES ($1,$2,$3,$4)",
    )
    .bind(account)
    .bind(revision)
    .bind(payload)
    .bind(T)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(revision)
}

/// Per batch of a page: its change count and `(index, last)` if it is a piece.
type PageShape = Vec<(usize, Option<(u32, bool)>)>;

/// F2: small batches pack up to `m`, a batch of exactly `m` is whole, an oversize batch starts
/// its own response, and `limit` still counts batches.
#[tokio::test]
async fn packing_and_isolation() -> Result<()> {
    let (_d, store) = fixture().await?;
    let a = account(&store).await?;
    let person = id();
    apply(
        &store,
        &a,
        &[Command::CreatePerson {
            id: person.clone(),
            name: "Packing".into(),
            initial_policy: None,
        }],
    )
    .await?;
    let cursor = head(&store, &a, "phone", T).await?;
    let tablet = head(&store, &a, "tablet", T).await?;
    let watch = head(&store, &a, "watch", T).await?;
    let fields = |count: usize| -> Vec<Command> {
        (0..count)
            .map(|_| Command::CreateField {
                id: id(),
                person_id: person.clone(),
                label: "F".into(),
                value: String::new(),
                initial_policy: None,
            })
            .collect()
    };
    // Revisions of 2, 2, 2, 10, 5 and 1 changes (each field brings its person).
    for size in [1, 1, 1, 9, 4, 0] {
        if size == 0 {
            edit_person(&store, &a, &person).await?;
        } else {
            apply(&store, &a, &fields(size)).await?;
        }
    }
    let sizes = |pages: &[Page]| -> Vec<PageShape> {
        pages
            .iter()
            .map(|p| {
                p.batches
                    .iter()
                    .map(|b| (b.changes.len(), b.segment.map(|s| (s.index, s.last))))
                    .collect()
            })
            .collect()
    };
    let (pages, _) = drain(&store, &a, "phone", Some(cursor.clone()), 200, Some(5), T).await?;
    assert_eq!(
        sizes(&pages),
        [
            vec![(2, None), (2, None)],
            vec![(2, None)],
            vec![(5, Some((0, false)))],
            vec![(5, Some((1, false)))],
            vec![(2, Some((2, true)))],
            vec![(5, None)],
            vec![(1, None)],
        ]
    );
    let more: Vec<_> = pages.iter().map(|p| p.has_more).collect();
    assert_eq!(more, [true, true, true, true, true, true, false]);
    // `limit` keeps counting batches.
    let (pages, _) = drain(&store, &a, "tablet", Some(tablet), 1, Some(800), T).await?;
    assert_eq!(
        pages.iter().map(|p| p.batches.len()).collect::<Vec<_>>(),
        [1; 6]
    );
    let (pages, _) = drain(&store, &a, "watch", Some(watch), 2, Some(800), T).await?;
    assert_eq!(
        pages.iter().map(|p| p.batches.len()).collect::<Vec<_>>(),
        [2, 2, 2]
    );
    Ok(())
}
async fn edit_person(store: &Store, owner: &str, person: &str) -> Result<i64> {
    let (version, policy) = versions(store, person).await?;
    apply(
        store,
        owner,
        &[Command::Edit {
            id: person.into(),
            expected_version: version,
            expected_policy_version: Some(policy),
            label: "Packing again".into(),
            value: String::new(),
        }],
    )
    .await
}

/// F3, I4, I9: `d1 → d2 … → d1`; every intermediate cursor retried returns the identical page
/// and leaves the delivery ledger unchanged.
#[tokio::test]
async fn cursor_chain_and_retries() -> Result<()> {
    let (_d, store) = fixture().await?;
    let (a, b) = (account(&store).await?, account(&store).await?);
    let (person, _) = hidden_person(&store, &a, &b, 20).await?;
    let start = head(&store, &b, "reader", T).await?;
    grant(&store, &a, &person, &b).await?;
    edit_person(&store, &a, &person).await?;
    let mut cursor = start;
    let mut cursors = vec![cursor.clone()];
    let mut pages = Vec::new();
    loop {
        let page = store
            .sync_bounded(&b, "reader", Some(&cursor), 200, Some(6), T + 1)
            .await?;
        let before = ledger(&store, &b, "reader").await?;
        let again = store
            .sync_bounded(&b, "reader", Some(&cursor), 200, Some(6), T + 1)
            .await?;
        assert_eq!(
            serde_json::to_string(&again)?,
            serde_json::to_string(&page)?
        );
        assert_eq!(
            ledger(&store, &b, "reader").await?,
            before,
            "a retry adds no deliveries"
        );
        cursor = page.next_cursor.clone();
        cursors.push(cursor.clone());
        let more = page.has_more;
        pages.push(page);
        if !more {
            break;
        }
    }
    // 21 changes at m = 6: four pieces of the person and five fields, then the edit.
    let shape: Vec<_> = pages
        .iter()
        .map(|p| {
            (
                count(p),
                p.batches[0].segment.map(|s| (s.index, s.last)),
                p.has_more,
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            (6, Some((0, false)), true),
            (6, Some((1, false)), true),
            (6, Some((2, false)), true),
            (6, Some((3, true)), true),
            (1, None, false),
        ]
    );
    let forms: Vec<_> = cursors.iter().map(|c| &c[..2]).collect();
    assert_eq!(forms, ["d1", "d2", "d2", "d2", "d1", "d1"]);
    assert!(cursors.iter().all(|c| c.len() <= 512));
    // An old delta cursor and a snapshot token still work with the bound (I9).
    let (snapshot_pages, _) = drain(&store, &b, "fresh", None, 200, Some(4), T + 1).await?;
    assert!(
        snapshot_pages
            .iter()
            .all(|p| count(p) <= 4 && p.batches[0].segment.is_none())
    );
    Ok(())
}

/// F4: the brief's 800-change synthetic control (799 dense checklist fields + their person) is
/// delivered in bounded pieces that reassemble to the whole batch, which the same request
/// without the bound still returns in one response (I3).
#[tokio::test]
async fn synthetic_800_change_control() -> Result<()> {
    let (_d, store) = fixture().await?;
    let (a, b) = (account(&store).await?, account(&store).await?);
    let person = id();
    apply(
        &store,
        &a,
        &[Command::CreatePerson {
            id: person.clone(),
            name: "Synthetic".into(),
            initial_policy: Some(share(&b)),
        }],
    )
    .await?;
    let fields = checklists(&store, &a, &person, Some(&b), 799).await?;
    let ms = [4, 50, 100, 200, 800];
    let cursors = devices(&store, &b, &ms).await?;
    let mut notices = vec![(person.as_str(), "person")];
    notices.extend(fields.iter().map(|f| (f.as_str(), "field")));
    synthetic(&store, &b, &notices).await?;
    let (whole, _) = drain(
        &store,
        &b,
        "whole",
        Some(cursors[0].1.clone()),
        200,
        None,
        T,
    )
    .await?;
    assert_eq!(whole.len(), 1);
    assert_eq!(
        count(&whole[0]),
        800,
        "without the bound: one 800-change batch"
    );
    for (m, (device, cursor)) in ms.iter().zip(&cursors[1..]) {
        let (pages, _) = drain(&store, &b, device, Some(cursor.clone()), 200, Some(*m), T).await?;
        reassembles(&whole, &pages, *m);
        if *m < 800 {
            // Each piece re-sends the person, so it holds the person and m - 1 fields.
            assert_eq!(pages.len(), 799_usize.div_ceil(m - 1));
        }
    }
    Ok(())
}

/// F5: the real server-generated dense update (one grant, 200 changes) at several bounds.
#[tokio::test]
async fn real_dense_grant() -> Result<()> {
    let (_d, store) = fixture().await?;
    let (a, b) = (account(&store).await?, account(&store).await?);
    let person = id();
    apply(
        &store,
        &a,
        &[Command::CreatePerson {
            id: person.clone(),
            name: "Real".into(),
            initial_policy: Some(share(&b)),
        }],
    )
    .await?;
    checklists(&store, &a, &person, Some(&b), 199).await?;
    revoke(&store, &a, &person, &b).await?;
    let ms = [4, 25, 50, 100, 200];
    let cursors = devices(&store, &b, &ms).await?;
    grant(&store, &a, &person, &b).await?;
    compare(&store, &b, &ms, cursors, T).await?;
    Ok(())
}

/// F6: a snapshot page is `min(limit, m)` resources; its cursors and expiry are unchanged.
#[tokio::test]
async fn snapshot_pages_narrow() -> Result<()> {
    let (_d, store) = fixture().await?;
    let a = account(&store).await?;
    let person = id();
    let mut commands = vec![Command::CreatePerson {
        id: person.clone(),
        name: "Snapshot".into(),
        initial_policy: None,
    }];
    commands.extend((0..9).map(|_| Command::CreateField {
        id: id(),
        person_id: person.clone(),
        label: "F".into(),
        value: String::new(),
        initial_policy: None,
    }));
    apply(&store, &a, &commands).await?;
    for (device, limit, m, sizes) in [
        ("p1", 200, 4, vec![4, 4, 2]),
        ("p2", 3, 4, vec![3, 3, 3, 1]),
        ("p3", 200, 800, vec![10]),
    ] {
        let (pages, end) = drain(&store, &a, device, None, limit, Some(m), T).await?;
        assert_eq!(pages.iter().map(count).collect::<Vec<_>>(), sizes);
        assert!(
            pages
                .iter()
                .all(|p| p.phase == "snapshot" && p.batches[0].segment.is_none())
        );
        for page in &pages[..pages.len() - 1] {
            Uuid::parse_str(&page.next_cursor)?;
        }
        assert!(end.starts_with("d1."));
    }
    // An expired snapshot cursor is still `resync_required` with the bound.
    let first = store.sync_bounded(&a, "p4", None, 200, Some(4), T).await?;
    let error = store
        .sync_bounded(&a, "p4", Some(&first.next_cursor), 200, Some(4), T + 3600)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "resync_required");
    Ok(())
}

/// The first piece of a 21-change revision at `m = 6`, and its continuation.
async fn chain(store: &Store) -> Result<(String, String, String, Page)> {
    let (a, b) = (account(store).await?, account(store).await?);
    let (person, _) = hidden_person(store, &a, &b, 20).await?;
    let start = head(store, &b, "reader", T).await?;
    grant(store, &a, &person, &b).await?;
    let first = store
        .sync_bounded(&b, "reader", Some(&start), 200, Some(6), T)
        .await?;
    assert!(first.next_cursor.starts_with("d2."));
    Ok((a, b, person, first))
}

/// F8: receipts replay mid-chain; pruning the revision answers `resync_required` for its
/// continuation while receipts survive.
#[tokio::test]
async fn receipts_and_retention() -> Result<()> {
    let (_d, store) = fixture().await?;
    let (a, b, person, first) = chain(&store).await?;
    let operation = id();
    let command = Command::Edit {
        id: person.clone(),
        expected_version: versions(&store, &person).await?.0,
        expected_policy_version: Some(versions(&store, &person).await?.1),
        label: "Receipt".into(),
        value: String::new(),
    };
    let revision = store
        .apply(&a, &operation, std::slice::from_ref(&command))
        .await?;
    let second = store
        .sync_bounded(&b, "reader", Some(&first.next_cursor), 200, Some(6), T)
        .await?;
    assert_eq!(second.batches[0].segment.map(|s| s.index), Some(1));
    assert_eq!(
        store
            .apply(&a, &operation, std::slice::from_ref(&command))
            .await?,
        revision,
        "the original receipt is replayed"
    );
    sqlx::query("UPDATE sync_batches SET created_at=1000")
        .execute(&store.pool)
        .await?;
    store.collect_expired(8_000_000).await?;
    let error = store
        .sync_bounded(
            &b,
            "reader",
            Some(&second.next_cursor),
            200,
            Some(6),
            8_000_000,
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "resync_required");
    assert_eq!(store.apply(&a, &operation, &[command]).await?, revision);
    Ok(())
}

/// N2, I6: every field of a continuation is authenticated and bound to the account, device and
/// revision; a failure has no ledger or `last_seen` side effect.
#[tokio::test]
async fn continuation_tokens_resist_tampering() -> Result<()> {
    let (_d, store) = fixture().await?;
    let (a, b, _, first) = chain(&store).await?;
    let token = first.next_cursor.clone();
    assert_eq!(token.len(), 136);
    let side_effects = |store: Store, b: String| async move {
        let ledger = ledger(&store, &b, "reader").await?;
        let seen: i64 = sqlx::query_scalar(
            "SELECT last_seen FROM sync_devices WHERE account_id=$1 AND device_id=$2",
        )
        .bind(&b)
        .bind("reader")
        .fetch_one(&store.pool)
        .await?;
        anyhow::Ok((ledger, seen))
    };
    let before = side_effects(store.clone(), b.clone()).await?;
    let refused = |cursor: String, device: &'static str, actor: String, now: i64| {
        let store = store.clone();
        async move {
            let error = store
                .sync_bounded(&actor, device, Some(&cursor), 200, Some(6), now)
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), "resync_required", "{cursor}");
            anyhow::Ok(())
        }
    };
    // Every character, including the separators and the pass letter.
    for i in 2..token.len() {
        let mut bytes = token.clone().into_bytes();
        bytes[i] = match bytes[i] {
            b'0' => b'1',
            b'.' => b'0',
            b'u' => b'r',
            _ => b'0',
        };
        refused(String::from_utf8(bytes)?, "reader", b.clone(), T).await?;
    }
    refused(format!("{token}0"), "reader", b.clone(), T).await?;
    refused(token[..token.len() - 1].to_owned(), "reader", b.clone(), T).await?;
    // The same token presented as the other cursor form, and a continuation signed with the
    // delta-cursor domain by someone holding the key.
    refused(format!("d1{}", &token[2..]), "reader", b.clone(), T).await?;
    let key = cursor_key(&store, &b, "reader").await?;
    let sign = |domain: &str, payload: &str| {
        format!("{payload}.{}", tag(&key, domain, &b, "reader", payload))
    };
    let payload = token.rsplit_once('.').expect("mac").0;
    // The MAC covers the plain position, so the test seals like the server rather than signing
    // the sealed payload (adaptation of the pre-R1 `sign(v2, payload) == token` check).
    let (header, position) = open(&key, &b, "reader", &token);
    assert_eq!(
        seal(&key, "atlas-delta-v2", &b, "reader", &header, position),
        token,
        "the test seals like the server"
    );
    refused(sign("atlas-delta-v1", payload), "reader", b.clone(), T).await?;
    refused(
        seal(&key, "atlas-delta-v1", &b, "reader", &header, position),
        "reader",
        b.clone(),
        T,
    )
    .await?;
    let d1 = first_d1(&token);
    refused(sign("atlas-delta-v2", &d1), "reader", b.clone(), T).await?;
    // A delta cursor signed in the continuation domain is not a delta cursor either.
    let error = store
        .sync_bounded(
            &b,
            "reader",
            Some(&sign("atlas-delta-v2", &d1)),
            200,
            None,
            T,
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "resync_required");
    assert_eq!(
        side_effects(store.clone(), b.clone()).await?,
        before,
        "no side effects"
    );
    // Another device or account, expiry, and a retired device.
    store.sync(&b, "other", None, 200, T).await?;
    refused(token.clone(), "other", b.clone(), T).await?;
    store.sync(&a, "reader", None, 200, T).await?;
    refused(token.clone(), "reader", a.clone(), T).await?;
    refused(token.clone(), "reader", b.clone(), T + 90 * 86400).await?;
    store
        .sync_bounded(&b, "reader", Some(&token), 200, Some(6), T)
        .await?;
    retire(&store, &b, "reader", T).await?;
    store.sync(&b, "reader", None, 200, T + 1).await?;
    refused(token, "reader", b, T + 1).await?;
    Ok(())
}
async fn cursor_key(store: &Store, actor: &str, device: &str) -> Result<String> {
    Ok(sqlx::query_scalar(
        "SELECT cursor_key FROM sync_devices WHERE account_id=$1 AND device_id=$2",
    )
    .bind(actor)
    .bind(device)
    .fetch_one(&store.pool)
    .await?)
}
/// The server's MAC framing, written out independently.
fn tag(key: &str, domain: &str, actor: &str, device: &str, payload: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("key");
    mac.update(format!("{domain}\n{actor}\n{}:{device}\n{payload}", device.len()).as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}
fn pad(key: &str, actor: &str, device: &str, tag_hex: &str) -> u32 {
    let pad = tag(key, "atlas-delta-v2-siv", actor, device, tag_hex);
    u32::from_str_radix(&pad[..8], 16).expect("hex")
}
/// A continuation with `header` (`d2.{boundary}.{expiry}.{revision}.{index}`) and the plain
/// `position`, its MAC computed in `domain`.
fn seal(key: &str, domain: &str, actor: &str, device: &str, header: &str, position: u32) -> String {
    let mac = tag(
        key,
        domain,
        actor,
        device,
        &format!("{header}.{position:08x}"),
    );
    format!(
        "{header}.{:08x}.{mac}",
        position ^ pad(key, actor, device, &mac)
    )
}
/// The header and plain position of a server continuation; only the device key opens it.
fn open(key: &str, actor: &str, device: &str, token: &str) -> (String, u32) {
    let parts: Vec<_> = token.split('.').collect();
    let sealed = u32::from_str_radix(parts[5], 16).expect("hex");
    (
        parts[..5].join("."),
        sealed ^ pad(key, actor, device, parts[6]),
    )
}
fn sealed(token: &str) -> u32 {
    u32::from_str_radix(token.split('.').nth(5).expect("position"), 16).expect("hex")
}

/// R1 (review 28 October 2026, reviewer probe in `.agtx/evidence/q23/review-20261028`): two
/// continuations of the same revision and index, from different bounds or from a repeat after
/// a visibility change, must not reveal how their positions differ. Positions count notices the
/// reader cannot see, so a shared pad would expose a field that was never sent through the XOR
/// of the public fields. A healthy store and one with such a field return the same visible
/// pages; the device key (test only) confirms the plain positions the comparison would expose.
#[tokio::test]
async fn continuations_do_not_compare() -> Result<()> {
    let never_sent = fixed(350);
    let mut visible = Vec::new();
    for hidden in [false, true] {
        let (_d, store) = fixture().await?;
        // DA-H1 (Review 12): fixed accounts and device key, installed before any cursor, so the
        // XOR comparison below is a fixed vector rather than a 2^-32 chance per run.
        let (a, b) = fixed_accounts(&store, "reader").await?;
        let person = fixed(1);
        let fields: Vec<String> = (1..=12).map(|i| fixed(i * 100)).collect();
        let mut commands = vec![Command::CreatePerson {
            id: person.clone(),
            name: "Segmented".into(),
            initial_policy: Some(share(&b)),
        }];
        for f in fields.iter().chain(hidden.then_some(&never_sent)) {
            commands.push(Command::CreateField {
                id: f.clone(),
                person_id: person.clone(),
                label: "Shared".into(),
                value: "Shared value".into(),
                initial_policy: Some(share(&b)),
            });
        }
        apply(&store, &a, &commands).await?;
        revoke(&store, &a, &person, &b).await?;
        let start = head(&store, &b, "reader", T).await?;
        grant(&store, &a, &person, &b).await?;
        if hidden {
            // Between the third and fourth visible fields; hidden before any delta request.
            revoke(&store, &a, &never_sent, &b).await?;
        }
        let key = cursor_key(&store, &b, "reader").await?;
        assert_eq!(key, VECTOR_KEY);
        let piece = |m: usize| {
            let (store, b, start) = (store.clone(), b.clone(), start.clone());
            async move {
                store
                    .sync_bounded(&b, "reader", Some(&start), 1, Some(m), T)
                    .await
            }
        };
        let (four, five) = (piece(4).await?, piece(5).await?);
        // A repeat with the same bound after a delivered field is hidden: the window moves on.
        revoke(&store, &a, &fields[1], &b).await?;
        let repeat = piece(4).await?;
        let pages = [&four, &five, &repeat];
        let text = pages
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<String, _>>()?;
        assert!(!text.contains(&never_sent), "never disclosed in a body");
        let header = |p: &Page| open(&key, &b, "reader", &p.next_cursor).0;
        assert!(
            pages.iter().all(|p| header(p) == header(&four)),
            "same boundary, expiry, revision and index"
        );
        // Person, then fields in id order; the hidden field shifts every later position by one.
        let shift = u32::from(hidden);
        let plain = [4 + shift, 5 + shift, 5 + shift];
        for (x, y) in [(0, 1), (0, 2)] {
            assert_ne!(
                sealed(&pages[x].next_cursor) ^ sealed(&pages[y].next_cursor),
                plain[x] ^ plain[y],
                "the public fields cancel to the positions' XOR"
            );
        }
        let opened: Vec<u32> = pages
            .iter()
            .map(|p| open(&key, &b, "reader", &p.next_cursor).1)
            .collect();
        assert_eq!(opened, plain, "the device key opens those positions");
        visible.push(
            pages
                .iter()
                .map(|p| {
                    upserts(&p.batches[0].changes)
                        .iter()
                        .map(|r| r.id.clone())
                        .collect()
                })
                .collect::<Vec<Vec<String>>>(),
        );
    }
    assert_eq!(visible[0], visible[1], "the visible pages are identical");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// DA-H1/Q6 (Review 12, 3 November 2026; authority BE-Q23-HMAC-TOKEN-SCOPE-20261103): fixed
// continuation vectors. Every signing input is declared: accounts, device and its key (installed
// through test-only database access before any cursor is issued), the clock, operation and
// resource IDs, notice order, policies and the delivery ledger. The expected tokens come from
// `tests/fixtures/q23_continuation_vectors.py` (Python's standard library), not from the Rust
// signer. A finite vector is a regression case, not a general non-collision or security proof.
// ---------------------------------------------------------------------------------------------

/// Test-only device key: 64 hex characters, like a generated one. Never a production value.
const VECTOR_KEY: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";
/// `T + 90 days` (the default retention), as every vector's expiry.
const VECTOR_EXPIRY: i64 = T + 90 * 86400;
const NEVER_SENT: u128 = 350;
mod golden {
    //! `python3 -I crates/core/tests/fixtures/q23_continuation_vectors.py`, device `reader`.
    pub const START: &str = "d1.0000000000000002.000000006b169fc0.1f1994f82dc7c0a2e796cb18ac811a20046e86a8e78d72074b3b9fa608949f1b";
    pub const DONE: &str = "d1.0000000000000003.000000006b169fc0.33caaa7472536ab2f99f5a4c7739a356b4aa0b83c118eeab1df026f348b59bde";
    pub const I1_U4: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000001.0c166316.38915ffeedc4377e88b70ae62960246a720499804a8cc7d49529ecab4de63a18";
    pub const I1_U5: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000001.9261a0b8.b54819fb981978ca7e79968698b04d4832d3f4d1172a5fba7ff6faa31dc2fd76";
    pub const I1_U6: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000001.2be62377.08b1e6c60ebe44142bd91f31b446b8873004eeaefca573e1c3388937f4989f07";
    pub const I1_U7: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000001.0ad2c4bb.9504c8bbdf7dd35763d2d5c73f7f34f3cf59ca8fde21489997ca5c2ce1ac1b04";
    pub const I2_U7: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000002.643bcdfa.e4bea02e6ef95b02b79dfcd4819204e994f0a35e5c6b6757678dce2440cbc6fe";
    pub const I2_U8: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000002.13d3e3be.b75604f9378e4646447e43fea365450dc7d3c23f8ca2c860e4ca92b42fcb32ae";
    pub const I2_U9: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000002.e87db10d.c5813fd33f5ffc279c7e982339dc29e9ca5dcf4fe2844499870a722f47f6614f";
    pub const I2_U10: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000002.d0e8238d.de74b65d32d94265cbf8447d544f36fd7110abef14a540c712d276973e64b83d";
    pub const I3_R4: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000003.61f7002c.34b9aa2b5604ac84490dc679ab9edc786823f89f6cb3a6560bfd5853f4131a0f";
    pub const I3_R5: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000003.8b0784f2.26664b9b73e4dd8f40c27fdca77cdb9ec3ad3def7a2c0d058a3b94a15f3d1a4a";
    pub const I3_U10: &str = "d2.0000000000000002.000000006b169fc0.0000000000000003.00000003.556de9d8.78902db048218e234787ee5852456128f33be4b3546c333bd1e018db9ad6eff1";
}

fn fixed(n: u128) -> String {
    Uuid::from_u128(n).to_string()
}

/// The fixed owner (`…a0`) and reader (`…b0`), with the reader's `device` registered and its key
/// replaced by [`VECTOR_KEY`] before any delta cursor or continuation is issued.
async fn fixed_accounts(store: &Store, device: &str) -> Result<(String, String)> {
    let (a, b) = (fixed(0xA0), fixed(0xB0));
    store.add_account(&a, "q23-owner", "test-only").await?;
    store.add_account(&b, "q23-reader", "test-only").await?;
    // Registration generates a random key; the only token it signs here is this snapshot's
    // stored UUID, which no vector uses.
    store.sync(&b, device, None, 200, T).await?;
    sqlx::query("UPDATE sync_devices SET cursor_key=$1 WHERE account_id=$2 AND device_id=$3")
        .bind(VECTOR_KEY)
        .bind(&b)
        .bind(device)
        .execute(&store.pool)
        .await?;
    assert_eq!(cursor_key(store, &b, device).await?, VECTOR_KEY);
    Ok((a, b))
}

async fn fixed_apply(store: &Store, actor: &str, op: u128, commands: &[Command]) -> Result<i64> {
    store.apply(actor, &fixed(0xF000 + op), commands).await
}

/// One fixed history: revision 1 creates person `…01` and fields `…64` (100) to `…4b0` (1200),
/// shared with the reader, plus `NEVER_SENT` (between the third and fourth field) when `hidden`;
/// revision 2 revokes the person; the reader then anchors at boundary 2; revision 3 grants the
/// person again; with `hidden`, revision 4 revokes `NEVER_SENT` before any delta request. The
/// reader's notices for revision 3 are the person, then the fields (and `NEVER_SENT`) in id order.
struct History {
    _dir: tempfile::TempDir,
    store: Store,
    owner: String,
    reader: String,
    person: String,
    fields: Vec<String>,
    start: String,
}

async fn history(hidden: bool) -> Result<History> {
    let (dir, store) = fixture().await?;
    let (a, b) = fixed_accounts(&store, "reader").await?;
    let person = fixed(1);
    let fields: Vec<String> = (1..=12).map(|i| fixed(i * 100)).collect();
    let never_sent = fixed(NEVER_SENT);
    let mut commands = vec![Command::CreatePerson {
        id: person.clone(),
        name: "Vector".into(),
        initial_policy: Some(share(&b)),
    }];
    for f in fields.iter().chain(hidden.then_some(&never_sent)) {
        commands.push(Command::CreateField {
            id: f.clone(),
            person_id: person.clone(),
            label: "Shared".into(),
            value: "Shared value".into(),
            initial_policy: Some(share(&b)),
        });
    }
    assert_eq!(fixed_apply(&store, &a, 1, &commands).await?, 1);
    let policy = versions(&store, &person).await?.1;
    let revoked = fixed_apply(&store, &a, 2, &[revoke_command(&person, policy, &b)]).await?;
    assert_eq!(revoked, 2);
    let start = head(&store, &b, "reader", T).await?;
    let grant = Command::Grant {
        id: person.clone(),
        expected_version: versions(&store, &person).await?.1,
        account_id: b.clone(),
        edit: false,
    };
    assert_eq!(fixed_apply(&store, &a, 3, &[grant]).await?, 3);
    if hidden {
        let policy = versions(&store, &never_sent).await?.1;
        let hide = revoke_command(&never_sent, policy, &b);
        assert_eq!(fixed_apply(&store, &a, 4, &[hide]).await?, 4);
    }
    Ok(History {
        _dir: dir,
        store,
        owner: a,
        reader: b,
        person,
        fields,
        start,
    })
}

impl History {
    async fn piece(&self, cursor: &str, m: usize) -> Result<Page> {
        self.store
            .sync_bounded(&self.reader, "reader", Some(cursor), 1, Some(m), T)
            .await
    }
    /// Asserts the response's next cursor, opened with the device key, before its golden value:
    /// its header and plain position, then the exact token.
    fn expect(&self, page: &Page, index: u32, plain: u32, golden: &str, label: &str) {
        assert!(
            page.next_cursor.starts_with("d2."),
            "{label}: a continuation after {:?}",
            Self::ids(page)
        );
        let (header, position) = open(VECTOR_KEY, &self.reader, "reader", &page.next_cursor);
        assert_eq!(
            header,
            format!("d2.{:016x}.{VECTOR_EXPIRY:016x}.{:016x}.{index:08x}", 2, 3),
            "{label}: header"
        );
        assert_eq!(position, plain, "{label}: plain position");
        assert_eq!(page.next_cursor, golden, "{label}: golden token");
    }
    fn ids(page: &Page) -> Vec<String> {
        page.batches
            .iter()
            .flat_map(|b| &b.changes)
            .map(|c| match c {
                Change::Upsert { resource } => format!("+{}", resource.id),
                Change::Remove { id } => format!("-{id}"),
            })
            .collect()
    }
}

/// Plain positions at one public header never cancel: the sealed fields' XOR differs from the
/// positions' XOR. Checked before any golden comparison, so a common-pad construction fails here.
fn no_cancellation(tokens: &[(&str, u32, &str)]) {
    for (i, (x, a, lx)) in tokens.iter().enumerate() {
        for (y, b, ly) in &tokens[i + 1..] {
            assert_eq!(
                x.rsplitn(3, '.').nth(2),
                y.rsplitn(3, '.').nth(2),
                "{lx} / {ly}: same public header"
            );
            if a != b {
                assert_ne!(
                    sealed(x) ^ sealed(y),
                    a ^ b,
                    "privacy: {lx} / {ly} cancel to the positions' XOR"
                );
            } else {
                assert_eq!(x, y, "{lx} / {ly}: identical inputs, identical token");
            }
        }
    }
}

/// Two bounds, then for each history: index-1 position and token, index-2 position and token.
type Chain = ((usize, usize), [(u32, &'static str, u32, &'static str); 2]);

/// DA-H1/Q6 upsert pass: two fixed histories (without and with a never-sent hidden notice) and
/// four multi-piece prefixes reach the same public revision and index with the declared plain
/// positions and golden tokens. Different positions never cancel; the same position, reached by
/// different prefixes or histories, gives the same token. Each traversal completes to the whole
/// batch, and a retried request at the same time returns the same response and ledger.
#[tokio::test]
async fn continuation_vectors_upsert_pass() -> Result<()> {
    use golden::*;
    let never_sent = fixed(NEVER_SENT);
    // (first bound, second bound) -> index 1 and index 2 plain positions, without/with hidden.
    let chains: [Chain; 4] = [
        ((4, 4), [(4, I1_U4, 7, I2_U7), (5, I1_U5, 8, I2_U8)]),
        ((4, 5), [(4, I1_U4, 8, I2_U8), (5, I1_U5, 9, I2_U9)]),
        ((5, 4), [(5, I1_U5, 8, I2_U8), (6, I1_U6, 9, I2_U9)]),
        ((6, 4), [(6, I1_U6, 9, I2_U9), (7, I1_U7, 10, I2_U10)]),
    ];
    let mut index2 = Vec::new();
    let mut bodies = Vec::new();
    for (h, hidden) in [false, true].into_iter().enumerate() {
        let history = history(hidden).await?;
        assert_eq!(history.start, START, "the anchor is a fixed vector");
        let mut whole_text = String::new();
        for ((m1, m2), expected) in &chains {
            let (p1, g1, p2, g2) = expected[h];
            let label = format!("history {h}, bounds {m1}/{m2}");
            let first = history.piece(&history.start, *m1).await?;
            history.expect(&first, 1, p1, g1, &format!("{label} index 1"));
            let second = history.piece(&first.next_cursor, *m2).await?;
            index2.push((second.next_cursor.clone(), p2, label.clone()));
            history.expect(&second, 2, p2, g2, &format!("{label} index 2"));
            for page in [&first, &second] {
                whole_text.push_str(&serde_json::to_string(page)?);
                assert_eq!(History::ids(page)[0], format!("+{}", history.person));
            }
            if (*m1, *m2) == (4, 4) {
                bodies.push((History::ids(&first), History::ids(&second)));
            }
        }
        assert!(!whole_text.contains(&never_sent), "never disclosed");
        // Complete traversal with bound 4 from the anchor reassembles the whole batch, and the
        // last piece of revision 3 returns the fixed `d1` cursor.
        let (pieces, _) = drain(
            &history.store,
            &history.reader,
            "reader",
            Some(history.start.clone()),
            1,
            Some(4),
            T,
        )
        .await?;
        let last = pieces
            .iter()
            .find(|p| p.batches[0].segment.is_some_and(|s| s.last))
            .expect("a last piece");
        assert_eq!(last.next_cursor, DONE);
        // The reference: the same anchor, without the bound, at the same time.
        let (whole, _) = drain(
            &history.store,
            &history.reader,
            "reader",
            Some(history.start.clone()),
            1,
            None,
            T,
        )
        .await?;
        let whole: Vec<Page> = whole
            .into_iter()
            .filter(|p| p.batches.iter().any(|b| b.revision == 3))
            .collect();
        let pieces: Vec<Page> = pieces
            .into_iter()
            .filter(|p| p.batches.iter().any(|b| b.revision == 3))
            .collect();
        reassembles(&whole, &pieces, 4);
        // Retry of a committed piece at the same time: same response, same token, same ledger.
        let ledger_before = ledger(&history.store, &history.reader, "reader").await?;
        let again = history.piece(&history.start, 4).await?;
        let retry = history.piece(&history.start, 4).await?;
        assert_eq!(
            serde_json::to_string(&again)?,
            serde_json::to_string(&retry)?
        );
        assert_eq!(retry.next_cursor, [I1_U4, I1_U5][h]);
        assert_eq!(
            ledger(&history.store, &history.reader, "reader").await?,
            ledger_before
        );
    }
    let refs: Vec<(&str, u32, &str)> = index2
        .iter()
        .map(|(t, p, l)| (t.as_str(), *p, l.as_str()))
        .collect();
    no_cancellation(&refs);
    // The hidden notice changes positions, never the visible bodies.
    assert_eq!(bodies[0], bodies[1], "identical visible pieces");
    Ok(())
}

/// DA-H1/Q6 removal pass, and the revocation and merge controls. After the reader receives two
/// pieces (the person and six fields), the owner hides the person: the rest of revision 3 is a
/// removal pass whose index-3 continuation has removal position 4 without the hidden notice and 5
/// with it (never delivered, so never removed), at the same public header and with the same
/// bodies. A revocation of a delivered field before a repeated first piece moves its window to
/// position 5, the same token the hidden-notice history gives, without sending that field.
#[tokio::test]
async fn continuation_vectors_removal_pass_and_controls() -> Result<()> {
    use golden::*;
    let never_sent = fixed(NEVER_SENT);
    let mut index3 = Vec::new();
    let mut bodies = Vec::new();
    for (h, hidden) in [false, true].into_iter().enumerate() {
        let history = history(hidden).await?;
        assert_eq!(history.start, START, "the anchor is a fixed vector");
        let f = &history.fields;
        let first = history.piece(&history.start, 4).await?;
        let second = history.piece(&first.next_cursor, 4).await?;
        history.expect(&second, 2, [7, 8][h], [I2_U7, I2_U8][h], "prefix");
        let delivered = ledger(&history.store, &history.reader, "reader").await?;
        let policy = versions(&history.store, &history.person).await?.1;
        let hide = revoke_command(&history.person, policy, &history.reader);
        fixed_apply(&history.store, &history.owner, 5, &[hide]).await?;
        let third = history.piece(&second.next_cursor, 4).await?;
        let label = format!("history {h} index 3");
        let (position, golden) = [(4, I3_R4), (5, I3_R5)][h];
        index3.push((
            third.next_cursor.clone(),
            (1 << 16) | position,
            label.clone(),
        ));
        history.expect(&third, 3, (1 << 16) | position, golden, &label);
        assert_eq!(
            History::ids(&third),
            f[..4].iter().map(|id| format!("-{id}")).collect::<Vec<_>>(),
            "{label}: removals of delivered fields only"
        );
        let last = history.piece(&third.next_cursor, 4).await?;
        assert_eq!(
            History::ids(&last),
            [&f[4], &f[5], &history.person]
                .map(|id| format!("-{id}"))
                .to_vec(),
            "history {h}: the remaining delivered resources"
        );
        assert_eq!(
            last.batches[0].segment.map(|s| (s.index, s.last)),
            Some((3, true)),
            "the index-3 continuation returns the last piece"
        );
        assert_eq!(last.next_cursor, DONE);
        // Removals keep the ledger: a lost removal response can be retried.
        assert_eq!(
            ledger(&history.store, &history.reader, "reader").await?,
            delivered
        );
        let text = [&first, &second, &third, &last]
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<String, _>>()?;
        assert!(!text.contains(&never_sent), "history {h}: never disclosed");
        bodies.push(
            [&first, &second, &third, &last]
                .iter()
                .map(|p| serde_json::to_string(&p.batches))
                .collect::<Result<String, _>>()?,
        );
    }
    let refs: Vec<(&str, u32, &str)> = index3
        .iter()
        .map(|(t, p, l)| (t.as_str(), *p, l.as_str()))
        .collect();
    no_cancellation(&refs);
    assert_eq!(bodies[0], bodies[1], "the hidden notice changes no body");

    // Revocation between requests: a delivered field hidden, then the first piece repeated.
    let history = history(false).await?;
    assert_eq!(history.start, START, "the anchor is a fixed vector");
    let first = history.piece(&history.start, 4).await?;
    history.expect(&first, 1, 4, I1_U4, "before revocation");
    let policy = versions(&history.store, &history.fields[0]).await?.1;
    let hide = revoke_command(&history.fields[0], policy, &history.reader);
    fixed_apply(&history.store, &history.owner, 5, &[hide]).await?;
    let repeat = history.piece(&history.start, 4).await?;
    history.expect(&repeat, 1, 5, I1_U5, "after revocation");
    assert_eq!(
        History::ids(&repeat),
        [
            &history.person,
            &history.fields[1],
            &history.fields[2],
            &history.fields[3]
        ]
        .map(|id| format!("+{id}"))
        .to_vec()
    );
    no_cancellation(&[
        (first.next_cursor.as_str(), 4, "before revocation"),
        (repeat.next_cursor.as_str(), 5, "after revocation"),
    ]);
    Ok(())
}

/// DA-H1/Q6 binding: the same fixed key installed (test-only) on another device of the reader and
/// on a device of the same name for another account does not make the reader's anchor or
/// continuation valid there; the MAC binds account and device, not only the key. The rightful
/// device then continues with the declared vector.
#[tokio::test]
async fn continuation_binding_with_shared_key() -> Result<()> {
    use golden::*;
    let history = history(false).await?;
    let first = history.piece(&history.start, 4).await?;
    // Goldens are checked after the refusals, so a binding fault fails at its own assertion.
    let other = fixed(0xC0);
    history
        .store
        .add_account(&other, "q23-other", "test-only")
        .await?;
    for (actor, device) in [(history.reader.clone(), "other"), (other, "reader")] {
        history.store.sync(&actor, device, None, 200, T).await?;
        sqlx::query("UPDATE sync_devices SET cursor_key=$1 WHERE account_id=$2 AND device_id=$3")
            .bind(VECTOR_KEY)
            .bind(&actor)
            .bind(device)
            .execute(&history.store.pool)
            .await?;
        for token in [&history.start, &first.next_cursor] {
            let result = history
                .store
                .sync_bounded(&actor, device, Some(token), 1, Some(4), T)
                .await;
            assert_eq!(
                result
                    .map(|page| page.next_cursor)
                    .map_err(|e| e.to_string()),
                Err("resync_required".into()),
                "binding: {actor}/{device} with the same key"
            );
        }
    }
    let second = history.piece(&first.next_cursor, 4).await?;
    assert_eq!(history.start, START, "the anchor is a fixed vector");
    history.expect(&first, 1, 4, I1_U4, "first");
    history.expect(&second, 2, 7, I2_U7, "the rightful device");
    Ok(())
}

/// DA-H1/Q6 merge between pieces: after two pieces, the owner merges the shared person into a new
/// person (`…02`), which takes over its fields and sharing. Access is re-resolved: the next piece
/// carries the target as the fields' parent, never the merged person's stale projection, at the
/// declared upsert position 10; the next piece completes the revision with the fixed `d1` cursor
/// (the merged person is settled by the merge's own revision), and the reader converges.
#[tokio::test]
async fn continuation_vectors_merge_between_pieces() -> Result<()> {
    use golden::*;
    let history = history(false).await?;
    assert_eq!(history.start, START, "the anchor is a fixed vector");
    let f = &history.fields;
    let first = history.piece(&history.start, 4).await?;
    let second = history.piece(&first.next_cursor, 4).await?;
    history.expect(&second, 2, 7, I2_U7, "before merge");
    let target = fixed(2);
    fixed_apply(
        &history.store,
        &history.owner,
        5,
        &[Command::CreatePerson {
            id: target.clone(),
            name: "Private".into(),
            initial_policy: None,
        }],
    )
    .await?;
    let preview = history
        .store
        .merge_preview(&history.owner, &history.person, &target)
        .await?;
    history
        .store
        .people_command(
            &history.owner,
            &fixed(0xF006),
            &PeopleCommand::Merge {
                source_id: history.person.clone(),
                target_id: target.clone(),
                preview_token: preview.token,
                name: "Merged".into(),
            },
            T,
        )
        .await?;
    let third = history.piece(&second.next_cursor, 4).await?;
    history.expect(&third, 3, 10, I3_U10, "after merge");
    assert_eq!(
        History::ids(&third),
        [&target, &f[6], &f[7], &f[8]]
            .map(|id| format!("+{id}"))
            .to_vec()
    );
    for change in &third.batches[0].changes[1..] {
        let Change::Upsert { resource } = change else {
            panic!("an upsert")
        };
        assert_eq!(resource.parent_id.as_ref(), Some(&target), "re-parented");
    }
    let fourth = history.piece(&third.next_cursor, 4).await?;
    assert_eq!(
        History::ids(&fourth),
        [&target, &f[9], &f[10], &f[11]]
            .map(|id| format!("+{id}"))
            .to_vec()
    );
    // The merged person is still visible to revision 3's removal pass, so it is not removed
    // there; the merge's own later revision settles it.
    assert_eq!(
        fourth.batches[0].segment.map(|s| (s.index, s.last)),
        Some((3, true))
    );
    assert_eq!(fourth.next_cursor, DONE);
    // Eventual whole-reference convergence: staged delivery plus the later revisions equals a
    // fresh snapshot of the reader at the same time.
    let mut staging = Client {
        staging: true,
        ..Client::default()
    };
    for page in [&first, &second, &third, &fourth] {
        staging.receive(page);
    }
    for page in drain(
        &history.store,
        &history.reader,
        "reader",
        Some(DONE.into()),
        200,
        Some(4),
        T,
    )
    .await?
    .0
    {
        staging.receive(&page);
    }
    let (fresh, _) = drain(&history.store, &history.reader, "fresh", None, 200, None, T).await?;
    let mut reference = Client::default();
    fresh.iter().for_each(|p| reference.receive(p));
    assert_eq!(staging.live, reference.live, "staged delivery converges");
    assert!(staging.staged.is_empty());
    assert!(
        reference
            .live
            .values()
            .filter(|p| p.kind == "field")
            .all(|p| p.parent_id.as_deref() == Some(target.as_str())),
        "the merge re-parented the fields"
    );
    no_cancellation(&[
        (third.next_cursor.as_str(), 10, "upsert 10"),
        (I3_R4, (1 << 16) | 4, "removal 4"),
        (I3_R5, (1 << 16) | 5, "removal 5"),
    ]);
    Ok(())
}

/// The `d1` payload with the continuation's boundary and expiry.
fn first_d1(token: &str) -> String {
    let parts: Vec<_> = token.split('.').collect();
    format!("d1.{}.{}", parts[1], parts[2])
}

/// The state a client reaches by applying every whole batch at once and every split revision
/// only when its last piece arrives (contract §4), or, for the control, each piece at once.
#[derive(Default)]
struct Client {
    live: BTreeMap<String, Projection>,
    staged: Vec<Change>,
    staging: bool,
    partial: bool,
}
impl Client {
    fn apply(&mut self, changes: &[Change]) {
        for change in changes {
            match change {
                Change::Upsert { resource } => {
                    self.live.insert(resource.id.clone(), resource.clone());
                }
                Change::Remove { id } => {
                    self.live.remove(id);
                }
            }
        }
    }
    /// After each whole batch or activated revision, every parent is present (revision 1).
    fn whole(&self) {
        if let Some(orphan) = self.live.values().find(|p| {
            p.parent_id
                .as_ref()
                .is_some_and(|parent| !self.live.contains_key(parent))
        }) {
            panic!("{} is applied without its parent", orphan.id);
        }
    }
    fn receive(&mut self, page: &Page) {
        for batch in &page.batches {
            match batch.segment {
                None => {
                    self.apply(&batch.changes);
                    if self.staging {
                        self.whole();
                    }
                }
                Some(segment) if self.staging => {
                    self.staged.extend(batch.changes.iter().cloned());
                    if segment.last {
                        let staged = std::mem::take(&mut self.staged);
                        self.apply(&staged);
                        self.whole();
                    }
                }
                Some(segment) => {
                    self.apply(&batch.changes);
                    // A reader of the live state now sees part of a revision.
                    self.partial |= !segment.last;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Action {
    EditDelivered,
    RevokeDelivered,
    RevokeUndelivered,
    RevokePerson,
    RegrantPerson,
    AddField,
    JoinHousehold,
    LeaveHousehold,
    /// Merge the shared person into a private one of its owner, re-parenting its fields.
    MergePerson,
}

/// F7, F9, I5, I8: writers commit between pieces, including a person merge (revision 1); a
/// staging client converges to a fresh snapshot, never receives a hidden undelivered resource,
/// and the live-applying control exposes a partial revision.
#[tokio::test]
async fn concurrent_writers_converge() -> Result<()> {
    use Action::*;
    for schedule in [
        &[(1, EditDelivered)][..],
        &[(1, RevokeDelivered)],
        &[(1, RevokeUndelivered)],
        &[(2, RevokePerson)],
        &[(1, RevokePerson), (2, RegrantPerson)],
        &[(1, AddField), (3, EditDelivered)],
        &[(1, JoinHousehold)],
        &[(1, JoinHousehold), (2, LeaveHousehold)],
        &[(1, MergePerson)],
        &[
            (0, RevokeUndelivered),
            (1, RevokeDelivered),
            (2, EditDelivered),
            (3, AddField),
        ],
    ] {
        converge(schedule).await?;
    }
    Ok(())
}

async fn converge(schedule: &[(usize, Action)]) -> Result<()> {
    use Action::*;
    let (_d, store) = fixture().await?;
    let (a, b) = (account(&store).await?, account(&store).await?);
    let (person, fields) = hidden_person(&store, &a, &b, 24).await?;
    // A second person reaches `b` only through a household.
    let home = id();
    store
        .management(
            &a,
            &id(),
            &[M::CreateHousehold {
                id: home.clone(),
                name: "Home".into(),
            }],
            T,
        )
        .await?;
    let other = id();
    apply(
        &store,
        &a,
        &[Command::CreatePerson {
            id: other.clone(),
            name: "Household".into(),
            initial_policy: Some(Policy {
                grants: vec![PrincipalGrant::Household {
                    id: home.clone(),
                    edit: false,
                }],
                exclude_accounts: vec![],
            }),
        }],
    )
    .await?;
    let mut staging = Client {
        staging: true,
        ..Client::default()
    };
    let mut live = Client::default();
    let (pages, start) = drain(&store, &b, "staging", None, 200, Some(5), T).await?;
    let (live_pages, live_start) = drain(&store, &b, "live", None, 200, Some(5), T).await?;
    pages.iter().for_each(|p| staging.receive(p));
    live_pages.iter().for_each(|p| live.receive(p));
    // 25 changes at m = 5: six pieces of the person and four fields.
    grant(&store, &a, &person, &b).await?;
    let (mut cursor, mut live_cursor) = (start, live_start);
    let mut bodies = String::new();
    let mut merged = None;
    let mut step = 0;
    loop {
        let page = store
            .sync_bounded(&b, "staging", Some(&cursor), 200, Some(5), T + 1)
            .await?;
        let live_page = store
            .sync_bounded(&b, "live", Some(&live_cursor), 200, Some(5), T + 1)
            .await?;
        bodies.push_str(&serde_json::to_string(&page)?);
        staging.receive(&page);
        live.receive(&live_page);
        for (_, action) in schedule.iter().filter(|(at, _)| *at == step) {
            match action {
                EditDelivered => edit(&store, &a, &fields[0], "Edited mid-chain")
                    .await
                    .map(drop)?,
                RevokeDelivered => revoke(&store, &a, &fields[0], &b).await.map(drop)?,
                RevokeUndelivered => revoke(&store, &a, &fields[23], &b).await.map(drop)?,
                RevokePerson => revoke(&store, &a, &person, &b).await.map(drop)?,
                RegrantPerson => grant(&store, &a, &person, &b).await.map(drop)?,
                AddField => apply(
                    &store,
                    &a,
                    &[Command::CreateField {
                        id: id(),
                        person_id: person.clone(),
                        label: "Added".into(),
                        value: "Mid-chain".into(),
                        initial_policy: Some(share(&b)),
                    }],
                )
                .await
                .map(drop)?,
                JoinHousehold => join(&store, &a, &b, &home).await?,
                LeaveHousehold => {
                    let version = household_version(&store, &a, &home).await?;
                    store
                        .management(
                            &a,
                            &id(),
                            &[M::RemoveHouseholdMember {
                                household_id: home.clone(),
                                account_id: b.clone(),
                                expected_version: version,
                            }],
                            T + 1,
                        )
                        .await
                        .map(drop)?
                }
                MergePerson => {
                    let target = id();
                    apply(
                        &store,
                        &a,
                        &[Command::CreatePerson {
                            id: target.clone(),
                            name: "Private".into(),
                            initial_policy: None,
                        }],
                    )
                    .await?;
                    let preview = store.merge_preview(&a, &person, &target).await?;
                    store
                        .people_command(
                            &a,
                            &id(),
                            &PeopleCommand::Merge {
                                source_id: person.clone(),
                                target_id: target.clone(),
                                preview_token: preview.token,
                                name: "Merged".into(),
                            },
                            T + 1,
                        )
                        .await?;
                    merged = Some(target);
                }
            }
        }
        cursor = page.next_cursor.clone();
        live_cursor = live_page.next_cursor.clone();
        step += 1;
        if !page.has_more && !live_page.has_more {
            break;
        }
    }
    // A last drain picks up anything committed after the final piece.
    for page in drain(&store, &b, "staging", Some(cursor), 200, Some(5), T + 2)
        .await?
        .0
    {
        bodies.push_str(&serde_json::to_string(&page)?);
        staging.receive(&page);
    }
    let (fresh, _) = drain(&store, &b, "fresh", None, 200, None, T + 2).await?;
    let mut reference = Client::default();
    fresh.iter().for_each(|p| reference.receive(p));
    assert_eq!(
        staging.live, reference.live,
        "{schedule:?}: staged delivery converges"
    );
    assert!(staging.staged.is_empty());
    if let Some(target) = merged {
        // The merge is visible to the reader: its fields now hang under the target.
        assert!(reference.live.contains_key(&target));
        assert!(
            reference
                .live
                .values()
                .filter(|p| p.kind == "field")
                .all(|p| p.parent_id.as_deref() == Some(target.as_str())),
            "the merge re-parented the fields"
        );
    }
    assert!(
        live.partial,
        "{schedule:?}: applying pieces live exposes a partial revision"
    );
    if schedule
        .iter()
        .any(|(at, a)| matches!(a, RevokeUndelivered) && *at <= 1)
    {
        // The bodies include every cursor.
        assert!(
            !bodies.contains(&fields[23]),
            "an undelivered hidden field is never disclosed"
        );
    }
    Ok(())
}
async fn household_version(store: &Store, actor: &str, household: &str) -> Result<i64> {
    Ok(store
        .households(actor)
        .await?
        .into_iter()
        .find(|h| h.id == household)
        .expect("a member")
        .version)
}
async fn join(store: &Store, owner: &str, other: &str, household: &str) -> Result<()> {
    let invitation = id();
    let expected_version = household_version(store, owner, household).await?;
    store
        .management(
            owner,
            &id(),
            &[M::InviteToHousehold {
                id: invitation.clone(),
                household_id: household.into(),
                recipient_id: other.into(),
                expected_version,
            }],
            T + 1,
        )
        .await?;
    store
        .management(
            other,
            &id(),
            &[M::RespondToHouseholdInvitation {
                id: invitation,
                expected_version: 1,
                accept: true,
            }],
            T + 1,
        )
        .await?;
    Ok(())
}

/// F9, I5: a field hidden before its slice is never sent and no removal follows; one delivered
/// and hidden before the removal pass is removed in the same revision.
#[tokio::test]
async fn access_is_checked_per_piece() -> Result<()> {
    let (_d, store) = fixture().await?;
    let (a, b) = (account(&store).await?, account(&store).await?);
    let (person, fields) = hidden_person(&store, &a, &b, 12).await?;
    let start = head(&store, &b, "reader", T).await?;
    grant(&store, &a, &person, &b).await?;
    let first = store
        .sync_bounded(&b, "reader", Some(&start), 200, Some(4), T)
        .await?;
    let sent: Vec<String> = upserts(&first.batches[0].changes)
        .iter()
        .map(|p| p.id.clone())
        .collect();
    let delivered = sent
        .iter()
        .find(|id| **id != person)
        .expect("a field")
        .clone();
    // The next candidate after the first piece: an id-keyed continuation would name it.
    let hidden = fields
        .iter()
        .find(|f| !sent.contains(f))
        .expect("an unsent field")
        .clone();
    revoke(&store, &a, &hidden, &b).await?;
    revoke(&store, &a, &delivered, &b).await?;
    let (rest, _) = drain(
        &store,
        &b,
        "reader",
        Some(first.next_cursor.clone()),
        200,
        Some(4),
        T,
    )
    .await?;
    let text = rest
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<String, _>>()?
        + &first.next_cursor
        + &rest
            .iter()
            .map(|p| p.next_cursor.clone())
            .collect::<String>();
    assert!(
        !text.contains(&hidden),
        "never disclosed, in a body or a cursor"
    );
    let pieces: Vec<&Batch> = rest
        .iter()
        .flat_map(|p| &p.batches)
        .filter(|b| b.segment.is_some())
        .collect();
    let removed: Vec<&String> = pieces.iter().flat_map(|b| removals(&b.changes)).collect();
    assert_eq!(
        removed,
        [&delivered],
        "the delivered field is removed in this revision's pass"
    );
    // Each piece still carries the person before its fields.
    for piece in pieces.iter().filter(|b| !upserts(&b.changes).is_empty()) {
        assert_eq!(upserts(&piece.changes)[0].id, person);
    }
    // Parent hidden mid-chain: later pieces carry no orphan field, and delivered ones are removed.
    let (_d, store) = fixture().await?;
    let (a, b) = (account(&store).await?, account(&store).await?);
    let (person, _) = hidden_person(&store, &a, &b, 12).await?;
    let start = head(&store, &b, "reader", T).await?;
    grant(&store, &a, &person, &b).await?;
    let first = store
        .sync_bounded(&b, "reader", Some(&start), 200, Some(4), T)
        .await?;
    revoke(&store, &a, &person, &b).await?;
    let (rest, _) = drain(
        &store,
        &b,
        "reader",
        Some(first.next_cursor.clone()),
        200,
        Some(4),
        T,
    )
    .await?;
    let piece = &rest[0].batches[0];
    assert!(
        upserts(&piece.changes).is_empty(),
        "no field without its person"
    );
    let delivered: BTreeSet<String> = upserts(&first.batches[0].changes)
        .iter()
        .map(|p| p.id.clone())
        .collect();
    let removed: BTreeSet<String> = rest
        .iter()
        .flat_map(|p| &p.batches)
        .filter(|b| b.segment.is_some())
        .flat_map(|b| removals(&b.changes))
        .cloned()
        .collect();
    assert_eq!(
        removed, delivered,
        "everything delivered from the revision is removed"
    );
    Ok(())
}

/// N4: a continuation without the bound returns the rest at once; a different bound keeps the
/// index; trailing invisible candidates give an empty last piece; an access-epoch change does
/// not invalidate a continuation; values outside 4..=800 are rejected; a lost response is
/// retried exactly.
#[tokio::test]
async fn continuation_edge_cases() -> Result<()> {
    let (_d, store) = fixture().await?;
    let (_, b, _, first) = chain(&store).await?;
    let rest = store
        .sync_bounded(&b, "reader", Some(&first.next_cursor), 200, None, T)
        .await?;
    assert_eq!(
        count(&rest),
        21 - 5,
        "the person again and the 15 remaining fields"
    );
    assert_eq!(
        rest.batches[0].segment.map(|s| (s.index, s.last)),
        Some((1, true))
    );
    assert!(rest.next_cursor.starts_with("d1.") && !rest.has_more);
    let smaller = store
        .sync_bounded(&b, "reader", Some(&first.next_cursor), 200, Some(4), T)
        .await?;
    assert_eq!(smaller.batches[0].segment.map(|s| s.index), Some(1));
    assert_eq!(count(&smaller), 4);
    let larger = store
        .sync_bounded(&b, "reader", Some(&smaller.next_cursor), 200, Some(10), T)
        .await?;
    assert_eq!(
        larger.batches[0].segment.map(|s| (s.index, s.last)),
        Some((2, false))
    );
    // Bounds outside 4..=800.
    for m in [0, 1, 3, 801, usize::MAX] {
        let error = store
            .sync_bounded(&b, "reader", Some(&first.next_cursor), 200, Some(m), T)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "invalid_value", "{m}");
    }

    // Every remaining candidate hidden: the last piece is empty; all upserts hidden: removals only.
    let (_d, store) = fixture().await?;
    let (a, b, person, first) = chain(&store).await?;
    let delivered = upserts(&first.batches[0].changes).len();
    let remaining: Vec<String> =
        sqlx::query_scalar("SELECT id FROM resources WHERE parent_id=$1 ORDER BY id")
            .bind(&person)
            .fetch_all(&store.pool)
            .await?;
    for field in remaining.iter().skip(delivered - 1) {
        revoke(&store, &a, field, &b).await?;
    }
    let last = store
        .sync_bounded(&b, "reader", Some(&first.next_cursor), 200, Some(6), T)
        .await?;
    assert_eq!(last.batches[0].changes, [], "an empty final piece");
    assert_eq!(
        last.batches[0].segment.map(|s| (s.index, s.last)),
        Some((1, true))
    );
    // An access-epoch change mid-chain (an edit permission change) gives no `access_changed`.
    let (_d, store) = fixture().await?;
    let (a, b, person, first) = chain(&store).await?;
    let command = Command::Grant {
        id: person.clone(),
        expected_version: versions(&store, &person).await?.1,
        account_id: b.clone(),
        edit: true,
    };
    let epoch = |store: Store, b: String| async move {
        anyhow::Ok(
            sqlx::query_scalar::<_, i64>("SELECT access_epoch FROM accounts WHERE id=$1")
                .bind(b)
                .fetch_one(&store.pool)
                .await?,
        )
    };
    let before = epoch(store.clone(), b.clone()).await?;
    apply(&store, &a, &[command]).await?;
    assert!(
        epoch(store.clone(), b.clone()).await? > before,
        "the epoch moved"
    );
    let next = store
        .sync_bounded(&b, "reader", Some(&first.next_cursor), 200, Some(6), T)
        .await?;
    assert_eq!(next.batches[0].segment.map(|s| s.index), Some(1));
    assert!(
        upserts(&next.batches[0].changes)
            .iter()
            .all(|p| p.id != person || p.can_edit)
    );
    // Lost response: the same continuation again, no duplicate ledger rows.
    let rows = ledger(&store, &b, "reader").await?;
    let again = store
        .sync_bounded(&b, "reader", Some(&first.next_cursor), 200, Some(6), T)
        .await?;
    assert_eq!(
        serde_json::to_string(&again)?,
        serde_json::to_string(&next)?
    );
    assert_eq!(ledger(&store, &b, "reader").await?, rows);
    Ok(())
}
