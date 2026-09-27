//! Durable per-operation outcomes.
//!
//! Device retirement and keyed session revocation each commit one `operation_outcomes` row in
//! the same transaction as their effects (or as their rejection). The row is what lets a client
//! whose response was lost, or whose session the operation itself revoked, learn what happened:
//! an absent row is "not yet resolved", never confirmation.
use super::*;
use crate::error::ErrorCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// This operation's own effects committed.
    ConfirmedApplied,
    /// Retirement only: the recomputed approved state differed from the approved one and
    /// something remained or was added. Nothing was changed.
    RejectedStale,
    /// Nothing was left for this operation to act on.
    Superseded,
}

impl Outcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConfirmedApplied => "confirmed_applied",
            Self::RejectedStale => "rejected_stale",
            Self::Superseded => "superseded",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "confirmed_applied" => Self::ConfirmedApplied,
            "rejected_stale" => Self::RejectedStale,
            "superseded" => Self::Superseded,
            _ => return Err(anyhow!(ErrorCode::InternalError)),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationKind {
    RetireDevice,
    RevokeSession,
}

impl OperationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RetireDevice => "retire_device",
            Self::RevokeSession => "revoke_session",
        }
    }
}

/// What a call carrying an operation ID reports, on first evaluation and on every replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Operation {
    pub operation_id: String,
    pub account_id: String,
    pub outcome: Outcome,
}

/// Lower-case hex SHA-256 of the request an operation ID first answered, so that reusing the ID
/// for a different target or approved state is refused rather than replayed.
pub fn request_digest(kind: OperationKind, target: &str, state_token: Option<&str>) -> String {
    Sha256::digest(format!(
        "atlas-operation-v1\n{}\n{target}\n{}",
        kind.as_str(),
        state_token.unwrap_or_default()
    ))
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

impl Store {
    /// The recorded outcome of `operation_id` for `actor`, or `None` while it is unresolved.
    /// Not routed over HTTP: recovery is the identical call, which replays this row.
    pub async fn operation_outcome(
        &self,
        actor: &str,
        operation_id: &str,
    ) -> Result<Option<Outcome>> {
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT outcome FROM operation_outcomes WHERE account_id=$1 AND operation_id=$2",
        )
        .bind(actor)
        .bind(operation_id)
        .fetch_optional(&self.pool)
        .await?;
        stored.as_deref().map(Outcome::parse).transpose()
    }

    /// Ledger-first read inside the operation's serialised transaction.
    pub(crate) async fn ledger_read(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        operation_id: &str,
    ) -> Result<Option<(OperationKind, String, Outcome)>> {
        let Some(row) = sqlx::query(
            "SELECT kind,digest,outcome FROM operation_outcomes WHERE account_id=$1 AND operation_id=$2",
        )
        .bind(actor)
        .bind(operation_id)
        .fetch_optional(&mut **tx)
        .await?
        else {
            return Ok(None);
        };
        let kind = match row.get::<String, _>(0).as_str() {
            "retire_device" => OperationKind::RetireDevice,
            "revoke_session" => OperationKind::RevokeSession,
            _ => return Err(anyhow!(ErrorCode::InternalError)),
        };
        Ok(Some((
            kind,
            row.get::<String, _>(1),
            Outcome::parse(&row.get::<String, _>(2))?,
        )))
    }

    /// Record the outcome. A row that does not insert exactly once is an invariant failure, so
    /// the enclosing transaction rolls back rather than committing an unrecorded operation.
    pub(crate) async fn ledger_write(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        operation_id: &str,
        kind: OperationKind,
        digest: &str,
        outcome: Outcome,
        now: i64,
    ) -> Result<()> {
        let inserted = sqlx::query("INSERT INTO operation_outcomes(account_id,operation_id,kind,digest,outcome,created_at) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(actor)
            .bind(operation_id)
            .bind(kind.as_str())
            .bind(digest)
            .bind(outcome.as_str())
            .bind(now)
            .execute(&mut **tx)
            .await?;
        ensure!(inserted.rows_affected() == 1, ErrorCode::InternalError);
        Ok(())
    }

    /// Replay a recorded outcome for an identical request; refuse the ID for any other.
    pub(crate) fn replay(
        actor: &str,
        operation_id: &str,
        expected: &str,
        (_, stored, outcome): (OperationKind, String, Outcome),
    ) -> Result<Operation> {
        ensure!(stored == expected, ErrorCode::InvalidValue);
        Ok(Operation {
            operation_id: operation_id.to_owned(),
            account_id: actor.to_owned(),
            outcome,
        })
    }
}
