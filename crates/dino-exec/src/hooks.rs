//! Commands from the user's config that run after a file tool edits a file.
//!
//! They come from the user file only, never a project file, because a hook is
//! code execution. Each runs in the session's sandbox, like `exec`.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use dino_sandbox::Sandbox;
use serde_json::Value;
use serde_json::json;

use crate::exec::capped;
use crate::process;

/// The tools whose successful calls trigger an after-edit hook.
const EDITS: [&str; 2] = ["apply_patch", "write_file"];
/// What a failing hook may say, so a noisy formatter cannot fill the context.
const MAX_HOOK_OUTPUT_BYTES: usize = 2 * 1_024;

/// One after-edit command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    /// An argument vector, with `{path}` standing for the edited file and
    /// `{edition}` for the edition of the package it belongs to.
    pub command: Vec<String>,
    /// Run only for files with this extension, such as `rs`.
    pub extension: Option<String>,
}

impl Hook {
    fn applies_to(&self, path: &str) -> bool {
        self.extension.as_deref().is_none_or(|extension| {
            std::path::Path::new(path)
                .extension()
                .is_some_and(|found| found == extension)
        })
    }
}

/// Wraps the file tools and runs `hooks` after each edit they make.
pub struct AfterEdit {
    inner: Box<dyn ToolRuntime>,
    hooks: Vec<Hook>,
    /// Per path, the edits still to run this model turn and whether one has
    /// succeeded, so the hooks run once, after the last. Running them after
    /// each would reformat the file under the turn's later edits.
    pending: Mutex<HashMap<String, (usize, bool)>>,
    /// Per path, the file as it was before the turn's first edit to it.
    before: Mutex<HashMap<String, Option<Vec<u8>>>>,
    root: PathBuf,
    sandbox: Arc<dyn Sandbox>,
    timeout: Duration,
}

impl AfterEdit {
    #[must_use]
    pub fn new(
        inner: Box<dyn ToolRuntime>,
        hooks: Vec<Hook>,
        root: impl Into<PathBuf>,
        sandbox: Arc<dyn Sandbox>,
    ) -> Self {
        Self {
            inner,
            hooks,
            pending: Mutex::new(HashMap::new()),
            before: Mutex::new(HashMap::new()),
            root: root.into(),
            sandbox,
            timeout: Duration::from_secs(120),
        }
    }

    /// Runs the hooks on `path`, except any the file was not already clean
    /// under before this turn's edits: reformatting the lines the model did not
    /// touch turns its next patch stale. `before` is None for a new file.
    async fn run_hooks(&self, path: &str, before: Option<&[u8]>) -> Value {
        let file = self.root.join(path);
        let edited = std::fs::read(&file).ok();
        let mut ran = Vec::new();
        for hook in self.hooks.iter().filter(|hook| hook.applies_to(path)) {
            let command = hook.command.join(" ");
            let edition = match edition(hook, &self.root, path) {
                Ok(edition) => edition,
                Err(error) => {
                    ran.push(json!({"command": command, "error": error.to_string()}));
                    continue;
                }
            };
            if let Some(before) = before
                && !self.clean_under(hook, path, before, &edition).await
            {
                ran.push(json!({
                    "command": command,
                    "skipped": "the file was not clean under this hook before the edit",
                }));
                continue;
            }
            match self.run_one(hook, path, &edition).await {
                Ok(execution) if execution.exit_code == Some(0) => {
                    ran.push(json!({"command": command, "exit_code": 0}));
                }
                Ok(execution) => {
                    let said = format!("{}{}", execution.stdout, execution.stderr);
                    ran.push(json!({
                        "command": command,
                        "exit_code": execution.exit_code,
                        "timed_out": execution.timed_out,
                        "output": capped(said.trim(), MAX_HOOK_OUTPUT_BYTES),
                    }));
                }
                Err(error) => ran.push(json!({"command": command, "error": error.to_string()})),
            }
        }
        let changed = std::fs::read(&file).ok() != edited;
        let mut report = json!({"after_edit": ran, "changed_file": changed});
        if changed {
            report["note"] = json!(
                "a hook changed this file; the next edit's old_text must match it as it is now"
            );
        }
        report
    }

    async fn run_one(
        &self,
        hook: &Hook,
        path: &str,
        edition: &str,
    ) -> Result<process::Execution, ToolError> {
        let argv: Vec<String> = hook
            .command
            .iter()
            .map(|part| part.replace("{path}", path).replace("{edition}", edition))
            .collect();
        let Some((program, arguments)) = argv.split_first() else {
            return Err(ToolError::new("the hook has no command"));
        };
        process::run(
            program,
            arguments,
            &self.root,
            self.sandbox.as_ref(),
            64 * 1_024,
            self.timeout,
        )
        .await
    }

