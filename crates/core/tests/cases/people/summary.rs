//! BE-PEOPLE-SUMMARY: viewer-safe People summaries (`people_summaries`, `person_summary`,
//! `person_field_audiences`) and original-creator attribution (schema `1006`).
//!
//! The cases follow plan §4 (S1–S17). Fixtures are built through the public commands; raw SQL is
//! used only to make a person look as it did before `1006` (no attribution row). Kieran's three
//! 14 October 2026 decisions are the oracle: attribution is separate from ownership and unknown when
//! unrecorded; a non-owner is always `partial` and learns nothing about access it does not hold; In
//! Home means the primary household.
use crate::cases::accounts::writer_inventory::{enclosing, functions, sources};
use crate::support::{
    database::{fixture, postgres},
    sharing::{NOW, account, id, replace_policy},
};
use anyhow::Result;
use atlas_core::{
    Command, Store,
    households::ManagementCommand as M,
    people::{
        Access, AddedBy, Disclosure, Linkage, PeopleCommand, PeopleScope, PersonSummary,
        PersonSummaryDetail,
    },
    policy::{Policy, PrincipalGrant},
    tasks::TaskCommand,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const SCOPES: [PeopleScope; 4] = [
    PeopleScope::All,
    PeopleScope::InHome,
    PeopleScope::SharedWithMe,
    PeopleScope::NotLinked,
];

fn home_grant(household: &str, edit: bool) -> PrincipalGrant {
    PrincipalGrant::Household {
        id: household.into(),
        edit,
    }
}

fn account_grant(account: &str, edit: bool) -> PrincipalGrant {
    PrincipalGrant::Account {
        id: account.into(),
        edit,
    }
}

fn policy(grants: Vec<PrincipalGrant>, exclude: &[&str]) -> Policy {
    Policy {
        grants,
        exclude_accounts: exclude.iter().map(|a| (*a).to_owned()).collect(),
    }
}

async fn manage(s: &Store, actor: &str, command: M) -> Result<i64> {
    s.management(actor, &id(), &[command], NOW).await
}

/// A new household managed by `owner`. It becomes the owner's primary household if they had none.
async fn household(s: &Store, owner: &str, name: &str) -> Result<String> {
    let household = id();
    manage(
        s,
        owner,
        M::CreateHousehold {
            id: household.clone(),
            name: name.into(),
        },
    )
    .await?;
    Ok(household)
}

async fn household_version(s: &Store, member: &str, household: &str) -> Result<i64> {
    Ok(s.households(member)
        .await?
        .into_iter()
        .find(|h| h.id == household)
        .expect("a household the account belongs to")
        .version)
}

/// `owner` invites `other` into `household` and `other` accepts (it becomes their primary
/// household if they had none).
async fn join(s: &Store, owner: &str, other: &str, household: &str) -> Result<()> {
    let invitation = id();
    let expected_version = household_version(s, owner, household).await?;
    manage(
        s,
        owner,
        M::InviteToHousehold {
            id: invitation.clone(),
            household_id: household.into(),
            recipient_id: other.into(),
            expected_version,
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

async fn leave(s: &Store, member: &str, household: &str) -> Result<()> {
    let expected_version = household_version(s, member, household).await?;
    manage(
        s,
        member,
        M::RemoveHouseholdMember {
            household_id: household.into(),
            account_id: member.into(),
            expected_version,
        },
    )
    .await?;
    Ok(())
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
    policy: Option<Policy>,
) -> Result<String> {
    let field = id();
    s.apply(
        owner,
        &id(),
        &[Command::CreateField {
            id: field.clone(),
            person_id: person.into(),
            label: "Birthday".into(),
            value: "1 May".into(),
            initial_policy: policy,
        }],
    )
    .await?;
    Ok(field)
}

async fn archive(s: &Store, actor: &str, resource: &str, archived: bool) -> Result<()> {
    let version = sqlx::query_scalar::<_, i64>("SELECT version FROM resources WHERE id=$1")
        .bind(resource)
        .fetch_one(&s.pool)
        .await?;
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

/// `sender` asks `subject` to link `person`, and `subject` accepts: ownership moves to `subject`.
async fn link(s: &Store, sender: &str, subject: &str, person: &str) -> Result<()> {
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
    s.people_command(
        subject,
        &id(),
        &PeopleCommand::RespondRequest {
            id: request,
            accept: true,
            recipient_preview_token: None,
        },
        NOW,
    )
    .await?;
    Ok(())
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

/// Make `person` look as it did before schema 1006: no attribution row.
async fn forget_attribution(s: &Store, person: &str) -> Result<()> {
    sqlx::query("DELETE FROM person_attributions WHERE person_id=$1")
        .bind(person)
        .execute(&s.pool)
        .await?;
    Ok(())
}

/// Every row of `scope` for `viewer`, following `next_after` to the end at page size `limit`.
async fn traverse(
    s: &Store,
    viewer: &str,
    scope: PeopleScope,
    archived: bool,
    limit: u16,
) -> Result<Vec<PersonSummary>> {
    let mut items = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = s
            .people_summaries(viewer, scope, archived, after.as_deref(), limit)
            .await?;
        let full = page.items.len() == usize::from(limit);
        assert!(page.items.len() <= usize::from(limit));
        // `next_after` is the last listed id exactly when the page is full.
        assert_eq!(
            page.next_after,
            full.then(|| page.items.last().map(|p| p.id.clone()))
                .flatten()
        );
        items.extend(page.items);
        match page.next_after {
            Some(next) => after = Some(next),
            None => return Ok(items),
        }
    }
}

async fn ids(s: &Store, viewer: &str, scope: PeopleScope) -> Result<BTreeSet<String>> {
    Ok(traverse(s, viewer, scope, false, 100)
        .await?
        .into_iter()
        .map(|p| p.id)
        .collect())
}

/// The scopes `viewer` finds `person` in.
async fn scopes_of(s: &Store, viewer: &str, person: &str) -> Result<Vec<PeopleScope>> {
    let mut found = Vec::new();
    for scope in SCOPES {
        if ids(s, viewer, scope).await?.contains(person) {
            found.push(scope);
        }
    }
    Ok(found)
}

async fn row(s: &Store, viewer: &str, person: &str) -> Result<PersonSummary> {
    Ok(traverse(s, viewer, PeopleScope::All, false, 100)
        .await?
        .into_iter()
        .find(|p| p.id == person)
        .unwrap_or_else(|| panic!("{person} is not listed for {viewer}")))
}

async fn detail(s: &Store, viewer: &str, person: &str) -> Result<PersonSummaryDetail> {
    s.person_summary(viewer, person).await
}

async fn username(s: &Store, account: &str) -> Result<String> {
    Ok(
        sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
            .bind(account)
            .fetch_one(&s.pool)
            .await?,
    )
}

fn code(error: anyhow::Error) -> String {
    error.to_string()
}

/// Replace the members a person's own identity determines, so two people the viewer cannot tell
/// apart compare equal.
fn normalised(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| match k.as_str() {
                    "id" | "label" | "version" | "policy_version" => (k, Value::Null),
                    _ => (k, normalised(v)),
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(normalised).collect()),
        other => other,
    }
}

// S1 ------------------------------------------------------------------------------------------

#[tokio::test]
async fn s1_an_owners_private_unlinked_person_is_complete_and_only_in_all_and_not_linked()
-> Result<()> {
    let (_dir, s) = fixture().await?;
    let a = account(&s).await?;
    let p = create_person(&s, &a, "Robin", Policy::default()).await?;
    let page = s
        .people_summaries(&a, PeopleScope::All, false, None, 50)
        .await?;
    assert_eq!(page.home, None, "no household, so no primary household");
    let r = &page.items[0];
    assert_eq!((r.id.as_str(), r.label.as_str()), (p.as_str(), "Robin"));
    assert_eq!(r.linkage, Linkage::None);
    assert_eq!(r.linked_account_id, None);
    assert!(r.owner.you);
    assert_eq!(r.owner.account_id, a);
    assert_eq!(r.owner.username, username(&s, &a).await?);
    assert_eq!(r.owner.person, None);
    assert_eq!(r.added_by_you, AddedBy::Yes);
    assert_eq!(r.your_access, Access::Owner);
    assert_eq!(r.field_count, 0);
    assert_eq!(r.audience.disclosure, Disclosure::Complete);
    assert!(!r.audience.in_home);
    assert!(r.audience.households.is_empty());
    assert_eq!(r.audience.your_direct_grant, None);
    assert_eq!(
        (r.audience.direct_grant_count, r.audience.excluded_count),
        (Some(0), Some(0))
    );
    assert_eq!(
        scopes_of(&s, &a, &p).await?,
        [PeopleScope::All, PeopleScope::NotLinked]
    );
    let d = detail(&s, &a, &p).await?;
    assert_eq!(d.summary, *r);
    assert_eq!(d.shared_with.disclosure, Disclosure::Complete);
    assert!(d.shared_with.accounts.is_empty() && d.shared_with.households.is_empty());
    assert_eq!(d.shared_with.excluded_accounts, Some(vec![]));
    assert_eq!(d.shared_with.owner, r.owner);
    Ok(())
}

// S2 ------------------------------------------------------------------------------------------

struct Shared {
    a: String,
    b: String,
    c: String,
    d: String,
    home: String,
    person: String,
}

/// A manages Home with members B and D. A's person is shared with Home (view), directly with C,
/// and excludes D.
async fn shared(s: &Store) -> Result<Shared> {
    let (a, b, c, d) = (
        account(s).await?,
        account(s).await?,
        account(s).await?,
        account(s).await?,
    );
    let home = household(s, &a, "Atlas Home").await?;
    join(s, &a, &b, &home).await?;
    join(s, &a, &d, &home).await?;
    let person = create_person(
        s,
        &a,
        "Mo",
        policy(
            vec![home_grant(&home, false), account_grant(&c, false)],
            &[&d],
        ),
    )
    .await?;
    Ok(Shared {
        a,
        b,
        c,
        d,
        home,
        person,
    })
}

#[tokio::test]
async fn s2_a_household_member_gets_a_partial_view_and_the_owner_the_complete_one() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let w = shared(&s).await?;
    let home_version = household_version(&s, &w.a, &w.home).await?;

    // B: partial, Home described in full (B is a member), nothing about C or D.
    let page = s
        .people_summaries(&w.b, PeopleScope::All, false, None, 50)
        .await?;
    let home = page.home.clone().expect("B's primary household is Home");
    assert_eq!(
        (
            home.id.as_str(),
            home.name.as_str(),
            home.version,
            home.member_count
        ),
        (w.home.as_str(), "Atlas Home", home_version, 3)
    );
    let r = row(&s, &w.b, &w.person).await?;
    assert_eq!(r.audience.disclosure, Disclosure::Partial);
    assert!(r.audience.in_home);
    assert_eq!(r.audience.households.len(), 1);
    let h = &r.audience.households[0];
    assert_eq!(
        (
            h.id.as_str(),
            h.can_edit,
            h.you_are_member,
            h.name.as_deref(),
            h.version,
            h.member_count
        ),
        (
            w.home.as_str(),
            false,
            true,
            Some("Atlas Home"),
            Some(home_version),
            Some(3)
        )
    );
    assert_eq!(r.audience.your_direct_grant, None);
    assert_eq!(
        (r.audience.direct_grant_count, r.audience.excluded_count),
        (None, None)
    );
    assert_eq!(r.your_access, Access::View);
    assert_eq!(r.added_by_you, AddedBy::No);
    assert!(!r.owner.you);
    assert_eq!(r.owner.account_id, w.a);
    assert_eq!(
        scopes_of(&s, &w.b, &w.person).await?,
        SCOPES,
        "B finds it in every scope"
    );
    let bd = detail(&s, &w.b, &w.person).await?;
    assert_eq!(bd.shared_with.disclosure, Disclosure::Partial);
    assert!(
        bd.shared_with.accounts.is_empty(),
        "B has no direct grant, and C's is not B's to know"
    );
    assert_eq!(bd.shared_with.households.len(), 1);
    assert_eq!(bd.shared_with.households[0].household, *h);
    assert_eq!(
        bd.shared_with.households[0].members, None,
        "never expanded for a non-owner"
    );
    assert_eq!(bd.shared_with.excluded_accounts, None);

    // A: complete.
    let r = row(&s, &w.a, &w.person).await?;
    assert_eq!(r.audience.disclosure, Disclosure::Complete);
    assert!(r.audience.in_home);
    assert_eq!(
        (r.audience.direct_grant_count, r.audience.excluded_count),
        (Some(1), Some(1))
    );
    let ad = detail(&s, &w.a, &w.person).await?;
    assert_eq!(ad.shared_with.accounts.len(), 1);
    assert_eq!(ad.shared_with.accounts[0].account_id, w.c);
    assert_eq!(
        ad.shared_with.accounts[0].username,
        username(&s, &w.c).await?
    );
    assert!(!ad.shared_with.accounts[0].can_edit);
    let members: BTreeMap<String, bool> = ad.shared_with.households[0]
        .members
        .clone()
        .expect("the owner belongs to Home")
        .into_iter()
        .map(|m| (m.account_id, m.excluded))
        .collect();
    assert_eq!(
        members,
        BTreeMap::from([
            (w.a.clone(), false),
            (w.b.clone(), false),
            (w.d.clone(), true)
        ])
    );
    let excluded = ad.shared_with.excluded_accounts.expect("owner");
    assert_eq!(excluded.len(), 1);
    assert_eq!(excluded[0].account_id, w.d);

    // C: its own direct grant only; Home is not C's household, so it is not mentioned.
    let r = row(&s, &w.c, &w.person).await?;
    assert_eq!(r.audience.disclosure, Disclosure::Partial);
    assert!(r.audience.households.is_empty() && !r.audience.in_home);
    assert_eq!(
        r.audience.your_direct_grant.as_ref().map(|g| g.can_edit),
        Some(false)
    );
    let cd = detail(&s, &w.c, &w.person).await?;
    assert_eq!(cd.shared_with.accounts.len(), 1);
    assert_eq!(cd.shared_with.accounts[0].account_id, w.c);

    // D is excluded: the person does not exist for D.
    for scope in SCOPES {
        assert!(!ids(&s, &w.d, scope).await?.contains(&w.person));
    }
    assert_eq!(
        code(detail(&s, &w.d, &w.person).await.unwrap_err()),
        "not_found"
    );
    Ok(())
}

// S3 ------------------------------------------------------------------------------------------

/// B's bytes for one person, its detail and its field audiences.
async fn view(s: &Store, viewer: &str, person: &str) -> Result<Value> {
    let fields = s
        .person_field_audiences(viewer, person, false, None, 100)
        .await?;
    Ok(serde_json::json!({
        "row": serde_json::to_value(row(s, viewer, person).await?)?,
        "detail": serde_json::to_value(detail(s, viewer, person).await?)?,
        "fields": serde_json::to_value(fields)?,
    }))
}

#[tokio::test]
async fn s3_hidden_grants_exclusions_and_households_never_change_a_non_owners_view() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (a, b, c, d, e, f) = (
        account(&s).await?,
        account(&s).await?,
        account(&s).await?,
        account(&s).await?,
        account(&s).await?,
        account(&s).await?,
    );
    let home = household(&s, &a, "Home").await?;
    join(&s, &a, &b, &home).await?;
    join(&s, &a, &d, &home).await?;
    let away = household(&s, &a, "Away").await?;
    join(&s, &a, &e, &away).await?;
    // Same visible state for B; the first also has a direct grant to C, a household B is not in
    // and an exclusion of D.
    let wide = create_person(
        &s,
        &a,
        "Twin",
        policy(
            vec![
                home_grant(&home, false),
                account_grant(&c, true),
                home_grant(&away, true),
            ],
            &[&d],
        ),
    )
    .await?;
    let narrow = create_person(&s, &a, "Twin", policy(vec![home_grant(&home, false)], &[])).await?;
    create_field(
        &s,
        &a,
        &wide,
        Some(policy(
            vec![home_grant(&home, false), account_grant(&c, false)],
            &[&d],
        )),
    )
    .await?;
    create_field(
        &s,
        &a,
        &narrow,
        Some(policy(vec![home_grant(&home, false)], &[])),
    )
    .await?;

    let wide_view = view(&s, &b, &wide).await?;
    let narrow_view = view(&s, &b, &narrow).await?;
    assert_eq!(wide_view["row"]["audience"]["disclosure"], "partial");
    assert_eq!(narrow_view["row"]["audience"]["disclosure"], "partial");
    assert_eq!(
        normalised(wide_view.clone()),
        normalised(narrow_view),
        "B cannot tell a widely shared person from one shared only with Home"
    );

    // Changing access B does not hold leaves B's bytes unchanged apart from policy_version.
    for change in [
        policy(
            vec![home_grant(&home, false), account_grant(&f, false)],
            &[&d],
        ),
        policy(
            vec![home_grant(&home, false), home_grant(&away, false)],
            &[],
        ),
        policy(vec![home_grant(&home, false)], &[&d]),
    ] {
        replace_policy(&s, &a, &wide, change).await?;
        let after = view(&s, &b, &wide).await?;
        assert_eq!(normalised(after), normalised(wide_view.clone()));
    }
    Ok(())
}

// S4 ------------------------------------------------------------------------------------------

#[tokio::test]
async fn s4_a_direct_grantee_sees_only_its_own_grant() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (a, b, c) = (account(&s).await?, account(&s).await?, account(&s).await?);
    let p = create_person(
        &s,
        &a,
        "Sam",
        policy(vec![account_grant(&b, true), account_grant(&c, false)], &[]),
    )
    .await?;
    let r = row(&s, &b, &p).await?;
    assert_eq!(
        r.audience.your_direct_grant.as_ref().map(|g| g.can_edit),
        Some(true)
    );
    assert_eq!(r.your_access, Access::Edit);
    assert!(!r.audience.in_home && r.audience.households.is_empty());
    assert_eq!(
        scopes_of(&s, &b, &p).await?,
        [
            PeopleScope::All,
            PeopleScope::SharedWithMe,
            PeopleScope::NotLinked
        ]
    );
    let d = detail(&s, &b, &p).await?;
    assert_eq!(d.shared_with.accounts.len(), 1);
    assert_eq!(d.shared_with.accounts[0].account_id, b);
    assert_eq!(d.shared_with.accounts[0].username, username(&s, &b).await?);
    assert!(d.shared_with.accounts[0].can_edit);
    Ok(())
}

// S5 ------------------------------------------------------------------------------------------

#[tokio::test]
async fn s5_an_unrelated_account_sees_nothing_and_cannot_tell_hidden_from_missing() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let w = shared(&s).await?;
    let private = create_person(&s, &w.a, "Private", Policy::default()).await?;
    let e = account(&s).await?;
    for scope in SCOPES {
        for archived in [false, true] {
            assert!(traverse(&s, &e, scope, archived, 100).await?.is_empty());
        }
    }
    let missing = id();
    for target in [&w.person, &private, &missing] {
        assert_eq!(code(detail(&s, &e, target).await.unwrap_err()), "not_found");
        assert_eq!(
            code(
                s.person_field_audiences(&e, target, false, None, 50)
                    .await
                    .unwrap_err()
            ),
            "not_found"
        );
    }
    Ok(())
}

