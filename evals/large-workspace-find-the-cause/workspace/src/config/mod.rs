//! The config file: an INI-style text with `[sections]` and `key = value`.

pub mod ini;
pub mod schema;
pub mod validate;
pub mod value;

pub use schema::{BillingConfig, Config, FilterConfig, ReportConfig, SortKey};
