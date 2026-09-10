use crate::error::ErrorCode;
use crate::{Store, identifier, policy::Policy};
use anyhow::{Result, anyhow, ensure};
use sqlx::{Any, Transaction};
use std::collections::BTreeSet;

use super::workflows::*;
impl Store {
    pub(crate) async fn owner_can_restore_visibility(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        id: &str,
    ) -> Result<bool> {
        let parent:Option<String>=sqlx::query_scalar("SELECT r.parent_id FROM resources r JOIN frozen_owner_visibility f ON f.resource_id=r.id WHERE r.id=$1 AND r.owner_id=$2").bind(id).bind(actor).fetch_optional(&mut **tx).await?.flatten();
        match parent {
            Some(parent) => Ok(Self::subset(tx, actor, &BTreeSet::from([parent]))
                .await?
                .len()
                == 1),
            None => Ok(false),
        }
    }

    pub(crate) async fn canonical_person(
        tx: &mut Transaction<'_, Any>,
        id: &str,
    ) -> Result<String> {
        identifier(id)?;
        Ok(
            sqlx::query_scalar("SELECT canonical_id FROM person_aliases WHERE source_id=$1")
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?
                .unwrap_or_else(|| id.into()),
        )
    }

    pub(crate) async fn unused_person_id(tx: &mut Transaction<'_, Any>, id: &str) -> Result<()> {
        identifier(id)?;
        ensure!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM person_aliases WHERE source_id=$1")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?
                == 0,
            ErrorCode::Conflict
        );
        Ok(())
    }

    pub(crate) async fn person_name_authority(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        id: &str,
    ) -> Result<()> {
        let linked: Option<String> =
            sqlx::query_scalar("SELECT account_id FROM person_accounts WHERE person_id=$1")
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?;
        ensure!(linked.is_none_or(|a| a == actor), ErrorCode::Forbidden);
        Ok(())
    }

    pub(crate) async fn person_field_policy(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        parent: &str,
        explicit: Option<Policy>,
        mut default: Policy,
    ) -> Result<Policy> {
        if let Some(policy) = explicit {
            return Ok(policy);
        }
        if let Some(subject) = sqlx::query_scalar::<_, String>(
            "SELECT account_id FROM person_accounts WHERE person_id=$1",
        )
        .bind(parent)
        .fetch_optional(&mut **tx)
        .await?
            && subject != actor
            && !default.exclude_accounts.contains(&subject)
        {
            default.exclude_accounts.push(subject)
        }
        Ok(default)
    }

    pub(super) async fn person_owner(tx: &mut Transaction<'_, Any>, id: &str) -> Result<String> {
        sqlx::query_scalar("SELECT owner_id FROM resources WHERE id=$1 AND kind='person'")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| anyhow!(ErrorCode::NotFound))
    }

    pub async fn person_detail(&self, actor: &str, id: &str) -> Result<PersonDetail> {
        let mut tx = self.task_read().await?;
        let canonical = Self::canonical_person(&mut tx, id).await?;
        let person = Self::task_resource(&mut tx, actor, &canonical, "person", false).await?;
        let linked_account_id =
            sqlx::query_scalar("SELECT account_id FROM person_accounts WHERE person_id=$1")
                .bind(canonical)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(PersonDetail {
            person,
            linked_account_id,
        })
    }
}
