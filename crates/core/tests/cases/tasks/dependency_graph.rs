//! BE-DEPENDENCY-GRAPH: `Store::dependency_graph`, on SQLite and (under
//! `scripts/test-postgres.sh`) PostgreSQL.
//!
//! Every graph is checked by `verify`, an oracle that knows only the stored
//! `occurrence_dependencies` rows, resource versions and which occurrences the scenario hid from
//! the caller. It does not reuse the traversal: edges, expansion, restriction, depth, relation,
//! coverage and status are each recomputed from those facts.
use anyhow::Result;
use atlas_core::{
    Store,
    households::ManagementCommand,
    policy::{Policy, PrincipalGrant},
    tasks::*,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use crate::support::database::postgres;
use crate::support::task_workflows::{account, complete, id, set, setup};

const NOW: i64 = 1788868800;

fn checkbox() -> Definition {
    Definition {
        schedule: Schedule {
            start_date: None,
            time: None,
            timezone: "Europe/Vienna".into(),
            repeat: None,
        },
        goal: Goal::Checkbox,
        carry: Carry::RetainOne,
        participation: Participation::Personal,
        open_days_before: 0,
        close_days_after: 1,
        allow_streak_exclusions: false,
    }
}

/// A once task with a title; returns its occurrence.
async fn make(s: &Store, a: &str, title: &str, policy: Option<Policy>) -> Result<String> {
    make_with(s, a, title, checkbox(), policy).await
}

async fn make_with(
    s: &Store,
    a: &str,
    title: &str,
    definition: Definition,
    policy: Option<Policy>,
) -> Result<String> {
    let t = id();
    s.task_command(
        a,
        &id(),
        &TaskCommand::CreateTask {
            id: t.clone(),
            execution_id: id(),
            title: title.into(),
            anchor: None,
            initial_policy: policy,
            definition,
        },
        None,
        NOW,
    )
    .await?;
    occurrence_id(&t, "once")
}

fn grant(accounts: &[&str], edit: bool) -> Policy {
    Policy {
        grants: accounts
            .iter()
            .map(|a| PrincipalGrant::Account {
                id: (*a).into(),
                edit,
            })
            .collect(),
        exclude_accounts: vec![],
    }
}

async fn version(s: &Store, resource: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT version FROM resources WHERE id=$1")
            .bind(resource)
            .fetch_one(&s.pool)
            .await?,
    )
}

/// Replace the policy of one resource (not its ancestors).
async fn replace(s: &Store, owner: &str, resource: &str, policy: Policy) -> Result<()> {
    let v = version(s, resource).await?;
    s.management(
        owner,
        &id(),
        &[ManagementCommand::ReplacePolicy {
            id: resource.into(),
            expected_version: v,
            policy,
        }],
        NOW,
    )
    .await?;
    Ok(())
}

/// The occurrence and every ancestor (task, execution).
async fn lineage(s: &Store, occurrence: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT ancestor_id FROM (SELECT ancestor_id,depth FROM resource_ancestors WHERE resource_id=$1 UNION ALL SELECT $1,0) ancestors ORDER BY depth DESC",
    )
    .bind(occurrence)
    .fetch_all(&s.pool)
    .await?)
}

async fn graph(s: &Store, actor: &str, root: &str, depth: u16) -> Result<DependencyGraph> {
    s.dependency_graph(actor, root, depth, NOW).await
}

async fn stored(s: &Store) -> Result<BTreeMap<String, Vec<String>>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT occurrence_id,prerequisite_id FROM occurrence_dependencies ORDER BY occurrence_id,prerequisite_id",
    )
    .fetch_all(&s.pool)
    .await?;
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (dependant, prerequisite) in rows {
        map.entry(dependant).or_default().push(prerequisite);
    }
    Ok(map)
}

fn node<'a>(g: &'a DependencyGraph, occurrence: &str) -> &'a DependencyGraphNode {
    g.nodes
        .iter()
        .find(|n| n.occurrence_id == occurrence)
        .unwrap_or_else(|| panic!("{occurrence} is not a node"))
}

fn ids(g: &DependencyGraph) -> Vec<String> {
    g.nodes.iter().map(|n| n.occurrence_id.clone()).collect()
}

fn edge_set(g: &DependencyGraph) -> BTreeSet<(String, String)> {
    g.edges
        .iter()
        .map(|e| (e.prerequisite_id.clone(), e.dependant_id.clone()))
        .collect()
}

fn pair(prerequisite: &str, dependant: &str) -> (String, String) {
    (prerequisite.to_owned(), dependant.to_owned())
}

