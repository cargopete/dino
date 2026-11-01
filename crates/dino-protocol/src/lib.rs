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
    #[serde(rename = "xhigh", alias = "x_high")]
    XHigh,
    Max,
}

impl ReasoningEffort {
    pub const ALL: [Self; 6] = [
        Self::None,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    /// The level as providers spell it, and as a flag or config file takes it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl std::fmt::Display for ReasoningEffort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

impl std::str::FromStr for ReasoningEffort {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|effort| effort.name() == text)
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|effort| effort.name()).collect();
                format!(
                    "unknown reasoning effort {text:?}; expected one of {}",
                    names.join(", ")
                )
            })
    }
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
    /// Moonshot's Kimi platform: `reasoning_effort`, and `thinking` to turn it off.
    Moonshot,
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
    /// The effort sent when nothing else sets one. Absent leaves it to the
    /// provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<ReasoningEffort>,
    pub parallel_tool_calls: bool,
    pub image_input: bool,
    pub tool_call_parser: Option<String>,
    pub reasoning_parser: Option<String>,
    pub sampling: Option<SamplingDefaults>,
    #[serde(default)]
    pub chat_server: ChatServer,
    /// List price, where one has been checked. Absent for a model served on
    /// hardware someone pays for by the hour or not at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Pricing>,
}

/// USD per million tokens, at the provider's list price.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pricing {
    pub input: f64,
    pub cached_input: f64,
    /// Absent where the provider publishes no separate rate, and cache writes
    /// are then priced as input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    pub output: f64,
    /// The provider reports reasoning beside its output count rather than
    /// inside it, and bills it as output.
    #[serde(default)]
    pub reasoning_beside_output: bool,
}

impl Pricing {
    #[must_use]
    pub const fn new(input: f64, cached_input: f64, output: f64) -> Self {
        Self {
            input,
            cached_input,
            cache_write: None,
            output,
            reasoning_beside_output: false,
        }
    }

    /// What `usage` costs in USD.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn cost(&self, usage: &TokenUsage) -> f64 {
        let plain = usage
            .input_tokens
            .saturating_sub(usage.cached_input_tokens)
            .saturating_sub(usage.cache_write_input_tokens);
        let output = if self.reasoning_beside_output {
            usage.output_tokens + usage.reasoning_tokens
        } else {
            usage.output_tokens
        };
        (plain as f64 * self.input
            + usage.cached_input_tokens as f64 * self.cached_input
            + usage.cache_write_input_tokens as f64 * self.cache_write.unwrap_or(self.input)
            + output as f64 * self.output)
            / 1_000_000.0
    }
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
        /// Dino's own words for the model, sent after the results as a message
        /// rather than inside them, where a model rightly distrusts instructions.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
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
    /// Input written to the provider's cache, inside `input_tokens` like the
    /// cached reads, and priced above plain input by those that charge for it.
    #[serde(default)]
    pub cache_write_input_tokens: u64,
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
    /// Set when a process asks to run outside the sandbox: the model's reason.
    /// Such a call is asked about every time and no rule ever covers it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unconfined: Option<String>,
}

impl ToolAction {
    #[must_use]
    pub fn read_only() -> Self {
        Self {
            class: ToolClass::ReadOnly,
            subject: None,
            unconfined: None,
        }
    }

    #[must_use]
    pub fn mutating(subject: impl Into<String>) -> Self {
        Self {
            class: ToolClass::Mutating,
            subject: Some(subject.into()),
            unconfined: None,
        }
    }

    #[must_use]
    pub fn process(subject: impl Into<String>) -> Self {
        Self {
            class: ToolClass::Process,
            subject: Some(subject.into()),
            unconfined: None,
        }
    }

    /// A process that asks to run outside the sandbox, for `reason`.
    #[must_use]
    pub fn unconfined_process(subject: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            class: ToolClass::Process,
            subject: Some(subject.into()),
            unconfined: Some(reason.into()),
        }
    }

    #[must_use]
    pub fn network(host: impl Into<String>) -> Self {
        Self {
            class: ToolClass::Network,
            subject: Some(host.into()),
            unconfined: None,
        }
    }

    /// The key a session-scoped approval rule is stored under.
    ///
    /// Deliberately built from the tool name and resolved subject rather than
    /// from any rendered description of the action.
    #[must_use]
    pub fn rule_key(&self, tool: &str) -> String {
        if self.unconfined.is_some() {
            return format!(
                "{tool}\u{1f}unconfined\u{1f}{}",
                self.subject.as_deref().unwrap_or("")
            );
        }
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
    /// What the person typed while the model worked, delivered with the next
    /// tool results rather than waiting for the run to end.
    Steered {
        model_turn: u32,
        text: String,
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
    /// A note dino sent the model after a call finished, such as a loop noticed
    /// or the work verified. It goes with the turn's results, not inside any of
    /// them, so each result stays as the tool returned it.
    Noted {
        model_turn: u32,
        call_id: String,
        note: String,
    },
    /// The model ended its turn, a check found the work unfinished, and the
    /// model was told why and given another turn. Once per run.
    SentBack {
        model_turn: u32,
        reason: String,
    },
    /// The model ended its turn with the checks passing, and a model that saw
    /// only the task, the change and the latest check results looked it over.
    /// Any objections went to the model once, as its next message.
    Reviewed {
        model_turn: u32,
        objections: Vec<String>,
        /// Why the review said nothing, when it failed rather than approved.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// The reviewer's own tokens, which are not the session's.
        #[serde(default)]
        usage: TokenUsage,
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
    use super::Pricing;
    use super::ReasoningEffort;
    use super::TokenUsage;

    #[test]
    fn an_effort_reads_as_it_is_written_and_nothing_else() {
        for effort in ReasoningEffort::ALL {
            assert_eq!(effort.name().parse::<ReasoningEffort>(), Ok(effort));
            assert_eq!(
                serde_json::to_string(&effort).unwrap(),
                format!("\"{effort}\"")
            );
        }
        let error = "hihg".parse::<ReasoningEffort>().unwrap_err();
        assert!(error.contains("hihg") && error.contains("xhigh"), "{error}");
    }

    #[test]
    fn each_kind_of_token_is_priced_at_its_own_rate() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 600_000,
            cache_write_input_tokens: 100_000,
            output_tokens: 200_000,
            reasoning_tokens: 50_000,
        };
        let price = Pricing::new(2.0, 0.2, 10.0);
        // 300k plain and 100k written at 2.00, 600k cached at 0.20, 200k out at 10.00.
        assert!((price.cost(&usage) - 2.92).abs() < 1e-9);

        let written = Pricing {
            cache_write: Some(4.0),
            ..price
        };
        assert!((written.cost(&usage) - 3.12).abs() < 1e-9);

        let beside = Pricing {
            reasoning_beside_output: true,
            ..price
        };
        assert!((beside.cost(&usage) - 3.42).abs() < 1e-9);
    }

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
