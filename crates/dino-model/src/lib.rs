//! Model sessions and Dino's bounded built-in model catalogue.

use std::future::Future;
use std::pin::Pin;

use dino_protocol::ChatServer;
use dino_protocol::ModelEvent;
use dino_protocol::ModelProfile;
use dino_protocol::Pricing;
use dino_protocol::ProfileStatus;
use dino_protocol::ProviderKind;
use dino_protocol::ReasoningEffort;
use dino_protocol::ReasoningSupport;
use dino_protocol::SamplingDefaults;
use dino_protocol::TurnInput;
use futures_core::Stream;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

pub mod anthropic;
pub mod fallback;
pub mod openai;
pub mod qwen;
mod sse;

pub type ModelEventStream =
    Pin<Box<dyn Stream<Item = Result<ModelEvent, ModelError>> + Send + 'static>>;

pub type ModelFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ModelError>> + Send + 'a>>;

/// A provider-native conversation owned by one adapter.
///
/// Implementations retain their native history and continuation handles. The
/// agent core supplies only new user content or results for the preceding tool
/// calls.
pub trait ModelSession: Send {
    fn profile(&self) -> &ModelProfile;

    /// Starts the next turn.
    ///
    /// History is committed only once the stream finishes. A stream that fails
    /// partway must leave it untouched, because the agent sends the same input
    /// again.
    fn next(&mut self, input: TurnInput) -> ModelFuture<'_, ModelEventStream>;

    /// Replaces what the profile can only assume with what the server reports,
    /// such as the context length a self-hosted model was started with. Best
    /// effort: a server that says nothing leaves the profile as it was.
    ///
    /// # Errors
    ///
    /// None of the built-in adapters fail here; the signature leaves room.
    fn discover_limits(&mut self) -> ModelFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    /// Hollows out the oldest tool results, keeping the most recent intact.
    ///
    /// The adapter owns its native history, so the adapter does the work; the
    /// core decides when and how much. What is removed is the *content* of a
    /// result, not the call or the assistant text around it, and what replaces
    /// it says so. A tool result silently becoming empty would be absent data
    /// rendering as healthy state.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Configuration`] where the provider keeps no local
    /// history to reduce. The default is that error rather than a silent
    /// success, so a provider that cannot do this cannot appear to have.
    fn elide_tool_results(&mut self, keep_recent: usize) -> Result<Elision, ModelError> {
        let _ = keep_recent;
        Err(ModelError::Configuration(
            "this provider keeps no local history to elide".into(),
        ))
    }

    /// Serialises this session's native history so it can be continued later.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError`] when the history cannot be serialised.
    fn export_state(&self) -> Result<SessionState, ModelError> {
        Err(ModelError::Configuration(
            "this provider cannot export its session state".into(),
        ))
    }

    /// Replaces this session's history with a previously exported one.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Configuration`] when the state belongs to another
    /// provider, another model, or another payload version.
    fn restore_state(&mut self, state: &SessionState) -> Result<(), ModelError> {
        let _ = state;
        Err(ModelError::Configuration(
            "this provider cannot restore a session state".into(),
        ))
    }
}

/// Below this, a result is left alone: replacing it would reclaim nothing and
/// would lose information for no gain.
pub(crate) const ELIDED_MARKER_FLOOR: usize = 160;

/// What replaces an elided tool result.
///
/// It names what happened and how much went, so the model can ask again rather
/// than conclude the file was empty.
pub(crate) fn elided_marker(bytes: usize) -> String {
    format!(
        "{ELIDED_PREFIX}{bytes}{ELIDED_INFIX} tool result to stay within the context window. \
         Run the tool again if you need it.]"
    )
}

const ELIDED_PREFIX: &str = "[dino elided ";
const ELIDED_INFIX: &str = " bytes of this earlier";

/// Whether a call's arguments carry one of the markers elision leaves behind.
///
/// A model that sees its old calls hollowed out may copy the marker into a new
/// call, as an `old_text` that can match nothing. Qwen did, in three of fifteen
/// long runs.
#[must_use]
pub fn holds_elided_marker(arguments: &serde_json::Value) -> bool {
    match arguments {
        serde_json::Value::String(text) => text.match_indices(ELIDED_PREFIX).any(|(at, _)| {
            let rest = &text[at + ELIDED_PREFIX.len()..];
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            digits > 0 && rest[digits..].starts_with(ELIDED_INFIX)
        }),
        serde_json::Value::Array(items) => items.iter().any(holds_elided_marker),
        serde_json::Value::Object(fields) => fields.values().any(holds_elided_marker),
        _ => false,
    }
}

