//! N1: the additive `1000 -> 1004` upgrade chain and the reset error for anything it cannot walk.
//!
//! Every legacy baseline is rooted on a *frozen*, genuine `1003` schema captured from the
//! `1003` release (`tests/fixtures/schema_1003_*.sql`), never on the current schema. Deriving a
//! legacy database from the current schema by dropping objects would silently carry whatever the
//! current step creates (here the `1004` timer indexes) and could not prove the step's `DROP`.
use std::collections::BTreeMap;

use anyhow::Result;
use atlas_core::Store;

use crate::support::devices::{FAR, add_session, id};

const FROZEN_1003_SQLITE: &str = include_str!("../../fixtures/schema_1003_sqlite.sql");
const FROZEN_1003_POSTGRES: &str = include_str!("../../fixtures/schema_1003_postgres.sql");

async fn version(store: &Store) -> Result<Vec<i64>> {
    Ok(
        sqlx::query_scalar("SELECT version FROM atlas_schema ORDER BY version")
            .fetch_all(&store.pool)
            .await?,
    )
}

async fn table_exists(store: &Store, name: &str) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(if crate::support::database::postgres() {
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=current_schema() AND table_name=$1"
    } else {
        "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name=$1"
    })
    .bind(name)
    .fetch_one(&store.pool)
    .await?;
    Ok(count == 1)
}

/// A database exactly as the `1003` release left it: the frozen baseline, executed verbatim.
async fn baseline_1003(url: &str) -> Result<Store> {
    let store = Store::connect(url).await?;
    sqlx::raw_sql(if crate::support::database::postgres() {
        FROZEN_1003_POSTGRES
    } else {
        FROZEN_1003_SQLITE
    })
    .execute(&store.pool)
    .await?;
    assert_eq!(version(&store).await?, [1003]);
    Ok(store)
}

/// A database exactly as the `1002` release left it (BE-Q19's activation grants and OIDC
/// attempt columns present, BE-Q16's durable sent-request/invitation history absent),
/// recorded version 1002.
async fn baseline_1002(url: &str) -> Result<Store> {
    let store = baseline_1003(url).await?;
    sqlx::query("DROP TABLE people_request_history")
        .execute(&store.pool)
        .await?;
    sqlx::query("DROP TABLE household_invitation_history")
        .execute(&store.pool)
        .await?;
    sqlx::query("UPDATE atlas_schema SET version=1002")
        .execute(&store.pool)
        .await?;
    Ok(store)
}

/// A database exactly as the `1001` release left it (BE-Q11-B7's ledger present, BE-Q19's
/// activation grants and OIDC attempt columns absent), recorded version 1001.
async fn baseline_1001(url: &str) -> Result<Store> {
    let store = baseline_1002(url).await?;
    sqlx::query("DROP TABLE activation_grants")
        .execute(&store.pool)
        .await?;
    sqlx::query("ALTER TABLE oidc_flows DROP COLUMN attempt_id")
        .execute(&store.pool)
        .await?;
    sqlx::query("ALTER TABLE oidc_flows DROP COLUMN attempt_challenge")
        .execute(&store.pool)
        .await?;
    sqlx::query("UPDATE atlas_schema SET version=1001")
        .execute(&store.pool)
        .await?;
    Ok(store)
}

/// A database exactly as the previous release left it: no ledger table, no activation grants, no
/// OIDC attempt columns, recorded version 1000.
async fn baseline_1000(url: &str) -> Result<Store> {
    let store = baseline_1001(url).await?;
    sqlx::query("DROP TABLE operation_outcomes")
        .execute(&store.pool)
        .await?;
    sqlx::query("UPDATE atlas_schema SET version=1000")
        .execute(&store.pool)
        .await?;
    Ok(store)
}

