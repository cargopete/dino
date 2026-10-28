//! Renders agent events to a terminal.
//!
//! The renderer is an [`EventSink`] and holds no agent state, so the session in
//! RFC-0007 is the same renderer driven repeatedly rather than a second one.
//!
//! Colour follows the house rule: if a shell printed it, it is slate; if a
//! person wrote it, it is terracotta.
//!
//! On a terminal a call's line is committed as it starts and redrawn in place
//! when it finishes, or when the next look joins it, for as long as nothing has
//! been written beneath it. Without colour every line is written once, in order.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Write;
use std::io::stdout;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use dino_core::session::EventSink;
use dino_core::session::RunOutcome;
use dino_core::session::SinkError;
use dino_protocol::AgentEvent;
use dino_protocol::ApprovalDecision;
use dino_protocol::DecisionSource;
use dino_protocol::ModelEvent;
use dino_protocol::StopReason;
use dino_protocol::TokenUsage;
use dino_protocol::ToolCall;
use dino_protocol::ToolResult;
use dino_protocol::WindowSource;
use serde_json::Value;

use crate::calls;
use crate::calls::Mark;
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
/// Up one line and clear it. Not a carriage return, which a terminal set to
/// translate it turns into a newline.
const BACK_UP: &str = "\u{1b}[1A\u{1b}[2K\u{1b}[1G";

/// Counts writes to the terminal, so a line knows whether it is still the last.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// For anything else that writes to the terminal during a turn, such as the
/// approval prompt, so the renderer does not redraw a line that has moved up.
pub fn others_wrote() {
    WRITES.fetch_add(1, Ordering::Relaxed);
}

/// The call line drawn last and what it showed.
struct Last {
    /// [`WRITES`] just after it was drawn.
    writes: u64,
    entries: Vec<Entry>,
    /// Whether further looks may join it.
    folds: bool,
}

impl Last {
    /// Whether nothing has been written beneath it.
    fn is_last(&self) -> bool {
        self.writes == WRITES.load(Ordering::Relaxed)
    }

    /// The line, cut to the terminal so it never wraps: redrawing in place
    /// goes up exactly one row.
    fn line(&self) -> String {
        // The mark and icon take six columns, and one is spare.
        let room = crate::markdown::terminal_width().saturating_sub(7);
        if let [entry] = &self.entries[..] {
            return call_line(entry.mark, &entry.call, entry.gist.as_ref(), room);
        }
        let marks = || self.entries.iter().map(|entry| entry.mark);
        let mark = if marks().any(|mark| mark == Mark::Running) {
            Mark::Running
        } else if marks().any(|mark| mark == Mark::Failed) {
            Mark::Failed
        } else {
            Mark::Done
        };
        let looks: Vec<calls::Look> = self
            .entries
            .iter()
            .map(|entry| calls::Look::of(&entry.call, entry.mark == Mark::Failed))
            .collect();
        format!(
            "  {} {}",
            paint_mark(mark),
            style::paint(SLATE, &format!("→ {}", calls::explored(&looks, room)))
        )
    }
}

struct Entry {
    call: ToolCall,
    mark: Mark,
    gist: Option<(crate::summary::Tone, String)>,
}

/// One call on one line: `  ✓ → read src/main.rs · 62 lines`.
fn call_line(
    mark: Mark,
    call: &ToolCall,
    gist: Option<&(crate::summary::Tone, String)>,
    room: usize,
) -> String {
    let (gist, colour) = match gist {
        Some((crate::summary::Tone::Failed, gist)) => (gist.as_str(), WARN),
        Some((crate::summary::Tone::Fine, gist)) => (gist.as_str(), FAINT),
        None => ("", FAINT),
    };
    let (description, gist) = calls::fit(&describe_call(call), gist, room);
    let mut line = format!(
        "  {} {}",
        paint_mark(mark),
        style::paint(SLATE, &format!("{} {description}", calls::icon(&call.name)))
    );
    if !gist.is_empty() {
        line.push_str(&style::paint(colour, &format!(" · {gist}")));
    }
    line
}

fn paint_mark(mark: Mark) -> String {
    let colour = match mark {
        Mark::Running => SLATE,
        Mark::Done => MUTED,
        Mark::Failed => WARN,
        Mark::Refused => RUST,
    };
    style::paint(colour, &mark.glyph().to_string())
}