/// Replaces every long string in an earlier call's arguments, and returns the
/// bytes reclaimed.
///
/// A long task's history fills with the model's own writes: whole files in
/// `write_file`, both sides of every `apply_patch`. The call has run and its
/// effect is on disk, so a `read` gets back anything the marker removes. Short
/// strings, such as a path, are kept so the call still says what it did.
pub(crate) fn elide_arguments(arguments: &mut serde_json::Value) -> usize {
    match arguments {
        serde_json::Value::String(text) if text.len() > ELIDED_MARKER_FLOOR => {
            let bytes = text.len();
            let marker = format!(
                "{ELIDED_PREFIX}{bytes}{ELIDED_INFIX} argument. The call ran as sent; read the \
                 file if you need what it wrote.]"
            );
            let reclaimed = bytes.saturating_sub(marker.len());
            *text = marker;
            reclaimed
        }
        serde_json::Value::Array(items) => items.iter_mut().map(elide_arguments).sum(),
        serde_json::Value::Object(fields) => fields.values_mut().map(elide_arguments).sum(),
        _ => 0,
    }
}

/// One adapter's native history, as bytes the core stores and never reads.
///
/// RFC-0003 puts native history inside the adapter so continuation does not
/// lose reasoning items or content ordering. That boundary holds here: the
/// adapter serialises its own history and the core persists an opaque payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub provider: ProviderKind,
    pub model_key: String,
    /// Bumped when a payload shape changes, so an old file fails loudly rather
    /// than deserialising into something almost right.
    pub version: u32,
    pub payload: Value,
}

impl SessionState {
    /// Refuses a state that does not belong to this session.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Configuration`] on a provider, model or version
    /// that does not match. All three are refusals rather than warnings:
    /// Anthropic history in a Qwen session is not a degraded conversation, it
    /// is a corrupt one, and continuing an Opus conversation on Sonnet is a
    /// decision a person should make deliberately.
    pub fn check(&self, profile: &ModelProfile, version: u32) -> Result<(), ModelError> {
        if self.provider != profile.provider {
            return Err(ModelError::Configuration(format!(
                "this state belongs to a {:?} session, not {:?}",
                self.provider, profile.provider
            )));
        }
        if self.model_key != profile.key {
            return Err(ModelError::Configuration(format!(
                "this state belongs to model {}, not {}",
                self.model_key, profile.key
            )));
        }
        if self.version != version {
            return Err(ModelError::Configuration(format!(
                "this state is version {}, and this build reads version {version}",
                self.version
            )));
        }
        Ok(())
    }
}

/// What one elision actually removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Elision {
    pub results_elided: usize,
    /// Earlier tool calls whose long arguments were replaced.
    pub calls_elided: usize,
    pub bytes_reclaimed: usize,
}

/// Lets a frontend hold one boxed session whatever provider it selected.
impl ModelSession for Box<dyn ModelSession> {
    fn profile(&self) -> &ModelProfile {
        (**self).profile()
    }

    fn next(&mut self, input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
        (**self).next(input)
    }

    fn discover_limits(&mut self) -> ModelFuture<'_, ()> {
        (**self).discover_limits()
    }

    fn elide_tool_results(&mut self, keep_recent: usize) -> Result<Elision, ModelError> {
        (**self).elide_tool_results(keep_recent)
    }

    fn export_state(&self) -> Result<SessionState, ModelError> {
        (**self).export_state()
    }

    fn restore_state(&mut self, state: &SessionState) -> Result<(), ModelError> {
        (**self).restore_state(state)
    }
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("model transport failed: {0}")]
    Transport(String),
    #[error("provider protocol failed: {0}")]
    Protocol(String),
    #[error("model configuration is invalid: {0}")]
    Configuration(String),
    /// The provider refused the account itself: no quota left, unpaid, or not
    /// authorised. Retrying will not help; another provider might.
    #[error("the provider refused the account: {0}")]
    Account(String),
}

