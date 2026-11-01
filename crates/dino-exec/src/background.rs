//! The `background` tool: a command line that keeps running across calls, such
//! as a development server, inside the same sandbox as `shell`.
//!
//! Each process runs in a group of its own, and every group is ended when it is
//! stopped or when the tool is dropped with the session, so nothing a model
//! starts outlives the session that started it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_core::workspace::WorkspaceRoot;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use dino_sandbox::Sandbox;
use dino_sandbox::Snapshot;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;

use crate::exec::json_output;
use crate::process;

const NAME: &str = "background";
const SH: &str = "/bin/sh";
const MAX_RUNNING: usize = 4;
/// Output kept per process; older output is dropped from the front.
const KEPT_BYTES: usize = 256 * 1_024;
/// Most output one call returns.
const RETURNED_BYTES: usize = 16 * 1_024;
/// How long `start` waits, so a command that fails at once says so.
const SETTLE: Duration = Duration::from_millis(700);

#[derive(Debug, Default)]
struct Output {
    text: String,
    /// Bytes dropped from the front of `text` to keep it within bounds.
    dropped: usize,
    /// Absolute offset the model has read up to.
    read_to: usize,
}

impl Output {
    fn push(&mut self, chunk: &str) {
        self.text.push_str(chunk);
        if self.text.len() > KEPT_BYTES {
            let mut cut = self.text.len() - KEPT_BYTES;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
            self.dropped += cut;
        }
    }

    /// What arrived since the last read, at most [`RETURNED_BYTES`] of the
    /// latest, and how much was skipped.
    fn unread(&mut self) -> (String, usize) {
        let end = self.dropped + self.text.len();
        let from = self.read_to.max(self.dropped);
        let mut start = from - self.dropped;
        let mut skipped = from - self.read_to;
        if self.text.len() - start > RETURNED_BYTES {
            let mut cut = self.text.len() - RETURNED_BYTES;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            skipped += cut - start;
            start = cut;
        }
        self.read_to = end;
        (self.text[start..].to_owned(), skipped)
    }
}

/// Whether a process is still going, and how it ended if not. A process
/// killed by a signal ends with no code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    Exited(Option<i32>),
}

impl State {
    fn running(self) -> bool {
        self == Self::Running
    }

    fn code(self) -> Option<i32> {
        match self {
            Self::Running => None,
            Self::Exited(code) => code,
        }
    }
}

#[derive(Debug)]
struct Process {
    command: String,
    group: Option<i32>,
    started: Instant,
    output: Arc<Mutex<Output>>,
    exit: Arc<Mutex<State>>,
    snapshot: Option<Snapshot>,
}

impl Process {
    fn state(&self) -> State {
        self.exit.lock().map_or(State::Running, |state| *state)
    }

    /// Ends the group, and puts protected paths back where the sandbox can
    /// only restore them afterwards.
    fn end(&mut self) -> Vec<String> {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(group),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        self.snapshot
            .as_mut()
            .and_then(|snapshot| snapshot.restore().ok())
            .unwrap_or_default()
    }
}

#[derive(Debug)]
pub struct BackgroundTool {
    root: WorkspaceRoot,
    sandbox: Arc<dyn Sandbox>,
    processes: Mutex<BTreeMap<u32, Process>>,
    next: Mutex<u32>,
}

impl BackgroundTool {
    /// Creates the tool for one workspace root.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the root cannot be canonicalised or is not a
    /// directory.
    pub fn new(
        root: impl AsRef<std::path::Path>,
        sandbox: Arc<dyn Sandbox>,
    ) -> Result<Self, ToolError> {
        Ok(Self {
            root: WorkspaceRoot::new(root)?,
            sandbox,
            processes: Mutex::new(BTreeMap::new()),
            next: Mutex::new(1),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<u32, Process>>, ToolError> {
        self.processes
            .lock()
            .map_err(|_| ToolError::new("the process table lock was poisoned"))
    }

    async fn start(
        &self,
        command: String,
        directory: Option<String>,
    ) -> Result<ToolOutput, ToolError> {
        let running = self
            .lock()?
            .values()
            .filter(|process| process.state().running())
            .count();
        if running >= MAX_RUNNING {
            return Err(ToolError::new(format!(
                "{running} processes are already running; stop one first"
            )));
        }
        let directory: PathBuf = match &directory {
            Some(directory) => self.root.resolve_directory(directory)?,
            None => self.root.path().to_path_buf(),
        };
        let (mut spawn, snapshot) = process::prepare(
            SH,
            &["-c".to_owned(), command.clone()],
            &directory,
            self.sandbox.as_ref(),
            &[],
        )?;
        let mut child = spawn
            .kill_on_drop(false)
            .spawn()
            .map_err(|error| ToolError::new(format!("cannot start {command}: {error}")))?;
        let group = child.id().and_then(|id| i32::try_from(id).ok());
        let output = Arc::new(Mutex::new(Output::default()));
        let exit = Arc::new(Mutex::new(State::Running));
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(collect(stdout, Arc::clone(&output)));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(collect(stderr, Arc::clone(&output)));
        }
        let finished = Arc::clone(&exit);
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|status| status.code());
            if let Ok(mut exit) = finished.lock() {
                *exit = State::Exited(code);
            }
        });

