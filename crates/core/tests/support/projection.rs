//! Reads what a client would hold: the projection its own account is allowed to see.
use anyhow::{Result, anyhow};
use atlas_core::{Change, Projection, Store};

/// The resource as `account` sees it in a fresh sync snapshot, as a client would capture
/// its `version` and `policy_version` before editing. Errors if the account cannot see it.
pub(crate) async fn projection(store: &Store, account: &str, id: &str) -> Result<Projection> {
    let mut page = store
        .sync(account, "projection-probe", None, 200, 1000)
        .await?;
    loop {
        for change in page.batches.iter().flat_map(|b| &b.changes) {
            if let Change::Upsert { resource } = change
                && resource.id == id
            {
                return Ok(resource.clone());
            }
        }
        if !page.has_more {
            return Err(anyhow!("resource {id} is not visible to {account}"));
        }
        page = store
            .sync(
                account,
                "projection-probe",
                Some(&page.next_cursor),
                200,
                1000,
            )
            .await?;
    }
}

/// The policy revision `account` would send as `expected_policy_version`.
pub(crate) async fn policy_version(store: &Store, account: &str, id: &str) -> Result<i64> {
    projection(store, account, id)
        .await?
        .policy_version
        .ok_or_else(|| anyhow!("resource {id} reported no policy_version to {account}"))
}
