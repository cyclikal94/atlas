//! The combined sharing-defaults and household-membership read (`Store::sharing_snapshot`).
//!
//! The read is correct only if the defaults, the households their revision covers and those
//! households' members come from one database snapshot, so the schedules below pause a real read
//! between its two halves, commit a real `management` writer, and require the paused read to
//! return exactly the state it began with. The negative controls prove those schedules can fail.
use crate::cases::accounts::writer_inventory::{calls, enclosing, functions, sources};
use crate::support::{
    database::{fixture, postgres},
    sharing::{NOW, account, id},
};
use anyhow::Result;
use atlas_core::{
    Command, Store,
    households::{Household, ManagementCommand as M},
    policy::{DefaultTemplate, Policy, PrincipalGrant, SharingSnapshot},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

const POINT: &str = "sharing_snapshot.between_reads";

/// Fixed identifiers, so the revision below is a function of the state alone.
const ALICE: &str = "00000000-0000-4000-8000-0000000000a1";
const BOB: &str = "00000000-0000-4000-8000-0000000000b2";
const HOME: &str = "00000000-0000-4000-8000-000000000e01";
const FLAT: &str = "00000000-0000-4000-8000-000000000e02";

async fn manage(store: &Store, actor: &str, command: M) -> Result<i64> {
    store.management(actor, &id(), &[command], NOW).await
}

async fn household_version(store: &Store, actor: &str, household: &str) -> Result<i64> {
    Ok(store
        .households(actor)
        .await?
        .into_iter()
        .find(|h| h.id == household)
        .expect("a household the actor belongs to")
        .version)
}

/// `owner` invites `other` into `household`, and `other` accepts.
async fn join(store: &Store, owner: &str, other: &str, household: &str) -> Result<()> {
    let invitation = id();
    let expected_version = household_version(store, owner, household).await?;
    manage(
        store,
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
        store,
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

fn household_grant(household: &str) -> DefaultTemplate {
    DefaultTemplate::Explicit {
        policy: Policy {
            grants: vec![PrincipalGrant::Household {
                id: household.into(),
                edit: false,
            }],
            exclude_accounts: vec![],
        },
    }
}

/// Alice is the primary-household manager, has a personal template naming a household she
/// belongs to, and a retained personal template naming one she was removed from (which hashes
/// as `household:{id}:None`). Every line the revision hashes is therefore exercised.
async fn golden_state(store: &Store) -> Result<()> {
    store.add_account(ALICE, "alice-golden", "test").await?;
    store.add_account(BOB, "bob-golden", "test").await?;
    manage(
        store,
        ALICE,
        M::CreateHousehold {
            id: HOME.into(),
            name: "Home".into(),
        },
    )
    .await?;
    manage(
        store,
        BOB,
        M::CreateHousehold {
            id: FLAT.into(),
            name: "Flat".into(),
        },
    )
    .await?;
    join(store, BOB, ALICE, FLAT).await?;
    manage(
        store,
        ALICE,
        M::SetDefaults {
            household_id: None,
            resource_kind: "task".into(),
            expected_version: 0,
            template: Some(household_grant(HOME)),
        },
    )
    .await?;
    manage(
        store,
        ALICE,
        M::SetDefaults {
            household_id: None,
            resource_kind: "field".into(),
            expected_version: 0,
            template: Some(household_grant(FLAT)),
        },
    )
    .await?;
    manage(
        store,
        ALICE,
        M::SetDefaults {
            household_id: Some(HOME.into()),
            resource_kind: "list".into(),
            expected_version: 0,
            template: Some(DefaultTemplate::PrimaryHousehold { edit: true }),
        },
    )
    .await?;
    let expected_version = household_version(store, BOB, FLAT).await?;
    manage(
        store,
        BOB,
        M::RemoveHouseholdMember {
            household_id: FLAT.into(),
            account_id: ALICE.into(),
            expected_version,
        },
    )
    .await?;
    Ok(())
}

/// The revision string is stored inside queued offline drafts (`defaults_revision`), so a change
/// to how it is computed would flip every draft made before an upgrade to `defaults_changed`.
/// This constant was captured on the unmodified base (`f37ba6c`, API 0.24.0). If this test
/// fails, the formula moved: revert that change, do not update the constant.
const GOLDEN_REVISION: &str = "35354ed706a14728cdbfa50976d20b1a495a7a978a22f20bcb4301e7ea61d517";

#[tokio::test]
async fn the_defaults_revision_formula_is_unchanged() -> Result<()> {
    let (_dir, store) = fixture().await?;
    golden_state(&store).await?;
    let defaults = store.defaults(ALICE).await?;
    assert_eq!(defaults.primary_household_id.as_deref(), Some(HOME));
    assert_eq!(defaults.revision, GOLDEN_REVISION);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// A small world: alice manages HOME (her primary household) and belongs to FLAT, which bob
// manages; bob is a member of HOME with HOME as his primary household; carol holds a pending
// invitation to HOME; dave has no household.
// ---------------------------------------------------------------------------------------------

struct World {
    alice: String,
    bob: String,
    carol: String,
    dave: String,
    home: String,
    flat: String,
    /// Alice's pending invitation of carol to HOME.
    invitation: String,
}

async fn world(store: &Store) -> Result<World> {
    let (alice, bob, carol, dave) = (
        account(store).await?,
        account(store).await?,
        account(store).await?,
        account(store).await?,
    );
    let (home, flat, invitation) = (id(), id(), id());
    manage(
        store,
        &alice,
        M::CreateHousehold {
            id: home.clone(),
            name: "Home".into(),
        },
    )
    .await?;
    join(store, &alice, &bob, &home).await?;
    manage(
        store,
        &bob,
        M::CreateHousehold {
            id: flat.clone(),
            name: "Flat".into(),
        },
    )
    .await?;
    join(store, &bob, &alice, &flat).await?;
    let expected_version = household_version(store, &alice, &home).await?;
    manage(
        store,
        &alice,
        M::InviteToHousehold {
            id: invitation.clone(),
            household_id: home.clone(),
            recipient_id: carol.clone(),
            expected_version,
        },
    )
    .await?;
    Ok(World {
        alice,
        bob,
        carol,
        dave,
        home,
        flat,
        invitation,
    })
}

#[derive(Clone, Copy, Debug)]
enum Reader {
    Alice,
    Bob,
}

impl World {
    fn reader(&self, reader: Reader) -> &str {
        match reader {
            Reader::Alice => &self.alice,
            Reader::Bob => &self.bob,
        }
    }
}

/// Every kind of committed change that can move the revision or a listed household.
#[derive(Clone, Copy, Debug)]
enum Action {
    AcceptInvitation,
    RevokeInvitation,
    InviteDave,
    RemoveBob,
    PromoteBob,
    RenameHome,
    /// Household-scope `set_defaults`: HOME's `list` template names FLAT.
    HomeTemplateNamesFlat,
    /// Personal `set_defaults`: alice's `task` template names FLAT.
    PersonalTemplateNamesFlat,
    PrimaryToFlat,
}

const SCENARIOS: [(Action, Reader); 10] = [
    (Action::AcceptInvitation, Reader::Alice),
    (Action::RevokeInvitation, Reader::Alice),
    (Action::InviteDave, Reader::Alice),
    (Action::RemoveBob, Reader::Alice),
    // The caller is removed from their primary household by another manager mid-read.
    (Action::RemoveBob, Reader::Bob),
    (Action::PromoteBob, Reader::Alice),
    (Action::RenameHome, Reader::Alice),
    (Action::HomeTemplateNamesFlat, Reader::Alice),
    (Action::PersonalTemplateNamesFlat, Reader::Alice),
    (Action::PrimaryToFlat, Reader::Alice),
];

/// Runs the real `management` writer for `action` to completion.
async fn perform(store: &Store, w: &World, action: Action) -> Result<()> {
    let home_version = household_version(store, &w.alice, &w.home).await?;
    let command = match action {
        Action::AcceptInvitation => {
            return manage(
                store,
                &w.carol,
                M::RespondToHouseholdInvitation {
                    id: w.invitation.clone(),
                    expected_version: 1,
                    accept: true,
                },
            )
            .await
            .map(drop);
        }
        Action::RevokeInvitation => M::RevokeHouseholdInvitation {
            id: w.invitation.clone(),
            expected_version: 1,
        },
        Action::InviteDave => M::InviteToHousehold {
            id: id(),
            household_id: w.home.clone(),
            recipient_id: w.dave.clone(),
            expected_version: home_version,
        },
        Action::RemoveBob => M::RemoveHouseholdMember {
            household_id: w.home.clone(),
            account_id: w.bob.clone(),
            expected_version: home_version,
        },
        Action::PromoteBob => M::SetHouseholdRole {
            household_id: w.home.clone(),
            account_id: w.bob.clone(),
            expected_version: home_version,
            manager: true,
        },
        Action::RenameHome => M::RenameHousehold {
            id: w.home.clone(),
            name: "Renamed".into(),
            expected_version: home_version,
        },
        Action::HomeTemplateNamesFlat => M::SetDefaults {
            household_id: Some(w.home.clone()),
            resource_kind: "list".into(),
            expected_version: 0,
            template: Some(household_grant(&w.flat)),
        },
        Action::PersonalTemplateNamesFlat => M::SetDefaults {
            household_id: None,
            resource_kind: "task".into(),
            expected_version: 0,
            template: Some(household_grant(&w.flat)),
        },
        Action::PrimaryToFlat => M::SetPrimaryHousehold {
            household_id: Some(w.flat.clone()),
            expected_version: store.defaults(&w.alice).await?.preferences_version,
        },
    };
    manage(store, &w.alice, command).await.map(drop)
}

// ---------------------------------------------------------------------------------------------
// Reading helpers
// ---------------------------------------------------------------------------------------------

fn canonical_household(household: &Household) -> Value {
    let mut value = serde_json::to_value(household).unwrap();
    value["members"]
        .as_array_mut()
        .unwrap()
        .sort_by_key(|m| m["account_id"].as_str().unwrap().to_owned());
    value
}

fn body(snapshot: &SharingSnapshot) -> Value {
    serde_json::to_value(snapshot).unwrap()
}

fn revision(snapshot: &Value) -> &str {
    snapshot["defaults"]["revision"].as_str().unwrap()
}

fn household_ids(snapshot: &Value) -> Vec<String> {
    snapshot["households"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap().to_owned())
        .collect()
}

fn listed<'a>(snapshot: &'a Value, household: &str) -> &'a Value {
    snapshot["households"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == household)
        .unwrap_or_else(|| panic!("{household} is not listed"))
}

fn member_ids(snapshot: &Value, household: &str) -> Vec<String> {
    listed(snapshot, household)["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["account_id"].as_str().unwrap().to_owned())
        .collect()
}

/// A read that pauses between the defaults and the households, lets `write` commit, then
/// releases. The writer must finish while the read is still paused: a read that blocked writers
/// would hang here, so the wait is bounded.
async fn read_while<F>(store: &Store, reader: &str, write: F) -> Result<Result<SharingSnapshot>>
where
    F: Future<Output = Result<()>>,
{
    let mut gate = store.hooks().arm(POINT);
    let read = {
        let (store, reader) = (store.clone(), reader.to_owned());
        tokio::spawn(async move { store.sharing_snapshot(&reader).await })
    };
    gate.reached().await;
    tokio::time::timeout(Duration::from_secs(20), write)
        .await
        .expect("the writer was blocked by the paused reader")?;
    gate.release();
    Ok(read.await?)
}

// ---------------------------------------------------------------------------------------------
// F2 - content and scope
// ---------------------------------------------------------------------------------------------

/// Quiescent, the snapshot is exactly what the two separate reads return, so the shared helpers
/// cannot have drifted from `defaults` and `households`.
async fn agrees_with_the_separate_reads(store: &Store, actor: &str) -> Result<Value> {
    let snapshot = store.sharing_snapshot(actor).await?;
    assert_eq!(
        serde_json::to_value(&snapshot.defaults)?,
        serde_json::to_value(store.defaults(actor).await?)?
    );
    let separate: BTreeMap<String, Value> = store
        .households(actor)
        .await?
        .iter()
        .map(|h| (h.id.clone(), canonical_household(h)))
        .collect();
    for household in &snapshot.households {
        assert_eq!(
            Some(&canonical_household(household)),
            separate.get(&household.id),
            "{} differs from GET /households",
            household.id
        );
    }
    Ok(body(&snapshot))
}

#[tokio::test]
async fn an_account_with_no_household_gets_defaults_and_an_empty_list() -> Result<()> {
    let (_dir, store) = fixture().await?;
    let alice = account(&store).await?;
    let value = agrees_with_the_separate_reads(&store, &alice).await?;
    assert!(value["households"].as_array().unwrap().is_empty());
    assert!(value["defaults"]["primary_household_id"].is_null());
    assert_eq!(value["defaults"]["person"]["kind"], "primary_household");
    Ok(())
}

#[tokio::test]
async fn an_unknown_account_is_unauthenticated() -> Result<()> {
    let (_dir, store) = fixture().await?;
    let error = store.sharing_snapshot(&id()).await.err().unwrap();
    assert_eq!(error.to_string(), "unauthenticated");
    Ok(())
}

#[tokio::test]
async fn the_primary_household_is_listed_with_the_callers_role_and_every_member() -> Result<()> {
    let (_dir, store) = fixture().await?;
    let w = world(&store).await?;
    let alice = agrees_with_the_separate_reads(&store, &w.alice).await?;
    assert_eq!(household_ids(&alice), std::slice::from_ref(&w.home));
    let home = listed(&alice, &w.home);
    assert_eq!(home["role"], "manager");
    assert_eq!(home["name"], "Home");
    let mut expected = vec![w.alice.clone(), w.bob.clone()];
    expected.sort();
    assert_eq!(member_ids(&alice, &w.home), expected, "account-id order");
    // Pending invitations are not membership.
    assert!(!member_ids(&alice, &w.home).contains(&w.carol));

    let bob = agrees_with_the_separate_reads(&store, &w.bob).await?;
    assert_eq!(bob["defaults"]["primary_household_id"], w.home.as_str());
    assert_eq!(listed(&bob, &w.home)["role"], "member");
    // Bob belongs to FLAT too, but nothing resolved names it.
    assert_eq!(household_ids(&bob), std::slice::from_ref(&w.home));
    Ok(())
}

#[tokio::test]
async fn a_household_named_by_a_template_is_listed_once_however_often_it_is_named() -> Result<()> {
    let (_dir, store) = fixture().await?;
    let w = world(&store).await?;
    for kind in ["task", "list", "field"] {
        manage(
            &store,
            &w.alice,
            M::SetDefaults {
                household_id: None,
                resource_kind: kind.into(),
                expected_version: 0,
                template: Some(household_grant(&w.flat)),
            },
        )
        .await?;
    }
    // A fourth names the primary household, which is listed anyway.
    manage(
        &store,
        &w.alice,
        M::SetDefaults {
            household_id: None,
            resource_kind: "progress".into(),
            expected_version: 0,
            template: Some(household_grant(&w.home)),
        },
    )
    .await?;
    let value = agrees_with_the_separate_reads(&store, &w.alice).await?;
    let mut expected = vec![w.home.clone(), w.flat.clone()];
    expected.sort();
    assert_eq!(household_ids(&value), expected, "once each, in id order");
    assert_eq!(listed(&value, &w.flat)["role"], "member");
    Ok(())
}

#[tokio::test]
async fn a_household_the_caller_does_not_belong_to_is_never_listed_or_counted() -> Result<()> {
    let (_dir, store) = fixture().await?;
    let w = world(&store).await?;
    // HOME's own list template names FLAT (alice, a manager of HOME and member of FLAT, may
    // write that). Carol joins HOME and resolves it, but does not belong to FLAT.
    manage(
        &store,
        &w.alice,
        M::SetDefaults {
            household_id: Some(w.home.clone()),
            resource_kind: "list".into(),
            expected_version: 0,
            template: Some(household_grant(&w.flat)),
        },
    )
    .await?;
    perform(&store, &w, Action::AcceptInvitation).await?;
    let carol = agrees_with_the_separate_reads(&store, &w.carol).await?;
    assert_eq!(
        carol["defaults"]["list"]["policy"]["grants"][0]["id"],
        w.flat.as_str()
    );
    assert_eq!(
        household_ids(&carol),
        std::slice::from_ref(&w.home),
        "FLAT is omitted"
    );
    // Activity in FLAT leaves carol's revision alone; the same activity moves alice's.
    let alice_before =
        revision(&agrees_with_the_separate_reads(&store, &w.alice).await?).to_owned();
    let version = household_version(&store, &w.bob, &w.flat).await?;
    manage(
        &store,
        &w.bob,
        M::RenameHousehold {
            id: w.flat.clone(),
            name: "Renamed flat".into(),
            expected_version: version,
        },
    )
    .await?;
    let carol_after = agrees_with_the_separate_reads(&store, &w.carol).await?;
    assert_eq!(revision(&carol_after), revision(&carol));
    let alice_after = agrees_with_the_separate_reads(&store, &w.alice).await?;
    assert_ne!(revision(&alice_after), alice_before);
    Ok(())
}

/// At the membership cap every household is returned, in id order, and the answer is still what
/// the separate reads give.
#[tokio::test]
async fn an_account_at_the_household_cap_gets_every_household() -> Result<()> {
    let (_dir, store) = fixture().await?;
    let alice = account(&store).await?;
    let mut ids = Vec::new();
    for n in 0..32 {
        let household = id();
        manage(
            &store,
            &alice,
            M::CreateHousehold {
                id: household.clone(),
                name: format!("Household {n}"),
            },
        )
        .await?;
        ids.push(household);
    }
    ids.sort();
    // Five templates cannot name 32 households one by one; one policy can name all of them.
    let all = DefaultTemplate::Explicit {
        policy: Policy {
            grants: ids
                .iter()
                .map(|id| PrincipalGrant::Household {
                    id: id.clone(),
                    edit: false,
                })
                .collect(),
            exclude_accounts: vec![],
        },
    };
    manage(
        &store,
        &alice,
        M::SetDefaults {
            household_id: None,
            resource_kind: "task".into(),
            expected_version: 0,
            template: Some(all),
        },
    )
    .await?;
    let value = agrees_with_the_separate_reads(&store, &alice).await?;
    assert_eq!(household_ids(&value), ids);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// F3 - the former-member oracle
// ---------------------------------------------------------------------------------------------

/// A retained personal template still names a household its author was removed from. Neither the
/// listing nor the revision may reveal what happens there afterwards.
#[tokio::test]
async fn a_former_member_learns_nothing_about_the_household_from_the_snapshot() -> Result<()> {
    let (_dir, store) = fixture().await?;
    let w = world(&store).await?;
    manage(
        &store,
        &w.bob,
        M::SetDefaults {
            household_id: None,
            resource_kind: "person".into(),
            expected_version: 0,
            template: Some(household_grant(&w.home)),
        },
    )
    .await?;
    let version = household_version(&store, &w.alice, &w.home).await?;
    manage(
        &store,
        &w.alice,
        M::RemoveHouseholdMember {
            household_id: w.home.clone(),
            account_id: w.bob.clone(),
            expected_version: version,
        },
    )
    .await?;
    let before = agrees_with_the_separate_reads(&store, &w.bob).await?;
    assert!(household_ids(&before).is_empty(), "bob no longer belongs");
    assert_eq!(
        before["defaults"]["person"]["policy"]["grants"][0]["id"],
        w.home.as_str()
    );

    // Rename, invite, accept, promote and remove, all inside the household bob left.
    perform(&store, &w, Action::RenameHome).await?;
    perform(&store, &w, Action::AcceptInvitation).await?;
    let version = household_version(&store, &w.alice, &w.home).await?;
    manage(
        &store,
        &w.alice,
        M::SetHouseholdRole {
            household_id: w.home.clone(),
            account_id: w.carol.clone(),
            expected_version: version,
            manager: true,
        },
    )
    .await?;
    perform(&store, &w, Action::InviteDave).await?;
    let version = household_version(&store, &w.alice, &w.home).await?;
    manage(
        &store,
        &w.alice,
        M::RemoveHouseholdMember {
            household_id: w.home.clone(),
            account_id: w.carol.clone(),
            expected_version: version,
        },
    )
    .await?;
    let after = agrees_with_the_separate_reads(&store, &w.bob).await?;
    assert_eq!(after, before, "the whole body, not only the revision");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// F4 - a paused read returns exactly the state it began with
// ---------------------------------------------------------------------------------------------

/// What the change must have done, checked on the read taken after it.
fn expect_change(action: Action, reader: Reader, w: &World, pre: &Value, post: &Value) {
    let home_version = |value: &Value| listed(value, &w.home)["version"].as_i64().unwrap();
    match (action, reader) {
        (Action::AcceptInvitation, _) => {
            assert!(member_ids(post, &w.home).contains(&w.carol));
        }
        (Action::RevokeInvitation | Action::InviteDave, _) => {
            // Invitations are not membership: the members are unchanged, the version is not.
            assert_eq!(member_ids(post, &w.home), member_ids(pre, &w.home));
            assert!(home_version(post) > home_version(pre));
        }
        (Action::RemoveBob, Reader::Alice) => {
            assert!(!member_ids(post, &w.home).contains(&w.bob));
        }
        (Action::RemoveBob, Reader::Bob) => {
            assert!(household_ids(post).is_empty());
            assert!(post["defaults"]["primary_household_id"].is_null());
        }
        (Action::PromoteBob, _) => {
            let home = listed(post, &w.home);
            let bob = home["members"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["account_id"] == w.bob.as_str())
                .unwrap();
            assert_eq!(bob["role"], "manager");
        }
        (Action::RenameHome, _) => assert_eq!(listed(post, &w.home)["name"], "Renamed"),
        (Action::HomeTemplateNamesFlat | Action::PersonalTemplateNamesFlat, _) => {
            let mut expected = vec![w.home.clone(), w.flat.clone()];
            expected.sort();
            assert_eq!(household_ids(post), expected, "the relevant set grew");
        }
        (Action::PrimaryToFlat, _) => {
            assert_eq!(post["defaults"]["primary_household_id"], w.flat.as_str());
            assert_eq!(household_ids(post), std::slice::from_ref(&w.flat));
        }
    }
}

#[tokio::test]
async fn a_paused_read_returns_the_state_before_a_concurrent_change() -> Result<()> {
    for (action, reader) in SCENARIOS {
        let label = format!("{action:?} read by {reader:?}");
        let (_dir, store) = fixture().await?;
        let w = world(&store).await?;
        let actor = w.reader(reader).to_owned();
        let pre = body(&store.sharing_snapshot(&actor).await?);

        let paused = read_while(&store, &actor, perform(&store, &w, action))
            .await?
            .unwrap_or_else(|e| panic!("{label}: the paused read failed: {e}"));
        assert_eq!(
            body(&paused),
            pre,
            "{label}: the paused read must not see the change"
        );

        let post = body(&store.sharing_snapshot(&actor).await?);
        assert_ne!(
            revision(&post),
            revision(&pre),
            "{label}: the change moves the revision"
        );
        expect_change(action, reader, &w, &pre, &post);
        // A read after the change agrees with the separate reads about that state.
        assert_eq!(
            post["defaults"],
            serde_json::to_value(store.defaults(&actor).await?)?,
            "{label}"
        );
        if postgres() {
            let seen = store.hooks().isolation_seen();
            assert_eq!(seen.len() as u32, store.hooks().count(POINT), "{label}");
            assert!(
                seen.iter().all(|level| level == "repeatable read"),
                "{label}: {seen:?}"
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// F5 - negative controls: the schedules can fail
// ---------------------------------------------------------------------------------------------

/// N0: what a client does today. Read the defaults, let a member be removed, read the households:
/// the audience shown under the first revision belongs to the second state.
#[tokio::test]
async fn two_separate_reads_tear_under_the_same_schedule() -> Result<()> {
    let (_dir, store) = fixture().await?;
    let w = world(&store).await?;
    let pre = body(&store.sharing_snapshot(&w.alice).await?);
    let defaults = store.defaults(&w.alice).await?;
    perform(&store, &w, Action::RemoveBob).await?;
    let households = store.households(&w.alice).await?;
    assert_eq!(
        defaults.revision,
        revision(&pre),
        "the revision is the old one"
    );
    let shown = households.iter().find(|h| h.id == w.home).unwrap();
    assert_ne!(
        canonical_household(shown),
        *listed(&pre, &w.home),
        "but the members shown with it are the new ones"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    /// The household reads use a later snapshot than the defaults (both engines).
    Split,
    /// PostgreSQL only: the whole read runs at READ COMMITTED.
    ReadCommitted,
}

/// Pauses a read that carries `fault`, removes a member, and returns what the read returned
/// alongside the states before and after.
async fn faulted_read(
    fault: Fault,
    verified: bool,
) -> Result<(Result<SharingSnapshot>, Value, Value, Vec<String>)> {
    let (_dir, store) = fixture().await?;
    let w = world(&store).await?;
    let pre = body(&store.sharing_snapshot(&w.alice).await?);
    store.hooks().verify_snapshot(verified);
    match fault {
        Fault::Split => store.hooks().split_snapshot_reads(true),
        Fault::ReadCommitted => store.hooks().snapshot_read_committed(true),
    }
    let read = read_while(&store, &w.alice, perform(&store, &w, Action::RemoveBob)).await?;
    let seen = store.hooks().isolation_seen();
    store.hooks().split_snapshot_reads(false);
    store.hooks().snapshot_read_committed(false);
    let post = body(&store.sharing_snapshot(&w.alice).await?);
    Ok((read, pre, post, seen))
}

fn faults() -> Vec<Fault> {
    let mut faults = vec![Fault::Split];
    if postgres() {
        faults.push(Fault::ReadCommitted);
    }
    faults
}

/// N1 and N2: with the consistency check off the fault returns a torn body, which proves the
/// schedule detects it; with the check on the same read fails closed and returns nothing.
#[tokio::test]
async fn a_faulted_read_tears_and_the_consistency_check_refuses_it() -> Result<()> {
    for fault in faults() {
        let (torn, pre, post, seen) = faulted_read(fault, false).await?;
        let torn = body(&torn.unwrap_or_else(|e| panic!("{fault:?}: {e}")));
        assert_eq!(
            revision(&torn),
            revision(&pre),
            "{fault:?}: the old revision..."
        );
        assert_eq!(
            torn["households"], post["households"],
            "{fault:?}: ...with the new members, which is the tear"
        );
        assert_ne!(torn["households"], pre["households"], "{fault:?}");
        if matches!(fault, Fault::ReadCommitted) {
            assert_eq!(seen, ["repeatable read", "read committed"]);
        }

        let (refused, ..) = faulted_read(fault, true).await?;
        assert_eq!(
            refused.err().unwrap().to_string(),
            "internal_error",
            "{fault:?}: a torn read must fail closed"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// F6 - unpaused stress
// ---------------------------------------------------------------------------------------------

/// Readers never wait, so this races them against a writer with no help from a hook. It can only
/// fail if a tear happens to be sampled: it is a floor, and the paused schedules above are the
/// proof. The requirement that many distinct revisions be seen stops it passing vacuously.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_readers_only_ever_see_a_revision_with_its_own_households() -> Result<()> {
    let (_dir, store) = fixture().await?;
    let w = Arc::new(world(&store).await?);
    let done = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for _ in 0..4 {
        let (store, w, done) = (store.clone(), w.clone(), done.clone());
        readers.push(tokio::spawn(async move {
            let mut seen = Vec::new();
            loop {
                let finished = done.load(Ordering::SeqCst);
                seen.push(body(&store.sharing_snapshot(&w.alice).await?));
                if finished {
                    return anyhow::Ok(seen);
                }
            }
        }));
    }
    for turn in 0..60 {
        let version = household_version(&store, &w.alice, &w.home).await?;
        manage(
            &store,
            &w.alice,
            M::SetHouseholdRole {
                household_id: w.home.clone(),
                account_id: w.bob.clone(),
                expected_version: version,
                manager: turn % 2 == 0,
            },
        )
        .await?;
    }
    done.store(true, Ordering::SeqCst);

    let mut by_revision = BTreeMap::<String, Value>::new();
    for reader in readers {
        let mut last_version = 0;
        for snapshot in reader.await?? {
            let version = listed(&snapshot, &w.home)["version"].as_i64().unwrap();
            assert!(
                version >= last_version,
                "a reader saw a household go back in time"
            );
            last_version = version;
            let primary = snapshot["defaults"]["primary_household_id"]
                .as_str()
                .unwrap();
            assert!(household_ids(&snapshot).iter().any(|id| id == primary));
            let known = by_revision
                .entry(revision(&snapshot).to_owned())
                .or_insert_with(|| snapshot["households"].clone());
            assert_eq!(
                *known, snapshot["households"],
                "one revision was returned with two different sets of households"
            );
        }
    }
    assert!(
        by_revision.len() >= 2,
        "the readers saw {} revision(s): the writer never overlapped them",
        by_revision.len()
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// F7 - the submission-time guard still uses the revision the snapshot returned
// ---------------------------------------------------------------------------------------------

fn create_person() -> Command {
    Command::CreatePerson {
        id: id(),
        name: "Morgan".into(),
        initial_policy: None,
    }
}

#[tokio::test]
async fn a_revision_from_the_snapshot_is_the_queued_create_guard() -> Result<()> {
    let (_dir, store) = fixture().await?;
    let w = world(&store).await?;
    let captured = body(&store.sharing_snapshot(&w.alice).await?);
    // The draft is created with the revision the snapshot returned, and is accepted...
    store
        .apply_with_defaults(
            &w.alice,
            &id(),
            &[create_person()],
            Some(revision(&captured)),
        )
        .await?;

    // ...until a membership change makes it stale. The rejection consumes nothing: the same
    // operation ID succeeds once the draft is re-based on a fresh snapshot.
    let operation = id();
    let draft = [create_person()];
    perform(&store, &w, Action::AcceptInvitation).await?;
    let error = store
        .apply_with_defaults(&w.alice, &operation, &draft, Some(revision(&captured)))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "defaults_changed");
    let fresh = body(&store.sharing_snapshot(&w.alice).await?);
    assert!(member_ids(&fresh, &w.home).contains(&w.carol));
    store
        .apply_with_defaults(&w.alice, &operation, &draft, Some(revision(&fresh)))
        .await?;
    Ok(())
}

/// A revision captured by a read that was overtaken by a change behaves as the state before the
/// change: it is exactly the stale revision, and never the newer one.
#[tokio::test]
async fn a_revision_from_a_read_overtaken_by_a_change_is_stale() -> Result<()> {
    let (_dir, store) = fixture().await?;
    let w = world(&store).await?;
    let paused = read_while(&store, &w.alice, perform(&store, &w, Action::RemoveBob)).await??;
    let fresh = body(&store.sharing_snapshot(&w.alice).await?);
    assert_ne!(paused.defaults.revision, revision(&fresh));
    let error = store
        .apply_with_defaults(
            &w.alice,
            &id(),
            &[create_person()],
            Some(&paused.defaults.revision),
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "defaults_changed");
    store
        .apply_with_defaults(&w.alice, &id(), &[create_person()], Some(revision(&fresh)))
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// F9 - every membership writer moves the household version, and so the revision
// ---------------------------------------------------------------------------------------------

/// The revision covers a household only through `households.version`, so each writer that
/// changes what the snapshot lists must raise it. (A primary-household or personal-template
/// change moves the revision through `preferences_version` instead.)
#[tokio::test]
async fn every_change_to_a_listed_household_raises_its_version_and_the_revision() -> Result<()> {
    for action in [
        Action::AcceptInvitation,
        Action::RevokeInvitation,
        Action::InviteDave,
        Action::RemoveBob,
        Action::PromoteBob,
        Action::RenameHome,
        Action::HomeTemplateNamesFlat,
    ] {
        let (_dir, store) = fixture().await?;
        let w = world(&store).await?;
        let before = body(&store.sharing_snapshot(&w.alice).await?);
        perform(&store, &w, action).await?;
        let after = body(&store.sharing_snapshot(&w.alice).await?);
        let version = |value: &Value| listed(value, &w.home)["version"].as_i64().unwrap();
        assert_eq!(version(&after), version(&before) + 1, "{action:?}");
        assert_ne!(revision(&after), revision(&before), "{action:?}");
    }
    Ok(())
}

#[tokio::test]
async fn creating_a_household_lists_it_at_version_one_for_its_creator() -> Result<()> {
    let (_dir, store) = fixture().await?;
    let dave = account(&store).await?;
    let before = body(&store.sharing_snapshot(&dave).await?);
    let household = id();
    manage(
        &store,
        &dave,
        M::CreateHousehold {
            id: household.clone(),
            name: "New".into(),
        },
    )
    .await?;
    let after = body(&store.sharing_snapshot(&dave).await?);
    assert_ne!(revision(&after), revision(&before));
    assert_eq!(listed(&after, &household)["version"], 1);
    assert_eq!(listed(&after, &household)["role"], "manager");
    assert_eq!(
        after["defaults"]["primary_household_id"],
        household.as_str()
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// F8 - static tripwires for what the revision's correspondence depends on
// ---------------------------------------------------------------------------------------------

const MEMBERSHIP_VERBS: [&str; 3] = ["INSERT INTO", "UPDATE", "DELETE FROM"];

/// Every `(file, function, verb)` that writes `household_memberships`, with its count.
fn membership_writers(files: &[(String, String)]) -> BTreeMap<(String, String, String), usize> {
    let mut found = BTreeMap::new();
    for (file, source) in files {
        let functions = functions(source);
        for verb in MEMBERSHIP_VERBS {
            let mut from = 0;
            while let Some(at) = source[from..].find(verb) {
                let start = from + at;
                from = start + verb.len();
                if !source[from..]
                    .trim_start()
                    .starts_with("household_memberships")
                {
                    continue;
                }
                let function = enclosing(&functions, start).map_or("<none>", |f| f.name.as_str());
                *found
                    .entry((file.clone(), function.to_owned(), verb.to_owned()))
                    .or_insert(0) += 1;
            }
        }
    }
    found
}

/// The snapshot lists a household only under the version its revision hashed, and a revision
/// moves only when a household version does. So every statement that adds, removes or
/// re-roles a member must sit in a function that also bumps `households.version`, and a new
/// writer fails this list until someone has decided that it does too. This is a tripwire at
/// function granularity; `every_change_to_a_listed_household_raises_its_version_and_the_revision`
/// and the server's onboarding case prove it for each writer.
#[test]
fn every_membership_writer_is_listed_and_raises_the_household_version() {
    let files = sources();
    let expected: BTreeMap<(String, String, String), usize> = [
        // create_household (manager), accept an invitation (member), remove, set role.
        (
            ("core/src/households.rs", "management_on", "INSERT INTO"),
            2,
        ),
        (
            ("core/src/households.rs", "management_on", "DELETE FROM"),
            1,
        ),
        (("core/src/households.rs", "management_on", "UPDATE"), 1),
        // A household signup invitation adds the new account as a member.
        (
            ("server/src/onboarding.rs", "create_account", "INSERT INTO"),
            1,
        ),
    ]
    .into_iter()
    .map(|((file, function, verb), count)| ((file.into(), function.into(), verb.into()), count))
    .collect();
    let found = membership_writers(&files);
    assert_eq!(
        found, expected,
        "The writers of `household_memberships` changed. A membership change must also bump \
         `households.version` in the same transaction, or a sharing-defaults revision will not \
         move when the audience does (docs/sharing.md). Decide that for the new writer, then \
         update this list."
    );
    for (file, function, _) in expected.keys() {
        let source = &files.iter().find(|(f, _)| f == file).unwrap().1;
        let text = &functions(source)
            .into_iter()
            .find(|(_, f)| &f.name == function)
            .unwrap()
            .1
            .text;
        assert!(
            text.contains("UPDATE households SET") && text.contains("version=version+1"),
            "{file}::{function} writes members but no longer bumps households.version"
        );
    }
}

/// The scan must be able to fail: an unlisted writer, in a function that never bumps the version,
/// is found where the real sources have none.
#[test]
fn the_membership_scan_finds_a_writer_it_was_not_told_about() {
    let sneaky = "fn sneaky(tx: &mut Tx) {\n    sqlx::query(\"DELETE FROM household_memberships WHERE account_id=$1\");\n}\n";
    let files = vec![("core/src/sneaky.rs".to_owned(), sneaky.to_owned())];
    assert_eq!(
        membership_writers(&files),
        BTreeMap::from([(
            (
                "core/src/sneaky.rs".to_owned(),
                "sneaky".to_owned(),
                "DELETE FROM".to_owned()
            ),
            1
        )])
    );
    // Not a writer: reading, and the other tables that share a prefix.
    let quiet =
        "fn quiet() { \"SELECT 1 FROM household_memberships\"; \"UPDATE households SET x=1\"; }";
    assert!(membership_writers(&[("core/src/quiet.rs".to_owned(), quiet.to_owned())]).is_empty());
}

/// Usernames are returned in `households[].members` but do not enter the revision, which is
/// safe only while they cannot change. A rename feature must decide that first.
#[test]
fn usernames_are_still_immutable() {
    for (file, source) in sources() {
        assert!(
            !source.contains("SET username"),
            "{file} changes a username: the sharing snapshot returns usernames the defaults \
             revision does not hash. Decide how a rename reaches the revision first."
        );
    }
}

/// The contract's bounds must be ones the code enforces: a household cannot have more than 100
/// members (the instance-wide account cap) and an account cannot belong to more than 32
/// households, so no truncation is needed and none is done.
#[test]
fn the_contract_bounds_are_the_ones_the_code_enforces() {
    let files = sources();
    let function = |file: &str, name: &str| -> String {
        let source = &files.iter().find(|(f, _)| f == file).unwrap().1;
        functions(source)
            .into_iter()
            .find(|(_, f)| f.name == name)
            .unwrap_or_else(|| panic!("{file} has no fn {name}"))
            .1
            .text
    };
    assert!(
        calls(
            &function("core/src/accounts.rs", "add_account_in"),
            "count < 100"
        ),
        "the instance account cap no longer bounds a household at 100 members"
    );
    assert!(
        calls(
            &function("core/src/households.rs", "household_capacity"),
            "count < 32"
        ),
        "the household cap no longer bounds a snapshot at 32 households"
    );

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../api/openapi.json");
    let contract: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let schemas = &contract["components"]["schemas"];
    assert_eq!(
        schemas["Household"]["properties"]["members"]["maxItems"],
        100
    );
    assert_eq!(
        schemas["SharingSnapshot"]["properties"]["households"]["maxItems"],
        32
    );
    assert_eq!(
        contract["paths"]["/households"]["get"]["responses"]["200"]["content"]["application/json"]
            ["schema"]["maxItems"],
        32
    );
    assert_eq!(
        contract["paths"]["/defaults/snapshot"]["get"]["responses"]["200"]["content"]["application/json"]
            ["schema"],
        json!({"$ref": "#/components/schemas/SharingSnapshot"})
    );
}
