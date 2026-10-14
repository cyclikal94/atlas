//! Viewer-safe People summaries (BE-PEOPLE-SUMMARY, API `0.29.0`).
//!
//! Each read answers, for the viewing account only, how a person or field is linked, who owns it,
//! whether the viewer is known to have added it, and who it is shared with. Kieran's 14 October
//! 2026 decisions govern the result:
//!
//! 1. Original-creator attribution (`person_attributions`) is separate from ownership and grants
//!    nothing. A person without a row (everything created before schema `1006`) is `unknown`.
//! 2. A non-owner receives only sharing information already authorised for them and is always told
//!    the summary is `partial`, whether or not hidden grants exist. No existence flag, hidden
//!    identity or hidden count is returned.
//! 3. "In Home" means a grant to the viewer's primary household.
//!
//! Every call reads one database snapshot (`task_read`), so a row's audience, versions and field
//! count are mutually consistent. [`disclose`] and [`shared_with`] are the only places that turn
//! the raw audience into a response; the owner-only statements are never issued for rows the
//! viewer does not own, and a tripwire refuses a raw audience that breaks that rule.
use crate::error::ErrorCode;
use crate::{Projection, Store, identifier, policy};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use sqlx::any::AnyRow;
use sqlx::{Any, Row, Transaction};
use std::collections::{BTreeMap, BTreeSet};

/// The People list filters, each evaluated over the viewer's whole visible set.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeopleScope {
    #[default]
    All,
    InHome,
    SharedWithMe,
    NotLinked,
}

