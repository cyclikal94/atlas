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
            expected_policy_version: None,
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
                accept: true,
                recipient_preview_token: None
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
            recipient_preview_token: None,
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
                expected_policy_version: p.policy_version,
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
                accept: true,
                recipient_preview_token: None
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
    let recipient_preview = s.recipient_merge_preview(&b, &request, NOW).await?;
    assert!(!recipient_preview.stale);
    command(
        s,
        &b,
        PeopleCommand::RespondRequest {
            id: request,
            accept: true,
            recipient_preview_token: Some(recipient_preview.token),
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
    let recipient_preview = s.recipient_merge_preview(&subject, &request, NOW).await?;
    command(
        s,
        &subject,
        PeopleCommand::RespondRequest {
            id: request,
            accept: true,
            recipient_preview_token: Some(recipient_preview.token),
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
            expected_policy_version: None,
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
                    recipient_preview_token: None,
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
                    accept: true,
                    recipient_preview_token: None
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

/// Readers and editors may hold a person without owning it. Sending a link request and
/// reading the ACL stay owner-only whatever else a reader can see about the resource.
#[tokio::test]
async fn request_link_and_acl_read_still_require_ownership() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let b = account(&s).await?;
    let c = account(&s).await?;
    let p = person(&s, &a, "Morgan", share(&b)).await?;
    let request = id();
    let error = s
        .people_command(
            &b,
            &id(),
            &PeopleCommand::RequestLink {
                id: request.clone(),
                person_id: p.clone(),
                account_id: c.clone(),
                expected_version: 1,
            },
            NOW,
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "conflict");
    assert!(s.people_requests(&c, NOW).await?.is_empty());
    match s.resource_policy(&b, &p).await {
        Ok(_) => panic!("a non-owner must not read the ACL"),
        Err(error) => assert_eq!(error.to_string(), "forbidden"),
    }
    // The owner keeps both capabilities.
    command(
        &s,
        &a,
        PeopleCommand::RequestLink {
            id: request,
            person_id: p.clone(),
            account_id: c.clone(),
            expected_version: 1,
        },
    )
    .await?;
    assert_eq!(s.people_requests(&c, NOW).await?.len(), 1);
    assert!(s.resource_policy(&a, &p).await.is_ok());
    Ok(())
}

/// The counters of an edit belong to the resource its author read. After a merge the source
/// person's old ID resolves to the canonical person, whose own counters can coincide with
/// the source's while its audience differs, so the edit is refused rather than re-addressed.
#[tokio::test]
async fn edit_through_a_merged_alias_is_refused_even_when_the_counters_coincide() -> Result<()> {
    use crate::support::projection::projection;
    use crate::support::sharing::ledger;
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let b = account(&s).await?;
    let source = person(&s, &a, "Morgan", Policy::default()).await?;
    let target = person(&s, &a, "Morgan Jones", Policy::default()).await?;
    let edit = |id: &str, version, policy_version, label: &str| Command::Edit {
        id: id.into(),
        expected_version: version,
        expected_policy_version: Some(policy_version),
        label: label.into(),
        value: String::new(),
    };
    let grant = |id: &str, version| Command::Grant {
        id: id.into(),
        expected_version: version,
        account_id: b.clone(),
        edit: false,
    };
    // The source is edited once, shared with bob and unshared again: private, at (2, 3).
    s.apply(&a, &id(), &[edit(&source, 1, 1, "Morgan")]).await?;
    s.apply(&a, &id(), &[grant(&source, 1)]).await?;
    s.apply(
        &a,
        &id(),
        &[Command::Revoke {
            id: source.clone(),
            expected_version: 2,
            account_id: b.clone(),
        }],
    )
    .await?;
    // The target is shared with bob at (1, 2). Alice's unsent draft is captured against the
    // source's private projection.
    s.apply(&a, &id(), &[grant(&target, 1)]).await?;
    let captured = projection(&s, &a, &source).await?;
    assert_eq!((captured.version, captured.policy_version), (2, Some(3)));
    assert!(projection(&s, &b, &source).await.is_err());
    let before_merge = projection(&s, &a, &target).await?;
    assert_eq!(
        (before_merge.version, before_merge.policy_version),
        (1, Some(2))
    );
    // Another session merges the source into the target.
    let preview = s.merge_preview(&a, &source, &target).await?;
    command(
        &s,
        &a,
        PeopleCommand::Merge {
            source_id: source.clone(),
            target_id: target.clone(),
            preview_token: preview.token,
            name: "Morgan Jones".into(),
        },
    )
    .await?;
    let canonical = projection(&s, &a, &target).await?;
    // The premise: the target's counters now equal the captured pair, and bob can read it.
    assert_eq!(
        (canonical.version, canonical.policy_version),
        (captured.version, captured.policy_version)
    );
    assert!(projection(&s, &b, &target).await.is_ok());

    let unchanged = ledger(&s, &target).await?;
    let unsent = edit(
        &source,
        captured.version,
        captured.policy_version.unwrap(),
        "Unsent text",
    );
    let operation = id();
    let error = s
        .apply(&a, &operation, std::slice::from_ref(&unsent))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "conflict");
    // Nothing was written, receipted or published, and bob never saw the text.
    assert_eq!(ledger(&s, &target).await?, unchanged);
    assert_eq!(projection(&s, &b, &target).await?.label, "Morgan Jones");
    assert_eq!(
        s.person_detail(&a, &source).await?.person.label,
        "Morgan Jones"
    );
    // A rejected write stores no receipt, so retrying the same body is refused again.
    let retry = s
        .apply(&a, &operation, std::slice::from_ref(&unsent))
        .await
        .unwrap_err();
    assert_eq!(retry.to_string(), "conflict");
    assert_eq!(ledger(&s, &target).await?, unchanged);

    // After reading the canonical person, an edit addressed to it commits normally.
    s.apply(
        &a,
        &id(),
        &[edit(
            &target,
            canonical.version,
            canonical.policy_version.unwrap(),
            "Reviewed text",
        )],
    )
    .await?;
    assert_eq!(projection(&s, &b, &target).await?.label, "Reviewed text");
    Ok(())
}

/// A committed edit addressed to an ID that later became an alias still replays: the
/// receipt is read before any identity is resolved.
#[tokio::test]
async fn an_edit_receipted_before_a_merge_still_replays_afterwards() -> Result<()> {
    use crate::support::sharing::ledger;
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let source = person(&s, &a, "Morgan", Policy::default()).await?;
    let target = person(&s, &a, "Morgan Jones", Policy::default()).await?;
    let rename = |label: &str| Command::Edit {
        id: source.clone(),
        expected_version: 1,
        expected_policy_version: Some(1),
        label: label.into(),
        value: String::new(),
    };
    let operation = id();
    let revision = s.apply(&a, &operation, &[rename("Renamed")]).await?;
    let preview = s.merge_preview(&a, &source, &target).await?;
    command(
        &s,
        &a,
        PeopleCommand::Merge {
            source_id: source.clone(),
            target_id: target.clone(),
            preview_token: preview.token,
            name: "Morgan Jones".into(),
        },
    )
    .await?;
    let unchanged = ledger(&s, &target).await?;
    assert_eq!(
        s.apply(&a, &operation, &[rename("Renamed")]).await?,
        revision
    );
    assert_eq!(ledger(&s, &target).await?, unchanged);
    // The receipt binds the original payload; a different body is still a conflict.
    let other = s
        .apply(&a, &operation, &[rename("Other")])
        .await
        .unwrap_err();
    assert_eq!(other.to_string(), "operation_conflict");
    Ok(())
}

/// BE-Q16: the recipient of a pending merge request can preview it with one side hidden, sees
/// `stale: true` once the sender's own visibility changes, and a merge acceptance with a
/// stale/missing `recipient_preview_token` is rejected without committing.
async fn recipient_safe_preview_scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    // `source` is private to `a`; `target` is owned by `b` and shared only with `a` — the
    // initiator (`a`) can see both sides, but the recipient (`b`) can never see `source`.
    let source = person(s, &a, "Morgan", Policy::default()).await?;
    let target = person(s, &b, "Morgan", share(&a)).await?;
    let preview = s.merge_preview(&a, &source, &target).await?;
    assert!(preview.requires_approval);
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

    let safe = s.recipient_merge_preview(&b, &request, NOW).await?;
    assert!(
        safe.source.is_none(),
        "b cannot see source, omitted not errored"
    );
    assert_eq!(
        safe.target.as_ref().map(|p| p.id.as_str()),
        Some(target.as_str())
    );
    assert!(!safe.stale);
    assert!(safe.requires_approval);

    // A stale or missing `recipient_preview_token` at acceptance is rejected; the merge does
    // not commit.
    for bad_token in [None, Some("0".repeat(64))] {
        assert_eq!(
            s.people_command(
                &b,
                &id(),
                &PeopleCommand::RespondRequest {
                    id: request.clone(),
                    accept: true,
                    recipient_preview_token: bad_token,
                },
                NOW,
            )
            .await
            .unwrap_err()
            .to_string(),
            "conflict"
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT owner_id FROM resources WHERE id=$1")
            .bind(&target)
            .fetch_one(&s.pool)
            .await?,
        b,
        "not merged yet"
    );

    // Accepting with the freshly-read recipient token commits.
    let fresh = s.recipient_merge_preview(&b, &request, NOW).await?;
    command(
        s,
        &b,
        PeopleCommand::RespondRequest {
            id: request,
            accept: true,
            recipient_preview_token: Some(fresh.token),
        },
    )
    .await?;
    assert_eq!(s.person_detail(&a, &source).await?.person.id, target);

    // Separate pair: the sender's own visibility changes after proposing (`b` revokes `a`'s
    // access to `target2`), so `a` can no longer reproduce their original preview — the
    // recipient read reports `stale: true`, still 200, not an error.
    let source2 = person(s, &a, "Morgan Two", Policy::default()).await?;
    let target2 = person(s, &b, "Morgan Two", share(&a)).await?;
    let preview2 = s.merge_preview(&a, &source2, &target2).await?;
    let request2 = id();
    command(
        s,
        &a,
        PeopleCommand::RequestMerge {
            id: request2.clone(),
            source_id: source2,
            target_id: target2.clone(),
            preview_token: preview2.token,
            name: "Morgan Two".into(),
        },
    )
    .await?;
    let target2_version = s.resource_policy(&b, &target2).await?.version;
    s.management(
        &b,
        &id(),
        &[ManagementCommand::ReplacePolicy {
            id: target2,
            expected_version: target2_version,
            policy: Policy::default(),
        }],
        NOW,
    )
    .await?;
    let stale = s.recipient_merge_preview(&b, &request2, NOW).await?;
    assert!(stale.stale);
    assert!(stale.source.is_none());
    Ok(())
}
#[tokio::test]
async fn recipient_safe_merge_preview_and_acceptance_check() -> Result<()> {
    let (s, _dir) = setup().await?;
    recipient_safe_preview_scenario(&s).await
}

/// BE-Q16 edge cases: withdrawn (404, distinct from expired), expired-but-unswept (410), the
/// wrong proposal kind (422), and ownership shifting away from the recipient after the request
/// was created (403, re-checked fresh rather than cached from request-creation time).
async fn recipient_preview_edge_cases_scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;

    // Withdrawn: cancelled by the sender before the recipient reads the preview -> 404, not 410.
    let source = person(s, &a, "Morgan", Policy::default()).await?;
    let target = person(s, &b, "Morgan", share(&a)).await?;
    let preview = s.merge_preview(&a, &source, &target).await?;
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
        &a,
        PeopleCommand::CancelRequest {
            id: request.clone(),
        },
    )
    .await?;
    assert_eq!(
        s.recipient_merge_preview(&b, &request, NOW)
            .await
            .unwrap_err()
            .to_string(),
        "not_found"
    );

    // Expired but not yet swept: distinguishable from "withdrawn".
    let source2 = person(s, &a, "Morgan Two", Policy::default()).await?;
    let target2 = person(s, &b, "Morgan Two", share(&a)).await?;
    let preview2 = s.merge_preview(&a, &source2, &target2).await?;
    let request2 = id();
    command(
        s,
        &a,
        PeopleCommand::RequestMerge {
            id: request2.clone(),
            source_id: source2.clone(),
            target_id: target2.clone(),
            preview_token: preview2.token,
            name: "Morgan Two".into(),
        },
    )
    .await?;
    assert_eq!(
        s.recipient_merge_preview(&b, &request2, NOW + 604801)
            .await
            .unwrap_err()
            .to_string(),
        "invitation_expired"
    );

    // A `Link` proposal's request ID is the wrong kind for this endpoint.
    let link_target = person(s, &a, "Linkable", Policy::default()).await?;
    let link_request = id();
    command(
        s,
        &a,
        PeopleCommand::RequestLink {
            id: link_request.clone(),
            person_id: link_target,
            account_id: b.clone(),
            expected_version: 1,
        },
    )
    .await?;
    assert_eq!(
        s.recipient_merge_preview(&b, &link_request, NOW)
            .await
            .unwrap_err()
            .to_string(),
        "invalid_value"
    );

    // Ownership shifts away from the recipient after the request was created: a fresh
    // ownership check at read time, not one cached from request-creation time, applies.
    let source3 = person(s, &a, "Morgan Three", share(&b)).await?;
    let target3 = person(s, &b, "Morgan Three", share(&a)).await?;
    let preview3 = s.merge_preview(&a, &source3, &target3).await?;
    let request3 = id();
    command(
        s,
        &a,
        PeopleCommand::RequestMerge {
            id: request3.clone(),
            source_id: source3.clone(),
            target_id: target3.clone(),
            preview_token: preview3.token,
            name: "Morgan Three".into(),
        },
    )
    .await?;
    let c = account(s).await?;
    let link_id = id();
    command(
        s,
        &b,
        PeopleCommand::RequestLink {
            id: link_id.clone(),
            person_id: target3.clone(),
            account_id: c.clone(),
            expected_version: 1,
        },
    )
    .await?;
    command(
        s,
        &c,
        PeopleCommand::RespondRequest {
            id: link_id,
            accept: true,
            recipient_preview_token: None,
        },
    )
    .await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT owner_id FROM resources WHERE id=$1")
            .bind(&target3)
            .fetch_one(&s.pool)
            .await?,
        c
    );
    assert_eq!(
        s.recipient_merge_preview(&b, &request3, NOW)
            .await
            .unwrap_err()
            .to_string(),
        "forbidden"
    );
    Ok(())
}
#[tokio::test]
async fn recipient_preview_withdrawn_expired_wrong_kind_and_ownership_shift() -> Result<()> {
    let (s, _dir) = setup().await?;
    recipient_preview_edge_cases_scenario(&s).await
}

