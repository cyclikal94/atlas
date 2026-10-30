//! BE-Q23 R3 diagnostic fixture: a real Atlas HTTP server whose `/sync` time and history collection
//! are driven by its owner, for real continuation-expiry (X0) and pruning (X2) evidence.
//!
//! ```text
//! atlas-sync-diagnostic --fixture-dir NEW_ABSOLUTE_DIR --attempt ID --bind 127.0.0.1:PORT
//! ```
//!
//! It creates `NEW_ABSOLUTE_DIR` (which must not exist) holding a new SQLite database and a
//! receipt binding the attempt, database file identity and executable hash, serves the normal
//! router on a loopback address only, and takes commands as JSON lines on stdin, answering each
//! on stdout. Its sync time starts at the system time and moves only by `advance_clock`; the real
//! `Store::collect_expired` runs only on `collect`. There is no periodic maintenance, integration
//! worker, HTTP control route, configuration switch or way to open an existing database. EOF on
//! stdin stops it, keeping the fixture for the owner; `shutdown` with `remove` deletes exactly the
//! files it created.
#[cfg(not(unix))]
compile_error!("atlas-sync-diagnostic needs Unix file identities");

use anyhow::{Context, Result, anyhow, bail, ensure};
use atlas_core::Store;
use atlas_server::{App, diagnostic::SyncTime, hash_password};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{
    fs,
    io::{BufRead, Read, Write},
    net::SocketAddr,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;

/// The production default (`ATLAS_SYNC_RETENTION_DAYS` unset); the fixture has no other setting.
const RETENTION_DAYS: i64 = 90;
const MAX_LINE: usize = 8192;
const MAX_ROWS: i64 = 1000;
const MAX_AGED: usize = 16;
const QUIESCENCE: Duration = Duration::from_secs(10);
const DATABASE: &str = "fixture.sqlite";
const RECEIPT: &str = "receipt.json";
/// Everything the fixture may create in its directory, and so all that `remove` may delete.
const OWNED: [&str; 5] = [
    DATABASE,
    "fixture.sqlite-wal",
    "fixture.sqlite-shm",
    "fixture.sqlite-journal",
    RECEIPT,
];

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn emit(value: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

struct Args {
    dir: PathBuf,
    attempt: String,
    bind: SocketAddr,
}

fn args() -> Result<Args> {
    let mut args = std::env::args().skip(1);
    let (mut dir, mut attempt, mut bind) = (None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| anyhow!("{flag} needs a value"))?;
        let slot = match flag.as_str() {
            "--fixture-dir" => &mut dir,
            "--attempt" => &mut attempt,
            "--bind" => &mut bind,
            _ => bail!("unsupported argument {flag}"),
        };
        ensure!(slot.replace(value).is_none(), "{flag} given twice");
    }
    let usage = "usage: atlas-sync-diagnostic --fixture-dir NEW_ABSOLUTE_DIR --attempt ID --bind 127.0.0.1:PORT";
    let (Some(dir), Some(attempt), Some(bind)) = (dir, attempt, bind) else {
        bail!(usage);
    };
    ensure!(
        (1..=64).contains(&attempt.len())
            && attempt
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "the attempt id must be 1-64 characters of [A-Za-z0-9_-]"
    );
    // An IP literal only: no name resolution, and never a non-loopback interface.
    let bind: SocketAddr = bind
        .parse()
        .map_err(|_| anyhow!("--bind must be an IP:port literal"))?;
    ensure!(
        bind.ip().is_loopback(),
        "refusing a non-loopback bind {bind}"
    );
    Ok(Args {
        dir: PathBuf::from(dir),
        attempt,
        bind,
    })
}

/// The fixture's directory, database file and receipt, by path and by file identity.
struct Fixture {
    dir: PathBuf,
    dir_id: (u64, u64),
    database: PathBuf,
    database_id: (u64, u64),
    receipt_sha256: Option<String>,
}

fn identity(path: &Path, directory: bool) -> Result<(u64, u64)> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        !meta.file_type().is_symlink()
            && if directory {
                meta.is_dir()
            } else {
                meta.is_file()
            },
        "{} is not the expected {}",
        path.display(),
        if directory { "directory" } else { "file" }
    );
    Ok((meta.dev(), meta.ino()))
}

