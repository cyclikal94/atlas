//! Device listing and atomic, recoverable device retirement.
//!
//! A device is not a table: it is the set of rows sharing an `(account, device_id)`. The
//! *approved device state* is the members a user could have confirmed retiring, and its opaque
//! `state_token` is what a retirement must still match when it commits. See the retirement
//! protocol on [`Store::retire_device`] and `docs/authentication.md`.
use super::*;
use crate::error::ErrorCode;
use crate::operations::{Operation, OperationKind, Outcome, request_digest};
use crate::sync::retry_unit;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

const TOKEN_PREFIX: &str = "v1.";

/// Live sessions an account may hold: `sessions::issue` refuses the next one with
/// `device_capacity`, and `GET /sessions` reports it as `session_capacity.limit` (BE-Q25).
pub const SESSION_LIMIT: i64 = 32;
/// Sync registrations an account may hold: a sync from an unregistered device is refused with
/// `device_capacity`, and `GET /devices` reports it as `sync_capacity.limit` (BE-Q25).
pub const SYNC_DEVICE_LIMIT: i64 = 32;

/// The devices of account `$1` that hold a live member of their approved state at time `$2`. The
/// listing, a rename's membership check and the orphan-profile prune ([`forget_unlisted`]) all use
/// this one text, so what is listed, what may be named and what keeps its name can never disagree.
macro_rules! live_devices {
    () => {
        "SELECT device_id FROM sync_devices WHERE account_id=$1 UNION SELECT device_id FROM sessions WHERE account_id=$1 AND expires_at>$2 UNION SELECT device_id FROM native_handoffs WHERE account_id=$1 AND expires_at>$2 UNION SELECT device_id FROM notification_subscriptions WHERE account_id=$1 AND active=1 UNION SELECT device_id FROM activation_grants WHERE account_id=$1 AND state='issued' AND expires_at>$2"
    };
}

/// How a session was issued: `browser` by `POST /browser-sessions/activate` (the cookie route),
/// `native` by the three bearer-issuing routes. Taken from the route, never from a header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Browser,
    Native,
}

/// Coarse browser family. Self-reported by the client, so a recognition hint, never identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Browser {
    Chrome,
    Edge,
    Firefox,
    Safari,
    Opera,
    SamsungInternet,
    Unknown,
}

/// Coarse operating-system family, with the same trust as [`Browser`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Windows,
    Macos,
    Ios,
    Android,
    Linux,
    Chromeos,
    Unknown,
}

macro_rules! closed_enum {
    ($name:ident { $($variant:ident = $text:literal),* $(,)? }) => {
        impl $name {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text,)*
                }
            }
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)*
                    _ => None,
                }
            }
        }
    };
}
closed_enum!(Channel {
    Browser = "browser",
    Native = "native"
});
closed_enum!(Browser {
    Chrome = "chrome",
    Edge = "edge",
    Firefox = "firefox",
    Safari = "safari",
    Opera = "opera",
    SamsungInternet = "samsung_internet",
    Unknown = "unknown",
});
closed_enum!(Platform {
    Windows = "windows",
    Macos = "macos",
    Ios = "ios",
    Android = "android",
    Linux = "linux",
    Chromeos = "chromeos",
    Unknown = "unknown",
});

/// The client description captured when a session is issued. `browser` is `None` exactly when
/// `channel` is `Native`. Only these three closed values are stored: no raw header, version,
/// model, client hint or address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ClientLabel {
    pub channel: Channel,
    pub browser: Option<Browser>,
    pub platform: Platform,
}

impl ClientLabel {
    /// The label stored in three nullable columns, or `None` for a legacy (pre-`1005`) row. A
    /// combination the writer never produces is also `None`: this is display metadata, so a
    /// damaged row degrades to "unknown" rather than failing a listing.
    pub fn from_columns(
        channel: Option<String>,
        browser: Option<String>,
        platform: Option<String>,
    ) -> Option<Self> {
        let channel = Channel::parse(&channel?)?;
        let platform = Platform::parse(&platform?)?;
        let browser = match (channel, browser) {
            (Channel::Native, None) => None,
            (Channel::Browser, Some(browser)) => Some(Browser::parse(&browser)?),
            _ => return None,
        };
        Some(Self {
            channel,
            browser,
            platform,
        })
    }
}

/// Used and remaining slots under one enforced limit, read from the same rows the enforcement
/// counts. `limit: None` means not enforced (never produced by this release). A count is advisory:
/// it reserves nothing, and a concurrent sign-in or registration can take the last slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Capacity {
    pub limit: Option<i64>,
    pub used: i64,
    pub remaining: Option<i64>,
}

impl Capacity {
    pub fn of(limit: Option<i64>, used: i64) -> Self {
        Self {
            limit,
            used,
            remaining: limit.map(|limit| (limit - used).max(0)),
        }
    }
}

/// Code points refused in a device name besides controls: bidirectional overrides, embeddings
/// and isolates, invisible separators and joiners other than ZWJ/ZWNJ, the byte-order mark and
/// interlinear annotation characters. Each can make a name render as something it is not.
fn invisible_or_bidi(c: char) -> bool {
    matches!(
        c,
        '\u{061C}'
            | '\u{200B}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
    )
}

