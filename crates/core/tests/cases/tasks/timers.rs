use anyhow::Result;
use atlas_core::{Store, tasks::*};

const NOW: i64 = 1788868800;
use crate::support::task_workflows::{account, id, setup, task};

async fn timer_scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let o = task(
        s,
        &a,
        Goal::Numeric {
            minimum: Some(Decimal("60".into())),
            maximum: None,
            unit: "seconds".into(),
        },
    )
    .await?;
    let session = id();
    let operation = id();
    let start = TaskCommand::StartTimer {
        occurrence_id: o.clone(),
        session_id: session.clone(),
        started_at: NOW - 120,
    };
    s.task_command(&a, &operation, &start, None, NOW).await?;
    s.task_command(&a, &operation, &start, None, NOW).await?;
    assert_eq!(s.timer_sessions(&a, &o, None, 50).await?.len(), 1);
    assert!(s.timer_sessions(&b, &o, None, 50).await.is_err());
    assert!(
        s.task_command(
            &a,
            &id(),
            &TaskCommand::StartTimer {
                occurrence_id: o.clone(),
                session_id: id(),
                started_at: NOW
            },
            None,
            NOW
        )
        .await
        .is_err()
    );
    let stop = TaskCommand::StopTimer {
        occurrence_id: o.clone(),
        session_id: session.clone(),
        expected_version: 1,
        stopped_at: NOW - 60,
    };
    assert!(s.task_command(&b, &id(), &stop, None, NOW).await.is_err());
    let operation = id();
    s.task_command(&a, &operation, &stop, None, NOW).await?;
    s.task_command(&a, &operation, &stop, None, NOW).await?;
    let progress = s
        .resources(&a, "progress", Some(&o), false, None, 50)
        .await?;
    let state: ProgressState = serde_json::from_value(progress[0].value.clone())?;
    assert_eq!(state.action.amount, Some(Decimal("60".into())));
    assert_eq!(state.entry_count, 1);
    // A delayed offline session starts before an existing interval. Stopping it
    // across that interval must roll back; it can then be cancelled to recover.
    let delayed = id();
    s.task_command(
        &a,
        &id(),
        &TaskCommand::StartTimer {
            occurrence_id: o.clone(),
            session_id: delayed.clone(),
            started_at: NOW - 180,
        },
        None,
        NOW,
    )
    .await?;
    assert!(
        s.task_command(
            &a,
            &id(),
            &TaskCommand::StopTimer {
                occurrence_id: o.clone(),
                session_id: delayed.clone(),
                expected_version: 1,
                stopped_at: NOW
            },
            None,
            NOW
        )
        .await
        .is_err()
    );
    s.task_command(
        &a,
        &id(),
        &TaskCommand::CancelTimer {
            occurrence_id: o.clone(),
            session_id: delayed,
            expected_version: 1,
        },
        None,
        NOW,
    )
    .await?;
    assert_eq!(s.timer_sessions(&a, &o, None, 50).await?.len(), 1);
    // A touching interval is valid and contributes once.
    let resumed = id();
    s.task_command(
        &a,
        &id(),
        &TaskCommand::StartTimer {
            occurrence_id: o.clone(),
            session_id: resumed.clone(),
            started_at: NOW - 60,
        },
        None,
        NOW,
    )
    .await?;
    s.task_command(
        &a,
        &id(),
        &TaskCommand::StopTimer {
            occurrence_id: o.clone(),
            session_id: resumed,
            expected_version: 1,
            stopped_at: NOW,
        },
        None,
        NOW,
    )
    .await?;
    let progress = s
        .resources(&a, "progress", Some(&o), false, None, 50)
        .await?;
    let state: ProgressState = serde_json::from_value(progress[0].value.clone())?;
    assert_eq!(state.action.amount, Some(Decimal("120".into())));
    assert_eq!(state.entry_count, 2);
    Ok(())
}

#[tokio::test]
async fn timers_overlap_replay_and_authority() -> Result<()> {
    let (s, _dir) = setup().await?;
    timer_scenario(&s).await
}