    /// Whether `hook` leaves `content` as it is, tried on a copy so the file
    /// itself is not touched. A hook that fails on it is not clean either.
    async fn clean_under(&self, hook: &Hook, path: &str, content: &[u8], edition: &str) -> bool {
        static COPIES: AtomicU64 = AtomicU64::new(0);
        let Some(name) = Path::new(path).file_name() else {
            return false;
        };
        let base = self
            .sandbox
            .temp_dir()
            .map_or_else(std::env::temp_dir, Path::to_path_buf);
        let directory = base.join(format!(
            "dino-hook-{}-{}",
            std::process::id(),
            COPIES.fetch_add(1, Ordering::Relaxed)
        ));
        let copy = directory.join(name);
        if std::fs::create_dir_all(&directory)
            .and_then(|()| std::fs::write(&copy, content))
            .is_err()
        {
            return false;
        }
        let clean = match self
            .run_one(hook, &copy.display().to_string(), edition)
            .await
        {
            Ok(execution) => {
                execution.exit_code == Some(0)
                    && std::fs::read(&copy).is_ok_and(|after| after == content)
            }
            Err(_) => false,
        };
        let _ = std::fs::remove_dir_all(&directory);
        clean
    }
}

/// The edition `{edition}` stands for in `hook`, for the file at `path`: from
/// its package's manifest, else Cargo's default.
fn edition(hook: &Hook, root: &Path, path: &str) -> Result<String, ToolError> {
    if !hook.command.iter().any(|part| part.contains("{edition}")) {
        return Ok(String::new());
    }
    let edition = crate::rust::edition_of(root, Path::new(path))?.unwrap_or_else(|| "2015".into());
    // The manifest arrives with the repository, and a hook may be a shell script.
    if edition.is_empty() || !edition.chars().all(|character| character.is_ascii_digit()) {
        return Err(ToolError::new(format!(
            "the manifest for {path} gives the edition {edition:?}, which is not a year"
        )));
    }
    Ok(edition)
}

impl ToolRuntime for AfterEdit {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        self.inner.classify(call)
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let call = call.clone();
        Box::pin(async move {
            let path = call.arguments.get("path").and_then(Value::as_str);
            let wanted = path.is_some_and(|path| {
                EDITS.contains(&call.name.as_str())
                    && self.hooks.iter().any(|hook| hook.applies_to(path))
            });
            if let Some(path) = path.filter(|_| wanted)
                && let Ok(mut before) = self.before.lock()
            {
                before
                    .entry(path.to_owned())
                    .or_insert_with(|| std::fs::read(self.root.join(path)).ok());
            }
            let output = self.inner.execute(&call).await?;
            let Some(path) = path.filter(|_| wanted) else {
                return Ok(output);
            };
            let (last, any_succeeded) = self.settle_edit(path, !output.is_error);
            if !last {
                return Ok(with_report(
                    &output,
                    json!({"deferred": "the hooks run after this turn's last edit to this file"}),
                ));
            }
            let before = self
                .before
                .lock()
                .ok()
                .and_then(|mut before| before.remove(path))
                .flatten();
            if !any_succeeded {
                return Ok(output);
            }
            let report = self.run_hooks(path, before.as_deref()).await;
            if report["changed_file"] == json!(true) {
                self.inner.rewritten(path);
            }
            Ok(with_report(&output, report))
        })
    }

    fn history_lost(&self) {
        self.inner.history_lost();
    }

    fn begin_turn(&self, calls: &[ToolCall]) {
        let mut counts: HashMap<String, (usize, bool)> = HashMap::new();
        for call in calls
            .iter()
            .filter(|call| EDITS.contains(&call.name.as_str()))
        {
            if let Some(path) = call.arguments.get("path").and_then(Value::as_str) {
                counts.entry(path.to_owned()).or_default().0 += 1;
            }
        }
        if let Ok(mut pending) = self.pending.lock() {
            *pending = counts;
        }
        self.inner.begin_turn(calls);
    }
}

impl AfterEdit {
    /// Counts one edit to `path` as run, and says whether it was the turn's last
    /// to that path and whether any of them succeeded. An edit the turn did not
    /// announce counts as its own last.
    fn settle_edit(&self, path: &str, succeeded: bool) -> (bool, bool) {
        let Ok(mut pending) = self.pending.lock() else {
            return (true, succeeded);
        };
        let Some((left, any)) = pending.get_mut(path) else {
            return (true, succeeded);
        };
        *left = left.saturating_sub(1);
        *any |= succeeded;
        let settled = (*left == 0, *any);
        if *left == 0 {
            pending.remove(path);
        }
        settled
    }
}

fn with_report(output: &ToolOutput, report: Value) -> ToolOutput {
    let content = match serde_json::from_str::<Value>(&output.content) {
        Ok(Value::Object(mut fields)) => {
            fields.insert("hooks".into(), report);
            Value::Object(fields).to_string()
        }
        _ => format!("{}\n\nhooks: {report}", output.content),
    };
    ToolOutput {
        content,
        is_error: output.is_error,
    }
}

