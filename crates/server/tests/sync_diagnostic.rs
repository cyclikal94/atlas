//! BE-Q23 R3: the `atlas-sync-diagnostic` fixture, driven as its owner would drive it: a real
//! process, real HTTP and its private stdin/stdout channel. X0 is real continuation expiry, X2 real
//! pruning by `Store::collect_expired`, each through bearer and through browser-cookie sessions;
//! the rest are its browser composition, refusals, ownership admission and quiescent boundaries.
//!
//! Runs only with `--features sync-diagnostic`. `SYNC_DIAGNOSTIC_BIN` selects a separately built
//! executable instead of Cargo's; `SYNC_DIAGNOSTIC_EVIDENCE` names a directory that receives every
//! control line and HTTP exchange (with body digests) as JSON lines.
//!
//! Schedules are ordered by barriers, never by sleeps: the fixture logs each admission phase to
//! stderr (`diagnostic_http`, `diagnostic_control`, `diagnostic_sync_time`), and a test holding
//! the fixture database's SQLite write lock ([`Writer`]) parks an admitted request or command at a
//! known point. Deadlines only turn a hang into a failure.
use anyhow::{Context, Result, anyhow, ensure};
use openidconnect::PkceCodeChallenge;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row};
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
use uuid::Uuid;

const V1: &str = "/api/experimental/v1";
const PASSWORD: &str = "local-test-password-123";
const RETENTION: i64 = 90 * 86400;
/// The browser origin the fixture is launched with: a preview-style loopback origin, deliberately
/// not the fixture's own bind address (the pair runner serves the client from its own port).
const ORIGIN: &str = "http://127.0.0.1:4173";
const DEADLINE: Duration = Duration::from_secs(30);

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn binary() -> PathBuf {
    std::env::var_os("SYNC_DIAGNOSTIC_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_atlas-sync-diagnostic").into())
}

/// Runs the executable with a cleared environment plus `env`, returning its exit status and
/// stdout lines; for invocations expected to stop by themselves.
fn run_once(args: &[&str], env: &[(&str, &str)]) -> Result<(Option<i32>, Vec<Value>)> {
    let output = Command::new(binary())
        .args(args)
        .env_clear()
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .output()?;
    let lines = String::from_utf8(output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    Ok((output.status.code(), lines))
}

/// Waits for `child` to exit, killing it at the deadline so that a hang fails instead.
fn exit_code(child: &mut Child) -> Result<Option<i32>> {
    let until = Instant::now() + DEADLINE;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code());
        }
        if Instant::now() > until {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("the fixture did not exit by the deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[derive(Clone)]
enum Auth {
    Bearer(String),
    /// A browser session: its cookie and CSRF value, sent with the configured Origin.
    Cookie {
        cookie: String,
        csrf: String,
    },
}

struct Response {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: Value,
    raw: String,
}

/// The HTTP side, separable from the fixture so a request can run in its own task.
#[derive(Clone)]
struct Http {
    client: reqwest::Client,
    base: String,
    evidence: Option<PathBuf>,
}

fn record(evidence: &Option<PathBuf>, entry: Value) -> Result<()> {
    if let Some(file) = evidence {
        std::fs::create_dir_all(file.parent().unwrap())?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)?;
        writeln!(file, "{entry}")?;
    }
    Ok(())
}

impl Http {
    /// One request with exactly `headers` (beyond `content-type` for a JSON body).
    async fn raw(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<Response> {
        let mut request = self
            .client
            .request(method.parse()?, format!("{}{V1}{path}", self.base));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(body) = &body {
            request = request.json(body);
        }
        let response = tokio::time::timeout(DEADLINE, request.send())
            .await
            .context("no HTTP response by the deadline")??;
        let status = response.status().as_u16();
        let response_headers = response.headers().clone();
        let raw = response.text().await?;
        let parsed = if raw.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&raw)?
        };
        let sent: Vec<_> = headers
            .iter()
            .map(|(n, v)| {
                let shown = if ["authorization", "cookie", "x-csrf-token"].contains(n) {
                    format!("sha256:{}", sha256(v.as_bytes()))
                } else {
                    (*v).to_owned()
                };
                json!([n, shown])
            })
            .collect();
        record(
            &self.evidence,
            json!({"http": {"method": method, "path": path, "headers": sent,
            "request": body, "status": status, "body_sha256": sha256(raw.as_bytes()), "body": parsed}}),
        )?;
        Ok(Response {
            status,
            headers: response_headers,
            body: parsed,
            raw,
        })
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        auth: Option<&Auth>,
        operation: Option<&str>,
        body: Option<Value>,
    ) -> Result<Response> {
        let mut headers = Vec::new();
        let bearer;
        match auth {
            Some(Auth::Bearer(token)) => {
                bearer = format!("Bearer {token}");
                headers.push(("authorization", bearer.as_str()));
            }
            Some(Auth::Cookie { cookie, csrf }) => {
                headers.push(("cookie", cookie.as_str()));
                headers.push(("x-csrf-token", csrf.as_str()));
                headers.push(("origin", ORIGIN));
            }
            None => {}
        }
        if let Some(operation) = operation {
            headers.push(("idempotency-key", operation));
        }
        self.raw(method, path, &headers, body).await
    }
}

/// An external SQLite write lock on the fixture's database: a request or command that needs to
/// write waits for it (within the 10 s busy timeout) while it is held.
struct Writer(sqlx::AnyConnection);

impl Writer {
    async fn hold(database: &Path) -> Result<Self> {
        let mut connection = connect(database, "rw").await?;
        sqlx::raw_sql("BEGIN IMMEDIATE")
            .execute(&mut connection)
            .await?;
        Ok(Self(connection))
    }

    async fn release(mut self) -> Result<()> {
        sqlx::raw_sql("ROLLBACK").execute(&mut self.0).await?;
        self.0.close().await?;
        Ok(())
    }
}

async fn connect(database: &Path, mode: &str) -> Result<sqlx::AnyConnection> {
    sqlx::any::install_default_drivers();
    Ok(
        sqlx::AnyConnection::connect(&format!("sqlite://{}?mode={mode}", database.display()))
            .await?,
    )
}

/// A copy of a stopped fixture's database `main` with its write-ahead log `wal`, under a name that
/// pairs them again: SQLite finds a WAL by the database's name, and a renamed database leaves its
/// WAL under the original name.
fn paired_copy(main: &Path, wal: &Path) -> Result<(tempfile::TempDir, PathBuf)> {
    let dir = tempfile::tempdir()?;
    let copy = dir.path().join("copy.sqlite");
    std::fs::copy(main, &copy)?;
    if wal.exists() {
        std::fs::copy(wal, dir.path().join("copy.sqlite-wal"))?;
    }
    Ok((dir, copy))
}

/// One value read directly from a fixture database, for after the fixture has stopped.
async fn scalar(database: &Path, query: &str) -> Result<i64> {
    let mut connection = connect(database, "ro").await?;
    let row = sqlx::query(sqlx::AssertSqlSafe(query.to_owned()))
        .fetch_one(&mut connection)
        .await?;
    let value = row.try_get::<i64, _>(0)?;
    connection.close().await?;
    Ok(value)
}

struct Diagnostic {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: mpsc::Receiver<String>,
    attempt: String,
    seq: u64,
    dir: PathBuf,
    log: PathBuf,
    ready: Value,
    evidence: Option<PathBuf>,
    http: Http,
    _root: tempfile::TempDir,
}

