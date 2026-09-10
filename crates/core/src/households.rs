//! Online household membership, invitations, sharing policies and default templates.
use super::*;
use crate::error::ErrorCode;
use crate::policy::{DefaultTemplate, Policy};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagementCommand {
    CreateHousehold {
        id: String,
        name: String,
    },
    RenameHousehold {
        id: String,
        name: String,
        expected_version: i64,
    },
    RevokeHouseholdInvitation {
        id: String,
        expected_version: i64,
    },
    InviteToHousehold {
        id: String,
        household_id: String,
        recipient_id: String,
        expected_version: i64,
    },
    RespondToHouseholdInvitation {
        id: String,
        expected_version: i64,
        accept: bool,
    },
    RemoveHouseholdMember {
        household_id: String,
        account_id: String,
        expected_version: i64,
    },
    SetHouseholdRole {
        household_id: String,
        account_id: String,
        expected_version: i64,
        manager: bool,
    },
    SetPrimaryHousehold {
        household_id: Option<String>,
        expected_version: i64,
    },
    SetDefaults {
        household_id: Option<String>,
        resource_kind: String,
        expected_version: i64,
        template: Option<DefaultTemplate>,
    },
    ReplacePolicy {
        id: String,
        expected_version: i64,
        policy: Policy,
    },
}

#[derive(Serialize)]
pub struct HouseholdMember {
    pub account_id: String,
    pub username: String,
    pub role: String,
}
#[derive(Serialize)]
pub struct Household {
    pub id: String,
    pub name: String,
    pub version: i64,
    pub role: String,
    pub members: Vec<HouseholdMember>,
}
#[derive(Serialize)]
pub struct Invitation {
    pub id: String,
    pub household_id: String,
    pub household_name: String,
    pub sender_id: String,
    pub status: String,
    pub expires_at: String,
    pub version: i64,
}

