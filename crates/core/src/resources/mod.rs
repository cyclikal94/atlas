mod fields;
use crate::error::ErrorCode;
use crate::{
    Change, Command, Notice, Projection, RESOURCE_ORDER, Store, content, identifier, policy,
    projection_row, receipt_digest, unix_now,
};
use anyhow::{Result, anyhow, ensure};
pub use fields::FieldValue;
use sqlx::{Any, Row, Transaction};
use std::collections::{BTreeMap, BTreeSet};
impl Store {
    pub async fn apply(&self, actor: &str, operation: &str, commands: &[Command]) -> Result<i64> {
        let mut tx = self.begin_serial().await?;
        let revision = Self::apply_in(&mut tx, actor, operation, commands).await?;
        tx.commit().await?;
        Ok(revision)
    }

    pub async fn apply_with_defaults(
        &self,
        actor: &str,
        operation: &str,
        commands: &[Command],
        defaults_revision: Option<&str>,
    ) -> Result<i64> {
        let mut tx = self.begin_serial().await?;
        let revision =
            Self::apply_guarded(&mut tx, actor, operation, commands, defaults_revision).await?;
        tx.commit().await?;
        Ok(revision)
    }

    /// Transaction seam used by concurrency and rollback tests. Caller must hold gate.
    pub async fn apply_in(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        operation: &str,
        commands: &[Command],
    ) -> Result<i64> {
        Self::apply_guarded(tx, actor, operation, commands, None).await
    }

