//! BE-PEOPLE-ACTIVITY: the People activity timeline (`person_activity`) and its write side
//! (schema `1007`). Cases follow plan §9.2 (A1–A14; A12 lives in `storage::migration`).
//!
//! Kieran's 1 November 2026 decisions are the oracle: metadata only and no old values (D-VAL);
//! actors named only when they are the reader, the person's current owner or its currently linked
//! account (D-ACT); sharing changes are not recorded (D-SHR). Every read applies current access.
//! Fixtures are built through the public commands; raw SQL only reads the event table (the
//! independent reference model) or forces a failure.
use crate::support::{
    database::{fixture, postgres},
    sharing::{NOW, account, id, replace_policy},
    task_fixtures::{definition, task},
};
use anyhow::Result;
use atlas_core::{
    Command, Store,
    error::ErrorCode,
    households::ManagementCommand as M,
    people::{
        ActivityAccount, ActivityType as T, ChangedPart, PeopleCommand, PersonActivityEvent,
        PersonActivityPage, TargetState,
    },
    policy::{Policy, PrincipalGrant},
    resources::FieldValue,
    tasks::{Goal, TaskCommand},
};
use std::collections::BTreeSet;
use std::time::Duration;

// ------------------------------------------------------------------------------------- helpers

fn grants(accounts: &[(&str, bool)]) -> Policy {
    Policy {
        grants: accounts
            .iter()
            .map(|(id, edit)| PrincipalGrant::Account {
                id: (*id).into(),
                edit: *edit,
            })
            .collect(),
        exclude_accounts: vec![],
    }
}

fn code(error: &anyhow::Error) -> Option<ErrorCode> {
    error.downcast_ref::<ErrorCode>().copied()
}

async fn versions(s: &Store, resource: &str) -> Result<(i64, i64)> {
    Ok(
        sqlx::query_as("SELECT version,policy_version FROM resources WHERE id=$1")
            .bind(resource)
            .fetch_one(&s.pool)
            .await?,
    )
}

async fn create_person(s: &Store, owner: &str, name: &str, policy: Policy) -> Result<String> {
    let person = id();
    s.apply(
        owner,
        &id(),
        &[Command::CreatePerson {
            id: person.clone(),
            name: name.into(),
            initial_policy: Some(policy),
        }],
    )
    .await?;
    Ok(person)
}

async fn create_field(
    s: &Store,
    owner: &str,
    person: &str,
    label: &str,
    policy: Policy,
) -> Result<String> {
    let field = id();
    s.apply(
        owner,
        &id(),
        &[Command::CreateField {
            id: field.clone(),
            person_id: person.into(),
            label: label.into(),
            value: "Initial".into(),
            initial_policy: Some(policy),
        }],
    )
    .await?;
    Ok(field)
}

async fn edit(s: &Store, actor: &str, resource: &str, label: &str, value: &str) -> Result<i64> {
    let (version, policy_version) = versions(s, resource).await?;
    s.apply(
        actor,
        &id(),
        &[Command::Edit {
            id: resource.into(),
            expected_version: version,
            expected_policy_version: Some(policy_version),
            label: label.into(),
            value: value.into(),
        }],
    )
    .await
}

async fn put_field(
    s: &Store,
    actor: &str,
    parent: &str,
    label: &str,
    value: FieldValue,
    policy: Policy,
) -> Result<String> {
    let field = id();
    s.task_command(
        actor,
        &id(),
        &TaskCommand::PutField {
            id: field.clone(),
            parent_id: parent.into(),
            expected_version: None,
            expected_policy_version: None,
            label: label.into(),
            value,
            initial_policy: Some(policy),
        },
        None,
        NOW,
    )
    .await?;
    Ok(field)
}

async fn revise_field(
    s: &Store,
    actor: &str,
    field: &str,
    parent: &str,
    label: &str,
    value: FieldValue,
) -> Result<i64> {
    let (version, policy_version) = versions(s, field).await?;
    Ok(s.task_command(
        actor,
        &id(),
        &TaskCommand::PutField {
            id: field.into(),
            parent_id: parent.into(),
            expected_version: Some(version),
            expected_policy_version: Some(policy_version),
            label: label.into(),
            value,
            initial_policy: None,
        },
        None,
        NOW,
    )
    .await?
    .revision)
}

async fn archive(s: &Store, actor: &str, resource: &str, archived: bool) -> Result<()> {
    let (version, _) = versions(s, resource).await?;
    s.task_command(
        actor,
        &id(),
        &TaskCommand::Archive {
            id: resource.into(),
            expected_version: version,
            archived,
        },
        None,
        NOW,
    )
    .await?;
    Ok(())
}

async fn request_link(s: &Store, sender: &str, subject: &str, person: &str) -> Result<String> {
    let request = id();
    let version = s.person_detail(sender, person).await?.person.version;
    s.people_command(
        sender,
        &id(),
        &PeopleCommand::RequestLink {
            id: request.clone(),
            person_id: person.into(),
            account_id: subject.into(),
            expected_version: version,
        },
        NOW,
    )
    .await?;
    Ok(request)
}

async fn respond(s: &Store, recipient: &str, request: &str, accept: bool) -> Result<()> {
    s.people_command(
        recipient,
        &id(),
        &PeopleCommand::RespondRequest {
            id: request.into(),
            accept,
            recipient_preview_token: None,
        },
        NOW,
    )
    .await?;
    Ok(())
}

/// `sender` asks `subject` to link `person`, and `subject` accepts: ownership moves to `subject`.
async fn link(s: &Store, sender: &str, subject: &str, person: &str) -> Result<()> {
    let request = request_link(s, sender, subject, person).await?;
    respond(s, subject, &request, true).await
}

async fn unlink(s: &Store, actor: &str, person: &str) -> Result<()> {
    let version = s.person_detail(actor, person).await?.person.version;
    s.people_command(
        actor,
        &id(),
        &PeopleCommand::Unlink {
            person_id: person.into(),
            expected_version: version,
        },
        NOW,
    )
    .await?;
    Ok(())
}

async fn rename_profile(s: &Store, actor: &str, person: &str, name: &str) -> Result<()> {
    let version = s.person_detail(actor, person).await?.person.version;
    s.people_command(
        actor,
        &id(),
        &PeopleCommand::RenameProfile {
            person_id: person.into(),
            expected_version: version,
            name: name.into(),
        },
        NOW,
    )
    .await?;
    Ok(())
}

async fn reference(s: &Store, actor: &str, account: &str) -> Result<String> {
    Ok(s.people_command(
        actor,
        &id(),
        &PeopleCommand::ReferenceAccount {
            account_id: account.into(),
            person_id: id(),
        },
        NOW,
    )
    .await?
    .person_id)
}

async fn merge(s: &Store, actor: &str, source: &str, target: &str, name: &str) -> Result<()> {
    let preview = s.merge_preview(actor, source, target).await?;
    s.people_command(
        actor,
        &id(),
        &PeopleCommand::Merge {
            source_id: source.into(),
            target_id: target.into(),
            preview_token: preview.token,
            name: name.into(),
        },
        NOW,
    )
    .await?;
    Ok(())
}

async fn manage(s: &Store, actor: &str, command: M) -> Result<i64> {
    s.management(actor, &id(), &[command], NOW).await
}

