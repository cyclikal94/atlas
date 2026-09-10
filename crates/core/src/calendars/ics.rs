//! Bounded iCalendar ingestion. Unsupported semantics fail the whole import.
use crate::error::ErrorCode;
use anyhow::{Result, anyhow, ensure};
use chrono::{
    Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike,
};
use chrono_tz::Tz;
use ical::{parser::ical::component::IcalEvent, property::Property};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::BufReader,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventTime {
    pub date: String,
    pub time: Option<String>,
    pub timezone: String,
    pub instant: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventStatus {
    Present,
    Cancelled,
    Missing,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    pub uid: String,
    pub original: String,
    pub title: String,
    pub start: EventTime,
    pub end: Option<EventTime>,
    pub status: EventStatus,
    pub sequence: i64,
}
#[derive(Clone, Debug)]
pub struct Feed {
    pub events: Vec<Event>,
    pub cancelled_series: BTreeMap<String, i64>,
    pub cancelled_instances: BTreeMap<(String, String), i64>,
    pub cancellations_only: bool,
    pub from: String,
    pub through: String,
}
#[derive(Clone, Debug)]
struct Stamp {
    local: NaiveDateTime,
    zone: Tz,
    date_only: bool,
}
impl Stamp {
    fn key(&self) -> String {
        self.local
            .format(if self.date_only {
                "%Y%m%d"
            } else {
                "%Y%m%dT%H%M%S"
            })
            .to_string()
    }
    fn project(&self, generated: bool) -> Result<Option<EventTime>> {
        let instant = if self.date_only {
            None
        } else {
            if generated
                && matches!(
                    self.zone.from_local_datetime(&self.local),
                    LocalResult::None
                )
            {
                return Ok(None);
            }
            Some(crate::tasks::resolve(self.zone, self.local)?)
        };
        Ok(Some(EventTime {
            date: self.local.date().to_string(),
            time: (!self.date_only).then(|| self.local.time().format("%H:%M:%S").to_string()),
            timezone: self.zone.to_string(),
            instant,
        }))
    }
}
fn property<'a>(e: &'a IcalEvent, name: &str) -> Result<Option<&'a Property>> {
    let p = e
        .properties
        .iter()
        .filter(|p| p.name == name)
        .collect::<Vec<_>>();
    ensure!(p.len() <= 1, ErrorCode::InvalidIcs);
    Ok(p.first().copied())
}
fn value<'a>(e: &'a IcalEvent, name: &str) -> Result<Option<&'a str>> {
    Ok(property(e, name)?.and_then(|p| p.value.as_deref()))
}
fn parameter<'a>(p: &'a Property, name: &str) -> Result<Option<&'a str>> {
    let vals = p
        .params
        .as_ref()
        .into_iter()
        .flatten()
        .filter(|(n, _)| n == name)
        .collect::<Vec<_>>();
    ensure!(vals.len() <= 1, ErrorCode::InvalidIcs);
    if let Some((_, v)) = vals.first() {
        ensure!(v.len() == 1, ErrorCode::InvalidIcs);
        Ok(Some(&v[0]))
    } else {
        Ok(None)
    }
}
fn stamp(p: &Property, default: Tz, raw: Option<&str>) -> Result<Stamp> {
    let text = raw
        .or(p.value.as_deref())
        .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?;
    let date_only = parameter(p, "VALUE")? == Some("DATE");
    ensure!(
        parameter(p, "VALUE")?.is_none_or(|v| matches!(v, "DATE" | "DATE-TIME")),
        ErrorCode::UnsupportedCalendarRule
    );
    ensure!(
        text.is_ascii()
            && if date_only {
                text.len() == 8
            } else {
                text.len() == 15 || (text.len() == 16 && text.ends_with('Z'))
            },
        ErrorCode::InvalidIcs
    );
    let local = if date_only {
        NaiveDate::parse_from_str(text, "%Y%m%d")
            .map_err(|_| anyhow!(ErrorCode::InvalidIcs))?
            .and_time(NaiveTime::MIN)
    } else {
        NaiveDateTime::parse_from_str(text.strip_suffix('Z').unwrap_or(text), "%Y%m%dT%H%M%S")
            .map_err(|_| anyhow!(ErrorCode::InvalidIcs))?
    };
    ensure!(
        (1900..=9999).contains(&local.year()) && local.nanosecond() == 0,
        ErrorCode::InvalidIcs
    );
    let tzid = parameter(p, "TZID")?;
    ensure!(
        !(text.ends_with('Z') && tzid.is_some()) && !(date_only && tzid.is_some()),
        ErrorCode::InvalidIcs
    );
    let zone = if text.ends_with('Z') {
        chrono_tz::UTC
    } else if let Some(tz) = tzid {
        tz.parse()
            .map_err(|_| anyhow!(ErrorCode::UnsupportedCalendarTimezone))?
    } else {
        default
    };
    Ok(Stamp {
        local,
        zone,
        date_only,
    })
}
fn unescape(s: &str) -> Result<String> {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(match chars.next() {
                Some('n' | 'N') => '\n',
                Some('\\') => '\\',
                Some(',') => ',',
                Some(';') => ';',
                _ => return Err(anyhow!(ErrorCode::InvalidIcs)),
            });
        } else {
            out.push(c)
        }
    }
    Ok(out)
}
fn rule_dates(
    start: &Stamp,
    rule: &str,
    through: NaiveDate,
    budget: &mut usize,
) -> Result<Vec<Stamp>> {
    ensure!(rule.len() <= 512, ErrorCode::CalendarLimit);
    let mut parts = BTreeMap::new();
    for p in rule.split(';') {
        let (k, v) = p
            .split_once('=')
            .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?;
        ensure!(
            matches!(
                k,
                "FREQ"
                    | "INTERVAL"
                    | "COUNT"
                    | "UNTIL"
                    | "BYDAY"
                    | "BYMONTH"
                    | "BYMONTHDAY"
                    | "WKST"
            ) && parts.insert(k, v).is_none(),
            ErrorCode::UnsupportedCalendarRule
        );
    }
    let freq = *parts
        .get("FREQ")
        .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?;
    ensure!(
        matches!(freq, "DAILY" | "WEEKLY" | "MONTHLY" | "YEARLY"),
        ErrorCode::UnsupportedCalendarRule
    );
    let interval = parts
        .get("INTERVAL")
        .unwrap_or(&"1")
        .parse::<i64>()
        .map_err(|_| anyhow!(ErrorCode::InvalidIcs))?;
    ensure!((1..=366).contains(&interval), ErrorCode::CalendarLimit);
    let count = parts
        .get("COUNT")
        .map(|v| v.parse::<usize>())
        .transpose()
        .map_err(|_| anyhow!(ErrorCode::InvalidIcs))?;
    ensure!(
        count.is_none_or(|v| (1..=100000).contains(&v))
            && !(count.is_some() && parts.contains_key("UNTIL")),
        ErrorCode::CalendarLimit
    );
    let days = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];
    let byday = parts
        .get("BYDAY")
        .map(|s| {
            s.split(',')
                .map(|v| {
                    ensure!(
                        v.len() >= 2 && v.is_ascii(),
                        ErrorCode::UnsupportedCalendarRule
                    );
                    let (number, day) = v.split_at(v.len() - 2);
                    let ordinal = if number.is_empty() {
                        0
                    } else {
                        number
                            .parse::<i32>()
                            .map_err(|_| anyhow!(ErrorCode::UnsupportedCalendarRule))?
                    };
                    ensure!(
                        (-5..=5).contains(&ordinal)
                            && (ordinal == 0 || matches!(freq, "MONTHLY" | "YEARLY")),
                        ErrorCode::UnsupportedCalendarRule
                    );
                    let weekday = days
                        .iter()
                        .position(|d| *d == day)
                        .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?
                        as u32;
                    Ok((weekday, ordinal))
                })
                .collect::<Result<BTreeSet<_>>>()
        })
        .transpose()?;
    ensure!(
        !(freq == "WEEKLY" && parts.contains_key("BYMONTHDAY")),
        ErrorCode::UnsupportedCalendarRule
    );
    let numbers = |name: &str, min: i32, max: i32| -> Result<Option<BTreeSet<i32>>> {
        parts
            .get(name)
            .map(|s| {
                s.split(',')
                    .map(|v| {
                        let n = v
                            .parse::<i32>()
                            .map_err(|_| anyhow!(ErrorCode::InvalidIcs))?;
                        ensure!(n != 0 && (min..=max).contains(&n), ErrorCode::InvalidIcs);
                        Ok(n)
                    })
                    .collect()
            })
            .transpose()
    };
    let months = numbers("BYMONTH", 1, 12)?;
    let monthdays = numbers("BYMONTHDAY", -31, 31)?;
    ensure!(
        freq != "YEARLY"
            || months.is_some()
            || byday
                .as_ref()
                .is_none_or(|v| v.iter().all(|(_, n)| *n == 0)),
        ErrorCode::UnsupportedCalendarRule
    );
    let wkst = parts.get("WKST").unwrap_or(&"MO");
    let wkst = days
        .iter()
        .position(|d| d == wkst)
        .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))? as i64;
    let until = parts
        .get("UNTIL")
        .map(|raw| {
            let p = Property {
                name: "UNTIL".into(),
                params: if start.date_only {
                    Some(vec![("VALUE".into(), vec!["DATE".into()])])
                } else {
                    None
                },
                value: Some((*raw).into()),
            };
            stamp(&p, start.zone, None)
        })
        .transpose()?;
    let start_week = start.local.date()
        - Duration::days(
            (i64::from(start.local.weekday().num_days_from_monday()) - wkst).rem_euclid(7),
        );
    let mut date = start.local.date();
    let mut found = Vec::new();
    let mut valid_count = 0;
    while date <= through {
        ensure!(*budget > 0, ErrorCode::CalendarLimit);
        *budget -= 1;
        let daydiff = (date - start.local.date()).num_days();
        let md = (date.year() - start.local.year()) * 12 + date.month() as i32
            - start.local.month() as i32;
        let period = match freq {
            "DAILY" => daydiff % interval == 0,
            "WEEKLY" => ((date - start_week).num_days() / 7) % interval == 0,
            "MONTHLY" => i64::from(md) % interval == 0,
            _ => i64::from(date.year() - start.local.year()) % interval == 0,
        };
        let next_month = if date.month() == 12 {
            NaiveDate::from_ymd_opt(date.year() + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(date.year(), date.month() + 1, 1)
        }
        .ok_or_else(|| anyhow!(ErrorCode::CalendarLimit))?;
        let last = (next_month - Duration::days(1)).day() as i32;
        let month_match = months.as_ref().map_or(
            freq != "YEARLY"
                || byday.is_some()
                || monthdays.is_some()
                || date.month() == start.local.month(),
            |v| v.contains(&(date.month() as i32)),
        );
        let day_match = monthdays.as_ref().map_or(
            !matches!(freq, "MONTHLY" | "YEARLY")
                || byday.is_some()
                || date.day() == start.local.day(),
            |v| v.contains(&(date.day() as i32)) || v.contains(&(date.day() as i32 - last - 1)),
        );
        let weekday_match = byday.as_ref().map_or(
            freq != "WEEKLY" || date.weekday() == start.local.weekday(),
            |v| {
                v.iter().any(|(day, n)| {
                    *day == date.weekday().num_days_from_monday()
                        && (*n == 0
                            || *n == (date.day() as i32 - 1) / 7 + 1
                            || *n == -((last - date.day() as i32) / 7 + 1))
                })
            },
        );
        if period && month_match && day_match && weekday_match {
            let candidate = Stamp {
                local: date.and_time(start.local.time()),
                ..start.clone()
            };
            if let Some(end) = &until {
                let past = if end.date_only {
                    date > end.local.date()
                } else if end.zone == chrono_tz::UTC {
                    crate::tasks::resolve(candidate.zone, candidate.local)?
                        > end.local.and_utc().timestamp()
                } else {
                    candidate.local > end.local
                };
                if past {
                    break;
                }
            }
            // DTSTART is explicit; only generated gap times are omitted.
            if candidate.project(candidate.local != start.local)?.is_some() {
                valid_count += 1;
                found.push(candidate);
                if count.is_some_and(|n| valid_count >= n) {
                    break;
                }
            }
        }
        date = date
            .succ_opt()
            .ok_or_else(|| anyhow!(ErrorCode::CalendarLimit))?;
    }
    Ok(found)
}

