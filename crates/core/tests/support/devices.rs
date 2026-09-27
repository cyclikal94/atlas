//! Device fixtures and an *independent* implementation of the approved-state token.
//!
//! The token is computed here from the tables and the specification alone (never from
//! `atlas_core`'s own listing), so a test that compares the two is a real check of the canonical
//! form, and a test can obtain a token for a device the listing does not show.
use anyhow::Result;
use atlas_core::{Store, operations::Operation};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(crate) const FAR: i64 = 9_999_999_999;

pub(crate) fn id() -> String {
    Uuid::new_v4().to_string()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The specification's canonical form: device ID length-prefixed, then one line per member.
pub(crate) fn token_of(device: &str, mut lines: Vec<(u8, String)>) -> String {
    lines.sort();
    let mut canonical = format!("atlas-device-state-v1\n{}:{device}\n", device.len());
    for (component, identity) in lines {
        canonical.push_str(&format!("{component} {identity}\n"));
    }
    format!(
        "v1.{}",
        URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
    )
}

/// Read the device's members straight from the tables (live rows only) and hash them.
pub(crate) async fn expected_token(
    store: &Store,
    actor: &str,
    device: &str,
    now: i64,
) -> Result<String> {
    let mut lines = Vec::new();
    for session in sqlx::query_scalar::<_, String>(
        "SELECT session_id FROM sessions WHERE account_id=$1 AND device_id=$2 AND expires_at>$3",
    )
    .bind(actor)
    .bind(device)
    .bind(now)
    .fetch_all(&store.pool)
    .await?
    {
        lines.push((1, session));
    }
    for handoff in sqlx::query_scalar::<_, String>(
        "SELECT code_hash FROM native_handoffs WHERE account_id=$1 AND device_id=$2 AND expires_at>$3",
    )
    .bind(actor)
    .bind(device)
    .bind(now)
    .fetch_all(&store.pool)
    .await?
    {
        lines.push((2, handoff));
    }
    for (subscription, version) in sqlx::query_as::<_, (String, i64)>(
        "SELECT id,version FROM notification_subscriptions WHERE account_id=$1 AND device_id=$2 AND active=1",
    )
    .bind(actor)
    .bind(device)
    .fetch_all(&store.pool)
    .await?
    {
        lines.push((3, format!("{subscription}@{version}")));
    }
    if let Some(key) = sqlx::query_scalar::<_, String>(
        "SELECT cursor_key FROM sync_devices WHERE account_id=$1 AND device_id=$2",
    )
    .bind(actor)
    .bind(device)
    .fetch_optional(&store.pool)
    .await?
    {
        lines.push((5, hex(&Sha256::digest(key.as_bytes()))));
    }
    Ok(token_of(device, lines))
}

/// Retire with a fresh operation ID and the token of the device's current state.
pub(crate) async fn retire(
    store: &Store,
    actor: &str,
    device: &str,
    now: i64,
) -> Result<Operation> {
    let token = expected_token(store, actor, device, now).await?;
    store.retire_device(actor, device, &id(), &token, now).await
}

pub(crate) async fn add_session(
    store: &Store,
    actor: &str,
    device: &str,
    expires_at: i64,
) -> Result<String> {
    let session = id();
    sqlx::query("INSERT INTO sessions(token_hash,account_id,device_id,expires_at,session_id,created_at,auth_kind) VALUES ($1,$2,$3,$4,$5,1,'local')")
        .bind(id()).bind(actor).bind(device).bind(expires_at).bind(&session)
        .execute(&store.pool).await?;
    Ok(session)
}

pub(crate) async fn add_session_named(
    store: &Store,
    actor: &str,
    device: &str,
    session: &str,
    expires_at: i64,
) -> Result<()> {
    sqlx::query("INSERT INTO sessions(token_hash,account_id,device_id,expires_at,session_id,created_at,auth_kind) VALUES ($1,$2,$3,$4,$5,1,'local')")
        .bind(id()).bind(actor).bind(device).bind(expires_at).bind(session)
        .execute(&store.pool).await?;
    Ok(())
}

pub(crate) async fn add_handoff(
    store: &Store,
    actor: &str,
    device: &str,
    expires_at: i64,
) -> Result<String> {
    let hash = id();
    sqlx::query("INSERT INTO native_handoffs(code_hash,account_id,device_id,challenge,configuration_hash,redirect_uri,expires_at) VALUES ($1,$2,$3,'c','h','r',$4)")
        .bind(&hash).bind(actor).bind(device).bind(expires_at)
        .execute(&store.pool).await?;
    Ok(hash)
}

pub(crate) async fn add_subscription(store: &Store, actor: &str, device: &str) -> Result<String> {
    let subscription = id();
    sqlx::query("INSERT INTO notification_subscriptions(id,account_id,device_id,transport,secret,version,active) VALUES ($1,$2,$3,'webpush','s',1,1)")
        .bind(&subscription).bind(actor).bind(device)
        .execute(&store.pool).await?;
    Ok(subscription)
}

pub(crate) async fn add_registration(store: &Store, actor: &str, device: &str) -> Result<String> {
    let key = id();
    sqlx::query(
        "INSERT INTO sync_devices(account_id,device_id,last_seen,cursor_key) VALUES ($1,$2,1,$3)",
    )
    .bind(actor)
    .bind(device)
    .bind(&key)
    .execute(&store.pool)
    .await?;
    Ok(key)
}

/// Counts of every row a retirement acts on, for before/after equality on a stale rejection.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) sessions: Vec<String>,
    pub(crate) handoffs: Vec<String>,
    pub(crate) subscriptions: Vec<(String, i64, i64, String)>,
    pub(crate) registrations: Vec<String>,
    pub(crate) cursors: i64,
}