// S6 ------------------------------------------------------------------------------------------

#[tokio::test]
async fn s6_linking_moves_ownership_but_not_who_added_the_person() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (a, b, m) = (account(&s).await?, account(&s).await?, account(&s).await?);
    let home = household(&s, &a, "Home").await?;
    join(&s, &a, &b, &home).await?;
    let p = create_person(&s, &a, "Mo", policy(vec![home_grant(&home, true)], &[])).await?;
    assert_eq!(row(&s, &a, &p).await?.added_by_you, AddedBy::Yes);
    link(&s, &a, &m, &p).await?;

    let r = row(&s, &a, &p).await?;
    assert_eq!(
        r.added_by_you,
        AddedBy::Yes,
        "linkage does not erase who added it"
    );
    assert_eq!(r.linkage, Linkage::Other);
    assert_eq!(r.linked_account_id.as_deref(), Some(m.as_str()));
    assert_eq!(r.your_access, Access::View, "the subject controls the name");
    assert!(!r.owner.you);
    assert_eq!(r.owner.account_id, m);
    assert_eq!(
        r.owner.person.as_ref().map(|p| p.id.as_str()),
        Some(p.as_str()),
        "M's own linked person, visible to A"
    );
    assert_eq!(r.audience.disclosure, Disclosure::Partial);
    assert_eq!(
        scopes_of(&s, &a, &p).await?,
        [PeopleScope::All, PeopleScope::InHome],
        "not Shared with me: A is known to have added it"
    );

    let r = row(&s, &m, &p).await?;
    assert_eq!(r.your_access, Access::Owner);
    assert_eq!(r.linkage, Linkage::You);
    assert_eq!(r.added_by_you, AddedBy::No);
    assert_eq!(r.audience.disclosure, Disclosure::Complete);
    assert_eq!(r.audience.direct_grant_count, Some(1));
    let md = detail(&s, &m, &p).await?;
    assert_eq!(md.shared_with.accounts.len(), 1);
    assert_eq!(md.shared_with.accounts[0].account_id, a);
    assert!(md.shared_with.accounts[0].can_edit);
    // M owns it but is not in Home: the household is listed without its private details.
    let h = &md.shared_with.households[0];
    assert!(!h.household.you_are_member);
    assert_eq!(
        (
            h.household.name.as_deref(),
            h.household.version,
            h.household.member_count
        ),
        (None, None, None)
    );
    assert_eq!(h.members, None);
    assert_eq!(scopes_of(&s, &m, &p).await?, [PeopleScope::All]);

    let r = row(&s, &b, &p).await?;
    assert_eq!(r.added_by_you, AddedBy::No);
    assert_eq!(r.linkage, Linkage::Other);
    assert_eq!(
        scopes_of(&s, &b, &p).await?,
        [
            PeopleScope::All,
            PeopleScope::InHome,
            PeopleScope::SharedWithMe
        ]
    );

    unlink(&s, &m, &p).await?;
    for (viewer, added) in [(&a, AddedBy::Yes), (&m, AddedBy::No), (&b, AddedBy::No)] {
        let r = row(&s, viewer, &p).await?;
        assert_eq!(r.added_by_you, added, "unlink keeps the attribution");
        assert_eq!(r.linkage, Linkage::None);
        assert_eq!(r.owner.account_id, m, "unlink keeps the transferred owner");
        assert!(
            scopes_of(&s, viewer, &p)
                .await?
                .contains(&PeopleScope::NotLinked)
        );
    }
    Ok(())
}

