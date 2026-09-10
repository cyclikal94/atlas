use anyhow::Result;
use atlas_core::{Command, Store};

use uuid::Uuid;

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

pub(crate) use crate::support::database::fixture;

pub(crate) async fn create(store: &Store, owner: &str) -> Result<String> {
    let person = id();
    store
        .apply(
            owner,
            &id(),
            &[Command::CreatePerson {
                initial_policy: None,
                id: person.clone(),
                name: "Private name".into(),
            }],
        )
        .await?;
    Ok(person)
}

pub(crate) async fn share(
    store: &Store,
    owner: &str,
    person: &str,
    recipient: &str,
    version: i64,
) -> Result<()> {
    store
        .apply(
            owner,
            &id(),
            &[Command::Grant {
                id: person.into(),
                expected_version: version,
                account_id: recipient.into(),
                edit: true,
            }],
        )
        .await?;
    Ok(())
}
