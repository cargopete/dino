//! Metrics, read back out of the session log the run produced.
//!
//! The harness does not instrument the agent. RFC-0006 said the log's jobs were
//! debugging, approval audit and the eval corpus, and a claim like that wants a
//! consumer rather than an intention. If the log turns out to be insufficient
//! to reconstruct a run, this module is where that shows up, rather than it
//! hiding behind a second parallel record.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;

use gyr_protocol::AgentEvent;
use gyr_protocol::ApprovalDecision;
use gyr_protocol::ModelEvent;
use gyr_protocol::StopReason;
use gyr_protocol::TokenUsage;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::EvalError;

/// Counts from one run. These decide nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metrics {
    pub model_turns: u32,
    /// How many times each tool was called, by name.
    pub tool_calls: BTreeMap<String, usize>,
    /// Calls a policy refused. Under allow-all this should be zero, and a
    /// non-zero value means a tool refused itself.
    pub refusals: usize,
    /// Calls that ran and returned an error result.
    pub tool_errors: usize,
    /// Every verdict the gate returned, in order.
    pub gate_verdicts: Vec<String>,
    /// How many earlier tool results were hollowed out to stay inside the
    /// window, and how much that reclaimed.
    pub results_elided: usize,
    pub bytes_reclaimed: usize,
    /// Whether the run was told it was filling the window.
    pub context_warned: bool,
    /// Turns sent again after their stream broke partway.
    #[serde(default)]
    pub turns_retried: usize,
    pub tokens: TokenUsage,
    pub stop_reason: Option<StopReason>,
}

/// Reads a session log and counts what happened.
///
/// # Errors
///
/// Returns [`EvalError`] when the log cannot be read or holds a line that is
/// not a record. A malformed log is a defect worth failing on rather than
/// skipping past.
pub fn from_log(path: &Path) -> Result<Metrics, EvalError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| EvalError::Log(format!("cannot read {}: {error}", path.display())))?;

    let mut metrics = Metrics::default();
    // ToolFinished carries a call ID rather than a name, so the names come from
    // the ToolStarted records that precede it.
    let mut names: HashMap<String, String> = HashMap::new();

    for (number, line) in text.lines().enumerate() {
        let record: Value = serde_json::from_str(line).map_err(|error| {
            EvalError::Log(format!(
                "{}:{}: not a session record: {error}",
                path.display(),
                number + 1
            ))
        })?;
        match record.get("record").and_then(Value::as_str) {
            Some("event") => {
                let event: AgentEvent =
                    serde_json::from_value(record["event"].clone()).map_err(|error| {
                        EvalError::Log(format!(
                            "{}:{}: not an agent event: {error}",
                            path.display(),
                            number + 1
                        ))
                    })?;
                absorb(&mut metrics, &mut names, &event);
            }
            Some("finished") => {
                metrics.model_turns = record["outcome"]["model_turns"]
                    .as_u64()
                    .and_then(|turns| u32::try_from(turns).ok())
                    .unwrap_or(0);
                metrics.stop_reason =
                    serde_json::from_value(record["outcome"]["stop_reason"].clone()).ok();
            }
            _ => {}
        }
    }
    Ok(metrics)
}

fn absorb(metrics: &mut Metrics, names: &mut HashMap<String, String>, event: &AgentEvent) {
    match event {
        AgentEvent::Model {
            event: ModelEvent::Usage { usage },
            ..
        } => {
            metrics.tokens.input_tokens += usage.input_tokens;
            metrics.tokens.cached_input_tokens += usage.cached_input_tokens;
            metrics.tokens.output_tokens += usage.output_tokens;
            metrics.tokens.reasoning_tokens += usage.reasoning_tokens;
        }
        AgentEvent::ToolDecided {
            decision: ApprovalDecision::Denied { .. },
            ..
        } => metrics.refusals += 1,
        AgentEvent::ToolStarted { call, .. } => {
            *metrics.tool_calls.entry(call.name.clone()).or_insert(0) += 1;
            names.insert(call.id.clone(), call.name.clone());
        }
        AgentEvent::ToolFinished { result, .. } => {
            if result.output.is_error {
                metrics.tool_errors += 1;
            }
            if names.get(&result.call_id).map(String::as_str) == Some("gate")
                && let Ok(body) = serde_json::from_str::<Value>(&result.output.content)
                && let Some(verdict) = body.get("verdict").and_then(Value::as_str)
            {
                metrics.gate_verdicts.push(verdict.to_owned());
            }
        }
        AgentEvent::Elided {
            results_elided,
            bytes_reclaimed,
            ..
        } => {
            metrics.results_elided += results_elided;
            metrics.bytes_reclaimed += bytes_reclaimed;
        }
        AgentEvent::ContextWarning { .. } => metrics.context_warned = true,
        AgentEvent::TurnRetried { .. } => metrics.turns_retried += 1,
        // Counted by nothing: a submission is the unit a metric is per, not a
        // metric itself.
        AgentEvent::Submitted { .. }
        | AgentEvent::Model { .. }
        | AgentEvent::ToolDecided { .. } => {}
    }
}
