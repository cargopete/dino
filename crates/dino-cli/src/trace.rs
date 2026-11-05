//! A session's trajectory: each submission, its turns and calls, and where the
//! time went.
//!
//! Read from the log alone. Every record has carried `unix_millis` since the
//! first log was written, and the loop runs a turn's calls one after another,
//! so a duration is the gap between two records: a turn's model time runs from
//! the record before its first event to its `finished`, a call's wait from the
//! record before its decision to the decision, its run from start to finish.

use std::fmt::Write as _;
use std::io::BufRead as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use anyhow::Result;
use dino_core::session::RunOutcome;
use dino_protocol::AgentEvent;
use dino_protocol::ApprovalDecision;
use dino_protocol::DecisionSource;
use dino_protocol::ModelEvent;
use dino_protocol::StopReason;
use dino_protocol::TokenUsage;
use dino_protocol::ToolCall;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::calls;
use crate::config;
use crate::style;
use crate::style::EMBER;
use crate::style::FAINT;
use crate::style::MUTED;
use crate::style::TEXT;
use crate::style::WARN;

/// The most of a turn's text, a call's arguments or its output kept by
/// default. The log has the rest.
pub const CAP_BYTES: usize = 2 * 1_024;

/// What `--full` keeps of each.
const FULL_CAP_BYTES: usize = 16 * 1_024;

