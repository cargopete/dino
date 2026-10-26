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
use crate::style::WARN;

const MAX_SUMMARY_BYTES: usize = 120;
const BELL_AFTER_SECONDS: u64 = 15;

pub struct Renderer {
    show_reasoning: bool,
    stream: Stream,
    /// Model text not yet ended by a newline. Text is drawn a line at a time,
    /// because Markdown cannot be drawn from a fragment.
    pending: String,
    markdown: crate::markdown::Markdown,
    /// The call whose line is open, waiting for its result to finish it.
    open_call: Option<ToolCall>,
    /// When the current submission started, for the closing line.
    started: Option<std::time::Instant>,
    spinner: crate::spinner::Spinner,
    /// Accumulated across every submission, because a session's cost is the
    /// question a person actually has. Shared so `/status` can read the same
    /// number the turn summary prints, rather than keeping a second tally that
    /// could disagree with it.
    usage: Arc<Mutex<TokenUsage>>,
    /// Model responses, which is what a request-metered plan counts.
    requests: Arc<std::sync::atomic::AtomicU64>,
    /// Set when the core warns that the context window is filling.
    context_full: Arc<std::sync::atomic::AtomicBool>,
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
            pending: String::new(),
            markdown: crate::markdown::Markdown::default(),
            open_call: None,
            started: None,
            spinner: crate::spinner::Spinner::default(),
            usage: Arc::new(Mutex::new(TokenUsage::default())),
            requests: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            context_full: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Set by a context warning, for a session that compacts on its own.
    #[must_use]
    pub fn context_handle(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.context_full)
    }

    /// A handle on the number of model responses, for `/cost`.
    #[must_use]
    pub fn requests_handle(&self) -> Arc<std::sync::atomic::AtomicU64> {
        Arc::clone(&self.requests)
    }

    /// A handle on the running total, for `/status`.
    #[must_use]
    pub fn usage_handle(&self) -> Arc<Mutex<TokenUsage>> {
        Arc::clone(&self.usage)
    }

    /// Draws whatever is held back: a partial line of text, an open call.
    ///
    /// # Errors
    ///
    /// Returns [`SinkError`] when the terminal cannot be written.
    pub fn flush(&mut self) -> Result<(), SinkError> {
        self.spinner.stop();
        self.break_stream()
    }

    /// Closes any open streaming line before printing a structured one.
    fn break_stream(&mut self) -> Result<(), SinkError> {
        if self.open_call.take().is_some() {
            write_out("\n")?;
        }
        if self.stream == Stream::Idle {
            return Ok(());
        }
        if self.stream == Stream::Text {
            let rest = std::mem::take(&mut self.pending);
            if !rest.is_empty() {
                write_out(&self.markdown.line(&rest))?;
            }
            self.markdown.reset();
        }
        self.stream = Stream::Idle;
        write_out("\n")
    }

    fn stream_delta(&mut self, kind: Stream, delta: &str) -> Result<(), SinkError> {
        // Models often open a message with blank lines before calling a tool,
        // which would otherwise be drawn as empty space.
        let delta = if self.stream == kind {
            delta
        } else {
            delta.trim_start()
        };
        if delta.is_empty() {
            return Ok(());
        }
        if self.stream != kind || self.open_call.is_some() {
            self.break_stream()?;
            self.stream = kind;
            if kind == Stream::Text {
                write_out("\n")?;
            }
        }
        if kind == Stream::Reasoning {
            return write_out(&style::paint_with(FAINT, &[ITALIC], delta));
        }
        self.pending.push_str(delta);
        while let Some(end) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=end).collect();
            let drawn = self.markdown.line(line.trim_end_matches('\n'));
            write_out(&format!("{drawn}\n"))?;
        }
        Ok(())
    }

    fn model_event(&mut self, event: &ModelEvent) -> Result<(), SinkError> {
        match event {
            ModelEvent::TextDelta { text } => self.stream_delta(Stream::Text, text),
            ModelEvent::ReasoningDelta { text } if self.show_reasoning => {
                self.stream_delta(Stream::Reasoning, text)
            }
            ModelEvent::Usage { usage } => {
                self.requests
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

impl Renderer {
    /// Whether an event puts anything on screen. Those that do not, such as
    /// hidden reasoning, token counts and a call's arguments arriving, leave
    /// the spinner ticking.
    /// Stops the spinner before anything is drawn, and starts it when the
    /// model begins a turn.
    fn tick(&mut self, event: &AgentEvent) -> Result<(), SinkError> {
        if self.draws(event) {
            self.spinner.stop();
        }
        if let AgentEvent::Model {
            event: ModelEvent::Started { .. },
            ..
        } = event
        {
            self.break_stream()?;
            self.spinner.thinking();
        }
        Ok(())
    }

    fn draws(&self, event: &AgentEvent) -> bool {
        match event {
            AgentEvent::Model { event, .. } => match event {
                ModelEvent::ReasoningDelta { .. } => self.show_reasoning,
                ModelEvent::Usage { .. }
                | ModelEvent::Started { .. }
                | ModelEvent::ToolCallStarted { .. }
                | ModelEvent::ToolCallArgumentsDelta { .. }
                | ModelEvent::ToolCallCompleted { .. } => false,
                ModelEvent::TextDelta { .. } | ModelEvent::Finished { .. } => true,
            },
            _ => true,
        }
    }
}

impl EventSink for Renderer {
    fn emit(&mut self, event: &AgentEvent) -> Result<(), SinkError> {
        self.tick(event)?;
        match event {
            // The person's own words, in the person's colour.
            AgentEvent::Submitted { text } => {
                self.break_stream()?;
                self.started = Some(std::time::Instant::now());
                submitted(text)
            }
            AgentEvent::Model { event, .. } => self.model_event(event),
            AgentEvent::ToolDecided {
                tool,
                action,
                decision,
                ..
            } => self.decision(tool, action.subject.as_deref(), decision),
            AgentEvent::ToolStarted { call, .. } if call.name == gyr_core::todo::TODO => {
                self.break_stream()?;
                todo_started(call)
            }
            AgentEvent::ToolStarted { call, .. } => {
                self.break_stream()?;
                // The machine is about to do something, so the machine's colour.
                // The line stays open for the result to finish it.
                let line = style::paint(SLATE, &format!("  › {}", describe_call(call)));
                write_out(&line)?;
                self.open_call = Some(call.clone());
                self.spinner.after(line);
                Ok(())
            }
            AgentEvent::ContextWarning {
                input_tokens,
                window_tokens,
                ..
            } => {
                self.break_stream()?;
                self.context_full
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                context_warning(*input_tokens, *window_tokens)
            }
            AgentEvent::HandedOver {
                session,
                reason,
                pending,
            } => {
                self.break_stream()?;
                handed_over(*session, reason, *pending)
            }
            AgentEvent::Elided {
                results_elided,
                calls_elided,
                bytes_reclaimed,
                ..
            } => {
                self.break_stream()?;
                elided(*results_elided, *calls_elided, *bytes_reclaimed)
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
            AgentEvent::ToolFinished { result, .. } => self.tool_finished(result),
        }
    }

    fn finish(&mut self, outcome: &RunOutcome) -> Result<(), SinkError> {
        self.spinner.stop();
        self.break_stream()?;
        let usage = self
            .usage
            .lock()
            .map_or_else(|_| TokenUsage::default(), |total| *total);
        let elapsed = self.started.take().map(|started| started.elapsed());
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
        let time = elapsed.map_or_else(String::new, |elapsed| format!(" · {}", duration(elapsed)));
        let turns = if outcome.model_turns == 1 {
            "turn"
        } else {
            "turns"
        };
        let tail = style::paint(
            FAINT,
            &format!(
                "{time} · {} {turns} · {}\n",
                outcome.model_turns,
                describe_usage(usage)
            ),
        );
        write_out(&format!("\n  {painted_ending}{tail}"))?;
        // A long turn rings, so a person who looked away hears it finish.
        if style::enabled()
            && elapsed.is_some_and(|elapsed| elapsed.as_secs() >= BELL_AFTER_SECONDS)
        {
            write_out("\u{7}")?;
        }
        Ok(())
    }
}

/// One line of running cost, in the words the summary uses.
#[must_use]
pub fn describe_usage(usage: TokenUsage) -> String {
    let cached = (usage.cached_input_tokens * 100)
        .checked_div(usage.input_tokens)
        .map_or_else(String::new, |percent| format!(", {percent}% cached"));
    let reasoning = if usage.reasoning_tokens == 0 {
        String::new()
    } else {
        format!(" ({} reasoning)", compact(usage.reasoning_tokens))
    };
    format!(
        "{} in{cached} · {} out{reasoning}",
        compact(usage.input_tokens),
        compact(usage.output_tokens)
    )
}

/// A label's first clause, for a one-line banner: `workspace (seatbelt:
/// writes confined, …)` becomes `workspace (seatbelt)`. `/status` keeps the whole.
#[must_use]
pub fn short_label(label: &str) -> String {
    match label.split_once(" (") {
        Some((head, rest)) => {
            let first = rest
                .split([':', ')', ','])
                .next()
                .unwrap_or_default()
                .trim();
            if first.is_empty() || first.len() > 24 {
                head.to_owned()
            } else {
                format!("{head} ({first})")
            }
        }
        None => label.to_owned(),
    }
}

/// A path relative to the workspace where it is inside it, else with the home
/// directory written `~`.
#[must_use]
pub fn short_path(path: &std::path::Path, workspace: &std::path::Path) -> String {
    if !workspace.as_os_str().is_empty()
        && let Ok(inside) = path.strip_prefix(workspace)
    {
        return inside.display().to_string();
    }
    match std::env::var_os("HOME").map(std::path::PathBuf::from) {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    }
}

/// A count as a person reads it at a glance: 852, 19.7k, 1.2M.
#[must_use]
pub fn compact(number: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let value = number as f64;
    match number {
        0..1_000 => number.to_string(),
        1_000..1_000_000 => format!("{:.1}k", value / 1_000.0),
        _ => format!("{:.1}M", value / 1_000_000.0),
    }
}

/// Elapsed time to the second, in the largest units that say it.
#[must_use]
pub fn duration(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3_600, seconds % 3_600 / 60),
    }
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
        "background" => argument_string(&call.arguments, "op").map(|op| {
            match (
                argument_string(&call.arguments, "command"),
                argument_number(&call.arguments, "id"),
            ) {
                (Some(command), _) => format!("{op} {command}"),
                (None, Some(id)) => format!("{op} #{id}"),
                (None, None) => op,
            }
        }),
        "web_fetch" => argument_string(&call.arguments, "url"),
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

/// The first line of a submission and how many follow. Attachments, summaries
/// and pastes can run to pages; the log keeps them whole.
fn submitted(text: &str) -> Result<(), SinkError> {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    let more = lines.count();
    write_out(&style::paint(RUST, &format!("\n› {first}\n")))?;
    if more > 0 {
        write_out(&style::paint(FAINT, &format!("  (+{more} more lines)\n")))?;
    }
    Ok(())
}

fn elided(results: usize, calls: usize, bytes: usize) -> Result<(), SinkError> {
    write_out(&style::paint(
        FAINT,
        &format!(
            "  elided {results} earlier result(s) and {calls} call(s), {bytes} bytes, to stay \
             inside the window\n"
        ),
    ))
}

fn context_warning(input_tokens: u64, window_tokens: u32) -> Result<(), SinkError> {
    let percent = (input_tokens * 100) / u64::from(window_tokens).max(1);
    write_out(&style::paint(
        WARN,
        &format!(
            "  the context window is {percent}% full ({input_tokens} of {window_tokens}); a new \
             session starts clean\n"
        ),
    ))
}

fn handed_over(session: u32, reason: &str, pending: usize) -> Result<(), SinkError> {
    write_out(&style::paint(
        SLATE,
        &format!("\n  ── session {session}: {reason}; {pending} item(s) pending ──\n"),
    ))
}

/// The checklist itself, rather than a one-line summary of the call.
fn todo_started(call: &gyr_protocol::ToolCall) -> Result<(), SinkError> {
    let list = gyr_core::todo::items(&call.arguments)
        .map(|items| gyr_core::todo::render(&items))
        .unwrap_or_default();
    let indented = list.lines().fold(String::new(), |mut block, line| {
        block.push_str("    ");
        block.push_str(line);
        block.push('\n');
        block
    });
    write_out(&style::paint(SLATE, &format!("  › todo\n{indented}")))
}

impl Renderer {
    /// Finishes the call's open line with the gist of its result.
    fn tool_finished(&mut self, result: &ToolResult) -> Result<(), SinkError> {
        let Some(call) = self.open_call.take() else {
            // A refused call has had its line already, and a todo its list.
            return Ok(());
        };
        let Some((tone, gist)) = crate::summary::summarise(&call, &result.output) else {
            return write_out("\n");
        };
        let colour = match tone {
            crate::summary::Tone::Fine => FAINT,
            crate::summary::Tone::Failed => WARN,
        };
        write_out(&format!(
            "{}\n",
            style::paint(colour, &format!("  · {}", cap(&gist, MAX_SUMMARY_BYTES)))
        ))?;
        if call.name == "apply_patch" && tone == crate::summary::Tone::Fine {
            write_out(&patch_preview(&call))?;
        }
        Ok(())
    }
}

/// The changed lines of a patch, beneath its call: removed in terracotta,
/// added in green, behind a faint gutter.
fn patch_preview(call: &ToolCall) -> String {
    use crate::summary::Change;
    const PREVIEW_LINES: usize = 12;
    crate::summary::patch_preview(call, PREVIEW_LINES)
        .into_iter()
        .fold(String::new(), |mut block, change| {
            let line = match change {
                Change::Removed(text) => {
                    style::paint(RUST, &format!("- {}", cap(&text, MAX_SUMMARY_BYTES)))
                }
                Change::Added(text) => {
                    style::paint(OK, &format!("+ {}", cap(&text, MAX_SUMMARY_BYTES)))
                }
                Change::More(hidden) => style::paint(FAINT, &format!("… {hidden} more")),
            };
            block.push_str(&style::paint(FAINT, "      │ "));
            block.push_str(&line);
            block.push('\n');
            block
        })
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

#[cfg(test)]
mod tests {
    use gyr_core::session::EventSink as _;
    use gyr_protocol::AgentEvent;

    use super::Renderer;

    #[test]
    fn a_context_warning_raises_the_flag_auto_compaction_watches() {
        let mut renderer = Renderer::new(false);
        let full = renderer.context_handle();

        renderer
            .emit(&AgentEvent::ContextWarning {
                model_turn: 3,
                input_tokens: 190_000,
                window_tokens: 262_144,
            })
            .unwrap();

        assert!(full.load(std::sync::atomic::Ordering::Relaxed));
    }
}
