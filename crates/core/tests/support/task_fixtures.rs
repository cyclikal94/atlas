use anyhow::Result;
use atlas_core::{Command, Store, policy::Policy, tasks::*};

const NOW: i64 = 1788868800;
use uuid::Uuid;

pub(crate) fn id() -> String {
    Uuid::new_v4().to_string()
}

pub(crate) use crate::support::database::setup;

pub(crate) async fn account(s: &Store) -> Result<String> {
    let a = id();
    s.add_account(&a, &format!("r{a}"), "test-only").await?;
    Ok(a)
}

pub(crate) async fn person(s: &Store, a: &str) -> Result<String> {
    let p = id();
    s.apply(
        a,
        &id(),
        &[Command::CreatePerson {
            id: p.clone(),
            name: "Friend".into(),
            initial_policy: Some(Policy::default()),
        }],
    )
    .await?;
    Ok(p)
}

pub(crate) async fn ledger(s: &Store, a: &str, device: &str) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM sync_deliveries WHERE account_id=$1 AND device_id=$2",
    )
    .bind(a)
    .bind(device)
    .fetch_one(&s.pool)
    .await?)
}

pub(crate) fn definition(goal: Goal) -> Definition {
    Definition {
        schedule: Schedule {
            start_date: None,
            time: None,
            timezone: "UTC".into(),
            repeat: None,
        },
        goal,
        carry: Carry::RetainOne,
        participation: Participation::Personal,
        open_days_before: 0,
        close_days_after: 1,
        allow_streak_exclusions: false,
    }
}

pub(crate) async fn task(s: &Store, a: &str, definition: Definition) -> Result<(String, String)> {
    let t = id();
    let o = occurrence_id(&t, "once")?;
    s.task_command(
        a,
        &id(),
        &TaskCommand::CreateTask {
            id: t.clone(),
            execution_id: id(),
            title: "Review".into(),
            definition,
            initial_policy: None,
            anchor: None,
        },
        None,
        NOW,
    )
    .await?;
    Ok((t, o))
}

pub(crate) async fn run(s: &Store, a: &str, c: TaskCommand, now: i64) -> Result<()> {
    s.task_command(a, &id(), &c, None, now).await?;
    Ok(())
}