/// The independent oracle. `hidden` names every occurrence the scenario hid from `actor`.
async fn verify(
    s: &Store,
    actor: &str,
    g: &DependencyGraph,
    hidden: &BTreeSet<String>,
) -> Result<()> {
    let rows = stored(s).await?;
    let stored_of = |id: &str| rows.get(id).cloned().unwrap_or_default();
    let present: BTreeSet<&str> = g.nodes.iter().map(|n| n.occurrence_id.as_str()).collect();
    assert_eq!(present.len(), g.nodes.len(), "each node appears once");
    assert!(g.nodes.len() <= DEPENDENCY_GRAPH_NODE_LIMIT);
    assert_eq!(g.nodes[0].occurrence_id, g.root_id, "the root comes first");
    let body = serde_json::to_string(g)?;
    for h in hidden {
        assert!(!present.contains(h.as_str()), "hidden {h} returned");
        assert!(!body.contains(h.as_str()), "hidden {h} disclosed");
    }
    // Edges: exactly the stored rows from each expanded node to its visible prerequisites.
    let mut expected = BTreeSet::new();
    for n in &g.nodes {
        let visible: Vec<String> = stored_of(&n.occurrence_id)
            .into_iter()
            .filter(|p| !hidden.contains(p))
            .collect();
        let restricted = visible.len() < stored_of(&n.occurrence_id).len();
        match n.prerequisites {
            NodeExpansion::Expanded => {
                for p in &visible {
                    assert!(
                        present.contains(p.as_str()),
                        "expanded {} lacks visible prerequisite {p}",
                        n.occurrence_id
                    );
                    expected.insert(pair(p, &n.occurrence_id));
                }
                assert_eq!(n.restricted_prerequisites, Some(restricted));
            }
            NodeExpansion::Unexpanded => {
                assert_eq!(n.restricted_prerequisites, None);
                assert!(
                    visible.iter().any(|p| !present.contains(p.as_str())),
                    "{} is unexpanded although every prerequisite is present",
                    n.occurrence_id
                );
            }
        }
        // Stored facts reported as they are.
        assert_eq!(n.version, version(s, &n.occurrence_id).await?);
        let strict: Option<i64> =
            sqlx::query_scalar("SELECT strict FROM dependency_rules WHERE occurrence_id=$1")
                .bind(&n.occurrence_id)
                .fetch_optional(&s.pool)
                .await?;
        assert_eq!(n.strict, strict == Some(1));
    }
    assert_eq!(
        edge_set(g),
        expected,
        "edges are exactly the stored visible rows"
    );
    assert_eq!(g.edges.len(), expected.len(), "no duplicate edges");
    let mut sorted = g.edges.clone();
    sorted.sort_by(|a, b| {
        (&a.dependant_id, &a.prerequisite_id).cmp(&(&b.dependant_id, &b.prerequisite_id))
    });
    assert_eq!(sorted, g.edges);
    // Every non-root node is a prerequisite of an expanded node: nothing beyond a hidden node.
    for n in g.nodes.iter().skip(1) {
        assert!(
            g.edges.iter().any(|e| e.prerequisite_id == n.occurrence_id),
            "{} is not reachable through returned edges",
            n.occurrence_id
        );
    }
    // Depth is the shortest path over the edges; relation follows the root's direct list.
    let mut depth = BTreeMap::from([(g.root_id.clone(), 0u16)]);
    let mut queue = VecDeque::from([g.root_id.clone()]);
    while let Some(id) = queue.pop_front() {
        for e in g.edges.iter().filter(|e| e.dependant_id == id) {
            if !depth.contains_key(&e.prerequisite_id) {
                depth.insert(e.prerequisite_id.clone(), depth[&id] + 1);
                queue.push_back(e.prerequisite_id.clone());
            }
        }
    }
    let direct: Vec<String> = stored_of(&g.root_id)
        .into_iter()
        .filter(|p| !hidden.contains(p))
        .collect();
    assert_eq!(g.direct_prerequisites, direct);
    for n in &g.nodes {
        assert_eq!(n.depth, depth[&n.occurrence_id], "{}", n.occurrence_id);
        assert!(n.depth <= g.max_depth);
        let relation = if n.occurrence_id == g.root_id {
            GraphRelation::Root
        } else if direct.contains(&n.occurrence_id) {
            GraphRelation::Direct
        } else {
            GraphRelation::Transitive
        };
        assert_eq!(n.relation, relation);
    }
    let unexpanded = g
        .nodes
        .iter()
        .any(|n| n.prerequisites == NodeExpansion::Unexpanded);
    assert_eq!(
        g.coverage,
        if unexpanded {
            GraphCoverage::Partial
        } else {
            GraphCoverage::Complete
        }
    );
    assert_eq!(
        g.restricted,
        g.nodes
            .iter()
            .any(|n| n.restricted_prerequisites == Some(true))
    );
    // Status from direct prerequisites only, against each node's own outcome.
    let outcome: BTreeMap<&str, Outcome> = g
        .nodes
        .iter()
        .map(|n| (n.occurrence_id.as_str(), n.outcome))
        .collect();
    for n in &g.nodes {
        let visible: Vec<String> = stored_of(&n.occurrence_id)
            .into_iter()
            .filter(|p| !hidden.contains(p))
            .collect();
        let state = if n.outcome == Outcome::Complete {
            DependencyState::Satisfied
        } else if n.prerequisites == NodeExpansion::Unexpanded {
            DependencyState::Unknown
        } else if visible
            .iter()
            .any(|p| outcome[p.as_str()] != Outcome::Complete)
        {
            DependencyState::Waiting
        } else if n.restricted_prerequisites == Some(true) {
            DependencyState::Restricted
        } else {
            DependencyState::Ready
        };
        assert_eq!(n.dependency_state, state, "{}", n.occurrence_id);
    }
    // One task entry per distinct task, sorted, titled as the caller sees it.
    let tasks: BTreeSet<&str> = g.nodes.iter().map(|n| n.task_id.as_str()).collect();
    assert_eq!(
        g.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        tasks.into_iter().collect::<Vec<_>>()
    );
    for t in &g.tasks {
        let label: String = sqlx::query_scalar("SELECT label FROM resources WHERE id=$1")
            .bind(&t.id)
            .fetch_one(&s.pool)
            .await?;
        assert_eq!(t.title, label);
    }
    // Outcome, version and edit right agree with the ordinary occurrence read.
    for n in &g.nodes {
        let views = s
            .occurrence_view(
                actor,
                &ViewFilter {
                    task_id: Some(n.task_id.clone()),
                    limit: Some(200),
                    ..Default::default()
                },
                NOW,
            )
            .await?
            .items;
        let view = views
            .iter()
            .find(|v| v.id == n.occurrence_id)
            .expect("a graph node is a visible occurrence");
        assert_eq!(
            (view.outcome, view.version, view.can_edit, &view.data.slot),
            (n.outcome, n.version, n.can_edit, &n.slot)
        );
    }
    Ok(())
}

fn none() -> BTreeSet<String> {
    BTreeSet::new()
}

// C1 topologies ------------------------------------------------------------------------------

#[tokio::test]
async fn c1_empty_root_is_a_complete_single_node_graph() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let root = make(&s, &a, "Cook dinner", None).await?;
    let g = graph(&s, &a, &root, 200).await?;
    verify(&s, &a, &g, &none()).await?;
    assert_eq!(ids(&g), std::slice::from_ref(&root));
    assert!(g.edges.is_empty() && g.direct_prerequisites.is_empty());
    assert_eq!(g.coverage, GraphCoverage::Complete);
    assert!(!g.restricted);
    assert_eq!(g.max_depth, 200);
    let n = &g.nodes[0];
    assert_eq!(n.relation, GraphRelation::Root);
    assert_eq!(n.dependency_state, DependencyState::Ready);
    assert_eq!(n.restricted_prerequisites, Some(false));
    assert_eq!(g.tasks.len(), 1);
    assert_eq!(g.tasks[0].title, "Cook dinner");
    Ok(())
}

#[tokio::test]
async fn c1_chain_fork_join_diamond_and_redundant_edges() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    // root → {left, right} → table (a shared prerequisite: one node, two edges); root → table
    // directly as well (a redundant edge), and table → shop (a chain).
    let shop = make(&s, &a, "Buy food", None).await?;
    let table = make(&s, &a, "Set the table", None).await?;
    let left = make(&s, &a, "Cook", None).await?;
    let right = make(&s, &a, "Pour drinks", None).await?;
    let root = make(&s, &a, "Serve dinner", None).await?;
    set(&s, &a, &table, vec![shop.clone()], false).await?;
    set(&s, &a, &left, vec![table.clone()], false).await?;
    set(&s, &a, &right, vec![table.clone()], false).await?;
    set(
        &s,
        &a,
        &root,
        vec![left.clone(), right.clone(), table.clone()],
        true,
    )
    .await?;
    let g = graph(&s, &a, &root, 200).await?;
    verify(&s, &a, &g, &none()).await?;
    assert_eq!(g.nodes.len(), 5);
    assert_eq!(
        g.nodes.iter().filter(|n| n.occurrence_id == table).count(),
        1,
        "a shared prerequisite appears once"
    );
    assert_eq!(
        edge_set(&g),
        BTreeSet::from([
            pair(&shop, &table),
            pair(&table, &left),
            pair(&table, &right),
            pair(&table, &root),
            pair(&left, &root),
            pair(&right, &root),
        ])
    );
    // The redundant edge makes the table direct, at depth 1, though it is also transitive.
    assert_eq!(node(&g, &table).relation, GraphRelation::Direct);
    assert_eq!(node(&g, &table).depth, 1);
    assert_eq!(node(&g, &shop).relation, GraphRelation::Transitive);
    assert_eq!(node(&g, &shop).depth, 2);
    assert!(node(&g, &root).strict);
    let mut direct = vec![left.clone(), right.clone(), table.clone()];
    direct.sort();
    assert_eq!(g.direct_prerequisites, direct);
    // Order carries no meaning, but it is a function of the stored state: read it again.
    assert_eq!(graph(&s, &a, &root, 200).await?, g);
    // Rooted lower down, the same stored edges are returned and nothing above it.
    let sub = graph(&s, &a, &left, 200).await?;
    verify(&s, &a, &sub, &none()).await?;
    assert_eq!(
        edge_set(&sub),
        BTreeSet::from([pair(&shop, &table), pair(&table, &left)])
    );
    Ok(())
}

