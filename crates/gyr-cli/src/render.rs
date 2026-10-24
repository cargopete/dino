//! Renders agent events to a terminal.
//!
//! The renderer is an [`EventSink`] and holds no agent state, so the session in
//! RFC-0007 is the same renderer driven repeatedly rather than a second one.
//!
//! Colour follows the house rule: if a shell printed it, it is slate; if a
//! person wrote it, it is terracotta.

use std::io::Write;
use std::io::stdout;
use std::sync::Arc;
use std::sync::Mutex;

use gyr_core::session::EventSink;
use gyr_core::session::RunOutcome;
use gyr_core::session::SinkError;
use gyr_protocol::AgentEvent;
use gyr_protocol::ApprovalDecision;
use gyr_protocol::DecisionSource;
use gyr_protocol::ModelEvent;
use gyr_protocol::StopReason;
use gyr_protocol::TokenUsage;
use gyr_protocol::ToolCall;
use gyr_protocol::ToolResult;
use serde_json::Value;

use crate::style;
use crate::style::FAINT;
use crate::style::ITALIC;
use crate::style::MUTED;
use crate::style::OK;
use crate::style::RUST;
use crate::style::SLATE;
use crate::style::TEXT;
use crate::style::WARN;

const MAX_SUMMARY_BYTES: usize = 120;

pub struct Renderer {
    show_reasoning: bool,
    stream: Stream,
    /// Accumulated across every submission, because a session's cost is the
    /// question a person actually has. Shared so `/status` can read the same
    /// number the turn summary prints, rather than keeping a second tally that
    /// could disagree with it.
    usage: Arc<Mutex<TokenUsage>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    Idle,
    Text,
    Reasoning,
}

impl Renderer {
    #[must_use]
    pub fn new(show_reasoning: bool) -> Self {
        Self {
            show_reasoning,
            stream: Stream::Idle,
            usage: Arc::new(Mutex::new(TokenUsage::default())),
        }
    }

    /// A handle on the running total, for `/status`.
    #[must_use]
    pub fn usage_handle(&self) -> Arc<Mutex<TokenUsage>> {
        Arc::clone(&self.usage)
    }

    /// Closes any open streaming line before printing a structured one.
    fn break_stream(&mut self) -> Result<(), SinkError> {
        if self.stream == Stream::Idle {
            return Ok(());
        }
        self.stream = Stream::Idle;
        write_out("\n")
    }

    fn stream_delta(&mut self, kind: Stream, delta: &str) -> Result<(), SinkError> {
        if self.stream != kind {
            if self.stream != Stream::Idle {
                write_out("\n")?;
            }
            self.stream = kind;
        }
        let painted = match kind {
            Stream::Reasoning => style::paint_with(FAINT, &[ITALIC], delta),
            _ => style::paint(TEXT, delta),
        };
        write_out(&painted)
    }

    fn model_event(&mut self, event: &ModelEvent) -> Result<(), SinkError> {
        match event {
            ModelEvent::TextDelta { text } => self.stream_delta(Stream::Text, text),
            ModelEvent::ReasoningDelta { text } if self.show_reasoning => {
                self.stream_delta(Stream::Reasoning, text)
            }
            ModelEvent::Usage { usage } => {
                if let Ok(mut total) = self.usage.lock() {
                    total.input_tokens += usage.input_tokens;
                    total.cached_input_tokens += usage.cached_input_tokens;
                    total.output_tokens += usage.output_tokens;
                    total.reasoning_tokens += usage.reasoning_tokens;
                }
                Ok(())
            }
            ModelEvent::Finished { reason } => {
                self.break_stream()?;
                if !matches!(reason, StopReason::EndTurn | StopReason::ToolUse) {
                    let line = format!("  the model stopped: {}\n", stop_reason_label(*reason));
                    write_out(&style::paint(WARN, &line))?;
                }
                Ok(())
            }
            ModelEvent::ReasoningDelta { .. }
            | ModelEvent::Started { .. }
            | ModelEvent::ToolCallStarted { .. }
            | ModelEvent::ToolCallArgumentsDelta { .. }
            | ModelEvent::ToolCallCompleted { .. } => Ok(()),
        }
    }

