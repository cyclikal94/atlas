//! Task domain rules. Persisted occurrences snapshot these definitions so later
//! target edits cannot rewrite history. Calendar imports use a separate adapter.
use crate::error::ErrorCode;
mod presets;
mod schedule;
pub use presets::{Preset, presets};
mod service;
use anyhow::{Result, anyhow, ensure};
pub use schedule::{Cadence, Frequency, Schedule, Slot, resolve};
use serde::{Deserialize, Serialize};
pub use service::*;
use std::collections::{BTreeMap, BTreeSet};

/// Exact decimal values with at most six fractional digits. Wire values are
/// strings; arithmetic is checked integer arithmetic, never floating point.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Decimal(pub String);
impl Decimal {
    pub fn units(&self) -> Result<i128> {
        let text = &self.0;
        ensure!(
            !text.is_empty() && text.len() <= 21,
            ErrorCode::InvalidValue
        );
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        ensure!(
            !whole.is_empty() && whole.len() <= 13 && whole.bytes().all(|b| b.is_ascii_digit()),
            ErrorCode::InvalidValue
        );
        ensure!(
            whole == "0" || !whole.starts_with('0'),
            ErrorCode::InvalidValue
        );
        ensure!(
            fraction.len() <= 6 && fraction.bytes().all(|b| b.is_ascii_digit()),
            ErrorCode::InvalidValue
        );
        ensure!(
            !text.contains('.') || !fraction.is_empty(),
            ErrorCode::InvalidValue
        );
        let value = whole.parse::<i128>()? * 1_000_000
            + if fraction.is_empty() {
                0
            } else {
                fraction.parse::<i128>()? * 10_i128.pow(6 - fraction.len() as u32)
            };
        ensure!(value <= 1_000_000_000_000_000_000, ErrorCode::InvalidValue);
        Ok(value)
    }
    fn from_units(value: i128) -> Self {
        let whole = value / 1_000_000;
        let fraction = format!("{:06}", value % 1_000_000);
        let fraction = fraction.trim_end_matches('0');
        Self(if fraction.is_empty() {
            whole.to_string()
        } else {
            format!("{whole}.{fraction}")
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChecklistItem {
    pub id: String,
    pub label: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Goal {
    Checkbox,
    Numeric {
        minimum: Option<Decimal>,
        maximum: Option<Decimal>,
        unit: String,
    },
    Checklist {
        items: Vec<ChecklistItem>,
    },
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Carry {
    CloseIncomplete,
    RetainOne,
    Accumulate,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Participation {
    Personal,
    Anyone,
    Everyone,
    Pooled,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub schedule: Schedule,
    pub goal: Goal,
    pub carry: Carry,
    pub participation: Participation,
    /// Calendar days before/after the intended date; the end is exclusive.
    pub open_days_before: u16,
    pub close_days_after: u16,
    #[serde(default)]
    pub allow_streak_exclusions: bool,
}
impl Definition {
    pub fn validate(&self) -> Result<()> {
        self.schedule.validate()?;
        self.goal.validate()?;
        ensure!(
            serde_json::to_vec(self)?.len() <= 8192,
            ErrorCode::InvalidValue
        );
        ensure!(
            self.open_days_before <= 366 && (1..=366).contains(&self.close_days_after),
            ErrorCode::InvalidValue
        );
        ensure!(
            self.participation != Participation::Pooled
                || matches!(self.goal, Goal::Numeric { .. }),
            ErrorCode::InvalidValue
        );
        Ok(())
    }
}
impl Goal {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Checkbox => {}
            Self::Numeric {
                minimum,
                maximum,
                unit,
            } => {
                ensure!(
                    minimum.is_some() || maximum.is_some(),
                    ErrorCode::InvalidValue
                );
                ensure!(
                    !unit.trim().is_empty() && unit.chars().count() <= 40,
                    ErrorCode::InvalidValue
                );
                let min = minimum.as_ref().map(Decimal::units).transpose()?;
                let max = maximum.as_ref().map(Decimal::units).transpose()?;
                ensure!(
                    min.zip(max).is_none_or(|(a, b)| a <= b),
                    ErrorCode::InvalidValue
                );
            }
            Self::Checklist { items } => {
                ensure!(
                    !items.is_empty() && items.len() <= 100,
                    ErrorCode::InvalidValue
                );
                let mut ids = BTreeSet::new();
                for item in items {
                    super::identifier(&item.id)?;
                    ensure!(ids.insert(&item.id), ErrorCode::InvalidValue);
                    super::content(&item.label, "")?;
                }
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Evidence {
    Checkbox {
        complete: bool,
    },
    Quantity {
        amount: Decimal,
    },
    Checklist {
        item_id: String,
        complete: bool,
    },
    /// Explicitly observed a numeric window, including an observed zero.
    Observe,
}
impl Evidence {
    pub fn validate(&self, goal: &Goal) -> Result<()> {
        match (self, goal) {
            (Self::Checkbox { .. }, Goal::Checkbox) => Ok(()),
            (Self::Quantity { amount }, Goal::Numeric { .. }) => {
                amount.units()?;
                Ok(())
            }
            (Self::Observe, Goal::Numeric { .. }) => Ok(()),
            (Self::Checklist { item_id, .. }, Goal::Checklist { items })
                if items.iter().any(|i| &i.id == item_id) =>
            {
                Ok(())
            }
            _ => Err(anyhow!(ErrorCode::InvalidValue)),
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Unknown,
    Incomplete,
    Provisional,
    Complete,
    Missed,
    Excluded,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub outcome: Outcome,
    pub amount: Option<Decimal>,
    pub checked: Vec<String>,
}
/// Evidence must already be authorised, ordered and have explicit corrections
/// applied by the transaction layer. An absent stream is unknown, never zero.
pub fn evaluate(goal: &Goal, entries: &[Evidence], closed: bool) -> Result<Progress> {
    goal.validate()?;
    ensure!(entries.len() <= 10_000, ErrorCode::InvalidValue);
    let mut observed = false;
    let mut quantity = 0_i128;
    let mut checkbox = false;
    let mut checked = BTreeMap::new();
    for entry in entries {
        entry.validate(goal)?;
        observed = true;
        match entry {
            Evidence::Checkbox { complete } => checkbox = *complete,
            Evidence::Quantity { amount } => {
                quantity = quantity
                    .checked_add(amount.units()?)
                    .ok_or_else(|| anyhow!(ErrorCode::InvalidValue))?;
                ensure!(
                    quantity <= 1_000_000_000_000_000_000,
                    ErrorCode::InvalidValue
                );
            }
            Evidence::Checklist { item_id, complete } => {
                checked.insert(item_id.clone(), *complete);
            }
            Evidence::Observe => {}
        }
    }
    let success = match goal {
        Goal::Checkbox => checkbox,
        Goal::Checklist { items } => items
            .iter()
            .all(|item| checked.get(&item.id) == Some(&true)),
        Goal::Numeric {
            minimum, maximum, ..
        } => {
            minimum
                .as_ref()
                .map(Decimal::units)
                .transpose()?
                .is_none_or(|v| quantity >= v)
                && maximum
                    .as_ref()
                    .map(Decimal::units)
                    .transpose()?
                    .is_none_or(|v| quantity <= v)
        }
    };
    let outcome = if !observed {
        if closed
            && matches!(
                goal,
                Goal::Checkbox
                    | Goal::Checklist { .. }
                    | Goal::Numeric {
                        minimum: Some(_),
                        maximum: None,
                        ..
                    }
            )
        {
            Outcome::Missed
        } else {
            Outcome::Unknown
        }
    } else if success
        && !closed
        && matches!(
            goal,
            Goal::Numeric {
                maximum: Some(_),
                ..
            }
        )
    {
        Outcome::Provisional
    } else if success {
        Outcome::Complete
    } else if closed {
        Outcome::Missed
    } else {
        Outcome::Incomplete
    };
    Ok(Progress {
        outcome,
        amount: (observed && matches!(goal, Goal::Numeric { .. }))
            .then(|| Decimal::from_units(quantity)),
        checked: checked
            .into_iter()
            .filter_map(|(id, v)| v.then_some(id))
            .collect(),
    })
}
/// Required-participant aggregates need every stream. Anyone goals can be proven
/// by one visible completion; hidden participation never changes that result.
pub fn aggregate(
    goal: &Goal,
    participation: Participation,
    streams: &[Option<Vec<Evidence>>],
    closed: bool,
) -> Result<Outcome> {
    if participation == Participation::Anyone {
        // A visible completion is sufficient proof of an existential goal. Never
        // let hidden participation alter this answer; without a visible witness
        // it stays unknown, including when an editor records privately offline.
        for entries in streams.iter().flatten() {
            if evaluate(goal, entries, closed)?.outcome == Outcome::Complete {
                return Ok(Outcome::Complete);
            }
        }
        return Ok(Outcome::Unknown);
    }
    if streams.is_empty() || streams.iter().any(Option::is_none) {
        return Ok(Outcome::Unknown);
    }
    let streams = streams
        .iter()
        .map(|s| s.as_deref().unwrap_or_default())
        .collect::<Vec<_>>();
    if participation == Participation::Pooled {
        ensure!(
            matches!(goal, Goal::Numeric { .. }),
            ErrorCode::InvalidValue
        );
        // Empty participant streams still need explicit observation before a
        // pooled result can claim all participants' contributions are known.
        if streams.iter().any(|s| s.is_empty()) {
            return Ok(Outcome::Unknown);
        }
        return Ok(evaluate(
            goal,
            &streams.into_iter().flatten().cloned().collect::<Vec<_>>(),
            closed,
        )?
        .outcome);
    }
    let outcomes = streams
        .iter()
        .map(|s| evaluate(goal, s, closed).map(|p| p.outcome))
        .collect::<Result<Vec<_>>>()?;
    if outcomes.contains(&Outcome::Unknown) {
        return Ok(Outcome::Unknown);
    }
    let success = if participation == Participation::Anyone {
        outcomes.contains(&Outcome::Complete)
    } else {
        outcomes.iter().all(|o| *o == Outcome::Complete)
    };
    Ok(if success {
        Outcome::Complete
    } else if outcomes.contains(&Outcome::Provisional) && !closed {
        Outcome::Provisional
    } else if closed {
        Outcome::Missed
    } else {
        Outcome::Incomplete
    })
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Streak {
    pub current: Option<u32>,
    pub longest: Option<u32>,
}
/// Ordered intended periods, including missed/covered periods. Exclusions only
/// bridge a streak when the snapshotted policy explicitly permits them.
pub fn streak(periods: &[(Outcome, bool)]) -> Streak {
    let (mut current, mut longest) = (0, 0);
    for &(outcome, allow_exclusion) in periods {
        match outcome {
            Outcome::Unknown => {
                return Streak {
                    current: None,
                    longest: None,
                };
            }
            Outcome::Complete => {
                current += 1;
                longest = longest.max(current);
            }
            Outcome::Excluded if allow_exclusion => {}
            Outcome::Provisional | Outcome::Incomplete => {} // still-open current period
            _ => current = 0,
        }
    }
    Streak {
        current: Some(current),
        longest: Some(longest),
    }
}
