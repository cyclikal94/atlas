//! Test-only schedule control for the retirement protocol (feature `test-hooks`).
//!
//! Named points inside a transaction can be paused and released by a test, or made to run one
//! extra statement on the transaction's own connection. Registries belong to one `Store` (and its
//! clones), never to the process, so parallel tests cannot affect each other. Without the feature
//! the `hook!` call sites expand to nothing: no code and no point name reaches a release build.
use anyhow::Result;
use sqlx::{Any, Transaction};
use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
};
use tokio::sync::oneshot;

#[derive(Default)]
struct State {
    armed: HashMap<String, (oneshot::Sender<()>, oneshot::Receiver<()>)>,
    injected: HashMap<String, (String, Option<u32>)>,
    counts: HashMap<String, u32>,
    isolation_seen: Vec<String>,
    pids: HashMap<String, i32>,
    unlocked: bool,
    raised: bool,
    split_reads: bool,
    read_committed: bool,
    unverified: bool,
}

/// Held by a test for one armed point. Dropping it without `release` also releases the point,
/// so an abandoned schedule can never leave the code under test waiting.
pub struct Gate {
    reached: Option<oneshot::Receiver<()>>,
    release: Option<oneshot::Sender<()>>,
}

impl Gate {
    /// Wait until the code under test is paused at the point (bounded, so a wrong schedule
    /// fails the test instead of hanging it).
    pub async fn reached(&mut self) {
        let reached = self.reached.take().expect("reached() may be awaited once");
        tokio::time::timeout(std::time::Duration::from_secs(20), reached)
            .await
            .expect("point was not reached within 20 seconds")
            .expect("hooks were dropped before the point was reached");
    }

    pub fn release(mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

#[derive(Default)]
pub struct Hooks(Mutex<State>);

impl Hooks {
    fn state(&self) -> MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Pause the next arrival at `point`. The arming is consumed by that arrival.
    pub fn arm(&self, point: &str) -> Gate {
        let (reached_tx, reached_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        self.state()
            .armed
            .insert(point.to_owned(), (reached_tx, release_rx));
        Gate {
            reached: Some(reached_rx),
            release: Some(release_tx),
        }
    }

    /// Run `sql` on the transaction's own connection every time `point` is reached.
    pub fn inject_sql(&self, point: &str, sql: &str) {
        self.state()
            .injected
            .insert(point.to_owned(), (sql.to_owned(), None));
    }

    /// As `inject_sql`, but only for the next `times` arrivals.
    pub fn inject_sql_times(&self, point: &str, sql: &str, times: u32) {
        self.state()
            .injected
            .insert(point.to_owned(), (sql.to_owned(), Some(times)));
    }

    /// PostgreSQL negative control: read members without `FOR UPDATE`.
    pub fn row_locking(&self, enabled: bool) {
        self.state().unlocked = !enabled;
    }

    /// PostgreSQL check (ai): run the retirement at REPEATABLE READ instead of READ COMMITTED.
    pub fn raise_isolation(&self) {
        self.state().raised = true;
    }

    /// Negative control for the sharing snapshot: read the households on a second, later
    /// snapshot instead of the one the defaults were read in, on either engine.
    pub fn split_snapshot_reads(&self, enabled: bool) {
        self.state().split_reads = enabled;
    }

    /// PostgreSQL negative control for the sharing snapshot: READ COMMITTED instead of
    /// REPEATABLE READ.
    pub fn snapshot_read_committed(&self, enabled: bool) {
        self.state().read_committed = enabled;
    }

    /// Turn off the sharing snapshot's own consistency check (on by default), so a control can
    /// observe the torn result the check would otherwise refuse to return.
    pub fn verify_snapshot(&self, enabled: bool) {
        self.state().unverified = !enabled;
    }

    /// `SHOW transaction_isolation` as observed after the locking reads of each attempt, and
    /// between the two halves of a sharing snapshot.
    pub fn isolation_seen(&self) -> Vec<String> {
        self.state().isolation_seen.clone()
    }

    /// PostgreSQL backend that was paused at an armed in-transaction `point`, so a test can prove
    /// a writer is blocked *by that transaction* with `pg_blocking_pids`.
    pub fn backend_pid(&self, point: &str) -> Option<i32> {
        self.state().pids.get(point).copied()
    }

    /// How many times `point` has been reached.
    pub fn count(&self, point: &str) -> u32 {
        self.state().counts.get(point).copied().unwrap_or(0)
    }

    /// Attempts made by an operation whose points are `"<prefix>.attempt"`.
    pub fn attempts(&self, prefix: &str) -> u32 {
        self.count(&format!("{prefix}.attempt"))
    }

    pub(crate) fn locking(&self) -> bool {
        !self.state().unlocked
    }

    pub(crate) fn raised_isolation(&self) -> bool {
        self.state().raised
    }

    pub(crate) fn splits_snapshot_reads(&self) -> bool {
        self.state().split_reads
    }

    pub(crate) fn reads_committed(&self) -> bool {
        self.state().read_committed
    }

    pub(crate) fn verifies_snapshot(&self) -> bool {
        !self.state().unverified
    }

    /// Reach a point outside any transaction (or one that must not run injected SQL).
    pub async fn reach(&self, point: &str) {
        let gate = {
            let mut state = self.state();
            *state.counts.entry(point.to_owned()).or_default() += 1;
            state.armed.remove(point)
        };
        if let Some((reached, release)) = gate {
            let _ = reached.send(());
            // A dropped Gate releases too.
            let _ = release.await;
        }
    }

    /// Reach a point inside `tx`: record the isolation level where asked, pause, then run any
    /// injected statement on the transaction's connection.
    pub(crate) async fn reach_in(
        &self,
        point: &str,
        tx: &mut Transaction<'_, Any>,
        sqlite: bool,
    ) -> Result<()> {
        if matches!(
            point,
            "retire.after_locking_reads" | "sharing_snapshot.between_reads"
        ) && !sqlite
        {
            let level: String = sqlx::query_scalar("SHOW transaction_isolation")
                .fetch_one(&mut **tx)
                .await?;
            self.state().isolation_seen.push(level);
        }
        if !sqlite && self.state().armed.contains_key(point) {
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut **tx)
                .await?;
            self.state().pids.insert(point.to_owned(), pid);
        }
        self.reach(point).await;
        let injected = {
            let mut state = self.state();
            match state.injected.get_mut(point) {
                Some((sql, None)) => Some(sql.clone()),
                Some((sql, Some(remaining))) if *remaining > 0 => {
                    *remaining -= 1;
                    Some(sql.clone())
                }
                _ => None,
            }
        };
        if let Some(sql) = injected {
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }
}