fn noncharacter(c: char) -> bool {
    let c = c as u32;
    (0xFDD0..=0xFDEF).contains(&c) || c & 0xFFFE == 0xFFFE
}

/// A user-chosen device name: 1–64 Unicode scalar values (so at most 256 UTF-8 bytes), no leading
/// or trailing white space, and no control, bidirectional-control, invisible or noncharacter code
/// point. Nothing is normalised or trimmed: what is accepted is stored and returned exactly.
pub fn valid_device_name(name: &str) -> bool {
    let count = name.chars().count();
    (1..=64).contains(&count)
        && !name.starts_with(char::is_whitespace)
        && !name.ends_with(char::is_whitespace)
        && !name
            .chars()
            .any(|c| c.is_control() || invisible_or_bidi(c) || noncharacter(c))
}

/// Where a membership decision, or a public listing, takes its time from (BE-Q25). It is read only
/// once the transaction holds the account's ordering point ([`order_membership`],
/// [`Store::begin_membership_read`]), never before a lock or pool wait, and again on every retried
/// attempt. Production passes the system clock (a `fn() -> i64`); a test passes a fixed `i64`.
pub trait Clock {
    fn now(&self) -> i64;
}

impl Clock for i64 {
    fn now(&self) -> i64 {
        *self
    }
}

impl<F: Fn() -> i64> Clock for F {
    fn now(&self) -> i64 {
        self()
    }
}

/// Proof that a transaction holds `account`'s membership ordering point, with the time read after
/// taking it (BE-Q25). Only [`order_membership`] makes one, and every profile helper below takes
/// one, so none of them can run unordered or judge membership by a time read before a wait.
#[derive(Debug)]
pub struct Ordered {
    account: String,
    now: i64,
}

impl Ordered {
    pub fn account(&self) -> &str {
        &self.account
    }

    /// The decision time: read after the ordering point was taken.
    pub fn now(&self) -> i64 {
        self.now
    }

    /// The continuity of a transaction that verified, and is consuming, a member of `device` that
    /// expires at `expires_at` (an activation grant being redeemed, a native handoff being
    /// exchanged): [`Continuity::Replaces`] only if that member is live at this transaction's own
    /// decision time, otherwise [`Continuity::Unknown`].
    pub fn replacing(&self, device: &str, expires_at: i64) -> Continuity {
        if expires_at > self.now {
            Continuity::Replaces(Replaced(device.to_owned()))
        } else {
            Continuity::Unknown
        }
    }
}

/// The device whose live member the transaction is replacing; made only by [`Ordered::replacing`].
#[derive(Debug, PartialEq, Eq)]
pub struct Replaced(String);

/// What a transaction that is about to give a device a live member knows about that device's
/// membership (BE-Q25).
#[derive(Debug, PartialEq, Eq)]
pub enum Continuity {
    /// Nothing: the device may have no live member, so this transaction may be listing a device
    /// that was forgotten, whose old profile must go first.
    Unknown,
    /// The transaction holds the account's ordering point, verified a member of this device live
    /// at its decision time and replaces it with the new one in the same transaction. The device is
    /// listed throughout, so it keeps its profile, although the consumed member is already gone by
    /// the time the profile helpers run.
    Replaces(Replaced),
}

/// Take `account`'s membership ordering point in `tx` and read the decision time after it
/// (BE-Q25). Every change to the account's device membership takes it before its first member
/// write: the session, grant, handoff, subscription and sync-registration writers, every
/// revocation, cancellation and retirement, and the aged-registration sweep. Public listings take
/// it shared ([`Store::begin_membership_read`]).
///
/// The point is the account row, through the same no-op update that already orders a retirement
/// against a sync registration. On PostgreSQL that row lock is held to commit, so a decision that
/// a device is still listed (and keeps its name) stays true until the new member commits: a
/// removal waits for it, or commits first and is then seen. A REPEATABLE READ caller whose
/// snapshot predates a change of the row fails with a serialisation error instead, retried as a
/// whole. On SQLite every writer already holds the database's single writer slot. Lock order
/// is the retirement's: `begin_serial()`, then this, then member rows; never the reverse.
pub async fn order_membership(
    tx: &mut Transaction<'_, Any>,
    account: &str,
    clock: &impl Clock,
) -> Result<Ordered> {
    let ordered = sqlx::query("UPDATE accounts SET access_epoch=access_epoch WHERE id=$1")
        .bind(account)
        .execute(&mut **tx)
        .await?;
    ensure!(ordered.rows_affected() == 1, ErrorCode::Unauthenticated);
    Ok(Ordered {
        account: account.to_owned(),
        now: clock.now(),
    })
}

