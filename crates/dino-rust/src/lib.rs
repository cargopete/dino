//! The structured Cargo tool.
//!
//! Dino treats the compiler as evidence, so this crate runs a fixed set of
//! Cargo subcommands and returns parsed diagnostics rather than compiler noise.
//! It is not a shell: there is no free-form argument surface, and there is no
//! sandbox either, which RFC-0008 says out loud rather than implying otherwise.

mod cargo;
mod diagnostics;
mod edit_check;
mod finish;
mod gate;
pub mod outline;
mod trim;

pub use crate::cargo::CargoLimits;
pub use crate::cargo::CargoTool;
pub use crate::diagnostics::Diagnostic;
pub use crate::diagnostics::DiagnosticCounts;
pub use crate::edit_check::EditCheck;
pub use crate::finish::BuildCheck;
pub use crate::gate::GateTool;
pub use crate::gate::Verdict;
