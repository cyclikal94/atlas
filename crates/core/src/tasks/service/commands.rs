use super::*;
use crate::error::ErrorCode;

impl Store {
    pub async fn task_command(
        &self,
        actor: &str,
        operation: &str,
        command: &TaskCommand,
        defaults_revision: Option<&str>,
        now: i64,
    ) -> Result<TaskResult> {
        identifier(operation)?;
        let mut tx = self.begin_serial().await?;
        Self::epoch(&mut tx, actor).await?;
        let payload = receipt_digest(&format!(
            "task-command-v1\n{defaults_revision:?}\n{}",
            serde_json::to_string(command)?
        ));
        if let Some(r)=sqlx::query("SELECT payload,revision,digest_version FROM receipts WHERE account_id=$1 AND operation_id=$2").bind(actor).bind(operation).fetch_optional(&mut *tx).await? {
            ensure!(r.get::<i64,_>(2)==1, ErrorCode::UnsupportedReceipt);ensure!(r.get::<String,_>(0)==payload, ErrorCode::OperationConflict);return Ok(TaskResult{revision:r.get(1)});
        }
        let mut resolved = command.clone();
        match &mut resolved {
            TaskCommand::PutField { parent_id, .. } => {
                *parent_id = Self::canonical_person(&mut tx, parent_id).await?
            }
            TaskCommand::Archive { id, .. } => *id = Self::canonical_person(&mut tx, id).await?,
            _ => {}
        }
        let command = &resolved;
        let defaults = Self::defaults_in(&mut tx, actor).await?;
        if let Some(expected) = defaults_revision {
            ensure!(expected == defaults.revision, ErrorCode::DefaultsChanged);
        }
        let root = match command {
            TaskCommand::CreateTask { id, .. }
            | TaskCommand::ReviseTask { id, .. }
            | TaskCommand::CreateList { id, .. }
            | TaskCommand::EditList { id, .. }
            | TaskCommand::Archive { id, .. }
            | TaskCommand::PutField { id, .. }
            | TaskCommand::ReviseOccurrence { id, .. } => id,
            TaskCommand::SetRota { task_id, .. }
            | TaskCommand::RotaConsent { task_id, .. }
            | TaskCommand::Enrol { task_id, .. }
            | TaskCommand::Materialise { task_id, .. } => task_id,
            TaskCommand::Record { occurrence_id, .. }
            | TaskCommand::Exclude { occurrence_id, .. }
            | TaskCommand::Resolve { occurrence_id, .. }
            | TaskCommand::CancelTimer { occurrence_id, .. }
            | TaskCommand::StartTimer { occurrence_id, .. }
            | TaskCommand::StopTimer { occurrence_id, .. }
            | TaskCommand::SetDependencies { occurrence_id, .. }
            | TaskCommand::CompleteDependencies { occurrence_id, .. } => occurrence_id,
            TaskCommand::ListItem { list_id, .. } => list_id,
        };
        let mut touched = Self::task_touched(&mut tx, root).await?;
        if let TaskCommand::CompleteDependencies { occurrence_id, .. } = command {
            let preview = Self::dependency_preview_in(&mut tx, actor, occurrence_id, now).await?;
            for item in preview.items {
                touched.extend(Self::task_touched(&mut tx, &item.occurrence_id).await?);
            }
        }
        let mut accounts = Self::audience(&mut tx, &touched).await?;
        accounts.insert(actor.into());
        let mut before = BTreeMap::new();
        for account in &accounts {
            before.insert(
                account.clone(),
                Self::subset(&mut tx, account, &touched).await?,
            );
        }
        match command {
            TaskCommand::SetRota { .. } | TaskCommand::RotaConsent { .. } => {
                Self::rota_command(&mut tx, actor, command).await?;
            }
            TaskCommand::StartTimer { .. }
            | TaskCommand::StopTimer { .. }
            | TaskCommand::CancelTimer { .. } => {
                Self::timer_command(&mut tx, actor, command, &defaults, now, &mut touched).await?;
            }
            TaskCommand::SetDependencies {
                occurrence_id,
                expected_version,
                prerequisites,
                strict,
            } => {
                Self::set_dependencies(
                    &mut tx,
                    actor,
                    occurrence_id,
                    *expected_version,
                    prerequisites,
                    *strict,
                )
                .await?;
            }
            TaskCommand::CompleteDependencies {
                root_evidence,
                occurrence_id,
                preview_token,
                mode,
                happened_at,
            } => {
                Self::complete_dependencies(
                    &mut tx,
                    actor,
                    operation,
                    occurrence_id,
                    preview_token,
                    *mode,
                    root_evidence.as_ref(),
                    *happened_at,
                    now,
                    &defaults,
                    &mut touched,
                )
                .await?;
            }
            TaskCommand::CreateTask {
                id,
                execution_id,
                title,
                definition,
                anchor,
                initial_policy,
            } => {
                definition.validate()?;
                ensure!(
                    serde_json::to_vec(definition)?.len() <= 4096,
                    ErrorCode::InvalidValue
                );
                let policy = initial_policy
                    .clone()
                    .unwrap_or_else(|| Self::resolved_policy(&defaults, "task"));
                Self::new_resource(&mut tx, actor, id, None, "task", title, "", &policy).await?;
                Self::new_resource(
                    &mut tx,
                    actor,
                    execution_id,
                    Some(id),
                    "execution",
                    "Task details",
                    &serde_json::to_string(definition)?,
                    &policy,
                )
                .await?;
                sqlx::query(
                    "INSERT INTO tasks(task_id,execution_id,definition_revision) VALUES ($1,$2,1)",
                )
                .bind(id)
                .bind(execution_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query("INSERT INTO task_definitions VALUES ($1,1,$2)")
                    .bind(id)
                    .bind(serde_json::to_string(definition)?)
                    .execute(&mut *tx)
                    .await?;
                let progress = Self::resolved_policy(&defaults, "progress");
                Self::validate_policy(&mut tx, actor, &progress).await?;
                sqlx::query("INSERT INTO task_enrolments VALUES ($1,$2,1,1,$3,1)")
                    .bind(id)
                    .bind(actor)
                    .bind(serde_json::to_string(&progress)?)
                    .execute(&mut *tx)
                    .await?;
                let created_date = DateTime::<Utc>::from_timestamp(now, 0)
                    .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?
                    .date_naive();
                // Creating an anchored task explicitly accepts its bounded initial
                // backlog for the creator. Later enrolments still start prospectively.
                let effective = definition.schedule.start_date.clone().unwrap_or(
                    (created_date - chrono::Duration::days(if anchor.is_some() { 30 } else { 0 }))
                        .to_string(),
                );
                sqlx::query("INSERT INTO task_enrolment_history SELECT task_id,account_id,version,$3,active,aggregate_consent,policy FROM task_enrolments WHERE task_id=$1 AND account_id=$2").bind(id).bind(actor).bind(effective).execute(&mut *tx).await?;
                touched.insert(execution_id.clone());
                let zone: chrono_tz::Tz = definition.schedule.timezone.parse()?;
                let today = DateTime::<Utc>::from_timestamp(now, 0)
                    .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?
                    .with_timezone(&zone)
                    .date_naive()
                    .to_string();
                let slots = if anchor.is_some() {
                    Vec::new()
                } else {
                    definition.schedule.slots(None, &today, 1)?.0
                };
                let execution = Self::execution(&mut tx, actor, id, false).await?;
                for slot in slots {
                    Self::make_occurrence(
                        &mut tx,
                        id,
                        &execution,
                        definition,
                        slot,
                        now,
                        &mut touched,
                    )
                    .await?;
                }
                if let Some(anchor) = anchor {
                    Self::set_task_anchor(&mut tx, actor, id, anchor).await?;
                    Self::reconcile_task_anchor(&mut tx, id, now, &mut touched).await?;
                }
            }
            TaskCommand::ReviseTask {
                id,
                expected_version,
                title,
                definition,
            } => {
                let task = Self::task_resource(&mut tx, actor, id, "task", true).await?;
                ensure!(task.version == *expected_version, ErrorCode::Conflict);
                let execution = Self::execution(&mut tx, actor, id, true).await?;
                definition.validate()?;
                ensure!(
                    serde_json::to_vec(definition)?.len() <= 4096,
                    ErrorCode::InvalidValue
                );
                let old: Definition = serde_json::from_value(execution.value.clone())?;
                // Changing the participation model requires a new task; silently
                // reinterpreting already accepted enrolment would be unsafe.
                ensure!(
                    old.schedule.repeat.is_some() == definition.schedule.repeat.is_some(),
                    ErrorCode::InvalidValue
                );
                ensure!(
                    old.schedule
                        .repeat
                        .as_ref()
                        .is_some_and(|r| r.frequency == Frequency::AfterCompletion)
                        == definition
                            .schedule
                            .repeat
                            .as_ref()
                            .is_some_and(|r| r.frequency == Frequency::AfterCompletion),
                    ErrorCode::InvalidValue
                );
                ensure!(
                    old.participation == definition.participation,
                    ErrorCode::InvalidValue
                );
                Self::value(&mut tx, id, title, &task.value.to_string()).await?;
                Self::value(
                    &mut tx,
                    &execution.id,
                    "Task details",
                    &serde_json::to_string(definition)?,
                )
                .await?;
                sqlx::query(
                    "UPDATE tasks SET definition_revision=definition_revision+1 WHERE task_id=$1",
                )
                .bind(id)
                .execute(&mut *tx)
                .await?;
                sqlx::query("INSERT INTO task_definitions SELECT task_id,definition_revision,$2 FROM tasks WHERE task_id=$1").bind(id).bind(serde_json::to_string(definition)?).execute(&mut *tx).await?;
            }
            TaskCommand::Enrol {
                task_id,
                expected_version,
                active,
                aggregate_consent,
                initial_policy,
            } => {
                let execution = Self::execution(&mut tx, actor, task_id, false).await?;
                let definition: Definition = serde_json::from_value(execution.value.clone())?;
                let owner: String =
                    sqlx::query_scalar("SELECT owner_id FROM resources WHERE id=$1")
                        .bind(task_id)
                        .fetch_one(&mut *tx)
                        .await?;
                ensure!(
                    definition.participation != Participation::Personal || actor == owner,
                    ErrorCode::Forbidden
                );
                let current: Option<i64> = sqlx::query_scalar(
                    "SELECT version FROM task_enrolments WHERE task_id=$1 AND account_id=$2",
                )
                .bind(task_id)
                .bind(actor)
                .fetch_optional(&mut *tx)
                .await?;
                ensure!(
                    current.unwrap_or(0) == *expected_version,
                    ErrorCode::Conflict
                );
                let policy = initial_policy
                    .clone()
                    .unwrap_or_else(|| Self::resolved_policy(&defaults, "progress"));
                Self::validate_policy(&mut tx, actor, &policy).await?;
                for audience in Self::policy_audience(&mut tx, &policy).await? {
                    Self::task_resource(&mut tx, &audience, &execution.id, "execution", false)
                        .await?;
                }
                sqlx::query("INSERT INTO task_enrolments VALUES ($1,$2,$3,$4,$5,1) ON CONFLICT(task_id,account_id) DO UPDATE SET active=excluded.active,aggregate_consent=excluded.aggregate_consent,policy=excluded.policy,version=task_enrolments.version+1").bind(task_id).bind(actor).bind(i64::from(*active)).bind(i64::from(*aggregate_consent)).bind(serde_json::to_string(&policy)?).execute(&mut *tx).await?;
                let zone: chrono_tz::Tz = definition.schedule.timezone.parse()?;
                let effective = DateTime::<Utc>::from_timestamp(now, 0)
                    .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?
                    .with_timezone(&zone)
                    .date_naive()
                    .to_string();
                sqlx::query("INSERT INTO task_enrolment_history SELECT task_id,account_id,version,$3,active,aggregate_consent,policy FROM task_enrolments WHERE task_id=$1 AND account_id=$2").bind(task_id).bind(actor).bind(effective).execute(&mut *tx).await?;
                ensure!(
                    sqlx::query_scalar::<_, i64>(
                        "SELECT COUNT(*) FROM task_enrolments WHERE task_id=$1 AND active=1"
                    )
                    .bind(task_id)
                    .fetch_one(&mut *tx)
                    .await?
                        <= 8,
                    ErrorCode::SliceCapacity
                );
            }
            TaskCommand::Materialise {
                task_id,
                through_date,
                limit,
            } => {
                ensure!((1..=16).contains(limit), ErrorCode::InvalidValue);
                let task = Self::task_resource(&mut tx, actor, task_id, "task", false).await?;
                ensure!(!task.archived, ErrorCode::InvalidValue);
                let execution = Self::execution(&mut tx, actor, task_id, false).await?;
                let definition: Definition = serde_json::from_value(execution.value.clone())?;
                let zone: chrono_tz::Tz = definition.schedule.timezone.parse()?;
                let today = DateTime::<Utc>::from_timestamp(now, 0)
                    .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?
                    .with_timezone(&zone)
                    .date_naive();
                ensure!(
                    schedule::date(through_date)? <= today + Duration::days(31),
                    ErrorCode::InvalidValue
                );
                let state =
                    sqlx::query("SELECT last_date,once_created FROM tasks WHERE task_id=$1")
                        .bind(task_id)
                        .fetch_one(&mut *tx)
                        .await?;
                if definition.schedule.repeat.is_some() || state.get::<i64, _>(1) == 0 {
                    let (slots, _) = definition.schedule.slots(
                        state.get::<Option<String>, _>(0).as_deref(),
                        through_date,
                        usize::from(*limit),
                    )?;
                    for slot in slots {
                        Self::make_occurrence(
                            &mut tx,
                            task_id,
                            &execution,
                            &definition,
                            slot,
                            now,
                            &mut touched,
                        )
                        .await?;
                    }
                }
            }
            TaskCommand::Record {
                occurrence_id,
                subject_account_id,
                ..
            } => {
                Self::record_entry(&mut tx, actor, &defaults, command, now, &mut touched).await?;
                let stream = Self::participant_stream(
                    &mut tx,
                    actor,
                    occurrence_id,
                    subject_account_id,
                    false,
                )
                .await?;
                let state: ProgressState = serde_json::from_value(stream.value.clone())?;
                if state.action.outcome == Outcome::Complete {
                    Self::require_dependencies(&mut tx, actor, occurrence_id, now).await?;
                }
            }
            TaskCommand::Exclude {
                occurrence_id,
                expected_version,
                excluded,
            } => {
                let occurrence =
                    Self::task_resource(&mut tx, actor, occurrence_id, "occurrence", false).await?;
                let data: OccurrenceData = serde_json::from_value(occurrence.value.clone())?;
                ensure!(
                    data.definition.allow_streak_exclusions && data.covered_by.is_none(),
                    ErrorCode::InvalidValue
                );
                let stream =
                    Self::participant_stream(&mut tx, actor, occurrence_id, actor, true).await?;
                ensure!(stream.version == *expected_version, ErrorCode::Conflict);
                sqlx::query("UPDATE occurrence_participants SET excluded=$1 WHERE progress_id=$2")
                    .bind(i64::from(*excluded))
                    .bind(&stream.id)
                    .execute(&mut *tx)
                    .await?;
                Self::refresh_stream(&mut tx, &stream.id, &data.definition.goal, data.closes_at)
                    .await?;
                touched.insert(stream.id);
            }
            TaskCommand::Resolve {
                occurrence_id,
                expected_version,
                resolved,
            } => {
                let p =
                    Self::task_resource(&mut tx, actor, occurrence_id, "occurrence", true).await?;
                ensure!(p.version == *expected_version, ErrorCode::Conflict);
                let mut data: OccurrenceData = serde_json::from_value(p.value.clone())?;
                data.resolved = *resolved;
                sqlx::query("UPDATE task_occurrences SET resolved=$1 WHERE id=$2")
                    .bind(i64::from(*resolved))
                    .bind(occurrence_id)
                    .execute(&mut *tx)
                    .await?;
                Self::value(
                    &mut tx,
                    occurrence_id,
                    "Occurrence",
                    &serde_json::to_string(&data)?,
                )
                .await?;
            }
            TaskCommand::ReviseOccurrence {
                id,
                expected_version,
                definition,
                date,
                time,
            } => {
                let p = Self::task_resource(&mut tx, actor, id, "occurrence", true).await?;
                ensure!(p.version == *expected_version, ErrorCode::Conflict);
                let mut data: OccurrenceData = serde_json::from_value(p.value.clone())?;
                definition.validate()?;
                ensure!(
                    data.definition.participation == definition.participation,
                    ErrorCode::InvalidValue
                );
                let schedule = Schedule {
                    start_date: date.clone(),
                    time: time.clone(),
                    timezone: definition.schedule.timezone.clone(),
                    repeat: None,
                };
                schedule.validate()?;
                let mut slot = schedule
                    .slots(None, date.as_deref().unwrap_or("9999-12-31"), 1)?
                    .0
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
                slot.key = data.slot.key.clone();
                data.slot = slot;
                ensure!(
                    serde_json::to_string(definition)?.len() <= 4096,
                    ErrorCode::InvalidValue
                );
                data.definition = definition.clone();
                (data.opens_at, data.closes_at) = windows(&data.slot, definition)?;
                // A goal correction must remain compatible with the existing evidence.
                for row in sqlx::query(
                    "SELECT progress_id FROM occurrence_participants WHERE occurrence_id=$1",
                )
                .bind(id)
                .fetch_all(&mut *tx)
                .await?
                {
                    let stream = Self::stream_data(&mut tx, &row.get::<String, _>(0)).await?;
                    for entry in stream.entries {
                        entry.evidence.validate(&definition.goal)?;
                    }
                }
                sqlx::query("UPDATE task_occurrences SET definition=$1,slot=$2,opens_at=$3,closes_at=$4 WHERE id=$5").bind(serde_json::to_string(definition)?).bind(serde_json::to_string(&data.slot)?).bind(data.opens_at).bind(data.closes_at).bind(id).execute(&mut *tx).await?;
                Self::value(&mut tx, id, "Occurrence", &serde_json::to_string(&data)?).await?;
                let streams: Vec<String> = sqlx::query_scalar(
                    "SELECT progress_id FROM occurrence_participants WHERE occurrence_id=$1",
                )
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
                for stream in streams {
                    Self::refresh_stream(&mut tx, &stream, &definition.goal, data.closes_at)
                        .await?;
                }
            }
            TaskCommand::CreateList {
                id,
                name,
                initial_policy,
            } => {
                Self::new_resource(
                    &mut tx,
                    actor,
                    id,
                    None,
                    "list",
                    name,
                    "",
                    &initial_policy
                        .clone()
                        .unwrap_or_else(|| Self::resolved_policy(&defaults, "list")),
                )
                .await?
            }
            TaskCommand::EditList {
                id,
                expected_version,
                name,
            } => {
                let p = Self::task_resource(&mut tx, actor, id, "list", true).await?;
                ensure!(p.version == *expected_version, ErrorCode::Conflict);
                Self::value(&mut tx, id, name, "").await?;
            }
            TaskCommand::ListItem {
                list_id,
                expected_version,
                task_id,
                included,
            } => {
                let p = Self::task_resource(&mut tx, actor, list_id, "list", true).await?;
                ensure!(p.version == *expected_version, ErrorCode::Conflict);
                Self::task_resource(&mut tx, actor, task_id, "task", false).await?;
                if *included {
                    sqlx::query("INSERT INTO list_items VALUES ($1,$2) ON CONFLICT DO NOTHING")
                        .bind(list_id)
                        .bind(task_id)
                        .execute(&mut *tx)
                        .await?;
                } else {
                    sqlx::query("DELETE FROM list_items WHERE list_id=$1 AND task_id=$2")
                        .bind(list_id)
                        .bind(task_id)
                        .execute(&mut *tx)
                        .await?;
                }
                ensure!(
                    sqlx::query_scalar::<_, i64>(
                        "SELECT COUNT(*) FROM list_items WHERE list_id=$1"
                    )
                    .bind(list_id)
                    .fetch_one(&mut *tx)
                    .await?
                        <= 100,
                    ErrorCode::SliceCapacity
                );
                Self::value(&mut tx, list_id, &p.label, "").await?;
            }
            TaskCommand::Archive {
                id,
                expected_version,
                archived,
            } => {
                let p = Self::task_resource(&mut tx, actor, id, "", true).await?;
                ensure!(
                    matches!(p.kind.as_str(), "person" | "field" | "task" | "list"),
                    ErrorCode::InvalidValue
                );
                ensure!(p.version == *expected_version, ErrorCode::Conflict);
                sqlx::query("UPDATE resources SET archived=$1,version=version+1 WHERE id=$2")
                    .bind(i64::from(*archived))
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            TaskCommand::PutField {
                id,
                parent_id,
                expected_version,
                label,
                value,
                initial_policy,
            } => {
                value.validate()?;
                let parent = Self::task_resource(&mut tx, actor, parent_id, "", false).await?;
                ensure!(
                    matches!(parent.kind.as_str(), "person" | "task"),
                    ErrorCode::InvalidValue
                );
                if let Some(version) = expected_version {
                    ensure!(initial_policy.is_none(), ErrorCode::InvalidValue);
                    let p = Self::task_resource(&mut tx, actor, id, "field", true).await?;
                    ensure!(p.version == *version, ErrorCode::Conflict);
                    ensure!(
                        p.parent_id.as_ref() == Some(parent_id),
                        ErrorCode::InvalidValue
                    );
                    Self::value(&mut tx, id, label, &serde_json::to_string(value)?).await?;
                } else {
                    let policy = Self::person_field_policy(
                        &mut tx,
                        actor,
                        parent_id,
                        initial_policy.clone(),
                        Self::resolved_policy(&defaults, "field"),
                    )
                    .await?;
                    Self::new_resource(
                        &mut tx,
                        actor,
                        id,
                        Some(parent_id),
                        "field",
                        label,
                        &serde_json::to_string(value)?,
                        &policy,
                    )
                    .await?;
                }
            }
        }
        if matches!(
            command,
            TaskCommand::Record { .. }
                | TaskCommand::CompleteDependencies { .. }
                | TaskCommand::StopTimer { .. }
                | TaskCommand::Materialise { .. }
                | TaskCommand::ReviseOccurrence { .. }
                | TaskCommand::Archive { .. }
        ) {
            Self::completion_successors(&mut tx, &mut touched, now).await?;
        }
        touched.extend(Self::task_touched(&mut tx, root).await?);
        accounts.extend(Self::audience(&mut tx, &touched).await?);
        for account in &accounts {
            before.entry(account.clone()).or_default();
        }
        let revision = Self::publish(&mut tx, &accounts, &touched, &before, false).await?;
        sqlx::query(
            "INSERT INTO receipts(account_id,operation_id,payload,revision) VALUES ($1,$2,$3,$4)",
        )
        .bind(actor)
        .bind(operation)
        .bind(payload)
        .bind(revision)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(TaskResult { revision })
    }
    pub(super) async fn ensure_own_participation(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        defaults: &crate::policy::Defaults,
        occurrence: &Projection,
        data: &OccurrenceData,
        touched: &mut BTreeSet<String>,
    ) -> Result<()> {
        let occurrence_id = &occurrence.id;
        if data.definition.participation == Participation::Anyone {
            let exists:i64=sqlx::query_scalar("SELECT COUNT(*) FROM occurrence_participants WHERE occurrence_id=$1 AND account_id=$2").bind(occurrence_id).bind(actor).fetch_one(&mut **tx).await?;
            if exists == 0 {
                ensure!(occurrence.can_edit, ErrorCode::Forbidden);
                let progress = progress_id(occurrence_id, actor)?;
                let policy = Self::resolved_policy(defaults, "progress");
                Self::validate_policy(tx, actor, &policy).await?;
                let stream = Stream {
                    subject_account_id: actor.into(),
                    entries: vec![],
                    excluded: false,
                    aggregate_consent: true,
                };
                Self::new_resource(
                    tx,
                    actor,
                    &progress,
                    Some(occurrence_id),
                    "progress",
                    "Progress",
                    &serde_json::to_string(&progress_state(
                        &stream,
                        &data.definition.goal,
                        data.opens_at,
                        data.closes_at,
                    )?)?,
                    &Policy::default(),
                )
                .await?;
                Self::install_copied_policy(tx, &progress, &policy).await?;
                sqlx::query("INSERT INTO occurrence_participants(occurrence_id,account_id,progress_id,aggregate_consent) VALUES ($1,$2,$3,1)").bind(occurrence_id).bind(actor).bind(&progress).execute(&mut **tx).await?;
                touched.insert(progress);
            }
        }
        Ok(())
    }
    pub(super) async fn record_entry(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        defaults: &crate::policy::Defaults,
        command: &TaskCommand,
        now: i64,
        touched: &mut BTreeSet<String>,
    ) -> Result<()> {
        let TaskCommand::Record {
            occurrence_id,
            subject_account_id,
            entry_id,
            evidence,
            replaces,
            expected_version,
            happened_at,
        } = command
        else {
            return Err(anyhow!(ErrorCode::InvalidValue));
        };
        identifier(entry_id)?;
        ensure!(
            *happened_at <= now + 300 && DateTime::<Utc>::from_timestamp(*happened_at, 0).is_some(),
            ErrorCode::InvalidValue
        );
        let occurrence = Self::task_resource(tx, actor, occurrence_id, "occurrence", false).await?;
        let data: OccurrenceData = serde_json::from_value(occurrence.value.clone())?;
        ensure!(data.covered_by.is_none(), ErrorCode::InvalidValue);
        evidence.validate(&data.definition.goal)?;
        if subject_account_id == actor {
            Self::ensure_own_participation(tx, actor, defaults, &occurrence, &data, touched)
                .await?;
        }
        let stream =
            Self::participant_stream(tx, actor, occurrence_id, subject_account_id, true).await?;
        if !matches!(evidence, Evidence::Quantity { .. } | Evidence::Observe) && replaces.is_none()
        {
            ensure!(expected_version.is_some(), ErrorCode::InvalidValue);
        }
        if let Some(version) = expected_version {
            ensure!(*version == stream.version, ErrorCode::Conflict);
        }
        if let Some(previous) = replaces {
            ensure!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM progress_entries e WHERE e.id=$1 AND e.progress_id=$2 AND NOT EXISTS(SELECT 1 FROM progress_entries r WHERE r.replaces=e.id)").bind(previous).bind(&stream.id).fetch_one(&mut **tx).await?==1, ErrorCode::Conflict);
        }
        ensure!(
            data.opens_at.is_none_or(|v| *happened_at >= v),
            ErrorCode::InvalidValue
        );
        if data.definition.carry == Carry::CloseIncomplete {
            ensure!(
                data.closes_at.is_none_or(|v| *happened_at < v),
                ErrorCode::InvalidValue
            );
        }
        let logical_order = if let Some(previous) = replaces {
            sqlx::query_scalar::<_, i64>("SELECT logical_order FROM progress_entries WHERE id=$1")
                .bind(previous)
                .fetch_one(&mut **tx)
                .await?
        } else {
            stream.version
        };
        sqlx::query("INSERT INTO progress_entries(id,progress_id,sequence,logical_order,evidence,replaces,happened_at,recorded_by) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)").bind(entry_id).bind(&stream.id).bind(stream.version).bind(logical_order).bind(serde_json::to_string(evidence)?).bind(replaces).bind(happened_at).bind(actor).execute(&mut **tx).await?;
        Self::refresh_stream(tx, &stream.id, &data.definition.goal, data.closes_at).await?;
        touched.insert(stream.id);
        Ok(())
    }
}
