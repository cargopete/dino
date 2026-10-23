//! Running one program with one argument vector, inside the sandbox.
//!
//! There is no shell. The argument vector is exactly what is approved, what is
//! recorded and what is executed, with no parsing step in between where those
//! three could come to differ.

mod exec;
mod github;
pub mod process;

pub use crate::exec::ExecLimits;
pub use crate::exec::ExecTool;
pub use crate::github::GithubLimits;
pub use crate::github::GithubTool;
