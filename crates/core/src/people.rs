//! People workflows use the same authorised resource projections as sync.
use crate::error::ErrorCode;
mod identity;
mod merging;
mod requests;
mod workflows;
use crate::{Projection, Store};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::collections::BTreeSet;
pub use workflows::*;

#[derive(Debug, Serialize)]
pub struct DuplicatePage {
    pub items: Vec<Projection>,
    /// Cursor advances over scanned visible identities, including empty pages.
    pub next_after: Option<String>,
}
fn normalise_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
impl Store {
    /// Names are hints, never proof of identity. This deliberately does not
    /// inspect hidden fields, guess email addresses or link accounts silently.
    pub async fn person_duplicates(
        &self,
        actor: &str,
        person: &str,
        after: Option<&str>,
        limit: u16,
    ) -> Result<DuplicatePage> {
        ensure!((1..=200).contains(&limit), ErrorCode::InvalidValue);
        crate::identifier(person)?;
        if let Some(after) = after {
            crate::identifier(after)?;
        }
        let mut tx = self.task_read().await?;
        let source = Self::task_resource(&mut tx, actor, person, "person", false).await?;
        let target = normalise_name(&source.label);
        // One bounded, policy-filtered scan page. Neither the cursor nor a
        // suggestion count reveals names or identities outside the audience.
        let ids:Vec<String>=sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT r.id FROM resources r LEFT JOIN resource_grants g ON g.resource_id=r.id AND g.account_id=$1 WHERE r.kind='person' AND r.archived=0 AND ($2 IS NULL OR r.id>$2) AND {} ORDER BY r.id LIMIT $3",crate::policy::VISIBLE))).bind(actor).bind(after).bind(i64::from(limit)).fetch_all(&mut *tx).await?;
        let next_after = (ids.len() == usize::from(limit))
            .then(|| ids.last().cloned())
            .flatten();
        let visible =
            Self::subset(&mut tx, actor, &ids.into_iter().collect::<BTreeSet<_>>()).await?;
        let items = visible
            .into_values()
            .filter(|p| p.id != person && normalise_name(&p.label) == target)
            .collect();
        tx.commit().await?;
        Ok(DuplicatePage { items, next_after })
    }
}
