//! BE-Q23 R3: the sync time of the `atlas-sync-diagnostic` fixture (feature `sync-diagnostic`).
//!
//! The fixture executable creates one [`SyncTime`] and installs it for its process. `/sync` then
//! reads its time from it, through the existing [`atlas_core::devices::Clock`] interface of
//! `Store::sync_bounded`; nothing else does. The executable also wraps its router in [`gate`]. A normal build compiles none of this, and a
//! diagnostic-feature process that installs no time (such as the crate's own tests) keeps the
//! system clock.
use anyhow::{Result, anyhow};
use axum::{extract::Request, middleware::Next, response::Response};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicI64, Ordering},
};
use tokio::sync::{OwnedRwLockWriteGuard, RwLock};

static INSTALLED: OnceLock<Arc<SyncTime>> = OnceLock::new();

pub struct SyncTime {
    now: AtomicI64,
    /// Held shared by every `/sync` request for its whole duration and exclusively by the owner
    /// while it moves time or collects, so those happen at an acknowledged quiescent boundary.
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

    /// Waits for in-flight `/sync` requests and holds new ones back until the guard drops.
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

/// The fixture's outermost layer: each `/sync` request holds the gate shared for its whole
/// duration, so the owner's exclusive hold is a quiescent boundary.
pub async fn gate(request: Request, next: Next) -> Response {
    let held = match INSTALLED.get() {
        Some(time) if request.uri().path() == "/api/experimental/v1/sync" => {
            Some(time.gate.clone().read_owned().await)
        }
        _ => None,
    };
    let response = next.run(request).await;
    drop(held);
    response
}
