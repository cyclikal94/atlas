use atlas_core::tasks::*;
use chrono::{TimeZone, Timelike};
use chrono_tz::Tz;
use uuid::Uuid;
fn decimal(value: &str) -> Decimal {
    Decimal(value.into())
}
fn schedule(date: &str, time: Option<&str>, frequency: Frequency) -> Schedule {
    Schedule {
        start_date: Some(date.into()),
        time: time.map(str::to_owned),
        timezone: "Europe/Vienna".into(),
        repeat: Some(Cadence {
            frequency,
            interval: 1,
        }),
    }
}
#[test]
fn saved_wall_slots_survive_gaps_folds_and_timezone_changes() -> anyhow::Result<()> {
    let original = schedule("2026-03-28", Some("02:30:00"), Frequency::Daily);
    let (slots, more) = original.slots(None, "2026-03-30", 20)?;
    assert!(!more);
    assert_eq!(slots.len(), 3);
    let zone: Tz = "Europe/Vienna".parse()?;
    assert_eq!(
        zone.timestamp_opt(slots[1].instant.unwrap(), 0)
            .unwrap()
            .hour(),
        3
    );
    assert_eq!(
        zone.timestamp_opt(slots[2].instant.unwrap(), 0)
            .unwrap()
            .hour(),
        2
    );
    let mut changed = original.clone();
    changed.timezone = "America/New_York".into();
    let other = changed.slots(None, "2026-03-30", 20)?.0;
    assert_eq!(
        slots.iter().map(|s| &s.key).collect::<Vec<_>>(),
        other.iter().map(|s| &s.key).collect::<Vec<_>>()
    );
    assert_ne!(slots[0].instant, other[0].instant);
    let fold = schedule("2026-10-25", Some("02:30:00"), Frequency::Daily)
        .slots(None, "2026-10-25", 2)?
        .0;
    let expected = zone
        .with_ymd_and_hms(2026, 10, 25, 2, 30, 0)
        .earliest()
        .unwrap();
    assert_eq!(fold[0].instant, Some(expected.timestamp()));
    // Clock rollback just returns the same intended IDs, never new identities.
    assert_eq!(original.slots(None, "2026-03-29", 20)?.0, slots[..2]);
    Ok(())
}
#[test]
fn date_only_month_end_leap_day_and_bounded_pagination() -> anyhow::Result<()> {
    let dates = schedule("2026-01-31", None, Frequency::Monthly);
    let (first, more) = dates.slots(None, "2026-05-31", 2)?;
    assert!(more);
    assert_eq!(first[1].date.as_deref(), Some("2026-03-31"));
    assert!(first.iter().all(|s| s.instant.is_none()));
    let (second, more) = dates.slots(first[1].date.as_deref(), "2026-05-31", 2)?;
    assert!(!more);
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].date.as_deref(), Some("2026-05-31"));
    let leap = schedule("2024-02-29", None, Frequency::Yearly)
        .slots(None, "2028-02-29", 20)?
        .0;
    assert_eq!(leap.len(), 2);
    assert_eq!(leap[1].date.as_deref(), Some("2028-02-29"));
    let mut invalid = dates;
    invalid.timezone = "not/a-zone".into();
    assert!(invalid.slots(None, "2026-05-31", 20).is_err());
    Ok(())
}
#[test]
fn exact_quantities_and_explicit_observation() -> anyhow::Result<()> {
    for invalid in ["-1", "NaN", "1e3", "01", "1.", "0.1234567", "1000000000001"] {
        assert!(decimal(invalid).units().is_err(), "{invalid}");
    }
    let goal = Goal::Numeric {
        minimum: Some(decimal("0.3")),
        maximum: None,
        unit: "litres".into(),
    };
    let progress = evaluate(
        &goal,
        &[
            Evidence::Quantity {
                amount: decimal("0.1"),
            },
            Evidence::Quantity {
                amount: decimal("0.2"),
            },
        ],
        false,
    )?;
    assert_eq!(progress.outcome, Outcome::Complete);
    assert_eq!(progress.amount, Some(decimal("0.3")));
    let upper = Goal::Numeric {
        minimum: None,
        maximum: Some(decimal("2")),
        unit: "cups".into(),
    };
    assert_eq!(evaluate(&upper, &[], true)?.outcome, Outcome::Unknown);
    assert_eq!(
        evaluate(&upper, &[Evidence::Observe], false)?.outcome,
        Outcome::Provisional
    );
    assert_eq!(
        evaluate(&upper, &[Evidence::Observe], true)?.outcome,
        Outcome::Complete
    );
    assert_eq!(
        evaluate(
            &upper,
            &[Evidence::Quantity {
                amount: decimal("3")
            }],
            true
        )?
        .outcome,
        Outcome::Missed
    );
    Ok(())
}
#[test]
fn checklist_corrections_and_goal_snapshots() -> anyhow::Result<()> {
    let first = Uuid::new_v4().to_string();
    let second = Uuid::new_v4().to_string();
    let goal = Goal::Checklist {
        items: vec![
            ChecklistItem {
                id: first.clone(),
                label: "First".into(),
            },
            ChecklistItem {
                id: second.clone(),
                label: "Second".into(),
            },
        ],
    };
    let entries = vec![
        Evidence::Checklist {
            item_id: first.clone(),
            complete: true,
        },
        Evidence::Checklist {
            item_id: second,
            complete: true,
        },
    ];
    assert_eq!(evaluate(&goal, &entries, false)?.outcome, Outcome::Complete);
    let mut corrected = entries.clone();
    corrected.push(Evidence::Checklist {
        item_id: first,
        complete: false,
    });
    assert_eq!(
        evaluate(&goal, &corrected, false)?.outcome,
        Outcome::Incomplete
    );
    assert!(
        evaluate(
            &goal,
            &[Evidence::Quantity {
                amount: decimal("1")
            }],
            false
        )
        .is_err()
    );
    let old = Goal::Numeric {
        minimum: Some(decimal("3")),
        maximum: None,
        unit: "workouts".into(),
    };
    let new = Goal::Numeric {
        minimum: Some(decimal("5")),
        maximum: None,
        unit: "workouts".into(),
    };
    let entries = [Evidence::Quantity {
        amount: decimal("3"),
    }];
    assert_eq!(evaluate(&old, &entries, true)?.outcome, Outcome::Complete);
    assert_eq!(evaluate(&new, &entries, true)?.outcome, Outcome::Missed);
    Ok(())
}
#[test]
fn joint_progress_is_unknown_when_a_stream_is_hidden() -> anyhow::Result<()> {
    let complete = vec![Evidence::Checkbox { complete: true }];
    assert_eq!(
        aggregate(
            &Goal::Checkbox,
            Participation::Everyone,
            &[Some(complete.clone()), None],
            true
        )?,
        Outcome::Unknown
    );
    assert_eq!(
        aggregate(
            &Goal::Checkbox,
            Participation::Anyone,
            &[Some(complete.clone()), None],
            true
        )?,
        Outcome::Complete
    );
    assert_eq!(
        aggregate(
            &Goal::Checkbox,
            Participation::Everyone,
            &[Some(complete.clone()), Some(complete)],
            true
        )?,
        Outcome::Complete
    );
    let goal = Goal::Numeric {
        minimum: Some(decimal("3")),
        maximum: None,
        unit: "workouts".into(),
    };
    assert_eq!(
        aggregate(
            &goal,
            Participation::Pooled,
            &[
                Some(vec![Evidence::Quantity {
                    amount: decimal("1")
                }]),
                Some(vec![Evidence::Quantity {
                    amount: decimal("2")
                }])
            ],
            true
        )?,
        Outcome::Complete
    );
    let five = vec![(Outcome::Complete, false); 5];
    assert_eq!(
        streak(&five),
        Streak {
            current: Some(5),
            longest: Some(5)
        }
    );
    let mut periods = five;
    periods.push((Outcome::Excluded, false));
    assert_eq!(streak(&periods).current, Some(0));
    periods.last_mut().unwrap().1 = true;
    assert_eq!(streak(&periods).current, Some(5));
    periods.push((Outcome::Unknown, false));
    assert_eq!(streak(&periods).current, None);
    Ok(())
}