#[tokio::test]
async fn c1_several_occurrences_of_one_task_share_one_task_entry() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let mut daily = checkbox();
    daily.schedule.start_date = Some("2026-09-06".into());
    daily.schedule.repeat = Some(Cadence {
        frequency: Frequency::Daily,
        interval: 1,
    });
    let task = id();
    s.task_command(
        &a,
        &id(),
        &TaskCommand::CreateTask {
            id: task.clone(),
            execution_id: id(),
            title: "Water plants".into(),
            anchor: None,
            initial_policy: None,
            definition: daily,
        },
        None,
        NOW,
    )
    .await?;
    s.task_command(
        &a,
        &id(),
        &TaskCommand::Materialise {
            task_id: task.clone(),
            through_date: "2026-09-10".into(),
            limit: 16,
        },
        None,
        NOW,
    )
    .await?;
    let occurrences = s
        .occurrence_view(
            &a,
            &ViewFilter {
                limit: Some(200),
                ..Default::default()
            },
            NOW,
        )
        .await?
        .items
        .into_iter()
        .filter(|v| v.data.task_id == task)
        .map(|v| v.id)
        .collect::<Vec<_>>();
    assert!(
        occurrences.len() >= 2,
        "the daily task has several occurrences: {occurrences:?}"
    );
    let root = make(&s, &a, "Holiday", None).await?;
    set(&s, &a, &root, occurrences[..2].to_vec(), false).await?;
    let g = graph(&s, &a, &root, 200).await?;
    verify(&s, &a, &g, &none()).await?;
    assert_eq!(g.nodes.len(), 3);
    assert_eq!(g.tasks.len(), 2, "two occurrences, one task definition");
    let watering = &g.nodes[1];
    assert_eq!(watering.task_id, g.nodes[2].task_id);
    assert_ne!(watering.slot, g.nodes[2].slot, "distinct slots");
    Ok(())
}

// C2 status ----------------------------------------------------------------------------------

/// Dependants before their prerequisites, so completing one never changes a later check.
fn dependants_first(g: &DependencyGraph) -> Vec<String> {
    let mut order = Vec::new();
    let mut done = BTreeSet::new();
    while order.len() < g.nodes.len() {
        for n in &g.nodes {
            if done.contains(&n.occurrence_id) {
                continue;
            }
            let waiting = g
                .edges
                .iter()
                .any(|e| e.prerequisite_id == n.occurrence_id && !done.contains(&e.dependant_id));
            if !waiting {
                done.insert(n.occurrence_id.clone());
                order.push(n.occurrence_id.clone());
            }
        }
    }
    order
}

/// `ready` exactly when an ordinary `require_satisfied` completion is accepted for the caller.
/// `evidence` supplies completing root evidence for non-checkbox goals.
async fn cross_check(
    s: &Store,
    actor: &str,
    g: &DependencyGraph,
    evidence: &BTreeMap<String, Evidence>,
) -> Result<()> {
    for occurrence in dependants_first(g) {
        let n = node(g, &occurrence);
        if n.dependency_state == DependencyState::Satisfied {
            continue;
        }
        let preview = s.dependency_preview(actor, &occurrence, NOW).await?;
        let accepted = s
            .task_command(
                actor,
                &id(),
                &TaskCommand::CompleteDependencies {
                    root_evidence: evidence.get(&occurrence).cloned(),
                    occurrence_id: occurrence.clone(),
                    preview_token: preview.token,
                    mode: CompletionMode::RequireSatisfied,
                    happened_at: NOW,
                },
                None,
                NOW,
            )
            .await;
        assert_eq!(
            accepted.is_ok(),
            n.dependency_state == DependencyState::Ready,
            "{occurrence} {:?}: {accepted:?}",
            n.dependency_state
        );
        if let Err(e) = accepted {
            assert_eq!(e.to_string(), "conflict");
        }
    }
    Ok(())
}

#[tokio::test]
async fn c2_statuses_follow_direct_prerequisites_and_match_enforcement() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let done = make(&s, &a, "Done", None).await?;
    let todo = make(&s, &a, "To do", None).await?;
    let numeric = make_with(
        &s,
        &a,
        "Run",
        Definition {
            goal: Goal::Numeric {
                minimum: Some(Decimal("5".into())),
                maximum: None,
                unit: "km".into(),
            },
            ..checkbox()
        },
        None,
    )
    .await?;
    let after_done = make(&s, &a, "After done", None).await?;
    let after_todo = make(&s, &a, "After to do", None).await?;
    let after_numeric = make(&s, &a, "After run", None).await?;
    let overridden = make(&s, &a, "Overridden", None).await?;
    let root = make(&s, &a, "Root", None).await?;
    set(&s, &a, &after_done, vec![done.clone()], false).await?;
    set(&s, &a, &after_todo, vec![todo.clone()], false).await?;
    set(&s, &a, &after_numeric, vec![numeric.clone()], false).await?;
    set(&s, &a, &overridden, vec![todo.clone()], false).await?;
    set(
        &s,
        &a,
        &root,
        vec![
            after_done.clone(),
            after_todo.clone(),
            after_numeric.clone(),
            overridden.clone(),
        ],
        true,
    )
    .await?;
    complete(&s, &a, &done, CompletionMode::RequireSatisfied).await?;
    // An advisory override completes a node whose prerequisite is still outstanding.
    complete(&s, &a, &overridden, CompletionMode::AdvisoryOverride).await?;
    // Partial numeric progress is not complete.
    s.task_command(
        &a,
        &id(),
        &TaskCommand::Record {
            occurrence_id: numeric.clone(),
            subject_account_id: a.clone(),
            entry_id: id(),
            evidence: Evidence::Quantity {
                amount: Decimal("2".into()),
            },
            replaces: None,
            expected_version: None,
            happened_at: NOW,
        },
        None,
        NOW,
    )
    .await?;
    let g = graph(&s, &a, &root, 200).await?;
    verify(&s, &a, &g, &none()).await?;
    let state = |o: &str| node(&g, o).dependency_state;
    assert_eq!(state(&done), DependencyState::Satisfied);
    assert_eq!(state(&todo), DependencyState::Ready);
    assert_eq!(state(&numeric), DependencyState::Ready);
    assert_ne!(node(&g, &numeric).outcome, Outcome::Complete);
    assert_eq!(state(&after_done), DependencyState::Ready);
    assert_eq!(state(&after_todo), DependencyState::Waiting);
    assert_eq!(state(&after_numeric), DependencyState::Waiting);
    assert_eq!(state(&overridden), DependencyState::Satisfied);
    assert_eq!(node(&g, &overridden).outcome, Outcome::Complete);
    // Only direct prerequisites count: the root waits on after_todo and after_numeric, and
    // would wait on nothing transitive if those were complete.
    assert_eq!(state(&root), DependencyState::Waiting);
    assert!(node(&g, &root).strict);
    let evidence = BTreeMap::from([(
        numeric.clone(),
        Evidence::Quantity {
            amount: Decimal("5".into()),
        },
    )]);
    cross_check(&s, &a, &g, &evidence).await?;
    // after_done was ready, so the cross-check completed it; the graph follows.
    let g = graph(&s, &a, &root, 200).await?;
    verify(&s, &a, &g, &none()).await?;
    assert_eq!(
        node(&g, &after_done).dependency_state,
        DependencyState::Satisfied
    );
    Ok(())
}

