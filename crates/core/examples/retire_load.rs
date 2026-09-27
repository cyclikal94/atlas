//! Reproducible device-retirement workload: cost of one retirement, the device listing and the
//! effect of retirements on unrelated command writers.
//! Uses a disposable SQLite file, or a fresh schema in the disposable PostgreSQL database named
//! by ATLAS_TEST_POSTGRES_URL (as the test suites do); it never touches an existing schema.
use anyhow::{Result, ensure};
use atlas_core::{Command, Store};
use std::time::Instant;
use uuid::Uuid;

const SESSIONS: usize = 32;
const HANDOFFS: usize = 20;
const SUBSCRIPTIONS: usize = 5;
const DELIVERIES: i64 = 10_000;

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

/// Populate one device with every kind of approved-state member and a large delivery ledger.
async fn heavy_device(store: &Store, account: &str, device: &str) -> Result<()> {
    let mut tx = store.begin_serial().await?;
    for _ in 0..SESSIONS {
        sqlx::query("INSERT INTO sessions(token_hash,account_id,device_id,expires_at,session_id,created_at,auth_kind) VALUES ($1,$2,$3,9999999999,$4,1,'local')")
            .bind(Uuid::new_v4().to_string()).bind(account).bind(device).bind(Uuid::new_v4().to_string())
            .execute(&mut *tx).await?;
    }
    for _ in 0..HANDOFFS {
        sqlx::query("INSERT INTO native_handoffs(code_hash,account_id,device_id,challenge,configuration_hash,redirect_uri,expires_at) VALUES ($1,$2,$3,'c','h','r',9999999999)")
            .bind(Uuid::new_v4().to_string()).bind(account).bind(device).execute(&mut *tx).await?;
    }
    for _ in 0..SUBSCRIPTIONS {
        sqlx::query("INSERT INTO notification_subscriptions(id,account_id,device_id,transport,secret,version,active) VALUES ($1,$2,$3,'webpush','s',1,1)")
            .bind(Uuid::new_v4().to_string()).bind(account).bind(device).execute(&mut *tx).await?;
    }
    sqlx::query(
        "INSERT INTO sync_devices(account_id,device_id,last_seen,cursor_key) VALUES ($1,$2,1,$3)",
    )
    .bind(account)
    .bind(device)
    .bind(Uuid::new_v4().to_string())
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO sync_deliveries(account_id,device_id,resource_id) SELECT $1,$2,'r'||n FROM (WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t WHERE n<$3) SELECT n FROM t) x")
        .bind(account).bind(device).bind(DELIVERIES).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// The retirement under measurement. Kept in one place so the same workload measures the
/// pre-change and post-change implementations.
async fn retire(store: &Store, account: &str, device: &str) -> Result<()> {
    let token = store
        .devices(account, 1000)
        .await?
        .into_iter()
        .find(|d| d.id == device)
        .ok_or_else(|| anyhow::anyhow!("device not listed"))?
        .state_token;
    let outcome = store
        .retire_device(account, device, &Uuid::new_v4().to_string(), &token, 1000)
        .await?;
    ensure!(
        outcome.outcome == atlas_core::operations::Outcome::ConfirmedApplied,
        "retirement was not applied"
    );
    Ok(())
}

async fn write_latency(
    store: &Store,
    account: &str,
    writers: usize,
    each: usize,
) -> Result<Vec<f64>> {
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..writers {
        let (store, account) = (store.clone(), account.to_owned());
        jobs.spawn(async move {
            let mut samples = Vec::new();
            for _ in 0..each {
                let start = Instant::now();
                store
                    .apply(
                        &account,
                        &Uuid::new_v4().to_string(),
                        &[Command::CreatePerson {
                            id: Uuid::new_v4().to_string(),
                            name: "Load".into(),
                            initial_policy: None,
                        }],
                    )
                    .await?;
                samples.push(start.elapsed().as_secs_f64() * 1000.);
            }
            Ok::<_, anyhow::Error>(samples)
        });
    }
    let mut all = Vec::new();
    while let Some(result) = jobs.join_next().await {
        all.extend(result??);
    }
    Ok(all)
}

