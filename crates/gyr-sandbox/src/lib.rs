//! Operating-system containment for the processes Gyrfalcon starts.
//!
//! The crate rewrites an argument vector and never spawns anything, so it needs
//! no async runtime and the process layer keeps sole responsibility for
//! spawning, capping and killing.
//!
//! What this contains is writes outside the workspace, and the network. What it
//! does not contain is reads. A sandboxed build script may learn a secret; it
//! cannot write it down outside the workspace or transmit it. That is the whole
//! guarantee, and RFC-0009 section 2 explains why a narrower read profile was
//! rejected rather than attempted badly.

mod landlock;
mod seatbelt;

use std::fmt::Debug;
use std::path::Path;

use thiserror::Error;

pub use crate::landlock::Landlock;
pub use crate::seatbelt::Seatbelt;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("no sandbox is available on this platform: {0}")]
    Unavailable(String),
    #[error("cannot build a sandbox profile: {0}")]
    Profile(String),
}

/// A command rewritten so its child runs contained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedCommand {
    pub program: String,
    pub arguments: Vec<String>,
}

pub trait Sandbox: Send + Sync + Debug {
    /// A short name for prompts, logs and the system prompt.
    ///
    /// Owned rather than borrowed so an implementation can describe what it
    /// actually confines rather than a fixed phrase.
    fn label(&self) -> String;

    fn confines_writes(&self) -> bool;

    fn denies_network(&self) -> bool;

    /// Whether writes beneath the protected paths are refused to processes, and
    /// not only to the file tools.
    fn protects_paths(&self) -> bool {
        false
    }

    /// A temporary directory the child may write to, inside the confined set.
    ///
    /// Without one, anything that uses the system temporary directory fails
    /// under confinement, and widening the writable set to reach it would hand
    /// every child a staging area outside the workspace.
    fn temp_dir(&self) -> Option<&Path>;

    /// Rewrites a command so the child runs contained.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] when a profile cannot be built for this
    /// workspace.
    fn wrap(&self, program: &str, arguments: &[String]) -> Result<WrappedCommand, SandboxError>;
}

/// No containment at all.
///
/// Never a default. It exists so a person on a platform without a sandbox can
/// say so deliberately, and so the log can record that they did.
#[derive(Debug, Default, Clone, Copy)]
pub struct Unconfined;

impl Sandbox for Unconfined {
    fn label(&self) -> String {
        "unconfined".to_owned()
    }

    fn confines_writes(&self) -> bool {
        false
    }

    fn denies_network(&self) -> bool {
        false
    }

    fn temp_dir(&self) -> Option<&Path> {
        None
    }

    fn wrap(&self, program: &str, arguments: &[String]) -> Result<WrappedCommand, SandboxError> {
        Ok(WrappedCommand {
            program: program.to_owned(),
            arguments: arguments.to_vec(),
        })
    }
}

/// Builds the platform's sandbox for one workspace root.
///
/// # Errors
///
/// Returns [`SandboxError::Unavailable`] where no implementation exists. It
/// does not quietly fall back to [`Unconfined`]; a caller that wants no
/// containment has to ask for it by name.
pub fn detect(workspace: &Path) -> Result<Box<dyn Sandbox>, SandboxError> {
    detect_protecting(workspace, &[])
}

/// As [`detect`], and also denies writes beneath these workspace-relative
/// paths where the platform can.
///
/// Landlock cannot: it only grants, and a grant on the workspace covers
/// everything beneath it. On Linux the paths are protected by the file tools
/// alone, and a process can still write them.
///
/// # Errors
///
/// As [`detect`].
pub fn detect_protecting(
    workspace: &Path,
    protected: &[std::path::PathBuf],
) -> Result<Box<dyn Sandbox>, SandboxError> {
    if cfg!(target_os = "macos") {
        return Ok(Box::new(Seatbelt::protecting(workspace, protected)?));
    }
    if cfg!(target_os = "linux") {
        return Ok(Box::new(Landlock::new(workspace)?));
    }
    Err(SandboxError::Unavailable(format!(
        "{} has no Gyrfalcon sandbox yet; see RFC-0009 section 5",
        std::env::consts::OS
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[test]
    fn detection_reports_unavailability_rather_than_falling_back() {
        let error = detect(Path::new(".")).unwrap_err();

        assert!(
            matches!(error, SandboxError::Unavailable(_)),
            "a platform without a sandbox must say so: {error}"
        );
        assert!(error.to_string().contains(std::env::consts::OS));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn detection_yields_a_confining_sandbox_on_macos() {
        let sandbox = detect(&std::env::temp_dir()).unwrap();

        assert!(sandbox.confines_writes());
        assert!(sandbox.denies_network());
        assert!(sandbox.temp_dir().is_some());
    }

    #[test]
    fn unconfined_returns_the_command_untouched_and_admits_it() {
        let sandbox = Unconfined;
        let arguments = vec!["check".to_owned(), "--workspace".to_owned()];

        let wrapped = sandbox.wrap("cargo", &arguments).unwrap();

        assert_eq!(wrapped.program, "cargo");
        assert_eq!(wrapped.arguments, arguments);
        assert!(!sandbox.confines_writes());
        assert!(!sandbox.denies_network());
        assert_eq!(sandbox.temp_dir(), None);
        assert_eq!(sandbox.label(), "unconfined");
    }
}
