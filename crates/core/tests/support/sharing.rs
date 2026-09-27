//! A person and a text field owned by one account and shared with two others, for tests
//! that change the sharing policy underneath a collaborator's edit.
use anyhow::Result;
use atlas_core::{
    Command, Store,
    households::ManagementCommand,
    policy::{Policy, PrincipalGrant},
};
use uuid::Uuid;

pub(crate) const NOW: i64 = 1_788_868_800;

pub(crate) fn id() -> String {
    Uuid::new_v4().to_string()
}

pub(crate) async fn account(store: &Store) -> Result<String> {
    let id = id();
    store
        .add_account(&id, &format!("u{}", id.replace('-', "")), "test-only")
        .await?;
    Ok(id)
}

/// Direct account grants, each `(account, may_edit)`.
pub(crate) fn policy(grants: &[(&str, bool)]) -> Policy {
    Policy {
        grants: grants
            .iter()
            .map(|(id, edit)| PrincipalGrant::Account {
                id: (*id).into(),
                edit: *edit,
            })
            .collect(),
        exclude_accounts: vec![],
    }
}

pub(crate) struct Shared {
    pub alice: String,
    pub bob: String,
    pub carol: String,
    pub person: String,
    pub field: String,
}

/// Alice owns both resources. Bob and carol may edit the field; bob may edit the person
/// and carol may read it, because a field is only shareable with those who see its person.
pub(crate) async fn shared(store: &Store) -> Result<Shared> {
    let alice = account(store).await?;
    let bob = account(store).await?;
    let carol = account(store).await?;
    let (person, field) = (id(), id());
    store
        .apply(
            &alice,
            &id(),
            &[
                Command::CreatePerson {
                    id: person.clone(),
                    name: "Morgan".into(),
                    initial_policy: Some(policy(&[(&bob, true), (&carol, false)])),
                },
                Command::CreateField {
                    id: field.clone(),
                    person_id: person.clone(),
                    label: "Hobby".into(),
                    value: "Surfing".into(),
                    initial_policy: Some(policy(&[(&bob, true), (&carol, true)])),
                },
            ],
        )
        .await?;
    Ok(Shared {
        alice,
        bob,
        carol,
        person,
        field,
    })
}

/// The owner replaces a policy at its current revision, as the sharing editor would.
pub(crate) async fn replace_policy(
    store: &Store,
    owner: &str,
    resource: &str,
    policy: Policy,
) -> Result<i64> {
    let version = store.resource_policy(owner, resource).await?.version;
    store
        .management(
            owner,
            &id(),
            &[ManagementCommand::ReplacePolicy {
                id: resource.into(),
                expected_version: version,
                policy,
            }],
            NOW,
        )
        .await
}

/// Committed state a rejected write must leave untouched.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Ledger {
    pub content: (i64, i64, String, String),
    pub revision: i64,
    pub receipts: i64,
    pub batches: i64,
}

pub(crate) async fn ledger(store: &Store, resource: &str) -> Result<Ledger> {
    let mut tx = store.pool.begin().await?;
    let ledger = ledger_in(&mut tx, resource).await?;
    tx.rollback().await?;
    Ok(ledger)
}

/// The same ledger as one transaction sees it, including its own uncommitted writes.
pub(crate) async fn ledger_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    resource: &str,
) -> Result<Ledger> {
    let row: (i64, i64, String, String) =
        sqlx::query_as("SELECT version,policy_version,label,value FROM resources WHERE id=$1")
            .bind(resource)
            .fetch_one(&mut **tx)
            .await?;
    let revision = sqlx::query_scalar("SELECT revision FROM sync_clock WHERE id=1")
        .fetch_one(&mut **tx)
        .await?;
    let receipts = sqlx::query_scalar("SELECT COUNT(*) FROM receipts")
        .fetch_one(&mut **tx)
        .await?;
    let batches = sqlx::query_scalar("SELECT COUNT(*) FROM sync_batches")
        .fetch_one(&mut **tx)
        .await?;
    Ok(Ledger {
        content: row,
        revision,
        receipts,
        batches,
    })
}
