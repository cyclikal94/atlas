//! People activity timeline (BE-PEOPLE-ACTIVITY, schema `1007`, API `0.30.2`; recorded on its
//! branch as `0.29.2` and re-sequenced at the 4 November 2026 base merge).
//!
//! Supported People changes append metadata-only rows to `person_activity_events` in the same
//! transaction as the change, after `publish` has assigned the commit's `sync_clock` revision and
//! before the operation receipt is written. A failed or rolled-back write therefore leaves no
//! event, and a replayed operation returns at its receipt before anything is recorded.
//!
//! Kieran's 1 November 2026 decisions govern the design:
//!
//! 1. Retention: metadata for the lifetime of the person or field. No row holds a name, label,
//!    value or summary; the label shown is always the item's *current* visible label.
//! 2. Old values: none are stored. Value history is a separate backlog feature.
//! 3. Actors: named only when the actor is the reader, the person's current owner or its currently
//!    linked account. Anyone else is `someone_else`, with no identifier and no per-actor distinction.
//! 4. Sharing and audience changes are not recorded.
//!
//! A read applies the reader's current permissions to every event: the person gate first (the same
//! `NotFound` as `GET /people/{id}`), then the current visibility of each event's own target (and
//! of a merge's source), using the production `VISIBLE` rule through `subset_batch`. Hidden events
//! never count toward a page, its continuation or anything else in the response.
//!
//! The page is selected by target, not by scanning history (BE-PEOPLE-ACTIVITY R3). Every event of
//! a timeline targets one of its people (the canonical person or a merged alias) or a field whose
//! parent is one of them: an event's field belongs to the event's person when it is written, and the
//! schema's hierarchy triggers allow a field's parent to change only from a merged alias to the
//! person it was merged into. So the read collects those resources from the current hierarchy,
//! judges each one's visibility once, and then reads events only for what the reader can see: when
//! every target is visible, each alias's newest events through `person_activity_events_person`;
//! otherwise the newest events of the visible targets through `person_activity_events_target`.
//! Either way each statement returns at most one page plus one row, so hidden history is never read.
use crate::error::ErrorCode;
use crate::{Projection, Store, identifier};
use anyhow::{Result, anyhow, bail, ensure};
use serde::Serialize;
use sqlx::{Any, Row, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// The closed event catalogue. Stored as its snake-case name.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityType {
    PersonCreated,
    PersonRenamed,
    PersonArchived,
    PersonRestored,
    PersonLinked,
    PersonUnlinked,
    PersonMerged,
    FieldCreated,
    FieldEdited,
    FieldArchived,
    FieldRestored,
}

impl ActivityType {
    const ALL: [Self; 11] = [
        Self::PersonCreated,
        Self::PersonRenamed,
        Self::PersonArchived,
        Self::PersonRestored,
        Self::PersonLinked,
        Self::PersonUnlinked,
        Self::PersonMerged,
        Self::FieldCreated,
        Self::FieldEdited,
        Self::FieldArchived,
        Self::FieldRestored,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PersonCreated => "person_created",
            Self::PersonRenamed => "person_renamed",
            Self::PersonArchived => "person_archived",
            Self::PersonRestored => "person_restored",
            Self::PersonLinked => "person_linked",
            Self::PersonUnlinked => "person_unlinked",
            Self::PersonMerged => "person_merged",
            Self::FieldCreated => "field_created",
            Self::FieldEdited => "field_edited",
            Self::FieldArchived => "field_archived",
            Self::FieldRestored => "field_restored",
        }
    }

    fn parse(raw: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|t| t.as_str() == raw)
            .ok_or_else(|| anyhow!("unknown person activity type {raw:?}"))
    }

    /// Fixed UK-English phrase. It never contains user content; presentation belongs to clients.
    pub fn summary(self) -> &'static str {
        match self {
            Self::PersonCreated => "Added this person",
            Self::PersonRenamed => "Renamed this person",
            Self::PersonArchived => "Archived this person",
            Self::PersonRestored => "Restored this person",
            Self::PersonLinked => "Linked this person to an account",
            Self::PersonUnlinked => "Unlinked this person from an account",
            Self::PersonMerged => "Merged another profile into this person",
            Self::FieldCreated => "Added a field",
            Self::FieldEdited => "Edited a field",
            Self::FieldArchived => "Archived a field",
            Self::FieldRestored => "Restored a field",
        }
    }
}

