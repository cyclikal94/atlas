//! Schema-level facts about `activation_grants` (BE-Q19) that do not depend on any HTTP route:
//! the retention sweep's SQL correctly selects only rows past their terminal-or-expiry time, the
//! issuance-cap count only counts `issued` rows, and the database itself enforces
//! `challenge_hash` uniqueness and the `state` `CHECK` constraint, on both engines.
use anyhow::Result;

use crate::support::devices::id;
use crate::support::resource_commands::{account, fixture};

const NOW: i64 = 1_700_000_000;
const DAY: i64 = 86_400;

#[allow(clippy::too_many_arguments)]
async fn insert(
    store: &atlas_core::Store,
    actor: &str,
    device: &str,
    state: &str,
    created_at: i64,
    expires_at: i64,
    redeemed_at: Option<i64>,
    cancelled_at: Option<i64>,
) -> Result<String> {
    let grant_id = id();
    sqlx::query("INSERT INTO activation_grants(grant_hash,grant_id,account_id,device_id,auth_kind,challenge_hash,state,failed_verifiers,created_at,expires_at,redeemed_at,cancelled_at) VALUES ($1,$2,$3,$4,'local',$5,$6,0,$7,$8,$9,$10)")
        .bind(id())
        .bind(&grant_id)
        .bind(actor)
        .bind(device)
        .bind(id())
        .bind(state)
        .bind(created_at)
        .bind(expires_at)
        .bind(redeemed_at)
        .bind(cancelled_at)
        .execute(&store.pool)
        .await?;
    Ok(grant_id)
}

/// The production sweep statement (`server/src/activation.rs::issue_grant`), run directly here:
/// only rows whose terminal-or-expiry time is more than 48h in the past are removed.
async fn sweep(store: &atlas_core::Store, before: i64) -> Result<u64> {
    Ok(sqlx::query(
        "DELETE FROM activation_grants WHERE COALESCE(redeemed_at,cancelled_at,expires_at)<=$1",
    )
    .bind(before)
    .execute(&store.pool)
    .await?
    .rows_affected())
}

#[tokio::test]
async fn the_retention_sweep_removes_only_rows_past_their_terminal_or_expiry_time() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    // Still `issued`, unexpired: never swept, however old the row.
    insert(
        &s,
        &a,
        "unexpired",
        "issued",
        NOW - 10 * DAY,
        NOW + 60,
        None,
        None,
    )
    .await?;
    // `issued` but expired less than 48h ago: not yet swept.
    insert(
        &s,
        &a,
        "recent-expiry",
        "issued",
        NOW - 3600,
        NOW - 3600,
        None,
        None,
    )
    .await?;
    // `issued` and expired more than 48h ago: swept.
    insert(
        &s,
        &a,
        "old-expiry",
        "issued",
        NOW - 3 * DAY,
        NOW - 3 * DAY,
        None,
        None,
    )
    .await?;
    // `redeemed`/`cancelled` less than 48h ago: not yet swept, regardless of `expires_at`.
    insert(
        &s,
        &a,
        "recent-redeemed",
        "redeemed",
        NOW - 3 * DAY,
        NOW - 3 * DAY,
        Some(NOW - 3600),
        None,
    )
    .await?;
    insert(
        &s,
        &a,
        "recent-cancelled",
        "cancelled",
        NOW - 3 * DAY,
        NOW - 3 * DAY,
        None,
        Some(NOW - 3600),
    )
    .await?;
    // `redeemed`/`cancelled` more than 48h ago: swept.
    insert(
        &s,
        &a,
        "old-redeemed",
        "redeemed",
        NOW - 3 * DAY,
        NOW - 3 * DAY,
        Some(NOW - 3 * DAY),
        None,
    )
    .await?;
    insert(
        &s,
        &a,
        "old-cancelled",
        "cancelled",
        NOW - 3 * DAY,
        NOW - 3 * DAY,
        None,
        Some(NOW - 3 * DAY),
    )
    .await?;

    let removed = sweep(&s, NOW - 48 * 3600).await?;
    assert_eq!(removed, 3, "old-expiry, old-redeemed, old-cancelled");
    let remaining: Vec<String> = sqlx::query_scalar(
        "SELECT device_id FROM activation_grants WHERE account_id=$1 ORDER BY device_id",
    )
    .bind(&a)
    .fetch_all(&s.pool)
    .await?;
    assert_eq!(
        remaining,
        [
            "recent-cancelled",
            "recent-expiry",
            "recent-redeemed",
            "unexpired"
        ]
    );
    Ok(())
}

/// The production issuance-cap count (`SELECT COUNT(*) ... WHERE state='issued'`) counts only
/// `issued` rows, so a terminal row never occupies the 1000-row budget.
#[tokio::test]
async fn the_issuance_cap_count_only_counts_issued_rows() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    for (device, state) in [("a", "issued"), ("b", "redeemed"), ("c", "cancelled")] {
        insert(&s, &a, device, state, NOW, NOW + 60, None, None).await?;
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM activation_grants WHERE account_id=$1 AND state='issued'",
    )
    .bind(&a)
    .fetch_one(&s.pool)
    .await?;
    assert_eq!(count, 1);
    Ok(())
}

#[tokio::test]
async fn challenge_hash_uniqueness_is_enforced_by_the_database() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    let challenge = id();
    insert(&s, &a, "first", "issued", NOW, NOW + 60, None, None).await?;
    sqlx::query(
        "UPDATE activation_grants SET challenge_hash=$1 WHERE account_id=$2 AND device_id='first'",
    )
    .bind(&challenge)
    .bind(&a)
    .execute(&s.pool)
    .await?;
    insert(&s, &a, "second", "issued", NOW, NOW + 60, None, None).await?;
    let error = sqlx::query(
        "UPDATE activation_grants SET challenge_hash=$1 WHERE account_id=$2 AND device_id='second'",
    )
    .bind(&challenge)
    .bind(&a)
    .execute(&s.pool)
    .await
    .unwrap_err();
    assert!(
        error
            .as_database_error()
            .is_some_and(|e| e.is_unique_violation()),
        "{error}"
    );
    Ok(())
}

#[tokio::test]
async fn the_state_check_constraint_rejects_an_invalid_value() -> Result<()> {
    let (_d, s) = fixture().await?;
    let a = account(&s).await?;
    let error = insert(&s, &a, "phone", "bogus", NOW, NOW + 60, None, None)
        .await
        .unwrap_err();
    let sqlx_error = error
        .downcast_ref::<sqlx::Error>()
        .expect("a sqlx-level error");
    let db_error = sqlx_error
        .as_database_error()
        .expect("a database-level rejection");
    assert_eq!(
        db_error.kind(),
        sqlx::error::ErrorKind::CheckViolation,
        "{db_error}"
    );
    Ok(())
}
