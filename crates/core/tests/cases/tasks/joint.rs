use crate::support::database::fixture as local;
use crate::support::tasks::{
    account, at, command, create, definition, id, record, refresh, shared, view,
};
use anyhow::Result;
use atlas_core::{Change, Command, Store, policy::Policy, tasks::*};
async fn joint(s: Store) -> Result<()> {
    s.migrate().await?;
    let a = account(&s).await?;
    let b = account(&s).await?;
    let task = create(
        &s,
        &a,
        definition(
            "2026-09-04",
            Carry::CloseIncomplete,
            Participation::Everyone,
        ),
        Some(shared(&b, true)),
        at(3),
    )
    .await?;
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
            at(3),
        )
        .await?;
    }
    refresh(&s, &a, &task, "2026-09-08", at(8)).await?;
    let rows = view(&s, &a, &task, at(8)).await?;
    assert_eq!(rows.len(), 5);
    for row in &rows {
        assert_eq!(row.data.participants.len(), 2);
        let date =
            chrono::NaiveDate::parse_from_str(row.data.slot.date.as_deref().unwrap(), "%Y-%m-%d")?;
        let happened = date.and_hms_opt(12, 0, 0).unwrap().and_utc().timestamp();
        for actor in [&a, &b] {
            record(
                &s,
                actor,
                &row.id,
                Evidence::Checkbox { complete: true },
                Some(1),
                happened,
                at(8),
            )
            .await?;
        }
    }
    assert_eq!(s.task_streak(&a, &task, at(8)).await?.current, Some(5));
    assert_eq!(s.task_streak(&b, &task, at(8)).await?.current, Some(5));
    let old = rows
        .iter()
        .find(|r| r.data.slot.date.as_deref() == Some("2026-09-04"))
        .unwrap();
    let stream: String = sqlx::query_scalar(
        "SELECT progress_id FROM occurrence_participants WHERE occurrence_id=$1 AND account_id=$2",
    )
    .bind(&old.id)
    .bind(&a)
    .fetch_one(&s.pool)
    .await?;
    let denied = TaskCommand::Record {
        occurrence_id: old.id.clone(),
        subject_account_id: a.clone(),
        entry_id: id(),
        evidence: Evidence::Checkbox { complete: false },
        replaces: None,
        expected_version: Some(2),
        happened_at: at(4),
    };
    assert_eq!(
        s.task_command(&b, &id(), &denied, None, at(8))
            .await
            .unwrap_err()
            .to_string(),
        "forbidden"
    );
    s.apply(
        &a,
        &id(),
        &[Command::Revoke {
            id: stream.clone(),
            expected_version: 1,
            account_id: b.clone(),
        }],
    )
    .await?;
    assert_eq!(s.task_streak(&b, &task, at(8)).await?.current, None);
    assert_eq!(
        view(&s, &b, &task, at(8))
            .await?
            .iter()
            .find(|r| r.id == old.id)
            .unwrap()
            .outcome,
        Outcome::Unknown
    );
    // Leaving affects future intended dates; existing participants and targets stay frozen.
    command(
        &s,
        &b,
        TaskCommand::Enrol {
            task_id: task.clone(),
            expected_version: 1,
            active: false,
            aggregate_consent: false,
            initial_policy: Some(Policy::default()),
        },
        at(9),
    )
    .await?;
    refresh(&s, &a, &task, "2026-09-09", at(9)).await?;
    let rows = view(&s, &a, &task, at(9)).await?;
    assert_eq!(
        rows.iter()
            .find(|r| r.data.slot.date.as_deref() == Some("2026-09-09"))
            .unwrap()
            .data
            .participants,
        vec![a.clone()]
    );
    assert!(
        rows.iter()
            .filter(|r| r.data.slot.date.as_deref() != Some("2026-09-09"))
            .all(|r| r.data.participants.len() == 2)
    );
    // A page sent before revocation must be invalidated, including private streams.
    let page = s.sync(&b, "joint-phone", None, 200, at(9)).await?;
    s.apply(
        &a,
        &id(),
        &[Command::Revoke {
            id: task.clone(),
            expected_version: 1,
            account_id: b.clone(),
        }],
    )
    .await?;
    let delta = s
        .sync(&b, "joint-phone", Some(&page.next_cursor), 200, at(9))
        .await?;
    assert!(
        delta
            .batches
            .iter()
            .flat_map(|b| &b.changes)
            .any(|c| matches!(c,Change::Remove{id} if id==&task))
    );
    assert!(view(&s, &b, &task, at(9)).await.is_err());
    Ok(())
}

#[tokio::test]
async fn tasks_joint() -> Result<()> {
    let (_dir, s) = local().await?;
    joint(s).await
}
