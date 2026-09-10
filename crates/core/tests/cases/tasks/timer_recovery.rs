use anyhow::Result;
use atlas_core::{Store, tasks::*};

const NOW: i64 = 1788868800;

use crate::support::task_fixtures::{account, definition, id, run, setup, task};

async fn timer_recovery(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let (_, prerequisite) = task(s, &a, definition(Goal::Checkbox)).await?;
    let (_, root) = task(
        s,
        &a,
        definition(Goal::Numeric {
            minimum: Some(Decimal("60".into())),
            maximum: None,
            unit: "seconds".into(),
        }),
    )
    .await?;
    run(
        s,
        &a,
        TaskCommand::SetDependencies {
            occurrence_id: root.clone(),
            expected_version: 1,
            prerequisites: vec![prerequisite.clone()],
            strict: true,
        },
        NOW,
    )
    .await?;
    let timer = id();
    run(
        s,
        &a,
        TaskCommand::StartTimer {
            occurrence_id: root.clone(),
            session_id: timer.clone(),
            started_at: NOW - 120,
        },
        NOW,
    )
    .await?;
    let stop = TaskCommand::StopTimer {
        occurrence_id: root.clone(),
        session_id: timer.clone(),
        expected_version: 1,
        stopped_at: NOW,
    };
    assert!(s.task_command(&a, &id(), &stop, None, NOW).await.is_err());
    assert!(
        s.timer_sessions(&a, &root, None, 20).await?[0]
            .stopped_at
            .is_none()
    );
    run(
        s,
        &a,
        TaskCommand::CancelTimer {
            occurrence_id: root.clone(),
            session_id: timer,
            expected_version: 1,
        },
        NOW,
    )
    .await?;
    // Cancellation frees the account. No elapsed evidence bypassed the prerequisite.
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM progress_entries e JOIN resources r ON r.id=e.progress_id WHERE r.parent_id=$1").bind(&root).fetch_one(&s.pool).await?,0);
    run(
        s,
        &a,
        TaskCommand::Record {
            occurrence_id: prerequisite,
            subject_account_id: a.clone(),
            entry_id: id(),
            evidence: Evidence::Checkbox { complete: true },
            replaces: None,
            expected_version: Some(1),
            happened_at: NOW,
        },
        NOW,
    )
    .await?;
    let later = id();
    run(
        s,
        &a,
        TaskCommand::StartTimer {
            occurrence_id: root.clone(),
            session_id: later.clone(),
            started_at: NOW - 120,
        },
        NOW,
    )
    .await?;
    run(
        s,
        &a,
        TaskCommand::StopTimer {
            occurrence_id: root.clone(),
            session_id: later,
            expected_version: 1,
            stopped_at: NOW - 60,
        },
        NOW,
    )
    .await?;
    // Backdated work before that interval is valid and stoppable at the boundary.
    let earlier = id();
    run(
        s,
        &a,
        TaskCommand::StartTimer {
            occurrence_id: root.clone(),
            session_id: earlier.clone(),
            started_at: NOW - 180,
        },
        NOW,
    )
    .await?;
    run(
        s,
        &a,
        TaskCommand::StopTimer {
            occurrence_id: root.clone(),
            session_id: earlier,
            expected_version: 1,
            stopped_at: NOW - 120,
        },
        NOW,
    )
    .await?;
    assert_eq!(s.timer_sessions(&a, &root, None, 20).await?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn rejected_timer_stops_have_safe_recovery() -> Result<()> {
    let (s, _d) = setup().await?;
    timer_recovery(&s).await
}