async fn household_version(s: &Store, member: &str, household: &str) -> Result<i64> {
    Ok(s.households(member)
        .await?
        .into_iter()
        .find(|h| h.id == household)
        .expect("a member")
        .version)
}

/// A household managed by `owner` that `member` has joined.
async fn household(s: &Store, owner: &str, member: &str) -> Result<String> {
    let household = id();
    manage(
        s,
        owner,
        M::CreateHousehold {
            id: household.clone(),
            name: "Home".into(),
        },
    )
    .await?;
    let invitation = id();
    let expected_version = household_version(s, owner, &household).await?;
    manage(
        s,
        owner,
        M::InviteToHousehold {
            id: invitation.clone(),
            household_id: household.clone(),
            recipient_id: member.into(),
            expected_version,
        },
    )
    .await?;
    manage(
        s,
        member,
        M::RespondToHouseholdInvitation {
            id: invitation,
            expected_version: 1,
            accept: true,
        },
    )
    .await?;
    Ok(household)
}

/// One raw event row: the independent reference model reads the table directly.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Raw {
    id: String,
    event_type: String,
    person_id: String,
    target_id: String,
    actor_id: String,
    account_id: Option<String>,
    merged_from: Option<String>,
    label_changed: i64,
    value_changed: i64,
    revision: i64,
    ordinal: i64,
}

/// Every event in commit order (oldest first).
async fn raw(s: &Store) -> Result<Vec<Raw>> {
    use sqlx::Row;
    Ok(sqlx::query("SELECT id,event_type,person_id,target_id,actor_id,account_id,merged_from,label_changed,value_changed,revision,ordinal FROM person_activity_events ORDER BY revision,ordinal")
        .fetch_all(&s.pool)
        .await?
        .iter()
        .map(|r| Raw {
            id: r.get(0),
            event_type: r.get(1),
            person_id: r.get(2),
            target_id: r.get(3),
            actor_id: r.get(4),
            account_id: r.get(5),
            merged_from: r.get(6),
            label_changed: r.get(7),
            value_changed: r.get(8),
            revision: r.get(9),
            ordinal: r.get(10),
        })
        .collect())
}

async fn count(s: &Store) -> Result<usize> {
    Ok(raw(s).await?.len())
}

/// Events added after `before` rows, as `(type, target)`.
async fn added(s: &Store, before: usize) -> Result<Vec<(String, String)>> {
    Ok(raw(s)
        .await?
        .into_iter()
        .skip(before)
        .map(|r| (r.event_type, r.target_id))
        .collect())
}

/// The whole timeline `viewer` sees, following `next_after` at page size `limit`.
async fn walk(
    s: &Store,
    viewer: &str,
    person: &str,
    limit: u16,
) -> Result<Vec<PersonActivityEvent>> {
    let mut items = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = s
            .person_activity(viewer, person, after.as_deref(), limit)
            .await?;
        assert!(page.items.len() <= usize::from(limit));
        if let Some(next) = &page.next_after {
            assert_eq!(
                page.items.len(),
                usize::from(limit),
                "only a full page continues"
            );
            assert_eq!(Some(next), page.items.last().map(|e| &e.id));
        }
        items.extend(page.items);
        match page.next_after {
            Some(next) => after = Some(next),
            None => return Ok(items),
        }
    }
}

fn types(items: &[PersonActivityEvent]) -> Vec<T> {
    items.iter().map(|e| e.event_type).collect()
}

fn ids_of(items: &[PersonActivityEvent]) -> Vec<String> {
    items.iter().map(|e| e.id.clone()).collect()
}

// A1 ------------------------------------------------------------------------------------------
// Every supported write records exactly its catalogue events; everything else records nothing.

#[tokio::test]
async fn a1_each_supported_change_records_exactly_its_event() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (a, b) = (account(&s).await?, account(&s).await?);

    // Command::CreatePerson and CreateField.
    let n = count(&s).await?;
    let p = create_person(&s, &a, "Morgan", Policy::default()).await?;
    assert_eq!(added(&s, n).await?, [("person_created".into(), p.clone())]);
    let n = count(&s).await?;
    let f = create_field(&s, &a, &p, "Hobby", Policy::default()).await?;
    assert_eq!(added(&s, n).await?, [("field_created".into(), f.clone())]);
    let row = raw(&s).await?.pop().unwrap();
    assert_eq!(
        (row.person_id.as_str(), row.actor_id.as_str()),
        (p.as_str(), a.as_str())
    );

    // Command::Edit: a rename of the person, then label-only, value-only and both on the field.
    let n = count(&s).await?;
    edit(&s, &a, &p, "Morgan Lee", "").await?;
    assert_eq!(added(&s, n).await?, [("person_renamed".into(), p.clone())]);
    for (label, value, flags) in [
        ("Pastime", "Initial", (1, 0)),
        ("Pastime", "Surfing", (0, 1)),
        ("Sport", "Rowing", (1, 1)),
    ] {
        let n = count(&s).await?;
        edit(&s, &a, &f, label, value).await?;
        assert_eq!(added(&s, n).await?, [("field_edited".into(), f.clone())]);
        let row = raw(&s).await?.pop().unwrap();
        assert_eq!((row.label_changed, row.value_changed), flags);
        assert_eq!(row.person_id, p);
    }

    // TaskCommand::PutField (typed) create and revise, on a person.
    let n = count(&s).await?;
    let typed = put_field(
        &s,
        &a,
        &p,
        "Anniversary",
        FieldValue::Date {
            year: None,
            month: 5,
            day: 1,
        },
        Policy::default(),
    )
    .await?;
    assert_eq!(
        added(&s, n).await?,
        [("field_created".into(), typed.clone())]
    );
    let n = count(&s).await?;
    revise_field(
        &s,
        &a,
        &typed,
        &p,
        "Anniversary",
        FieldValue::Date {
            year: None,
            month: 6,
            day: 1,
        },
    )
    .await?;
    assert_eq!(
        added(&s, n).await?,
        [("field_edited".into(), typed.clone())]
    );
    assert_eq!(raw(&s).await?.pop().unwrap().value_changed, 1);

    // TaskCommand::Archive on the field and the person, both ways.
    for (resource, archived, event) in [
        (&f, true, "field_archived"),
        (&f, false, "field_restored"),
        (&p, true, "person_archived"),
        (&p, false, "person_restored"),
    ] {
        let n = count(&s).await?;
        archive(&s, &a, resource, archived).await?;
        assert_eq!(added(&s, n).await?, [(event.to_string(), resource.clone())]);
    }

    // Link (request + accept): only the acceptance records, attributed to the accepter.
    let n = count(&s).await?;
    let request = request_link(&s, &a, &b, &p).await?;
    assert_eq!(count(&s).await?, n, "a request records nothing");
    respond(&s, &b, &request, true).await?;
    assert_eq!(added(&s, n).await?, [("person_linked".into(), p.clone())]);
    let row = raw(&s).await?.pop().unwrap();
    assert_eq!(
        (row.actor_id.as_str(), row.account_id.as_deref()),
        (b.as_str(), Some(b.as_str()))
    );

    // RenameProfile by the linked account, then Unlink.
    let n = count(&s).await?;
    rename_profile(&s, &b, &p, "Morgan B").await?;
    assert_eq!(added(&s, n).await?, [("person_renamed".into(), p.clone())]);
    let n = count(&s).await?;
    unlink(&s, &b, &p).await?;
    assert_eq!(added(&s, n).await?, [("person_unlinked".into(), p.clone())]);
    assert_eq!(
        raw(&s).await?.pop().unwrap().account_id.as_deref(),
        Some(b.as_str())
    );

    // ReferenceAccount that creates: created then linked, one commit, in that order.
    let c = account(&s).await?;
    let n = count(&s).await?;
    let referenced = reference(&s, &a, &c).await?;
    let rows: Vec<Raw> = raw(&s).await?.into_iter().skip(n).collect();
    assert_eq!(
        rows.iter()
            .map(|r| (r.event_type.as_str(), r.ordinal))
            .collect::<Vec<_>>(),
        [("person_created", 0), ("person_linked", 1)]
    );
    assert_eq!(rows[0].revision, rows[1].revision);
    assert!(
        rows.iter()
            .all(|r| r.target_id == referenced && r.actor_id == a)
    );
    assert_eq!(rows[1].account_id.as_deref(), Some(c.as_str()));

    // Merge of two people the actor owns: one event on the target naming the source.
    let source = create_person(&s, &a, "Source", Policy::default()).await?;
    let target = create_person(&s, &a, "Target", Policy::default()).await?;
    let n = count(&s).await?;
    merge(&s, &a, &source, &target, "Merged").await?;
    let rows: Vec<Raw> = raw(&s).await?.into_iter().skip(n).collect();
    assert_eq!(
        rows.len(),
        1,
        "re-parenting, archiving and renaming are part of the merge"
    );
    assert_eq!(
        (
            rows[0].event_type.as_str(),
            rows[0].target_id.as_str(),
            rows[0].merged_from.as_deref()
        ),
        ("person_merged", target.as_str(), Some(source.as_str()))
    );
    Ok(())
}