#[test]
fn date_line_and_half_hour_gaps_keep_distinct_intended_slots() -> anyhow::Result<()> {
    let mut samoa = schedule("2011-12-30", Some("09:00:00"), Frequency::Daily);
    samoa.timezone = "Pacific/Apia".into();
    let slots = samoa.slots(None, "2011-12-31", 20)?.0;
    assert_ne!(slots[0].key, slots[1].key);
    assert_eq!(slots[0].instant, slots[1].instant);
    let mut lord_howe = schedule("2026-10-04", Some("02:15:00"), Frequency::Daily);
    lord_howe.timezone = "Australia/Lord_Howe".into();
    let slot = lord_howe.slots(None, "2026-10-04", 20)?.0.remove(0);
    let zone: Tz = "Australia/Lord_Howe".parse()?;
    let actual = zone.timestamp_opt(slot.instant.unwrap(), 0).unwrap();
    assert_eq!((actual.hour(), actual.minute()), (2, 45));
    Ok(())
}
#[test]
fn old_series_seek_without_replaying_their_entire_lifetime() -> anyhow::Result<()> {
    let daily = schedule("1900-01-01", None, Frequency::Daily);
    let (slots, more) = daily.slots(Some("9000-01-01"), "9000-01-03", 20)?;
    assert!(!more);
    assert_eq!(slots.len(), 2);
    assert_eq!(slots[0].date.as_deref(), Some("9000-01-02"));
    let upper = Goal::Numeric {
        minimum: None,
        maximum: Some(decimal("2")),
        unit: "cups".into(),
    };
    assert_eq!(evaluate(&upper, &[], true)?.amount, None);
    assert_eq!(
        evaluate(&Goal::Checkbox, &[], true)?.outcome,
        Outcome::Missed
    );
    Ok(())
}

#[test]
fn domain_contract_example_matches_rust_values() -> anyhow::Result<()> {
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../../../../../api/openapi.json"))?;
    for example in schema["components"]["schemas"]["TaskDefinition"]["examples"]
        .as_array()
        .unwrap()
    {
        let definition: Definition = serde_json::from_value(example.clone())?;
        definition.validate()?;
        assert_eq!(serde_json::to_value(definition)?, *example);
    }
    Ok(())
}

#[test]
fn numeric_accumulation_rejects_overflow() -> anyhow::Result<()> {
    let goal = Goal::Numeric {
        minimum: Some(decimal("1")),
        maximum: None,
        unit: "units".into(),
    };
    assert!(
        evaluate(
            &goal,
            &[
                Evidence::Quantity {
                    amount: decimal("1000000000000")
                },
                Evidence::Quantity {
                    amount: decimal("1")
                }
            ],
            false
        )
        .is_err()
    );
    Ok(())
}