// S7 ------------------------------------------------------------------------------------------

#[tokio::test]
async fn s7_referencing_an_account_attributes_the_new_person_to_the_referencer() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (a, b, c, m) = (
        account(&s).await?,
        account(&s).await?,
        account(&s).await?,
        account(&s).await?,
    );
    let first = reference(&s, &a, &m).await?;
    let r = row(&s, &a, &first).await?;
    assert_eq!(r.added_by_you, AddedBy::Yes);
    assert_eq!(
        r.owner.account_id, m,
        "the referenced account owns its identity"
    );
    assert_eq!(r.linkage, Linkage::Other);
    let r = row(&s, &m, &first).await?;
    assert_eq!(
        (r.added_by_you, r.linkage, r.your_access),
        (AddedBy::No, Linkage::You, Access::Owner)
    );

    // A second referencer reaches the same identity and is not its creator.
    assert_eq!(reference(&s, &b, &m).await?, first);
    assert_eq!(row(&s, &b, &first).await?.added_by_you, AddedBy::No);
    assert!(
        ids(&s, &b, PeopleScope::SharedWithMe)
            .await?
            .contains(&first)
    );
    assert!(
        !ids(&s, &a, PeopleScope::SharedWithMe)
            .await?
            .contains(&first)
    );

    // N5: after unlink, referencing the account again creates a new person, attributed to its
    // caller; the old person keeps its attribution.
    unlink(&s, &m, &first).await?;
    let second = reference(&s, &c, &m).await?;
    assert_ne!(second, first);
    assert_eq!(row(&s, &c, &second).await?.added_by_you, AddedBy::Yes);
    assert_eq!(row(&s, &m, &second).await?.added_by_you, AddedBy::No);
    assert_eq!(row(&s, &a, &first).await?.added_by_you, AddedBy::Yes);
    assert_eq!(row(&s, &m, &first).await?.added_by_you, AddedBy::No);
    let recorded: Vec<(String, String)> =
        sqlx::query_as("SELECT person_id,added_by FROM person_attributions ORDER BY person_id")
            .fetch_all(&s.pool)
            .await?;
    let mut expected = vec![(first, a), (second, c)];
    expected.sort();
    assert_eq!(recorded, expected, "exactly one row per created person");
    Ok(())
}

