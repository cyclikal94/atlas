use crate::error::ErrorCode;
use crate::{
    Projection, Store, content,
    policy::{Policy, PrincipalGrant},
    receipt_digest,
};
use anyhow::{Result, anyhow, ensure};
use serde::Serialize;
use sqlx::{Any, Row, Transaction};
use std::collections::{BTreeMap, BTreeSet};

use super::workflows::*;

/// Visibility-tolerant preview computation returned to a merge recipient: requires the actor
/// to *own* one of the two canonical identities (a fresh ownership check, not a visibility
/// check), but never requires visibility of the other. Fields/identities the actor cannot
/// currently see are simply absent from the result, never a permission error.
pub(super) struct SafeMergeState {
    pub(super) token: String,
    pub(super) source: Option<Projection>,
    pub(super) target: Option<Projection>,
    pub(super) fields: Vec<MergeField>,
    pub(super) requires_approval: bool,
}

#[derive(Debug, Serialize)]
pub struct RecipientMergePreview {
    pub token: String,
    pub source: Option<Projection>,
    pub target: Option<Projection>,
    pub fields: Vec<MergeField>,
    pub requires_approval: bool,
    pub combines_identity_audiences: bool,
    pub freezes_field_policies: bool,
    /// True when the sender's own view of this merge (recomputed now) no longer matches the
    /// `preview_token` captured when the request was created — i.e. the underlying content
    /// changed since the request was made. Not an error: still returns the recipient's
    /// current-state preview so they can inspect it.
    pub stale: bool,
}

impl Store {
    /// Existence/archived check that does not require the caller to see the resource — only
    /// `merge_preview_in` (the owner/initiator path) still gates on visibility.
    async fn person_exists_unarchived(tx: &mut Transaction<'_, Any>, id: &str) -> Result<()> {
        let archived: Option<i64> =
            sqlx::query_scalar("SELECT archived FROM resources WHERE id=$1 AND kind='person'")
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?;
        ensure!(archived == Some(0), ErrorCode::NotFound);
        Ok(())
    }

    pub(super) async fn safe_merge_state(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        source: &str,
        target: &str,
    ) -> Result<SafeMergeState> {
        let source = Self::canonical_person(tx, source).await?;
        let target = Self::canonical_person(tx, target).await?;
        ensure!(source != target, ErrorCode::InvalidValue);
        Self::person_exists_unarchived(tx, &source).await?;
        Self::person_exists_unarchived(tx, &target).await?;
        let owns_source = Self::person_owner(tx, &source).await? == actor;
        let owns_target = Self::person_owner(tx, &target).await? == actor;
        ensure!(owns_source || owns_target, ErrorCode::Forbidden);
        let (touched, _, _) = Self::people_scope(tx, &[&source, &target]).await?;
        let visible = Self::subset(tx, actor, &touched).await?;
        let token = receipt_digest(&serde_json::to_string(&(
            actor, &source, &target, &visible,
        ))?);
        let fields = visible
            .values()
            .filter(|p| p.kind == "field")
            .cloned()
            .map(|p| MergeField {
                id: p.id,
                parent_id: p.parent_id,
                label: p.label,
                version: p.version,
            })
            .collect();
        Ok(SafeMergeState {
            token,
            source: visible.get(&source).cloned(),
            target: visible.get(&target).cloned(),
            fields,
            requires_approval: !(owns_source && owns_target),
        })
    }

    /// Request-authorised preview for the recipient of a pending merge request: filtered to
    /// fields the recipient can currently see, without requiring them to independently pass
    /// `merge_preview`'s direct-visibility check for the identity they don't own.
    pub async fn recipient_merge_preview(
        &self,
        actor: &str,
        request_id: &str,
        now: i64,
    ) -> Result<RecipientMergePreview> {
        let mut tx = self.task_read().await?;
        crate::identifier(request_id)?;
        let row = sqlx::query(
            "SELECT sender_id,state,expires_at,payload FROM people_requests WHERE id=$1 AND recipient_id=$2",
        )
        .bind(request_id)
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
        ensure!(row.get::<String, _>(1) == "pending", ErrorCode::NotFound);
        ensure!(row.get::<i64, _>(2) > now, ErrorCode::InvitationExpired);
        let (source_id, target_id, sender_preview_token) =
            match serde_json::from_str::<Proposal>(&row.get::<String, _>(3))? {
                Proposal::Merge {
                    source_id,
                    target_id,
                    preview_token,
                    ..
                } => (source_id, target_id, preview_token),
                Proposal::Link { .. } => return Err(anyhow!(ErrorCode::InvalidValue)),
            };
        let sender: String = row.get(0);
        let stale = match Self::merge_preview_in(&mut tx, &sender, &source_id, &target_id).await {
            Ok(current) => current.token != sender_preview_token,
            Err(_) => true,
        };
        let safe = Self::safe_merge_state(&mut tx, actor, &source_id, &target_id).await?;
        tx.commit().await?;
        Ok(RecipientMergePreview {
            token: safe.token,
            source: safe.source,
            target: safe.target,
            fields: safe.fields,
            requires_approval: safe.requires_approval,
            combines_identity_audiences: true,
            freezes_field_policies: true,
            stale,
        })
    }
}