impl PeopleScope {
    /// Repository-owned predicate over `r` for the viewer `$1`. Request values only choose which
    /// constant applies; they are never interpolated.
    fn predicate(self) -> &'static str {
        match self {
            Self::All => "1=1",
            Self::InHome => {
                "EXISTS(SELECT 1 FROM resource_household_grants hh JOIN accounts va ON va.id=$1 AND hh.household_id=va.primary_household_id JOIN household_memberships vm ON vm.household_id=hh.household_id AND vm.account_id=$1 WHERE hh.resource_id=r.id)"
            }
            // Owned rows and rows known to have been added by the viewer are excluded. Unknown
            // attribution is not known to be the viewer's, so another owner's row stays.
            Self::SharedWithMe => {
                "r.owner_id<>$1 AND NOT EXISTS(SELECT 1 FROM person_attributions pa WHERE pa.person_id=r.id AND pa.added_by=$1)"
            }
            Self::NotLinked => {
                "NOT EXISTS(SELECT 1 FROM person_accounts pl WHERE pl.person_id=r.id)"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Linkage {
    You,
    Other,
    None,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AddedBy {
    Yes,
    No,
    Unknown,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Owner,
    Edit,
    View,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Disclosure {
    Complete,
    Partial,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PersonRef {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SummaryOwner {
    pub account_id: String,
    pub username: String,
    pub you: bool,
    /// The owner's own linked person, only when the viewer can see it.
    pub person: Option<PersonRef>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct HouseholdAudience {
    pub id: String,
    pub can_edit: bool,
    pub you_are_member: bool,
    /// `name`, `version` and `member_count` are present only for a household the viewer belongs
    /// to, as in `GET /households`. `member_count` is the household's size, not its audience.
    pub name: Option<String>,
    pub version: Option<i64>,
    pub member_count: Option<i64>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DirectGrant {
    pub can_edit: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AudienceSummary {
    pub disclosure: Disclosure,
    pub in_home: bool,
    pub households: Vec<HouseholdAudience>,
    pub your_direct_grant: Option<DirectGrant>,
    pub direct_grant_count: Option<i64>,
    pub excluded_count: Option<i64>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PersonSummary {
    pub id: String,
    pub label: String,
    pub version: i64,
    pub policy_version: i64,
    pub archived: bool,
    pub linkage: Linkage,
    pub linked_account_id: Option<String>,
    pub owner: SummaryOwner,
    pub added_by_you: AddedBy,
    pub your_access: Access,
    pub field_count: i64,
    pub audience: AudienceSummary,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PeopleHome {
    pub id: String,
    pub name: String,
    pub version: i64,
    pub member_count: i64,
}

#[derive(Debug, Serialize)]
pub struct PersonSummaryPage {
    pub items: Vec<PersonSummary>,
    pub next_after: Option<String>,
    /// The viewer's primary household, or `None` when there is none (In Home is then empty).
    pub home: Option<PeopleHome>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AccountGrant {
    pub account_id: String,
    pub username: String,
    pub can_edit: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AccountRef {
    pub account_id: String,
    pub username: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SharedMember {
    pub account_id: String,
    pub username: String,
    pub excluded: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SharedHousehold {
    #[serde(flatten)]
    pub household: HouseholdAudience,
    pub members: Option<Vec<SharedMember>>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SharedWith {
    pub disclosure: Disclosure,
    pub owner: SummaryOwner,
    pub accounts: Vec<AccountGrant>,
    pub households: Vec<SharedHousehold>,
    pub excluded_accounts: Option<Vec<AccountRef>>,
}

#[derive(Debug, Serialize)]
pub struct PersonSummaryDetail {
    pub summary: PersonSummary,
    pub shared_with: SharedWith,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FieldAudience {
    pub id: String,
    pub version: i64,
    pub policy_version: i64,
    pub archived: bool,
    pub owner_is_you: bool,
    pub your_access: Access,
    pub audience: AudienceSummary,
}

#[derive(Debug, Serialize)]
pub struct FieldAudiencePage {
    pub items: Vec<FieldAudience>,
    pub next_after: Option<String>,
}

/// One household grant as stored, before disclosure.
struct RawHousehold {
    id: String,
    can_edit: bool,
    member: bool,
    name: String,
    version: i64,
    member_count: i64,
}

/// A row's audience as read from the database, before disclosure. `grants` and `exclusions` are
/// read only for rows the viewer owns; `your_grant` only for the others.
struct RawAudience {
    owner_id: String,
    owner_username: String,
    households: Vec<RawHousehold>,
    grants: Vec<AccountGrant>,
    exclusions: Vec<AccountRef>,
    your_grant: Option<bool>,
}

/// The audience summary the viewer may receive. The only place that removes non-member
/// households, other accounts' grants and the counts for a viewer who does not own the row.
fn disclose(owned: bool, home: Option<&str>, raw: &RawAudience) -> AudienceSummary {
    AudienceSummary {
        disclosure: if owned {
            Disclosure::Complete
        } else {
            Disclosure::Partial
        },
        in_home: home.is_some_and(|p| raw.households.iter().any(|h| h.member && h.id == p)),
        households: raw
            .households
            .iter()
            .filter(|h| owned || h.member)
            .map(household_audience)
            .collect(),
        your_direct_grant: if owned {
            None
        } else {
            raw.your_grant.map(|can_edit| DirectGrant { can_edit })
        },
        direct_grant_count: owned.then_some(raw.grants.len() as i64),
        excluded_count: owned.then_some(raw.exclusions.len() as i64),
    }
}

fn household_audience(h: &RawHousehold) -> HouseholdAudience {
    HouseholdAudience {
        id: h.id.clone(),
        can_edit: h.can_edit,
        you_are_member: h.member,
        name: h.member.then(|| h.name.clone()),
        version: h.member.then_some(h.version),
        member_count: h.member.then_some(h.member_count),
    }
}

/// The detail rail. The owner receives every grant, every household (members listed for those
/// the owner belongs to, each marked when excluded) and the exclusions. Anyone else receives
/// their own direct grant and the households they belong to, never a member list.
fn shared_with(
    owned: bool,
    owner: SummaryOwner,
    viewer: &AccountRef,
    raw: &RawAudience,
    members: &BTreeMap<String, Vec<AccountRef>>,
) -> SharedWith {
    let excluded: BTreeSet<&str> = raw
        .exclusions
        .iter()
        .map(|x| x.account_id.as_str())
        .collect();
    SharedWith {
        disclosure: if owned {
            Disclosure::Complete
        } else {
            Disclosure::Partial
        },
        owner,
        accounts: if owned {
            raw.grants.clone()
        } else {
            raw.your_grant
                .map(|can_edit| AccountGrant {
                    account_id: viewer.account_id.clone(),
                    username: viewer.username.clone(),
                    can_edit,
                })
                .into_iter()
                .collect()
        },
        households: raw
            .households
            .iter()
            .filter(|h| owned || h.member)
            .map(|h| SharedHousehold {
                household: household_audience(h),
                members: (owned && h.member).then(|| {
                    members
                        .get(&h.id)
                        .into_iter()
                        .flatten()
                        .map(|m| SharedMember {
                            account_id: m.account_id.clone(),
                            username: m.username.clone(),
                            excluded: excluded.contains(m.account_id.as_str()),
                        })
                        .collect()
                }),
            })
            .collect(),
        excluded_accounts: owned.then(|| raw.exclusions.clone()),
    }
}

/// The id query of one `people_summaries` page: `$1` viewer, `$2` archived (0/1), `$3` the
/// exclusive `after` id or NULL, `$4` the limit. Public only so the profiling example plans the
/// exact statement the read runs.
#[doc(hidden)]
pub fn summary_id_query(scope: PeopleScope) -> String {
    format!(
        "SELECT r.id FROM resources r LEFT JOIN resource_grants g ON g.resource_id=r.id AND g.account_id=$1 WHERE r.kind='person' AND r.archived=$2 AND ($3 IS NULL OR r.id>$3) AND {} AND {} ORDER BY r.id LIMIT $4",
        policy::VISIBLE,
        scope.predicate()
    )
}

/// `$first,…` placeholders for an `IN` list.
fn placeholders(first: usize, count: usize) -> String {
    (0..count)
        .map(|i| format!("${}", first + i))
        .collect::<Vec<_>>()
        .join(",")
}

/// Run `sql`, whose `{ids}` is replaced by one placeholder per id. With `viewer`, `$1` binds the
/// viewer and the ids follow. Pages are at most 100 rows, so one statement covers a page.
async fn rows_in(
    tx: &mut Transaction<'_, Any>,
    sql: &str,
    viewer: Option<&str>,
    ids: &[&str],
) -> Result<Vec<AnyRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let first = if viewer.is_some() { 2 } else { 1 };
    let sql = sql.replace("{ids}", &placeholders(first, ids.len()));
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    if let Some(viewer) = viewer {
        query = query.bind(viewer);
    }
    for id in ids {
        query = query.bind(*id);
    }
    Ok(query.fetch_all(&mut **tx).await?)
}

/// Visible rows for `ids` from the same snapshot as the id query. A row the id query returned
/// cannot be missing here within one snapshot; a gap means that guarantee failed, so refuse.
async fn projections(
    tx: &mut Transaction<'_, Any>,
    actor: &str,
    ids: &[String],
    kind: &str,
) -> Result<BTreeMap<String, Projection>> {
    let found = Store::subset_batch(tx, actor, &ids.iter().cloned().collect()).await?;
    ensure!(
        found.len() == ids.len() && found.values().all(|p| p.kind == kind),
        ErrorCode::InternalError
    );
    Ok(found)
}

impl Store {
    /// The viewer's primary household, if the viewer still belongs to it.
    async fn people_home(tx: &mut Transaction<'_, Any>, actor: &str) -> Result<Option<PeopleHome>> {
        Ok(sqlx::query("SELECT h.id,h.name,h.version,(SELECT COUNT(*) FROM household_memberships c WHERE c.household_id=h.id) FROM accounts a JOIN households h ON h.id=a.primary_household_id JOIN household_memberships m ON m.household_id=h.id AND m.account_id=a.id WHERE a.id=$1")
            .bind(actor)
            .fetch_optional(&mut **tx)
            .await?
            .map(|r| PeopleHome {
                id: r.get(0),
                name: r.get(1),
                version: r.get(2),
                member_count: r.get(3),
            }))
    }

    /// The raw audience of each id, each statement once per page.
    async fn raw_audiences(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        ids: &[&str],
    ) -> Result<BTreeMap<String, RawAudience>> {
        let mut raw = BTreeMap::new();
        for r in rows_in(tx, "SELECT r.id,r.owner_id,a.username FROM resources r JOIN accounts a ON a.id=r.owner_id WHERE r.id IN ({ids})", None, ids).await? {
            raw.insert(
                r.get::<String, _>(0),
                RawAudience {
                    owner_id: r.get(1),
                    owner_username: r.get(2),
                    households: Vec::new(),
                    grants: Vec::new(),
                    exclusions: Vec::new(),
                    your_grant: None,
                },
            );
        }
        ensure!(raw.len() == ids.len(), ErrorCode::InternalError);
        for r in rows_in(tx, "SELECT hg.resource_id,hg.household_id,hg.can_edit,h.name,h.version,(SELECT COUNT(*) FROM household_memberships c WHERE c.household_id=hg.household_id),CASE WHEN EXISTS(SELECT 1 FROM household_memberships m WHERE m.household_id=hg.household_id AND m.account_id=$1) THEN 1 ELSE 0 END FROM resource_household_grants hg JOIN households h ON h.id=hg.household_id WHERE hg.resource_id IN ({ids}) ORDER BY hg.resource_id,hg.household_id", Some(actor), ids).await? {
            if let Some(entry) = raw.get_mut(&r.get::<String, _>(0)) {
                entry.households.push(RawHousehold {
                    id: r.get(1),
                    can_edit: r.get::<i64, _>(2) == 1,
                    name: r.get(3),
                    version: r.get(4),
                    member_count: r.get(5),
                    member: r.get::<i64, _>(6) == 1,
                });
            }
        }
        let (owned, others): (Vec<&str>, Vec<&str>) = ids
            .iter()
            .copied()
            .partition(|id| raw.get(*id).is_some_and(|e| e.owner_id == actor));
        // Owner-only: every direct grant and exclusion, read only for rows the viewer owns.
        for r in rows_in(tx, "SELECT g.resource_id,g.account_id,a.username,g.can_edit FROM resource_grants g JOIN accounts a ON a.id=g.account_id WHERE g.resource_id IN ({ids}) ORDER BY g.resource_id,g.account_id", None, &owned).await? {
            if let Some(entry) = raw.get_mut(&r.get::<String, _>(0)) {
                entry.grants.push(AccountGrant {
                    account_id: r.get(1),
                    username: r.get(2),
                    can_edit: r.get::<i64, _>(3) == 1,
                });
            }
        }
        for r in rows_in(tx, "SELECT x.resource_id,x.account_id,a.username FROM resource_exclusions x JOIN accounts a ON a.id=x.account_id WHERE x.resource_id IN ({ids}) ORDER BY x.resource_id,x.account_id", None, &owned).await? {
            if let Some(entry) = raw.get_mut(&r.get::<String, _>(0)) {
                entry.exclusions.push(AccountRef {
                    account_id: r.get(1),
                    username: r.get(2),
                });
            }
        }
        // Everyone else: only the viewer's own direct grant.
        for r in rows_in(tx, "SELECT resource_id,account_id,can_edit FROM resource_grants WHERE account_id=$1 AND resource_id IN ({ids})", Some(actor), &others).await? {
            ensure!(r.get::<String, _>(1) == actor, ErrorCode::InternalError);
            if let Some(entry) = raw.get_mut(&r.get::<String, _>(0)) {
                entry.your_grant = Some(r.get::<i64, _>(2) == 1);
            }
        }
        // Tripwire for the SQL layer: a row the viewer does not own never carries another
        // account's grant or any exclusion, whatever `disclose` would later drop.
        ensure!(
            raw.values()
                .all(|e| e.owner_id == actor || (e.grants.is_empty() && e.exclusions.is_empty())),
            ErrorCode::InternalError
        );
        Ok(raw)
    }

    /// Each owner's own linked person, when the viewer can see it.
    async fn owner_people(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        owners: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, PersonRef>> {
        let owners: Vec<&str> = owners.iter().map(String::as_str).collect();
        let mut linked = BTreeMap::new();
        for r in rows_in(
            tx,
            "SELECT account_id,person_id FROM person_accounts WHERE account_id IN ({ids})",
            None,
            &owners,
        )
        .await?
        {
            linked.insert(r.get::<String, _>(1), r.get::<String, _>(0));
        }
        let visible = Self::subset_batch(tx, actor, &linked.keys().cloned().collect()).await?;
        Ok(visible
            .into_values()
            .filter_map(|p| {
                linked.get(&p.id).map(|owner| {
                    (
                        owner.clone(),
                        PersonRef {
                            id: p.id,
                            label: p.label,
                        },
                    )
                })
            })
            .collect())
    }

    /// Summaries of visible person `ids` in this snapshot, in `ids` order, each with its raw
    /// audience for the detail rail.
    async fn summaries_in(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        ids: &[String],
        home: Option<&str>,
    ) -> Result<Vec<(PersonSummary, RawAudience)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut projected = projections(tx, actor, ids, "person").await?;
        let keys: Vec<&str> = ids.iter().map(String::as_str).collect();
        let mut raw = Self::raw_audiences(tx, actor, &keys).await?;
        let mut links = BTreeMap::new();
        for r in rows_in(
            tx,
            "SELECT person_id,account_id FROM person_accounts WHERE person_id IN ({ids})",
            None,
            &keys,
        )
        .await?
        {
            links.insert(r.get::<String, _>(0), r.get::<String, _>(1));
        }
        let mut added = BTreeMap::new();
        for r in rows_in(
            tx,
            "SELECT person_id,added_by FROM person_attributions WHERE person_id IN ({ids})",
            None,
            &keys,
        )
        .await?
        {
            added.insert(r.get::<String, _>(0), r.get::<String, _>(1));
        }
        let owners: BTreeSet<String> = raw.values().map(|e| e.owner_id.clone()).collect();
        let owner_people = Self::owner_people(tx, actor, &owners).await?;
        let mut fields = BTreeMap::new();
        for r in rows_in(tx, &format!("SELECT r.parent_id,COUNT(*) FROM resources r LEFT JOIN resource_grants g ON g.resource_id=r.id AND g.account_id=$1 WHERE r.kind='field' AND r.archived=0 AND r.parent_id IN ({{ids}}) AND {} GROUP BY r.parent_id", policy::VISIBLE), Some(actor), &keys).await? {
            fields.insert(r.get::<String, _>(0), r.get::<i64, _>(1));
        }
        let mut items = Vec::with_capacity(ids.len());
        for id in ids {
            let p = projected
                .remove(id)
                .ok_or_else(|| anyhow!(ErrorCode::InternalError))?;
            let audience = raw
                .remove(id)
                .ok_or_else(|| anyhow!(ErrorCode::InternalError))?;
            let owned = audience.owner_id == actor;
            let linked_account_id = links.get(id).cloned();
            items.push((
                PersonSummary {
                    id: p.id,
                    label: p.label,
                    version: p.version,
                    policy_version: p
                        .policy_version
                        .ok_or_else(|| anyhow!(ErrorCode::InternalError))?,
                    archived: p.archived,
                    linkage: match linked_account_id.as_deref() {
                        Some(a) if a == actor => Linkage::You,
                        Some(_) => Linkage::Other,
                        None => Linkage::None,
                    },
                    linked_account_id,
                    owner: SummaryOwner {
                        account_id: audience.owner_id.clone(),
                        username: audience.owner_username.clone(),
                        you: owned,
                        person: owner_people.get(&audience.owner_id).cloned(),
                    },
                    added_by_you: match added.get(id) {
                        Some(a) if a == actor => AddedBy::Yes,
                        Some(_) => AddedBy::No,
                        None => AddedBy::Unknown,
                    },
                    your_access: access(owned, p.can_edit),
                    field_count: fields.get(id).copied().unwrap_or(0),
                    audience: disclose(owned, home, &audience),
                },
                audience,
            ));
        }
        Ok(items)
    }

    /// `GET /people/summaries`: one page of the viewer's visible people matching `scope`, in id
    /// order. The filter runs over the whole visible set, so chaining `next_after` is complete.
    pub async fn people_summaries(
        &self,
        actor: &str,
        scope: PeopleScope,
        archived: bool,
        after: Option<&str>,
        limit: u16,
    ) -> Result<PersonSummaryPage> {
        ensure!((1..=100).contains(&limit), ErrorCode::InvalidValue);
        if let Some(after) = after {
            identifier(after)?;
        }
        let mut tx = self.task_read().await?;
        Self::epoch(&mut tx, actor).await?;
        let ids: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(summary_id_query(scope)))
            .bind(actor)
            .bind(i64::from(archived))
            .bind(after)
            .bind(i64::from(limit))
            .fetch_all(&mut *tx)
            .await?;
        hook!(self, "people_summary.between_reads", &mut tx);
        let home = Self::people_home(&mut tx, actor).await?;
        let items = Self::summaries_in(&mut tx, actor, &ids, home.as_ref().map(|h| h.id.as_str()))
            .await?
            .into_iter()
            .map(|(summary, _)| summary)
            .collect();
        tx.commit().await?;
        Ok(PersonSummaryPage {
            items,
            next_after: (ids.len() == usize::from(limit))
                .then(|| ids.last().cloned())
                .flatten(),
            home,
        })
    }

    /// `GET /people/{id}/summary`: one visible person's summary and detail rail. A merged id
    /// resolves to its canonical person; anything the viewer cannot see is `not_found`.
    pub async fn person_summary(&self, actor: &str, id: &str) -> Result<PersonSummaryDetail> {
        let mut tx = self.task_read().await?;
        Self::epoch(&mut tx, actor).await?;
        let canonical = Self::canonical_person(&mut tx, id).await?;
        Self::task_resource(&mut tx, actor, &canonical, "person", false).await?;
        let home = Self::people_home(&mut tx, actor).await?;
        let (summary, raw) = Self::summaries_in(
            &mut tx,
            actor,
            std::slice::from_ref(&canonical),
            home.as_ref().map(|h| h.id.as_str()),
        )
        .await?
        .pop()
        .ok_or_else(|| anyhow!(ErrorCode::InternalError))?;
        let owned = raw.owner_id == actor;
        let viewer = AccountRef {
            account_id: actor.to_owned(),
            username: sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
                .bind(actor)
                .fetch_one(&mut *tx)
                .await?,
        };
        // Member lists are read only for the owner, and only for households the owner is in.
        let mut members: BTreeMap<String, Vec<AccountRef>> = BTreeMap::new();
        if owned {
            let households: Vec<&str> = raw
                .households
                .iter()
                .filter(|h| h.member)
                .map(|h| h.id.as_str())
                .collect();
            for r in rows_in(&mut tx, "SELECT m.household_id,m.account_id,a.username FROM household_memberships m JOIN accounts a ON a.id=m.account_id WHERE m.household_id IN ({ids}) ORDER BY m.household_id,m.account_id", None, &households).await? {
                members.entry(r.get(0)).or_default().push(AccountRef {
                    account_id: r.get(1),
                    username: r.get(2),
                });
            }
        }
        tx.commit().await?;
        let shared_with = shared_with(owned, summary.owner.clone(), &viewer, &raw, &members);
        Ok(PersonSummaryDetail {
            summary,
            shared_with,
        })
    }

    /// `GET /people/{id}/field-audiences`: who can see each field of a visible person that the
    /// viewer can see, under the same disclosure rules, in id order.
    pub async fn person_field_audiences(
        &self,
        actor: &str,
        person: &str,
        archived: bool,
        after: Option<&str>,
        limit: u16,
    ) -> Result<FieldAudiencePage> {
        ensure!((1..=100).contains(&limit), ErrorCode::InvalidValue);
        if let Some(after) = after {
            identifier(after)?;
        }
        let mut tx = self.task_read().await?;
        Self::epoch(&mut tx, actor).await?;
        let canonical = Self::canonical_person(&mut tx, person).await?;
        Self::task_resource(&mut tx, actor, &canonical, "person", false).await?;
        let ids: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT r.id FROM resources r LEFT JOIN resource_grants g ON g.resource_id=r.id AND g.account_id=$1 WHERE r.kind='field' AND r.parent_id=$2 AND r.archived=$3 AND ($4 IS NULL OR r.id>$4) AND {} ORDER BY r.id LIMIT $5", policy::VISIBLE)))
            .bind(actor)
            .bind(&canonical)
            .bind(i64::from(archived))
            .bind(after)
            .bind(i64::from(limit))
            .fetch_all(&mut *tx)
            .await?;
        let home = Self::people_home(&mut tx, actor).await?;
        let mut items = Vec::with_capacity(ids.len());
        if !ids.is_empty() {
            let mut projected = projections(&mut tx, actor, &ids, "field").await?;
            let keys: Vec<&str> = ids.iter().map(String::as_str).collect();
            let mut raw = Self::raw_audiences(&mut tx, actor, &keys).await?;
            for id in &ids {
                let p = projected
                    .remove(id)
                    .ok_or_else(|| anyhow!(ErrorCode::InternalError))?;
                let audience = raw
                    .remove(id)
                    .ok_or_else(|| anyhow!(ErrorCode::InternalError))?;
                let owned = audience.owner_id == actor;
                items.push(FieldAudience {
                    id: p.id,
                    version: p.version,
                    policy_version: p
                        .policy_version
                        .ok_or_else(|| anyhow!(ErrorCode::InternalError))?,
                    archived: p.archived,
                    owner_is_you: owned,
                    your_access: access(owned, p.can_edit),
                    audience: disclose(owned, home.as_ref().map(|h| h.id.as_str()), &audience),
                });
            }
        }
        tx.commit().await?;
        Ok(FieldAudiencePage {
            items,
            next_after: (ids.len() == usize::from(limit))
                .then(|| ids.last().cloned())
                .flatten(),
        })
    }
}

fn access(owned: bool, can_edit: bool) -> Access {
    if owned {
        Access::Owner
    } else if can_edit {
        Access::Edit
    } else {
        Access::View
    }
}
