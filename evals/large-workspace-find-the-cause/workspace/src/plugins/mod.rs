//! Summary renderers, looked up by the name in `report.format`.

pub mod csv;
pub mod json;
pub mod markdown;
pub mod plain;
pub mod registry;

pub use registry::PluginRegistry;

use crate::config::Config;
use crate::fmt::duration::DurationStyle;
use crate::fmt::money::{format_money, Currency};
use crate::report::summary::Summary;

pub trait ReportPlugin {
    fn name(&self) -> &'static str;
    fn render(&self, summary: &Summary, context: &RenderContext) -> String;
}

/// The formatting choices a plugin should honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderContext {
    pub style: DurationStyle,
    pub currency: Currency,
}

impl RenderContext {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            style: config.report.duration_style,
            currency: config.billing.currency,
        }
    }

    #[must_use]
    pub fn duration(&self, minutes: u64) -> String {
        self.style.render(minutes)
    }

    #[must_use]
    pub fn money(&self, cents: u64) -> String {
        format_money(cents, self.currency)
    }
}
