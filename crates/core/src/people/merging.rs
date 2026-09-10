use crate::error::ErrorCode;
use crate::{
    Store, content,
    policy::{Policy, PrincipalGrant},
    receipt_digest,
};
use anyhow::{Result, ensure};
use sqlx::{Any, Transaction};
use std::collections::{BTreeMap, BTreeSet};

use super::workflows::*;
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
