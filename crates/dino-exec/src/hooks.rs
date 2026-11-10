//! Commands from the user's config that run around edits, calls, submissions
//! and the end of a task.
//!
//! They come from the user file only, never a project file, because a hook is
//! code execution. Each runs in the session's sandbox, like `exec`, except the
//! pings, which have to reach the network.

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
use dino_core::approval::ApprovalPolicy;
use dino_core::approval::DecisionFuture;
use dino_protocol::ApprovalDecision;
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
    /// For `before_call` and `after_call`: run only for calls of this tool.
    pub tool: Option<String>,
    /// For `before_call` and `after_call`: run only for an `exec` call of this
    /// program, or a `shell` line whose first word it is.
    pub program: Option<String>,
}

impl Hook {
    fn applies_to(&self, path: &str) -> bool {
        self.extension.as_deref().is_none_or(|extension| {
            std::path::Path::new(path)
                .extension()
                .is_some_and(|found| found == extension)
        })
    }

    fn applies_to_call(&self, call: &ToolCall) -> bool {
        self.tool.as_deref().is_none_or(|tool| tool == call.name)
            && self
                .program
                .as_deref()
                .is_none_or(|program| program_of(call).as_deref() == Some(program))
    }

    /// What a failure names: the program alone, as an argument may be a token.
    fn program(&self) -> &str {
        self.command.first().map_or("", String::as_str)
    }

    /// Runs this hook in `sandbox` with `input` on its standard input.
    async fn feed(
        &self,
        root: &Path,
        sandbox: &dyn Sandbox,
        input: &Value,
        timeout: Duration,
    ) -> Result<process::Execution, ToolError> {
        let Some((program, arguments)) = self.command.split_first() else {
            return Err(ToolError::new("the hook has no command"));
        };
        process::run_fed(
            program,
            arguments,
            root,
            sandbox,
            64 * 1_024,
            timeout,
            &[],
            Some(input.to_string().as_bytes()),
        )
        .await
    }
}

/// The program an `exec` call runs, or the first word of a `shell` line, by
/// file name, so `/bin/rm` and `rm` are one.
fn program_of(call: &ToolCall) -> Option<String> {
    let word = match call.name.as_str() {
        "exec" => call.arguments.get("program")?.as_str()?,
        "shell" => call
            .arguments
            .get("command")?
            .as_str()?
            .split_whitespace()
            .next()?,
        _ => return None,
    };
    Path::new(word)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// How long a call or submission hook may take.
const CALL_HOOK_SECONDS: u64 = 60;
/// The exit code that refuses, as Claude Code's hooks have it.
const REFUSE: i32 = 2;

/// An approval policy with the `before_call` hooks in front of it.
///
/// A hook runs once the call is classified and before anyone is asked, and can
/// only refuse: exit 0 passes the call on to the policy it wraps, exit 2 refuses
/// it with the hook's output as the reason, and anything else refuses too,
/// since a gate that failed has not said yes.
pub struct BeforeCall<P> {
    inner: P,
    hooks: Vec<Hook>,
    root: PathBuf,
    sandbox: Arc<dyn Sandbox>,
}

impl<P> BeforeCall<P> {
    #[must_use]
    pub fn new(
        inner: P,
        hooks: Vec<Hook>,
        root: impl Into<PathBuf>,
        sandbox: Arc<dyn Sandbox>,
    ) -> Self {
        Self {
            inner,
            hooks,
            root: root.into(),
            sandbox,
        }
    }

    async fn refusal(&self, call: &ToolCall, action: &ToolAction) -> Option<String> {
        let input = json!({"call": call, "action": action});
        for hook in self.hooks.iter().filter(|hook| hook.applies_to_call(call)) {
            let program = hook.program();
            let execution = match hook
                .feed(
                    &self.root,
                    self.sandbox.as_ref(),
                    &input,
                    Duration::from_secs(CALL_HOOK_SECONDS),
                )
                .await
            {
                Ok(execution) => execution,
                Err(error) => return Some(format!("the before_call hook `{program}`: {error}")),
            };
            match execution.exit_code {
                Some(0) => {}
                Some(REFUSE) => {
                    let said = execution.stdout.trim();
                    return Some(if said.is_empty() {
                        format!("the before_call hook `{program}` refused this call")
                    } else {
                        capped(said, MAX_HOOK_OUTPUT_BYTES)
                    });
                }
                _ if execution.timed_out => {
                    return Some(format!("the before_call hook `{program}` timed out"));
                }
                code => {
                    return Some(format!("the before_call hook `{program}` {}", exited(code)));
                }
            }
        }
        None
    }
}

impl<P: ApprovalPolicy> ApprovalPolicy for BeforeCall<P> {
    fn decide(&self, call: &ToolCall, action: &ToolAction) -> DecisionFuture<'_> {
        let call = call.clone();
        let action = action.clone();
        Box::pin(async move {
            if let Some(reason) = self.refusal(&call, &action).await {
                return ApprovalDecision::denied(reason);
            }
            self.inner.decide(&call, &action).await
        })
    }

    fn begin_turn(&self, calls: &[(ToolCall, ToolAction)]) {
        self.inner.begin_turn(calls);
    }
}