// S8 ------------------------------------------------------------------------------------------

#[tokio::test]
async fn s8_unrecorded_attribution_is_unknown_and_still_shared_with_a_grantee() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (a, b) = (account(&s).await?, account(&s).await?);
    let home = household(&s, &a, "Home").await?;
    join(&s, &a, &b, &home).await?;
    let p = create_person(&s, &a, "Legacy", policy(vec![home_grant(&home, true)], &[])).await?;
    let before = (
        s.person_detail(&a, &p).await?.person,
        s.person_detail(&b, &p).await?.person,
    );
    forget_attribution(&s, &p).await?;
    assert_eq!(row(&s, &a, &p).await?.added_by_you, AddedBy::Unknown);
    assert_eq!(row(&s, &b, &p).await?.added_by_you, AddedBy::Unknown);
    assert!(
        !ids(&s, &a, PeopleScope::SharedWithMe).await?.contains(&p),
        "owned"
    );
    assert!(
        ids(&s, &b, PeopleScope::SharedWithMe).await?.contains(&p),
        "not known to have been added by B"
    );
    // No access changed.
    assert_eq!(
        (
            s.person_detail(&a, &p).await?.person,
            s.person_detail(&b, &p).await?.person
        ),
        before
    );
    Ok(())
}

// S9 ------------------------------------------------------------------------------------------

#[tokio::test]
async fn s9_in_home_follows_the_primary_household_only() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (a, b) = (account(&s).await?, account(&s).await?);
    let x = create_person(&s, &a, "Before any household", Policy::default()).await?;
    let none = s
        .people_summaries(&a, PeopleScope::InHome, false, None, 50)
        .await?;
    assert!(none.items.is_empty() && none.home.is_none());
    let _ = x;

    let home = household(&s, &a, "Home").await?;
    let second = household(&s, &a, "Second").await?;
    join(&s, &a, &b, &home).await?;
    join(&s, &a, &b, &second).await?;
    let in_home =
        create_person(&s, &a, "Home", policy(vec![home_grant(&home, false)], &[])).await?;
    let in_second = create_person(
        &s,
        &a,
        "Second",
        policy(vec![home_grant(&second, false)], &[]),
    )
    .await?;
    assert_eq!(
        ids(&s, &b, PeopleScope::InHome).await?,
        BTreeSet::from([in_home.clone()])
    );
    let r = row(&s, &b, &in_second).await?;
    assert!(!r.audience.in_home, "B's second household is not its home");
    assert_eq!(r.audience.households[0].id, second);

    let preferences = s.defaults(&b).await?.preferences_version;
    manage(
        &s,
        &b,
        M::SetPrimaryHousehold {
            household_id: Some(second.clone()),
            expected_version: preferences,
        },
    )
    .await?;
    assert_eq!(
        ids(&s, &b, PeopleScope::InHome).await?,
        BTreeSet::from([in_second.clone()])
    );
    let page = s
        .people_summaries(&b, PeopleScope::InHome, false, None, 50)
        .await?;
    assert_eq!(page.home.map(|h| h.name), Some("Second".to_owned()));
    assert!(!row(&s, &b, &in_home).await?.audience.in_home);

    leave(&s, &b, &second).await?;
    let page = s
        .people_summaries(&b, PeopleScope::InHome, false, None, 50)
        .await?;
    assert!(page.items.is_empty() && page.home.is_none());
    assert!(
        ids(&s, &b, PeopleScope::All).await?.contains(&in_home),
        "everything authorised stays under All"
    );
    Ok(())
}

