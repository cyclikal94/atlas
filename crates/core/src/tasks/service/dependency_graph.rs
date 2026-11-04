//! `GET /occurrences/{id}/dependency-graph` (BE-DEPENDENCY-GRAPH): the caller-authorised upstream
//! prerequisite graph of one occurrence, read from one snapshot.
//!
//! Every edge is one stored `occurrence_dependencies` row between two occurrences the caller can
//! see; nothing is derived from order. Hidden occurrences are never returned, expanded or counted,
//! so neither the node budget nor the node order depends on data the caller cannot see. A hidden
//! direct prerequisite contributes only a boolean on its visible dependant.
//!
//! The completion preview (`dependency_preview`) and its token are deliberately untouched: the
//! graph is a display and editor read, not a completion authority.
use super::*;
use crate::error::ErrorCode;
use std::collections::{BTreeMap, VecDeque};

/// Visible nodes in one graph, the root included. Fixed, so a client never sees a limit that
/// depends on anything but the visible graph.
pub const DEPENDENCY_GRAPH_NODE_LIMIT: usize = 200;
/// Largest (and default) `max_depth`.
pub const DEPENDENCY_GRAPH_MAX_DEPTH: u16 = 200;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GraphCoverage {
    /// Every returned node is expanded: the response is the caller's whole visible graph.
    Complete,
    /// At least one returned node is unexpanded (depth or node budget).
    Partial,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeExpansion {
    /// Every edge to this node's visible direct prerequisites is in the response.
    Expanded,
    /// None of this node's prerequisite edges is in the response.
    Unexpanded,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GraphRelation {
    Root,
    /// A stored direct prerequisite of the root (even if it is also reachable transitively).
    Direct,
    Transitive,
}

/// Judged from the caller's view and from direct prerequisites only, as completion enforces.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DependencyState {
    /// The node's own outcome is complete.
    Satisfied,
    /// Some visible direct prerequisite is not satisfied.
    Waiting,
    /// Every visible direct prerequisite is satisfied, but one or more is hidden from the caller.
    Restricted,
    /// No unsatisfied or hidden direct prerequisite.
    Ready,
    /// Unexpanded and not satisfied.
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DependencyEdge {
    pub prerequisite_id: String,
    pub dependant_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DependencyGraphTask {
    pub id: String,
    pub title: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DependencyGraphNode {
    pub occurrence_id: String,
    pub task_id: String,
    pub slot: Slot,
    pub version: i64,
    pub can_edit: bool,
    pub strict: bool,
    pub depth: u16,
    pub relation: GraphRelation,
    pub outcome: Outcome,
    pub prerequisites: NodeExpansion,
    pub restricted_prerequisites: Option<bool>,
    pub dependency_state: DependencyState,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DependencyGraph {
    pub root_id: String,
    pub nodes: Vec<DependencyGraphNode>,
    pub edges: Vec<DependencyEdge>,
    pub tasks: Vec<DependencyGraphTask>,
    pub direct_prerequisites: Vec<String>,
    pub coverage: GraphCoverage,
    pub restricted: bool,
    pub max_depth: u16,
}

/// One visible node during traversal.
struct Visit {
    projection: Projection,
    depth: u16,
    /// Visible stored direct prerequisites, ascending.
    visible: Vec<String>,
    /// Some stored direct prerequisite is hidden from the caller.
    hidden: bool,
    expanded: bool,
}

impl Store {
    pub async fn dependency_graph(
        &self,
        actor: &str,
        occurrence: &str,
        max_depth: u16,
        now: i64,
    ) -> Result<DependencyGraph> {
        ensure!(
            (1..=DEPENDENCY_GRAPH_MAX_DEPTH).contains(&max_depth),
            ErrorCode::InvalidValue
        );
        let mut tx = self.task_read().await?;
        Self::epoch(&mut tx, actor).await?;
        let root = Self::task_resource(&mut tx, actor, occurrence, "occurrence", false).await?;
        hook!(self, "dependency_graph.after_root_read", &mut tx);
        // Negative control only: expand on a second, later snapshot.
        #[cfg(feature = "test-hooks")]
        let mut later = if self.hooks.splits_snapshot_reads() {
            let mut later = self.pool.begin().await?;
            if !self.sqlite {
                sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                    .execute(&mut *later)
                    .await?;
            }
            Some(later)
        } else {
            None
        };
        #[cfg(feature = "test-hooks")]
        let reader = later.as_mut().unwrap_or(&mut tx);
        #[cfg(not(feature = "test-hooks"))]
        let reader = &mut tx;
        let graph = Self::dependency_graph_in(reader, actor, root, max_depth, now).await?;
        #[cfg(feature = "test-hooks")]
        if let Some(later) = later {
            later.commit().await?;
        }
        tx.commit().await?;
        Ok(graph)
    }

    async fn dependency_graph_in(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        root: Projection,
        max_depth: u16,
        now: i64,
    ) -> Result<DependencyGraph> {
        let root_id = root.id.clone();
        let mut visits = vec![Visit {
            projection: root,
            depth: 0,
            visible: Vec::new(),
            hidden: false,
            expanded: false,
        }];
        let mut index = BTreeMap::from([(root_id.clone(), 0usize)]);
        let mut queue = VecDeque::from([0usize]);
        while let Some(i) = queue.pop_front() {
            let stored: Vec<String> = sqlx::query_scalar(
                "SELECT prerequisite_id FROM occurrence_dependencies WHERE occurrence_id=$1 ORDER BY prerequisite_id",
            )
            .bind(&visits[i].projection.id)
            .fetch_all(&mut **tx)
            .await?;
            let mut found = Self::subset_batch(tx, actor, &stored.iter().cloned().collect())
                .await?
                .into_iter()
                .filter(|(_, p)| p.kind == "occurrence")
                .collect::<BTreeMap<_, _>>();
            let visible: Vec<String> = stored
                .iter()
                .filter(|id| found.contains_key(*id))
                .cloned()
                .collect();
            let fresh: Vec<String> = visible
                .iter()
                .filter(|id| !index.contains_key(*id))
                .cloned()
                .collect();
            let depth = visits[i].depth;
            // All or nothing: a node is expanded only with every one of its visible prerequisites.
            let fits = fresh.is_empty()
                || (depth < max_depth && fresh.len() <= DEPENDENCY_GRAPH_NODE_LIMIT - visits.len());
            let visit = &mut visits[i];
            visit.hidden = visible.len() < stored.len();
            visit.visible = visible;
            visit.expanded = fits;
            if fits {
                for id in fresh {
                    let projection = found
                        .remove(&id)
                        .ok_or_else(|| anyhow!(ErrorCode::InternalError))?;
                    index.insert(id, visits.len());
                    queue.push_back(visits.len());
                    visits.push(Visit {
                        projection,
                        depth: depth + 1,
                        visible: Vec::new(),
                        hidden: false,
                        expanded: false,
                    });
                }
            }
        }
        // No final pass is needed for the invariant "expanded exactly when every visible
        // prerequisite is present": a node refused for budget needed more new nodes than can
        // still be added, and one refused for depth is at `max_depth`, after which breadth-first
        // order processes only nodes that cannot add anything. Neither can later be completed.
        let mut edges = Vec::new();
        for visit in visits.iter().filter(|v| v.expanded) {
            for prerequisite in &visit.visible {
                edges.push(DependencyEdge {
                    prerequisite_id: prerequisite.clone(),
                    dependant_id: visit.projection.id.clone(),
                });
            }
        }
        edges.sort_by(|a, b| {
            (&a.dependant_id, &a.prerequisite_id).cmp(&(&b.dependant_id, &b.prerequisite_id))
        });
        let mut outcomes = BTreeMap::new();
        let mut datas = Vec::with_capacity(visits.len());
        for visit in &visits {
            let data: OccurrenceData = serde_json::from_value(visit.projection.value.clone())?;
            let outcome =
                Self::outcome_for(tx, actor, &visit.projection.id, &data, now, false).await?;
            outcomes.insert(visit.projection.id.clone(), outcome);
            datas.push(data);
        }
        let direct: BTreeSet<&String> = visits[0].visible.iter().collect();
        let mut tasks = BTreeMap::new();
        let mut nodes = Vec::with_capacity(visits.len());
        for (visit, data) in visits.iter().zip(datas) {
            let id = &visit.projection.id;
            if !tasks.contains_key(&data.task_id) {
                // Ancestor visibility makes this infallible; an error is propagated, not defaulted.
                let task = Self::task_resource(tx, actor, &data.task_id, "task", false).await?;
                tasks.insert(data.task_id.clone(), task.label);
            }
            let strict = sqlx::query_scalar::<_, i64>(
                "SELECT strict FROM dependency_rules WHERE occurrence_id=$1",
            )
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .unwrap_or(0)
                == 1;
            let outcome = outcomes[id];
            let dependency_state = if outcome == Outcome::Complete {
                DependencyState::Satisfied
            } else if !visit.expanded {
                DependencyState::Unknown
            } else if visit
                .visible
                .iter()
                .any(|p| outcomes.get(p) != Some(&Outcome::Complete))
            {
                DependencyState::Waiting
            } else if visit.hidden {
                DependencyState::Restricted
            } else {
                DependencyState::Ready
            };
            nodes.push(DependencyGraphNode {
                occurrence_id: id.clone(),
                task_id: data.task_id,
                slot: data.slot,
                version: visit.projection.version,
                can_edit: visit.projection.can_edit,
                strict,
                // Breadth-first discovery depth is the shortest path over the returned edges:
                // an earlier expanded node with an edge to this one would have discovered it.
                depth: visit.depth,
                relation: if *id == root_id {
                    GraphRelation::Root
                } else if direct.contains(id) {
                    GraphRelation::Direct
                } else {
                    GraphRelation::Transitive
                },
                outcome,
                prerequisites: if visit.expanded {
                    NodeExpansion::Expanded
                } else {
                    NodeExpansion::Unexpanded
                },
                restricted_prerequisites: visit.expanded.then_some(visit.hidden),
                dependency_state,
            });
        }
        Ok(DependencyGraph {
            root_id,
            direct_prerequisites: visits[0].visible.clone(),
            coverage: if visits.iter().all(|v| v.expanded) {
                GraphCoverage::Complete
            } else {
                GraphCoverage::Partial
            },
            restricted: visits.iter().any(|v| v.expanded && v.hidden),
            nodes,
            edges,
            tasks: tasks
                .into_iter()
                .map(|(id, title)| DependencyGraphTask { id, title })
                .collect(),
            max_depth,
        })
    }
}
