//! Reproducible workload for the account-wide timer list (BE-B6): `account_timer_sessions` for one
//! account holding 1k, 10k and 100k finished sessions across 500 occurrences plus 20 running
//! timers, half of them on tasks the caller can no longer read.
//!
//! Reports, per engine: the first, middle and deepest cursor page at page sizes 50 and 200; a
//! 200-row page over 200 distinct occurrences against 200 rows on one occurrence (the access-check
//! cost); payload bytes; and the query plans and timings with and without the
//! `timer_account_stopped` history index, so the index is kept on evidence, not on assumption.
//! `ATLAS_LOAD_MODE=migrate` instead times the `1003 -> 1004` upgrade of a frozen-1003 database
//! holding 100k and 1M timer rows, and, on PostgreSQL, the longest a concurrent timer write waits
//! while the index is built.
//!
//! Uses a disposable SQLite file, or a fresh schema in the disposable PostgreSQL database named
//! by ATLAS_TEST_POSTGRES_URL (as the test suites do); it never touches an existing schema.
//! `ATLAS_LOAD_ITERATIONS` (default 30) sets the timed iterations of each read.
use anyhow::{Result, ensure};
use atlas_core::{
    Command, Store,
    policy::{Policy, PrincipalGrant},
    tasks::*,
};
use serde_json::json;
use sqlx::Row;
use std::time::{Duration, Instant};
use uuid::Uuid;

const NOW: i64 = 1_788_868_800;
const OCCURRENCES: usize = 500;
const RUNNING: usize = 20;
const SIZES: [usize; 3] = [1_000, 10_000, 100_000];

fn percentile(samples: &mut [f64], quantile: f64) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[((samples.len() as f64 * quantile).ceil() as usize).saturating_sub(1)]
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
            .append_pair("options", &format!("--search_path={schema}"));
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

fn seconds() -> Goal {
    Goal::Numeric {
        minimum: Some(Decimal("60".into())),
        maximum: None,
        unit: "seconds".into(),
    }
}

fn definition(participation: Participation) -> Definition {
    Definition {
        schedule: Schedule {
            start_date: None,
            time: None,
            timezone: "UTC".into(),
            repeat: None,
        },
        goal: seconds(),
        carry: Carry::RetainOne,
        participation,
        open_days_before: 0,
        close_days_after: 1,
        allow_streak_exclusions: false,
    }
}

async fn create(
    store: &Store,
    owner: &str,
    policy: Option<Policy>,
    participation: Participation,
) -> Result<(String, String)> {
    let task = Uuid::new_v4().to_string();
    store
        .task_command(
            owner,
            &Uuid::new_v4().to_string(),
            &TaskCommand::CreateTask {
                id: task.clone(),
                execution_id: Uuid::new_v4().to_string(),
                title: "Timed".into(),
                definition: definition(participation),
                anchor: None,
                initial_policy: policy,
            },
            None,
            NOW,
        )
        .await?;
    let occurrence = occurrence_id(&task, "once")?;
    Ok((task, occurrence))
}

async fn start(store: &Store, actor: &str, occurrence: &str, at: i64) -> Result<()> {
    store
        .task_command(
            actor,
            &Uuid::new_v4().to_string(),
            &TaskCommand::StartTimer {
                occurrence_id: occurrence.into(),
                session_id: Uuid::new_v4().to_string(),
                started_at: at,
            },
            None,
            NOW,
        )
        .await?;
    Ok(())
}

async fn account(store: &Store, label: &str) -> Result<String> {
    let id = Uuid::new_v4().to_string();
    store
        .add_account(&id, &format!("{label}{}", id.replace('-', "")), "load")
        .await?;
    Ok(id)
}

struct Scene {
    caller: String,
    /// Progress streams of the caller's own readable tasks, in a fixed order.
    streams: Vec<String>,
    /// One occurrence's stream holding many sessions, for the access-check comparison.
    single: (String, String),
}

