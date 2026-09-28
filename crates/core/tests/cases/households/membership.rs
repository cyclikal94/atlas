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
async fn manage_at(s: &Store, a: &str, c: M, now: i64) -> Result<i64> {
    s.management(a, &id(), &[c], now).await
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
    // A member can edit shared content, but cannot invite or read the owner's ACL. Their
    // authority comes from the household default, which is still a policy state: an edit
    // that omits or misstates the revision it was based on is rejected.
    let rename = |policy_version| Command::Edit {
        id: p.clone(),
        expected_version: 1,
        expected_policy_version: policy_version,
        label: "Shared rename".into(),
        value: String::new(),
    };
    let current = crate::support::projection::policy_version(s, &bob, &p).await?;
    for wrong in [None, Some(current + 1)] {
        let error = s.apply(&bob, &id(), &[rename(wrong)]).await.unwrap_err();
        assert_eq!(error.to_string(), "conflict");
    }
    // A household member obtains the revision from their own projection.
    s.apply(&bob, &id(), &[rename(Some(current))]).await?;
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

/// BE-Q16: sent-invitation history across accepted/declined/revoked, all reported by
/// `sent_invitations`, and the "pending && expired" rule only firing on the still-pending one.
async fn sent_invitation_history_scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let accepted_by = account(s).await?;
    let declined_by = account(s).await?;
    let revoked_recipient = account(s).await?;
    let pending_recipient = account(s).await?;
    let h = id();
    manage(
        s,
        &a,
        M::CreateHousehold {
            id: h.clone(),
            name: "Sent history".into(),
        },
    )
    .await?;

    let accepted = id();
    let version = s.households(&a).await?[0].version;
    manage(
        s,
        &a,
        M::InviteToHousehold {
            id: accepted.clone(),
            household_id: h.clone(),
            recipient_id: accepted_by.clone(),
            expected_version: version,
        },
    )
    .await?;
    manage(
        s,
        &accepted_by,
        M::RespondToHouseholdInvitation {
            id: accepted.clone(),
            expected_version: 1,
            accept: true,
        },
    )
    .await?;

    let declined = id();
    let version = s.households(&a).await?[0].version;
    manage(
        s,
        &a,
        M::InviteToHousehold {
            id: declined.clone(),
            household_id: h.clone(),
            recipient_id: declined_by.clone(),
            expected_version: version,
        },
    )
    .await?;
    manage(
        s,
        &declined_by,
        M::RespondToHouseholdInvitation {
            id: declined.clone(),
            expected_version: 1,
            accept: false,
        },
    )
    .await?;

    let revoked = id();
    let version = s.households(&a).await?[0].version;
    manage(
        s,
        &a,
        M::InviteToHousehold {
            id: revoked.clone(),
            household_id: h.clone(),
            recipient_id: revoked_recipient.clone(),
            expected_version: version,
        },
    )
    .await?;
    manage(
        s,
        &a,
        M::RevokeHouseholdInvitation {
            id: revoked.clone(),
            expected_version: 1,
        },
    )
    .await?;

    let pending = id();
    let version = s.households(&a).await?[0].version;
    manage(
        s,
        &a,
        M::InviteToHousehold {
            id: pending.clone(),
            household_id: h,
            recipient_id: pending_recipient,
            expected_version: version,
        },
    )
    .await?;

    let page = s.sent_invitations(&a, None, 200, 1000).await?;
    let states: std::collections::BTreeMap<String, String> = page
        .items
        .iter()
        .map(|item| (item.id.clone(), item.status.clone()))
        .collect();
    assert_eq!(states[&accepted], "accepted");
    assert_eq!(states[&declined], "declined");
    assert_eq!(states[&revoked], "revoked");
    assert_eq!(states[&pending], "pending");
    assert!(page.next_after.is_none());

    // Past the expiry horizon: only the still-pending invitation computes as "expired"; the
    // already-terminal ones keep their true state.
    let later = s
        .sent_invitations(&a, None, 200, 1000 + 7 * 86400 + 1)
        .await?;
    let states: std::collections::BTreeMap<String, String> = later
        .items
        .iter()
        .map(|item| (item.id.clone(), item.status.clone()))
        .collect();
    assert_eq!(states[&accepted], "accepted");
    assert_eq!(states[&declined], "declined");
    assert_eq!(states[&revoked], "revoked");
    assert_eq!(states[&pending], "expired");

    assert!(s.sent_invitations(&a, None, 0, 1000).await.is_err());
    assert!(s.sent_invitations(&a, None, 201, 1000).await.is_err());
    Ok(())
}
#[tokio::test]
async fn sent_invitations_reports_all_states_including_computed_expiry() -> Result<()> {
    let (_d, s) = fixture().await?;
    sent_invitation_history_scenario(&s).await
}

