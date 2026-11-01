//! Reproducible dependency-graph workload (BE-DEPENDENCY-GRAPH): latency of `dependency_graph`
//! for the two largest shapes the 200-node budget allows, and the number of statements one read
//! issues (PostgreSQL with `pg_stat_statements` loaded; otherwise reported as unavailable).
//!
//! - `chain`: a 250-occurrence chain, read from its head (200 nodes, 199 edges, partial).
//! - `fan`: a root with 32 direct prerequisites, each with 32 of its own (1,057 occurrences);
//!   the read returns 193 nodes and 192 edges and leaves 27 tier nodes unexpanded.
//! - `fan_depth_1`: the same root at `max_depth=1`, the cheap editor read (33 nodes).
//!
//! Occurrences are created and linked through the real task commands, so every row is what the
//! API would write; the instance stays far below the 10,000-resource ceiling. Uses a disposable
//! SQLite file, or a fresh schema in the disposable PostgreSQL database named by
//! ATLAS_TEST_POSTGRES_URL; it never touches an existing schema. `ATLAS_LOAD_ITERATIONS`
//! (default 30) sets the timed reads per shape. Run it in a release build, alone on the host:
//! `cargo run --release --locked -p atlas-core --example dependency_graph_load`.
use anyhow::{Result, ensure};
use atlas_core::{Store, tasks::*};
use serde_json::json;
use std::time::Instant;
use uuid::Uuid;

const NOW: i64 = 1788868800;

fn percentile(samples: &mut [f64], quantile: f64) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[((samples.len() as f64 * quantile).ceil() as usize).saturating_sub(1)]
}

fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn id() -> String {
    Uuid::new_v4().to_string()
}

async fn connect() -> Result<(Store, &'static str, Option<tempfile::TempDir>)> {
    if let Ok(base) = std::env::var("ATLAS_TEST_POSTGRES_URL") {
        let admin = Store::connect(&base).await?;
        let schema = format!("load_{}", Uuid::new_v4().simple());
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin.pool)
            .await?;
        admin.pool.close().await;
        let mut url = url::Url::parse(&base)?;
        url.query_pairs_mut()
            .append_pair("options", &format!("--search_path={schema},public"));
        Ok((Store::connect(url.as_str()).await?, "postgres", None))
    } else {
        let dir = tempfile::tempdir()?;
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("load.sqlite").display()
        );
        Ok((Store::connect(&url).await?, "sqlite", Some(dir)))
    }
}

async fn occurrence(store: &Store, actor: &str, title: &str) -> Result<String> {
    let task = id();
    store
        .task_command(
            actor,
            &id(),
            &TaskCommand::CreateTask {
                id: task.clone(),
                execution_id: id(),
                title: title.into(),
                anchor: None,
                initial_policy: None,
                definition: Definition {
                    schedule: Schedule {
                        start_date: None,
                        time: None,
                        timezone: "Europe/London".into(),
                        repeat: None,
                    },
                    goal: Goal::Checkbox,
                    carry: Carry::RetainOne,
                    participation: Participation::Personal,
                    open_days_before: 0,
                    close_days_after: 1,
                    allow_streak_exclusions: false,
                },
            },
            None,
            NOW,
        )
        .await?;
    occurrence_id(&task, "once")
}

async fn link(
    store: &Store,
    actor: &str,
    dependant: &str,
    prerequisites: Vec<String>,
) -> Result<()> {
    store
        .task_command(
            actor,
            &id(),
            &TaskCommand::SetDependencies {
                occurrence_id: dependant.into(),
                expected_version: 1,
                prerequisites,
                strict: false,
            },
            None,
            NOW,
        )
        .await?;
    Ok(())
}

/// Statements issued by one read, from `pg_stat_statements` (reset first). `None` when the
/// extension is not loaded.
async fn statements(store: &Store, actor: &str, root: &str, depth: u16) -> Result<Option<i64>> {
    if sqlx::query("SELECT count(*) FROM (SELECT public.pg_stat_statements_reset()) r")
        .execute(&store.pool)
        .await
        .is_err()
    {
        return Ok(None);
    }
    store.dependency_graph(actor, root, depth, NOW).await?;
    let count: i64 = sqlx::query_scalar("SELECT COALESCE(sum(calls),0)::bigint FROM public.pg_stat_statements WHERE query NOT ILIKE '%pg_stat_statements%'")
        .fetch_one(&store.pool)
        .await?;
    Ok(Some(count))
}

#[tokio::main]
async fn main() -> Result<()> {
    let iterations: usize = std::env::var("ATLAS_LOAD_ITERATIONS")
        .ok()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(30);
    let (store, engine, _dir) = connect().await?;
    store.migrate().await?;
    let actor = id();
    store.add_account(&actor, "graph-load", "load").await?;
    let seeding = Instant::now();
    let mut chain = Vec::new();
    for i in 0..250 {
        chain.push(occurrence(&store, &actor, &format!("Step {i}")).await?);
    }
    for i in 0..249 {
        link(&store, &actor, &chain[i], vec![chain[i + 1].clone()]).await?;
    }
    let root = occurrence(&store, &actor, "Root").await?;
    let mut tier = Vec::new();
    for i in 0..32 {
        let t = occurrence(&store, &actor, &format!("Tier {i}")).await?;
        let mut leaves = Vec::new();
        for j in 0..32 {
            leaves.push(occurrence(&store, &actor, &format!("Leaf {i}.{j}")).await?);
        }
        link(&store, &actor, &t, leaves).await?;
        tier.push(t);
    }
    link(&store, &actor, &root, tier).await?;
    let seed_seconds = seeding.elapsed().as_secs_f64();
    let mut shapes = serde_json::Map::new();
    for (name, head, depth) in [
        ("chain", &chain[0], 200u16),
        ("fan", &root, 200),
        ("fan_depth_1", &root, 1),
    ] {
        let first = store.dependency_graph(&actor, head, depth, NOW).await?;
        let mut samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let started = Instant::now();
            let graph = store.dependency_graph(&actor, head, depth, NOW).await?;
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
            ensure!(
                graph == first,
                "repeated reads of an unchanged graph differ"
            );
        }
        let unexpanded = first
            .nodes
            .iter()
            .filter(|n| n.prerequisites == NodeExpansion::Unexpanded)
            .count();
        shapes.insert(
            name.into(),
            json!({
                "max_depth": depth,
                "nodes": first.nodes.len(),
                "edges": first.edges.len(),
                "unexpanded": unexpanded,
                "coverage": first.coverage,
                "body_bytes": serde_json::to_vec(&first)?.len(),
                "statements": statements(&store, &actor, head, depth).await?,
                "ms_p50": round(percentile(&mut samples, 0.5)),
                "ms_p95": round(percentile(&mut samples, 0.95)),
                "ms_max": round(percentile(&mut samples, 1.0)),
            }),
        );
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "engine": engine,
            "iterations": iterations,
            "occurrences": 250 + 1 + 32 + 32 * 32,
            "seed_seconds": round(seed_seconds),
            "shapes": shapes,
        }))?
    );
    Ok(())
}