/// 500 readable tasks, 10 running timers on them, and 10 running timers on shared tasks whose
/// access was then withdrawn (so those rows are restricted and cost a full visibility check).
async fn seed_scene(store: &Store) -> Result<Scene> {
    let caller = account(store, "caller").await?;
    let owner = account(store, "owner").await?;
    for index in 0..OCCURRENCES {
        let (_, occurrence) = create(store, &caller, None, Participation::Personal).await?;
        if index < RUNNING / 2 {
            start(store, &caller, &occurrence, NOW - 5_000 - index as i64).await?;
        }
    }
    for index in 0..RUNNING / 2 {
        let policy = Policy {
            grants: vec![PrincipalGrant::Account {
                id: caller.clone(),
                edit: true,
            }],
            exclude_accounts: vec![],
        };
        let (task, occurrence) = create(store, &owner, Some(policy), Participation::Anyone).await?;
        start(store, &caller, &occurrence, NOW - 4_000 - index as i64).await?;
        let version: i64 = sqlx::query_scalar("SELECT policy_version FROM resources WHERE id=$1")
            .bind(&task)
            .fetch_one(&store.pool)
            .await?;
        store
            .apply(
                &owner,
                &Uuid::new_v4().to_string(),
                &[Command::Revoke {
                    id: task,
                    expected_version: version,
                    account_id: caller.clone(),
                }],
            )
            .await?;
    }
    let streams: Vec<String> = sqlx::query_scalar(
        "SELECT p.progress_id FROM occurrence_participants p JOIN resources r ON r.id=p.progress_id \
         WHERE p.account_id=$1 AND r.owner_id=$1 AND NOT EXISTS(SELECT 1 FROM timer_sessions t WHERE t.progress_id=p.progress_id) \
         ORDER BY p.progress_id",
    )
    .bind(&caller)
    .fetch_all(&store.pool)
    .await?;
    ensure!(
        streams.len() == OCCURRENCES - RUNNING / 2,
        "streams: {}",
        streams.len()
    );
    // A second account with one occurrence to hold a page of 200 rows on a single stream.
    let single_owner = account(store, "single").await?;
    let (_, occurrence) = create(store, &single_owner, None, Participation::Personal).await?;
    let single_stream: String = sqlx::query_scalar(
        "SELECT progress_id FROM occurrence_participants WHERE occurrence_id=$1 AND account_id=$2",
    )
    .bind(&occurrence)
    .bind(&single_owner)
    .fetch_one(&store.pool)
    .await?;
    Ok(Scene {
        caller,
        streams,
        single: (single_owner, single_stream),
    })
}

/// Finished sessions `from..to` (index `i` finished at NOW-10*i-1, started ten seconds before),
/// spread round-robin over `streams`, so any 200 consecutive rows cover 200 distinct streams
/// when there are at least 200 streams.
async fn insert_finished(
    store: &Store,
    account: &str,
    streams: &[String],
    from: usize,
    to: usize,
) -> Result<()> {
    for start in (from..to).step_by(400) {
        let end = (start + 400).min(to);
        let mut sql = String::from(
            "INSERT INTO timer_sessions(id,progress_id,account_id,started_at,stopped_at,version,cancelled) VALUES ",
        );
        for (n, _) in (start..end).enumerate() {
            if n > 0 {
                sql.push(',');
            }
            let base = n * 5;
            sql.push_str(&format!(
                "(${},${},${},${},${},1,0)",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5
            ));
        }
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for i in start..end {
            let stopped = NOW - 10 * i as i64 - 1;
            query = query
                .bind(Uuid::new_v4().to_string())
                .bind(&streams[i % streams.len()])
                .bind(account)
                .bind(stopped - 9)
                .bind(stopped);
        }
        query.execute(&store.pool).await?;
    }
    Ok(())
}

struct Timing {
    p50: f64,
    p95: f64,
    bytes: usize,
    rows: usize,
}

async fn time(
    store: &Store,
    actor: &str,
    state: TimerState,
    after: Option<&str>,
    limit: u16,
    iterations: usize,
) -> Result<Timing> {
    let mut samples = Vec::with_capacity(iterations);
    let (mut bytes, mut rows) = (0, 0);
    for _ in 0..iterations {
        let started = Instant::now();
        let page = store
            .account_timer_sessions(actor, state, after, limit)
            .await?;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
        bytes = serde_json::to_vec(&page)?.len();
        rows = page.items.len();
    }
    Ok(Timing {
        p50: percentile(&mut samples.clone(), 0.5),
        p95: percentile(&mut samples, 0.95),
        bytes,
        rows,
    })
}