#[tokio::test]
async fn c2_ready_is_judged_from_direct_prerequisites_only() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let deep = make(&s, &a, "Deep", None).await?;
    let middle = make(&s, &a, "Middle", None).await?;
    let root = make(&s, &a, "Root", None).await?;
    set(&s, &a, &middle, vec![deep.clone()], false).await?;
    set(&s, &a, &root, vec![middle.clone()], false).await?;
    complete(&s, &a, &middle, CompletionMode::AdvisoryOverride).await?;
    let g = graph(&s, &a, &root, 200).await?;
    verify(&s, &a, &g, &none()).await?;
    // The transitive prerequisite is still outstanding, but the direct one is complete.
    assert_eq!(node(&g, &deep).dependency_state, DependencyState::Ready);
    assert_eq!(node(&g, &root).dependency_state, DependencyState::Ready);
    cross_check(&s, &a, &g, &BTreeMap::new()).await
}

// C3 restriction and privacy -----------------------------------------------------------------

struct Shared {
    s: Store,
    _dir: tempfile::TempDir,
    a: String,
    b: String,
    c: String,
    /// B's root.
    root: String,
    /// A's, shared with B.
    visible: String,
    /// A's, shared with B, below `visible`.
    below: String,
    /// A's, hidden from B, a direct prerequisite of `visible`.
    private: String,
    /// A's, shared with B, reachable only through `private`.
    beyond: String,
}

/// root(B) → visible(A, shared) → {below (shared), private (hidden) → beyond (shared)}.
async fn shared() -> Result<Shared> {
    let (s, dir) = setup().await?;
    let (a, b, c) = (account(&s).await?, account(&s).await?, account(&s).await?);
    let to_b = || Some(grant(&[&b], false));
    let beyond = make(&s, &a, "Beyond the secret", to_b()).await?;
    let private = make(&s, &a, "Secret surprise", None).await?;
    let below = make(&s, &a, "Below", to_b()).await?;
    let visible = make(&s, &a, "Shared step", to_b()).await?;
    set(&s, &a, &private, vec![beyond.clone()], false).await?;
    set(
        &s,
        &a,
        &visible,
        vec![below.clone(), private.clone()],
        false,
    )
    .await?;
    let root = make(&s, &b, "B's root", None).await?;
    set(&s, &b, &root, vec![visible.clone()], false).await?;
    Ok(Shared {
        s,
        _dir: dir,
        a,
        b,
        c,
        root,
        visible,
        below,
        private,
        beyond,
    })
}

#[tokio::test]
async fn c3_hidden_prerequisites_contribute_only_a_flag() -> Result<()> {
    let w = shared().await?;
    let (s, b) = (&w.s, &w.b);
    let hidden = BTreeSet::from([w.private.clone()]);
    let g = graph(s, b, &w.root, 200).await?;
    verify(s, b, &g, &hidden).await?;
    assert_eq!(ids(&g).len(), 3, "root, visible, below: {:?}", ids(&g));
    assert!(
        !ids(&g).contains(&w.beyond),
        "a visible occurrence reachable only through a hidden one is not returned"
    );
    let body = serde_json::to_string(&g)?;
    for secret in [
        w.private.as_str(),
        w.beyond.as_str(),
        "Secret surprise",
        "Beyond",
    ] {
        assert!(!body.contains(secret), "{secret} disclosed");
    }
    assert!(g.restricted);
    assert_eq!(
        g.coverage,
        GraphCoverage::Complete,
        "coverage says nothing about access"
    );
    assert_eq!(node(&g, &w.visible).restricted_prerequisites, Some(true));
    assert_eq!(node(&g, &w.root).restricted_prerequisites, Some(false));
    // A's completed prerequisite progress is not B's to see, so `below` is not satisfied for B.
    complete(s, &w.a, &w.below, CompletionMode::RequireSatisfied).await?;
    let g = graph(s, b, &w.root, 200).await?;
    verify(s, b, &g, &hidden).await?;
    assert_ne!(node(&g, &w.below).outcome, Outcome::Complete);
    assert_eq!(
        node(&g, &w.visible).dependency_state,
        DependencyState::Waiting
    );
    // The owner sees everything, including the complete prerequisite.
    let full = graph(s, &w.a, &w.visible, 200).await?;
    verify(s, &w.a, &full, &none()).await?;
    assert_eq!(full.nodes.len(), 4);
    assert!(!full.restricted);
    assert_eq!(
        node(&full, &w.below).dependency_state,
        DependencyState::Satisfied
    );
    // The private prerequisite is still outstanding, which only the owner can see.
    assert_eq!(
        node(&full, &w.visible).dependency_state,
        DependencyState::Waiting
    );
    // A third account sees neither the root nor anything else.
    assert_eq!(
        graph(s, &w.c, &w.root, 200).await.unwrap_err().to_string(),
        "not_found"
    );
    Ok(())
}

#[tokio::test]
async fn c3_restricted_state_when_only_hidden_prerequisites_remain() -> Result<()> {
    let (s, _dir) = setup().await?;
    let (a, b) = (account(&s).await?, account(&s).await?);
    let private = make(&s, &a, "Secret", None).await?;
    let open = make(&s, &a, "Open", Some(grant(&[&b], true))).await?;
    set(&s, &a, &open, vec![private.clone()], false).await?;
    let g = graph(&s, &b, &open, 200).await?;
    verify(&s, &b, &g, &BTreeSet::from([private.clone()])).await?;
    assert_eq!(g.nodes.len(), 1);
    assert!(g.restricted && g.direct_prerequisites.is_empty());
    assert_eq!(g.nodes[0].dependency_state, DependencyState::Restricted);
    assert_eq!(g.nodes[0].restricted_prerequisites, Some(true));
    assert!(g.nodes[0].can_edit);
    // Enforcement agrees: B cannot complete it under require_satisfied.
    assert_eq!(
        complete(&s, &b, &open, CompletionMode::RequireSatisfied)
            .await
            .unwrap_err()
            .to_string(),
        "conflict"
    );
    // A hidden branch beside a visible unsatisfied one: waiting takes precedence.
    let todo = make(&s, &a, "Visible to do", Some(grant(&[&b], false))).await?;
    set(&s, &a, &open, vec![private.clone(), todo.clone()], false).await?;
    let g = graph(&s, &b, &open, 200).await?;
    verify(&s, &b, &g, &BTreeSet::from([private.clone()])).await?;
    assert_eq!(g.nodes[0].dependency_state, DependencyState::Waiting);
    Ok(())
}

