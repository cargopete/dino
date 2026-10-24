//! Commands from the user's config that run after a file tool edits a file.
//!
//! They come from the user file only, never a project file, because a hook is
//! code execution. Each runs in the session's sandbox, like `exec`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gyr_core::ToolError;
use gyr_core::ToolFuture;
use gyr_core::ToolRuntime;
use gyr_protocol::ToolAction;
use gyr_protocol::ToolCall;
use gyr_protocol::ToolDefinition;
use gyr_protocol::ToolOutput;
use gyr_sandbox::Sandbox;
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
            let (Some(path), false) = (path, output.is_error) else {
                return Ok(output);
            };
            let wanted = self.hooks.iter().any(|hook| hook.applies_to(path));
            if !wanted || !EDITS.contains(&call.name.as_str()) {
                return Ok(output);
            }
            let report = self.run_hooks(path).await;
            let content = match serde_json::from_str::<Value>(&output.content) {
                Ok(Value::Object(mut fields)) => {
                    fields.insert("hooks".into(), report);
                    Value::Object(fields).to_string()
                }
                _ => format!("{}\n\nhooks: {report}", output.content),
            };
            Ok(ToolOutput {
                content,
                is_error: false,
            })
        })
    }
}