/// The cursor naming the row at `offset` in the finished-session order.
async fn cursor_at(store: &Store, account: &str, offset: usize) -> Result<String> {
    let row = sqlx::query("SELECT stopped_at,id FROM timer_sessions WHERE account_id=$1 AND stopped_at IS NOT NULL AND cancelled=0 ORDER BY stopped_at DESC,id DESC LIMIT 1 OFFSET $2")
        .bind(account)
        .bind(offset as i64)
        .fetch_one(&store.pool)
        .await?;
    Ok(format!(
        "s.{}.{}",
        row.get::<i64, _>(0),
        row.get::<String, _>(1)
    ))
}

fn line(label: &str, t: &Timing) -> serde_json::Value {
    json!({"case": label, "rows": t.rows, "p50_ms": round(t.p50), "p95_ms": round(t.p95), "payload_bytes": t.bytes})
}

fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// The two history statements as `finished_timers` runs them, with literals for the plan tools.
fn statements(account: &str, resume: Option<(i64, &str)>, limit: u16) -> String {
    match resume {
        None => format!(
            "SELECT id,progress_id,started_at,stopped_at,version FROM timer_sessions WHERE account_id='{account}' AND stopped_at IS NOT NULL AND cancelled=0 ORDER BY stopped_at DESC,id DESC LIMIT {limit}"
        ),
        Some((at, id)) => format!(
            "SELECT id,progress_id,started_at,stopped_at,version FROM timer_sessions WHERE account_id='{account}' AND stopped_at IS NOT NULL AND cancelled=0 AND stopped_at<={at} AND (stopped_at<{at} OR id<'{id}') ORDER BY stopped_at DESC,id DESC LIMIT {limit}"
        ),
    }
}

/// The running-timer statement as `running_timers` runs it.
fn running_statement(engine: &str, account: &str) -> String {
    let index = if engine == "sqlite" {
        " INDEXED BY timer_active_account_occurrence"
    } else {
        ""
    };
    format!(
        "SELECT id,progress_id,started_at,stopped_at,version FROM timer_sessions{index} WHERE account_id='{account}' AND stopped_at IS NULL AND cancelled=0"
    )
}

async fn plan(store: &Store, engine: &str, sql: &str) -> Result<Vec<String>> {
    // A comment makes each variant's text distinct, so no connection reuses a cached plan.
    let sql = format!("{sql} /* {} */", Uuid::new_v4());
    let explain = if engine == "postgres" {
        format!("EXPLAIN (ANALYZE, BUFFERS, TIMING OFF) {sql}")
    } else {
        format!("EXPLAIN QUERY PLAN {sql}")
    };
    let rows = sqlx::query(sqlx::AssertSqlSafe(explain))
        .fetch_all(&store.pool)
        .await?;
    Ok(rows
        .iter()
        .map(|row| row.get::<String, _>(if engine == "postgres" { 0 } else { 3 }))
        .collect())
}

