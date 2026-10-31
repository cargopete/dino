//! The `shell` tool: one command line through `sh -c`, inside the sandbox.
//!
//! RFC-0010 section 2 preferred an argument vector so that what is approved is
//! exactly what runs. That still holds here: the approval and any session rule
//! are keyed on the whole line, never on a prefix of it.

use std::path::PathBuf;
use std::sync::Arc;

use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_core::workspace::WorkspaceRoot;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use dino_sandbox::Sandbox;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::ExecLimits;
use crate::exec::ExecOutput;
use crate::exec::capped;
use crate::exec::json_output;
use crate::process;

const NAME: &str = "shell";
const SH: &str = "/bin/sh";

#[derive(Debug)]
pub struct ShellTool {
    root: WorkspaceRoot,
    limits: ExecLimits,
    sandbox: Arc<dyn Sandbox>,
    escalation: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    command: String,
    directory: Option<String>,
    #[serde(default)]
    unconfined: bool,
    justification: Option<String>,
}

impl Request {
    fn parse(arguments: &Value) -> Result<Self, ToolError> {
        let request: Self = serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::new(format!("invalid {NAME} arguments: {error}")))?;
        if request.command.trim().is_empty() {
            return Err(ToolError::new("the command line must not be empty"));
        }
        Ok(request)
    }
}

impl ShellTool {
    /// Creates the tool for one workspace root.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the root cannot be canonicalised or is not a
    /// directory.
    pub fn new(
        root: impl AsRef<std::path::Path>,
        limits: ExecLimits,
        sandbox: Arc<dyn Sandbox>,
    ) -> Result<Self, ToolError> {
        Ok(Self {
            root: WorkspaceRoot::new(root)?,
            limits,
            sandbox,
            escalation: false,
        })
    }

    /// Lets the model ask for one line to run outside the sandbox, which a
    /// person then approves or refuses. For sessions a person is watching.
    #[must_use]
    pub fn with_escalation(mut self) -> Self {
        self.escalation = true;
        self
    }

    fn unconfined(&self, request: &Request) -> Result<Option<String>, ToolError> {
        crate::escalation::requested(
            self.escalation,
            request.unconfined,
            request.justification.as_deref(),
        )
    }

    fn directory(&self, request: &Request) -> Result<PathBuf, ToolError> {
        match &request.directory {
            Some(directory) => self.root.resolve_directory(directory),
            None => Ok(self.root.path().to_path_buf()),
        }
    }

    async fn run(&self, request: Request) -> Result<ToolOutput, ToolError> {
        let directory = self.directory(&request)?;
        let arguments = ["-c".to_owned(), request.command.clone()];
        let sandbox: &dyn Sandbox = if self.unconfined(&request)?.is_some() {
            &dino_sandbox::Unconfined
        } else {
            self.sandbox.as_ref()
        };
        let execution = process::run(
            SH,
            &arguments,
            &directory,
            sandbox,
            self.limits.max_captured_bytes,
            self.limits.timeout,
        )
        .await?;
        let note = process::network_note(sandbox, &execution);
        let max = self.limits.max_output_bytes;
        let stderr = if execution.timed_out {
            format!(
                "no result: killed after {} seconds",
                self.limits.timeout.as_secs()
            )
        } else {
            capped(&execution.stderr, max)
        };
        json_output(&ExecOutput {
            command: request.command,
            exit_code: execution.exit_code,
            timed_out: execution.timed_out,
            stdout: capped(&execution.stdout, max),
            stderr,
            truncated: execution.truncated
                || execution.stdout.len() > max
                || execution.stderr.len() > max,
            restored_protected_paths: execution.restored,
            note,
        })
    }
}

impl ToolRuntime for ShellTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definition = ToolDefinition {
            name: NAME.into(),
            description: format!(
                "Run one command line through `sh -c`, for when you need a pipe, a \
                 redirect, a glob or `&&`. Prefer `exec` for a single program and the \
                 read and search tools over cat and grep. Each distinct line needs its own \
                 approval. Anything the line starts, `&` included, is stopped when it \
                 finishes, so start a server, use it and stop it in one line. Containment: {}.{}",
                self.sandbox.label(),
                if self.escalation {
                    crate::escalation::DESCRIPTION
                } else {
                    ""
                }
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The command line, exactly as sh should run it."
                    },
                    "directory": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Working directory relative to the workspace root."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        };
        if self.escalation
            && let (Some(properties), Value::Object(extra)) = (
                definition.input_schema["properties"].as_object_mut(),
                crate::escalation::properties(),
            )
        {
            properties.extend(extra);
        }
        vec![definition]
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if call.name != NAME {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        let request = Request::parse(&call.arguments)?;
        self.directory(&request)?;
        Ok(match self.unconfined(&request)? {
            Some(reason) => ToolAction::unconfined_process(request.command, reason),
            None => ToolAction::process(request.command),
        })
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let request = if call.name == NAME {
            Request::parse(&call.arguments)
        } else {
            Err(ToolError::new(format!("unknown tool {:?}", call.name)))
        };
        Box::pin(async move { self.run(request?).await })
    }
}
