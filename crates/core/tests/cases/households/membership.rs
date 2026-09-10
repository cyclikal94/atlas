use anyhow::Result;
use atlas_core::{
    Change, Command, Store,
    households::ManagementCommand as M,
    policy::{DefaultTemplate, Policy, PrincipalGrant},
};
use uuid::Uuid;
fn id() -> String {
    Uuid::new_v4().to_string()
}
async fn account(s: &Store) -> Result<String> {
    let id = id();
    s.add_account(&id, &format!("u{}", id.replace('-', "")), "test")
        .await?;
    Ok(id)
}
async fn manage(s: &Store, a: &str, c: M) -> Result<i64> {
    s.management(a, &id(), &[c], 1000).await
}
fn person(id: &str, policy: Option<Policy>) -> Command {
    Command::CreatePerson {
        id: id.into(),
        name: "Morgan".into(),
        initial_policy: policy,
    }
}
fn household_policy(household: &str) -> Policy {
    Policy {
        grants: vec![PrincipalGrant::Household {
            id: household.into(),
            edit: true,
        }],
        exclude_accounts: vec![],
    }
}
async fn join(s: &Store, owner: &str, other: &str, household: &str) -> Result<()> {
    let invitation = id();
    let version = s
        .households(owner)
        .await?
        .into_iter()
        .find(|h| h.id == household)
        .unwrap()
        .version;
    manage(
        s,
        owner,
        M::InviteToHousehold {
            id: invitation.clone(),
            household_id: household.into(),
            recipient_id: other.into(),
            expected_version: version,
        },
    )
    .await?;
    manage(
        s,
        other,
        M::RespondToHouseholdInvitation {
            id: invitation,
            expected_version: 1,
            accept: true,
        },
    )
    .await?;
    Ok(())
}
fn visible(page: &atlas_core::Page, id: &str) -> bool {
    page.batches
        .iter()
        .flat_map(|b| &b.changes)
        .any(|c| matches!(c,Change::Upsert {resource} if resource.id==id))
}

async fn membership_and_defaults(s: &Store) -> Result<()> {
    let alice = account(s).await?;
    let bob = account(s).await?;
    let eve = account(s).await?;
    let household = id();
    manage(
        s,
        &alice,
        M::CreateHousehold {
            id: household.clone(),
            name: "Home".into(),
        },
    )
    .await?;
    let p = id();
    let private = id();
    s.apply(
        &alice,
        &id(),
        &[person(&p, None), person(&private, Some(Policy::default()))],
    )
    .await?;
    let before = s.sync(&bob, "phone", None, 200, 1000).await?;
    assert!(!visible(&before, &p));
    let defaults_before = s.defaults(&alice).await?;
    join(s, &alice, &bob, &household).await?;
    assert_ne!(defaults_before.revision, s.defaults(&alice).await?.revision);
    let shared = s
        .sync(&bob, "phone", Some(&before.next_cursor), 200, 1001)
        .await?;
    assert!(visible(&shared, &p));
    assert!(!visible(&shared, &private));
    // A member can edit shared content, but cannot invite or read the owner's ACL.
    s.apply(
        &bob,
        &id(),
        &[Command::Edit {
            id: p.clone(),
            expected_version: 1,
            label: "Shared rename".into(),
            value: String::new(),
        }],
    )
    .await?;
    assert!(s.resource_policy(&bob, &p).await.is_err());
    let version = s.households(&alice).await?[0].version;
    assert!(
        manage(
            s,
            &bob,
            M::InviteToHousehold {
                id: id(),
                household_id: household.clone(),
                recipient_id: eve,
                expected_version: version
            }
        )
        .await
        .is_err()
    );
    // A private direct grant survives losing household-derived access.
    s.apply(
        &alice,
        &id(),
        &[Command::Grant {
            id: private.clone(),
            expected_version: 1,
            account_id: bob.clone(),
            edit: false,
        }],
    )
    .await?;
    let latest = s
        .sync(&bob, "phone", Some(&shared.next_cursor), 200, 1002)
        .await?;
    assert!(visible(&latest, &private));
    manage(
        s,
        &alice,
        M::RemoveHouseholdMember {
            household_id: household.clone(),
            account_id: bob.clone(),
            expected_version: version,
        },
    )
    .await?;
    let removed = s
        .sync(&bob, "phone", Some(&latest.next_cursor), 200, 1003)
        .await?;
    assert!(
        removed
            .batches
            .iter()
            .flat_map(|b| &b.changes)
            .any(|c| matches!(c,Change::Remove{id} if id==&p))
    );
    assert!(visible(
        &s.sync(&bob, "phone", None, 200, 1003).await?,
        &private
    ));
    assert!(s.defaults(&bob).await?.primary_household_id.is_none());
    let version = s.households(&alice).await?[0].version;
    assert_eq!(
        manage(
            s,
            &alice,
            M::RemoveHouseholdMember {
                household_id: household,
                account_id: alice.clone(),
                expected_version: version
            }
        )
        .await
        .unwrap_err()
        .to_string(),
        "last_manager"
    );
    Ok(())
}