#[tokio::test]
async fn a1_paths_outside_the_catalogue_and_no_op_writes_record_nothing() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (a, b, c) = (account(&s).await?, account(&s).await?, account(&s).await?);
    let p = create_person(&s, &a, "Morgan", Policy::default()).await?;
    let f = create_field(&s, &a, &p, "Hobby", Policy::default()).await?;
    let n = count(&s).await?;

    // No-op edits, rename and archive: the version moves, the timeline does not.
    edit(&s, &a, &p, "Morgan", "").await?;
    edit(&s, &a, &f, "Hobby", "Initial").await?;
    archive(&s, &a, &f, false).await?;
    archive(&s, &a, &p, false).await?;
    revise_field(
        &s,
        &a,
        &f,
        &p,
        "Hobby",
        FieldValue::Text {
            text: "Initial".into(),
        },
    )
    .await?;
    // Sharing changes: Grant, Revoke and a whole-policy replacement (D-SHR).
    let (_, pv) = versions(&s, &p).await?;
    s.apply(
        &a,
        &id(),
        &[Command::Grant {
            id: p.clone(),
            expected_version: pv,
            account_id: b.clone(),
            edit: false,
        }],
    )
    .await?;
    let (_, pv) = versions(&s, &p).await?;
    s.apply(
        &a,
        &id(),
        &[Command::Revoke {
            id: p.clone(),
            expected_version: pv,
            account_id: b.clone(),
        }],
    )
    .await?;
    replace_policy(&s, &a, &p, grants(&[(&b, false)])).await?;
    // Households.
    household(&s, &a, &c).await?;
    // Requests that are cancelled or declined, and a merge request.
    let cancelled = request_link(&s, &a, &b, &p).await?;
    s.people_command(
        &a,
        &id(),
        &PeopleCommand::CancelRequest { id: cancelled },
        NOW,
    )
    .await?;
    let declined = request_link(&s, &a, &b, &p).await?;
    respond(&s, &b, &declined, false).await?;
    let theirs = create_person(&s, &b, "Theirs", grants(&[(&a, false)])).await?;
    let preview = s.merge_preview(&a, &p, &theirs).await?;
    s.people_command(
        &a,
        &id(),
        &PeopleCommand::RequestMerge {
            id: id(),
            source_id: p.clone(),
            target_id: theirs.clone(),
            preview_token: preview.token,
            name: "Morgan".into(),
        },
        NOW,
    )
    .await?;
    let n_theirs = count(&s).await?;
    // Referencing an identity that already exists creates nothing.
    let existing = reference(&s, &a, &c).await?;
    let n_ref = count(&s).await?;
    assert_eq!(reference(&s, &b, &c).await?, existing);
    assert_eq!(count(&s).await?, n_ref);
    // Tasks, task fields and lists.
    let (t, _) = task(&s, &a, definition(Goal::Checkbox)).await?;
    let tf = put_field(
        &s,
        &a,
        &t,
        "Note",
        FieldValue::Text { text: "x".into() },
        Policy::default(),
    )
    .await?;
    revise_field(
        &s,
        &a,
        &tf,
        &t,
        "Note 2",
        FieldValue::Text { text: "y".into() },
    )
    .await?;
    edit(&s, &a, &tf, "Note 3", "z").await?;
    archive(&s, &a, &tf, true).await?;
    archive(&s, &a, &t, true).await?;
    let list = id();
    s.task_command(
        &a,
        &id(),
        &TaskCommand::CreateList {
            id: list.clone(),
            name: "Errands".into(),
            initial_policy: None,
        },
        None,
        NOW,
    )
    .await?;
    archive(&s, &a, &list, true).await?;

    let rows: Vec<Raw> = raw(&s).await?.into_iter().skip(n).collect();
    let kinds: Vec<&str> = rows.iter().map(|r| r.event_type.as_str()).collect();
    // Only the person `theirs` (created by B) and the referenced identity were recorded.
    assert_eq!(
        kinds,
        ["person_created", "person_created", "person_linked"],
        "{rows:?}"
    );
    assert_eq!(rows[0].target_id, theirs);
    assert_eq!(n_theirs, n + 1);
    assert!(rows[1..].iter().all(|r| r.target_id == existing));
    Ok(())
}

// A2 ------------------------------------------------------------------------------------------
// Events commit or roll back with their change; nothing is recorded by a failed write.

