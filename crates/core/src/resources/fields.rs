use crate::{
    error::ErrorCode,
    tasks::{ChecklistItem, Decimal, Goal},
};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FieldValue {
    Text {
        text: String,
    },
    Quantity {
        amount: Decimal,
        unit: String,
    },
    Date {
        year: Option<i32>,
        month: u32,
        day: u32,
    },
    Url {
        url: String,
    },
    Boolean {
        value: bool,
    },
    Choices {
        options: Vec<String>,
        selected: Vec<String>,
    },
    Checklist {
        items: Vec<ChecklistItem>,
        checked: Vec<String>,
    },
}
impl FieldValue {
    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Self::Text { text } => ensure!(text.chars().count() <= 4096, ErrorCode::InvalidValue),
            Self::Quantity { amount, unit } => {
                amount.units()?;
                ensure!(
                    !unit.trim().is_empty() && unit.chars().count() <= 40,
                    ErrorCode::InvalidValue
                );
            }
            Self::Date { year, month, day } => {
                ensure!(
                    year.is_none_or(|y| (1900..=9999).contains(&y)),
                    ErrorCode::InvalidValue
                );
                ensure!(
                    chrono::NaiveDate::from_ymd_opt(year.unwrap_or(2000), *month, *day).is_some(),
                    ErrorCode::InvalidValue
                );
            }
            Self::Url { url } => {
                let url = url::Url::parse(url).map_err(|_| anyhow!(ErrorCode::InvalidValue))?;
                ensure!(
                    matches!(url.scheme(), "http" | "https")
                        && url.host_str().is_some()
                        && url.username().is_empty()
                        && url.password().is_none(),
                    ErrorCode::InvalidValue
                );
            }
            Self::Boolean { .. } => {}
            Self::Choices { options, selected } => {
                ensure!(
                    !options.is_empty() && options.len() <= 100 && selected.len() <= 100,
                    ErrorCode::InvalidValue
                );
                ensure!(
                    options.iter().collect::<BTreeSet<_>>().len() == options.len()
                        && selected.iter().collect::<BTreeSet<_>>().len() == selected.len(),
                    ErrorCode::InvalidValue
                );
                for v in options {
                    ensure!(
                        !v.trim().is_empty() && v.chars().count() <= 100,
                        ErrorCode::InvalidValue
                    );
                }
                ensure!(
                    selected.iter().all(|v| options.contains(v)),
                    ErrorCode::InvalidValue
                );
            }
            Self::Checklist { items, checked } => {
                Goal::Checklist {
                    items: items.clone(),
                }
                .validate()?;
                ensure!(
                    checked.iter().collect::<BTreeSet<_>>().len() == checked.len()
                        && checked.iter().all(|id| items.iter().any(|i| &i.id == id)),
                    ErrorCode::InvalidValue
                );
            }
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= 8192,
            ErrorCode::InvalidValue
        );
        Ok(())
    }
}