async fn run_listing(iterations: usize) -> Result<()> {
    let (store, engine, _dir) = connect().await?;
    store.migrate().await?;
    let scene = seed_scene(&store).await?;
    let mut inserted = 0;
    let mut first_p95 = Vec::new();
    for size in SIZES {
        let started = Instant::now();
        insert_finished(&store, &scene.caller, &scene.streams, inserted, size).await?;
        inserted = size;
        let seed_seconds = started.elapsed().as_secs_f64();
        if engine == "postgres" {
            // What autovacuum's analyse gives a real table; without it the planner has no row estimate.
            sqlx::query("ANALYZE timer_sessions")
                .execute(&store.pool)
                .await?;
        }
        let deep = cursor_at(&store, &scene.caller, size - 201).await?;
        let middle = cursor_at(&store, &scene.caller, size / 2).await?;
        let mut results = Vec::new();
        for limit in [50_u16, 200] {
            let first = time(
                &store,
                &scene.caller,
                TimerState::All,
                None,
                limit,
                iterations,
            )
            .await?;
            let mid = time(
                &store,
                &scene.caller,
                TimerState::All,
                Some(&middle),
                limit,
                iterations,
            )
            .await?;
            let last = time(
                &store,
                &scene.caller,
                TimerState::All,
                Some(&deep),
                limit,
                iterations,
            )
            .await?;
            if limit == 200 {
                first_p95.push((size, first.p95, last.p95));
            }
            results.push(line(&format!("all first page, limit {limit}"), &first));
            results.push(line(
                &format!("finished, middle cursor, limit {limit}"),
                &mid,
            ));
            results.push(line(
                &format!("finished, deepest cursor, limit {limit}"),
                &last,
            ));
        }
        println!(
            "{}",
            json!({"engine": engine, "finished_sessions": size, "running": RUNNING, "seed_seconds": round(seed_seconds), "results": results})
        );
        if size == SIZES[0] {
            // The access-check cost: 200 rows over one stream against 200 over 200 distinct streams.
            insert_finished(
                &store,
                &scene.single.0,
                std::slice::from_ref(&scene.single.1),
                0,
                1_000,
            )
            .await?;
            let one = time(
                &store,
                &scene.single.0,
                TimerState::Stopped,
                None,
                200,
                iterations,
            )
            .await?;
            let many = time(
                &store,
                &scene.caller,
                TimerState::Stopped,
                None,
                200,
                iterations,
            )
            .await?;
            let running = time(
                &store,
                &scene.caller,
                TimerState::Running,
                None,
                200,
                iterations,
            )
            .await?;
            println!(
                "{}",
                json!({"engine": engine, "access_cost": [line("200 rows, one occurrence", &one), line("200 rows, 200 distinct occurrences", &many), line("all running timers (half restricted)", &running)]})
            );
        }
    }
    // Scale independence: the first page must not slow with history, nor the deepest with depth.
    let (small, large) = (first_p95[0], first_p95[2]);
    println!(
        "{}",
        json!({"engine": engine, "scale": {"first_page_p95_1k_ms": round(small.1), "first_page_p95_100k_ms": round(large.1), "deepest_p95_100k_ms": round(large.2),
        "first_page_ratio_100k_to_1k": round(large.1 / small.1), "deepest_to_first_ratio_at_100k": round(large.2 / large.1)}})
    );

    // The history index against its counterfactual: plans, then the same reads without it.
    let deep = cursor_at(&store, &scene.caller, inserted - 201).await?;
    let (at, id) = {
        let mut parts = deep.splitn(3, '.');
        parts.next();
        (
            parts.next().unwrap().parse::<i64>()?,
            parts.next().unwrap().to_owned(),
        )
    };
    let mut plans = serde_json::Map::new();
    let mut timings = serde_json::Map::new();
    for variant in [
        "with timer_account_stopped",
        "without timer_account_stopped",
    ] {
        if variant.starts_with("without") {
            sqlx::query("DROP INDEX timer_account_stopped")
                .execute(&store.pool)
                .await?;
            if engine == "sqlite" {
                sqlx::query("ANALYZE").execute(&store.pool).await?;
            }
        }
        plans.insert(
            format!("{variant}, running first page"),
            json!(plan(&store, engine, &running_statement(engine, &scene.caller)).await?),
        );
        plans.insert(
            format!("{variant}, first page"),
            json!(plan(&store, engine, &statements(&scene.caller, None, 200)).await?),
        );
        plans.insert(
            format!("{variant}, deepest cursor"),
            json!(
                plan(
                    &store,
                    engine,
                    &statements(&scene.caller, Some((at, &id)), 200)
                )
                .await?
            ),
        );
        let first = time(
            &store,
            &scene.caller,
            TimerState::Stopped,
            None,
            200,
            iterations,
        )
        .await?;
        let last = time(
            &store,
            &scene.caller,
            TimerState::Stopped,
            Some(&deep),
            200,
            iterations,
        )
        .await?;
        timings.insert(
            variant.into(),
            json!([
                line("finished first page, limit 200", &first),
                line("finished deepest cursor, limit 200", &last)
            ]),
        );
    }
    sqlx::query("CREATE INDEX timer_account_stopped ON timer_sessions(account_id,stopped_at,id) WHERE stopped_at IS NOT NULL AND cancelled=0")
        .execute(&store.pool)
        .await?;
    println!(
        "{}",
        json!({"engine": engine, "index_counterfactual": {"timings": timings, "plans": plans}})
    );
    Ok(())
}