/// Commands that must pass before the model may end its turn. A failure sends
/// it back once with the command's output, as a broken build does.
pub struct EndTurnHooks {
    hooks: Vec<Hook>,
    root: PathBuf,
    sandbox: Arc<dyn Sandbox>,
}

impl EndTurnHooks {
    #[must_use]
    pub fn new(hooks: Vec<Hook>, root: impl Into<PathBuf>, sandbox: Arc<dyn Sandbox>) -> Self {
        Self {
            hooks,
            root: root.into(),
            sandbox,
        }
    }
}

impl dino_core::FinishCheck for EndTurnHooks {
    fn unfinished(&self) -> dino_core::CheckFuture<'_> {
        Box::pin(async move {
            for hook in &self.hooks {
                let Some((program, arguments)) = hook.command.split_first() else {
                    continue;
                };
                let execution = process::run(
                    program,
                    arguments,
                    &self.root,
                    self.sandbox.as_ref(),
                    64 * 1_024,
                    Duration::from_secs(300),
                )
                .await
                .ok()?;
                if execution.exit_code != Some(0) {
                    let said = format!("{}{}", execution.stdout, execution.stderr);
                    let code = execution
                        .exit_code
                        .map_or_else(|| "was killed".to_owned(), |code| format!("exited {code}"));
                    return Some(format!(
                        "The before_end_turn hook `{}` {code}:\n{}",
                        hook.command.join(" "),
                        capped(said.trim(), MAX_HOOK_OUTPUT_BYTES)
                    ));
                }
            }
            None
        })
    }
}

/// How a run ended, as a task-end hook is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEnd {
    /// One word: `finished`, `failed`, `cancelled` or `stopped`. `finished`
    /// means the model ended its turn, not that anything checked the work.
    pub status: String,
    /// A sentence or two for a person to read where the ping lands.
    pub message: String,
}

/// Runs each task-end hook once, with the outcome in `{status}` and `{message}`
/// in its arguments and in `DINO_STATUS` and `DINO_MESSAGE`. Returns what went
/// wrong with any, by program name only: an argument may be a webhook token.
///
/// Outside the sandbox, because a ping has to reach the network. The hook is
/// from the user file, which a repository cannot write. A hook that exits 0 ran;
/// whether its message arrived is the hook's affair, and nothing is retried.
pub async fn run_task_end_hooks(
    hooks: &[Hook],
    root: &std::path::Path,
    ended: &TaskEnd,
) -> Vec<String> {
    let mut problems = Vec::new();
    for hook in hooks {
        let argv: Vec<String> = hook
            .command
            .iter()
            .map(|part| {
                part.replace("{status}", &ended.status)
                    .replace("{message}", &ended.message)
            })
            .collect();
        let Some((program, arguments)) = argv.split_first() else {
            continue;
        };
        let outcome = process::run_with(
            program,
            arguments,
            root,
            &dino_sandbox::Unconfined,
            4 * 1_024,
            Duration::from_secs(30),
            &[
                ("DINO_STATUS", ended.status.as_str()),
                ("DINO_MESSAGE", ended.message.as_str()),
            ],
        )
        .await;
        match outcome {
            Ok(execution) if execution.exit_code == Some(0) => {}
            Ok(execution) if execution.timed_out => {
                problems.push(format!("the task_end hook `{program}` timed out"));
            }
            Ok(execution) => problems.push(format!(
                "the task_end hook `{program}` {}",
                execution
                    .exit_code
                    .map_or_else(|| "was killed".to_owned(), |code| format!("exited {code}"))
            )),
            Err(error) => problems.push(format!(
                "the task_end hook `{program}` did not run: {error}"
            )),
        }
    }
    problems
}

/// Runs each session-start hook once and returns what it printed, for the
/// system prompt. A hook that fails says so in place of its output.
pub async fn run_start_hooks(
    hooks: &[Hook],
    root: &std::path::Path,
    sandbox: &Arc<dyn Sandbox>,
) -> Vec<(String, String)> {
    let mut said = Vec::new();
    for hook in hooks {
        let Some((program, arguments)) = hook.command.split_first() else {
            continue;
        };
        let command = hook.command.join(" ");
        let text = match process::run(
            program,
            arguments,
            root,
            sandbox.as_ref(),
            64 * 1_024,
            Duration::from_secs(60),
        )
        .await
        {
            Ok(execution) if execution.exit_code == Some(0) => {
                capped(execution.stdout.trim(), 4 * 1_024)
            }
            Ok(execution) => format!("(failed with {:?})", execution.exit_code),
            Err(error) => format!("(could not run: {error})"),
        };
        said.push((command, text));
    }
    said
}