impl Diagnostic {
    fn spawn(name: &str) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let attempt = format!("{name}-{}", &Uuid::new_v4().simple().to_string()[..8]);
        let dir = root.path().canonicalize()?.join("fixture");
        let log = root.path().join("server.log");
        let mut child = Command::new(binary())
            .args([
                "--fixture-dir",
                dir.to_str().unwrap(),
                "--attempt",
                &attempt,
            ])
            .args(["--bind", "127.0.0.1:0", "--public-origin", ORIGIN])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&log)?)
            .spawn()?;
        let stdin = child.stdin.take();
        // Read on a thread so every wait for an answer or event has a deadline.
        let reader = BufReader::new(child.stdout.take().unwrap());
        let (send, stdout) = mpsc::channel();
        std::thread::spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let evidence = std::env::var_os("SYNC_DIAGNOSTIC_EVIDENCE")
            .map(|dir| PathBuf::from(dir).join(format!("{attempt}.jsonl")));
        let mut d = Self {
            child,
            stdin,
            stdout,
            seq: 0,
            attempt,
            dir,
            log,
            ready: Value::Null,
            evidence: evidence.clone(),
            http: Http {
                client: reqwest::Client::new(),
                base: String::new(),
                evidence,
            },
            _root: root,
        };
        let ready = d.next()?;
        ensure!(ready["event"] == "ready", "{ready}");
        d.http.base = format!("http://{}", ready["receipt"]["address"].as_str().unwrap());
        d.ready = ready;
        Ok(d)
    }

    fn record(&self, entry: Value) -> Result<()> {
        record(&self.evidence, entry)
    }

    fn sync_start(&self) -> i64 {
        self.ready["receipt"]["sync_start"].as_i64().unwrap()
    }

    fn database(&self) -> PathBuf {
        self.dir.join("fixture.sqlite")
    }

    /// The next stdout line: an answer or an event.
    fn next(&mut self) -> Result<Value> {
        let line = self
            .stdout
            .recv_timeout(DEADLINE)
            .map_err(|e| anyhow!("no fixture output by the deadline ({e})"))?;
        let value: Value = serde_json::from_str(&line)
            .map_err(|e| anyhow!("unreadable output ({e}): {line:?}"))?;
        self.record(json!({"received": value}))?;
        Ok(value)
    }

    /// Whether an answer or event is already waiting, without blocking.
    fn pending(&self) -> bool {
        self.stdout.try_recv().is_ok()
    }

    /// Sends one raw line without waiting for its answer.
    fn send_line(&mut self, line: &str) -> Result<()> {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        self.record(json!({"sent": line}))
    }

    /// Sends one raw line and reads the one line it answers.
    fn line(&mut self, line: &str) -> Result<Value> {
        self.send_line(line)?;
        self.next()
    }

    /// Sends the next command in sequence without waiting; returns its sequence number.
    fn send(&mut self, op: &str, args: Value) -> Result<u64> {
        self.seq += 1;
        let line = json!({"attempt": self.attempt, "seq": self.seq, "op": op, "args": args});
        self.send_line(&line.to_string())?;
        Ok(self.seq)
    }

    /// The next command in sequence; returns its whole answer.
    fn command(&mut self, op: &str, args: Value) -> Result<Value> {
        self.send(op, args)?;
        self.next()
    }

    /// A command that must succeed; returns its result.
    fn ok(&mut self, op: &str, args: Value) -> Result<Value> {
        let answer = self.command(op, args)?;
        ensure!(answer["ok"] == true, "{op} failed: {answer}");
        Ok(answer["result"].clone())
    }

    fn refused(&mut self, op: &str, args: Value, reason: &str) -> Result<()> {
        let answer = self.command(op, args)?;
        assert_eq!(answer["ok"], false, "{op} was acknowledged: {answer}");
        assert!(
            answer["error"].as_str().unwrap().contains(reason),
            "{op}: {answer}"
        );
        Ok(())
    }

    fn inspect(&mut self) -> Result<Value> {
        self.ok("inspect", Value::Null)
    }

    /// Waits until the fixture's stderr holds at least `count` events matching `matches`.
    fn wait_log(&self, what: &str, count: usize, matches: impl Fn(&Value) -> bool) -> Result<()> {
        let until = Instant::now() + DEADLINE;
        loop {
            let seen = std::fs::read_to_string(&self.log)?
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter(|v| matches(v))
                .count();
            if seen >= count {
                return Ok(());
            }
            ensure!(Instant::now() < until, "no {what} by the deadline");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for the `n`th HTTP admission event of `phase` for `path`.
    fn wait_http(&self, phase: &str, path: &str, n: usize) -> Result<()> {
        let path = format!("{V1}{path}");
        self.wait_log(&format!("{phase} {path}"), n, |v| {
            v["event"] == "diagnostic_http" && v["phase"] == phase && v["path"] == path.as_str()
        })
    }

    /// Waits for command `seq` to reach `phase` of its admission.
    fn wait_control(&self, seq: u64, phase: &str) -> Result<()> {
        self.wait_log(&format!("command {seq} {phase}"), 1, |v| {
            v["event"] == "diagnostic_control" && v["seq"] == seq && v["phase"] == phase
        })
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        auth: Option<&Auth>,
        operation: Option<&str>,
        body: Option<Value>,
    ) -> Result<Response> {
        self.http.call(method, path, auth, operation, body).await
    }

    fn add_account(&mut self, name: &str) -> Result<String> {
        Ok(self.ok(
            "add_account",
            json!({"username": name, "password": PASSWORD}),
        )?["account"]
            .as_str()
            .unwrap()
            .to_owned())
    }

    /// A new account signed in through the bearer session route.
    async fn login(&mut self, name: &str, device: &str) -> Result<(String, Auth)> {
        let account = self.add_account(name)?;
        let r = self
            .call(
                "POST",
                "/sessions",
                None,
                None,
                Some(json!({"username": name, "password": PASSWORD, "device_id": device})),
            )
            .await?;
        ensure!(r.status == 200, "login: {}", r.raw);
        Ok((
            account,
            Auth::Bearer(r.body["access_token"].as_str().unwrap().to_owned()),
        ))
    }

    /// A new account signed in as a browser would: grant, activation (the only route that sets
    /// the cookie) and the reload bootstrap of `/browser-sessions/current` for the CSRF value.
    async fn browser_login(&mut self, name: &str, device: &str) -> Result<(String, Auth)> {
        let account = self.add_account(name)?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let grant = self
            .http
            .raw(
                "POST",
                "/browser-sessions",
                &[("origin", ORIGIN)],
                Some(
                    json!({"username": name, "password": PASSWORD, "device_id": device,
                    "attempt_challenge": challenge.as_str()}),
                ),
            )
            .await?;
        ensure!(grant.status == 200, "browser login: {}", grant.raw);
        let activated = self
            .http
            .raw(
                "POST",
                "/browser-sessions/activate",
                &[("origin", ORIGIN)],
                Some(json!({"grant": grant.body["grant"], "verifier": verifier.secret()})),
            )
            .await?;
        ensure!(activated.status == 200, "activation: {}", activated.raw);
        let cookie = activated.headers["set-cookie"]
            .to_str()?
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let current = self
            .http
            .raw(
                "GET",
                "/browser-sessions/current",
                &[("cookie", cookie.as_str()), ("x-atlas-session", "1")],
                None,
            )
            .await?;
        ensure!(current.status == 200, "current session: {}", current.raw);
        assert_eq!(current.body["account_id"], account);
        Ok((
            account,
            Auth::Cookie {
                cookie,
                csrf: current.body["csrf_token"].as_str().unwrap().to_owned(),
            },
        ))
    }

    async fn account(&mut self, name: &str, device: &str, browser: bool) -> Result<(String, Auth)> {
        if browser {
            self.browser_login(name, device).await
        } else {
            self.login(name, device).await
        }
    }

    async fn sync(&self, auth: &Auth, query: &str) -> Result<Response> {
        self.call("GET", &format!("/sync{query}"), Some(auth), None, None)
            .await
    }

    /// Every page from no cursor to the live edge; returns the resource IDs and the final cursor.
    async fn snapshot(&self, auth: &Auth) -> Result<(Vec<String>, String)> {
        let mut page = self.sync(auth, "").await?;
        let mut ids = Vec::new();
        loop {
            ensure!(page.status == 200, "snapshot: {}", page.raw);
            for batch in page.body["batches"].as_array().unwrap() {
                for change in batch["changes"].as_array().unwrap() {
                    ids.push(change["resource"]["id"].as_str().unwrap().to_owned());
                }
            }
            let cursor = page.body["next_cursor"].as_str().unwrap().to_owned();
            if page.body["has_more"] == false {
                ids.sort();
                return Ok((ids, cursor));
            }
            page = self.sync(auth, &format!("?cursor={cursor}")).await?;
        }
    }

    /// One person with twelve fields in one revision: four bounded pieces at `max_changes=4`.
    async fn publish(&self, auth: &Auth, operation: &str) -> Result<(i64, Value, Vec<String>)> {
        let (body, ids) = person_commands();
        let r = self
            .call(
                "POST",
                "/commands",
                Some(auth),
                Some(operation),
                Some(body.clone()),
            )
            .await?;
        ensure!(r.status == 200, "publish: {}", r.raw);
        Ok((r.body["revision"].as_i64().unwrap(), body, ids))
    }

    /// Ends the attempt with `shutdown` and waits for the process.
    fn finish(mut self, remove: bool) -> Result<Value> {
        let answer = self.command("shutdown", json!({"remove": remove}))?;
        assert_eq!(answer["event"], "stopped", "{answer}");
        assert_eq!(answer["seq"], self.seq, "{answer}");
        drop(self.stdin.take());
        assert_eq!(exit_code(&mut self.child)?, Some(0));
        assert_eq!(self.dir.exists(), !remove);
        Ok(answer)
    }

    /// After a loss: the `ownership_lost` event, then `stopped` without any cleanup, exit 2.
    fn stops_lost(&mut self, reason: &str) -> Result<()> {
        let lost = self.next()?;
        assert_eq!(lost["event"], "ownership_lost", "{lost}");
        assert!(lost["error"].as_str().unwrap().contains(reason), "{lost}");
        let stopped = self.next()?;
        assert_eq!(stopped["event"], "stopped", "{stopped}");
        assert!(
            stopped["cleanup_error"]
                .as_str()
                .is_some_and(|e| e.contains("ownership lost")),
            "a lost fixture is not cleaned up: {stopped}"
        );
        assert_eq!(exit_code(&mut self.child)?, Some(2));
        Ok(())
    }

    /// The fixture times the server logged for its `/sync` transaction attempts.
    fn logged_sync_times(&self) -> Result<Vec<i64>> {
        Ok(std::fs::read_to_string(&self.log)?
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["event"] == "diagnostic_sync_time")
            .map(|v| v["now"].as_i64().unwrap())
            .collect())
    }
}