#[tokio::main]
async fn main() -> Result<()> {
    let iterations: usize = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "20".into())
        .parse()?;
    ensure!((3..=200).contains(&iterations), "iterations must be 3..200");
    let (store, backend, _dir) = connect().await?;
    store.migrate().await?;
    let account = Uuid::from_u128(1).to_string();
    let writer_account = Uuid::from_u128(2).to_string();
    store
        .add_account(&account, "retiring", "benchmark-only")
        .await?;
    store
        .add_account(&writer_account, "writing", "benchmark-only")
        .await?;

    // 1. One retirement of a heavy device.
    let mut retirement_ms = Vec::new();
    for n in 0..iterations {
        let device = format!("heavy-{n}");
        heavy_device(&store, &account, &device).await?;
        let start = Instant::now();
        retire(&store, &account, &device).await?;
        retirement_ms.push(start.elapsed().as_secs_f64() * 1000.);
    }

    // 2. Listing 32 devices, each with a session, a registration, a subscription and a handoff.
    for n in 0..32 {
        let device = format!("listed-{n}");
        let mut tx = store.begin_serial().await?;
        sqlx::query("INSERT INTO sessions(token_hash,account_id,device_id,expires_at,session_id,created_at,auth_kind) VALUES ($1,$2,$3,9999999999,$4,1,'local')")
            .bind(Uuid::new_v4().to_string()).bind(&account).bind(&device).bind(Uuid::new_v4().to_string()).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO sync_devices(account_id,device_id,last_seen,cursor_key) VALUES ($1,$2,1,$3)")
            .bind(&account).bind(&device).bind(Uuid::new_v4().to_string()).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO notification_subscriptions(id,account_id,device_id,transport,secret,version,active) VALUES ($1,$2,$3,'webpush','s',1,1)")
            .bind(Uuid::new_v4().to_string()).bind(&account).bind(&device).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO native_handoffs(code_hash,account_id,device_id,challenge,configuration_hash,redirect_uri,expires_at) VALUES ($1,$2,$3,'c','h','r',9999999999)")
            .bind(Uuid::new_v4().to_string()).bind(&account).bind(&device).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    let mut listing_ms = Vec::new();
    for _ in 0..50 {
        let start = Instant::now();
        let devices = store.devices(&account, 1000).await?;
        listing_ms.push(start.elapsed().as_secs_f64() * 1000.);
        ensure!(devices.len() == 32, "expected 32 listed devices");
    }

    // 3. Unrelated command writers with and without retirements running.
    let mut alone = write_latency(&store, &writer_account, 8, 25).await?;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let retiring = {
        let (store, account, stop) = (store.clone(), account.clone(), stop.clone());
        tokio::spawn(async move {
            let mut done = 0_u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let device = format!("churn-{done}");
                heavy_device(&store, &account, &device).await?;
                retire(&store, &account, &device).await?;
                done += 1;
            }
            Ok::<_, anyhow::Error>(done)
        })
    };
    let mut beside = write_latency(&store, &writer_account, 8, 25).await?;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let retirements_during = retiring.await??;

    println!(
        "{}",
        serde_json::json!({
            "backend": backend, "profile": if cfg!(debug_assertions) {"debug"} else {"release"},
            "fixture": {"sessions": SESSIONS, "native_handoffs": HANDOFFS, "subscriptions": SUBSCRIPTIONS, "deliveries": DELIVERIES},
            "retirement_iterations": iterations,
            "retirement_p50_ms": percentile(&mut retirement_ms, 0.5),
            "retirement_p95_ms": percentile(&mut retirement_ms, 0.95),
            "listing_32_devices_p50_ms": percentile(&mut listing_ms, 0.5),
            "listing_32_devices_p95_ms": percentile(&mut listing_ms, 0.95),
            "writers_alone_p50_ms": percentile(&mut alone, 0.5),
            "writers_alone_p95_ms": percentile(&mut alone, 0.95),
            "writers_beside_retirements_p50_ms": percentile(&mut beside, 0.5),
            "writers_beside_retirements_p95_ms": percentile(&mut beside, 0.95),
            "retirements_during_writer_phase": retirements_during,
        })
    );
    Ok(())
}