    async fn apply_guarded(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        operation: &str,
        commands: &[Command],
        defaults_revision: Option<&str>,
    ) -> Result<i64> {
        identifier(operation)?;
        ensure!(
            !commands.is_empty() && commands.len() <= 20,
            ErrorCode::InvalidValue
        );
        Self::epoch(tx, actor).await?;
        let canonical = serde_json::to_string(commands)?;
        let payload = receipt_digest(&if let Some(guard) = defaults_revision {
            format!("guarded-defaults-v1\n{guard}\n{canonical}")
        } else {
            canonical
        });
        if let Some(row) = sqlx::query(
            "SELECT payload,revision,digest_version FROM receipts WHERE account_id=$1 AND operation_id=$2",
        )
        .bind(actor)
        .bind(operation)
        .fetch_optional(&mut **tx)
        .await?
        {
            ensure!(row.get::<i64, _>(2) == 1, ErrorCode::UnsupportedReceipt);
            ensure!(row.get::<String, _>(0) == payload, ErrorCode::OperationConflict);
            // Receipt contains no protected projection. Current authentication is mandatory;
            // access may have changed since execution. Never replay cached resource content.
            return Ok(row.get(1));
        }
        let mut resolved = commands.to_vec();
        for command in &mut resolved {
            match command {
                Command::CreatePerson { id, .. } => Self::unused_person_id(tx, id).await?,
                Command::CreateField { person_id, .. } => {
                    *person_id = Self::canonical_person(tx, person_id).await?
                }
                Command::Edit { id, .. }
                | Command::Grant { id, .. }
                | Command::Revoke { id, .. } => *id = Self::canonical_person(tx, id).await?,
            }
        }
        let commands = resolved.as_slice();
        let defaults = if commands.iter().any(|c| {
            matches!(
                c,
                Command::CreatePerson {
                    initial_policy: None,
                    ..
                } | Command::CreateField {
                    initial_policy: None,
                    ..
                }
            )
        }) {
            let defaults = Self::defaults_in(tx, actor).await?;
            if let Some(expected) = defaults_revision {
                ensure!(defaults.revision == expected, ErrorCode::DefaultsChanged);
            }
            Some(defaults)
        } else {
            None
        };
        // Only changed resources and their possible audiences need publication work.
        let mut touched = BTreeSet::new();
        let mut accounts = BTreeSet::from([actor.to_owned()]);
        let mut creates = 0_i64;
        for command in commands {
            let id = match command {
                Command::CreatePerson { id, .. } | Command::CreateField { id, .. } => {
                    creates += 1;
                    id
                }
                Command::Edit { id, .. } => id,
                Command::Grant { id, account_id, .. } | Command::Revoke { id, account_id, .. } => {
                    accounts.insert(account_id.clone());
                    // A person's visibility gates all of its independently shared fields.
                    let children: Vec<String> = sqlx::query_scalar(
                        "SELECT resource_id FROM resource_ancestors WHERE ancestor_id=$1",
                    )
                    .bind(id)
                    .fetch_all(&mut **tx)
                    .await?;
                    touched.extend(children);
                    id
                }
            };
            touched.insert(id.clone());
        }
        Self::related_lists(tx, &mut touched).await?;
        accounts.extend(Self::audience(tx, &touched).await?);
        if let Some(defaults) = &defaults {
            for kind in ["person", "field"] {
                accounts.extend(
                    Self::policy_audience(tx, &Self::resolved_policy(defaults, kind)).await?,
                );
            }
        }
        for command in commands {
            if let Command::CreatePerson {
                initial_policy: Some(policy),
                ..
            }
            | Command::CreateField {
                initial_policy: Some(policy),
                ..
            } = command
            {
                accounts.extend(Self::policy_audience(tx, policy).await?);
            }
        }
        let mut before = BTreeMap::new();
        for account in &accounts {
            before.insert(account.clone(), Self::subset(tx, account, &touched).await?);
        }
        let mut access_changed = false;
        for command in commands {
            match command {
                Command::CreatePerson {
                    id,
                    name,
                    initial_policy,
                } => {
                    identifier(id)?;
                    content(name, "")?;
                    sqlx::query("INSERT INTO resources(id,owner_id,kind,label,value) VALUES ($1,$2,'person',$3,'')").bind(id).bind(actor).bind(name).execute(&mut **tx).await?;
                    Self::put_policy(
                        tx,
                        actor,
                        id,
                        &initial_policy.clone().unwrap_or_else(|| {
                            Self::resolved_policy(
                                defaults.as_ref().expect("creation defaults"),
                                "person",
                            )
                        }),
                    )
                    .await?;
                }
                Command::CreateField {
                    id,
                    person_id,
                    label,
                    value,
                    initial_policy,
                } => {
                    identifier(id)?;
                    content(label, value)?;
                    let value = FieldValue::Text {
                        text: value.clone(),
                    };
                    value.validate()?;
                    let value = serde_json::to_string(&value)?;
                    let visible =
                        Self::subset(tx, actor, &BTreeSet::from([person_id.clone()])).await?;
                    ensure!(
                        visible.get(person_id).is_some_and(|p| p.kind == "person"),
                        ErrorCode::NotFound
                    );
                    sqlx::query("INSERT INTO resources(id,owner_id,parent_id,kind,label,value) VALUES ($1,$2,$3,'field',$4,$5)").bind(id).bind(actor).bind(person_id).bind(label).bind(value).execute(&mut **tx).await?;
                    let policy = Self::person_field_policy(
                        tx,
                        actor,
                        person_id,
                        initial_policy.clone(),
                        defaults
                            .as_ref()
                            .map(|d| Self::resolved_policy(d, "field"))
                            .unwrap_or_default(),
                    )
                    .await?;
                    Self::put_policy(tx, actor, id, &policy).await?;
                }
                Command::Edit {
                    id,
                    expected_version,
                    label,
                    value,
                } => {
                    content(label, value)?;
                    let visible = Self::subset(tx, actor, &BTreeSet::from([id.clone()])).await?;
                    let p = visible
                        .get(id)
                        .ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
                    ensure!(p.can_edit, ErrorCode::Forbidden);
                    Self::person_name_authority(tx, actor, id).await?;
                    ensure!(p.version == *expected_version, ErrorCode::Conflict);
                    ensure!(
                        matches!(p.kind.as_str(), "person" | "field"),
                        ErrorCode::InvalidValue
                    );
                    ensure!(
                        p.kind != "person" || value.is_empty(),
                        ErrorCode::InvalidValue
                    );
                    let value = if p.kind == "field" {
                        let existing: FieldValue = serde_json::from_value(p.value.clone())?;
                        ensure!(
                            matches!(existing, FieldValue::Text { .. }),
                            ErrorCode::InvalidValue
                        );
                        let field = FieldValue::Text {
                            text: value.clone(),
                        };
                        field.validate()?;
                        serde_json::to_string(&field)?
                    } else {
                        "null".to_owned()
                    };
                    sqlx::query(
                        "UPDATE resources SET label=$1,value=$2,version=version+1 WHERE id=$3",
                    )
                    .bind(label)
                    .bind(value)
                    .bind(id)
                    .execute(&mut **tx)
                    .await?;
                }
                Command::Grant {
                    id,
                    expected_version,
                    account_id,
                    edit,
                } => {
                    Self::manage(tx, actor, id, *expected_version).await?;
                    ensure!(
                        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM accounts WHERE id=$1")
                            .bind(account_id)
                            .fetch_one(&mut **tx)
                            .await?
                            == 1,
                        ErrorCode::NotFound
                    );
                    let parent: Option<String> =
                        sqlx::query_scalar("SELECT parent_id FROM resources WHERE id=$1")
                            .bind(id)
                            .fetch_one(&mut **tx)
                            .await?;
                    if let Some(parent) = parent {
                        ensure!(
                            Self::subset(tx, account_id, &BTreeSet::from([parent.clone()]))
                                .await?
                                .contains_key(&parent),
                            ErrorCode::IdentityGrantRequired
                        );
                    }
                    sqlx::query("INSERT INTO resource_grants VALUES ($1,$2,$3) ON CONFLICT(resource_id,account_id) DO UPDATE SET can_edit=excluded.can_edit").bind(id).bind(account_id).bind(i64::from(*edit)).execute(&mut **tx).await?;
                    sqlx::query("UPDATE resources SET policy_version=policy_version+1 WHERE id=$1")
                        .bind(id)
                        .execute(&mut **tx)
                        .await?;
                    access_changed = true;
                }
                Command::Revoke {
                    id,
                    expected_version,
                    account_id,
                } => {
                    Self::manage(tx, actor, id, *expected_version).await?;
                    sqlx::query(
                        "DELETE FROM resource_grants WHERE resource_id=$1 AND account_id=$2",
                    )
                    .bind(id)
                    .bind(account_id)
                    .execute(&mut **tx)
                    .await?;
                    sqlx::query("UPDATE resources SET policy_version=policy_version+1 WHERE id=$1")
                        .bind(id)
                        .execute(&mut **tx)
                        .await?;
                    access_changed = true;
                }
            }
        }
        if creates > 0 {
            sqlx::query("UPDATE sync_clock SET resource_count=resource_count+$1 WHERE id=1")
                .bind(creates)
                .execute(&mut **tx)
                .await?;
            let count: i64 = sqlx::query_scalar("SELECT resource_count FROM sync_clock WHERE id=1")
                .fetch_one(&mut **tx)
                .await?;
            ensure!(count <= 10000, ErrorCode::SliceCapacity);
        }
        let revision = Self::publish(tx, &accounts, &touched, &before, access_changed).await?;
        sqlx::query(
            "INSERT INTO receipts(account_id,operation_id,payload,revision) VALUES ($1,$2,$3,$4)",
        )
        .bind(actor)
        .bind(operation)
        .bind(payload)
        .bind(revision)
        .execute(&mut **tx)
        .await?;
        Ok(revision)
    }