/// One HTTP client for every provider. Alibaba's Coding Plan refuses a request
/// that carries no User-Agent, and reqwest sends none by default.
pub(crate) fn http_client() -> Result<reqwest::Client, ModelError> {
    reqwest::Client::builder()
        .user_agent(concat!("dino/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| ModelError::Configuration(format!("HTTP client: {error}")))
}

/// Arguments that did not parse, wrapped for the tool set to refuse with a
/// message the model can act on. See [`dino_protocol::UNPARSED_ARGUMENTS`].
pub(crate) fn unparsed(raw: &str, error: &serde_json::Error) -> Value {
    serde_json::json!({ dino_protocol::UNPARSED_ARGUMENTS: {
        "raw": raw,
        "error": error.to_string(),
    }})
}

/// The model used when none is selected, chosen on the 2026-10-30 run in
/// `evals/MODEL-CHOICE.md`.
pub const DEFAULT_PROFILE: &str = "kimi-k3.1-pro";

/// Where a hosted model on the chat adapter is served, for the profiles that
/// do not take the Qwen endpoint, and the variable that holds its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostedEndpoint {
    pub api_base: &'static str,
    pub key_variable: &'static str,
}

#[must_use]
pub fn hosted_endpoint(key: &str) -> Option<HostedEndpoint> {
    let (api_base, key_variable) = match key {
        "kimi-k3.1-pro" => ("https://api.moonshot.ai/v1", "MOONSHOT_API_KEY"),
        "grok-4.7" => ("https://api.x.ai/v1", "XAI_API_KEY"),
        _ => return None,
    };
    Some(HostedEndpoint {
        api_base,
        key_variable,
    })
}

/// The MVP coding targets, without development-only entries.
#[must_use]
pub fn supported_profiles() -> Vec<ModelProfile> {
    builtin_profiles()
        .into_iter()
        .filter(|profile| profile.status == ProfileStatus::Supported)
        .collect()
}

#[must_use]
pub fn builtin_profiles() -> Vec<ModelProfile> {
    vec![
        terra(),
        terra_6_1(),
        claude_opus(),
        claude_opus_5_5(),
        claude_sonnet(),
        claude_sonnet_5_5(),
        claude_haiku_5_5(),
        qwen_coder_480b(),
        qwen_coder_next(),
        qwen_coder_30b(),
        qwen_3_6_27b(),
        qwen_3_6_35b_a3b(),
        qwen_3_8_27b(),
        qwen_3_7_plus(),
        qwen_3_8_max(),
        qwen_3_7_max(),
        deepseek_v4_pro(),
        glm_5_3(),
        kimi_k3_1_pro(),
        grok_4_7(),
        qwen3_8b(),
    ]
}

#[must_use]
pub fn builtin_profile(key: &str) -> Option<ModelProfile> {
    builtin_profiles()
        .into_iter()
        .find(|profile| profile.key == key)
}

fn terra() -> ModelProfile {
    ModelProfile {
        key: "terra".into(),
        provider_model: "gpt-5.6-terra".into(),
        display_name: "GPT-5.6 Terra".into(),
        provider: ProviderKind::OpenAi,
        status: ProfileStatus::Development,
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        reasoning: ReasoningSupport::Effort(vec![
            ReasoningEffort::None,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ]),
        parallel_tool_calls: true,
        image_input: true,
        tool_call_parser: None,
        reasoning_parser: None,
        sampling: None,
        chat_server: ChatServer::Vllm,
        price: None,
    }
}

/// Released 2026-10-27 at half `gpt-5.6-terra`'s price; window and efforts assumed unchanged.
fn terra_6_1() -> ModelProfile {
    ModelProfile {
        key: "terra-6.1".into(),
        provider_model: "gpt-6.1-terra".into(),
        display_name: "GPT-6.1 Terra".into(),
        price: Some(Pricing::new(1.00, 0.05, 5.00)),
        ..terra()
    }
}

/// **Provider-documented, inspected 2026-10-28:** `claude-opus-5-5` has the
/// same window and output as Opus 5, cannot disable thinking at any effort, and
/// offers fast mode. Kept beside `claude-opus` so earlier eval results keep
/// their meaning.
fn claude_opus_5_5() -> ModelProfile {
    ModelProfile {
        key: "claude-opus-5.5".into(),
        provider_model: "claude-opus-5-5".into(),
        display_name: "Claude Opus 5.5".into(),
        status: ProfileStatus::Development,
        price: Some(Pricing::new(4.00, 0.20, 20.00)),
        ..claude_opus()
    }
}

/// **Provider-documented, inspected 2026-10-28:** `claude-sonnet-5-5` keeps
/// Sonnet 5's window, output and price, and cannot disable thinking at any effort.
fn claude_sonnet_5_5() -> ModelProfile {
    ModelProfile {
        key: "claude-sonnet-5.5".into(),
        provider_model: "claude-sonnet-5-5".into(),
        display_name: "Claude Sonnet 5.5".into(),
        status: ProfileStatus::Development,
        price: Some(Pricing::new(2.00, 0.20, 10.00)),
        ..claude_sonnet()
    }
}

/// **Provider-documented, inspected 2026-10-28:** `claude-haiku-5-5` has a
/// 1,000,000-token window, up to 128,000 output tokens, every effort level from
/// low through max (default medium), and disables thinking only at high or below.
fn claude_haiku_5_5() -> ModelProfile {
    ModelProfile {
        key: "claude-haiku-5.5".into(),
        provider_model: "claude-haiku-5-5".into(),
        display_name: "Claude Haiku 5.5".into(),
        status: ProfileStatus::Development,
        price: Some(Pricing::new(0.10, 0.01, 0.50)),
        ..claude_sonnet()
    }
}

fn claude_opus() -> ModelProfile {
    ModelProfile {
        key: "claude-opus".into(),
        provider_model: "claude-opus-5".into(),
        display_name: "Claude Opus".into(),
        provider: ProviderKind::Anthropic,
        status: ProfileStatus::Supported,
        context_window_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        reasoning: ReasoningSupport::Effort(vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ]),
        parallel_tool_calls: true,
        image_input: true,
        tool_call_parser: None,
        reasoning_parser: None,
        sampling: None,
        chat_server: ChatServer::Vllm,
        price: None,
    }
}