pub(crate) async fn snapshot(store: &Store, actor: &str, device: &str) -> Result<Snapshot> {
    let list = |sql: &'static str| {
        let (pool, actor, device) = (store.pool.clone(), actor.to_owned(), device.to_owned());
        async move {
            sqlx::query_scalar::<_, String>(sql)
                .bind(actor)
                .bind(device)
                .fetch_all(&pool)
                .await
        }
    };
    Ok(Snapshot {
        sessions: list("SELECT session_id FROM sessions WHERE account_id=$1 AND device_id=$2 ORDER BY session_id").await?,
        handoffs: list("SELECT code_hash FROM native_handoffs WHERE account_id=$1 AND device_id=$2 ORDER BY code_hash").await?,
        subscriptions: sqlx::query_as::<_, (String, i64, i64, String)>("SELECT id,version,active,secret FROM notification_subscriptions WHERE account_id=$1 AND device_id=$2 ORDER BY id")
            .bind(actor).bind(device).fetch_all(&store.pool).await?,
        registrations: list("SELECT cursor_key FROM sync_devices WHERE account_id=$1 AND device_id=$2").await?,
        cursors: sqlx::query_scalar("SELECT COUNT(*) FROM sync_cursors WHERE account_id=$1 AND device_id=$2")
            .bind(actor).bind(device).fetch_one(&store.pool).await?,
    })
}

pub(crate) async fn ledger_rows(
    store: &Store,
    actor: &str,
) -> Result<Vec<(String, String, String)>> {
    Ok(sqlx::query_as::<_, (String, String, String)>(
        "SELECT operation_id,kind,outcome FROM operation_outcomes WHERE account_id=$1 ORDER BY operation_id",
    )
    .bind(actor)
    .fetch_all(&store.pool)
    .await?)
}

/// A device holding every kind of member: two sessions, a handoff, a subscription, a registration.
pub(crate) struct Members {
    pub(crate) sessions: Vec<String>,
    pub(crate) handoff: String,
    pub(crate) subscription: String,
    pub(crate) key: String,
}

pub(crate) async fn populate(store: &Store, actor: &str, device: &str) -> Result<Members> {
    let mut sessions = vec![
        add_session(store, actor, device, FAR).await?,
        add_session(store, actor, device, FAR).await?,
    ];
    sessions.sort();
    Ok(Members {
        sessions,
        handoff: add_handoff(store, actor, device, FAR).await?,
        subscription: add_subscription(store, actor, device).await?,
        key: add_registration(store, actor, device).await?,
    })
}

/// Remove every trace of the device, as another operation retiring it would.
pub(crate) async fn clear(store: &Store, actor: &str, device: &str) -> Result<()> {
    for sql in [
        "DELETE FROM sessions WHERE account_id=$1 AND device_id=$2",
        "DELETE FROM native_handoffs WHERE account_id=$1 AND device_id=$2",
        "DELETE FROM notification_subscriptions WHERE account_id=$1 AND device_id=$2",
        "DELETE FROM sync_cursors WHERE account_id=$1 AND device_id=$2",
        "DELETE FROM sync_devices WHERE account_id=$1 AND device_id=$2",
    ] {
        sqlx::query(sql)
            .bind(actor)
            .bind(device)
            .execute(&store.pool)
            .await?;
    }
    Ok(())
}
