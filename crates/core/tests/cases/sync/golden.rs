//! Non-opt-in `/sync` transcripts (BE-Q23 N3). The recorded file was captured on the base
//! implementation (`4396c99`) before bounded delivery existed. Every later build must reproduce
//! it byte for byte, so a client that never sends `max_changes` observes no change.
use anyhow::{Context, Result};
use atlas_core::{
    Command, Page, Store,
    policy::{Policy, PrincipalGrant},
    tasks::*,
};
use uuid::Uuid;

use crate::support::database::fixture;
use crate::support::task_fixtures::definition;

const T: i64 = 1_788_868_800;
const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/sync_non_opt_in_golden.json"
);

fn n(value: u128) -> String {
    Uuid::from_u128(value).to_string()
}

/// The cursor is opaque and partly random (a per-device key, a snapshot token), so the transcript
/// keeps its form and signed fields and replaces only the random parts.
fn normalised(mut page: Page) -> Result<String> {
    let cursor = &page.next_cursor;
    page.next_cursor = if let Some(rest) = cursor.strip_prefix("d1.") {
        let parts: Vec<_> = rest.split('.').collect();
        assert_eq!(parts.len(), 3, "{cursor}");
        assert!(parts[2].len() == 64 && parts[2].bytes().all(|b| b.is_ascii_hexdigit()));
        format!("d1.{}.{}.<mac>", parts[0], parts[1])
    } else {
        assert_eq!(Uuid::parse_str(cursor)?.to_string(), *cursor);
        "<snapshot>".into()
    };
    Ok(serde_json::to_string(&page)?)
}

async fn drain(
    store: &Store,
    account: &str,
    device: &str,
    mut cursor: Option<String>,
    limit: usize,
    now: i64,
    out: &mut Vec<String>,
) -> Result<String> {
    loop {
        let page = store
            .sync(account, device, cursor.as_deref(), limit, now)
            .await?;
        let more = page.has_more;
        cursor = Some(page.next_cursor.clone());
        out.push(format!("{device} limit={limit}: {}", normalised(page)?));
        if !more {
            return Ok(cursor.expect("drained"));
        }
    }
}

async fn versions(store: &Store, id: &str) -> Result<(i64, i64)> {
    let row: (i64, i64) = sqlx::query_as("SELECT version,policy_version FROM resources WHERE id=$1")
        .bind(id)
        .fetch_one(&store.pool)
        .await?;
    Ok(row)
}

async fn apply(store: &Store, actor: &str, op: u128, commands: &[Command]) -> Result<()> {
    store
        .apply(actor, &n(op), commands)
        .await
        .with_context(|| format!("operation {op:#x}"))?;
    Ok(())
}

