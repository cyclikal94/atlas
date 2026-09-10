use anyhow::Result;
use atlas_core::{Store, tasks::*};

const NOW: i64 = 1788868800;

use crate::support::task_fixtures::{account, definition, id, run, setup, task};

async fn missed_recurrence(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let mut d = definition(Goal::Checkbox);
    d.schedule.start_date = Some("2026-09-08".into());
    d.schedule.repeat = Some(Cadence {
        frequency: Frequency::AfterCompletion,
        interval: 1,
    });
    d.carry = Carry::CloseIncomplete;
    let (t, o) = task(s, &a, d.clone()).await?;
    let later = NOW + 2 * 86400;
    s.reconcile_completion_tasks(later).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM completion_pending WHERE occurrence_id=$1"
        )
        .bind(&o)
        .fetch_one(&s.pool)
        .await?,
        0
    );
    assert_eq!(
        s.occurrence_view(
            &a,
            &ViewFilter {
                task_id: Some(t.clone()),
                ..Default::default()
            },
            later
        )
        .await?
        .items
        .len(),
        1
    );
    // Reopening the saved window restores background reconciliation.
    run(
        s,
        &a,
        TaskCommand::ReviseOccurrence {
            id: o.clone(),
            expected_version: 1,
            definition: d.clone(),
            date: Some("2026-09-11".into()),
            time: None,
        },
        later,
    )
    .await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM completion_pending WHERE occurrence_id=$1"
        )
        .bind(&o)
        .fetch_one(&s.pool)
        .await?,
        1
    );
    run(
        s,
        &a,
        TaskCommand::ReviseOccurrence {
            id: o.clone(),
            expected_version: 2,
            definition: d,
            date: Some("2026-09-08".into()),
            time: None,
        },
        later,
    )
    .await?;
    // A late offline completion is valid because happened_at was within the window.
    run(
        s,
        &a,
        TaskCommand::Record {
            occurrence_id: o.clone(),
            subject_account_id: a.clone(),
            entry_id: id(),
            evidence: Evidence::Checkbox { complete: true },
            replaces: None,
            expected_version: Some(3),
            happened_at: NOW,
        },
        later,
    )
    .await?;
    let rows = s
        .occurrence_view(
            &a,
            &ViewFilter {
                task_id: Some(t.clone()),
                ..Default::default()
            },
            later,
        )
        .await?
        .items;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(
        |r| r.id == occurrence_id(&t, &format!("after:{o}")).unwrap()
            && r.data.slot.date.as_deref() == Some("2026-09-09")
    ));
    Ok(())
}

#[tokio::test]
async fn missed_completion_schedule_waits_for_evidence() -> Result<()> {
    let (s, _d) = setup().await?;
    missed_recurrence(&s).await
}
