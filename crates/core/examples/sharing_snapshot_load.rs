//! Reproducible sharing-snapshot workload: the payload and latency of `sharing_snapshot` against
//! the two separate reads it replaces (`defaults` then `households`), for an account that belongs
//! to 1, 8 and 32 households of about 100 members each, with all five default templates naming
//! every household it belongs to.
//! Uses a disposable SQLite file, or a fresh schema in the disposable PostgreSQL database named
//! by ATLAS_TEST_POSTGRES_URL (as the test suites do); it never touches an existing schema.
//! `ATLAS_LOAD_ITERATIONS` (default 200) sets the timed iterations of each read at each size.
use anyhow::{Result, ensure};
use atlas_core::{
    Store,
    households::ManagementCommand,
    policy::{DefaultTemplate, Policy, PrincipalGrant},
};
use serde_json::{Value, json};
use std::time::Instant;
use uuid::Uuid;

/// The instance account cap is 100, so a household holds at most 100 members. Three of the 100
/// accounts are the measured callers; the rest belong to every household.
const ACCOUNTS: usize = 100;
const SIZES: [usize; 3] = [1, 8, 32];
const KINDS: [&str; 5] = ["person", "field", "task", "list", "progress"];

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

fn canonical(mut value: Value) -> Value {
    if let Some(members) = value.get_mut("members").and_then(Value::as_array_mut) {
        members.sort_by_key(|m| m["account_id"].as_str().unwrap().to_owned());
    }
    value
}

/// 32 households. Every filler account belongs to all of them; caller `n` belongs to the first
/// `n`, so a caller's households have 97 to 100 members.
async fn seed(store: &Store) -> Result<(Vec<String>, Vec<(usize, String)>)> {
    let households: Vec<String> = (0..32).map(|_| Uuid::new_v4().to_string()).collect();
    let mut fillers = Vec::new();
    for _ in 0..ACCOUNTS - SIZES.len() {
        let id = Uuid::new_v4().to_string();
        store
            .add_account(&id, &format!("f{}", id.replace('-', "")), "load")
            .await?;
        fillers.push(id);
    }
    let mut callers = Vec::new();
    for size in SIZES {
        let id = Uuid::new_v4().to_string();
        store
            .add_account(&id, &format!("c{size}-{}", id.replace('-', "")), "load")
            .await?;
        callers.push((size, id));
    }
    let mut tx = store.begin_serial().await?;
    for (index, household) in households.iter().enumerate() {
        sqlx::query("INSERT INTO households(id,name) VALUES ($1,$2)")
            .bind(household)
            .bind(format!("Household {index}"))
            .execute(&mut *tx)
            .await?;
        for filler in &fillers {
            sqlx::query("INSERT INTO household_memberships VALUES ($1,$2,'member')")
                .bind(household)
                .bind(filler)
                .execute(&mut *tx)
                .await?;
        }
        for (size, caller) in &callers {
            if index < *size {
                let role = if index == 0 { "manager" } else { "member" };
                sqlx::query("INSERT INTO household_memberships VALUES ($1,$2,$3)")
                    .bind(household)
                    .bind(caller)
                    .bind(role)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    tx.commit().await?;
    Ok((households, callers))
}

async fn manage(store: &Store, caller: &str, command: ManagementCommand) -> Result<()> {
    store
        .management(caller, &Uuid::new_v4().to_string(), &[command], 1000)
        .await?;
    Ok(())
}

/// The real writers: the caller's primary household and five personal templates, each naming
/// every household the caller belongs to.
async fn configure(store: &Store, caller: &str, households: &[String]) -> Result<()> {
    manage(
        store,
        caller,
        ManagementCommand::SetPrimaryHousehold {
            household_id: Some(households[0].clone()),
            expected_version: 1,
        },
    )
    .await?;
    let template = DefaultTemplate::Explicit {
        policy: Policy {
            grants: households
                .iter()
                .map(|id| PrincipalGrant::Household {
                    id: id.clone(),
                    edit: false,
                })
                .collect(),
            exclude_accounts: vec![],
        },
    };
    for kind in KINDS {
        manage(
            store,
            caller,
            ManagementCommand::SetDefaults {
                household_id: None,
                resource_kind: kind.into(),
                expected_version: 0,
                template: Some(template.clone()),
            },
        )
        .await?;
    }
    Ok(())
}

async fn time<F, Fut>(iterations: usize, mut read: F) -> Result<(f64, f64)>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    for _ in 0..iterations.min(20) {
        read().await?;
    }
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        read().await?;
        samples.push(start.elapsed().as_secs_f64() * 1000.);
    }
    Ok((
        percentile(&mut samples, 0.5),
        percentile(&mut samples, 0.95),
    ))
}

/// The plans of the two statements the snapshot adds per household, with real identifiers, so a
/// full scan of `household_memberships` or `households` would show.
async fn plans(store: &Store, sqlite: bool, household: &str, caller: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for sql in [
        format!(
            "SELECT h.name,h.version,m.role FROM households h JOIN household_memberships m ON m.household_id=h.id WHERE h.id='{household}' AND m.account_id='{caller}'"
        ),
        format!(
            "SELECT a.id,a.username,m.role FROM household_memberships m JOIN accounts a ON a.id=m.account_id WHERE m.household_id='{household}' ORDER BY a.id"
        ),
    ] {
        let explain = if sqlite {
            "EXPLAIN QUERY PLAN "
        } else {
            "EXPLAIN "
        };
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!("{explain}{sql}")))
            .fetch_all(&store.pool)
            .await?;
        use sqlx::Row;
        for row in rows {
            let detail: String = row.get(if sqlite { 3 } else { 0 });
            out.push(detail);
        }
    }
    Ok(out)
}

