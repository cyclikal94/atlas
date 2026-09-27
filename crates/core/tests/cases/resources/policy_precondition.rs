//! A content write commits only against the sharing policy its author last saw.
use anyhow::Result;
use atlas_core::{Change, Command, Store};

use crate::support::database::fixture;
use crate::support::projection::{policy_version, projection};
use crate::support::sharing::{account, id, ledger, policy, replace_policy, shared};

fn edit(resource: &str, version: i64, policy_version: Option<i64>, label: &str) -> Command {
    Command::Edit {
        id: resource.into(),
        expected_version: version,
        expected_policy_version: policy_version,
        label: label.into(),
        value: "Climbing".into(),
    }
}

/// A person's name is its label; its value must stay empty.
fn rename(person: &str, version: i64, policy_version: Option<i64>, name: &str) -> Command {
    Command::Edit {
        id: person.into(),
        expected_version: version,
        expected_policy_version: policy_version,
        label: name.into(),
        value: String::new(),
    }
}

async fn error(store: &Store, actor: &str, operation: &str, command: Command) -> Result<String> {
    Ok(store
        .apply(actor, operation, &[command])
        .await
        .expect_err("the write must be rejected")
        .to_string())
}

/// Readers get the revision they need to edit, never the list of who else can read.
async fn reader_sees_revision_but_no_acl(s: &Store) -> Result<()> {
    let w = shared(s).await?;
    let seen = projection(s, &w.bob, &w.field).await?;
    assert_eq!(seen.policy_version, Some(1));
    assert!(seen.can_edit);
    let page = s.sync(&w.bob, "phone", None, 200, 1000).await?;
    let text = serde_json::to_string(&page)?;
    assert!(text.contains("policy_version"));
    assert!(!text.contains(&w.carol) && !text.contains(&w.alice));
    // The same value is on the other read routes a client may use to capture it.
    let listed = s
        .resources(&w.bob, "field", Some(&w.person), false, None, 50)
        .await?;
    assert_eq!(listed[0].policy_version, Some(1));
    assert_eq!(
        s.person_detail(&w.bob, &w.person)
            .await?
            .person
            .policy_version,
        Some(1)
    );
    // The ACL itself stays with the owner.
    let denied = s.resource_policy(&w.bob, &w.field).await.err();
    assert_eq!(denied.map(|e| e.to_string()), Some("forbidden".into()));
    assert_eq!(
        s.resource_policy(&w.alice, &w.field)
            .await?
            .policy
            .grants
            .len(),
        2
    );
    Ok(())
}

/// Acceptance check 2: the retained-field race for a non-owner editor, against `Edit`.
async fn stale_edit_after_narrowing(s: &Store) -> Result<()> {
    let w = shared(s).await?;
    let seen = projection(s, &w.bob, &w.field).await?;
    // Alice stops sharing with carol from a second session; bob keeps edit access.
    replace_policy(s, &w.alice, &w.field, policy(&[(&w.bob, true)])).await?;
    let before = ledger(s, &w.field).await?;
    let operation = id();
    let stale = edit(&w.field, seen.version, seen.policy_version, "Stale");
    assert_eq!(error(s, &w.bob, &operation, stale).await?, "conflict");
    // Rejected atomically: no content, version, revision, batch or receipt.
    assert_eq!(ledger(s, &w.field).await?, before);
    let refreshed = projection(s, &w.bob, &w.field).await?;
    assert_eq!(refreshed.policy_version, Some(2));
    assert_eq!(refreshed.label, "Hobby");
    // Refetching and retrying under the same operation ID commits once.
    let current = edit(&w.field, seen.version, refreshed.policy_version, "Fresh");
    s.apply(&w.bob, &operation, std::slice::from_ref(&current))
        .await?;
    let after = projection(s, &w.bob, &w.field).await?;
    assert_eq!(
        (after.label.as_str(), after.version, after.policy_version),
        ("Fresh", 2, Some(2))
    );
    Ok(())
}

/// Widening the audience is as unsafe to write across as narrowing it.
async fn stale_edit_after_widening(s: &Store) -> Result<()> {
    let w = shared(s).await?;
    replace_policy(s, &w.alice, &w.field, policy(&[(&w.bob, true)])).await?;
    let seen = projection(s, &w.bob, &w.field).await?;
    replace_policy(
        s,
        &w.alice,
        &w.field,
        policy(&[(&w.bob, true), (&w.carol, true)]),
    )
    .await?;
    let stale = edit(&w.field, seen.version, seen.policy_version, "Stale");
    assert_eq!(error(s, &w.bob, &id(), stale).await?, "conflict");
    Ok(())
}

