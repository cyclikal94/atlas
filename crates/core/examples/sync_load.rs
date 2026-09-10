//! Reproducible core workload: 100 accounts, one owner's N resources, one-resource edits.
//! Use a disposable SQLite database; this never accepts an existing database URL.
use anyhow::{Result, ensure};
use atlas_core::{Command, Store};
use std::time::Instant;
use uuid::Uuid;

fn percentile(samples: &mut [f64], quantile: f64) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[((samples.len() as f64 * quantile).ceil() as usize).saturating_sub(1)]
}
#[tokio::main]
async fn main() -> Result<()> {
    let count: i64 = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "10000".into())
        .parse()?;
    ensure!(
        (1..=10000).contains(&count),
        "resource count must be 1..10000"
    );
    let dir = tempfile::tempdir()?;
    let store = Store::connect(&format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("load.sqlite").display()
    ))
    .await?;
    store.migrate().await?;
    let owner = Uuid::from_u128(1).to_string();
    for n in 1..=100 {
        store
            .add_account(
                &Uuid::from_u128(n).to_string(),
                &format!("user{n}"),
                "benchmark-only",
            )
            .await?;
    }
    let target = Uuid::from_u128(1001).to_string();
    let mut tx = store.begin_serial().await?;
    for n in 1..=count {
        sqlx::query("INSERT INTO resources(id,owner_id,kind,label,value) VALUES ($1,$2,'person','Fixture','')")
            .bind(Uuid::from_u128(1000 + n as u128).to_string()).bind(&owner).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE sync_clock SET resource_count=$1 WHERE id=1")
        .bind(count)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let start = Instant::now();
    let mut page = store.sync(&owner, "load", None, 200, 1000).await?;
    let first_page_ms = start.elapsed().as_secs_f64() * 1000.;
    let mut snapshot_pages = 1;
    while page.has_more {
        page = store
            .sync(&owner, "load", Some(&page.next_cursor), 200, 1000)
            .await?;
        snapshot_pages += 1;
    }
    let snapshot_ms = start.elapsed().as_secs_f64() * 1000.;
    let mut writes = Vec::new();
    let mut deltas = Vec::new();
    for n in 1..=100 {
        let start = Instant::now();
        store
            .apply(
                &owner,
                &Uuid::new_v4().to_string(),
                &[Command::Edit {
                    id: target.clone(),
                    expected_version: n,
                    label: format!("Edit {n}"),
                    value: String::new(),
                }],
            )
            .await?;
        writes.push(start.elapsed().as_secs_f64() * 1000.);
        let start = Instant::now();
        page = store
            .sync(&owner, "load", Some(&page.next_cursor), 200, 1000 + n)
            .await?;
        ensure!(
            page.batches.len() == 1 && page.batches[0].changes.len() == 1,
            "unexpected publication scope"
        );
        deltas.push(start.elapsed().as_secs_f64() * 1000.);
    }
    println!(
        "{}",
        serde_json::json!({"backend":"sqlite","accounts":100,"resources":count,"iterations":100,"profile":if cfg!(debug_assertions) {"debug"} else {"release"},"first_snapshot_page_ms":first_page_ms,"snapshot_pages":snapshot_pages,"snapshot_total_ms":snapshot_ms,"write_p50_ms":percentile(&mut writes,0.5),"write_p95_ms":percentile(&mut writes,0.95),"delta_p50_ms":percentile(&mut deltas,0.5),"delta_p95_ms":percentile(&mut deltas,0.95)})
    );
    Ok(())
}
