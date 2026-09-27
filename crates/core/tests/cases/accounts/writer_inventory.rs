//! (aj) Writer-inventory guard.
//!
//! The retirement is correct only against the writers of an approved-state member that it knows
//! about. This test lists every source statement that inserts into, updates or deletes one of those
//! tables and fails unless each appears below with its coordination. Adding a writer, moving one
//! into or out of `begin_serial()`, or removing one forces this table, and a decision about how the
//! new writer is ordered against a retirement, to be updated. `activation_grants` (BE-Q19) is now
//! written by grant issue/redeem/cancel, the three account-wide session revocations, and
//! retirement's own `cancel_grants`, all inside `begin_serial()`/`begin_retirement()`.
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
        // --- BE-Q19: activation-grant issue, redemption and cancellation
        entry(
            "server/src/activation.rs",
            "issue_grant",
            "DELETE FROM",
            "activation_grants",
            1,
            Serial,
            None,
        ),
        entry(
            "server/src/activation.rs",
            "issue_grant",
            "INSERT INTO",
            "activation_grants",
            1,
            Serial,
            None,
        ),
        entry(
            "server/src/activation.rs",
            "activate",
            "UPDATE",
            "activation_grants",
            3,
            Serial,
            None,
        ),
        entry(
            "server/src/activation.rs",
            "activate_cancel",
            "UPDATE",
            "activation_grants",
            1,
            Serial,
            None,
        ),
        entry(
            "server/src/activation.rs",
            "activate_cancel",
            "DELETE FROM",
            "sessions",
            1,
            Serial,
            None,
        ),
        // --- BE-Q19: account-wide session revocation also cancels outstanding grants
        entry(
            "server/src/sessions.rs",
            "change_password",
            "UPDATE",
            "activation_grants",
            1,
            Serial,
            None,
        ),
        entry(
            "server/src/sessions.rs",
            "reset_password",
            "UPDATE",
            "activation_grants",
            1,
            Serial,
            None,
        ),
        entry(
            "server/src/sessions.rs",
            "unlink_oidc",
            "UPDATE",
            "activation_grants",
            1,
            Serial,
            None,
        ),
        // --- BE-Q19: retirement cancels the device's issued grants (component 4)
        entry(
            "core/src/devices.rs",
            "cancel_grants",
            "UPDATE",
            "activation_grants",
            1,
            Serial,
            None,
        ),
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
        entry(
            "core/src/storage/mod.rs",
            "prepare_restored_database",
            "DELETE FROM",
            "activation_grants",
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
/// `begin_serial()`/`begin_retirement()`, be another helper in this list, or call one of the
/// `TRANSACTION_PRODUCING_HELPERS` below. `activation::issue_grant` takes a `Transaction` rather
/// than opening one, exactly like `sessions::issue`, so it belongs here: its own signature is not
/// evidence that whoever calls it acquired `begin_serial()` first (R5).
const TRANSACTION_HELPERS: [(&str, &str); 4] = [
    ("server/src/sessions.rs", "sessions::issue("),
    ("core/src/devices.rs", "Self::retire_members("),
    ("core/src/devices.rs", "Self::delete_identities("),
    ("server/src/activation.rs", "activation::issue_grant("),
];

/// Helpers that acquire `begin_serial()`/`begin_retirement()` themselves and hand the open
/// transaction back to their caller, rather than taking one as a parameter. `browser::login`
/// reaches `activation::issue_grant` this way, through `authenticate_local`, so a caller of one of
/// these is serialised precisely because the check below confirms the helper itself still is —
/// never merely because it is named here.
const TRANSACTION_PRODUCING_HELPERS: [(&str, &str); 1] =
    [("server/src/lib.rs", "authenticate_local(")];

/// Removes every comment and string/char/byte-char literal from `text`, replacing each with
/// spaces (newlines kept, so line numbers in any later message stay meaningful), leaving
/// executable code otherwise untouched. Handles line comments, nested block comments (Rust block
/// comments nest), plain and raw (optionally byte-prefixed) string literals, and char/byte-char
/// literals including their escapes (`\\'`, `\\"`, `\\n`, `\\0`, `\\xHH`, `\\u{...}`).
///
/// A char literal is masked too, not merely skipped: an earlier version of this scan left `'` as
/// plain text on the theory that a char literal can never itself hold a multi-character needle
/// like `begin_serial(`, so there was nothing to hide. That is true of the literal's own content,
/// but wrong about its effect on what comes after it: `'"'` is a valid char literal containing one
/// double quote, and treating that `'` as plain text left its interior `"` free to be read as the
/// *start* of an ordinary string by the check immediately below, which then closed that fictitious
/// string at the next real `"` in the source — leaving an actual following string's contents
/// unmasked and readable as code. Only a real lifetime (`'a`, `'static`, a label `'outer:`) is left
/// as plain text now: `char_literal_len` recognises it as *not* a char literal because nothing
/// closes it with a matching `'` immediately after one character or escape, so there is no
/// closing-quote ambiguity for this scan to get wrong.
///
/// This exists because the raw `.contains("begin_serial(")` checks below previously could not
/// tell a real call from a comment: `/* previously app.store.begin_serial() */ app.store.pool
/// .begin()` still contained the literal text `begin_serial(` even though it no longer calls it
/// (R5). The char-literal gap above is the same finding, continued: a quote character literal
/// ahead of a string retaining the old call text defeated the first fix (R5, second round).
fn mask_non_code(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some((prefix_len, hashes)) = raw_string_prefix(&bytes[i..]) {
            let content_start = i + prefix_len;
            let mut close = vec![b'"'];
            close.extend(std::iter::repeat_n(b'#', hashes));
            let mut end = bytes.len();
            let mut k = content_start;
            while k + close.len() <= bytes.len() {
                if bytes[k..k + close.len()] == close[..] {
                    end = k + close.len();
                    break;
                }
                k += 1;
            }
            for &b in &bytes[i..end] {
                out.push(if b == b'\n' { b'\n' } else { b' ' });
            }
            i = end;
            continue;
        }
        if bytes[i] == b'"' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            for &b in &bytes[start..i] {
                out.push(if b == b'\n' { b'\n' } else { b' ' });
            }
            continue;
        }
        if bytes[i] == b'\'' {
            if let Some(len) = char_literal_len(&bytes[i..]) {
                for &b in &bytes[i..i + len] {
                    out.push(if b == b'\n' { b'\n' } else { b' ' });
                }
                i += len;
                continue;
            }
            // Not a char literal that closes with a matching `'` immediately after one character
            // or escape — a lifetime or label. Pass the quote through as plain code; nothing here
            // needs masking (see `mask_non_code`'s doc comment).
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"//") {
            let start = i;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.extend(std::iter::repeat_n(b' ', i - start));
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            let start = i;
            let mut depth = 1u32;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            for &b in &bytes[start..i] {
                out.push(if b == b'\n' { b'\n' } else { b' ' });
            }
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).expect("masking only replaces bytes with ASCII spaces/newlines")
}

