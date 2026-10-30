//! BE-Q23 R3: the sync time of the `atlas-sync-diagnostic` fixture (feature `sync-diagnostic`).
//!
//! The fixture executable creates one [`SyncTime`] and installs it for its process. `/sync` then
//! reads its time from it, through the existing [`atlas_core::devices::Clock`] interface of
//! `Store::sync_bounded`; nothing else does. The executable's admission layer holds
//! [`SyncTime::shared`] for every HTTP request, and its owner's commands hold
//! [`SyncTime::exclusive`]. A normal build compiles none of this, and a diagnostic-feature process
//! that installs no time (such as the crate's own tests) keeps the system clock.
use anyhow::{Result, anyhow};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicI64, Ordering},
};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

static INSTALLED: OnceLock<Arc<SyncTime>> = OnceLock::new();

pub struct SyncTime {
    now: AtomicI64,
    /// Held shared by every HTTP request for its whole duration and exclusively by the owner
    /// while it inspects, sets up, moves time or collects, so each of those happens at an
    /// acknowledged quiescent boundary.
    gate: Arc<RwLock<()>>,
}

impl SyncTime {
    pub fn new(start: i64) -> Self {
        Self {
            now: AtomicI64::new(start),
            gate: Arc::new(RwLock::new(())),
        }
    }

    pub fn get(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }

    /// Only the owner calls this, while it holds [`SyncTime::exclusive`].
    pub fn set(&self, time: i64) {
        self.now.store(time, Ordering::SeqCst);
    }

    /// Held by one HTTP request; waits while the owner holds the gate exclusively.
    pub async fn shared(&self) -> OwnedRwLockReadGuard<()> {
        self.gate.clone().read_owned().await
    }

    /// Waits for in-flight HTTP requests and holds new ones back until the guard drops.
    pub async fn exclusive(&self) -> OwnedRwLockWriteGuard<()> {
        self.gate.clone().write_owned().await
    }
}

/// Makes `time` the process's `/sync` time; a process has at most one.
pub fn install(time: Arc<SyncTime>) -> Result<()> {
    INSTALLED
        .set(time)
        .map_err(|_| anyhow!("a sync time is already installed"))
}

/// The time of one `/sync` transaction attempt: the fixture's, logged at every read so each
/// attempt's chosen time is attributable, or the system clock in a process without a fixture.
pub(crate) fn sync_now() -> i64 {
    match INSTALLED.get() {
        Some(time) => {
            let now = time.get();
            eprintln!(
                "{}",
                serde_json::json!({"event":"diagnostic_sync_time","now":now})
            );
            now
        }
        None => crate::now(),
    }
}