#[tokio::test]
async fn c3_hidden_changes_leave_another_accounts_graph_byte_identical() -> Result<()> {
    let w = shared().await?;
    let (s, a, b) = (&w.s, &w.a, &w.b);
    let before = serde_json::to_vec(&graph(s, b, &w.root, 200).await?)?;
    // Hidden structure below a hidden node, and hidden completion.
    let deeper = make(s, a, "Deeper secret", None).await?;
    set(
        s,
        a,
        &w.private,
        vec![w.beyond.clone(), deeper.clone()],
        false,
    )
    .await?;
    complete(s, a, &deeper, CompletionMode::RequireSatisfied).await?;
    complete(s, a, &w.beyond, CompletionMode::RequireSatisfied).await?;
    complete(s, a, &w.private, CompletionMode::RequireSatisfied).await?;
    let after = serde_json::to_vec(&graph(s, b, &w.root, 200).await?)?;
    assert_eq!(before, after, "hidden changes are not observable");
    // Removing the hidden structure entirely is not observable either.
    set(s, a, &w.private, vec![], false).await?;
    assert_eq!(
        serde_json::to_vec(&graph(s, b, &w.root, 200).await?)?,
        before
    );
    // A change to a visible node's own prerequisites is: the flag (and its version) change, and
    // nothing else does.
    set(s, a, &w.visible, vec![w.below.clone()], false).await?;
    let mut changed = graph(s, b, &w.root, 200).await?;
    verify(s, b, &changed, &BTreeSet::from([w.private.clone()])).await?;
    let mut original: DependencyGraph = serde_json::from_slice(&before)?;
    assert!(original.restricted && !changed.restricted);
    for g in [&mut original, &mut changed] {
        g.restricted = false;
        for n in &mut g.nodes {
            if n.occurrence_id == w.visible {
                n.version = 0;
                n.restricted_prerequisites = None;
                n.dependency_state = DependencyState::Unknown;
            }
        }
    }
    assert_eq!(original, changed);
    Ok(())
}

#[tokio::test]
async fn c3_revocation_removes_the_node_and_restricts_its_dependant() -> Result<()> {
    let w = shared().await?;
    let (s, a, b) = (&w.s, &w.a, &w.b);
    replace(s, a, &w.below, Policy::default()).await?;
    let hidden = BTreeSet::from([w.private.clone(), w.below.clone()]);
    let g = graph(s, b, &w.root, 200).await?;
    verify(s, b, &g, &hidden).await?;
    assert_eq!(ids(&g), [w.root.clone(), w.visible.clone()]);
    assert_eq!(
        node(&g, &w.visible).dependency_state,
        DependencyState::Restricted
    );
    // Revoking the task itself hides every occurrence below it: the root's prerequisite.
    replace(s, a, &lineage(s, &w.visible).await?[0], Policy::default()).await?;
    let g = graph(s, b, &w.root, 200).await?;
    let hidden = BTreeSet::from([w.private.clone(), w.below.clone(), w.visible.clone()]);
    verify(s, b, &g, &hidden).await?;
    assert_eq!(ids(&g), std::slice::from_ref(&w.root));
    assert!(g.restricted && g.direct_prerequisites.is_empty());
    assert_eq!(g.nodes[0].restricted_prerequisites, Some(true));
    // A revoked root is not_found, like a nonexistent one.
    assert_eq!(
        graph(s, b, &w.visible, 200).await.unwrap_err().to_string(),
        "not_found"
    );
    assert_eq!(
        graph(s, b, &id(), 200).await.unwrap_err().to_string(),
        "not_found"
    );
    // A task (not an occurrence) is not_found too.
    assert_eq!(
        graph(s, a, &lineage(s, &w.visible).await?[0], 200)
            .await
            .unwrap_err()
            .to_string(),
        "not_found"
    );
    Ok(())
}

#[tokio::test]
async fn c3_two_accounts_see_their_own_views_of_one_graph() -> Result<()> {
    let (s, _dir) = setup().await?;
    let (a, b) = (account(&s).await?, account(&s).await?);
    // A shared root with one prerequisite private to A and one private to B.
    let mine = make(&s, &a, "A's private errand", None).await?;
    let theirs = make(&s, &b, "B's private errand", None).await?;
    let root = make(&s, &a, "Joint plan", Some(grant(&[&b], true))).await?;
    // Only an account that can see a prerequisite can add it, so each adds their own.
    set(&s, &a, &root, vec![mine.clone()], false).await?;
    // B replaces the set: B cannot see A's prerequisite, which is therefore removed (D1).
    let before_b = graph(&s, &b, &root, 1).await?;
    assert_eq!(before_b.nodes[0].restricted_prerequisites, Some(true));
    assert!(before_b.direct_prerequisites.is_empty());
    s.task_command(
        &b,
        &id(),
        &TaskCommand::SetDependencies {
            occurrence_id: root.clone(),
            expected_version: before_b.nodes[0].version,
            prerequisites: vec![theirs.clone()],
            strict: false,
        },
        None,
        NOW,
    )
    .await?;
    assert_eq!(stored(&s).await?[&root], std::slice::from_ref(&theirs));
    let for_a = graph(&s, &a, &root, 200).await?;
    let for_b = graph(&s, &b, &root, 200).await?;
    verify(&s, &a, &for_a, &BTreeSet::from([theirs.clone()])).await?;
    verify(&s, &b, &for_b, &BTreeSet::from([mine.clone()])).await?;
    assert!(for_a.restricted && !for_b.restricted);
    let body = serde_json::to_string(&for_a)?;
    assert!(!body.contains(&theirs) && !body.contains("B's private"));
    let body = serde_json::to_string(&for_b)?;
    assert!(!body.contains(&mine) && !body.contains("A's private"));
    Ok(())
}

// C4 bounds ----------------------------------------------------------------------------------

#[tokio::test]
async fn c4_long_chain_stops_at_the_node_budget_and_depth() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let mut chain = Vec::new();
    for i in 0..250 {
        chain.push(make(&s, &a, &format!("Step {i}"), None).await?);
    }
    // chain[i] requires chain[i + 1].
    for i in 0..249 {
        set(&s, &a, &chain[i], vec![chain[i + 1].clone()], false).await?;
    }
    let g = graph(&s, &a, &chain[0], 200).await?;
    verify(&s, &a, &g, &none()).await?;
    assert_eq!(ids(&g), chain[..200].to_vec());
    assert_eq!(g.coverage, GraphCoverage::Partial);
    assert_eq!(g.nodes[199].prerequisites, NodeExpansion::Unexpanded);
    assert_eq!(g.nodes[199].dependency_state, DependencyState::Unknown);
    assert_eq!(g.nodes[199].depth, 199);
    assert!(
        g.nodes[..199]
            .iter()
            .all(|n| n.prerequisites == NodeExpansion::Expanded)
    );
    assert_eq!(g.edges.len(), 199);
    // Depth limits: exactly max_depth levels below the root.
    for depth in [1u16, 2, 5] {
        let g = graph(&s, &a, &chain[0], depth).await?;
        verify(&s, &a, &g, &none()).await?;
        assert_eq!(g.max_depth, depth);
        assert_eq!(g.nodes.len(), usize::from(depth) + 1);
        assert_eq!(g.nodes.last().unwrap().depth, depth);
        assert_eq!(
            g.nodes.last().unwrap().prerequisites,
            NodeExpansion::Unexpanded
        );
        assert_eq!(g.coverage, GraphCoverage::Partial);
    }
    // The tail of the chain is complete within budget.
    let tail = graph(&s, &a, &chain[60], 200).await?;
    verify(&s, &a, &tail, &none()).await?;
    assert_eq!(tail.nodes.len(), 190);
    assert_eq!(tail.coverage, GraphCoverage::Complete);
    for bad in [0u16, 201] {
        assert_eq!(
            graph(&s, &a, &chain[0], bad).await.unwrap_err().to_string(),
            "invalid_value"
        );
    }
    Ok(())
}

async fn fan_out(s: &Store, a: &str, of: &str, count: usize, tag: &str) -> Result<Vec<String>> {
    let mut children = Vec::new();
    for i in 0..count {
        children.push(make(s, a, &format!("{tag} {i}"), None).await?);
    }
    set(s, a, of, children.clone(), false).await?;
    Ok(children)
}

