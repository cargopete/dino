//! Dino's provider-neutral act-observe loop.
//!
//! The runtime classifies a tool call, a policy decides it, and this core
//! records the decision before dispatching. Approval is enforced here rather
//! than in a decorator so that the session log can honestly claim to hold the
//! proposed action, the decision, the execution and the result.

pub mod approval;
pub mod blocker;
pub mod git;
pub mod loops;
pub mod memory;
pub mod progress;
pub mod prompt;
pub mod resume;
pub mod review;
pub mod session;
pub mod skills;
pub mod text;
pub mod todo;
pub mod verify;
pub mod workspace;

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::future::Future;
use std::num::NonZeroU32;
use std::pin::Pin;

use dino_model::ModelError;
use dino_model::ModelSession;
use dino_protocol::AgentEvent;
use dino_protocol::ApprovalDecision;
use dino_protocol::ModelEvent;
use dino_protocol::StopReason;
use dino_protocol::TokenUsage;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use dino_protocol::ToolResult;
use dino_protocol::TurnInput;
use dino_protocol::WindowSource;
use futures_util::StreamExt;
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::approval::ApprovalPolicy;
use crate::session::EventSink;
use crate::session::NullSink;
use crate::session::RunOutcome;
use crate::session::SinkError;

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + 'a>>;

pub type CheckFuture<'a> = Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;

/// Looks at the workspace when a model ends its turn, and as its turns run
/// out, for runs nobody is watching. A model that stops with the build broken
/// has not finished, and with no person to hand back to, ending there wastes
/// the turns it had left.
pub trait FinishCheck: Send + Sync {
    /// What is still wrong, in a sentence or two the core frames for the
    /// model, or `None` when nothing is. A check that cannot run says `None`:
    /// it may not end a run on its own.
    fn unfinished(&self) -> CheckFuture<'_>;
}

/// Several checks, asked in order; the first to find work unfinished speaks.
pub struct FirstUnfinished {
    checks: Vec<Box<dyn FinishCheck>>,
}

impl FirstUnfinished {
    #[must_use]
    pub fn new(checks: Vec<Box<dyn FinishCheck>>) -> Self {
        Self { checks }
    }
}

impl FinishCheck for FirstUnfinished {
    fn unfinished(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            for check in &self.checks {
                if let Some(unfinished) = check.unfinished().await {
                    return Some(unfinished);
                }
            }
            None
        })
    }
}

/// A set of tools, which both classifies and executes calls against itself.
///
/// Classification lives here rather than in the core because the runtime owns
/// the tool schemas. An implementation must resolve a call's target the same
/// way in both methods, or an approval granted for one target could be spent on
/// another.
pub trait ToolRuntime: Send + Sync {
    /// The tools this runtime offers, as the provider will be told about them.
    ///
    /// A runtime describes its own surface so a [`ToolSet`] can refuse two
    /// runtimes claiming one name at construction, rather than dispatching to
    /// whichever it happened to check first for the rest of the session.
    fn definitions(&self) -> Vec<ToolDefinition>;

    /// Describes what a call would do, without doing it.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] for an unknown tool or arguments that do not
    /// parse. Such a call is never decided and never executed.
    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError>;

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_>;

    /// The model's history no longer holds earlier tool results: a new
    /// conversation, or results elided to fit the window. A runtime that holds
    /// back output it has already returned must stop, or it withholds bytes
    /// the model no longer has.
    fn history_lost(&self) {}

    /// Told every call of a model turn before the first runs.
    fn begin_turn(&self, _calls: &[ToolCall]) {}
}

/// Several tool runtimes behind one, dispatching by tool name.
pub struct ToolSet {
    runtimes: Vec<Box<dyn ToolRuntime>>,
    /// Tool name to the index of the runtime that owns it.
    owners: HashMap<String, usize>,
    /// Tool name to the optional arguments no empty string can satisfy.
    blank_is_absent: HashMap<String, Vec<String>>,
}

impl ToolSet {
    /// Combines runtimes into one surface.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when two runtimes claim the same tool name. A
    /// silent winner there would make every later approval decision describe a
    /// different tool from the one that ran.
    pub fn new(runtimes: Vec<Box<dyn ToolRuntime>>) -> Result<Self, ToolError> {
        let mut owners = HashMap::new();
        let mut blank_is_absent = HashMap::new();
        for (index, runtime) in runtimes.iter().enumerate() {
            for definition in runtime.definitions() {
                let blank = blank_means_absent(&definition.input_schema);
                if !blank.is_empty() {
                    blank_is_absent.insert(definition.name.clone(), blank);
                }
                if owners.insert(definition.name.clone(), index).is_some() {
                    return Err(ToolError::new(format!(
                        "two tool runtimes both offer {:?}",
                        definition.name
                    )));
                }
            }
        }
        Ok(Self {
            runtimes,
            owners,
            blank_is_absent,
        })
    }