// S10 -----------------------------------------------------------------------------------------

#[tokio::test]
async fn s10_field_counts_and_audiences_cover_only_fields_the_viewer_can_see() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (a, b, m) = (account(&s).await?, account(&s).await?, account(&s).await?);
    let home = household(&s, &a, "Home").await?;
    join(&s, &a, &b, &home).await?;
    let p = create_person(&s, &a, "Robin", policy(vec![home_grant(&home, true)], &[])).await?;
    let shared = create_field(
        &s,
        &a,
        &p,
        Some(policy(vec![home_grant(&home, false)], &[])),
    )
    .await?;
    let private = create_field(&s, &a, &p, Some(Policy::default())).await?;
    let excluding_b = create_field(
        &s,
        &a,
        &p,
        Some(policy(vec![home_grant(&home, false)], &[&b])),
    )
    .await?;
    let bs = create_field(&s, &b, &p, Some(policy(vec![home_grant(&home, true)], &[]))).await?;
    let archived = create_field(
        &s,
        &a,
        &p,
        Some(policy(vec![home_grant(&home, false)], &[])),
    )
    .await?;
    archive(&s, &a, &archived, true).await?;

    assert_eq!(row(&s, &a, &p).await?.field_count, 4);
    assert_eq!(row(&s, &b, &p).await?.field_count, 2);

    let listed = |page: atlas_core::people::FieldAudiencePage| {
        page.items
            .into_iter()
            .map(|f| (f.id, (f.owner_is_you, f.your_access, f.audience.disclosure)))
            .collect::<BTreeMap<_, _>>()
    };
    let for_b = listed(s.person_field_audiences(&b, &p, false, None, 100).await?);
    assert_eq!(
        for_b,
        BTreeMap::from([
            (shared.clone(), (false, Access::View, Disclosure::Partial)),
            (bs.clone(), (true, Access::Owner, Disclosure::Complete)),
        ])
    );
    let for_a = listed(s.person_field_audiences(&a, &p, false, None, 100).await?);
    assert_eq!(
        for_a.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([shared.clone(), private, excluding_b.clone(), bs.clone()])
    );
    assert_eq!(for_a[&bs], (false, Access::Edit, Disclosure::Partial));
    let a_page = s.person_field_audiences(&a, &p, false, None, 100).await?;
    let excl = a_page.items.iter().find(|f| f.id == excluding_b).unwrap();
    assert_eq!(excl.audience.excluded_count, Some(1));
    assert_eq!(
        listed(s.person_field_audiences(&a, &p, true, None, 100).await?)
            .into_keys()
            .collect::<Vec<_>>(),
        [archived]
    );
    // Paging the field audiences one at a time yields the same set exactly once.
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = s
            .person_field_audiences(&a, &p, false, after.as_deref(), 1)
            .await?;
        seen.extend(page.items.into_iter().map(|f| f.id));
        match page.next_after {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(seen.len(), 4);
    assert_eq!(
        seen.into_iter().collect::<BTreeSet<_>>(),
        for_a.into_keys().collect()
    );

    // PPL-6: a field A writes about a person now linked to M excludes M by default. M owns the
    // person but neither sees nor counts the field; A sees the exclusion.
    let q = create_person(&s, &a, "Mo", Policy::default()).await?;
    link(&s, &a, &m, &q).await?;
    let about = create_field(&s, &a, &q, None).await?;
    assert_eq!(row(&s, &m, &q).await?.field_count, 0);
    assert!(
        s.person_field_audiences(&m, &q, false, None, 100)
            .await?
            .items
            .is_empty()
    );
    assert_eq!(row(&s, &a, &q).await?.field_count, 1);
    let page = s.person_field_audiences(&a, &q, false, None, 100).await?;
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, about);
    assert_eq!(page.items[0].audience.excluded_count, Some(1));
    let excluded = s.resource_policy(&a, &about).await?.policy.exclude_accounts;
    assert_eq!(excluded, [m]);
    Ok(())
}

// S11 -----------------------------------------------------------------------------------------

/// What the test knows about a person it built, independently of the summary SQL.
struct Facts {
    id: String,
    owner: String,
    home_grant: bool,
    added_by: Option<String>,
}