    fn decision(
        &mut self,
        tool: &str,
        subject: Option<&str>,
        decision: &ApprovalDecision,
    ) -> Result<(), SinkError> {
        let subject = subject.unwrap_or(tool);
        match decision {
            // A person decided this, so a person's colour carries it.
            ApprovalDecision::Denied { reason } => {
                self.break_stream()?;
                write_out(&style::paint(RUST, &format!("  refused  {subject}\n")))?;
                write_out(&style::paint(FAINT, &format!("           {reason}\n")))
            }
            // A person answering the prompt has already seen it, and an
            // auto-allowed read needs no announcement. A standing rule being
            // spent is the one case worth showing.
            ApprovalDecision::Allowed {
                source: source @ (DecisionSource::SessionRule | DecisionSource::SavedRule),
            } => {
                self.break_stream()?;
                let rule = if *source == DecisionSource::SavedRule {
                    "saved rule"
                } else {
                    "session rule"
                };
                write_out(&style::paint(
                    FAINT,
                    &format!("  allowed  {subject}  by {rule}\n"),
                ))
            }
            ApprovalDecision::Allowed { .. } => Ok(()),
        }
    }
}

impl EventSink for Renderer {
    fn emit(&mut self, event: &AgentEvent) -> Result<(), SinkError> {
        match event {
            // The person's own words, in the person's colour.
            AgentEvent::Submitted { text } => {
                self.break_stream()?;
                write_out(&style::paint(RUST, &format!("\n› {text}\n")))
            }
            AgentEvent::Model { event, .. } => self.model_event(event),
            AgentEvent::ToolDecided {
                tool,
                action,
                decision,
                ..
            } => self.decision(tool, action.subject.as_deref(), decision),
            AgentEvent::ToolStarted { call, .. } => {
                self.break_stream()?;
                // The machine is about to do something, so the machine's colour.
                write_out(&style::paint(
                    SLATE,
                    &format!("  › {}\n", describe_call(call)),
                ))
            }
            AgentEvent::ContextWarning {
                input_tokens,
                window_tokens,
                ..
            } => {
                self.break_stream()?;
                let percent = (*input_tokens * 100) / u64::from(*window_tokens).max(1);
                write_out(&style::paint(
                    WARN,
                    &format!(
                        "  the context window is {percent}% full ({input_tokens} of \
                         {window_tokens}); a new session starts clean\n"
                    ),
                ))
            }
            AgentEvent::Elided {
                results_elided,
                calls_elided,
                bytes_reclaimed,
                ..
            } => {
                self.break_stream()?;
                write_out(&style::paint(
                    FAINT,
                    &format!(
                        "  elided {results_elided} earlier result(s) and {calls_elided} call(s), \
                         {bytes_reclaimed} bytes, to stay inside the window\n"
                    ),
                ))
            }
            AgentEvent::TurnsRunningOut { turns_left, .. } => {
                self.break_stream()?;
                write_out(&style::paint(
                    FAINT,
                    &format!("  told the model it has {turns_left} turn(s) left\n"),
                ))
            }
            AgentEvent::TurnCutOff {
                input_tokens,
                output_tokens,
                window_tokens,
                ..
            } => {
                self.break_stream()?;
                write_out(&style::paint(
                    WARN,
                    &format!(
                        "  the window filled mid-turn ({input_tokens} in, {output_tokens} out, \
                         of {window_tokens}); continuing after elision\n"
                    ),
                ))
            }
            AgentEvent::TurnRetried { attempt, error, .. } => {
                self.break_stream()?;
                write_out(&style::paint(
                    WARN,
                    &format!(
                        "  the stream broke ({}); sending the turn again, attempt {}\n",
                        cap(first_line(error), MAX_SUMMARY_BYTES),
                        attempt + 1
                    ),
                ))
            }
            AgentEvent::SentBack { reason, .. } => {
                self.break_stream()?;
                write_out(&style::paint(
                    WARN,
                    &format!(
                        "  not finished, sent back: {}\n",
                        cap(first_line(reason), MAX_SUMMARY_BYTES)
                    ),
                ))
            }
            AgentEvent::ToolFinished { result, .. } => tool_finished(result),
        }
    }

    fn finish(&mut self, outcome: &RunOutcome) -> Result<(), SinkError> {
        self.break_stream()?;
        let usage = self
            .usage
            .lock()
            .map_or_else(|_| TokenUsage::default(), |total| *total);
        let ending = match (&outcome.error, outcome.stop_reason) {
            (Some(error), _) => format!("failed: {error}"),
            (None, Some(reason)) => stop_reason_label(reason).to_owned(),
            (None, None) => "ended without a stop reason".to_owned(),
        };
        // One green word per submission, and only when it genuinely finished.
        // Motion and status colour are spent, not sprinkled.
        let painted_ending = match (&outcome.error, outcome.stop_reason) {
            (Some(_), _) => style::paint(WARN, &ending),
            (None, Some(StopReason::EndTurn)) => style::paint(OK, &ending),
            _ => style::paint(FAINT, &ending),
        };
        let tail = style::paint(
            FAINT,
            &format!(
                " · {} model turn(s) · {}\n",
                outcome.model_turns,
                describe_usage(usage)
            ),
        );
        write_out(&format!("\n  {painted_ending}{tail}"))
    }
}