#[tokio::test]
async fn c4_fan_expands_whole_nodes_and_never_part_of_one() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let root = make(&s, &a, "Root", None).await?;
    // Ascending ids give the expansion order of the root's 32 prerequisites.
    let mut tier = Vec::new();
    for i in 0..32 {
        tier.push(make(&s, &a, &format!("Tier {i}"), None).await?);
    }
    tier.sort();
    set(&s, &a, &root, tier.clone(), false).await?;
    // 1 + 32 + 5 × 32 = 193 nodes; then a 32 that does not fit, a 3 that does (196), a 5 that
    // does not, a 4 that fits exactly (200), one already present (fits) and one new (does not).
    let mut kids = Vec::new();
    for t in &tier[..5] {
        kids.push(fan_out(&s, &a, t, 32, "Leaf").await?);
    }
    let too_big = fan_out(&s, &a, &tier[5], 32, "Big").await?;
    let three = fan_out(&s, &a, &tier[6], 3, "Three").await?;
    let five = fan_out(&s, &a, &tier[7], 5, "Five").await?;
    let four = fan_out(&s, &a, &tier[8], 4, "Four").await?;
    set(&s, &a, &tier[9], vec![kids[0][0].clone()], false).await?;
    let lone = fan_out(&s, &a, &tier[10], 1, "Lone").await?;
    let g = graph(&s, &a, &root, 200).await?;
    verify(&s, &a, &g, &none()).await?;
    assert_eq!(g.nodes.len(), 200);
    assert_eq!(g.coverage, GraphCoverage::Partial);
    let expanded = |o: &str| node(&g, o).prerequisites == NodeExpansion::Expanded;
    assert!(tier[..5].iter().all(|t| expanded(t)));
    assert!(!expanded(&tier[5]), "32 do not fit in 7");
    assert!(
        too_big.iter().all(|k| !ids(&g).contains(k)),
        "no partial expansion"
    );
    assert!(expanded(&tier[6]) && three.iter().all(|k| ids(&g).contains(k)));
    assert!(!expanded(&tier[7]) && five.iter().all(|k| !ids(&g).contains(k)));
    assert!(expanded(&tier[8]) && four.iter().all(|k| ids(&g).contains(k)));
    assert!(
        expanded(&tier[9]),
        "an already-present prerequisite needs no budget"
    );
    assert!(g.edges.contains(&DependencyEdge {
        prerequisite_id: kids[0][0].clone(),
        dependant_id: tier[9].clone(),
    }));
    assert!(!expanded(&tier[10]) && !ids(&g).contains(&lone[0]));
    // Prerequisite-free tiers are expanded (they have nothing to add).
    assert!(tier[11..].iter().all(|t| expanded(t)));
    assert_eq!(node(&g, &tier[5]).restricted_prerequisites, None);
    assert_eq!(
        node(&g, &tier[5]).dependency_state,
        DependencyState::Unknown
    );
    // Rooted at an unexpanded node, its own prerequisites are returned.
    let sub = graph(&s, &a, &tier[5], 200).await?;
    verify(&s, &a, &sub, &none()).await?;
    assert_eq!(sub.nodes.len(), 33);
    assert_eq!(sub.coverage, GraphCoverage::Complete);
    Ok(())
}

#[tokio::test]
async fn c4_hidden_prerequisites_do_not_consume_the_budget() -> Result<()> {
    let (s, _dir) = setup().await?;
    let (a, b) = (account(&s).await?, account(&s).await?);
    let to_b = || Some(grant(&[&b], false));
    let root = make(&s, &a, "Root", to_b()).await?;
    let mut tier = Vec::new();
    for i in 0..32 {
        tier.push(make(&s, &a, &format!("Tier {i}"), to_b()).await?);
    }
    tier.sort();
    set(&s, &a, &root, tier.clone(), false).await?;
    for t in &tier[..5] {
        let mut leaves = Vec::new();
        for i in 0..32 {
            leaves.push(make(&s, &a, &format!("Leaf {i}"), to_b()).await?);
        }
        set(&s, &a, t, leaves, false).await?;
    }
    // 193 visible nodes so far. tier[5] has 7 prerequisites B can see and 25 B cannot.
    let mut hidden = BTreeSet::new();
    let mut needs = Vec::new();
    for i in 0..7 {
        needs.push(make(&s, &a, &format!("Seen {i}"), to_b()).await?);
    }
    for i in 0..25 {
        let h = make(&s, &a, &format!("Unseen {i}"), None).await?;
        hidden.insert(h.clone());
        needs.push(h);
    }
    set(&s, &a, &tier[5], needs.clone(), false).await?;
    let for_b = graph(&s, &b, &root, 200).await?;
    verify(&s, &b, &for_b, &hidden).await?;
    // Only the 7 visible ones count: they fit exactly.
    assert_eq!(for_b.nodes.len(), 200);
    let n = node(&for_b, &tier[5]);
    assert_eq!(n.prerequisites, NodeExpansion::Expanded);
    assert_eq!(n.restricted_prerequisites, Some(true));
    // The owner sees all 32, which do not fit: the cut depends only on the reader's own view.
    let for_a = graph(&s, &a, &root, 200).await?;
    verify(&s, &a, &for_a, &none()).await?;
    assert_eq!(for_a.nodes.len(), 193);
    assert_eq!(
        node(&for_a, &tier[5]).prerequisites,
        NodeExpansion::Unexpanded
    );
    // B's graph is the same with the hidden prerequisites gone, apart from the flag and the
    // version their removal bumped.
    set(&s, &a, &tier[5], needs[..7].to_vec(), false).await?;
    let mut without = graph(&s, &b, &root, 200).await?;
    verify(&s, &b, &without, &hidden).await?;
    let mut with = for_b;
    for g in [&mut with, &mut without] {
        g.restricted = false;
        for n in &mut g.nodes {
            if n.occurrence_id == tier[5] {
                n.version = 0;
                n.restricted_prerequisites = None;
                n.dependency_state = DependencyState::Unknown;
            }
        }
    }
    assert_eq!(with, without);
    Ok(())
}

#[tokio::test]
async fn c4_present_prerequisites_need_neither_depth_nor_budget() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    // root → {t0, t1, shared}; t0 → shared; t1 → shared. At max_depth 1 the tier nodes may add
    // nothing new, but their only prerequisite is already present, so they are expanded and
    // both edges to the shared node are returned.
    let shared_prerequisite = make(&s, &a, "Shared", None).await?;
    let mut direct = Vec::new();
    for i in 0..2 {
        let t = make(&s, &a, &format!("Needs shared {i}"), None).await?;
        set(&s, &a, &t, vec![shared_prerequisite.clone()], false).await?;
        direct.push(t);
    }
    let root = make(&s, &a, "Root", None).await?;
    set(
        &s,
        &a,
        &root,
        [direct.clone(), vec![shared_prerequisite.clone()]].concat(),
        false,
    )
    .await?;
    let g = graph(&s, &a, &root, 1).await?;
    verify(&s, &a, &g, &none()).await?;
    assert_eq!(g.nodes.len(), 4);
    assert_eq!(g.coverage, GraphCoverage::Complete);
    for t in &direct {
        assert!(edge_set(&g).contains(&pair(&shared_prerequisite, t)));
    }
    // Without the direct edge the shared node would be at depth 2: refused at max_depth 1.
    set(&s, &a, &root, direct.clone(), false).await?;
    let g = graph(&s, &a, &root, 1).await?;
    verify(&s, &a, &g, &none()).await?;
    assert_eq!(g.nodes.len(), 3);
    assert_eq!(g.coverage, GraphCoverage::Partial);
    assert!(g.edges.iter().all(|e| e.dependant_id == root));
    Ok(())
}

