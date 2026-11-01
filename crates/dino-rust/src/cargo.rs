//! The `cargo` tool: a closed set of subcommands, parsed output, no shell.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_core::text::capped_output as capped;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use dino_sandbox::Sandbox;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::diagnostics;
use crate::diagnostics::Diagnostic;
use crate::diagnostics::DiagnosticCounts;
use crate::diagnostics::ParsedDiagnostics;
use crate::trim;
use crate::trim::Source;
use dino_exec::process;

/// The longest a package name or test filter may be.
const MAX_ARGUMENT_BYTES: usize = 128;

/// Appended to the approval subject of a command that reaches the network.
const FETCH_NOTE: &str = " (network: HTTPS, Cargo cache)";

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
        // Checked per call, not here: a new project starts with no manifest,
        // and a session without this tool cannot fetch the crates it needs.
        let manifest = root.join("Cargo.toml");
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
            CargoCommand::Check | CargoCommand::Clippy | CargoCommand::Build => {
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
            CargoCommand::Fetch => arguments.push("fetch".to_owned()),
            CargoCommand::Add => {
                arguments.push("add".to_owned());
                arguments.extend(request.crates.iter().cloned());
                if !request.features.is_empty() {
                    arguments.extend(["--features".to_owned(), request.features.join(",")]);
                }
                if request.dev {
                    arguments.push("--dev".to_owned());
                }
                if let Some(package) = &request.package {
                    arguments.extend(["-p".to_owned(), package.clone()]);
                }
            }
            CargoCommand::Fmt | CargoCommand::FmtCheck => {
                arguments.push("fmt".to_owned());
                match &request.package {
                    Some(package) => arguments.extend(["-p".to_owned(), package.clone()]),
                    None => arguments.push("--all".to_owned()),
                }
                if request.command == CargoCommand::FmtCheck {
                    arguments.push("--check".to_owned());
                }
            }
            CargoCommand::Run => {
                arguments.push("run".to_owned());
                if let Some(package) = &request.package {
                    arguments.extend(["-p".to_owned(), package.clone()]);
                }
            }
        }
        arguments.extend(["--manifest-path".to_owned(), manifest]);
        // A confined child has no network, and without --offline Cargo fails on
        // DNS in a way that reads as a network fault rather than as policy.
        // `cargo fmt` does not accept the flag and does not need it.
        if self.sandbox.denies_network()
            && !matches!(request.command, CargoCommand::Fmt | CargoCommand::FmtCheck)
            && !request.command.fetches()
        {
            arguments.push("--offline".to_owned());
        }
        if let Some(filter) = &request.filter
            && request.command == CargoCommand::Test
        {
            arguments.push(filter.clone());
        }
        if request.command == CargoCommand::Run && !request.args.is_empty() {
            arguments.push("--".to_owned());
            arguments.extend(request.args.iter().cloned());
        }
        arguments
    }

    /// The program a request runs and its arguments: Cargo, or rustfmt for a
    /// format held to the files it names.
    fn command_line(&self, request: &CargoRequest) -> Result<(String, Vec<String>), ToolError> {
        if request.files.is_empty() {
            return Ok((Self::program(), self.arguments(request)));
        }
        let mut arguments = Vec::new();
        if let Some(edition) = self.edition_for(&request.files)? {
            arguments.extend(["--edition".to_owned(), edition]);
        }
        if request.command == CargoCommand::FmtCheck {
            arguments.push("--check".to_owned());
        }
        arguments.extend(request.files.iter().cloned());
        Ok(("rustfmt".to_owned(), arguments))
    }

    /// The edition rustfmt must parse `files` with, from the manifest of the
    /// package each belongs to. None is Cargo's default, which is rustfmt's.
    fn edition_for(&self, files: &[String]) -> Result<Option<String>, ToolError> {
        let mut editions = std::collections::BTreeSet::new();
        for file in files {
            editions.insert(self.edition_of(Path::new(file))?);
        }
        if editions.len() > 1 {
            return Err(ToolError::new(
                "these files belong to packages of different editions; format each edition's \
                 files in a call of its own",
            ));
        }
        Ok(editions.pop_first().flatten())
    }

    fn edition_of(&self, file: &Path) -> Result<Option<String>, ToolError> {
        let path = self.root.join(file);
        for directory in path.ancestors().skip(1) {
            if !directory.starts_with(&self.root) {
                break;
            }
            let Some(package) = read_manifest(&directory.join("Cargo.toml"))?
                .and_then(|manifest| manifest.get("package").cloned())
            else {
                continue;
            };
            return Ok(match package.get("edition") {
                Some(toml::Value::String(edition)) => Some(edition.clone()),
                Some(toml::Value::Table(_)) => read_manifest(&self.manifest)?.and_then(|root| {
                    root.get("workspace")?
                        .get("package")?
                        .get("edition")?
                        .as_str()
                        .map(ToOwned::to_owned)
                }),
                _ => None,
            });
        }
        Ok(None)
    }

    /// The command as a person would read it, and as a session rule is keyed.
    ///
    /// The manifest path is shown relative to the root because it is invariant
    /// for the session; every other argument appears exactly as it will be run.
    fn subject_for(&self, request: &CargoRequest) -> Result<String, ToolError> {
        let (program, arguments) = self.command_line(request)?;
        let subject = if request.files.is_empty() {
            self.subject(&arguments)
        } else {
            format!("{program} {}", arguments.join(" "))
        };
        Ok(if request.command.fetches() {
            subject + FETCH_NOTE
        } else {
            subject
        })
    }

    /// The sandbox a command runs in: the session's, or for a fetch the same
    /// confinement with HTTPS and Cargo's cache opened.
    fn sandbox_for(&self, command: CargoCommand) -> Result<Arc<dyn Sandbox>, ToolError> {
        if !command.fetches() {
            return Ok(Arc::clone(&self.sandbox));
        }
        let cache = cargo_cache()?;
        self.sandbox
            .for_fetching(&cache)
            .map(Arc::from)
            .ok_or_else(|| {
                ToolError::new(format!(
                    "this sandbox ({}) cannot allow fetching; run `cargo fetch` yourself",
                    self.sandbox.label()
                ))
            })
    }

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
        self.require_manifest()?;
        let request = CargoRequest::plain(CargoCommand::Check);
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
        self.require_manifest()?;
        let arguments = self.arguments(&CargoRequest::plain(CargoCommand::Test));
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
        self.subject(&self.arguments(&CargoRequest::plain(CargoCommand::Check)))
    }

    fn require_manifest(&self) -> Result<(), ToolError> {
        if self.manifest.is_file() {
            return Ok(());
        }
        Err(ToolError::new(
            "there is no Cargo.toml at the workspace root yet. Write one with write_file, \
             declaring the dependencies, then call cargo with fetch to download them.",
        ))
    }

    async fn run(&self, request: CargoRequest) -> Result<ToolOutput, ToolError> {
        self.require_manifest()?;
        let (program, arguments) = self.command_line(&request)?;
        let subject = self.subject_for(&request)?;
        let sandbox = self.sandbox_for(request.command)?;
        // A program that does not exit, such as a server, is cut short sooner.
        let timeout = if request.command == CargoCommand::Run {
            self.limits.timeout.min(Duration::from_secs(120))
        } else {
            self.limits.timeout
        };
        let execution = process::run(
            &program,
            &arguments,
            &self.root,
            sandbox.as_ref(),
            self.limits.max_captured_bytes,
            timeout,
        )
        .await?;

        if execution.timed_out {
            return json_output(&CargoOutput {
                command: subject,
                exit_code: None,
                timed_out: true,
                counts: DiagnosticCounts::default(),
                diagnostics: Vec::new(),
                dropped_diagnostics: 0,
                packages: None,
                added: Vec::new(),
                output: format!("no result: killed after {} seconds", timeout.as_secs()),
                trimmed: None,
                truncated: execution.truncated,
                restored_protected_paths: execution.restored,
            });
        }

        let mut output = CargoOutput {
            command: subject,
            exit_code: execution.exit_code,
            timed_out: false,
            counts: DiagnosticCounts::default(),
            diagnostics: Vec::new(),
            dropped_diagnostics: 0,
            packages: None,
            added: Vec::new(),
            output: String::new(),
            trimmed: None,
            truncated: execution.truncated,
            restored_protected_paths: execution.restored,
        };

        match request.command {
            CargoCommand::Check | CargoCommand::Clippy | CargoCommand::Build => {
                let parsed = diagnostics::parse(&execution.stdout);
                output.counts = parsed.counts;
                let (kept, dropped) = diagnostics::cap(
                    parsed.diagnostics,
                    self.limits.max_diagnostics,
                    self.limits.max_rendered_bytes,
                );
                output.diagnostics = kept;
                output.dropped_diagnostics = dropped;
                if !request.full {
                    let shortened = diagnostics::shorten(&mut output.diagnostics);
                    output.trimmed = (shortened > 0).then(|| {
                        format!(
                            "{shortened} repeated or later diagnostics without their rendering; \
                             pass full for all of it"
                        )
                    });
                }
                // Cargo's own failures, as opposed to the compiler's, arrive on
                // stderr and are not JSON. They count as one error, so `errors: 0`
                // never describes a build that failed.
                if output.counts.errors == 0 && execution.exit_code != Some(0) {
                    output.counts.errors = 1;
                    output.output = self.text(&execution.stderr, &request, &mut output);
                }
            }
            CargoCommand::Metadata => {
                output.packages = Some(summarise_metadata(&execution.stdout, &self.root)?);
                if execution.exit_code != Some(0) {
                    output.output = self.text(&execution.stderr, &request, &mut output);
                }
            }
            CargoCommand::Test
            | CargoCommand::Fmt
            | CargoCommand::FmtCheck
            | CargoCommand::Run
            | CargoCommand::Fetch
            | CargoCommand::Add => {
                // Neither has a machine-readable stream worth parsing yet, so
                // the captured text is returned as text and labelled as such.
                let combined = format!("{}{}", execution.stdout, execution.stderr);
                output.output = self.text(&combined, &request, &mut output);
                if request.command == CargoCommand::Add {
                    output.added = added(&execution.stderr);
                }
            }
        }
        json_output(&output)
    }

    /// Cargo's text, trimmed unless the request asked for all of it, then capped.
    fn text(&self, text: &str, request: &CargoRequest, output: &mut CargoOutput) -> String {
        let text = if request.full {
            text.to_owned()
        } else {
            let source = match request.command {
                CargoCommand::Test => Source::Test,
                CargoCommand::FmtCheck => Source::FormatCheck,
                _ => Source::Other,
            };
            let trimmed = trim::trim(text, &self.root, source);
            output.trimmed = trimmed.summary();
            trimmed.text
        };
        output.truncated |= text.len() > self.limits.max_output_bytes;
        capped(&text, self.limits.max_output_bytes)
    }
}

