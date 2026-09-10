use super::*;
use crate::error::ErrorCode;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompletionMode {
    RequireSatisfied,
    CompletePrerequisites,
    AdvisoryOverride,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DependencyItem {
    pub occurrence_id: String,
    pub task_id: String,
    pub complete: bool,
    /// Only the caller's own checkbox stream is offered. Editing another
    /// participant's stream is an explicit ordinary progress operation.
    pub can_complete: bool,
    pub progress_version: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DependencyPreview {
    pub token: String,
    /// Prerequisites precede dependants; the requested occurrence is last.
    pub items: Vec<DependencyItem>,
    pub unavailable: bool,
    pub strict: bool,
}
impl Store {
    pub async fn dependency_preview(
        &self,
        actor: &str,
        occurrence: &str,
        now: i64,
    ) -> Result<DependencyPreview> {
        let mut tx = self.task_read().await?;
        Self::epoch(&mut tx, actor).await?;
        let result = Self::dependency_preview_in(&mut tx, actor, occurrence, now).await?;
        tx.commit().await?;
        Ok(result)
    }
    pub(super) async fn dependency_preview_in(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        occurrence: &str,
        now: i64,
    ) -> Result<DependencyPreview> {
        Self::task_resource(tx, actor, occurrence, "occurrence", false).await?;
        let mut pending = vec![(occurrence.to_owned(), false)];
        let mut visited = BTreeSet::new();
        let mut items = Vec::new();
        let mut fingerprint = Vec::new();
        let mut unavailable = false;
        while let Some((id, expanded)) = pending.pop() {
            if !expanded && !visited.insert(id.clone()) {
                continue;
            }
            ensure!(visited.len() <= 200, ErrorCode::ExpansionLimit);
            let p = match Self::task_resource(tx, actor, &id, "occurrence", false).await {
                Ok(p) => p,
                Err(e) if e.to_string() == "not_found" => {
                    unavailable = true;
                    continue;
                }
                Err(e) => return Err(e),
            };
            if !expanded {
                pending.push((id.clone(), true));
                let dependencies: Vec<String> = sqlx::query_scalar("SELECT prerequisite_id FROM occurrence_dependencies WHERE occurrence_id=$1 ORDER BY prerequisite_id DESC").bind(&id).fetch_all(&mut **tx).await?;
                pending.extend(dependencies.into_iter().map(|v| (v, false)));
                continue;
            }
            let data: OccurrenceData = serde_json::from_value(p.value.clone())?;
            let complete =
                Self::outcome_for(tx, actor, &id, &data, now, false).await? == Outcome::Complete;
            let stream = match Self::participant_stream(tx, actor, &id, actor, false).await {
                Ok(p) => Some(p),
                Err(e) if e.to_string() == "not_found" => None,
                Err(e) => return Err(e),
            };
            let on_demand = stream.is_none()
                && p.can_edit
                && data.definition.participation == Participation::Anyone;
            let can_complete = matches!(data.definition.goal, Goal::Checkbox)
                && data.covered_by.is_none()
                && (on_demand || stream.as_ref().is_some_and(|p| p.can_edit));
            // Only caller-visible state contributes to the public token. Hidden
            // progress must not become a change oracle through an opaque digest.
            let touched = Self::task_touched(tx, &id).await?;
            let visible = Self::subset(tx, actor, &touched).await?;
            fingerprint.push(serde_json::to_value(visible)?);
            items.push(DependencyItem {
                occurrence_id: id,
                task_id: data.task_id,
                complete,
                can_complete,
                progress_version: stream.map(|p| p.version).or(on_demand.then_some(1)),
            });
        }
        let strict = sqlx::query_scalar::<_, i64>(
            "SELECT strict FROM dependency_rules WHERE occurrence_id=$1",
        )
        .bind(occurrence)
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(0)
            == 1;
        let token = receipt_digest(&serde_json::to_string(&(
            actor,
            occurrence,
            &items,
            &fingerprint,
            unavailable,
            strict,
        ))?);
        Ok(DependencyPreview {
            token,
            items,
            unavailable,
            strict,
        })
    }
    pub(super) async fn set_dependencies(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        occurrence: &str,
        expected_version: i64,
        prerequisites: &[String],
        strict: bool,
    ) -> Result<()> {
        ensure!(
            prerequisites.len() <= 32
                && prerequisites.iter().collect::<BTreeSet<_>>().len() == prerequisites.len(),
            ErrorCode::InvalidValue
        );
        let p = Self::task_resource(tx, actor, occurrence, "occurrence", true).await?;
        ensure!(p.version == expected_version, ErrorCode::Conflict);
        for prerequisite in prerequisites {
            Self::task_resource(tx, actor, prerequisite, "occurrence", false).await?;
            ensure!(prerequisite != occurrence, ErrorCode::InvalidValue);
            // begin_serial serialises graph mutation on both database engines.
            let cycle: i64 = sqlx::query_scalar("WITH RECURSIVE reachable(id) AS (SELECT prerequisite_id FROM occurrence_dependencies WHERE occurrence_id=$1 UNION SELECT d.prerequisite_id FROM occurrence_dependencies d JOIN reachable r ON d.occurrence_id=r.id) SELECT COUNT(*) FROM reachable WHERE id=$2").bind(prerequisite).bind(occurrence).fetch_one(&mut **tx).await?;
            ensure!(cycle == 0, ErrorCode::Conflict);
        }
        sqlx::query("DELETE FROM occurrence_dependencies WHERE occurrence_id=$1")
            .bind(occurrence)
            .execute(&mut **tx)
            .await?;
        for prerequisite in prerequisites {
            sqlx::query("INSERT INTO occurrence_dependencies VALUES ($1,$2)")
                .bind(occurrence)
                .bind(prerequisite)
                .execute(&mut **tx)
                .await?;
        }
        sqlx::query("INSERT INTO dependency_rules VALUES ($1,$2) ON CONFLICT(occurrence_id) DO UPDATE SET strict=excluded.strict").bind(occurrence).bind(i64::from(strict)).execute(&mut **tx).await?;
        // Publish a revision without embedding potentially private prerequisite IDs.
        Self::value(tx, occurrence, &p.label, &p.value.to_string()).await
    }
    pub(super) async fn require_dependencies(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        occurrence: &str,
        now: i64,
    ) -> Result<()> {
        let prerequisites: Vec<String> = sqlx::query_scalar(
            "SELECT prerequisite_id FROM occurrence_dependencies WHERE occurrence_id=$1",
        )
        .bind(occurrence)
        .fetch_all(&mut **tx)
        .await?;
        for id in prerequisites {
            let p = Self::task_resource(tx, actor, &id, "occurrence", false)
                .await
                .map_err(|e| {
                    if e.to_string() == "not_found" {
                        anyhow!(ErrorCode::Conflict)
                    } else {
                        e
                    }
                })?;
            let data = serde_json::from_value(p.value.clone())?;
            ensure!(
                Self::outcome_for(tx, actor, &id, &data, now, false).await? == Outcome::Complete,
                ErrorCode::Conflict
            );
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn complete_dependencies(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        operation: &str,
        occurrence: &str,
        token: &str,
        mode: CompletionMode,
        root_evidence: Option<&Evidence>,
        happened_at: i64,
        now: i64,
        defaults: &crate::policy::Defaults,
        touched: &mut BTreeSet<String>,
    ) -> Result<()> {
        let preview = Self::dependency_preview_in(tx, actor, occurrence, now).await?;
        ensure!(preview.token == token, ErrorCode::Conflict);
        ensure!(
            mode != CompletionMode::AdvisoryOverride || !preview.strict,
            ErrorCode::Conflict
        );
        if mode == CompletionMode::CompletePrerequisites {
            ensure!(!preview.unavailable, ErrorCode::Conflict);
        }
        for item in preview.items {
            let root = item.occurrence_id == occurrence;
            if !root && (mode != CompletionMode::CompletePrerequisites || item.complete) {
                continue;
            }
            if mode != CompletionMode::AdvisoryOverride {
                Self::require_dependencies(tx, actor, &item.occurrence_id, now).await?;
            }
            ensure!(
                (root && root_evidence.is_some()) || item.can_complete,
                ErrorCode::Conflict
            );
            let entry_id = Uuid::new_v5(
                &Uuid::parse_str(operation)?,
                format!("dependency-completion:{}:{actor}", item.occurrence_id).as_bytes(),
            )
            .to_string();
            Self::record_entry(
                tx,
                actor,
                defaults,
                &TaskCommand::Record {
                    occurrence_id: item.occurrence_id,
                    subject_account_id: actor.into(),
                    entry_id,
                    evidence: if root {
                        root_evidence
                            .cloned()
                            .unwrap_or(Evidence::Checkbox { complete: true })
                    } else {
                        Evidence::Checkbox { complete: true }
                    },
                    replaces: None,
                    expected_version: item.progress_version,
                    happened_at,
                },
                now,
                touched,
            )
            .await?;
        }
        Ok(())
    }
}
