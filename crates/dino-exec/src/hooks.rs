//! Commands from the user's config that run after a file tool edits a file.
//!
//! They come from the user file only, never a project file, because a hook is
//! code execution. Each runs in the session's sandbox, like `exec`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
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
    /// An argument vector, with `{path}` standing for the edited file.
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
            root: root.into(),
            sandbox,
            timeout: Duration::from_secs(120),
        }
    }

    async fn run_hooks(&self, path: &str) -> Value {
        let file = self.root.join(path);
        let before = std::fs::read(&file).ok();
        let mut ran = Vec::new();
        for hook in self.hooks.iter().filter(|hook| hook.applies_to(path)) {
            let argv: Vec<String> = hook
                .command
                .iter()
                .map(|part| part.replace("{path}", path))
                .collect();
            let Some((program, arguments)) = argv.split_first() else {
                continue;
            };
            let command = argv.join(" ");
            match process::run(
                program,
                arguments,
                &self.root,
                self.sandbox.as_ref(),
                64 * 1_024,
                self.timeout,
            )
            .await
            {
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
        let changed = std::fs::read(&file).ok() != before;
        let mut report = json!({"after_edit": ran, "changed_file": changed});
        if changed {
            report["note"] = json!("a hook changed this file; read it again before editing it");
        }
        report
    }
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
            let output = self.inner.execute(&call).await?;
            let path = call.arguments.get("path").and_then(Value::as_str);
            let Some(path) = path else {
                return Ok(output);
            };
            let wanted = self.hooks.iter().any(|hook| hook.applies_to(path));
            if !wanted || !EDITS.contains(&call.name.as_str()) {
                return Ok(output);
            }
            let (last, any_succeeded) = self.settle_edit(path, !output.is_error);
            if !last {
                return Ok(with_report(
                    &output,
                    json!({"deferred": "the hooks run after this turn's last edit to this file"}),
                ));
            }
            if !any_succeeded {
                return Ok(output);
            }
            let report = self.run_hooks(path).await;
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
