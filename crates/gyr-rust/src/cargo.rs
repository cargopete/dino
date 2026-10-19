//! The `cargo` tool: a closed set of subcommands, parsed output, no shell.

use std::path::Path;
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
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::diagnostics;
use crate::diagnostics::Diagnostic;
use crate::diagnostics::DiagnosticCounts;
use crate::diagnostics::ParsedDiagnostics;
use gyr_exec::process;

/// The longest a package name or test filter may be.
const MAX_ARGUMENT_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CargoLimits {
    pub max_diagnostics: usize,
    pub max_rendered_bytes: usize,
    pub max_output_bytes: usize,
    pub max_captured_bytes: usize,
    pub timeout: Duration,
}

impl Default for CargoLimits {
    fn default() -> Self {
        Self {
            max_diagnostics: 50,
            max_rendered_bytes: 32 * 1_024,
            max_output_bytes: 32 * 1_024,
            // What is read from the pipes before the rest is discarded. Larger
            // than what is returned, because diagnostics are counted from the
            // whole stream even when the returned list is capped.
            max_captured_bytes: 8 * 1_024 * 1_024,
            timeout: Duration::from_secs(600),
        }
    }
}

#[derive(Debug)]
pub struct CargoTool {
    root: PathBuf,
    manifest: PathBuf,
    limits: CargoLimits,
    sandbox: Arc<dyn Sandbox>,
}

/// The outcome of [`CargoTool::run_tests`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestRun {
    pub passed: bool,
    /// Result lines and the first panics or errors, for a failure message.
    pub summary: String,
}

impl CargoTool {
    /// Creates the tool for one workspace root.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be canonicalised or holds no
    /// `Cargo.toml`.
    pub fn new(
        root: impl AsRef<Path>,
        limits: CargoLimits,
        sandbox: Arc<dyn Sandbox>,
    ) -> Result<Self, ToolError> {
        let root = std::fs::canonicalize(root.as_ref()).map_err(|error| {
            ToolError::new(format!(
                "cannot resolve workspace root {}: {error}",
                root.as_ref().display()
            ))
        })?;
        let manifest = root.join("Cargo.toml");
        if !manifest.is_file() {
            return Err(ToolError::new(format!(
                "no Cargo.toml at the workspace root: {}",
                root.display()
            )));
        }
        Ok(Self {
            root,
            manifest,
            limits,
            sandbox,
        })
    }

    /// The program to run. Cargo sets `CARGO` for its own child processes, so
    /// a test run picks up the same toolchain that is running the test.
    fn program() -> String {
        std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
    }

    /// Builds the argument vector for a validated request.
    ///
    /// The manifest path is always explicit. Cargo otherwise searches parent
    /// directories for a manifest, and a tool confined to a workspace that then
    /// built its grandparent would undo the whole point of the confinement.
    fn arguments(&self, request: &CargoRequest) -> Vec<String> {
        let manifest = self.manifest.display().to_string();
        let scope = |arguments: &mut Vec<String>| match &request.package {
            Some(package) => arguments.extend(["-p".to_owned(), package.clone()]),
            None => arguments.push("--workspace".to_owned()),
        };

        let mut arguments = Vec::new();
        match request.command {
            CargoCommand::Metadata => arguments.extend([
                "metadata".to_owned(),
                "--format-version".to_owned(),
                "1".to_owned(),
                "--no-deps".to_owned(),
            ]),
            CargoCommand::Check | CargoCommand::Clippy => {
                arguments.push(request.command.subcommand().to_owned());
                scope(&mut arguments);
                arguments.extend([
                    "--all-targets".to_owned(),
                    "--message-format=json".to_owned(),
                ]);
            }
            CargoCommand::Test => {
                arguments.push("test".to_owned());
                scope(&mut arguments);
            }
            CargoCommand::Fmt => {
                arguments.push("fmt".to_owned());
                match &request.package {
                    Some(package) => arguments.extend(["-p".to_owned(), package.clone()]),
                    None => arguments.push("--all".to_owned()),
                }
                arguments.push("--check".to_owned());
            }
        }
        arguments.extend(["--manifest-path".to_owned(), manifest]);
        // A confined child has no network, and without --offline Cargo fails on
        // DNS in a way that reads as a network fault rather than as policy.
        // `cargo fmt` does not accept the flag and does not need it.
        if self.sandbox.denies_network() && request.command != CargoCommand::Fmt {
            arguments.push("--offline".to_owned());
        }
        if let Some(filter) = &request.filter
            && request.command == CargoCommand::Test
        {
            arguments.push(filter.clone());
        }
        arguments
    }

