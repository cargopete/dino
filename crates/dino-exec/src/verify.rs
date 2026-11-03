//! The workspace's own check, such as clippy and the test suite together: run
//! before a task the person said to finish may end, and before a commit the
//! model makes, so a commit is not followed by its fix-up.

use std::path::PathBuf;
use std::sync::Arc;
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

use crate::exec::capped;
use crate::process;

/// What a failing verify command may say to the model.
const MAX_OUTPUT_BYTES: usize = 4 * 1_024;

/// One argument vector, run in the session's sandbox.
#[derive(Clone)]
pub struct Verify {
    command: Vec<String>,
    root: PathBuf,
    sandbox: Arc<dyn Sandbox>,
    timeout: Duration,
}

impl Verify {
    #[must_use]
    pub fn new(command: Vec<String>, root: impl Into<PathBuf>, sandbox: Arc<dyn Sandbox>) -> Self {
        Self {
            command,
            root: root.into(),
            sandbox,
            timeout: Duration::from_secs(1_200),
        }
    }

    /// How the command failed, with what it printed, or `None` when it passed.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when it could not be run at all.
    pub async fn failure(&self) -> Result<Option<String>, ToolError> {
        let Some((program, arguments)) = self.command.split_first() else {
            return Ok(None);
        };
        let execution = process::run(
            program,
            arguments,
            &self.root,
            self.sandbox.as_ref(),
            256 * 1_024,
            self.timeout,
        )
        .await?;
        if execution.exit_code == Some(0) {
            return Ok(None);
        }
        let said = format!("{}{}", execution.stdout, execution.stderr);
        let code = execution
            .exit_code
            .map_or_else(|| "was killed".to_owned(), |code| format!("exited {code}"));
        Ok(Some(format!(
            "The workspace's verify command `{}` {code}:\n{}",
            self.command.join(" "),
            capped(said.trim(), MAX_OUTPUT_BYTES)
        )))
    }
}

impl dino_core::FinishCheck for Verify {
    fn unfinished(&self) -> dino_core::CheckFuture<'_> {
        Box::pin(async move { self.failure().await.ok().flatten() })
    }
}

/// Wraps `exec` and `shell`, and runs [`Verify`] before any call of theirs
/// that makes a git commit. A failure refuses the commit with the output.
pub struct BeforeCommit {
    inner: Box<dyn ToolRuntime>,
    verify: Verify,
}

impl BeforeCommit {
    #[must_use]
    pub fn new(inner: Box<dyn ToolRuntime>, verify: Verify) -> Self {
        Self { inner, verify }
    }
}

impl ToolRuntime for BeforeCommit {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        self.inner.classify(call)
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let call = call.clone();
        Box::pin(async move {
            if commits(&call) {
                let failure = match self.verify.failure().await {
                    Ok(failure) => failure,
                    Err(error) => Some(format!(
                        "The workspace's verify command could not run: {error}"
                    )),
                };
                if let Some(failure) = failure {
                    return Ok(ToolOutput::error(format!(
                        "Not committed. {failure}\nFix what it reports, then commit again."
                    )));
                }
            }
            self.inner.execute(&call).await
        })
    }

    fn history_lost(&self) {
        self.inner.history_lost();
    }

    fn begin_turn(&self, calls: &[ToolCall]) {
        self.inner.begin_turn(calls);
    }
}

/// Whether an `exec` or `shell` call runs `git commit`, with or without git's
/// own options before the subcommand.
fn commits(call: &ToolCall) -> bool {
    let command = call.arguments.get("command");
    let commands: Vec<Vec<String>> = match call.name.as_str() {
        "exec" => vec![
            command
                .and_then(Value::as_array)
                .map(|words| {
                    words
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        ],
        "shell" => command
            .and_then(Value::as_str)
            .unwrap_or_default()
            .split([';', '&', '|', '(', ')', '\n'])
            .map(|part| {
                part.split_whitespace()
                    .map(|word| word.trim_matches(['"', '\'']).to_owned())
                    .collect()
            })
            .collect(),
        _ => return false,
    };
    commands.iter().any(|words| git_commit(words))
}

/// One command, as words: `git`, after any variable assignments, then git's
/// options, then `commit`.
fn git_commit(words: &[String]) -> bool {
    let mut words = words
        .iter()
        .skip_while(|word| word.contains('=') && !word.starts_with('-'));
    if !words
        .next()
        .is_some_and(|program| program == "git" || program.ends_with("/git"))
    {
        return false;
    }
    while let Some(word) = words.next() {
        match word.as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => {
                words.next();
            }
            option if option.starts_with('-') => {}
            subcommand => return subcommand == "commit",
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::commits;
    use dino_protocol::ToolCall;
    use serde_json::Value;
    use serde_json::json;

    fn call(name: &str, command: &Value) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: json!({"command": command}),
        }
    }

    #[test]
    fn a_commit_is_seen_however_it_is_spelled() {
        for (name, command) in [
            ("exec", json!(["git", "commit", "-m", "x"])),
            ("exec", json!(["git", "-C", "core", "commit", "--amend"])),
            (
                "shell",
                json!("cargo fmt && git add -A && git commit -m 'Fix it'"),
            ),
            (
                "shell",
                json!("cd core; /usr/bin/git -c user.name=x commit -qm x"),
            ),
        ] {
            assert!(commits(&call(name, &command)), "{command}");
        }
    }

    #[test]
    fn other_git_commands_and_other_tools_are_not_commits() {
        for (name, command) in [
            ("exec", json!(["git", "log", "--grep", "commit"])),
            ("shell", json!("git show HEAD --stat | grep commit")),
            ("shell", json!("echo git commit")),
            ("background", json!("git commit -m x")),
        ] {
            assert!(!commits(&call(name, &command)), "{command}");
        }
    }
}