/// Wraps a tool runtime and runs the `after_call` hooks after each of its calls,
/// with the call and its result on standard input. What a hook prints is added
/// to the result under `after_call`, and a failing hook is named there.
pub struct AfterCall {
    inner: Box<dyn ToolRuntime>,
    hooks: Vec<Hook>,
    root: PathBuf,
    sandbox: Arc<dyn Sandbox>,
}

impl AfterCall {
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
            root: root.into(),
            sandbox,
        }
    }
}

impl ToolRuntime for AfterCall {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        self.inner.classify(call)
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let call = call.clone();
        Box::pin(async move {
            let output = self
                .inner
                .execute(&call)
                .await
                .unwrap_or_else(|error| ToolOutput::error(error.to_string()));
            let input = json!({
                "call": call,
                "result": {"content": output.content, "is_error": output.is_error},
            });
            let mut said = Vec::new();
            for hook in self.hooks.iter().filter(|hook| hook.applies_to_call(&call)) {
                let program = hook.program();
                match hook
                    .feed(
                        &self.root,
                        self.sandbox.as_ref(),
                        &input,
                        Duration::from_secs(CALL_HOOK_SECONDS),
                    )
                    .await
                {
                    Ok(execution) if execution.exit_code == Some(0) => {
                        let text = execution.stdout.trim();
                        if !text.is_empty() {
                            said.push(json!({
                                "program": program,
                                "output": capped(text, MAX_HOOK_OUTPUT_BYTES),
                            }));
                        }
                    }
                    Ok(execution) => said.push(json!({
                        "program": program,
                        "exit_code": execution.exit_code,
                        "timed_out": execution.timed_out,
                    })),
                    Err(error) => {
                        said.push(json!({"program": program, "error": error.to_string()}));
                    }
                }
            }
            if said.is_empty() {
                return Ok(output);
            }
            Ok(with_field(&output, "after_call", Value::Array(said)))
        })
    }

    fn history_lost(&self) {
        self.inner.history_lost();
    }

    fn begin_turn(&self, calls: &[ToolCall]) {
        self.inner.begin_turn(calls);
    }

    fn rewritten(&self, path: &str) {
        self.inner.rewritten(path);
    }
}

/// The `prompt_submit` hooks, run on each message a person sends.
#[derive(Clone)]
pub struct PromptHooks {
    hooks: Vec<Hook>,
    root: PathBuf,
    sandbox: Arc<dyn Sandbox>,
}

/// What the `prompt_submit` hooks made of one message.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Submitted {
    /// What each hook that passed printed, under its command, for the model.
    pub context: Vec<(String, String)>,
    /// Set when a hook exited 2: what it printed, for the person.
    pub refused: Option<String>,
    /// Hooks that failed otherwise, by program. The message still goes.
    pub problems: Vec<String>,
}

impl PromptHooks {
    #[must_use]
    pub fn new(hooks: Vec<Hook>, root: impl Into<PathBuf>, sandbox: Arc<dyn Sandbox>) -> Self {
        Self {
            hooks,
            root: root.into(),
            sandbox,
        }
    }