/// An account as one reader may see it on an event.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivityAccount {
    You,
    Account {
        account_id: String,
        username: String,
    },
    SomeoneElse,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Person,
    Field,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetState {
    /// The target is the person read, or a field it has now.
    Current,
    /// A person ID that now resolves to the canonical person (merged into it).
    Merged,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangedPart {
    Label,
    Value,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ActivityTarget {
    pub kind: TargetKind,
    pub id: String,
    /// The resource's *current* label, never the label it had when the event happened.
    pub label: String,
    pub archived: bool,
    pub state: TargetState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_person_id: Option<String>,
    /// The field value's current `kind` (for example `text` or `date`). Fields only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field_kind: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ActivityPersonRef {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PersonActivityEvent {
    pub id: String,
    pub occurred_at: i64,
    #[serde(rename = "type")]
    pub event_type: ActivityType,
    pub summary: String,
    pub actor: ActivityAccount,
    pub target: ActivityTarget,
    /// `field_edited` only: which parts changed (never their old or new content).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<Vec<ChangedPart>>,
    /// `person_linked` / `person_unlinked` only: the linked or previously linked account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<ActivityAccount>,
    /// `person_merged` only, and only while the reader can see the merged-in person.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_from: Option<ActivityPersonRef>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PersonActivityPage {
    /// The canonical person: the path ID may be a merged alias, as for `GET /people/{id}`.
    pub person_id: String,
    /// Newest first.
    pub items: Vec<PersonActivityEvent>,
    /// The last item's ID, present only when a further visible item exists.
    pub next_after: Option<String>,
    /// When recording began on this database. Nothing earlier was recorded.
    pub coverage_started_at: i64,
}

/// One event collected while a command runs, written by [`Store::record_activity`].
#[derive(Clone, Debug)]
pub(crate) struct PendingEvent {
    person_id: String,
    target_id: String,
    event_type: ActivityType,
    account_id: Option<String>,
    merged_from: Option<String>,
    label_changed: bool,
    value_changed: bool,
}

/// Events of one write, in command order. Empty for writes that change nothing recordable.
#[derive(Debug, Default)]
pub(crate) struct ActivityLog(Vec<PendingEvent>);

impl ActivityLog {
    fn push(&mut self, person: &str, target: &str, event_type: ActivityType) -> &mut PendingEvent {
        self.0.push(PendingEvent {
            person_id: person.to_owned(),
            target_id: target.to_owned(),
            event_type,
            account_id: None,
            merged_from: None,
            label_changed: false,
            value_changed: false,
        });
        self.0.last_mut().expect("just pushed")
    }

    pub(crate) fn person(&mut self, person: &str, event_type: ActivityType) {
        self.push(person, person, event_type);
    }

    pub(crate) fn field(&mut self, person: &str, field: &str, event_type: ActivityType) {
        self.push(person, field, event_type);
    }

    pub(crate) fn link(&mut self, person: &str, account: &str, linked: bool) {
        let event = if linked {
            ActivityType::PersonLinked
        } else {
            ActivityType::PersonUnlinked
        };
        self.push(person, person, event).account_id = Some(account.to_owned());
    }

    pub(crate) fn merge(&mut self, target: &str, source: &str) {
        self.push(target, target, ActivityType::PersonMerged)
            .merged_from = Some(source.to_owned());
    }

    /// A field edit: recorded only when the label or the value actually changed.
    pub(crate) fn field_edit(&mut self, person: &str, field: &str, label: bool, value: bool) {
        if label || value {
            let event = self.push(person, field, ActivityType::FieldEdited);
            event.label_changed = label;
            event.value_changed = value;
        }
    }

    /// An archive flag change: recorded only when the flag actually changes.
    pub(crate) fn archive(&mut self, p: &Projection, person: Option<&str>, archived: bool) {
        if p.archived == archived {
            return;
        }
        match (p.kind.as_str(), person) {
            ("person", _) => self.person(
                &p.id,
                if archived {
                    ActivityType::PersonArchived
                } else {
                    ActivityType::PersonRestored
                },
            ),
            ("field", Some(person)) => self.field(
                person,
                &p.id,
                if archived {
                    ActivityType::FieldArchived
                } else {
                    ActivityType::FieldRestored
                },
            ),
            _ => {}
        }
    }
}

/// Whether a stored JSON value and a newly serialised one differ.
pub(crate) fn value_differs(old: &serde_json::Value, new: &str) -> Result<bool> {
    let new: serde_json::Value = serde_json::from_str(new)?;
    Ok(*old != new)
}

/// Visible targets bound per event statement when some of the timeline's targets are hidden.
const TARGET_CHUNK: usize = 100;

const EVENT_COLUMNS: &str = "id,revision,ordinal,target_id,event_type,actor_id,account_id,merged_from,label_changed,value_changed,occurred_at";

/// The events one statement reads, newest first.
enum Stream<'a> {
    /// Every event of one timeline alias (the reader can see all the timeline's targets).
    Member(&'a str),
    /// The events of these visible targets.
    Targets(&'a [String]),
}

enum Value {
    Text(String),
    Int(i64),
}

struct Candidate {
    id: String,
    revision: i64,
    ordinal: i64,
    target_id: String,
    event_type: ActivityType,
    actor_id: String,
    account_id: Option<String>,
    merged_from: Option<String>,
    label_changed: bool,
    value_changed: bool,
    occurred_at: i64,
}

impl Store {
    /// At most `take` events of `stream` strictly older than `position`, newest first.
    async fn activity_stream(
        &self,
        tx: &mut Transaction<'_, Any>,
        stream: &Stream<'_>,
        members: &[String],
        position: Option<(i64, i64)>,
        take: i64,
    ) -> Result<Vec<Candidate>> {
        let mut binds = Vec::new();
        let mut place = |value: Value| {
            binds.push(value);
            format!("${}", binds.len())
        };
        // Once statistics exist (Atlas never gathers them, but an operator can), SQLite prefers the
        // unique (revision, ordinal) index, which satisfies the ORDER BY by walking every person's
        // history, so each statement names the index it is designed for. PostgreSQL's planner is
        // left to its statistics; the tests bound the rows it examines.
        let index = |name: &str| {
            if self.sqlite {
                format!(" INDEXED BY {name}")
            } else {
                String::new()
            }
        };
        let (kind, mut sql) = match stream {
            Stream::Member(member) => (
                "member",
                format!(
                    "SELECT {EVENT_COLUMNS} FROM person_activity_events{} WHERE person_id={}",
                    index("person_activity_events_person"),
                    place(Value::Text((*member).to_owned()))
                ),
            ),
            Stream::Targets(targets) => {
                let targets = targets
                    .iter()
                    .map(|t| place(Value::Text(t.clone())))
                    .collect::<Vec<_>>()
                    .join(",");
                let members = members
                    .iter()
                    .map(|m| place(Value::Text(m.clone())))
                    .collect::<Vec<_>>()
                    .join(",");
                (
                    "targets",
                    format!(
                        "SELECT {EVENT_COLUMNS} FROM person_activity_events{} WHERE target_id IN ({targets}) AND person_id IN ({members})",
                        index("person_activity_events_target")
                    ),
                )
            }
        };
        // A strict (revision, ordinal) bound whose first term is an index range on both engines.
        if let Some((revision, ordinal)) = position {
            let (revision, ordinal) = (place(Value::Int(revision)), place(Value::Int(ordinal)));
            sql.push_str(&format!(
                " AND revision<={revision} AND (revision<{revision} OR ordinal<{ordinal})"
            ));
        }
        sql.push_str(&format!(
            " ORDER BY revision DESC,ordinal DESC LIMIT {}",
            place(Value::Int(take))
        ));
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
        for value in &binds {
            query = match value {
                Value::Text(text) => query.bind(text.as_str()),
                Value::Int(int) => query.bind(*int),
            };
        }
        let rows = query.fetch_all(&mut **tx).await?;
        #[cfg(feature = "test-hooks")]
        if self.hooks.records_activity() {
            self.hooks
                .activity_statement(crate::hooks::ActivityStatement {
                    stream: kind,
                    sql,
                    binds: binds
                        .iter()
                        .map(|value| match value {
                            Value::Text(text) => crate::hooks::Bind::Text(text.clone()),
                            Value::Int(int) => crate::hooks::Bind::Int(*int),
                        })
                        .collect(),
                    rows: rows.len(),
                });
        }
        #[cfg(not(feature = "test-hooks"))]
        let _ = (kind, sql);
        rows.iter()
            .map(|r| {
                Ok(Candidate {
                    id: r.get(0),
                    revision: r.get(1),
                    ordinal: r.get(2),
                    target_id: r.get(3),
                    event_type: ActivityType::parse(&r.get::<String, _>(4))?,
                    actor_id: r.get(5),
                    account_id: r.get(6),
                    merged_from: r.get(7),
                    label_changed: r.get::<i64, _>(8) == 1,
                    value_changed: r.get::<i64, _>(9) == 1,
                    occurred_at: r.get(10),
                })
            })
            .collect()
    }

    /// The parent person of a field, when it has one (a field can also belong to a task).
    pub(crate) async fn field_person(
        tx: &mut Transaction<'_, Any>,
        field: &Projection,
    ) -> Result<Option<String>> {
        let Some(parent) = &field.parent_id else {
            return Ok(None);
        };
        let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM resources WHERE id=$1")
            .bind(parent)
            .fetch_optional(&mut **tx)
            .await?;
        Ok((kind.as_deref() == Some("person")).then(|| parent.clone()))
    }

    /// Append the events of one committed-to-be write. Runs inside the write's transaction, after
    /// `publish` returned its revision and before the receipt, so the events commit or roll back
    /// with the change and a replay never reaches it.
    pub(crate) async fn record_activity(
        tx: &mut Transaction<'_, Any>,
        actor: &str,
        revision: i64,
        occurred_at: i64,
        log: &ActivityLog,
    ) -> Result<()> {
        ensure!(log.0.len() < 100, ErrorCode::BatchTooLarge);
        for (ordinal, event) in log.0.iter().enumerate() {
            sqlx::query("INSERT INTO person_activity_events(id,revision,ordinal,person_id,target_id,event_type,actor_id,account_id,merged_from,label_changed,value_changed,occurred_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
                .bind(Uuid::new_v4().to_string())
                .bind(revision)
                .bind(i64::try_from(ordinal)?)
                .bind(&event.person_id)
                .bind(&event.target_id)
                .bind(event.event_type.as_str())
                .bind(actor)
                .bind(&event.account_id)
                .bind(&event.merged_from)
                .bind(i64::from(event.label_changed))
                .bind(i64::from(event.value_changed))
                .bind(occurred_at)
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }

    /// `GET /people/{id}/activity`: one page of the person's visible activity, newest first.
    pub async fn person_activity(
        &self,
        actor: &str,
        id: &str,
        after: Option<&str>,
        limit: u16,
    ) -> Result<PersonActivityPage> {
        ensure!((1..=100).contains(&limit), ErrorCode::InvalidValue);
        identifier(id)?;
        if let Some(after) = after {
            identifier(after)?;
        }
        let limit = usize::from(limit);
        let mut tx = self.task_read().await?;
        #[cfg(feature = "test-hooks")]
        self.hooks.activity_begin();
        let canonical = Self::canonical_person(&mut tx, id).await?;
        let person = Self::task_resource(&mut tx, actor, &canonical, "person", false).await?;
        hook!(self, "people_activity.between_person_and_events", &mut tx);
        // Every ID merged into the canonical person resolves to it (merges re-point older
        // aliases), so this set is the person's whole history.
        let mut members: Vec<String> =
            sqlx::query_scalar("SELECT source_id FROM person_aliases WHERE canonical_id=$1")
                .bind(&canonical)
                .fetch_all(&mut *tx)
                .await?;
        members.push(canonical.clone());
        members.sort();
        let members_sql = (0..members.len())
            .map(|i| format!("${}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let mut visible: BTreeMap<String, Option<Projection>> = BTreeMap::new();
        // A cursor is accepted only while its event is one this read would itself return: an
        // event of this person's timeline whose own target the reader can see now, judged in
        // this snapshot by the same rule as every returned row. An event that has become hidden
        // (revoked or narrowed field access, an exclusion, a merge source the reader cannot see)
        // is the same error as a missing event and another person's event, so the cursor never
        // reveals that a hidden event exists or belongs to this timeline.
        let position = if let Some(after) = after {
            let mut query = sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT revision,ordinal,target_id FROM person_activity_events WHERE person_id IN ({members_sql}) AND id=${}",
                members.len() + 1
            )));
            for member in &members {
                query = query.bind(member.as_str());
            }
            let row = query
                .bind(after)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
            let target: String = row.get(2);
            #[cfg(feature = "test-hooks")]
            self.hooks.activity_visibility(1);
            let mut found =
                Self::subset_batch(&mut tx, actor, &BTreeSet::from([target.clone()])).await?;
            let projection = found.remove(&target);
            let shown = projection.is_some();
            visible.insert(target, projection);
            ensure!(shown, ErrorCode::InvalidValue);
            Some((row.get::<i64, _>(0), row.get::<i64, _>(1)))
        } else {
            None
        };
        // The timeline's targets, from the current hierarchy rather than from its history: its
        // people and their fields (archived ones included). See the module comment for why no
        // event can target anything else.
        let mut query = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(format!(
            "SELECT id FROM resources WHERE parent_id IN ({members_sql})"
        )));
        for member in &members {
            query = query.bind(member.as_str());
        }
        let mut targets: BTreeSet<String> = query.fetch_all(&mut *tx).await?.into_iter().collect();
        targets.extend(members.iter().cloned());
        let unseen: BTreeSet<String> = targets
            .iter()
            .filter(|t| !visible.contains_key(*t))
            .cloned()
            .collect();
        #[cfg(feature = "test-hooks")]
        {
            self.hooks.activity_targets(&targets);
            self.hooks.activity_visibility(unseen.len());
        }
        let mut found = Self::subset_batch(&mut tx, actor, &unseen).await?;
        for id in unseen {
            let p = found.remove(&id);
            visible.insert(id, p);
        }
        hook!(self, "people_activity.between_targets_and_events", &mut tx);
        let shown: Vec<String> = targets
            .iter()
            .filter(|t| matches!(visible.get(*t), Some(Some(_))))
            .cloned()
            .collect();
        let streams: Vec<Stream> = if shown.len() == targets.len() {
            members.iter().map(|m| Stream::Member(m)).collect()
        } else {
            shown.chunks(TARGET_CHUNK).map(Stream::Targets).collect()
        };
        // Each stream's own newest `limit + 1` rows include every row of the global newest
        // `limit + 1`, and the streams are disjoint, so one statement per stream fills the page
        // and its lookahead.
        let take = i64::try_from(limit + 1)?;
        let mut candidates = Vec::new();
        for stream in &streams {
            candidates.extend(
                self.activity_stream(&mut tx, stream, &members, position, take)
                    .await?,
            );
        }
        candidates.sort_by_key(|c| std::cmp::Reverse((c.revision, c.ordinal)));
        candidates.truncate(limit + 1);
        // A merge source is always one of the timeline's people; anything else is judged here.
        let sources: BTreeSet<String> = candidates
            .iter()
            .filter_map(|c| c.merged_from.clone())
            .filter(|id| !visible.contains_key(id))
            .collect();
        if !sources.is_empty() {
            #[cfg(feature = "test-hooks")]
            self.hooks.activity_visibility(sources.len());
            let mut found = Self::subset_batch(&mut tx, actor, &sources).await?;
            for id in sources {
                let p = found.remove(&id);
                visible.insert(id, p);
            }
        }
        let mut kept: Vec<(Candidate, Projection)> = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            // Every selected row targets a resource judged visible above. A row outside the
            // timeline's targets would break the hierarchy invariant: refuse rather than guess.
            let Some(Some(target)) = visible.get(&candidate.target_id) else {
                bail!(
                    "person activity event {} targets {}, outside the timeline of {canonical}",
                    candidate.id,
                    candidate.target_id
                );
            };
            let target = target.clone();
            kept.push((candidate, target));
        }
        let next_after = (kept.len() > limit).then(|| kept[limit - 1].0.id.clone());
        kept.truncate(limit);

        // Accounts every viewer of this person already sees: its owner and its linked account.
        let mut named = BTreeSet::from([actor.to_owned()]);
        let owner: String = sqlx::query_scalar("SELECT owner_id FROM resources WHERE id=$1")
            .bind(&canonical)
            .fetch_one(&mut *tx)
            .await?;
        named.insert(owner);
        if let Some(linked) = sqlx::query_scalar::<_, String>(
            "SELECT account_id FROM person_accounts WHERE person_id=$1",
        )
        .bind(&canonical)
        .fetch_optional(&mut *tx)
        .await?
        {
            named.insert(linked);
        }
        let mut usernames = BTreeMap::new();
        for account in &named {
            let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
                .bind(account)
                .fetch_one(&mut *tx)
                .await?;
            usernames.insert(account.clone(), username);
        }
        let disclose = |account: &str| {
            if account == actor {
                ActivityAccount::You
            } else if let Some(username) = usernames.get(account) {
                ActivityAccount::Account {
                    account_id: account.to_owned(),
                    username: username.clone(),
                }
            } else {
                ActivityAccount::SomeoneElse
            }
        };
        let items = kept
            .into_iter()
            .map(|(c, target)| {
                let person_target = target.kind == "person";
                let merged = person_target && target.id != canonical;
                PersonActivityEvent {
                    id: c.id,
                    occurred_at: c.occurred_at,
                    event_type: c.event_type,
                    summary: c.event_type.summary().to_owned(),
                    actor: disclose(&c.actor_id),
                    changed: (c.event_type == ActivityType::FieldEdited).then(|| {
                        let mut parts = Vec::new();
                        if c.label_changed {
                            parts.push(ChangedPart::Label);
                        }
                        if c.value_changed {
                            parts.push(ChangedPart::Value);
                        }
                        parts
                    }),
                    account: c.account_id.as_deref().map(disclose),
                    merged_from: c
                        .merged_from
                        .as_ref()
                        .and_then(|id| visible.get(id).cloned().flatten())
                        .map(|p| ActivityPersonRef {
                            id: p.id,
                            label: p.label,
                        }),
                    target: ActivityTarget {
                        kind: if person_target {
                            TargetKind::Person
                        } else {
                            TargetKind::Field
                        },
                        field_kind: (!person_target)
                            .then(|| target.value.get("kind").and_then(|k| k.as_str()))
                            .flatten()
                            .map(str::to_owned),
                        id: target.id,
                        label: target.label,
                        archived: target.archived,
                        state: if merged {
                            TargetState::Merged
                        } else {
                            TargetState::Current
                        },
                        canonical_person_id: merged.then(|| canonical.clone()),
                    },
                }
            })
            .collect();
        let coverage_started_at: i64 =
            sqlx::query_scalar("SELECT started_at FROM person_activity_coverage WHERE id=1")
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(PersonActivityPage {
            person_id: person.id,
            items,
            next_after,
            coverage_started_at,
        })
    }
}