#[tokio::test]
async fn a2_a_failed_write_records_nothing_and_its_retry_records_once() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let a = account(&s).await?;
    let p = create_person(&s, &a, "Morgan", Policy::default()).await?;
    let n = count(&s).await?;
    let receipts = || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM receipts")
            .fetch_one(&s.pool)
            .await
    };
    let before_receipts = receipts().await?;

    // A 20-command batch whose last command conflicts.
    let fields: Vec<String> = (0..19).map(|_| id()).collect();
    let mut batch: Vec<Command> = fields
        .iter()
        .map(|f| Command::CreateField {
            id: f.clone(),
            person_id: p.clone(),
            label: "Item".into(),
            value: "v".into(),
            initial_policy: Some(Policy::default()),
        })
        .collect();
    let (version, policy_version) = versions(&s, &p).await?;
    batch.push(Command::Edit {
        id: p.clone(),
        expected_version: version + 7,
        expected_policy_version: Some(policy_version),
        label: "Renamed".into(),
        value: String::new(),
    });
    let error = s.apply(&a, &id(), &batch).await.unwrap_err();
    assert_eq!(code(&error), Some(ErrorCode::Conflict));
    assert_eq!(count(&s).await?, n);
    assert_eq!(receipts().await?, before_receipts);
    // Positive control: the corrected batch records all twenty, in command order.
    batch[19] = Command::Edit {
        id: p.clone(),
        expected_version: version,
        expected_policy_version: Some(policy_version),
        label: "Renamed".into(),
        value: String::new(),
    };
    s.apply(&a, &id(), &batch).await?;
    let rows: Vec<Raw> = raw(&s).await?.into_iter().skip(n).collect();
    assert_eq!(rows.len(), 20);
    assert!(rows.iter().all(|r| r.revision == rows[0].revision));
    assert_eq!(
        rows.iter().map(|r| r.ordinal).collect::<Vec<_>>(),
        (0..20).collect::<Vec<_>>()
    );
    assert_eq!(rows[19].event_type, "person_renamed");
    assert_eq!(
        rows.iter()
            .take(19)
            .map(|r| r.target_id.clone())
            .collect::<Vec<_>>(),
        fields
    );

    // Both transaction seams, dropped without commit.
    let n = count(&s).await?;
    {
        let mut tx = s.begin_serial().await?;
        let (version, policy_version) = versions(&s, &p).await?;
        Store::apply_in(
            &mut tx,
            &a,
            &id(),
            &[Command::Edit {
                id: p.clone(),
                expected_version: version,
                expected_policy_version: Some(policy_version),
                label: "Dropped".into(),
                value: String::new(),
            }],
        )
        .await?;
        let inside: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM person_activity_events")
            .fetch_one(&mut *tx)
            .await?;
        assert_eq!(
            inside as usize,
            n + 1,
            "recorded inside the write's transaction"
        );
    }
    {
        let tx = s.begin_serial().await?;
        let (version, _) = versions(&s, &p).await?;
        let (_, tx) = Store::task_command_on(
            tx,
            &a,
            &id(),
            &TaskCommand::Archive {
                id: p.clone(),
                expected_version: version,
                archived: true,
            },
            None,
            NOW,
        )
        .await?;
        drop(tx);
    }
    assert_eq!(count(&s).await?, n);

    // Capacity overflow rejects the create and its event.
    sqlx::query("UPDATE sync_clock SET resource_count=10000 WHERE id=1")
        .execute(&s.pool)
        .await?;
    let error = create_person(&s, &a, "Overflow", Policy::default())
        .await
        .unwrap_err();
    assert_eq!(code(&error), Some(ErrorCode::SliceCapacity));
    assert_eq!(count(&s).await?, n);
    sqlx::query("UPDATE sync_clock SET resource_count=0 WHERE id=1")
        .execute(&s.pool)
        .await?;

    // A stale merge preview rejects the merge and its event.
    let other = create_person(&s, &a, "Other", Policy::default()).await?;
    let n = count(&s).await?;
    let error = s
        .people_command(
            &a,
            &id(),
            &PeopleCommand::Merge {
                source_id: other.clone(),
                target_id: p.clone(),
                preview_token: "stale".into(),
                name: "Morgan".into(),
            },
            NOW,
        )
        .await
        .unwrap_err();
    assert_eq!(code(&error), Some(ErrorCode::Conflict));
    assert_eq!(count(&s).await?, n);
    Ok(())
}

#[tokio::test]
async fn a2_a_statement_failing_just_before_recording_rolls_back_the_change() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (a, b) = (account(&s).await?, account(&s).await?);
    let p = create_person(&s, &a, "Morgan", Policy::default()).await?;
    link(&s, &a, &b, &p).await?;
    let n = count(&s).await?;
    let version = s.person_detail(&b, &p).await?.person.version;
    let operation = id();
    let command = PeopleCommand::Unlink {
        person_id: p.clone(),
        expected_version: version,
    };
    let reached = s.hooks().count("people_activity.before_record");
    s.hooks().inject_sql_times(
        "people_activity.before_record",
        "SELECT no_such_column FROM accounts",
        1,
    );
    s.people_command(&b, &operation, &command, NOW)
        .await
        .expect_err("the injected statement fails the write");
    assert_eq!(count(&s).await?, n);
    assert_eq!(
        s.person_detail(&b, &p).await?.linked_account_id.as_deref(),
        Some(b.as_str()),
        "the unlink rolled back with its event"
    );
    // The same operation retried afterwards succeeds and records once.
    s.people_command(&b, &operation, &command, NOW).await?;
    assert_eq!(added(&s, n).await?, [("person_unlinked".into(), p.clone())]);
    assert_eq!(
        s.hooks().count("people_activity.before_record"),
        reached + 2
    );
    Ok(())
}

// A3 ------------------------------------------------------------------------------------------

#[tokio::test]
async fn a3_a_replayed_operation_records_nothing_more_and_a_reused_id_records_nothing() -> Result<()>
{
    let (_dir, s) = fixture().await?;
    let (a, b) = (account(&s).await?, account(&s).await?);
    let p = create_person(&s, &a, "Morgan", Policy::default()).await?;

    // Command path.
    let operation = id();
    let commands = [Command::CreateField {
        id: id(),
        person_id: p.clone(),
        label: "Hobby".into(),
        value: "v".into(),
        initial_policy: Some(Policy::default()),
    }];
    let first = s.apply(&a, &operation, &commands).await?;
    let n = count(&s).await?;
    assert_eq!(s.apply(&a, &operation, &commands).await?, first);
    assert_eq!(count(&s).await?, n);
    let mut changed = commands.clone();
    if let Command::CreateField { label, .. } = &mut changed[0] {
        *label = "Other".into();
    }
    let error = s.apply(&a, &operation, &changed).await.unwrap_err();
    assert_eq!(code(&error), Some(ErrorCode::OperationConflict));
    assert_eq!(count(&s).await?, n);

    // Task command path.
    let (version, _) = versions(&s, &p).await?;
    let operation = id();
    let command = TaskCommand::Archive {
        id: p.clone(),
        expected_version: version,
        archived: true,
    };
    let first = s
        .task_command(&a, &operation, &command, None, NOW)
        .await?
        .revision;
    let n = count(&s).await?;
    assert_eq!(
        s.task_command(&a, &operation, &command, None, NOW)
            .await?
            .revision,
        first
    );
    assert_eq!(count(&s).await?, n);
    let other = TaskCommand::Archive {
        id: p.clone(),
        expected_version: version,
        archived: false,
    };
    let error = s
        .task_command(&a, &operation, &other, None, NOW)
        .await
        .unwrap_err();
    assert_eq!(code(&error), Some(ErrorCode::OperationConflict));
    assert_eq!(count(&s).await?, n);
    archive(&s, &a, &p, false).await?;

    // People command path.
    let c = account(&s).await?;
    let operation = id();
    let command = PeopleCommand::ReferenceAccount {
        account_id: c.clone(),
        person_id: id(),
    };
    let first = s.people_command(&b, &operation, &command, NOW).await?;
    let n = count(&s).await?;
    let again = s.people_command(&b, &operation, &command, NOW).await?;
    assert_eq!(
        (again.revision, again.person_id),
        (first.revision, first.person_id)
    );
    assert_eq!(count(&s).await?, n);
    let error = s
        .people_command(
            &b,
            &operation,
            &PeopleCommand::ReferenceAccount {
                account_id: c,
                person_id: id(),
            },
            NOW,
        )
        .await
        .unwrap_err();
    assert_eq!(code(&error), Some(ErrorCode::OperationConflict));
    assert_eq!(count(&s).await?, n);
    Ok(())
}