/// Visibility and authority are decided before the policy, so a caller learns nothing
/// about a resource they cannot see and a lost right is reported as such.
async fn precedence_of_rejections(s: &Store) -> Result<()> {
    let w = shared(s).await?;
    let seen = projection(s, &w.bob, &w.field).await?;
    let stale = || edit(&w.field, seen.version, seen.policy_version, "Stale");
    replace_policy(
        s,
        &w.alice,
        &w.field,
        policy(&[(&w.bob, false), (&w.carol, true)]),
    )
    .await?;
    assert_eq!(error(s, &w.bob, &id(), stale()).await?, "forbidden");
    replace_policy(s, &w.alice, &w.field, policy(&[(&w.carol, true)])).await?;
    assert_eq!(error(s, &w.bob, &id(), stale()).await?, "not_found");
    // An account that never saw the resource cannot use a guessed counter either.
    let outsider = account(s).await?;
    assert_eq!(
        error(s, &outsider, &id(), edit(&w.field, 1, Some(3), "Guess")).await?,
        "not_found"
    );
    Ok(())
}

/// Acceptance check 3, and the SHARE-9 invariants in both directions.
async fn owner_is_gated_and_versions_stay_independent(s: &Store) -> Result<()> {
    let w = shared(s).await?;
    let seen = projection(s, &w.alice, &w.field).await?;
    replace_policy(s, &w.alice, &w.field, policy(&[(&w.bob, true)])).await?;
    let stale = edit(&w.field, seen.version, seen.policy_version, "Stale");
    assert_eq!(error(s, &w.alice, &id(), stale).await?, "conflict");
    let current = policy_version(s, &w.alice, &w.field).await?;
    s.apply(
        &w.alice,
        &id(),
        &[edit(&w.field, seen.version, Some(current), "Owner edit")],
    )
    .await?;
    // A content edit leaves the policy counter alone, so a sharing change prepared
    // before it still applies...
    let grant = |expected| Command::Grant {
        id: w.field.clone(),
        expected_version: expected,
        account_id: w.carol.clone(),
        edit: false,
    };
    s.apply(&w.alice, &id(), &[grant(current)]).await?;
    // ...while a stale sharing change conflicts without touching content.
    let before = ledger(s, &w.field).await?;
    assert_eq!(error(s, &w.alice, &id(), grant(current)).await?, "conflict");
    assert_eq!(ledger(s, &w.field).await?, before);
    let after = projection(s, &w.alice, &w.field).await?;
    assert_eq!(
        (after.version, after.policy_version),
        (2, Some(current + 1))
    );
    Ok(())
}

/// Acceptance check 4: absence fails closed, even when nothing has changed since the
/// author read the resource and whether or not the policy is the household default.
async fn omission_is_rejected(s: &Store) -> Result<()> {
    let w = shared(s).await?;
    let untouched = projection(s, &w.bob, &w.field).await?;
    assert_eq!(untouched.policy_version, Some(1));
    for actor in [&w.alice, &w.bob] {
        let omitted = edit(&w.field, 1, None, "Omitted");
        assert_eq!(error(s, actor, &id(), omitted).await?, "conflict");
    }
    // Zero is not a revision any resource has.
    assert_eq!(
        error(s, &w.bob, &id(), edit(&w.field, 1, Some(0), "Zero")).await?,
        "conflict"
    );
    // A private resource created under the default policy is protected the same way.
    let private = id();
    s.apply(
        &w.alice,
        &id(),
        &[Command::CreatePerson {
            id: private.clone(),
            name: "Private".into(),
            initial_policy: None,
        }],
    )
    .await?;
    assert_eq!(
        error(s, &w.alice, &id(), rename(&private, 1, None, "Omitted")).await?,
        "conflict"
    );
    s.apply(&w.alice, &id(), &[rename(&private, 1, Some(1), "Present")])
        .await?;
    Ok(())
}