/// Column names, types, nullability and constraint names of `table`.
async fn shape(store: &Store, table: &str) -> Result<Vec<String>> {
    let mut lines: Vec<String> = if crate::support::database::postgres() {
        let mut columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name||' '||data_type||' '||is_nullable FROM information_schema.columns WHERE table_schema=current_schema() AND table_name=$1",
        )
        .bind(table)
        .fetch_all(&store.pool)
        .await?;
        columns.extend(
            sqlx::query_scalar::<_, String>(
                "SELECT conname||' '||contype::text||' '||pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid=$1::regclass",
            )
            .bind(table)
            .fetch_all(&store.pool)
            .await?,
        );
        columns
    } else {
        let mut columns: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT name||' '||type||' '||\"notnull\"||' '||pk FROM pragma_table_info('{table}')"
        )))
        .fetch_all(&store.pool)
        .await?;
        columns.extend(
            sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(format!(
                "SELECT \"table\"||'.'||\"from\"||'->'||\"to\" FROM pragma_foreign_key_list('{table}')"
            )))
            .fetch_all(&store.pool)
            .await?,
        );
        columns
    };
    lines.sort();
    Ok(lines)
}

/// Every explicitly created index as `name: definition`, optionally for one table. The
/// definition is the engine's own rendering (`sqlite_schema.sql` / `pg_indexes.indexdef`), so an
/// upgrade and a fresh baseline agree only if they create the same index.
async fn indexes(store: &Store, table: Option<&str>) -> Result<Vec<String>> {
    let mut lines: Vec<String> = if crate::support::database::postgres() {
        sqlx::query_scalar(
            "SELECT indexname||': '||replace(indexdef, current_schema()||'.', '') FROM pg_indexes WHERE schemaname=current_schema() AND ($1 IS NULL OR tablename=$1)",
        )
        .bind(table)
        .fetch_all(&store.pool)
        .await?
    } else {
        sqlx::query_scalar(
            "SELECT name||': '||sql FROM sqlite_schema WHERE type='index' AND sql IS NOT NULL AND ($1 IS NULL OR tbl_name=$1)",
        )
        .bind(table)
        .fetch_all(&store.pool)
        .await?
    };
    lines.sort();
    Ok(lines)
}

async fn index_names(store: &Store, table: &str) -> Result<Vec<String>> {
    Ok(indexes(store, Some(table))
        .await?
        .into_iter()
        .map(|line| line.split(':').next().unwrap_or_default().to_string())
        .collect())
}

/// Every table's rows as one canonical string, keyed by table name (`atlas_schema` excluded: its
/// version is what an upgrade legitimately changes). Two databases, or one before and after an
/// upgrade, hold identical rows exactly when the maps are equal.
async fn row_digests(store: &Store) -> Result<BTreeMap<String, String>> {
    let tables: Vec<String> = if crate::support::database::postgres() {
        sqlx::query_scalar(
            "SELECT table_name::text FROM information_schema.tables WHERE table_schema=current_schema() AND table_type='BASE TABLE' AND table_name<>'atlas_schema' ORDER BY 1",
        )
        .fetch_all(&store.pool)
        .await?
    } else {
        sqlx::query_scalar(
            "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name<>'atlas_schema' ORDER BY 1",
        )
        .fetch_all(&store.pool)
        .await?
    };
    let mut digests = BTreeMap::new();
    for table in tables {
        let rows: String = if crate::support::database::postgres() {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COALESCE(string_agg(t::text, E'\\n' ORDER BY t::text),'') FROM \"{table}\" t"
            )))
            .fetch_one(&store.pool)
            .await?
        } else {
            let columns: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT name FROM pragma_table_info('{table}') ORDER BY cid"
            )))
            .fetch_all(&store.pool)
            .await?;
            let row = columns
                .iter()
                .map(|c| format!("quote(\"{c}\")"))
                .collect::<Vec<_>>()
                .join("||'|'||");
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COALESCE(group_concat(r, char(10)),'') FROM (SELECT {row} AS r FROM \"{table}\" ORDER BY r)"
            )))
            .fetch_one(&store.pool)
            .await?
        };
        digests.insert(table, rows);
    }
    Ok(digests)
}

