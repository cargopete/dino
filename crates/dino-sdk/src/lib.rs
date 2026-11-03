//! The supported way to drive Dino from another Rust program. docs/sdk.md.
//!
//! Every item is re-exported from the crate that implements it. What this
//! crate names is kept stable; the rest of those crates may change under it.

use std::path::Path;

pub use dino_core::Agent;
pub use dino_core::AgentConfig;
pub use dino_core::AgentError;
pub use dino_core::RunResult;
pub use dino_core::ToolError;
pub use dino_core::ToolFuture;
pub use dino_core::ToolRuntime;
pub use dino_core::ToolSet;
pub use dino_core::approval::AllowAll;
pub use dino_core::approval::ApprovalPolicy;
pub use dino_core::approval::ReadOnly;
pub use dino_core::session::EventSink;
pub use dino_core::session::JsonlSessionLog;
pub use dino_core::session::RunOutcome;
pub use dino_core::session::SinkError;
pub use dino_model::ModelError;
pub use dino_model::ModelSession;
pub use dino_protocol::AgentEvent;
pub use dino_protocol::ModelEvent;
pub use dino_protocol::StopReason;
pub use dino_protocol::ToolAction;
pub use dino_protocol::ToolCall;
pub use dino_protocol::ToolDefinition;
pub use dino_protocol::ToolOutput;
pub use dino_protocol::ToolResult;
pub use tokio_util::sync::CancellationToken;

use dino_model::anthropic::AnthropicConfig;
use dino_model::anthropic::AnthropicSession;
use dino_model::openai::OpenAiConfig;
use dino_model::openai::OpenAiSession;
use dino_model::qwen::QwenConfig;
use dino_model::qwen::QwenSession;
use dino_protocol::ProviderKind;
use tokio::sync::mpsc;

/// An agent as this crate assembles one.
pub type DinoAgent = Agent<Box<dyn ModelSession>, ToolSet>;

/// Opens a conversation with a model from `dino models`, reading its
/// credential from the environment as `dino` does: `ANTHROPIC_API_KEY`,
/// `OPENAI_API_KEY`, a hosted model's own key, or `QWEN_API_BASE` and
/// `QWEN_API_KEY` for a self-served one.
///
/// # Errors
///
/// Returns [`ModelError::Configuration`] for an unknown model or a missing
/// credential, before anything is sent.
pub fn connect(
    model: &str,
    system_prompt: impl Into<String>,
    tools: Vec<ToolDefinition>,
) -> Result<Box<dyn ModelSession>, ModelError> {
    connect_with(model, system_prompt.into(), tools, &|name| {
        std::env::var(name).ok().filter(|value| !value.is_empty())
    })
}

fn connect_with(
    model: &str,
    system_prompt: String,
    tools: Vec<ToolDefinition>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Box<dyn ModelSession>, ModelError> {
    let profile = dino_model::builtin_profile(model).ok_or_else(|| {
        ModelError::Configuration(format!("{model:?} is not a model `dino models` lists"))
    })?;
    let needs = |name: &str| {
        env(name).ok_or_else(|| ModelError::Configuration(format!("{model} needs {name} set")))
    };
    Ok(match profile.provider {
        ProviderKind::Anthropic => {
            let mut config = AnthropicConfig::new(needs("ANTHROPIC_API_KEY")?, profile.clone());
            config.system_prompt = system_prompt;
            config.tools = tools;
            if let Some(limit) = profile.max_output_tokens {
                config.max_output_tokens = limit;
            }
            Box::new(AnthropicSession::new(config)?)
        }
        ProviderKind::OpenAi => {
            let mut config = OpenAiConfig::new(needs("OPENAI_API_KEY")?, profile.clone());
            config.instructions = system_prompt;
            config.tools = tools;
            config.max_output_tokens = profile.max_output_tokens;
            Box::new(OpenAiSession::new(config)?)
        }
        ProviderKind::Qwen => {
            // A model with an endpoint of its own never takes QWEN_API_BASE, so
            // its key cannot be sent anywhere else.
            let (api_base, api_key) = match dino_model::hosted_endpoint(model) {
                Some(hosted) => (
                    hosted.api_base.to_owned(),
                    Some(needs(hosted.key_variable)?),
                ),
                None => (needs("QWEN_API_BASE")?, env("QWEN_API_KEY")),
            };
            let mut config = QwenConfig::new(api_base, profile.clone());
            config.api_key = api_key;
            config.system_prompt = system_prompt;
            config.tools = tools;
            config.max_output_tokens = profile.max_output_tokens;
            Box::new(QwenSession::new(config)?)
        }
    })
}

/// The file tools, confined to `root`: `read`, `search` and `list`, and the
/// edits `apply_patch` and `write_file`, which [`ReadOnly`] refuses.
///
/// # Errors
///
/// Returns [`ToolError`] when `root` is not a directory.
pub fn workspace_tools(root: impl AsRef<Path>) -> Result<ToolSet, ToolError> {
    let tools = dino_tools::WorkspaceTools::new(root, dino_tools::ToolLimits::default())?;
    ToolSet::new(vec![Box::new(tools)])
}

/// Dino's own system prompt for a session in `workspace`, with the
/// repository's instruction files. `approvals` says in a few words what the
/// policy allows, such as "read-only".
#[must_use]
pub fn system_prompt(
    workspace: &Path,
    model: &str,
    tools: &[ToolDefinition],
    approvals: &str,
) -> String {
    dino_core::prompt::system_prompt(&dino_core::prompt::PromptContext {
        workspace_root: workspace.display().to_string(),
        model: Some(model.to_owned()),
        tools: tools.iter().map(|tool| tool.name.clone()).collect(),
        approval_mode: approvals.to_owned(),
        max_model_turns: None,
        git: dino_core::git::GitSnapshot::take(workspace),
        outline: None,
        instructions: dino_core::prompt::load_instructions(None, workspace),
        memory: Vec::new(),
        skills: Vec::new(),
    })
}

/// What a run says as it goes, then how it ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    Event(AgentEvent),
    Finished(RunOutcome),
}