// A4 ------------------------------------------------------------------------------------------

#[tokio::test]
async fn a4_pages_are_newest_first_bounded_and_complete() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let a = account(&s).await?;
    let p = id();
    let fields: Vec<String> = (0..3).map(|_| id()).collect();
    let mut batch = vec![Command::CreatePerson {
        id: p.clone(),
        name: "Morgan".into(),
        initial_policy: Some(Policy::default()),
    }];
    batch.extend(fields.iter().map(|f| Command::CreateField {
        id: f.clone(),
        person_id: p.clone(),
        label: "Item".into(),
        value: "v".into(),
        initial_policy: Some(Policy::default()),
    }));
    s.apply(&a, &id(), &batch).await?;
    for i in 0..6 {
        edit(&s, &a, &fields[i % 3], &format!("Item {i}"), "v").await?;
    }
    // Reference model: commit order reversed.
    let model: Vec<String> = raw(&s).await?.into_iter().rev().map(|r| r.id).collect();
    assert_eq!(model.len(), 10);
    let page = s.person_activity(&a, &p, None, 100).await?;
    assert_eq!(ids_of(&page.items), model);
    assert_eq!(page.next_after, None);
    // One batch: the later command first.
    assert_eq!(
        types(&page.items[6..]),
        [
            T::FieldCreated,
            T::FieldCreated,
            T::FieldCreated,
            T::PersonCreated
        ]
    );
    assert_eq!(page.items[6].target.id, fields[2]);
    for limit in [1, 2, 3, 9, 10, 11, 50] {
        assert_eq!(
            ids_of(&walk(&s, &a, &p, limit).await?),
            model,
            "limit {limit}"
        );
    }
    // Exact-limit boundary: a full page continues only when another visible item exists.
    let full = s.person_activity(&a, &p, None, 10).await?;
    assert_eq!((full.items.len(), full.next_after), (10, None));
    let nine = s.person_activity(&a, &p, None, 9).await?;
    assert_eq!(nine.next_after.as_deref(), Some(model[8].as_str()));
    let rest = s
        .person_activity(&a, &p, nine.next_after.as_deref(), 9)
        .await?;
    assert_eq!(
        (ids_of(&rest.items), rest.next_after),
        (vec![model[9].clone()], None)
    );
    // The default page size of 50 is the route's; the core bounds are 1..=100.
    for limit in [0, 101] {
        let error = s.person_activity(&a, &p, None, limit).await.unwrap_err();
        assert_eq!(code(&error), Some(ErrorCode::InvalidValue), "limit {limit}");
    }
    let error = s
        .person_activity(&a, &p, Some("not-a-uuid"), 10)
        .await
        .unwrap_err();
    assert_eq!(code(&error), Some(ErrorCode::InvalidValue));

    // A missing event and another person's event are the same error.
    let q = create_person(&s, &a, "Other", Policy::default()).await?;
    let foreign = s.person_activity(&a, &q, None, 1).await?.items[0]
        .id
        .clone();
    let foreign_error = s
        .person_activity(&a, &p, Some(&foreign), 10)
        .await
        .unwrap_err();
    let missing_error = s
        .person_activity(&a, &p, Some(&id()), 10)
        .await
        .unwrap_err();
    assert_eq!(code(&foreign_error), Some(ErrorCode::InvalidValue));
    assert_eq!(foreign_error.to_string(), missing_error.to_string());

    // Writes between page reads never appear after the cursor, and nothing repeats.
    let first = s.person_activity(&a, &p, None, 4).await?;
    edit(&s, &a, &fields[0], "Newest", "v").await?;
    archive(&s, &a, &fields[1], true).await?;
    let mut seen = ids_of(&first.items);
    let mut after = first.next_after;
    while let Some(cursor) = after {
        let page = s.person_activity(&a, &p, Some(&cursor), 4).await?;
        seen.extend(ids_of(&page.items));
        after = page.next_after;
    }
    assert_eq!(
        seen, model,
        "the walk that began before the writes is exactly the old list"
    );
    let fresh = walk(&s, &a, &p, 4).await?;
    assert_eq!(fresh.len(), 12);
    assert_eq!(types(&fresh[..2]), [T::FieldArchived, T::FieldEdited]);
    assert_eq!(ids_of(&fresh[2..]), model);
    let unique: BTreeSet<String> = ids_of(&fresh).into_iter().collect();
    assert_eq!(unique.len(), 12, "stable, unique identities");

    // Re-reading gives the identical list (cross-device reads are covered over HTTP).
    assert_eq!(walk(&s, &a, &p, 5).await?, fresh);
    Ok(())
}

// A6 ------------------------------------------------------------------------------------------

#[tokio::test]
async fn a6_actors_are_named_only_when_the_reader_already_knows_them() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (alice, bob, carol, dave, erin) = (
        account(&s).await?,
        account(&s).await?,
        account(&s).await?,
        account(&s).await?,
        account(&s).await?,
    );
    let p = create_person(
        &s,
        &alice,
        "Morgan",
        grants(&[(&bob, true), (&carol, false), (&erin, true)]),
    )
    .await?;
    let f = create_field(
        &s,
        &alice,
        &p,
        "Hobby",
        grants(&[(&bob, true), (&carol, false), (&erin, true)]),
    )
    .await?;
    edit(&s, &bob, &f, "Hobby", "Bob's edit").await?;
    edit(&s, &erin, &f, "Hobby", "Erin's edit").await?;

    let alice_name: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(&alice)
        .fetch_one(&s.pool)
        .await?;
    let for_carol = walk(&s, &carol, &p, 50).await?;
    assert_eq!(
        types(&for_carol),
        [
            T::FieldEdited,
            T::FieldEdited,
            T::FieldCreated,
            T::PersonCreated
        ]
    );
    // Bob and Erin are neither the reader, the owner nor linked: indistinguishable.
    assert_eq!(for_carol[0].actor, ActivityAccount::SomeoneElse);
    assert_eq!(for_carol[1].actor, ActivityAccount::SomeoneElse);
    assert_eq!(
        serde_json::to_value(&for_carol[0].actor)?,
        serde_json::to_value(&for_carol[1].actor)?
    );
    assert_eq!(
        serde_json::to_value(&for_carol[0].actor)?,
        serde_json::json!({"kind":"someone_else"})
    );
    let owner = ActivityAccount::Account {
        account_id: alice.clone(),
        username: alice_name.clone(),
    };
    assert_eq!(for_carol[2].actor, owner);
    // The owner sees its grantees as someone else too (D-ACT names no grantee).
    let for_alice = walk(&s, &alice, &p, 50).await?;
    assert_eq!(for_alice[0].actor, ActivityAccount::SomeoneElse);
    assert_eq!(for_alice[3].actor, ActivityAccount::You);
    let for_bob = walk(&s, &bob, &p, 50).await?;
    assert_eq!(for_bob[1].actor, ActivityAccount::You);
    assert_eq!(for_bob[0].actor, ActivityAccount::SomeoneElse);

    // A link moves ownership to Dave: Alice becomes someone else for Carol, Dave is named.
    link(&s, &alice, &dave, &p).await?;
    let dave_name: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(&dave)
        .fetch_one(&s.pool)
        .await?;
    let dave_named = ActivityAccount::Account {
        account_id: dave.clone(),
        username: dave_name,
    };
    let for_carol = walk(&s, &carol, &p, 50).await?;
    assert_eq!(for_carol[0].event_type, T::PersonLinked);
    assert_eq!(for_carol[0].actor, dave_named);
    assert_eq!(for_carol[0].account, Some(dave_named.clone()));
    assert_eq!(
        for_carol.last().unwrap().actor,
        ActivityAccount::SomeoneElse,
        "former owner"
    );
    let for_dave = walk(&s, &dave, &p, 50).await?;
    assert_eq!(for_dave[0].account, Some(ActivityAccount::You));
    // Unlink: Dave still owns the person, so it stays named; nothing else changes.
    unlink(&s, &dave, &p).await?;
    let for_carol = walk(&s, &carol, &p, 50).await?;
    assert_eq!(for_carol[0].event_type, T::PersonUnlinked);
    assert_eq!(for_carol[0].account, Some(dave_named.clone()));
    assert_eq!(for_carol[0].actor, dave_named);
    Ok(())
}

