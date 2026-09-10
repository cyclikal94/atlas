use anyhow::Result;
use atlas_core::{Store, tasks::*};

const NOW: i64 = 1788868800;
use crate::support::task_workflows::{account, id, setup};

async fn completion_worker_scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let t = id();
    let mut definition = presets("2026-09-08", "Europe/Vienna")?
        .pop()
        .unwrap()
        .definition;
    definition.goal = Goal::Numeric {
        minimum: None,
        maximum: Some(Decimal("1".into())),
        unit: "cups".into(),
    };
    s.task_command(
        &a,
        &id(),
        &TaskCommand::CreateTask {
            id: t.clone(),
            execution_id: id(),
            title: "At most one coffee".into(),
            definition,
            anchor: None,
            initial_policy: None,
        },
        None,
        NOW,
    )
    .await?;
    let o = occurrence_id(&t, "once")?;
    s.task_command(
        &a,
        &id(),
        &TaskCommand::Record {
            occurrence_id: o.clone(),
            subject_account_id: a.clone(),
            entry_id: id(),
            evidence: Evidence::Observe,
            replaces: None,
            expected_version: None,
            happened_at: NOW,
        },
        None,
        NOW,
    )
    .await?;
    let before: i64 = sqlx::query_scalar("SELECT revision FROM sync_clock WHERE id=1")
        .fetch_one(&s.pool)
        .await?;
    s.reconcile_completion_tasks(NOW).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT revision FROM sync_clock WHERE id=1")
            .fetch_one(&s.pool)
            .await?,
        before
    );
    // Two workers race after the quota window closes; only one successor is issued.
    let later = NOW + 86400;
    let (left, right) = tokio::join!(
        s.reconcile_completion_tasks(later),
        s.reconcile_completion_tasks(later)
    );
    left?;
    right?;
    let rows: Vec<String> = sqlx::query_scalar("SELECT id FROM task_occurrences WHERE task_id=$1")
        .bind(&t)
        .fetch_all(&s.pool)
        .await?;
    assert_eq!(rows.len(), 2);
    let successor = occurrence_id(&t, &format!("after:{o}"))?;
    assert!(rows.contains(&successor));
    let data: OccurrenceData = serde_json::from_str(
        &sqlx::query_scalar::<_, String>("SELECT value FROM resources WHERE id=$1")
            .bind(&successor)
            .fetch_one(&s.pool)
            .await?,
    )?;
    assert_eq!(data.slot.date.as_deref(), Some("2026-09-16"));
    s.reconcile_completion_tasks(NOW - 86400).await?;
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM task_occurrences WHERE id=$1")
            .bind(successor)
            .fetch_one(&s.pool)
            .await?
            == 1
    );
    Ok(())
}

#[tokio::test]
async fn completion_worker_closes_windows_without_duplicate_successors() -> Result<()> {
    let (s, _dir) = setup().await?;
    completion_worker_scenario(&s).await
}