impl Fixture {
    /// Creates a new directory and an empty database file in it; refuses anything that exists.
    fn create(requested: &Path) -> Result<Self> {
        ensure!(requested.is_absolute(), "--fixture-dir must be absolute");
        let name = requested
            .file_name()
            .filter(|n| Path::new(n).components().count() == 1 && *n != "." && *n != "..")
            .ok_or_else(|| anyhow!("--fixture-dir must name a new directory"))?;
        let parent = requested
            .parent()
            .ok_or_else(|| anyhow!("--fixture-dir needs a parent"))?
            .canonicalize()
            .context("the parent of --fixture-dir must exist")?;
        identity(&parent, true)?;
        let dir = parent.join(name);
        // Kept to characters that need no escaping in a SQLite URL.
        ensure!(
            dir.to_str().is_some_and(|s| s
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))),
            "--fixture-dir may contain only [A-Za-z0-9/._-]"
        );
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("creating {} (it must not exist)", dir.display()))?;
        let dir_id = identity(&dir, true)?;
        let database = dir.join(DATABASE);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&database)?;
        let database_id = identity(&database, false)?;
        Ok(Self {
            dir,
            dir_id,
            database,
            database_id,
            receipt_sha256: None,
        })
    }

    fn url(&self) -> String {
        // `mode=rw`: the file already exists and is never created by SQLite.
        format!("sqlite://{}?mode=rw", self.database.display())
    }

    /// Before every mutation and before cleanup: the same directory, database and receipt.
    fn verify(&self) -> Result<()> {
        ensure!(
            identity(&self.dir, true)? == self.dir_id,
            "fixture directory replaced"
        );
        ensure!(
            identity(&self.database, false)? == self.database_id,
            "fixture database replaced"
        );
        if let Some(expected) = &self.receipt_sha256 {
            let receipt = self.dir.join(RECEIPT);
            identity(&receipt, false)?;
            ensure!(
                &sha256(&fs::read(&receipt)?) == expected,
                "fixture receipt changed"
            );
        }
        Ok(())
    }

    fn write_receipt(&mut self, receipt: &Value) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(receipt)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.dir.join(RECEIPT))?
            .write_all(&bytes)?;
        self.receipt_sha256 = Some(sha256(&bytes));
        Ok(())
    }

    /// Deletes only the owned file names that are regular files, then the directory if that
    /// leaves it empty. Anything else is left and reported, never removed.
    fn remove(&self) -> Result<Vec<String>> {
        self.verify()?;
        let mut removed = Vec::new();
        for name in OWNED {
            let path = self.dir.join(name);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_file() => {
                    fs::remove_file(&path)?;
                    removed.push(name.to_owned());
                }
                Ok(_) => bail!("{} is not a regular file; left in place", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        fs::remove_dir(&self.dir)
            .with_context(|| format!("{} is not empty; left in place", self.dir.display()))?;
        Ok(removed)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    attempt: String,
    seq: u64,
    op: String,
    #[serde(default)]
    args: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddAccount {
    username: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgeHistory {
    batches: Vec<Aged>,
}

/// One declared batch: identified by account and revision and pinned by its payload digest.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Aged {
    account: String,
    revision: i64,
    payload_sha256: String,
    created_at: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct At {
    time: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Shutdown {
    remove: bool,
}

fn parse<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T> {
    let args = if args.is_null() { json!({}) } else { args };
    serde_json::from_value(args).map_err(|e| anyhow!("malformed arguments: {e}"))
}

struct Fixtured {
    store: Store,
    time: Arc<SyncTime>,
    start: i64,
    retention: i64,
    /// Until the first `advance_clock` or `collect`; only setup commands need it.
    setup: bool,
    aged: bool,
}

async fn rows(
    tx: &mut sqlx::Transaction<'static, sqlx::Any>,
    query: &'static str,
) -> Result<Vec<sqlx::any::AnyRow>> {
    Ok(sqlx::query(query)
        .bind(MAX_ROWS + 1)
        .fetch_all(&mut **tx)
        .await?)
}

/// The owned state the X0/X2 evidence depends on, read in one transaction.
async fn state(f: &Fixtured) -> Result<Value> {
    let mut tx = f.store.pool.begin().await?;
    let revision: i64 = sqlx::query_scalar("SELECT revision FROM sync_clock WHERE id=1")
        .fetch_one(&mut *tx)
        .await?;
    let accounts = rows(
        &mut tx,
        "SELECT id,sync_floor FROM accounts ORDER BY id LIMIT $1",
    )
    .await?;
    let batches = rows(&mut tx,
        "SELECT account_id,revision,created_at,payload FROM sync_batches ORDER BY account_id,revision LIMIT $1",
    )
    .await?;
    let devices = rows(&mut tx,
        "SELECT account_id,device_id,last_seen,cursor_key FROM sync_devices ORDER BY account_id,device_id LIMIT $1",
    )
    .await?;
    let sessions = rows(&mut tx,
        "SELECT account_id,device_id,expires_at,token_hash FROM sessions ORDER BY account_id,device_id,token_hash LIMIT $1",
    )
    .await?;
    let receipts = rows(&mut tx,
        "SELECT account_id,operation_id,payload,revision FROM receipts ORDER BY account_id,operation_id LIMIT $1",
    )
    .await?;
    let resources = rows(&mut tx,
        "SELECT id,owner_id,parent_id,kind,label,value,version,policy_version,archived FROM resources ORDER BY id LIMIT $1",
    )
    .await?;
    let truncated = [
        &accounts, &batches, &devices, &sessions, &receipts, &resources,
    ]
    .iter()
    .any(|set| set.len() as i64 > MAX_ROWS);
    let mut counts = serde_json::Map::new();
    for table in ["sync_cursors", "sync_snapshots", "sync_deliveries"] {
        let n: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                .fetch_one(&mut *tx)
                .await?;
        counts.insert(table.into(), n.into());
    }
    let digest = |rows: Vec<Value>| sha256(Value::Array(rows).to_string().as_bytes());
    let receipt_rows: Vec<Value> = receipts
        .iter()
        .map(|r| {
            json!([
                r.get::<String, _>(0),
                r.get::<String, _>(1),
                r.get::<String, _>(2),
                r.get::<i64, _>(3)
            ])
        })
        .collect();
    let resource_rows: Vec<Value> = resources
        .iter()
        .map(|r| {
            json!([
                r.get::<String, _>(0),
                r.get::<String, _>(1),
                r.get::<Option<String>, _>(2),
                r.get::<String, _>(3),
                r.get::<String, _>(4),
                r.get::<String, _>(5),
                r.get::<i64, _>(6),
                r.get::<i64, _>(7),
                r.get::<i64, _>(8)
            ])
        })
        .collect();
    let value = json!({
        "sync_now": f.time.get(),
        "revision_clock": revision,
        "accounts": accounts.iter().map(|r| json!({"id": r.get::<String,_>(0), "sync_floor": r.get::<i64,_>(1)})).collect::<Vec<_>>(),
        "batches": batches.iter().map(|r| {
            let payload: String = r.get(3);
            json!({"account": r.get::<String,_>(0), "revision": r.get::<i64,_>(1), "created_at": r.get::<i64,_>(2),
                "payload_sha256": sha256(payload.as_bytes()), "payload_bytes": payload.len()})
        }).collect::<Vec<_>>(),
        "devices": devices.iter().map(|r| json!({"account": r.get::<String,_>(0), "device": r.get::<String,_>(1),
            "last_seen": r.get::<i64,_>(2), "cursor_key_sha256": sha256(r.get::<String,_>(3).as_bytes())})).collect::<Vec<_>>(),
        "sessions": sessions.iter().map(|r| json!({"account": r.get::<String,_>(0), "device": r.get::<String,_>(1),
            "expires_at": r.get::<i64,_>(2), "token_hash": r.get::<String,_>(3)})).collect::<Vec<_>>(),
        "counts": counts,
        "receipts": receipt_rows.len(),
        "receipts_sha256": digest(receipt_rows),
        "resources": resource_rows.len(),
        "resources_sha256": digest(resource_rows),
        "truncated": truncated,
    });
    tx.commit().await?;
    Ok(value)
}

async fn quiescent(f: &Fixtured) -> Result<tokio::sync::OwnedRwLockWriteGuard<()>> {
    tokio::time::timeout(QUIESCENCE, f.time.exclusive())
        .await
        .map_err(|_| anyhow!("not quiescent: a /sync request held the gate"))
}

/// One accepted command. A failure answers the command and changes nothing, except where it sets
/// `partial`: the real collector failed after possibly committing some of its chunks.
async fn run(
    f: &mut Fixtured,
    fixture: &Fixture,
    op: &str,
    args: Value,
    partial: &mut Option<Value>,
) -> Result<Value> {
    match op {
        "inspect" => {
            parse::<NoArgs>(args)?;
            state(f).await
        }
        "add_account" => {
            let a: AddAccount = parse(args)?;
            ensure!(f.setup, "add_account is a setup command");
            fixture.verify()?;
            let id = Uuid::new_v4().to_string();
            let hash = hash_password(a.password).await?;
            f.store.add_account(&id, &a.username, &hash).await?;
            Ok(json!({"account": id}))
        }
        "age_history" => {
            let a: AgeHistory = parse(args)?;
            ensure!(f.setup && !f.aged, "age_history is a single setup command");
            ensure!(
                (1..=MAX_AGED).contains(&a.batches.len()),
                "age_history takes 1-{MAX_AGED} batches"
            );
            let mut keys: Vec<_> = a.batches.iter().map(|b| (&b.account, b.revision)).collect();
            keys.sort();
            keys.dedup();
            ensure!(keys.len() == a.batches.len(), "a batch is declared twice");
            fixture.verify()?;
            let _gate = quiescent(f).await?;
            let earliest = f.time.get() - f.retention;
            let mut tx = f.store.pool.begin_with("BEGIN IMMEDIATE").await?;
            let mut changed = Vec::new();
            for b in &a.batches {
                let row = sqlx::query(
                    "SELECT payload,created_at FROM sync_batches WHERE account_id=$1 AND revision=$2",
                )
                .bind(&b.account)
                .bind(b.revision)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| anyhow!("no batch {} for {}", b.revision, b.account))?;
                let payload: String = row.get(0);
                let created: i64 = row.get(1);
                ensure!(
                    sha256(payload.as_bytes()) == b.payload_sha256,
                    "batch {} payload differs from its declaration",
                    b.revision
                );
                // Only back-dating, and never beyond what the collector's cutoff needs now.
                ensure!(
                    (earliest..=created).contains(&b.created_at),
                    "created_at {} outside [{earliest}, {created}]",
                    b.created_at
                );
                let updated = sqlx::query("UPDATE sync_batches SET created_at=$1 WHERE account_id=$2 AND revision=$3 AND created_at=$4")
                    .bind(b.created_at).bind(&b.account).bind(b.revision).bind(created)
                    .execute(&mut *tx).await?;
                ensure!(updated.rows_affected() == 1, "batch {} changed", b.revision);
                changed.push(json!({"account": b.account, "revision": b.revision,
                    "payload_sha256": b.payload_sha256, "created_at_before": created, "created_at": b.created_at}));
            }
            tx.commit().await?;
            f.aged = true;
            Ok(json!({"aged": changed}))
        }
        "advance_clock" => {
            let a: At = parse(args)?;
            let from = f.time.get();
            let latest = f.start + 2 * f.retention;
            ensure!(
                a.time > from && a.time <= latest,
                "the sync time only advances, within ({from}, {latest}]"
            );
            fixture.verify()?;
            let _gate = quiescent(f).await?;
            f.time.set(a.time);
            f.setup = false;
            Ok(json!({"from": from, "to": a.time}))
        }
        "collect" => {
            let a: At = parse(args)?;
            let now = f.time.get();
            ensure!(
                (f.start..=now).contains(&a.time),
                "collection time outside [{}, {now}]",
                f.start
            );
            fixture.verify()?;
            let _gate = quiescent(f).await?;
            f.setup = false;
            let before = state(f).await?;
            let outcome = f.store.collect_expired(a.time).await;
            let after = state(f).await?;
            let key = |b: &Value| (b["account"].to_string(), b["revision"].to_string());
            let kept: Vec<_> = after["batches"]
                .as_array()
                .unwrap()
                .iter()
                .map(key)
                .collect();
            let removed: Vec<_> = before["batches"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| !kept.contains(&key(b)))
                .cloned()
                .collect();
            let report = json!({"time": a.time, "removed_batches": removed, "before": before, "after": after});
            match outcome {
                Ok(()) => Ok(report),
                // Earlier chunks may have committed: the states say what did.
                Err(e) => {
                    *partial = Some(report);
                    Err(e.context("the collector failed"))
                }
            }
        }
        _ => bail!("unsupported command {op}"),
    }
}

/// Reads stdin on its own thread (no async stdin in the normal build's runtime features): each
/// line, `Err` for an over-long line, then `None` at EOF.
fn commands() -> tokio::sync::mpsc::Receiver<Option<Result<String>>> {
    let (send, receive) = tokio::sync::mpsc::channel(1);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        loop {
            let mut line = Vec::new();
            let read = (&mut stdin)
                .take(MAX_LINE as u64 + 1)
                .read_until(b'\n', &mut line);
            let item = match read {
                Ok(0) => None,
                Ok(_) if line.len() > MAX_LINE || line.last() != Some(&b'\n') => Some(Err(
                    anyhow!("command line over {MAX_LINE} bytes or unterminated"),
                )),
                Ok(_) => Some(String::from_utf8(line).map_err(|_| anyhow!("command is not UTF-8"))),
                Err(e) => Some(Err(e.into())),
            };
            let stop = !matches!(item, Some(Ok(_)));
            if send.blocking_send(item).is_err() || stop {
                break;
            }
        }
    });
    receive
}

#[tokio::main]
async fn main() {
    if let Err(e) = serve().await {
        emit(&json!({"event": "failed", "error": format!("{e:#}")}));
        std::process::exit(2);
    }
}

async fn serve() -> Result<()> {
    // No inherited database, bind or integration configuration can reach this process.
    let inherited: Vec<_> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| k.starts_with("ATLAS_"))
        .collect();
    ensure!(
        inherited.is_empty(),
        "refusing inherited configuration {inherited:?}"
    );
    let args = args()?;
    let mut fixture = Fixture::create(&args.dir)?;
    let started = async {
        let store = Store::connect(&fixture.url())
            .await?
            .with_retention_days(RETENTION_DAYS)?;
        store.migrate().await?;
        fixture.verify()?;
        let listener = tokio::net::TcpListener::bind(args.bind).await?;
        anyhow::Ok((store, listener))
    }
    .await;
    let (store, listener) = match started {
        Ok(started) => started,
        Err(e) => {
            let cleanup = fixture.remove().map_err(|e| format!("{e:#}"));
            bail!("startup failed: {e:#}; cleanup {cleanup:?}");
        }
    };
    let address = listener.local_addr()?;
    let start = atlas_server::now();
    let time = Arc::new(SyncTime::new(start));
    let executable = std::env::current_exe()?;
    let receipt = json!({
        "schema": 1,
        "kind": "atlas-sync-diagnostic-receipt",
        "attempt": args.attempt,
        "pid": std::process::id(),
        "fixture_dir": {"path": fixture.dir, "dev": fixture.dir_id.0, "ino": fixture.dir_id.1},
        "database": {"path": fixture.database, "dev": fixture.database_id.0, "ino": fixture.database_id.1},
        "executable": {"path": executable, "sha256": sha256(&fs::read(&executable)?)},
        "features": {"sync-diagnostic": true, "test-hooks": cfg!(feature = "test-hooks")},
        "api_version": atlas_server::API_VERSION,
        "address": address.to_string(),
        "sync_start": start,
        "retention_seconds": RETENTION_DAYS * 86400,
    });
    fixture.write_receipt(&receipt)?;
    atlas_server::diagnostic::install(time.clone())?;
    let app = App::new(store.clone()).await?;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.router()
                .layer(axum::middleware::from_fn(atlas_server::diagnostic::gate))
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = stopped.await;
        })
        .await
    });
    emit(&json!({"event": "ready", "receipt": receipt, "receipt_sha256": fixture.receipt_sha256}));
    let mut f = Fixtured {
        store,
        time,
        start,
        retention: RETENTION_DAYS * 86400,
        setup: true,
        aged: false,
    };
    let mut next = 1u64;
    let mut lines = commands();
    // The `shutdown` command's sequence number, answered by the `stopped` event.
    let mut shutdown = None;
    let remove = loop {
        let line = match lines.recv().await.flatten() {
            None => break false,
            Some(Ok(line)) => line,
            Some(Err(e)) => {
                // Fail closed: the channel can no longer be read reliably.
                emit(&json!({"event": "rejected", "error": format!("{e:#}"), "stopping": true}));
                break false;
            }
        };
        let command: Command = match serde_json::from_str(&line) {
            Ok(command) => command,
            Err(e) => {
                emit(&json!({"event": "rejected", "error": format!("malformed command: {e}")}));
                continue;
            }
        };
        if command.attempt != args.attempt || command.seq != next {
            emit(
                &json!({"event": "rejected", "attempt": command.attempt, "seq": command.seq,
                "error": format!("expected attempt {} seq {next}", args.attempt)}),
            );
            continue;
        }
        next += 1;
        let base = json!({"attempt": command.attempt, "seq": command.seq, "op": command.op});
        let mut answer = base.as_object().unwrap().clone();
        if command.op == "shutdown" {
            match parse::<Shutdown>(command.args) {
                Ok(s) => {
                    shutdown = Some(command.seq);
                    break s.remove;
                }
                Err(e) => {
                    answer.insert("ok".into(), false.into());
                    answer.insert("error".into(), format!("{e:#}").into());
                    emit(&Value::Object(answer));
                    continue;
                }
            }
        }
        let mut partial = None;
        match run(&mut f, &fixture, &command.op, command.args, &mut partial).await {
            Ok(result) => {
                answer.insert("ok".into(), true.into());
                answer.insert("result".into(), result);
            }
            Err(e) => {
                answer.insert("ok".into(), false.into());
                answer.insert("error".into(), format!("{e:#}").into());
                if let Some(partial) = partial {
                    answer.insert("partial".into(), partial);
                }
            }
        }
        answer.insert("sync_now".into(), f.time.get().into());
        emit(&Value::Object(answer));
        // A replaced fixture stops the attempt instead of serving it.
        if let Err(e) = fixture.verify() {
            emit(&json!({"event": "ownership_lost", "error": format!("{e:#}"), "stopping": true}));
            break false;
        }
    };
    let _ = stop.send(());
    // A client holding a connection open must not keep the attempt alive indefinitely.
    let served = match tokio::time::timeout(QUIESCENCE, &mut server).await {
        Ok(served) => served.is_ok_and(|r| r.is_ok()),
        Err(_) => {
            server.abort();
            let _ = server.await;
            false
        }
    };
    f.store.pool.close().await;
    let cleanup = if remove {
        fixture.remove().map(|removed| json!(removed))
    } else {
        fixture.verify().map(|()| Value::Null)
    };
    emit(
        &json!({"event": "stopped", "attempt": args.attempt, "seq": shutdown,
        "served": served,
        "removed": cleanup.as_ref().ok(), "cleanup_error": cleanup.as_ref().err().map(|e| format!("{e:#}"))}),
    );
    cleanup.map(|_| ())
}
