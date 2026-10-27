//! The Rust diagnostic gate.
//!
//! A multi-site Rust change passes through a red state, so the question is not
//! "does it compile" after every edit but "is this getting better". The gate
//! answers that from the diagnostic set, and refuses to call a batch done when
//! it has stopped improving or when nothing in the workspace actually moved.
//!
//! It measures and reports. It does not roll back, because rolling back
//! arbitrary edits means keeping a shadow copy of the workspace, which is a
//! worse version of git that will drift from the real one. RFC-0011 section 2
//! argues that at length.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use dancho_core::ToolError;
use dancho_core::ToolFuture;
use dancho_core::ToolRuntime;
use dancho_protocol::ToolAction;
use dancho_protocol::ToolCall;
use dancho_protocol::ToolDefinition;
use dancho_protocol::ToolOutput;
use dancho_sandbox::Sandbox;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

use crate::CargoLimits;
use crate::CargoTool;
use crate::diagnostics::Diagnostic;
use crate::diagnostics::DiagnosticCounts;
use crate::diagnostics::ParsedDiagnostics;

/// Matches RFC-0005's search cap, because it is the same walk.
const MAX_FINGERPRINTED_FILES: usize = 20_000;

/// How many consecutive non-improving checks end a batch.
///
/// Two, not one: a single stalled check is an ordinary step in a multi-site
/// change where the next edit unblocks a cascade.
const EXHAUSTED_AFTER: u32 = 2;

/// What identifies one mistake across edits.
///
/// Line and column are deliberately absent. An edit above a diagnostic shifts
/// its line, and a batch that fixed nothing would otherwise look like one that
/// resolved eleven diagnostics and introduced eleven others.
type Identity = (String, Option<String>, Option<String>, String);

fn identity(diagnostic: &Diagnostic) -> Identity {
    (
        diagnostic.level.clone(),
        diagnostic.code.clone(),
        diagnostic.file.clone(),
        diagnostic.message.clone(),
    )
}

/// A diagnostic set reduced to identities and their counts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiagnosticSet {
    errors: BTreeMap<Identity, usize>,
    counts: DiagnosticCounts,
}

impl DiagnosticSet {
    #[must_use]
    pub fn from_parsed(parsed: &ParsedDiagnostics) -> Self {
        let mut errors = BTreeMap::new();
        for diagnostic in &parsed.diagnostics {
            if diagnostic.level == "error" {
                *errors.entry(identity(diagnostic)).or_insert(0) += 1;
            }
        }
        Self {
            errors,
            counts: parsed.counts,
        }
    }

    /// How many errors went away, and how many arrived.
    ///
    /// Counted per identity rather than by presence. Two mismatched-types
    /// errors in one file share an identity, so a first draft that compared
    /// key sets called fixing one of them "stalled". Multiplicity is part of
    /// the measurement; line numbers still are not.
    ///
    /// Warnings decide nothing: a batch that traded an error for a warning has
    /// made progress.
    fn delta(&self, previous: &Self) -> (usize, usize) {
        let mut resolved = 0;
        let mut introduced = 0;
        for (key, before) in &previous.errors {
            let now = self.errors.get(key).copied().unwrap_or(0);
            resolved += before.saturating_sub(now);
        }
        for (key, now) in &self.errors {
            let before = previous.errors.get(key).copied().unwrap_or(0);
            introduced += now.saturating_sub(before);
        }
        (resolved, introduced)
    }

