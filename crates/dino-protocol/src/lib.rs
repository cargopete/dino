//! Provider-neutral values exchanged by Dino's model, core and frontend
//! boundaries.
//!
//! Provider-native conversation history deliberately does not live here. An
//! adapter owns that history so continuation does not lose reasoning items or
//! provider-specific content ordering.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    Qwen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "efforts", rename_all = "snake_case")]
pub enum ReasoningSupport {
    None,
    Toggle,
    Effort(Vec<ReasoningEffort>),
    ProviderManaged,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SamplingDefaults {
    pub temperature: f64,
    pub top_p: f64,
    pub top_k: Option<u32>,
    pub min_p: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub repetition_penalty: Option<f64>,
}

/// Why a model is in the catalogue.
///
/// The catalogue is deliberately bounded, so anything present for another
/// reason has to say so rather than sitting alongside the supported targets and
/// being mistaken for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileStatus {
    /// A coding target that has completed the behavioural evals against a live
    /// endpoint.
    Supported,
    /// Not yet measured live, or not a coding target at all.
    Development,
}

/// Whose extensions a Chat Completions request may carry. They overlap only in
/// the standard fields; each puts thinking controls somewhere the other ignores.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatServer {
    #[default]
    Vllm,
    /// Alibaba Model Studio, including its Coding Plan.
    DashScope,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelProfile {
    /// Stable Dino configuration key.
    pub key: String,
    /// Model identifier sent to the provider by default.
    pub provider_model: String,
    pub display_name: String,
    pub provider: ProviderKind,
    pub status: ProfileStatus,
    pub context_window_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning: ReasoningSupport,
    pub parallel_tool_calls: bool,
    pub image_input: bool,
    pub tool_call_parser: Option<String>,
    pub reasoning_parser: Option<String>,
    pub sampling: Option<SamplingDefaults>,
    #[serde(default)]
    pub chat_server: ChatServer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    #[must_use]
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    #[must_use]
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub output: ToolOutput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnInput {
    User {
        content: String,
    },
    /// A person's message with images, for a profile that accepts them.
    UserWithImages {
        content: String,
        images: Vec<Image>,
    },
    ToolResults {
        results: Vec<ToolResult>,
    },
}

/// The key a tool call's arguments are wrapped in when they were not valid
/// JSON: `{key: {"raw": ..., "error": ...}}`. The tool set refuses such a call
/// with a message the model can act on, rather than the run ending.
pub const UNPARSED_ARGUMENTS: &str = "dino_unparsed_arguments";

/// An image a person attached, encoded for the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub media_type: String,
    /// Base64, without a data-URL prefix.
    pub data: String,
}

impl Image {
    #[must_use]
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Cancelled,
    /// The model reported that the task cannot be done as asked.
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEvent {
    Started { response_id: Option<String> },
    TextDelta { text: String },
    ReasoningDelta { text: String },
    ToolCallStarted { id: String, name: String },
    ToolCallArgumentsDelta { id: String, delta: String },
    ToolCallCompleted { call: ToolCall },
    Usage { usage: TokenUsage },
    Finished { reason: StopReason },
}

/// What a tool call would do, as classified by the runtime that owns the tool.
///
/// Classes are added with the tool that needs them rather than in advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolClass {
    /// Reads workspace state and changes nothing.
    ReadOnly,
    /// Changes a workspace file. The subject names which one.
    Mutating,
    /// Runs code on the host. Its effects are not bounded by the filesystem
    /// fence, and no policy auto-allows it.
    Process,
    /// Sends a request off the machine. The subject is the host, because a
    /// request can carry workspace content out in its URL.
    Network,
}

/// Which window a context warning measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowSource {
    /// The catalogue window.
    Documented,
    /// A smaller window the server reported for this session.
    Served,
}

/// A classified tool call, with the narrow subject a session rule may be keyed on.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolAction {
    pub class: ToolClass,
    /// A resolved, workspace-relative target where the tool has one.
    pub subject: Option<String>,
}

impl ToolAction {
    #[must_use]
    pub fn read_only() -> Self {
        Self {
            class: ToolClass::ReadOnly,
            subject: None,
        }
    }

    #[must_use]
    pub fn mutating(subject: impl Into<String>) -> Self {
        Self {
            class: ToolClass::Mutating,
            subject: Some(subject.into()),
        }
    }

    #[must_use]
    pub fn process(subject: impl Into<String>) -> Self {
        Self {
            class: ToolClass::Process,
            subject: Some(subject.into()),
        }
    }