/// **Provider-documented, inspected 2026-08-27:** `claude-sonnet-5` has a
/// 1,000,000-token context window, supports up to 128,000 output tokens, takes
/// `thinking: {type: "adaptive"}` as its only on-mode, and accepts every effort
/// level from low through max. It is priced below Opus, which is why a person
/// watching the bill reaches for it.
fn claude_sonnet() -> ModelProfile {
    ModelProfile {
        key: "claude-sonnet".into(),
        provider_model: "claude-sonnet-5".into(),
        display_name: "Claude Sonnet".into(),
        provider: ProviderKind::Anthropic,
        status: ProfileStatus::Supported,
        context_window_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        reasoning: ReasoningSupport::Effort(vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ]),
        parallel_tool_calls: true,
        image_input: true,
        tool_call_parser: None,
        reasoning_parser: None,
        sampling: None,
        chat_server: ChatServer::Vllm,
        price: None,
    }
}

fn qwen_coder_480b() -> ModelProfile {
    qwen_coder(
        "qwen3-coder-480b-a35b",
        "Qwen/Qwen3-Coder-480B-A35B-Instruct",
        "Qwen3-Coder 480B-A35B",
        Some(SamplingDefaults {
            temperature: 0.7,
            top_p: 0.8,
            top_k: Some(20),
            min_p: None,
            presence_penalty: None,
            repetition_penalty: Some(1.05),
        }),
    )
}

fn qwen_coder_next() -> ModelProfile {
    qwen_coder(
        "qwen3-coder-next",
        "Qwen/Qwen3-Coder-Next",
        "Qwen3-Coder-Next 80B-A3B",
        Some(SamplingDefaults {
            temperature: 1.0,
            top_p: 0.95,
            top_k: Some(40),
            min_p: None,
            presence_penalty: None,
            repetition_penalty: None,
        }),
    )
}

