//! Atlas domain services with shared policies, durable commands and offline synchronisation.
use crate::error::ErrorCode;
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Any, AnyPool, Row, Transaction};
use std::collections::{BTreeMap, BTreeSet};
pub mod accounts;
pub mod calendars;
pub mod devices;
pub mod error;
pub mod households;
pub mod people;
pub mod policy;
pub mod resources;
mod storage;
mod sync;
mod sync_cursor;
pub mod tasks;
use uuid::Uuid;

#[derive(Clone)]
pub struct Store {
    pub pool: AnyPool,
    sqlite: bool,
    retention_seconds: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Projection {
    pub id: String,
    pub kind: String,
    pub parent_id: Option<String>,
    pub label: String,
    pub value: serde_json::Value,
    pub version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_version: Option<i64>,
    pub can_edit: bool,
    #[serde(default)]
    pub archived: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Upsert { resource: Projection },
    Remove { id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Batch {
    pub revision: i64,
    pub changes: Vec<Change>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Page {
    pub phase: String,
    pub batches: Vec<Batch>,
    pub next_cursor: String,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    CreatePerson {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initial_policy: Option<policy::Policy>,
    },
    CreateField {
        id: String,
        person_id: String,
        label: String,
        value: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initial_policy: Option<policy::Policy>,
    },
    Edit {
        id: String,
        expected_version: i64,
        label: String,
        value: String,
    },
    Grant {
        id: String,
        expected_version: i64,
        account_id: String,
        edit: bool,
    },
    Revoke {
        id: String,
        expected_version: i64,
        account_id: String,
    },
}

const RESOURCE_ORDER: [&str; 11] = [
    "calendar_source",
    "event",
    "person",
    "task",
    "list",
    "field",
    "execution",
    "occurrence",
    "progress",
    "review",
    "reminder",
];

fn identifier(id: &str) -> Result<()> {
    let parsed = Uuid::parse_str(id).map_err(|_| anyhow!(ErrorCode::InvalidValue))?;
    ensure!(parsed.to_string() == id, ErrorCode::InvalidValue);
    Ok(())
}
fn content(label: &str, value: &str) -> Result<()> {
    ensure!(
        !label.trim().is_empty() && label.chars().count() <= 300 && value.chars().count() <= 8192,
        ErrorCode::InvalidValue
    );
    Ok(())
}

fn projection_row(row: &sqlx::any::AnyRow) -> Result<Projection> {
    let raw: String = row.get(4);
    let value = if raw.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&raw)?
    };
    Ok(Projection {
        id: row.get(0),
        kind: row.get(1),
        parent_id: row.get(2),
        label: row.get(3),
        value,
        version: row.get(5),
        policy_version: row.get(6),
        can_edit: row.get::<i64, _>(7) == 1,
        archived: row.get::<i64, _>(8) == 1,
    })
}

#[derive(Serialize, Deserialize)]
struct Notice {
    id: String,
    resource_kind: String,
}
impl Notice {
    fn from_change(change: &Change) -> Self {
        match change {
            Change::Upsert { resource } => Self {
                id: resource.id.clone(),
                resource_kind: resource.kind.clone(),
            },
            // Removal ordering is resolved from the resource row; resources aren't deleted yet.
            Change::Remove { id } => Self {
                id: id.clone(),
                resource_kind: String::new(),
            },
        }
    }
}
fn receipt_digest(payload: &str) -> String {
    Sha256::digest(format!("atlas-command-v1\n{payload}"))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn unix_now() -> Result<i64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs()
        .try_into()?)
}