    /// The command as a person would read it, and as a session rule is keyed.
    ///
    /// The manifest path is shown relative to the root because it is invariant
    /// for the session; every other argument appears exactly as it will be run.
    fn subject(&self, arguments: &[String]) -> String {
        let manifest = self.manifest.display().to_string();
        let shown: Vec<&str> = arguments
            .iter()
            .map(|argument| {
                if argument == &manifest {
                    "Cargo.toml"
                } else {
                    argument.as_str()
                }
            })
            .collect();
        format!("cargo {}", shown.join(" "))
    }

    /// Runs `cargo check` and returns the parsed diagnostics.
    ///
    /// The gate needs the diagnostic set rather than the rendered tool output,
    /// and it needs it from exactly the same invocation the `cargo` tool would
    /// make, so a verdict and a `cargo check` result cannot disagree.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the child cannot be run, and reports a
    /// timeout as an error because a gate has no honest verdict without a
    /// complete diagnostic set.
    pub async fn parsed_check(&self) -> Result<ParsedDiagnostics, ToolError> {
        let request = CargoRequest {
            command: CargoCommand::Check,
            package: None,
            filter: None,
        };
        let arguments = self.arguments(&request);
        let execution = process::run(
            &Self::program(),
            &arguments,
            &self.root,
            self.sandbox.as_ref(),
            self.limits.max_captured_bytes,
            self.limits.timeout,
        )
        .await?;
        if execution.timed_out {
            return Err(ToolError::new(format!(
                "cargo check was killed after {} seconds, so there is no diagnostic set to \
                 compare",
                self.limits.timeout.as_secs()
            )));
        }
        let parsed = diagnostics::parse(&execution.stdout);
        // Cargo failing without emitting a single compiler message means it did
        // not get as far as the compiler. Reporting zero errors there would be
        // the absent-data-as-healthy failure this project keeps refusing.
        if parsed.counts.errors == 0 && execution.exit_code != Some(0) {
            return Err(ToolError::new(format!(
                "cargo check failed before compiling: {}",
                capped(execution.stderr.trim(), self.limits.max_output_bytes)
            )));
        }
        Ok(parsed)
    }

    /// Runs `cargo test` over the whole workspace, for a harness rather than a
    /// model: the result is a verdict and the tail of the output, not a tool
    /// result.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the child cannot be run.
    pub async fn run_tests(&self) -> Result<TestRun, ToolError> {
        let arguments = self.arguments(&CargoRequest {
            command: CargoCommand::Test,
            package: None,
            filter: None,
        });
        let execution = process::run(
            &Self::program(),
            &arguments,
            &self.root,
            self.sandbox.as_ref(),
            self.limits.max_captured_bytes,
            self.limits.timeout,
        )
        .await?;
        if execution.timed_out {
            return Ok(TestRun {
                passed: false,
                summary: format!("killed after {} seconds", self.limits.timeout.as_secs()),
            });
        }
        let passed = execution.exit_code == Some(0);
        let output = format!("{}\n{}", execution.stdout, execution.stderr);
        let summary = output
            .lines()
            .filter(|line| {
                line.starts_with("test result:")
                    || line.contains("panicked at")
                    || line.starts_with("error")
            })
            .take(6)
            .collect::<Vec<_>>()
            .join(" | ");
        Ok(TestRun { passed, summary })
    }