fn qwen_coder_30b() -> ModelProfile {
    qwen_coder(
        "qwen3-coder-30b-a3b",
        "Qwen/Qwen3-Coder-30B-A3B-Instruct",
        "Qwen3-Coder 30B-A3B",
        Some(SamplingDefaults {
            temperature: 0.7,
            top_p: 0.8,
            top_k: Some(20),
            min_p: None,
            presence_penalty: None,
            repetition_penalty: Some(1.05),
        }),
    )
}

fn qwen_coder(
    key: &str,
    provider_model: &str,
    display_name: &str,
    sampling: Option<SamplingDefaults>,
) -> ModelProfile {
    ModelProfile {
        key: key.into(),
        provider_model: provider_model.into(),
        display_name: display_name.into(),
        provider: ProviderKind::Qwen,
        status: ProfileStatus::Development,
        context_window_tokens: Some(262_144),
        max_output_tokens: None,
        reasoning: ReasoningSupport::None,
        parallel_tool_calls: true,
        image_input: false,
        tool_call_parser: Some("qwen3_coder".into()),
        reasoning_parser: None,
        sampling,
        chat_server: ChatServer::Vllm,
        price: None,
    }
}

fn qwen_3_6_27b() -> ModelProfile {
    qwen_3_6("qwen3.6-27b", "Qwen/Qwen3.6-27B", "Qwen3.6 27B")
}

fn qwen_3_6_35b_a3b() -> ModelProfile {
    qwen_3_6("qwen3.6-35b-a3b", "Qwen/Qwen3.6-35B-A3B", "Qwen3.6 35B-A3B")
}

/// Apache-2.0 weights at `Qwen/Qwen3.8-27B`. The server must serve them under
/// the name `qwen3.8-27b`. **Observed 2026-10-19** on vLLM 0.32.0 with the
/// recipe's parsers: tool calls arrive structured and reasoning separately.
/// Thinking control and sampling are not yet measured, so are left unset.
fn qwen_3_8_27b() -> ModelProfile {
    ModelProfile {
        key: "qwen3.8-27b".into(),
        provider_model: "qwen3.8-27b".into(),
        display_name: "Qwen3.8 27B".into(),
        provider: ProviderKind::Qwen,
        status: ProfileStatus::Development,
        context_window_tokens: Some(262_144),
        max_output_tokens: None,
        reasoning: ReasoningSupport::None,
        parallel_tool_calls: true,
        image_input: true,
        tool_call_parser: Some("qwen3_coder".into()),
        reasoning_parser: Some("qwen3".into()),
        sampling: None,
        chat_server: ChatServer::Vllm,
        price: None,
    }
}

/// Models Alibaba serves through Model Studio and its Coding and Token Plans,
/// all over the same `DashScope` dialect. **Not measured.** Alibaba publishes no
/// windows we have checked, so each is a floor; sampling is left to the server.
/// Each accepted a streamed tool call with thinking on, probed 2026-10-25.
fn dashscope(key: &str, display_name: &str, window: u32, image_input: bool) -> ModelProfile {
    ModelProfile {
        key: key.into(),
        provider_model: key.into(),
        display_name: format!("{display_name} (Alibaba Model Studio)"),
        provider: ProviderKind::Qwen,
        status: ProfileStatus::Development,
        context_window_tokens: Some(window),
        max_output_tokens: None,
        reasoning: ReasoningSupport::Toggle,
        parallel_tool_calls: true,
        image_input,
        tool_call_parser: None,
        reasoning_parser: None,
        sampling: None,
        chat_server: ChatServer::DashScope,
        price: None,
    }
}

fn qwen_3_7_plus() -> ModelProfile {
    dashscope("qwen3.7-plus", "Qwen3.7-Plus", 262_144, true)
}

fn qwen_3_8_max() -> ModelProfile {
    dashscope("qwen3.8-max", "Qwen3.8-Max", 262_144, true)
}

fn qwen_3_7_max() -> ModelProfile {
    ModelProfile {
        price: Some(Pricing::new(2.50, 0.50, 7.50)),
        ..dashscope("qwen3.7-max", "Qwen3.7-Max", 262_144, false)
    }
}

fn deepseek_v4_pro() -> ModelProfile {
    dashscope("deepseek-v4-pro", "DeepSeek V4 Pro", 131_072, false)
}

