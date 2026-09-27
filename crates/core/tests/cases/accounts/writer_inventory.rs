//! (aj) Writer-inventory guard.
//!
//! The retirement is correct only against the writers of an approved-state member that it knows
//! about. This test lists every source statement that inserts into, updates or deletes one of those
//! tables and fails unless each appears below with its coordination. Adding a writer, moving one
//! into or out of `begin_serial()`, or removing one forces this table, and a decision about how the
//! new writer is ordered against a retirement, to be updated. `activation_grants` (BE-Q19) has no
//! writers yet; the day it does, this test fails until they are listed and shown to run inside
//! `begin_serial()` as that work is required to do.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const TABLES: [&str; 6] = [
    "sessions",
    "native_handoffs",
    "notification_subscriptions",
    "activation_grants",
    "sync_devices",
    // The retirement's generic identity delete interpolates a fixed table name.
    "{table}",
];
const VERBS: [&str; 3] = ["INSERT INTO", "UPDATE", "DELETE FROM"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Coordination {
    /// Runs inside `begin_serial()`/`begin_retirement()` (or a transaction handed in by a caller
    /// that does), so it is ordered against a retirement by `sync_clock`.
    Serial,
    /// One autocommit statement, ordered against a retirement by the row lock (PostgreSQL) or the
    /// write lock (SQLite). Never inside `begin_serial()`.
    RowLock,
    /// Its own transaction with its own ordering point, named in `needs`.
    Own,
}
use Coordination::*;

struct Entry {
    file: &'static str,
    function: &'static str,
    verb: &'static str,
    table: &'static str,
    count: usize,
    coordination: Coordination,
    /// Text the function must still contain for its ordering argument to hold.
    needs: Option<&'static str>,
}

const fn entry(
    file: &'static str,
    function: &'static str,
    verb: &'static str,
    table: &'static str,
    count: usize,
    coordination: Coordination,
    needs: Option<&'static str>,
) -> Entry {
    Entry {
        file,
        function,
        verb,
        table,
        count,
        coordination,
        needs,
    }
}

/// The inventory at the base of BE-Q11-B7 plus this task's own writers.
fn inventory() -> Vec<Entry> {
    vec![
        // --- this task's writers
        entry(
            "core/src/devices.rs",
            "retire_members",
            "DELETE FROM",
            "sync_devices",
            1,
            Serial,
            None,
        ),
        entry(
            "core/src/devices.rs",
            "retire_members",
            "UPDATE",
            "notification_subscriptions",
            1,
            Serial,
            None,
        ),
        entry(
            "core/src/devices.rs",
            "delete_identities",
            "DELETE FROM",
            "{table}",
            1,
            Serial,
            None,
        ),
        entry(
            "core/src/devices.rs",
            "revoke_once",
            "DELETE FROM",
            "sessions",
            1,
            Serial,
            None,
        ),
        // --- notification subscriptions (set, replace, remove)
        entry(
            "core/src/calendars/reminders.rs",
            "reminder_command",
            "INSERT INTO",
            "notification_subscriptions",
            1,
            Serial,
            None,
        ),
        entry(
            "core/src/calendars/reminders.rs",
            "reminder_command",
            "UPDATE",
            "notification_subscriptions",
            2,
            Serial,
            None,
        ),
        // --- restore preparation: servers are stopped, and it holds the gate anyway
        entry(
            "core/src/storage/mod.rs",
            "prepare_restored_database",
            "DELETE FROM",
            "native_handoffs",
            1,
            Serial,
            None,
        ),
        entry(
            "core/src/storage/mod.rs",
            "prepare_restored_database",
            "DELETE FROM",
            "sessions",
            1,
            Serial,
            None,
        ),
        entry(
            "core/src/storage/mod.rs",
            "prepare_restored_database",
            "DELETE FROM",
            "sync_devices",
            1,
            Serial,
            None,
        ),
        // --- expiry sweeps and the retention cleanup: autocommit chunks and a per-device transaction
        entry(
            "core/src/sync.rs",
            "collect_expired",
            "DELETE FROM",
            "native_handoffs",
            1,
            RowLock,
            None,
        ),
        entry(
            "core/src/sync.rs",
            "collect_expired",
            "DELETE FROM",
            "sessions",
            1,
            RowLock,
            None,
        ),
        entry(
            "core/src/sync.rs",
            "collect_expired",
            "DELETE FROM",
            "sync_devices",
            1,
            Own,
            Some("FOR UPDATE"),
        ),
        // --- sync registration: creation (per-account update) and the `last_seen` row update
        entry(
            "core/src/sync.rs",
            "sync_once",
            "INSERT INTO",
            "sync_devices",
            1,
            Own,
            Some("UPDATE accounts SET access_epoch=access_epoch"),
        ),
        entry(
            "core/src/sync.rs",
            "sync_once",
            "UPDATE",
            "sync_devices",
            1,
            Own,
            Some("UPDATE accounts SET access_epoch=access_epoch"),
        ),
        // --- the two session revocations that take no lock beyond their own row
        entry(
            "server/src/lib.rs",
            "logout",
            "DELETE FROM",
            "sessions",
            1,
            RowLock,
            None,
        ),
        entry(
            "server/src/sessions.rs",
            "revoke",
            "DELETE FROM",
            "sessions",
            1,
            RowLock,
            None,
        ),
        // --- everything else in the server runs inside begin_serial()
        entry(
            "server/src/oidc.rs",
            "callback",
            "DELETE FROM",
            "native_handoffs",
            1,
            Serial,
            None,
        ),
        entry(
            "server/src/oidc.rs",
            "callback",
            "INSERT INTO",
            "native_handoffs",
            1,
            Serial,
            None,
        ),
        entry(
            "server/src/oidc.rs",
            "native_exchange",
            "DELETE FROM",
            "native_handoffs",
            1,
            Serial,
            None,
        ),
        entry(
            "server/src/sessions.rs",
            "change_password",
            "DELETE FROM",
            "native_handoffs",
            1,
            Serial,
            None,
        ),
        entry(
            "server/src/sessions.rs",
            "change_password",
            "DELETE FROM",
            "sessions",
            1,
            Serial,
            None,
        ),
        entry(
            "server/src/sessions.rs",
            "issue",
            "DELETE FROM",
            "sessions",
            1,
            Serial,
            None,
        ),
        entry(
            "server/src/sessions.rs",
            "issue",
            "INSERT INTO",
            "sessions",
            1,
            Serial,
            None,
        ),
        entry(
            "server/src/sessions.rs",
            "reset_password",
            "DELETE FROM",
            "native_handoffs",
            1,
            Serial,
            None,
        ),
        entry(
            "server/src/sessions.rs",
            "reset_password",
            "DELETE FROM",
            "sessions",
            1,
            Serial,
            None,
        ),
        entry(
            "server/src/sessions.rs",
            "unlink_oidc",
            "DELETE FROM",
            "native_handoffs",
            1,
            Serial,
            None,
        ),
        entry(
            "server/src/sessions.rs",
            "unlink_oidc",
            "DELETE FROM",
            "sessions",
            1,
            Serial,
            None,
        ),
    ]
}

/// Transaction-taking helpers that write members. Each of their callers must itself run inside
/// `begin_serial()`/`begin_retirement()` or be another helper in this list.
const TRANSACTION_HELPERS: [(&str, &str); 3] = [
    ("server/src/sessions.rs", "sessions::issue("),
    ("core/src/devices.rs", "Self::retire_members("),
    ("core/src/devices.rs", "Self::delete_identities("),
];

struct Function {
    name: String,
    text: String,
}

fn functions(source: &str) -> Vec<(usize, Function)> {
    let mut starts = Vec::new();
    let bytes = source.as_bytes();
    let mut from = 0;
    while let Some(found) = source[from..].find("fn ") {
        let at = from + found;
        let before = if at == 0 { b' ' } else { bytes[at - 1] };
        if before == b' ' || before == b'\n' || before == b'(' {
            let name: String = source[at + 3..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                starts.push((at, name));
            }
        }
        from = at + 3;
    }
    let mut out = Vec::new();
    for (i, (at, name)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map_or(source.len(), |next| next.0);
        out.push((
            *at,
            Function {
                name: name.clone(),
                text: source[*at..end].to_owned(),
            },
        ));
    }
    out
}

fn enclosing(functions: &[(usize, Function)], position: usize) -> Option<&Function> {
    functions
        .iter()
        .take_while(|(at, _)| *at <= position)
        .last()
        .map(|(_, function)| function)
}

fn sources() -> Vec<(String, String)> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for krate in ["core", "server"] {
        collect(&crates.join(krate).join("src"), &mut files);
    }
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(&crates)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).unwrap();
            (relative, text)
        })
        .collect()
}