    /// Identities whose count here exceeds their count there, for display.
    ///
    /// The surplus is shown, so three of one error against one of the same
    /// reads as two rather than as a repeated line.
    fn identities(&self, other: &Self) -> Vec<String> {
        self.errors
            .iter()
            .filter_map(|((level, code, file, message), now)| {
                let before =
                    other
                        .errors
                        .get(&(level.clone(), code.clone(), file.clone(), message.clone()));
                let surplus = now.saturating_sub(before.copied().unwrap_or(0));
                if surplus == 0 {
                    return None;
                }
                let code = code.as_deref().unwrap_or("-");
                let file = file.as_deref().unwrap_or("-");
                let count = if surplus > 1 {
                    format!(" (x{surplus})")
                } else {
                    String::new()
                };
                Some(format!("{level}[{code}] {file}: {message}{count}"))
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// No errors, and the workspace actually changed.
    Green,
    /// No errors, and nothing changed. Someone else's success.
    Unchanged,
    Improving,
    Regressing,
    Stalled,
    /// Non-improving twice running. Stop and reconsider.
    Exhausted,
}

impl Verdict {
    fn message(self) -> &'static str {
        match self {
            Self::Green => "No errors, and the workspace changed. This batch is done.",
            Self::Unchanged => {
                "The build is clean and no file has changed since the baseline. Either nothing \
                 needed doing, or the baseline was taken after the work: this verdict cannot \
                 see edits made before start was called."
            }
            Self::Improving => "Fewer distinct errors than at the last check. Keep going.",
            Self::Regressing => {
                "More distinct errors than at the last check. The last edits made it worse; \
                 revert them before continuing."
            }
            Self::Stalled => {
                "The last edits changed no distinct error. Try a different approach rather \
                 than the same one again."
            }
            Self::Exhausted => {
                "Two checks running without improvement. Stop editing, revert this batch and \
                 reconsider the approach."
            }
        }
    }
}

/// Everything one batch remembers between calls.
#[derive(Debug)]
struct Batch {
    baseline: DiagnosticSet,
    previous: DiagnosticSet,
    fingerprints: Fingerprints,
    /// Consecutive checks that did not improve.
    stalls: u32,
    last: Option<Report>,
}

#[derive(Debug)]
pub struct GateTool {
    root: PathBuf,
    cargo: CargoTool,
    batch: Mutex<Option<Batch>>,
}

impl GateTool {
    /// Creates the gate for one workspace root.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the root holds no `Cargo.toml`, since a
    /// diagnostic gate over something that is not a Cargo workspace has nothing
    /// to measure.
    pub fn new(
        root: impl AsRef<Path>,
        limits: CargoLimits,
        sandbox: Arc<dyn Sandbox>,
    ) -> Result<Self, ToolError> {
        let cargo = CargoTool::new(root.as_ref(), limits, sandbox)?;
        Ok(Self {
            root: std::fs::canonicalize(root.as_ref()).map_err(|error| {
                ToolError::new(format!(
                    "cannot resolve workspace root {}: {error}",
                    root.as_ref().display()
                ))
            })?,
            cargo,
            batch: Mutex::new(None),
        })
    }

    async fn start(&self) -> Result<ToolOutput, ToolError> {
        let parsed = self.cargo.parsed_check().await?;
        let set = DiagnosticSet::from_parsed(&parsed);
        let fingerprints = Fingerprints::take(&self.root)?;
        let report = Report {
            phase: "start",
            verdict: None,
            baseline: set.counts,
            current: set.counts,
            resolved_since_last_check: 0,
            introduced_since_last_check: 0,
            resolved: Vec::new(),
            introduced: Vec::new(),
            files_changed: 0,
            bytes_changed: 0,
            files_fingerprinted: fingerprints.len(),
            truncated: fingerprints.truncated,
            message: "Baseline recorded. Edit, then check.".to_owned(),
        };
        let mut batch = self.batch.lock().map_err(lock_error)?;
        *batch = Some(Batch {
            baseline: set.clone(),
            previous: set,
            fingerprints,
            stalls: 0,
            last: Some(report.clone()),
        });
        json_output(&report)
    }

    async fn check(&self) -> Result<ToolOutput, ToolError> {
        // Nothing is held across the await; a gate that kept a lock while
        // running Cargo would deadlock the moment two calls overlapped.
        {
            let batch = self.batch.lock().map_err(lock_error)?;
            if batch.is_none() {
                return Err(ToolError::new(
                    "no baseline: call the gate with start before check. A gate that invents \
                     its own baseline always reports improvement.",
                ));
            }
        }

        let parsed = self.cargo.parsed_check().await?;
        let current = DiagnosticSet::from_parsed(&parsed);
        let fingerprints = Fingerprints::take(&self.root)?;

        let mut guard = self.batch.lock().map_err(lock_error)?;
        let batch = guard
            .as_mut()
            .ok_or_else(|| ToolError::new("the batch was cleared while checking"))?;

        let (resolved_since, introduced_since) = current.delta(&batch.previous);
        let change = fingerprints.compare(&batch.fingerprints);

        let verdict = if current.counts.errors == 0 {
            // Withheld rather than guessed where the walk could not see
            // everything, because a gate that cannot see the whole workspace
            // must not claim nothing in it moved.
            if change.any() || fingerprints.truncated {
                Verdict::Green
            } else {
                Verdict::Unchanged
            }
        } else if introduced_since > resolved_since {
            Verdict::Regressing
        } else if resolved_since > introduced_since {
            Verdict::Improving
        } else {
            Verdict::Stalled
        };

        batch.stalls = if verdict == Verdict::Improving {
            0
        } else if matches!(verdict, Verdict::Green | Verdict::Unchanged) {
            batch.stalls
        } else {
            batch.stalls + 1
        };
        let verdict = if batch.stalls >= EXHAUSTED_AFTER {
            Verdict::Exhausted
        } else {
            verdict
        };

        let report = Report {
            phase: "check",
            verdict: Some(verdict),
            baseline: batch.baseline.counts,
            current: current.counts,
            resolved_since_last_check: resolved_since,
            introduced_since_last_check: introduced_since,
            resolved: batch.baseline.identities(&current),
            introduced: current.identities(&batch.baseline),
            files_changed: change.files,
            bytes_changed: change.bytes,
            files_fingerprinted: fingerprints.len(),
            truncated: fingerprints.truncated,
            message: verdict.message().to_owned(),
        };
        batch.previous = current;
        batch.fingerprints = fingerprints;
        batch.last = Some(report.clone());
        drop(guard);
        json_output(&report)
    }

    fn status(&self) -> Result<ToolOutput, ToolError> {
        let batch = self.batch.lock().map_err(lock_error)?;
        match batch.as_ref().and_then(|batch| batch.last.as_ref()) {
            Some(report) => json_output(report),
            None => Err(ToolError::new(
                "no batch has been started, so there is no verdict to report",
            )),
        }
    }
}

impl ToolRuntime for GateTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "gate".into(),
            description: format!(
                "Track whether an edit batch is making measurable progress. Call start BEFORE \
                 your first edit, then check after every few edits. A baseline taken after the \
                 work cannot see that work and will report unchanged, which tells you nothing. \
                 A batch may pass through a state that does not compile; what matters is \
                 whether the distinct error set is shrinking. Verdicts: green, unchanged, \
                 improving, regressing, stalled, exhausted. Runs {}.",
                self.cargo.check_subject()
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["start", "check", "status"]}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }]
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if call.name != "gate" {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        let request = GateRequest::parse(&call.arguments)?;
        Ok(match request.command {
            // Runs nothing, so it is nobody's business but the model's.
            GateCommand::Status => ToolAction::read_only(),
            GateCommand::Start | GateCommand::Check => ToolAction::process(format!(
                "gate {} ({})",
                request.command.name(),
                self.cargo.check_subject()
            )),
        })
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let request = if call.name == "gate" {
            GateRequest::parse(&call.arguments)
        } else {
            Err(ToolError::new(format!("unknown tool {:?}", call.name)))
        };
        Box::pin(async move {
            match request?.command {
                GateCommand::Start => self.start().await,
                GateCommand::Check => self.check().await,
                GateCommand::Status => self.status(),
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GateCommand {
    Start,
    Check,
    Status,
}

impl GateCommand {
    fn name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Check => "check",
            Self::Status => "status",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GateRequest {
    command: GateCommand,
}

impl GateRequest {
    fn parse(arguments: &Value) -> Result<Self, ToolError> {
        serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::new(format!("invalid gate arguments: {error}")))
    }
}

#[derive(Debug, Clone, Serialize)]
struct Report {
    phase: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    verdict: Option<Verdict>,
    baseline: DiagnosticCounts,
    current: DiagnosticCounts,
    resolved_since_last_check: usize,
    introduced_since_last_check: usize,
    /// Distinct errors in the baseline that are gone now.
    resolved: Vec<String>,
    /// Distinct errors absent from the baseline that are present now.
    introduced: Vec<String>,
    files_changed: usize,
    bytes_changed: i64,
    files_fingerprinted: usize,
    truncated: bool,
    message: String,
}

/// SHA-256 of every `.rs` file the workspace's ignore rules admit.
#[derive(Debug, Default)]
struct Fingerprints {
    files: HashMap<String, (String, u64)>,
    truncated: bool,
}

#[derive(Debug, Clone, Copy)]
struct Change {
    files: usize,
    bytes: i64,
}

impl Change {
    fn any(self) -> bool {
        self.files > 0
    }
}

impl Fingerprints {
    /// Walks and hashes.
    ///
    /// Fingerprints rather than an edit count on purpose: a patch that wrote a
    /// file and a later one that wrote it back are two edits and no change, and
    /// the gate should say no change.
    fn take(root: &Path) -> Result<Self, ToolError> {
        let mut walker = WalkBuilder::new(root);
        walker
            .follow_links(false)
            .parents(false)
            .require_git(false)
            .sort_by_file_path(std::cmp::Ord::cmp);

        let mut files = HashMap::new();
        let mut truncated = false;
        for entry in walker.build() {
            let entry = entry
                .map_err(|error| ToolError::new(format!("cannot walk the workspace: {error}")))?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if entry.path().extension().and_then(std::ffi::OsStr::to_str) != Some("rs") {
                continue;
            }
            if files.len() == MAX_FINGERPRINTED_FILES {
                truncated = true;
                break;
            }
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            let name = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .into_owned();
            let length = bytes.len() as u64;
            files.insert(name, (fingerprint(&bytes), length));
        }
        Ok(Self { files, truncated })
    }

    fn len(&self) -> usize {
        self.files.len()
    }

    fn compare(&self, previous: &Self) -> Change {
        let mut files = 0;
        let mut bytes = 0_i64;
        for (name, (digest, length)) in &self.files {
            match previous.files.get(name) {
                Some((was, _)) if was == digest => {}
                Some((_, before)) => {
                    files += 1;
                    bytes += i64::try_from(*length).unwrap_or(i64::MAX)
                        - i64::try_from(*before).unwrap_or(i64::MAX);
                }
                None => {
                    files += 1;
                    bytes += i64::try_from(*length).unwrap_or(i64::MAX);
                }
            }
        }
        for (name, (_, before)) in &previous.files {
            if !self.files.contains_key(name) {
                files += 1;
                bytes -= i64::try_from(*before).unwrap_or(i64::MAX);
            }
        }
        Change { files, bytes }
    }
}

fn fingerprint(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut encoded, byte| {
            let _ = write!(&mut encoded, "{byte:02x}");
            encoded
        })
}

fn lock_error<T>(_error: T) -> ToolError {
    ToolError::new("the gate's batch lock was poisoned by an earlier panic")
}

fn json_output<T: Serialize>(value: &T) -> Result<ToolOutput, ToolError> {
    serde_json::to_string(value)
        .map(ToolOutput::success)
        .map_err(|error| ToolError::new(format!("cannot encode gate output: {error}")))
}
