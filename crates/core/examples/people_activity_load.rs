//! Reproducible People activity read workload (BE-PEOPLE-ACTIVITY plan §9.1, P1): page latency of
//! `person_activity` when the reader sees everything, when the reader sees one field of 99, when
//! the reader sees only the person (every event hidden but its creation: the worst-case scan), and
//! for a person with three merged aliases. Every event is written by the real commands (no raw
//! SQL), so the rows, revisions and visibility are what production writes.
//!
//! The seed: an owner's person with 99 fields, shared with a reader `one_field` (who sees the
//! person and field 0) and a reader `person_only` (who sees only the person); 5,000 events in all
//! (the person, 99 field creations and 4,900 edits, one in every 98 on field 0). A second person
//! has three people merged into it, each with ten fields and ten edits. Uses a disposable SQLite
//! file, or a fresh schema in the disposable PostgreSQL database named by ATLAS_TEST_POSTGRES_URL.
//! `ATLAS_LOAD_ITERATIONS` (default 200) sets the timed reads per case, for the first page and,
//! when there is one, for the page after it (`second_page`, which resolves a cursor).
use anyhow::{Result, ensure};
use atlas_core::{
    Command, Store,
    policy::{Policy, PrincipalGrant},
};
use serde_json::json;
use std::time::Instant;
use uuid::Uuid;

const FIELDS: usize = 99;
const EVENTS: usize = 5_000;

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

fn grants(accounts: &[&str]) -> Policy {
    Policy {
        grants: accounts
            .iter()
            .map(|a| PrincipalGrant::Account {
                id: (*a).into(),
                edit: false,
            })
            .collect(),
        exclude_accounts: vec![],
    }
}

async fn create(s: &Store, owner: &str, commands: Vec<Command>) -> Result<()> {
    for chunk in commands.chunks(20) {
        s.apply(owner, &id(), chunk).await?;
    }
    Ok(())
}

async fn edit(s: &Store, owner: &str, field: &str, n: usize) -> Result<()> {
    let (version, policy_version): (i64, i64) =
        sqlx::query_as("SELECT version,policy_version FROM resources WHERE id=$1")
            .bind(field)
            .fetch_one(&s.pool)
            .await?;
    s.apply(
        owner,
        &id(),
        &[Command::Edit {
            id: field.into(),
            expected_version: version,
            expected_policy_version: Some(policy_version),
            label: format!("Field {n}"),
            value: format!("value {n}"),
        }],
    )
    .await?;
    Ok(())
}

