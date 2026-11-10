//! `dino dash`: the sessions on this machine, what each is doing, and whether
//! it has stopped doing it.
//!
//! Read from the session logs alone, as `dino trace` is, so it needs no
//! cooperation from a running session and sees one that was started before it.
//! A log says what a session last did and when; the pid in its opening record
//! says whether the process is still there to do more. The rest is judgement,
//! and the thresholds below are where it is made.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::io::Read as _;
use std::io::Seek as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dino_protocol::AgentEvent;
use dino_protocol::ApprovalDecision;
use dino_protocol::ModelEvent;
use dino_protocol::ToolCall;
use hyper::Method;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use hyper_util::rt::TokioTimer;
use serde::Serialize;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use super::server;

/// Sessions shown from one workspace, newest first.
const PER_WORKSPACE: usize = 12;

/// A log untouched for longer than this, and finished or gone, is not listed.
const LISTED_FOR: Duration = Duration::from_secs(24 * 60 * 60);

/// A model that has said nothing for this long, mid-answer or before one, is
/// taken to be stalled. Slow reasoning models stream; they do not go quiet.
const MODEL_SILENT_MS: u64 = 120_000;

/// A tool call running longer than this is flagged. It may be a long build;
/// it is still the first place to look.
const TOOL_LONG_MS: u64 = 600_000;

/// A call that has been proposed but not decided for this long is waiting for
/// a person. Shorter than this is the loop getting to it.
const APPROVAL_GRACE_MS: u64 = 4_000;

/// With no pid to ask, a log this quiet and unfinished is abandoned.
const ABANDONED_MS: u64 = 15 * 60_000;

/// Identical calls in a row before they count as a loop.
const LOOP_CALLS: usize = 3;

/// A turn sent again this many times in one submission is not a blip.
const RETRIES: u32 = 3;

const STEPS_KEPT: usize = 8;
const NOTES_KEPT: usize = 4;
const TEXT_CAP: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Running, and something is wrong with how.
    Stuck,
    /// Waiting for a person to approve a call.
    NeedsYou,
    Working,
    /// The process is there and the last run is over: waiting for the next prompt.
    Idle,
    Done,
    Failed,
    /// The process exited without writing an end.
    Gone,
}

impl State {
    fn live(self) -> bool {
        matches!(
            self,
            Self::Stuck | Self::NeedsYou | Self::Working | Self::Idle
        )
    }