/// The rows a timer-bearing database holds, written by raw SQL so the seed does not depend on
/// current-schema code paths. `history` is only valid on a `1003` database (the tables exist).
struct Seed {
    account: String,
    other: String,
    busy: String,
    idle: String,
    elsewhere: String,
}

async fn seed(store: &Store, history: bool) -> Result<Seed> {
    let seed = Seed {
        account: id(),
        other: id(),
        busy: id(),
        idle: id(),
        elsewhere: id(),
    };
    for account in [&seed.account, &seed.other] {
        sqlx::query("INSERT INTO accounts(id,username,password_hash) VALUES ($1,$2,'x')")
            .bind(account)
            .bind(format!("u{}", account.replace('-', "")))
            .execute(&store.pool)
            .await?;
    }
    for (resource, owner) in [
        (&seed.busy, &seed.account),
        (&seed.idle, &seed.account),
        (&seed.elsewhere, &seed.other),
    ] {
        sqlx::query("INSERT INTO resources(id,owner_id,parent_id,kind,label,value) VALUES ($1,$2,NULL,'person','stream','{}')")
            .bind(resource)
            .bind(owner)
            .execute(&store.pool)
            .await?;
    }
    // Stopped, cancelled, stopped elsewhere, and the one running session the old index allowed
    // per account; another account's own running session.
    for (session, progress, account, started, stopped, version, cancelled) in [
        (
            id(),
            &seed.busy,
            &seed.account,
            100_i64,
            Some(200_i64),
            2_i64,
            0_i64,
        ),
        (id(), &seed.busy, &seed.account, 300, None, 2, 1),
        (id(), &seed.idle, &seed.account, 400, Some(500), 2, 0),
        (id(), &seed.busy, &seed.account, 1_000, None, 1, 0),
        (id(), &seed.elsewhere, &seed.other, 1_000, None, 1, 0),
    ] {
        sqlx::query("INSERT INTO timer_sessions(id,progress_id,account_id,started_at,stopped_at,version,cancelled) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(session)
            .bind(progress)
            .bind(account)
            .bind(started)
            .bind(stopped)
            .bind(version)
            .bind(cancelled)
            .execute(&store.pool)
            .await?;
    }
    sqlx::query(
        "INSERT INTO receipts(account_id,operation_id,payload,revision) VALUES ($1,$2,'{}',1)",
    )
    .bind(&seed.account)
    .bind(id())
    .execute(&store.pool)
    .await?;
    if table_exists(store, "operation_outcomes").await? {
        sqlx::query("INSERT INTO operation_outcomes(account_id,operation_id,kind,digest,outcome,created_at) VALUES ($1,$2,'revoke_session','d','confirmed_applied',1)")
            .bind(&seed.account)
            .bind(id())
            .execute(&store.pool)
            .await?;
    }
    if history {
        sqlx::query("INSERT INTO people_request_history(id,sender_id,recipient_id,kind,payload,state,expires_at,updated_at) VALUES ($1,$2,$3,'link','{}','accepted',4102444800,1)")
            .bind(id())
            .bind(&seed.account)
            .bind(&seed.other)
            .execute(&store.pool)
            .await?;
        let household = id();
        sqlx::query("INSERT INTO households(id,name) VALUES ($1,'Home')")
            .bind(&household)
            .execute(&store.pool)
            .await?;
        sqlx::query("INSERT INTO household_invitation_history(id,household_id,sender_id,recipient_id,status,expires_at,version,updated_at) VALUES ($1,$2,$3,$4,'revoked',4102444800,2,1)")
            .bind(id())
            .bind(&household)
            .bind(&seed.account)
            .bind(&seed.other)
            .execute(&store.pool)
            .await?;
    }
    Ok(seed)
}

async fn insert_running(
    store: &Store,
    account: &str,
    progress: &str,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO timer_sessions(id,progress_id,account_id,started_at,version) VALUES ($1,$2,$3,5000,1)")
        .bind(id())
        .bind(progress)
        .bind(account)
        .execute(&store.pool)
        .await
        .map(|_| ())
}

fn unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|e| e.is_unique_violation())
}