pub fn parse(input: &str, timezone: &str, from: &str, through: &str) -> Result<Feed> {
    ensure!(
        input.len() <= 1024 * 1024 && input.lines().count() <= 20000,
        ErrorCode::CalendarLimit
    );
    let zone = timezone
        .parse::<Tz>()
        .map_err(|_| anyhow!(ErrorCode::InvalidValue))?;
    let from_date = NaiveDate::parse_from_str(from, "%Y-%m-%d")
        .map_err(|_| anyhow!(ErrorCode::InvalidValue))?;
    let through_date = NaiveDate::parse_from_str(through, "%Y-%m-%d")
        .map_err(|_| anyhow!(ErrorCode::InvalidValue))?;
    ensure!(
        from_date.to_string() == from
            && through_date.to_string() == through
            && (1900..=9999).contains(&from_date.year())
            && (1900..=9999).contains(&through_date.year()),
        ErrorCode::InvalidValue
    );
    ensure!(
        (0..=730).contains(&(through_date - from_date).num_days()),
        ErrorCode::CalendarLimit
    );
    let mut calendars = ical::IcalParser::new(BufReader::new(input.as_bytes()));
    let calendar = calendars
        .next()
        .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?
        .map_err(|_| anyhow!(ErrorCode::InvalidIcs))?;
    ensure!(calendars.next().is_none(), ErrorCode::InvalidIcs);
    // Do not silently replace custom definitions with similarly named IANA rules.
    validate_timezones(&calendar.timezones, from_date, through_date)?;
    ensure!(calendar.events.len() <= 256, ErrorCode::CalendarLimit);
    let mut masters = BTreeMap::new();
    let mut overrides = BTreeMap::new();
    let methods = calendar
        .properties
        .iter()
        .filter(|p| p.name == "METHOD")
        .collect::<Vec<_>>();
    ensure!(methods.len() <= 1, ErrorCode::InvalidIcs);
    let method = methods.first().and_then(|p| p.value.as_deref());
    ensure!(
        method.is_none_or(|m| matches!(m, "PUBLISH" | "CANCEL")),
        ErrorCode::UnsupportedCalendarRule
    );
    let cancellations_only = method == Some("CANCEL");
    let mut cancelled_series = BTreeMap::new();
    let mut cancelled_instances = BTreeMap::new();
    for e in &calendar.events {
        let uid = value(e, "UID")?.ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?;
        ensure!(!uid.is_empty() && uid.len() <= 512, ErrorCode::InvalidIcs);
        if let Some(p) = property(e, "RECURRENCE-ID")? {
            ensure!(
                parameter(p, "RANGE")?.is_none(),
                ErrorCode::UnsupportedCalendarRule
            );
            let key = stamp(p, zone, None)?.key();
            ensure!(
                overrides.insert((uid.to_owned(), key), e).is_none(),
                ErrorCode::InvalidIcs
            );
        } else {
            ensure!(
                masters.insert(uid.to_owned(), e).is_none(),
                ErrorCode::InvalidIcs
            );
        }
    }
    // Exceptions may use UTC while the master uses a local zone. Convert the
    // recurrence identifier to the master's representation before matching it.
    let mut normalised = BTreeMap::new();
    for ((uid, key), event) in overrides {
        let key = if let Some(master) = masters.get(&uid)
            && let Some(start) = property(master, "DTSTART")?
        {
            let master = stamp(start, zone, None)?;
            let mut original = stamp(property(event, "RECURRENCE-ID")?.unwrap(), zone, None)?;
            ensure!(
                original.date_only == master.date_only,
                ErrorCode::InvalidIcs
            );
            if !original.date_only && original.zone != master.zone {
                original.local = chrono::DateTime::from_timestamp(
                    crate::tasks::resolve(original.zone, original.local)?,
                    0,
                )
                .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?
                .with_timezone(&master.zone)
                .naive_local();
            }
            original.key()
        } else {
            key
        };
        ensure!(
            normalised.insert((uid, key), event).is_none(),
            ErrorCode::InvalidIcs
        );
    }
    let overrides = normalised;
    let mut events = BTreeMap::new();
    let mut budget = 1_000_000;
    for (uid, e) in &masters {
        if cancellations_only || value(e, "STATUS")? == Some("CANCELLED") {
            cancelled_series.insert(uid.clone(), sequence(e)?);
            continue;
        }
        ensure!(
            property(e, "DURATION")?.is_none() && property(e, "EXRULE")?.is_none(),
            ErrorCode::UnsupportedCalendarRule
        );
        let start = stamp(
            property(e, "DTSTART")?.ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?,
            zone,
            None,
        )?;
        let recurring =
            property(e, "RRULE")?.is_some() || e.properties.iter().any(|p| p.name == "RDATE");
        let mut dates = if let Some(rule) = value(e, "RRULE")? {
            rule_dates(&start, rule, through_date, &mut budget)?
        } else {
            vec![start.clone()]
        };
        let mut excluded = BTreeSet::new();
        for p in e
            .properties
            .iter()
            .filter(|p| matches!(p.name.as_str(), "RDATE" | "EXDATE"))
        {
            for raw in p
                .value
                .as_deref()
                .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?
                .split(',')
            {
                let mut d = stamp(p, start.zone, Some(raw))?;
                if !d.date_only && d.zone != start.zone {
                    d.local = chrono::DateTime::from_timestamp(
                        crate::tasks::resolve(d.zone, d.local)?,
                        0,
                    )
                    .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?
                    .with_timezone(&start.zone)
                    .naive_local();
                    d.zone = start.zone;
                }
                ensure!(d.date_only == start.date_only, ErrorCode::InvalidIcs);
                if p.name == "EXDATE" {
                    excluded.insert(d.key());
                } else {
                    dates.push(d);
                }
            }
        }
        for d in dates {
            let key = if recurring { d.key() } else { "once".into() };
            if excluded.contains(&d.key()) {
                cancelled_instances.insert((uid.clone(), key), sequence(e)?);
                continue;
            }
            if overrides.contains_key(&(uid.clone(), key.clone())) {
                continue;
            }
            if d.local.date() < from_date || d.local.date() > through_date {
                continue;
            }
            // Rule expansion already removed generated gaps. DTSTART and RDATE
            // are explicit dates and retain their intended wall-time identity.
            if let Some(projected) = d.project(false)? {
                let event = build(e, uid, &key, &start, &d, projected)?;
                events.insert((uid.clone(), key), event);
            }
        }
    }
    for ((uid, key), e) in overrides {
        if cancellations_only || value(e, "STATUS")? == Some("CANCELLED") {
            cancelled_instances.insert((uid, key), sequence(e)?);
            continue;
        }
        let start = stamp(
            property(e, "DTSTART")?.ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?,
            zone,
            None,
        )?;
        // Detached exceptions outside coverage still update previously anchored work.
        let projected = start
            .project(false)?
            .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?;
        events.insert(
            (uid.clone(), key.clone()),
            build(e, &uid, &key, &start, &start, projected)?,
        );
    }
    ensure!(events.len() <= 2048, ErrorCode::CalendarLimit);
    Ok(Feed {
        events: events.into_values().collect(),
        cancelled_series,
        cancelled_instances,
        cancellations_only,
        from: from.into(),
        through: through.into(),
    })
}
fn sequence(e: &IcalEvent) -> Result<i64> {
    let sequence = value(e, "SEQUENCE")?
        .unwrap_or("0")
        .parse::<i64>()
        .map_err(|_| anyhow!(ErrorCode::InvalidIcs))?;
    ensure!(sequence >= 0, ErrorCode::InvalidIcs);
    Ok(sequence)
}
fn build(
    e: &IcalEvent,
    uid: &str,
    key: &str,
    start: &Stamp,
    instance: &Stamp,
    projected: EventTime,
) -> Result<Event> {
    let title = unescape(
        value(e, "SUMMARY")?
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Calendar event"),
    )?;
    crate::content(&title, "")?;
    let sequence = sequence(e)?;
    let end = property(e, "DTEND")?
        .map(|p| -> Result<EventTime> {
            let end = stamp(p, start.zone, None)?;
            ensure!(end.date_only == start.date_only, ErrorCode::InvalidIcs);
            ensure!(
                end.zone == start.zone,
                ErrorCode::UnsupportedCalendarTimezone
            );
            let delta = end.local - start.local;
            ensure!(delta >= Duration::zero(), ErrorCode::InvalidIcs);
            Stamp {
                local: instance
                    .local
                    .checked_add_signed(delta)
                    .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?,
                ..end
            }
            .project(false)?
            .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))
        })
        .transpose()?;
    Ok(Event {
        uid: uid.into(),
        original: key.into(),
        title,
        start: projected,
        end,
        status: EventStatus::Present,
        sequence,
    })
}

