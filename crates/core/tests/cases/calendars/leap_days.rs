use anyhow::Result;
use atlas_core::{Store, tasks::*};

const NOW: i64 = 1788868800;

use crate::support::task_fixtures::{account, definition, id, person, run, setup};

async fn leap_day(s: &Store) -> Result<()> {
    use atlas_core::calendars::{Anchor, Offset, Reference};
    let a = account(s).await?;
    let p = person(s, &a).await?;
    let field = id();
    let task = id();
    run(
        s,
        &a,
        TaskCommand::PutField {
            id: field.clone(),
            parent_id: p,
            expected_version: None,
            label: "Birthday".into(),
            value: FieldValue::Date {
                year: None,
                month: 2,
                day: 29,
            },
            initial_policy: None,
        },
        NOW,
    )
    .await?;
    run(
        s,
        &a,
        TaskCommand::CreateTask {
            id: task.clone(),
            execution_id: id(),
            title: "Birthday".into(),
            definition: definition(Goal::Checkbox),
            initial_policy: None,
            anchor: Some(Anchor {
                reference: Reference::PersonDate {
                    field_id: field,
                    annual: true,
                },
                offset: Offset::CalendarDays {
                    days: 0,
                    time: None,
                },
                weekdays: vec![],
                title_contains: None,
            }),
        },
        NOW,
    )
    .await?;
    let filter = ViewFilter {
        task_id: Some(task),
        ..Default::default()
    };
    let before = s.occurrence_view(&a, &filter, NOW).await?.items;
    assert!(
        before
            .iter()
            .any(|r| r.data.slot.date.as_deref() == Some("2027-02-28"))
    );
    let future = chrono::DateTime::parse_from_rfc3339("2028-01-08T12:00:00Z")?.timestamp();
    s.reconcile_calendar_tasks(future).await?;
    let after = s.occurrence_view(&a, &filter, future).await?.items;
    assert!(
        after
            .iter()
            .any(|r| r.data.slot.date.as_deref() == Some("2028-02-29"))
    );
    for row in before {
        assert!(
            after
                .iter()
                .any(|r| r.id == row.id && r.data.slot.date == row.data.slot.date)
        );
    }
    Ok(())
}

#[tokio::test]
async fn annual_leap_birthdays_use_february_28_and_keep_ids() -> Result<()> {
    let (s, _d) = setup().await?;
    leap_day(&s).await
}
