//! Typed task-command writes commit only against the sharing policy their author saw.
use crate::support::database::fixture;
use crate::support::projection::{policy_version, projection};
use crate::support::sharing::{NOW, id, ledger, policy, replace_policy, shared};
use crate::support::tasks::{at, command, create, definition};
use anyhow::Result;
use atlas_core::{Store, tasks::*};

fn put(
    field: &str,
    parent: &str,
    version: i64,
    policy_version: Option<i64>,
    text: &str,
) -> TaskCommand {
    TaskCommand::PutField {
        id: field.into(),
        parent_id: parent.into(),
        expected_version: Some(version),
        expected_policy_version: policy_version,
        label: "Hobby".into(),
        value: FieldValue::Text { text: text.into() },
        initial_policy: None,
    }
}

async fn rejected(store: &Store, actor: &str, operation: &str, c: &TaskCommand) -> Result<String> {
    match store.task_command(actor, operation, c, None, NOW).await {
        Ok(_) => panic!("the write must be rejected"),
        Err(error) => Ok(error.to_string()),
    }
}

/// Acceptance check 1: the real retained-field flow through `PutField`'s existing-field
/// branch, as an authorised non-owner editor, with the owner narrowing from a second session.
async fn put_field_race(s: &Store) -> Result<()> {
    let w = shared(s).await?;
    // The collaborator reads the field and its sharing revision from their own projection.
    let seen = projection(s, &w.bob, &w.field).await?;
    assert_eq!(seen.policy_version, Some(1));
    replace_policy(s, &w.alice, &w.field, policy(&[(&w.bob, true)])).await?;
    let before = ledger(s, &w.field).await?;
    let operation = id();
    let stale = put(
        &w.field,
        &w.person,
        seen.version,
        seen.policy_version,
        "Stale",
    );
    assert_eq!(rejected(s, &w.bob, &operation, &stale).await?, "conflict");
    // Atomic rejection: no content, revision, batch or receipt changed.
    assert_eq!(ledger(s, &w.field).await?, before);
    // Refetch, then retry under the same operation ID.
    let now = projection(s, &w.bob, &w.field).await?;
    let fresh = put(
        &w.field,
        &w.person,
        now.version,
        now.policy_version,
        "Fresh",
    );
    s.task_command(&w.bob, &operation, &fresh, None, NOW)
        .await?;
    let after = projection(s, &w.bob, &w.field).await?;
    assert_eq!(
        (
            after.value["text"].as_str(),
            after.version,
            after.policy_version
        ),
        (Some("Fresh"), 2, Some(2))
    );
    Ok(())
}

/// Acceptance checks 3 and 4, and the creation branch: owner writes are gated, absence
/// fails closed, and creating a field has no earlier policy to race against.
async fn put_field_owner_and_create_rules(s: &Store) -> Result<()> {
    let w = shared(s).await?;
    for actor in [&w.alice, &w.bob] {
        let omitted = put(&w.field, &w.person, 1, None, "Omitted");
        assert_eq!(rejected(s, actor, &id(), &omitted).await?, "conflict");
    }
    // Owner: a sharing change makes an earlier capture stale, and the current value works.
    replace_policy(s, &w.alice, &w.field, policy(&[(&w.bob, true)])).await?;
    let stale = put(&w.field, &w.person, 1, Some(1), "Stale");
    assert_eq!(rejected(s, &w.alice, &id(), &stale).await?, "conflict");
    let current = policy_version(s, &w.alice, &w.field).await?;
    let ok = put(&w.field, &w.person, 1, Some(current), "Owner");
    s.task_command(&w.alice, &id(), &ok, None, NOW).await?;
    // Creation carries no policy revision; supplying one is a malformed request.
    let create = |policy_version| TaskCommand::PutField {
        id: id(),
        parent_id: w.person.clone(),
        expected_version: None,
        expected_policy_version: policy_version,
        label: "New".into(),
        value: FieldValue::Text { text: "New".into() },
        initial_policy: Some(policy(&[(&w.bob, true)])),
    };
    assert_eq!(
        rejected(s, &w.alice, &id(), &create(Some(1))).await?,
        "invalid_value"
    );
    s.task_command(&w.alice, &id(), &create(None), None, NOW)
        .await?;
    Ok(())
}