#[tokio::test]
async fn s11_paginated_filters_are_complete_over_the_whole_authorised_set() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (v, a, m, n) = (
        account(&s).await?,
        account(&s).await?,
        account(&s).await?,
        account(&s).await?,
    );
    let home = household(&s, &a, "Home").await?;
    join(&s, &a, &v, &home).await?;
    let other = household(&s, &a, "Other").await?;
    join(&s, &a, &v, &other).await?;
    let mut facts = Vec::new();
    let mut add = |id: String, owner: &str, home_grant: bool, added_by: Option<&str>| {
        facts.push(Facts {
            id,
            owner: owner.to_owned(),
            home_grant,
            added_by: added_by.map(str::to_owned),
        })
    };
    // V's own: private, shared with Home, and V's own identity.
    add(
        create_person(&s, &v, "Own", Policy::default()).await?,
        &v,
        false,
        Some(&v),
    );
    add(
        create_person(
            &s,
            &v,
            "Own home",
            policy(vec![home_grant(&home, false)], &[]),
        )
        .await?,
        &v,
        true,
        Some(&v),
    );
    add(reference(&s, &v, &v).await?, &v, false, Some(&v));
    // A's: Home, Other, direct, private (invisible), excluding V (invisible).
    for (name, p, grant) in [
        ("A home", policy(vec![home_grant(&home, true)], &[]), true),
        (
            "A other",
            policy(vec![home_grant(&other, false)], &[]),
            false,
        ),
        (
            "A direct",
            policy(vec![account_grant(&v, false)], &[]),
            false,
        ),
        ("A private", Policy::default(), false),
        (
            "A excluding",
            policy(vec![home_grant(&home, false)], &[&v]),
            true,
        ),
        (
            "A home 2",
            policy(vec![home_grant(&home, false)], &[]),
            true,
        ),
    ] {
        add(create_person(&s, &a, name, p).await?, &a, grant, Some(&a));
    }
    // Linked to another account by V (owned by M afterwards, added by V).
    let linked = create_person(
        &s,
        &v,
        "Linked",
        policy(vec![home_grant(&home, false)], &[]),
    )
    .await?;
    link(&s, &v, &m, &linked).await?;
    add(linked, &m, true, Some(&v));
    // Linked to N by A, then unknown attribution.
    let legacy = create_person(
        &s,
        &a,
        "Legacy",
        policy(vec![home_grant(&home, false)], &[]),
    )
    .await?;
    link(&s, &a, &n, &legacy).await?;
    forget_attribution(&s, &legacy).await?;
    add(legacy, &n, true, None);
    // An archived one (only with archived=true).
    let gone = create_person(&s, &a, "Gone", policy(vec![home_grant(&home, false)], &[])).await?;
    archive(&s, &a, &gone, true).await?;
    // Twelve people in all: eleven active (two of them invisible to V) and one archived.
    assert_eq!(facts.len(), 11);

    let mut linked_ids = BTreeSet::new();
    for f in &facts {
        if s.person_detail(&v, &f.id)
            .await
            .is_ok_and(|d| d.linked_account_id.is_some())
        {
            linked_ids.insert(f.id.clone());
        }
    }
    let visible = |f: &&Facts| {
        let s = s.clone();
        let (v, id) = (v.clone(), f.id.clone());
        async move {
            s.person_detail(&v, &id)
                .await
                .is_ok_and(|d| !d.person.archived)
        }
    };
    let mut reference_sets: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for f in &facts {
        if !visible(&f).await {
            continue;
        }
        let entry = |name| (name, f.id.clone());
        let mut matches = vec![entry("all")];
        if f.home_grant {
            matches.push(entry("in_home"));
        }
        if f.owner != v && f.added_by.as_deref() != Some(v.as_str()) {
            matches.push(entry("shared_with_me"));
        }
        if !linked_ids.contains(&f.id) {
            matches.push(entry("not_linked"));
        }
        for (name, id) in matches {
            reference_sets.entry(name).or_default().insert(id);
        }
    }
    assert_eq!(reference_sets["all"].len(), 9, "{reference_sets:#?}");
    for name in ["in_home", "shared_with_me", "not_linked"] {
        let set = &reference_sets[name];
        assert!(
            !set.is_empty() && set.len() < reference_sets["all"].len(),
            "{name} is a proper, non-empty filter: {reference_sets:#?}"
        );
    }
    for (scope, name) in [
        (PeopleScope::All, "all"),
        (PeopleScope::InHome, "in_home"),
        (PeopleScope::SharedWithMe, "shared_with_me"),
        (PeopleScope::NotLinked, "not_linked"),
    ] {
        for limit in [1, 2, 3, 100] {
            let listed: Vec<String> = traverse(&s, &v, scope, false, limit)
                .await?
                .into_iter()
                .map(|p| p.id)
                .collect();
            let unique: BTreeSet<String> = listed.iter().cloned().collect();
            assert_eq!(unique.len(), listed.len(), "{name} at {limit}: duplicates");
            let mut sorted = listed.clone();
            sorted.sort();
            assert_eq!(sorted, listed, "{name} at {limit}: ascending id order");
            assert_eq!(
                unique,
                reference_sets.get(name).cloned().unwrap_or_default(),
                "{name} at {limit}"
            );
        }
    }
    assert_eq!(
        traverse(&s, &v, PeopleScope::All, true, 2)
            .await?
            .into_iter()
            .map(|p| p.id)
            .collect::<Vec<_>>(),
        [gone]
    );
    for limit in [0, 101, 200] {
        assert_eq!(
            code(
                s.people_summaries(&v, PeopleScope::All, false, None, limit)
                    .await
                    .unwrap_err()
            ),
            "invalid_value",
            "{limit}"
        );
        assert_eq!(
            code(
                s.person_field_audiences(&v, &facts[0].id, false, None, limit)
                    .await
                    .unwrap_err()
            ),
            "invalid_value",
            "{limit}"
        );
    }
    for after in ["not-a-uuid", "00000000-0000-4000-8000-00000000000G"] {
        assert_eq!(
            code(
                s.people_summaries(&v, PeopleScope::All, false, Some(after), 50)
                    .await
                    .unwrap_err()
            ),
            "invalid_value"
        );
    }
    Ok(())
}

// S12 -----------------------------------------------------------------------------------------

#[tokio::test]
async fn s12_revocation_and_policy_changes_apply_on_the_next_read() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (a, b, c, d) = (
        account(&s).await?,
        account(&s).await?,
        account(&s).await?,
        account(&s).await?,
    );
    let home = household(&s, &a, "Home").await?;
    join(&s, &a, &b, &home).await?;
    join(&s, &a, &c, &home).await?;
    let narrowed = create_person(
        &s,
        &a,
        "Narrowed",
        policy(
            vec![home_grant(&home, false), account_grant(&d, false)],
            &[],
        ),
    )
    .await?;
    let removed = create_person(
        &s,
        &a,
        "Removed",
        policy(vec![home_grant(&home, false)], &[]),
    )
    .await?;
    let excluded = create_person(
        &s,
        &a,
        "Excluded",
        policy(vec![home_grant(&home, false)], &[]),
    )
    .await?;
    let gone = |viewer: String, person: String| {
        let s = s.clone();
        async move {
            for scope in SCOPES {
                assert!(
                    !ids(&s, &viewer, scope).await?.contains(&person),
                    "{scope:?}"
                );
            }
            assert_eq!(
                code(detail(&s, &viewer, &person).await.unwrap_err()),
                "not_found"
            );
            assert_eq!(
                code(
                    s.person_field_audiences(&viewer, &person, false, None, 50)
                        .await
                        .unwrap_err()
                ),
                "not_found"
            );
            anyhow::Ok(())
        }
    };
    assert!(ids(&s, &b, PeopleScope::All).await?.contains(&narrowed));
    let before = row(&s, &a, &narrowed).await?.policy_version;
    replace_policy(
        &s,
        &a,
        &narrowed,
        policy(vec![account_grant(&d, false)], &[]),
    )
    .await?;
    gone(b.clone(), narrowed.clone()).await?;
    let after = row(&s, &a, &narrowed).await?;
    assert!(after.policy_version > before);
    assert!(after.audience.households.is_empty());
    assert!(ids(&s, &d, PeopleScope::All).await?.contains(&narrowed));

    assert!(ids(&s, &c, PeopleScope::InHome).await?.contains(&removed));
    let version = household_version(&s, &a, &home).await?;
    manage(
        &s,
        &a,
        M::RemoveHouseholdMember {
            household_id: home.clone(),
            account_id: c.clone(),
            expected_version: version,
        },
    )
    .await?;
    gone(c.clone(), removed.clone()).await?;
    let page = s
        .people_summaries(&c, PeopleScope::InHome, false, None, 50)
        .await?;
    assert!(
        page.home.is_none(),
        "membership loss clears the primary household"
    );

    replace_policy(
        &s,
        &a,
        &excluded,
        policy(vec![home_grant(&home, false)], &[&b]),
    )
    .await?;
    gone(b.clone(), excluded.clone()).await?;
    Ok(())
}

// S13 -----------------------------------------------------------------------------------------

