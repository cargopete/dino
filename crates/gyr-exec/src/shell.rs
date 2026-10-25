//! The `shell` tool: one command line through `sh -c`, inside the sandbox.
//!
//! RFC-0010 section 2 preferred an argument vector so that what is approved is
//! exactly what runs. That still holds here: the approval and any session rule
//! are keyed on the whole line, never on a prefix of it.

use std::path::PathBuf;
use std::sync::Arc;

use gyr_core::ToolError;
use gyr_core::ToolFuture;
use gyr_core::ToolRuntime;
use gyr_core::workspace::WorkspaceRoot;
use gyr_protocol::ToolAction;
use gyr_protocol::ToolCall;
use gyr_protocol::ToolDefinition;
use gyr_protocol::ToolOutput;
use gyr_sandbox::Sandbox;
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    command: String,
    directory: Option<String>,
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
        })
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
        let execution = process::run(
            SH,
            &arguments,
            &directory,
            self.sandbox.as_ref(),
            self.limits.max_captured_bytes,
            self.limits.timeout,
        )
        .await?;
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
        })
    }
}

impl ToolRuntime for ShellTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: NAME.into(),
            description: format!(
                "Run one command line through `sh -c`, for when you need a pipe, a \
                 redirect, a glob or `&&`. Prefer `exec` for a single program and the \
                 read and search tools over cat and grep. Each distinct line needs its own \
                 approval. Anything the line starts, `&` included, is stopped when it \
                 finishes, so start a server, use it and stop it in one line. Containment: {}.",
                self.sandbox.label()
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
                        "description": "Working directory relative to the workspace root."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }]
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if call.name != NAME {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        let request = Request::parse(&call.arguments)?;
        self.directory(&request)?;
        Ok(ToolAction::process(request.command))
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