/// Omitted and JSON `null` decode to the same rejected value rather than a decode error,
/// and an omitted field leaves a legacy command's canonical form unchanged.
#[test]
fn wire_forms_of_the_precondition() -> Result<()> {
    let base = r#""kind":"edit","id":"x","expected_version":1,"label":"L","value":"V""#;
    for body in [
        format!("{{{base}}}"),
        format!(r#"{{{base},"expected_policy_version":null}}"#),
    ] {
        let Command::Edit {
            expected_policy_version,
            ..
        } = serde_json::from_str(&body)?
        else {
            panic!("expected an edit")
        };
        assert_eq!(expected_policy_version, None);
    }
    let legacy: Command = serde_json::from_str(&format!("{{{base}}}"))?;
    assert!(!serde_json::to_string(&legacy)?.contains("expected_policy_version"));
    let current: Command =
        serde_json::from_str(&format!(r#"{{{base},"expected_policy_version":7}}"#))?;
    assert!(serde_json::to_string(&current)?.contains(r#""expected_policy_version":7"#));
    assert!(
        serde_json::from_str::<Command>(&format!(r#"{{{base},"expected_policy_version":"7"}}"#))
            .is_err()
    );
    assert!(
        serde_json::from_str::<Command>(&format!(
            r#"{{{base},"expected_policy_version":9223372036854775808}}"#
        ))
        .is_err()
    );
    Ok(())
}

/// Receipts precede preconditions: a lost response is answered from the receipt even
/// though sharing has since changed, and a different payload under that ID is refused.
async fn replay_after_sharing_change(s: &Store) -> Result<()> {
    let w = shared(s).await?;
    let seen = projection(s, &w.bob, &w.field).await?;
    let operation = id();
    let command = edit(&w.field, seen.version, seen.policy_version, "Once");
    let revision = s
        .apply(&w.bob, &operation, std::slice::from_ref(&command))
        .await?;
    replace_policy(s, &w.alice, &w.field, policy(&[(&w.bob, true)])).await?;
    let before = ledger(s, &w.field).await?;
    assert_eq!(
        s.apply(&w.bob, &operation, std::slice::from_ref(&command))
            .await?,
        revision
    );
    assert_eq!(ledger(s, &w.field).await?, before);
    let changed = edit(&w.field, seen.version, Some(2), "Different");
    assert_eq!(
        error(s, &w.bob, &operation, changed).await?,
        "operation_conflict"
    );
    Ok(())
}

/// One stale command rejects the whole call; nothing earlier in the batch commits.
async fn stale_command_rejects_the_batch(s: &Store) -> Result<()> {
    let w = shared(s).await?;
    let field = projection(s, &w.bob, &w.field).await?;
    let person = projection(s, &w.bob, &w.person).await?;
    replace_policy(s, &w.alice, &w.field, policy(&[(&w.bob, true)])).await?;
    let before = (ledger(s, &w.person).await?, ledger(s, &w.field).await?);
    // The person edit is sound on its own; only the field command is stale.
    let sound = rename(&w.person, person.version, person.policy_version, "Renamed");
    let stale = edit(&w.field, field.version, field.policy_version, "Stale");
    assert_eq!(
        s.apply(&w.bob, &id(), &[sound.clone(), stale])
            .await
            .expect_err("the batch must be rejected")
            .to_string(),
        "conflict"
    );
    assert_eq!(
        (ledger(s, &w.person).await?, ledger(s, &w.field).await?),
        before
    );
    // The same sound command commits by itself, so the batch failed for the stale one.
    s.apply(&w.bob, &id(), &[sound]).await?;
    assert_eq!(ledger(s, &w.person).await?.content.2, "Renamed");
    Ok(())
}

/// In-process, sharing and content commands in one call evaluate in order against the
/// transaction's own state (over HTTP they are separate routes). A sharing command earlier in
/// the batch advances the revision, so a later edit built on the old one is stale.
async fn batch_order_is_respected(s: &Store) -> Result<()> {
    let w = shared(s).await?;
    let grant = |expected| Command::Grant {
        id: w.field.clone(),
        expected_version: expected,
        account_id: w.carol.clone(),
        edit: false,
    };
    // Edit first: it is checked against the revision it was built on, then the grant applies.
    s.apply(
        &w.alice,
        &id(),
        &[edit(&w.field, 1, Some(1), "First"), grant(1)],
    )
    .await?;
    let settled = ledger(s, &w.field).await?;
    assert_eq!((settled.content.0, settled.content.1), (2, 2));
    // Grant first: the revision moved within the batch, so the edit conflicts and the
    // whole batch, including the grant, is rolled back.
    let stale = [grant(2), edit(&w.field, 2, Some(2), "Second")];
    assert_eq!(
        s.apply(&w.alice, &id(), &stale)
            .await
            .expect_err("the edit is stale within the batch")
            .to_string(),
        "conflict"
    );
    assert_eq!(ledger(s, &w.field).await?, settled);
    Ok(())
}

/// A sharing change is a projection change for everyone who keeps access, so their
/// clients learn the new revision through ordinary sync, in one batch.
async fn sharing_change_reaches_readers(s: &Store) -> Result<()> {
    let w = shared(s).await?;
    let bob_page = s.sync(&w.bob, "phone", None, 200, 1000).await?;
    let carol_page = s.sync(&w.carol, "phone", None, 200, 1000).await?;
    // Carol keeps access as a reader and bob keeps edit; both hear the new revision.
    replace_policy(
        s,
        &w.alice,
        &w.field,
        policy(&[(&w.bob, true), (&w.carol, false)]),
    )
    .await?;
    for (account, page) in [(&w.bob, &bob_page), (&w.carol, &carol_page)] {
        let delta = s
            .sync(account, "phone", Some(&page.next_cursor), 200, 1001)
            .await?;
        assert_eq!(delta.batches.len(), 1);
        // Delta batches carry a changed resource's identity ahead of it, unchanged.
        let seen: Vec<_> = delta.batches[0]
            .changes
            .iter()
            .map(|c| match c {
                Change::Upsert { resource } => (
                    resource.id.clone(),
                    resource.version,
                    resource.policy_version,
                ),
                Change::Remove { id } => panic!("unexpected removal of {id}"),
            })
            .collect();
        assert_eq!(
            seen,
            [
                (w.person.clone(), 1, Some(1)),
                (w.field.clone(), 1, Some(2))
            ]
        );
    }
    // A content edit alone does not disturb the revision that other readers hold.
    let seen = projection(s, &w.bob, &w.field).await?;
    s.apply(
        &w.bob,
        &id(),
        &[edit(&w.field, seen.version, seen.policy_version, "Edit")],
    )
    .await?;
    assert_eq!(
        projection(s, &w.carol, &w.field).await?.policy_version,
        Some(2)
    );
    Ok(())
}

#[tokio::test]
async fn readers_receive_the_revision_without_the_acl() -> Result<()> {
    let (_dir, s) = fixture().await?;
    reader_sees_revision_but_no_acl(&s).await
}

#[tokio::test]
async fn stale_edit_after_narrowing_conflicts_for_a_non_owner() -> Result<()> {
    let (_dir, s) = fixture().await?;
    stale_edit_after_narrowing(&s).await
}

#[tokio::test]
async fn stale_edit_after_widening_conflicts() -> Result<()> {
    let (_dir, s) = fixture().await?;
    stale_edit_after_widening(&s).await
}

#[tokio::test]
async fn visibility_and_authority_precede_the_policy_check() -> Result<()> {
    let (_dir, s) = fixture().await?;
    precedence_of_rejections(&s).await
}

#[tokio::test]
async fn owner_writes_are_gated_and_versions_stay_independent() -> Result<()> {
    let (_dir, s) = fixture().await?;
    owner_is_gated_and_versions_stay_independent(&s).await
}

#[tokio::test]
async fn omitted_policy_version_conflicts_at_every_policy() -> Result<()> {
    let (_dir, s) = fixture().await?;
    omission_is_rejected(&s).await
}

#[tokio::test]
async fn replay_after_a_sharing_change_returns_the_original_revision() -> Result<()> {
    let (_dir, s) = fixture().await?;
    replay_after_sharing_change(&s).await
}

#[tokio::test]
async fn one_stale_command_rejects_the_whole_batch() -> Result<()> {
    let (_dir, s) = fixture().await?;
    stale_command_rejects_the_batch(&s).await
}

#[tokio::test]
async fn commands_in_one_batch_are_checked_in_order() -> Result<()> {
    let (_dir, s) = fixture().await?;
    batch_order_is_respected(&s).await
}

#[tokio::test]
async fn a_sharing_change_reaches_readers_in_one_batch() -> Result<()> {
    let (_dir, s) = fixture().await?;
    sharing_change_reaches_readers(&s).await
}