/// If `bytes` starts with a raw string opener (`r"`, `r#"`, `r##"`, ... or the byte-prefixed
/// `br"`, `br#"`, ...), returns the opener's length and its hash count, so the matching closer
/// (`"`, `"#`, `"##`, ...) can be located.
fn raw_string_prefix(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    if bytes.first() == Some(&b'b') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'r') {
        return None;
    }
    i += 1;
    let mut hashes = 0;
    while bytes.get(i + hashes) == Some(&b'#') {
        hashes += 1;
    }
    if bytes.get(i + hashes) == Some(&b'"') {
        Some((i + hashes + 1, hashes))
    } else {
        None
    }
}

/// If `bytes` starts with a complete char or byte-char literal (`'x'`, `'\''`, `'\"'`, `'\n'`,
/// `'\0'`, `'\xHH'`, `'\u{...}'`, or a non-ASCII scalar such as `'é'`), returns its length
/// including both quotes. Returns `None` for anything that is not a char literal closed by a
/// matching `'` immediately after exactly one character or one escape — in particular a lifetime
/// (`'a`) or a label (`'outer:`), neither of which is ever followed by a closing `'` there. The
/// caller (the `'` branch in `mask_non_code`) relies on that distinction to leave lifetimes as
/// plain code while still recognising every real char literal, including one whose content is
/// itself a quote character (R5, second round: `'"'` is exactly this case).
fn char_literal_len(bytes: &[u8]) -> Option<usize> {
    if bytes.first() != Some(&b'\'') {
        return None;
    }
    let mut i = 1;
    match *bytes.get(i)? {
        b'\'' | b'\n' => return None,
        b'\\' => {
            i += 1;
            match *bytes.get(i)? {
                b'x' => {
                    i += 1;
                    for _ in 0..2 {
                        if bytes.get(i).is_some_and(u8::is_ascii_hexdigit) {
                            i += 1;
                        } else {
                            return None;
                        }
                    }
                }
                b'u' => {
                    i += 1;
                    if bytes.get(i) != Some(&b'{') {
                        return None;
                    }
                    i += 1;
                    let digits_start = i;
                    while bytes.get(i).is_some_and(u8::is_ascii_hexdigit) {
                        i += 1;
                    }
                    if i == digits_start || bytes.get(i) != Some(&b'}') {
                        return None;
                    }
                    i += 1;
                }
                b'n' | b'r' | b't' | b'\\' | b'0' | b'\'' | b'"' => i += 1,
                _ => return None,
            }
        }
        lead => {
            let extra = if lead < 0x80 {
                0
            } else if lead >> 5 == 0b110 {
                1
            } else if lead >> 4 == 0b1110 {
                2
            } else if lead >> 3 == 0b1_1110 {
                3
            } else {
                return None;
            };
            i += 1;
            for _ in 0..extra {
                if bytes.get(i).is_some_and(|b| b & 0xC0 == 0x80) {
                    i += 1;
                } else {
                    return None;
                }
            }
        }
    }
    if bytes.get(i) == Some(&b'\'') {
        Some(i + 1)
    } else {
        None
    }
}

