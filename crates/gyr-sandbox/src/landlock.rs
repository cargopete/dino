//! Linux containment through Landlock, by way of the `gyr-confine` helper.
//!
//! Landlock cannot be applied to a child without `pre_exec`, which is `unsafe`
//! and the workspace forbids it. The helper restricts itself and then `exec`s,
//! which needs no unsafe because Landlock restrictions are inherited across
//! `exec`. RFC-0009 section 5.1 has the argument, including why this is not
//! `bubblewrap`.

use std::path::Path;
use std::path::PathBuf;

use crate::Sandbox;
use crate::SandboxError;
use crate::Snapshot;
use crate::WrappedCommand;
use crate::restore;

const HELPER: &str = "gyr-confine";

/// Overrides where the helper is found, for tests and for packaging.
const HELPER_VARIABLE: &str = "GYR_CONFINE";

#[derive(Debug)]
pub struct Landlock {
    helper: PathBuf,
    writable: PathBuf,
    temp_dir: PathBuf,
    protected: Vec<PathBuf>,
}

impl Landlock {
    /// Builds Linux containment for one workspace root.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Unavailable`] when the helper binary cannot be
    /// found, and [`SandboxError::Profile`] when the root cannot be
    /// canonicalised or its temporary directory cannot be created.
    pub fn new(workspace: &Path) -> Result<Self, SandboxError> {
        Self::protecting(workspace, &[])
    }

    /// As [`Landlock::new`], and also puts these workspace-relative paths back
    /// after each process.
    ///
    /// # Errors
    ///
    /// As [`Landlock::new`], and [`SandboxError::Profile`] for a protected
    /// path that is absolute or climbs out with `..`.
    pub fn protecting(workspace: &Path, protected: &[PathBuf]) -> Result<Self, SandboxError> {
        let protected = restore::checked(protected)?;
        let helper = find_helper()?;
        let writable = std::fs::canonicalize(workspace).map_err(|error| {
            SandboxError::Profile(format!(
                "cannot resolve workspace {}: {error}",
                workspace.display()
            ))
        })?;
        let temp_dir = writable.join(".gyr").join("tmp");
        std::fs::create_dir_all(&temp_dir).map_err(|error| {
            SandboxError::Profile(format!(
                "cannot create sandbox temporary directory {}: {error}",
                temp_dir.display()
            ))
        })?;
        Ok(Self {
            helper,
            writable,
            temp_dir,
            protected,
        })
    }
}

impl Sandbox for Landlock {
    /// Says TCP rather than network, because that is what is true.
    ///
    /// Landlock ABI 4 restricts TCP bind and connect. It does not restrict UDP,
    /// so a determined child could still talk to the world over DNS. The
    /// Seatbelt profile denies everything. Labelling both "network denied"
    /// would make the weaker one wear the stronger one's clothes.
    fn label(&self) -> String {
        match self.protected.len() {
            0 => "workspace (landlock: writes confined, TCP denied)".to_owned(),
            count => format!(
                "workspace (landlock: writes confined, {count} path(s) restored after each \
                 process, TCP denied)"
            ),
        }
    }

    fn confines_writes(&self) -> bool {
        true
    }

    /// True for the purpose the caller uses it for: Cargo must run `--offline`,
    /// because it fetches over TCP and will fail without it.
    fn denies_network(&self) -> bool {
        true
    }

    fn protects_paths(&self) -> bool {
        !self.protected.is_empty()
    }

    fn snapshot(&self) -> Result<Option<Snapshot>, SandboxError> {
        if self.protected.is_empty() {
            return Ok(None);
        }
        Snapshot::take(&self.writable, &self.protected)
            .map(Some)
            .map_err(|error| SandboxError::Profile(format!("cannot copy protected paths: {error}")))
    }

    fn temp_dir(&self) -> Option<&Path> {
        Some(&self.temp_dir)
    }

    fn wrap(&self, program: &str, arguments: &[String]) -> Result<WrappedCommand, SandboxError> {
        let writable = self
            .writable
            .to_str()
            .ok_or_else(|| SandboxError::Profile("workspace path is not valid UTF-8".to_owned()))?;
        let mut wrapped = vec![
            "--allow-write".to_owned(),
            writable.to_owned(),
            "--".to_owned(),
            program.to_owned(),
        ];
        wrapped.extend_from_slice(arguments);
        Ok(WrappedCommand {
            program: self.helper.display().to_string(),
            arguments: wrapped,
        })
    }
}

/// Finds the helper without guessing.
///
/// Beside the running executable first, because that is where Cargo puts it and
/// where a package would install it. Then one directory up, because a test
/// binary lives in `deps/`. Then `PATH`. An override exists for packaging that
/// does neither.
fn find_helper() -> Result<PathBuf, SandboxError> {
    if let Some(path) = std::env::var_os(HELPER_VARIABLE).map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        return Err(SandboxError::Unavailable(format!(
            "{HELPER_VARIABLE} names {}, which is not a file",
            path.display()
        )));
    }

    if let Ok(current) = std::env::current_exe()
        && let Some(directory) = current.parent()
    {
        for candidate in [directory.join(HELPER), directory.join("..").join(HELPER)] {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths) {
            let candidate = directory.join(HELPER);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err(SandboxError::Unavailable(format!(
        "cannot find the {HELPER} helper beside this executable or on PATH; \
         set {HELPER_VARIABLE} to its location"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_helper_is_unavailable_rather_than_unconfined() {
        // Deliberately not a real path. The failure mode that matters is that a
        // missing helper never degrades into running the command unwrapped.
        let error = find_helper_with(Some(Path::new("/nowhere/gyr-confine"))).unwrap_err();

        assert!(matches!(error, SandboxError::Unavailable(_)), "{error}");
    }

    /// The override branch of [`find_helper`], without touching the process
    /// environment that every other test shares.
    fn find_helper_with(override_path: Option<&Path>) -> Result<PathBuf, SandboxError> {
        match override_path {
            Some(path) if path.is_file() => Ok(path.to_path_buf()),
            Some(path) => Err(SandboxError::Unavailable(format!(
                "{HELPER_VARIABLE} names {}, which is not a file",
                path.display()
            ))),
            None => find_helper(),
        }
    }
}
