//! Shared account/household policy primitives. Owners retain management authority.
use super::*;
use crate::error::ErrorCode;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PrincipalGrant {
    Account { id: String, edit: bool },
    Household { id: String, edit: bool },
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub grants: Vec<PrincipalGrant>,
    pub exclude_accounts: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DefaultTemplate {
    Private,
    PrimaryHousehold { edit: bool },
    Explicit { policy: Policy },
}
#[derive(Serialize)]
pub struct Defaults {
    pub revision: String,
    pub preferences_version: i64,
    pub primary_household_id: Option<String>,
    pub person: DefaultTemplate,
    pub field: DefaultTemplate,
    pub task: DefaultTemplate,
    pub list: DefaultTemplate,
    pub progress: DefaultTemplate,
}

// Each query is built only from these repository-owned fragments; user values bind.
pub(super) const COLUMNS: &str = "r.id,r.kind,r.parent_id,r.label,r.value,r.version,CASE WHEN r.owner_id=$1 THEN r.policy_version ELSE NULL END AS policy_version,CASE WHEN EXISTS(SELECT 1 FROM person_accounts pa WHERE pa.person_id=r.id AND pa.account_id<>$1) THEN 0 WHEN r.owner_id=$1 OR COALESCE(g.can_edit,0)=1 OR EXISTS(SELECT 1 FROM resource_household_grants hg JOIN household_memberships hm ON hm.household_id=hg.household_id WHERE hg.resource_id=r.id AND hm.account_id=$1 AND hg.can_edit=1) THEN 1 ELSE 0 END AS editable,r.archived";
pub(super) const VISIBLE: &str = "NOT (r.owner_id=$1 AND EXISTS(SELECT 1 FROM frozen_owner_visibility f WHERE f.resource_id=r.id)) AND (r.owner_id=$1 OR ((g.account_id=$1 OR EXISTS(SELECT 1 FROM resource_household_grants hg JOIN household_memberships hm ON hm.household_id=hg.household_id WHERE hg.resource_id=r.id AND hm.account_id=$1)) AND NOT EXISTS(SELECT 1 FROM resource_exclusions x WHERE x.resource_id=r.id AND x.account_id=$1))) AND NOT EXISTS(SELECT 1 FROM resource_ancestors a JOIN resources p ON p.id=a.ancestor_id WHERE a.resource_id=r.id AND NOT (p.owner_id=$1 OR ((EXISTS(SELECT 1 FROM resource_grants pg WHERE pg.resource_id=p.id AND pg.account_id=$1) OR EXISTS(SELECT 1 FROM resource_household_grants phg JOIN household_memberships phm ON phm.household_id=phg.household_id WHERE phg.resource_id=p.id AND phm.account_id=$1)) AND NOT EXISTS(SELECT 1 FROM resource_exclusions px WHERE px.resource_id=p.id AND px.account_id=$1))))";

impl Store {
    pub(super) async fn audience(
        tx: &mut Transaction<'_, Any>,
        ids: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>> {
        let mut accounts = BTreeSet::new();
        for id in ids {
            let rows: Vec<String> = sqlx::query_scalar("SELECT owner_id FROM resources WHERE id=$1 UNION SELECT account_id FROM resource_grants WHERE resource_id=$1 UNION SELECT hm.account_id FROM resource_household_grants hg JOIN household_memberships hm ON hm.household_id=hg.household_id WHERE hg.resource_id=$1")
                .bind(id).fetch_all(&mut **tx).await?;
            accounts.extend(rows);
        }
        Ok(accounts)
    }
    pub(super) async fn policy_audience(
        tx: &mut Transaction<'_, Any>,
        policy: &Policy,
    ) -> Result<BTreeSet<String>> {
        let mut accounts = BTreeSet::new();
        for grant in &policy.grants {
            match grant {
                PrincipalGrant::Account { id, .. } => {
                    accounts.insert(id.clone());
                }
                PrincipalGrant::Household { id, .. } => {
                    let members: Vec<String> = sqlx::query_scalar(
                        "SELECT account_id FROM household_memberships WHERE household_id=$1",
                    )
                    .bind(id)
                    .fetch_all(&mut **tx)
                    .await?;
                    accounts.extend(members);
                }
            }
        }
        accounts.retain(|id| !policy.exclude_accounts.contains(id));
        Ok(accounts)
    }
    pub(super) async fn validate_policy(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        policy: &Policy,
    ) -> Result<()> {
        ensure!(
            policy.grants.len() <= 100 && policy.exclude_accounts.len() <= 100,
            ErrorCode::InvalidValue
        );
        let mut unique = BTreeSet::new();
        for grant in &policy.grants {
            let (kind, id) = match grant {
                PrincipalGrant::Account { id, .. } => {
                    ensure!(
                        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM accounts WHERE id=$1")
                            .bind(id)
                            .fetch_one(&mut **tx)
                            .await?
                            == 1,
                        ErrorCode::NotFound
                    );
                    ("account", id)
                }
                PrincipalGrant::Household { id, .. } => {
                    Self::member(tx, actor, id, false).await?;
                    ("household", id)
                }
            };
            identifier(id)?;
            ensure!(unique.insert((kind, id)), ErrorCode::InvalidValue);
        }
        let mut unique = BTreeSet::new();
        for id in &policy.exclude_accounts {
            identifier(id)?;
            ensure!(unique.insert(id), ErrorCode::InvalidValue);
            ensure!(
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM accounts WHERE id=$1")
                    .bind(id)
                    .fetch_one(&mut **tx)
                    .await?
                    == 1,
                ErrorCode::NotFound
            );
        }
        Ok(())
    }
    pub(super) async fn put_policy(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        id: &str,
        policy: &Policy,
    ) -> Result<()> {
        Self::validate_policy(tx, actor, policy).await?;
        sqlx::query("DELETE FROM frozen_owner_visibility WHERE resource_id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        let parent: Option<String> =
            sqlx::query_scalar("SELECT parent_id FROM resources WHERE id=$1")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
        if let Some(parent) = parent {
            for account in Self::policy_audience(tx, policy).await? {
                ensure!(
                    Self::subset(tx, &account, &BTreeSet::from([parent.clone()]))
                        .await?
                        .contains_key(&parent),
                    ErrorCode::IdentityGrantRequired
                );
            }
        }
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
        for grant in &policy.grants {
            match grant {
                PrincipalGrant::Account { id: account, edit } => {
                    sqlx::query("INSERT INTO resource_grants VALUES ($1,$2,$3)")
                        .bind(id)
                        .bind(account)
                        .bind(i64::from(*edit))
                        .execute(&mut **tx)
                        .await?;
                }
                PrincipalGrant::Household {
                    id: household,
                    edit,
                } => {
                    sqlx::query("INSERT INTO resource_household_grants VALUES ($1,$2,$3)")
                        .bind(id)
                        .bind(household)
                        .bind(i64::from(*edit))
                        .execute(&mut **tx)
                        .await?;
                }
            }
        }
        for account in &policy.exclude_accounts {
            sqlx::query("INSERT INTO resource_exclusions VALUES ($1,$2)")
                .bind(id)
                .bind(account)
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }

    pub async fn defaults(&self, actor: &str) -> Result<Defaults> {
        let mut tx = self.pool.begin().await?;
        if !self.sqlite {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
        }
        let result = Self::defaults_in(&mut tx, actor).await?;
        tx.commit().await?;
        Ok(result)
    }
    pub(super) async fn defaults_in(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
    ) -> Result<Defaults> {
        let row = sqlx::query(
            "SELECT primary_household_id,preferences_version FROM accounts WHERE id=$1",
        )
        .bind(actor)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| anyhow!(ErrorCode::Unauthenticated))?;
        let primary: Option<String> = row.get(0);
        let preferences_version: i64 = row.get(1);
        let mut templates = Vec::new();
        let mut revisions = vec![format!("{actor}:{preferences_version}:{primary:?}")];
        for kind in ["person", "field", "task", "list", "progress"] {
            let personal = sqlx::query("SELECT template,version FROM default_templates WHERE account_id=$1 AND resource_kind=$2")
                .bind(actor).bind(kind).fetch_optional(&mut **tx).await?;
            let household = if let Some(household) = &primary {
                sqlx::query("SELECT template,version FROM default_templates WHERE household_id=$1 AND resource_kind=$2")
                    .bind(household).bind(kind).fetch_optional(&mut **tx).await?
            } else {
                None
            };
            let selected = personal.or(household);
            if let Some(row) = selected {
                let payload: String = row.get(0);
                revisions.push(format!("{kind}:{}:{payload}", row.get::<i64, _>(1)));
                templates.push(serde_json::from_str::<DefaultTemplate>(&payload)?);
            } else {
                // People default to the primary household. New free-text notes stay
                // private unless a household/personal template deliberately shares them.
                templates.push(if kind == "person" {
                    DefaultTemplate::PrimaryHousehold { edit: true }
                } else if kind == "progress" {
                    DefaultTemplate::PrimaryHousehold { edit: false }
                } else {
                    DefaultTemplate::Private
                });
                revisions.push(format!("{kind}:application-v1"));
            }
        }
        let mut households = BTreeSet::new();
        if let Some(id) = &primary {
            households.insert(id.clone());
        }
        for template in &templates {
            if let DefaultTemplate::Explicit { policy } = template {
                for grant in &policy.grants {
                    if let PrincipalGrant::Household { id, .. } = grant {
                        households.insert(id.clone());
                    }
                }
            }
        }
        for household in households {
            let version: Option<i64> = sqlx::query_scalar("SELECT h.version FROM households h JOIN household_memberships m ON m.household_id=h.id WHERE h.id=$1 AND m.account_id=$2")
                .bind(&household).bind(actor).fetch_optional(&mut **tx).await?;
            // A retained personal template must not turn its revision into an
            // activity oracle for a household the account can no longer see.
            revisions.push(format!("household:{household}:{version:?}"));
        }
        Ok(Defaults {
            revision: receipt_digest(&revisions.join("\n")),
            preferences_version,
            primary_household_id: primary,
            person: templates.remove(0),
            field: templates.remove(0),
            task: templates.remove(0),
            list: templates.remove(0),
            progress: templates.remove(0),
        })
    }
    pub(super) fn resolved_policy(defaults: &Defaults, kind: &str) -> Policy {
        match match kind {
            "person" => &defaults.person,
            "task" => &defaults.task,
            "list" => &defaults.list,
            "progress" => &defaults.progress,
            _ => &defaults.field,
        } {
            DefaultTemplate::Private => Policy::default(),
            DefaultTemplate::PrimaryHousehold { edit } => Policy {
                grants: defaults
                    .primary_household_id
                    .iter()
                    .map(|id| PrincipalGrant::Household {
                        id: id.clone(),
                        edit: *edit,
                    })
                    .collect(),
                exclude_accounts: Vec::new(),
            },
            DefaultTemplate::Explicit { policy } => policy.clone(),
        }
    }
}

#[derive(Serialize)]
pub struct TemplateRecord {
    pub template: Option<DefaultTemplate>,
    pub version: i64,
}
#[derive(Serialize)]
pub struct TemplateScope {
    pub person: TemplateRecord,
    pub field: TemplateRecord,
    pub task: TemplateRecord,
    pub list: TemplateRecord,
    pub progress: TemplateRecord,
}
#[derive(Serialize)]
pub struct PolicyState {
    pub version: i64,
    pub policy: Policy,
}

impl Store {
    pub async fn default_templates(
        &self,
        actor: &str,
        household: Option<&str>,
    ) -> Result<TemplateScope> {
        let mut tx = self.pool.begin().await?;
        if !self.sqlite {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
        }
        Self::epoch(&mut tx, actor).await?;
        let (column, scope) = if let Some(id) = household {
            Self::member(&mut tx, actor, id, false).await?;
            ("household_id", id)
        } else {
            ("account_id", actor)
        };
        let mut list = Vec::new();
        for kind in ["person", "field", "task", "list", "progress"] {
            let row = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT template,version FROM default_templates WHERE {column}=$1 AND resource_kind=$2")))
                .bind(scope).bind(kind).fetch_optional(&mut *tx).await?;
            list.push(if let Some(row) = row {
                TemplateRecord {
                    template: Some(serde_json::from_str(&row.get::<String, _>(0))?),
                    version: row.get(1),
                }
            } else {
                TemplateRecord {
                    template: None,
                    version: 0,
                }
            });
        }
        tx.commit().await?;
        Ok(TemplateScope {
            person: list.remove(0),
            field: list.remove(0),
            task: list.remove(0),
            list: list.remove(0),
            progress: list.remove(0),
        })
    }
    pub async fn resource_policy(&self, actor: &str, id: &str) -> Result<PolicyState> {
        let mut tx = self.pool.begin().await?;
        if !self.sqlite {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
        }
        let visible = Self::subset(&mut tx, actor, &BTreeSet::from([id.to_owned()])).await?;
        let version = if let Some(p) = visible.get(id) {
            p.policy_version
                .ok_or_else(|| anyhow!(ErrorCode::Forbidden))?
        } else {
            ensure!(
                Self::owner_can_restore_visibility(&mut tx, actor, id).await?,
                ErrorCode::NotFound
            );
            sqlx::query_scalar("SELECT policy_version FROM resources WHERE id=$1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?
        };
        let mut grants = Vec::new();
        for row in sqlx::query("SELECT account_id,can_edit FROM resource_grants WHERE resource_id=$1 ORDER BY account_id").bind(id).fetch_all(&mut *tx).await? {
            grants.push(PrincipalGrant::Account { id:row.get(0), edit:row.get::<i64,_>(1)==1 });
        }
        for row in sqlx::query("SELECT household_id,can_edit FROM resource_household_grants WHERE resource_id=$1 ORDER BY household_id").bind(id).fetch_all(&mut *tx).await? {
            grants.push(PrincipalGrant::Household { id:row.get(0), edit:row.get::<i64,_>(1)==1 });
        }
        let exclude_accounts = sqlx::query_scalar(
            "SELECT account_id FROM resource_exclusions WHERE resource_id=$1 ORDER BY account_id",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(PolicyState {
            version,
            policy: Policy {
                grants,
                exclude_accounts,
            },
        })
    }
}
