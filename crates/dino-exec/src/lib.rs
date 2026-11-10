//! Running one program with one argument vector, inside the sandbox.
//!
//! There is no shell. The argument vector is exactly what is approved, what is
//! recorded and what is executed, with no parsing step in between where those
//! three could come to differ.

mod background;
mod code;
mod escalation;
mod exec;
mod github;
mod hooks;
mod lsp;
mod mcp;
mod packages;
pub mod process;
mod reads;
pub mod rust;
mod search;
mod shell;
mod tracked;
mod verify;
mod web;

pub use crate::background::BackgroundTool;
pub use crate::code::CodeLimits;
pub use crate::code::CodeTool;
pub use crate::exec::ExecLimits;
pub use crate::exec::ExecTool;
pub use crate::github::GithubLimits;
pub use crate::github::GithubTool;
pub use crate::hooks::AfterCall;
pub use crate::hooks::AfterEdit;
pub use crate::hooks::BeforeCall;
pub use crate::hooks::EndTurnHooks;
pub use crate::hooks::Hook;
pub use crate::hooks::PromptHooks;
pub use crate::hooks::Submitted;
pub use crate::hooks::TaskEnd;
pub use crate::hooks::run_awaiting_hooks;
pub use crate::hooks::run_start_hooks;
pub use crate::hooks::run_task_end_hooks;
pub use crate::hooks::with_context;
pub use crate::mcp::McpServer;
pub use crate::mcp::McpTools;
pub use crate::packages::PackagesTool;
pub use crate::search::SearchBackend;
pub use crate::search::SearchEngine;
pub use crate::search::WEB_SEARCH;
pub use crate::search::WebSearchTool;
pub use crate::shell::ShellTool;
pub use crate::verify::BeforeCommit;
pub use crate::verify::Verify;
pub use crate::web::WebFetchTool;
pub use crate::web::WebLimits;