/// True if `text` contains `needle` as executable code — never inside a comment or a string/char
/// literal. Use this, not `str::contains`, for every check below that treats a literal call as
/// evidence of serialisation (R5).
fn calls(text: &str, needle: &str) -> bool {
    mask_non_code(text).contains(needle)
}

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
            calls(&function.text, "begin_serial(") || calls(&function.text, "begin_retirement(");
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

/// The actual check behind `transaction_helpers_are_only_reached_from_serialised_writers`,
/// pulled out to a plain function so the negative-control tests below can run it against a
/// mutated in-memory copy of the sources and assert it fails, rather than only asserting it
/// passes against the real worktree.
fn check_transaction_helpers_reached_only_from_serialised_writers(
    files: &[(String, String)],
) -> Result<(), String> {
    // A transaction-producing helper is only a valid reason to trust a caller if it still
    // acquires begin_serial()/begin_retirement() itself — checked here directly, not assumed
    // because its name appears in the list below.
    for (file, call) in TRANSACTION_PRODUCING_HELPERS {
        let name = call.trim_end_matches('(').rsplit("::").next().unwrap();
        let source = &files
            .iter()
            .find(|(f, _)| f == file)
            .ok_or_else(|| format!("{file} is not a source file"))?
            .1;
        let function = functions(source)
            .into_iter()
            .map(|(_, f)| f)
            .find(|f| f.name == name)
            .ok_or_else(|| format!("no function {name} in {file}"))?;
        if !(calls(&function.text, "begin_serial(") || calls(&function.text, "begin_retirement(")) {
            return Err(format!(
                "{file}::{name} no longer acquires begin_serial()/begin_retirement(); its \
                 callers rely on this"
            ));
        }
    }
    let helper_names: Vec<&str> = TRANSACTION_HELPERS
        .iter()
        .map(|(_, call)| call.trim_end_matches('(').rsplit("::").next().unwrap())
        .collect();
    for (file, source) in files {
        let functions = functions(source);
        for (_, call) in TRANSACTION_HELPERS {
            let mut from = 0;
            while let Some(at) = source[from..].find(call) {
                let start = from + at;
                from = start + call.len();
                let caller = enclosing(&functions, start)
                    .ok_or_else(|| format!("{file}: a call to {call} sits outside any function"))?;
                // Not satisfied by the caller merely taking a `Transaction` parameter, nor by a
                // comment or string literal mentioning `begin_serial`/`begin_retirement`/a
                // producing helper's name: `calls` masks out comments and string/char literals
                // first, so only an executable call to one of these can satisfy this check (R5).
                let serialised = calls(&caller.text, "begin_serial(")
                    || calls(&caller.text, "begin_retirement(")
                    || helper_names.contains(&caller.name.as_str())
                    || TRANSACTION_PRODUCING_HELPERS
                        .iter()
                        .any(|(_, producing_call)| calls(&caller.text, producing_call));
                if !serialised {
                    return Err(format!(
                        "{file}::{} calls {call} outside begin_serial()",
                        caller.name
                    ));
                }
            }
        }
    }
    // Sanity: the scan found the callers it is meant to check.
    let count: usize = files
        .iter()
        .map(|(_, source)| source.matches("sessions::issue(").count())
        .sum();
    if count < 4 {
        return Err(format!(
            "expected the four session-issuing routes, found {count}"
        ));
    }
    // Sanity: activation::issue_grant's three callers (browser login, browser registration, the
    // OIDC browser callback) were all found and checked above, not silently skipped.
    let issuers: usize = files
        .iter()
        .map(|(_, source)| source.matches("activation::issue_grant(").count())
        .sum();
    if issuers < 3 {
        return Err(format!(
            "expected the three grant-issuing call sites, found {issuers}"
        ));
    }
    Ok(())
}

