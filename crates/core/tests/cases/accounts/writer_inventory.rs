//! (aj) Writer-inventory guard.
//!
//! The retirement is correct only against the writers of an approved-state member that it knows
//! about. This test lists every source statement that inserts into, updates or deletes one of those
//! tables and fails unless each appears below with its coordination. Adding a writer, moving one
//! into or out of `begin_serial()`, or removing one forces this table, and a decision about how the
//! new writer is ordered against a retirement, to be updated. `activation_grants` (BE-Q19) is now
//! written by grant issue/redeem/cancel, the three account-wide session revocations, and
//! retirement's own `cancel_grants`, all inside `begin_serial()`/`begin_retirement()`.
//!
//! `device_profiles` (BE-Q25) is not a member: a device's name and label are display metadata and
//! never part of its `state_token`. It is inventoried anyway because its lifetime is defined by
//! ordering against a retirement (a rename either commits before it and is deleted with the
//! device, or runs after it and finds nothing listed), so each of its writers must stay serialised:
//! the orphan prune `forget_unlisted` (reached from session issue's `record_client`, grant and
//! handoff issue and a subscription change), the label upsert, the rename, the retirement's own
//! cleanup and restore. `every_listing_writer_forgets_unlisted_profiles_first` below separately
//! requires every statement that can give a device a live member to run after that prune.
//!
//! BE-Q25 revision 2: serialisation alone does not keep a profile's lifetime right, because the
//! removals that do not take `begin_serial()` (sign-out, sync registration, the aged-registration
//! sweep) and the public listings could still interleave with a decision that a device is listed.
//! Every membership change and every profile write now holds the account's membership ordering
//! point (`devices::order_membership`), and the listings take it shared
//! (`Store::begin_membership_read`). `every_membership_change_holds_the_account_ordering_point`
//! and `every_profile_reader_reads_its_time_after_the_ordering_point` check that structure; they
//! are architectural alarms, and the concurrency itself is proved by the scheduled tests in
//! `device_names.rs` and the server's `labels.rs`.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const TABLES: [&str; 7] = [
    "sessions",
    "native_handoffs",
    "notification_subscriptions",
    "activation_grants",
    "sync_devices",
    // Not a member; see the module comment.
    "device_profiles",
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
            2,
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
        // --- BE-Q25: device names and labels
        entry(
            "core/src/devices.rs",
            "forget_unlisted",
            "DELETE FROM",
            "device_profiles",
            2,
            Serial,
            None,
        ),
        entry(
            "core/src/devices.rs",
            "record_client",
            "INSERT INTO",
            "device_profiles",
            1,
            Serial,
            None,
        ),
        entry(
            "core/src/devices.rs",
            "set_name_once",
            "INSERT INTO",
            "device_profiles",
            1,
            Serial,
            None,
        ),
        entry(
            "core/src/devices.rs",
            "set_name_once",
            "UPDATE",
            "device_profiles",
            1,
            Serial,
            None,
        ),
        entry(
            "core/src/devices.rs",
            "retire_members",
            "DELETE FROM",
            "device_profiles",
            1,
            Serial,
            None,
        ),
        entry(
            "core/src/storage/mod.rs",
            "prepare_restored_database",
            "DELETE FROM",
            "device_profiles",
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
            Some("order_membership("),
        ),
        entry(
            "core/src/sync.rs",
            "sync_once",
            "UPDATE",
            "sync_devices",
            1,
            Own,
            Some("order_membership("),
        ),
        // --- the two session revocations outside begin_serial(): BE-Q25 revision 2 made each its
        // own transaction holding the account's membership ordering point (formerly autocommit)
        entry(
            "server/src/lib.rs",
            "logout",
            "DELETE FROM",
            "sessions",
            1,
            Own,
            Some("order_membership("),
        ),
        entry(
            "server/src/sessions.rs",
            "revoke",
            "DELETE FROM",
            "sessions",
            1,
            Own,
            Some("order_membership("),
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
const TRANSACTION_HELPERS: [(&str, &str); 6] = [
    ("server/src/sessions.rs", "sessions::issue("),
    ("core/src/devices.rs", "Self::retire_members("),
    ("core/src/devices.rs", "Self::delete_identities("),
    ("server/src/activation.rs", "activation::issue_grant("),
    // BE-Q25: takes `sessions::issue`'s transaction.
    ("core/src/devices.rs", "devices::record_client("),
    // BE-Q25 revision 1: the orphan prune, which grant/handoff issue and a subscription change call
    // with their own transaction. (Revision 2: a sync registration also calls it, unqualified and
    // outside begin_serial(), holding the account's membership ordering point instead; its
    // `&Ordered` parameter and `every_membership_change_holds_the_account_ordering_point` cover
    // that caller.)
    ("core/src/devices.rs", "devices::forget_unlisted("),
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
pub(crate) fn mask_non_code(text: &str) -> String {
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
pub(crate) fn calls(text: &str, needle: &str) -> bool {
    mask_non_code(text).contains(needle)
}

pub(crate) struct Function {
    pub(crate) name: String,
    pub(crate) text: String,
}

pub(crate) fn functions(source: &str) -> Vec<(usize, Function)> {
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

pub(crate) fn enclosing(functions: &[(usize, Function)], position: usize) -> Option<&Function> {
    functions
        .iter()
        .take_while(|(at, _)| *at <= position)
        .last()
        .map(|(_, function)| function)
}

pub(crate) fn sources() -> Vec<(String, String)> {
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

/// The check behind `each_writer_still_has_the_ordering_it_is_listed_with`, pulled out so a
/// negative control can run it against a mutated in-memory copy of the sources (BE-Q25).
fn check_writer_ordering(files: &[(String, String)]) -> Result<(), String> {
    for e in inventory() {
        let source = &files
            .iter()
            .find(|(file, _)| file == e.file)
            .ok_or_else(|| format!("{} is not a source file", e.file))?
            .1;
        let function = functions(source)
            .into_iter()
            .map(|(_, f)| f)
            .find(|f| f.name == e.function)
            .ok_or_else(|| format!("no function {} in {}", e.function, e.file))?;
        let signature_end = function.text.find('{').unwrap_or(function.text.len());
        let takes_transaction = function.text[..signature_end].contains("Transaction<");
        let serial =
            calls(&function.text, "begin_serial(") || calls(&function.text, "begin_retirement(");
        match e.coordination {
            Serial if !(serial || takes_transaction) => {
                return Err(format!(
                    "{}::{} is listed as running inside begin_serial() but does not",
                    e.file, e.function
                ));
            }
            RowLock | Own if serial => {
                return Err(format!(
                    "{}::{} is listed as ordered by its own lock but now runs inside \
                     begin_serial(); update the table",
                    e.file, e.function
                ));
            }
            _ => {}
        }
        if let Some(needs) = e.needs
            && !function.text.contains(needs)
        {
            return Err(format!(
                "{}::{} lost the ordering point `{needs}` its coordination relies on",
                e.file, e.function
            ));
        }
    }
    Ok(())
}

#[test]
fn each_writer_still_has_the_ordering_it_is_listed_with() {
    if let Err(message) = check_writer_ordering(&sources()) {
        panic!("{message}");
    }
}

/// BE-Q25 negative control (mutation M7): a rename whose transaction is no longer `begin_serial()`
/// would not be ordered against a retirement, so the guard must fail on it, including when the
/// old call survives only in a comment.
#[test]
fn guard_fails_when_a_rename_bypasses_begin_serial() {
    for replacement in [
        "self.pool.begin()",
        "/* previously self.begin_serial() */ self.pool.begin()",
    ] {
        let files = mutate_function(
            &sources(),
            "core/src/devices.rs",
            "set_name_once",
            "self.begin_serial()",
            replacement,
        );
        assert!(
            check_writer_ordering(&files).is_err(),
            "the guard passed even though set_name_once opens {replacement}"
        );
    }
}

/// BE-Q25: `record_client` takes a transaction, so the guard must also reject a caller of it that
/// is not itself serialised (here: a call moved into the unserialised `revoke` handler).
#[test]
fn guard_fails_when_record_client_is_reached_outside_begin_serial() {
    let files = mutate_function(
        &sources(),
        "server/src/sessions.rs",
        "revoke",
        "hook!(app.store, \"session_delete.after_identity\");",
        "atlas_core::devices::record_client(todo!(), &account, &id, todo!(), 0).await?;",
    );
    assert!(
        check_transaction_helpers_reached_only_from_serialised_writers(&files).is_err(),
        "the guard passed even though record_client is called from the unserialised revoke"
    );
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

/// BE-Q25 revision 1 (R1/R2, family `device-profile-membership-boundary`): every statement that can
/// give a device a live member, with the call that must precede it in the same function. That call
/// drops the profiles of unlisted devices (`forget_unlisted`, or `record_client`, which starts with
/// it), so a forgotten device is listed again unnamed whichever transition lists it.
///
/// Revision 2 (Review 2 R1B): `sync_once`'s registration insert is listed too. Revision 1 exempted
/// it on the claim that the authenticating session keeps the device listed, but authentication is
/// an earlier, separate read: the session can be signed out, and a listing can see the device
/// absent, before the registration commits. It now prunes, holding the ordering point.
const LISTING_WRITERS: [(&str, &str, &str, &str); 7] = [
    (
        "server/src/sessions.rs",
        "issue",
        "INSERT INTO sessions",
        "record_client(",
    ),
    (
        "server/src/activation.rs",
        "issue_grant",
        "INSERT INTO activation_grants",
        "forget_unlisted(",
    ),
    (
        "server/src/oidc.rs",
        "callback",
        "INSERT INTO native_handoffs",
        "forget_unlisted(",
    ),
    (
        "core/src/calendars/reminders.rs",
        "reminder_command",
        "INSERT INTO notification_subscriptions",
        "forget_unlisted(",
    ),
    // Re-enabling a subscription, or moving it to another device ID, also lists a device.
    (
        "core/src/calendars/reminders.rs",
        "reminder_command",
        "UPDATE notification_subscriptions SET device_id",
        "forget_unlisted(",
    ),
    (
        "core/src/devices.rs",
        "record_client",
        "INSERT INTO device_profiles",
        "forget_unlisted(",
    ),
    (
        "core/src/sync.rs",
        "sync_once",
        "INSERT INTO sync_devices",
        "forget_unlisted(",
    ),
];

/// Member tables whose insert lists a device.
const LISTING_TABLES: [&str; 5] = [
    "sessions",
    "native_handoffs",
    "notification_subscriptions",
    "activation_grants",
    "sync_devices",
];

fn check_listing_writers(files: &[(String, String)]) -> Result<(), String> {
    // Completeness: a new insert into a member table must be classified here.
    for ((file, function, verb, table), _) in discovered(files) {
        if verb == "INSERT INTO"
            && LISTING_TABLES.contains(&table.as_str())
            && !LISTING_WRITERS.iter().any(|(f, n, statement, _)| {
                *f == file && *n == function && statement.ends_with(&table)
            })
        {
            return Err(format!(
                "{file}::{function} inserts into {table}, which lists a device: it must call \
                 forget_unlisted first and be added to LISTING_WRITERS"
            ));
        }
    }
    for (file, name, statement, prune) in LISTING_WRITERS {
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
        let at = function
            .text
            .find(statement)
            .ok_or_else(|| format!("{file}::{name} no longer contains `{statement}`"))?;
        // `mask_non_code` keeps byte offsets, so a call hidden in a comment or string is not found
        // and the executable call's position compares directly with the statement's.
        let pruned = mask_non_code(&function.text).find(prune);
        if pruned.is_none_or(|p| p > at) {
            return Err(format!(
                "{file}::{name} runs `{statement}` without first calling `{prune}`: a forgotten \
                 device could be listed again with its old name"
            ));
        }
    }
    Ok(())
}

#[test]
fn every_listing_writer_forgets_unlisted_profiles_first() {
    if let Err(message) = check_listing_writers(&sources()) {
        panic!("{message}");
    }
}

/// Negative controls for the check above: the grant insert without its prune (the R1 defect),
/// the prune retained only in a comment, the handoff insert running before its prune, and an
/// unclassified new insert into a member table.
#[test]
fn guard_fails_when_a_listing_writer_skips_the_prune() {
    let prune = "atlas_core::devices::forget_unlisted(tx, ordered, &Continuity::Unknown).await?;";
    let cases = [
        mutate_function(
            &sources(),
            "server/src/activation.rs",
            "issue_grant",
            prune,
            "",
        ),
        mutate_function(
            &sources(),
            "server/src/activation.rs",
            "issue_grant",
            prune,
            "/* atlas_core::devices::forget_unlisted(tx, ordered, &Continuity::Unknown) */",
        ),
        mutate_function(
            &sources(),
            "server/src/oidc.rs",
            "callback",
            "atlas_core::devices::forget_unlisted(&mut tx, &ordered, &Continuity::Unknown).await?;",
            "",
        ),
        // Review 2 R1B: the registration insert without its prune.
        mutate_function(
            &sources(),
            "core/src/sync.rs",
            "sync_once",
            "forget_unlisted(&mut tx, &ordered, &Continuity::Unknown).await?;",
            "",
        ),
        mutate_function(
            &sources(),
            "server/src/sessions.rs",
            "revoke",
            "hook!(app.store, \"session_delete.after_identity\");",
            "sqlx::query(\"INSERT INTO sessions(token_hash) VALUES ($1)\");",
        ),
    ];
    let expected = [
        "issue_grant runs `INSERT INTO activation_grants` without",
        "issue_grant runs `INSERT INTO activation_grants` without",
        "callback runs `INSERT INTO native_handoffs` without",
        "sync_once runs `INSERT INTO sync_devices` without",
        "revoke inserts into sessions",
    ];
    for (files, expected) in cases.iter().zip(expected) {
        let message = check_listing_writers(files).expect_err(expected);
        assert!(message.contains(expected), "{message}");
    }
    // The handoff insert moved ahead of the prune.
    let mut files = sources();
    let entry = files
        .iter_mut()
        .find(|(f, _)| f == "server/src/oidc.rs")
        .unwrap();
    let prune =
        "atlas_core::devices::forget_unlisted(&mut tx, &ordered, &Continuity::Unknown).await?;";
    assert_eq!(entry.1.matches(prune).count(), 1);
    entry.1 = entry.1.replacen(prune, "", 1);
    let close = entry
        .1
        .find("tx.commit().await?;\n        let mut destination")
        .unwrap();
    entry.1.insert_str(close, prune);
    let message = check_listing_writers(&files).expect_err("prune after the handoff insert");
    assert!(
        message.contains("callback runs `INSERT INTO native_handoffs` without"),
        "{message}"
    );
}

/// How a statement that changes an account's device membership, or a device profile, holds the
/// account's membership ordering point (BE-Q25 revision 2, Review 2 R1).
#[derive(Clone, Copy)]
enum Held {
    /// The function itself calls `order_membership(` before every such statement.
    Takes,
    /// The function takes `&Ordered`, which only `order_membership` makes, so its caller held the
    /// point to call it.
    Given,
    /// A transaction helper, reached only from these functions, each of which holds the point
    /// before the call (or is itself a `Reached` helper).
    Reached(&'static [&'static str]),
    /// Not a change of membership; the reason.
    Exempt(&'static str),
}
use Held::*;

/// Every `(file, function, verb, table)` of the inventory above, with how it holds the point.
const MEMBERSHIP_ORDER: [(&str, &str, &str, &str, Held); 43] = [
    (
        "server/src/activation.rs",
        "issue_grant",
        "DELETE FROM",
        "activation_grants",
        Given,
    ),
    (
        "server/src/activation.rs",
        "issue_grant",
        "INSERT INTO",
        "activation_grants",
        Given,
    ),
    (
        "server/src/activation.rs",
        "activate",
        "UPDATE",
        "activation_grants",
        Takes,
    ),
    (
        "server/src/activation.rs",
        "activate_cancel",
        "UPDATE",
        "activation_grants",
        Takes,
    ),
    (
        "server/src/activation.rs",
        "activate_cancel",
        "DELETE FROM",
        "sessions",
        Takes,
    ),
    (
        "server/src/sessions.rs",
        "change_password",
        "UPDATE",
        "activation_grants",
        Takes,
    ),
    (
        "server/src/sessions.rs",
        "change_password",
        "DELETE FROM",
        "native_handoffs",
        Takes,
    ),
    (
        "server/src/sessions.rs",
        "change_password",
        "DELETE FROM",
        "sessions",
        Takes,
    ),
    (
        "server/src/sessions.rs",
        "reset_password",
        "UPDATE",
        "activation_grants",
        Takes,
    ),
    (
        "server/src/sessions.rs",
        "reset_password",
        "DELETE FROM",
        "native_handoffs",
        Takes,
    ),
    (
        "server/src/sessions.rs",
        "reset_password",
        "DELETE FROM",
        "sessions",
        Takes,
    ),
    (
        "server/src/sessions.rs",
        "unlink_oidc",
        "UPDATE",
        "activation_grants",
        Takes,
    ),
    (
        "server/src/sessions.rs",
        "unlink_oidc",
        "DELETE FROM",
        "native_handoffs",
        Takes,
    ),
    (
        "server/src/sessions.rs",
        "unlink_oidc",
        "DELETE FROM",
        "sessions",
        Takes,
    ),
    (
        "server/src/sessions.rs",
        "revoke",
        "DELETE FROM",
        "sessions",
        Takes,
    ),
    (
        "server/src/sessions.rs",
        "issue",
        "DELETE FROM",
        "sessions",
        Given,
    ),
    (
        "server/src/sessions.rs",
        "issue",
        "INSERT INTO",
        "sessions",
        Given,
    ),
    (
        "server/src/lib.rs",
        "logout",
        "DELETE FROM",
        "sessions",
        Takes,
    ),
    (
        "server/src/oidc.rs",
        "callback",
        "DELETE FROM",
        "native_handoffs",
        Takes,
    ),
    (
        "server/src/oidc.rs",
        "callback",
        "INSERT INTO",
        "native_handoffs",
        Takes,
    ),
    (
        "server/src/oidc.rs",
        "native_exchange",
        "DELETE FROM",
        "native_handoffs",
        Takes,
    ),
    (
        "core/src/devices.rs",
        "cancel_grants",
        "UPDATE",
        "activation_grants",
        Reached(&["retire_members"]),
    ),
    (
        "core/src/devices.rs",
        "retire_members",
        "DELETE FROM",
        "sync_devices",
        Reached(&["retire_once"]),
    ),
    (
        "core/src/devices.rs",
        "retire_members",
        "UPDATE",
        "notification_subscriptions",
        Reached(&["retire_once"]),
    ),
    (
        "core/src/devices.rs",
        "retire_members",
        "DELETE FROM",
        "device_profiles",
        Reached(&["retire_once"]),
    ),
    (
        "core/src/devices.rs",
        "delete_identities",
        "DELETE FROM",
        "{table}",
        Reached(&["retire_members"]),
    ),
    (
        "core/src/devices.rs",
        "revoke_once",
        "DELETE FROM",
        "sessions",
        Takes,
    ),
    (
        "core/src/devices.rs",
        "forget_unlisted",
        "DELETE FROM",
        "device_profiles",
        Given,
    ),
    (
        "core/src/devices.rs",
        "record_client",
        "INSERT INTO",
        "device_profiles",
        Given,
    ),
    (
        "core/src/devices.rs",
        "set_name_once",
        "INSERT INTO",
        "device_profiles",
        Takes,
    ),
    (
        "core/src/devices.rs",
        "set_name_once",
        "UPDATE",
        "device_profiles",
        Takes,
    ),
    (
        "core/src/calendars/reminders.rs",
        "reminder_command",
        "INSERT INTO",
        "notification_subscriptions",
        Takes,
    ),
    (
        "core/src/calendars/reminders.rs",
        "reminder_command",
        "UPDATE",
        "notification_subscriptions",
        Takes,
    ),
    (
        "core/src/sync.rs",
        "sync_once",
        "INSERT INTO",
        "sync_devices",
        Takes,
    ),
    (
        "core/src/sync.rs",
        "sync_once",
        "UPDATE",
        "sync_devices",
        Exempt(
            "refreshes `last_seen` of a registration that exists; lists nothing new, removes nothing",
        ),
    ),
    (
        "core/src/sync.rs",
        "collect_expired",
        "DELETE FROM",
        "sync_devices",
        Takes,
    ),
    (
        "core/src/sync.rs",
        "collect_expired",
        "DELETE FROM",
        "native_handoffs",
        Exempt(
            "only rows expired at the sweep's time, which no listing at that time or later counts",
        ),
    ),
    (
        "core/src/sync.rs",
        "collect_expired",
        "DELETE FROM",
        "sessions",
        Exempt(
            "only rows expired at the sweep's time, which no listing at that time or later counts",
        ),
    ),
    (
        "core/src/storage/mod.rs",
        "prepare_restored_database",
        "DELETE FROM",
        "device_profiles",
        Exempt(
            "restore preparation with every server stopped: forgets every device and every profile",
        ),
    ),
    (
        "core/src/storage/mod.rs",
        "prepare_restored_database",
        "DELETE FROM",
        "native_handoffs",
        Exempt("restore preparation with every server stopped"),
    ),
    (
        "core/src/storage/mod.rs",
        "prepare_restored_database",
        "DELETE FROM",
        "sessions",
        Exempt("restore preparation with every server stopped"),
    ),
    (
        "core/src/storage/mod.rs",
        "prepare_restored_database",
        "DELETE FROM",
        "sync_devices",
        Exempt("restore preparation with every server stopped"),
    ),
    (
        "core/src/storage/mod.rs",
        "prepare_restored_database",
        "DELETE FROM",
        "activation_grants",
        Exempt("restore preparation with every server stopped"),
    ),
];

fn function_in(files: &[(String, String)], file: &str, name: &str) -> Result<Function, String> {
    let source = &files
        .iter()
        .find(|(f, _)| f == file)
        .ok_or_else(|| format!("{file} is not a source file"))?
        .1;
    functions(source)
        .into_iter()
        .map(|(_, f)| f)
        .find(|f| f.name == name)
        .ok_or_else(|| format!("no function {name} in {file}"))
}

const ORDER_CALL: &str = "order_membership(";

fn check_membership_order(files: &[(String, String)]) -> Result<(), String> {
    // Completeness, both ways: every member or profile writer is classified here.
    let found = discovered(files);
    for (file, function, verb, table) in found.keys() {
        if !MEMBERSHIP_ORDER
            .iter()
            .any(|(f, n, v, t, _)| f == file && n == function && v == verb && t == table)
        {
            return Err(format!(
                "{file}::{function} runs `{verb} {table}` but MEMBERSHIP_ORDER does not say how \
                 it holds the account's membership ordering point"
            ));
        }
    }
    for (file, name, verb, table, held) in MEMBERSHIP_ORDER {
        let key = (
            file.to_owned(),
            name.to_owned(),
            verb.to_owned(),
            table.to_owned(),
        );
        if !found.contains_key(&key) {
            return Err(format!("{file}::{name} no longer runs `{verb} {table}`"));
        }
        let function = function_in(files, file, name)?;
        let code = mask_non_code(&function.text);
        match held {
            Takes => {
                let statement = format!("{verb} {table}");
                let first_order = code.find(ORDER_CALL);
                for (at, _) in function.text.match_indices(&statement) {
                    if first_order.is_none_or(|order| order > at) {
                        return Err(format!(
                            "{file}::{name} runs `{statement}` before calling `{ORDER_CALL}`: a \
                             removal or a listing could interleave with its membership decision"
                        ));
                    }
                }
            }
            Given => {
                let signature_end = function.text.find('{').unwrap_or(function.text.len());
                if !function.text[..signature_end].contains("&Ordered") {
                    return Err(format!(
                        "{file}::{name} no longer takes `&Ordered`, so nothing proves its caller \
                         holds the account's membership ordering point"
                    ));
                }
            }
            Reached(callers) => {
                let call = format!("::{name}(");
                let mut sites = 0;
                for (source_file, source) in files {
                    let functions = functions(source);
                    let masked = mask_non_code(source);
                    for (at, _) in masked.match_indices(&call) {
                        sites += 1;
                        let caller = enclosing(&functions, at).ok_or_else(|| {
                            format!("{source_file}: a call to {name} sits outside any function")
                        })?;
                        if !callers.contains(&caller.name.as_str()) {
                            return Err(format!(
                                "{source_file}::{} calls {name}, which only {callers:?} may call",
                                caller.name
                            ));
                        }
                        let caller_reached = MEMBERSHIP_ORDER
                            .iter()
                            .any(|(_, n, _, _, h)| *n == caller.name && matches!(h, Reached(_)));
                        let start = functions
                            .iter()
                            .find(|(_, f)| f.name == caller.name && f.text == caller.text)
                            .map(|(s, _)| *s)
                            .unwrap();
                        let ordered_before = mask_non_code(&caller.text)
                            .find(ORDER_CALL)
                            .is_some_and(|order| start + order < at);
                        if !(caller_reached || ordered_before) {
                            return Err(format!(
                                "{source_file}::{} calls {name} without first calling \
                                 `{ORDER_CALL}`",
                                caller.name
                            ));
                        }
                    }
                }
                if sites == 0 {
                    return Err(format!("{file}::{name} is no longer called"));
                }
            }
            Exempt(reason) => {
                if reason.is_empty() {
                    return Err(format!("{file}::{name} is exempt without a reason"));
                }
            }
        }
    }
    // The ordering point itself: the account row's no-op update, and the time read after it.
    let order = function_in(files, "core/src/devices.rs", "order_membership")?;
    let update = order
        .text
        .find("UPDATE accounts SET access_epoch=access_epoch WHERE id=$1")
        .ok_or("order_membership no longer updates the account row")?;
    if mask_non_code(&order.text)
        .find("clock.now()")
        .is_none_or(|time| time < update)
    {
        return Err("order_membership reads its time before taking the account row".into());
    }
    Ok(())
}

#[test]
fn every_membership_change_holds_the_account_ordering_point() {
    if let Err(message) = check_membership_order(&sources()) {
        panic!("{message}");
    }
}

/// Negative controls: the review's two routes (sign-out without the point; a sync registration
/// without it), a handoff insert before the point, the account update gone from the point itself,
/// a retirement helper called from an unordered function, and an unclassified new writer.
#[test]
fn guard_fails_when_a_membership_change_skips_the_ordering_point() {
    let cases = [
        (
            mutate_function(
                &sources(),
                "server/src/lib.rs",
                "logout",
                "atlas_core::devices::order_membership(&mut tx, &account, &now).await?;",
                "",
            ),
            "logout runs `DELETE FROM sessions` before calling",
        ),
        (
            mutate_function(
                &sources(),
                "core/src/sync.rs",
                "sync_once",
                "let ordered = order_membership(&mut tx, actor, time).await?;",
                "/* let ordered = order_membership(&mut tx, actor, time).await?; */",
            ),
            "sync_once runs `INSERT INTO sync_devices` before calling",
        ),
        (
            mutate_function(
                &sources(),
                "core/src/sync.rs",
                "collect_expired",
                "order_membership(&mut tx, &account, &now).await?;",
                "",
            ),
            "collect_expired runs `DELETE FROM sync_devices` before calling",
        ),
        (
            mutate_function(
                &sources(),
                "core/src/devices.rs",
                "order_membership",
                "UPDATE accounts SET access_epoch=access_epoch WHERE id=$1",
                "SELECT id FROM accounts WHERE id=$1",
            ),
            "order_membership no longer updates the account row",
        ),
        (
            mutate_function(
                &sources(),
                "server/src/sessions.rs",
                "revoke",
                "hook!(app.store, \"session_delete.after_identity\");",
                "atlas_core::Store::retire_members(todo!(), todo!(), todo!(), todo!(), 0);",
            ),
            "calls retire_members, which only",
        ),
        (
            mutate_function(
                &sources(),
                "server/src/sessions.rs",
                "me",
                "let (account, _) = identity(&app, &headers).await?;",
                "let (account, _) = identity(&app, &headers).await?; sqlx::query(\"DELETE FROM sync_devices WHERE account_id=$1\");",
            ),
            "me runs `DELETE FROM sync_devices` but MEMBERSHIP_ORDER",
        ),
    ];
    for (files, expected) in cases {
        let message = check_membership_order(&files).expect_err(expected);
        assert!(message.contains(expected), "{message}");
    }
    // The handoff statements moved ahead of the point (the point stays, later).
    let order =
        "let ordered = atlas_core::devices::order_membership(&mut tx, &account, &now).await?;";
    let late = "tx.commit().await?;\n        let mut destination";
    let files = mutate_function(&sources(), "server/src/oidc.rs", "callback", order, "");
    let files = mutate_function(
        &files,
        "server/src/oidc.rs",
        "callback",
        late,
        &format!("{order}{late}"),
    );
    let message = check_membership_order(&files).expect_err("point after the handoff insert");
    assert!(
        message.contains("callback runs `DELETE FROM native_handoffs` before calling"),
        "{message}"
    );
}

/// The reads that show device profiles, each of which must open its transaction with
/// `begin_membership_read(` and read its time only after it (BE-Q25 revision 2, design-assessment
/// decision P1: listings participate in the ordering).
const PROFILE_READERS: [(&str, &str); 2] = [
    ("core/src/devices.rs", "devices_once"),
    ("core/src/devices.rs", "sessions_once"),
];

fn check_profile_readers(files: &[(String, String)]) -> Result<(), String> {
    for (file, source) in files {
        for (_, function) in functions(source) {
            if function.text.contains("JOIN device_profiles")
                && !PROFILE_READERS
                    .iter()
                    .any(|(f, n)| f == file && *n == function.name)
            {
                return Err(format!(
                    "{file}::{} reads device_profiles but is not a PROFILE_READER",
                    function.name
                ));
            }
        }
    }
    for (file, name) in PROFILE_READERS {
        let function = function_in(files, file, name)?;
        let code = mask_non_code(&function.text);
        let begin = code
            .find("self.begin_membership_read(")
            .ok_or_else(|| format!("{file}::{name} no longer opens begin_membership_read()"))?;
        let time = code
            .find("clock.now()")
            .ok_or_else(|| format!("{file}::{name} no longer reads its clock"))?;
        if time < begin {
            return Err(format!(
                "{file}::{name} reads its time before taking the ordering point"
            ));
        }
        if code.contains("pool.begin(") {
            return Err(format!("{file}::{name} opens an unordered transaction"));
        }
        let join = function
            .text
            .find("JOIN device_profiles")
            .ok_or_else(|| format!("{file}::{name} no longer reads device_profiles"))?;
        if join < begin {
            return Err(format!(
                "{file}::{name} reads profiles before the ordering point"
            ));
        }
    }
    let begin = function_in(files, "core/src/devices.rs", "begin_membership_read")?;
    for needed in ["BEGIN IMMEDIATE", "REPEATABLE READ", "FOR SHARE"] {
        if !begin.text.contains(needed) {
            return Err(format!("begin_membership_read no longer uses `{needed}`"));
        }
    }
    Ok(())
}

#[test]
fn every_profile_reader_reads_its_time_after_the_ordering_point() {
    if let Err(message) = check_profile_readers(&sources()) {
        panic!("{message}");
    }
}

#[test]
fn guard_fails_when_a_profile_reader_skips_the_ordering_point() {
    let cases = [
        (
            mutate_function(
                &sources(),
                "core/src/devices.rs",
                "devices_once",
                "let mut tx = self.begin_membership_read(actor).await?;",
                "let mut tx = self.pool.begin().await?;",
            ),
            "devices_once no longer opens begin_membership_read()",
        ),
        (
            mutate_function(
                &sources(),
                "core/src/devices.rs",
                "sessions_once",
                "let mut tx = self.begin_membership_read(actor).await?;\n        let now = clock.now();",
                "let now = clock.now();\n        let mut tx = self.begin_membership_read(actor).await?;",
            ),
            "sessions_once reads its time before taking the ordering point",
        ),
        (
            mutate_function(
                &sources(),
                "core/src/devices.rs",
                "begin_membership_read",
                "SELECT id FROM accounts WHERE id=$1 FOR SHARE",
                "SELECT id FROM accounts WHERE id=$1",
            ),
            "begin_membership_read no longer uses `FOR SHARE`",
        ),
        (
            mutate_function(
                &sources(),
                "server/src/sessions.rs",
                "me",
                "let (account, _) = identity(&app, &headers).await?;",
                "let (account, _) = identity(&app, &headers).await?; sqlx::query(\"SELECT p.name FROM sessions s LEFT JOIN device_profiles p\");",
            ),
            "me reads device_profiles but is not a PROFILE_READER",
        ),
    ];
    for (files, expected) in cases {
        let message = check_profile_readers(&files).expect_err(expected);
        assert!(message.contains(expected), "{message}");
    }
}