/// A sink that passes each event to a channel, for reading while a run is
/// awaited. Made by [`updates`].
#[derive(Debug, Clone)]
pub struct Updates(mpsc::UnboundedSender<Update>);

/// A sink for [`Agent::with_sink`], and the receiver its updates arrive on.
#[must_use]
pub fn updates() -> (Updates, mpsc::UnboundedReceiver<Update>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (Updates(sender), receiver)
}

impl EventSink for Updates {
    // A caller that dropped the receiver stopped listening; the run goes on.
    fn emit(&mut self, event: &AgentEvent) -> Result<(), SinkError> {
        let _ = self.0.send(Update::Event(event.clone()));
        Ok(())
    }

    fn finish(&mut self, outcome: &RunOutcome) -> Result<(), SinkError> {
        let _ = self.0.send(Update::Finished(outcome.clone()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_model_or_a_missing_key_fails_before_anything_is_sent() {
        let none = |_: &str| None;
        let unknown = connect_with("no-such-model", String::new(), Vec::new(), &none)
            .err()
            .unwrap();
        assert!(unknown.to_string().contains("dino models"), "{unknown}");

        let anthropic = dino_model::builtin_profiles()
            .into_iter()
            .find(|profile| profile.provider == ProviderKind::Anthropic)
            .unwrap();
        let missing = connect_with(&anthropic.key, String::new(), Vec::new(), &none)
            .err()
            .unwrap();
        assert!(
            missing.to_string().contains("ANTHROPIC_API_KEY"),
            "{missing}"
        );
        let given = |name: &str| (name == "ANTHROPIC_API_KEY").then(|| "key".to_owned());
        assert!(connect_with(&anthropic.key, String::new(), Vec::new(), &given).is_ok());
    }

    #[test]
    fn a_self_served_model_needs_an_endpoint_and_a_hosted_one_its_own_key() {
        let qwen = dino_model::builtin_profiles()
            .into_iter()
            .find(|profile| {
                profile.provider == ProviderKind::Qwen
                    && dino_model::hosted_endpoint(&profile.key).is_none()
            })
            .unwrap();
        let none = |_: &str| None;
        let error = connect_with(&qwen.key, String::new(), Vec::new(), &none)
            .err()
            .unwrap();
        assert!(error.to_string().contains("QWEN_API_BASE"), "{error}");

        // The self-served endpoint is set, and must not be taken.
        let base_only = |name: &str| (name == "QWEN_API_BASE").then(|| "http://x".to_owned());
        let error = connect_with("kimi-k3.1-pro", String::new(), Vec::new(), &base_only)
            .err()
            .unwrap();
        assert!(error.to_string().contains("MOONSHOT_API_KEY"), "{error}");
    }
}
