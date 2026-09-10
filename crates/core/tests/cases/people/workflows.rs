use anyhow::Result;
use atlas_core::{
    Command, Store,
    households::ManagementCommand,
    people::PeopleCommand,
    policy::{Policy, PrincipalGrant},
    tasks::*,
};
use uuid::Uuid;
fn id() -> String {
    Uuid::new_v4().to_string()
}
const NOW: i64 = 1788868800;
use crate::support::database::setup;
async fn account(s: &Store) -> Result<String> {
    let a = id();
    s.add_account(&a, &format!("u{a}"), "test").await?;
    Ok(a)
}
fn share(a: &str) -> Policy {
    Policy {
        grants: vec![PrincipalGrant::Account {
            id: a.into(),
            edit: true,
        }],
        exclude_accounts: vec![],
    }
}
async fn person(s: &Store, a: &str, name: &str, p: Policy) -> Result<String> {
    let p_id = id();
    s.apply(
        a,
        &id(),
        &[Command::CreatePerson {
            id: p_id.clone(),
            name: name.into(),
            initial_policy: Some(p),
        }],
    )
    .await?;
    Ok(p_id)
}
async fn field(s: &Store, a: &str, parent: &str, label: &str) -> Result<String> {
    let f = id();
    s.apply(
        a,
        &id(),
        &[Command::CreateField {
            id: f.clone(),
            person_id: parent.into(),
            label: label.into(),
            value: "private content".into(),
            initial_policy: Some(Policy::default()),
        }],
    )
    .await?;
    Ok(f)
}
async fn visible(s: &Store, a: &str, parent: &str) -> Result<Vec<String>> {
    Ok(s.resources(a, "field", Some(parent), false, None, 200)
        .await?
        .into_iter()
        .map(|p| p.id)
        .collect())
}
async fn policy(s: &Store, a: &str, p: &str, new: Policy) -> Result<()> {
    let version = s.resource_policy(a, p).await?.version;
    s.management(
        a,
        &id(),
        &[ManagementCommand::ReplacePolicy {
            id: p.into(),
            expected_version: version,
            policy: new,
        }],
        NOW,
    )
    .await?;
    Ok(())
}
async fn command(s: &Store, a: &str, c: PeopleCommand) -> Result<String> {
    Ok(s.people_command(a, &id(), &c, NOW).await?.person_id)
}