/// BE-Q16: `RemoveHouseholdMember`'s cascade-revoke of a removed member's other stale pending
/// invitations to the same household must flip every matching history row, reflected via the
/// identical `WHERE` predicate rather than an id list.
async fn cascade_revoke_reflected_in_history_scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let h = id();
    manage(
        s,
        &a,
        M::CreateHousehold {
            id: h.clone(),
            name: "Cascade".into(),
        },
    )
    .await?;

    // An old invitation to `b` that nobody ever responded to and that has since fallen past its
    // own expiry window — still `status='pending'` in storage (no cleanup job purges it).
    let stale = id();
    let version = s.households(&a).await?[0].version;
    manage_at(
        s,
        &a,
        M::InviteToHousehold {
            id: stale.clone(),
            household_id: h.clone(),
            recipient_id: b.clone(),
            expected_version: version,
        },
        1000,
    )
    .await?;

    // Well past `stale`'s expiry, `b` is invited again and joins via this second invitation.
    let later_now = 1000 + 7 * 86400 + 1;
    let joined = id();
    let version = s.households(&a).await?[0].version;
    manage_at(
        s,
        &a,
        M::InviteToHousehold {
            id: joined.clone(),
            household_id: h.clone(),
            recipient_id: b.clone(),
            expected_version: version,
        },
        later_now,
    )
    .await?;
    manage_at(
        s,
        &b,
        M::RespondToHouseholdInvitation {
            id: joined.clone(),
            expected_version: 1,
            accept: true,
        },
        later_now,
    )
    .await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM household_invitations WHERE id=$1")
            .bind(&stale)
            .fetch_one(&s.pool)
            .await?,
        "pending",
        "the stale invitation is still nominally pending in storage"
    );

    let version = s.households(&a).await?[0].version;
    manage_at(
        s,
        &a,
        M::RemoveHouseholdMember {
            household_id: h,
            account_id: b,
            expected_version: version,
        },
        later_now,
    )
    .await?;

    for table in ["household_invitations", "household_invitation_history"] {
        let stale_status: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT status FROM {table} WHERE id=$1"
        )))
        .bind(&stale)
        .fetch_one(&s.pool)
        .await?;
        assert_eq!(
            stale_status, "revoked",
            "{table} reflects the cascade for the stale invite"
        );
        // The cascade predicate only touches still-`pending` rows: the invitation `b` actually
        // joined through was already `accepted` and must be untouched by it.
        let joined_status: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT status FROM {table} WHERE id=$1"
        )))
        .bind(&joined)
        .fetch_one(&s.pool)
        .await?;
        assert_eq!(
            joined_status, "accepted",
            "{table} leaves the already-accepted invitation alone"
        );
    }
    Ok(())
}
#[tokio::test]
async fn cascade_revoke_on_removal_reflected_in_both_tables() -> Result<()> {
    let (_d, s) = fixture().await?;
    cascade_revoke_reflected_in_history_scenario(&s).await
}

/// Two concurrent `RespondToHouseholdInvitation` calls (accept and decline) against the same
/// pending invitation: exactly one commits, and the operational and durable history rows end up
/// consistent with each other.
async fn concurrent_invitation_responses_scenario(s: &Store) -> Result<()> {
    let a = account(s).await?;
    let b = account(s).await?;
    let h = id();
    manage(
        s,
        &a,
        M::CreateHousehold {
            id: h.clone(),
            name: "Race".into(),
        },
    )
    .await?;
    let invitation = id();
    let version = s.households(&a).await?[0].version;
    manage(
        s,
        &a,
        M::InviteToHousehold {
            id: invitation.clone(),
            household_id: h,
            recipient_id: b.clone(),
            expected_version: version,
        },
    )
    .await?;

    let accept_op = id();
    let accept_command = [M::RespondToHouseholdInvitation {
        id: invitation.clone(),
        expected_version: 1,
        accept: true,
    }];
    let decline_op = id();
    let decline_command = [M::RespondToHouseholdInvitation {
        id: invitation.clone(),
        expected_version: 1,
        accept: false,
    }];
    let accept = s.management(&b, &accept_op, &accept_command, 1000);
    let decline = s.management(&b, &decline_op, &decline_command, 1000);
    let (accept, decline) = tokio::join!(accept, decline);
    assert_ne!(accept.is_ok(), decline.is_ok());

    let operational: String =
        sqlx::query_scalar("SELECT status FROM household_invitations WHERE id=$1")
            .bind(&invitation)
            .fetch_one(&s.pool)
            .await?;
    let historical: String =
        sqlx::query_scalar("SELECT status FROM household_invitation_history WHERE id=$1")
            .bind(&invitation)
            .fetch_one(&s.pool)
            .await?;
    assert_eq!(operational, historical);
    assert!(operational == "accepted" || operational == "declined");
    Ok(())
}
#[tokio::test]
async fn concurrent_invitation_responses_keep_history_consistent() -> Result<()> {
    let (_d, s) = fixture().await?;
    concurrent_invitation_responses_scenario(&s).await
}