/// OpenAI-compatible chat endpoints, served through the Qwen adapter at the
/// address and with the key [`hosted_endpoint`] names.
fn kimi_k3_1_pro() -> ModelProfile {
    ModelProfile {
        display_name: "Kimi K3.1 Pro (Moonshot)".into(),
        // Low measured 2026-10-31 at 156 reasoning tokens against the default's
        // 558-844, in half the time.
        reasoning: ReasoningSupport::Effort(vec![
            ReasoningEffort::None,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
        ]),
        chat_server: ChatServer::Moonshot,
        // platform.kimi.ai/docs/pricing/chat, read 2026-10-31: writes at the
        // default 5-minute TTL cost input, 1-hour ones $4.00, which Dino never asks for.
        price: Some(Pricing {
            cache_write: Some(2.00),
            ..Pricing::new(2.00, 0.20, 10.00)
        }),
        ..dashscope("kimi-k3.1-pro", "", 1_048_576, true)
    }
}

fn grok_4_7() -> ModelProfile {
    ModelProfile {
        display_name: "Grok 4.7 (xAI)".into(),
        price: Some(Pricing {
            reasoning_beside_output: true,
            ..Pricing::new(2.00, 0.50, 6.00)
        }),
        ..dashscope("grok-4.7", "", 500_000, false)
    }
}

fn glm_5_3() -> ModelProfile {
    dashscope("glm-5.3", "GLM-5.3", 131_072, false)
}

/// A small local model, present as a development target rather than a coding
/// target.
///
/// It exists so the loop, the tool round trip and the approval path can be
/// exercised against real inference on a laptop. It is not claimed to be a
/// useful Rust coding agent, and it has not been through a conformance suite.
///
/// **Not measured.** The provider model string is Ollama's tag format. The
/// listed context window is the model's native one; Ollama serves a far smaller
/// default unless `num_ctx` is raised, which will silently truncate the system
/// prompt and tool schemas before the model ever sees them.
fn qwen3_8b() -> ModelProfile {
    ModelProfile {
        key: "qwen3-8b".into(),
        provider_model: "qwen3:8b".into(),
        display_name: "Qwen3 8B (local development)".into(),
        provider: ProviderKind::Qwen,
        status: ProfileStatus::Development,
        context_window_tokens: Some(32_768),
        max_output_tokens: None,
        reasoning: ReasoningSupport::Toggle,
        parallel_tool_calls: false,
        image_input: false,
        // vLLM's documented parser for Qwen3, recorded for the day this is
        // served by something other than Ollama, which uses its own template.
        tool_call_parser: Some("hermes".into()),
        reasoning_parser: Some("qwen3".into()),
        sampling: Some(SamplingDefaults {
            temperature: 0.6,
            top_p: 0.95,
            top_k: Some(20),
            min_p: Some(0.0),
            presence_penalty: Some(0.0),
            repetition_penalty: Some(1.0),
        }),
        chat_server: ChatServer::Vllm,
        price: None,
    }
}