fn collect(directory: &Path, into: &mut Vec<PathBuf>) {
    for item in std::fs::read_dir(directory).unwrap() {
        let path = item.unwrap().path();
        if path.is_dir() {
            collect(&path, into);
        } else if path.extension().is_some_and(|e| e == "rs") {
            into.push(path);
        }
    }
}

/// Every `(file, function, verb, table)` written by a source statement, with its count.
fn discovered(files: &[(String, String)]) -> BTreeMap<(String, String, String, String), usize> {
    let mut found = BTreeMap::new();
    for (file, source) in files {
        let functions = functions(source);
        for verb in VERBS {
            let mut from = 0;
            while let Some(at) = source[from..].find(verb) {
                let start = from + at;
                from = start + verb.len();
                // `verb` must start a word: `DO UPDATE SET` and `FOR UPDATE` are not writers here,
                // and neither is followed by a table name.
                let after = source[from..].trim_start();
                let name: String = after
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '{' || *c == '}')
                    .collect();
                if !TABLES.contains(&name.as_str()) {
                    continue;
                }
                let function = enclosing(&functions, start).map_or("<none>", |f| f.name.as_str());
                *found
                    .entry((file.clone(), function.to_owned(), verb.to_owned(), name))
                    .or_insert(0) += 1;
            }
        }
    }
    found
}

