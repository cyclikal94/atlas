use crate::support::database::fixture as local;
use crate::support::tasks::{account, at, command, create, definition, id, shared};
use anyhow::Result;
use atlas_core::{Change, Command, Store, tasks::*};
async fn lists_and_fields(s: Store) -> Result<()> {
    s.migrate().await?;
    let a = account(&s).await?;
    let b = account(&s).await?;
    let task = create(
        &s,
        &a,
        definition(
            "2026-09-08",
            Carry::CloseIncomplete,
            Participation::Personal,
        ),
        None,
        at(8),
    )
    .await?;
    let list = id();
    command(
        &s,
        &a,
        TaskCommand::CreateList {
            id: list.clone(),
            name: "Shared list".into(),
            initial_policy: Some(shared(&b, true)),
        },
        at(8),
    )
    .await?;
    command(
        &s,
        &a,
        TaskCommand::ListItem {
            list_id: list.clone(),
            expected_version: 1,
            task_id: task.clone(),
            included: true,
        },
        at(8),
    )
    .await?;
    let lists = s.resources(&b, "list", None, false, None, 50).await?;
    assert_eq!(
        serde_json::from_value::<serde_json::Value>(lists[0].value.clone())?["task_ids"],
        serde_json::json!([])
    );
    s.apply(
        &a,
        &id(),
        &[Command::Grant {
            id: task.clone(),
            expected_version: 1,
            account_id: b.clone(),
            edit: false,
        }],
    )
    .await?;
    let snapshot = s.sync(&b, "lists", None, 200, at(8)).await?;
    assert!(snapshot.batches.iter().flat_map(|b|&b.changes).any(|c|matches!(c,Change::Upsert{resource} if resource.id==list && resource.value.to_string().contains(&task))));
    s.apply(
        &a,
        &id(),
        &[Command::Revoke {
            id: task.clone(),
            expected_version: 2,
            account_id: b.clone(),
        }],
    )
    .await?;
    let delta = s
        .sync(&b, "lists", Some(&snapshot.next_cursor), 200, at(8))
        .await?;
    assert!(delta.batches.iter().flat_map(|b|&b.changes).any(|c|matches!(c,Change::Upsert{resource} if resource.id==list && !resource.value.to_string().contains(&task))));
    let field = id();
    command(
        &s,
        &a,
        TaskCommand::PutField {
            id: field.clone(),
            parent_id: task.clone(),
            expected_version: None,
            label: "Target date".into(),
            value: FieldValue::Date {
                year: None,
                month: 2,
                day: 29,
            },
            initial_policy: None,
        },
        at(8),
    )
    .await?;
    assert!(
        s.resources(&b, "field", None, false, None, 50)
            .await?
            .is_empty()
    );
    assert_eq!(
        s.apply(
            &a,
            &id(),
            &[Command::Edit {
                id: field.clone(),
                expected_version: 1,
                label: "Bypass type".into(),
                value: "bad".into()
            }]
        )
        .await
        .unwrap_err()
        .to_string(),
        "invalid_value"
    );
    command(
        &s,
        &a,
        TaskCommand::Archive {
            id: field,
            expected_version: 1,
            archived: true,
        },
        at(8),
    )
    .await?;
    assert!(
        s.resources(&a, "field", Some(&task), false, None, 50)
            .await?
            .is_empty()
    );
    assert_eq!(
        s.resources(&a, "field", Some(&task), true, None, 50)
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn tasks_lists_fields() -> Result<()> {
    let (_dir, s) = local().await?;
    lists_and_fields(s).await
}