    /// The Cargo invocation the gate will make, for the approval subject.
    #[must_use]
    pub fn check_subject(&self) -> String {
        self.subject(&self.arguments(&CargoRequest {
            command: CargoCommand::Check,
            package: None,
            filter: None,
        }))
    }

    async fn run(&self, request: CargoRequest) -> Result<ToolOutput, ToolError> {
        let arguments = self.arguments(&request);
        let execution = process::run(
            &Self::program(),
            &arguments,
            &self.root,
            self.sandbox.as_ref(),
            self.limits.max_captured_bytes,
            self.limits.timeout,
        )
        .await?;

        if execution.timed_out {
            return json_output(&CargoOutput {
                command: self.subject(&arguments),
                exit_code: None,
                timed_out: true,
                counts: DiagnosticCounts::default(),
                diagnostics: Vec::new(),
                dropped_diagnostics: 0,
                packages: None,
                output: format!(
                    "no result: killed after {} seconds",
                    self.limits.timeout.as_secs()
                ),
                truncated: execution.truncated,
            });
        }

        let mut output = CargoOutput {
            command: self.subject(&arguments),
            exit_code: execution.exit_code,
            timed_out: false,
            counts: DiagnosticCounts::default(),
            diagnostics: Vec::new(),
            dropped_diagnostics: 0,
            packages: None,
            output: String::new(),
            truncated: execution.truncated,
        };

        match request.command {
            CargoCommand::Check | CargoCommand::Clippy => {
                let parsed = diagnostics::parse(&execution.stdout);
                output.counts = parsed.counts;
                let (kept, dropped) = diagnostics::cap(
                    parsed.diagnostics,
                    self.limits.max_diagnostics,
                    self.limits.max_rendered_bytes,
                );
                output.diagnostics = kept;
                output.dropped_diagnostics = dropped;
                // Cargo's own failures, as opposed to the compiler's, arrive on
                // stderr and are not JSON. They matter when nothing parsed.
                if output.counts.errors == 0 && execution.exit_code != Some(0) {
                    output.output = capped(&execution.stderr, self.limits.max_output_bytes);
                }
            }
            CargoCommand::Metadata => {
                output.packages = Some(summarise_metadata(&execution.stdout, &self.root)?);
                if execution.exit_code != Some(0) {
                    output.output = capped(&execution.stderr, self.limits.max_output_bytes);
                }
            }
            CargoCommand::Test | CargoCommand::Fmt => {
                // Neither has a machine-readable stream worth parsing yet, so
                // the captured text is returned as text and labelled as such.
                let combined = format!("{}{}", execution.stdout, execution.stderr);
                output.output = capped(&combined, self.limits.max_output_bytes);
                output.truncated |= combined.len() > self.limits.max_output_bytes;
            }
        }
        json_output(&output)
    }
}

