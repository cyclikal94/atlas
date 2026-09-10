use crate::support::database::fixture as local;
use crate::support::tasks::{
    account, at, command, create, definition, id, record, refresh, shared, view,
};
use anyhow::Result;
use atlas_core::{Store, tasks::*};
async fn participation_and_history(s: Store) -> Result<()> {
    s.migrate().await?;
    let a = account(&s).await?;
    let b = account(&s).await?;
    let mut d = definition("2026-09-08", Carry::CloseIncomplete, Participation::Anyone);
    let task = create(&s, &a, d.clone(), Some(shared(&b, true)), at(8)).await?;
    let occ = view(&s, &b, &task, at(8)).await?[0].id.clone();
    assert_eq!(occ, occurrence_id(&task, "2026-09-08Tdate")?);
    // Anyone may start their own private evidence stream without changing the roster.
    record(
        &s,
        &b,
        &occ,
        Evidence::Checkbox { complete: true },
        Some(1),
        at(8),
        at(8),
    )
    .await?;
    assert_eq!(
        view(&s, &b, &task, at(8)).await?[0].outcome,
        Outcome::Complete
    );
    assert_eq!(
        view(&s, &a, &task, at(8)).await?[0].outcome,
        Outcome::Unknown
    );
    assert_eq!(
        view(&s, &a, &task, at(8)).await?[0].data.participants,
        vec![a.clone()]
    );
    let stream = progress_id(&occ, &b)?;
    assert_eq!(
        s.progress_journal(&a, &stream, 0, 20)
            .await
            .unwrap_err()
            .to_string(),
        "not_found"
    );
    assert_eq!(
        s.task_streak_scoped(&b, &task, "personal", at(8))
            .await?
            .current,
        Some(1)
    );
    assert!(
        s.task_command(
            &a,
            &id(),
            &TaskCommand::Exclude {
                occurrence_id: occ.clone(),
                expected_version: 1,
                excluded: true
            },
            None,
            at(8)
        )
        .await
        .is_err()
    );

    d.allow_streak_exclusions = true;
    let exclusion_task = create(&s, &a, d.clone(), Some(shared(&b, true)), at(8)).await?;
    let exclusion_occ = view(&s, &a, &exclusion_task, at(8)).await?[0].id.clone();
    command(
        &s,
        &a,
        TaskCommand::Exclude {
            occurrence_id: exclusion_occ.clone(),
            expected_version: 1,
            excluded: true,
        },
        at(8),
    )
    .await?;
    let before = view(&s, &a, &exclusion_task, at(8)).await?[0].outcome;
    record(
        &s,
        &b,
        &exclusion_occ,
        Evidence::Checkbox { complete: true },
        Some(1),
        at(8),
        at(8),
    )
    .await?;
    assert_eq!(before, Outcome::Unknown);
    assert_eq!(
        view(&s, &a, &exclusion_task, at(8)).await?[0].outcome,
        before
    );

    d.participation = Participation::Personal;
    d.allow_streak_exclusions = true;
    let task = create(&s, &a, d.clone(), None, at(8)).await?;
    let occ = view(&s, &a, &task, at(8)).await?[0].id.clone();
    let first = id();
    command(
        &s,
        &a,
        TaskCommand::Record {
            occurrence_id: occ.clone(),
            subject_account_id: a.clone(),
            entry_id: first.clone(),
            evidence: Evidence::Checkbox { complete: true },
            replaces: None,
            expected_version: Some(1),
            happened_at: at(8),
        },
        at(8),
    )
    .await?;
    record(
        &s,
        &a,
        &occ,
        Evidence::Checkbox { complete: false },
        Some(2),
        at(8),
        at(8),
    )
    .await?;
    command(
        &s,
        &a,
        TaskCommand::Record {
            occurrence_id: occ.clone(),
            subject_account_id: a.clone(),
            entry_id: id(),
            evidence: Evidence::Checkbox { complete: true },
            replaces: Some(first),
            expected_version: Some(3),
            happened_at: at(8),
        },
        at(8),
    )
    .await?;
    assert_eq!(
        view(&s, &a, &task, at(8)).await?[0].outcome,
        Outcome::Incomplete
    );
    let journal = s
        .progress_journal(&a, &progress_id(&occ, &a)?, 0, 2)
        .await?;
    assert_eq!(journal["entries"].as_array().unwrap().len(), 2);
    let page = s
        .progress_journal(
            &a,
            &progress_id(&occ, &a)?,
            journal["next_after"].as_i64().unwrap(),
            2,
        )
        .await?;
    assert_eq!(page["entries"][0]["logical_order"], 1);
    record(
        &s,
        &a,
        &occ,
        Evidence::Checkbox { complete: true },
        Some(4),
        at(8),
        at(8),
    )
    .await?;
    refresh(&s, &a, &task, "2026-09-09", at(9)).await?;
    let next = view(&s, &a, &task, at(9))
        .await?
        .into_iter()
        .find(|v| v.data.slot.date.as_deref() == Some("2026-09-09"))
        .unwrap();
    command(
        &s,
        &a,
        TaskCommand::Exclude {
            occurrence_id: next.id,
            expected_version: 1,
            excluded: true,
        },
        at(9),
    )
    .await?;
    assert_eq!(s.task_streak(&a, &task, at(9)).await?.current, Some(1));

    d.allow_streak_exclusions = false;
    d.goal = Goal::Numeric {
        minimum: Some(Decimal("30".into())),
        maximum: None,
        unit: "reps".into(),
    };
    let task = create(&s, &a, d, None, at(8)).await?;
    let occ = view(&s, &a, &task, at(8)).await?[0].id.clone();
    for _ in 0..30 {
        record(
            &s,
            &a,
            &occ,
            Evidence::Quantity {
                amount: Decimal("1".into()),
            },
            None,
            at(8),
            at(8),
        )
        .await?;
    }
    let resources = s
        .resources(&a, "progress", Some(&occ), false, None, 50)
        .await?;
    let state: ProgressState = serde_json::from_value(resources[0].value.clone())?;
    assert_eq!(state.entry_count, 30);
    assert_eq!(state.recent_entries.len(), 8);
    assert_eq!(state.action.amount, Some(Decimal("30".into())));
    assert_eq!(
        view(&s, &a, &task, at(8)).await?[0].outcome,
        Outcome::Complete
    );

    // Pooled quantities require consent and visibility from every participant.
    let mut d = definition("2026-09-09", Carry::CloseIncomplete, Participation::Pooled);
    d.goal = Goal::Numeric {
        minimum: Some(Decimal("3".into())),
        maximum: Some(Decimal("4".into())),
        unit: "sessions".into(),
    };
    let task = create(&s, &a, d, Some(shared(&b, true)), at(8)).await?;
    for (actor, other, version) in [(&a, &b, 1), (&b, &a, 0)] {
        command(
            &s,
            actor,
            TaskCommand::Enrol {
                task_id: task.clone(),
                expected_version: version,
                active: true,
                aggregate_consent: true,
                initial_policy: Some(shared(other, false)),
            },
            at(8),
        )
        .await?;
    }
    refresh(&s, &a, &task, "2026-09-09", at(9)).await?;
    let occ = view(&s, &a, &task, at(9)).await?[0].id.clone();
    record(
        &s,
        &a,
        &occ,
        Evidence::Quantity {
            amount: Decimal("1".into()),
        },
        None,
        at(9),
        at(9),
    )
    .await?;
    record(
        &s,
        &b,
        &occ,
        Evidence::Quantity {
            amount: Decimal("2".into()),
        },
        None,
        at(9),
        at(9),
    )
    .await?;
    assert_eq!(
        view(&s, &a, &task, at(9)).await?[0].outcome,
        Outcome::Provisional
    );
    assert_eq!(
        view(&s, &a, &task, at(10)).await?[0].outcome,
        Outcome::Complete
    );
    command(
        &s,
        &a,
        TaskCommand::Enrol {
            task_id: task.clone(),
            expected_version: 2,
            active: true,
            aggregate_consent: false,
            initial_policy: Some(shared(&b, false)),
        },
        at(10),
    )
    .await?;
    refresh(&s, &a, &task, "2026-09-10", at(10)).await?;
    let next = view(&s, &b, &task, at(10))
        .await?
        .into_iter()
        .find(|v| v.data.slot.date.as_deref() == Some("2026-09-10"))
        .unwrap();
    let shared_state = s
        .resources(&b, "progress", Some(&next.id), false, None, 50)
        .await?
        .into_iter()
        .find(|r| r.id == progress_id(&next.id, &a).unwrap())
        .unwrap();
    assert!(
        !serde_json::from_value::<ProgressState>(shared_state.value.clone())?.aggregate_consent
    );
    assert_eq!(next.outcome, Outcome::Unknown);

    Ok(())
}

#[tokio::test]
async fn tasks_participation_history() -> Result<()> {
    let (_dir, s) = local().await?;
    participation_and_history(s).await
}
