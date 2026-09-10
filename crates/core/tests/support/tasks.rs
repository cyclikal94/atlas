use anyhow::Result;
use atlas_core::{
    Store,
    policy::{Policy, PrincipalGrant},
    tasks::*,
};
use chrono::{TimeZone, Utc};
use uuid::Uuid;

pub(crate) fn id() -> String {
    Uuid::new_v4().to_string()
}

pub(crate) fn at(day: u32) -> i64 {
    Utc.with_ymd_and_hms(2026, 9, day, 12, 0, 0)
        .unwrap()
        .timestamp()
}

pub(crate) fn definition(start: &str, carry: Carry, participation: Participation) -> Definition {
    Definition {
        schedule: Schedule {
            start_date: Some(start.into()),
            time: None,
            timezone: "Europe/Vienna".into(),
            repeat: Some(Cadence {
                frequency: Frequency::Daily,
                interval: 1,
            }),
        },
        goal: Goal::Checkbox,
        carry,
        participation,
        open_days_before: 0,
        close_days_after: 1,
        allow_streak_exclusions: false,
    }
}

pub(crate) fn shared(account: &str, edit: bool) -> Policy {
    Policy {
        grants: vec![PrincipalGrant::Account {
            id: account.into(),
            edit,
        }],
        exclude_accounts: vec![],
    }
}

pub(crate) async fn account(s: &Store) -> Result<String> {
    let a = id();
    s.add_account(
        &a,
        &format!("task-{}", Uuid::new_v4().simple()),
        "test-only",
    )
    .await?;
    Ok(a)
}

pub(crate) async fn command(s: &Store, a: &str, c: TaskCommand, now: i64) -> Result<i64> {
    Ok(s.task_command(a, &id(), &c, None, now).await?.revision)
}

pub(crate) async fn create(
    s: &Store,
    a: &str,
    definition: Definition,
    policy: Option<Policy>,
    now: i64,
) -> Result<String> {
    let task = id();
    command(
        s,
        a,
        TaskCommand::CreateTask {
            id: task.clone(),
            execution_id: id(),
            title: "Task".into(),
            definition,
            anchor: None,
            initial_policy: policy,
        },
        now,
    )
    .await?;
    Ok(task)
}

pub(crate) async fn view(s: &Store, a: &str, t: &str, now: i64) -> Result<Vec<OccurrenceView>> {
    Ok(s.occurrence_view(
        a,
        &ViewFilter {
            task_id: Some(t.into()),
            limit: Some(200),
            ..Default::default()
        },
        now,
    )
    .await?
    .items)
}

pub(crate) async fn refresh(s: &Store, a: &str, t: &str, day: &str, now: i64) -> Result<()> {
    command(
        s,
        a,
        TaskCommand::Materialise {
            task_id: t.into(),
            through_date: day.into(),
            limit: 16,
        },
        now,
    )
    .await?;
    Ok(())
}

pub(crate) async fn record(
    s: &Store,
    a: &str,
    occurrence: &str,
    evidence: Evidence,
    version: Option<i64>,
    happened_at: i64,
    now: i64,
) -> Result<()> {
    command(
        s,
        a,
        TaskCommand::Record {
            occurrence_id: occurrence.into(),
            subject_account_id: a.into(),
            entry_id: id(),
            evidence,
            replaces: None,
            expected_version: version,
            happened_at,
        },
        now,
    )
    .await?;
    Ok(())
}