#[derive(Debug, clap::Args)]
pub struct TraceArgs {
    /// Session id, or the path of a session log. Defaults to the most recent
    /// session in this workspace.
    pub session: Option<String>,
    /// Workspace root. Defaults to the current directory.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// Show only the last N submissions.
    #[arg(long)]
    pub last: Option<usize>,
    /// Show each call's arguments and more of its output.
    #[arg(long)]
    pub full: bool,
    /// Print the trajectory as JSON.
    #[arg(long, conflicts_with = "full")]
    pub json: bool,
    /// Never emit terminal colour.
    #[arg(long)]
    pub plain: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct Trajectory {
    /// The opening record's session, as written.
    pub session: Option<Value>,
    pub submissions: Vec<Submission>,
    pub totals: Totals,
    /// Lines that were not a record, or held an event this build does not know.
    pub skipped: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct Submission {
    /// Absent in a log written before submissions were recorded.
    pub text: Option<String>,
    pub unix_millis: u64,
    /// Absent while it runs, or when the log ends before it finished.
    pub wall_ms: Option<u64>,
    pub stop_reason: Option<StopReason>,
    pub error: Option<String>,
    pub turns: Vec<Turn>,
    pub notes: Vec<String>,
    pub totals: Totals,
}

#[derive(Debug, Default, Serialize)]
pub struct Turn {
    pub number: u32,
    /// From the request to the end of the response, retries included.
    pub model_ms: Option<u64>,
    /// From the request to the response starting, for the attempt that stood.
    pub first_response_ms: Option<u64>,
    pub attempts: u32,
    pub stop: Option<StopReason>,
    pub usage: TokenUsage,
    pub text: String,
    pub calls: Vec<Call>,
    pub notes: Vec<String>,
    #[serde(skip)]
    requested_at: u64,
    #[serde(skip)]
    text_cut: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct Call {
    pub id: String,
    pub tool: String,
    pub summary: String,
    pub arguments: String,
    /// `policy`, `session_rule`, `saved_rule` or `user`; `refused` with its
    /// reason. Absent when the call failed before any policy saw it.
    pub decision: Option<String>,
    pub refusal: Option<String>,
    /// From the record before the decision to the decision: a person's time
    /// when one was asked, close to nothing otherwise.
    pub decided_ms: Option<u64>,
    pub run_ms: Option<u64>,
    /// What the result came to, in the words beside a call in a live session.
    pub gist: Option<String>,
    /// The result as a person reads it: a process's stdout and stderr rather
    /// than the JSON that carries them.
    pub output: Option<String>,
    pub is_error: bool,
    #[serde(skip)]
    started_at: Option<u64>,
    #[serde(skip)]
    made: Option<ToolCall>,
}

impl Call {
    /// Whether a person decided, so the decision's time was theirs.
    fn asked(&self) -> bool {
        self.decision.as_deref() == Some("user") || self.refusal.is_some()
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct Totals {
    pub turns: usize,
    pub calls: usize,
    pub model_ms: u64,
    pub tools_ms: u64,
    pub approvals_ms: u64,
    pub wall_ms: u64,
}

impl Totals {
    fn add(&mut self, other: Self) {
        self.turns += other.turns;
        self.calls += other.calls;
        self.model_ms += other.model_ms;
        self.tools_ms += other.tools_ms;
        self.approvals_ms += other.approvals_ms;
        self.wall_ms += other.wall_ms;
    }
}

/// One line of a log, without its event decoded. Not a whole `SessionRecord`:
/// its `u128` times do not survive serde's buffering for a tagged enum.
#[derive(Deserialize)]
struct Line {
    record: String,
    #[serde(default)]
    unix_millis: Option<u64>,
    #[serde(default)]
    session: Option<Value>,
    #[serde(default)]
    event: Option<Value>,
    #[serde(default)]
    outcome: Option<RunOutcome>,
}

/// Reads the trajectory of the log at `path`, keeping `cap` bytes of each text.
///
/// # Errors
///
/// Returns an error when the log cannot be opened or read.
pub fn read(path: &Path, cap: usize) -> Result<Trajectory> {
    let file =
        std::fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut builder = Builder::new(cap);
    for line in std::io::BufReader::new(file).lines() {
        let line = line.with_context(|| format!("cannot read {}", path.display()))?;
        builder.line(&line);
    }
    Ok(builder.finish())
}

struct Builder {
    trajectory: Trajectory,
    last_at: u64,
    cap: usize,
}

impl Builder {
    fn new(cap: usize) -> Self {
        Self {
            trajectory: Trajectory::default(),
            last_at: 0,
            cap,
        }
    }

    fn line(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let Ok(line) = serde_json::from_str::<Line>(text) else {
            self.trajectory.skipped += 1;
            return;
        };
        let at = line.unix_millis.unwrap_or(self.last_at);
        match line.record.as_str() {
            "started" => {
                if self.trajectory.session.is_none() {
                    self.trajectory.session = line.session;
                }
            }
            "event" => match line.event.map(serde_json::from_value::<AgentEvent>) {
                Some(Ok(event)) => self.event(event, at),
                _ => self.trajectory.skipped += 1,
            },
            "finished" => {
                let submission = self.submission(at);
                submission.wall_ms = Some(at.saturating_sub(submission.unix_millis));
                if let Some(outcome) = line.outcome {
                    submission.stop_reason = outcome.stop_reason;
                    submission.error = outcome.error;
                }
            }
            _ => self.trajectory.skipped += 1,
        }
        self.last_at = at;
    }

    /// The submission under way, or one standing in for a log that predates
    /// the event.
    fn submission(&mut self, at: u64) -> &mut Submission {
        if self.trajectory.submissions.is_empty() {
            self.trajectory.submissions.push(Submission {
                unix_millis: at,
                ..Submission::default()
            });
        }
        self.trajectory
            .submissions
            .last_mut()
            .expect("a submission was just pushed")
    }

    fn turn(&mut self, number: u32, at: u64) -> &mut Turn {
        let requested_at = self.last_at;
        let submission = self.submission(at);
        if submission
            .turns
            .last()
            .is_none_or(|turn| turn.number != number)
        {
            submission.turns.push(Turn {
                number,
                requested_at,
                ..Turn::default()
            });
        }
        submission.turns.last_mut().expect("a turn was just pushed")
    }

    fn call(&mut self, id: &str) -> Option<&mut Call> {
        self.trajectory
            .submissions
            .last_mut()?
            .turns
            .iter_mut()
            .rev()
            .flat_map(|turn| turn.calls.iter_mut())
            .find(|call| call.id == id)
    }

    fn note(&mut self, at: u64, text: String) {
        let submission = self.submission(at);
        match submission.turns.last_mut() {
            Some(turn) => turn.notes.push(text),
            None => submission.notes.push(text),
        }
    }

    fn new_call(&self, call: &ToolCall) -> Call {
        let arguments = serde_json::to_string_pretty(&call.arguments).unwrap_or_default();
        let summary = crate::render::describe_call(call);
        let mut first = summary.lines().next().unwrap_or_default().to_owned();
        if first.len() < summary.trim_end().len() {
            first.push_str(" …");
        }
        Call {
            id: call.id.clone(),
            tool: call.name.clone(),
            summary: first,
            arguments: dino_core::text::capped_head(&arguments, self.cap),
            made: Some(call.clone()),
            ..Call::default()
        }
    }

    fn event(&mut self, event: AgentEvent, at: u64) {
        let since = at.saturating_sub(self.last_at);
        match event {
            AgentEvent::Submitted { text } => self.trajectory.submissions.push(Submission {
                text: Some(text),
                unix_millis: at,
                ..Submission::default()
            }),
            AgentEvent::Model { model_turn, event } => {
                let cap = self.cap;
                let call = match &event {
                    ModelEvent::ToolCallCompleted { call } => Some(self.new_call(call)),
                    _ => None,
                };
                let turn = self.turn(model_turn, at);
                let elapsed = at.saturating_sub(turn.requested_at);
                match event {
                    ModelEvent::Started { .. } => {
                        turn.attempts += 1;
                        turn.first_response_ms = Some(elapsed);
                    }
                    ModelEvent::TextDelta { text } => {
                        let room = cap.saturating_sub(turn.text.len());
                        if text.len() <= room {
                            turn.text.push_str(&text);
                        } else {
                            let mut end = room;
                            while !text.is_char_boundary(end) {
                                end -= 1;
                            }
                            turn.text.push_str(&text[..end]);
                            turn.text_cut += text.len() - end;
                        }
                    }
                    ModelEvent::Usage { usage } => {
                        turn.usage.input_tokens += usage.input_tokens;
                        turn.usage.cached_input_tokens += usage.cached_input_tokens;
                        turn.usage.cache_write_input_tokens += usage.cache_write_input_tokens;
                        turn.usage.output_tokens += usage.output_tokens;
                        turn.usage.reasoning_tokens += usage.reasoning_tokens;
                    }
                    ModelEvent::Finished { reason } => {
                        turn.stop = Some(reason);
                        turn.model_ms = Some(elapsed);
                    }
                    ModelEvent::ToolCallCompleted { .. } => turn.calls.extend(call),
                    ModelEvent::ReasoningDelta { .. }
                    | ModelEvent::ToolCallStarted { .. }
                    | ModelEvent::ToolCallArgumentsDelta { .. } => {}
                }
            }
            AgentEvent::ToolDecided {
                call_id, decision, ..
            } => {
                if let Some(call) = self.call(&call_id) {
                    call.decided_ms = Some(since);
                    match decision {
                        ApprovalDecision::Allowed { source } => {
                            call.decision = Some(source_name(source).to_owned());
                        }
                        ApprovalDecision::Denied { reason } => {
                            call.decision = Some("refused".to_owned());
                            call.refusal = Some(reason);
                        }
                    }
                }
            }
            AgentEvent::ToolStarted { model_turn, call } => {
                if self.call(&call.id).is_none() {
                    let added = self.new_call(&call);
                    self.turn(model_turn, at).calls.push(added);
                }
                if let Some(found) = self.call(&call.id) {
                    found.started_at = Some(at);
                }
            }
            AgentEvent::ToolFinished { result, .. } => {
                let cap = self.cap;
                if let Some(call) = self.call(&result.call_id) {
                    call.run_ms = call.started_at.map(|started| at.saturating_sub(started));
                    call.gist = call
                        .made
                        .as_ref()
                        .and_then(|made| crate::summary::summarise(made, &result.output))
                        .map(|(_, gist)| gist);
                    let readable = readable(&result.output.content);
                    call.output = Some(dino_core::text::capped_output(&readable, cap));
                    call.is_error = result.output.is_error;
                }
            }
            other => {
                if let Some(text) = describe_note(&other) {
                    self.note(at, text);
                }
            }
        }
    }

    fn finish(mut self) -> Trajectory {
        let mut all = Totals::default();
        for submission in &mut self.trajectory.submissions {
            let mut totals = Totals {
                turns: submission.turns.len(),
                wall_ms: submission.wall_ms.unwrap_or(0),
                ..Totals::default()
            };
            for turn in &mut submission.turns {
                if turn.text_cut > 0 {
                    let _ = write!(turn.text, " … ({} bytes more in the log)", turn.text_cut);
                }
                totals.model_ms += turn.model_ms.unwrap_or(0);
                totals.calls += turn.calls.len();
                for call in &turn.calls {
                    totals.tools_ms += call.run_ms.unwrap_or(0);
                    if call.asked() {
                        totals.approvals_ms += call.decided_ms.unwrap_or(0);
                    }
                }
            }
            submission.totals = totals;
            all.add(totals);
        }
        self.trajectory.totals = all;
        self.trajectory
    }
}

/// A result's text: a process's streams, which may be empty, cargo's output or
/// a file's lines where the JSON has them, else the JSON laid out, else the text.
fn readable(content: &str) -> String {
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(content) else {
        return content.to_owned();
    };
    let text = |key: &str| {
        fields
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
    };
    if fields.contains_key("stdout") || fields.contains_key("stderr") {
        return match (text("stdout"), text("stderr")) {
            (Some(out), Some(err)) => format!("{}\n{err}", out.trim_end()),
            (Some(out), None) => out.to_owned(),
            (None, Some(err)) => err.to_owned(),
            (None, None) => String::new(),
        };
    }
    if let Some(text) = text("output").or_else(|| text("content")) {
        return text.to_owned();
    }
    serde_json::to_string_pretty(&fields).unwrap_or_else(|_| content.to_owned())
}

fn source_name(source: DecisionSource) -> &'static str {
    match source {
        DecisionSource::Policy => "policy",
        DecisionSource::SessionRule => "session_rule",
        DecisionSource::SavedRule => "saved_rule",
        DecisionSource::User => "user",
    }
}

/// A line for an event that is neither a turn nor a call, or `None` for one a
/// trajectory leaves out.
fn describe_note(event: &AgentEvent) -> Option<String> {
    Some(match event {
        AgentEvent::Steered { text, .. } => format!("you, during the run: {text}"),
        AgentEvent::Noted { note, .. } => format!("noted: {note}"),
        AgentEvent::SentBack { reason, .. } => format!("sent back: {reason}"),
        AgentEvent::Reviewed {
            objections, error, ..
        } => match (error, objections.len()) {
            (Some(error), _) => format!("review failed: {error}"),
            (None, 0) => "reviewed: no objections".to_owned(),
            (None, count) => format!("reviewed: {count} objection(s)"),
        },
        AgentEvent::TurnRetried { attempt, error, .. } => {
            format!("retried, attempt {attempt}: {error}")
        }
        AgentEvent::WaitingForNetwork {
            error,
            retry_in_seconds,
            ..
        } => format!("waiting for the network, retry in {retry_in_seconds}s: {error}"),
        AgentEvent::TurnsRunningOut { turns_left, .. } => {
            format!("told {turns_left} turn(s) are left")
        }
        AgentEvent::ContextWarning {
            input_tokens,
            window_tokens,
            ..
        } => format!("context {input_tokens} of {window_tokens} tokens"),
        AgentEvent::Elided {
            bytes_reclaimed, ..
        } => format!("history reduced by {bytes_reclaimed} bytes"),
        AgentEvent::TurnCutOff { .. } => "cut off for length; history reduced".to_owned(),
        AgentEvent::HandedOver {
            session, reason, ..
        } => format!("handed over to session {session}: {reason}"),
        _ => return None,
    })
}

/// A duration as a person reads it: `82ms`, `1.4s`, `6m 05s`, `1h 02m`.
#[must_use]
pub fn took(ms: u64) -> String {
    match ms {
        0..1_000 => format!("{ms}ms"),
        1_000..60_000 => format!("{}.{}s", ms / 1_000, ms % 1_000 / 100),
        60_000..3_600_000 => format!("{}m {:02}s", ms / 60_000, ms / 1_000 % 60),
        _ => format!("{}h {:02}m", ms / 3_600_000, ms / 60_000 % 60),
    }
}

fn tokens(count: u64) -> String {
    if count >= 1_000 {
        format!("{}.{}k", count / 1_000, count % 1_000 / 100)
    } else {
        count.to_string()
    }
}

/// Prints a session's trajectory.
///
/// # Errors
///
/// Returns an error when there is no such session, or its log cannot be read.
pub fn run(args: &TraceArgs) -> Result<ExitCode> {
    style::enable(args.plain || args.json);
    let workspace = config::resolve_workspace(args.workspace.as_deref())?;
    let (id, path) = crate::replay::locate(args.session.as_deref(), &workspace)?;
    let cap = if args.full { FULL_CAP_BYTES } else { CAP_BYTES };
    let mut trajectory = read(&path, cap)?;
    let hidden = keep_last(&mut trajectory, args.last);
    if args.json {
        println!("{}", serde_json::to_string(&trajectory)?);
        return Ok(ExitCode::SUCCESS);
    }
    let width = crate::markdown::terminal_width().clamp(40, 160);
    print!("{}", render(&id, &trajectory, hidden, args.full, width));
    Ok(ExitCode::SUCCESS)
}

/// Drops all but the last `count` submissions, returning how many went.
pub fn keep_last(trajectory: &mut Trajectory, count: Option<usize>) -> usize {
    let Some(count) = count.filter(|count| *count > 0) else {
        return 0;
    };
    let hidden = trajectory.submissions.len().saturating_sub(count);
    trajectory.submissions.drain(..hidden);
    hidden
}

fn render(id: &str, trajectory: &Trajectory, hidden: usize, full: bool, width: usize) -> String {
    let mut out = String::new();
    let field = |name: &str| {
        trajectory
            .session
            .as_ref()
            .and_then(|session| session.get(name))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned()
    };
    out.push_str(&crate::render::status_block(&[
        ("session", id.to_owned()),
        ("model", field("model_key")),
        ("workspace", field("workspace_root")),
    ]));
    if hidden > 0 {
        let _ = writeln!(
            out,
            "{}",
            style::paint(
                FAINT,
                &format!("  … {hidden} earlier submission(s) not shown")
            )
        );
    }
    for submission in &trajectory.submissions {
        render_submission(&mut out, submission, full, width);
    }
    if trajectory.submissions.len() > 1 {
        let _ = writeln!(
            out,
            "\n  {}",
            style::paint(
                MUTED,
                &summary(
                    &trajectory.totals,
                    format!("in all {}", took(trajectory.totals.wall_ms))
                )
            )
        );
    }
    if trajectory.skipped > 0 {
        let _ = writeln!(
            out,
            "  {}",
            style::paint(
                FAINT,
                &format!("{} line(s) of the log not read", trajectory.skipped)
            )
        );
    }
    out
}

fn summary(totals: &Totals, wall: String) -> String {
    let mut parts = vec![wall];
    parts.push(format!("{} turn(s)", totals.turns));
    parts.push(format!("{} call(s)", totals.calls));
    parts.push(format!("model {}", took(totals.model_ms)));
    parts.push(format!("tools {}", took(totals.tools_ms)));
    if totals.approvals_ms > 0 {
        parts.push(format!("approvals {}", took(totals.approvals_ms)));
    }
    parts.join(" · ")
}

fn render_submission(out: &mut String, submission: &Submission, full: bool, width: usize) {
    let asked = submission
        .text
        .as_deref()
        .map_or("(a log from before submissions were recorded)", str::trim);
    let first = asked.lines().next().unwrap_or_default();
    let _ = writeln!(
        out,
        "\n{}",
        style::paint(EMBER, &calls::cut(&format!("› {first}"), width))
    );
    let mut line = summary(
        &submission.totals,
        submission
            .wall_ms
            .map_or_else(|| "unfinished".to_owned(), took),
    );
    if let Some(reason) = submission.stop_reason {
        let _ = write!(line, " · {}", stop_name(reason));
    }
    let _ = writeln!(out, "  {}", style::paint(MUTED, &line));
    if let Some(error) = &submission.error {
        let _ = writeln!(
            out,
            "  {}",
            style::paint(WARN, &calls::cut(error, width - 2))
        );
    }
    for note in &submission.notes {
        let _ = writeln!(
            out,
            "  {}",
            style::paint(FAINT, &calls::cut(note, width - 2))
        );
    }
    for turn in &submission.turns {
        render_turn(out, turn, full, width);
    }
}

fn stop_name(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::MaxTokens => "max_tokens",
        StopReason::Refusal => "refusal",
        StopReason::Cancelled => "cancelled",
        StopReason::Blocked => "blocked",
    }
}

fn render_turn(out: &mut String, turn: &Turn, full: bool, width: usize) {
    let mut head = format!("turn {}", turn.number);
    match turn.model_ms {
        Some(ms) => {
            let _ = write!(head, "  model {}", took(ms));
        }
        None => head.push_str("  model unfinished"),
    }
    if let Some(first) = turn.first_response_ms {
        let _ = write!(head, " (first response {})", took(first));
    }
    if turn.attempts > 1 {
        let _ = write!(head, " · {} attempts", turn.attempts);
    }
    let _ = write!(
        head,
        " · {} in · {} out",
        tokens(turn.usage.input_tokens),
        tokens(turn.usage.output_tokens)
    );
    if let Some(stop) = turn.stop {
        let _ = write!(head, " · {}", stop_name(stop));
    }
    let _ = writeln!(out, "  {}", style::paint(TEXT, &head));
    let text = turn.text.trim();
    if !text.is_empty() {
        let limit = if full { usize::MAX } else { 3 };
        write_block(out, text, limit, width, MUTED, full);
    }
    for call in &turn.calls {
        render_call(out, call, full, width);
    }
    for note in &turn.notes {
        let _ = writeln!(
            out,
            "    {}",
            style::paint(FAINT, &calls::cut(note, width - 4))
        );
    }
}

fn render_call(out: &mut String, call: &Call, full: bool, width: usize) {
    let mark = if call.refusal.is_some() {
        calls::Mark::Refused
    } else if call.is_error {
        calls::Mark::Failed
    } else if call.output.is_some() {
        calls::Mark::Done
    } else {
        calls::Mark::Running
    };
    let mut timing = Vec::new();
    if let Some(ms) = call.decided_ms
        && call.asked()
    {
        let verb = if call.refusal.is_some() {
            "refused after"
        } else {
            "approved after"
        };
        timing.push(format!("{verb} {}", took(ms)));
    }
    if let Some(ms) = call.run_ms {
        timing.push(format!("ran {}", took(ms)));
    }
    if let Some(gist) = &call.gist {
        timing.push(calls::cut(gist, 32));
    }
    let timing = timing.join(" · ");
    let room = width.saturating_sub(8 + calls::width(&timing));
    let description = calls::cut(&call.summary, room.max(16));
    let gap = width.saturating_sub(8 + calls::width(&description) + calls::width(&timing));
    let colour = if mark == calls::Mark::Done {
        TEXT
    } else {
        WARN
    };
    let _ = writeln!(
        out,
        "    {} {}{}{} {}",
        calls::icon(&call.tool),
        style::paint(colour, &description),
        " ".repeat(gap.max(2)),
        style::paint(MUTED, &timing),
        style::paint(colour, &mark.glyph().to_string()),
    );
    if let Some(reason) = &call.refusal {
        let _ = writeln!(
            out,
            "      {}",
            style::paint(WARN, &calls::cut(reason, width - 6))
        );
    }
    if full {
        write_block(out, &call.arguments, usize::MAX, width, FAINT, full);
    }
    if let Some(output) = call
        .output
        .as_ref()
        .filter(|output| !output.trim().is_empty())
    {
        let limit = if full { usize::MAX } else { 3 };
        write_block(out, output.trim_end(), limit, width, FAINT, full);
    }
}

/// With `full`, each line is left whole for the terminal to wrap; otherwise it
/// is cut to the terminal width.
fn write_block(
    out: &mut String,
    text: &str,
    limit: usize,
    width: usize,
    colour: style::Colour,
    full: bool,
) {
    let lines: Vec<&str> = text.lines().collect();
    let shown = calls::shown(lines.len(), limit);
    for line in &lines[..shown] {
        let line = if full {
            (*line).to_owned()
        } else {
            calls::cut(line, width.saturating_sub(6))
        };
        let _ = writeln!(out, "      {}", style::paint(colour, &line));
    }
    if shown < lines.len() {
        let _ = writeln!(
            out,
            "      {}",
            style::paint(FAINT, &format!("… {} more line(s)", lines.len() - shown))
        );
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn record(at: u64, event: &Value) -> String {
        json!({ "record": "event", "seq": 0, "unix_millis": at, "event": event }).to_string()
    }

    fn model(at: u64, turn: u32, event: &Value) -> String {
        record(
            at,
            &json!({ "type": "model", "model_turn": turn, "event": event }),
        )
    }

    fn built(lines: &[String]) -> Trajectory {
        let mut builder = Builder::new(CAP_BYTES);
        for line in lines {
            builder.line(line);
        }
        builder.finish()
    }

    fn shell_call(id: &str) -> Value {
        json!({ "id": id, "name": "shell", "arguments": { "command": "cargo test" } })
    }

    /// One submission: a turn that asks for a call a person approves, then a
    /// turn that answers.
    fn session() -> Vec<String> {
        vec![
            json!({ "record": "started", "seq": 0, "unix_millis": 1_000,
                    "session": { "model_key": "m", "workspace_root": "/w" } })
            .to_string(),
            record(
                1_000,
                &json!({ "type": "submitted", "text": "run the tests" }),
            ),
            model(1_400, 1, &json!({ "type": "started", "response_id": null })),
            model(
                1_500,
                1,
                &json!({ "type": "text_delta", "text": "Running." }),
            ),
            model(
                2_000,
                1,
                &json!({ "type": "tool_call_completed", "call": shell_call("c1") }),
            ),
            model(
                2_000,
                1,
                &json!({ "type": "usage", "usage": { "input_tokens": 1200, "cached_input_tokens": 0,
                    "output_tokens": 30, "reasoning_tokens": 0 } }),
            ),
            model(
                2_000,
                1,
                &json!({ "type": "finished", "reason": "tool_use" }),
            ),
            record(
                9_000,
                &json!({ "type": "tool_decided", "model_turn": 1, "call_id": "c1", "tool": "shell",
                    "action": { "class": "process", "subject": "cargo test" },
                    "decision": { "outcome": "allowed", "source": "user" } }),
            ),
            record(
                9_000,
                &json!({ "type": "tool_started", "model_turn": 1, "call": shell_call("c1") }),
            ),
            record(
                12_500,
                &json!({ "type": "tool_finished", "model_turn": 1,
                    "result": { "call_id": "c1", "output": { "is_error": false, "content":
                        r#"{"command":"cargo test","exit_code":0,"timed_out":false,"stdout":"7 passed\n","stderr":""}"# } } }),
            ),
            model(
                13_000,
                2,
                &json!({ "type": "started", "response_id": null }),
            ),
            model(
                13_100,
                2,
                &json!({ "type": "text_delta", "text": "All pass." }),
            ),
            model(
                14_500,
                2,
                &json!({ "type": "finished", "reason": "end_turn" }),
            ),
            json!({ "record": "finished", "seq": 0, "unix_millis": 14_600,
                    "outcome": { "stop_reason": "end_turn", "model_turns": 2, "error": null } })
            .to_string(),
        ]
    }

    #[test]
    fn each_turn_and_call_is_timed_from_the_records_around_it() {
        let trajectory = built(&session());
        assert_eq!(trajectory.skipped, 0);
        let submission = &trajectory.submissions[0];
        assert_eq!(submission.text.as_deref(), Some("run the tests"));
        assert_eq!(submission.wall_ms, Some(13_600));
        assert_eq!(submission.stop_reason, Some(StopReason::EndTurn));

        let first = &submission.turns[0];
        assert_eq!(first.model_ms, Some(1_000));
        assert_eq!(first.first_response_ms, Some(400));
        assert_eq!(first.usage.input_tokens, 1_200);
        assert_eq!(first.text, "Running.");

        let call = &first.calls[0];
        assert_eq!(call.summary, "shell cargo test");
        assert_eq!(call.decision.as_deref(), Some("user"));
        assert_eq!(call.decided_ms, Some(7_000), "the person's time");
        assert_eq!(call.run_ms, Some(3_500));
        assert_eq!(call.output.as_deref(), Some("7 passed\n"));
        assert_eq!(call.gist.as_deref(), Some("ok"));

        // The second turn was requested when the call finished.
        assert_eq!(submission.turns[1].model_ms, Some(2_000));
        assert_eq!(submission.turns[1].first_response_ms, Some(500));

        let totals = trajectory.totals;
        assert_eq!((totals.turns, totals.calls), (2, 1));
        assert_eq!(totals.model_ms, 3_000);
        assert_eq!(totals.tools_ms, 3_500);
        assert_eq!(totals.approvals_ms, 7_000);
    }

    #[test]
    fn a_decision_by_policy_is_not_counted_as_a_wait() {
        let lines: Vec<String> = session()
            .into_iter()
            .map(|line| line.replace(r#""source":"user""#, r#""source":"policy""#))
            .collect();
        let trajectory = built(&lines);
        let call = &trajectory.submissions[0].turns[0].calls[0];
        assert_eq!(call.decision.as_deref(), Some("policy"));
        assert_eq!(trajectory.totals.approvals_ms, 0);
    }

    #[test]
    fn a_refused_call_keeps_its_reason_and_never_ran() {
        let mut lines = session();
        lines[7] = record(
            3_000,
            &json!({ "type": "tool_decided", "model_turn": 1, "call_id": "c1", "tool": "shell",
                "action": { "class": "process", "subject": "cargo test" },
                "decision": { "outcome": "denied", "reason": "not now" } }),
        );
        lines.remove(8);
        let trajectory = built(&lines);
        let call = &trajectory.submissions[0].turns[0].calls[0];
        assert_eq!(call.refusal.as_deref(), Some("not now"));
        assert_eq!(call.run_ms, None);
        assert_eq!(call.decided_ms, Some(1_000));
    }

    #[test]
    fn a_log_from_before_submissions_were_recorded_still_reads() {
        let lines: Vec<String> = session()
            .into_iter()
            .filter(|line| !line.contains(r#""type":"submitted""#))
            .collect();
        let trajectory = built(&lines);
        assert_eq!(trajectory.submissions.len(), 1);
        assert_eq!(trajectory.submissions[0].text, None);
        assert_eq!(trajectory.submissions[0].turns.len(), 2);
    }

    #[test]
    fn a_torn_last_line_and_an_unknown_event_are_counted_not_fatal() {
        let mut lines = session();
        lines.insert(3, record(1_450, &json!({ "type": "from_the_future" })));
        lines.push(r#"{"record":"event","seq":99,"unix_mi"#.to_owned());
        let trajectory = built(&lines);
        assert_eq!(trajectory.skipped, 2);
        assert_eq!(trajectory.submissions[0].turns.len(), 2);
    }

    #[test]
    fn long_text_and_output_are_cut_and_say_so() {
        let mut lines = session();
        let long = "x".repeat(CAP_BYTES * 2);
        lines[3] = model(1_500, 1, &json!({ "type": "text_delta", "text": long }));
        lines[9] = record(
            12_500,
            &json!({ "type": "tool_finished", "model_turn": 1,
                "result": { "call_id": "c1", "output": { "content": long, "is_error": true } } }),
        );
        let trajectory = built(&lines);
        let turn = &trajectory.submissions[0].turns[0];
        assert!(turn.text.len() < CAP_BYTES + 64, "{}", turn.text.len());
        assert!(
            turn.text.ends_with("bytes more in the log)"),
            "{}",
            turn.text
        );
        let output = turn.calls[0].output.as_deref().unwrap();
        assert!(output.len() < CAP_BYTES + 64, "{}", output.len());
        assert!(turn.calls[0].is_error);
    }

    /// Another test may have turned colour on for the whole process.
    fn visible(painted: &str) -> String {
        let mut out = String::new();
        let mut chars = painted.chars();
        while let Some(character) = chars.next() {
            if character == '\u{1b}' {
                chars.by_ref().find(|next| *next == 'm');
            } else {
                out.push(character);
            }
        }
        out
    }

    #[test]
    fn the_rendering_names_each_wait_and_run() {
        let text = visible(&render("s1", &built(&session()), 0, false, 100));
        assert!(text.contains("› run the tests"), "{text}");
        assert!(
            text.contains("turn 1  model 1.0s (first response 400ms)"),
            "{text}"
        );
        assert!(
            text.contains("approved after 7.0s · ran 3.5s · ok ✓"),
            "{text}"
        );
        assert!(text.contains("7 passed"), "{text}");
        assert!(text.contains("approvals 7.0s"), "{text}");
    }

    #[test]
    fn full_output_is_neither_cut_to_width_nor_capped_at_40_lines() {
        let long = "y".repeat(200);
        let mut body = long.clone();
        for n in 0..50 {
            body.push_str(&format!("\nline {n}"));
        }
        let content = json!({ "stdout": body, "stderr": "" }).to_string();
        let mut lines = session();
        lines[9] = record(
            12_500,
            &json!({ "type": "tool_finished", "model_turn": 1,
                "result": { "call_id": "c1", "output": { "is_error": false, "content": content } } }),
        );
        let mut builder = Builder::new(FULL_CAP_BYTES);
        for line in &lines {
            builder.line(line);
        }
        let trajectory = builder.finish();

        let full = visible(&render("s1", &trajectory, 0, true, 100));
        assert!(
            full.contains(&long),
            "the long line survives whole with --full"
        );
        assert!(
            full.contains("line 49"),
            "the 50th line is shown with --full"
        );
        assert!(
            !full.contains("more line(s)"),
            "nothing is hidden with --full: {full}"
        );

        let trimmed = visible(&render("s1", &trajectory, 0, false, 100));
        assert!(
            !trimmed.contains(&long),
            "the long line is cut without --full"
        );
        assert!(
            trimmed.contains("more line(s)"),
            "output is capped without --full"
        );
    }

    #[test]
    fn only_the_last_submissions_are_kept_when_asked() {
        let mut lines = session();
        lines.extend(session().into_iter().skip(1));
        let mut trajectory = built(&lines);
        assert_eq!(trajectory.submissions.len(), 2);
        assert_eq!(keep_last(&mut trajectory, Some(1)), 1);
        assert_eq!(trajectory.submissions.len(), 1);
        assert_eq!(keep_last(&mut trajectory, None), 0);
    }

    #[test]
    fn durations_read_as_a_person_says_them() {
        assert_eq!(took(82), "82ms");
        assert_eq!(took(1_440), "1.4s");
        assert_eq!(took(365_000), "6m 05s");
        assert_eq!(took(3_720_000), "1h 02m");
    }
}