    /// Drops the empty strings sent for optional arguments that cannot be
    /// empty. Kimi fills every optional field, with `""` for those it omits.
    fn normalise<'a>(&self, call: &'a ToolCall) -> Cow<'a, ToolCall> {
        let Some(names) = self.blank_is_absent.get(&call.name) else {
            return Cow::Borrowed(call);
        };
        let is_blank = |name: &String, value: &Value| value == "" && names.contains(name);
        let has_blank = call
            .arguments
            .as_object()
            .is_some_and(|arguments| arguments.iter().any(|(name, value)| is_blank(name, value)));
        if !has_blank {
            return Cow::Borrowed(call);
        }
        let mut call = call.clone();
        if let Some(arguments) = call.arguments.as_object_mut() {
            arguments.retain(|name, value| !is_blank(name, value));
        }
        Cow::Owned(call)
    }

    fn owner(&self, call: &ToolCall) -> Result<&dyn ToolRuntime, ToolError> {
        self.owners
            .get(&call.name)
            .and_then(|index| self.runtimes.get(*index))
            .map(AsRef::as_ref)
            .ok_or_else(|| {
                let mut names: Vec<&str> = self.owners.keys().map(String::as_str).collect();
                names.sort_unstable();
                ToolError::new(format!(
                    "unknown tool {:?}. The tools are: {}.",
                    call.name,
                    names.join(", ")
                ))
            })
    }
}

impl ToolRuntime for ToolSet {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.runtimes
            .iter()
            .flat_map(|runtime| runtime.definitions())
            .collect()
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if let Some(unparsed) = call.arguments.get(dino_protocol::UNPARSED_ARGUMENTS) {
            return Err(ToolError::new(format!(
                "The arguments for {} were not valid JSON ({}). You sent: {}\nSend the call \
                 again with valid JSON: every string quoted, every key quoted.",
                call.name,
                unparsed["error"].as_str().unwrap_or("unknown error"),
                unparsed["raw"].as_str().unwrap_or_default(),
            )));
        }
        self.owner(call)?.classify(&self.normalise(call))
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        match self.owner(call) {
            Ok(runtime) => runtime.execute(&self.normalise(call)),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }

    fn history_lost(&self) {
        for runtime in &self.runtimes {
            runtime.history_lost();
        }
    }

    fn begin_turn(&self, calls: &[ToolCall]) {
        for runtime in &self.runtimes {
            runtime.begin_turn(calls);
        }
    }
}

/// The optional properties of a tool schema that no empty string satisfies:
/// not string-typed, a `minLength` of one or more, or an enum without `""`.
fn blank_means_absent(schema: &Value) -> Vec<String> {
    let required = schema["required"].as_array();
    let is_required =
        |name: &str| required.is_some_and(|names| names.iter().any(|known| known == name));
    let admits_empty = |property: &Value| {
        let string_typed = match &property["type"] {
            Value::String(kind) => kind == "string",
            Value::Array(kinds) => kinds.iter().any(|kind| kind == "string"),
            _ => true,
        };
        string_typed
            && property["minLength"].as_u64().unwrap_or(0) == 0
            && property["enum"]
                .as_array()
                .is_none_or(|values| values.iter().any(|value| value == ""))
    };
    schema["properties"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, property)| !is_required(name) && !admits_empty(property))
        .map(|(name, _)| name.clone())
        .collect()
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct ToolError {
    message: String,
}

impl ToolError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentConfig {
    pub max_model_turns: NonZeroU32,
    /// Percentage of the window in force past which a person is told.
    ///
    /// A percentage rather than a fraction, because that is what a person
    /// setting it would write, and because integer arithmetic against a token
    /// count needs no casting to get wrong.
    pub warn_at_percent: u8,
    /// Percentage past which history is reduced.
    pub elide_at_percent: u8,
    /// Tool results kept intact when reducing.
    pub keep_recent_results: usize,
    /// Times a turn whose stream breaks partway is sent, the first included.
    pub turn_attempts: u32,
    /// From how many turns left the model is told the count.
    pub turn_notice_at: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_model_turns: NonZeroU32::new(32).expect("32 is non-zero"),
            // Guesses, and fields rather than constants so they can be wrong
            // cheaply.
            warn_at_percent: 70,
            elide_at_percent: 85,
            keep_recent_results: 8,
            turn_attempts: 3,
            turn_notice_at: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub events: Vec<AgentEvent>,
    pub text: String,
    pub stop_reason: StopReason,
    pub model_turns: u32,
}

fn blocked(events: Vec<AgentEvent>, mut text: String, reason: &str, turns: u32) -> RunResult {
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(reason);
    RunResult {
        events,
        text,
        stop_reason: StopReason::Blocked,
        model_turns: turns,
    }
}

/// Told to the model with its last tool result, because that is the one
/// channel every provider carries into the next turn.
fn turns_left_note(turns_left: u32, unfinished: Option<&str>) -> String {
    let mut note = if turns_left == 1 {
        "\n\n[dino: your next response is the last in this run. Make no tool calls: say \
         what is done, what is not, and what should happen next.]"
            .to_owned()
    } else {
        format!(
            "\n\n[dino: {turns_left} model turns left in this run, including your next \
             response. Finish the change, check it once, and say what is done and what is \
             not.]"
        )
    };
    if let Some(unfinished) = unfinished {
        let _ = write!(
            note,
            "\n[dino: right now {unfinished}\nLeave the workspace building: complete or \
             undo the change in progress before the turns run out.]"
        );
    }
    note
}

