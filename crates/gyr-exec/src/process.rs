//! Running one child process under a wall clock, with capped output.
//!
//! Nothing here is Rust-specific. It spawns, drains, caps, times out and kills,
//! and it asks a sandbox to rewrite the command before any of that.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use gyr_core::ToolError;
use gyr_sandbox::Sandbox;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// A finished, or abandoned, child process.
#[derive(Debug)]
pub struct Execution {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
    /// Protected paths the process changed, which have been put back.
    pub restored: Vec<String>,
}

/// The only environment variables a child is given.
///
/// The inherited environment is cleared first, so a build script cannot read
/// the agent's provider credentials out of it. The child still runs arbitrary
/// code; this narrows what that code can see, not what it can do.
fn child_environment(sandbox: &dyn Sandbox) -> BTreeMap<&'static str, String> {
    let mut environment = BTreeMap::new();
    for name in ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME"] {
        if let Ok(value) = std::env::var(name) {
            environment.insert(name, value);
        }
    }
    environment.insert("CARGO_TERM_COLOR", "never".to_owned());
    environment.insert("TERM", "dumb".to_owned());
    // Anything reaching for the system temporary directory would be denied by a
    // confining sandbox, and widening the writable set to reach it would hand
    // every child a staging area outside the workspace. It gets one inside.
    if let Some(temp_dir) = sandbox.temp_dir() {
        environment.insert("TMPDIR", temp_dir.display().to_string());
    }
    // With the network denied, package managers otherwise retry DNS for minutes
    // and then fail in words that read as an outage rather than as policy.
    if sandbox.denies_network() {
        for (name, value) in OFFLINE {
            environment.insert(name, (*value).to_owned());
        }
    }
    environment
}

/// Each package manager's own switch for working from its cache alone.
const OFFLINE: &[(&str, &str)] = &[
    ("CARGO_NET_OFFLINE", "true"),
    ("npm_config_offline", "true"),
    ("YARN_ENABLE_OFFLINE_MODE", "1"),
    ("GOPROXY", "off"),
    ("GOFLAGS", "-mod=mod"),
];

/// What a model is told when a command failed because it tried the network.
pub const NETWORK_DENIED: &str = "The network is denied in this sandbox. This is policy, not \
     an outage: nothing run here can download, and mirrors, caches elsewhere on disk and \
     registry settings will not get round it. Rust dependencies: declare them in Cargo.toml \
     and use the cargo tool's `fetch`, or its `add`, which may reach crates.io. Other package \
     managers cannot fetch here; use what is already installed, or say so with report_blocker.";

/// The note for a failed command whose output shows it reaching for the
/// network, when the sandbox is what stopped it.
#[must_use]
pub fn network_note(sandbox: &dyn Sandbox, execution: &Execution) -> Option<&'static str> {
    const SIGNS: &[&str] = &[
        "could not resolve host",
        "couldn't resolve host",
        "enotfound",
        "enotcached",
        "getaddrinfo",
        "name resolution",
        "nodename nor servname",
        "--offline was specified",
        "offline mode",
        "goproxy=off",
        "module lookup disabled",
        "network is unreachable",
        "spurious network error",
    ];
    if !sandbox.denies_network() || execution.exit_code == Some(0) {
        return None;
    }
    let output = format!("{}\n{}", execution.stdout, execution.stderr).to_lowercase();
    SIGNS
        .iter()
        .any(|sign| output.contains(sign))
        .then_some(NETWORK_DENIED)
}