impl Drop for Diagnostic {
    fn drop(&mut self) {
        // EOF stops an attempt that a failed assertion left running.
        drop(self.stdin.take());
        let _ = exit_code(&mut self.child);
    }
}

fn person_commands() -> (Value, Vec<String>) {
    let person = Uuid::new_v4().to_string();
    let mut ids = vec![person.clone()];
    let mut commands = vec![json!({"kind": "create_person", "id": person, "name": "Diagnostic"})];
    for _ in 0..12 {
        let id = Uuid::new_v4().to_string();
        commands.push(
            json!({"kind": "create_field", "id": id, "person_id": person,
            "label": "Field", "value": "Value"}),
        );
        ids.push(id);
    }
    ids.sort();
    (json!({"commands": commands}), ids)
}

/// A command answered after the fixture lost ownership: never an owned success, and with the
/// effect it had.
fn lost_answer(answer: &Value, effect: &str) {
    assert_eq!(answer["ok"], false, "acknowledged after the loss: {answer}");
    assert_eq!(
        answer["ownership_lost"], true,
        "not reported as lost: {answer}"
    );
    assert_eq!(answer["effect"], effect, "wrong effect reported: {answer}");
    assert!(answer.get("result").is_none(), "{answer}");
    assert!(answer.get("partial").is_none(), "{answer}");
}

/// The expiry of any signed cursor (`d1` live edge or `d2` continuation): its third field.
fn cursor_expiry(token: &str) -> i64 {
    assert!(
        token.starts_with("d1.") || token.starts_with("d2."),
        "{token}"
    );
    i64::from_str_radix(token.split('.').nth(2).unwrap(), 16).unwrap()
}

fn expiry(token: &str) -> i64 {
    assert!(token.starts_with("d2."), "{token}");
    i64::from_str_radix(token.split('.').nth(2).unwrap(), 16).unwrap()
}