// C5 cycles ----------------------------------------------------------------------------------

#[tokio::test]
async fn c5_a_stored_cycle_terminates_and_returns_each_node_once() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let x = make(&s, &a, "X", None).await?;
    let y = make(&s, &a, "Y", None).await?;
    let z = make(&s, &a, "Z", None).await?;
    set(&s, &a, &x, vec![y.clone()], false).await?;
    set(&s, &a, &y, vec![z.clone()], false).await?;
    // The write path refuses the cycle; insert it directly, as damaged data would be.
    assert_eq!(
        set(&s, &a, &z, vec![x.clone()], false)
            .await
            .unwrap_err()
            .to_string(),
        "conflict"
    );
    sqlx::query("INSERT INTO occurrence_dependencies VALUES ($1,$2)")
        .bind(&z)
        .bind(&x)
        .execute(&s.pool)
        .await?;
    let g = graph(&s, &a, &x, 200).await?;
    verify(&s, &a, &g, &none()).await?;
    assert_eq!(ids(&g), [x.clone(), y.clone(), z.clone()]);
    assert_eq!(
        edge_set(&g),
        BTreeSet::from([pair(&y, &x), pair(&z, &y), pair(&x, &z)])
    );
    assert_eq!(g.coverage, GraphCoverage::Complete);
    Ok(())
}

// C6 editing ---------------------------------------------------------------------------------

#[tokio::test]
async fn c6_edits_use_the_graph_version_and_keep_write_rules() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let p = make(&s, &a, "P", None).await?;
    let q = make(&s, &a, "Q", None).await?;
    let root = make(&s, &a, "Root", None).await?;
    let edit = |version: i64, prerequisites: Vec<String>, target: &str| {
        let (s, a, target) = (s.clone(), a.clone(), target.to_owned());
        async move {
            s.task_command(
                &a,
                &id(),
                &TaskCommand::SetDependencies {
                    occurrence_id: target,
                    expected_version: version,
                    prerequisites,
                    strict: false,
                },
                None,
                NOW,
            )
            .await
            .map(|_| ())
        }
    };
    let first = graph(&s, &a, &root, 1).await?;
    edit(first.nodes[0].version, vec![p.clone()], &root).await?;
    let second = graph(&s, &a, &root, 1).await?;
    verify(&s, &a, &second, &none()).await?;
    assert!(second.nodes[0].version > first.nodes[0].version);
    assert_eq!(second.direct_prerequisites, std::slice::from_ref(&p));
    // A stale edit made from the first read is refused and changes nothing.
    assert_eq!(
        edit(first.nodes[0].version, vec![q.clone()], &root)
            .await
            .unwrap_err()
            .to_string(),
        "conflict"
    );
    assert_eq!(graph(&s, &a, &root, 1).await?, second);
    // A cycle built from graph-supplied ids and versions is refused.
    let pg = graph(&s, &a, &p, 1).await?;
    assert_eq!(
        edit(pg.nodes[0].version, vec![root.clone()], &p)
            .await
            .unwrap_err()
            .to_string(),
        "conflict"
    );
    // A self-reference is invalid.
    assert_eq!(
        edit(second.nodes[0].version, vec![root.clone()], &root)
            .await
            .unwrap_err()
            .to_string(),
        "invalid_value"
    );
    assert_eq!(graph(&s, &a, &root, 1).await?, second);
    // The editor's full intended list replaces the set.
    edit(second.nodes[0].version, vec![q.clone()], &root).await?;
    let third = graph(&s, &a, &root, 200).await?;
    verify(&s, &a, &third, &none()).await?;
    assert_eq!(edge_set(&third), BTreeSet::from([pair(&q, &root)]));
    Ok(())
}

// C7 snapshot --------------------------------------------------------------------------------

const POINT: &str = "dependency_graph.after_root_read";

async fn paused_read(
    s: &Store,
    a: &str,
    root: &str,
    write: impl std::future::Future<Output = Result<()>>,
) -> Result<DependencyGraph> {
    let mut gate = s.hooks().arm(POINT);
    let read = {
        let (s, a, root) = (s.clone(), a.to_owned(), root.to_owned());
        tokio::spawn(async move { s.dependency_graph(&a, &root, 200, NOW).await })
    };
    gate.reached().await;
    tokio::time::timeout(Duration::from_secs(20), write)
        .await
        .expect("the writer was blocked by the paused reader")?;
    gate.release();
    read.await?
}

#[tokio::test]
async fn c7_one_graph_is_read_from_one_snapshot() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let p = make(&s, &a, "P", None).await?;
    let q = make(&s, &a, "Q", None).await?;
    let r = make(&s, &a, "R", None).await?;
    let root = make(&s, &a, "Root", None).await?;
    set(&s, &a, &p, vec![q.clone()], false).await?;
    set(&s, &a, &root, vec![p.clone()], false).await?;
    let before = graph(&s, &a, &root, 200).await?;
    // While the read is paused after its root read, another connection changes the edges below
    // the root and completes a prerequisite, and commits.
    let during = paused_read(&s, &a, &root, async {
        set(&s, &a, &p, vec![r.clone()], false).await?;
        complete(&s, &a, &r, CompletionMode::RequireSatisfied).await
    })
    .await?;
    assert_eq!(
        during, before,
        "every member comes from the snapshot the read began with"
    );
    let after = graph(&s, &a, &root, 200).await?;
    verify(&s, &a, &after, &none()).await?;
    assert_ne!(after, before, "the next read sees the writes");
    // Negative control: expanding on a later snapshot is detected by the same schedule.
    s.hooks().split_snapshot_reads(true);
    let torn = paused_read(&s, &a, &root, async {
        set(&s, &a, &p, vec![q.clone()], false).await
    })
    .await?;
    s.hooks().split_snapshot_reads(false);
    assert_ne!(torn, after, "a split read observes the later write");
    assert_eq!(
        torn.nodes[1].version,
        version(&s, &p).await?,
        "the torn read expanded on the later snapshot"
    );
    if postgres() {
        let seen = s.hooks().isolation_seen();
        assert_eq!(seen.len() as u32, s.hooks().count(POINT));
        assert!(seen.len() >= 4);
        assert!(
            seen.iter().all(|level| level == "repeatable read"),
            "{seen:?}"
        );
    }
    Ok(())
}

// C8 concurrent opposite edges on both engines -----------------------------------------------

#[tokio::test]
async fn c8_concurrent_opposite_edges_cannot_create_a_cycle_on_either_engine() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    for _ in 0..4 {
        let x = make(&s, &a, "X", None).await?;
        let y = make(&s, &a, "Y", None).await?;
        let (left, right) = tokio::join!(
            set(&s, &a, &x, vec![y.clone()], false),
            set(&s, &a, &y, vec![x.clone()], false)
        );
        assert_ne!(left.is_ok(), right.is_ok(), "{left:?} {right:?}");
        let (winner, loser) = if left.is_ok() { (&x, &y) } else { (&y, &x) };
        let failed = left.err().or(right.err()).unwrap();
        assert_eq!(failed.to_string(), "conflict");
        // The graph shows exactly the one edge that was stored.
        let g = graph(&s, &a, winner, 200).await?;
        verify(&s, &a, &g, &none()).await?;
        assert_eq!(edge_set(&g), BTreeSet::from([pair(loser, winner)]));
        let g = graph(&s, &a, loser, 200).await?;
        verify(&s, &a, &g, &none()).await?;
        assert!(g.edges.is_empty());
    }
    Ok(())
}