    pub(crate) async fn publish(
        tx: &mut Transaction<'_, Any>,
        accounts: &BTreeSet<String>,
        touched: &BTreeSet<String>,
        before: &BTreeMap<String, BTreeMap<String, Projection>>,
        access_changed: bool,
    ) -> Result<i64> {
        sqlx::query("UPDATE sync_clock SET revision=revision+1 WHERE id=1")
            .execute(&mut **tx)
            .await?;
        let revision: i64 = sqlx::query_scalar("SELECT revision FROM sync_clock WHERE id=1")
            .fetch_one(&mut **tx)
            .await?;
        for account in accounts {
            let after = Self::subset(tx, account, touched).await?;
            let old = &before[account];
            let mut changes = Vec::new();
            // Full projection replacement. Identity upserts precede their fields.
            for kind in RESOURCE_ORDER {
                for p in after.values().filter(|p| p.kind == kind) {
                    if old.get(&p.id) != Some(p) {
                        changes.push(Change::Upsert {
                            resource: p.clone(),
                        });
                    }
                }
            }
            for kind in RESOURCE_ORDER.into_iter().rev() {
                for p in old.values().filter(|p| p.kind == kind) {
                    if !after.contains_key(&p.id) {
                        changes.push(Change::Remove { id: p.id.clone() });
                    }
                }
            }
            if changes.len() > 200 {
                ensure!(access_changed, ErrorCode::BatchTooLarge);
                // Large audience changes use bounded snapshot recovery instead of
                // an oversized response or rejecting a legitimate household change.
                sqlx::query(
                    "UPDATE accounts SET access_epoch=access_epoch+1,sync_floor=$1 WHERE id=$2",
                )
                .bind(revision)
                .bind(account)
                .execute(&mut **tx)
                .await?;
                continue;
            }
            if !changes.is_empty() {
                if access_changed
                    && old
                        .iter()
                        .any(|(id, p)| after.get(id).is_none_or(|new| p.can_edit != new.can_edit))
                {
                    sqlx::query("UPDATE accounts SET access_epoch=access_epoch+1 WHERE id=$1")
                        .bind(account)
                        .execute(&mut **tx)
                        .await?;
                }
                sqlx::query("INSERT INTO sync_batches(account_id,revision,payload,created_at) VALUES ($1,$2,$3,$4)")
                    .bind(account)
                    .bind(revision)
                    .bind(serde_json::to_string(&changes.iter().map(Notice::from_change).collect::<Vec<_>>())?)
                    .bind(unix_now()?)
                    .execute(&mut **tx)
                    .await?;
            }
        }
        Ok(revision)
    }