/// Rows for a frozen-1003 database: 200 accounts, 2000 streams (person resources stand in for
/// progress streams; the index and the upgrade only see the column), at most one running timer per
/// account as the 1003 index requires. Generated in the database, not in Rust.
async fn seed_migration_rows(store: &Store, engine: &str, rows: i64) -> Result<()> {
    let accounts: Vec<String> = (0..200).map(|_| Uuid::new_v4().to_string()).collect();
    for account in &accounts {
        sqlx::query("INSERT INTO accounts(id,username,password_hash) VALUES ($1,$2,'x')")
            .bind(account)
            .bind(format!("m{}", account.replace('-', "")))
            .execute(&store.pool)
            .await?;
    }
    sqlx::query("CREATE TABLE load_streams(idx BIGINT PRIMARY KEY, progress_id TEXT NOT NULL, account_id TEXT NOT NULL)")
        .execute(&store.pool)
        .await?;
    for index in 0..2000_i64 {
        let (id, account) = (
            Uuid::new_v4().to_string(),
            &accounts[(index % 200) as usize],
        );
        sqlx::query("INSERT INTO resources(id,owner_id,parent_id,kind,label,value) VALUES ($1,$2,NULL,'person','stream','{}')")
            .bind(&id)
            .bind(account)
            .execute(&store.pool)
            .await?;
        sqlx::query("INSERT INTO load_streams VALUES ($1,$2,$3)")
            .bind(index)
            .bind(id)
            .bind(account)
            .execute(&store.pool)
            .await?;
    }
    // One running row per account (stream index < 200), the rest finished.
    let sql = if engine == "postgres" {
        "INSERT INTO timer_sessions(id,progress_id,account_id,started_at,stopped_at,version,cancelled) \
         SELECT gen_random_uuid()::text, s.progress_id, s.account_id, 1000000+i*10, CASE WHEN i<200 THEN NULL ELSE 1000000+i*10+5 END, 1, 0 \
         FROM generate_series(0,$1-1) i JOIN load_streams s ON s.idx=i%2000"
    } else {
        "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i+1 FROM n WHERE i<$1-1) \
         INSERT INTO timer_sessions(id,progress_id,account_id,started_at,stopped_at,version,cancelled) \
         SELECT lower(hex(randomblob(16))), s.progress_id, s.account_id, 1000000+i*10, CASE WHEN i<200 THEN NULL ELSE 1000000+i*10+5 END, 1, 0 \
         FROM n JOIN load_streams s ON s.idx=i%2000"
    };
    sqlx::query(sql).bind(rows).execute(&store.pool).await?;
    sqlx::query("DROP TABLE load_streams")
        .execute(&store.pool)
        .await?;
    Ok(())
}

async fn run_migration() -> Result<()> {
    for rows in [100_000_i64, 1_000_000] {
        let (store, engine, _dir) = connect().await?;
        sqlx::raw_sql(if engine == "postgres" {
            include_str!("../tests/fixtures/schema_1003_postgres.sql")
        } else {
            include_str!("../tests/fixtures/schema_1003_sqlite.sql")
        })
        .execute(&store.pool)
        .await?;
        let seeding = Instant::now();
        seed_migration_rows(&store, engine, rows).await?;
        let seed_seconds = seeding.elapsed().as_secs_f64();
        let counted: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM timer_sessions")
            .fetch_one(&store.pool)
            .await?;
        ensure!(counted == rows, "{counted} rows");
        // A writer probing while the upgrade runs: on PostgreSQL CREATE INDEX holds a SHARE lock, so a
        // timer write waits for the build. Measure the longest wait; SQLite blocks all writers.
        let writer = store.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let probe = tokio::spawn(async move {
            let mut longest = Duration::ZERO;
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                let started = Instant::now();
                let _ = sqlx::query("UPDATE timer_sessions SET version=version WHERE id=(SELECT id FROM timer_sessions LIMIT 1)").execute(&writer.pool).await;
                longest = longest.max(started.elapsed());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            longest
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = Instant::now();
        store.migrate().await?;
        let migrate_seconds = started.elapsed().as_secs_f64();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let longest = probe.await?;
        let version: i64 = sqlx::query_scalar("SELECT version FROM atlas_schema")
            .fetch_one(&store.pool)
            .await?;
        ensure!(version == 1004);
        let running: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM timer_sessions WHERE stopped_at IS NULL AND cancelled=0",
        )
        .fetch_one(&store.pool)
        .await?;
        println!(
            "{}",
            json!({"engine": engine, "migration": "1003 -> 1004", "timer_rows": rows, "running_rows": running,
            "seed_seconds": round(seed_seconds), "upgrade_seconds": round(migrate_seconds), "longest_concurrent_write_wait_ms": round(longest.as_secs_f64() * 1000.0),
            "unique_index_violated": false})
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let iterations: usize = std::env::var("ATLAS_LOAD_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    if std::env::var("ATLAS_LOAD_MODE").as_deref() == Ok("migrate") {
        run_migration().await
    } else {
        run_listing(iterations).await
    }
}