/// Runs a program to completion, or kills it when the clock or the caller says
/// so.
///
/// Dropping the returned future kills the child, because the command is spawned
/// with `kill_on_drop`. That is what makes a cancelled agent run leave no
/// process behind.
///
/// # Errors
///
/// Returns [`ToolError`] when the sandbox cannot wrap the command, when the
/// child cannot be spawned, or when its output cannot be read. Exceeding the
/// wall clock is not an error: it returns an [`Execution`] with `timed_out`
/// set, because a killed process is a result rather than a fault.
pub async fn run(
    program: &str,
    arguments: &[String],
    directory: &Path,
    sandbox: &dyn Sandbox,
    limit: usize,
    timeout: Duration,
) -> Result<Execution, ToolError> {
    let wrapped = sandbox
        .wrap(program, arguments)
        .map_err(|error| ToolError::new(error.to_string()))?;
    // The sandbox creates this when it is built, but the workspace can be
    // replaced or cleaned after that, and a TMPDIR that does not exist fails
    // the child in ways that look nothing like a missing directory.
    if let Some(temp_dir) = sandbox.temp_dir() {
        std::fs::create_dir_all(temp_dir).map_err(|error| {
            ToolError::new(format!(
                "cannot create the temporary directory {}: {error}",
                temp_dir.display()
            ))
        })?;
    }
    let mut snapshot = sandbox
        .snapshot()
        .map_err(|error| ToolError::new(error.to_string()))?;
    let mut command = Command::new(&wrapped.program);
    command
        .args(&wrapped.arguments)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_clear();
    // Its own group, so whatever it starts in the background can be ended with
    // it rather than outliving the call.
    #[cfg(unix)]
    command.process_group(0);
    for (name, value) in child_environment(sandbox) {
        command.env(name, value);
    }

    let mut child = spawn(&mut command)
        .await
        .map_err(|error| ToolError::new(format!("cannot run {}: {error}", wrapped.program)))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::new("child process had no stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::new("child process had no stderr"))?;

    let mut group = Group(child.id());
    // Both pipes are drained concurrently. Reading one to completion first
    // would deadlock as soon as the other filled its buffer.
    let out = tokio::spawn(read_capped(stdout, limit));
    let err = tokio::spawn(read_capped(stderr, limit));
    let collected = tokio::time::timeout(timeout, async {
        let status = child.wait().await?;
        // A background process still holds the pipes open; ending the group
        // closes them, so the call returns when its command does.
        group.kill();
        let joined = |result: Result<std::io::Result<(String, bool)>, tokio::task::JoinError>| {
            result.map_err(std::io::Error::other).and_then(|read| read)
        };
        Ok::<_, std::io::Error>((joined(out.await)?, joined(err.await)?, status))
    })
    .await;

    if collected.is_err() {
        // Killed before restoring, or it could write again afterwards.
        group.kill();
        let _ = child.kill().await;
    }
    let restored = match snapshot.as_mut() {
        Some(snapshot) => snapshot.restore().map_err(|error| {
            ToolError::new(format!("cannot restore the protected paths: {error}"))
        })?,
        None => Vec::new(),
    };

    match collected {
        Ok(Ok(((stdout, stdout_truncated), (stderr, stderr_truncated), status))) => Ok(Execution {
            exit_code: status.code(),
            stdout,
            stderr,
            truncated: stdout_truncated || stderr_truncated,
            timed_out: false,
            restored,
        }),
        Ok(Err(error)) => Err(ToolError::new(format!(
            "cannot read output from {program}: {error}"
        ))),
        Err(_elapsed) => Ok(Execution {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            timed_out: true,
            restored,
        }),
    }
}

/// A child's process group, killed when the call is over or abandoned.
struct Group(Option<u32>);

impl Group {
    fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(id) = self.0.take().and_then(|id| i32::try_from(id).ok()) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(id),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Spawns, retrying briefly on `ETXTBSY`.
///
/// On Linux a script that was just written can be refused as busy: a fork on
/// another thread inherits the writer's descriptor for the instant before its
/// own exec. The file is fine and a moment later it runs.
async fn spawn(command: &mut Command) -> std::io::Result<tokio::process::Child> {
    const TEXT_FILE_BUSY: i32 = 26;
    let mut delay = Duration::from_millis(10);
    for _ in 0..5 {
        match command.spawn() {
            Err(error) if error.raw_os_error() == Some(TEXT_FILE_BUSY) => {
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
            result => return result,
        }
    }
    command.spawn()
}

/// Reads a pipe, keeping the first `limit` bytes and discarding the rest.
///
/// Discarding rather than stopping matters: a reader that simply gave up would
/// block the child on its next write, and a build that hangs because its output
/// was too interesting is a poor way to learn that.
async fn read_capped<R>(mut reader: R, limit: usize) -> std::io::Result<(String, bool)>
where
    R: AsyncRead + Unpin,
{
    let mut kept: Vec<u8> = Vec::new();
    // On the heap: two of these on the stack would put sixteen kilobytes into
    // every future that awaits this, which the linter notices and is right to.
    let mut buffer = vec![0_u8; 8_192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if kept.len() < limit {
            let room = limit - kept.len();
            let take = room.min(read);
            kept.extend_from_slice(&buffer[..take]);
            truncated |= take < read;
        } else {
            truncated = true;
        }
    }
    Ok((String::from_utf8_lossy(&kept).into_owned(), truncated))
}