/// A refused call, telling a person's refusal from a policy's.
fn refused_line(tool: &str, description: &str, reason: &str) -> String {
    let room = crate::markdown::terminal_width().saturating_sub(7);
    let (mark, colour, tag, said) = match reason.strip_prefix("the operator refused this action") {
        Some(said) => (
            Mark::Refused,
            RUST,
            "refused by you".to_owned(),
            said.strip_prefix(": ").map(str::trim),
        ),
        None => (
            Mark::Failed,
            WARN,
            format!("blocked: {}", first_line(reason)),
            None,
        ),
    };
    let (description, tag) = calls::fit(description, &tag, room);
    let mut line = format!(
        "  {} {}{}\n",
        paint_mark(mark),
        style::paint(SLATE, &format!("{} {description}", calls::icon(tool))),
        style::paint(colour, &format!(" · {tag}"))
    );
    if let Some(said) = said.filter(|said| !said.is_empty()) {
        line.push_str(&style::paint(FAINT, &format!("      you said: {said}\n")));
    }
    line
}

pub struct Renderer {
    show_reasoning: bool,
    stream: Stream,
    /// Model text not yet ended by a newline. Text is drawn a line at a time,
    /// because Markdown cannot be drawn from a fragment.
    pending: String,
    markdown: crate::markdown::Markdown,
    /// The call whose line is open, waiting for its result to finish it.
    open_call: Option<ToolCall>,
    /// On a terminal, the line that may still be redrawn.
    last: Option<Last>,
    /// This submission's calls as the model made them, so a refusal can be
    /// drawn like the call it refused.
    calls: HashMap<String, ToolCall>,
    /// Calls a person approved at the prompt, which showed their change.
    asked: HashSet<String>,
    /// Drawing a log, where no prompt was drawn to show a change.
    replaying: bool,
    /// When the current submission started, for the closing line.
    started: Option<std::time::Instant>,
    spinner: crate::spinner::Spinner,
    /// What the status row shows, shared with the session, which names the model.
    status: Arc<Mutex<crate::spinner::Status>>,
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
        let status = Arc::new(Mutex::new(crate::spinner::Status::default()));
        Self {
            show_reasoning,
            stream: Stream::Idle,
            pending: String::new(),
            markdown: crate::markdown::Markdown::default(),
            open_call: None,
            last: None,
            calls: HashMap::new(),
            asked: HashSet::new(),
            replaying: false,
            started: None,
            spinner: crate::spinner::Spinner::new(Arc::clone(&status)),
            status,
            usage: Arc::new(Mutex::new(TokenUsage::default())),
            requests: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            context_full: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// For drawing a recorded session rather than a live one.
    #[must_use]
    pub fn replaying(mut self) -> Self {
        self.replaying = true;
        self
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

    /// A handle on the status row's figures, for naming the model and window.
    #[must_use]
    pub fn status_handle(&self) -> Arc<Mutex<crate::spinner::Status>> {
        Arc::clone(&self.status)
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
        let mut drew = false;
        if self.stream == Stream::Text {
            let rest = std::mem::take(&mut self.pending);
            let mut drawn = if rest.is_empty() {
                Vec::new()
            } else {
                self.markdown.lines(&rest)
            };
            drawn.extend(self.markdown.flush());
            for line in &drawn {
                write_out(&format!("{line}\n"))?;
            }
            drew = !drawn.is_empty();
            self.markdown.reset();
        }
        self.stream = Stream::Idle;
        // Each drawn line ended itself; otherwise the open line needs ending.
        if drew {
            return Ok(());
        }
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
            for drawn in self.markdown.lines(line.trim_end_matches('\n')) {
                write_out(&format!("{drawn}\n"))?;
            }
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
                if let Ok(mut status) = self.status.lock() {
                    status.context = usage.input_tokens;
                    status.input += usage.input_tokens;
                    status.output += usage.output_tokens;
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
            ModelEvent::ToolCallCompleted { call } => {
                self.calls.insert(call.id.clone(), call.clone());
                Ok(())
            }
            ModelEvent::ReasoningDelta { .. }
            | ModelEvent::Started { .. }
            | ModelEvent::ToolCallStarted { .. }
            | ModelEvent::ToolCallArgumentsDelta { .. } => Ok(()),
        }
    }

    fn decision(
        &mut self,
        call_id: &str,
        tool: &str,
        subject: Option<&str>,
        decision: &ApprovalDecision,
    ) -> Result<(), SinkError> {
        let subject = subject.unwrap_or(tool);
        match decision {
            ApprovalDecision::Denied { reason } if style::enabled() => {
                self.break_stream()?;
                let description = self
                    .calls
                    .get(call_id)
                    .map_or_else(|| subject.to_owned(), describe_call);
                write_out(&refused_line(tool, &description, reason))
            }
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
            ApprovalDecision::Allowed {
                source: DecisionSource::User,
            } if !self.replaying => {
                self.asked.insert(call_id.to_owned());
                Ok(())
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
                self.calls.clear();
                self.asked.clear();
                if let Ok(mut status) = self.status.lock() {
                    status.input = 0;
                    status.output = 0;
                }
                submitted(text)
            }
            AgentEvent::Model { event, .. } => self.model_event(event),
            AgentEvent::ToolDecided {
                call_id,
                tool,
                action,
                decision,
                ..
            } => self.decision(call_id, tool, action.subject.as_deref(), decision),
            AgentEvent::ToolStarted { call, .. } if call.name == dino_core::todo::TODO => {
                self.break_stream()?;
                todo_started(call)
            }
            AgentEvent::ToolStarted { call, .. } => self.tool_started(call),
            AgentEvent::ContextWarning {
                input_tokens,
                window_tokens,
                source,
                documented_tokens,
                ..
            } => {
                self.break_stream()?;
                self.context_full
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                context_warning(*input_tokens, *window_tokens, *source, *documented_tokens)
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
            (Some(error), _) if error.starts_with("model-turn limit reached") => {
                format!("stopped: {error} · reply to carry on where it left off")
            }
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
    WRITES.fetch_add(1, Ordering::Relaxed);
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

fn context_warning(
    input_tokens: u64,
    window_tokens: u32,
    source: Option<WindowSource>,
    documented_tokens: Option<u32>,
) -> Result<(), SinkError> {
    write_out(&style::paint(
        WARN,
        &context_line(input_tokens, window_tokens, source, documented_tokens),
    ))
}

fn context_line(
    input_tokens: u64,
    window_tokens: u32,
    source: Option<WindowSource>,
    documented_tokens: Option<u32>,
) -> String {
    let percent = (input_tokens * 100) / u64::from(window_tokens).max(1);
    let which = match (source, documented_tokens) {
        (Some(WindowSource::Served), Some(documented)) => {
            format!(", served; documented {documented}")
        }
        (Some(WindowSource::Served), None) => ", served".to_owned(),
        (Some(WindowSource::Documented), _) => ", documented".to_owned(),
        (None, _) => String::new(),
    };
    format!(
        "  the context window is {percent}% full ({input_tokens} of {window_tokens}{which}); a new \
         session starts clean\n"
    )
}

fn handed_over(session: u32, reason: &str, pending: usize) -> Result<(), SinkError> {
    write_out(&style::paint(
        SLATE,
        &format!("\n  ── session {session}: {reason}; {pending} item(s) pending ──\n"),
    ))
}

/// The checklist itself, rather than a one-line summary of the call.
fn todo_started(call: &dino_protocol::ToolCall) -> Result<(), SinkError> {
    let list = dino_core::todo::items(&call.arguments)
        .map(|items| dino_core::todo::render(&items))
        .unwrap_or_default();
    let indented = list.lines().fold(String::new(), |mut block, line| {
        block.push_str("    ");
        block.push_str(line);
        block.push('\n');
        block
    });
    let marker = if style::enabled() { "#" } else { "›" };
    write_out(&style::paint(
        SLATE,
        &format!("  {marker} todo\n{indented}"),
    ))
}

impl Renderer {
    fn tool_started(&mut self, call: &ToolCall) -> Result<(), SinkError> {
        self.break_stream()?;
        if !style::enabled() {
            // The machine is about to do something, so the machine's colour.
            // The line stays open for the result to finish it.
            let line = style::paint(SLATE, &format!("  › {}", describe_call(call)));
            write_out(&line)?;
            self.open_call = Some(call.clone());
            return Ok(());
        }
        let entry = Entry {
            call: call.clone(),
            mark: Mark::Running,
            gist: None,
        };
        let joined = match self.last.as_mut() {
            Some(last) if last.folds && last.is_last() && calls::explores(&call.name) => {
                last.entries.push(entry);
                true
            }
            _ => {
                self.last = Some(Last {
                    writes: 0,
                    entries: vec![entry],
                    folds: calls::explores(&call.name),
                });
                false
            }
        };
        self.draw_last(joined)?;
        self.spinner.running();
        Ok(())
    }

    /// Draws the last call line over itself, or as a new line.
    fn draw_last(&mut self, in_place: bool) -> Result<(), SinkError> {
        let Some(last) = &self.last else {
            return Ok(());
        };
        let up = if in_place { BACK_UP } else { "" };
        write_out(&format!("{up}{}\n", last.line()))?;
        if let Some(last) = &mut self.last {
            last.writes = WRITES.load(Ordering::Relaxed);
        }
        Ok(())
    }

    /// Marks the call done or failed on its line, with the gist of its result.
    fn finish_line(&mut self, result: &ToolResult) -> Result<(), SinkError> {
        let Some(last) = self.last.as_mut() else {
            return Ok(());
        };
        let in_place = last.is_last();
        let Some(entry) = last
            .entries
            .iter_mut()
            .find(|entry| entry.call.id == result.call_id)
        else {
            // A refused call has had its line already.
            return Ok(());
        };
        let gist = crate::summary::summarise(&entry.call, &result.output);
        let failed = matches!(gist, Some((crate::summary::Tone::Failed, _)));
        entry.mark = if failed { Mark::Failed } else { Mark::Done };
        entry.gist = gist;
        let call = entry.call.clone();
        self.draw_last(in_place)?;
        // A person who approved it at the prompt has just read it there.
        if call.name == "apply_patch" && !failed && !self.asked.contains(&call.id) {
            const PREVIEW_LINES: usize = 10;
            let rows = crate::diff::of_call(&call, None).unwrap_or_default();
            let width = crate::markdown::terminal_width().saturating_sub(1);
            write_out(&crate::diff::paint(&rows, PREVIEW_LINES, "      ", width))?;
        }
        Ok(())
    }

    /// Finishes the call's open line with the gist of its result.
    fn tool_finished(&mut self, result: &ToolResult) -> Result<(), SinkError> {
        if style::enabled() {
            return self.finish_line(result);
        }
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
        if call.name == "apply_patch"
            && tone == crate::summary::Tone::Fine
            && !self.asked.contains(&call.id)
        {
            write_out(&patch_preview(&call))?;
        }
        Ok(())
    }
}

/// The changed lines of a patch, beneath its call: removed in terracotta,
/// added in green, behind a faint gutter.
fn patch_preview(call: &ToolCall) -> String {
    use crate::diff::Row;
    const PREVIEW_LINES: usize = 10;
    let text = |parts: &[(bool, String)]| {
        let line: String = parts.iter().map(|(_, text)| text.as_str()).collect();
        cap(&line, MAX_SUMMARY_BYTES)
    };
    let changed: Vec<String> = crate::diff::of_call(call, None)
        .unwrap_or_default()
        .iter()
        .filter_map(|row| match row {
            Row::Removed { parts, .. } => Some(style::paint(RUST, &format!("- {}", text(parts)))),
            Row::Added { parts, .. } => Some(style::paint(OK, &format!("+ {}", text(parts)))),
            Row::Same { .. } | Row::Gap => None,
        })
        .collect();
    let shown = calls::shown(changed.len(), PREVIEW_LINES);
    let hidden = changed.len() - shown;
    changed
        .into_iter()
        .take(shown)
        .chain((hidden > 0).then(|| style::paint(FAINT, &format!("… {hidden} more"))))
        .fold(String::new(), |mut block, line| {
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
    use dino_core::session::EventSink as _;
    use dino_protocol::AgentEvent;
    use dino_protocol::WindowSource;

    use super::Renderer;
    use super::context_line;

    #[test]
    fn a_context_warning_raises_the_flag_auto_compaction_watches() {
        let mut renderer = Renderer::new(false);
        let full = renderer.context_handle();

        renderer
            .emit(&AgentEvent::ContextWarning {
                model_turn: 3,
                input_tokens: 190_000,
                window_tokens: 262_144,
                source: Some(WindowSource::Documented),
                documented_tokens: None,
            })
            .unwrap();

        assert!(full.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn a_context_warning_names_which_window() {
        let served = context_line(750, 1_000, Some(WindowSource::Served), Some(10_000));
        assert!(
            served.contains("750 of 1000, served; documented 10000"),
            "{served}"
        );
        let documented = context_line(750, 1_000, Some(WindowSource::Documented), None);
        assert!(
            documented.contains("750 of 1000, documented"),
            "{documented}"
        );
        assert!(!documented.contains("served"));
        // A log from before the field existed keeps the old sentence.
        let older = context_line(190_000, 262_144, None, None);
        assert!(older.contains("190000 of 262144);"), "{older}");
        assert!(!older.contains("documented"));
        assert!(!older.contains("served"));
    }
}