        let id = {
            let mut next = self
                .next
                .lock()
                .map_err(|_| ToolError::new("the process counter lock was poisoned"))?;
            let id = *next;
            *next += 1;
            id
        };
        self.lock()?.insert(
            id,
            Process {
                command,
                group,
                started: Instant::now(),
                output,
                exit,
                snapshot,
            },
        );
        tokio::time::sleep(SETTLE).await;
        self.report(id)
    }

    /// The process's state and what it has printed since the last look.
    fn report(&self, id: u32) -> Result<ToolOutput, ToolError> {
        let mut processes = self.lock()?;
        let process = processes
            .get_mut(&id)
            .ok_or_else(|| ToolError::new(format!("there is no background process {id}")))?;
        let state = process.state();
        let (output, skipped) = process
            .output
            .lock()
            .map(|mut output| output.unread())
            .unwrap_or_default();
        let restored = if state.running() {
            Vec::new()
        } else {
            process.end()
        };
        json_output(&Report {
            id,
            command: process.command.clone(),
            running: state.running(),
            exit_code: state.code(),
            output,
            skipped_bytes: skipped,
            restored_protected_paths: restored,
        })
    }

    async fn stop(&self, id: u32) -> Result<ToolOutput, ToolError> {
        let restored = {
            let mut processes = self.lock()?;
            let process = processes
                .get_mut(&id)
                .ok_or_else(|| ToolError::new(format!("there is no background process {id}")))?;
            process.end()
        };
        // A moment for the reader tasks to drain what the process left.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let report = self.report(id)?;
        self.lock()?.remove(&id);
        if restored.is_empty() {
            return Ok(report);
        }
        let mut value: Value = serde_json::from_str(&report.content).unwrap_or(Value::Null);
        value["restored_protected_paths"] = json!(restored);
        Ok(ToolOutput::success(value.to_string()))
    }

    fn list(&self) -> Result<ToolOutput, ToolError> {
        let processes = self.lock()?;
        let rows: Vec<Value> = processes
            .iter()
            .map(|(id, process)| {
                let state = process.state();
                json!({
                    "id": id,
                    "command": process.command,
                    "running": state.running(),
                    "exit_code": state.code(),
                    "seconds": process.started.elapsed().as_secs(),
                })
            })
            .collect();
        json_output(&json!({ "processes": rows }))
    }
}

impl Drop for BackgroundTool {
    fn drop(&mut self) {
        if let Ok(mut processes) = self.processes.lock() {
            for process in processes.values_mut() {
                process.end();
            }
        }
    }
}

async fn collect<R: AsyncRead + Unpin>(mut reader: R, output: Arc<Mutex<Output>>) {
    let mut buffer = vec![0_u8; 8_192];
    while let Ok(read) = reader.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        if let Ok(mut output) = output.lock() {
            output.push(&String::from_utf8_lossy(&buffer[..read]));
        }
    }
}

#[derive(Debug, Serialize)]
struct Report {
    id: u32,
    command: String,
    running: bool,
    exit_code: Option<i32>,
    output: String,
    /// Output that arrived but is not shown, because it was too much.
    #[serde(skip_serializing_if = "is_zero")]
    skipped_bytes: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    restored_protected_paths: Vec<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Start {
        command: String,
        directory: Option<String>,
    },
    Output {
        id: u32,
    },
    Stop {
        id: u32,
    },
    List,
}

impl Request {
    fn parse(arguments: &Value) -> Result<Self, ToolError> {
        let request: Self = serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::new(format!("invalid {NAME} arguments: {error}")))?;
        if let Self::Start { command, .. } = &request
            && command.trim().is_empty()
        {
            return Err(ToolError::new("the command line must not be empty"));
        }
        Ok(request)
    }
}