/// What `1004` changes, and only that: the account-wide unique index is gone, the per-occurrence
/// one and the history index exist with exactly a fresh database's definitions, and the new
/// index really replaced the old one (a second running timer on another occurrence is accepted,
/// a second on the same occurrence is not).
async fn assert_timer_indexes_replaced(store: &Store, seed: &Seed) -> Result<()> {
    let names = index_names(store, "timer_sessions").await?;
    assert!(
        !names.contains(&"timer_active_account".to_string()),
        "{names:?}"
    );
    for wanted in [
        "timer_active_account_occurrence",
        "timer_account_stopped",
        "timer_account_times",
        "timer_progress",
    ] {
        assert!(names.contains(&wanted.to_string()), "{wanted} in {names:?}");
    }
    let (_dir, url) = crate::support::database::database_url().await?;
    let fresh = Store::connect(&url).await?;
    fresh.migrate().await?;
    assert_eq!(
        indexes(store, Some("timer_sessions")).await?,
        indexes(&fresh, Some("timer_sessions")).await?,
        "upgrade and baseline agree on the timer indexes"
    );
    // `busy` holds the seeded running session. A running timer on another occurrence of the
    // same account is now allowed; a second one on `busy` is not.
    insert_running(store, &seed.account, &seed.idle).await?;
    let same = insert_running(store, &seed.account, &seed.busy)
        .await
        .expect_err("one running timer per occurrence");
    assert!(unique_violation(&same), "{same}");
    let elsewhere = insert_running(store, &seed.other, &seed.elsewhere)
        .await
        .expect_err("the other account already runs a timer on its own occurrence");
    assert!(unique_violation(&elsewhere), "{elsewhere}");
    Ok(())
}

async fn timer_rows(store: &Store, account: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM timer_sessions WHERE account_id=$1")
            .bind(account)
            .fetch_one(&store.pool)
            .await?,
    )
}

#[tokio::test]
async fn the_frozen_1003_baseline_is_the_account_wide_schema() -> Result<()> {
    // The negative control for every test below: on the real 1003 schema a second running
    // timer for one account is refused, so passing tests prove the 1004 step changed that.
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = baseline_1003(&url).await?;
    let seed = seed(&store, true).await?;
    let names = index_names(&store, "timer_sessions").await?;
    assert!(
        names.contains(&"timer_active_account".to_string()),
        "{names:?}"
    );
    assert!(!names.contains(&"timer_active_account_occurrence".to_string()));
    assert!(!names.contains(&"timer_account_stopped".to_string()));
    let refused = insert_running(&store, &seed.account, &seed.idle)
        .await
        .expect_err("1003 allows one running timer per account");
    assert!(unique_violation(&refused), "{refused}");
    Ok(())
}

#[tokio::test]
async fn a_1003_database_upgrades_to_1004_in_place() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = baseline_1003(&url).await?;
    let seed = seed(&store, true).await?;
    let before = row_digests(&store).await?;
    for table in [
        "timer_sessions",
        "people_request_history",
        "household_invitation_history",
        "operation_outcomes",
        "receipts",
        "resources",
        "accounts",
    ] {
        assert!(
            !before[table].is_empty(),
            "{table} is seeded, so equality is not vacuous"
        );
    }
    assert_eq!(timer_rows(&store, &seed.account).await?, 4);

    store.migrate().await?;
    assert_eq!(version(&store).await?, [1004]);
    assert_eq!(
        row_digests(&store).await?,
        before,
        "the step rewrites no row of any table"
    );

    // A second migrate is a no-op, and so is a restart through a new connection.
    store.migrate().await?;
    let restarted = Store::connect(&url).await?;
    restarted.migrate().await?;
    assert_eq!(version(&restarted).await?, [1004]);
    assert_eq!(row_digests(&restarted).await?, before);

    // Whole-database index parity with a fresh baseline: nothing else drifted.
    let (_fresh_dir, fresh_url) = crate::support::database::database_url().await?;
    let fresh = Store::connect(&fresh_url).await?;
    fresh.migrate().await?;
    assert_eq!(version(&fresh).await?, [1004]);
    assert_eq!(indexes(&store, None).await?, indexes(&fresh, None).await?);
    for table in [
        "timer_sessions",
        "people_request_history",
        "household_invitation_history",
    ] {
        assert_eq!(shape(&fresh, table).await?, shape(&store, table).await?);
    }

    assert_timer_indexes_replaced(&store, &seed).await?;
    Ok(())
}

