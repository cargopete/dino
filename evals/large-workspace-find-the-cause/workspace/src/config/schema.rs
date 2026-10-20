use std::collections::BTreeMap;

use crate::config::{ini, validate, value};
use crate::error::Error;
use crate::fmt::duration::DurationStyle;
use crate::fmt::money::Currency;
use crate::model::period::Period;
use crate::model::rate::Rate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportConfig {
    pub format: String,
    pub duration_style: DurationStyle,
    /// Project totals are rounded to the nearest multiple of this many
    /// minutes. Zero leaves them alone.
    pub round_to: u64,
    pub sort: SortKey,
    pub show_total: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingConfig {
    pub currency: Currency,
    pub default_rate: Rate,
    pub rates: BTreeMap<String, Rate>,
    pub billable_by_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilterConfig {
    pub period: Option<Period>,
    pub projects: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub report: ReportConfig,
    pub billing: BillingConfig,
    pub filter: FilterConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            report: ReportConfig {
                format: "plain".to_owned(),
                duration_style: DurationStyle::Compact,
                round_to: 0,
                sort: SortKey::Name,
                show_total: true,
            },
            billing: BillingConfig {
                currency: Currency::Eur,
                default_rate: Rate::per_hour(0),
                rates: BTreeMap::new(),
                billable_by_default: false,
            },
            filter: FilterConfig::default(),
        }
    }
}

impl Config {
    pub fn from_text(text: &str) -> Result<Self, Error> {
        let mut config = Config::default();
        for setting in ini::parse(text)? {
            config.apply(&setting)?;
        }
        validate::check(&config)?;
        Ok(config)
    }

    /// The hourly rate for a project, falling back to the default rate.
    #[must_use]
    pub fn rate_for(&self, project: &str) -> Rate {
        self.billing
            .rates
            .get(project)
            .copied()
            .unwrap_or(self.billing.default_rate)
    }

    fn apply(&mut self, setting: &ini::Setting) -> Result<(), Error> {
        let key = format!("{}.{}", setting.section, setting.key);
        let raw = setting.value.as_str();
        match (setting.section.as_str(), setting.key.as_str()) {
            ("report", "format") => self.report.format = raw.to_ascii_lowercase(),
            ("report", "duration_style") => {
                self.report.duration_style =
                    DurationStyle::from_name(raw).ok_or_else(|| Error::Value {
                        key: key.clone(),
                        message: format!("unknown duration style `{raw}`"),
                    })?;
            }
            ("report", "round_to") => self.report.round_to = value::whole(&key, raw)?,
            ("report", "sort") => {
                self.report.sort = match raw {
                    "name" => SortKey::Name,
                    "duration" => SortKey::Duration,
                    other => {
                        return Err(Error::Value {
                            key,
                            message: format!("cannot sort by `{other}`"),
                        })
                    }
                }
            }
            ("report", "show_total") => self.report.show_total = value::boolean(&key, raw)?,
            ("billing", "currency") => {
                self.billing.currency = Currency::from_code(raw).ok_or_else(|| Error::Value {
                    key: key.clone(),
                    message: format!("unknown currency `{raw}`"),
                })?;
            }
            ("billing", "default_rate") => {
                self.billing.default_rate = Rate::per_hour(value::hundredths(&key, raw)?);
            }
            ("billing", "billable_by_default") => {
                self.billing.billable_by_default = value::boolean(&key, raw)?;
            }
            ("billing", other) if other.starts_with("rate.") => {
                let project = other.trim_start_matches("rate.").to_owned();
                let rate = Rate::per_hour(value::hundredths(&key, raw)?);
                self.billing.rates.insert(project, rate);
            }
            ("filter", "week") => {
                self.filter.period = Some(Period::iso_week(raw).ok_or_else(|| Error::Value {
                    key: key.clone(),
                    message: format!("expected a week like 2026-W10, found `{raw}`"),
                })?);
            }
            ("filter", "projects") => self.filter.projects = value::list(raw),
            _ => {
                return Err(Error::Config {
                    line: setting.line,
                    message: format!("unknown key `{key}`"),
                })
            }
        }
        Ok(())
    }
}
