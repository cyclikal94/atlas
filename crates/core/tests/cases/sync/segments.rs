//! Bounded `/sync` delivery (BE-Q23, contract v1): opt-in `max_changes`, revision pieces and
//! `d2` continuations. Each piece sequence is compared with the whole batch the same server
//! returns without the bound, at the same state.
use anyhow::Result;
use atlas_core::{
    Batch, Change, Command, Page, Projection, Store,
    households::ManagementCommand as M,
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
    let key: String = sqlx::query_scalar(
        "SELECT cursor_key FROM sync_devices WHERE account_id=$1 AND device_id=$2",
    )
    .bind(&b)
    .bind("reader")
    .fetch_one(&store.pool)
    .await?;
    let sign = |domain: &str, payload: &str| {
        let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("key");
        mac.update(format!("{domain}\n{b}\n6:reader\n{payload}").as_bytes());
        let tag: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect();
        format!("{payload}.{tag}")
    };
    let payload = token.rsplit_once('.').expect("mac").0;
    assert_eq!(
        sign("atlas-delta-v2", payload),
        token,
        "the test signs like the server"
    );
    refused(sign("atlas-delta-v1", payload), "reader", b.clone(), T).await?;
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
    fn receive(&mut self, page: &Page) {
        for batch in &page.batches {
            match batch.segment {
                None => self.apply(&batch.changes),
                Some(segment) if self.staging => {
                    self.staged.extend(batch.changes.iter().cloned());
                    if segment.last {
                        let staged = std::mem::take(&mut self.staged);
                        self.apply(&staged);
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
}

/// F7, F9, I5, I8: writers commit between pieces; a staging client converges to a fresh
/// snapshot, never receives a hidden undelivered resource, and the live-applying control
/// exposes a partial revision.
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