fn qwen_3_6(key: &str, provider_model: &str, display_name: &str) -> ModelProfile {
    ModelProfile {
        key: key.into(),
        provider_model: provider_model.into(),
        display_name: display_name.into(),
        provider: ProviderKind::Qwen,
        status: ProfileStatus::Development,
        context_window_tokens: Some(262_144),
        max_output_tokens: None,
        reasoning: ReasoningSupport::Toggle,
        parallel_tool_calls: true,
        image_input: true,
        tool_call_parser: Some("qwen3_coder".into()),
        reasoning_parser: Some("qwen3".into()),
        sampling: Some(SamplingDefaults {
            temperature: 0.6,
            top_p: 0.95,
            top_k: Some(20),
            min_p: Some(0.0),
            presence_penalty: Some(0.0),
            repetition_penalty: Some(1.0),
        }),
        chat_server: ChatServer::Vllm,
        price: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_copied_elision_marker_is_recognised_and_ordinary_text_is_not() {
        let mut arguments = serde_json::json!({"path": "src/lib.rs", "old_text": "x".repeat(400)});
        elide_arguments(&mut arguments);
        assert!(holds_elided_marker(&arguments));
        assert!(holds_elided_marker(&serde_json::json!([elided_marker(9)])));

        // Dino's own source, which a model working on it may quote.
        for text in [
            "plain text",
            "[dino elided {bytes} bytes of this earlier argument",
            "assert!(content.starts_with(\"[dino elided 5000 bytes\"))",
        ] {
            assert!(
                !holds_elided_marker(&serde_json::json!({"t": text})),
                "{text}"
            );
        }
    }

    #[test]
    fn only_profiles_measured_live_are_supported() {
        let supported = supported_profiles()
            .into_iter()
            .map(|profile| profile.provider_model)
            .collect::<Vec<_>>();
        assert_eq!(supported, vec!["claude-opus-5", "claude-sonnet-5"]);

        let all = builtin_profiles();
        let unique_keys = all
            .iter()
            .map(|profile| profile.key.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(unique_keys.len(), all.len());
        assert_eq!(all.len(), 21);
        assert!(builtin_profile(DEFAULT_PROFILE).is_some());
    }

    #[test]
    fn list_prices_live_with_the_profiles_they_were_checked_for() {
        let priced: Vec<(String, f64, f64, f64)> = builtin_profiles()
            .into_iter()
            .filter_map(|profile| {
                let price = profile.price?;
                Some((profile.key, price.input, price.cached_input, price.output))
            })
            .collect();
        let expected = [
            ("terra-6.1", 1.00, 0.05, 5.00),
            ("claude-opus-5.5", 4.00, 0.20, 20.00),
            ("claude-sonnet-5.5", 2.00, 0.20, 10.00),
            ("claude-haiku-5.5", 0.10, 0.01, 0.50),
            ("qwen3.7-max", 2.50, 0.50, 7.50),
            ("kimi-k3.1-pro", 2.00, 0.20, 10.00),
            ("grok-4.7", 2.00, 0.50, 6.00),
        ]
        .map(|(key, input, cached, output)| (key.to_owned(), input, cached, output));
        assert_eq!(priced, expected);
        let kimi = builtin_profile("kimi-k3.1-pro").and_then(|profile| profile.price);
        assert_eq!(kimi.and_then(|price| price.cache_write), Some(2.00));
        for profile in builtin_profiles() {
            let beside = profile
                .price
                .is_some_and(|price| price.reasoning_beside_output);
            assert_eq!(beside, profile.key == "grok-4.7", "{}", profile.key);
        }
    }

    #[test]
    fn hosted_endpoints_belong_to_chat_profiles() {
        for key in ["kimi-k3.1-pro", "grok-4.7"] {
            let profile = builtin_profile(key).expect("a built-in profile");
            assert_eq!(profile.provider, ProviderKind::Qwen, "{key}");
            assert!(hosted_endpoint(key).is_some(), "{key}");
        }
        assert_eq!(hosted_endpoint("qwen3.7-max"), None);
    }

    #[test]
    fn only_self_served_profiles_name_server_parsers() {
        for profile in builtin_profiles() {
            let served =
                profile.provider == ProviderKind::Qwen && profile.chat_server == ChatServer::Vllm;
            assert_eq!(
                profile.tool_call_parser.is_some(),
                served,
                "{}",
                profile.key
            );
        }
    }

    #[test]
    fn qwen_profiles_declare_parser_and_reasoning_differences() {
        let profiles = builtin_profiles();
        let qwen = profiles
            .iter()
            .filter(|profile| {
                profile.provider == ProviderKind::Qwen
                    && profile.chat_server == ChatServer::Vllm
                    && !matches!(profile.key.as_str(), "qwen3-8b" | "qwen3.8-27b")
            })
            .collect::<Vec<_>>();

        assert_eq!(qwen.len(), 5);
        assert!(
            qwen.iter()
                .all(|profile| profile.tool_call_parser.as_deref() == Some("qwen3_coder"))
        );
        assert_eq!(qwen[0].reasoning, ReasoningSupport::None);
        assert_eq!(qwen[1].reasoning, ReasoningSupport::None);
        assert_eq!(qwen[2].reasoning, ReasoningSupport::None);
        assert_eq!(qwen[3].reasoning, ReasoningSupport::Toggle);
        assert_eq!(qwen[4].reasoning, ReasoningSupport::Toggle);
        assert_eq!(qwen[3].reasoning_parser.as_deref(), Some("qwen3"));
        assert_eq!(qwen[4].reasoning_parser.as_deref(), Some("qwen3"));
    }
}
