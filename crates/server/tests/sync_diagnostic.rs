//! BE-Q23 R3: the `atlas-sync-diagnostic` fixture, driven as its owner would drive it: a real
//! process, real HTTP and its private stdin/stdout channel. X0 is real continuation expiry, X2 real
//! pruning by `Store::collect_expired`; the rest are its refusals and ownership checks.
//!
//! Runs only with `--features sync-diagnostic`. `SYNC_DIAGNOSTIC_BIN` selects a separately built
//! executable instead of Cargo's; `SYNC_DIAGNOSTIC_EVIDENCE` names a directory that receives every
//! control line and HTTP exchange (with body digests) as JSON lines.
use anyhow::{Result, anyhow, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};
use uuid::Uuid;

const V1: &str = "/api/experimental/v1";
const PASSWORD: &str = "local-test-password-123";
const RETENTION: i64 = 90 * 86400;

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

struct Response {
    status: u16,
    body: Value,
    raw: String,
}

struct Diagnostic {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    attempt: String,
    seq: u64,
    base: String,
    dir: PathBuf,
    log: PathBuf,
    ready: Value,
    evidence: Option<PathBuf>,
    http: reqwest::Client,
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
            .args(["--bind", "127.0.0.1:0"])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&log)?)
            .spawn()?;
        let stdin = child.stdin.take();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line)?;
        let ready: Value =
            serde_json::from_str(&line).map_err(|e| anyhow!("no ready line ({e}): {line:?}"))?;
        ensure!(ready["event"] == "ready", "{ready}");
        let evidence = std::env::var_os("SYNC_DIAGNOSTIC_EVIDENCE").map(PathBuf::from);
        let d = Self {
            child,
            stdin,
            stdout,
            seq: 0,
            base: format!("http://{}", ready["receipt"]["address"].as_str().unwrap()),
            attempt,
            dir,
            log,
            ready: ready.clone(),
            evidence,
            http: reqwest::Client::new(),
            _root: root,
        };
        d.record(json!({"ready": ready}))?;
        Ok(d)
    }

    fn record(&self, entry: Value) -> Result<()> {
        if let Some(dir) = &self.evidence {
            std::fs::create_dir_all(dir)?;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(format!("{}.jsonl", self.attempt)))?;
            writeln!(file, "{entry}")?;
        }
        Ok(())
    }

    fn sync_start(&self) -> i64 {
        self.ready["receipt"]["sync_start"].as_i64().unwrap()
    }

    /// Sends one raw line and reads the one line it answers.
    fn line(&mut self, line: &str) -> Result<Value> {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        let mut answer = String::new();
        self.stdout.read_line(&mut answer)?;
        let answer: Value = serde_json::from_str(&answer)
            .map_err(|e| anyhow!("unreadable answer ({e}): {answer:?}"))?;
        self.record(json!({"sent": line, "answer": answer}))?;
        Ok(answer)
    }

    /// The next command in sequence; returns its whole answer.
    fn command(&mut self, op: &str, args: Value) -> Result<Value> {
        self.seq += 1;
        let line = json!({"attempt": self.attempt, "seq": self.seq, "op": op, "args": args});
        self.line(&line.to_string())
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

    async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        operation: Option<&str>,
        body: Option<Value>,
    ) -> Result<Response> {
        let mut request = self
            .http
            .request(method.parse()?, format!("{}{V1}{path}", self.base));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(operation) = operation {
            request = request.header("idempotency-key", operation);
        }
        if let Some(body) = &body {
            request = request.json(body);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let raw = response.text().await?;
        let parsed = if raw.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&raw)?
        };
        self.record(json!({"http": {"method": method, "path": path, "operation": operation,
            "request": body, "status": status, "body_sha256": sha256(raw.as_bytes()), "body": parsed}}))?;
        Ok(Response {
            status,
            body: parsed,
            raw,
        })
    }

    async fn login(&mut self, name: &str, device: &str) -> Result<(String, String)> {
        let account = self.ok(
            "add_account",
            json!({"username": name, "password": PASSWORD}),
        )?["account"]
            .as_str()
            .unwrap()
            .to_owned();
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
        Ok((account, r.body["access_token"].as_str().unwrap().to_owned()))
    }

    async fn sync(&self, token: &str, query: &str) -> Result<Response> {
        self.call("GET", &format!("/sync{query}"), Some(token), None, None)
            .await
    }

    /// Every page from no cursor to the live edge; returns the resource IDs and the final cursor.
    async fn snapshot(&self, token: &str) -> Result<(Vec<String>, String)> {
        let mut page = self.sync(token, "").await?;
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
            page = self.sync(token, &format!("?cursor={cursor}")).await?;
        }
    }

    /// One person with twelve fields in one revision: four bounded pieces at `max_changes=4`.
    async fn publish(&self, token: &str, operation: &str) -> Result<(i64, Value, Vec<String>)> {
        let person = Uuid::new_v4().to_string();
        let mut ids = vec![person.clone()];
        let mut commands =
            vec![json!({"kind": "create_person", "id": person, "name": "Diagnostic"})];
        for _ in 0..12 {
            let id = Uuid::new_v4().to_string();
            commands.push(
                json!({"kind": "create_field", "id": id, "person_id": person,
                "label": "Field", "value": "Value"}),
            );
            ids.push(id);
        }
        let body = json!({"commands": commands});
        let r = self
            .call(
                "POST",
                "/commands",
                Some(token),
                Some(operation),
                Some(body.clone()),
            )
            .await?;
        ensure!(r.status == 200, "publish: {}", r.raw);
        ids.sort();
        Ok((r.body["revision"].as_i64().unwrap(), body, ids))
    }

    /// Ends the attempt with `shutdown` and waits for the process.
    fn finish(mut self, remove: bool) -> Result<Value> {
        let answer = self.command("shutdown", json!({"remove": remove}))?;
        assert_eq!(answer["event"], "stopped", "{answer}");
        assert_eq!(answer["seq"], self.seq, "{answer}");
        drop(self.stdin.take());
        let status = self.child.wait()?;
        assert!(status.success(), "{status}");
        assert_eq!(self.dir.exists(), !remove);
        Ok(answer)
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
        let _ = self.child.wait();
    }
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
#[tokio::test(flavor = "multi_thread")]
async fn x0_real_continuation_expiry() -> Result<()> {
    let mut d = Diagnostic::spawn("x0")?;
    let t = d.sync_start();
    let (alice, token) = d.login("alice", "phone").await?;
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

/// X2 (S07): with declared history aged before the target continuation is issued, the real
/// collector at the fixture's own time prunes exactly that batch and advances the floor, and the
/// same token, still unexpired, is then refused. Another account's unpruned chain still succeeds;
/// sessions, devices, receipts and canonical content are unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn x2_real_collector_pruning() -> Result<()> {
    let mut d = Diagnostic::spawn("x2")?;
    let t = d.sync_start();
    let (alice, a_token) = d.login("alice", "phone").await?;
    let (bob, b_token) = d.login("bob", "phone").await?;
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
    let launch = |dir: &str, bind: &str| {
        vec![
            "--fixture-dir".to_owned(),
            dir.to_owned(),
            "--attempt".into(),
            "refusal".into(),
            "--bind".into(),
            bind.to_owned(),
        ]
    };
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
    let database = d.dir.join("fixture.sqlite");
    std::fs::rename(&database, d.dir.join("moved.sqlite"))?;
    std::fs::write(&database, b"")?;
    d.refused(
        "advance_clock",
        json!({"time": t + 1}),
        "fixture database replaced",
    )?;
    let mut lost = String::new();
    d.stdout.read_line(&mut lost)?;
    assert!(lost.contains("ownership_lost"), "{lost}");
    let mut stopped = String::new();
    d.stdout.read_line(&mut stopped)?;
    assert!(stopped.contains("cleanup_error"), "{stopped}");
    assert_eq!(d.child.wait()?.code(), Some(2));
    assert!(d.dir.join("moved.sqlite").exists() && database.exists());

    // A changed receipt stops the attempt at its next command boundary.
    let mut r = Diagnostic::spawn("receipt")?;
    std::fs::write(r.dir.join("receipt.json"), b"{}")?;
    r.command("inspect", Value::Null)?;
    let mut line = String::new();
    r.stdout.read_line(&mut line)?;
    assert!(line.contains("fixture receipt changed"), "{line}");
    assert_eq!(r.child.wait()?.code(), Some(2));

    // An over-long line stops the attempt; EOF stops it too, and both keep the fixture.
    let mut long = Diagnostic::spawn("long")?;
    let answer = long.line(&"x".repeat(9000))?;
    assert_eq!(answer["stopping"], true, "{answer}");
    assert_eq!(long.child.wait()?.code(), Some(0));
    assert!(long.dir.join("fixture.sqlite").exists());
    let mut eof = Diagnostic::spawn("eof")?;
    drop(eof.stdin.take());
    let mut stopped = String::new();
    eof.stdout.read_line(&mut stopped)?;
    assert!(stopped.contains("\"removed\":null"), "{stopped}");
    assert_eq!(eof.child.wait()?.code(), Some(0));
    assert!(eof.dir.join("receipt.json").exists());
    Ok(())
}