/// One line of running cost, in the words the summary uses.
#[must_use]
pub fn describe_usage(usage: TokenUsage) -> String {
    format!(
        "{} in ({} cached), {} out, {} reasoning",
        usage.input_tokens, usage.cached_input_tokens, usage.output_tokens, usage.reasoning_tokens
    )
}

/// The session banner and `/status`, in one place so they cannot drift apart.
#[must_use]
pub fn status_block(rows: &[(&str, String)]) -> String {
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    rows.iter()
        .fold(String::new(), |mut block, (label, value)| {
            use std::fmt::Write as _;
            let _ = writeln!(
                &mut block,
                "  {}  {}",
                style::kicker(&format!("{label:<width$}")),
                style::paint(MUTED, value)
            );
            block
        })
}

fn write_out(text: &str) -> Result<(), SinkError> {
    let mut out = stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|error| SinkError::new(format!("cannot write to the terminal: {error}")))
}

/// One readable line for a proposed call, built from its arguments rather than
/// from a description the model supplied.
#[must_use]
pub fn describe_call(call: &ToolCall) -> String {
    let detail = match call.name.as_str() {
        "read" => argument_string(&call.arguments, "path").map(|path| {
            match (
                argument_number(&call.arguments, "start_line"),
                argument_number(&call.arguments, "end_line"),
            ) {
                (Some(start), Some(end)) => format!("{path}:{start}-{end}"),
                (Some(start), None) => format!("{path}:{start}-"),
                _ => path,
            }
        }),
        "search" => argument_string(&call.arguments, "query").map(|query| {
            match argument_string(&call.arguments, "path") {
                Some(path) => format!("{query:?} in {path}"),
                None => format!("{query:?}"),
            }
        }),
        "list" => Some(argument_string(&call.arguments, "path").unwrap_or_else(|| ".".to_owned())),
        "apply_patch" | "write_file" => argument_string(&call.arguments, "path"),
        "exec" => call
            .arguments
            .get("command")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            }),
        "gate" | "shell" => argument_string(&call.arguments, "command"),
        "cargo" => argument_string(&call.arguments, "command").map(|command| {
            let package = argument_string(&call.arguments, "package")
                .map_or_else(String::new, |package| format!(" -p {package}"));
            let filter = argument_string(&call.arguments, "filter")
                .map_or_else(String::new, |filter| format!(" {filter}"));
            format!("{command}{package}{filter}")
        }),
        "github" => argument_string(&call.arguments, "command").map(|command| {
            match call.arguments.get("number").and_then(Value::as_u64) {
                Some(number) => format!("{command} #{number}"),
                None => command,
            }
        }),
        _ => None,
    };
    match detail {
        Some(detail) => format!("{} {}", call.name, cap(&detail, MAX_SUMMARY_BYTES)),
        None => format!(
            "{} {}",
            call.name,
            cap(&call.arguments.to_string(), MAX_SUMMARY_BYTES)
        ),
    }
}

fn stop_reason_label(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "finished",
        StopReason::ToolUse => "waiting on tools",
        StopReason::MaxTokens => "hit the output limit",
        StopReason::Refusal => "refused",
        StopReason::Blocked => "blocked",
        StopReason::Cancelled => "cancelled",
    }
}

fn argument_string(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn argument_number(arguments: &Value, key: &str) -> Option<u64> {
    arguments.get(key).and_then(Value::as_u64)
}

fn tool_finished(result: &ToolResult) -> Result<(), SinkError> {
    let (colour, summary) = if result.output.is_error {
        (
            WARN,
            format!(
                "    {}\n",
                cap(first_line(&result.output.content), MAX_SUMMARY_BYTES)
            ),
        )
    } else {
        (
            FAINT,
            format!("    {} bytes\n", result.output.content.len()),
        )
    };
    write_out(&style::paint(colour, &summary))
}

fn first_line(value: &str) -> &str {
    value.lines().next().unwrap_or(value)
}

fn cap(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}
