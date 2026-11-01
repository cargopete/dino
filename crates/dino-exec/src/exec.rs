//! The `exec` tool.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_core::workspace::WorkspaceRoot;

use crate::process;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use dino_sandbox::Sandbox;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecLimits {
    pub max_output_bytes: usize,
    pub max_captured_bytes: usize,
    pub timeout: Duration,
}

impl Default for ExecLimits {
    fn default() -> Self {
        Self {
            max_output_bytes: 32 * 1_024,
            max_captured_bytes: 8 * 1_024 * 1_024,
            timeout: Duration::from_secs(600),
        }
    }
}

#[derive(Debug)]
pub struct ExecTool {
    root: WorkspaceRoot,
    limits: ExecLimits,
    sandbox: Arc<dyn Sandbox>,
    escalation: bool,
}

impl ExecTool {
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

    /// Lets the model ask for one command to run outside the sandbox, which a
    /// person then approves or refuses. For sessions a person is watching.
    #[must_use]
    pub fn with_escalation(mut self) -> Self {
        self.escalation = true;
        self
    }

    /// Resolves a request's working directory and program against the fence.
    fn plan(&self, request: &ExecRequest) -> Result<Plan, ToolError> {
        let Some((program, arguments)) = request.command.split_first() else {
            return Err(ToolError::new("command must hold at least a program"));
        };
        if program.is_empty() {
            return Err(ToolError::new("the program name must not be empty"));
        }

        let directory = match &request.directory {
            Some(directory) => self.root.resolve_directory(directory)?,
            None => self.root.path().to_path_buf(),
        };

        // A *relative* program path is resolved against the fence, so
        // `./scripts/build.sh` works and `../../elsewhere/script` does not.
        //
        // An absolute path is passed through. Refusing `/usr/bin/curl` while
        // allowing `curl` by way of PATH would be a distinction with no
        // security content: the same binary, reached two ways. What a program
        // may do is the sandbox's business, and choosing which programs exist at
        // all is the allow-list RFC-0010 section 3 declined to build.
        let path = std::path::Path::new(program);
        let program = if path.is_absolute() {
            program.clone()
        } else if program.contains(std::path::MAIN_SEPARATOR) {
            self.root.resolve_file(program)?.display().to_string()
        } else {
            program.clone()
        };

        Ok(Plan {
            program,
            arguments: arguments.to_vec(),
            directory,
        })
    }

    async fn run(&self, request: ExecRequest) -> Result<ToolOutput, ToolError> {
        let plan = self.plan(&request)?;
        let unconfined = self.unconfined(&request)?.is_some();
        let sandbox: &dyn Sandbox = if unconfined {
            &dino_sandbox::Unconfined
        } else {
            self.sandbox.as_ref()
        };
        let execution = process::run(
            &plan.program,
            &plan.arguments,
            &plan.directory,
            sandbox,
            self.limits.max_captured_bytes,
            self.limits.timeout,
        )
        .await?;
        let note = process::network_note(sandbox, &execution);

        if execution.timed_out {
            return json_output(&ExecOutput {
                command: plan.subject(),
                exit_code: None,
                timed_out: true,
                stdout: String::new(),
                stderr: format!(
                    "no result: killed after {} seconds",
                    self.limits.timeout.as_secs()
                ),
                truncated: execution.truncated,
                restored_protected_paths: execution.restored,
                note,
            });
        }

        json_output(&ExecOutput {
            command: plan.subject(),
            exit_code: execution.exit_code,
            stdout: capped(&execution.stdout, self.limits.max_output_bytes),
            stderr: capped(&execution.stderr, self.limits.max_output_bytes),
            timed_out: false,
            truncated: execution.truncated
                || execution.stdout.len() > self.limits.max_output_bytes
                || execution.stderr.len() > self.limits.max_output_bytes,
            restored_protected_paths: execution.restored,
            note,
        })
    }
}

impl ExecTool {
    fn unconfined(&self, request: &ExecRequest) -> Result<Option<String>, ToolError> {
        crate::escalation::requested(
            self.escalation,
            request.unconfined,
            request.justification.as_deref(),
        )
    }
}