// A7 ------------------------------------------------------------------------------------------

#[tokio::test]
async fn a7_a_person_the_reader_cannot_see_is_indistinguishable_from_a_missing_one() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (a, b, outsider) = (account(&s).await?, account(&s).await?, account(&s).await?);
    let home = household(&s, &a, &b).await?;
    let p = create_person(
        &s,
        &a,
        "Morgan",
        Policy {
            grants: vec![PrincipalGrant::Household {
                id: home.clone(),
                edit: false,
            }],
            exclude_accounts: vec![],
        },
    )
    .await?;
    let alias = create_person(&s, &a, "Alias", Policy::default()).await?;
    merge(&s, &a, &alias, &p, "Morgan").await?;
    assert!(
        !walk(&s, &b, &p, 50).await?.is_empty(),
        "the household member reads it"
    );
    for subject in [&p, &alias, &id()] {
        let error = s
            .person_activity(&outsider, subject, None, 50)
            .await
            .unwrap_err();
        let detail = s.person_detail(&outsider, subject).await.unwrap_err();
        assert_eq!(code(&error), Some(ErrorCode::NotFound));
        assert_eq!(error.to_string(), detail.to_string());
    }
    // A cursor from a timeline the reader can see does not open one it cannot.
    let cursor = s.person_activity(&a, &p, None, 1).await?.items[0]
        .id
        .clone();
    let error = s
        .person_activity(&outsider, &p, Some(&cursor), 50)
        .await
        .unwrap_err();
    assert_eq!(code(&error), Some(ErrorCode::NotFound));
    Ok(())
}

// A8 ------------------------------------------------------------------------------------------

#[tokio::test]
async fn a8_hidden_events_change_no_page_cursor_or_continuation() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (alice, carol) = (account(&s).await?, account(&s).await?);
    let home = household(&s, &alice, &carol).await?;
    let p = create_person(&s, &alice, "Morgan", grants(&[(&carol, false)])).await?;
    // Hidden events older than every visible one but the first, and newer than all of them.
    let private = create_field(&s, &alice, &p, "Medical note", Policy::default()).await?;
    for i in 0..5 {
        edit(&s, &alice, &private, "Medical note", &format!("secret {i}")).await?;
    }
    let shared = create_field(&s, &alice, &p, "Hobby", grants(&[(&carol, false)])).await?;
    let excluded = create_field(
        &s,
        &alice,
        &p,
        "Gift ideas",
        Policy {
            grants: vec![PrincipalGrant::Household {
                id: home.clone(),
                edit: false,
            }],
            exclude_accounts: vec![carol.clone()],
        },
    )
    .await?;
    for i in 0..300 {
        edit(&s, &alice, &private, "Medical note", &format!("later {i}")).await?;
    }
    archive(&s, &alice, &excluded, true).await?;

    let visible: BTreeSet<&str> = [p.as_str(), shared.as_str()].into();
    let model: Vec<String> = raw(&s)
        .await?
        .into_iter()
        .rev()
        .filter(|r| visible.contains(r.target_id.as_str()))
        .map(|r| r.id)
        .collect();
    assert_eq!(model.len(), 2);
    for limit in [1, 2, 3, 50, 100] {
        let walked = walk(&s, &carol, &p, limit).await?;
        assert_eq!(ids_of(&walked), model, "limit {limit}");
    }
    // Exactly the visible count: no continuation, although 300+ hidden rows exist.
    let page = s.person_activity(&carol, &p, None, 2).await?;
    assert_eq!((page.items.len(), page.next_after), (2, None));
    let first = s.person_activity(&carol, &p, None, 1).await?;
    assert_eq!(first.next_after.as_deref(), Some(model[0].as_str()));
    // The owner sees everything, in the same order.
    let all = walk(&s, &alice, &p, 100).await?;
    assert_eq!(all.len(), raw(&s).await?.len());
    // Nothing about the hidden fields reaches Carol.
    let text = serde_json::to_string(&walk(&s, &carol, &p, 100).await?)?;
    for hidden in [
        &private,
        &excluded,
        &"Medical note".to_string(),
        &"Gift ideas".to_string(),
    ] {
        assert!(!text.contains(hidden.as_str()), "{hidden} leaked");
    }
    Ok(())
}

// A9 ------------------------------------------------------------------------------------------

#[tokio::test]
async fn a9_history_follows_current_access_on_every_read() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (alice, bob, carol) = (account(&s).await?, account(&s).await?, account(&s).await?);
    let home = household(&s, &alice, &bob).await?;
    let in_home = Policy {
        grants: vec![
            PrincipalGrant::Household {
                id: home.clone(),
                edit: false,
            },
            PrincipalGrant::Account {
                id: carol.clone(),
                edit: false,
            },
        ],
        exclude_accounts: vec![],
    };
    let p = create_person(&s, &alice, "Morgan", in_home.clone()).await?;
    let f = create_field(&s, &alice, &p, "Hobby", in_home.clone()).await?;
    edit(&s, &alice, &f, "Hobby", "Rowing").await?;
    assert_eq!(walk(&s, &carol, &p, 50).await?.len(), 3);
    assert_eq!(walk(&s, &bob, &p, 50).await?.len(), 3);

    // Revoking Carol's field grant hides the field's events at once; regaining shows them again
    // (current permissions, as documented: earlier metadata reappears with access).
    replace_policy(
        &s,
        &alice,
        &f,
        Policy {
            grants: vec![PrincipalGrant::Household {
                id: home.clone(),
                edit: false,
            }],
            exclude_accounts: vec![],
        },
    )
    .await?;
    assert_eq!(types(&walk(&s, &carol, &p, 50).await?), [T::PersonCreated]);
    replace_policy(&s, &alice, &f, in_home.clone()).await?;
    assert_eq!(walk(&s, &carol, &p, 50).await?.len(), 3);
    // An exclusion hides them too.
    replace_policy(
        &s,
        &alice,
        &f,
        Policy {
            exclude_accounts: vec![carol.clone()],
            ..in_home.clone()
        },
    )
    .await?;
    assert_eq!(types(&walk(&s, &carol, &p, 50).await?), [T::PersonCreated]);
    replace_policy(&s, &alice, &f, in_home.clone()).await?;

    // Leaving the household removes Bob's access to the person: the timeline is not found.
    let expected_version = household_version(&s, &bob, &home).await?;
    manage(
        &s,
        &bob,
        M::RemoveHouseholdMember {
            household_id: home.clone(),
            account_id: bob.clone(),
            expected_version,
        },
    )
    .await?;
    let error = s.person_activity(&bob, &p, None, 50).await.unwrap_err();
    assert_eq!(code(&error), Some(ErrorCode::NotFound));
    // Revoking Carol's person grant does the same, even with a cursor she was given.
    let cursor = s.person_activity(&carol, &p, None, 1).await?.items[0]
        .id
        .clone();
    replace_policy(
        &s,
        &alice,
        &p,
        Policy {
            grants: vec![PrincipalGrant::Household {
                id: home.clone(),
                edit: false,
            }],
            exclude_accounts: vec![],
        },
    )
    .await?;
    for after in [None, Some(cursor.as_str())] {
        let error = s.person_activity(&carol, &p, after, 50).await.unwrap_err();
        assert_eq!(code(&error), Some(ErrorCode::NotFound));
    }
    Ok(())
}

