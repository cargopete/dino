//! Gyrfalcon's provider-neutral act-observe loop.
//!
//! The runtime classifies a tool call, a policy decides it, and this core
//! records the decision before dispatching. Approval is enforced here rather
//! than in a decorator so that the session log can honestly claim to hold the
//! proposed action, the decision, the execution and the result.

pub mod approval;
pub mod prompt;
pub mod resume;
pub mod session;
pub mod workspace;

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::num::NonZeroU32;
use std::pin::Pin;

use futures_util::StreamExt;
use gyr_model::ModelError;
use gyr_model::ModelSession;
use gyr_protocol::AgentEvent;
use gyr_protocol::ApprovalDecision;
use gyr_protocol::ModelEvent;
use gyr_protocol::StopReason;
use gyr_protocol::ToolAction;
use gyr_protocol::ToolCall;
use gyr_protocol::ToolDefinition;
use gyr_protocol::ToolOutput;
use gyr_protocol::ToolResult;
use gyr_protocol::TurnInput;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::approval::ApprovalPolicy;
use crate::session::EventSink;
use crate::session::NullSink;
use crate::session::RunOutcome;
use crate::session::SinkError;

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + 'a>>;

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
}

/// Several tool runtimes behind one, dispatching by tool name.
pub struct ToolSet {
    runtimes: Vec<Box<dyn ToolRuntime>>,
    /// Tool name to the index of the runtime that owns it.
    owners: HashMap<String, usize>,
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
        for (index, runtime) in runtimes.iter().enumerate() {
            for definition in runtime.definitions() {
                if owners.insert(definition.name.clone(), index).is_some() {
                    return Err(ToolError::new(format!(
                        "two tool runtimes both offer {:?}",
                        definition.name
                    )));
                }
            }
        }
        Ok(Self { runtimes, owners })
    }

    fn owner(&self, call: &ToolCall) -> Result<&dyn ToolRuntime, ToolError> {
        self.owners
            .get(&call.name)
            .and_then(|index| self.runtimes.get(*index))
            .map(AsRef::as_ref)
            .ok_or_else(|| ToolError::new(format!("unknown tool {:?}", call.name)))
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
        self.owner(call)?.classify(call)
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        match self.owner(call) {
            Ok(runtime) => runtime.execute(call),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
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
    /// Percentage of the documented window past which a person is told.
    ///
    /// A percentage rather than a fraction, because that is what a person
    /// setting it would write, and because integer arithmetic against a token
    /// count needs no casting to get wrong.
    pub warn_at_percent: u8,
    /// Percentage past which history is reduced.
    pub elide_at_percent: u8,
    /// Tool results kept intact when reducing.
    pub keep_recent_results: usize,
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

pub struct Agent<S, T> {
    session: S,
    tools: T,
    policy: Box<dyn ApprovalPolicy>,
    sink: Box<dyn EventSink>,
    config: AgentConfig,
    // Kept outside `drive` so a run that ends in an error still reports what it
    // spent.
    turns_started: u32,
}

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
            turns_started: 0,
        }
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
        let mut input = TurnInput::User {
            content: user_message,
        };
        let mut text = String::new();
        let mut seen_call_ids = HashSet::new();
        // A window has one documented size or none. Without one, none of this
        // runs: guessing a window and cutting history against the guess would
        // be worse than an honest refusal later.
        let window = self.session.profile().context_window_tokens;
        let mut warned = false;

        for model_turn in 1..=self.config.max_model_turns.get() {
            if cancel.is_cancelled() {
                return Ok(cancelled(events, text, model_turn.saturating_sub(1)));
            }
            self.turns_started = model_turn;

            let mut stream = match cancel.run_until_cancelled(self.session.next(input)).await {
                Some(stream) => stream?,
                None => return Ok(cancelled(events, text, model_turn.saturating_sub(1))),
            };
            let mut calls = Vec::new();
            let mut stop_reason = None;

            loop {
                let Some(next) = cancel.run_until_cancelled(stream.next()).await else {
                    return Ok(cancelled(events, text, model_turn));
                };
                let Some(event) = next else { break };
                let event = event?;
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

                self.emit(AgentEvent::Model { model_turn, event }, &mut events)?;
            }

            if let Some(window) = window {
                self.mind_the_budget(model_turn, window, &mut warned, &mut events)?;
            }

            let stop_reason = stop_reason.ok_or(AgentError::StreamEndedWithoutFinish)?;
            match stop_reason {
                StopReason::ToolUse => {
                    if calls.is_empty() {
                        return Err(AgentError::ToolStopWithoutCalls);
                    }

                    let mut results = Vec::with_capacity(calls.len());
                    for call in calls {
                        let Some(result) =
                            self.settle(model_turn, call, &mut events, cancel).await?
                        else {
                            return Ok(cancelled(events, text, model_turn));
                        };
                        results.push(result);
                    }
                    input = TurnInput::ToolResults { results };
                }
                StopReason::EndTurn => {
                    if !calls.is_empty() {
                        return Err(AgentError::UnresolvedToolCalls(calls.len()));
                    }
                    return Ok(RunResult {
                        events,
                        text,
                        stop_reason,
                        model_turns: model_turn,
                    });
                }
                StopReason::MaxTokens | StopReason::Refusal | StopReason::Cancelled => {
                    return Ok(RunResult {
                        events,
                        text,
                        stop_reason,
                        model_turns: model_turn,
                    });
                }
            }
        }

        Err(AgentError::ModelTurnLimit(
            self.config.max_model_turns.get(),
        ))
    }

    /// Warns once as the window fills, and reduces history past a higher mark.
    ///
    /// The measure is the input tokens the provider reported for the request
    /// just made, which describes the history that produced it. It lags by one
    /// turn: a single enormous tool result can cross both marks at once and be
    /// refused before either fires. That is a mitigation, not a guarantee, and
    /// fixing it needs a local tokeniser Gyrfalcon does not have.
    fn mind_the_budget(
        &mut self,
        model_turn: u32,
        window: u32,
        warned: &mut bool,
        events: &mut Vec<AgentEvent>,
    ) -> Result<(), AgentError> {
        let used = last_input_tokens(events);
        let filled = |percent: u8| u64::from(window) * u64::from(percent.min(100)) / 100;

        if !*warned && used >= filled(self.config.warn_at_percent) {
            *warned = true;
            self.emit(
                AgentEvent::ContextWarning {
                    model_turn,
                    input_tokens: used,
                    window_tokens: window,
                },
                events,
            )?;
        }

        if used < filled(self.config.elide_at_percent) {
            return Ok(());
        }
        match self
            .session
            .elide_tool_results(self.config.keep_recent_results)
        {
            Ok(elision) if elision.results_elided > 0 => self.emit(
                AgentEvent::Elided {
                    model_turn,
                    results_elided: elision.results_elided,
                    bytes_reclaimed: elision.bytes_reclaimed,
                },
                events,
            ),
            // Nothing to reduce, or a provider that cannot. Neither fails the
            // run: the warning has already been given, and the wall, if it
            // comes, arrives in the provider's own words.
            Ok(_) | Err(_) => Ok(()),
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

/// The most recent usage the provider reported, or zero if it has reported none.
fn last_input_tokens(events: &[AgentEvent]) -> u64 {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            AgentEvent::Model {
                event: ModelEvent::Usage { usage },
                ..
            } => Some(usage.input_tokens),
            _ => None,
        })
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
