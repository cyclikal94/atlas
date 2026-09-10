use crate::support::database::fixture as local;
use crate::support::tasks::{account, at, command, create, definition, id, record, view};
use anyhow::Result;
use atlas_core::{Store, tasks::*};
async fn checklist_and_revision(s: Store) -> Result<()> {
    s.migrate().await?;
    let a = account(&s).await?;
    let mut d = definition(
        "2026-09-08",
        Carry::CloseIncomplete,
        Participation::Personal,
    );
    let one = id();
    let two = id();
    d.goal = Goal::Checklist {
        items: vec![
            ChecklistItem {
                id: one.clone(),
                label: "Pack".into(),
            },
            ChecklistItem {
                id: two.clone(),
                label: "Check keys".into(),
            },
        ],
    };
    let task = create(&s, &a, d.clone(), None, at(8)).await?;
    let occ = view(&s, &a, &task, at(8)).await?[0].id.clone();
    record(
        &s,
        &a,
        &occ,
        Evidence::Checklist {
            item_id: one,
            complete: true,
        },
        Some(1),
        at(8),
        at(8),
    )
    .await?;
    assert_eq!(
        record(
            &s,
            &a,
            &occ,
            Evidence::Checklist {
                item_id: two.clone(),
                complete: true
            },
            Some(1),
            at(8),
            at(8)
        )
        .await
        .unwrap_err()
        .to_string(),
        "conflict"
    );
    record(
        &s,
        &a,
        &occ,
        Evidence::Checklist {
            item_id: two,
            complete: true,
        },
        Some(2),
        at(8),
        at(8),
    )
    .await?;
    assert_eq!(
        view(&s, &a, &task, at(8)).await?[0].outcome,
        Outcome::Complete
    );
    if let Goal::Checklist { items } = &mut d.goal {
        items.push(ChecklistItem {
            id: id(),
            label: "Passport".into(),
        });
    }
    command(
        &s,
        &a,
        TaskCommand::ReviseOccurrence {
            id: occ.clone(),
            expected_version: 1,
            definition: d,
            date: Some("2026-09-08".into()),
            time: None,
        },
        at(8),
    )
    .await?;
    assert_eq!(
        view(&s, &a, &task, at(8)).await?[0].outcome,
        Outcome::Incomplete
    );
    let state: ProgressState = serde_json::from_value(
        s.resources(&a, "progress", Some(&occ), false, None, 50)
            .await?[0]
            .value
            .clone(),
    )?;
    assert_eq!(state.action.outcome, Outcome::Incomplete);
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
    assert_eq!(s.task_streak(&a, &task, at(10)).await?.current, Some(0));
    assert_eq!(view(&s, &a, &task, at(10)).await?.len(), 1);
    let cmd = TaskCommand::CreateList {
        id: id(),
        name: "Guarded".into(),
        initial_policy: None,
    };
    assert_eq!(
        s.task_command(&a, &id(), &cmd, Some(&"0".repeat(64)), at(8))
            .await
            .unwrap_err()
            .to_string(),
        "defaults_changed"
    );
    assert!(
        s.resources(&a, "list", None, false, None, 200)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn tasks_checklist_revision() -> Result<()> {
    let (_dir, s) = local().await?;
    checklist_and_revision(s).await
}