// A10 -----------------------------------------------------------------------------------------

#[tokio::test]
async fn a10_merges_combine_timelines_without_widening_the_source_history() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (alice, carol) = (account(&s).await?, account(&s).await?);
    let source = create_person(&s, &alice, "Source", Policy::default()).await?;
    let source_field = create_field(&s, &alice, &source, "Old note", Policy::default()).await?;
    let target = create_person(&s, &alice, "Target", grants(&[(&carol, false)])).await?;
    let target_field =
        create_field(&s, &alice, &target, "Hobby", grants(&[(&carol, false)])).await?;
    merge(&s, &alice, &source, &target, "Merged").await?;

    let for_alice = walk(&s, &alice, &target, 50).await?;
    assert_eq!(
        types(&for_alice),
        [
            T::PersonMerged,
            T::FieldCreated,
            T::PersonCreated,
            T::FieldCreated,
            T::PersonCreated
        ]
    );
    let merged = &for_alice[0];
    assert_eq!(merged.target.id, target);
    assert_eq!(
        merged.merged_from.as_ref().map(|m| m.id.as_str()),
        Some(source.as_str())
    );
    // The source's own event: archived, merged into the canonical person.
    let source_created = &for_alice[4];
    assert_eq!(source_created.target.id, source);
    assert_eq!(source_created.target.state, TargetState::Merged);
    assert_eq!(
        source_created.target.canonical_person_id.as_deref(),
        Some(target.as_str())
    );
    assert!(source_created.target.archived);
    // Its field now belongs to the target and is current there.
    assert_eq!(for_alice[3].target.id, source_field);
    assert_eq!(for_alice[3].target.state, TargetState::Current);
    // The source ID resolves to the canonical timeline.
    let via_alias = s.person_activity(&alice, &source, None, 50).await?;
    assert_eq!(via_alias.person_id, target);
    assert_eq!(via_alias.items, for_alice);

    // Carol could not see the source: she gets the merge without its source, and none of the
    // source's own history.
    let for_carol = walk(&s, &carol, &target, 50).await?;
    assert_eq!(
        types(&for_carol),
        [T::PersonMerged, T::FieldCreated, T::PersonCreated]
    );
    assert_eq!(for_carol[0].merged_from, None);
    assert_eq!(for_carol[1].target.id, target_field);
    let text = serde_json::to_string(&for_carol)?;
    for hidden in [
        &source,
        &source_field,
        &"Source".to_string(),
        &"Old note".to_string(),
    ] {
        assert!(!text.contains(hidden.as_str()), "{hidden} leaked");
    }

    // Chained: the merged person merged again keeps every alias's history.
    let last = create_person(&s, &alice, "Last", Policy::default()).await?;
    merge(&s, &alice, &target, &last, "Last").await?;
    let chained = walk(&s, &alice, &source, 50).await?;
    assert_eq!(
        s.person_activity(&alice, &source, None, 1).await?.person_id,
        last
    );
    assert_eq!(chained.len(), for_alice.len() + 2);
    assert_eq!(
        &chained[2..],
        &for_alice[..]
            .iter()
            .cloned()
            .map(|mut e| {
                if e.target.kind == atlas_core::people::TargetKind::Person {
                    e.target.state = TargetState::Merged;
                    e.target.canonical_person_id = Some(last.clone());
                    e.target.archived = true;
                }
                e
            })
            .collect::<Vec<_>>()[..]
    );
    Ok(())
}

#[tokio::test]
async fn a10_accepting_a_link_into_an_existing_person_is_a_merge() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (alice, bob, dave) = (account(&s).await?, account(&s).await?, account(&s).await?);
    // Bob references Dave: Dave's canonical person, owned by and linked to Dave, added by Bob.
    let canonical = reference(&s, &bob, &dave).await?;
    let p = create_person(&s, &alice, "Dave from work", Policy::default()).await?;
    let request = request_link(&s, &alice, &dave, &p).await?;
    respond(&s, &dave, &request, true).await?;
    let merges: Vec<Raw> = raw(&s)
        .await?
        .into_iter()
        .filter(|r| r.event_type == "person_merged")
        .collect();
    assert_eq!(merges.len(), 1);
    assert_eq!(
        (
            merges[0].target_id.as_str(),
            merges[0].merged_from.as_deref(),
            merges[0].actor_id.as_str()
        ),
        (canonical.as_str(), Some(p.as_str()), dave.as_str())
    );
    assert!(
        raw(&s)
            .await?
            .iter()
            .filter(|r| r.event_type == "person_linked")
            .count()
            == 1
    );

    // Dave never saw Alice's person: the merge without its source, and none of its history.
    let for_dave = walk(&s, &dave, &canonical, 50).await?;
    assert_eq!(
        types(&for_dave),
        [T::PersonMerged, T::PersonLinked, T::PersonCreated]
    );
    assert_eq!(for_dave[0].actor, ActivityAccount::You);
    assert_eq!(for_dave[0].merged_from, None);
    // Bob created the person, but Dave owns it and is linked: Bob is someone else.
    assert_eq!(for_dave[1].actor, ActivityAccount::SomeoneElse);
    assert_eq!(for_dave[1].account, Some(ActivityAccount::You));
    assert_eq!(for_dave[2].actor, ActivityAccount::SomeoneElse);
    // Alice saw her person, so the merged identity's audience includes her and she sees both.
    let for_alice = walk(&s, &alice, &canonical, 50).await?;
    assert_eq!(
        types(&for_alice),
        [
            T::PersonMerged,
            T::PersonCreated,
            T::PersonLinked,
            T::PersonCreated
        ]
    );
    assert_eq!(
        for_alice[0].merged_from.as_ref().map(|m| m.id.as_str()),
        Some(p.as_str())
    );
    assert_eq!(for_alice[1].target.id, p);
    assert_eq!(for_alice[1].target.state, TargetState::Merged);
    assert_eq!(for_alice[1].actor, ActivityAccount::You);
    assert!(
        matches!(&for_alice[0].actor, ActivityAccount::Account { account_id, .. } if *account_id == dave)
    );
    Ok(())
}

