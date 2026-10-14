//! Reproducible People summary workload (BE-PEOPLE-SUMMARY): page latency of
//! `people_summaries` for each filter at 50 and 100 rows, a full traversal of each filter, the
//! number of statements one page issues (PostgreSQL with `pg_stat_statements` loaded; otherwise
//! reported as unavailable) and, on PostgreSQL, `EXPLAIN ANALYZE` of each filter's id query. The
//! `all` filter adds no predicate, so its plan is the visibility scan `listPeople` already runs and
//! is the baseline the other filters are compared with.
//!
//! The instance holds at most 10,000 resources and 100 accounts, so the seed stays inside both:
//! 1,600 people with five fields each (9,600 resources), 40 accounts in three households, 38
//! people linked to accounts, every kind of grant, exclusions and half the people without an
//! attribution row. Rows are written by raw SQL in one transaction; the reads under test are the
//! real ones. Uses a disposable SQLite file, or a fresh schema in the disposable PostgreSQL
//! database named by ATLAS_TEST_POSTGRES_URL; it never touches an existing schema.
//! `ATLAS_LOAD_ITERATIONS` (default 30) sets the timed first-page reads per filter and size.
use anyhow::{Result, ensure};
use atlas_core::{
    Store,
    people::{PeopleScope, summary_id_query},
};
use serde_json::json;
use std::time::Instant;
use uuid::Uuid;

const PEOPLE: usize = 1_600;
const FIELDS: usize = 5;
const OTHERS: usize = 38;
const SCOPES: [(PeopleScope, &str); 4] = [
    (PeopleScope::All, "all"),
    (PeopleScope::InHome, "in_home"),
    (PeopleScope::SharedWithMe, "shared_with_me"),
    (PeopleScope::NotLinked, "not_linked"),
];

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

struct Seeded {
    viewer: String,
}