impl Store {
    pub(super) async fn member(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        household: &str,
        manager: bool,
    ) -> Result<()> {
        let role: Option<String> = sqlx::query_scalar(
            "SELECT role FROM household_memberships WHERE household_id=$1 AND account_id=$2",
        )
        .bind(household)
        .bind(actor)
        .fetch_optional(&mut **tx)
        .await?;
        let role = role.ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
        ensure!(!manager || role == "manager", ErrorCode::Forbidden);
        Ok(())
    }
    async fn household_version(
        tx: &mut Transaction<'_, Any>,
        household: &str,
        expected: i64,
    ) -> Result<()> {
        let version: i64 = sqlx::query_scalar("SELECT version FROM households WHERE id=$1")
            .bind(household)
            .fetch_one(&mut **tx)
            .await?;
        ensure!(version == expected, ErrorCode::Conflict);
        Ok(())
    }
    async fn household_capacity(tx: &mut Transaction<'_, Any>, account: &str) -> Result<()> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM household_memberships WHERE account_id=$1")
                .bind(account)
                .fetch_one(&mut **tx)
                .await?;
        ensure!(count < 32, ErrorCode::SliceCapacity);
        Ok(())
    }
    async fn retain_manager(
        tx: &mut Transaction<'_, Any>,
        household: &str,
        account: &str,
    ) -> Result<()> {
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM household_memberships WHERE household_id=$1 AND account_id<>$2 AND role='manager'")
            .bind(household).bind(account).fetch_one(&mut **tx).await?;
        ensure!(remaining > 0, ErrorCode::LastManager);
        Ok(())
    }
    pub async fn households(&self, actor: &str) -> Result<Vec<Household>> {
        let mut tx = self.pool.begin().await?;
        if !self.sqlite {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
        }
        let rows = sqlx::query("SELECT h.id,h.name,h.version,m.role FROM households h JOIN household_memberships m ON m.household_id=h.id WHERE m.account_id=$1 ORDER BY h.id")
            .bind(actor).fetch_all(&mut *tx).await?;
        let mut households = Vec::new();
        for row in rows {
            let id: String = row.get(0);
            let members = sqlx::query("SELECT a.id,a.username,m.role FROM household_memberships m JOIN accounts a ON a.id=m.account_id WHERE m.household_id=$1 ORDER BY a.id")
                .bind(&id).fetch_all(&mut *tx).await?.into_iter().map(|r|HouseholdMember { account_id:r.get(0), username:r.get(1), role:r.get(2) }).collect();
            households.push(Household {
                id,
                name: row.get(1),
                version: row.get(2),
                role: row.get(3),
                members,
            });
        }
        tx.commit().await?;
        Ok(households)
    }
    pub async fn invitations(&self, actor: &str) -> Result<Vec<Invitation>> {
        let rows = sqlx::query("SELECT i.id,i.household_id,h.name,i.sender_id,i.status,i.expires_at,i.version FROM household_invitations i JOIN households h ON h.id=i.household_id WHERE i.recipient_id=$1 AND i.status='pending' AND i.expires_at>$2 ORDER BY i.id LIMIT 201")
            .bind(actor).bind(unix_now()?).fetch_all(&self.pool).await?;
        ensure!(rows.len() <= 200, ErrorCode::SliceCapacity);
        rows.into_iter()
            .map(|r| {
                Ok(Invitation {
                    id: r.get(0),
                    household_id: r.get(1),
                    household_name: r.get(2),
                    sender_id: r.get(3),
                    status: r.get(4),
                    expires_at: chrono::DateTime::from_timestamp(r.get(5), 0)
                        .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    version: r.get(6),
                })
            })
            .collect()
    }
    pub async fn management(
        &self,
        actor: &str,
        operation: &str,
        commands: &[ManagementCommand],
        now: i64,
    ) -> Result<i64> {
        identifier(operation)?;
        ensure!(
            !commands.is_empty() && commands.len() <= 20,
            ErrorCode::InvalidValue
        );
        let mut tx = self.begin_serial().await?;
        Self::epoch(&mut tx, actor).await?;
        let payload = receipt_digest(&format!(
            "management-v1\n{}",
            serde_json::to_string(commands)?
        ));
        if let Some(row) = sqlx::query(
            "SELECT payload,revision,digest_version FROM receipts WHERE account_id=$1 AND operation_id=$2",
        )
        .bind(actor)
        .bind(operation)
        .fetch_optional(&mut *tx)
        .await?
        {
            ensure!(row.get::<i64, _>(2) == 1, ErrorCode::UnsupportedReceipt);
            ensure!(row.get::<String, _>(0) == payload, ErrorCode::OperationConflict);
            return Ok(row.get(1));
        }
        let mut touched = BTreeSet::new();
        let mut accounts = BTreeSet::from([actor.to_owned()]);
        for command in commands {
            match command {
                ManagementCommand::ReplacePolicy { id, policy, .. } => {
                    touched.insert(id.clone());
                    accounts.extend(Self::policy_audience(&mut tx, policy).await?);
                }
                ManagementCommand::RemoveHouseholdMember {
                    household_id,
                    account_id,
                    ..
                } => {
                    accounts.insert(account_id.clone());
                    let ids: Vec<String> = sqlx::query_scalar(
                        "SELECT resource_id FROM resource_household_grants WHERE household_id=$1",
                    )
                    .bind(household_id)
                    .fetch_all(&mut *tx)
                    .await?;
                    touched.extend(ids);
                }
                ManagementCommand::RespondToHouseholdInvitation { id, .. } => {
                    let ids: Vec<String> = sqlx::query_scalar("SELECT hg.resource_id FROM resource_household_grants hg JOIN household_invitations i ON i.household_id=hg.household_id WHERE i.id=$1")
                        .bind(id).fetch_all(&mut *tx).await?;
                    touched.extend(ids);
                }
                _ => {}
            }
        }
        for id in touched.clone() {
            let children: Vec<String> = sqlx::query_scalar(
                "SELECT resource_id FROM resource_ancestors WHERE ancestor_id=$1",
            )
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
            touched.extend(children);
        }
        Self::related_lists(&mut tx, &mut touched).await?;
        accounts.extend(Self::audience(&mut tx, &touched).await?);
        let mut before = BTreeMap::new();
        for account in &accounts {
            before.insert(
                account.clone(),
                Self::subset(&mut tx, account, &touched).await?,
            );
        }
        for command in commands {
            match command {
                ManagementCommand::CreateHousehold { id, name } => {
                    identifier(id)?;
                    content(name, "")?;
                    Self::household_capacity(&mut tx, actor).await?;
                    sqlx::query("INSERT INTO households(id,name) VALUES ($1,$2)")
                        .bind(id)
                        .bind(name)
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query("INSERT INTO household_memberships VALUES ($1,$2,'manager')")
                        .bind(id)
                        .bind(actor)
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query("UPDATE accounts SET primary_household_id=$1,preferences_version=preferences_version+1 WHERE id=$2 AND primary_household_id IS NULL")
                        .bind(id).bind(actor).execute(&mut *tx).await?;
                }
                ManagementCommand::RenameHousehold {
                    id,
                    name,
                    expected_version,
                } => {
                    Self::member(&mut tx, actor, id, true).await?;
                    Self::household_version(&mut tx, id, *expected_version).await?;
                    content(name, "")?;
                    sqlx::query("UPDATE households SET name=$1,version=version+1 WHERE id=$2")
                        .bind(name)
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                }
                ManagementCommand::RevokeHouseholdInvitation {
                    id,
                    expected_version,
                } => {
                    let row = sqlx::query(
                        "SELECT household_id,version,status FROM household_invitations WHERE id=$1",
                    )
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
                    let household: String = row.get(0);
                    Self::member(&mut tx, actor, &household, true).await?;
                    ensure!(
                        row.get::<i64, _>(1) == *expected_version
                            && row.get::<String, _>(2) == "pending",
                        ErrorCode::Conflict
                    );
                    sqlx::query("UPDATE household_invitations SET status='revoked',version=version+1 WHERE id=$1").bind(id).execute(&mut *tx).await?;
                    sqlx::query("UPDATE households SET version=version+1 WHERE id=$1")
                        .bind(household)
                        .execute(&mut *tx)
                        .await?;
                }
                ManagementCommand::InviteToHousehold {
                    id,
                    household_id,
                    recipient_id,
                    expected_version,
                } => {
                    identifier(id)?;
                    Self::member(&mut tx, actor, household_id, true).await?;
                    Self::household_version(&mut tx, household_id, *expected_version).await?;
                    ensure!(
                        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM accounts WHERE id=$1")
                            .bind(recipient_id)
                            .fetch_one(&mut *tx)
                            .await?
                            == 1,
                        ErrorCode::NotFound
                    );
                    ensure!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM household_memberships WHERE household_id=$1 AND account_id=$2").bind(household_id).bind(recipient_id).fetch_one(&mut *tx).await? == 0, ErrorCode::Conflict);
                    ensure!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM household_invitations WHERE household_id=$1 AND recipient_id=$2 AND status='pending' AND expires_at>$3")
                        .bind(household_id).bind(recipient_id).bind(now).fetch_one(&mut *tx).await? == 0, ErrorCode::Conflict);
                    ensure!(
                        sqlx::query_scalar::<_, i64>(
                            "SELECT COUNT(*) FROM household_invitations WHERE recipient_id=$1 AND status='pending' AND expires_at>$2"
                        )
                        .bind(recipient_id)
                        .bind(now)
                        .fetch_one(&mut *tx)
                        .await?
                            < 200, ErrorCode::SliceCapacity);
                    sqlx::query("INSERT INTO household_invitations(id,household_id,sender_id,recipient_id,status,expires_at) VALUES ($1,$2,$3,$4,'pending',$5)")
                        .bind(id).bind(household_id).bind(actor).bind(recipient_id).bind(now.checked_add(7*86400).ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?).execute(&mut *tx).await?;
                    sqlx::query("UPDATE households SET version=version+1 WHERE id=$1")
                        .bind(household_id)
                        .execute(&mut *tx)
                        .await?;
                }
                ManagementCommand::RespondToHouseholdInvitation {
                    id,
                    expected_version,
                    accept,
                } => {
                    let row = sqlx::query("SELECT household_id,version,status,expires_at FROM household_invitations WHERE id=$1 AND recipient_id=$2")
                        .bind(id).bind(actor).fetch_optional(&mut *tx).await?.ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
                    ensure!(
                        row.get::<i64, _>(1) == *expected_version
                            && row.get::<String, _>(2) == "pending",
                        ErrorCode::Conflict
                    );
                    ensure!(row.get::<i64, _>(3) > now, ErrorCode::InvitationExpired);
                    let household: String = row.get(0);
                    if *accept {
                        Self::household_capacity(&mut tx, actor).await?;
                        sqlx::query("INSERT INTO household_memberships VALUES ($1,$2,'member') ON CONFLICT DO NOTHING")
                            .bind(&household).bind(actor).execute(&mut *tx).await?;
                        sqlx::query("UPDATE accounts SET primary_household_id=$1,preferences_version=preferences_version+1 WHERE id=$2 AND primary_household_id IS NULL")
                            .bind(&household).bind(actor).execute(&mut *tx).await?;
                    }
                    sqlx::query(
                        "UPDATE household_invitations SET status=$1,version=version+1 WHERE id=$2",
                    )
                    .bind(if *accept { "accepted" } else { "declined" })
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query("UPDATE households SET version=version+1 WHERE id=$1")
                        .bind(&household)
                        .execute(&mut *tx)
                        .await?;
                }
                ManagementCommand::RemoveHouseholdMember {
                    household_id,
                    account_id,
                    expected_version,
                } => {
                    Self::member(&mut tx, actor, household_id, actor != account_id).await?;
                    Self::household_version(&mut tx, household_id, *expected_version).await?;
                    Self::member(&mut tx, account_id, household_id, false).await?;
                    Self::retain_manager(&mut tx, household_id, account_id).await?;
                    sqlx::query(
                        "DELETE FROM household_memberships WHERE household_id=$1 AND account_id=$2",
                    )
                    .bind(household_id)
                    .bind(account_id)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query("UPDATE accounts SET primary_household_id=CASE WHEN primary_household_id=$1 THEN NULL ELSE primary_household_id END,preferences_version=preferences_version+1 WHERE id=$2")
                        .bind(household_id).bind(account_id).execute(&mut *tx).await?;
                    sqlx::query("UPDATE account_invitations SET revoked=1 WHERE household_id=$1 AND issuer_id=$2 AND redeemed_by IS NULL")
                        .bind(household_id).bind(account_id).execute(&mut *tx).await?;
                    // A removed member cannot rejoin using another old pending invitation.
                    sqlx::query("UPDATE household_invitations SET status='revoked',version=version+1 WHERE household_id=$1 AND recipient_id=$2 AND status='pending'")
                        .bind(household_id).bind(account_id).execute(&mut *tx).await?;
                    sqlx::query("UPDATE households SET version=version+1 WHERE id=$1")
                        .bind(household_id)
                        .execute(&mut *tx)
                        .await?;
                }
                ManagementCommand::SetHouseholdRole {
                    household_id,
                    account_id,
                    expected_version,
                    manager,
                } => {
                    Self::member(&mut tx, actor, household_id, true).await?;
                    Self::household_version(&mut tx, household_id, *expected_version).await?;
                    Self::member(&mut tx, account_id, household_id, false).await?;
                    if !manager {
                        Self::retain_manager(&mut tx, household_id, account_id).await?;
                        sqlx::query("UPDATE account_invitations SET revoked=1 WHERE household_id=$1 AND issuer_id=$2 AND redeemed_by IS NULL")
                            .bind(household_id).bind(account_id).execute(&mut *tx).await?;
                    }
                    sqlx::query("UPDATE household_memberships SET role=$1 WHERE household_id=$2 AND account_id=$3")
                        .bind(if *manager {"manager"} else {"member"}).bind(household_id).bind(account_id).execute(&mut *tx).await?;
                    sqlx::query("UPDATE households SET version=version+1 WHERE id=$1")
                        .bind(household_id)
                        .execute(&mut *tx)
                        .await?;
                }
                ManagementCommand::SetPrimaryHousehold {
                    household_id,
                    expected_version,
                } => {
                    if let Some(id) = household_id {
                        Self::member(&mut tx, actor, id, false).await?;
                    }
                    let changed = sqlx::query("UPDATE accounts SET primary_household_id=$1,preferences_version=preferences_version+1 WHERE id=$2 AND preferences_version=$3")
                        .bind(household_id).bind(actor).bind(expected_version).execute(&mut *tx).await?;
                    ensure!(changed.rows_affected() == 1, ErrorCode::Conflict);
                }
                ManagementCommand::SetDefaults {
                    household_id,
                    resource_kind,
                    expected_version,
                    template,
                } => {
                    ensure!(
                        matches!(
                            resource_kind.as_str(),
                            "person" | "field" | "task" | "list" | "progress"
                        ),
                        ErrorCode::InvalidValue
                    );
                    if let Some(DefaultTemplate::Explicit { policy }) = template {
                        Self::validate_policy(&mut tx, actor, policy).await?;
                    }
                    let (column, scope) = if let Some(id) = household_id {
                        Self::member(&mut tx, actor, id, true).await?;
                        ("household_id", id.as_str())
                    } else {
                        ("account_id", actor)
                    };
                    let current: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT version FROM default_templates WHERE {column}=$1 AND resource_kind=$2")))
                        .bind(scope).bind(resource_kind).fetch_optional(&mut *tx).await?;
                    ensure!(
                        current.unwrap_or(0) == *expected_version,
                        ErrorCode::Conflict
                    );
                    if let Some(template) = template {
                        sqlx::query(sqlx::AssertSqlSafe(format!("INSERT INTO default_templates({column},resource_kind,template,version) VALUES ($1,$2,$3,$4) ON CONFLICT({column},resource_kind) DO UPDATE SET template=excluded.template,version=excluded.version")))
                            .bind(scope).bind(resource_kind).bind(serde_json::to_string(template)?).bind(current.unwrap_or(0)+1).execute(&mut *tx).await?;
                    } else {
                        sqlx::query(sqlx::AssertSqlSafe(format!(
                            "DELETE FROM default_templates WHERE {column}=$1 AND resource_kind=$2"
                        )))
                        .bind(scope)
                        .bind(resource_kind)
                        .execute(&mut *tx)
                        .await?;
                    }
                    if household_id.is_some() {
                        sqlx::query("UPDATE households SET version=version+1 WHERE id=$1")
                            .bind(scope)
                            .execute(&mut *tx)
                            .await?;
                    } else {
                        sqlx::query("UPDATE accounts SET preferences_version=preferences_version+1 WHERE id=$1").bind(actor).execute(&mut *tx).await?;
                    }
                }
                ManagementCommand::ReplacePolicy {
                    id,
                    expected_version,
                    policy,
                } => {
                    Self::manage(&mut tx, actor, id, *expected_version).await?;
                    Self::put_policy(&mut tx, actor, id, policy).await?;
                    sqlx::query("UPDATE resources SET policy_version=policy_version+1 WHERE id=$1")
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                }
            }
        }
        let revision = Self::publish(&mut tx, &accounts, &touched, &before, true).await?;
        sqlx::query(
            "INSERT INTO receipts(account_id,operation_id,payload,revision) VALUES ($1,$2,$3,$4)",
        )
        .bind(actor)
        .bind(operation)
        .bind(payload)
        .bind(revision)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(revision)
    }
}