#[tokio::test]
async fn s13_an_archived_person_is_listed_only_as_archived_and_comes_back_on_restore() -> Result<()>
{
    let (_dir, s) = fixture().await?;
    let a = account(&s).await?;
    let p = create_person(&s, &a, "Archived", Policy::default()).await?;
    create_field(&s, &a, &p, Some(Policy::default())).await?;
    archive(&s, &a, &p, true).await?;
    assert!(!ids(&s, &a, PeopleScope::All).await?.contains(&p));
    let archived = traverse(&s, &a, PeopleScope::All, true, 50).await?;
    assert_eq!(archived.len(), 1);
    assert!(archived[0].archived);
    assert_eq!(archived[0].field_count, 1);
    assert!(detail(&s, &a, &p).await?.summary.archived);
    archive(&s, &a, &p, false).await?;
    assert!(ids(&s, &a, PeopleScope::All).await?.contains(&p));
    assert!(
        traverse(&s, &a, PeopleScope::All, true, 50)
            .await?
            .is_empty()
    );
    Ok(())
}

// S14 -----------------------------------------------------------------------------------------

#[tokio::test]
async fn s14_a_merged_id_resolves_and_each_person_keeps_its_own_attribution() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let a = account(&s).await?;
    let source = create_person(&s, &a, "Robin", Policy::default()).await?;
    let target = create_person(&s, &a, "Robin", Policy::default()).await?;
    create_field(&s, &a, &source, Some(Policy::default())).await?;
    forget_attribution(&s, &target).await?;
    let preview = s.merge_preview(&a, &source, &target).await?;
    s.people_command(
        &a,
        &id(),
        &PeopleCommand::Merge {
            source_id: source.clone(),
            target_id: target.clone(),
            preview_token: preview.token,
            name: "Robin".into(),
        },
        NOW,
    )
    .await?;
    let d = detail(&s, &a, &source).await?;
    assert_eq!(
        d.summary.id, target,
        "the old id resolves to the canonical person"
    );
    assert_eq!(
        d.summary.added_by_you,
        AddedBy::Unknown,
        "the canonical keeps its own"
    );
    assert_eq!(d.summary.field_count, 1, "the field moved with the merge");
    let fields = s
        .person_field_audiences(&a, &source, false, None, 50)
        .await?;
    assert_eq!(fields.items.len(), 1);
    let alias = traverse(&s, &a, PeopleScope::All, true, 50).await?;
    assert_eq!(alias.len(), 1);
    assert_eq!(alias[0].id, source);
    assert_eq!(
        alias[0].added_by_you,
        AddedBy::Yes,
        "the alias keeps its own"
    );
    Ok(())
}

// S15 -----------------------------------------------------------------------------------------

/// Outcomes of the same writes for every account, by step name.
async fn write_outcomes(with_attribution: bool) -> Result<Vec<(String, String)>> {
    let (_dir, s) = fixture().await?;
    let (a, b, m) = (account(&s).await?, account(&s).await?, account(&s).await?);
    let home = household(&s, &a, "Home").await?;
    join(&s, &a, &b, &home).await?;
    let open = create_person(&s, &a, "Open", policy(vec![home_grant(&home, true)], &[])).await?;
    let linked =
        create_person(&s, &a, "Linked", policy(vec![home_grant(&home, true)], &[])).await?;
    link(&s, &a, &m, &linked).await?;
    if !with_attribution {
        sqlx::query("DELETE FROM person_attributions")
            .execute(&s.pool)
            .await?;
    }
    let mut outcomes = Vec::new();
    for (step, actor, person) in [
        ("creator renames linked", &a, &linked),
        ("member renames linked", &b, &linked),
        ("subject renames linked", &m, &linked),
        ("member renames open", &b, &open),
        ("creator renames open", &a, &open),
    ] {
        let p = s.person_detail(actor, person).await?.person;
        let result = s
            .apply(
                actor,
                &id(),
                &[Command::Edit {
                    id: person.clone(),
                    expected_version: p.version,
                    expected_policy_version: p.policy_version,
                    label: step.to_owned(),
                    value: String::new(),
                }],
            )
            .await;
        outcomes.push((
            step.to_owned(),
            result.map_or_else(code, |_| "ok".to_owned()),
        ));
    }
    for (name, viewer) in [("creator", &a), ("member", &b), ("subject", &m)] {
        let visible = s
            .resources(viewer, "person", None, false, None, 200)
            .await?;
        // Ids are random, so compare in label order.
        let mut seen: Vec<String> = visible
            .iter()
            .map(|p| format!("{}:{}", p.label, p.can_edit))
            .collect();
        seen.sort();
        outcomes.push((format!("{name} sees"), seen.join(",")));
    }
    Ok(outcomes)
}

#[tokio::test]
async fn s15_attribution_grants_no_access_and_changes_no_write_outcome() -> Result<()> {
    let with = write_outcomes(true).await?;
    assert_eq!(with, write_outcomes(false).await?);
    // The creator of a person now linked to someone else cannot rename it (subject name control).
    assert_eq!(
        with[0],
        ("creator renames linked".into(), "forbidden".into())
    );
    assert_eq!(with[2].1, "ok");
    Ok(())
}

// S16 -----------------------------------------------------------------------------------------

const POINT: &str = "people_summary.between_reads";

#[tokio::test]
async fn s16_one_page_is_read_from_one_snapshot() -> Result<()> {
    let (_dir, s) = fixture().await?;
    let (a, b) = (account(&s).await?, account(&s).await?);
    let home = household(&s, &a, "Home").await?;
    join(&s, &a, &b, &home).await?;
    let p = create_person(&s, &a, "Robin", policy(vec![home_grant(&home, false)], &[])).await?;
    let field = create_field(
        &s,
        &a,
        &p,
        Some(policy(vec![home_grant(&home, false)], &[])),
    )
    .await?;
    let before = row(&s, &b, &p).await?;

    let mut gate = s.hooks().arm(POINT);
    let read = {
        let (s, b) = (s.clone(), b.clone());
        tokio::spawn(async move {
            s.people_summaries(&b, PeopleScope::All, false, None, 50)
                .await
        })
    };
    gate.reached().await;
    // The owner removes B's access to the person and its field, and commits, while the read is
    // paused between its id query and its enrichment.
    tokio::time::timeout(Duration::from_secs(20), async {
        replace_policy(&s, &a, &field, Policy::default()).await?;
        replace_policy(&s, &a, &p, Policy::default()).await
    })
    .await
    .expect("the writer was blocked by the paused reader")?;
    gate.release();
    let page = read.await??;
    assert_eq!(
        page.items.len(),
        1,
        "the row the id query found is returned"
    );
    assert_eq!(
        page.items[0], before,
        "every member comes from the snapshot the read began with"
    );
    assert!(page.items[0].audience.in_home);
    assert_eq!(page.items[0].field_count, 1);
    assert!(
        ids(&s, &b, PeopleScope::All).await?.is_empty(),
        "the next read omits it"
    );
    if postgres() {
        // Every summary read in this test, the paused one included, ran at REPEATABLE READ.
        let seen = s.hooks().isolation_seen();
        assert_eq!(seen.len() as u32, s.hooks().count(POINT));
        assert!(seen.len() >= 3);
        assert!(
            seen.iter().all(|level| level == "repeatable read"),
            "{seen:?}"
        );
    }
    Ok(())
}