// A11 -----------------------------------------------------------------------------------------

#[tokio::test]
async fn a11_labels_are_current_and_no_old_content_is_returned() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let a = account(&s).await?;
    let p = create_person(&s, &a, "First name", Policy::default()).await?;
    let f = create_field(&s, &a, &p, "First label", Policy::default()).await?;
    edit(&s, &a, &f, "Second label", "Sensitive value").await?;
    edit(&s, &a, &p, "Second name", "").await?;
    let typed = put_field(
        &s,
        &a,
        &p,
        "Flag",
        FieldValue::Boolean { value: true },
        Policy::default(),
    )
    .await?;
    archive(&s, &a, &typed, true).await?;
    let items = walk(&s, &a, &p, 50).await?;
    for e in &items {
        let expected = if e.target.id == p {
            "Second name"
        } else if e.target.id == f {
            "Second label"
        } else {
            "Flag"
        };
        assert_eq!(e.target.label, expected, "{e:?}");
    }
    let field_events: Vec<_> = items.iter().filter(|e| e.target.id == f).collect();
    assert_eq!(field_events.len(), 2);
    assert!(
        field_events
            .iter()
            .all(|e| e.target.field_kind.as_deref() == Some("text"))
    );
    assert_eq!(
        field_events[0].changed.as_deref(),
        Some(&[ChangedPart::Label, ChangedPart::Value][..])
    );
    assert_eq!(field_events[1].changed, None);
    let flag: Vec<_> = items.iter().filter(|e| e.target.id == typed).collect();
    assert!(
        flag.iter()
            .all(|e| e.target.archived && e.target.field_kind.as_deref() == Some("boolean"))
    );
    let text = serde_json::to_string(&items)?;
    for old in ["First name", "First label", "Initial", "Sensitive value"] {
        assert!(!text.contains(old), "{old} was returned");
    }
    // Summaries are the fixed phrases, never user content.
    assert!(items.iter().all(|e| e.summary == e.event_type.summary()));
    assert!(text.contains("\"summary\":\"Edited a field\""));
    Ok(())
}

// A13 -----------------------------------------------------------------------------------------

const BETWEEN: &str = "people_activity.between_person_and_events";

#[tokio::test]
async fn a13_one_page_is_read_from_one_snapshot() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (alice, carol) = (account(&s).await?, account(&s).await?);
    let p = create_person(&s, &alice, "Morgan", grants(&[(&carol, false)])).await?;
    let f = create_field(&s, &alice, &p, "Hobby", grants(&[(&carol, false)])).await?;
    let before = walk(&s, &carol, &p, 50).await?;
    assert_eq!(before.len(), 2);

    let mut gate = s.hooks().arm(BETWEEN);
    let read = {
        let (s, carol, p) = (s.clone(), carol.clone(), p.clone());
        tokio::spawn(async move { s.person_activity(&carol, &p, None, 50).await })
    };
    gate.reached().await;
    // While the read is paused after its person gate, the owner revokes the field and records a
    // new event, and both commit.
    tokio::time::timeout(Duration::from_secs(20), async {
        replace_policy(&s, &alice, &f, Policy::default()).await?;
        edit(&s, &alice, &p, "Morgan Lee", "").await
    })
    .await
    .expect("the writer was not blocked by the paused reader")?;
    gate.release();
    let page: PersonActivityPage = read.await??;
    assert_eq!(
        page.items, before,
        "every member comes from the snapshot the read began with"
    );
    let after = walk(&s, &carol, &p, 50).await?;
    assert_eq!(
        types(&after),
        [T::PersonRenamed, T::PersonCreated],
        "the next read applies the revoke"
    );
    if postgres() {
        let seen = s.hooks().isolation_seen();
        assert_eq!(seen.len() as u32, s.hooks().count(BETWEEN));
        assert!(
            seen.iter().all(|level| level == "repeatable read"),
            "{seen:?}"
        );
    }
    Ok(())
}

// A14 -----------------------------------------------------------------------------------------
// Tripwire over a mixed history: nothing hidden appears anywhere in any reader's response.

#[tokio::test]
async fn a14_no_response_carries_a_hidden_identifier_label_account_or_revision() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (alice, bob, carol, dave) = (
        account(&s).await?,
        account(&s).await?,
        account(&s).await?,
        account(&s).await?,
    );
    let p = create_person(
        &s,
        &alice,
        "Visible person",
        grants(&[(&bob, true), (&carol, false)]),
    )
    .await?;
    let hidden_person =
        create_person(&s, &alice, "Hidden person", grants(&[(&bob, false)])).await?;
    let hidden_field =
        create_field(&s, &alice, &p, "Hidden field", grants(&[(&bob, true)])).await?;
    let shared = create_field(
        &s,
        &alice,
        &p,
        "Shared field",
        grants(&[(&bob, true), (&carol, false)]),
    )
    .await?;
    edit(&s, &bob, &hidden_field, "Hidden field", "bob was here").await?;
    edit(&s, &bob, &shared, "Shared field", "bob again").await?;
    merge(&s, &alice, &hidden_person, &p, "Visible person").await?;
    link(&s, &alice, &dave, &p).await?;
    unlink(&s, &dave, &p).await?;
    let usernames: Vec<String> = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(&bob)
        .fetch_all(&s.pool)
        .await?;
    let revisions: Vec<i64> = raw(&s).await?.into_iter().map(|r| r.revision).collect();
    for limit in [1, 3, 50] {
        let items = walk(&s, &carol, &p, limit).await?;
        assert!(!items.is_empty());
        let text = serde_json::to_string(&items)?;
        for hidden in [&hidden_person, &hidden_field, &bob, &usernames[0]] {
            assert!(
                !text.contains(hidden.as_str()),
                "{hidden} leaked at limit {limit}"
            );
        }
        for hidden in ["Hidden person", "Hidden field", "bob was here", "bob again"] {
            assert!(!text.contains(hidden), "{hidden} leaked at limit {limit}");
        }
        let json = serde_json::to_value(&items)?;
        for item in json.as_array().unwrap() {
            assert!(item.get("revision").is_none());
            assert!(item.get("ordinal").is_none());
        }
        // No revision value appears as any number in a response (occurred_at is NOW-based).
        let page = serde_json::to_value(s.person_activity(&carol, &p, None, limit).await?)?;
        let mut numbers = Vec::new();
        collect_numbers(&page, &mut numbers);
        assert!(revisions.iter().all(|r| *r < 1_000_000));
        assert!(
            numbers.iter().all(|n| *n > 1_000_000),
            "only timestamps: {numbers:?}"
        );
    }
    Ok(())
}

fn collect_numbers(value: &serde_json::Value, into: &mut Vec<i64>) {
    match value {
        serde_json::Value::Number(n) => into.extend(n.as_i64()),
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect_numbers(v, into)),
        serde_json::Value::Object(map) => map.values().for_each(|v| collect_numbers(v, into)),
        _ => {}
    }
}