async fn merge_scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let source = person(s, &a, "Morgan", share(&b)).await?;
    let target = person(s, &a, "MORGAN", Policy::default()).await?;
    let public_to_a = field(s, &a, &source, "Job").await?;
    let private = field(s, &b, &source, "Gift surprise").await?;
    let snapshot = s.sync(&a, "merge-device", None, 200, NOW).await?;
    let preview = s.merge_preview(&a, &source, &target).await?;
    assert!(!preview.requires_approval);
    assert!(preview.fields.iter().all(|f| f.id != private));
    let another = field(s, &b, &source, "Hidden contribution after preview").await?;
    assert_eq!(
        s.merge_preview(&a, &source, &target).await?.token,
        preview.token
    );
    let op = id();
    let merge = PeopleCommand::Merge {
        source_id: source.clone(),
        target_id: target.clone(),
        preview_token: preview.token,
        name: "Morgan Jones".into(),
    };
    let result = s.people_command(&a, &op, &merge, NOW).await?;
    assert_eq!(
        s.people_command(&a, &op, &merge, NOW).await?.revision,
        result.revision
    );
    let delta = s
        .sync(&a, "merge-device", Some(&snapshot.next_cursor), 200, NOW)
        .await?;
    let encoded = serde_json::to_string(&delta)?;
    assert!(
        !encoded.contains(&private)
            && !encoded.contains(&another)
            && !encoded.contains("Gift surprise")
    );
    assert_eq!(s.person_detail(&a, &source).await?.person.id, target);
    assert!(visible(s, &a, &target).await?.contains(&public_to_a));
    assert!(!visible(s, &a, &target).await?.contains(&private));
    let b_fields = visible(s, &b, &target).await?;
    assert!(b_fields.contains(&private) && b_fields.contains(&another));
    assert!(!b_fields.contains(&public_to_a));
    // Both legacy and typed offline commands retain the old parent UUID.
    let offline = field(s, &a, &source, "Offline draft").await?;
    let typed = id();
    s.task_command(
        &a,
        &id(),
        &TaskCommand::PutField {
            id: typed.clone(),
            parent_id: source.clone(),
            expected_version: None,
            label: "Birthday".into(),
            value: FieldValue::Date {
                year: None,
                month: 9,
                day: 10,
            },
            initial_policy: Some(Policy::default()),
        },
        None,
        NOW,
    )
    .await?;
    let fields = visible(s, &a, &target).await?;
    assert!(fields.contains(&offline) && fields.contains(&typed));
    // Reserved aliases cannot be resurrected as a second person.
    assert!(
        s.apply(
            &a,
            &id(),
            &[Command::CreatePerson {
                id: source.clone(),
                name: "Duplicate".into(),
                initial_policy: None
            }]
        )
        .await
        .is_err()
    );
    let third = person(s, &a, "Third duplicate", Policy::default()).await?;
    let preview = s.merge_preview(&a, &target, &third).await?;
    command(
        s,
        &a,
        PeopleCommand::Merge {
            source_id: target,
            target_id: third.clone(),
            preview_token: preview.token,
            name: "Morgan".into(),
        },
    )
    .await?;
    assert_eq!(s.person_detail(&a, &source).await?.person.id, third);
    Ok(())
}
async fn owner_gate_scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let source = person(s, &a, "Source", share(&b)).await?;
    let target = person(s, &a, "Destination", share(&b)).await?;
    let hidden = field(s, &b, &source, "Formerly visible to owner").await?;
    policy(s, &a, &source, Policy::default()).await?;
    assert!(visible(s, &b, &source).await?.is_empty());
    let preview = s.merge_preview(&a, &source, &target).await?;
    command(
        s,
        &a,
        PeopleCommand::Merge {
            source_id: source,
            target_id: target.clone(),
            preview_token: preview.token,
            name: "Merged".into(),
        },
    )
    .await?;
    assert!(!visible(s, &b, &target).await?.contains(&hidden));
    // Ownership still permits an explicit policy replacement once the new parent
    // is visible, but the merge itself did not silently restore content access.
    policy(s, &b, &hidden, Policy::default()).await?;
    assert!(visible(s, &b, &target).await?.contains(&hidden));
    Ok(())
}
async fn linking_scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let c = account(s).await?;
    let source = person(s, &a, "Morgan", Policy::default()).await?;
    let hidden = field(s, &a, &source, "Christmas present").await?;
    let request = id();
    command(
        s,
        &a,
        PeopleCommand::RequestLink {
            id: request.clone(),
            person_id: source.clone(),
            account_id: b.clone(),
            expected_version: 1,
        },
    )
    .await?;
    assert_eq!(s.people_requests(&b, NOW).await?.len(), 1);
    assert!(s.people_requests(&c, NOW).await?.is_empty());
    assert!(
        command(
            s,
            &c,
            PeopleCommand::RespondRequest {
                id: request.clone(),
                accept: true
            }
        )
        .await
        .is_err()
    );
    command(
        s,
        &b,
        PeopleCommand::RespondRequest {
            id: request,
            accept: true,
        },
    )
    .await?;
    assert_eq!(
        s.person_detail(&b, &source)
            .await?
            .linked_account_id
            .as_deref(),
        Some(b.as_str())
    );
    assert!(!visible(s, &b, &source).await?.contains(&hidden));
    let p = s.person_detail(&a, &source).await?.person;
    assert!(!p.can_edit);
    assert!(
        s.apply(
            &a,
            &id(),
            &[Command::Edit {
                id: source.clone(),
                expected_version: p.version,
                label: "Wrong name".into(),
                value: "".into()
            }]
        )
        .await
        .is_err()
    );
    command(
        s,
        &b,
        PeopleCommand::RenameProfile {
            person_id: source.clone(),
            expected_version: p.version,
            name: "Morgan's name".into(),
        },
    )
    .await?;
    let alias = id();
    assert_eq!(
        command(
            s,
            &c,
            PeopleCommand::ReferenceAccount {
                account_id: b.clone(),
                person_id: alias.clone()
            }
        )
        .await?,
        source
    );
    assert_eq!(s.person_detail(&c, &alias).await?.person.id, source);
    assert!(!visible(s, &c, &source).await?.contains(&hidden));
    // Default contributions exclude the subject; explicit sharing can include them.
    let default_field = id();
    s.apply(
        &a,
        &id(),
        &[Command::CreateField {
            id: default_field.clone(),
            person_id: source.clone(),
            label: "Another surprise".into(),
            value: "Secret".into(),
            initial_policy: None,
        }],
    )
    .await?;
    assert!(
        s.resource_policy(&a, &default_field)
            .await?
            .policy
            .exclude_accounts
            .contains(&b)
    );
    policy(s, &a, &default_field, share(&b)).await?;
    assert!(visible(s, &b, &source).await?.contains(&default_field));
    let duplicate = person(s, &a, "Unlinked duplicate", Policy::default()).await?;
    let private = field(s, &a, &duplicate, "Private date").await?;
    let request = id();
    command(
        s,
        &a,
        PeopleCommand::RequestLink {
            id: request.clone(),
            person_id: duplicate.clone(),
            account_id: b.clone(),
            expected_version: 1,
        },
    )
    .await?;
    assert_eq!(
        command(
            s,
            &b,
            PeopleCommand::RespondRequest {
                id: request,
                accept: true
            }
        )
        .await?,
        source
    );
    assert_eq!(s.person_detail(&a, &duplicate).await?.person.id, source);
    assert!(!visible(s, &b, &source).await?.contains(&private));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM person_accounts WHERE account_id=$1")
            .bind(&b)
            .fetch_one(&s.pool)
            .await?,
        1
    );
    // Explicit identity exclusions cannot be bypassed by referencing the account again.
    policy(
        s,
        &b,
        &source,
        Policy {
            grants: vec![],
            exclude_accounts: vec![c.clone()],
        },
    )
    .await?;
    assert!(
        command(
            s,
            &c,
            PeopleCommand::ReferenceAccount {
                account_id: b,
                person_id: id()
            }
        )
        .await
        .is_err()
    );
    Ok(())
}
async fn approved_merge_scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let source = person(s, &a, "Morgan", share(&b)).await?;
    let target = person(s, &b, "Morgan", share(&a)).await?;
    let hidden = field(s, &b, &source, "Private note").await?;
    let preview = s.merge_preview(&a, &source, &target).await?;
    assert!(preview.requires_approval);
    assert!(
        command(
            s,
            &a,
            PeopleCommand::Merge {
                source_id: source.clone(),
                target_id: target.clone(),
                preview_token: preview.token.clone(),
                name: "Morgan".into()
            }
        )
        .await
        .is_err()
    );
    let request = id();
    command(
        s,
        &a,
        PeopleCommand::RequestMerge {
            id: request.clone(),
            source_id: source.clone(),
            target_id: target.clone(),
            preview_token: preview.token,
            name: "Morgan".into(),
        },
    )
    .await?;
    command(
        s,
        &b,
        PeopleCommand::RespondRequest {
            id: request,
            accept: true,
        },
    )
    .await?;
    assert_eq!(s.person_detail(&a, &source).await?.person.id, target);
    assert!(!visible(s, &a, &target).await?.contains(&hidden));
    Ok(())
}
async fn linked_source_merge_scenario(s: &Store) -> Result<()> {
    let subject = account(s).await?;
    let other = account(s).await?;
    let source = command(
        s,
        &other,
        PeopleCommand::ReferenceAccount {
            account_id: subject.clone(),
            person_id: id(),
        },
    )
    .await?;
    let target = person(s, &other, "Duplicate", share(&subject)).await?;
    let private = field(s, &other, &target, "Surprise").await?;
    let name = s.person_detail(&subject, &source).await?.person.label;
    let preview = s.merge_preview(&other, &source, &target).await?;
    let request = id();
    command(
        s,
        &other,
        PeopleCommand::RequestMerge {
            id: request.clone(),
            source_id: source.clone(),
            target_id: target.clone(),
            preview_token: preview.token,
            name,
        },
    )
    .await?;
    command(
        s,
        &subject,
        PeopleCommand::RespondRequest {
            id: request,
            accept: true,
        },
    )
    .await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT owner_id FROM resources WHERE id=$1")
            .bind(&target)
            .fetch_one(&s.pool)
            .await?,
        subject
    );
    assert!(!visible(s, &subject, &target).await?.contains(&private));
    assert_eq!(s.person_detail(&subject, &source).await?.person.id, target);
    // Ownership of the old target does not let the other person remove the link.
    let version = s.person_detail(&other, &target).await?.person.version;
    assert!(
        command(
            s,
            &other,
            PeopleCommand::Unlink {
                person_id: target.clone(),
                expected_version: version
            }
        )
        .await
        .is_err()
    );
    command(
        s,
        &subject,
        PeopleCommand::RenameProfile {
            person_id: target,
            expected_version: version,
            name: "My name".into(),
        },
    )
    .await?;
    Ok(())
}
#[tokio::test]
async fn linked_source_merge_keeps_subject_identity_ownership() -> Result<()> {
    let (s, _dir) = setup().await?;
    linked_source_merge_scenario(&s).await
}