fn validate_timezones(
    zones: &[ical::parser::ical::component::IcalTimeZone],
    from: NaiveDate,
    through: NaiveDate,
) -> Result<()> {
    use chrono::Offset;
    ensure!(zones.len() <= 16, ErrorCode::CalendarLimit);
    let mut ids = BTreeSet::new();
    let mut budget = 1_000_000;
    for tz in zones {
        let id = tz
            .properties
            .iter()
            .find(|p| p.name == "TZID")
            .and_then(|p| p.value.as_deref())
            .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?;
        let zone = id
            .parse::<Tz>()
            .map_err(|_| anyhow!(ErrorCode::UnsupportedCalendarTimezone))?;
        ensure!(
            ids.insert(id) && tz.transitions.len() <= 32,
            ErrorCode::CalendarLimit
        );
        let offset = |e: &IcalEvent, name: &str| -> Result<i32> {
            let v = value(e, name)?.ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?;
            ensure!(
                v.is_ascii()
                    && matches!(v.len(), 5 | 7)
                    && matches!(v.as_bytes()[0], b'+' | b'-')
                    && v.as_bytes()[1..].iter().all(u8::is_ascii_digit),
                ErrorCode::InvalidIcs
            );
            let h = v[1..3].parse::<i32>()?;
            let m = v[3..5].parse::<i32>()?;
            let sec = if v.len() == 7 {
                v[5..7].parse::<i32>()?
            } else {
                0
            };
            ensure!(h < 24 && m < 60 && sec < 60, ErrorCode::InvalidIcs);
            Ok((h * 3600 + m * 60 + sec) * if v.starts_with('-') { -1 } else { 1 })
        };
        let mut transitions = BTreeMap::new();
        for component in &tz.transitions {
            let e = IcalEvent {
                properties: component.properties.clone(),
                alarms: vec![],
            };
            let start = stamp(
                property(&e, "DTSTART")?.ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?,
                chrono_tz::UTC,
                None,
            )?;
            let before = offset(&e, "TZOFFSETFROM")?;
            let after = offset(&e, "TZOFFSETTO")?;
            let mut dates = if let Some(rule) = value(&e, "RRULE")? {
                rule_dates(&start, rule, through + Duration::days(2), &mut budget)?
            } else {
                vec![start.clone()]
            };
            for p in e.properties.iter().filter(|p| p.name == "RDATE") {
                for v in p.value.as_deref().unwrap_or("").split(',') {
                    dates.push(stamp(p, chrono_tz::UTC, Some(v))?);
                }
            }
            for d in dates {
                let instant = d.local.and_utc().timestamp() - i64::from(before);
                transitions.insert(instant, after);
                if d.local.date() >= from - Duration::days(2)
                    && d.local.date() <= through + Duration::days(2)
                {
                    let actual = chrono::DateTime::from_timestamp(instant, 0)
                        .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?
                        .with_timezone(&zone)
                        .offset()
                        .fix()
                        .local_minus_utc();
                    let previous = chrono::DateTime::from_timestamp(instant - 1, 0)
                        .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?
                        .with_timezone(&zone)
                        .offset()
                        .fix()
                        .local_minus_utc();
                    ensure!(
                        actual == after && previous == before,
                        ErrorCode::UnsupportedCalendarTimezone
                    );
                }
            }
        }
        let mut day = from;
        while day <= through {
            let instant = day.and_hms_opt(12, 0, 0).unwrap().and_utc().timestamp();
            let expected = transitions
                .range(..=instant)
                .next_back()
                .map(|(_, v)| *v)
                .ok_or_else(|| anyhow!(ErrorCode::UnsupportedCalendarTimezone))?;
            let actual = chrono::DateTime::from_timestamp(instant, 0)
                .ok_or_else(|| anyhow!(ErrorCode::InvalidIcs))?
                .with_timezone(&zone)
                .offset()
                .fix()
                .local_minus_utc();
            ensure!(actual == expected, ErrorCode::UnsupportedCalendarTimezone);
            day = day
                .succ_opt()
                .ok_or_else(|| anyhow!(ErrorCode::CalendarLimit))?;
        }
    }
    Ok(())
}