fn sent_back_note(unfinished: &str) -> String {
    format!(
        "[dino: you ended your turn, but the work is not finished: {unfinished}\nFix it \
         and check again. If the task cannot be done, call report_blocker and say why.]"
    )
}

const CUT_OFF_NOTE: &str = "[dino: your last response was cut off because the context \
     window filled. Older tool results have been elided to make room. Carry on from where you \
     stopped, and keep each response short.]";

/// Which turn is being attempted, and how many times so far.
struct Turn<'a> {
    number: u32,
    attempt: u32,
    input: &'a TurnInput,
}

/// What settling a turn's tool calls came to.
enum Settled {
    Results(Vec<ToolResult>),
    /// The model reported a blocker, and this is what it said.
    Blocked(String),
    Cancelled,
}

/// What a turn came to, once any broken streams have been sent again.
enum Streamed {
    Finished {
        calls: Vec<ToolCall>,
        stop_reason: Option<StopReason>,
    },
    Cancelled {
        turns: u32,
    },
}

/// What one attempt at a turn came to.
enum Attempt {
    Finished {
        calls: Vec<ToolCall>,
        stop_reason: Option<StopReason>,
    },
    Broken {
        error: String,
    },
    Cancelled {
        turns: u32,
    },
}

pub struct Agent<S, T> {
    session: S,
    tools: T,
    policy: Box<dyn ApprovalPolicy>,
    sink: Box<dyn EventSink>,
    config: AgentConfig,
    finish_check: Option<Box<dyn FinishCheck>>,
    reviewer: Option<std::sync::Arc<review::Reviewer>>,
    /// Whether this run's work has been reviewed, so it is reviewed once.
    reviewed: bool,
    // Kept outside `drive` so a run that ends in an error still reports what it
    // spent.
    turns_started: u32,
    sent_back: bool,
    /// Images for the next submission's first turn.
    pending_images: Vec<dino_protocol::Image>,
    /// Tool results a run computed but never sent, because it hit the turn
    /// limit. The provider's history ends in those calls, so the next
    /// submission must answer them before anything else.
    unsent: Option<Vec<ToolResult>>,
    loops: loops::LoopWatch,
    /// Messages the person typed during a run, for its next tool results.
    inbox: Option<Inbox>,
}

/// Where a frontend leaves what the person types while a run is under way.
pub type Inbox = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