#[tokio::test]
async fn people_merge_privacy_aliases_and_replay() -> Result<()> {
    let (s, _dir) = setup().await?;
    merge_scenario(&s).await?;
    owner_gate_scenario(&s).await
}
#[tokio::test]
async fn people_linking_requires_consent_and_preserves_notes() -> Result<()> {
    let (s, _dir) = setup().await?;
    linking_scenario(&s).await
}
#[tokio::test]
async fn people_cross_owner_merge_requires_both_owners() -> Result<()> {
    let (s, _dir) = setup().await?;
    approved_merge_scenario(&s).await
}

#[tokio::test]
async fn merge_preserves_birthday_anchor_reference_and_occurrences() -> Result<()> {
    use atlas_core::calendars::{Anchor, Offset, Reference};
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let source = person(&s, &a, "Morgan", Policy::default()).await?;
    let target = person(&s, &a, "Morgan again", Policy::default()).await?;
    let date = id();
    s.task_command(
        &a,
        &id(),
        &TaskCommand::PutField {
            id: date.clone(),
            parent_id: source.clone(),
            expected_version: None,
            label: "Birthday".into(),
            value: FieldValue::Date {
                year: None,
                month: 9,
                day: 20,
            },
            initial_policy: None,
        },
        None,
        NOW,
    )
    .await?;
    let task = id();
    let mut definition = presets("2026-09-08", "Europe/Vienna")?.remove(0).definition;
    definition.schedule.start_date = None;
    definition.schedule.repeat = None;
    s.task_command(
        &a,
        &id(),
        &TaskCommand::CreateTask {
            id: task.clone(),
            execution_id: id(),
            title: "Buy a gift".into(),
            definition,
            initial_policy: None,
            anchor: Some(Anchor {
                reference: Reference::PersonDate {
                    field_id: date.clone(),
                    annual: true,
                },
                offset: Offset::CalendarDays {
                    days: -7,
                    time: None,
                },
                weekdays: vec![],
                title_contains: None,
            }),
        },
        None,
        NOW,
    )
    .await?;
    let before: Vec<String> =
        sqlx::query_scalar("SELECT id FROM task_occurrences WHERE task_id=$1 ORDER BY id")
            .bind(&task)
            .fetch_all(&s.pool)
            .await?;
    assert!(!before.is_empty());
    let preview = s.merge_preview(&a, &source, &target).await?;
    command(
        &s,
        &a,
        PeopleCommand::Merge {
            source_id: source,
            target_id: target,
            preview_token: preview.token,
            name: "Morgan".into(),
        },
    )
    .await?;
    s.reconcile_calendar_tasks(NOW).await?;
    let after: Vec<String> =
        sqlx::query_scalar("SELECT id FROM task_occurrences WHERE task_id=$1 ORDER BY id")
            .bind(&task)
            .fetch_all(&s.pool)
            .await?;
    assert_eq!(before, after);
    assert!(
        sqlx::query_scalar::<_, String>("SELECT spec FROM task_anchors WHERE task_id=$1")
            .bind(task)
            .fetch_one(&s.pool)
            .await?
            .contains(&date)
    );
    Ok(())
}
#[tokio::test]
async fn concurrent_reverse_merges_cannot_create_alias_cycles() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let x = person(&s, &a, "One", Policy::default()).await?;
    let y = person(&s, &a, "Two", Policy::default()).await?;
    let left = s.merge_preview(&a, &x, &y).await?;
    let right = s.merge_preview(&a, &y, &x).await?;
    let (left, right) = tokio::join!(
        command(
            &s,
            &a,
            PeopleCommand::Merge {
                source_id: x.clone(),
                target_id: y.clone(),
                preview_token: left.token,
                name: "One person".into()
            }
        ),
        command(
            &s,
            &a,
            PeopleCommand::Merge {
                source_id: y,
                target_id: x,
                preview_token: right.token,
                name: "One person".into()
            }
        )
    );
    assert_ne!(left.is_ok(), right.is_ok());
    Ok(())
}
#[tokio::test]
async fn people_requests_can_expire_decline_or_be_cancelled() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let b = account(&s).await?;
    let p = person(&s, &a, "Morgan", Policy::default()).await?;
    for state in ["decline", "cancel", "expire"] {
        let r = id();
        command(
            &s,
            &a,
            PeopleCommand::RequestLink {
                id: r.clone(),
                person_id: p.clone(),
                account_id: b.clone(),
                expected_version: 1,
            },
        )
        .await?;
        if state == "decline" {
            command(
                &s,
                &b,
                PeopleCommand::RespondRequest {
                    id: r.clone(),
                    accept: false,
                },
            )
            .await?;
        } else if state == "cancel" {
            command(&s, &a, PeopleCommand::CancelRequest { id: r.clone() }).await?;
        }
        assert!(
            s.people_command(
                &b,
                &id(),
                &PeopleCommand::RespondRequest {
                    id: r,
                    accept: true
                },
                if state == "expire" { NOW + 604801 } else { NOW }
            )
            .await
            .is_err()
        );
    }
    assert!(s.person_detail(&a, &p).await?.linked_account_id.is_none());
    Ok(())
}