    #[must_use]
    pub fn network(host: impl Into<String>) -> Self {
        Self {
            class: ToolClass::Network,
            subject: Some(host.into()),
        }
    }

    /// The key a session-scoped approval rule is stored under.
    ///
    /// Deliberately built from the tool name and resolved subject rather than
    /// from any rendered description of the action.
    #[must_use]
    pub fn rule_key(&self, tool: &str) -> String {
        match &self.subject {
            Some(subject) => format!("{tool}\u{1f}{subject}"),
            None => tool.to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    /// Allowed by classification, without asking anyone.
    Policy,
    /// Allowed by a narrow rule granted earlier in this session.
    SessionRule,
    /// Allowed by a rule the user saved for this workspace in an earlier one.
    SavedRule,
    /// Allowed once, by a person, just now.
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allowed { source: DecisionSource },
    Denied { reason: String },
}

impl ApprovalDecision {
    #[must_use]
    pub fn allowed(source: DecisionSource) -> Self {
        Self::Allowed { source }
    }

    #[must_use]
    pub fn denied(reason: impl Into<String>) -> Self {
        Self::Denied {
            reason: reason.into(),
        }
    }

    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// What the person asked, once per submission, before the first request.
    ///
    /// An agent event rather than a session record because it belongs to the
    /// turn sequence: a reader walking events in order should meet the question
    /// in its place rather than correlate a second stream by timestamp.
    Submitted {
        text: String,
    },
    Model {
        model_turn: u32,
        event: ModelEvent,
    },
    ToolDecided {
        model_turn: u32,
        call_id: String,
        tool: String,
        action: ToolAction,
        decision: ApprovalDecision,
    },
    ToolStarted {
        model_turn: u32,
        call: ToolCall,
    },
    ToolFinished {
        model_turn: u32,
        result: ToolResult,
    },
    /// The window is filling. Said once per crossing, to a person and not to the
    /// model: operator text in an assistant history is the thing RFC-0006 keeps
    /// approval out of prose for.
    ContextWarning {
        model_turn: u32,
        input_tokens: u64,
        window_tokens: u32,
        /// Which figure `window_tokens` is. Logs written before the budget
        /// recorded this omit it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<WindowSource>,
        /// Catalogue size, when the served window is the smaller one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        documented_tokens: Option<u32>,
    },
    /// A long task moved to a fresh session, carried by its progress record.
    /// RFC-0018 section 5.
    HandedOver {
        /// The session starting, counting the first as 1.
        session: u32,
        reason: String,
        pending: usize,
    },
    /// History was reduced, and by how much. An explicit event rather than a
    /// silent optimisation, so a transcript that behaves oddly afterwards can be
    /// traced to the moment it was cut.
    Elided {
        model_turn: u32,
        results_elided: usize,
        /// Earlier calls whose long arguments, such as a written file, were
        /// replaced.
        #[serde(default)]
        calls_elided: usize,
        bytes_reclaimed: usize,
    },
    /// The stream broke before the turn finished, and the turn is being sent
    /// again. Whatever it had streamed is discarded, so a reader should not
    /// take the events between the turn's start and this one as having
    /// happened.
    TurnRetried {
        model_turn: u32,
        attempt: u32,
        error: String,
    },
    /// The model was told how few turns it has left. Recorded because the note
    /// travels inside a tool result, where a reader would not otherwise know
    /// the model was the one being told.
    TurnsRunningOut {
        model_turn: u32,
        turns_left: u32,
    },
    /// The model ended its turn, a check found the work unfinished, and the
    /// model was told why and given another turn. Once per run.
    SentBack {
        model_turn: u32,
        reason: String,
    },
    /// The turn stopped for length because the prompt and its output filled
    /// the window, so history was reduced and the run went on.
    TurnCutOff {
        model_turn: u32,
        input_tokens: u64,
        output_tokens: u64,
        window_tokens: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::AgentEvent;

    #[test]
    fn an_old_context_warning_still_reads() {
        let event: AgentEvent = serde_json::from_str(
            r#"{"type":"context_warning","model_turn":1,"input_tokens":10,"window_tokens":100}"#,
        )
        .expect("old warning");
        match event {
            AgentEvent::ContextWarning {
                window_tokens,
                source,
                documented_tokens,
                ..
            } => {
                assert_eq!(window_tokens, 100);
                assert!(source.is_none());
                assert!(documented_tokens.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
