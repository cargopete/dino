//! The eval corpus and its harness.
//!
//! Assertions decide pass or fail and are about the outcome. Metrics decide
//! nothing and are counts read back out of the session log. Confusing the two
//! produces a corpus that fails when a model takes seven turns instead of six,
//! which is measuring the weather.

mod case;
mod metrics;
mod runner;

use thiserror::Error;

pub use crate::case::Case;
pub use crate::case::CheckExpectation;
pub use crate::case::Expectations;
pub use crate::case::TextExpectation;
pub use crate::case::fingerprint_tree;
pub use crate::metrics::Metrics;
pub use crate::runner::Outcome;
pub use crate::runner::ToolOptions;
pub use crate::runner::Variant;
pub use crate::runner::build_check;
pub use crate::runner::run_case;
pub use crate::runner::session_tools;
pub use crate::runner::session_tools_with;

#[derive(Debug, Error)]
pub enum EvalError {
    #[error("case is not usable: {0}")]
    Case(String),
    #[error("session log is not usable: {0}")]
    Log(String),
    #[error("cannot set the case up: {0}")]
    Setup(String),
}
