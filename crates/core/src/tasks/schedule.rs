use super::*;
use crate::error::ErrorCode;
use chrono::{
    Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeZone,
    Timelike,
};
use chrono_tz::Tz;
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Frequency {
    Daily,
    Weekly,
    Monthly,
    Yearly,
    AfterCompletion,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Cadence {
    pub frequency: Frequency,
    pub interval: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    /// None denotes a one-off undated inbox item.
    pub start_date: Option<String>,
    /// None is a date-only task; time is never inferred from the client.
    pub time: Option<String>,
    pub timezone: String,
    pub repeat: Option<Cadence>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Slot {
    pub key: String,
    pub date: Option<String>,
    pub intended_time: Option<String>,
    pub timezone: String,
    pub instant: Option<i64>,
}
pub(super) fn date(value: &str) -> Result<NaiveDate> {
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| anyhow!(ErrorCode::InvalidValue))?;
    ensure!(
        (1900..=9999).contains(&date.year()) && date.format("%Y-%m-%d").to_string() == value,
        ErrorCode::InvalidValue
    );
    Ok(date)
}
impl Schedule {
    pub fn validate(&self) -> Result<()> {
        self.timezone
            .parse::<Tz>()
            .map_err(|_| anyhow!(ErrorCode::InvalidValue))?;
        if let Some(start) = &self.start_date {
            date(start)?;
        }
        if let Some(time) = &self.time {
            ensure!(self.start_date.is_some(), ErrorCode::InvalidValue);
            let parsed = NaiveTime::parse_from_str(time, "%H:%M:%S")
                .map_err(|_| anyhow!(ErrorCode::InvalidValue))?;
            ensure!(
                parsed.format("%H:%M:%S").to_string() == *time && parsed.nanosecond() == 0,
                ErrorCode::InvalidValue
            );
        }
        if let Some(repeat) = &self.repeat {
            ensure!(
                self.start_date.is_some() && (1..=366).contains(&repeat.interval),
                ErrorCode::InvalidValue
            );
        }
        Ok(())
    }
    /// A bounded page of intended slots. `after` is the last persisted slot date;
    /// callers persist every returned slot, including periods covered by a chore.
    /// `more` explicitly reports an incomplete expansion, never silent truncation.
    pub fn slots(
        &self,
        after: Option<&str>,
        through: &str,
        limit: usize,
    ) -> Result<(Vec<Slot>, bool)> {
        self.validate()?;
        ensure!((1..=200).contains(&limit), ErrorCode::InvalidValue);
        let through = date(through)?;
        let after = after.map(date).transpose()?;
        if self
            .repeat
            .as_ref()
            .is_some_and(|r| r.frequency == Frequency::AfterCompletion)
        {
            if after.is_some() {
                return Ok((vec![], false));
            }
            let mut initial = self.clone();
            initial.repeat = None;
            return initial.slots(None, &through.to_string(), limit);
        }
        let Some(start) = &self.start_date else {
            return Ok((
                vec![Slot {
                    key: "once".into(),
                    date: None,
                    intended_time: None,
                    timezone: self.timezone.clone(),
                    instant: None,
                }],
                false,
            ));
        };
        let start = date(start)?;
        let mut slots = Vec::new();
        let mut index = match (&self.repeat, after) {
            (Some(cadence), Some(after)) if after > start => {
                let distance = match cadence.frequency {
                    Frequency::AfterCompletion => unreachable!(),
                    Frequency::Daily => after.signed_duration_since(start).num_days(),
                    Frequency::Weekly => after.signed_duration_since(start).num_days() / 7,
                    Frequency::Monthly => i64::from(
                        (after.year() - start.year()) * 12 + after.month() as i32
                            - start.month() as i32,
                    ),
                    Frequency::Yearly => i64::from(after.year() - start.year()),
                };
                (distance / i64::from(cadence.interval)) as u32
            }
            _ => 0,
        };
        let mut probes = 0;
        // Calendar arithmetic starts from DTSTART for every candidate, avoiding
        // cumulative month-end clamping or DST drift.
        loop {
            ensure!(probes < 4096, ErrorCode::ExpansionLimit);
            probes += 1;
            let candidate = if let Some(repeat) = &self.repeat {
                let n = index
                    .checked_mul(u32::from(repeat.interval))
                    .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
                match repeat.frequency {
                    Frequency::AfterCompletion => unreachable!(),
                    Frequency::Daily => start.checked_add_signed(Duration::days(i64::from(n))),
                    Frequency::Weekly => start.checked_add_signed(Duration::weeks(i64::from(n))),
                    Frequency::Monthly | Frequency::Yearly => {
                        let months = if repeat.frequency == Frequency::Yearly {
                            n.checked_mul(12)
                                .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?
                        } else {
                            n
                        };
                        let absolute = (start.year() as u32) * 12 + start.month0() + months;
                        if absolute / 12 > 9999 {
                            break;
                        }
                        NaiveDate::from_ymd_opt(
                            (absolute / 12) as i32,
                            absolute % 12 + 1,
                            start.day(),
                        )
                    }
                }
            } else if index == 0 {
                Some(start)
            } else {
                break;
            };
            index += 1;
            let Some(candidate) = candidate else {
                continue;
            };
            if candidate > through {
                break;
            }
            if after.is_some_and(|a| candidate <= a) {
                continue;
            }
            if slots.len() == limit {
                return Ok((slots, true));
            }
            let intended_time = self.time.clone();
            let instant = intended_time
                .as_ref()
                .map(|t| -> Result<i64> {
                    let time = NaiveTime::parse_from_str(t, "%H:%M:%S")?;
                    resolve(self.timezone.parse()?, candidate.and_time(time))
                })
                .transpose()?;
            let date = candidate.format("%Y-%m-%d").to_string();
            let key = if self.repeat.is_none() {
                "once".into()
            } else {
                format!("{date}T{}", self.time.as_deref().unwrap_or("date"))
            };
            slots.push(Slot {
                key,
                date: Some(date),
                intended_time,
                timezone: self.timezone.clone(),
                instant,
            });
        }
        Ok((slots, false))
    }
}
/// Atlas wall-time semantics: first fold occurrence; preserve the position in a
/// gap, including half-hour and date-line changes. Intended keys remain unchanged.
pub fn resolve(zone: Tz, wall: NaiveDateTime) -> Result<i64> {
    match zone.from_local_datetime(&wall) {
        LocalResult::Single(t) => Ok(t.timestamp()),
        LocalResult::Ambiguous(a, b) => Ok(a.min(b).timestamp()),
        LocalResult::None => {
            for minute in 1..=2880 {
                let before = wall
                    .checked_sub_signed(Duration::minutes(minute))
                    .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
                let after = wall
                    .checked_add_signed(Duration::minutes(minute))
                    .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
                if let (Some(a), Some(b)) = (
                    zone.from_local_datetime(&before).earliest(),
                    zone.from_local_datetime(&after).earliest(),
                ) {
                    let gap =
                        b.offset().fix().local_minus_utc() - a.offset().fix().local_minus_utc();
                    return zone
                        .from_local_datetime(
                            &wall
                                .checked_add_signed(Duration::seconds(gap.into()))
                                .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?,
                        )
                        .earliest()
                        .map(|t| t.timestamp())
                        .ok_or_else(|| anyhow!(ErrorCode::InvalidValue));
                }
            }
            Err(anyhow!(ErrorCode::InvalidValue))
        }
    }
}
