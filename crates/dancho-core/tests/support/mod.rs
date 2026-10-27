//! Scripted providers and tool runtimes shared by the core's integration tests.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use dancho_core::ToolError;
use dancho_core::ToolFuture;
use dancho_core::ToolRuntime;
use dancho_core::approval::ApprovalReply;
use dancho_core::approval::Approver;
use dancho_core::approval::ReplyFuture;
use dancho_core::session::EventSink;
use dancho_core::session::SinkError;
use dancho_model::ModelError;
use dancho_model::ModelEventStream;
use dancho_model::ModelFuture;
use dancho_model::ModelSession;
use dancho_protocol::AgentEvent;
use dancho_protocol::ModelEvent;
use dancho_protocol::ModelProfile;
use dancho_protocol::StopReason;
use dancho_protocol::ToolAction;
use dancho_protocol::ToolCall;
use dancho_protocol::ToolDefinition;
use dancho_protocol::ToolOutput;
use dancho_protocol::TurnInput;
use futures_util::stream;
use serde_json::json;

/// One scripted provider turn: either a fixed event list or a bespoke stream.
pub enum Turn {
    Events(Vec<ModelEvent>),
    Stream(ModelEventStream),
}

pub struct ScriptedSession {
    profile: ModelProfile,
    turns: VecDeque<Turn>,
    pub inputs: Vec<TurnInput>,
    /// What `elide_tool_results` will claim to have done, and what it was asked.
    pub elision: Option<dancho_model::Elision>,
    pub elide_requests: Vec<usize>,
    /// The window a server would report, applied by `discover_limits`.
    pub served_window: Option<u32>,
}

impl ScriptedSession {
    pub fn new(turns: Vec<Turn>) -> Self {
        Self {
            profile: dancho_model::builtin_profiles().remove(0),
            turns: VecDeque::from(turns),
            inputs: Vec::new(),
            elision: None,
            elide_requests: Vec::new(),
            served_window: None,
        }
    }

    /// Gives the session a documented window, so the budget logic engages.
    pub fn with_window(mut self, tokens: Option<u32>) -> Self {
        self.profile.context_window_tokens = tokens;
        self
    }

    pub fn eliding(mut self, results: usize, bytes: usize) -> Self {
        self.elision = Some(dancho_model::Elision {
            results_elided: results,
            bytes_reclaimed: bytes,
            ..Default::default()
        });
        self
    }
}

impl ModelSession for ScriptedSession {
    fn profile(&self) -> &ModelProfile {
        &self.profile
    }

    fn discover_limits(&mut self) -> ModelFuture<'_, ()> {
        if let Some(served) = self.served_window {
            self.profile.context_window_tokens = Some(served);
        }
        Box::pin(async { Ok(()) })
    }

    fn elide_tool_results(
        &mut self,
        keep_recent: usize,
    ) -> Result<dancho_model::Elision, ModelError> {
        self.elide_requests.push(keep_recent);
        self.elision.ok_or_else(|| {
            ModelError::Configuration("this scripted provider does not elide".into())
        })
    }

    fn next(&mut self, input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
        self.inputs.push(input);
        let turn = self.turns.pop_front();
        Box::pin(async move {
            match turn {
                Some(Turn::Events(events)) => {
                    Ok(Box::pin(stream::iter(events.into_iter().map(Ok))) as ModelEventStream)
                }
                Some(Turn::Stream(stream)) => Ok(stream),
                None => Err(ModelError::Protocol(
                    "scripted provider has no next turn".into(),
                )),
            }
        })
    }
}

/// A tool runtime that classifies by name and records what it actually ran.
#[derive(Default)]
pub struct ScriptedTools {
    pub executed: Mutex<Vec<ToolCall>>,
}

impl ScriptedTools {
    pub fn executed_names(&self) -> Vec<String> {
        self.executed
            .lock()
            .expect("executed tool lock")
            .iter()
            .map(|call| call.name.clone())
            .collect()
    }
}

