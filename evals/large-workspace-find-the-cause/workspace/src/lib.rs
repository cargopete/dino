//! Worklog: reads a plain-text time log and reports on it.

pub mod app;
pub mod config;
pub mod error;
pub mod fmt;
pub mod legacy;
pub mod model;
pub mod parse;
pub mod plugins;
pub mod report;
pub mod util;

pub use app::App;
pub use error::Error;