impl Store {
    pub async fn merge_preview(
        &self,
        actor: &str,
        source: &str,
        target: &str,
    ) -> Result<MergePreview> {
        let mut tx = self.task_read().await?;
        let result = Self::merge_preview_in(&mut tx, actor, source, target).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub(super) async fn merge_preview_in(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        source: &str,
        target: &str,
    ) -> Result<MergePreview> {
        let source = Self::canonical_person(tx, source).await?;
        let target = Self::canonical_person(tx, target).await?;
        ensure!(source != target, ErrorCode::InvalidValue);
        let source = Self::task_resource(tx, actor, &source, "person", false).await?;
        let target = Self::task_resource(tx, actor, &target, "person", false).await?;
        ensure!(
            !source.archived && !target.archived,
            ErrorCode::InvalidValue
        );
        let owns_source = Self::person_owner(tx, &source.id).await? == actor;
        let owns_target = Self::person_owner(tx, &target.id).await? == actor;
        ensure!(owns_source || owns_target, ErrorCode::Forbidden);
        let (touched, _, _) = Self::people_scope(tx, &[&source.id, &target.id]).await?;
        let visible = Self::subset(tx, actor, &touched).await?;
        // Hidden contribution changes deliberately do not alter this token.
        // Their current ACLs are preserved inside the eventual transaction.
        let token = receipt_digest(&serde_json::to_string(&(
            actor, &source, &target, &visible,
        ))?);
        let fields = visible
            .into_values()
            .filter(|p| p.kind == "field")
            .map(|p| MergeField {
                id: p.id,
                parent_id: p.parent_id,
                label: p.label,
                version: p.version,
            })
            .collect();
        Ok(MergePreview {
            token,
            source,
            target,
            fields,
            requires_approval: !(owns_source && owns_target),
            combines_identity_audiences: true,
            freezes_field_policies: true,
        })
    }

    pub(super) async fn freeze_one(
        tx: &mut Transaction<'_, Any>,
        id: &str,
        before: &Before,
    ) -> Result<()> {
        let owner: String = sqlx::query_scalar("SELECT owner_id FROM resources WHERE id=$1")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM resource_grants WHERE resource_id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM resource_household_grants WHERE resource_id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM resource_exclusions WHERE resource_id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM frozen_owner_visibility WHERE resource_id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        for (account, resources) in before {
            if let Some(p) = resources.get(id)
                && account != &owner
            {
                sqlx::query("INSERT INTO resource_grants VALUES ($1,$2,$3)")
                    .bind(id)
                    .bind(account)
                    .bind(i64::from(p.can_edit))
                    .execute(&mut **tx)
                    .await?;
            }
        }
        if !before.get(&owner).is_some_and(|v| v.contains_key(id)) {
            sqlx::query("INSERT INTO frozen_owner_visibility VALUES ($1)")
                .bind(id)
                .execute(&mut **tx)
                .await?;
        }
        sqlx::query("UPDATE resources SET policy_version=policy_version+1 WHERE id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    pub(super) async fn freeze_fields(
        tx: &mut Transaction<'_, Any>,
        touched: &BTreeSet<String>,
        before: &Before,
    ) -> Result<()> {
        for id in touched {
            let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM resources WHERE id=$1")
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?;
            if kind.as_deref() == Some("field") {
                Self::freeze_one(tx, id, before).await?
            }
        }
        Ok(())
    }

    pub(super) async fn merge_people_in(
        tx: &mut Transaction<'_, Any>,
        source: &str,
        target: &str,
        name: &str,
        before: &Before,
        touched: &mut BTreeSet<String>,
    ) -> Result<()> {
        content(name, "")?;
        ensure!(source != target, ErrorCode::InvalidValue);
        let links: Vec<String> = sqlx::query_scalar(
            "SELECT account_id FROM person_accounts WHERE person_id=$1 OR person_id=$2",
        )
        .bind(source)
        .bind(target)
        .fetch_all(&mut **tx)
        .await?;
        ensure!(links.len() <= 1, ErrorCode::Conflict);
        if !links.is_empty() {
            let existing:String=sqlx::query_scalar("SELECT r.label FROM resources r JOIN person_accounts p ON p.person_id=r.id WHERE p.account_id=$1").bind(&links[0]).fetch_one(&mut **tx).await?;
            ensure!(name == existing, ErrorCode::Conflict);
        }
        Self::freeze_fields(tx, touched, before).await?;
        // The account holder keeps ownership of the canonical basic identity,
        // whichever side of the merge originally carried the account link.
        let owner = if let Some(subject) = links.first() {
            sqlx::query("UPDATE resources SET owner_id=$1 WHERE id=$2")
                .bind(subject)
                .bind(target)
                .execute(&mut **tx)
                .await?;
            subject.clone()
        } else {
            Self::person_owner(tx, target).await?
        };
        let mut identity = BTreeMap::<String, bool>::new();
        for (account, resources) in before {
            for id in [source, target] {
                if let Some(p) = resources.get(id) {
                    identity
                        .entry(account.clone())
                        .and_modify(|edit| *edit |= p.can_edit)
                        .or_insert(p.can_edit);
                }
            }
        }
        let policy = Policy {
            grants: identity
                .into_iter()
                .filter(|(a, _)| a != &owner)
                .map(|(id, edit)| PrincipalGrant::Account { id, edit })
                .collect(),
            exclude_accounts: vec![],
        };
        Self::put_policy(tx, &owner, target, &policy).await?;
        sqlx::query("UPDATE resources SET policy_version=policy_version+1 WHERE id=$1")
            .bind(target)
            .execute(&mut **tx)
            .await?;
        sqlx::query("UPDATE person_aliases SET canonical_id=$1 WHERE canonical_id=$2")
            .bind(target)
            .bind(source)
            .execute(&mut **tx)
            .await?;
        sqlx::query("INSERT INTO person_aliases VALUES ($1,$2)")
            .bind(source)
            .bind(target)
            .execute(&mut **tx)
            .await?;
        let fields: Vec<String> = sqlx::query_scalar("SELECT id FROM resources WHERE parent_id=$1")
            .bind(source)
            .fetch_all(&mut **tx)
            .await?;
        for field in fields {
            sqlx::query("UPDATE resources SET parent_id=$1,version=version+1 WHERE id=$2")
                .bind(target)
                .bind(&field)
                .execute(&mut **tx)
                .await?;
        }
        sqlx::query("UPDATE person_accounts SET person_id=$1 WHERE person_id=$2")
            .bind(target)
            .bind(source)
            .execute(&mut **tx)
            .await?;
        Self::freeze_one(tx, source, before).await?;
        Self::value(tx, target, name, "").await?;
        let old_name: String = sqlx::query_scalar("SELECT label FROM resources WHERE id=$1")
            .bind(source)
            .fetch_one(&mut **tx)
            .await?;
        Self::value(
            tx,
            source,
            &old_name,
            &serde_json::json!({"canonical_person_id":target}).to_string(),
        )
        .await?;
        sqlx::query("UPDATE resources SET archived=1 WHERE id=$1")
            .bind(source)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    pub(super) async fn reserve_alias(
        tx: &mut Transaction<'_, Any>,
        alias: &str,
        target: &str,
    ) -> Result<()> {
        if alias == target {
            return Ok(());
        }
        Self::unused_person_id(tx, alias).await?;
        ensure!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM resources WHERE id=$1")
                .bind(alias)
                .fetch_one(&mut **tx)
                .await?
                == 0,
            ErrorCode::Conflict
        );
        sqlx::query("INSERT INTO person_aliases VALUES ($1,$2)")
            .bind(alias)
            .bind(target)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
}