impl ToolRuntime for CargoTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "cargo".into(),
            description: format!(
                "Run one of a fixed set of Cargo commands in the workspace and return parsed \
                 diagnostics. No subcommand is read-only: check and clippy run build scripts, \
                 and test runs test code. Containment: {}.",
                self.sandbox.label()
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "enum": ["metadata", "check", "clippy", "test", "fmt"]
                    },
                    "package": {
                        "type": "string",
                        "description": "One workspace member. Defaults to the whole workspace."
                    },
                    "filter": {
                        "type": "string",
                        "description": "Only for test: run tests whose name contains this."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }]
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if call.name != "cargo" {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        let request = CargoRequest::parse(&call.arguments)?;
        Ok(ToolAction::process(self.subject(&self.arguments(&request))))
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let request = if call.name == "cargo" {
            CargoRequest::parse(&call.arguments)
        } else {
            Err(ToolError::new(format!("unknown tool {:?}", call.name)))
        };
        Box::pin(async move { self.run(request?).await })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CargoCommand {
    Metadata,
    Check,
    Clippy,
    Test,
    Fmt,
}

impl CargoCommand {
    fn subcommand(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::Check => "check",
            Self::Clippy => "clippy",
            Self::Test => "test",
            Self::Fmt => "fmt",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoRequest {
    command: CargoCommand,
    package: Option<String>,
    filter: Option<String>,
}

impl CargoRequest {
    fn parse(arguments: &Value) -> Result<Self, ToolError> {
        let request: Self = serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::new(format!("invalid cargo arguments: {error}")))?;
        if let Some(package) = &request.package {
            validate("package", package)?;
        }
        if let Some(filter) = &request.filter {
            validate("filter", filter)?;
            if request.command != CargoCommand::Test {
                return Err(ToolError::new("filter applies only to the test command"));
            }
        }
        Ok(request)
    }
}

/// Rejects anything that is not plainly a package name or a test path.
///
/// The point is not tidiness. A value beginning with a hyphen would arrive at
/// Cargo as a flag, and a value containing whitespace would arrive as two
/// arguments, so this is where a closed argument surface is actually closed.
fn validate(field: &str, value: &str) -> Result<(), ToolError> {
    if value.is_empty() || value.len() > MAX_ARGUMENT_BYTES {
        return Err(ToolError::new(format!(
            "{field} must be between 1 and {MAX_ARGUMENT_BYTES} bytes"
        )));
    }
    if value.starts_with('-') {
        return Err(ToolError::new(format!(
            "{field} must not begin with a hyphen: {value:?}"
        )));
    }
    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "_-.:".contains(character))
    {
        return Err(ToolError::new(format!(
            "{field} may hold only letters, digits and _-.: characters: {value:?}"
        )));
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct CargoOutput {
    command: String,
    exit_code: Option<i32>,
    timed_out: bool,
    counts: DiagnosticCounts,
    diagnostics: Vec<Diagnostic>,
    /// Diagnostics counted but not returned, so a capped list never reads as a
    /// complete one.
    dropped_diagnostics: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    packages: Option<Vec<PackageSummary>>,
    output: String,
    truncated: bool,
}

#[derive(Debug, Serialize)]
struct PackageSummary {
    name: String,
    version: String,
    manifest_path: String,
    edition: Option<String>,
    rust_version: Option<String>,
    targets: Vec<String>,
}

/// Reduces `cargo metadata` to the workspace members and their targets.
///
/// The raw document is routinely larger than the file the agent was asked
/// about, and an agent's context is not a good place to keep it.
fn summarise_metadata(stdout: &str, root: &Path) -> Result<Vec<PackageSummary>, ToolError> {
    let document: Value = serde_json::from_str(stdout.trim())
        .map_err(|error| ToolError::new(format!("cannot parse cargo metadata: {error}")))?;
    let packages = document
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::new("cargo metadata held no packages"))?;

    Ok(packages
        .iter()
        .map(|package| {
            let manifest_path = package
                .get("manifest_path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            PackageSummary {
                name: string_field(package, "name"),
                version: string_field(package, "version"),
                manifest_path: Path::new(manifest_path).strip_prefix(root).map_or_else(
                    |_| manifest_path.to_owned(),
                    |path| path.display().to_string(),
                ),
                edition: package
                    .get("edition")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                rust_version: package
                    .get("rust_version")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                targets: package
                    .get("targets")
                    .and_then(Value::as_array)
                    .map(|targets| {
                        targets
                            .iter()
                            .map(|target| string_field(target, "name"))
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        })
        .collect())
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn json_output<T: Serialize>(value: &T) -> Result<ToolOutput, ToolError> {
    serde_json::to_string(value)
        .map(ToolOutput::success)
        .map_err(|error| ToolError::new(format!("cannot encode cargo output: {error}")))
}

fn capped(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (truncated)", &value[..end])
}
