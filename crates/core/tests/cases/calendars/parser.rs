use crate::support::calendars::feed;
use anyhow::Result;
use atlas_core::calendars::ics;
#[test]
fn calendar_recurrence_exceptions_and_timezones() -> Result<()> {
    let input = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:gym\r\nDTSTART;TZID=Europe/Vienna:20260907T090000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE;COUNT=4\r\nEXDATE;TZID=Europe/Vienna:20260909T090000\r\nSUMMARY:Gym\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:gym\r\nRECURRENCE-ID;TZID=Europe/Vienna:20260914T090000\r\nDTSTART;TZID=Europe/Vienna:20260915T100000\r\nSUMMARY:Moved gym\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let f = ics::parse(input, "UTC", "2026-09-01", "2026-09-30")?;
    assert_eq!(f.events.len(), 3);
    assert!(
        f.cancelled_instances
            .contains_key(&("gym".into(), "20260909T090000".into()))
    );
    let moved = f
        .events
        .iter()
        .find(|v| v.original == "20260914T090000")
        .unwrap();
    assert_eq!(moved.start.date, "2026-09-15");
    let utc_exceptions = input
        .replace(
            "EXDATE;TZID=Europe/Vienna:20260909T090000",
            "EXDATE:20260909T070000Z",
        )
        .replace(
            "RECURRENCE-ID;TZID=Europe/Vienna:20260914T090000",
            "RECURRENCE-ID:20260914T070000Z",
        );
    assert_eq!(
        ics::parse(&utc_exceptions, "UTC", "2026-09-01", "2026-09-30")?.events,
        f.events
    );

    let date =
        feed("20260910T100000Z").replace("DTSTART:20260910T100000Z", "DTSTART;VALUE=DATE:20260910");
    assert!(
        ics::parse(&date, "Europe/Vienna", "2026-09-01", "2026-09-30")?.events[0]
            .start
            .instant
            .is_none()
    );
    assert!(
        ics::parse(
            &input.replace("FREQ=WEEKLY", "FREQ=SECONDLY"),
            "UTC",
            "2026-09-01",
            "2026-09-30"
        )
        .is_err()
    );
    let monthly = input.split("BEGIN:VEVENT").next().unwrap().to_owned()
        + "BEGIN:VEVENT\r\nUID:month\r\nDTSTART:20260927T090000Z\r\nRRULE:FREQ=MONTHLY;BYDAY=-1SU;COUNT=3\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    assert_eq!(
        ics::parse(&monthly, "UTC", "2026-09-01", "2026-12-31")?
            .events
            .len(),
        3
    );
    Ok(())
}

#[test]
fn strict_dates_cancellation_and_embedded_zones() -> Result<()> {
    for date in ["20260910T100000ZZ", "20260910T100060Z", "20260910T1000Z"] {
        assert!(
            ics::parse(&feed(date), "UTC", "2026-09-01", "2026-12-31").is_err(),
            "{date}"
        );
    }
    assert!(ics::parse(&feed("20260910T100000Z"), "UTC", "2026-9-1", "2026-12-31").is_err());
    let cancel = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:CANCEL\r\nBEGIN:VEVENT\r\nUID:trip\r\nSEQUENCE:7\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let parsed = ics::parse(cancel, "UTC", "2026-09-01", "2026-12-31")?;
    assert!(parsed.cancellations_only);
    assert_eq!(parsed.cancelled_series["trip"], 7);
    let zone = "BEGIN:VTIMEZONE\r\nTZID:Europe/Vienna\r\nBEGIN:STANDARD\r\nDTSTART:19961027T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r\nEND:STANDARD\r\nBEGIN:DAYLIGHT\r\nDTSTART:19960331T020000\r\nTZOFFSETFROM:+0100\r\nTZOFFSETTO:+0200\r\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\r\nEND:DAYLIGHT\r\nEND:VTIMEZONE\r\n";
    let input = feed("20260910T100000Z").replace("BEGIN:VEVENT", &format!("{zone}BEGIN:VEVENT"));
    assert_eq!(
        ics::parse(&input, "UTC", "2026-03-01", "2026-11-30")?
            .events
            .len(),
        1
    );
    assert_eq!(
        ics::parse(
            &input.replace("TZOFFSETTO:+0200", "TZOFFSETTO:+0300"),
            "UTC",
            "2026-03-01",
            "2026-11-30"
        )
        .unwrap_err()
        .to_string(),
        "unsupported_calendar_timezone"
    );
    let dst = feed("20260328T023000")
        .replace("DTSTART:", "DTSTART;TZID=Europe/Vienna:")
        .replace("SUMMARY:Trip", "RRULE:FREQ=DAILY;COUNT=3\r\nSUMMARY:Trip");
    let f = ics::parse(&dst, "UTC", "2026-03-28", "2026-04-01")?;
    assert_eq!(
        f.events
            .iter()
            .map(|e| e.start.date.as_str())
            .collect::<Vec<_>>(),
        ["2026-03-28", "2026-03-30", "2026-03-31"]
    );
    Ok(())
}

#[test]
fn imported_recurrence_counts_real_dates_and_keeps_original_slots() -> Result<()> {
    fn expand(start: &str, rule: &str, from: &str, through: &str) -> Result<ics::Feed> {
        ics::parse(
            &format!(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:recurrence\r\nDTSTART;TZID=Europe/Vienna:{start}\r\nRRULE:{rule}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            ),
            "UTC",
            from,
            through,
        )
    }
    let dates = |feed: &ics::Feed| {
        feed.events
            .iter()
            .map(|e| e.start.date.clone())
            .collect::<Vec<_>>()
    };
    let gap = expand(
        "20260328T023000",
        "FREQ=DAILY;COUNT=3",
        "2026-03-01",
        "2026-04-02",
    )?;
    assert_eq!(dates(&gap), ["2026-03-28", "2026-03-30", "2026-03-31"]);
    let explicit = expand(
        "20260329T023000",
        "FREQ=DAILY;COUNT=2",
        "2026-03-01",
        "2026-04-02",
    )?;
    assert_eq!(dates(&explicit), ["2026-03-29", "2026-03-30"]);
    assert_eq!(explicit.events[0].original, "20260329T023000");
    let months = expand(
        "20260131T090000",
        "FREQ=MONTHLY;COUNT=3",
        "2026-01-01",
        "2026-06-01",
    )?;
    assert_eq!(dates(&months), ["2026-01-31", "2026-03-31", "2026-05-31"]);
    let folds = expand(
        "20261024T023000",
        "FREQ=DAILY;COUNT=3",
        "2026-10-01",
        "2026-11-01",
    )?;
    assert_eq!(dates(&folds), ["2026-10-24", "2026-10-25", "2026-10-26"]);
    assert_eq!(folds.events[1].start.instant, Some(1792888200));
    assert!(expand("20260101T090000", "FREQ=HOURLY", "2026-01-01", "2026-02-01").is_err());
    Ok(())
}
