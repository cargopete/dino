//! Model sessions and Gyrfalcon's bounded built-in model catalogue.

use std::future::Future;
use std::pin::Pin;

use futures_core::Stream;
use gyr_protocol::ModelEvent;
use gyr_protocol::ModelProfile;
use gyr_protocol::ProfileStatus;
use gyr_protocol::ProviderKind;
use gyr_protocol::ReasoningEffort;
use gyr_protocol::ReasoningSupport;
use gyr_protocol::SamplingDefaults;
use gyr_protocol::TurnInput;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

pub mod anthropic;
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
        "[gyrfalcon elided {bytes} bytes of this earlier tool result to stay within the context \
         window. Run the tool again if you need it.]"
    )
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
}

/// The model used when none is selected. Open weights, so anyone can serve it.
pub const DEFAULT_PROFILE: &str = "qwen3.8-27b";

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
        claude_opus(),
        claude_sonnet(),
        qwen_coder_480b(),
        qwen_coder_next(),
        qwen_coder_30b(),
        qwen_3_6_27b(),
        qwen_3_6_35b_a3b(),
        qwen_3_8_27b(),
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
    }
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
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use pretty_assertions::assert_eq;

    use super::*;

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
        assert_eq!(all.len(), 10);
        assert!(builtin_profile(DEFAULT_PROFILE).is_some());
    }

    #[test]
    fn only_self_served_profiles_name_server_parsers() {
        for profile in builtin_profiles() {
            let served = profile.provider == ProviderKind::Qwen;
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
