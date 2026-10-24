//! Running one program with one argument vector, inside the sandbox.
//!
//! There is no shell. The argument vector is exactly what is approved, what is
//! recorded and what is executed, with no parsing step in between where those
//! three could come to differ.

mod exec;
mod github;
mod hooks;
mod mcp;
pub mod process;
mod shell;
mod web;

pub use crate::exec::ExecLimits;
pub use crate::exec::ExecTool;
pub use crate::github::GithubLimits;
pub use crate::github::GithubTool;
pub use crate::hooks::AfterEdit;
pub use crate::hooks::EndTurnHooks;
pub use crate::hooks::Hook;
pub use crate::hooks::run_start_hooks;
pub use crate::mcp::McpServer;
pub use crate::mcp::McpTools;
pub use crate::shell::ShellTool;
pub use crate::web::WebFetchTool;
pub use crate::web::WebLimits;