impl ToolRuntime for CargoTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "cargo".into(),
            description: "Run a Cargo command and return parsed diagnostics. None is read-only: \
                 they run build scripts and test code. All run offline except fetch, which \
                 downloads the locked dependencies, and add, which adds crates to Cargo.toml. \
                 fmt formats the whole workspace, existing tests included, unless files names \
                 the files to format; fmt_check only reports. run is for programs that exit, \
                 and stops them after two minutes. Output leaves out progress, passing tests \
                 and repeated diagnostics; trimmed says what went, and full returns it all."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "enum": ["metadata", "check", "build", "clippy", "test", "run", "fmt", "fmt_check", "fetch", "add"]
                    },
                    "args": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Only for run: arguments passed to the program after --."
                    },
                    "crates": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Only for add: crate names, each optionally name@version."
                    },
                    "features": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Only for add: features to enable."
                    },
                    "dev": {
                        "type": "boolean",
                        "description": "Only for add: add as a dev-dependency."
                    },
                    "package": {
                        "type": "string",
                        "minLength": 1,
                        "description": "One workspace member. Defaults to the whole workspace."
                    },
                    "filter": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Only for test: run tests whose name contains this."
                    },
                    "files": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Only for fmt and fmt_check: the .rs files to format, leaving every other file as it is."
                    },
                    "full": {
                        "type": "boolean",
                        "description": "Return Cargo's output untrimmed: progress, passing tests and every diagnostic rendered in full."
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
        Ok(ToolAction::process(self.subject_for(&request)?))
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
    FmtCheck,
    Build,
    Run,
    Fetch,
    Add,
}

