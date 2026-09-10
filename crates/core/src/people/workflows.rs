use crate::error::ErrorCode;
use crate::{Projection, Store, content, identifier, policy::Policy, receipt_digest};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PeopleCommand {
    CancelRequest {
        id: String,
    },
    ReferenceAccount {
        account_id: String,
        person_id: String,
    },
    RequestLink {
        id: String,
        person_id: String,
        account_id: String,
        expected_version: i64,
    },
    RequestMerge {
        id: String,
        source_id: String,
        target_id: String,
        preview_token: String,
        name: String,
    },
    RespondRequest {
        id: String,
        accept: bool,
    },
    Merge {
        source_id: String,
        target_id: String,
        preview_token: String,
        name: String,
    },
    RenameProfile {
        person_id: String,
        expected_version: i64,
        name: String,
    },
    Unlink {
        person_id: String,
        expected_version: i64,
    },
}
#[derive(Debug, Serialize)]
pub struct PersonResult {
    pub revision: i64,
    pub person_id: String,
}
#[derive(Debug, Serialize)]
pub struct PersonDetail {
    pub person: Projection,
    pub linked_account_id: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct MergeField {
    pub id: String,
    pub parent_id: Option<String>,
    pub label: String,
    pub version: i64,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct MergePreview {
    pub token: String,
    pub source: Projection,
    pub target: Projection,
    pub fields: Vec<MergeField>,
    pub requires_approval: bool,
    pub combines_identity_audiences: bool,
    pub freezes_field_policies: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Proposal {
    Link {
        person_id: String,
        account_id: String,
        person_version: i64,
        policy_version: i64,
        name: String,
    },
    Merge {
        source_id: String,
        target_id: String,
        preview_token: String,
        name: String,
    },
}
#[derive(Debug, Serialize)]
pub struct PeopleRequest {
    pub id: String,
    pub sender_id: String,
    pub expires_at: i64,
    pub proposal: serde_json::Value,
}
pub(super) type Before = BTreeMap<String, BTreeMap<String, Projection>>;
impl Store {
    pub async fn people_command(
        &self,
        actor: &str,
        operation: &str,
        command: &PeopleCommand,
        now: i64,
    ) -> Result<PersonResult> {
        identifier(operation)?;
        let mut tx = self.begin_serial().await?;
        Self::epoch(&mut tx, actor).await?;
        let digest = receipt_digest(&format!(
            "people-command-v1:{}",
            serde_json::to_string(command)?
        ));
        if let Some(row)=sqlx::query("SELECT r.payload,r.revision,p.person_id FROM receipts r LEFT JOIN people_results p ON p.account_id=r.account_id AND p.operation_id=r.operation_id WHERE r.account_id=$1 AND r.operation_id=$2").bind(actor).bind(operation).fetch_optional(&mut *tx).await?{
            ensure!(row.get::<String,_>(0)==digest, ErrorCode::OperationConflict);return Ok(PersonResult{revision:row.get(1),person_id:row.get::<Option<String>,_>(2).ok_or_else(||anyhow!(ErrorCode::OperationConflict))?})
        }
        let mut extra = None;
        let person_id = match command {
            PeopleCommand::ReferenceAccount {
                account_id,
                person_id,
            } => {
                identifier(account_id)?;
                identifier(person_id)?;
                sqlx::query_scalar::<_, String>(
                    "SELECT person_id FROM person_accounts WHERE account_id=$1",
                )
                .bind(account_id)
                .fetch_optional(&mut *tx)
                .await?
                .unwrap_or_else(|| person_id.clone())
            }
            PeopleCommand::Merge {
                source_id,
                target_id,
                ..
            }
            | PeopleCommand::RequestMerge {
                source_id,
                target_id,
                ..
            } => {
                extra = Some(Self::canonical_person(&mut tx, source_id).await?);
                Self::canonical_person(&mut tx, target_id).await?
            }
            PeopleCommand::RequestLink {
                person_id,
                account_id,
                ..
            } => {
                extra =
                    sqlx::query_scalar("SELECT person_id FROM person_accounts WHERE account_id=$1")
                        .bind(account_id)
                        .fetch_optional(&mut *tx)
                        .await?;
                Self::canonical_person(&mut tx, person_id).await?
            }
            PeopleCommand::RenameProfile { person_id, .. }
            | PeopleCommand::Unlink { person_id, .. } => {
                Self::canonical_person(&mut tx, person_id).await?
            }
            PeopleCommand::CancelRequest { id } => {
                identifier(id)?;
                let payload:String=sqlx::query_scalar("SELECT payload FROM people_requests WHERE id=$1 AND sender_id=$2 AND state='pending'").bind(id).bind(actor).fetch_optional(&mut *tx).await?.ok_or_else(||anyhow!(ErrorCode::NotFound))?;
                match serde_json::from_str::<Proposal>(&payload)? {
                    Proposal::Link { person_id, .. } => person_id,
                    Proposal::Merge { target_id, .. } => target_id,
                }
            }
            PeopleCommand::RespondRequest { id, .. } => {
                identifier(id)?;
                let raw:String=sqlx::query_scalar("SELECT payload FROM people_requests WHERE id=$1 AND recipient_id=$2 AND state='pending' AND expires_at>$3").bind(id).bind(actor).bind(now).fetch_optional(&mut *tx).await?.ok_or_else(||anyhow!(ErrorCode::NotFound))?;
                match serde_json::from_str::<Proposal>(&raw)? {
                    Proposal::Link {
                        person_id,
                        account_id,
                        ..
                    } => {
                        extra = sqlx::query_scalar(
                            "SELECT person_id FROM person_accounts WHERE account_id=$1",
                        )
                        .bind(account_id)
                        .fetch_optional(&mut *tx)
                        .await?;
                        person_id
                    }
                    Proposal::Merge {
                        source_id,
                        target_id,
                        ..
                    } => {
                        extra = Some(source_id);
                        target_id
                    }
                }
            }
        };
        let mut roots = vec![person_id.as_str()];
        if let Some(extra) = &extra {
            roots.push(extra)
        }
        let (mut touched, mut accounts, mut before) = Self::people_scope(&mut tx, &roots).await?;
        accounts.insert(actor.into());
        before.entry(actor.into()).or_default();
        let mut result_person = person_id.clone();
        match command {
            PeopleCommand::CancelRequest { id } => {
                sqlx::query("UPDATE people_requests SET state='cancelled' WHERE id=$1 AND sender_id=$2 AND state='pending'").bind(id).bind(actor).execute(&mut *tx).await?;
            }

            PeopleCommand::ReferenceAccount {
                account_id,
                person_id: proposed,
            } => {
                let username: String =
                    sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
                        .bind(account_id)
                        .fetch_optional(&mut *tx)
                        .await?
                        .ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
                let exists = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM person_accounts WHERE account_id=$1",
                )
                .bind(account_id)
                .fetch_one(&mut *tx)
                .await?
                    == 1;
                if !exists {
                    Self::unused_person_id(&mut tx, &person_id).await?;
                    Self::new_resource(
                        &mut tx,
                        account_id,
                        &person_id,
                        None,
                        "person",
                        &username,
                        "",
                        &Policy::default(),
                    )
                    .await?;
                    sqlx::query("INSERT INTO person_accounts VALUES ($1,$2)")
                        .bind(&person_id)
                        .bind(account_id)
                        .execute(&mut *tx)
                        .await?;
                }
                ensure!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM resource_exclusions WHERE resource_id=$1 AND account_id=$2").bind(&person_id).bind(actor).fetch_one(&mut *tx).await?==0, ErrorCode::NotFound);
                if Self::subset(&mut tx, actor, &BTreeSet::from([person_id.clone()]))
                    .await?
                    .is_empty()
                {
                    // Referencing a public account identity must not unlock fields
                    // through a newly visible parent, even for a field's owner.
                    Self::freeze_fields(&mut tx, &touched, &before).await?;
                    sqlx::query(
                        "DELETE FROM resource_exclusions WHERE resource_id=$1 AND account_id=$2",
                    )
                    .bind(&person_id)
                    .bind(actor)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query("INSERT INTO resource_grants VALUES ($1,$2,0) ON CONFLICT(resource_id,account_id) DO NOTHING").bind(&person_id).bind(actor).execute(&mut *tx).await?;
                    sqlx::query("UPDATE resources SET policy_version=policy_version+1 WHERE id=$1")
                        .bind(&person_id)
                        .execute(&mut *tx)
                        .await?;
                }
                Self::reserve_alias(&mut tx, proposed, &person_id).await?;
            }
            PeopleCommand::RequestLink {
                id,
                account_id,
                expected_version,
                ..
            } => {
                let p = Self::task_resource(&mut tx, actor, &person_id, "person", false).await?;
                ensure!(
                    p.policy_version.is_some() && p.version == *expected_version && !p.archived,
                    ErrorCode::Conflict
                );
                ensure!(
                    sqlx::query_scalar::<_, i64>(
                        "SELECT COUNT(*) FROM person_accounts WHERE person_id=$1"
                    )
                    .bind(&person_id)
                    .fetch_one(&mut *tx)
                    .await?
                        == 0,
                    ErrorCode::Conflict
                );
                Self::propose(
                    &mut tx,
                    actor,
                    account_id,
                    id,
                    &Proposal::Link {
                        person_id: person_id.clone(),
                        account_id: account_id.clone(),
                        person_version: p.version,
                        policy_version: p.policy_version.unwrap(),
                        name: p.label,
                    },
                    now,
                )
                .await?;
            }
            PeopleCommand::Merge {
                preview_token,
                name,
                ..
            }
            | PeopleCommand::RequestMerge {
                preview_token,
                name,
                ..
            } => {
                let source = extra
                    .as_ref()
                    .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
                let preview = Self::merge_preview_in(&mut tx, actor, source, &person_id).await?;
                ensure!(preview.token == *preview_token, ErrorCode::Conflict);
                content(name, "")?;
                match command {
                    PeopleCommand::RequestMerge { id, .. } => {
                        ensure!(preview.requires_approval, ErrorCode::InvalidValue);
                        let source_owner = Self::person_owner(&mut tx, source).await?;
                        let target_owner = Self::person_owner(&mut tx, &person_id).await?;
                        let recipient = if source_owner == actor {
                            target_owner
                        } else {
                            source_owner
                        };
                        Self::propose(
                            &mut tx,
                            actor,
                            &recipient,
                            id,
                            &Proposal::Merge {
                                source_id: source.clone(),
                                target_id: person_id.clone(),
                                preview_token: preview_token.clone(),
                                name: name.clone(),
                            },
                            now,
                        )
                        .await?;
                    }
                    _ => {
                        ensure!(!preview.requires_approval, ErrorCode::Forbidden);
                        Self::merge_people_in(
                            &mut tx,
                            source,
                            &person_id,
                            name,
                            &before,
                            &mut touched,
                        )
                        .await?;
                    }
                }
            }
            PeopleCommand::RespondRequest { id, accept } => {
                let row=sqlx::query("SELECT sender_id,payload FROM people_requests WHERE id=$1 AND recipient_id=$2 AND state='pending' AND expires_at>$3").bind(id).bind(actor).bind(now).fetch_optional(&mut *tx).await?.ok_or_else(||anyhow!(ErrorCode::NotFound))?;
                let sender: String = row.get(0);
                if *accept {
                    match serde_json::from_str::<Proposal>(&row.get::<String, _>(1))? {
                        Proposal::Link {
                            person_id: source,
                            account_id,
                            person_version,
                            policy_version,
                            ..
                        } => {
                            ensure!(
                                account_id == actor
                                    && Self::person_owner(&mut tx, &source).await? == sender,
                                ErrorCode::Forbidden
                            );
                            let p = Self::task_resource(&mut tx, &sender, &source, "person", false)
                                .await?;
                            ensure!(
                                p.version == person_version
                                    && p.policy_version == Some(policy_version),
                                ErrorCode::Conflict
                            );
                            if let Some(target) = &extra {
                                ensure!(
                                    Self::person_owner(&mut tx, target).await? == actor,
                                    ErrorCode::Conflict
                                );
                                let name: String =
                                    sqlx::query_scalar("SELECT label FROM resources WHERE id=$1")
                                        .bind(target)
                                        .fetch_one(&mut *tx)
                                        .await?;
                                Self::merge_people_in(
                                    &mut tx,
                                    &source,
                                    target,
                                    &name,
                                    &before,
                                    &mut touched,
                                )
                                .await?;
                                result_person = target.clone();
                            } else {
                                Self::freeze_fields(&mut tx, &touched, &before).await?;
                                sqlx::query("INSERT INTO resource_grants VALUES ($1,$2,1) ON CONFLICT(resource_id,account_id) DO NOTHING").bind(&source).bind(&sender).execute(&mut *tx).await?;
                                sqlx::query("UPDATE resources SET owner_id=$1,policy_version=policy_version+1 WHERE id=$2").bind(actor).bind(&source).execute(&mut *tx).await?;
                                sqlx::query("INSERT INTO person_accounts VALUES ($1,$2)")
                                    .bind(&source)
                                    .bind(actor)
                                    .execute(&mut *tx)
                                    .await?;
                                Self::value(&mut tx, &source, &p.label, &p.value.to_string())
                                    .await?;
                            }
                        }
                        Proposal::Merge {
                            source_id,
                            target_id,
                            preview_token,
                            name,
                        } => {
                            let preview =
                                Self::merge_preview_in(&mut tx, &sender, &source_id, &target_id)
                                    .await?;
                            ensure!(preview.token == preview_token, ErrorCode::Conflict);
                            let owners = BTreeSet::from([
                                Self::person_owner(&mut tx, &source_id).await?,
                                Self::person_owner(&mut tx, &target_id).await?,
                            ]);
                            ensure!(
                                owners == BTreeSet::from([sender, actor.into()])
                                    && owners.len() == 2,
                                ErrorCode::Forbidden
                            );
                            Self::merge_people_in(
                                &mut tx,
                                &source_id,
                                &target_id,
                                &name,
                                &before,
                                &mut touched,
                            )
                            .await?;
                        }
                    }
                }
                sqlx::query("UPDATE people_requests SET state=$1 WHERE id=$2")
                    .bind(if *accept { "accepted" } else { "declined" })
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            PeopleCommand::RenameProfile {
                expected_version,
                name,
                ..
            } => {
                let subject: Option<String> =
                    sqlx::query_scalar("SELECT account_id FROM person_accounts WHERE person_id=$1")
                        .bind(&person_id)
                        .fetch_optional(&mut *tx)
                        .await?;
                ensure!(subject.as_deref() == Some(actor), ErrorCode::Forbidden);
                let version: i64 = sqlx::query_scalar("SELECT version FROM resources WHERE id=$1")
                    .bind(&person_id)
                    .fetch_one(&mut *tx)
                    .await?;
                ensure!(version == *expected_version, ErrorCode::Conflict);
                Self::value(&mut tx, &person_id, name, "").await?;
            }
            PeopleCommand::Unlink {
                expected_version, ..
            } => {
                let subject: Option<String> =
                    sqlx::query_scalar("SELECT account_id FROM person_accounts WHERE person_id=$1")
                        .bind(&person_id)
                        .fetch_optional(&mut *tx)
                        .await?;
                ensure!(
                    subject.as_deref() == Some(actor)
                        || Self::person_owner(&mut tx, &person_id).await? == actor,
                    ErrorCode::Forbidden
                );
                ensure!(subject.is_some(), ErrorCode::Conflict);
                let version: i64 = sqlx::query_scalar("SELECT version FROM resources WHERE id=$1")
                    .bind(&person_id)
                    .fetch_one(&mut *tx)
                    .await?;
                ensure!(version == *expected_version, ErrorCode::Conflict);
                sqlx::query("DELETE FROM person_accounts WHERE person_id=$1")
                    .bind(&person_id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("UPDATE resources SET version=version+1 WHERE id=$1")
                    .bind(&person_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        accounts.extend(Self::audience(&mut tx, &touched).await?);
        for account in &accounts {
            before.entry(account.clone()).or_default();
        }
        let revision = Self::publish(&mut tx, &accounts, &touched, &before, true).await?;
        sqlx::query(
            "INSERT INTO receipts(account_id,operation_id,payload,revision) VALUES ($1,$2,$3,$4)",
        )
        .bind(actor)
        .bind(operation)
        .bind(digest)
        .bind(revision)
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO people_results VALUES ($1,$2,$3)")
            .bind(actor)
            .bind(operation)
            .bind(&result_person)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(PersonResult {
            revision,
            person_id: result_person,
        })
    }
}