#[test]
fn transaction_helpers_are_only_reached_from_serialised_writers() {
    let files = sources();
    if let Err(message) = check_transaction_helpers_reached_only_from_serialised_writers(&files) {
        panic!("{message}");
    }
}

/// Replaces `from` with `to` (which must appear exactly once) inside `function`'s own body in
/// `file`, leaving the rest of that file and every other file untouched. Used only by the
/// negative-control tests below: it mutates an in-memory copy of the real worktree source, never
/// the file on disk.
fn mutate_function(
    files: &[(String, String)],
    file: &str,
    function: &str,
    from: &str,
    to: &str,
) -> Vec<(String, String)> {
    let mut files = files.to_vec();
    let entry = files
        .iter_mut()
        .find(|(f, _)| f == file)
        .unwrap_or_else(|| panic!("{file} is not a source file"));
    let (start, target) = functions(&entry.1)
        .into_iter()
        .find(|(_, f)| f.name == function)
        .unwrap_or_else(|| panic!("no function {function} in {file}"));
    let occurrences = target.text.matches(from).count();
    assert_eq!(
        occurrences, 1,
        "{file}::{function} must contain `{from}` exactly once for this negative control to \
         target it precisely; it no longer matches the shape this test assumes"
    );
    let mutated = target.text.replacen(from, to, 1);
    let end = start + target.text.len();
    entry.1.replace_range(start..end, &mutated);
    files
}

/// R5's confirmed negative control, made permanent: a real grant issuer's caller, with its
/// `begin_serial()` replaced in memory by an ordinary transaction, must now fail the guard. This
/// is the review's exact reproduction (`browser_register`) — previously all three inventory tests
/// passed unchanged against this mutation; `transaction_helpers_are_only_reached_from_serialised_writers`
/// must not.
#[test]
fn guard_fails_when_browser_registration_bypasses_begin_serial() {
    let files = mutate_function(
        &sources(),
        "server/src/onboarding.rs",
        "browser_register",
        "app.store.begin_serial()",
        "app.store.pool.begin()",
    );
    let result = check_transaction_helpers_reached_only_from_serialised_writers(&files);
    assert!(
        result.is_err(),
        "the guard passed even though browser_register no longer opens begin_serial() before \
         calling activation::issue_grant"
    );
}

/// The same reproduction against the indirect path: `browser::login` never calls `begin_serial()`
/// itself, it obtains its transaction from `authenticate_local`. A caller-only check (only
/// looking at `login`'s own text) would never catch this helper's own ordering breaking; the
/// direct check on `TRANSACTION_PRODUCING_HELPERS` above must.
#[test]
fn guard_fails_when_authenticate_local_bypasses_begin_serial() {
    let files = mutate_function(
        &sources(),
        "server/src/lib.rs",
        "authenticate_local",
        "app.store.begin_serial()",
        "app.store.pool.begin()",
    );
    let result = check_transaction_helpers_reached_only_from_serialised_writers(&files);
    assert!(
        result.is_err(),
        "the guard passed even though authenticate_local (browser::login's transaction source) \
         no longer opens begin_serial()"
    );
}

