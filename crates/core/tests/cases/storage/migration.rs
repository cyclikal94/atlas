//! N1: the additive `1000 -> 1003` upgrade chain and the reset error for anything it cannot walk.
use anyhow::Result;
use atlas_core::Store;

use crate::support::devices::{FAR, add_session, id};

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

/// A database exactly as the `1002` release left it (BE-Q19's activation grants and OIDC
/// attempt columns present, BE-Q16's durable sent-request/invitation history absent),
/// recorded version 1002.
async fn baseline_1002(url: &str) -> Result<Store> {
    let store = Store::connect(url).await?;
    store.migrate().await?;
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

#[tokio::test]
async fn a_1000_database_upgrades_in_place() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = baseline_1000(&url).await?;
    assert!(!table_exists(&store, "operation_outcomes").await?);
    assert!(!table_exists(&store, "activation_grants").await?);
    let account = id();
    store
        .add_account(&account, &format!("u{}", account.replace('-', "")), "x")
        .await?;
    let session = add_session(&store, &account, "phone", FAR).await?;

    store.migrate().await?;
    assert_eq!(version(&store).await?, [1003]);
    assert!(table_exists(&store, "operation_outcomes").await?);
    assert!(table_exists(&store, "activation_grants").await?);
    assert!(table_exists(&store, "people_request_history").await?);
    assert!(table_exists(&store, "household_invitation_history").await?);
    // Existing rows are untouched.
    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE session_id=$1")
        .bind(&session)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(kept, 1);

    // Idempotent restart, and the upgraded database is usable for the new feature.
    store.migrate().await?;
    assert_eq!(version(&store).await?, [1003]);
    let outcome = store
        .revoke_session(&account, &session, &id(), 1000)
        .await?;
    assert_eq!(outcome.outcome.as_str(), "confirmed_applied");

    // A fresh database records the same version and has the same tables.
    let (_fresh_dir, fresh_url) = crate::support::database::database_url().await?;
    let fresh = Store::connect(&fresh_url).await?;
    fresh.migrate().await?;
    assert_eq!(version(&fresh).await?, [1003]);
    for table in [
        "operation_outcomes",
        "activation_grants",
        "oidc_flows",
        "people_request_history",
        "household_invitation_history",
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
    let account = id();
    store
        .add_account(&account, &format!("u{}", account.replace('-', "")), "x")
        .await?;
    let session = add_session(&store, &account, "phone", FAR).await?;

    store.migrate().await?;
    assert_eq!(version(&store).await?, [1003]);
    assert!(table_exists(&store, "activation_grants").await?);
    assert!(table_exists(&store, "people_request_history").await?);
    assert!(table_exists(&store, "household_invitation_history").await?);
    // Existing rows are untouched, and a device with only a pending grant is now listed
    // (BE-Q19 component 4): insert one with raw SQL, as no server writer exists in this crate.
    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE session_id=$1")
        .bind(&session)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(kept, 1);
    sqlx::query("INSERT INTO activation_grants(grant_hash,grant_id,account_id,device_id,auth_kind,challenge_hash,state,failed_verifiers,created_at,expires_at) VALUES ($1,$2,$3,$4,'local',$5,'issued',0,0,4102444800)")
        .bind(id()).bind(id()).bind(&account).bind("grant-only-device").bind(id())
        .execute(&store.pool)
        .await?;
    let devices = store.devices(&account, 0).await?;
    let grant_only = devices
        .iter()
        .find(|d| d.id == "grant-only-device")
        .expect("grant-only device is listed");
    assert_eq!(grant_only.summary.pending_sign_ins, 1);

    // Idempotent restart.
    store.migrate().await?;
    assert_eq!(version(&store).await?, [1003]);

    let (_fresh_dir, fresh_url) = crate::support::database::database_url().await?;
    let fresh = Store::connect(&fresh_url).await?;
    fresh.migrate().await?;
    for table in ["activation_grants", "oidc_flows"] {
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
async fn a_1002_database_upgrades_to_1003_in_place() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let store = baseline_1002(&url).await?;
    assert!(!table_exists(&store, "people_request_history").await?);
    assert!(!table_exists(&store, "household_invitation_history").await?);
    let sender = id();
    let recipient = id();
    store
        .add_account(&sender, &format!("u{}", sender.replace('-', "")), "x")
        .await?;
    store
        .add_account(&recipient, &format!("u{}", recipient.replace('-', "")), "x")
        .await?;
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

    store.migrate().await?;
    assert_eq!(version(&store).await?, [1003]);
    assert!(table_exists(&store, "people_request_history").await?);
    assert!(table_exists(&store, "household_invitation_history").await?);

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
    assert_eq!(version(&store).await?, [1003]);
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
    for unsupported in [1_i64, 999, 1004, 2000] {
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
    sqlx::query("UPDATE atlas_schema SET version=1003")
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
    assert_eq!(version(&first).await?, [1003]);
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
    assert_eq!(version(&first).await?, [1003]);
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
    assert_eq!(version(&first).await?, [1003]);
    assert!(table_exists(&first, "people_request_history").await?);
    assert!(table_exists(&first, "household_invitation_history").await?);
    Ok(())
}