/// One owner, one reader, people with text and checklist fields, a task chain, edits, a
/// revocation of delivered content and a re-grant, read as snapshots and deltas at two limits.
pub(crate) async fn transcript(store: &Store) -> Result<Vec<String>> {
    let (a, b) = (n(0xA), n(0xB));
    store.add_account(&a, "golden-a", "test-only").await?;
    store.add_account(&b, "golden-b", "test-only").await?;
    let mut out = Vec::new();
    let b1 = store.sync(&b, "b1", None, 200, T).await?.next_cursor;
    let b200 = store.sync(&b, "b200", None, 200, T).await?.next_cursor;
    let share = |edit| Policy {
        grants: vec![PrincipalGrant::Account { id: b.clone(), edit }],
        exclude_accounts: vec![],
    };
    let (p1, p2, f1, f2, f3) = (n(0x101), n(0x102), n(0x201), n(0x202), n(0x203));
    apply(
        store,
        &a,
        0x1001,
        &[
            Command::CreatePerson {
                id: p1.clone(),
                name: "Golden one".into(),
                initial_policy: Some(share(false)),
            },
            Command::CreateField {
                id: f1.clone(),
                person_id: p1.clone(),
                label: "Note".into(),
                value: "Shared note".into(),
                initial_policy: Some(share(true)),
            },
            Command::CreateField {
                id: f2.clone(),
                person_id: p1.clone(),
                label: "Private".into(),
                value: "Owner only".into(),
                initial_policy: Some(Policy::default()),
            },
            Command::CreatePerson {
                id: p2.clone(),
                name: "Golden two".into(),
                initial_policy: Some(Policy::default()),
            },
        ],
    )
    .await?;
    store
        .task_command(
            &a,
            &n(0x1002),
            &TaskCommand::PutField {
                id: f3.clone(),
                parent_id: p1.clone(),
                expected_version: None,
                expected_policy_version: None,
                label: "Packing".into(),
                value: FieldValue::Checklist {
                    items: (0..5)
                        .map(|i| ChecklistItem {
                            id: n(0x3000 + i),
                            label: format!("Item {i}"),
                        })
                        .collect(),
                    checked: vec![n(0x3001)],
                },
                initial_policy: Some(share(false)),
            },
            None,
            T + 1,
        )
        .await
        .context("put field")?;
    store
        .task_command(
            &a,
            &n(0x1003),
            &TaskCommand::CreateTask {
                id: n(0x401),
                execution_id: n(0x402),
                title: "Golden task".into(),
                definition: definition(Goal::Checkbox),
                anchor: None,
                initial_policy: Some(share(false)),
            },
            None,
            T + 2,
        )
        .await
        .context("create task")?;
    apply(
        store,
        &a,
        0x1004,
        &[Command::Edit {
            id: f1.clone(),
            expected_version: versions(store, &f1).await?.0,
            expected_policy_version: Some(versions(store, &f1).await?.1),
            label: "Note".into(),
            value: "Edited note".into(),
        }],
    )
    .await?;
    let revoke = Command::Revoke {
        id: p1.clone(),
        expected_version: versions(store, &p1).await?.1,
        account_id: b.clone(),
    };
    apply(store, &a, 0x1005, &[revoke]).await?;
    let grant = Command::Grant {
        id: p1.clone(),
        expected_version: versions(store, &p1).await?.1,
        account_id: b.clone(),
        edit: false,
    };
    apply(store, &a, 0x1006, &[grant]).await?;
    drain(store, &a, "a1", None, 2, T + 10, &mut out).await?;
    let b1 = drain(store, &b, "b1", Some(b1), 1, T + 10, &mut out).await?;
    let b200 = drain(store, &b, "b200", Some(b200), 200, T + 10, &mut out).await?;
    // Content delivered to both devices and hidden afterwards becomes a removal.
    let hide = Command::Revoke {
        id: f1.clone(),
        expected_version: versions(store, &f1).await?.1,
        account_id: b.clone(),
    };
    apply(store, &a, 0x1007, &[hide]).await?;
    let end = drain(store, &b, "b1", Some(b1), 1, T + 10, &mut out).await?;
    drain(store, &b, "b200", Some(b200), 200, T + 10, &mut out).await?;
    drain(store, &b, "b-fresh", None, 3, T + 10, &mut out).await?;
    // An empty delta at the head, and the answers for a forged delta cursor and a string that
    // has the shape of a later cursor form.
    drain(store, &b, "b1", Some(end.clone()), 1, T + 11, &mut out).await?;
    let mut forged = end.clone();
    let last = forged.pop().expect("cursor");
    forged.push(if last == '0' { '1' } else { '0' });
    for (label, cursor) in [
        ("forged-d1", forged),
        ("d2-shape", format!("d2.{}", "0".repeat(60))),
        ("unknown", "not-a-cursor".to_owned()),
    ] {
        let error = store
            .sync(&b, "b1", Some(&cursor), 1, T + 12)
            .await
            .unwrap_err();
        out.push(format!("{label}: {error}"));
    }
    Ok(out)
}

#[tokio::test]
#[ignore = "records the golden file on the base implementation"]
async fn golden_capture() -> Result<()> {
    let (_d1, first) = fixture().await?;
    let (_d2, second) = fixture().await?;
    let recorded = transcript(&first).await?;
    assert_eq!(recorded, transcript(&second).await?, "the transcript is deterministic");
    std::fs::write(GOLDEN, serde_json::to_string_pretty(&recorded)? + "\n")?;
    Ok(())
}

#[tokio::test]
async fn non_opt_in_responses_match_the_base_golden() -> Result<()> {
    let (_d, store) = fixture().await?;
    let recorded: Vec<String> = serde_json::from_str(&std::fs::read_to_string(GOLDEN)?)?;
    let current = transcript(&store).await?;
    assert_eq!(current.len(), recorded.len());
    for (current, recorded) in current.iter().zip(&recorded) {
        assert_eq!(current, recorded);
    }
    Ok(())
}