    /// Where it sorts: what wants attention first.
    fn rank(self) -> u8 {
        match self {
            Self::Stuck => 0,
            Self::NeedsYou => 1,
            Self::Working => 2,
            Self::Idle => 3,
            Self::Failed => 4,
            Self::Gone => 5,
            Self::Done => 6,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub tool: String,
    pub summary: String,
    pub ms: Option<u64>,
    pub error: bool,
}

#[derive(Debug, Serialize)]
pub struct Card {
    pub id: String,
    pub workspace: String,
    pub model: String,
    pub approval: String,
    pub pid: Option<u32>,
    pub state: State,
    /// What it is doing now, or how it ended.
    pub doing: String,
    /// How long it has been doing that.
    pub for_ms: u64,
    /// Why it counts as stuck, if it does.
    pub problems: Vec<String>,
    pub task: String,
    pub submissions: usize,
    pub turn: u32,
    pub max_turns: u32,
    pub calls: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub started_ms: u64,
    pub last_ms: u64,
    pub steps: Vec<Step>,
    pub notes: Vec<String>,
    /// What it is doing as one of a fixed few words, and what is wrong as
    /// others: the only account of it the blurb is given.
    #[serde(skip)]
    pub kind: &'static str,
    #[serde(skip)]
    pub trouble: Vec<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// A request is out and nothing has come back.
    Waiting,
    Responding,
    Thinking,
    Writing,
    Drafting,
    /// The turn is over; its calls are next.
    Calls,
}

#[derive(Debug, Clone)]
struct Open {
    id: String,
    tool: String,
    summary: String,
    decided: bool,
    started_at: Option<u64>,
    finished: bool,
}

/// What a log has said so far. Fed a line at a time, so a log that grows is
/// read once and not again.
#[derive(Debug, Clone)]
pub struct Digest {
    id: String,
    workspace: String,
    model: String,
    approval: String,
    pid: Option<u32>,
    max_turns: u32,
    started_ms: u64,
    last_ms: u64,
    task: String,
    submissions: usize,
    turn: u32,
    calls: usize,
    input_tokens: u64,
    output_tokens: u64,
    phase: Phase,
    drafting: String,
    open: Vec<Open>,
    steps: VecDeque<Step>,
    recent_calls: VecDeque<(String, String)>,
    retries: u32,
    network: Option<(String, u64)>,
    notes: VecDeque<String>,
    ended: Option<(bool, String)>,
    seen_start: bool,
    /// When a subagent's log beside this one was last written.
    children_ms: u64,
}

impl Digest {
    pub(super) fn new(path: &Path) -> Self {
        Self {
            id: path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default(),
            workspace: String::new(),
            model: String::new(),
            approval: String::new(),
            pid: None,
            max_turns: 0,
            started_ms: 0,
            last_ms: 0,
            task: String::new(),
            submissions: 0,
            turn: 0,
            calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            phase: Phase::Waiting,
            drafting: String::new(),
            open: Vec::new(),
            steps: VecDeque::new(),
            recent_calls: VecDeque::new(),
            retries: 0,
            network: None,
            notes: VecDeque::new(),
            ended: None,
            seen_start: false,
            children_ms: 0,
        }
    }

    fn note(&mut self, text: &str) {
        if self.notes.len() == NOTES_KEPT {
            self.notes.pop_front();
        }
        self.notes.push_back(cut(text, TEXT_CAP));
    }

    fn step(&mut self, step: Step) {
        if self.steps.len() == STEPS_KEPT {
            self.steps.pop_front();
        }
        self.steps.push_back(step);
    }

    /// Takes one line of the log. A line that is not a record is skipped: the
    /// tail of a log being written is not always whole.
    pub fn line(&mut self, line: &str) {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let at = record
            .get("unix_millis")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if at > 0 {
            self.last_ms = at;
        }
        match record.get("record").and_then(Value::as_str) {
            Some("started") => self.started(&record, at),
            Some("event") => {
                if let Some(event) = record
                    .get("event")
                    .and_then(|event| serde_json::from_value::<AgentEvent>(event.clone()).ok())
                {
                    self.event(event, at);
                }
            }
            Some("finished") => {
                let outcome = record.get("outcome");
                let error = outcome
                    .and_then(|outcome| outcome.get("error"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let stop = outcome
                    .and_then(|outcome| outcome.get("stop_reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("ended");
                self.ended = Some(match error {
                    Some(error) => (true, error),
                    None => (false, stop.replace('_', " ")),
                });
            }
            _ => {}
        }
    }

    fn started(&mut self, record: &Value, at: u64) {
        let Some(session) = record.get("session") else {
            return;
        };
        let text = |key: &str| {
            session
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        if !self.seen_start {
            self.started_ms = at;
            self.seen_start = true;
        }
        self.workspace = text("workspace_root");
        self.model = text("model_key");
        self.approval = text("approval_mode");
        self.pid = session
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok());
        self.max_turns = session
            .get("max_model_turns")
            .and_then(Value::as_u64)
            .and_then(|turns| u32::try_from(turns).ok())
            .unwrap_or(0);
        // A log resumed into starts again; whatever ran before is over.
        self.open.clear();
        self.ended = None;
    }

    #[allow(clippy::too_many_lines)]
    fn event(&mut self, event: AgentEvent, at: u64) {
        // Run at the prompt, after the run's end, so it reopens nothing.
        if let AgentEvent::PersonRan {
            command, exit_code, ..
        } = &event
        {
            let how = exit_code.map_or_else(|| "killed".to_owned(), |code| format!("exit {code}"));
            self.note(&format!("you ran {command} ({how})"));
            return;
        }
        self.ended = None;
        match event {
            AgentEvent::Submitted { text } => {
                self.task = cut(&text, TEXT_CAP);
                self.submissions += 1;
                self.open.clear();
                self.retries = 0;
                self.network = None;
                self.phase = Phase::Waiting;
            }
            AgentEvent::Model { model_turn, event } => {
                if model_turn != self.turn {
                    self.turn = model_turn;
                    self.open.clear();
                }
                self.network = None;
                match event {
                    ModelEvent::Started { .. } => self.phase = Phase::Responding,
                    ModelEvent::ReasoningDelta { .. } => self.phase = Phase::Thinking,
                    ModelEvent::TextDelta { .. } => self.phase = Phase::Writing,
                    ModelEvent::ToolCallStarted { name, .. } => {
                        self.phase = Phase::Drafting;
                        self.drafting = name;
                    }
                    ModelEvent::ToolCallArgumentsDelta { .. } => {}
                    ModelEvent::ToolCallCompleted { call } => self.proposed(&call),
                    ModelEvent::Usage { usage } => {
                        self.input_tokens += usage.input_tokens;
                        self.output_tokens += usage.output_tokens;
                    }
                    ModelEvent::Searched { query, error, .. } => self.step(Step {
                        tool: "search".to_owned(),
                        summary: cut(&query, TEXT_CAP),
                        ms: None,
                        error: error.is_some(),
                    }),
                    ModelEvent::Finished { .. } => self.phase = Phase::Calls,
                }
            }
            AgentEvent::ToolDecided {
                call_id, decision, ..
            } => {
                let denied = self
                    .open
                    .iter_mut()
                    .find(|call| call.id == call_id)
                    .and_then(|call| {
                        call.decided = true;
                        let ApprovalDecision::Denied { reason } = decision else {
                            return None;
                        };
                        call.finished = true;
                        Some((call.tool.clone(), call.summary.clone(), reason))
                    });
                if let Some((tool, summary, reason)) = denied {
                    self.step(Step {
                        tool: tool.clone(),
                        summary,
                        ms: None,
                        error: true,
                    });
                    self.note(&format!("refused {tool}: {reason}"));
                }
            }
            AgentEvent::ToolStarted { call, .. } => {
                if !self.open.iter().any(|open| open.id == call.id) {
                    self.proposed(&call);
                }
                if let Some(open) = self.open.iter_mut().find(|open| open.id == call.id) {
                    open.decided = true;
                    open.started_at = Some(at);
                }
            }
            AgentEvent::ToolFinished { result, .. } => {
                let Some(call) = self
                    .open
                    .iter_mut()
                    .find(|call| call.id == result.call_id && !call.finished)
                else {
                    return;
                };
                call.finished = true;
                let step = Step {
                    tool: call.tool.clone(),
                    summary: call.summary.clone(),
                    ms: call.started_at.map(|started| at.saturating_sub(started)),
                    error: result.output.is_error,
                };
                let key = (call.tool.clone(), call.summary.clone());
                if self.recent_calls.len() == LOOP_CALLS {
                    self.recent_calls.pop_front();
                }
                self.recent_calls.push_back(key);
                self.step(step);
                if self.open.iter().all(|call| call.finished) {
                    self.phase = Phase::Waiting;
                }
            }
            AgentEvent::Noted { note, .. } => self.note(&note),
            AgentEvent::TurnRetried { attempt, error, .. } => {
                self.retries += 1;
                self.phase = Phase::Waiting;
                self.note(&format!("turn retried (attempt {attempt}): {error}"));
            }
            AgentEvent::WaitingForNetwork {
                error,
                retry_in_seconds,
                waited_seconds,
                ..
            } => {
                self.network = Some((
                    format!("{} (retrying in {retry_in_seconds}s)", cut(&error, 120)),
                    waited_seconds,
                ));
            }
            AgentEvent::TurnsRunningOut { turns_left, .. } => {
                self.note(&format!("{turns_left} model turns left"));
            }
            AgentEvent::SentBack { reason, .. } => self.note(&format!("sent back: {reason}")),
            AgentEvent::HandedOver {
                session, reason, ..
            } => {
                self.note(&format!("handed over to session {session}: {reason}"));
            }
            _ => {}
        }
    }

    fn proposed(&mut self, call: &ToolCall) {
        self.calls += 1;
        self.open.push(Open {
            id: call.id.clone(),
            tool: call.name.clone(),
            summary: cut(
                crate::render::describe_call(call)
                    .lines()
                    .next()
                    .unwrap_or_default(),
                TEXT_CAP,
            ),
            decided: false,
            started_at: None,
            finished: false,
        });
    }

    /// Whether this is a log worth a card: one that has opened.
    #[must_use]
    pub fn opened(&self) -> bool {
        self.seen_start
    }

    /// The card for `now`, given whether the process that wrote it still exists
    /// (`None` when that cannot be told).
    #[must_use]
    pub fn card(&self, now: u64, alive: Option<bool>) -> Card {
        let silent = now.saturating_sub(self.last_ms);
        let mut found: Vec<(&'static str, String)> = Vec::new();
        let (state, kind, doing, for_ms) = if let Some((failed, how)) = &self.ended {
            let state = if *failed {
                State::Failed
            } else if alive == Some(true) {
                State::Idle
            } else {
                State::Done
            };
            let (kind, doing) = match state {
                State::Idle => ("idle", "waiting for the next prompt".to_owned()),
                State::Failed => ("failed", format!("failed: {how}")),
                _ => ("done", format!("finished: {how}")),
            };
            (state, kind, doing, silent)
        } else if alive == Some(false) || (alive.is_none() && silent > ABANDONED_MS) {
            let doing = "the process exited without finishing".to_owned();
            (State::Gone, "gone", doing, silent)
        } else {
            self.live(now, silent, &mut found)
        };
        let trouble = found.iter().map(|(kind, _)| *kind).collect();
        let problems = found.into_iter().map(|(_, text)| text).collect();
        Card {
            id: self.id.clone(),
            workspace: self.workspace.clone(),
            model: self.model.clone(),
            approval: self.approval.clone(),
            pid: self.pid,
            state,
            doing,
            for_ms,
            problems,
            kind,
            trouble,
            task: self.task.clone(),
            submissions: self.submissions,
            turn: self.turn,
            max_turns: self.max_turns,
            calls: self.calls,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            started_ms: self.started_ms,
            last_ms: self.last_ms,
            steps: self.steps.iter().cloned().collect(),
            notes: self.notes.iter().cloned().collect(),
        }
    }

    fn live(
        &self,
        now: u64,
        silent: u64,
        problems: &mut Vec<(&'static str, String)>,
    ) -> (State, &'static str, String, u64) {
        let mut needs_you = false;
        let (kind, doing, since) = if let Some((error, waited)) = &self.network {
            if *waited > 300 {
                problems.push((
                    "no_network",
                    format!("no network for {}", took(waited * 1_000)),
                ));
            }
            (
                "network",
                format!("waiting for the network: {error}"),
                silent,
            )
        } else if let Some(call) = self
            .open
            .iter()
            .find(|call| call.started_at.is_some() && !call.finished)
        {
            let ran = now.saturating_sub(call.started_at.unwrap_or(now));
            let child_busy = call.tool == dino_core::subagent::AGENT
                && now.saturating_sub(self.children_ms) <= MODEL_SILENT_MS;
            if ran > TOOL_LONG_MS && !child_busy {
                problems.push((
                    "long_tool",
                    format!("{} has been running for {}", call.tool, took(ran)),
                ));
            }
            ("tool", format!("running {}", call.summary), ran)
        } else if let Some(call) = self
            .open
            .iter()
            .find(|call| !call.decided && !call.finished)
            .filter(|_| silent > APPROVAL_GRACE_MS)
        {
            needs_you = true;
            (
                "approval",
                format!("waiting for approval: {}", call.summary),
                silent,
            )
        } else {
            let (kind, doing) = match self.phase {
                Phase::Waiting => ("waiting", "waiting for the model".to_owned()),
                Phase::Responding => ("responding", "the model is starting to answer".to_owned()),
                Phase::Thinking => ("thinking", "thinking".to_owned()),
                Phase::Writing => ("writing", "writing".to_owned()),
                Phase::Drafting => ("drafting", format!("drafting a {} call", self.drafting)),
                Phase::Calls => ("dispatching", "dispatching calls".to_owned()),
            };
            if self.phase != Phase::Calls && silent > MODEL_SILENT_MS {
                problems.push((
                    "silent_model",
                    format!("nothing from the model for {}", took(silent)),
                ));
            }
            (kind, doing, silent)
        };
        if let Some((_, summary)) = self.recent_calls.front().filter(|first| {
            self.recent_calls.len() == LOOP_CALLS
                && self.recent_calls.iter().all(|call| call == *first)
        }) {
            problems.push((
                "loop",
                format!("the same call {LOOP_CALLS} times: {summary}"),
            ));
        }
        if self.retries >= RETRIES {
            problems.push((
                "retries",
                format!("this turn has been sent {} times", self.retries),
            ));
        }
        let state = if !problems.is_empty() {
            State::Stuck
        } else if needs_you {
            State::NeedsYou
        } else {
            State::Working
        };
        (state, kind, doing, since)
    }
}

fn took(ms: u64) -> String {
    let seconds = ms / 1_000;
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3_600, seconds % 3_600 / 60),
    }
}

fn cut(text: &str, room: usize) -> String {
    if text.len() <= room {
        return text.to_owned();
    }
    let mut end = room;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Whether the process exists. `None` where that cannot be asked.
#[cfg(unix)]
fn process_alive(pid: u32) -> Option<bool> {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    let pid = Pid::from_raw(i32::try_from(pid).ok()?);
    match kill(pid, None) {
        Ok(()) | Err(Errno::EPERM) => Some(true),
        Err(_) => Some(false),
    }
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> Option<bool> {
    None
}

/// Logs read so far, by path: how far in, and what they said.
#[derive(Default)]
pub struct Watch {
    logs: HashMap<PathBuf, (u64, Digest)>,
}

impl Watch {
    /// Brings the digest of `path` up to date by reading what was appended.
    fn refresh(&mut self, path: &Path) -> Option<&Digest> {
        let length = std::fs::metadata(path).ok()?.len();
        let entry = self
            .logs
            .entry(path.to_path_buf())
            .or_insert_with(|| (0, Digest::new(path)));
        if length < entry.0 {
            *entry = (0, Digest::new(path));
        }
        if length > entry.0 {
            let mut file = std::fs::File::open(path).ok()?;
            file.seek(std::io::SeekFrom::Start(entry.0)).ok()?;
            let mut added = Vec::new();
            file.take(length - entry.0).read_to_end(&mut added).ok()?;
            // Whole lines only: the rest is a record still being written.
            let whole = added
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |at| at + 1);
            for line in String::from_utf8_lossy(&added[..whole]).lines() {
                entry.1.line(line);
            }
            entry.0 += whole as u64;
        }
        entry.1.children_ms = children_written(path);
        Some(&entry.1)
    }

    /// Every session worth showing under `workspaces`, what wants attention first.
    pub fn cards(&mut self, workspaces: &[PathBuf]) -> Vec<Card> {
        let now = unix_millis();
        let mut cards = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for workspace in workspaces {
            for path in recent_logs(workspace) {
                if !seen.insert(std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone())) {
                    continue;
                }
                let Some(digest) = self.refresh(&path) else {
                    continue;
                };
                if !digest.opened() {
                    continue;
                }
                let alive = digest.pid.and_then(process_alive);
                let card = digest.card(now, alive);
                let stale = now.saturating_sub(card.last_ms) > duration_ms(LISTED_FOR);
                if stale && !card.state.live() {
                    continue;
                }
                cards.push(card);
            }
        }
        cards.sort_by(|left, right| {
            left.state
                .rank()
                .cmp(&right.state.rank())
                .then(right.last_ms.cmp(&left.last_ms))
        });
        cards
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The session logs directly in `directory`, with when each was written.
/// Subagents' logs are a level down, in a directory named after the parent.
fn logs_in(directory: &Path) -> Vec<(SystemTime, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .filter_map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            metadata
                .is_file()
                .then_some((metadata.modified().ok()?, path))
        })
        .collect()
}

/// When any subagent of the session logged at `path` last wrote, in unix ms.
fn children_written(path: &Path) -> u64 {
    logs_in(&path.with_extension(""))
        .into_iter()
        .filter_map(|(modified, _)| modified.duration_since(UNIX_EPOCH).ok())
        .map(duration_ms)
        .max()
        .unwrap_or(0)
}

/// The newest logs of `workspace`, and of the worktree sessions made in it,
/// touched within the listing window.
fn recent_logs(workspace: &Path) -> Vec<PathBuf> {
    let state = workspace.join(".dino");
    let worktrees = std::fs::read_dir(state.join("worktrees"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(".dino").join("sessions"));
    let cutoff = SystemTime::now()
        .checked_sub(LISTED_FOR)
        .unwrap_or(UNIX_EPOCH);
    let mut logs: Vec<(SystemTime, PathBuf)> = std::iter::once(state.join("sessions"))
        .chain(worktrees)
        .flat_map(|directory| logs_in(&directory))
        .filter(|(modified, _)| *modified >= cutoff)
        .collect();
    logs.sort_by_key(|log| std::cmp::Reverse(log.0));
    logs.truncate(PER_WORKSPACE);
    logs.into_iter().map(|(_, path)| path).collect()
}

const INDEX_HTML: &str = include_str!("dash/index.html");
const DASH_JS: &str = include_str!("dash/dash.js");
const DASH_CSS: &str = include_str!("dash/dash.css");

struct Shared {
    token: String,
    places: super::Workspaces,
    current: PathBuf,
    watch: Mutex<Watch>,
    blurb: Option<Arc<super::Blurb>>,
}

/// Answers connections on `listener` for as long as it runs: the page, and the
/// cards it asks for with the token.
pub async fn serve(
    listener: TcpListener,
    token: String,
    places: super::Workspaces,
    current: PathBuf,
    blurb: Option<Arc<super::Blurb>>,
) -> Infallible {
    let shared = Arc::new(Shared {
        token,
        places,
        current,
        watch: Mutex::new(Watch::default()),
        blurb,
    });
    let slots = Arc::new(Semaphore::new(server::MAX_CONNECTIONS));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            continue;
        };
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let shared = Arc::clone(&shared);
                async move { Ok::<_, Infallible>(route(&request, &shared).await) }
            });
            let connection = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(server::HEADER_TIMEOUT)
                .max_buf_size(server::MAX_BUFFER_BYTES)
                .serve_connection(TokioIo::new(stream), service);
            let _ = connection.await;
            drop(slot);
        });
    }
}

async fn route(request: &Request<Incoming>, shared: &Shared) -> Response<server::Body> {
    if request.method() != Method::GET {
        return server::status(StatusCode::METHOD_NOT_ALLOWED);
    }
    match request.uri().path() {
        "/" | "/index.html" => {
            server::respond(StatusCode::OK, "text/html; charset=utf-8", INDEX_HTML)
        }
        "/dash.js" => server::respond(StatusCode::OK, "text/javascript; charset=utf-8", DASH_JS),
        "/dash.css" => server::respond(StatusCode::OK, "text/css; charset=utf-8", DASH_CSS),
        "/api/sessions" => {
            if !server::authorised(request.headers(), &shared.token) {
                tokio::time::sleep(server::REFUSAL_DELAY).await;
                return server::status(StatusCode::UNAUTHORIZED);
            }
            let workspaces: Vec<PathBuf> = shared
                .places
                .offered(&shared.current)
                .into_iter()
                .map(|place| place.path)
                .collect();
            let cards = match shared.watch.lock() {
                Ok(mut watch) => watch.cards(&workspaces),
                Err(poisoned) => poisoned.into_inner().cards(&workspaces),
            };
            let now = unix_millis();
            let blurb = shared
                .blurb
                .as_ref()
                .and_then(|blurb| blurb.text(super::blurb::Fact::of(&cards, now)));
            server::json(
                StatusCode::OK,
                &serde_json::json!({ "now": now, "sessions": cards, "blurb": blurb }),
            )
        }
        _ => server::status(StatusCode::NOT_FOUND),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: u64 = 1_000_000;

    fn started(pid: Option<u32>) -> String {
        let pid = pid.map_or(String::new(), |pid| format!(r#","pid":{pid}"#));
        format!(
            r#"{{"record":"started","seq":0,"unix_millis":{START},"session":{{"session_id":"s1","dino_version":"0","model_key":"m","provider":"qwen","workspace_root":"/w","approval_mode":"ask","sandbox":"none","max_model_turns":100{pid}}}}}"#
        )
    }

    fn event(at: u64, body: &str) -> String {
        format!(r#"{{"record":"event","seq":1,"unix_millis":{at},"event":{body}}}"#)
    }

    fn model(at: u64, turn: u32, body: &str) -> String {
        event(
            at,
            &format!(r#"{{"type":"model","model_turn":{turn},"event":{body}}}"#),
        )
    }

    fn call(id: &str, path: &str) -> String {
        format!(
            r#"{{"type":"tool_call_completed","call":{{"id":"{id}","name":"read","arguments":{{"path":"{path}"}}}}}}"#
        )
    }

    fn digest(lines: &[String]) -> Digest {
        let mut digest = Digest::new(Path::new("s1.jsonl"));
        for line in lines {
            digest.line(line);
        }
        digest
    }

    fn opening() -> Vec<String> {
        vec![
            started(Some(7)),
            event(START + 1, r#"{"type":"submitted","text":"fix the thing"}"#),
        ]
    }

    #[test]
    fn a_model_that_is_streaming_is_working() {
        let mut lines = opening();
        lines.push(model(
            START + 100,
            1,
            r#"{"type":"started","response_id":null}"#,
        ));
        lines.push(model(
            START + 200,
            1,
            r#"{"type":"reasoning_delta","text":"hm"}"#,
        ));
        let card = digest(&lines).card(START + 5_000, Some(true));
        assert_eq!(card.state, State::Working);
        assert_eq!(card.doing, "thinking");
        assert_eq!(card.task, "fix the thing");
        assert!(card.problems.is_empty());
    }

    #[test]
    fn a_model_gone_quiet_is_stuck() {
        let mut lines = opening();
        lines.push(model(
            START + 200,
            1,
            r#"{"type":"reasoning_delta","text":"hm"}"#,
        ));
        let card = digest(&lines).card(START + 200 + MODEL_SILENT_MS + 1, Some(true));
        assert_eq!(card.state, State::Stuck);
        assert!(card.problems[0].contains("nothing from the model"));
    }

    #[test]
    fn a_call_nobody_has_decided_wants_a_person_once_the_loop_has_had_its_chance() {
        let mut lines = opening();
        lines.push(model(START + 100, 1, &call("c1", "a.rs")));
        lines.push(model(
            START + 110,
            1,
            r#"{"type":"finished","reason":"tool_use"}"#,
        ));
        let digest = digest(&lines);
        assert_eq!(digest.card(START + 1_000, Some(true)).state, State::Working);
        let card = digest.card(START + 60_000, Some(true));
        assert_eq!(card.state, State::NeedsYou);
        assert_eq!(card.doing, "waiting for approval: read a.rs");
    }

    #[test]
    fn a_running_call_shows_how_long_and_is_stuck_only_when_it_is_very_long() {
        let mut lines = opening();
        lines.push(model(START + 100, 1, &call("c1", "a.rs")));
        lines.push(event(
            START + 150,
            r#"{"type":"tool_started","model_turn":1,"call":{"id":"c1","name":"read","arguments":{"path":"a.rs"}}}"#,
        ));
        let digest = digest(&lines);
        let early = digest.card(START + 30_150, Some(true));
        assert_eq!(early.state, State::Working);
        assert_eq!(early.for_ms, 30_000);
        let late = digest.card(START + 150 + TOOL_LONG_MS + 1, Some(true));
        assert_eq!(late.state, State::Stuck);
    }

    #[test]
    fn the_same_call_three_times_in_a_row_is_a_loop() {
        let mut lines = opening();
        for turn in 1..=3_u32 {
            let id = format!("c{turn}");
            let at = START + u64::from(turn) * 1_000;
            lines.push(model(at, turn, &call(&id, "a.rs")));
            lines.push(event(
                at + 1,
                &format!(
                    r#"{{"type":"tool_started","model_turn":{turn},"call":{{"id":"{id}","name":"read","arguments":{{"path":"a.rs"}}}}}}"#
                ),
            ));
            lines.push(event(
                at + 2,
                &format!(
                    r#"{{"type":"tool_finished","model_turn":{turn},"result":{{"call_id":"{id}","output":{{"content":"x","is_error":false}}}}}}"#
                ),
            ));
        }
        let card = digest(&lines).card(START + 4_000, Some(true));
        assert_eq!(card.state, State::Stuck);
        assert!(card.problems[0].contains("the same call 3 times"));
        assert_eq!(card.steps.len(), 3);
    }

    #[test]
    fn a_finished_log_is_idle_while_its_process_lives_and_done_after() {
        let mut lines = opening();
        lines.push(format!(
            r#"{{"record":"finished","seq":9,"unix_millis":{},"outcome":{{"stop_reason":"end_turn","model_turns":2,"error":null}}}}"#,
            START + 500
        ));
        let digest = digest(&lines);
        assert_eq!(digest.card(START + 600, Some(true)).state, State::Idle);
        assert_eq!(digest.card(START + 600, Some(false)).state, State::Done);
    }

    #[test]
    fn a_dead_process_with_no_end_is_gone_not_stuck() {
        let card = digest(&opening()).card(START + 5_000, Some(false));
        assert_eq!(card.state, State::Gone);
    }

    #[test]
    fn without_a_pid_a_long_silence_reads_as_abandoned() {
        let lines = vec![
            started(None),
            event(START + 1, r#"{"type":"submitted","text":"go"}"#),
        ];
        let digest = digest(&lines);
        assert_eq!(digest.card(START + 60_000, None).state, State::Working);
        assert_eq!(
            digest.card(START + ABANDONED_MS + 2, None).state,
            State::Gone
        );
    }

    #[test]
    fn an_error_ending_is_failed_whatever_the_process_does() {
        let mut lines = opening();
        lines.push(format!(
            r#"{{"record":"finished","seq":9,"unix_millis":{},"outcome":{{"stop_reason":null,"model_turns":2,"error":"transport failed"}}}}"#,
            START + 500
        ));
        let card = digest(&lines).card(START + 600, Some(true));
        assert_eq!(card.state, State::Failed);
        assert!(card.doing.contains("transport failed"));
    }

    fn finished(at: u64) -> String {
        format!(
            r#"{{"record":"finished","seq":9,"unix_millis":{at},"outcome":{{"stop_reason":"end_turn","model_turns":2,"error":null}}}}"#
        )
    }

    #[test]
    fn a_line_the_person_ran_at_the_prompt_leaves_the_session_idle() {
        let mut lines = opening();
        lines.push(finished(START + 500));
        lines.push(event(
            START + 600,
            r#"{"type":"person_ran","command":"git status","exit_code":0,"output":"clean"}"#,
        ));
        let card = digest(&lines).card(START + 600 + MODEL_SILENT_MS + 1, Some(true));
        assert_eq!(card.state, State::Idle);
        assert_eq!(card.notes, ["you ran git status (exit 0)"]);
    }

    #[test]
    fn a_providers_search_is_a_step() {
        let mut lines = opening();
        lines.push(model(
            START + 100,
            1,
            r#"{"type":"searched","query":"tokio select","results":[]}"#,
        ));
        let card = digest(&lines).card(START + 200, Some(true));
        assert_eq!(card.steps[0].tool, "search");
        assert_eq!(card.steps[0].summary, "tokio select");
    }

    #[test]
    fn a_long_subagent_call_is_not_stuck_while_its_child_is_writing() {
        let mut lines = opening();
        lines.push(event(
            START + 150,
            r#"{"type":"tool_started","model_turn":1,"call":{"id":"c1","name":"agent","arguments":{"task":"look"}}}"#,
        ));
        let mut digest = digest(&lines);
        let now = START + 150 + TOOL_LONG_MS + 1;
        assert_eq!(digest.card(now, Some(true)).state, State::Stuck);
        digest.children_ms = now - 1_000;
        assert_eq!(digest.card(now, Some(true)).state, State::Working);
    }

    #[test]
    fn subagents_forks_and_checkpoints_are_not_sessions_and_worktrees_are() {
        let workspace = std::env::temp_dir().join(format!("dino-dash-logs-{}", std::process::id()));
        let sessions = workspace.join(".dino/sessions");
        let worktree = workspace.join(".dino/worktrees/look/.dino/sessions");
        for directory in [
            &sessions.join("p1"),
            &worktree,
            &workspace.join(".dino/checkpoints"),
        ] {
            std::fs::create_dir_all(directory).unwrap();
        }
        for file in [
            sessions.join("p1.jsonl"),
            sessions.join("p1/p1.1.jsonl"),
            sessions.join("p1.fork.json"),
            worktree.join("w1.jsonl"),
        ] {
            std::fs::write(&file, format!("{}\n", started(Some(7)))).unwrap();
        }
        let mut found: Vec<String> = recent_logs(&workspace)
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(found, ["p1.jsonl", "w1.jsonl"]);
        assert!(children_written(&sessions.join("p1.jsonl")) > 0);
        assert_eq!(children_written(&worktree.join("w1.jsonl")), 0);
        std::fs::remove_dir_all(&workspace).unwrap();
    }

    #[test]
    fn a_half_written_last_line_is_left_for_the_next_read() {
        let directory = std::env::temp_dir().join(format!("dino-dash-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("s1.jsonl");
        let first = event(START + 1, r#"{"type":"submitted","text":"go"}"#);
        std::fs::write(&path, format!("{}\n{}", started(Some(7)), &first[..20])).unwrap();
        let mut watch = Watch::default();
        assert_eq!(watch.refresh(&path).unwrap().submissions, 0);
        std::fs::write(&path, format!("{}\n{first}\n", started(Some(7)))).unwrap();
        assert_eq!(watch.refresh(&path).unwrap().submissions, 1);
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