// S17 -----------------------------------------------------------------------------------------

/// One statement that creates a person row.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PersonSite {
    file: String,
    function: String,
    attributed: bool,
}

/// Blank out `//` comment lines so commented-out code is neither a site nor its attribution.
fn without_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("//") {
                " ".repeat(line.len())
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The top-level, comma-separated arguments of the call whose `(` is at `open`.
fn arguments(source: &str, open: usize) -> Vec<String> {
    let mut depth = 0;
    let mut args = vec![String::new()];
    for c in source[open..].chars() {
        match c {
            '(' | '[' | '{' => {
                depth += 1;
                if depth == 1 {
                    continue;
                }
            }
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            ',' if depth == 1 => {
                args.push(String::new());
                continue;
            }
            _ => {}
        }
        args.last_mut().unwrap().push(c);
    }
    args.into_iter().map(|a| a.trim().to_owned()).collect()
}

/// Whether `INSERT INTO person_attributions` follows `at` before the next match arm or function.
fn attributed_after(source: &str, at: usize) -> bool {
    let rest = &source[at..];
    let end = rest
        .lines()
        .scan(0, |offset, line| {
            let start = *offset;
            *offset += line.len() + 1;
            Some((start, line))
        })
        .skip(1)
        .find(|(_, line)| line.trim_end().ends_with("=> {") || line.contains("fn "))
        .map_or(rest.len(), |(start, _)| start);
    rest[..end].contains("INSERT INTO person_attributions")
}

/// Every source statement that creates a person: an `INSERT INTO resources(` naming `'person'`, or
/// a `new_resource(` call whose kind is `"person"`. A generic insert outside `new_resource`, or a
/// call whose kind is not a literal, cannot be classified and is an error.
fn person_sites(files: &[(String, String)]) -> Result<Vec<PersonSite>, String> {
    let mut sites = Vec::new();
    for (file, raw) in files {
        let source = without_comments(raw);
        let functions = functions(&source);
        let function_at =
            |at: usize| enclosing(&functions, at).map_or("<none>", |f| f.name.as_str());
        let mut from = 0;
        while let Some(found) = source[from..].find("INSERT INTO resources(") {
            let at = from + found;
            from = at + 1;
            let statement: String = source[at..].chars().take_while(|c| *c != '"').collect();
            if statement.contains("'person'") {
                sites.push(PersonSite {
                    file: file.clone(),
                    function: function_at(at).to_owned(),
                    attributed: attributed_after(&source, at),
                });
            } else if !statement.contains('\'') && function_at(at) != "new_resource" {
                return Err(format!(
                    "{file}::{}: a resource insert whose kind is not a literal",
                    function_at(at)
                ));
            }
        }
        let mut from = 0;
        while let Some(found) = source[from..].find("new_resource(") {
            let at = from + found;
            from = at + 1;
            if source[..at].ends_with("fn ") {
                continue;
            }
            let kind = arguments(&source, at + "new_resource".len())
                .get(4)
                .cloned()
                .unwrap_or_default();
            if kind == "\"person\"" {
                sites.push(PersonSite {
                    file: file.clone(),
                    function: function_at(at).to_owned(),
                    attributed: attributed_after(&source, at),
                });
            } else if !(kind.starts_with('"') && kind.ends_with('"')) {
                return Err(format!(
                    "{file}::{}: new_resource with a non-literal kind `{kind}`",
                    function_at(at)
                ));
            }
        }
    }
    sites.sort();
    Ok(sites)
}

fn expected_sites() -> Vec<PersonSite> {
    let mut sites = vec![
        PersonSite {
            file: "core/src/resources/mod.rs".into(),
            function: "apply_guarded".into(),
            attributed: true,
        },
        PersonSite {
            file: "core/src/people/workflows.rs".into(),
            function: "people_command".into(),
            attributed: true,
        },
    ];
    sites.sort();
    sites
}

#[test]
fn s17_every_person_insert_records_attribution() {
    assert_eq!(
        person_sites(&sources()),
        Ok(expected_sites()),
        "every statement that creates a person must record who added it in the same \
         transaction (schema 1006); a new person writer needs a decision about its attribution"
    );
}

/// Negative controls: the scanner finds an unlisted or unattributed person writer.
#[test]
fn s17_the_attribution_guard_detects_unlisted_and_unattributed_writers() {
    let mut files = sources();
    files.push((
        "core/src/synthetic.rs".into(),
        "async fn sneaky(tx: &mut T) -> Result<()> {\n    Self::new_resource(\n        tx,\n        owner,\n        &id,\n        None,\n        \"person\",\n        &name,\n        \"\",\n        &policy,\n    )\n    .await?;\n    Ok(())\n}\n".into(),
    ));
    files.push((
        "core/src/synthetic_sql.rs".into(),
        "async fn raw(tx: &mut T) -> Result<()> {\n    sqlx::query(\"INSERT INTO resources(id,owner_id,kind,label,value) VALUES ($1,$2,'person',$3,'')\").execute(tx).await?;\n    Ok(())\n}\n".into(),
    ));
    let sites = person_sites(&files).unwrap();
    assert_eq!(sites.len(), 4);
    assert!(
        sites
            .iter()
            .filter(|s| s.file.starts_with("core/src/synthetic"))
            .all(|s| !s.attributed)
    );
    assert_ne!(sites, expected_sites());

    // M10's shape: the reference-account writer loses its attribution insert.
    let mut mutated = sources();
    let workflows = mutated
        .iter_mut()
        .find(|(file, _)| file == "core/src/people/workflows.rs")
        .unwrap();
    workflows.1 = workflows.1.replace(
        "INSERT INTO person_attributions",
        "INSERT INTO person_accounts",
    );
    let sites = person_sites(&mutated).unwrap();
    assert!(
        sites
            .iter()
            .any(|s| s.file == "core/src/people/workflows.rs" && !s.attributed)
    );

    // A commented-out attribution does not count.
    let mut commented = sources();
    let resources = commented
        .iter_mut()
        .find(|(file, _)| file == "core/src/resources/mod.rs")
        .unwrap();
    resources.1 = resources.1.replace(
        "\"INSERT INTO person_attributions",
        "// \"INSERT INTO person_attributions",
    );
    assert!(
        person_sites(&commented)
            .unwrap()
            .iter()
            .any(|s| s.file == "core/src/resources/mod.rs" && !s.attributed)
    );

    // A kind the scanner cannot read is refused rather than ignored.
    let mut opaque = sources();
    opaque.push((
        "core/src/opaque.rs".into(),
        "async fn opaque(tx: &mut T) -> Result<()> {\n    Self::new_resource(tx, owner, &id, None, kind, &name, \"\", &policy).await?;\n    Ok(())\n}\n".into(),
    ));
    assert!(person_sites(&opaque).is_err());
}