fn piece(r: &Response) -> (i64, i64, Vec<String>) {
    let batch = &r.body["batches"][0];
    let ids = batch["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["resource"]["id"].as_str().unwrap().to_owned())
        .collect();
    (
        batch["revision"].as_i64().unwrap(),
        batch["segment"]["index"].as_i64().unwrap(),
        ids,
    )
}

fn floor(state: &Value, account: &str) -> i64 {
    state["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == account)
        .unwrap()["sync_floor"]
        .as_i64()
        .unwrap()
}

fn batch<'a>(state: &'a Value, account: &str, revision: i64) -> Option<&'a Value> {
    state["batches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["account"] == account && b["revision"] == revision)
}

fn wall() -> i64 {
    atlas_server::now()
}

/// The parts of the owned state that neither expiry nor this test's sync requests may change.
fn durable(state: &Value) -> Value {
    let devices: Vec<_> = state["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| json!([d["account"], d["device"], d["cursor_key_sha256"]]))
        .collect();
    json!({"accounts": state["accounts"], "batches": state["batches"], "sessions": state["sessions"],
        "devices": devices, "revision_clock": state["revision_clock"],
        "receipts_sha256": state["receipts_sha256"], "resources_sha256": state["resources_sha256"]})
}

/// X0 (S06): a genuinely issued continuation succeeds before its expiry and, with only the
/// fixture's sync time moved, is refused at and after it by the real expiry check: no collection,
/// the session still authenticates on the system clock, and history, floor and key are unchanged.
async fn x0(browser: bool) -> Result<()> {
    let mut d = Diagnostic::spawn(if browser { "x0-browser" } else { "x0" })?;
    let t = d.sync_start();
    let (alice, token) = d.account("alice", "phone", browser).await?;
    let (_, head) = d.snapshot(&token).await?;
    let operation = Uuid::new_v4().to_string();
    let (revision, body, published) = d.publish(&token, &operation).await?;
    let first = d
        .sync(&token, &format!("?cursor={head}&max_changes=4"))
        .await?;
    assert_eq!(first.status, 200, "{}", first.raw);
    assert_eq!(piece(&first).1, 0);
    let target = first.body["next_cursor"].as_str().unwrap().to_owned();
    let e = expiry(&target);
    assert_eq!(e, t + RETENTION, "issued at the fixture's sync time");
    let query = format!("?cursor={target}&max_changes=4");
    let positive = d.sync(&token, &query).await?;
    assert_eq!(positive.status, 200, "pre-expiry: {}", positive.raw);
    assert_eq!(piece(&positive).0, revision);
    assert_eq!(piece(&positive).1, 1);
    let again = d.sync(&token, &query).await?;
    assert_eq!(
        again.raw, positive.raw,
        "a retry at the same time is identical"
    );
    let before = d.inspect()?;
    assert_eq!(batch(&before, &alice, revision).map(|_| ()), Some(()));

    d.ok("advance_clock", json!({"time": e - 1}))?;
    let last_second = d.sync(&token, &query).await?;
    assert_eq!(last_second.status, 200, "at E-1: {}", last_second.raw);
    assert_eq!(
        piece(&last_second).2,
        piece(&positive).2,
        "same piece content"
    );
    d.ok("advance_clock", json!({"time": e}))?;
    let refused = d.sync(&token, &query).await?;
    assert_eq!(
        (refused.status, refused.body["code"].as_str()),
        (410, Some("resync_required")),
        "X0 at E: {}",
        refused.raw
    );
    d.ok("advance_clock", json!({"time": e + 1}))?;
    let after_expiry = d.sync(&token, &query).await?;
    assert_eq!(after_expiry.status, 410, "X0 after E: {}", after_expiry.raw);

    // Expiry alone: no collection ran, nothing durable moved, the session is live on wall time.
    let after = d.inspect()?;
    assert_eq!(durable(&after), durable(&before), "only time moved");
    assert_eq!(floor(&after, &alice), 0);
    for session in after["sessions"].as_array().unwrap() {
        assert!(session["expires_at"].as_i64().unwrap() > wall());
    }
    let me = d.call("GET", "/me", Some(&token), None, None).await?;
    assert_eq!(me.status, 200, "the refusal is not authentication");
    let times = d.logged_sync_times()?;
    assert!(times.contains(&e) && times.contains(&(e - 1)), "{times:?}");

    // Recovery: a fresh snapshot under the fixture time holds the whole published revision, and
    // the original command's receipt replays unchanged.
    let (ids, _) = d.snapshot(&token).await?;
    assert_eq!(ids, published);
    let receipt = d
        .call(
            "POST",
            "/commands",
            Some(&token),
            Some(&operation),
            Some(body),
        )
        .await?;
    assert_eq!(receipt.body["revision"].as_i64(), Some(revision));
    assert_eq!(d.inspect()?["receipts_sha256"], before["receipts_sha256"]);
    d.finish(true)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn x0_real_continuation_expiry() -> Result<()> {
    x0(false).await
}

/// X0 through a browser session: cookie, CSRF and Origin on every request, as the client sends.
#[tokio::test(flavor = "multi_thread")]
async fn x0_real_continuation_expiry_browser() -> Result<()> {
    x0(true).await
}

/// X2 (S07): with declared history aged before the target continuation is issued, the real
/// collector at the fixture's own time prunes exactly that batch and advances the floor, and the
/// same token, still unexpired, is then refused. Another account's unpruned chain still succeeds;
/// sessions, devices, receipts and canonical content are unchanged.
async fn x2(browser: bool) -> Result<()> {
    let mut d = Diagnostic::spawn(if browser { "x2-browser" } else { "x2" })?;
    let t = d.sync_start();
    let (alice, a_token) = d.account("alice", "phone", browser).await?;
    let (bob, b_token) = d.account("bob", "phone", browser).await?;
    let (_, a_head) = d.snapshot(&a_token).await?;
    let (_, b_head) = d.snapshot(&b_token).await?;
    let a_op = Uuid::new_v4().to_string();
    let (a_rev, a_body, a_ids) = d.publish(&a_token, &a_op).await?;
    let (b_rev, _, _) = d.publish(&b_token, &Uuid::new_v4().to_string()).await?;
    let setup = d.inspect()?;
    let declared = |account: &str, revision: i64, created_at: i64| {
        json!({"account": account, "revision": revision, "created_at": created_at,
            "payload_sha256": batch(&setup, account, revision).unwrap()["payload_sha256"]})
    };
    // The target batch reaches the collector's cutoff at T; the control is one second younger.
    let aged = json!({"batches": [declared(&alice, a_rev, t - RETENTION), declared(&bob, b_rev, t - RETENTION + 1)]});
    d.ok("age_history", aged.clone())?;
    d.refused("age_history", aged, "single setup command")?;
    let first = d
        .sync(&a_token, &format!("?cursor={a_head}&max_changes=4"))
        .await?;
    let target = first.body["next_cursor"].as_str().unwrap().to_owned();
    let control_first = d
        .sync(&b_token, &format!("?cursor={b_head}&max_changes=4"))
        .await?;
    let control = control_first.body["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let a_query = format!("?cursor={target}&max_changes=4");
    let b_query = format!("?cursor={control}&max_changes=4");
    let positive = d.sync(&a_token, &a_query).await?;
    assert_eq!(positive.status, 200, "unpruned: {}", positive.raw);
    assert_eq!(piece(&positive).0, a_rev);
    let control_before = d.sync(&b_token, &b_query).await?;
    assert_eq!(control_before.status, 200, "{}", control_before.raw);

    let collected = d.ok("collect", json!({"time": t}))?;
    let (before, after) = (&collected["before"], &collected["after"]);
    let removed: Vec<_> = collected["removed_batches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            (
                b["account"].as_str().unwrap().to_owned(),
                b["revision"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        removed,
        [(alice.clone(), a_rev)],
        "exactly the aged target batch"
    );
    assert_eq!(floor(before, &alice), 0);
    assert_eq!(
        floor(after, &alice),
        a_rev,
        "the real collector advanced the floor"
    );
    assert_eq!(floor(after, &bob), floor(before, &bob));
    for key in ["sessions", "devices"] {
        assert_eq!(after[key], before[key], "{key} untouched by the collection");
    }
    for key in ["receipts_sha256", "resources_sha256", "revision_clock"] {
        assert_eq!(after[key], before[key], "{key}");
    }

    // Same token at the same time: not expired, session and device live, refused by pruning.
    assert!(expiry(&target) > d.inspect()?["sync_now"].as_i64().unwrap());
    let refused = d.sync(&a_token, &a_query).await?;
    assert_eq!(
        (refused.status, refused.body["code"].as_str()),
        (410, Some("resync_required")),
        "X2: {}",
        refused.raw
    );
    let me = d.call("GET", "/me", Some(&a_token), None, None).await?;
    assert_eq!(me.status, 200, "the refusal is not authentication");
    let control_after = d.sync(&b_token, &b_query).await?;
    assert_eq!(
        control_after.raw, control_before.raw,
        "the unpruned control is unaffected"
    );

    let (ids, _) = d.snapshot(&a_token).await?;
    assert_eq!(ids, a_ids, "recovery holds the whole canonical revision");
    let receipt = d
        .call(
            "POST",
            "/commands",
            Some(&a_token),
            Some(&a_op),
            Some(a_body),
        )
        .await?;
    assert_eq!(receipt.body["revision"].as_i64(), Some(a_rev));
    d.finish(true)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn x2_real_collector_pruning() -> Result<()> {
    x2(false).await
}

#[tokio::test(flavor = "multi_thread")]
async fn x2_real_collector_pruning_browser() -> Result<()> {
    x2(true).await
}

/// R3a: the fixture serves the normal browser-session composition for its configured origin:
/// grant, activation (the only cookie), the reload bootstrap and cookie-authenticated sync, each
/// refusing what the normal server refuses, without issuing a session or a sync write for it.
#[tokio::test(flavor = "multi_thread")]
async fn browser_session_composition() -> Result<()> {
    let mut d = Diagnostic::spawn("browser")?;
    assert_eq!(d.ready["receipt"]["public_origin"], ORIGIN);
    assert_ne!(d.http.base, ORIGIN, "the origin is not the bind address");
    let account = d.add_account("alice")?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let login = |password: &str| {
        json!({"username": "alice", "password": password, "device_id": "browser",
            "attempt_challenge": challenge.as_str()})
    };
    let http = d.http.clone();
    let backend = http.base.clone();
    for headers in [
        vec![],
        vec![("origin", "https://attacker.example")],
        vec![("origin", backend.as_str())],
    ] {
        let r = http
            .raw("POST", "/browser-sessions", &headers, Some(login(PASSWORD)))
            .await?;
        assert_eq!(r.status, 403, "login with {headers:?}: {}", r.raw);
    }
    let wrong = http
        .raw(
            "POST",
            "/browser-sessions",
            &[("origin", ORIGIN)],
            Some(login("wrong-password-123456")),
        )
        .await?;
    assert_eq!(wrong.status, 401, "wrong password: {}", wrong.raw);
    assert!(wrong.body.get("grant").is_none());
    let grant = http
        .raw(
            "POST",
            "/browser-sessions",
            &[("origin", ORIGIN)],
            Some(login(PASSWORD)),
        )
        .await?;
    assert_eq!(grant.status, 200, "browser login: {}", grant.raw);
    assert_eq!(grant.body["account_id"], account);
    assert!(
        !grant.headers.contains_key("set-cookie"),
        "a grant, not a cookie"
    );
    let activate = |verifier: &str| json!({"grant": grant.body["grant"], "verifier": verifier});
    let (_, other) = PkceCodeChallenge::new_random_sha256();
    for (headers, body, status) in [
        (vec![], activate(verifier.secret()), 403),
        (
            vec![("origin", "https://attacker.example")],
            activate(verifier.secret()),
            403,
        ),
        (vec![("origin", ORIGIN)], activate(other.secret()), 401),
    ] {
        let r = http
            .raw("POST", "/browser-sessions/activate", &headers, Some(body))
            .await?;
        assert_eq!(r.status, status, "activation with {headers:?}: {}", r.raw);
        assert!(!r.headers.contains_key("set-cookie"), "{}", r.raw);
    }
    let unissued = d.inspect()?;
    assert_eq!(
        unissued["sessions"],
        json!([]),
        "no refused step issued a session"
    );

    let activated = http
        .raw(
            "POST",
            "/browser-sessions/activate",
            &[("origin", ORIGIN)],
            Some(activate(verifier.secret())),
        )
        .await?;
    assert_eq!(activated.status, 200, "activation: {}", activated.raw);
    let set_cookie = activated.headers["set-cookie"].to_str()?.to_owned();
    // The loopback HTTP origin's cookie: the normal non-Secure name and flags.
    for flag in ["atlas_session=", "HttpOnly", "SameSite=Lax", "Path=/"] {
        assert!(set_cookie.contains(flag), "{set_cookie}");
    }
    assert!(!set_cookie.contains("Secure"), "{set_cookie}");
    let cookie = set_cookie.split(';').next().unwrap().to_owned();
    for headers in [
        vec![("cookie", cookie.as_str())],
        vec![
            ("cookie", cookie.as_str()),
            ("x-atlas-session", "1"),
            ("origin", "https://attacker.example"),
        ],
    ] {
        let r = http
            .raw("GET", "/browser-sessions/current", &headers, None)
            .await?;
        assert_eq!(r.status, 403, "current with {headers:?}: {}", r.raw);
    }
    let current = http
        .raw(
            "GET",
            "/browser-sessions/current",
            &[("cookie", cookie.as_str()), ("x-atlas-session", "1")],
            None,
        )
        .await?;
    assert_eq!(current.status, 200, "current session: {}", current.raw);
    assert_eq!(current.body["account_id"], account);
    assert_eq!(current.body["session_id"], activated.body["session_id"]);
    assert_eq!(current.body["csrf_token"], activated.body["csrf_token"]);
    let csrf = current.body["csrf_token"].as_str().unwrap().to_owned();

    let issued = d.inspect()?;
    assert_eq!(issued["sessions"].as_array().unwrap().len(), 1);
    let wrong_csrf = "0".repeat(64);
    for (headers, code) in [
        (vec![("cookie", cookie.as_str())], "forbidden"),
        (
            vec![
                ("cookie", cookie.as_str()),
                ("x-csrf-token", wrong_csrf.as_str()),
            ],
            "credential_mismatch",
        ),
        (
            vec![
                ("cookie", cookie.as_str()),
                ("x-csrf-token", csrf.as_str()),
                ("origin", "https://attacker.example"),
            ],
            "forbidden",
        ),
    ] {
        let r = http.raw("GET", "/sync", &headers, None).await?;
        assert_eq!(
            (r.status, r.body["code"].as_str()),
            (403, Some(code)),
            "sync with {headers:?}: {}",
            r.raw
        );
    }
    let refused = d.inspect()?;
    assert_eq!(durable(&refused), durable(&issued), "no refused sync wrote");
    assert_eq!(refused["counts"], issued["counts"], "no refused sync wrote");

    let auth = Auth::Cookie { cookie, csrf };
    let (_, head) = d.snapshot(&auth).await?;
    let (revision, _, ids) = d.publish(&auth, &Uuid::new_v4().to_string()).await?;
    let first = d
        .sync(&auth, &format!("?cursor={head}&max_changes=4"))
        .await?;
    assert_eq!(first.status, 200, "{}", first.raw);
    assert_eq!(piece(&first).0, revision);
    let (all, _) = d.snapshot(&auth).await?;
    assert_eq!(all, ids);
    let me = d.call("GET", "/me", Some(&auth), None, None).await?;
    assert_eq!(me.status, 200, "{}", me.raw);
    assert_eq!(me.body["id"], account, "{}", me.raw);
    d.finish(true)?;
    Ok(())
}

fn failed(result: (Option<i32>, Vec<Value>), reason: &str) {
    let (code, lines) = result;
    assert_eq!(code, Some(2), "{lines:?}");
    let error = lines.last().unwrap()["error"].as_str().unwrap().to_owned();
    assert!(error.contains(reason), "{error}");
}

/// S19/S21: refused launches create nothing; refused commands change nothing; a replaced
/// fixture stops the attempt; EOF keeps the fixture; attempts do not cross.
#[tokio::test(flavor = "multi_thread")]
async fn fixture_refusals_and_ownership() -> Result<()> {
    let root = tempfile::tempdir()?;
    let parent = root.path().canonicalize()?;
    let new = parent.join("new");
    let new = new.to_str().unwrap();
    let launch_with = |dir: &str, bind: &str, origin: &str| {
        vec![
            "--fixture-dir".to_owned(),
            dir.to_owned(),
            "--attempt".into(),
            "refusal".into(),
            "--bind".into(),
            bind.to_owned(),
            "--public-origin".into(),
            origin.to_owned(),
        ]
    };
    let launch = |dir: &str, bind: &str| launch_with(dir, bind, ORIGIN);
    let run = |args: Vec<String>, env: &[(&str, &str)]| {
        run_once(&args.iter().map(String::as_str).collect::<Vec<_>>(), env)
    };
    // TEST-NET first: a build that lost the loopback check fails here without listening widely.
    for bind in ["192.0.2.1:0", "0.0.0.0:0", "[::]:0", "localhost:0"] {
        let reason = if bind.starts_with("localhost") {
            "IP:port literal"
        } else {
            "non-loopback"
        };
        failed(run(launch(new, bind), &[])?, reason);
    }
    failed(
        run(
            launch(new, "127.0.0.1:0"),
            &[("ATLAS_DATABASE_URL", "sqlite://x")],
        )?,
        "inherited configuration",
    );
    // The origin is validated exactly as the normal server's, and a refused one leaves nothing.
    for origin in [
        "http://atlas.example",
        "https://atlas.example/path",
        "https://user@atlas.example",
        "not an origin",
    ] {
        failed(
            run(launch_with(new, "127.0.0.1:0", origin), &[])?,
            "--public-origin",
        );
    }
    let mut missing = launch(new, "127.0.0.1:0");
    missing.truncate(6);
    failed(run(missing, &[])?, "usage");
    let mut twice = launch(new, "127.0.0.1:0");
    twice.extend(["--public-origin".into(), ORIGIN.into()]);
    failed(run(twice, &[])?, "given twice");
    failed(run(launch("relative/dir", "127.0.0.1:0"), &[])?, "absolute");
    let existing = parent.join("existing");
    std::fs::create_dir(&existing)?;
    std::fs::write(existing.join("atlas.sqlite"), b"not owned")?;
    failed(
        run(launch(existing.to_str().unwrap(), "127.0.0.1:0"), &[])?,
        "must not exist",
    );
    let link = parent.join("link");
    std::os::unix::fs::symlink(&existing, &link)?;
    failed(
        run(launch(link.to_str().unwrap(), "127.0.0.1:0"), &[])?,
        "must not exist",
    );
    assert_eq!(
        std::fs::read_dir(&existing)?.count(),
        1,
        "a foreign directory is never written"
    );
    let mut bad_attempt = launch(new, "127.0.0.1:0");
    bad_attempt[3] = "no spaces".into();
    failed(run(bad_attempt, &[])?, "attempt id");
    failed(
        run(vec!["--unknown".into(), "x".into()], &[])?,
        "unsupported argument",
    );
    assert!(
        !Path::new(new).exists(),
        "no refused launch created a fixture"
    );

    let mut d = Diagnostic::spawn("refusal")?;
    let (alice, token) = d.login("alice", "phone").await?;
    let (_, head) = d.snapshot(&token).await?;
    let (revision, _, _) = d.publish(&token, &Uuid::new_v4().to_string()).await?;
    let state = d.inspect()?;
    let digest = batch(&state, &alice, revision).unwrap()["payload_sha256"].clone();
    let aged = |created_at: i64, digest: &Value| {
        json!({"batches": [{"account": alice, "revision": revision, "created_at": created_at,
            "payload_sha256": digest}]})
    };
    let t = d.sync_start();
    let attempt = d.attempt.clone();
    let rejected = |answer: Value| {
        assert_eq!(answer["event"], "rejected", "{answer}");
    };
    rejected(d.line("not json")?);
    rejected(d.line(
        &json!({"attempt": attempt, "seq": d.seq + 1, "op": "inspect", "extra": 1}).to_string(),
    )?);
    rejected(
        d.line(&json!({"attempt": "another", "seq": d.seq + 1, "op": "inspect"}).to_string())?,
    );
    rejected(d.line(&json!({"attempt": attempt, "seq": d.seq, "op": "inspect"}).to_string())?);
    rejected(d.line(&json!({"attempt": attempt, "seq": d.seq + 2, "op": "inspect"}).to_string())?);
    d.refused("format_disk", Value::Null, "unsupported command")?;
    d.refused(
        "inspect",
        json!({"sql": "DELETE FROM sync_batches"}),
        "malformed arguments",
    )?;
    d.refused(
        "age_history",
        aged(t - RETENTION, &json!("0".repeat(64))),
        "payload differs",
    )?;
    d.refused("age_history", aged(t - RETENTION - 1, &digest), "outside")?;
    d.refused("age_history", aged(wall() + 10, &digest), "outside")?;
    d.refused("advance_clock", json!({"time": t}), "only advances")?;
    d.refused(
        "advance_clock",
        json!({"time": t + 2 * RETENTION + 1}),
        "only advances",
    )?;
    d.refused("collect", json!({"time": t - 1}), "outside")?;
    d.refused("collect", json!({"time": t + 1}), "outside")?;
    assert_eq!(d.inspect()?, state, "refused commands changed nothing");
    d.ok("collect", json!({"time": t}))?;
    d.refused(
        "age_history",
        aged(t - RETENTION, &digest),
        "single setup command",
    )?;
    d.refused(
        "add_account",
        json!({"username": "late", "password": PASSWORD}),
        "setup command",
    )?;
    let still = d.sync(&token, &format!("?cursor={head}")).await?;
    assert_eq!(
        still.status, 200,
        "nothing was aged or collected: {}",
        still.raw
    );

    // Another attempt's commands never reach this one.
    let mut other = Diagnostic::spawn("other")?;
    assert_ne!(
        other.ready["receipt"]["database"]["ino"],
        d.ready["receipt"]["database"]["ino"]
    );
    rejected(other.line(&json!({"attempt": d.attempt, "seq": 1, "op": "inspect"}).to_string())?);
    other.finish(true)?;

    // A replaced database stops the attempt before the command and keeps what is there.
    let database = d.database();
    std::fs::rename(&database, d.dir.join("moved.sqlite"))?;
    std::fs::write(&database, b"")?;
    let answer = d.command("advance_clock", json!({"time": t + 1}))?;
    lost_answer(&answer, "none");
    assert!(
        answer["error"]
            .as_str()
            .is_some_and(|e| e.contains("fixture database replaced")),
        "{answer}"
    );
    assert_eq!(answer["sync_now"], t, "{answer}");
    d.stops_lost("fixture database replaced")?;
    assert!(d.dir.join("moved.sqlite").exists() && database.exists());

    // A changed receipt refuses the inspection itself: no owned state is acknowledged.
    let mut r = Diagnostic::spawn("receipt")?;
    std::fs::write(r.dir.join("receipt.json"), b"{}")?;
    let answer = r.command("inspect", Value::Null)?;
    lost_answer(&answer, "none");
    assert!(
        answer["error"]
            .as_str()
            .is_some_and(|e| e.contains("fixture receipt changed")),
        "{answer}"
    );
    r.stops_lost("fixture receipt changed")?;
    assert_eq!(std::fs::read(r.dir.join("receipt.json"))?, b"{}");

    // `shutdown` with `remove` after a replacement nobody observed yet deletes nothing.
    let mut s = Diagnostic::spawn("remove-lost")?;
    let database = s.database();
    std::fs::rename(&database, s.dir.join("moved.sqlite"))?;
    std::fs::write(&database, b"replacement")?;
    let stopped = s.command("shutdown", json!({"remove": true}))?;
    assert_eq!(stopped["event"], "stopped", "{stopped}");
    assert!(
        stopped["cleanup_error"]
            .as_str()
            .is_some_and(|e| e.contains("fixture database replaced")),
        "a replaced fixture is not removed: {stopped}"
    );
    assert_eq!(exit_code(&mut s.child)?, Some(2));
    assert_eq!(std::fs::read(&database)?, b"replacement");
    assert!(s.dir.join("moved.sqlite").exists() && s.dir.join("receipt.json").exists());

    // An over-long line stops the attempt; EOF stops it too, and both keep the fixture.
    let mut long = Diagnostic::spawn("long")?;
    let answer = long.line(&"x".repeat(9000))?;
    assert_eq!(answer["stopping"], true, "{answer}");
    assert_eq!(exit_code(&mut long.child)?, Some(0));
    assert!(long.dir.join("fixture.sqlite").exists());
    let mut eof = Diagnostic::spawn("eof")?;
    drop(eof.stdin.take());
    let stopped = eof.next()?;
    assert_eq!(stopped["removed"], Value::Null, "{stopped}");
    assert_eq!(stopped["cleanup_error"], Value::Null, "{stopped}");
    assert_eq!(exit_code(&mut eof.child)?, Some(0));
    assert!(eof.dir.join("receipt.json").exists());
    Ok(())
}

/// R3b, HTTP admission (S19): with stdin left idle, a request arriving after the receipt,
/// database or directory is moved, replaced or turned into a symbolic link is refused before any
/// handler runs, and that refusal alone stops the attempt without touching what is there now.
/// Each form has its healthy control first.
#[tokio::test(flavor = "multi_thread")]
async fn ownership_http_admission() -> Result<()> {
    for form in [
        "receipt-moved",
        "receipt-symlink",
        "database-replaced",
        "directory-replaced",
    ] {
        let mut d = Diagnostic::spawn(form)?;
        let (account, token) = d.login("alice", "phone").await?;
        let healthy = d.call("GET", "/me", Some(&token), None, None).await?;
        assert_eq!(healthy.status, 200, "{form} control: {}", healthy.raw);
        d.inspect()?;
        let (receipt, database) = (d.dir.join("receipt.json"), d.database());
        let reason = match form {
            "receipt-moved" => {
                std::fs::rename(&receipt, d.dir.join("receipt.moved"))?;
                "receipt.json"
            }
            "receipt-symlink" => {
                // The same bytes, but no longer the receipt file the fixture wrote.
                std::fs::rename(&receipt, d.dir.join("receipt.moved"))?;
                std::os::unix::fs::symlink(d.dir.join("receipt.moved"), &receipt)?;
                "receipt.json"
            }
            "database-replaced" => {
                std::fs::rename(&database, d.dir.join("moved.sqlite"))?;
                std::fs::write(&database, b"replacement")?;
                "fixture database replaced"
            }
            _ => {
                std::fs::rename(&d.dir, d.dir.with_extension("moved"))?;
                std::fs::create_dir(&d.dir)?;
                std::fs::write(&database, b"replacement")?;
                "fixture directory replaced"
            }
        };
        // A writer: it would publish a revision for the original account if admitted.
        let (body, _) = person_commands();
        let refused = d
            .call(
                "POST",
                "/commands",
                Some(&token),
                Some(&Uuid::new_v4().to_string()),
                Some(body),
            )
            .await?;
        assert_eq!(
            (refused.status, refused.body["code"].as_str()),
            (503, Some("diagnostic_fixture_unavailable")),
            "{form}: {}",
            refused.raw
        );
        d.stops_lost(reason)?;
        // Nothing was written to either database, and what is there now was left in place.
        let moved = d.dir.with_extension("moved");
        let (main, wal) = match form {
            "database-replaced" => (d.dir.join("moved.sqlite"), d.dir.join("fixture.sqlite-wal")),
            "directory-replaced" => (
                moved.join("fixture.sqlite"),
                moved.join("fixture.sqlite-wal"),
            ),
            _ => (database.clone(), d.dir.join("fixture.sqlite-wal")),
        };
        let (_copy, original) = paired_copy(&main, &wal)?;
        let found = scalar(
            &original,
            &format!("SELECT COUNT(*) FROM accounts WHERE id='{account}'"),
        )
        .await?;
        assert_eq!(found, 1, "{form}: the read sees the original database");
        let published = scalar(
            &original,
            &format!("SELECT COUNT(*) FROM resources WHERE owner_id='{account}'"),
        )
        .await?;
        assert_eq!(published, 0, "{form}: the refused writer published");
        if form.ends_with("replaced") {
            assert_eq!(std::fs::read(&database)?, b"replacement", "{form}");
        }
        assert!(d.dir.exists(), "{form}: never cleaned up");
    }
    Ok(())
}

/// R3b, admission after waiting: a check made before an awaited wait is not a permit. With
/// `age_history` holding the gate exclusively and parked on the SQLite write lock, a request that
/// passed its arrival check queues for the gate; then the receipt changes. A new request is refused
/// on arrival at once (while the gate is still held), the queued one after its wait, and
/// `age_history`, rechecked once it has the write lock, changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn ownership_rechecked_after_waits() -> Result<()> {
    let mut d = Diagnostic::spawn("after-wait")?;
    let t = d.sync_start();
    let (alice, token) = d.login("alice", "phone").await?;
    let (revision, _, _) = d.publish(&token, &Uuid::new_v4().to_string()).await?;
    let state = d.inspect()?;
    let created = batch(&state, &alice, revision).unwrap()["created_at"].clone();
    let aged = json!({"batches": [{"account": alice, "revision": revision, "created_at": t - RETENTION,
        "payload_sha256": batch(&state, &alice, revision).unwrap()["payload_sha256"]}]});
    let writer = Writer::hold(&d.database()).await?;
    let seq = d.send("age_history", aged)?;
    d.wait_control(seq, "admitted")?;
    let (http, queued_auth) = (d.http.clone(), token.clone());
    let queued = tokio::spawn(async move {
        http.call("GET", "/me", Some(&queued_auth), None, None)
            .await
    });
    d.wait_http("queued", "/me", 1)?;
    std::fs::write(d.dir.join("receipt.json"), b"{}")?;
    // Bounded well inside SQLite's 10 s busy timeout: only a refusal on arrival can answer while
    // the gate is still held.
    let arrival = tokio::time::timeout(
        Duration::from_secs(5),
        d.call("GET", "/me", Some(&token), None, None),
    )
    .await
    .map_err(|_| anyhow!("not refused on arrival while the gate was held"))??;
    assert_eq!(arrival.status, 503, "refused on arrival: {}", arrival.raw);
    assert!(
        !queued.is_finished(),
        "the queued request waited for the gate"
    );
    assert!(!d.pending(), "age_history still waits for the write lock");
    writer.release().await?;
    let queued = queued.await??;
    assert_eq!(queued.status, 503, "refused after its wait: {}", queued.raw);
    let answer = d.next()?;
    assert_eq!(answer["seq"], seq, "{answer}");
    lost_answer(&answer, "none");
    d.stops_lost("fixture receipt changed")?;
    let after = scalar(
        &d.database(),
        &format!(
            "SELECT created_at FROM sync_batches WHERE account_id='{alice}' AND revision={revision}"
        ),
    )
    .await?;
    assert_eq!(json!(after), created, "age_history changed nothing");
    Ok(())
}

/// R3b, terminal loss and commands queued behind an admitted request. A `/sync` parked on the
/// SQLite write lock holds the gate shared; a command queues for it exclusively; ownership is then
/// lost. Once released, the command (rechecked after its wait) refuses with no effect. For
/// `advance_clock` an HTTP request observes the loss and the receipt is then restored: the loss
/// stays terminal.
#[tokio::test(flavor = "multi_thread")]
async fn ownership_loss_while_queued() -> Result<()> {
    for op in ["advance_clock", "add_account", "collect", "inspect"] {
        let mut d = Diagnostic::spawn(&op.replace('_', "-"))?;
        let t = d.sync_start();
        let (alice, token) = d.login("alice", "phone").await?;
        let (revision, _, _) = d.publish(&token, &Uuid::new_v4().to_string()).await?;
        if op == "collect" {
            // Something the collector would really remove if it ran.
            let state = d.inspect()?;
            d.ok(
                "age_history",
                json!({"batches": [{"account": alice, "revision": revision, "created_at": t - RETENTION,
                    "payload_sha256": batch(&state, &alice, revision).unwrap()["payload_sha256"]}]}),
            )?;
        }
        let writer = Writer::hold(&d.database()).await?;
        let (http, held_auth) = (d.http.clone(), token.clone());
        let held = tokio::spawn(async move {
            http.call("GET", "/sync", Some(&held_auth), None, None)
                .await
        });
        d.wait_http("admitted", "/sync", 1)?;
        let args = match op {
            "advance_clock" => json!({"time": t + 1}),
            "add_account" => json!({"username": "late", "password": PASSWORD}),
            "collect" => json!({"time": t}),
            _ => Value::Null,
        };
        let seq = d.send(op, args)?;
        d.wait_control(seq, "waiting")?;
        let receipt = d.dir.join("receipt.json");
        let original = std::fs::read(&receipt)?;
        std::fs::write(&receipt, b"{}")?;
        if op == "advance_clock" {
            let observed = tokio::time::timeout(
                Duration::from_secs(5),
                d.call("GET", "/me", Some(&token), None, None),
            )
            .await
            .map_err(|_| anyhow!("the loss is observed over HTTP: no answer"))??;
            assert_eq!(
                observed.status, 503,
                "the loss is observed over HTTP: {}",
                observed.raw
            );
            std::fs::write(&receipt, &original)?;
        }
        assert!(!d.pending(), "{op} waits for the admitted /sync");
        writer.release().await?;
        // Admitted before the loss: it completes. Its effects are not attributed to the attempt.
        assert_eq!(held.await??.status, 200);
        let answer = d.next()?;
        assert_eq!(answer["seq"], seq, "{answer}");
        lost_answer(&answer, "none");
        assert_eq!(answer["sync_now"], t, "{op}: the sync time did not move");
        d.stops_lost("fixture receipt changed")?;
        let database = d.database();
        assert_eq!(
            scalar(&database, "SELECT COUNT(*) FROM accounts").await?,
            1,
            "{op}"
        );
        assert_eq!(
            scalar(&database, &format!("SELECT COUNT(*) FROM sync_batches WHERE account_id='{alice}' AND revision={revision}")).await?,
            1,
            "{op}: nothing was collected"
        );
        assert_eq!(
            scalar(&database, "SELECT MAX(sync_floor) FROM accounts").await?,
            0,
            "{op}"
        );
    }
    Ok(())
}

/// Invariant 4: a command's exclusive hold waits for every admitted HTTP request, not only
/// `/sync`. A held `/sync` keeps its old time and `advance_clock` is acknowledged only after it;
/// a held publication is in the state `collect` reports before collecting.
#[tokio::test(flavor = "multi_thread")]
async fn quiescent_boundaries() -> Result<()> {
    let mut d = Diagnostic::spawn("quiescent")?;
    let t = d.sync_start();
    let (_, token) = d.login("alice", "phone").await?;

    let writer = Writer::hold(&d.database()).await?;
    let (http, auth) = (d.http.clone(), token.clone());
    let held =
        tokio::spawn(async move { http.call("GET", "/sync", Some(&auth), None, None).await });
    d.wait_http("admitted", "/sync", 1)?;
    let seq = d.send("advance_clock", json!({"time": t + 1}))?;
    d.wait_control(seq, "waiting")?;
    assert!(!d.pending(), "advance_clock waits for the admitted /sync");
    writer.release().await?;
    let response = held.await??;
    assert_eq!(response.status, 200, "{}", response.raw);
    assert_eq!(
        cursor_expiry(response.body["next_cursor"].as_str().unwrap()),
        t + RETENTION,
        "the held /sync kept the old time"
    );
    let answer = d.next()?;
    assert_eq!(
        (answer["seq"].as_u64(), &answer["ok"]),
        (Some(seq), &json!(true)),
        "{answer}"
    );
    assert_eq!(answer["result"]["to"], t + 1);

    let before = d.inspect()?["revision_clock"].as_i64().unwrap();
    let writer = Writer::hold(&d.database()).await?;
    let (http, auth) = (d.http.clone(), token.clone());
    let (body, _) = person_commands();
    let operation = Uuid::new_v4().to_string();
    let publication = tokio::spawn(async move {
        http.call(
            "POST",
            "/commands",
            Some(&auth),
            Some(&operation),
            Some(body),
        )
        .await
    });
    d.wait_http("admitted", "/commands", 1)?;
    let seq = d.send("collect", json!({"time": t + 1}))?;
    d.wait_control(seq, "waiting")?;
    assert!(!d.pending(), "collect waits for the admitted publication");
    writer.release().await?;
    let published = publication.await??;
    assert_eq!(published.status, 200, "{}", published.raw);
    let revision = published.body["revision"].as_i64().unwrap();
    assert!(revision > before);
    let answer = d.next()?;
    assert_eq!(
        (answer["seq"].as_u64(), &answer["ok"]),
        (Some(seq), &json!(true)),
        "{answer}"
    );
    assert_eq!(
        answer["result"]["before"]["revision_clock"], revision,
        "collect's before-state includes the admitted publication"
    );
    d.finish(true)?;
    Ok(())
}

/// S08 partial outcome: the real collector commits its earlier units (expired sessions) and then
/// fails in the history unit (a disposable trigger in this fixture's database aborts its delete).
/// The answer is a failure with both states: sessions gone, history and floor unchanged. Without
/// the trigger, the healthy collector then prunes the batch and advances the floor.
#[tokio::test(flavor = "multi_thread")]
async fn collector_partial_outcome() -> Result<()> {
    let mut d = Diagnostic::spawn("partial")?;
    let t = d.sync_start();
    let (alice, token) = d.login("alice", "phone").await?;
    let (revision, _, _) = d.publish(&token, &Uuid::new_v4().to_string()).await?;
    let state = d.inspect()?;
    d.ok(
        "age_history",
        json!({"batches": [{"account": alice, "revision": revision, "created_at": t - RETENTION,
            "payload_sha256": batch(&state, &alice, revision).unwrap()["payload_sha256"]}]}),
    )?;
    let expires = state["sessions"][0]["expires_at"].as_i64().unwrap();
    d.ok("advance_clock", json!({"time": expires}))?;
    let mut connection = connect(&d.database(), "rw").await?;
    sqlx::raw_sql("CREATE TRIGGER diagnostic_fault BEFORE DELETE ON sync_batches BEGIN SELECT RAISE(ABORT,'disposable history fault'); END")
        .execute(&mut connection)
        .await?;
    let answer = d.command("collect", json!({"time": expires}))?;
    assert_eq!(
        answer["ok"], false,
        "the faulty collection failed: {answer}"
    );
    assert_eq!(
        answer["effect"], "partial",
        "effect of a failed collection: {answer}"
    );
    assert!(
        answer.get("partial").is_some(),
        "the partial outcome is reported: {answer}"
    );
    assert!(
        answer["error"]
            .as_str()
            .unwrap()
            .contains("collector failed"),
        "{answer}"
    );
    let (before, after) = (&answer["partial"]["before"], &answer["partial"]["after"]);
    assert_eq!(before["sessions"].as_array().unwrap().len(), 1, "{answer}");
    assert_eq!(after["sessions"], json!([]), "the earlier unit committed");
    assert_eq!(
        after["batches"], before["batches"],
        "the history unit rolled back"
    );
    assert_eq!(floor(after, &alice), 0, "the floor rolled back with it");
    assert_eq!(answer["partial"]["removed_batches"], json!([]));
    sqlx::raw_sql("DROP TRIGGER diagnostic_fault")
        .execute(&mut connection)
        .await?;
    connection.close().await?;
    let healthy = d.ok("collect", json!({"time": expires}))?;
    assert_eq!(
        healthy["removed_batches"].as_array().unwrap().len(),
        1,
        "{healthy}"
    );
    assert_eq!(
        floor(&healthy["after"], &alice),
        revision,
        "the healthy collector advanced the floor"
    );
    assert_eq!(
        healthy["before"]["receipts_sha256"],
        healthy["after"]["receipts_sha256"]
    );
    assert_eq!(
        healthy["before"]["resources_sha256"],
        healthy["after"]["resources_sha256"]
    );
    d.finish(true)?;
    Ok(())
}

/// R3b, a loss while an admitted command runs: `collect` has passed its checks and holds the gate
/// when the receipt changes; the real collector, parked on the SQLite write lock, then runs.
/// Its answer is not an owned success, and it reports the effect it really had.
#[tokio::test(flavor = "multi_thread")]
async fn ownership_lost_during_command() -> Result<()> {
    let mut d = Diagnostic::spawn("during")?;
    let t = d.sync_start();
    let (alice, token) = d.login("alice", "phone").await?;
    let (revision, _, _) = d.publish(&token, &Uuid::new_v4().to_string()).await?;
    let state = d.inspect()?;
    d.ok(
        "age_history",
        json!({"batches": [{"account": alice, "revision": revision, "created_at": t - RETENTION,
            "payload_sha256": batch(&state, &alice, revision).unwrap()["payload_sha256"]}]}),
    )?;
    let writer = Writer::hold(&d.database()).await?;
    let seq = d.send("collect", json!({"time": t}))?;
    d.wait_control(seq, "admitted")?;
    std::fs::write(d.dir.join("receipt.json"), b"{}")?;
    assert!(!d.pending(), "collect waits for the write lock");
    writer.release().await?;
    let answer = d.next()?;
    assert_eq!(answer["seq"], seq, "{answer}");
    lost_answer(&answer, "applied");
    d.stops_lost("fixture receipt changed")?;
    let left = scalar(
        &d.database(),
        &format!(
            "SELECT COUNT(*) FROM sync_batches WHERE account_id='{alice}' AND revision={revision}"
        ),
    )
    .await?;
    assert_eq!(left, 0, "the collection really happened, and is reported");
    Ok(())
}
