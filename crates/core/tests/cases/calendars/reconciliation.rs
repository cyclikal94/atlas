use anyhow::Result;
use atlas_core::{Store, calendars::*, tasks::*};
use chrono::{TimeZone, Utc};
use uuid::Uuid;
fn id() -> String {
    Uuid::new_v4().to_string()
}
fn now() -> i64 {
    Utc.with_ymd_and_hms(2026, 9, 8, 12, 0, 0)
        .unwrap()
        .timestamp()
}
use crate::support::calendars::feed;
async fn source(s: &Store, a: &str) -> Result<String> {
    let source = id();
    s.calendar_command(
        a,
        &id(),
        &CalendarCommand::CreateSource {
            id: source.clone(),
            label: "Calendar".into(),
            timezone: "UTC".into(),
            connection: None,
            initial_policy: None,
        },
    )
    .await?;
    Ok(source)
}
async fn import(s: &Store, a: &str, id: &str, text: &str, t: i64) -> Result<()> {
    let r = s.begin_calendar_refresh(a, id, t).await?;
    let f = ics::parse(text, &r.timezone, "2026-09-01", "2026-12-31")?;
    s.finish_calendar_refresh(a, id, r.generation, Some(&f), (None, None), None, t)
        .await?;
    Ok(())
}
async fn account(s: &Store) -> Result<String> {
    let a = id();
    s.add_account(&a, &format!("cal-{}", Uuid::new_v4().simple()), "test")
        .await?;
    Ok(a)
}
fn definition() -> Definition {
    Definition {
        schedule: Schedule {
            start_date: None,
            time: None,
            timezone: "UTC".into(),
            repeat: None,
        },
        goal: Goal::Checkbox,
        carry: Carry::Accumulate,
        participation: Participation::Personal,
        open_days_before: 0,
        close_days_after: 1,
        allow_streak_exclusions: false,
    }
}
async fn scenario(s: Store, other: Store) -> Result<()> {
    s.migrate().await?;
    let a = account(&s).await?;
    let b = account(&s).await?;
    let source = source(&s, &a).await?;
    import(&s, &a, &source, &feed("20260910T100000Z"), now()).await?;
    let events = s
        .calendar_resources(&a, "event", Some(&source), None, 200)
        .await?;
    assert_eq!(events.len(), 1);
    assert!(
        s.calendar_resources(&b, "event", Some(&source), None, 200)
            .await?
            .is_empty()
    );
    let task = id();
    s.task_command(
        &a,
        &id(),
        &TaskCommand::CreateTask {
            id: task.clone(),
            execution_id: id(),
            title: "Pack".into(),
            definition: definition(),
            initial_policy: None,
            anchor: Some(Anchor {
                reference: Reference::EventSeries {
                    source_id: source.clone(),
                    uid: "trip".into(),
                },
                offset: Offset::CalendarDays {
                    days: -1,
                    time: Some("09:00:00".into()),
                },
                weekdays: vec![],
                title_contains: None,
            }),
        },
        None,
        now(),
    )
    .await?;
    let view = |task: String| ViewFilter {
        task_id: Some(task),
        ..Default::default()
    };
    let first = s
        .occurrence_view(&a, &view(task.clone()), now())
        .await?
        .items
        .remove(0);
    assert_eq!(first.data.slot.date.as_deref(), Some("2026-09-09"));
    import(&s, &a, &source, &feed("20260911T100000Z"), now() + 1).await?;
    let moved = s
        .occurrence_view(&a, &view(task.clone()), now())
        .await?
        .items
        .remove(0);
    assert_eq!(moved.id, first.id);
    assert_eq!(moved.data.slot.date.as_deref(), Some("2026-09-10"));
    assert_eq!(moved.data.slot.key, first.data.slot.key);
    let review = s
        .calendar_resources(&a, "review", Some(&moved.id), None, 200)
        .await?;
    assert_eq!(review.len(), 1);
    assert!(review[0].value.to_string().contains("moved"));
    let last_good = s
        .calendar_resources(&a, "event", Some(&source), None, 200)
        .await?[0]
        .value
        .clone();
    let r = s.begin_calendar_refresh(&a, &source, now() + 2).await?;
    s.finish_calendar_refresh(
        &a,
        &source,
        r.generation,
        None,
        (None, None),
        Some("fetch_failed"),
        now() + 2,
    )
    .await?;
    assert_eq!(
        s.calendar_resources(&a, "event", Some(&source), None, 200)
            .await?[0]
            .value,
        last_good
    );
    import(
        &s,
        &a,
        &source,
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n",
        now() + 3,
    )
    .await?;
    assert!(
        s.calendar_resources(&a, "event", Some(&source), None, 200)
            .await?[0]
            .value
            .to_string()
            .contains("missing")
    );
    assert_eq!(
        s.occurrence_view(&a, &view(task.clone()), now())
            .await?
            .items[0]
            .id,
        first.id
    );
    import(&s,&a,&source,"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:trip\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",now()+4).await?;
    assert!(
        s.calendar_resources(&a, "event", Some(&source), None, 200)
            .await?[0]
            .value
            .to_string()
            .contains("cancelled")
    );
    let old = s.begin_calendar_refresh(&a, &source, now() + 5).await?;
    let newer = other
        .begin_calendar_refresh(&a, &source, now() + 66)
        .await?;
    assert_eq!(
        s.finish_calendar_refresh(
            &a,
            &source,
            old.generation,
            None,
            (None, None),
            Some("fetch_failed"),
            now() + 67
        )
        .await
        .unwrap_err()
        .to_string(),
        "stale_refresh"
    );
    s.finish_calendar_refresh(
        &a,
        &source,
        newer.generation,
        None,
        (None, None),
        Some("fetch_failed"),
        now() + 67,
    )
    .await?;
    // Private durable reminders, retry fencing, successful deduplication and local ownership.
    let reminder = id();
    let subscription = id();
    let rule = ReminderRule {
        offset_seconds: 0,
        time: Some("09:00:00".into()),
        late_seconds: 3600,
        enabled: true,
        delivery: DeliveryMode::Server,
    };
    s.reminder_command(
        &a,
        &id(),
        &ReminderCommand::SetRule {
            id: reminder.clone(),
            occurrence_id: moved.id.clone(),
            expected_version: None,
            rule: rule.clone(),
        },
    )
    .await?;
    s.reminder_command(
        &a,
        &id(),
        &ReminderCommand::SetSubscription {
            id: subscription.clone(),
            expected_version: 0,
            device_id: "phone".into(),
            transport: "ntfy".into(),
            secret: "test-encrypted".into(),
            enabled: true,
        },
    )
    .await?;
    let due = Utc
        .with_ymd_and_hms(2026, 9, 10, 9, 0, 0)
        .unwrap()
        .timestamp();
    s.schedule_reminders(due).await?;
    let lease = s.claim_reminder_delivery(due).await?.unwrap();
    assert!(other.claim_reminder_delivery(due).await?.is_none());
    let replacement = other.claim_reminder_delivery(due + 61).await?.unwrap();
    assert_eq!(lease.id, replacement.id);
    assert_ne!(lease.token, replacement.token);
    assert_eq!(
        s.finish_reminder_delivery(&lease.id, &lease.token, true, false, due + 62)
            .await
            .unwrap_err()
            .to_string(),
        "stale_delivery"
    );
    other
        .finish_reminder_delivery(&replacement.id, &replacement.token, true, false, due + 62)
        .await?;
    s.schedule_reminders(due + 63).await?;
    assert!(s.claim_reminder_delivery(due + 63).await?.is_none());
    assert!(
        s.notification_subscriptions(&b).await?["subscriptions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut local = rule.clone();
    local.delivery = DeliveryMode::Device {
        device_id: "phone".into(),
    };
    s.reminder_command(
        &a,
        &id(),
        &ReminderCommand::SetRule {
            id: reminder.clone(),
            occurrence_id: moved.id.clone(),
            expected_version: Some(1),
            rule: local,
        },
    )
    .await?;
    s.schedule_reminders(due + 64).await?;
    assert!(s.claim_reminder_delivery(due + 64).await?.is_none());
    // Exhausted leases after repeated worker crashes terminate, rather than sticking.
    s.reminder_command(
        &a,
        &id(),
        &ReminderCommand::SetRule {
            id: reminder.clone(),
            occurrence_id: moved.id.clone(),
            expected_version: Some(2),
            rule: rule.clone(),
        },
    )
    .await?;
    s.schedule_reminders(due + 65).await?;
    let mut delivery_id = String::new();
    for attempt in 0..8 {
        let lease = s
            .claim_reminder_delivery(due + 65 + 61 * attempt)
            .await?
            .unwrap();
        delivery_id = lease.id;
    }
    assert!(
        s.claim_reminder_delivery(due + 65 + 61 * 8)
            .await?
            .is_none()
    );
    let history = s.reminder_delivery_history(&a, None, 200).await?;
    assert!(
        history["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["id"] == delivery_id && v["state"] == "failed")
    );
    // A stale cancellation cannot override a more recent source event.
    import(
        &s,
        &a,
        &source,
        &feed("20260911T100000Z").replace("UID:trip", "UID:trip\r\nSEQUENCE:10"),
        now() + 70,
    )
    .await?;
    let stale = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:CANCEL\r\nBEGIN:VEVENT\r\nUID:trip\r\nSEQUENCE:9\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    import(&s, &a, &source, stale, now() + 71).await?;
    let event: ics::Event = serde_json::from_value(
        s.calendar_resources(&a, "event", Some(&source), None, 200)
            .await?[0]
            .value
            .clone(),
    )?;
    assert_eq!(event.status, ics::EventStatus::Present);
    assert_eq!(event.sequence, 10);
    // Completed work is left untouched when its source moves or is cancelled.
    let progress_version: i64 = sqlx::query_scalar("SELECT r.version FROM resources r JOIN occurrence_participants p ON p.progress_id=r.id WHERE p.occurrence_id=$1 AND p.account_id=$2").bind(&moved.id).bind(&a).fetch_one(&s.pool).await?;
    s.task_command(
        &a,
        &id(),
        &TaskCommand::Record {
            occurrence_id: moved.id.clone(),
            subject_account_id: a.clone(),
            entry_id: id(),
            evidence: Evidence::Checkbox { complete: true },
            replaces: None,
            expected_version: Some(progress_version),
            happened_at: due,
        },
        None,
        due,
    )
    .await?;
    let before_reviews = s
        .calendar_resources(&a, "review", Some(&moved.id), None, 200)
        .await?
        .len();
    import(
        &s,
        &a,
        &source,
        &feed("20260912T100000Z").replace("UID:trip", "UID:trip\r\nSEQUENCE:11"),
        due + 1,
    )
    .await?;
    assert_eq!(
        s.calendar_resources(&a, "review", Some(&moved.id), None, 200)
            .await?
            .len(),
        before_reviews
    );
    let preserved = s
        .occurrence_view(&a, &view(task.clone()), due + 1)
        .await?
        .items
        .remove(0);
    assert_eq!(preserved.data.slot.date, moved.data.slot.date);
    s.schedule_reminders(due + 1).await?;
    assert!(s.claim_reminder_delivery(due + 1).await?.is_none());
    // Seeing a calendar source must not reveal the UID of a private event via
    // another shared task's binding configuration.
    let execution: String = sqlx::query_scalar("SELECT execution_id FROM tasks WHERE task_id=$1")
        .bind(&task)
        .fetch_one(&s.pool)
        .await?;
    for resource in [&source, &task, &execution, &moved.id] {
        s.apply(
            &a,
            &id(),
            &[atlas_core::Command::Grant {
                id: resource.clone(),
                expected_version: 1,
                account_id: b.clone(),
                edit: false,
            }],
        )
        .await?;
    }
    assert!(s.task_anchor(&b, &task).await?.is_none());
    s.apply(
        &a,
        &id(),
        &[atlas_core::Command::Grant {
            id: events[0].id.clone(),
            expected_version: 1,
            account_id: b.clone(),
            edit: false,
        }],
    )
    .await?;
    assert!(s.task_anchor(&b, &task).await?.is_some());

    Ok(())
}
#[tokio::test]
async fn calendars() -> Result<()> {
    let (_dir, url) = crate::support::database::database_url().await?;
    let s = Store::connect(&url).await?;
    let other = Store::connect(&url).await?;
    scenario(s.clone(), other).await?;
    people_and_private_anchors(s).await
}

async fn people_and_private_anchors(s: Store) -> Result<()> {
    let a = account(&s).await?;
    let b = account(&s).await?;
    let person = id();
    let field = id();
    s.apply(
        &a,
        &id(),
        &[atlas_core::Command::CreatePerson {
            id: person.clone(),
            name: "Friend".into(),
            initial_policy: None,
        }],
    )
    .await?;
    s.task_command(
        &a,
        &id(),
        &TaskCommand::PutField {
            id: field.clone(),
            parent_id: person,
            expected_version: None,
            label: "Birthday".into(),
            value: FieldValue::Date {
                year: None,
                month: 9,
                day: 20,
            },
            initial_policy: None,
        },
        None,
        now(),
    )
    .await?;
    let task = id();
    let execution = id();
    s.task_command(
        &a,
        &id(),
        &TaskCommand::CreateTask {
            id: task.clone(),
            execution_id: execution.clone(),
            title: "Buy gift".into(),
            definition: definition(),
            initial_policy: None,
            anchor: Some(Anchor {
                reference: Reference::PersonDate {
                    field_id: field.clone(),
                    annual: true,
                },
                offset: Offset::CalendarDays {
                    days: -7,
                    time: Some("09:00:00".into()),
                },
                weekdays: vec![],
                title_contains: None,
            }),
        },
        None,
        now(),
    )
    .await?;
    let filter = ViewFilter {
        task_id: Some(task.clone()),
        ..Default::default()
    };
    let rows = s.occurrence_view(&a, &filter, now()).await?.items;
    let occurrence = rows
        .iter()
        .find(|r| r.data.slot.date.as_deref() == Some("2026-09-13"))
        .unwrap();
    // Sharing an existing task doesn't disclose its private person-date source.
    for resource in [&task, &execution, &occurrence.id] {
        s.apply(
            &a,
            &id(),
            &[atlas_core::Command::Grant {
                id: resource.clone(),
                expected_version: 1,
                account_id: b.clone(),
                edit: true,
            }],
        )
        .await?;
    }
    assert!(s.task_anchor(&b, &task).await?.is_none());
    s.task_command(
        &a,
        &id(),
        &TaskCommand::PutField {
            id: field.clone(),
            parent_id: s
                .resources(&a, "field", None, false, None, 200)
                .await?
                .iter()
                .find(|p| p.id == field)
                .unwrap()
                .parent_id
                .clone()
                .unwrap(),
            expected_version: Some(1),
            label: "Birthday".into(),
            value: FieldValue::Date {
                year: None,
                month: 9,
                day: 21,
            },
            initial_policy: None,
        },
        None,
        now(),
    )
    .await?;
    s.reconcile_calendar_tasks(now()).await?;
    let public = s.occurrence_view(&b, &filter, now()).await?.items;
    assert_eq!(public.len(), 1);
    assert_eq!(public[0].data.slot.date, occurrence.data.slot.date);
    assert!(
        s.calendar_resources(&b, "review", Some(&occurrence.id), None, 200)
            .await?
            .is_empty()
    );
    assert!(
        s.calendar_resources(&a, "review", Some(&occurrence.id), None, 200)
            .await?
            .iter()
            .any(|p| p.value.to_string().contains("context_unavailable"))
    );
    // Revocation between claim and dispatch suppresses a notification, even if
    // the worker already holds a valid lease. Device retirement erases settings.
    let rule = ReminderRule {
        offset_seconds: 0,
        time: Some("09:00:00".into()),
        late_seconds: 3600,
        enabled: true,
        delivery: DeliveryMode::Server,
    };
    s.reminder_command(
        &b,
        &id(),
        &ReminderCommand::SetRule {
            id: id(),
            occurrence_id: occurrence.id.clone(),
            expected_version: None,
            rule,
        },
    )
    .await?;
    let sub = id();
    s.reminder_command(
        &b,
        &id(),
        &ReminderCommand::SetSubscription {
            id: sub.clone(),
            expected_version: 0,
            device_id: "phone".into(),
            transport: "ntfy".into(),
            secret: "encrypted-test".into(),
            enabled: true,
        },
    )
    .await?;
    let due = Utc
        .with_ymd_and_hms(2026, 9, 13, 9, 0, 0)
        .unwrap()
        .timestamp();
    s.schedule_reminders(due).await?;
    let lease = s.claim_reminder_delivery(due).await?.unwrap();
    s.apply(
        &a,
        &id(),
        &[atlas_core::Command::Revoke {
            id: occurrence.id.clone(),
            expected_version: 2,
            account_id: b.clone(),
        }],
    )
    .await?;
    assert!(
        s.validate_reminder_delivery(&lease.id, &lease.token, due + 1)
            .await?
            .is_none()
    );
    s.schedule_reminders(due + 1).await?;
    assert!(s.claim_reminder_delivery(due + 61).await?.is_none());
    s.forget_device(&b, "phone").await?;
    assert_eq!(
        s.notification_subscriptions(&b).await?["subscriptions"][0]["enabled"],
        false
    );
    let secret: String =
        sqlx::query_scalar("SELECT secret FROM notification_subscriptions WHERE id=$1")
            .bind(sub)
            .fetch_one(&s.pool)
            .await?;
    assert!(secret.is_empty());

    // Event UUIDs must not decide streak order.
    let source = source(&s, &a).await?;
    import(
        &s,
        &a,
        &source,
        &feed("20260907T090000Z")
            .replace("SUMMARY:Trip", "RRULE:FREQ=DAILY;COUNT=6\r\nSUMMARY:Trip"),
        now(),
    )
    .await?;
    let task = id();
    s.task_command(
        &a,
        &id(),
        &TaskCommand::CreateTask {
            id: task.clone(),
            execution_id: id(),
            title: "Prepare".into(),
            definition: definition(),
            initial_policy: None,
            anchor: Some(Anchor {
                reference: Reference::EventSeries {
                    source_id: source,
                    uid: "trip".into(),
                },
                offset: Offset::CalendarDays {
                    days: 0,
                    time: None,
                },
                weekdays: vec![],
                title_contains: None,
            }),
        },
        None,
        now(),
    )
    .await?;
    let filter = ViewFilter {
        task_id: Some(task.clone()),
        ..Default::default()
    };
    let rows = s.occurrence_view(&a, &filter, now()).await?.items;
    assert_eq!(rows.len(), 6);
    for row in rows {
        if row.data.slot.date.as_deref() == Some("2026-09-11") {
            continue;
        }
        let at = row.data.slot.instant.unwrap();
        s.task_command(
            &a,
            &id(),
            &TaskCommand::Record {
                occurrence_id: row.id,
                subject_account_id: a.clone(),
                entry_id: id(),
                evidence: Evidence::Checkbox { complete: true },
                replaces: None,
                expected_version: Some(1),
                happened_at: at,
            },
            None,
            at,
        )
        .await?;
    }
    let streak = s.task_streak(&a, &task, due).await?;
    assert_eq!(streak.current, Some(1));
    assert_eq!(streak.longest, Some(4));

    Ok(())
}