impl<S, T> Agent<S, T>
where
    S: ModelSession,
    T: ToolRuntime,
{
    /// Builds an agent that refuses every mutation and records nothing.
    ///
    /// The default policy is deliberately [`approval::ReadOnly`]: an agent
    /// assembled without an explicit choice cannot write to the workspace.
    pub fn new(session: S, tools: T, config: AgentConfig) -> Self {
        Self {
            session,
            tools,
            policy: Box::new(approval::ReadOnly),
            sink: Box::new(NullSink),
            config,
            finish_check: None,
            reviewer: None,
            reviewed: false,
            turns_started: 0,
            sent_back: false,
            pending_images: Vec::new(),
            unsent: None,
            loops: loops::LoopWatch::default(),
            inbox: None,
        }
    }

    /// Sends a model that ends its turn with work unfinished back to it, once
    /// per run.
    #[must_use]
    pub fn with_finish_check(mut self, check: impl FinishCheck + 'static) -> Self {
        self.finish_check = Some(Box::new(check));
        self
    }

    /// Has `reviewer` look over the work once per run that changed anything,
    /// when the model ends its turn with nothing else unfinished.
    #[must_use]
    pub fn with_reviewer(mut self, reviewer: std::sync::Arc<review::Reviewer>) -> Self {
        self.reviewer = Some(reviewer);
        self
    }

    /// Delivers what the person types during a run with the next tool results.
    #[must_use]
    pub fn with_inbox(mut self, inbox: Inbox) -> Self {
        self.inbox = Some(inbox);
        self
    }

    #[must_use]
    pub fn with_policy(mut self, policy: impl ApprovalPolicy + 'static) -> Self {
        self.policy = Box::new(policy);
        self
    }

    #[must_use]
    pub fn with_sink(mut self, sink: impl EventSink + 'static) -> Self {
        self.sink = Box::new(sink);
        self
    }

    pub fn session(&self) -> &S {
        &self.session
    }

    /// The first turn of a submission, carrying any attached images.
    fn first_input(&mut self, content: String) -> TurnInput {
        if let Some(mut results) = self.unsent.take() {
            // Images wait for a submission that can carry them as a message.
            if let Some(last) = results.last_mut() {
                let _ = write!(
                    last.output.content,
                    "\n\n[dino: the last run reached its turn limit before these results \
                     were sent. The person has since written:]\n{content}"
                );
            }
            return TurnInput::ToolResults { results };
        }
        let images = std::mem::take(&mut self.pending_images);
        if images.is_empty() {
            TurnInput::User { content }
        } else {
            TurnInput::UserWithImages { content, images }
        }
    }

    /// Records an event that happened outside a run, such as a handover, so
    /// the log meets it in its place.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when the sink cannot record it.
    pub fn record(&mut self, event: &AgentEvent) -> Result<(), AgentError> {
        self.sink.emit(event)?;
        Ok(())
    }

    /// Caps the turns of the next run, as a handover chain does per session.
    pub fn set_max_model_turns(&mut self, turns: NonZeroU32) {
        self.config.max_model_turns = turns;
    }

    /// Sends these images with the next submission, after its text.
    pub fn attach_images(&mut self, images: Vec<dino_protocol::Image>) {
        self.pending_images = images;
    }

    /// Swaps in another model session, for a new conversation or another
    /// model, and returns the old one. Tools, policy and sink are kept.
    pub fn replace_session(&mut self, session: S) -> S {
        self.tools.history_lost();
        std::mem::replace(&mut self.session, session)
    }

    pub fn tools(&self) -> &T {
        &self.tools
    }

    /// Runs one user request to completion, without cancellation.
    ///
    /// # Errors
    ///
    /// See [`Agent::run_cancellable`].
    pub async fn run(&mut self, user_message: impl Into<String>) -> Result<RunResult, AgentError> {
        self.run_cancellable(user_message, &CancellationToken::new())
            .await
    }

    /// Runs one user request until the model finishes, the caller cancels, or
    /// the safety limit is reached.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when the provider stream violates the event
    /// protocol, a model request fails, the session sink fails, or the
    /// configured model-turn limit is exhausted.
    pub async fn run_cancellable(
        &mut self,
        user_message: impl Into<String>,
        cancel: &CancellationToken,
    ) -> Result<RunResult, AgentError> {
        self.turns_started = 0;
        let outcome = self.drive(user_message.into(), cancel).await;
        let record = match &outcome {
            Ok(result) => RunOutcome {
                stop_reason: Some(result.stop_reason),
                model_turns: result.model_turns,
                error: None,
            },
            Err(error) => RunOutcome {
                stop_reason: None,
                model_turns: self.turns_started,
                error: Some(error.to_string()),
            },
        };
        match (outcome, self.sink.finish(&record)) {
            (Ok(result), Ok(())) => Ok(result),
            (Ok(_), Err(error)) => Err(AgentError::Sink(error)),
            // A sink failure while reporting an earlier failure does not get to
            // replace the more informative error.
            (Err(error), _) => Err(error),
        }
    }

    async fn drive(
        &mut self,
        user_message: String,
        cancel: &CancellationToken,
    ) -> Result<RunResult, AgentError> {
        let mut events = Vec::new();
        self.emit(
            AgentEvent::Submitted {
                text: user_message.clone(),
            },
            &mut events,
        )?;
        let mut input = self.first_input(user_message);
        let mut text = String::new();
        let mut seen_call_ids = HashSet::new();
        // Snapshot first. `discover_limits` overwrites the profile with what the
        // server serves, and the warning has to be able to say which number it used.
        let documented = self.session.profile().context_window_tokens;
        self.session.discover_limits().await?;
        let budget = budget_window(documented, self.session.profile().context_window_tokens);
        let mut warned = false;
        self.begin_run();

        for model_turn in 1..=self.config.max_model_turns.get() {
            if cancel.is_cancelled() {
                return Ok(cancelled(events, text, model_turn.saturating_sub(1)));
            }
            self.turns_started = model_turn;

            let (calls, stop_reason) = match self
                .stream_turn(
                    model_turn,
                    &input,
                    &mut text,
                    &mut seen_call_ids,
                    &mut events,
                    cancel,
                )
                .await?
            {
                Streamed::Finished { calls, stop_reason } => (calls, stop_reason),
                Streamed::Cancelled { turns } => return Ok(cancelled(events, text, turns)),
            };

            let elided = match budget {
                Some(budget) => {
                    self.mind_the_budget(model_turn, budget, &mut warned, &mut events)?
                }
                None => false,
            };

            let stop_reason = stop_reason.ok_or(AgentError::StreamEndedWithoutFinish)?;
            let cut_off = self.recover_from_a_full_window(
                model_turn,
                budget
                    .filter(|_| stop_reason == StopReason::MaxTokens)
                    .map(|budget| budget.tokens),
                elided,
                &mut events,
            )?;
            let note = match stop_reason {
                StopReason::ToolUse if calls.is_empty() => {
                    return Err(AgentError::ToolStopWithoutCalls);
                }
                StopReason::ToolUse => None,
                StopReason::MaxTokens if cut_off => Some(CUT_OFF_NOTE),
                StopReason::EndTurn => {
                    if !calls.is_empty() {
                        return Err(AgentError::UnresolvedToolCalls(calls.len()));
                    }
                    if let Some(note) = self.send_back(model_turn, &mut events, cancel).await? {
                        input = TurnInput::User { content: note };
                        continue;
                    }
                    return Ok(RunResult {
                        events,
                        text,
                        stop_reason,
                        model_turns: model_turn,
                    });
                }
                StopReason::MaxTokens
                | StopReason::Refusal
                | StopReason::Cancelled
                | StopReason::Blocked => {
                    return Ok(RunResult {
                        events,
                        text,
                        stop_reason,
                        model_turns: model_turn,
                    });
                }
            };
            match self
                .answer(model_turn, calls, note, &text, &mut events, cancel)
                .await?
            {
                Ok(next) => input = next,
                Err(ended) => return Ok(ended),
            }
        }

        if let TurnInput::ToolResults { results } = input {
            self.unsent = Some(results);
        }
        Err(AgentError::ModelTurnLimit(
            self.config.max_model_turns.get(),
        ))
    }

    /// Forgets what the last run was told, and takes the state this one's
    /// review starts from.
    fn begin_run(&mut self) {
        self.sent_back = false;
        self.reviewed = false;
        if let Some(reviewer) = &self.reviewer {
            reviewer.begin();
        }
        self.loops.reset();
    }

    /// Settles every call of a turn in order, noticing a reported blocker.
    async fn settle_all(
        &mut self,
        model_turn: u32,
        calls: Vec<ToolCall>,
        events: &mut Vec<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<Settled, AgentError> {
        let mut results = Vec::with_capacity(calls.len());
        let mut reported_blocker = None;
        let classified: Vec<(ToolCall, ToolAction)> = calls
            .iter()
            .filter_map(|call| Some((call.clone(), self.tools.classify(call).ok()?)))
            .collect();
        self.policy.begin_turn(&classified);
        self.tools.begin_turn(&calls);
        for call in calls {
            let reported = (call.name == blocker::REPORT_BLOCKER)
                .then(|| blocker::describe(&call).ok())
                .flatten();
            if let Some(refusal) = self.loops.refuse(&call) {
                let refused =
                    self.finish_call(model_turn, call.id, ToolOutput::error(refusal), events)?;
                results.push(refused);
                continue;
            }
            let note = self.loops.record(&call);
            let Some(mut result) = self.settle(model_turn, call, events, cancel).await? else {
                return Ok(Settled::Cancelled);
            };
            if let Some(note) = note {
                result.output.content.push_str("\n\n");
                result.output.content.push_str(&note);
            }
            // Only a call that went through counts: a refused one is a refusal.
            if !result.output.is_error {
                reported_blocker = reported_blocker.or(reported);
            }
            results.push(result);
        }
        Ok(match reported_blocker {
            Some(reason) => Settled::Blocked(reason),
            None => Settled::Results(results),
        })
    }

    /// Whether a length stop came from a full window and history was reduced
    /// to make room, in which case the run goes on. `window` is given only for
    /// a length stop.
    fn recover_from_a_full_window(
        &mut self,
        model_turn: u32,
        window: Option<u32>,
        elided: bool,
        events: &mut Vec<AgentEvent>,
    ) -> Result<bool, AgentError> {
        let Some((window, usage)) =
            window.and_then(|window| filled_window(events, window).map(|usage| (window, usage)))
        else {
            return Ok(false);
        };
        if !elided && !self.elide(model_turn, events)? {
            return Ok(false);
        }
        self.emit(
            AgentEvent::TurnCutOff {
                model_turn,
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                window_tokens: window,
            },
            events,
        )?;
        Ok(true)
    }

    /// Settles a turn's calls into the next turn's input, or the run's end when
    /// the model reported a blocker or the run was cancelled. A note with no
    /// calls to carry it goes to the model as a message of its own.
    async fn answer(
        &mut self,
        model_turn: u32,
        calls: Vec<ToolCall>,
        note: Option<&str>,
        text: &str,
        events: &mut Vec<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<Result<TurnInput, RunResult>, AgentError> {
        if calls.is_empty() {
            return Ok(Ok(TurnInput::User {
                content: note.unwrap_or_default().to_owned(),
            }));
        }
        let mut results = match self.settle_all(model_turn, calls, events, cancel).await? {
            Settled::Results(results) => results,
            Settled::Blocked(reason) => {
                let events = std::mem::take(events);
                return Ok(Err(blocked(events, text.to_owned(), &reason, model_turn)));
            }
            Settled::Cancelled => {
                let events = std::mem::take(events);
                return Ok(Err(cancelled(events, text.to_owned(), model_turn)));
            }
        };
        if let (Some(note), Some(last)) = (note, results.last_mut()) {
            last.output.content.push_str("\n\n");
            last.output.content.push_str(note);
        }
        self.count_down(model_turn, &mut results, events, cancel)
            .await?;
        self.deliver_inbox(model_turn, &mut results, events)?;
        Ok(Ok(TurnInput::ToolResults { results }))
    }

    /// What to tell a model that ended its turn with work unfinished, if this
    /// run checks and has not already sent it back, or what a reviewer
    /// objects to in work that is otherwise finished.
    async fn send_back(
        &mut self,
        model_turn: u32,
        events: &mut Vec<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, AgentError> {
        if model_turn >= self.config.max_model_turns.get() {
            return Ok(None);
        }
        if self.sent_back {
            return self.review(model_turn, events, cancel).await;
        }
        let checked = match self.finish_check.as_ref() {
            Some(check) => cancel.run_until_cancelled(check.unfinished()).await,
            None => Some(None),
        };
        let Some(checked) = checked else {
            return Ok(None);
        };
        let Some(reason) = checked
            .or_else(|| unfinished_checklist(events))
            .or_else(|| verify::untested_edits(events))
        else {
            return self.review(model_turn, events, cancel).await;
        };
        self.sent_back = true;
        let note = sent_back_note(&reason);
        self.emit(
            AgentEvent::SentBack {
                model_turn,
                reason: note.clone(),
            },
            events,
        )?;
        Ok(Some(note))
    }

    /// The reviewer's objections, framed for the model, the first time this
    /// run's work is reviewed.
    async fn review(
        &mut self,
        model_turn: u32,
        events: &mut Vec<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, AgentError> {
        let Some(reviewer) = self.reviewer.clone().filter(|_| !self.reviewed) else {
            return Ok(None);
        };
        self.reviewed = true;
        let task = review::task(events);
        let checks = review::latest_checks(events);
        let Some(Some(verdict)) = cancel
            .run_until_cancelled(reviewer.review(&task, &checks))
            .await
        else {
            return Ok(None);
        };
        let note =
            (!verdict.objections.is_empty()).then(|| review::objections_note(&verdict.objections));
        self.emit(
            AgentEvent::Reviewed {
                model_turn,
                objections: verdict.objections,
                error: verdict.error,
                usage: verdict.usage,
            },
            events,
        )?;
        Ok(note)
    }

    /// Hands the model, with its tool results, anything the person has typed
    /// since the last ones.
    fn deliver_inbox(
        &mut self,
        model_turn: u32,
        results: &mut [ToolResult],
        events: &mut Vec<AgentEvent>,
    ) -> Result<(), AgentError> {
        let Some(last) = results.last_mut() else {
            return Ok(());
        };
        let typed: Vec<String> = match &self.inbox {
            Some(inbox) => inbox
                .lock()
                .map(|mut inbox| std::mem::take(&mut *inbox))
                .unwrap_or_default(),
            None => return Ok(()),
        };
        for text in typed {
            let _ = write!(
                last.output.content,
                "\n\n[dino: the person wrote this while you were working:]\n{text}"
            );
            self.emit(AgentEvent::Steered { model_turn, text }, events)?;
        }
        Ok(())
    }

    /// Tells the model, with its tool results, how few turns remain, and what
    /// the finish check finds unfinished, since a run cut off by the turn limit
    /// is never sent back.
    async fn count_down(
        &mut self,
        model_turn: u32,
        results: &mut [ToolResult],
        events: &mut Vec<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let max = self.config.max_model_turns.get();
        let turns_left = max - model_turn;
        if turns_left == 0 {
            return Ok(());
        }
        if turns_left > self.config.turn_notice_at {
            // From half-way the count rides along, so the end is no surprise.
            if model_turn * 2 >= max
                && let Some(last) = results.last_mut()
            {
                let _ = write!(
                    last.output.content,
                    "\n\n[dino: {model_turn} of {max} model turns used.]"
                );
            }
            return Ok(());
        }
        self.emit(
            AgentEvent::TurnsRunningOut {
                model_turn,
                turns_left,
            },
            events,
        )?;
        let unfinished = match self.finish_check.as_ref() {
            Some(check) => cancel
                .run_until_cancelled(check.unfinished())
                .await
                .flatten(),
            None => None,
        };
        if let Some(last) = results.last_mut() {
            last.output
                .content
                .push_str(&turns_left_note(turns_left, unfinished.as_deref()));
        }
        Ok(())
    }

    /// Streams a turn, sending it again while its stream breaks partway and
    /// attempts remain.
    async fn stream_turn(
        &mut self,
        model_turn: u32,
        input: &TurnInput,
        text: &mut String,
        seen_call_ids: &mut HashSet<String>,
        events: &mut Vec<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<Streamed, AgentError> {
        let mut attempt = 1;
        loop {
            let turn = Turn {
                number: model_turn,
                attempt,
                input,
            };
            match self
                .stream_attempt(turn, text, seen_call_ids, events, cancel)
                .await?
            {
                Attempt::Broken { error } => {
                    self.emit(
                        AgentEvent::TurnRetried {
                            model_turn,
                            attempt,
                            error,
                        },
                        events,
                    )?;
                    attempt += 1;
                }
                Attempt::Finished { calls, stop_reason } => {
                    return Ok(Streamed::Finished { calls, stop_reason });
                }
                Attempt::Cancelled { turns } => return Ok(Streamed::Cancelled { turns }),
            }
        }
    }

    /// Streams one attempt at a turn, recording its events as they arrive.
    ///
    /// A stream that breaks in transit, with attempts to spare, comes back as
    /// [`Attempt::Broken`] with its text and call ids withdrawn. Nothing from a
    /// turn is acted on until it finishes, so sending it again repeats nothing
    /// the model asked for.
    async fn stream_attempt(
        &mut self,
        turn: Turn<'_>,
        text: &mut String,
        seen_call_ids: &mut HashSet<String>,
        events: &mut Vec<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<Attempt, AgentError> {
        let model_turn = turn.number;
        let Some(stream) = cancel
            .run_until_cancelled(self.session.next(turn.input.clone()))
            .await
        else {
            return Ok(Attempt::Cancelled {
                turns: model_turn.saturating_sub(1),
            });
        };
        let mut stream = stream?;
        let text_before = text.len();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut stop_reason = None;

        loop {
            let Some(next) = cancel.run_until_cancelled(stream.next()).await else {
                return Ok(Attempt::Cancelled { turns: model_turn });
            };
            let Some(event) = next else { break };
            let event = match event {
                Ok(event) => event,
                Err(ModelError::Transport(error)) if turn.attempt < self.config.turn_attempts => {
                    text.truncate(text_before);
                    for call in &calls {
                        seen_call_ids.remove(&call.id);
                    }
                    return Ok(Attempt::Broken { error });
                }
                Err(error) => return Err(error.into()),
            };
            if stop_reason.is_some() {
                return Err(AgentError::EventAfterFinished);
            }

            match &event {
                ModelEvent::TextDelta { text: delta } => text.push_str(delta),
                ModelEvent::ToolCallCompleted { call } => {
                    if !seen_call_ids.insert(call.id.clone()) {
                        return Err(AgentError::DuplicateToolCallId(call.id.clone()));
                    }
                    calls.push(call.clone());
                }
                ModelEvent::Finished { reason } => stop_reason = Some(*reason),
                ModelEvent::Started { .. }
                | ModelEvent::ReasoningDelta { .. }
                | ModelEvent::ToolCallStarted { .. }
                | ModelEvent::ToolCallArgumentsDelta { .. }
                | ModelEvent::Usage { .. } => {}
            }

            self.emit(AgentEvent::Model { model_turn, event }, events)?;
        }
        Ok(Attempt::Finished { calls, stop_reason })
    }

    /// Warns once as the window fills, and reduces history past a higher mark
    /// or once the next turn might not fit. Returns whether anything was
    /// reduced.
    ///
    /// The measure is the input tokens the provider reported for the request
    /// just made, which describes the history that produced it. It lags by one
    /// turn: a single enormous tool result can cross both marks at once and be
    /// refused before either fires. That is a mitigation, not a guarantee, and
    /// fixing it needs a local tokeniser Dino does not have.
    fn mind_the_budget(
        &mut self,
        model_turn: u32,
        window: BudgetWindow,
        warned: &mut bool,
        events: &mut Vec<AgentEvent>,
    ) -> Result<bool, AgentError> {
        let used = last_usage(events).map_or(0, |usage| usage.input_tokens);
        let filled = |percent: u8| u64::from(window.tokens) * u64::from(percent.min(100)) / 100;

        if !*warned && used >= filled(self.config.warn_at_percent) {
            *warned = true;
            self.emit(
                AgentEvent::ContextWarning {
                    model_turn,
                    input_tokens: used,
                    window_tokens: window.tokens,
                    source: Some(window.source),
                    documented_tokens: window.documented,
                },
                events,
            )?;
        }

        // The next turn replays this one's output and writes about as much
        // again, so on a small window a percentage alone leaves too little.
        let next_turn = 2 * peak_output_tokens(events);
        if used < filled(self.config.elide_at_percent)
            && used + next_turn < u64::from(window.tokens)
        {
            return Ok(false);
        }
        self.elide(model_turn, events)
    }

    /// Reduces history once, and says whether that reclaimed anything.
    fn elide(&mut self, model_turn: u32, events: &mut Vec<AgentEvent>) -> Result<bool, AgentError> {
        match self
            .session
            .elide_tool_results(self.config.keep_recent_results)
        {
            Ok(elision) if elision.results_elided + elision.calls_elided > 0 => {
                self.tools.history_lost();
                self.emit(
                    AgentEvent::Elided {
                        model_turn,
                        results_elided: elision.results_elided,
                        calls_elided: elision.calls_elided,
                        bytes_reclaimed: elision.bytes_reclaimed,
                    },
                    events,
                )?;
                Ok(true)
            }
            // Nothing to reduce, or a provider that cannot. Neither fails the
            // run: the warning has already been given, and the wall, if it
            // comes, arrives in the provider's own words.
            Ok(_) | Err(_) => Ok(false),
        }
    }

    /// Classifies, decides and, where permitted, executes one tool call.
    ///
    /// Every call that reaches this method produces exactly one [`ToolResult`],
    /// so the provider always receives a result for every call ID it issued. A
    /// refusal is an ordinary observation carrying that ID, not a control-flow
    /// escape.
    ///
    /// Returns `None` when the run was cancelled, in which case no result is
    /// invented for the call that was interrupted.
    async fn settle(
        &mut self,
        model_turn: u32,
        call: ToolCall,
        events: &mut Vec<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<Option<ToolResult>, AgentError> {
        if cancel.is_cancelled() {
            return Ok(None);
        }

        if dino_model::holds_elided_marker(&call.arguments) {
            let refusal = ToolOutput::error(
                "an argument holds the placeholder dino leaves in place of an earlier call's \
                 text, not the text itself. Read the file and send the text you mean.",
            );
            return self
                .finish_call(model_turn, call.id, refusal, events)
                .map(Some);
        }

        let action = match self.tools.classify(&call) {
            Ok(action) => action,
            Err(error) => {
                // Not classified, so not decided and not executed. The log shows
                // a call that failed before any policy could apply to it.
                return self
                    .finish_call(
                        model_turn,
                        call.id,
                        ToolOutput::error(error.to_string()),
                        events,
                    )
                    .map(Some);
            }
        };

        let Some(decision) = cancel
            .run_until_cancelled(self.policy.decide(&call, &action))
            .await
        else {
            return Ok(None);
        };
        self.emit(
            AgentEvent::ToolDecided {
                model_turn,
                call_id: call.id.clone(),
                tool: call.name.clone(),
                action,
                decision: decision.clone(),
            },
            events,
        )?;

        if let ApprovalDecision::Denied { reason } = decision {
            return self
                .finish_call(
                    model_turn,
                    call.id,
                    ToolOutput::error(format!("refused by approval policy: {reason}")),
                    events,
                )
                .map(Some);
        }

        self.emit(
            AgentEvent::ToolStarted {
                model_turn,
                call: call.clone(),
            },
            events,
        )?;
        // Dropping the execution future cancels the tool. A tool that spawns a
        // child process is responsible for killing it on drop; the core cannot
        // do that on its behalf.
        let Some(output) = cancel.run_until_cancelled(self.tools.execute(&call)).await else {
            return Ok(None);
        };
        let output = output.unwrap_or_else(|error| ToolOutput::error(error.to_string()));
        self.finish_call(model_turn, call.id, output, events)
            .map(Some)
    }

    fn finish_call(
        &mut self,
        model_turn: u32,
        call_id: String,
        output: ToolOutput,
        events: &mut Vec<AgentEvent>,
    ) -> Result<ToolResult, AgentError> {
        let result = ToolResult { call_id, output };
        self.emit(
            AgentEvent::ToolFinished {
                model_turn,
                result: result.clone(),
            },
            events,
        )?;
        Ok(result)
    }

    fn emit(&mut self, event: AgentEvent, events: &mut Vec<AgentEvent>) -> Result<(), AgentError> {
        self.sink.emit(&event)?;
        events.push(event);
        Ok(())
    }
}

/// The items of this run's latest checklist that are not done, as a reason to
/// send the model back. Read from the call as the model made it, so it holds
/// whichever tool set is attached.
fn unfinished_checklist(events: &[AgentEvent]) -> Option<String> {
    let call = events.iter().rev().find_map(|event| match event {
        AgentEvent::Model {
            event: ModelEvent::ToolCallCompleted { call },
            ..
        } if call.name == todo::TODO => Some(call),
        _ => None,
    })?;
    let open: Vec<String> = todo::items(&call.arguments)
        .ok()?
        .into_iter()
        .filter(|item| !matches!(item.status, todo::Status::Done | todo::Status::Blocked))
        .map(|item| item.text)
        .collect();
    (!open.is_empty()).then(|| {
        format!(
            "your checklist still has {} item(s) not done: {}. Finish them, or mark the \
             ones that cannot be done here blocked, with the reason.",
            open.len(),
            open.join("; ")
        )
    })
}

fn usages(events: &[AgentEvent]) -> impl DoubleEndedIterator<Item = &TokenUsage> {
    events.iter().filter_map(|event| match event {
        AgentEvent::Model {
            event: ModelEvent::Usage { usage },
            ..
        } => Some(usage),
        _ => None,
    })
}

/// The usage of a turn whose prompt and output filled the window.
///
/// A self-hosted server shrinks a turn's output allowance to whatever the
/// window has left, so a length stop there says nothing about the model.
/// The window the budget watches: the smaller of the catalogue figure and what
/// the server reported. `documented` on the result is set only when the served
/// window won, so a warning can name both.
#[derive(Clone, Copy)]
struct BudgetWindow {
    tokens: u32,
    source: WindowSource,
    documented: Option<u32>,
}

fn budget_window(documented: Option<u32>, reported: Option<u32>) -> Option<BudgetWindow> {
    // No catalogue figure, no budget. A server number alone is still a guess
    // the catalogue declined to make, and eliding against a guess cuts history.
    let documented = documented?;
    match reported {
        Some(served) if served > 0 && served < documented => Some(BudgetWindow {
            tokens: served,
            source: WindowSource::Served,
            documented: Some(documented),
        }),
        _ => Some(BudgetWindow {
            tokens: documented,
            source: WindowSource::Documented,
            documented: None,
        }),
    }
}

fn filled_window(events: &[AgentEvent], window: u32) -> Option<TokenUsage> {
    let usage = last_usage(events)?;
    let filled = usage.input_tokens + usage.output_tokens;
    (filled >= u64::from(window) - u64::from(window / 100)).then_some(usage)
}

/// The most recent usage the provider reported.
fn last_usage(events: &[AgentEvent]) -> Option<TokenUsage> {
    usages(events).next_back().copied()
}

/// The most any one turn of this run has written.
fn peak_output_tokens(events: &[AgentEvent]) -> u64 {
    usages(events)
        .map(|usage| usage.output_tokens)
        .max()
        .unwrap_or(0)
}

fn cancelled(events: Vec<AgentEvent>, text: String, model_turns: u32) -> RunResult {
    RunResult {
        events,
        text,
        stop_reason: StopReason::Cancelled,
        model_turns,
    }
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Sink(#[from] SinkError),
    #[error("model stream ended without a terminal event")]
    StreamEndedWithoutFinish,
    #[error("provider emitted an event after its terminal event")]
    EventAfterFinished,
    #[error("provider stopped for tool use without a completed tool call")]
    ToolStopWithoutCalls,
    #[error("provider ended the turn with {0} unresolved tool call(s)")]
    UnresolvedToolCalls(usize),
    #[error("provider reused tool call id {0}")]
    DuplicateToolCallId(String),
    #[error("model-turn limit reached after {0} turns")]
    ModelTurnLimit(u32),
}
