use crate::error::ErrorCode;
mod commands;
mod dependencies;
pub use dependencies::{CompletionMode, DependencyItem, DependencyPreview};
mod lifecycle;
mod reads;
mod rotas;
pub use rotas::{RotaAssignment, RotaState};
mod successors;
mod timers;
use lifecycle::windows;
pub use timers::TimerSession;

use super::*;
use crate::{
    Projection, Store, content, identifier,
    policy::{Policy, PrincipalGrant},
    receipt_digest,
};
use chrono::{DateTime, Duration, NaiveTime, Utc};
use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskCommand {
    SetRota {
        task_id: String,
        expected_version: i64,
        participants: Vec<String>,
    },
    RotaConsent {
        task_id: String,
        expected_version: i64,
        accepted: bool,
    },
    CancelTimer {
        occurrence_id: String,
        session_id: String,
        expected_version: i64,
    },
    StartTimer {
        occurrence_id: String,
        session_id: String,
        started_at: i64,
    },
    StopTimer {
        occurrence_id: String,
        session_id: String,
        expected_version: i64,
        stopped_at: i64,
    },
    SetDependencies {
        occurrence_id: String,
        expected_version: i64,
        prerequisites: Vec<String>,
        strict: bool,
    },
    CompleteDependencies {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_evidence: Option<Evidence>,
        occurrence_id: String,
        preview_token: String,
        mode: CompletionMode,
        happened_at: i64,
    },
    CreateTask {
        id: String,
        execution_id: String,
        title: String,
        definition: Definition,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<crate::calendars::Anchor>,
        initial_policy: Option<Policy>,
    },
    ReviseTask {
        id: String,
        expected_version: i64,
        title: String,
        definition: Definition,
    },
    Enrol {
        task_id: String,
        expected_version: i64,
        active: bool,
        aggregate_consent: bool,
        initial_policy: Option<Policy>,
    },
    Materialise {
        task_id: String,
        through_date: String,
        limit: u16,
    },
    Record {
        occurrence_id: String,
        subject_account_id: String,
        entry_id: String,
        evidence: Evidence,
        replaces: Option<String>,
        expected_version: Option<i64>,
        happened_at: i64,
    },
    Exclude {
        occurrence_id: String,
        expected_version: i64,
        excluded: bool,
    },
    Resolve {
        occurrence_id: String,
        expected_version: i64,
        resolved: bool,
    },
    ReviseOccurrence {
        id: String,
        expected_version: i64,
        definition: Definition,
        date: Option<String>,
        time: Option<String>,
    },
    CreateList {
        id: String,
        name: String,
        initial_policy: Option<Policy>,
    },
    EditList {
        id: String,
        expected_version: i64,
        name: String,
    },
    ListItem {
        list_id: String,
        expected_version: i64,
        task_id: String,
        included: bool,
    },
    Archive {
        id: String,
        expected_version: i64,
        archived: bool,
    },
    PutField {
        id: String,
        parent_id: String,
        expected_version: Option<i64>,
        label: String,
        value: FieldValue,
        initial_policy: Option<Policy>,
    },
}
impl TaskCommand {
    pub fn online(&self) -> bool {
        matches!(
            self,
            Self::Enrol { .. } | Self::SetRota { .. } | Self::RotaConsent { .. }
        ) || match self {
            Self::CreateTask { initial_policy, .. }
            | Self::CreateList { initial_policy, .. }
            | Self::PutField { initial_policy, .. } => initial_policy
                .as_ref()
                .is_some_and(|p| !p.grants.is_empty() || !p.exclude_accounts.is_empty()),
            _ => false,
        }
    }
}
pub use crate::resources::FieldValue;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OccurrenceData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rota: Option<RotaAssignment>,
    pub task_id: String,
    pub definition: Definition,
    pub slot: Slot,
    pub opens_at: Option<i64>,
    pub closes_at: Option<i64>,
    pub participants: Vec<String>,
    pub covered_by: Option<String>,
    pub resolved: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub evidence: Evidence,
    pub happened_at: i64,
    pub recorded_by: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stream {
    pub subject_account_id: String,
    pub entries: Vec<Entry>,
    pub excluded: bool,
    pub aggregate_consent: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProgressState {
    pub subject_account_id: String,
    pub recent_entries: Vec<Entry>,
    pub entry_count: usize,
    pub excluded: bool,
    pub aggregate_consent: bool,
    pub action: Progress,
    pub period: Progress,
}
fn progress_state(
    stream: &Stream,
    goal: &Goal,
    open: Option<i64>,
    close: Option<i64>,
) -> Result<ProgressState> {
    let all = stream
        .entries
        .iter()
        .map(|e| e.evidence.clone())
        .collect::<Vec<_>>();
    let period = stream
        .entries
        .iter()
        .filter(|e| {
            open.is_none_or(|v| e.happened_at >= v) && close.is_none_or(|v| e.happened_at < v)
        })
        .map(|e| e.evidence.clone())
        .collect::<Vec<_>>();
    Ok(ProgressState {
        subject_account_id: stream.subject_account_id.clone(),
        recent_entries: stream
            .entries
            .iter()
            .rev()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect(),
        entry_count: stream.entries.len(),
        excluded: stream.excluded,
        aggregate_consent: stream.aggregate_consent,
        action: evaluate(goal, &all, false)?,
        period: evaluate(goal, &period, false)?,
    })
}
#[derive(Debug, Serialize)]
pub struct TaskResult {
    pub revision: i64,
}
#[derive(Debug, Serialize)]
pub struct OccurrenceView {
    pub id: String,
    pub version: i64,
    pub can_edit: bool,
    pub data: OccurrenceData,
    pub outcome: Outcome,
    pub period_outcome: Outcome,
    pub actionable: bool,
}
#[derive(Debug, Serialize)]
pub struct ViewPage {
    pub items: Vec<OccurrenceView>,
    pub next_after: Option<String>,
    pub revision: i64,
    pub pending_tasks: Vec<String>,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewFilter {
    pub task_id: Option<String>,
    pub list_id: Option<String>,
    pub day: Option<String>,
    pub timezone: Option<String>,
    pub state: Option<String>,
    pub after: Option<String>,
    pub limit: Option<u16>,
    pub revision: Option<i64>,
    pub scope: Option<String>,
}

impl Store {
    pub(crate) async fn related_lists(
        tx: &mut Transaction<'_, Any>,
        touched: &mut BTreeSet<String>,
    ) -> Result<()> {
        for id in touched.clone() {
            touched.extend(
                sqlx::query_scalar::<_, String>("SELECT list_id FROM list_items WHERE task_id=$1")
                    .bind(id)
                    .fetch_all(&mut **tx)
                    .await?,
            );
        }
        Ok(())
    }
    pub(crate) async fn list_value(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        id: &str,
    ) -> Result<String> {
        let ids:Vec<String>=sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT r.id FROM list_items li JOIN resources r ON r.id=li.task_id LEFT JOIN resource_grants g ON g.resource_id=r.id AND g.account_id=$1 WHERE li.list_id=$2 AND {} ORDER BY r.id",crate::policy::VISIBLE))).bind(actor).bind(id).fetch_all(&mut **tx).await?;
        Ok(serde_json::json!({"task_ids":ids}).to_string())
    }
    pub(crate) async fn task_resource(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        id: &str,
        kind: &str,
        edit: bool,
    ) -> Result<Projection> {
        identifier(id)?;
        let p = Self::subset(tx, actor, &BTreeSet::from([id.to_owned()]))
            .await?
            .remove(id)
            .ok_or_else(|| anyhow!(ErrorCode::NotFound))?;
        ensure!(kind.is_empty() || p.kind == kind, ErrorCode::NotFound);
        ensure!(!edit || p.can_edit, ErrorCode::Forbidden);
        Ok(p)
    }
    #[allow(clippy::too_many_arguments)] // One validated resource row and its initial policy.
    pub(crate) async fn new_resource(
        tx: &mut Transaction<'_, Any>,
        owner: &str,
        id: &str,
        parent: Option<&str>,
        kind: &str,
        label: &str,
        value: &str,
        policy: &Policy,
    ) -> Result<()> {
        identifier(id)?;
        content(label, value)?;
        ensure!(value.len() <= 8192, ErrorCode::InvalidValue);
        sqlx::query("INSERT INTO resources(id,owner_id,parent_id,kind,label,value) VALUES ($1,$2,$3,$4,$5,$6)").bind(id).bind(owner).bind(parent).bind(kind).bind(label).bind(value).execute(&mut **tx).await?;
        Self::put_policy(tx, owner, id, policy).await?;
        sqlx::query("UPDATE sync_clock SET resource_count=resource_count+1 WHERE id=1")
            .execute(&mut **tx)
            .await?;
        ensure!(
            sqlx::query_scalar::<_, i64>("SELECT resource_count FROM sync_clock WHERE id=1")
                .fetch_one(&mut **tx)
                .await?
                <= 10000,
            ErrorCode::SliceCapacity
        );
        Ok(())
    }
    pub(crate) async fn value(
        tx: &mut Transaction<'_, Any>,
        id: &str,
        label: &str,
        value: &str,
    ) -> Result<()> {
        content(label, value)?;
        ensure!(value.len() <= 8192, ErrorCode::InvalidValue);
        sqlx::query("UPDATE resources SET label=$1,value=$2,version=version+1 WHERE id=$3")
            .bind(label)
            .bind(value)
            .bind(id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
    pub(crate) async fn copied_policy(tx: &mut Transaction<'_, Any>, id: &str) -> Result<Policy> {
        let mut grants = Vec::new();
        for r in sqlx::query("SELECT account_id,can_edit FROM resource_grants WHERE resource_id=$1 ORDER BY account_id").bind(id).fetch_all(&mut **tx).await? {grants.push(PrincipalGrant::Account{id:r.get(0),edit:r.get::<i64,_>(1)==1});}
        for r in sqlx::query("SELECT household_id,can_edit FROM resource_household_grants WHERE resource_id=$1 ORDER BY household_id").bind(id).fetch_all(&mut **tx).await? {grants.push(PrincipalGrant::Household{id:r.get(0),edit:r.get::<i64,_>(1)==1});}
        let exclude_accounts = sqlx::query_scalar(
            "SELECT account_id FROM resource_exclusions WHERE resource_id=$1 ORDER BY account_id",
        )
        .bind(id)
        .fetch_all(&mut **tx)
        .await?;
        Ok(Policy {
            grants,
            exclude_accounts,
        })
    }
    pub(crate) async fn execution(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        task: &str,
        edit: bool,
    ) -> Result<Projection> {
        let id: Option<String> =
            sqlx::query_scalar("SELECT execution_id FROM tasks WHERE task_id=$1")
                .bind(task)
                .fetch_optional(&mut **tx)
                .await?;
        Self::task_resource(
            tx,
            actor,
            &id.ok_or_else(|| anyhow!(ErrorCode::NotFound))?,
            "execution",
            edit,
        )
        .await
    }
    pub(crate) async fn task_touched(
        tx: &mut Transaction<'_, Any>,
        id: &str,
    ) -> Result<BTreeSet<String>> {
        let mut ids = BTreeSet::from([id.to_owned()]);
        ids.extend(
            sqlx::query_scalar::<_, String>(
                "SELECT resource_id FROM resource_ancestors WHERE ancestor_id=$1",
            )
            .bind(id)
            .fetch_all(&mut **tx)
            .await?,
        );
        Self::related_lists(tx, &mut ids).await?;
        Ok(ids)
    }
}

/// Public deterministic identities let offline clients queue creation and progress.
/// UUIDv5 is used only for domain IDs, never sessions or other credentials.
pub fn occurrence_id(task: &str, slot_key: &str) -> Result<String> {
    identifier(task)?;
    ensure!(
        !slot_key.is_empty() && slot_key.len() <= 100,
        ErrorCode::InvalidValue
    );
    Ok(Uuid::new_v5(
        &Uuid::parse_str(task)?,
        format!("atlas-occurrence-v1:{slot_key}").as_bytes(),
    )
    .to_string())
}
pub fn progress_id(occurrence: &str, account: &str) -> Result<String> {
    identifier(occurrence)?;
    identifier(account)?;
    Ok(Uuid::new_v5(
        &Uuid::parse_str(occurrence)?,
        format!("atlas-progress-v1:{account}").as_bytes(),
    )
    .to_string())
}
