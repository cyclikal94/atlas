use anyhow::Result;
use atlas_core::{Store, tasks::*};

use uuid::Uuid;
const NOW: i64 = 1788868800;

pub(crate) fn id() -> String {
    Uuid::new_v4().to_string()
}

pub(crate) use crate::support::database::setup;

pub(crate) async fn account(s: &Store) -> Result<String> {
    let a = id();
    s.add_account(&a, &format!("a{a}"), "test-only").await?;
    Ok(a)
}

pub(crate) async fn task(s: &Store, a: &str, goal: Goal) -> Result<String> {
    let t = id();
    s.task_command(
        a,
        &id(),
        &TaskCommand::CreateTask {
            id: t.clone(),
            execution_id: id(),
            title: "Task".into(),
            anchor: None,
            initial_policy: None,
            definition: Definition {
                schedule: Schedule {
                    start_date: None,
                    time: None,
                    timezone: "Europe/Vienna".into(),
                    repeat: None,
                },
                goal,
                carry: Carry::RetainOne,
                participation: Participation::Personal,
                open_days_before: 0,
                close_days_after: 1,
                allow_streak_exclusions: false,
            },
        },
        None,
        NOW,
    )
    .await?;
    occurrence_id(&t, "once")
}

pub(crate) async fn set(
    s: &Store,
    a: &str,
    o: &str,
    dependencies: Vec<String>,
    strict: bool,
) -> Result<()> {
    let v = sqlx::query_scalar("SELECT version FROM resources WHERE id=$1")
        .bind(o)
        .fetch_one(&s.pool)
        .await?;
    s.task_command(
        a,
        &id(),
        &TaskCommand::SetDependencies {
            occurrence_id: o.into(),
            expected_version: v,
            prerequisites: dependencies,
            strict,
        },
        None,
        NOW,
    )
    .await?;
    Ok(())
}

pub(crate) async fn complete(s: &Store, a: &str, o: &str, mode: CompletionMode) -> Result<()> {
    let p = s.dependency_preview(a, o, NOW).await?;
    s.task_command(
        a,
        &id(),
        &TaskCommand::CompleteDependencies {
            root_evidence: None,
            occurrence_id: o.into(),
            preview_token: p.token,
            mode,
            happened_at: NOW,
        },
        None,
        NOW,
    )
    .await?;
    Ok(())
}

pub(crate) async fn count(s: &Store) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM progress_entries")
        .fetch_one(&s.pool)
        .await?)
}
