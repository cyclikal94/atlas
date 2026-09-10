use crate::support::database::fixture as local;
use crate::support::tasks::{account, at, command, create, definition, id, record, view};
use anyhow::Result;
use atlas_core::{Store, tasks::*};
async fn numeric(s: Store) -> Result<()> {
    s.migrate().await?;
    let a = account(&s).await?;
    let mut d = definition(
        "2026-09-07",
        Carry::CloseIncomplete,
        Participation::Personal,
    );
    d.schedule.repeat.as_mut().unwrap().frequency = Frequency::Weekly;
    d.close_days_after = 7;
    d.goal = Goal::Numeric {
        minimum: Some(Decimal("3".into())),
        maximum: None,
        unit: "workouts".into(),
    };
    let task = create(&s, &a, d, None, at(8)).await?;
    let occurrence = view(&s, &a, &task, at(8)).await?.remove(0).id;
    let entry = id();
    let op = id();
    let c = TaskCommand::Record {
        occurrence_id: occurrence.clone(),
        subject_account_id: a.clone(),
        entry_id: entry.clone(),
        evidence: Evidence::Quantity {
            amount: Decimal("1".into()),
        },
        replaces: None,
        expected_version: None,
        happened_at: at(8),
    };
    let (one, two) = tokio::join!(
        s.task_command(&a, &op, &c, None, at(8)),
        record(
            &s,
            &a,
            &occurrence,
            Evidence::Quantity {
                amount: Decimal("2".into())
            },
            None,
            at(8),
            at(8)
        )
    );
    let revision = one?.revision;
    two?;
    assert_eq!(
        s.task_command(&a, &op, &c, None, at(8)).await?.revision,
        revision
    );
    assert_eq!(
        view(&s, &a, &task, at(8)).await?[0].outcome,
        Outcome::Complete
    );
    let correction = TaskCommand::Record {
        occurrence_id: occurrence.clone(),
        subject_account_id: a.clone(),
        entry_id: id(),
        evidence: Evidence::Quantity {
            amount: Decimal("0".into()),
        },
        replaces: Some(entry.clone()),
        expected_version: None,
        happened_at: at(8),
    };
    command(&s, &a, correction, at(8)).await?;
    assert_ne!(
        view(&s, &a, &task, at(8)).await?[0].outcome,
        Outcome::Complete
    );
    let duplicate = TaskCommand::Record {
        occurrence_id: occurrence,
        subject_account_id: a.clone(),
        entry_id: id(),
        evidence: Evidence::Quantity {
            amount: Decimal("7".into()),
        },
        replaces: Some(entry),
        expected_version: None,
        happened_at: at(8),
    };
    assert_eq!(
        s.task_command(&a, &id(), &duplicate, None, at(8))
            .await
            .unwrap_err()
            .to_string(),
        "conflict"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM receipts WHERE account_id=$1 AND operation_id=$2"
        )
        .bind(&a)
        .bind(&op)
        .fetch_one(&s.pool)
        .await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn tasks_numeric() -> Result<()> {
    let (_dir, s) = local().await?;
    numeric(s).await
}