impl ToolRuntime for ExecTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definition = ToolDefinition {
            name: "exec".into(),
            description: format!(
                "Run one program with one argument vector. There is no shell: no pipes, no \
                 redirection, no globbing and no &&; use the program's own flags. Prefer \
                 read and search over cat and grep, and the cargo tool for Cargo.{}",
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
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "description": "The program followed by its arguments."
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
        if call.name != "exec" {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        let request = ExecRequest::parse(&call.arguments)?;
        let subject = self.plan(&request)?.subject();
        Ok(match self.unconfined(&request)? {
            Some(reason) => ToolAction::unconfined_process(subject, reason),
            None => ToolAction::process(subject),
        })
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let request = if call.name == "exec" {
            ExecRequest::parse(&call.arguments)
        } else {
            Err(ToolError::new(format!("unknown tool {:?}", call.name)))
        };
        Box::pin(async move { self.run(request?).await })
    }
}

/// A resolved request, ready to run.
#[derive(Debug)]
struct Plan {
    program: String,
    arguments: Vec<String>,
    directory: PathBuf,
}

impl Plan {
    /// The command as it will be run, and as a session rule is keyed.
    fn subject(&self) -> String {
        let mut parts = Vec::with_capacity(self.arguments.len() + 1);
        parts.push(self.program.clone());
        parts.extend(self.arguments.iter().cloned());
        parts.join(" ")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecRequest {
    command: Vec<String>,
    directory: Option<String>,
    #[serde(default)]
    unconfined: bool,
    justification: Option<String>,
}

impl ExecRequest {
    fn parse(arguments: &Value) -> Result<Self, ToolError> {
        let mut arguments = arguments.clone();
        if let Some(Value::String(line)) = arguments.get("command") {
            // Models sometimes send the array JSON-encoded inside a string, or a
            // plain line. Both are read when unambiguous; a line a shell would
            // expand, quote or redirect is refused.
            let command = serde_json::from_str::<Vec<String>>(line)
                .ok()
                .or_else(|| plain_words(line));
            let Some(command) = command else {
                return Err(ToolError::new(
                    "invalid exec arguments: this line needs a shell to read its quotes, pipes \
                     or redirects, and exec runs no shell. Send it to the shell tool, or send \
                     command as an array with one string per argument, such as \
                     [\"cargo\", \"test\", \"--lib\"].",
                ));
            };
            arguments["command"] = Value::from(command);
        }
        let request: Self = serde_json::from_value(arguments)
            .map_err(|error| ToolError::new(format!("invalid exec arguments: {error}")))?;
        if request.command.is_empty() {
            return Err(ToolError::new("command must hold at least a program"));
        }
        Ok(request)
    }
}

/// Splits a line on whitespace when a shell would split it the same way.
fn plain_words(line: &str) -> Option<Vec<String>> {
    const SHELL: &[char] = &[
        '\'', '"', '\\', '$', '`', '|', '&', ';', '<', '>', '*', '?', '(', ')', '[', ']', '{', '}',
        '~', '#', '!',
    ];
    let words: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
    let assigns = words.first().is_some_and(|program| program.contains('='));
    (!words.is_empty() && !assigns && !line.contains(SHELL)).then_some(words)
}

#[derive(Debug, Serialize)]
pub(crate) struct ExecOutput {
    pub(crate) command: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) timed_out: bool,
    /// Kept apart from stderr. Merging them loses which stream said what, and a
    /// tool whose output cannot tell a result from a warning teaches a model to
    /// guess.
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) truncated: bool,
    /// Protected paths this command changed, which have been put back.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) restored_protected_paths: Vec<String>,
    /// Why the command failed, when the output alone would mislead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) note: Option<&'static str>,
}

pub(crate) fn json_output<T: Serialize>(value: &T) -> Result<ToolOutput, ToolError> {
    serde_json::to_string(value)
        .map(ToolOutput::success)
        .map_err(|error| ToolError::new(format!("cannot encode exec output: {error}")))
}

pub(crate) use dino_core::text::capped_head;
pub(crate) use dino_core::text::capped_output as capped;