impl ToolRuntime for ScriptedTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        ["read", "search", "apply_patch", "cargo"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: format!("scripted {name}"),
                input_schema: json!({"type": "object"}),
            })
            .collect()
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        match call.name.as_str() {
            "read" | "search" => Ok(ToolAction::read_only()),
            "cargo" => Ok(ToolAction::process("cargo check --workspace")),
            "apply_patch" => {
                let path = call
                    .arguments
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| ToolError::new("apply_patch needs a path"))?;
                Ok(ToolAction::mutating(path))
            }
            name => Err(ToolError::new(format!("unknown tool {name:?}"))),
        }
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        self.executed
            .lock()
            .expect("executed tool lock")
            .push(call.clone());
        Box::pin(async { Ok(ToolOutput::success("fn main() {}")) })
    }
}

/// An approver that answers from a script and records what it was asked.
pub struct ScriptedApprover {
    replies: Mutex<VecDeque<ApprovalReply>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl ScriptedApprover {
    pub fn new(replies: Vec<ApprovalReply>) -> (Self, Arc<Mutex<Vec<String>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                replies: Mutex::new(VecDeque::from(replies)),
                asked: Arc::clone(&asked),
            },
            asked,
        )
    }
}

impl Approver for ScriptedApprover {
    fn ask(&self, call: &ToolCall, action: &ToolAction) -> ReplyFuture<'_> {
        self.asked
            .lock()
            .expect("asked lock")
            .push(action.rule_key(&call.name));
        let reply = self
            .replies
            .lock()
            .expect("reply lock")
            .pop_front()
            .unwrap_or(ApprovalReply::Reject(Some("script exhausted".into())));
        Box::pin(async move { reply })
    }
}

/// A sink that fails on the nth event, to prove the agent notices.
pub struct FailingSink {
    pub fail_at: usize,
    pub seen: usize,
}

impl EventSink for FailingSink {
    fn emit(&mut self, _event: &AgentEvent) -> Result<(), SinkError> {
        self.seen += 1;
        if self.seen == self.fail_at {
            return Err(SinkError::new("the disk is imaginary and also full"));
        }
        Ok(())
    }
}

pub fn tool_call(id: &str, name: &str, path: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({"path": path}),
    }
}

pub fn tool_turn(call: ToolCall) -> Turn {
    Turn::Events(vec![
        ModelEvent::ToolCallCompleted { call },
        ModelEvent::Finished {
            reason: StopReason::ToolUse,
        },
    ])
}

pub fn text_turn(text: &str) -> Turn {
    Turn::Events(vec![
        ModelEvent::TextDelta { text: text.into() },
        ModelEvent::Finished {
            reason: StopReason::EndTurn,
        },
    ])
}

/// A second runtime, for proving that a [`ToolSet`] dispatches by name.
pub struct EchoTool {
    pub name: &'static str,
}

impl ToolRuntime for EchoTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: self.name.into(),
            description: "echoes its own name".into(),
            input_schema: json!({"type": "object"}),
        }]
    }

    fn classify(&self, _call: &ToolCall) -> Result<ToolAction, ToolError> {
        Ok(ToolAction::read_only())
    }

    fn execute(&self, _call: &ToolCall) -> ToolFuture<'_> {
        Box::pin(async move { Ok(ToolOutput::success(self.name)) })
    }
}

/// A turn that reports usage, so the budget has something to read.
pub fn usage_turn(input_tokens: u64, text: &str) -> Turn {
    Turn::Events(vec![
        ModelEvent::TextDelta { text: text.into() },
        ModelEvent::Usage {
            usage: dancho_protocol::TokenUsage {
                input_tokens,
                cached_input_tokens: 0,
                output_tokens: 4,
                reasoning_tokens: 0,
            },
        },
        ModelEvent::Finished {
            reason: StopReason::EndTurn,
        },
    ])
}
