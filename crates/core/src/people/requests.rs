use crate::error::ErrorCode;
use crate::{Store, identifier};
use anyhow::{Result, ensure};
use serde::Serialize;
use sqlx::{Any, Row, Transaction};
use std::collections::{BTreeMap, BTreeSet};

use super::workflows::*;

fn proposal_summary(payload: &str) -> Result<serde_json::Value> {
    Ok(match serde_json::from_str::<Proposal>(payload)? {
        Proposal::Link {
            person_id,
            account_id,
            name,
            ..
        } => {
            serde_json::json!({"kind":"link","person_id":person_id,"account_id":account_id,"name":name})
        }
        Proposal::Merge {
            source_id,
            target_id,
            name,
            ..
        } => {
            serde_json::json!({"kind":"merge","source_id":source_id,"target_id":target_id,"name":name})
        }
    })
}

#[derive(Debug, Serialize)]
pub struct SentPeopleRequest {
    pub id: String,
    pub recipient_id: String,
    pub recipient_username: String,
    pub expires_at: i64,
    pub state: String,
    pub proposal: serde_json::Value,
}
#[derive(Debug, Serialize)]
pub struct SentPeopleRequestPage {
    pub items: Vec<SentPeopleRequest>,
    pub next_after: Option<String>,
}

impl Store {
    pub async fn people_requests(&self, actor: &str, now: i64) -> Result<Vec<PeopleRequest>> {
        let mut tx = self.task_read().await?;
        Self::epoch(&mut tx, actor).await?;
        let rows=sqlx::query("SELECT id,sender_id,expires_at,payload FROM people_requests WHERE recipient_id=$1 AND state='pending' AND expires_at>$2 ORDER BY id LIMIT 100").bind(actor).bind(now).fetch_all(&mut *tx).await?;
        let result = rows
            .into_iter()
            .map(|r| {
                Ok(PeopleRequest {
                    id: r.get(0),
                    sender_id: r.get(1),
                    expires_at: r.get(2),
                    proposal: proposal_summary(&r.get::<String, _>(3))?,
                })
            })
            .collect::<Result<_>>()?;
        tx.commit().await?;
        Ok(result)
    }

    /// Durable, sender-keyed, all-states history: draws from `people_request_history`, which
    /// is mirrored alongside (never instead of) `people_requests` and is never swept by
    /// `collect_expired`, so a sender's history survives operational cleanup/expiry.
    pub async fn sent_people_requests(
        &self,
        actor: &str,
        after: Option<&str>,
        limit: u16,
        now: i64,
    ) -> Result<SentPeopleRequestPage> {
        ensure!((1..=200).contains(&limit), ErrorCode::InvalidValue);
        if let Some(after) = after {
            identifier(after)?;
        }
        let mut tx = self.task_read().await?;
        let rows = sqlx::query(
            "SELECT h.id,h.recipient_id,a.username,h.expires_at,h.state,h.payload \
             FROM people_request_history h JOIN accounts a ON a.id=h.recipient_id \
             WHERE h.sender_id=$1 AND ($2 IS NULL OR h.id>$2) ORDER BY h.id LIMIT $3",
        )
        .bind(actor)
        .bind(after)
        .bind(i64::from(limit))
        .fetch_all(&mut *tx)
        .await?;
        let next_after = (rows.len() == usize::from(limit)).then(|| rows.last().unwrap().get(0));
        let items = rows
            .into_iter()
            .map(|r| {
                let state: String = r.get(4);
                let expires_at: i64 = r.get(3);
                let state = if state == "pending" && expires_at <= now {
                    "expired".into()
                } else {
                    state
                };
                Ok(SentPeopleRequest {
                    id: r.get(0),
                    recipient_id: r.get(1),
                    recipient_username: r.get(2),
                    expires_at,
                    state,
                    proposal: proposal_summary(&r.get::<String, _>(5))?,
                })
            })
            .collect::<Result<_>>()?;
        tx.commit().await?;
        Ok(SentPeopleRequestPage { items, next_after })
    }

    pub(super) async fn people_scope(
        tx: &mut Transaction<'_, Any>,
        roots: &[&str],
    ) -> Result<(BTreeSet<String>, BTreeSet<String>, Before)> {
        let mut touched = BTreeSet::new();
        for root in roots {
            touched.extend(Self::task_touched(tx, root).await?)
        }
        ensure!(touched.len() <= 200, ErrorCode::ExpansionLimit);
        let accounts = Self::audience(tx, &touched).await?;
        let mut before = BTreeMap::new();
        for account in &accounts {
            before.insert(account.clone(), Self::subset(tx, account, &touched).await?);
        }
        Ok((touched, accounts, before))
    }

    pub(super) async fn propose(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        recipient: &str,
        id: &str,
        proposal: &Proposal,
        now: i64,
    ) -> Result<()> {
        identifier(id)?;
        Self::epoch(tx, recipient).await?;
        ensure!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM people_requests WHERE recipient_id=$1 AND state='pending' AND expires_at>$2").bind(recipient).bind(now).fetch_one(&mut **tx).await?<100, ErrorCode::SliceCapacity);
        sqlx::query("INSERT INTO people_request_ids VALUES ($1)")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        let kind = match proposal {
            Proposal::Link { .. } => "link",
            Proposal::Merge { .. } => "merge",
        };
        let payload = serde_json::to_string(proposal)?;
        let expires_at = now + 604800;
        sqlx::query("INSERT INTO people_requests(id,sender_id,recipient_id,kind,payload,expires_at) VALUES ($1,$2,$3,$4,$5,$6)").bind(id).bind(actor).bind(recipient).bind(kind).bind(&payload).bind(expires_at).execute(&mut **tx).await?;
        sqlx::query("INSERT INTO people_request_history(id,sender_id,recipient_id,kind,payload,state,expires_at,updated_at) VALUES ($1,$2,$3,$4,$5,'pending',$6,$7)").bind(id).bind(actor).bind(recipient).bind(kind).bind(&payload).bind(expires_at).bind(now).execute(&mut **tx).await?;
        Ok(())
    }
}
