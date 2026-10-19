//! Provider-neutral values exchanged by Gyrfalcon's model, core and frontend
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelProfile {
    /// Stable Gyrfalcon configuration key.
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
    /// Endpoint used when none is configured, for open weights that are also
    /// served by a provider. A self-hosted server must use `provider_model` as
    /// its served model name.
    pub hosted_api_base: Option<String>,
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
    User { content: String },
    ToolResults { results: Vec<ToolResult> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Cancelled,
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
    },
    /// History was reduced, and by how much. An explicit event rather than a
    /// silent optimisation, so a transcript that behaves oddly afterwards can be
    /// traced to the moment it was cut.
    Elided {
        model_turn: u32,
        results_elided: usize,
        bytes_reclaimed: usize,
    },
}