/// BE-Q16 R1: a receipt committed under the wire format that existed before
/// `recipient_preview_token` was added must still replay its stored result — rather than
/// failing with `409 operation_conflict` — now that the struct has gained that field.
///
/// This does not run old code. It fixes the pre-existing wire format as a literal: the digest
/// below is the independently precomputed SHA-256 of
/// `atlas-command-v1\npeople-command-v1:{"kind":"respond_request","id":"11111111-1111-1111-1111-111111111111","accept":true}`
/// — the exact two-field JSON a `RespondRequest{id,accept}` without this member serialised to.
/// Manually seeding a `receipts`/`people_results` row with that literal digest and replaying
/// today's three-field struct (the third field omitted via `skip_serializing_if`) checks that
/// today's serialisation reproduces the same digest, which is exactly the guarantee R1 required.
async fn legacy_receipt_wire_format_replay_scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let p = person(s, &a, "Legacy Wire Format", Policy::default()).await?;
    let request_id = "11111111-1111-1111-1111-111111111111".to_string();
    command(
        s,
        &a,
        PeopleCommand::RequestLink {
            id: request_id.clone(),
            person_id: p.clone(),
            account_id: b.clone(),
            expected_version: 1,
        },
    )
    .await?;

    const LEGACY_DIGEST: &str = "abc0dd75600b2a6c8167f8982857e2cf131ae2a4aba432a73a27f3e556f1ee3d";
    let legacy_operation = id();
    sqlx::query(
        "INSERT INTO receipts(account_id,operation_id,payload,revision) VALUES ($1,$2,$3,$4)",
    )
    .bind(&b)
    .bind(&legacy_operation)
    .bind(LEGACY_DIGEST)
    .bind(999_999_i64)
    .execute(&s.pool)
    .await?;
    sqlx::query("INSERT INTO people_results VALUES ($1,$2,$3)")
        .bind(&b)
        .bind(&legacy_operation)
        .bind(&p)
        .execute(&s.pool)
        .await?;

    let replay = s
        .people_command(
            &b,
            &legacy_operation,
            &PeopleCommand::RespondRequest {
                id: request_id,
                accept: true,
                recipient_preview_token: None,
            },
            NOW,
        )
        .await?;
    assert_eq!(
        replay.revision, 999_999,
        "the seeded legacy receipt was returned, not a fresh commit"
    );
    assert_eq!(replay.person_id, p);
    Ok(())
}
#[tokio::test]
async fn legacy_receipt_wire_format_still_replays() -> Result<()> {
    let (s, _dir) = setup().await?;
    legacy_receipt_wire_format_replay_scenario(&s).await
}