impl ToolRuntime for BackgroundTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: NAME.into(),
            description: format!(
                "Run a command line that keeps going across calls, such as a server or a \
                 watcher, contained as shell is. start returns an id and its first output; \
                 output returns what it printed since you last looked; stop ends it and all \
                 it started; list shows them. At most {MAX_RUNNING} at once, all stopped when \
                 the session ends."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["start", "output", "stop", "list"]},
                    "command": {"type": "string", "minLength": 1, "description": "For start: the command line."},
                    "directory": {"type": "string", "minLength": 1, "description": "For start: relative to the workspace root."},
                    "id": {"type": "integer", "minimum": 1, "description": "For output and stop."}
                },
                "required": ["op"],
                "additionalProperties": false
            }),
        }]
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if call.name != NAME {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        Ok(match Request::parse(&call.arguments)? {
            Request::Start { command, .. } => {
                ToolAction::process(format!("{command} (in the background)"))
            }
            Request::Output { .. } | Request::Stop { .. } | Request::List => {
                ToolAction::read_only()
            }
        })
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let request = if call.name == NAME {
            Request::parse(&call.arguments)
        } else {
            Err(ToolError::new(format!("unknown tool {:?}", call.name)))
        };
        Box::pin(async move {
            match request? {
                Request::Start { command, directory } => self.start(command, directory).await,
                Request::Output { id } => self.report(id),
                Request::Stop { id } => self.stop(id).await,
                Request::List => self.list(),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use dino_sandbox::Unconfined;

    use super::*;

    fn call(arguments: Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: NAME.into(),
            arguments,
        }
    }

    async fn run(tool: &BackgroundTool, arguments: Value) -> Value {
        let output = tool.execute(&call(arguments)).await.unwrap();
        serde_json::from_str(&output.content).unwrap()
    }

    #[tokio::test]
    async fn a_process_keeps_running_reports_new_output_and_stops() {
        let root = std::env::temp_dir().join(format!("dino-background-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let tool = BackgroundTool::new(&root, Arc::new(Unconfined)).unwrap();

        let started = run(
            &tool,
            json!({"op": "start", "command": "echo ready; while true; do sleep 0.1; done"}),
        )
        .await;
        assert_eq!(started["id"], 1);
        assert_eq!(started["running"], true);
        assert_eq!(started["output"], "ready\n");

        let again = run(&tool, json!({"op": "output", "id": 1})).await;
        assert_eq!(again["output"], "", "only what is new");
        assert_eq!(
            run(&tool, json!({"op": "list"})).await["processes"][0]["running"],
            true
        );

        let stopped = run(&tool, json!({"op": "stop", "id": 1})).await;
        assert_eq!(stopped["running"], false);
        assert!(
            tool.execute(&call(json!({"op": "output", "id": 1})))
                .await
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_command_that_fails_at_once_says_so_on_start() {
        let root =
            std::env::temp_dir().join(format!("dino-background-fail-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let tool = BackgroundTool::new(&root, Arc::new(Unconfined)).unwrap();

        let started = run(
            &tool,
            json!({"op": "start", "command": "echo broken >&2; exit 3"}),
        )
        .await;
        assert_eq!(started["running"], false);
        assert_eq!(started["exit_code"], 3);
        assert_eq!(started["output"], "broken\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn ending_the_session_ends_its_processes() {
        let root =
            std::env::temp_dir().join(format!("dino-background-drop-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let pid_file = root.join("pid");
        let tool = BackgroundTool::new(&root, Arc::new(Unconfined)).unwrap();
        run(
            &tool,
            json!({"op": "start", "command": format!("echo $$ > {}; while true; do sleep 0.1; done", pid_file.display())}),
        )
        .await;
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let alive =
            |pid: i32| nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
        assert!(alive(pid));

        drop(tool);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!alive(pid), "the process outlived the session");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn output_keeps_its_bounds_and_says_what_it_skipped() {
        let mut output = Output::default();
        output.push(&"x".repeat(KEPT_BYTES + 10));
        let (text, skipped) = output.unread();
        assert_eq!(text.len(), RETURNED_BYTES);
        assert_eq!(skipped, KEPT_BYTES + 10 - RETURNED_BYTES);
        assert_eq!(output.unread(), (String::new(), 0));
    }
}