impl CargoCommand {
    fn fetches(self) -> bool {
        matches!(self, Self::Fetch | Self::Add)
    }

    fn subcommand(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::Check => "check",
            Self::Clippy => "clippy",
            Self::Test => "test",
            Self::Fmt | Self::FmtCheck => "fmt",
            Self::Build => "build",
            Self::Run => "run",
            Self::Fetch => "fetch",
            Self::Add => "add",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoRequest {
    command: CargoCommand,
    package: Option<String>,
    filter: Option<String>,
    #[serde(default)]
    crates: Vec<String>,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    dev: bool,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    full: bool,
}

impl CargoRequest {
    /// A whole-workspace request with no options, for the harness's own calls.
    fn plain(command: CargoCommand) -> Self {
        Self {
            command,
            package: None,
            filter: None,
            crates: Vec::new(),
            features: Vec::new(),
            dev: false,
            args: Vec::new(),
            files: Vec::new(),
            full: false,
        }
    }

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
        if request.command == CargoCommand::Add {
            if request.crates.is_empty() {
                return Err(ToolError::new("add needs at least one crate"));
            }
            for name in &request.crates {
                validate_spec("crate", name, "@^~=<>*.+")?;
            }
            for feature in &request.features {
                validate_spec("feature", feature, "/+.")?;
            }
        } else if !request.crates.is_empty() || !request.features.is_empty() || request.dev {
            return Err(ToolError::new("crates, features and dev apply only to add"));
        }
        if request.command != CargoCommand::Run && !request.args.is_empty() {
            return Err(ToolError::new("args apply only to run"));
        }
        if !request.files.is_empty() {
            if !matches!(request.command, CargoCommand::Fmt | CargoCommand::FmtCheck) {
                return Err(ToolError::new("files apply only to fmt and fmt_check"));
            }
            if request.package.is_some() {
                return Err(ToolError::new("give files or package, not both"));
            }
            for file in &request.files {
                validate_file(file)?;
            }
        }
        if request
            .args
            .iter()
            .any(|argument| argument.chars().any(char::is_control))
        {
            return Err(ToolError::new("an argument holds a control character"));
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

/// Accepts a `.rs` path relative to the workspace root that stays inside it.
fn validate_file(file: &str) -> Result<(), ToolError> {
    let path = Path::new(file);
    let inside = path
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir));
    if file.starts_with('-')
        || !inside
        || file.chars().any(char::is_control)
        || path.extension().is_none_or(|extension| extension != "rs")
    {
        return Err(ToolError::new(format!(
            "files must be .rs paths relative to the workspace root and inside it: {file:?}"
        )));
    }
    Ok(())
}

/// A manifest as a table, or None when there is no file to read.
fn read_manifest(path: &Path) -> Result<Option<toml::Table>, ToolError> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|error| ToolError::new(format!("cannot read {}: {error}", path.display())))
}