async fn layered_defaults(s: &Store) -> Result<()> {
    let alice = account(s).await?;
    let bob = account(s).await?;
    let household = id();
    manage(
        s,
        &alice,
        M::CreateHousehold {
            id: household.clone(),
            name: "Household".into(),
        },
    )
    .await?;
    join(s, &alice, &bob, &household).await?;
    let old = s.defaults(&alice).await?;
    manage(
        s,
        &alice,
        M::SetDefaults {
            household_id: Some(household.clone()),
            resource_kind: "person".into(),
            expected_version: 0,
            template: Some(DefaultTemplate::Private),
        },
    )
    .await?;
    let op = id();
    let task = person(&id(), None);
    assert_eq!(
        s.apply_with_defaults(
            &alice,
            &op,
            std::slice::from_ref(&task),
            Some(&old.revision)
        )
        .await
        .unwrap_err()
        .to_string(),
        "defaults_changed"
    );
    let defaults = s.defaults(&alice).await?;
    let revision = s
        .apply_with_defaults(
            &alice,
            &op,
            std::slice::from_ref(&task),
            Some(&defaults.revision),
        )
        .await?;
    manage(
        s,
        &alice,
        M::SetDefaults {
            household_id: None,
            resource_kind: "person".into(),
            expected_version: 0,
            template: Some(DefaultTemplate::PrimaryHousehold { edit: true }),
        },
    )
    .await?;
    // Successful operation retry acknowledges the original decision after defaults change.
    assert_eq!(
        revision,
        s.apply_with_defaults(&alice, &op, &[task], Some(&defaults.revision))
            .await?
    );
    let shared = id();
    s.apply(&alice, &id(), &[person(&shared, None)]).await?;
    let private = id();
    s.apply(&alice, &id(), &[person(&private, Some(Policy::default()))])
        .await?;
    let page = s.sync(&bob, "phone", None, 200, 1000).await?;
    assert!(visible(&page, &shared));
    assert!(!visible(&page, &private));
    let templates = s.default_templates(&alice, None).await?;
    assert_eq!(templates.person.version, 1);
    let second = id();
    manage(
        s,
        &alice,
        M::CreateHousehold {
            id: second.clone(),
            name: "Other home".into(),
        },
    )
    .await?;
    let preferences = s.defaults(&alice).await?.preferences_version;
    manage(
        s,
        &alice,
        M::SetPrimaryHousehold {
            household_id: Some(second.clone()),
            expected_version: preferences,
        },
    )
    .await?;
    assert_eq!(
        s.defaults(&alice).await?.primary_household_id.as_deref(),
        Some(second.as_str())
    );
    assert_eq!(s.households(&alice).await?.len(), 2);
    Ok(())
}

async fn policy_exclusions(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let h = id();
    manage(
        s,
        &a,
        M::CreateHousehold {
            id: h.clone(),
            name: "Home".into(),
        },
    )
    .await?;
    join(s, &a, &b, &h).await?;
    let p = id();
    let field = id();
    s.apply(
        &a,
        &id(),
        &[
            person(&p, None),
            Command::CreateField {
                id: field.clone(),
                person_id: p.clone(),
                label: "Secret".into(),
                value: "Gift".into(),
                initial_policy: None,
            },
        ],
    )
    .await?;
    let first = s.sync(&b, "phone", None, 200, 1000).await?;
    assert!(!visible(&first, &field));
    manage(
        s,
        &a,
        M::ReplacePolicy {
            id: field.clone(),
            expected_version: 1,
            policy: household_policy(&h),
        },
    )
    .await?;
    let second = s
        .sync(&b, "phone", Some(&first.next_cursor), 200, 1001)
        .await?;
    assert!(visible(&second, &field));
    let mut policy = household_policy(&h);
    policy.exclude_accounts.push(b.clone());
    manage(
        s,
        &a,
        M::ReplacePolicy {
            id: p.clone(),
            expected_version: 1,
            policy,
        },
    )
    .await?;
    let gone = s
        .sync(&b, "phone", Some(&second.next_cursor), 200, 1002)
        .await?;
    assert_eq!(
        gone.batches.last().unwrap().changes,
        [
            Change::Remove { id: field },
            Change::Remove { id: p.clone() }
        ]
    );
    assert!(visible(&s.sync(&a, "owner", None, 200, 1002).await?, &p));
    Ok(())
}