/// R5's second, previously missing negative control: the review's independent reproduction showed
/// that leaving the old `begin_serial()` text inside a comment, while replacing the real call with
/// an ordinary transaction, satisfied the old raw `.contains` check. This mutates
/// `browser_register` exactly as the review did — the real call becomes `app.store.pool.begin()`,
/// and the retired call sits in a comment immediately before it — and must still fail the direct
/// caller check (`browser_register` calls `activation::issue_grant` and must itself open
/// `begin_serial()` as executable code, not merely mention it).
#[test]
fn guard_fails_when_browser_registration_hides_begin_serial_in_a_comment() {
    let files = mutate_function(
        &sources(),
        "server/src/onboarding.rs",
        "browser_register",
        "app.store.begin_serial()",
        "/* previously app.store.begin_serial() */ app.store.pool.begin()",
    );
    let result = check_transaction_helpers_reached_only_from_serialised_writers(&files);
    assert!(
        result.is_err(),
        "the guard passed even though browser_register's only begin_serial() call now sits inside \
         a comment, exactly the review's independent reproduction of R5"
    );
}

/// The same comment-evasion reproduction against `authenticate_local`, the transaction-producing
/// helper `browser::login` relies on. This exercises the other checked path: the producing
/// helper's own body check (not the direct caller check above), which must also see through a
/// comment retaining the old call text.
#[test]
fn guard_fails_when_authenticate_local_hides_begin_serial_in_a_comment() {
    let files = mutate_function(
        &sources(),
        "server/src/lib.rs",
        "authenticate_local",
        "app.store.begin_serial()",
        "/* previously app.store.begin_serial() */ app.store.pool.begin()",
    );
    let result = check_transaction_helpers_reached_only_from_serialised_writers(&files);
    assert!(
        result.is_err(),
        "the guard passed even though authenticate_local's only begin_serial() call now sits \
         inside a comment, exactly the review's independent reproduction of R5"
    );
}

/// R5's third negative control (second revision round): the independent review found that the
/// comment-evasion fix above still had a gap. A `'"'` char literal ahead of a string was read by
/// the pre-fix `mask_non_code` as opening a fictitious string at its interior quote; that
/// fictitious string then closed at the *next* real `"`, leaving the actual following string's
/// contents — including the retired `begin_serial(` text — unmasked and readable as code. This
/// mutates `browser_register` with the review's own reproduction: the real call becomes an
/// ordinary transaction, preceded by a quote char literal and a string retaining the old call
/// text, and must still fail the direct caller check.
#[test]
fn guard_fails_when_browser_registration_hides_begin_serial_after_a_quote_char_literal() {
    let files = mutate_function(
        &sources(),
        "server/src/onboarding.rs",
        "browser_register",
        "app.store.begin_serial()",
        r#"{ let _quote = '"'; let _reason = "previously app.store.begin_serial()"; app.store.pool.begin() }"#,
    );
    let result = check_transaction_helpers_reached_only_from_serialised_writers(&files);
    assert!(
        result.is_err(),
        "the guard passed even though browser_register's only begin_serial() call now sits \
         inside a string that follows a quote char literal, exactly the review's independent \
         reproduction of R5's second round"
    );
}

/// The same quote-char-literal reproduction against `authenticate_local`, exercising the
/// transaction-producing helper's-own-body-check path rather than the direct caller check above.
#[test]
fn guard_fails_when_authenticate_local_hides_begin_serial_after_a_quote_char_literal() {
    let files = mutate_function(
        &sources(),
        "server/src/lib.rs",
        "authenticate_local",
        "app.store.begin_serial()",
        r#"{ let _quote = '"'; let _reason = "previously app.store.begin_serial()"; app.store.pool.begin() }"#,
    );
    let result = check_transaction_helpers_reached_only_from_serialised_writers(&files);
    assert!(
        result.is_err(),
        "the guard passed even though authenticate_local's only begin_serial() call now sits \
         inside a string that follows a quote char literal, exactly the review's independent \
         reproduction of R5's second round"
    );
}