#[tokio::main]
async fn main() -> Result<()> {
    let iterations: usize = std::env::var("ATLAS_LOAD_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let (store, engine, _dir) = connect().await?;
    store.migrate().await?;
    let (households, callers) = seed(&store).await?;
    for (size, caller) in &callers {
        configure(&store, caller, &households[..*size]).await?;
        // The workload is meaningful only if the caller resolves to what was seeded, and the
        // snapshot is the two separate reads over the same state.
        let snapshot = store.sharing_snapshot(caller).await?;
        ensure!(
            snapshot.households.len() == *size,
            "{size} households expected, {} listed",
            snapshot.households.len()
        );
        ensure!(
            serde_json::to_value(&snapshot.defaults)?
                == serde_json::to_value(store.defaults(caller).await?)?,
            "snapshot defaults differ from defaults"
        );
        let separate = store.households(caller).await?;
        for (listed, plain) in snapshot.households.iter().zip(&separate) {
            ensure!(
                canonical(serde_json::to_value(listed)?) == canonical(serde_json::to_value(plain)?),
                "snapshot household differs from households"
            );
        }
        let members: Vec<usize> = snapshot
            .households
            .iter()
            .map(|h| h.members.len())
            .collect();

        let snapshot_bytes = serde_json::to_vec(&snapshot)?.len();
        let separate_bytes = serde_json::to_vec(&store.defaults(caller).await?)?.len()
            + serde_json::to_vec(&separate)?.len();
        let (snapshot_p50, snapshot_p95) = time(iterations, || async {
            serde_json::to_vec(&store.sharing_snapshot(caller).await?)?;
            Ok(())
        })
        .await?;
        let (pair_p50, pair_p95) = time(iterations, || async {
            serde_json::to_vec(&store.defaults(caller).await?)?;
            serde_json::to_vec(&store.households(caller).await?)?;
            Ok(())
        })
        .await?;
        let (households_p50, households_p95) = time(iterations, || async {
            serde_json::to_vec(&store.households(caller).await?)?;
            Ok(())
        })
        .await?;
        println!(
            "{}",
            json!({
                "scenario": "sharing-snapshot",
                "engine": engine,
                "households": size,
                "members_per_household": {"min": members.iter().min(), "max": members.iter().max()},
                "iterations": iterations,
                "snapshot_bytes": snapshot_bytes,
                "separate_reads_bytes": separate_bytes,
                "snapshot_ms": {"p50": snapshot_p50, "p95": snapshot_p95},
                "defaults_then_households_ms": {"p50": pair_p50, "p95": pair_p95},
                "households_only_ms": {"p50": households_p50, "p95": households_p95},
                "snapshot_p95_over_households_p95": snapshot_p95 / households_p95,
                "snapshot_ms_per_household_p50": snapshot_p50 / *size as f64,
            })
        );
    }
    let (size, caller) = callers.last().unwrap();
    println!(
        "{}",
        json!({
            "scenario": "sharing-snapshot-query-plans",
            "engine": engine,
            "households": size,
            "plans": plans(&store, engine == "sqlite", &households[0], caller).await?,
        })
    );
    Ok(())
}
