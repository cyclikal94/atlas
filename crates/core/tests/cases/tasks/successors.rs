use anyhow::Result;
use atlas_core::{
    Store,
    policy::{Policy, PrincipalGrant},
    tasks::*,
};

const NOW: i64 = 1788868800;
use crate::support::task_workflows::{account, complete, id, setup};

async fn successor_scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let t = id();
    let definition = Definition {
        schedule: Schedule {
            start_date: Some("2026-09-08".into()),
            time: None,
            timezone: "Europe/Vienna".into(),
            repeat: Some(Cadence {
                frequency: Frequency::AfterCompletion,
                interval: 2,
            }),
        },
        goal: Goal::Checkbox,
        carry: Carry::RetainOne,
        participation: Participation::Personal,
        open_days_before: 0,
        close_days_after: 1,
        allow_streak_exclusions: false,
    };
    s.task_command(
        &a,
        &id(),
        &TaskCommand::CreateTask {
            id: t.clone(),
            execution_id: id(),
            title: "Water plants".into(),
            definition,
            anchor: None,
            initial_policy: None,
        },
        None,
        NOW,
    )
    .await?;
    let o = occurrence_id(&t, "once")?;
    complete(s, &a, &o, CompletionMode::RequireSatisfied).await?;
    let successor = occurrence_id(&t, &format!("after:{o}"))?;
    let data: String = sqlx::query_scalar("SELECT value FROM resources WHERE id=$1")
        .bind(&successor)
        .fetch_one(&s.pool)
        .await?;
    let data: OccurrenceData = serde_json::from_str(&data)?;
    assert_eq!(data.slot.date.as_deref(), Some("2026-09-10"));
    assert!(data.covered_by.is_none());
    // Explicitly correcting the predecessor and moving the server clock back
    // does not remove/re-date the already-issued successor.
    let progress = s
        .resources(&a, "progress", Some(&o), false, None, 50)
        .await?;
    s.task_command(
        &a,
        &id(),
        &TaskCommand::Record {
            occurrence_id: o.clone(),
            subject_account_id: a.clone(),
            entry_id: id(),
            evidence: Evidence::Checkbox { complete: false },
            replaces: None,
            expected_version: Some(progress[0].version),
            happened_at: NOW,
        },
        None,
        NOW,
    )
    .await?;
    s.task_command(
        &a,
        &id(),
        &TaskCommand::Materialise {
            task_id: t.clone(),
            through_date: "2026-09-30".into(),
            limit: 16,
        },
        None,
        NOW - 86400,
    )
    .await?;
    let rows: Vec<String> = sqlx::query_scalar("SELECT id FROM task_occurrences WHERE task_id=$1")
        .bind(&t)
        .fetch_all(&s.pool)
        .await?;
    assert_eq!(rows.len(), 2);
    assert!(rows.contains(&successor));
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM resources WHERE id=$1")
            .bind(&successor)
            .fetch_one(&s.pool)
            .await?,
        serde_json::to_string(&data)?
    );
    Ok(())
}

#[tokio::test]
async fn completion_relative_identity_survives_correction_and_clock() -> Result<()> {
    let (s, _dir) = setup().await?;
    successor_scenario(&s).await
}

#[tokio::test]
async fn completion_relative_does_not_reveal_private_anyone_timestamp() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let b = account(&s).await?;
    let t = id();
    let mut definition = presets("2026-09-08", "Europe/Vienna")?
        .pop()
        .unwrap()
        .definition;
    definition.participation = Participation::Anyone;
    s.task_command(
        &a,
        &id(),
        &TaskCommand::CreateTask {
            id: t.clone(),
            execution_id: id(),
            title: "Shared chore".into(),
            definition,
            anchor: None,
            initial_policy: Some(Policy {
                grants: vec![PrincipalGrant::Account {
                    id: b.clone(),
                    edit: true,
                }],
                exclude_accounts: vec![],
            }),
        },
        None,
        NOW,
    )
    .await?;
    let o = occurrence_id(&t, "once")?;
    complete(&s, &a, &o, CompletionMode::RequireSatisfied).await?;
    let later = NOW + 86400;
    // The other participant records privately on a later calendar date.
    s.task_command(
        &b,
        &id(),
        &TaskCommand::Record {
            occurrence_id: o.clone(),
            subject_account_id: b.clone(),
            entry_id: id(),
            evidence: Evidence::Checkbox { complete: true },
            replaces: None,
            expected_version: Some(1),
            happened_at: later,
        },
        None,
        later,
    )
    .await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_occurrences WHERE task_id=$1")
        .bind(&t)
        .fetch_one(&s.pool)
        .await?;
    assert_eq!(count, 1);
    let progress = progress_id(&o, &a)?;
    s.management(
        &a,
        &id(),
        &[atlas_core::households::ManagementCommand::ReplacePolicy {
            id: progress,
            expected_version: 1,
            policy: Policy {
                grants: vec![PrincipalGrant::Account {
                    id: b.clone(),
                    edit: false,
                }],
                exclude_accounts: vec![],
            },
        }],
        later,
    )
    .await?;
    s.reconcile_completion_tasks(later).await?;
    let successor = occurrence_id(&t, &format!("after:{o}"))?;
    let data: OccurrenceData = serde_json::from_str(
        &sqlx::query_scalar::<_, String>("SELECT value FROM resources WHERE id=$1")
            .bind(successor)
            .fetch_one(&s.pool)
            .await?,
    )?;
    // Only the common visible proof (8 September) determines the successor date.
    assert_eq!(data.slot.date.as_deref(), Some("2026-09-15"));
    Ok(())
}
