//! Deterministic schedules for the retirement protocol.
//!
//! The two engines order a competing writer differently, so a schedule names the point at which
//! the retirement is held *and* which writers can still commit there:
//!
//! * SQLite: `begin_serial()`'s first statement takes the database's single write lock, so a
//!   writer can commit first only at `retire.before_begin`; at any later pause it can only wait.
//! * PostgreSQL: until step 2 the retirement holds only the `sync_clock` row lock, so a writer
//!   that does not take `sync_clock` can also commit at `retire.after_ledger_read`.
//!   `begin_serial()` writers block on `sync_clock` and need `retire.before_begin` on both.
use anyhow::Result;
use atlas_core::{Store, hooks::Gate, operations::Operation};
use std::time::Duration;
use tokio::task::JoinHandle;

pub(crate) use crate::support::database::postgres;

/// PostgreSQL check (ai): every attempt of every retirement on `store` ran at the level the test
/// asked for, and at least one reached its locking reads, so a raise that silently did nothing
/// fails here. SQLite has no isolation level to raise.
pub(crate) fn assert_isolation(store: &Store, raised: bool) {
    if !postgres() {
        return;
    }
    let expected = if raised {
        "repeatable read"
    } else {
        "read committed"
    };
    let seen = store.hooks().isolation_seen();
    assert!(!seen.is_empty(), "no retirement reached its locking reads");
    assert!(seen.iter().all(|level| level == expected), "{seen:?}");
}

/// Held here, a writer that does not take `sync_clock` (revoke, logout, sweeps, retention
/// cleanup, `last_seen`, registration creation) commits *before* the retirement takes its locks.
pub(crate) fn writer_first_point() -> &'static str {
    if postgres() {
        "retire.after_ledger_read"
    } else {
        "retire.before_begin"
    }
}

/// Held here, no writer of a member row can commit until the retirement does.
pub(crate) const RETIREMENT_FIRST: &str = "retire.after_locking_reads";
/// A `begin_serial()` writer needs this on both engines.
pub(crate) const BEFORE_BEGIN: &str = "retire.before_begin";

/// A retirement held at a named point by an armed gate.
pub(crate) struct Paused {
    pub(crate) handle: JoinHandle<Result<Operation>>,
    gate: Gate,
    pub(crate) point: &'static str,
}

pub(crate) async fn pause(
    store: &Store,
    actor: &str,
    device: &str,
    operation: &str,
    token: &str,
    now: i64,
    point: &'static str,
) -> Paused {
    let mut gate = store.hooks().arm(point);
    let (store, actor, device, operation, token) = (
        store.clone(),
        actor.to_owned(),
        device.to_owned(),
        operation.to_owned(),
        token.to_owned(),
    );
    let handle = tokio::spawn(async move {
        store
            .retire_device(&actor, &device, &operation, &token, now)
            .await
    });
    gate.reached().await;
    Paused {
        handle,
        gate,
        point,
    }
}

impl Paused {
    /// Release the point and return the retirement's own result.
    pub(crate) async fn finish(self) -> Result<Operation> {
        self.gate.release();
        self.handle.await?
    }
}

/// Prove `task` is waiting on the paused retirement rather than merely slow.
/// PostgreSQL: a backend in a `Lock` wait that `pg_blocking_pids` attributes to the retirement's
/// own backend. SQLite: the call has not returned after a bounded wait (well inside the 10 s busy
/// timeout), so the test's completion must follow the retirement's commit.
pub(crate) async fn assert_blocked<T>(
    store: &Store,
    point: &str,
    task: &mut JoinHandle<T>,
) -> Result<()> {
    if postgres() {
        let pid = store
            .hooks()
            .backend_pid(point)
            .expect("an armed in-transaction point records its backend");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waits: Vec<Option<String>> = sqlx::query_scalar(
                    "SELECT wait_event_type FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))",
                )
                .bind(pid)
                .fetch_all(&store.pool)
                .await?;
                if waits.iter().any(|w| w.as_deref() == Some("Lock")) {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        assert!(
            !task.is_finished(),
            "a blocked writer must not have finished"
        );
    } else {
        assert!(
            // Short on purpose: a `sync()` writer retries SQLITE_BUSY for only ~0.6 s in total.
            tokio::time::timeout(Duration::from_millis(300), &mut *task)
                .await
                .is_err(),
            "the writer must still be waiting for the retirement's write lock"
        );
    }
    Ok(())
}
