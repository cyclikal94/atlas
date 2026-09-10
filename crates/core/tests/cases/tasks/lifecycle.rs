use crate::support::database::fixture as local;
use crate::support::tasks::{account, at, command, create, definition, record, refresh, view};
use anyhow::Result;
use atlas_core::{Store, tasks::*};
async fn lifecycle(s: Store) -> Result<()> {
    s.migrate().await?;
    let a = account(&s).await?;
    let teeth = create(
        &s,
        &a,
        definition(
            "2026-09-07",
            Carry::CloseIncomplete,
            Participation::Personal,
        ),
        None,
        at(8),
    )
    .await?;
    refresh(&s, &a, &teeth, "2026-09-08", at(8)).await?;
    let rows = view(&s, &a, &teeth, at(8)).await?;
    assert_eq!(rows.len(), 2);
    let yesterday = rows
        .iter()
        .find(|v| v.data.slot.date.as_deref() == Some("2026-09-07"))
        .unwrap();
    let today = rows
        .iter()
        .find(|v| v.data.slot.date.as_deref() == Some("2026-09-08"))
        .unwrap();
    assert_eq!(yesterday.period_outcome, Outcome::Missed);
    assert!(!yesterday.actionable);
    assert!(today.actionable);
    let first_id = today.id.clone();
    record(
        &s,
        &a,
        &first_id,
        Evidence::Checkbox { complete: true },
        Some(1),
        at(8),
        at(8),
    )
    .await?;
    refresh(&s, &a, &teeth, "2026-09-07", at(7)).await?;
    assert_eq!(view(&s, &a, &teeth, at(8)).await?.len(), 2);
    assert_eq!(
        view(&s, &a, &teeth, at(8))
            .await?
            .iter()
            .find(|v| v.id == first_id)
            .unwrap()
            .outcome,
        Outcome::Complete
    );
    // A later target edit does not replace historical goal snapshots.
    let mut revised = definition(
        "2026-09-07",
        Carry::CloseIncomplete,
        Participation::Personal,
    );
    revised.goal = Goal::Numeric {
        minimum: Some(Decimal("3".into())),
        maximum: None,
        unit: "times".into(),
    };
    command(
        &s,
        &a,
        TaskCommand::ReviseTask {
            id: teeth.clone(),
            expected_version: 1,
            title: "Changed future target".into(),
            definition: revised,
        },
        at(8),
    )
    .await?;
    assert!(
        view(&s, &a, &teeth, at(8))
            .await?
            .iter()
            .all(|v| v.data.definition.goal == Goal::Checkbox)
    );
    refresh(&s, &a, &teeth, "2026-09-09", at(9)).await?;
    assert!(
        view(&s, &a, &teeth, at(9))
            .await?
            .iter()
            .any(|v| matches!(v.data.definition.goal, Goal::Numeric { .. }))
    );
    for carry in [Carry::Accumulate, Carry::RetainOne] {
        let task = create(
            &s,
            &a,
            definition("2026-09-07", carry, Participation::Personal),
            None,
            at(8),
        )
        .await?;
        refresh(&s, &a, &task, "2026-09-08", at(8)).await?;
        let rows = view(&s, &a, &task, at(8)).await?;
        assert_eq!(
            rows.iter().filter(|v| v.actionable).count(),
            if carry == Carry::Accumulate { 2 } else { 1 }
        );
        let original = rows
            .iter()
            .find(|v| v.data.slot.date.as_deref() == Some("2026-09-07"))
            .unwrap();
        record(
            &s,
            &a,
            &original.id,
            Evidence::Checkbox { complete: true },
            Some(1),
            at(8),
            at(8),
        )
        .await?;
        let late = view(&s, &a, &task, at(8))
            .await?
            .into_iter()
            .find(|v| v.id == original.id)
            .unwrap();
        assert_eq!(late.outcome, Outcome::Complete);
        assert_eq!(late.period_outcome, Outcome::Missed);
        if carry == Carry::RetainOne {
            refresh(&s, &a, &task, "2026-09-09", at(9)).await?;
            assert_eq!(
                view(&s, &a, &task, at(9))
                    .await?
                    .iter()
                    .filter(|v| v.actionable)
                    .count(),
                1
            );
        }
    }
    // No date conversion or clock advancement can remove an undated inbox task.
    let mut undated = definition("2026-09-08", Carry::RetainOne, Participation::Personal);
    undated.schedule.start_date = None;
    undated.schedule.repeat = None;
    let task = create(&s, &a, undated, None, at(8)).await?;
    let rows = view(&s, &a, &task, at(30)).await?;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].actionable);
    assert!(rows[0].data.slot.instant.is_none());
    // Pagination is filtered first and a changed revision invalidates continuation.
    let first = s
        .occurrence_view(
            &a,
            &ViewFilter {
                state: Some("actionable".into()),
                limit: Some(1),
                ..Default::default()
            },
            at(8),
        )
        .await?;
    assert_eq!(first.items.len(), 1);
    assert!(first.next_after.is_some());
    command(
        &s,
        &a,
        TaskCommand::Archive {
            id: task.clone(),
            expected_version: 1,
            archived: true,
        },
        at(8),
    )
    .await?;
    assert_eq!(
        s.occurrence_view(
            &a,
            &ViewFilter {
                after: first.next_after,
                revision: Some(first.revision),
                ..Default::default()
            },
            at(8)
        )
        .await
        .unwrap_err()
        .to_string(),
        "conflict"
    );
    Ok(())
}

#[tokio::test]
async fn tasks_lifecycle() -> Result<()> {
    let (_dir, s) = local().await?;
    lifecycle(s).await
}