/// Drops the account's profiles of devices with no live member at the decision time, except the
/// device being replaced. Every transaction that can give a device a live member calls this,
/// holding the ordering point, before its first member write: activation-grant issue,
/// native-handoff issue, a notification-subscription change, a sync registration and, through
/// [`record_client`], session issue. A device that was forgotten (retired, or every member expired,
/// revoked or swept) therefore starts unnamed whichever route lists it again, and a profile that
/// outlived its device (a removal leaves it for the next transition) is never shown.
///
/// Because the decision holds the ordering point to commit and every removal and public listing
/// takes the same point, the membership it relies on cannot disappear before the new member is
/// visible: a removal that commits first is seen here, and one that comes later finds the new
/// member. A listing that has seen the device absent read its time before this decision's, so
/// the member that justified keeping a profile would have to be live at a later time than one at
/// which nothing was: impossible, so the profile goes.
pub async fn forget_unlisted(
    tx: &mut Transaction<'_, Any>,
    ordered: &Ordered,
    continuity: &Continuity,
) -> Result<()> {
    match continuity {
        Continuity::Unknown => {
            sqlx::query(concat!(
                "DELETE FROM device_profiles WHERE account_id=$1 AND device_id NOT IN (",
                live_devices!(),
                ")"
            ))
            .bind(ordered.account())
            .bind(ordered.now())
            .execute(&mut **tx)
            .await?;
        }
        Continuity::Replaces(Replaced(device)) => {
            sqlx::query(concat!(
                "DELETE FROM device_profiles WHERE account_id=$1 AND device_id<>$3 AND device_id NOT IN (",
                live_devices!(),
                ")"
            ))
            .bind(ordered.account())
            .bind(ordered.now())
            .bind(device)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

/// Called by `sessions::issue` in its caller's ordered transaction, before the session row is
/// inserted: first [`forget_unlisted`], keeping `device` only when the caller is replacing one of
/// its live members, then records `client` as this device's latest label.
pub async fn record_client(
    tx: &mut Transaction<'_, Any>,
    ordered: &Ordered,
    device: &str,
    client: &ClientLabel,
    continuity: &Continuity,
) -> Result<()> {
    if let Continuity::Replaces(Replaced(replaced)) = continuity {
        ensure!(replaced == device, ErrorCode::InternalError);
    }
    forget_unlisted(tx, ordered, continuity).await?;
    sqlx::query("INSERT INTO device_profiles(account_id,device_id,name,client_channel,client_browser,client_platform,updated_at) VALUES ($1,$2,NULL,$3,$4,$5,$6) ON CONFLICT(account_id,device_id) DO UPDATE SET client_channel=excluded.client_channel,client_browser=excluded.client_browser,client_platform=excluded.client_platform,updated_at=excluded.updated_at")
        .bind(ordered.account())
        .bind(device)
        .bind(client.channel.as_str())
        .bind(client.browser.map(Browser::as_str))
        .bind(client.platform.as_str())
        .bind(ordered.now())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[derive(Serialize)]
pub struct DeviceSummary {
    pub sessions: i64,
    pub native_handoffs: i64,
    pub notification_subscriptions: i64,
    pub pending_sign_ins: i64,
    pub sync_registered: bool,
}

#[derive(Serialize)]
pub struct Device {
    pub id: String,
    pub last_synced_at: Option<i64>,
    pub active_sessions: i64,
    pub state_token: String,
    pub summary: DeviceSummary,
    /// The user-chosen name (BE-Q25). Display metadata only: not a member of the approved state,
    /// so never part of `state_token`.
    pub name: Option<String>,
    /// The client label of the device's most recently issued session, kept while the device is
    /// listed; `None` for a device that has not signed in since `1005`.
    pub client: Option<ClientLabel>,
}

/// One row of `GET /sessions` (BE-Q25: with its client label and its device's current name).
pub struct SessionRow {
    pub id: String,
    pub device_id: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub authentication: String,
    pub token_hash: String,
    pub client: Option<ClientLabel>,
    pub device_name: Option<String>,
}

/// `GET /devices`: the listing and the sync capacity, from one snapshot.
pub struct DeviceList {
    pub devices: Vec<Device>,
    pub sync_capacity: Capacity,
}

/// Whether `token` is a well-formed version-1 state token: `v1.` and the unpadded base64url of a
/// SHA-256 digest in its one canonical spelling. Anything else is refused, not compared.
pub fn valid_state_token(token: &str) -> bool {
    token.strip_prefix(TOKEN_PREFIX).is_some_and(|rest| {
        rest.len() == 43
            && URL_SAFE_NO_PAD
                .decode(rest)
                .is_ok_and(|bytes| bytes.len() == 32)
    })
}

/// The members of one device's approved state, each read from a single statement.
#[derive(Default)]
struct State {
    /// Component 1: live `session_id`s.
    sessions: Vec<String>,
    /// Component 2: live `code_hash`es.
    handoffs: Vec<String>,
    /// Component 3: active `(id, version)` pairs.
    subscriptions: Vec<(String, i64)>,
    /// Component 4: live `grant_id`s (BE-Q19).
    grants: Vec<String>,
    /// Component 5: the registration's `cursor_key`, when a registration exists.
    registration: Option<String>,
}

impl State {
    fn is_empty(&self) -> bool {
        self.sessions.is_empty()
            && self.handoffs.is_empty()
            && self.subscriptions.is_empty()
            && self.grants.is_empty()
            && self.registration.is_none()
    }

    /// `v1.` + base64url(SHA-256(canonical)). The canonical form length-prefixes the device ID so
    /// no two devices share a spelling, then lists one `<component> <identity>` line per member,
    /// sorted by component and then by identity *bytes* in Rust, so a database collation can never
    /// change a token. An empty state has no member lines.
    fn token(&self, device: &str) -> String {
        let mut lines: Vec<(u8, String)> = Vec::new();
        lines.extend(self.sessions.iter().map(|id| (1, id.clone())));
        lines.extend(self.handoffs.iter().map(|hash| (2, hash.clone())));
        lines.extend(
            self.subscriptions
                .iter()
                .map(|(id, version)| (3, format!("{id}@{version}"))),
        );
        lines.extend(self.grants.iter().map(|id| (4, id.clone())));
        if let Some(key) = &self.registration {
            lines.push((5, hex(&Sha256::digest(key.as_bytes()))));
        }
        lines.sort();
        let mut canonical = format!("atlas-device-state-v1\n{}:{device}\n", device.len());
        for (component, identity) in lines {
            canonical.push_str(&format!("{component} {identity}\n"));
        }
        format!(
            "{TOKEN_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
        )
    }

    fn summary(&self) -> DeviceSummary {
        DeviceSummary {
            sessions: self.sessions.len() as i64,
            native_handoffs: self.handoffs.len() as i64,
            notification_subscriptions: self.subscriptions.len() as i64,
            pending_sign_ins: self.grants.len() as i64,
            sync_registered: self.registration.is_some(),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl Store {
    /// The account's devices with the approved state of each, all read from one snapshot so what
    /// a confirmation dialog shows is exactly what its token covers. A device is listed when any
    /// member of its approved state exists (a sync registration, a live session, a live native
    /// handoff, an active subscription or a pending activation grant), so every device a
    /// retirement could act on offers a token, including one whose only remaining state is a
    /// pending grant.
    ///
    /// The same snapshot supplies each device's name and last client label (BE-Q25) and the sync
    /// capacity, whose `used` is every registration row of the account: exactly what a sync from
    /// an unregistered device counts, including aged rows the retention sweep has not yet removed.
    ///
    /// The listing takes the account's membership ordering point shared before it reads its time
    /// ([`Store::begin_membership_read`]), so a device it reports absent can never come back with
    /// the name it had (BE-Q25): every transition that could still keep that name decided before
    /// this listing's time and has already committed, and every later one decides after it.
    pub async fn devices(&self, actor: &str, clock: impl Clock) -> Result<DeviceList> {
        retry_unit(actor, || self.devices_once(actor, &clock)).await
    }

    async fn devices_once(&self, actor: &str, clock: &impl Clock) -> Result<DeviceList> {
        let mut tx = self.begin_membership_read(actor).await?;
        let now = clock.now();
        // The listing set is the union of the same five member predicates the components below
        // read, so it can never disagree with a token.
        let listed = sqlx::query(concat!(
            "SELECT d.device_id,s.last_seen,p.name,p.client_channel,p.client_browser,p.client_platform FROM (",
            live_devices!(),
            ") d LEFT JOIN sync_devices s ON s.account_id=$1 AND s.device_id=d.device_id LEFT JOIN device_profiles p ON p.account_id=$1 AND p.device_id=d.device_id ORDER BY d.device_id"
        ))
            .bind(actor)
            .bind(now)
            .fetch_all(&mut *tx)
            .await?;
        let mut states = BTreeMap::<String, State>::new();
        for row in sqlx::query(
            "SELECT device_id,session_id FROM sessions WHERE account_id=$1 AND expires_at>$2",
        )
        .bind(actor)
        .bind(now)
        .fetch_all(&mut *tx)
        .await?
        {
            states
                .entry(row.get(0))
                .or_default()
                .sessions
                .push(row.get(1));
        }
        for row in sqlx::query(
            "SELECT device_id,code_hash FROM native_handoffs WHERE account_id=$1 AND expires_at>$2",
        )
        .bind(actor)
        .bind(now)
        .fetch_all(&mut *tx)
        .await?
        {
            states
                .entry(row.get(0))
                .or_default()
                .handoffs
                .push(row.get(1));
        }
        hook!(self, "devices.between_reads");
        for row in sqlx::query("SELECT device_id,id,version FROM notification_subscriptions WHERE account_id=$1 AND active=1")
            .bind(actor)
            .fetch_all(&mut *tx)
            .await?
        {
            states
                .entry(row.get(0))
                .or_default()
                .subscriptions
                .push((row.get(1), row.get(2)));
        }
        for row in sqlx::query(
            "SELECT device_id,grant_id FROM activation_grants WHERE account_id=$1 AND state='issued' AND expires_at>$2",
        )
        .bind(actor)
        .bind(now)
        .fetch_all(&mut *tx)
        .await?
        {
            states.entry(row.get(0)).or_default().grants.push(row.get(1));
        }
        let registrations =
            sqlx::query("SELECT device_id,cursor_key FROM sync_devices WHERE account_id=$1")
                .bind(actor)
                .fetch_all(&mut *tx)
                .await?;
        let sync_capacity = Capacity::of(Some(SYNC_DEVICE_LIMIT), registrations.len() as i64);
        for row in registrations {
            states.entry(row.get(0)).or_default().registration = Some(row.get(1));
        }
        tx.commit().await?;
        let devices = listed
            .iter()
            .map(|row| {
                let id: String = row.get(0);
                let state = states.remove(&id).unwrap_or_default();
                Device {
                    last_synced_at: row.get(1),
                    active_sessions: state.sessions.len() as i64,
                    state_token: state.token(&id),
                    summary: state.summary(),
                    name: row.get(2),
                    client: ClientLabel::from_columns(row.get(3), row.get(4), row.get(5)),
                    id,
                }
            })
            .collect();
        Ok(DeviceList {
            devices,
            sync_capacity,
        })
    }

    /// The transaction a public listing of `account`'s devices or sessions reads in (BE-Q25). It
    /// first takes the account's membership ordering point *shared*: it waits for any membership
    /// change of the account in flight, and every later change waits for it. The caller reads its
    /// clock only after this returns, so no transition that decided before that time can commit
    /// after the listing's snapshot, and none ordered after the listing can have decided earlier.
    ///
    /// PostgreSQL: REPEATABLE READ, first statement `FOR SHARE` on the account row (so not
    /// `READ ONLY`). The snapshot is taken as that statement starts; a membership change that
    /// commits after it, or that the lock waited for, fails the lock with a serialisation error,
    /// retried as a whole with a new snapshot. SQLite: `BEGIN IMMEDIATE`, the writer slot every
    /// membership change also holds; nothing can commit between the time and the snapshot.
    pub async fn begin_membership_read(&self, account: &str) -> Result<Transaction<'static, Any>> {
        if self.sqlite {
            return Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?);
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SELECT id FROM accounts WHERE id=$1 FOR SHARE")
            .bind(account)
            .fetch_optional(&mut *tx)
            .await?;
        Ok(tx)
    }

    /// `GET /sessions`: the account's live sessions, each with its device's current name, read in
    /// one statement after the account's membership ordering point (BE-Q25), so the rows listed are
    /// exactly the rows `sessions::issue` counts after its expiry sweep and the names follow the
    /// same lifetime as [`Store::devices`].
    pub async fn sessions(&self, actor: &str, clock: impl Clock) -> Result<Vec<SessionRow>> {
        retry_unit(actor, || self.sessions_once(actor, &clock)).await
    }

    async fn sessions_once(&self, actor: &str, clock: &impl Clock) -> Result<Vec<SessionRow>> {
        let mut tx = self.begin_membership_read(actor).await?;
        let now = clock.now();
        let rows = sqlx::query("SELECT s.session_id,s.device_id,s.created_at,s.expires_at,s.auth_kind,s.token_hash,s.client_channel,s.client_browser,s.client_platform,p.name FROM sessions s LEFT JOIN device_profiles p ON p.account_id=s.account_id AND p.device_id=s.device_id WHERE s.account_id=$1 AND s.expires_at>$2 ORDER BY s.created_at,s.session_id")
            .bind(actor)
            .bind(now)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows
            .iter()
            .map(|row| SessionRow {
                id: row.get(0),
                device_id: row.get(1),
                created_at: row.get(2),
                expires_at: row.get(3),
                authentication: row.get(4),
                token_hash: row.get(5),
                client: ClientLabel::from_columns(row.get(6), row.get(7), row.get(8)),
                device_name: row.get(9),
            })
            .collect())
    }

    /// Set (`Some`) or clear (`None`) the name of one of the account's devices (BE-Q25). `NotFound`
    /// unless the device is listed at the decision time, so a retired, expired or other account's
    /// device ID cannot gain a name. One `begin_serial()` transaction holding the account's
    /// membership ordering point, totally ordered with a retirement and with every membership
    /// change: a rename either commits before a retirement (whose token it does not change, and
    /// which then deletes the profile) or runs after it and finds nothing listed. The time is read
    /// after both, on every attempt.
    pub async fn set_device_name(
        &self,
        actor: &str,
        device: &str,
        name: Option<&str>,
        clock: impl Clock,
    ) -> Result<()> {
        ensure!(
            !device.is_empty() && device.len() <= 100,
            ErrorCode::InvalidValue
        );
        ensure!(name.is_none_or(valid_device_name), ErrorCode::InvalidValue);
        retry_unit(device, || self.set_name_once(actor, device, name, &clock)).await
    }

    async fn set_name_once(
        &self,
        actor: &str,
        device: &str,
        name: Option<&str>,
        clock: &impl Clock,
    ) -> Result<()> {
        let mut tx = self.begin_serial().await?;
        let ordered = order_membership(&mut tx, actor, clock).await?;
        let listed: i64 = sqlx::query_scalar(concat!(
            "SELECT COUNT(*) FROM (",
            live_devices!(),
            ") d WHERE d.device_id=$3"
        ))
        .bind(actor)
        .bind(ordered.now())
        .bind(device)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(listed == 1, ErrorCode::NotFound);
        match name {
            Some(name) => {
                sqlx::query("INSERT INTO device_profiles(account_id,device_id,name,updated_at) VALUES ($1,$2,$3,$4) ON CONFLICT(account_id,device_id) DO UPDATE SET name=excluded.name,updated_at=excluded.updated_at")
                    .bind(actor)
                    .bind(device)
                    .bind(name)
                    .bind(ordered.now())
                    .execute(&mut *tx)
                    .await?;
            }
            None => {
                sqlx::query("UPDATE device_profiles SET name=NULL,updated_at=$3 WHERE account_id=$1 AND device_id=$2")
                    .bind(actor)
                    .bind(device)
                    .bind(ordered.now())
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Retire `device` if, and only if, its approved state still equals the one `state_token`
    /// names, and record the outcome durably under `operation_id`.
    ///
    /// One serialised transaction, retried as a unit on a retryable conflict:
    /// 1. ledger first: a recorded outcome for this ID is replayed without evaluating anything;
    /// 2. the account's membership ordering point ([`order_membership`]) orders this against
    ///    sync-registration creation and every other membership change of the account;
    /// 3. the members are read again, `FOR UPDATE` on PostgreSQL, so every row compared is locked;
    /// 4. no member is `Superseded`; a different state is `RejectedStale` (nothing changes);
    /// 5. otherwise exactly the locked identities are deleted or deactivated, each statement's
    ///    rows-affected asserted, and any discrepancy rolls everything back without a ledger row;
    /// 6. the ledger row commits with the effects or with the rejection.
    ///
    /// `RejectedStale` is a committed answer, not an error: it needs its row to survive.
    pub async fn retire_device(
        &self,
        actor: &str,
        device: &str,
        operation_id: &str,
        state_token: &str,
        now: i64,
    ) -> Result<Operation> {
        ensure!(
            !device.is_empty() && device.len() <= 100,
            ErrorCode::InvalidValue
        );
        identifier(operation_id)?;
        ensure!(valid_state_token(state_token), ErrorCode::InvalidValue);
        let digest = request_digest(OperationKind::RetireDevice, device, Some(state_token));
        hook!(self, "retire.before_begin");
        retry_unit(device, || {
            self.retire_once(actor, device, operation_id, state_token, &digest, now)
        })
        .await
    }

    async fn retire_once(
        &self,
        actor: &str,
        device: &str,
        operation_id: &str,
        state_token: &str,
        digest: &str,
        now: i64,
    ) -> Result<Operation> {
        hook!(self, "retire.attempt");
        let mut tx = self.begin_retirement().await?;
        if let Some(recorded) = Self::ledger_read(&mut tx, actor, operation_id).await? {
            return Self::replay(actor, operation_id, digest, recorded);
        }
        hook!(self, "retire.after_ledger_read", &mut tx);
        // The account's membership ordering point (BE-Q25), the same no-op account update that
        // has always ordered a retirement against a sync registration; `Unauthenticated` when the
        // account is gone. A retirement only removes members, so its time stays the caller's.
        order_membership(&mut tx, actor, &now).await?;
        let state = self.locked_state(&mut tx, actor, device, now).await?;
        hook!(self, "retire.after_locking_reads", &mut tx);
        let outcome = if state.is_empty() {
            Outcome::Superseded
        } else if state.token(device) != state_token {
            Outcome::RejectedStale
        } else {
            Outcome::ConfirmedApplied
        };
        hook!(self, "retire.before_effects", &mut tx);
        if outcome == Outcome::ConfirmedApplied {
            Self::retire_members(&mut tx, actor, device, &state, now).await?;
        }
        hook!(self, "retire.before_commit", &mut tx);
        Self::ledger_write(
            &mut tx,
            actor,
            operation_id,
            OperationKind::RetireDevice,
            digest,
            outcome,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(Operation {
            operation_id: operation_id.to_owned(),
            account_id: actor.to_owned(),
            outcome,
        })
    }

    /// Read every member in component order and, by identity within a component, `FOR UPDATE` on
    /// PostgreSQL, holding the locks to commit. On SQLite the write lock taken by `begin_serial`
    /// already excludes every other writer, and the clause is not supported.
    async fn locked_state(
        &self,
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        device: &str,
        now: i64,
    ) -> Result<State> {
        let lock = if self.locks_rows() { " FOR UPDATE" } else { "" };
        // Every statement is repository-owned; only static text is interpolated.
        let sessions = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT session_id FROM sessions WHERE account_id=$1 AND device_id=$2 AND expires_at>$3 ORDER BY session_id{lock}")))
            .bind(actor).bind(device).bind(now).fetch_all(&mut **tx).await?;
        let handoffs = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT code_hash FROM native_handoffs WHERE account_id=$1 AND device_id=$2 AND expires_at>$3 ORDER BY code_hash{lock}")))
            .bind(actor).bind(device).bind(now).fetch_all(&mut **tx).await?;
        let subscriptions = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT id,version FROM notification_subscriptions WHERE account_id=$1 AND device_id=$2 AND active=1 ORDER BY id{lock}")))
            .bind(actor).bind(device).fetch_all(&mut **tx).await?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        let grants = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT grant_id FROM activation_grants WHERE account_id=$1 AND device_id=$2 AND state='issued' AND expires_at>$3 ORDER BY grant_id{lock}")))
            .bind(actor).bind(device).bind(now).fetch_all(&mut **tx).await?;
        let registration = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT cursor_key FROM sync_devices WHERE account_id=$1 AND device_id=$2{lock}"
        )))
        .bind(actor)
        .bind(device)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(State {
            sessions,
            handoffs,
            subscriptions,
            grants,
            registration,
        })
    }

    /// Delete, deactivate and cancel exactly the locked identities. Each statement's
    /// rows-affected must equal the number of members it locked; a mismatch means a writer
    /// escaped the protocol, so this fails and the caller's transaction rolls back unrecorded.
    async fn retire_members(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        device: &str,
        state: &State,
        now: i64,
    ) -> Result<()> {
        Self::delete_identities(tx, "sessions", "session_id", actor, &state.sessions).await?;
        Self::delete_identities(tx, "native_handoffs", "code_hash", actor, &state.handoffs).await?;
        for (id, version) in &state.subscriptions {
            let changed = sqlx::query("UPDATE notification_subscriptions SET active=0,secret='',version=version+1 WHERE id=$1 AND account_id=$2 AND active=1 AND version=$3")
                .bind(id).bind(actor).bind(version).execute(&mut **tx).await?;
            ensure!(changed.rows_affected() == 1, ErrorCode::InternalError);
        }
        // Already-inactive subscriptions are not members (`locked_state` never reads them), so
        // this is uncounted cleanup, exactly like the `sync_cursors` deletion below: it closes
        // CH2 — a subscription `SetSubscription` deactivated for an unrelated reason could still
        // be holding a secret indefinitely — so every subscription row tied to this device ends
        // up secret-erased, matching docs/notifications.md's existing claim.
        sqlx::query("UPDATE notification_subscriptions SET secret='',version=version+1 WHERE account_id=$1 AND device_id=$2 AND active=0 AND secret<>''")
            .bind(actor)
            .bind(device)
            .execute(&mut **tx)
            .await?;
        Self::cancel_grants(tx, actor, device, &state.grants, now).await?;
        if let Some(key) = &state.registration {
            let removed = sqlx::query(
                "DELETE FROM sync_devices WHERE account_id=$1 AND device_id=$2 AND cursor_key=$3",
            )
            .bind(actor)
            .bind(device)
            .bind(key)
            .execute(&mut **tx)
            .await?;
            ensure!(removed.rows_affected() == 1, ErrorCode::InternalError);
        }
        // Cursor churn is deleted with the device but is not a member, so it is not counted.
        sqlx::query("DELETE FROM sync_cursors WHERE account_id=$1 AND device_id=$2")
            .bind(actor)
            .bind(device)
            .execute(&mut **tx)
            .await?;
        // So is the device's name and label (BE-Q25): a device that signs in again after its
        // retirement is a new incarnation and starts unnamed.
        sqlx::query("DELETE FROM device_profiles WHERE account_id=$1 AND device_id=$2")
            .bind(actor)
            .bind(device)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    async fn delete_identities(
        tx: &mut Transaction<'_, Any>,
        table: &'static str,
        column: &'static str,
        actor: &str,
        identities: &[String],
    ) -> Result<()> {
        let mut removed = 0;
        for chunk in identities.chunks(500) {
            let placeholders = (0..chunk.len())
                .map(|i| format!("${}", i + 2))
                .collect::<Vec<_>>()
                .join(",");
            // Table and column are static names; every identity is bound.
            let statement =
                format!("DELETE FROM {table} WHERE account_id=$1 AND {column} IN ({placeholders})");
            let mut delete = sqlx::query(sqlx::AssertSqlSafe(statement)).bind(actor);
            for identity in chunk {
                delete = delete.bind(identity);
            }
            removed += delete.execute(&mut **tx).await?.rows_affected() as usize;
        }
        ensure!(removed == identities.len(), ErrorCode::InternalError);
        Ok(())
    }

    /// Cancel exactly the locked `issued` grants. An `UPDATE`, not a `DELETE`, like
    /// `delete_identities`: the row stays as evidence for a later `activate`/`activate/cancel`.
    async fn cancel_grants(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        device: &str,
        grants: &[String],
        now: i64,
    ) -> Result<()> {
        let mut cancelled = 0;
        for chunk in grants.chunks(500) {
            let placeholders = (0..chunk.len())
                .map(|i| format!("${}", i + 4))
                .collect::<Vec<_>>()
                .join(",");
            let statement = format!(
                "UPDATE activation_grants SET state='cancelled',cancelled_at=$3 WHERE account_id=$1 AND device_id=$2 AND state='issued' AND grant_id IN ({placeholders})"
            );
            let mut update = sqlx::query(sqlx::AssertSqlSafe(statement))
                .bind(actor)
                .bind(device)
                .bind(now);
            for grant in chunk {
                update = update.bind(grant);
            }
            cancelled += update.execute(&mut **tx).await?.rows_affected() as usize;
        }
        ensure!(cancelled == grants.len(), ErrorCode::InternalError);
        Ok(())
    }

    /// Revoke one session by ID and record the outcome under `operation_id`, ordered against a
    /// retirement by the same serialisation point. Never `RejectedStale`: logout has no
    /// precondition. An expired-but-unswept row is not credited to this call.
    pub async fn revoke_session(
        &self,
        actor: &str,
        session_id: &str,
        operation_id: &str,
        now: i64,
    ) -> Result<Operation> {
        identifier(session_id)?;
        identifier(operation_id)?;
        let digest = request_digest(OperationKind::RevokeSession, session_id, None);
        retry_unit(session_id, || {
            self.revoke_once(actor, session_id, operation_id, &digest, now)
        })
        .await
    }

    async fn revoke_once(
        &self,
        actor: &str,
        session_id: &str,
        operation_id: &str,
        digest: &str,
        now: i64,
    ) -> Result<Operation> {
        hook!(self, "revoke.attempt");
        let mut tx = self.begin_serial().await?;
        // Replay precedes any target lookup, so it survives the target's absence.
        if let Some(recorded) = Self::ledger_read(&mut tx, actor, operation_id).await? {
            return Self::replay(actor, operation_id, digest, recorded);
        }
        // A removal of a member, so ordered with every membership decision (BE-Q25).
        order_membership(&mut tx, actor, &now).await?;
        let removed = sqlx::query(
            "DELETE FROM sessions WHERE session_id=$1 AND account_id=$2 AND expires_at>$3",
        )
        .bind(session_id)
        .bind(actor)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let outcome = if removed.rows_affected() == 1 {
            Outcome::ConfirmedApplied
        } else {
            Outcome::Superseded
        };
        Self::ledger_write(
            &mut tx,
            actor,
            operation_id,
            OperationKind::RevokeSession,
            digest,
            outcome,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(Operation {
            operation_id: operation_id.to_owned(),
            account_id: actor.to_owned(),
            outcome,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// U2: what a device name may and may not be, including the character and byte boundaries.
    #[test]
    fn device_names_are_bounded_visible_text() {
        let emoji64 = "\u{1F600}".repeat(64);
        assert_eq!((emoji64.chars().count(), emoji64.len()), (64, 256));
        for accepted in [
            "Kieran's phone",
            "a",
            &"x".repeat(64),
            &emoji64,
            // ZWJ family emoji and a ZWNJ in Persian text stay allowed.
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} tablet",
            "\u{0645}\u{06CC}\u{200C}\u{062E}\u{0648}\u{0627}\u{0647}\u{0645}",
            "\u{5BB6}\u{65CF}\u{306E}iPad",
            "<script>alert(1)</script>",
            "tab\u{3000}inside ok",
        ] {
            assert!(valid_device_name(accepted), "{accepted:?}");
        }
        for refused in [
            String::new(),
            "x".repeat(65),
            "\u{1F600}".repeat(65),
            " leading".into(),
            "trailing ".into(),
            "\u{3000}ideographic space".into(),
            "line\nbreak".into(),
            "nul\0".into(),
            "tab\tinside".into(),
            "c1\u{0085}".into(),
            "rtl \u{202E}override".into(),
            "isolate \u{2066}x\u{2069}".into(),
            "zero\u{200B}width".into(),
            "bom\u{FEFF}".into(),
            "line\u{2028}separator".into(),
            "arabic\u{061C}mark".into(),
            "word\u{2060}joiner".into(),
            "annotation\u{FFF9}".into(),
            "non\u{FFFF}char".into(),
            "non\u{FDD0}char".into(),
            "plane\u{1FFFE}".into(),
        ] {
            assert!(!valid_device_name(&refused), "{refused:?}");
        }
    }

    #[test]
    fn capacity_is_never_negative_and_unlimited_has_no_remaining() {
        assert_eq!(
            Capacity::of(Some(32), 31),
            Capacity {
                limit: Some(32),
                used: 31,
                remaining: Some(1)
            }
        );
        assert_eq!(Capacity::of(Some(32), 40).remaining, Some(0));
        assert_eq!(Capacity::of(None, 7).remaining, None);
    }

    #[test]
    fn stored_labels_round_trip_and_inconsistent_rows_are_unknown() {
        let text = |v: &str| Some(v.to_owned());
        assert_eq!(
            ClientLabel::from_columns(text("browser"), text("samsung_internet"), text("android")),
            Some(ClientLabel {
                channel: Channel::Browser,
                browser: Some(Browser::SamsungInternet),
                platform: Platform::Android
            })
        );
        assert_eq!(
            ClientLabel::from_columns(text("native"), None, text("ios")),
            Some(ClientLabel {
                channel: Channel::Native,
                browser: None,
                platform: Platform::Ios
            })
        );
        assert_eq!(ClientLabel::from_columns(None, None, None), None);
        assert_eq!(
            ClientLabel::from_columns(text("native"), text("chrome"), text("ios")),
            None
        );
        assert_eq!(
            ClientLabel::from_columns(text("browser"), None, text("ios")),
            None
        );
        assert_eq!(
            ClientLabel::from_columns(text("browser"), text("netscape"), text("ios")),
            None
        );
        for browser in [
            Browser::Chrome,
            Browser::Edge,
            Browser::Firefox,
            Browser::Safari,
            Browser::Opera,
            Browser::SamsungInternet,
            Browser::Unknown,
        ] {
            assert_eq!(Browser::parse(browser.as_str()), Some(browser));
            assert_eq!(
                serde_json::to_value(browser).unwrap(),
                serde_json::json!(browser.as_str())
            );
        }
    }
}