    /// Runs each hook with `{"message": ...}` on standard input, stopping at
    /// the first that refuses.
    pub async fn submitted(&self, message: &str) -> Submitted {
        let mut submitted = Submitted::default();
        let input = json!({"message": message});
        for hook in &self.hooks {
            let program = hook.program();
            match hook
                .feed(
                    &self.root,
                    self.sandbox.as_ref(),
                    &input,
                    Duration::from_secs(CALL_HOOK_SECONDS),
                )
                .await
            {
                Ok(execution) if execution.exit_code == Some(0) => {
                    let text = execution.stdout.trim();
                    if !text.is_empty() {
                        submitted
                            .context
                            .push((hook.command.join(" "), capped(text, 4 * 1_024)));
                    }
                }
                Ok(execution) if execution.exit_code == Some(REFUSE) => {
                    let said = execution.stdout.trim();
                    submitted.refused = Some(if said.is_empty() {
                        format!("the prompt_submit hook `{program}` refused the message")
                    } else {
                        capped(said, MAX_HOOK_OUTPUT_BYTES)
                    });
                    return submitted;
                }
                Ok(execution) if execution.timed_out => submitted
                    .problems
                    .push(format!("the prompt_submit hook `{program}` timed out")),
                Ok(execution) => submitted.problems.push(format!(
                    "the prompt_submit hook `{program}` {}",
                    exited(execution.exit_code)
                )),
                Err(error) => submitted
                    .problems
                    .push(format!("the prompt_submit hook `{program}`: {error}")),
            }
        }
        submitted
    }
}

/// `message` with what the hooks added after it, each under its command.
#[must_use]
pub fn with_context(message: String, context: &[(String, String)]) -> String {
    context.iter().fold(message, |message, (command, text)| {
        format!("{message}\n\nprompt_submit: {command}\n{text}")
    })
}

fn exited(code: Option<i32>) -> String {
    code.map_or_else(|| "was killed".to_owned(), |code| format!("exited {code}"))
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
    with_field(output, "hooks", report)
}

fn with_field(output: &ToolOutput, key: &str, report: Value) -> ToolOutput {
    let content = match serde_json::from_str::<Value>(&output.content) {
        Ok(Value::Object(mut fields)) => {
            fields.insert(key.into(), report);
            Value::Object(fields).to_string()
        }
        _ => format!("{}\n\n{key}: {report}", output.content),
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
    ping(
        "task_end",
        hooks,
        root,
        &[("status", &ended.status), ("message", &ended.message)],
    )
    .await
}

/// Runs each awaiting-approval hook once, with the tool asked about in `{tool}`
/// and a sentence in `{message}`, and in `DINO_TOOL` and `DINO_MESSAGE`.
/// Outside the sandbox, and reported, as a task-end hook is.
pub async fn run_awaiting_hooks(
    hooks: &[Hook],
    root: &std::path::Path,
    tool: &str,
    message: &str,
) -> Vec<String> {
    ping(
        "awaiting_approval",
        hooks,
        root,
        &[("tool", tool), ("message", message)],
    )
    .await
}

/// Runs each hook unconfined, with `{name}` in its arguments and `DINO_NAME`
/// in its environment for each of `values`, and says which failed.
async fn ping(
    event: &str,
    hooks: &[Hook],
    root: &std::path::Path,
    values: &[(&str, &str)],
) -> Vec<String> {
    let names: Vec<String> = values
        .iter()
        .map(|(name, _)| format!("DINO_{}", name.to_uppercase()))
        .collect();
    let environment: Vec<(&str, &str)> = names
        .iter()
        .zip(values)
        .map(|(name, (_, value))| (name.as_str(), *value))
        .collect();
    let mut problems = Vec::new();
    for hook in hooks {
        let argv: Vec<String> = hook
            .command
            .iter()
            .map(|part| {
                values.iter().fold(part.clone(), |part, (name, value)| {
                    part.replace(&format!("{{{name}}}"), value)
                })
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
            &environment,
        )
        .await;
        match outcome {
            Ok(execution) if execution.exit_code == Some(0) => {}
            Ok(execution) if execution.timed_out => {
                problems.push(format!("the {event} hook `{program}` timed out"));
            }
            Ok(execution) => problems.push(format!(
                "the {event} hook `{program}` {}",
                exited(execution.exit_code)
            )),
            Err(error) => {
                problems.push(format!("the {event} hook `{program}` did not run: {error}"));
            }
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