async fn timed(
    s: &Store,
    reader: &str,
    person: &str,
    iterations: usize,
) -> Result<(serde_json::Value, usize)> {
    let mut samples = Vec::with_capacity(iterations);
    let mut items = 0;
    let mut cursor = None;
    for _ in 0..iterations {
        let started = Instant::now();
        let page = s.person_activity(reader, person, None, 50).await?;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
        items = page.items.len();
        cursor = page.next_after;
    }
    // The page after the first one: the cursor is resolved and its own event's visibility checked
    // before the scan continues (BE-PEOPLE-ACTIVITY R1).
    let mut cursor_samples = Vec::with_capacity(iterations);
    if let Some(after) = &cursor {
        for _ in 0..iterations {
            let started = Instant::now();
            s.person_activity(reader, person, Some(after), 50).await?;
            cursor_samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
    }
    let cursor_page = if cursor_samples.is_empty() {
        serde_json::Value::Null
    } else {
        json!({
            "p50_ms": round(percentile(&mut cursor_samples, 0.5)),
            "p95_ms": round(percentile(&mut cursor_samples, 0.95)),
            "max_ms": round(percentile(&mut cursor_samples, 1.0)),
        })
    };
    // A complete walk at the default size, for the total cost of reading everything.
    let started = Instant::now();
    let (mut after, mut pages, mut walked) = (None::<String>, 0, 0);
    loop {
        let page = s
            .person_activity(reader, person, after.as_deref(), 50)
            .await?;
        pages += 1;
        walked += page.items.len();
        match page.next_after {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    let walk_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok((
        json!({
            "first_page_items": items,
            "first_page_p50_ms": round(percentile(&mut samples, 0.5)),
            "first_page_p95_ms": round(percentile(&mut samples, 0.95)),
            "first_page_max_ms": round(percentile(&mut samples, 1.0)),
            "walk_pages": pages,
            "walk_items": walked,
            "walk_ms": round(walk_ms),
            "second_page": cursor_page,
        }),
        walked,
    ))
}

#[tokio::main]
async fn main() -> Result<()> {
    let iterations: usize = std::env::var("ATLAS_LOAD_ITERATIONS")
        .ok()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(200);
    let (s, engine, _dir) = connect().await?;
    s.migrate().await?;
    let (owner, one_field, person_only) = (id(), id(), id());
    for (account, name) in [
        (&owner, "load-owner"),
        (&one_field, "load-one-field"),
        (&person_only, "load-person-only"),
    ] {
        s.add_account(account, name, "load").await?;
    }

    let seeding = Instant::now();
    let person = id();
    let fields: Vec<String> = (0..FIELDS).map(|_| id()).collect();
    let mut commands = vec![Command::CreatePerson {
        id: person.clone(),
        name: "Load person".into(),
        initial_policy: Some(grants(&[&one_field, &person_only])),
    }];
    for (n, field) in fields.iter().enumerate() {
        commands.push(Command::CreateField {
            id: field.clone(),
            person_id: person.clone(),
            label: format!("Field {n}"),
            value: "initial".into(),
            initial_policy: Some(if n == 0 {
                grants(&[&one_field])
            } else {
                Policy::default()
            }),
        });
    }
    create(&s, &owner, commands).await?;
    for n in 0..EVENTS - 1 - FIELDS {
        let field = if n % 98 == 0 {
            &fields[0]
        } else {
            &fields[1 + n % 98]
        };
        edit(&s, &owner, field, n).await?;
    }

    // Three people merged into a fourth, each with ten fields and ten edits.
    let target = id();
    create(
        &s,
        &owner,
        vec![Command::CreatePerson {
            id: target.clone(),
            name: "Merged".into(),
            initial_policy: Some(Policy::default()),
        }],
    )
    .await?;
    for _ in 0..3 {
        let source = id();
        let source_fields: Vec<String> = (0..10).map(|_| id()).collect();
        let mut commands = vec![Command::CreatePerson {
            id: source.clone(),
            name: "Alias".into(),
            initial_policy: Some(Policy::default()),
        }];
        commands.extend(source_fields.iter().map(|f| Command::CreateField {
            id: f.clone(),
            person_id: source.clone(),
            label: "Alias field".into(),
            value: "v".into(),
            initial_policy: Some(Policy::default()),
        }));
        create(&s, &owner, commands).await?;
        for (n, field) in source_fields.iter().enumerate() {
            edit(&s, &owner, field, n).await?;
        }
        let preview = s.merge_preview(&owner, &source, &target).await?;
        s.people_command(
            &owner,
            &id(),
            &atlas_core::people::PeopleCommand::Merge {
                source_id: source,
                target_id: target.clone(),
                preview_token: preview.token,
                name: "Merged".into(),
            },
            0,
        )
        .await?;
    }
    let seed_ms = seeding.elapsed().as_secs_f64() * 1000.0;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM person_activity_events")
        .fetch_one(&s.pool)
        .await?;

    let (owner_case, owner_walked) = timed(&s, &owner, &person, iterations).await?;
    ensure!(owner_walked == EVENTS, "the owner sees all {EVENTS} events");
    let (one_case, one_walked) = timed(&s, &one_field, &person, iterations).await?;
    ensure!(
        one_walked == 2 + (EVENTS - 1 - FIELDS).div_ceil(98),
        "the one-field reader sees the person, field 0 and its edits"
    );
    let (person_case, person_walked) = timed(&s, &person_only, &person, iterations).await?;
    ensure!(
        person_walked == 1,
        "the person-only reader sees only the creation"
    );
    let (merged_case, merged_walked) = timed(&s, &owner, &target, iterations).await?;
    ensure!(
        merged_walked == 1 + 3 * (1 + 10 + 10 + 1),
        "the merged timeline"
    );
    println!(
        "{}",
        json!({
            "engine": engine,
            "iterations": iterations,
            "events_total": total,
            "seed_ms": round(seed_ms),
            "owner_sees_all": owner_case,
            "reader_sees_one_field_of_99": one_case,
            "reader_sees_person_only": person_case,
            "merged_with_three_aliases": merged_case,
        })
    );
    Ok(())
}