// C9 views: whole responses, not merges (revision 1, review finding R1) ---------------------
//
// The documented client contract replaces a view only with a whole newer response and never
// unions separate responses. These cases pin the facts that make a version-keyed merge unsafe:
// versions are edit preconditions rather than graph revisions, responses never list what has
// gone, and several node fields are relative to the request.

fn versions(g: &DependencyGraph) -> BTreeMap<String, i64> {
    g.nodes
        .iter()
        .map(|n| (n.occurrence_id.clone(), n.version))
        .collect()
}

/// R → P → Q, all owned by `a`, each shared read-only with `with` when given.
async fn chain(s: &Store, a: &str, with: Option<&str>) -> Result<(String, String, String)> {
    let policy = || with.map(|b| grant(&[b], false));
    let q = make(s, a, "Queue the order", policy()).await?;
    let p = make(s, a, "Pay the deposit", policy()).await?;
    let r = make(s, a, "Reserve the room", policy()).await?;
    set(s, a, &p, vec![q.clone()], false).await?;
    set(s, a, &r, vec![p.clone()], false).await?;
    Ok((r, p, q))
}

#[tokio::test]
async fn c9_completion_changes_states_but_no_version() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let (r, p, q) = chain(&s, &a, None).await?;
    let before = graph(&s, &a, &r, 200).await?;
    verify(&s, &a, &before, &none()).await?;
    assert_eq!(node(&before, &r).dependency_state, DependencyState::Waiting);
    assert_eq!(node(&before, &p).dependency_state, DependencyState::Waiting);
    complete(&s, &a, &p, CompletionMode::AdvisoryOverride).await?;
    let after = graph(&s, &a, &r, 200).await?;
    verify(&s, &a, &after, &none()).await?;
    assert_eq!(node(&after, &r).dependency_state, DependencyState::Ready);
    assert_eq!(
        node(&after, &p).dependency_state,
        DependencyState::Satisfied
    );
    assert_eq!(node(&after, &p).outcome, Outcome::Complete);
    assert_eq!(node(&after, &q).dependency_state, DependencyState::Ready);
    // Every version is unchanged, yet two nodes differ: equal versions do not mean equal nodes,
    // so preferring the higher version (or either one on a tie) can keep the stale state.
    assert_eq!(versions(&before), versions(&after));
    assert_eq!(edge_set(&before), edge_set(&after));
    for o in [&r, &p] {
        assert_ne!(node(&before, o), node(&after, o), "{o}");
    }
    // Whole replacement is exactly the current view: re-reading gives the same response.
    assert_eq!(graph(&s, &a, &r, 200).await?, after);
    Ok(())
}

#[tokio::test]
async fn c9_a_view_rooted_elsewhere_reports_request_relative_fields() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let (r, p, q) = chain(&s, &a, None).await?;
    let shallow = graph(&s, &a, &r, 1).await?;
    verify(&s, &a, &shallow, &none()).await?;
    assert_eq!(ids(&shallow), [r.clone(), p.clone()]);
    assert_eq!(shallow.coverage, GraphCoverage::Partial);
    let under_r = node(&shallow, &p);
    assert_eq!(
        (
            under_r.relation,
            under_r.depth,
            under_r.prerequisites,
            under_r.restricted_prerequisites,
            under_r.dependency_state
        ),
        (
            GraphRelation::Direct,
            1,
            NodeExpansion::Unexpanded,
            None,
            DependencyState::Unknown
        )
    );
    // "Open from here": a separate, complete view rooted at the unexpanded node.
    let from_p = graph(&s, &a, &p, 200).await?;
    verify(&s, &a, &from_p, &none()).await?;
    assert_eq!(from_p.root_id, p);
    assert_eq!(from_p.coverage, GraphCoverage::Complete);
    assert_eq!(edge_set(&from_p), BTreeSet::from([pair(&q, &p)]));
    let own = node(&from_p, &p);
    assert_eq!(
        (
            own.relation,
            own.depth,
            own.prerequisites,
            own.restricted_prerequisites,
            own.dependency_state
        ),
        (
            GraphRelation::Root,
            0,
            NodeExpansion::Expanded,
            Some(false),
            DependencyState::Waiting
        )
    );
    // Same occurrence, same version, different request-relative fields.
    assert_eq!(under_r.version, own.version);
    assert_ne!(under_r, own);
    let full = graph(&s, &a, &r, 200).await?;
    verify(&s, &a, &full, &none()).await?;
    let (deep, near) = (node(&full, &q), node(&from_p, &q));
    assert_eq!((deep.relation, deep.depth), (GraphRelation::Transitive, 2));
    assert_eq!((near.relation, near.depth), (GraphRelation::Direct, 1));
    assert_eq!(deep.version, near.version);
    // Opening a view changes no other view: the parent re-reads identically.
    assert_eq!(graph(&s, &a, &r, 1).await?, shallow);
    Ok(())
}

#[tokio::test]
async fn c9_revocation_shrinks_a_fresh_view_with_no_version_change() -> Result<()> {
    let (s, _dir) = setup().await?;
    let (a, b) = (account(&s).await?, account(&s).await?);
    let (r, p, q) = chain(&s, &a, Some(&b)).await?;
    let before = graph(&s, &b, &r, 200).await?;
    verify(&s, &b, &before, &none()).await?;
    assert_eq!(ids(&before), [r.clone(), p.clone(), q.clone()]);
    assert!(!before.restricted);
    // B also opened a view rooted at P.
    let from_p = graph(&s, &b, &p, 200).await?;
    verify(&s, &b, &from_p, &none()).await?;
    assert_eq!(ids(&from_p), [p.clone(), q.clone()]);
    // A revokes B's access to P's task. Q is still shared, but reachable only through P.
    replace(&s, &a, &lineage(&s, &p).await?[0], Policy::default()).await?;
    let after = graph(&s, &b, &r, 200).await?;
    verify(&s, &b, &after, &BTreeSet::from([p.clone()])).await?;
    assert_eq!(ids(&after), std::slice::from_ref(&r));
    assert!(after.edges.is_empty() && after.direct_prerequisites.is_empty());
    assert!(after.restricted);
    assert_eq!(after.nodes[0].dependency_state, DependencyState::Restricted);
    // Nothing in the fresh response marks P or Q as removed, and the remaining node's version
    // is unchanged: only whole replacement drops them.
    assert_eq!(after.nodes[0].version, node(&before, &r).version);
    let body = serde_json::to_string(&after)?;
    for gone in [p.as_str(), q.as_str(), "Pay the deposit", "Queue the order"] {
        assert!(!body.contains(gone), "{gone} still in the fresh view");
    }
    // The view opened from P is gone too, as `not_found`.
    assert_eq!(
        graph(&s, &b, &p, 200).await.unwrap_err().to_string(),
        "not_found"
    );
    // Control: an actual change to R's own prerequisites advances R's version, which is what
    // the version is for (the editor's `expected_version`).
    set(&s, &a, &r, vec![], false).await?;
    let edited = graph(&s, &a, &r, 200).await?;
    verify(&s, &a, &edited, &none()).await?;
    assert!(edited.nodes[0].version > before.nodes[0].version);
    assert!(edited.edges.is_empty() && edited.nodes.len() == 1);
    Ok(())
}