/// The same race on a field under a shared task rather than a person.
async fn put_field_under_a_task(s: &Store) -> Result<()> {
    let w = shared(s).await?;
    let task = create(
        s,
        &w.alice,
        definition(
            "2026-09-08",
            Carry::CloseIncomplete,
            Participation::Personal,
        ),
        Some(policy(&[(&w.bob, true), (&w.carol, true)])),
        at(8),
    )
    .await?;
    let field = id();
    command(
        s,
        &w.alice,
        TaskCommand::PutField {
            id: field.clone(),
            parent_id: task.clone(),
            expected_version: None,
            expected_policy_version: None,
            label: "Hobby".into(),
            value: FieldValue::Text {
                text: "Surfing".into(),
            },
            initial_policy: Some(policy(&[(&w.bob, true), (&w.carol, true)])),
        },
        at(8),
    )
    .await?;
    let seen = projection(s, &w.bob, &field).await?;
    replace_policy(s, &w.alice, &field, policy(&[(&w.bob, true)])).await?;
    let stale = put(&field, &task, seen.version, seen.policy_version, "Stale");
    assert_eq!(rejected(s, &w.bob, &id(), &stale).await?, "conflict");
    Ok(())
}

/// Titles, definitions and list names are authored text on governed resources too.
async fn task_and_list_edits(s: &Store) -> Result<()> {
    let w = shared(s).await?;
    let both = policy(&[(&w.bob, true), (&w.carol, true)]);
    let def = || {
        definition(
            "2026-09-08",
            Carry::CloseIncomplete,
            Participation::Personal,
        )
    };
    let task = create(s, &w.alice, def(), Some(both.clone()), at(8)).await?;
    let list = id();
    command(
        s,
        &w.alice,
        TaskCommand::CreateList {
            id: list.clone(),
            name: "Chores".into(),
            initial_policy: Some(both),
        },
        at(8),
    )
    .await?;
    let revise = |pv| TaskCommand::ReviseTask {
        id: task.clone(),
        expected_version: 1,
        expected_policy_version: pv,
        title: "Renamed".into(),
        definition: def(),
    };
    let rename = |pv| TaskCommand::EditList {
        id: list.clone(),
        expected_version: 1,
        expected_policy_version: pv,
        name: "Errands".into(),
    };
    for (resource, seen) in [(&task, Some(1)), (&list, Some(1))] {
        assert_eq!(policy_version(s, &w.bob, resource).await?, seen.unwrap());
    }
    for resource in [&task, &list] {
        replace_policy(s, &w.alice, resource, policy(&[(&w.bob, true)])).await?;
    }
    let before = (ledger(s, &task).await?, ledger(s, &list).await?);
    for stale in [Some(1), None] {
        assert_eq!(
            rejected(s, &w.bob, &id(), &revise(stale)).await?,
            "conflict"
        );
        assert_eq!(
            rejected(s, &w.bob, &id(), &rename(stale)).await?,
            "conflict"
        );
    }
    assert_eq!((ledger(s, &task).await?, ledger(s, &list).await?), before);
    s.task_command(&w.bob, &id(), &revise(Some(2)), None, NOW)
        .await?;
    s.task_command(&w.bob, &id(), &rename(Some(2)), None, NOW)
        .await?;
    assert_eq!(projection(s, &w.bob, &task).await?.label, "Renamed");
    assert_eq!(projection(s, &w.bob, &list).await?.label, "Errands");
    Ok(())
}

/// The wire forms decode as for `Command::Edit`, and a legacy-shaped command keeps the
/// canonical text that receipts were computed from.
#[test]
fn wire_forms_of_the_precondition() -> Result<()> {
    let body = r#"{"kind":"put_field","id":"a","parent_id":"b","expected_version":2,"label":"L","value":{"kind":"text","text":"T"},"initial_policy":null"#;
    let omitted: TaskCommand = serde_json::from_str(&format!("{body}}}"))?;
    let null: TaskCommand =
        serde_json::from_str(&format!("{body},\"expected_policy_version\":null}}"))?;
    let set: TaskCommand =
        serde_json::from_str(&format!("{body},\"expected_policy_version\":5}}"))?;
    for command in [&omitted, &null] {
        let TaskCommand::PutField {
            expected_policy_version,
            ..
        } = command
        else {
            panic!("expected put_field")
        };
        assert_eq!(*expected_policy_version, None);
        assert!(!serde_json::to_string(command)?.contains("expected_policy_version"));
    }
    assert!(serde_json::to_string(&set)?.contains(r#""expected_policy_version":5"#));
    Ok(())
}

#[tokio::test]
async fn put_field_stale_after_narrowing_conflicts_for_a_non_owner() -> Result<()> {
    let (_dir, s) = fixture().await?;
    put_field_race(&s).await
}

#[tokio::test]
async fn put_field_owner_writes_and_creation_follow_the_rules() -> Result<()> {
    let (_dir, s) = fixture().await?;
    put_field_owner_and_create_rules(&s).await
}

#[tokio::test]
async fn put_field_under_a_shared_task_is_gated() -> Result<()> {
    let (_dir, s) = fixture().await?;
    put_field_under_a_task(&s).await
}

#[tokio::test]
async fn revise_task_and_edit_list_require_the_policy_version() -> Result<()> {
    let (_dir, s) = fixture().await?;
    task_and_list_edits(&s).await
}
