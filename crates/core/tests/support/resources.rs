use anyhow::Result;
use atlas_core::Store;

use uuid::Uuid;

pub(crate) fn id() -> String {
    Uuid::new_v4().to_string()
}

pub(crate) async fn account(store: &Store) -> Result<String> {
    let account = id();
    store
        .add_account(
            &account,
            &format!("review-{}", Uuid::new_v4().simple()),
            "unused-test-hash",
        )
        .await?;
    Ok(account)
}

pub(crate) use crate::support::database::fixture as local;
