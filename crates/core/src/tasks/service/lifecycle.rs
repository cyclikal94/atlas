use super::*;
use crate::error::ErrorCode;

impl Store {
    pub(crate) async fn participant_stream(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        occurrence: &str,
        subject: &str,
        edit: bool,
    ) -> Result<Projection> {
        let id:Option<String>=sqlx::query_scalar("SELECT progress_id FROM occurrence_participants WHERE occurrence_id=$1 AND account_id=$2").bind(occurrence).bind(subject).fetch_optional(&mut **tx).await?;
        Self::task_resource(
            tx,
            actor,
            &id.ok_or_else(|| anyhow!(ErrorCode::NotFound))?,
            "progress",
            edit,
        )
        .await
    }
    pub(crate) async fn stream_data(tx: &mut Transaction<'_, Any>, id: &str) -> Result<Stream> {
        let participant = sqlx::query(
            "SELECT account_id,excluded,aggregate_consent FROM occurrence_participants WHERE progress_id=$1",
        )
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
        let rows=sqlx::query("SELECT e.id,e.evidence,e.happened_at,e.recorded_by FROM progress_entries e WHERE e.progress_id=$1 AND NOT EXISTS(SELECT 1 FROM progress_entries r WHERE r.replaces=e.id) ORDER BY e.logical_order").bind(id).fetch_all(&mut **tx).await?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(Entry {
                id: row.get(0),
                evidence: serde_json::from_str(&row.get::<String, _>(1))?,
                happened_at: row.get(2),
                recorded_by: row.get(3),
            });
        }
        Ok(Stream {
            subject_account_id: participant.get(0),
            entries,
            excluded: participant.get::<i64, _>(1) == 1,
            aggregate_consent: participant.get::<i64, _>(2) == 1,
        })
    }
    pub(crate) async fn refresh_stream(
        tx: &mut Transaction<'_, Any>,
        id: &str,
        goal: &Goal,
        close: Option<i64>,
    ) -> Result<()> {
        let stream = Self::stream_data(tx, id).await?;
        let open:Option<i64>=sqlx::query_scalar("SELECT o.opens_at FROM task_occurrences o JOIN occurrence_participants p ON p.occurrence_id=o.id WHERE p.progress_id=$1").bind(id).fetch_one(&mut **tx).await?;
        let state = progress_state(&stream, goal, open, close)?;
        Self::value(tx, id, "Progress", &serde_json::to_string(&state)?).await
    }
    pub(crate) async fn install_copied_policy(
        tx: &mut Transaction<'_, Any>,
        id: &str,
        policy: &Policy,
    ) -> Result<()> {
        // Trusted snapshots only. Parent authority remains mandatory at read time,
        // including newly joined household members and revoked direct recipients.
        for grant in &policy.grants {
            match grant {
                PrincipalGrant::Account { id: account, edit } => {
                    sqlx::query("INSERT INTO resource_grants VALUES ($1,$2,$3)")
                        .bind(id)
                        .bind(account)
                        .bind(i64::from(*edit))
                        .execute(&mut **tx)
                        .await?;
                }
                PrincipalGrant::Household {
                    id: household,
                    edit,
                } => {
                    sqlx::query("INSERT INTO resource_household_grants VALUES ($1,$2,$3)")
                        .bind(id)
                        .bind(household)
                        .bind(i64::from(*edit))
                        .execute(&mut **tx)
                        .await?;
                }
            }
        }
        for account in &policy.exclude_accounts {
            sqlx::query("INSERT INTO resource_exclusions VALUES ($1,$2)")
                .bind(id)
                .bind(account)
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }
    // Identity keys are opaque for calendar anchors. Order by the saved local
    // schedule, with a stable identity tie-breaker, never by the anchor UUID.
    pub(crate) async fn chronological_occurrences(
        tx: &mut Transaction<'_, Any>,
        task: &str,
    ) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT id,slot FROM task_occurrences WHERE task_id=$1")
            .bind(task)
            .fetch_all(&mut **tx)
            .await?;
        let mut slots = rows
            .into_iter()
            .map(|row| {
                let slot: Slot = serde_json::from_str(&row.get::<String, _>(1))?;
                Ok((
                    slot.date,
                    slot.intended_time,
                    slot.key,
                    row.get::<String, _>(0),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        slots.sort();
        Ok(slots.into_iter().map(|(_, _, _, id)| id).collect())
    }
    pub(crate) async fn make_occurrence(
        tx: &mut Transaction<'_, Any>,
        task: &str,
        execution: &Projection,
        definition: &Definition,
        slot: Slot,
        now: i64,
        touched: &mut BTreeSet<String>,
    ) -> Result<()> {
        let effective_date = slot.date.clone().unwrap_or_else(|| "9999-12-31".into());
        let enrolments=sqlx::query("SELECT e.account_id,e.aggregate_consent,e.policy FROM task_enrolment_history e WHERE e.task_id=$1 AND e.effective_date<=$2 AND e.active=1 AND NOT EXISTS(SELECT 1 FROM task_enrolment_history n WHERE n.task_id=e.task_id AND n.account_id=e.account_id AND n.effective_date<=$2 AND n.version>e.version) ORDER BY e.account_id").bind(task).bind(&effective_date).fetch_all(&mut **tx).await?;
        let mut accepted = Vec::new();
        for row in enrolments {
            let account: String = row.get(0);
            if Self::subset(tx, &account, &BTreeSet::from([execution.id.clone()]))
                .await?
                .contains_key(&execution.id)
            {
                accepted.push((
                    account,
                    row.get::<i64, _>(1),
                    serde_json::from_str::<Policy>(&row.get::<String, _>(2))?,
                ));
            }
        }
        ensure!(accepted.len() <= 8, ErrorCode::SliceCapacity);
        let (opens_at, closes_at) = windows(&slot, definition)?;
        let mut covered_by = None;
        if definition.carry == Carry::RetainOne {
            let candidates = Self::chronological_occurrences(tx, task).await?;
            for candidate in candidates {
                let data: OccurrenceData = serde_json::from_str(
                    &sqlx::query_scalar::<_, String>("SELECT value FROM resources WHERE id=$1")
                        .bind(&candidate)
                        .fetch_one(&mut **tx)
                        .await?,
                )?;
                if data.covered_by.is_some() || data.resolved {
                    continue;
                }
                // Progress can only influence publicly visible scheduling if every
                // task reader may see the evidence and participants consented.
                let audience = Self::audience(tx, &BTreeSet::from([execution.id.clone()])).await?;
                let mut public_complete = !audience.is_empty();
                for account in audience {
                    if Self::subset(tx, &account, &BTreeSet::from([execution.id.clone()]))
                        .await?
                        .is_empty()
                    {
                        continue;
                    }
                    if Self::outcome_for(tx, &account, &candidate, &data, now, false).await?
                        != Outcome::Complete
                    {
                        public_complete = false;
                        break;
                    }
                }
                if !public_complete {
                    covered_by = Some(candidate);
                    break;
                }
            }
        }
        let owner: String = sqlx::query_scalar("SELECT owner_id FROM resources WHERE id=$1")
            .bind(task)
            .fetch_one(&mut **tx)
            .await?;
        let id = occurrence_id(task, &slot.key)?;
        let eligible = accepted.iter().map(|p| p.0.clone()).collect::<Vec<_>>();
        let rota = Self::assign_rota(tx, task, &eligible).await?;
        let data = OccurrenceData {
            rota,
            task_id: task.into(),
            definition: definition.clone(),
            slot: slot.clone(),
            opens_at,
            closes_at,
            participants: accepted.iter().map(|p| p.0.clone()).collect(),
            covered_by: covered_by.clone(),
            resolved: false,
        };
        Self::new_resource(
            tx,
            &owner,
            &id,
            Some(&execution.id),
            "occurrence",
            "Occurrence",
            &serde_json::to_string(&data)?,
            &Policy::default(),
        )
        .await?;
        let policy = Self::copied_policy(tx, &execution.id).await?;
        Self::install_copied_policy(tx, &id, &policy).await?;
        sqlx::query("INSERT INTO task_occurrences(id,task_id,slot_key,definition,slot,opens_at,closes_at,covered_by) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)").bind(&id).bind(task).bind(&slot.key).bind(serde_json::to_string(definition)?).bind(serde_json::to_string(&slot)?).bind(opens_at).bind(closes_at).bind(covered_by.clone()).execute(&mut **tx).await?;
        if definition
            .schedule
            .repeat
            .as_ref()
            .is_some_and(|r| r.frequency == Frequency::AfterCompletion)
        {
            sqlx::query("INSERT INTO completion_pending(occurrence_id,next_check) VALUES ($1,$2)")
                .bind(&id)
                .bind(opens_at.unwrap_or(now).max(now))
                .execute(&mut **tx)
                .await?;
        }
        touched.insert(id.clone());
        if covered_by.is_none() {
            for (account, consent, mut policy) in accepted {
                if let Err(error) = Self::validate_policy(tx, &account, &policy).await {
                    if matches!(error.to_string().as_str(), "forbidden" | "not_found") {
                        policy = Policy::default();
                    } else {
                        return Err(error);
                    }
                }
                let progress = progress_id(&id, &account)?;
                let stream = Stream {
                    subject_account_id: account.clone(),
                    entries: Vec::new(),
                    excluded: false,
                    aggregate_consent: consent == 1,
                };
                Self::new_resource(
                    tx,
                    &account,
                    &progress,
                    Some(&id),
                    "progress",
                    "Progress",
                    &serde_json::to_string(&progress_state(
                        &stream,
                        &definition.goal,
                        opens_at,
                        closes_at,
                    )?)?,
                    &Policy::default(),
                )
                .await?;
                Self::install_copied_policy(tx, &progress, &policy).await?;
                sqlx::query("INSERT INTO occurrence_participants(occurrence_id,account_id,progress_id,aggregate_consent) VALUES ($1,$2,$3,$4)").bind(&id).bind(account).bind(&progress).bind(consent).execute(&mut **tx).await?;
                touched.insert(progress);
            }
        }
        sqlx::query("UPDATE tasks SET last_date=$1,once_created=1 WHERE task_id=$2")
            .bind(slot.date)
            .bind(task)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
}

pub(super) fn windows(slot: &Slot, definition: &Definition) -> Result<(Option<i64>, Option<i64>)> {
    let Some(date) = &slot.date else {
        return Ok((None, None));
    };
    let date = schedule::date(date)?;
    let zone = slot
        .timezone
        .parse::<chrono_tz::Tz>()
        .map_err(|_| anyhow!(ErrorCode::InvalidValue))?;
    let open = date
        .checked_sub_signed(Duration::days(i64::from(definition.open_days_before)))
        .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
    let close = date
        .checked_add_signed(Duration::days(i64::from(definition.close_days_after)))
        .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
    Ok((
        Some(resolve(zone, open.and_time(NaiveTime::MIN))?),
        Some(resolve(zone, close.and_time(NaiveTime::MIN))?),
    ))
}