/// As [`validate`], allowing `extra` characters as well, for version
/// requirements and feature paths.
fn validate_spec(field: &str, value: &str, extra: &str) -> Result<(), ToolError> {
    if value.is_empty() || value.len() > MAX_ARGUMENT_BYTES || value.starts_with('-') {
        return Err(ToolError::new(format!(
            "{field} must be 1 to {MAX_ARGUMENT_BYTES} bytes and not begin with a hyphen: \
             {value:?}"
        )));
    }
    if !value.chars().all(|character| {
        character.is_ascii_alphanumeric() || "_-".contains(character) || extra.contains(character)
    }) {
        return Err(ToolError::new(format!(
            "{field} holds a character Cargo would not expect: {value:?}"
        )));
    }
    Ok(())
}

/// Where Cargo keeps what it downloads, created if absent so it can be granted.
fn cargo_cache() -> Result<Vec<PathBuf>, ToolError> {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .ok_or_else(|| ToolError::new("neither CARGO_HOME nor HOME is set"))?;
    let directories = [home.join("registry"), home.join("git")];
    // Cargo's lock files, which it opens for writing before it fetches.
    let locks = [
        home.join(".package-cache"),
        home.join(".package-cache-mutate"),
    ];
    for directory in &directories {
        std::fs::create_dir_all(directory).map_err(|error| {
            ToolError::new(format!("cannot create {}: {error}", directory.display()))
        })?;
    }
    for lock in &locks {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(lock)
            .map_err(|error| ToolError::new(format!("cannot open {}: {error}", lock.display())))?;
    }
    Ok(directories.into_iter().chain(locks).collect())
}

/// `cargo add`'s own lines for what it added, as `name version (section)`.
///
/// Its other `Adding` lines, without a section, are the lockfile's transitive
/// packages, and are left out.
fn added(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("Adding ")?;
            let (name, rest) = rest.split_once(" v")?;
            let (version, section) = rest.split_once(" to ")?;
            Some(format!("{name} {version} ({section})"))
        })
        .collect()
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
    /// What `add` put in the manifest, at the versions it resolved, which are
    /// the APIs the code must be written against.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    added: Vec<String>,
    output: String,
    /// What was cut from `output` and `diagnostics` to keep them short.
    #[serde(skip_serializing_if = "Option::is_none")]
    trimmed: Option<String>,
    truncated: bool,
    /// Protected paths this command changed, which have been put back.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    restored_protected_paths: Vec<String>,
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

#[cfg(test)]
mod tests {
    use super::added;

    #[test]
    fn add_reports_the_versions_it_chose_and_not_the_lockfile() {
        let stderr = "    Updating crates.io index\n      Adding axum v0.8.9 to dependencies\n             Features:\n             + json\n      Adding reqwest v0.13.5 to dev-dependencies\n     Locking 103 packages to latest Rust 1.98.1 compatible versions\n      Adding aws-lc-rs v1.18.2\n";
        assert_eq!(
            added(stderr),
            [
                "axum 0.8.9 (dependencies)",
                "reqwest 0.13.5 (dev-dependencies)"
            ]
        );
    }
}
