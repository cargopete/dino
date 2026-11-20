//! Linux containment through Landlock, by way of the `dino-confine` helper.
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

const HELPER: &str = "dino-confine";

/// Overrides where the helper is found, for tests and for packaging.
const HELPER_VARIABLE: &str = "DINO_CONFINE";

#[derive(Debug, Clone)]
pub struct Landlock {
    helper: PathBuf,
    writable: PathBuf,
    temp_dir: PathBuf,
    protected: Vec<PathBuf>,
    /// How many of `protected` the user chose, ahead of the executable config.
    chosen: usize,
    /// Extra writable paths and outbound HTTPS, for fetching dependencies.
    fetch_writable: Vec<PathBuf>,
    /// A linked worktree's git directories, and a target directory outside it.
    outside_writable: Vec<PathBuf>,
    target_dir: Option<PathBuf>,
    /// Credential paths that exist under the home directory, denied to reads.
    deny_read: Vec<PathBuf>,
    fetching: bool,
    /// Whether this machine lets a process have a network namespace of its
    /// own, so processes get loopback rather than no TCP at all.
    loopback: bool,
    /// Reached from inside the namespace through a relay to its Unix socket.
    proxy: Option<std::sync::Arc<crate::Proxy>>,
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
        Self::building_in(workspace, protected, None)
    }

    /// As [`Landlock::protecting`], with Cargo building in `target_dir`, an
    /// existing canonical directory that processes may then write.
    ///
    /// A linked worktree's `config.worktree` is writable here, where Seatbelt
    /// denies it: Landlock cannot carve it out, and it is not restored.
    ///
    /// # Errors
    ///
    /// As [`Landlock::protecting`].
    pub fn building_in(
        workspace: &Path,
        protected: &[PathBuf],
        target_dir: Option<PathBuf>,
    ) -> Result<Self, SandboxError> {
        let chosen = protected.len();
        let protected = restore::checked(&crate::with_executable_config(protected))?;
        let helper = find_helper()?;
        let writable = std::fs::canonicalize(workspace).map_err(|error| {
            SandboxError::Profile(format!(
                "cannot resolve workspace {}: {error}",
                workspace.display()
            ))
        })?;
        let temp_dir = writable.join(".dino").join("tmp");
        std::fs::create_dir_all(&temp_dir).map_err(|error| {
            SandboxError::Profile(format!(
                "cannot create sandbox temporary directory {}: {error}",
                temp_dir.display()
            ))
        })?;
        let loopback = loopback_available(&helper);
        let mut outside_writable = crate::LinkedWorktree::find(&writable)
            .map(|linked| linked.writable())
            .unwrap_or_default();
        outside_writable.extend(target_dir.clone());
        Ok(Self {
            helper,
            writable,
            temp_dir,
            protected,
            chosen,
            fetch_writable: Vec::new(),
            outside_writable,
            target_dir,
            deny_read: credential_paths(),
            fetching: false,
            loopback,
            proxy: None,
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
        let chosen = match self.chosen {
            0 => String::new(),
            count => format!(", {count} path(s)"),
        };
        // Landlock grants by allow-list, so it cannot carve credentials out of a
        // readable home. Said here rather than implied by Seatbelt's label.
        let network = if self.fetching {
            "fetching: TCP 443 out and writes to Cargo's cache".to_owned()
        } else if let Some(proxy) = &self.proxy {
            crate::proxy::describe(proxy)
        } else if self.loopback {
            "network: loopback only, in a namespace of its own".to_owned()
        } else {
            "TCP denied, loopback included: user namespaces are unavailable".to_owned()
        };
        let credentials = if self.deny_read.is_empty() {
            "no credential paths found to deny"
        } else {
            "credential reads denied"
        };
        format!(
            "workspace (landlock: writes confined, executable config{chosen} restored after \
             each process, {network}, {credentials})"
        )
    }

    fn confines_writes(&self) -> bool {
        true
    }

    /// Only with a namespace of its own: without one, Landlock could allow the
    /// proxy's port only on every address, which is not a list of hosts.
    fn through_proxy(
        &self,
        proxy: &std::sync::Arc<crate::Proxy>,
    ) -> Result<Box<dyn Sandbox>, SandboxError> {
        if !self.loopback {
            return Err(SandboxError::Unavailable(
                "--allow-net needs unprivileged user namespaces on Linux, which this machine \
                 does not allow; without one a process could reach any host on the proxy's port"
                    .to_owned(),
            ));
        }
        if proxy.socket().is_none() {
            return Err(SandboxError::Profile(
                "the --allow-net proxy has no Unix socket to relay to".to_owned(),
            ));
        }
        let mut proxied = self.clone();
        proxied.proxy = Some(std::sync::Arc::clone(proxy));
        Ok(Box::new(proxied))
    }

    fn proxy_port(&self) -> Option<u16> {
        self.proxy.as_ref().map(|proxy| proxy.port())
    }

    fn for_fetching(&self, writable: &[PathBuf]) -> Option<Box<dyn Sandbox>> {
        let mut fetching = self.clone();
        fetching.fetch_writable = writable.to_vec();
        fetching.fetching = true;
        Some(Box::new(fetching))
    }

    /// True for the purpose the caller uses it for: Cargo must run `--offline`,
    /// because it fetches over TCP and will fail without it.
    fn denies_network(&self) -> bool {
        !self.fetching
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

    fn target_dir(&self) -> Option<&Path> {
        self.target_dir.as_deref()
    }

    fn wrap(&self, program: &str, arguments: &[String]) -> Result<WrappedCommand, SandboxError> {
        let writable = self
            .writable
            .to_str()
            .ok_or_else(|| SandboxError::Profile("workspace path is not valid UTF-8".to_owned()))?;
        let mut wrapped = vec!["--allow-write".to_owned(), writable.to_owned()];
        for path in self.outside_writable.iter().chain(&self.fetch_writable) {
            let path = path.to_str().ok_or_else(|| {
                SandboxError::Profile("writable path is not valid UTF-8".to_owned())
            })?;
            wrapped.extend(["--allow-write".to_owned(), path.to_owned()]);
        }
        if self.fetching {
            wrapped.extend(["--allow-connect".to_owned(), "443".to_owned()]);
        } else if self.loopback {
            wrapped.push("--loopback".to_owned());
            if let Some(proxy) = &self.proxy
                && let Some(socket) = proxy.socket().and_then(Path::to_str)
            {
                wrapped.extend([
                    "--proxy".to_owned(),
                    socket.to_owned(),
                    "--proxy-port".to_owned(),
                    proxy.port().to_string(),
                ]);
            }
        }
        for path in &self.deny_read {
            let path = path.to_str().ok_or_else(|| {
                SandboxError::Profile("credential path is not valid UTF-8".to_owned())
            })?;
            wrapped.extend(["--deny-read".to_owned(), path.to_owned()]);
        }
        wrapped.extend(["--".to_owned(), program.to_owned()]);
        wrapped.extend_from_slice(arguments);
        Ok(WrappedCommand {
            program: self.helper.display().to_string(),
            arguments: wrapped,
        })
    }
}

/// Whether the helper can give a process a loopback-only network namespace
/// here. Asked once, by trying it, because kernels and distributions differ on
/// unprivileged user namespaces and only the attempt is sure.
fn loopback_available(helper: &Path) -> bool {
    std::process::Command::new(helper)
        .arg("--check-loopback")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The credential paths that exist under `$HOME`. Only existing ones, because
/// the helper grants around each, and a missing one leaves nothing to avoid.
fn credential_paths() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    crate::CREDENTIAL_PATHS
        .iter()
        .map(|path| home.join(path))
        .filter(|path| path.exists())
        .collect()
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
        let error = find_helper_with(Some(Path::new("/nowhere/dino-confine"))).unwrap_err();

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