#[test]
fn every_member_writer_is_listed_with_its_coordination() {
    let files = sources();
    let mut expected = BTreeMap::new();
    for e in inventory() {
        expected.insert(
            (
                e.file.to_owned(),
                e.function.to_owned(),
                e.verb.to_owned(),
                e.table.to_owned(),
            ),
            e.count,
        );
    }
    let found = discovered(&files);
    let unlisted: Vec<_> = found
        .iter()
        .filter(|(k, v)| expected.get(*k) != Some(*v))
        .collect();
    let missing: Vec<_> = expected
        .iter()
        .filter(|(k, v)| found.get(*k) != Some(*v))
        .collect();
    assert!(
        unlisted.is_empty() && missing.is_empty(),
        "The writers of approved-state members changed. Decide how each new or moved writer is \
         ordered against a device retirement (docs/authentication.md), then update this table.\n\
         found but not listed (or with another count): {unlisted:#?}\n\
         listed but no longer found: {missing:#?}"
    );
}

#[test]
fn each_writer_still_has_the_ordering_it_is_listed_with() {
    let files = sources();
    for e in inventory() {
        let source = &files
            .iter()
            .find(|(file, _)| file == e.file)
            .unwrap_or_else(|| panic!("{} is not a source file", e.file))
            .1;
        let function = functions(source)
            .into_iter()
            .map(|(_, f)| f)
            .find(|f| f.name == e.function)
            .unwrap_or_else(|| panic!("no function {} in {}", e.function, e.file));
        let signature_end = function.text.find('{').unwrap_or(function.text.len());
        let takes_transaction = function.text[..signature_end].contains("Transaction<");
        let serial =
            function.text.contains("begin_serial(") || function.text.contains("begin_retirement(");
        match e.coordination {
            Serial => assert!(
                serial || takes_transaction,
                "{}::{} is listed as running inside begin_serial() but does not",
                e.file,
                e.function
            ),
            RowLock | Own => assert!(
                !serial,
                "{}::{} is listed as ordered by its own lock but now runs inside begin_serial(); \
                 update the table",
                e.file, e.function
            ),
        }
        if let Some(needs) = e.needs {
            assert!(
                function.text.contains(needs),
                "{}::{} lost the ordering point `{needs}` its coordination relies on",
                e.file,
                e.function
            );
        }
    }
}

#[test]
fn transaction_helpers_are_only_reached_from_serialised_writers() {
    let files = sources();
    let helper_names: Vec<&str> = TRANSACTION_HELPERS
        .iter()
        .map(|(_, call)| call.trim_end_matches('(').rsplit("::").next().unwrap())
        .collect();
    for (file, source) in &files {
        let functions = functions(source);
        for (_, call) in TRANSACTION_HELPERS {
            let mut from = 0;
            while let Some(at) = source[from..].find(call) {
                let start = from + at;
                from = start + call.len();
                let caller = enclosing(&functions, start).expect("a call sits in a function");
                let serialised = caller.text.contains("begin_serial(")
                    || caller.text.contains("begin_retirement(")
                    || helper_names.contains(&caller.name.as_str());
                assert!(
                    serialised,
                    "{file}::{} calls {call} outside begin_serial()",
                    caller.name
                );
            }
        }
    }
    // Sanity: the scan found the callers it is meant to check.
    let count: usize = files
        .iter()
        .map(|(_, source)| source.matches("sessions::issue(").count())
        .sum();
    assert!(
        count >= 4,
        "expected the four session-issuing routes, found {count}"
    );
}