#[tokio::test]
async fn a_fresh_database_records_1004_and_restarts() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let fresh = Store::connect(&url).await?;
    fresh.migrate().await?;
    assert_eq!(version(&fresh).await?, [1004]);
    // Restarted twice: the fresh-database-bricked-on-restart regression must not reproduce.
    for _ in 0..2 {
        let restarted = Store::connect(&url).await?;
        restarted.migrate().await?;
        assert_eq!(version(&restarted).await?, [1004]);
    }
    let names = index_names(&fresh, "timer_sessions").await?;
    assert!(names.contains(&"timer_active_account_occurrence".to_string()));
    assert!(!names.contains(&"timer_active_account".to_string()));
    Ok(())
}

#[tokio::test]
async fn a_1000_database_upgrades_in_place() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = baseline_1000(&url).await?;
    assert!(!table_exists(&store, "operation_outcomes").await?);
    assert!(!table_exists(&store, "activation_grants").await?);
    let seed = seed(&store, false).await?;
    let session = add_session(&store, &seed.account, "phone", FAR).await?;
    let before = row_digests(&store).await?;

    store.migrate().await?;
    assert_eq!(version(&store).await?, [1004]);
    assert!(table_exists(&store, "operation_outcomes").await?);
    assert!(table_exists(&store, "activation_grants").await?);
    assert!(table_exists(&store, "people_request_history").await?);
    assert!(table_exists(&store, "household_invitation_history").await?);
    // Existing rows are untouched.
    let after = row_digests(&store).await?;
    for (table, rows) in &before {
        assert_eq!(&after[table], rows, "{table} rows are preserved");
    }
    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE session_id=$1")
        .bind(&session)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(kept, 1);
    assert_eq!(timer_rows(&store, &seed.account).await?, 4);

    // Idempotent restart, and the upgraded database is usable for the new feature.
    store.migrate().await?;
    assert_eq!(version(&store).await?, [1004]);
    let outcome = store
        .revoke_session(&seed.account, &session, &id(), 1000)
        .await?;
    assert_eq!(outcome.outcome.as_str(), "confirmed_applied");
    assert_timer_indexes_replaced(&store, &seed).await?;

    // A fresh database records the same version and has the same tables.
    let (_fresh_dir, fresh_url) = crate::support::database::database_url().await?;
    let fresh = Store::connect(&fresh_url).await?;
    fresh.migrate().await?;
    assert_eq!(version(&fresh).await?, [1004]);
    for table in [
        "operation_outcomes",
        "activation_grants",
        "oidc_flows",
        "people_request_history",
        "household_invitation_history",
        "timer_sessions",
    ] {
        assert_eq!(
            shape(&fresh, table).await?,
            shape(&store, table).await?,
            "upgrade and baseline agree on {table}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_1001_database_upgrades_in_place() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = baseline_1001(&url).await?;
    assert!(table_exists(&store, "operation_outcomes").await?);
    assert!(!table_exists(&store, "activation_grants").await?);
    let seed = seed(&store, false).await?;
    let session = add_session(&store, &seed.account, "phone", FAR).await?;
    let before = row_digests(&store).await?;

    store.migrate().await?;
    assert_eq!(version(&store).await?, [1004]);
    assert!(table_exists(&store, "activation_grants").await?);
    assert!(table_exists(&store, "people_request_history").await?);
    assert!(table_exists(&store, "household_invitation_history").await?);
    let after = row_digests(&store).await?;
    for (table, rows) in &before {
        assert_eq!(&after[table], rows, "{table} rows are preserved");
    }
    // Existing rows are untouched, and a device with only a pending grant is now listed
    // (BE-Q19 component 4): insert one with raw SQL, as no server writer exists in this crate.
    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE session_id=$1")
        .bind(&session)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(kept, 1);
    sqlx::query("INSERT INTO activation_grants(grant_hash,grant_id,account_id,device_id,auth_kind,challenge_hash,state,failed_verifiers,created_at,expires_at) VALUES ($1,$2,$3,$4,'local',$5,'issued',0,0,4102444800)")
        .bind(id()).bind(id()).bind(&seed.account).bind("grant-only-device").bind(id())
        .execute(&store.pool)
        .await?;
    let devices = store.devices(&seed.account, 0).await?;
    let grant_only = devices
        .iter()
        .find(|d| d.id == "grant-only-device")
        .expect("grant-only device is listed");
    assert_eq!(grant_only.summary.pending_sign_ins, 1);

    // Idempotent restart.
    store.migrate().await?;
    assert_eq!(version(&store).await?, [1004]);
    assert_timer_indexes_replaced(&store, &seed).await?;

    let (_fresh_dir, fresh_url) = crate::support::database::database_url().await?;
    let fresh = Store::connect(&fresh_url).await?;
    fresh.migrate().await?;
    for table in ["activation_grants", "oidc_flows", "timer_sessions"] {
        assert_eq!(
            shape(&fresh, table).await?,
            shape(&store, table).await?,
            "upgrade and baseline agree on {table}"
        );
    }
    Ok(())
}

/// BE-Q16: a request/invitation already pending at migration time must appear in the new
/// durable history tables immediately via the backfill, not only from the next transition.
#[tokio::test]
async fn a_1002_database_upgrades_to_1003_and_1004_in_place() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = baseline_1002(&url).await?;
    assert!(!table_exists(&store, "people_request_history").await?);
    assert!(!table_exists(&store, "household_invitation_history").await?);
    let seed = seed(&store, false).await?;
    let sender = seed.account.clone();
    let recipient = seed.other.clone();
    let request = id();
    sqlx::query("INSERT INTO people_requests(id,sender_id,recipient_id,kind,payload,expires_at) VALUES ($1,$2,$3,'link',$4,4102444800)")
        .bind(&request).bind(&sender).bind(&recipient)
        .bind(r#"{"kind":"link","person_id":"p","account_id":"a","person_version":1,"policy_version":1,"name":"Test"}"#)
        .execute(&store.pool)
        .await?;
    let household = id();
    sqlx::query("INSERT INTO households(id,name) VALUES ($1,'Home')")
        .bind(&household)
        .execute(&store.pool)
        .await?;
    let invitation = id();
    sqlx::query("INSERT INTO household_invitations(id,household_id,sender_id,recipient_id,status,expires_at) VALUES ($1,$2,$3,$4,'pending',4102444800)")
        .bind(&invitation).bind(&household).bind(&sender).bind(&recipient)
        .execute(&store.pool)
        .await?;
    let before = row_digests(&store).await?;

    store.migrate().await?;
    assert_eq!(version(&store).await?, [1004]);
    assert!(table_exists(&store, "people_request_history").await?);
    assert!(table_exists(&store, "household_invitation_history").await?);
    let after = row_digests(&store).await?;
    for (table, rows) in &before {
        assert_eq!(&after[table], rows, "{table} rows are preserved");
    }

    let (request_state, request_sender): (String, String) =
        sqlx::query_as("SELECT state,sender_id FROM people_request_history WHERE id=$1")
            .bind(&request)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(request_state, "pending");
    assert_eq!(request_sender, sender);

    let (invitation_status, invitation_version): (String, i64) =
        sqlx::query_as("SELECT status,version FROM household_invitation_history WHERE id=$1")
            .bind(&invitation)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(invitation_status, "pending");
    assert_eq!(invitation_version, 1);

    // Idempotent restart.
    store.migrate().await?;
    assert_eq!(version(&store).await?, [1004]);
    assert_timer_indexes_replaced(&store, &seed).await?;
    Ok(())
}

#[tokio::test]
async fn versions_the_chain_cannot_walk_keep_the_reset_error() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    let account = id();
    store
        .add_account(&account, &format!("u{}", account.replace('-', "")), "x")
        .await?;
    // 1004 is the current version, so the first version past it is the first unsupported one.
    for unsupported in [1_i64, 999, 1005, 2000] {
        sqlx::query("UPDATE atlas_schema SET version=$1")
            .bind(unsupported)
            .execute(&store.pool)
            .await?;
        let error = store.migrate().await.unwrap_err().to_string();
        assert!(error.contains("explicit reset"), "{unsupported}: {error}");
        assert_eq!(
            version(&store).await?,
            [unsupported],
            "the version is left as found"
        );
    }
    // More than one recorded version is not a baseline this code understands either. The single
    // surviving row is left at the current version (matching this store's actual, already-walked
    // shape), so the final `migrate()` below is a genuine no-op rather than re-running a step
    // against tables the fresh connection above already created.
    sqlx::query("UPDATE atlas_schema SET version=1004")
        .execute(&store.pool)
        .await?;
    sqlx::query("INSERT INTO atlas_schema(version) VALUES (1000)")
        .execute(&store.pool)
        .await?;
    assert!(
        store
            .migrate()
            .await
            .unwrap_err()
            .to_string()
            .contains("explicit reset")
    );
    sqlx::query("DELETE FROM atlas_schema WHERE version=1000")
        .execute(&store.pool)
        .await?;
    store.migrate().await?;
    let name: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(&account)
        .fetch_one(&store.pool)
        .await?;
    assert!(name.starts_with('u'), "content unchanged");
    Ok(())
}

