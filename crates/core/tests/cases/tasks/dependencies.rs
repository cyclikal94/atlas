use anyhow::Result;
use atlas_core::{
    Store,
    policy::{Policy, PrincipalGrant},
    tasks::*,
};

const NOW: i64 = 1788868800;
use crate::support::task_workflows::{account, complete, count, id, set, setup, task};

async fn scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let first = task(s, &a, Goal::Checkbox).await?;
    let second = task(s, &a, Goal::Checkbox).await?;
    let third = task(s, &a, Goal::Checkbox).await?;
    set(s, &a, &second, vec![first.clone()], false).await?;
    set(s, &a, &third, vec![second.clone(), first.clone()], false).await?;
    assert!(
        set(s, &a, &first, vec![third.clone()], false)
            .await
            .is_err()
    );
    let preview = s.dependency_preview(&a, &third, NOW).await?;
    assert_eq!(preview.items.len(), 3);
    assert_eq!(preview.items.last().unwrap().occurrence_id, third);
    let baseline = count(s).await?;
    assert!(
        complete(s, &a, &third, CompletionMode::RequireSatisfied)
            .await
            .is_err()
    );
    assert_eq!(count(s).await?, baseline);
    let op = id();
    let command = TaskCommand::CompleteDependencies {
        root_evidence: None,
        occurrence_id: third.clone(),
        preview_token: preview.token,
        mode: CompletionMode::CompletePrerequisites,
        happened_at: NOW,
    };
    let revision = s.task_command(&a, &op, &command, None, NOW).await?.revision;
    assert_eq!(
        s.task_command(&a, &op, &command, None, NOW + 1)
            .await?
            .revision,
        revision
    );
    assert_eq!(count(s).await?, baseline + 3);
    assert!(
        s.dependency_preview(&a, &third, NOW)
            .await?
            .items
            .iter()
            .all(|v| v.complete)
    );
    // A changed graph invalidates the earlier preview, even if it remains acyclic.
    let stale = s.dependency_preview(&a, &third, NOW).await?;
    set(s, &a, &third, vec![], false).await?;
    assert!(
        s.task_command(
            &a,
            &id(),
            &TaskCommand::CompleteDependencies {
                root_evidence: None,
                occurrence_id: third,
                preview_token: stale.token,
                mode: CompletionMode::RequireSatisfied,
                happened_at: NOW
            },
            None,
            NOW
        )
        .await
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn dependency_atomic_replay_and_cycles() -> Result<()> {
    let (s, _dir) = setup().await?;
    scenario(&s).await
}

#[tokio::test]
async fn dependency_failure_rolls_back_earlier_prerequisite_and_strict_override() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let checkbox = task(&s, &a, Goal::Checkbox).await?;
    let numeric = task(
        &s,
        &a,
        Goal::Numeric {
            minimum: Some(Decimal("5".into())),
            maximum: None,
            unit: "km".into(),
        },
    )
    .await?;
    let root = task(&s, &a, Goal::Checkbox).await?;
    // A chain guarantees an eligible write is attempted before the numeric failure.
    set(&s, &a, &numeric, vec![checkbox], false).await?;
    set(&s, &a, &root, vec![numeric], true).await?;
    assert!(
        complete(&s, &a, &root, CompletionMode::CompletePrerequisites)
            .await
            .is_err()
    );
    assert_eq!(count(&s).await?, 0);
    assert!(
        complete(&s, &a, &root, CompletionMode::AdvisoryOverride)
            .await
            .is_err()
    );
    set(&s, &a, &root, vec![], false).await?;
    complete(&s, &a, &root, CompletionMode::RequireSatisfied).await?;
    assert_eq!(count(&s).await?, 1);
    Ok(())
}

#[tokio::test]
async fn dependency_hidden_progress_does_not_change_preview() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let b = account(&s).await?;
    let private = task(&s, &a, Goal::Checkbox).await?;
    let root = task(&s, &a, Goal::Checkbox).await?;
    set(&s, &a, &root, vec![private.clone()], false).await?;
    // Share the full root ancestry, but neither the prerequisite nor its progress.
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT ancestor_id FROM (SELECT ancestor_id,depth FROM resource_ancestors WHERE resource_id=$1 UNION ALL SELECT $1,0) ancestors ORDER BY depth DESC",
    )
    .bind(&root)
    .fetch_all(&s.pool)
    .await?;
    for resource in ids {
        s.management(
            &a,
            &id(),
            &[atlas_core::households::ManagementCommand::ReplacePolicy {
                id: resource,
                expected_version: 1,
                policy: Policy {
                    grants: vec![PrincipalGrant::Account {
                        id: b.clone(),
                        edit: true,
                    }],
                    exclude_accounts: vec![],
                },
            }],
            NOW,
        )
        .await?;
    }
    let before = s.dependency_preview(&b, &root, NOW).await?;
    assert!(before.unavailable);
    assert_eq!(before.items.len(), 1);
    complete(&s, &a, &private, CompletionMode::RequireSatisfied).await?;
    let after = s.dependency_preview(&b, &root, NOW).await?;
    assert_eq!(before.token, after.token);
    assert!(
        complete(&s, &b, &root, CompletionMode::CompletePrerequisites)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn dependency_concurrent_opposite_edges_cannot_create_cycle() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("db").display());
    let s = Store::connect(&url).await?;
    s.migrate().await?;
    let a = account(&s).await?;
    let x = task(&s, &a, Goal::Checkbox).await?;
    let y = task(&s, &a, Goal::Checkbox).await?;
    let (left, right) = tokio::join!(
        set(&s, &a, &x, vec![y.clone()], false),
        set(&s, &a, &y, vec![x.clone()], false)
    );
    assert_ne!(left.is_ok(), right.is_ok());
    Ok(())
}

#[tokio::test]
async fn dependency_cascade_accepts_explicit_numeric_root_evidence() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let prerequisite = task(&s, &a, Goal::Checkbox).await?;
    let root = task(
        &s,
        &a,
        Goal::Numeric {
            minimum: Some(Decimal("5".into())),
            maximum: None,
            unit: "km".into(),
        },
    )
    .await?;
    set(&s, &a, &root, vec![prerequisite], true).await?;
    let preview = s.dependency_preview(&a, &root, NOW).await?;
    let command = TaskCommand::CompleteDependencies {
        root_evidence: Some(Evidence::Quantity {
            amount: Decimal("5".into()),
        }),
        occurrence_id: root.clone(),
        preview_token: preview.token,
        mode: CompletionMode::CompletePrerequisites,
        happened_at: NOW,
    };
    let op = id();
    s.task_command(&a, &op, &command, None, NOW).await?;
    s.task_command(&a, &op, &command, None, NOW).await?;
    assert_eq!(count(&s).await?, 2);
    let progress = s
        .resources(&a, "progress", Some(&root), false, None, 50)
        .await?;
    let state: ProgressState = serde_json::from_value(progress[0].value.clone())?;
    assert_eq!(state.action.amount, Some(Decimal("5".into())));
    Ok(())
}