use crate::support::database::fixture;
#[tokio::test]
async fn join_leave_preserves_independent_grants() -> Result<()> {
    let (_d, s) = fixture().await?;
    membership_and_defaults(&s).await
}
#[tokio::test]
async fn defaults_layer_and_offline_guards() -> Result<()> {
    let (_d, s) = fixture().await?;
    layered_defaults(&s).await
}
#[tokio::test]
async fn exclusions_preserve_person_identity_rules() -> Result<()> {
    let (_d, s) = fixture().await?;
    policy_exclusions(&s).await?;
    invitation_authority(&s).await?;
    large_membership_change(&s).await?;
    former_household_defaults(&s).await
}

async fn invitation_authority(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let stranger = account(s).await?;
    let h = id();
    let invitation = id();
    manage(
        s,
        &a,
        M::CreateHousehold {
            id: h.clone(),
            name: "Original".into(),
        },
    )
    .await?;
    manage(
        s,
        &a,
        M::RenameHousehold {
            id: h.clone(),
            name: "Renamed".into(),
            expected_version: 1,
        },
    )
    .await?;
    assert_eq!(s.households(&a).await?[0].name, "Renamed");
    manage(
        s,
        &a,
        M::InviteToHousehold {
            id: invitation.clone(),
            household_id: h.clone(),
            recipient_id: b.clone(),
            expected_version: 2,
        },
    )
    .await?;
    let accept = M::RespondToHouseholdInvitation {
        id: invitation.clone(),
        expected_version: 1,
        accept: true,
    };
    assert_eq!(
        manage(s, &stranger, accept.clone())
            .await
            .unwrap_err()
            .to_string(),
        "not_found"
    );
    assert_eq!(
        s.management(&b, &id(), std::slice::from_ref(&accept), 700000)
            .await
            .unwrap_err()
            .to_string(),
        "invitation_expired"
    );
    manage(
        s,
        &a,
        M::RevokeHouseholdInvitation {
            id: invitation,
            expected_version: 1,
        },
    )
    .await?;
    assert_eq!(
        manage(s, &b, accept).await.unwrap_err().to_string(),
        "conflict"
    );
    assert!(s.households(&b).await?.is_empty());
    Ok(())
}
async fn large_membership_change(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let h = id();
    manage(
        s,
        &a,
        M::CreateHousehold {
            id: h.clone(),
            name: "Large household".into(),
        },
    )
    .await?;
    for _ in 0..12 {
        let commands: Vec<_> = (0..20).map(|_| person(&id(), None)).collect();
        s.apply(&a, &id(), &commands).await?;
    }
    let before = s.sync(&b, "phone", None, 200, 1000).await?;
    join(s, &a, &b, &h).await?;
    assert_eq!(
        s.sync(&b, "phone", Some(&before.next_cursor), 200, 1001)
            .await
            .unwrap_err()
            .to_string(),
        "resync_required"
    );
    let first = s.sync(&b, "phone", None, 200, 1001).await?;
    assert_eq!(first.batches[0].changes.len(), 200);
    assert!(first.has_more);
    let tail = s
        .sync(&b, "phone", Some(&first.next_cursor), 200, 1001)
        .await?;
    assert_eq!(tail.batches[0].changes.len(), 40);
    assert!(!tail.has_more);
    Ok(())
}
#[tokio::test]
async fn invitation_authority_and_expiry() -> Result<()> {
    let (_d, s) = fixture().await?;
    invitation_authority(&s).await
}
#[tokio::test]
async fn large_access_change_uses_bounded_recovery() -> Result<()> {
    let (_d, s) = fixture().await?;
    large_membership_change(&s).await?;
    former_household_defaults(&s).await
}

async fn former_household_defaults(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let h = id();
    manage(
        s,
        &a,
        M::CreateHousehold {
            id: h.clone(),
            name: "Home".into(),
        },
    )
    .await?;
    join(s, &a, &b, &h).await?;
    manage(
        s,
        &b,
        M::SetDefaults {
            household_id: None,
            resource_kind: "person".into(),
            expected_version: 0,
            template: Some(DefaultTemplate::Explicit {
                policy: household_policy(&h),
            }),
        },
    )
    .await?;
    let version = s.households(&a).await?[0].version;
    manage(
        s,
        &a,
        M::RemoveHouseholdMember {
            household_id: h.clone(),
            account_id: b.clone(),
            expected_version: version,
        },
    )
    .await?;
    let before = s.defaults(&b).await?.revision;
    manage(
        s,
        &a,
        M::RenameHousehold {
            id: h,
            name: "Private household update".into(),
            expected_version: version + 1,
        },
    )
    .await?;
    assert_eq!(before, s.defaults(&b).await?.revision);
    Ok(())
}
#[tokio::test]
async fn former_member_cannot_observe_default_activity() -> Result<()> {
    let (_d, s) = fixture().await?;
    former_household_defaults(&s).await
}