#[tokio::test]
async fn two_processes_upgrading_at_once_both_succeed() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let first = baseline_1000(&url).await?;
    let second = Store::connect(&url).await?;
    let (a, b) = tokio::join!(first.migrate(), second.migrate());
    a?;
    b?;
    assert_eq!(version(&first).await?, [1004]);
    assert!(table_exists(&first, "operation_outcomes").await?);
    assert!(table_exists(&first, "activation_grants").await?);
    assert!(table_exists(&first, "people_request_history").await?);
    Ok(())
}

#[tokio::test]
async fn two_processes_upgrading_from_1001_at_once_both_succeed() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let first = baseline_1001(&url).await?;
    let second = Store::connect(&url).await?;
    let (a, b) = tokio::join!(first.migrate(), second.migrate());
    a?;
    b?;
    assert_eq!(version(&first).await?, [1004]);
    assert!(table_exists(&first, "activation_grants").await?);
    assert!(table_exists(&first, "people_request_history").await?);
    Ok(())
}

#[tokio::test]
async fn two_processes_upgrading_from_1002_at_once_both_succeed() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let first = baseline_1002(&url).await?;
    let second = Store::connect(&url).await?;
    let (a, b) = tokio::join!(first.migrate(), second.migrate());
    a?;
    b?;
    assert_eq!(version(&first).await?, [1004]);
    assert!(table_exists(&first, "people_request_history").await?);
    assert!(table_exists(&first, "household_invitation_history").await?);
    Ok(())
}

#[tokio::test]
async fn two_processes_upgrading_from_1003_at_once_both_succeed() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let first = baseline_1003(&url).await?;
    let seed = seed(&first, true).await?;
    let before = row_digests(&first).await?;
    let second = Store::connect(&url).await?;
    let (a, b) = tokio::join!(first.migrate(), second.migrate());
    a?;
    b?;
    assert_eq!(version(&first).await?, [1004]);
    assert_eq!(row_digests(&first).await?, before);
    assert_timer_indexes_replaced(&first, &seed).await?;
    Ok(())
}