    pub(crate) async fn manage(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        id: &str,
        version: i64,
    ) -> Result<()> {
        ensure!(
            Self::subset(tx, actor, &BTreeSet::from([id.to_owned()]))
                .await?
                .contains_key(id)
                || Self::owner_can_restore_visibility(tx, actor, id).await?,
            ErrorCode::NotFound
        );
        let row = sqlx::query("SELECT owner_id,policy_version FROM resources WHERE id=$1")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
        ensure!(row.get::<String, _>(0) == actor, ErrorCode::Forbidden);
        ensure!(row.get::<i64, _>(1) == version, ErrorCode::Conflict);
        Ok(())
    }

    pub(crate) async fn epoch(tx: &mut Transaction<'_, Any>, actor: &str) -> Result<i64> {
        sqlx::query_scalar("SELECT access_epoch FROM accounts WHERE id=$1")
            .bind(actor)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| anyhow!(ErrorCode::Unauthenticated))
    }

    pub(crate) async fn subset(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        ids: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, Projection>> {
        let mut map = BTreeMap::new();
        for id in ids {
            let row = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {} FROM resources r LEFT JOIN resource_grants g ON g.resource_id=r.id AND g.account_id=$1 WHERE r.id=$2 AND {}", policy::COLUMNS, policy::VISIBLE)))
                .bind(actor).bind(id).fetch_optional(&mut **tx).await?;
            if let Some(row) = row {
                let mut p = projection_row(&row)?;
                if p.kind == "list" {
                    p.value = serde_json::from_str(&Self::list_value(tx, actor, id).await?)?;
                }
                map.insert(p.id.clone(), p);
            }
        }
        Ok(map)
    }
}
