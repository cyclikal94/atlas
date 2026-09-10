use crate::error::ErrorCode;
use crate::{Store, identifier};
use anyhow::{Result, ensure};
use sqlx::{Any, Row, Transaction};
use std::collections::{BTreeMap, BTreeSet};

use super::workflows::*;
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
                    proposal: match serde_json::from_str::<Proposal>(&r.get::<String,_>(3))? {
                        Proposal::Link{person_id,account_id,name,..}=>serde_json::json!({"kind":"link","person_id":person_id,"account_id":account_id,"name":name}),
                        Proposal::Merge{source_id,target_id,name,..}=>serde_json::json!({"kind":"merge","source_id":source_id,"target_id":target_id,"name":name}),
                    },
                })
            })
            .collect::<Result<_>>()?;
        tx.commit().await?;
        Ok(result)
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
        sqlx::query("INSERT INTO people_requests(id,sender_id,recipient_id,kind,payload,expires_at) VALUES ($1,$2,$3,$4,$5,$6)").bind(id).bind(actor).bind(recipient).bind(match proposal{Proposal::Link{..}=>"link",Proposal::Merge{..}=>"merge"}).bind(serde_json::to_string(proposal)?).bind(now+604800).execute(&mut **tx).await?;
        Ok(())
    }
}
