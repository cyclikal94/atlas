//! People activity write overhead (BE-PEOPLE-ACTIVITY plan §9.1, P2): the latency of 1,000
//! person-field edits through `Store::apply`, the path that now also records a `field_edited`
//! event. It uses only APIs that exist before schema `1007`, so the same file builds at the base
//! commit as the control (no event recorded) and on the candidate (one event per edit); compare
//! the medians. Uses a disposable SQLite file, or a fresh schema in the disposable PostgreSQL
//! database named by ATLAS_TEST_POSTGRES_URL. `ATLAS_LOAD_WRITES` (default 1,000) sets the count.
use anyhow::Result;
use atlas_core::{Command, Store, policy::Policy};
use serde_json::json;
use std::time::Instant;
use uuid::Uuid;

fn percentile(samples: &mut [f64], quantile: f64) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[((samples.len() as f64 * quantile).ceil() as usize).saturating_sub(1)]
}

fn round(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
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

#[tokio::main]
async fn main() -> Result<()> {
    let writes: usize = std::env::var("ATLAS_LOAD_WRITES")
        .ok()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(1_000);
    let (s, engine, _dir) = connect().await?;
    s.migrate().await?;
    let owner = id();
    s.add_account(&owner, "load-owner", "load").await?;
    let person = id();
    let fields: Vec<String> = (0..10).map(|_| id()).collect();
    let mut commands = vec![Command::CreatePerson {
        id: person.clone(),
        name: "Load person".into(),
        initial_policy: Some(Policy::default()),
    }];
    commands.extend(fields.iter().map(|f| Command::CreateField {
        id: f.clone(),
        person_id: person.clone(),
        label: "Field".into(),
        value: "initial".into(),
        initial_policy: Some(Policy::default()),
    }));
    s.apply(&owner, &id(), &commands).await?;
    let mut samples = Vec::with_capacity(writes);
    for n in 0..writes {
        let field = &fields[n % fields.len()];
        let (version, policy_version): (i64, i64) =
            sqlx::query_as("SELECT version,policy_version FROM resources WHERE id=$1")
                .bind(field)
                .fetch_one(&s.pool)
                .await?;
        let command = Command::Edit {
            id: field.clone(),
            expected_version: version,
            expected_policy_version: Some(policy_version),
            label: "Field".into(),
            value: format!("value {n}"),
        };
        let started = Instant::now();
        s.apply(&owner, &id(), std::slice::from_ref(&command))
            .await?;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let events: Option<i64> = sqlx::query_scalar("SELECT COUNT(*) FROM person_activity_events")
        .fetch_one(&s.pool)
        .await
        .ok();
    println!(
        "{}",
        json!({
            "engine": engine,
            "writes": writes,
            "events_recorded": events,
            "edit_mean_ms": round(mean),
            "edit_p50_ms": round(percentile(&mut samples, 0.5)),
            "edit_p95_ms": round(percentile(&mut samples, 0.95)),
        })
    );
    Ok(())
}
