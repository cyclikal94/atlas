//! BE-B8: safe, distinguishable calendar-refresh error codes and the read-time
//! `refresh_in_progress` field, exercised directly at the `Store` level (no HTTP layer;
//! `crates/server/tests/cases/calendars.rs` covers the real router/`calendar_error()` wrapper).
//! Causes 1/2/3 (connection unavailable, fetch failure, missing server key) only arise inside
//! `refresh_link`'s own fetch attempt, so they are HTTP-level-only; causes 4/5/6a/6b are raised
//! directly by `finish_calendar_refresh_receipt` and are exercised here.
use anyhow::Result;
use atlas_core::{
    Command, Store,
    calendars::*,
    error::{ErrorCode, StaleRefreshReason},
    policy::{Policy, PrincipalGrant},
};
use uuid::Uuid;
fn id() -> String {
    Uuid::new_v4().to_string()
}
async fn account(s: &Store) -> Result<String> {
    let a = id();
    s.add_account(&a, &format!("cal-err-{}", Uuid::new_v4().simple()), "test")
        .await?;
    Ok(a)
}
async fn source(s: &Store, a: &str, initial_policy: Option<Policy>) -> Result<String> {
    let source = id();
    s.calendar_command(
        a,
        &id(),
        &CalendarCommand::CreateSource {
            id: source.clone(),
            label: "Calendar".into(),
            timezone: "UTC".into(),
            connection: None,
            initial_policy,
        },
    )
    .await?;
    Ok(source)
}
async fn health(s: &Store, id: &str) -> Result<String> {
    Ok(
        sqlx::query_scalar("SELECT health FROM calendar_sources WHERE id=$1")
            .bind(id)
            .fetch_one(&s.pool)
            .await?,
    )
}
async fn version(s: &Store, id: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT version FROM resources WHERE id=$1")
            .bind(id)
            .fetch_one(&s.pool)
            .await?,
    )
}
async fn policy_version(s: &Store, id: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT policy_version FROM resources WHERE id=$1")
            .bind(id)
            .fetch_one(&s.pool)
            .await?,
    )
}
async fn refresh_in_progress(
    s: &Store,
    a: &str,
    source_id: &str,
    now: i64,
) -> Result<Option<bool>> {
    Ok(
        s.calendar_resources(a, "calendar_source", None, None, 200, now)
            .await?
            .into_iter()
            .find(|p| p.id == source_id)
            .map(|p| p.value["refresh_in_progress"].as_bool().unwrap_or(false)),
    )
}
#[tokio::test]
async fn calendar_refresh_generation_change_is_distinguished_from_lease_expiry() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let s = Store::connect(&url).await?;
    s.migrate().await?;
    let a = account(&s).await?;

    // Cause 4: a concurrent settings edit bumps `generation` (and resets `lease_until`)
    // between begin and finish, with no newer refresh attempt in evidence.
    let source_id = source(&s, &a, None).await?;
    let before_health = health(&s, &source_id).await?;
    let refresh = s.begin_calendar_refresh(&a, &source_id, 1_000).await?;
    s.calendar_command(
        &a,
        &id(),
        &CalendarCommand::ConfigureSource {
            id: source_id.clone(),
            expected_version: version(&s, &source_id).await?,
            timezone: "Europe/London".into(),
            connection: None,
            disconnect: false,
            enabled: true,
        },
    )
    .await?;
    let error = s
        .finish_calendar_refresh(
            &a,
            &source_id,
            refresh.generation,
            None,
            (None, None),
            None,
            1_001,
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<StaleRefreshReason>().copied(),
        Some(StaleRefreshReason::GenerationChanged)
    );
    assert_eq!(
        health(&s, &source_id).await?,
        before_health,
        "a rejected stale attempt must not change recorded health"
    );

    // Cause 5: the identical `ensure!` also fires when only the lease has expired, with
    // `generation` unchanged — no settings edit, no newer refresh attempt.
    let lease_source = source(&s, &a, None).await?;
    let before_health = health(&s, &lease_source).await?;
    let refresh = s.begin_calendar_refresh(&a, &lease_source, 2_000).await?;
    let error = s
        .finish_calendar_refresh(
            &a,
            &lease_source,
            refresh.generation,
            None,
            (None, None),
            None,
            2_000 + 61, // past begin_calendar_refresh's fixed 60-second lease
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<StaleRefreshReason>().copied(),
        Some(StaleRefreshReason::LeaseExpired)
    );
    assert_eq!(
        health(&s, &lease_source).await?,
        before_health,
        "a rejected stale attempt must not change recorded health"
    );

    Ok(())
}
#[tokio::test]
async fn calendar_refresh_archived_and_access_lost_leave_health_unchanged() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let s = Store::connect(&url).await?;
    s.migrate().await?;
    let a = account(&s).await?;

    // Cause 6a: the source is archived (the user-facing "enabled" toggle) between begin and
    // finish — still visible/editable, so this is `invalid_value`, not `not_found`/`forbidden`.
    let archived_source = source(&s, &a, None).await?;
    let before_health = health(&s, &archived_source).await?;
    let refresh = s
        .begin_calendar_refresh(&a, &archived_source, 1_000)
        .await?;
    s.calendar_command(
        &a,
        &id(),
        &CalendarCommand::ConfigureSource {
            id: archived_source.clone(),
            expected_version: version(&s, &archived_source).await?,
            timezone: "UTC".into(),
            connection: None,
            disconnect: false,
            enabled: false,
        },
    )
    .await?;
    assert_eq!(
        s.finish_calendar_refresh(
            &a,
            &archived_source,
            refresh.generation,
            None,
            (None, None),
            None,
            1_001
        )
        .await
        .unwrap_err()
        .downcast_ref::<ErrorCode>()
        .copied(),
        Some(ErrorCode::InvalidValue)
    );
    assert_eq!(
        health(&s, &archived_source).await?,
        before_health,
        "an archived source's rejected attempt must not change recorded health"
    );

    // Cause 6b: a collaborator's edit access is revoked mid-refresh — the genuine
    // access-lost case, distinct from archiving.
    let b = account(&s).await?;
    let shared_source = source(
        &s,
        &a,
        Some(Policy {
            grants: vec![PrincipalGrant::Account {
                id: b.clone(),
                edit: true,
            }],
            exclude_accounts: vec![],
        }),
    )
    .await?;
    let before_health = health(&s, &shared_source).await?;
    let refresh = s.begin_calendar_refresh(&b, &shared_source, 1_000).await?;
    s.apply(
        &a,
        &id(),
        &[Command::Revoke {
            id: shared_source.clone(),
            expected_version: policy_version(&s, &shared_source).await?,
            account_id: b.clone(),
        }],
    )
    .await?;
    assert_eq!(
        s.finish_calendar_refresh(
            &b,
            &shared_source,
            refresh.generation,
            None,
            (None, None),
            None,
            1_001
        )
        .await
        .unwrap_err()
        .downcast_ref::<ErrorCode>()
        .copied(),
        Some(ErrorCode::NotFound)
    );
    assert_eq!(
        health(&s, &shared_source).await?,
        before_health,
        "a revoked collaborator's rejected attempt must not change recorded health"
    );

    Ok(())
}
/// The exact five-step trace from the approved brief's own acceptance section, at the
/// `Store` level: `refresh_in_progress` is a read-time comparison, never a stored value.
#[tokio::test]
async fn calendar_refresh_in_progress_reflects_live_lease_at_read_time() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let s = Store::connect(&url).await?;
    s.migrate().await?;
    let a = account(&s).await?;
    let source_id = source(&s, &a, None).await?;

    // 1. Before any refresh.
    assert_eq!(
        refresh_in_progress(&s, &a, &source_id, 1_000).await?,
        Some(false)
    );

    // 2. `begin_calendar_refresh`, listed again without finishing — live during the lease.
    s.begin_calendar_refresh(&a, &source_id, 1_000).await?;
    assert_eq!(
        refresh_in_progress(&s, &a, &source_id, 1_000).await?,
        Some(true)
    );

    // 3. `now` advanced past the lease with still no completing write — self-expires purely
    // from the read-time comparison, proving no write is needed at expiry.
    assert_eq!(
        refresh_in_progress(&s, &a, &source_id, 1_000 + 61).await?,
        Some(false)
    );

    // 4. `ConfigureSource` mid-lease resets `lease_until=0` in the same statement that bumps
    // `generation` — immediately false, no separate invalidation path.
    let refresh = s.begin_calendar_refresh(&a, &source_id, 2_000).await?;
    assert_eq!(
        refresh_in_progress(&s, &a, &source_id, 2_000).await?,
        Some(true)
    );
    s.calendar_command(
        &a,
        &id(),
        &CalendarCommand::ConfigureSource {
            id: source_id.clone(),
            expected_version: version(&s, &source_id).await?,
            timezone: "Europe/London".into(),
            connection: None,
            disconnect: false,
            enabled: true,
        },
    )
    .await?;
    assert_eq!(
        refresh_in_progress(&s, &a, &source_id, 2_000).await?,
        Some(false)
    );
    // The mid-lease refresh's own finish now rejects as a generation change (already covered
    // above); drop it here rather than leaving an unfinished lease dangling in this test.
    let _ = s
        .finish_calendar_refresh(
            &a,
            &source_id,
            refresh.generation,
            None,
            (None, None),
            None,
            2_000,
        )
        .await;

    // 5. Present only for `calendar_source`-kind rows, never for e.g. an `event` row from the
    // same source.
    let feed = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:trip\r\nDTSTART:20260910T090000Z\r\nSUMMARY:Trip\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let r = s.begin_calendar_refresh(&a, &source_id, 3_000).await?;
    let parsed = atlas_core::calendars::ics::parse(feed, &r.timezone, "2026-09-01", "2026-12-31")?;
    s.finish_calendar_refresh(
        &a,
        &source_id,
        r.generation,
        Some(&parsed),
        (None, None),
        None,
        3_000,
    )
    .await?;
    let events = s
        .calendar_resources(&a, "event", Some(&source_id), None, 200, 3_000)
        .await?;
    assert_eq!(events.len(), 1);
    assert!(
        events[0].value.get("refresh_in_progress").is_none(),
        "refresh_in_progress must only be added to calendar_source-kind projections"
    );

    Ok(())
}