/// The viewer V and owner A share Home (V's primary household, with 20 others) and Second; A
/// shares Third with the other 18. Person `i` by `i % 10`: 0–3 A's, shared with Home; 4–5 A's,
/// Second; 6 A's, directly with V; 7 A's, private; 8 A's, Home but excluding V; 9 V's own, Home.
/// The first 38 Home people are linked to (and owned by) the other accounts. Even people are
/// attributed to their creator; odd ones have no attribution (unknown).
async fn seed(store: &Store) -> Result<Seeded> {
    let viewer = id();
    let owner = id();
    store.add_account(&viewer, "load-viewer", "load").await?;
    store.add_account(&owner, "load-owner", "load").await?;
    let mut others = Vec::new();
    for n in 0..OTHERS {
        let other = id();
        store
            .add_account(&other, &format!("load-other-{n}"), "load")
            .await?;
        others.push(other);
    }
    let (home, second, third) = (id(), id(), id());
    let mut tx = store.begin_serial().await?;
    for (household, name) in [(&home, "Home"), (&second, "Second"), (&third, "Third")] {
        sqlx::query("INSERT INTO households(id,name) VALUES ($1,$2)")
            .bind(household)
            .bind(name)
            .execute(&mut *tx)
            .await?;
    }
    let mut memberships = vec![
        (&home, &viewer),
        (&home, &owner),
        (&second, &viewer),
        (&second, &owner),
        (&third, &owner),
    ];
    memberships.extend(others[..20].iter().map(|o| (&home, o)));
    memberships.extend(others[20..].iter().map(|o| (&third, o)));
    for (household, account) in memberships {
        sqlx::query("INSERT INTO household_memberships VALUES ($1,$2,'member')")
            .bind(household)
            .bind(account)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE accounts SET primary_household_id=$1 WHERE id=$2")
        .bind(&home)
        .bind(&viewer)
        .execute(&mut *tx)
        .await?;
    let mut linked = 0;
    for i in 0..PEOPLE {
        let person = id();
        let category = i % 10;
        let mut person_owner = if category == 9 { &viewer } else { &owner };
        let link = category < 4 && linked < OTHERS;
        if link {
            person_owner = &others[linked];
        }
        sqlx::query(
            "INSERT INTO resources(id,owner_id,kind,label,value) VALUES ($1,$2,'person',$3,'')",
        )
        .bind(&person)
        .bind(person_owner)
        .bind(format!("Person {i}"))
        .execute(&mut *tx)
        .await?;
        if link {
            sqlx::query("INSERT INTO person_accounts VALUES ($1,$2)")
                .bind(&person)
                .bind(&others[linked])
                .execute(&mut *tx)
                .await?;
            // The creator keeps a direct grant, as link acceptance leaves it.
            sqlx::query("INSERT INTO resource_grants VALUES ($1,$2,1)")
                .bind(&person)
                .bind(&owner)
                .execute(&mut *tx)
                .await?;
            linked += 1;
        }
        let household = match category {
            0..=3 | 8 | 9 => Some(&home),
            4 | 5 => Some(&second),
            _ => None,
        };
        if let Some(household) = household {
            sqlx::query("INSERT INTO resource_household_grants VALUES ($1,$2,$3)")
                .bind(&person)
                .bind(household)
                .bind(i64::from(i % 2 == 0))
                .execute(&mut *tx)
                .await?;
        }
        if category == 6 {
            sqlx::query("INSERT INTO resource_grants VALUES ($1,$2,0)")
                .bind(&person)
                .bind(&viewer)
                .execute(&mut *tx)
                .await?;
        }
        if category == 8 {
            sqlx::query("INSERT INTO resource_exclusions VALUES ($1,$2)")
                .bind(&person)
                .bind(&viewer)
                .execute(&mut *tx)
                .await?;
        }
        if i % 2 == 0 {
            let creator = if category == 9 { &viewer } else { &owner };
            sqlx::query("INSERT INTO person_attributions(person_id,added_by) VALUES ($1,$2)")
                .bind(&person)
                .bind(creator)
                .execute(&mut *tx)
                .await?;
        }
        for f in 0..FIELDS {
            let field = id();
            sqlx::query("INSERT INTO resources(id,owner_id,parent_id,kind,label,value) VALUES ($1,$2,$3,'field','Note',$4)")
                .bind(&field)
                .bind(person_owner)
                .bind(&person)
                .bind(r#"{"kind":"text","text":"x"}"#)
                .execute(&mut *tx)
                .await?;
            match f {
                0 | 4 => {
                    sqlx::query("INSERT INTO resource_household_grants VALUES ($1,$2,0)")
                        .bind(&field)
                        .bind(&home)
                        .execute(&mut *tx)
                        .await?;
                    if f == 4 {
                        sqlx::query("INSERT INTO resource_exclusions VALUES ($1,$2)")
                            .bind(&field)
                            .bind(&viewer)
                            .execute(&mut *tx)
                            .await?;
                    }
                }
                2 => {
                    sqlx::query("INSERT INTO resource_grants VALUES ($1,$2,0)")
                        .bind(&field)
                        .bind(&viewer)
                        .execute(&mut *tx)
                        .await?;
                }
                3 => {
                    sqlx::query("INSERT INTO resource_household_grants VALUES ($1,$2,0)")
                        .bind(&field)
                        .bind(&second)
                        .execute(&mut *tx)
                        .await?;
                }
                _ => {}
            }
        }
    }
    sqlx::query("UPDATE sync_clock SET resource_count=$1 WHERE id=1")
        .bind((PEOPLE * (FIELDS + 1)) as i64)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Seeded { viewer })
}

/// Statements issued by one first page, from `pg_stat_statements` (reset first). `None` when the
/// extension is not loaded.
async fn statements(
    store: &Store,
    viewer: &str,
    scope: PeopleScope,
    limit: u16,
) -> Result<Option<i64>> {
    if sqlx::query("SELECT count(*) FROM (SELECT public.pg_stat_statements_reset()) r")
        .execute(&store.pool)
        .await
        .is_err()
    {
        return Ok(None);
    }
    store
        .people_summaries(viewer, scope, false, None, limit)
        .await?;
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
    let started = Instant::now();
    let seeded = seed(&store).await?;
    let seed_seconds = started.elapsed().as_secs_f64();
    let postgres = engine == "postgres";
    let counting = postgres
        && sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_stat_statements SCHEMA public")
            .execute(&store.pool)
            .await
            .is_ok()
        && sqlx::query("SELECT count(*) FROM public.pg_stat_statements")
            .execute(&store.pool)
            .await
            .is_ok();
    println!(
        "{}",
        json!({"engine": engine, "people": PEOPLE, "fields_per_person": FIELDS, "accounts": OTHERS + 2,
               "seed_seconds": round(seed_seconds), "iterations": iterations, "statement_counter": counting})
    );
    let v = seeded.viewer.as_str();
    for (scope, name) in SCOPES {
        for limit in [50_u16, 100] {
            // One full traversal of the filter.
            let start = Instant::now();
            let (mut pages, mut rows, mut after) = (0, 0, None::<String>);
            loop {
                let page = store
                    .people_summaries(v, scope, false, after.as_deref(), limit)
                    .await?;
                pages += 1;
                rows += page.items.len();
                match page.next_after {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
            let traversal_ms = start.elapsed().as_secs_f64() * 1000.0;
            // Timed first pages.
            let mut samples = Vec::with_capacity(iterations);
            for _ in 0..iterations {
                let start = Instant::now();
                let page = store.people_summaries(v, scope, false, None, limit).await?;
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
                ensure!(!page.items.is_empty(), "{name} is empty");
            }
            let count = if counting {
                statements(&store, v, scope, limit).await?
            } else {
                None
            };
            let small = if counting {
                statements(&store, v, scope, 1).await?
            } else {
                None
            };
            if let (Some(full), Some(one)) = (count, small) {
                // No statement per row: a full page issues no more than a one-row page plus the
                // owner-only statements a mixed page can add.
                ensure!(
                    full <= one + 4,
                    "{name}: {full} statements at {limit}, {one} at 1"
                );
            }
            println!(
                "{}",
                json!({"engine": engine, "scope": name, "limit": limit, "rows": rows, "pages": pages,
                       "traversal_ms": round(traversal_ms),
                       "first_page_p50_ms": round(percentile(&mut samples, 0.5)),
                       "first_page_p95_ms": round(percentile(&mut samples, 0.95)),
                       "statements_per_page": count, "statements_one_row_page": small})
            );
        }
    }
    if postgres {
        for (scope, name) in SCOPES {
            let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY ON) {}",
                summary_id_query(scope)
            )))
            .bind(v)
            .bind(0_i64)
            .bind(None::<String>)
            .bind(100_i64)
            .fetch_all(&store.pool)
            .await?;
            let sequential: Vec<&String> = plan
                .iter()
                .filter(|line| line.contains("Seq Scan"))
                .collect();
            println!(
                "{}",
                json!({"engine": engine, "scope": name, "plan": plan, "seq_scans": sequential})
            );
        }
    }
    Ok(())
}
