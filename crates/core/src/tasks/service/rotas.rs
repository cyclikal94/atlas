use super::*;
use crate::error::ErrorCode;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RotaAssignment {
    pub revision: i64,
    pub ordinal: i64,
    pub account_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RotaState {
    pub version: i64,
    pub participants: Vec<String>,
    /// Consented, enrolled candidates, visible only to execution editors.
    pub eligible_participants: Vec<String>,
    pub consent_version: i64,
    pub accepted: bool,
}
impl Store {
    pub async fn task_rota(&self, actor: &str, task: &str) -> Result<RotaState> {
        let mut tx = self.task_read().await?;
        let execution = Self::execution(&mut tx, actor, task, false).await?;
        let mut result = Self::rota_state(&mut tx, actor, task).await?;
        if execution.can_edit {
            let candidates: Vec<String> = sqlx::query_scalar("SELECT c.account_id FROM rota_consents c JOIN task_enrolments e ON e.task_id=c.task_id AND e.account_id=c.account_id WHERE c.task_id=$1 AND c.accepted=1 AND e.active=1 ORDER BY c.account_id")
                .bind(task).fetch_all(&mut *tx).await?;
            for candidate in candidates {
                match Self::execution(&mut tx, &candidate, task, false).await {
                    Ok(_) => result.eligible_participants.push(candidate),
                    Err(e) if e.to_string() == "not_found" => {}
                    Err(e) => return Err(e),
                }
            }
        }
        tx.commit().await?;
        Ok(result)
    }
    async fn rota_state(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        task: &str,
    ) -> Result<RotaState> {
        let rota = sqlx::query("SELECT version,participants FROM task_rotas WHERE task_id=$1")
            .bind(task)
            .fetch_optional(&mut **tx)
            .await?;
        let consent = sqlx::query(
            "SELECT version,accepted FROM rota_consents WHERE task_id=$1 AND account_id=$2",
        )
        .bind(task)
        .bind(actor)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(RotaState {
            eligible_participants: Vec::new(),
            version: rota.as_ref().map_or(0, |r| r.get(0)),
            participants: rota
                .map(|r| serde_json::from_str(&r.get::<String, _>(1)))
                .transpose()?
                .unwrap_or_default(),
            consent_version: consent.as_ref().map_or(0, |r| r.get(0)),
            accepted: consent.is_some_and(|r| r.get::<i64, _>(1) == 1),
        })
    }
    pub(super) async fn rota_command(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        command: &TaskCommand,
    ) -> Result<()> {
        let task = match command {
            TaskCommand::SetRota { task_id, .. } | TaskCommand::RotaConsent { task_id, .. } => {
                task_id
            }
            _ => return Err(anyhow!(ErrorCode::InvalidValue)),
        };
        let execution = Self::execution(
            tx,
            actor,
            task,
            matches!(command, TaskCommand::SetRota { .. }),
        )
        .await?;
        let definition: Definition = serde_json::from_value(execution.value.clone())?;
        // Rotas identify the responsible person; Anyone still permits help.
        ensure!(
            definition.participation == Participation::Anyone,
            ErrorCode::InvalidValue
        );
        let state = Self::rota_state(tx, actor, task).await?;
        match command {
            TaskCommand::RotaConsent {
                expected_version,
                accepted,
                ..
            } => {
                ensure!(
                    *expected_version == state.consent_version,
                    ErrorCode::Conflict
                );
                if *accepted {
                    ensure!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM task_enrolments WHERE task_id=$1 AND account_id=$2 AND active=1").bind(task).bind(actor).fetch_one(&mut **tx).await?==1, ErrorCode::Conflict);
                }
                sqlx::query("INSERT INTO rota_consents VALUES ($1,$2,1,$3) ON CONFLICT(task_id,account_id) DO UPDATE SET version=rota_consents.version+1,accepted=excluded.accepted").bind(task).bind(actor).bind(i64::from(*accepted)).execute(&mut **tx).await?;
            }
            TaskCommand::SetRota {
                expected_version,
                participants,
                ..
            } => {
                ensure!(*expected_version == state.version, ErrorCode::Conflict);
                ensure!(
                    participants.len() <= 8
                        && participants.iter().collect::<BTreeSet<_>>().len() == participants.len(),
                    ErrorCode::InvalidValue
                );
                for participant in participants {
                    identifier(participant)?;
                    ensure!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM rota_consents c JOIN task_enrolments e ON e.task_id=c.task_id AND e.account_id=c.account_id WHERE c.task_id=$1 AND c.account_id=$2 AND c.accepted=1 AND e.active=1").bind(task).bind(participant).fetch_one(&mut **tx).await?==1, ErrorCode::Conflict);
                    Self::execution(tx, participant, task, false).await?;
                }
                sqlx::query("INSERT INTO task_rotas VALUES ($1,1,$2,0) ON CONFLICT(task_id) DO UPDATE SET version=task_rotas.version+1,participants=excluded.participants,next_ordinal=0").bind(task).bind(serde_json::to_string(participants)?).execute(&mut **tx).await?;
            }
            _ => unreachable!(),
        }
        // Notify sync clients without embedding an individual's consent state.
        Self::value(
            tx,
            &execution.id,
            &execution.label,
            &execution.value.to_string(),
        )
        .await
    }
    pub(super) async fn assign_rota(
        tx: &mut Transaction<'_, Any>,
        task: &str,
        eligible: &[String],
    ) -> Result<Option<RotaAssignment>> {
        let Some(row) = sqlx::query(
            "SELECT version,participants,next_ordinal FROM task_rotas WHERE task_id=$1",
        )
        .bind(task)
        .fetch_optional(&mut **tx)
        .await?
        else {
            return Ok(None);
        };
        let participants: Vec<String> = serde_json::from_str(&row.get::<String, _>(1))?;
        if participants.is_empty() {
            return Ok(None);
        }
        let ordinal: i64 = row.get(2);
        ensure!((0..i64::MAX).contains(&ordinal), ErrorCode::InvalidValue);
        let assigned = &participants[(ordinal % participants.len() as i64) as usize];
        let accepted = sqlx::query_scalar::<_, i64>(
            "SELECT accepted FROM rota_consents WHERE task_id=$1 AND account_id=$2",
        )
        .bind(task)
        .bind(assigned)
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(0)
            == 1;
        let account_id = (accepted && eligible.contains(assigned)).then(|| assigned.clone());
        sqlx::query("UPDATE task_rotas SET next_ordinal=next_ordinal+1 WHERE task_id=$1")
            .bind(task)
            .execute(&mut **tx)
            .await?;
        Ok(Some(RotaAssignment {
            revision: row.get(0),
            ordinal,
            account_id,
        }))
    }
}
