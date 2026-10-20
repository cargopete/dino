//! Flags, environment and the provider session they select.
//!
//! Configuration files arrived in RFC-0015, once the command surface had
//! settled enough to mirror. `settings.rs` owns the layering; this module owns
//! what a resolved setting turns into.

use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use gyr_model::DEFAULT_PROFILE;
use gyr_model::ModelSession;
use gyr_model::anthropic::AnthropicConfig;
use gyr_model::anthropic::AnthropicSession;
use gyr_model::builtin_profile;
use gyr_model::builtin_profiles;
use gyr_model::openai::OpenAiConfig;
use gyr_model::openai::OpenAiSession;
use gyr_model::qwen::QwenConfig;
use gyr_model::qwen::QwenSession;
use gyr_protocol::ModelProfile;
use gyr_protocol::ProviderKind;
use gyr_protocol::ToolDefinition;
use gyr_sandbox::Sandbox;
use gyr_sandbox::Unconfined;

/// How much the operating system confines a process Gyrfalcon starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[clap(rename_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// Writes confined to the workspace, network denied.
    Workspace,
    /// Nothing confined. Never a default, always recorded.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[clap(rename_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    /// Read-only calls proceed; mutations ask a person.
    Interactive,
    /// Mutations are refused without asking.
    ReadOnly,
    /// Everything proceeds without asking. Explicitly requested only.
    AllowAll,
}

impl ApprovalMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Interactive => "interactive (mutations require approval)",
            Self::ReadOnly => "read-only (mutations are refused)",
            Self::AllowAll => "allow-all (nothing is asked)",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunSettings {
    pub profile: ModelProfile,
    pub workspace: PathBuf,
    pub log_path: PathBuf,
    pub max_turns: NonZeroU32,
    pub mode: ApprovalMode,
    pub show_reasoning: bool,
    pub sandbox: SandboxMode,
    /// Overrides the endpoint for a self-served model.
    pub api_base: Option<String>,
    /// Asks a toggling model not to think. Absent leaves the server's default.
    pub disable_thinking: bool,
    /// Emit no terminal colour. Resolved like everything else, so a config file
    /// can ask for it.
    pub plain: bool,
}

/// Resolves the model profile named by a flag or `GYR_MODEL`.
pub fn resolve_profile(flag: Option<&str>) -> Result<ModelProfile> {
    let key = match flag {
        Some(key) => key.to_owned(),
        None => std::env::var("GYR_MODEL").unwrap_or_else(|_| DEFAULT_PROFILE.to_owned()),
    };
    builtin_profile(&key).with_context(|| format!("unknown model {key:?} ({})", known_keys()))
}

fn known_keys() -> String {
    let keys: Vec<String> = builtin_profiles()
        .into_iter()
        .map(|profile| profile.key)
        .collect();
    format!("known models: {}", keys.join(", "))
}

/// Resolves and canonicalises the workspace root.
pub fn resolve_workspace(flag: Option<&Path>) -> Result<PathBuf> {
    let requested = match flag {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("cannot determine the current directory")?,
    };
    let root = std::fs::canonicalize(&requested)
        .with_context(|| format!("cannot resolve workspace {}", requested.display()))?;
    if !root.is_dir() {
        bail!("workspace root is not a directory: {}", root.display());
    }
    Ok(root)
}

/// Builds a provider session, failing here rather than inside a stream when a
/// credential is missing.
pub fn build_session(
    settings: &RunSettings,
    system_prompt: String,
    tools: Vec<ToolDefinition>,
) -> Result<Box<dyn ModelSession>> {
    build_session_from(settings, system_prompt, tools, &|name| {
        std::env::var(name).ok()
    })
}

/// The credential lookup is a parameter so it can be tested without writing to
/// the process environment, which every other test would then have to share.
fn build_session_from(
    settings: &RunSettings,
    system_prompt: String,
    tools: Vec<ToolDefinition>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Box<dyn ModelSession>> {
    let profile = &settings.profile;
    match profile.provider {
        ProviderKind::Anthropic => {
            let key = require_env(env, "ANTHROPIC_API_KEY", profile)?;
            let mut config = AnthropicConfig::new(key, profile.clone());
            config.system_prompt = system_prompt;
            config.tools = tools;
            if let Some(limit) = profile.max_output_tokens {
                config.max_output_tokens = limit;
            }
            Ok(Box::new(AnthropicSession::new(config)?))
        }
        ProviderKind::OpenAi => {
            let key = require_env(env, "OPENAI_API_KEY", profile)?;
            let mut config = OpenAiConfig::new(key, profile.clone());
            config.instructions = system_prompt;
            config.tools = tools;
            config.max_output_tokens = profile.max_output_tokens;
            Ok(Box::new(OpenAiSession::new(config)?))
        }
        ProviderKind::Qwen => {
            let api_base = match &settings.api_base {
                Some(api_base) => api_base.clone(),
                None => require_env(env, "QWEN_API_BASE", profile)?,
            };
            let mut config = QwenConfig::new(api_base, profile.clone());
            if settings.disable_thinking {
                config.enable_thinking = Some(false);
            }
            config.api_key = env("QWEN_API_KEY").filter(|key| !key.is_empty());
            config.system_prompt = system_prompt;
            config.tools = tools;
            config.max_output_tokens = profile.max_output_tokens;
            // Deliberately not copying the profile's sampling here. The adapter
            // already falls back to it, and an explicit copy overrides the
            // adapter's own non-thinking sampling profile, which is a quieter
            // way of ignoring --no-thinking than one would like.
            Ok(Box::new(QwenSession::new(config)?))
        }
    }
}

fn require_env(
    env: &dyn Fn(&str) -> Option<String>,
    name: &str,
    profile: &ModelProfile,
) -> Result<String> {
    match env(name) {
        Some(value) if !value.is_empty() => Ok(value),
        _ => bail!(
            "model {} needs {name}, which is not set in the environment",
            profile.key
        ),
    }
}

/// Builds the containment for a run, refusing to pretend where there is none.
///
/// A platform without an implementation does not quietly fall through to
/// running unconfined. The person has to ask for that by name, and the log
/// records that they did.
pub fn build_sandbox(mode: SandboxMode, workspace: &Path) -> Result<Arc<dyn Sandbox>> {
    match mode {
        SandboxMode::None => Ok(Arc::new(Unconfined)),
        SandboxMode::Workspace => match gyr_sandbox::detect(workspace) {
            Ok(sandbox) => Ok(Arc::from(sandbox)),
            Err(error) => bail!(
                "{error}. Pass --sandbox none to run without containment, \
                 which is recorded in the session log."
            ),
        },
    }
}

/// The default session log path beneath the workspace.
pub fn default_log_path(workspace: &Path, session_id: &str) -> PathBuf {
    workspace
        .join(".gyr")
        .join("sessions")
        .join(format!("{session_id}.jsonl"))
}

#[cfg(test)]
mod tests {
    use gyr_model::builtin_profile;

    use super::*;

    fn empty_env(_name: &str) -> Option<String> {
        None
    }

    fn settings(profile: ModelProfile) -> RunSettings {
        RunSettings {
            profile,
            workspace: PathBuf::from("."),
            log_path: PathBuf::from("run.jsonl"),
            max_turns: NonZeroU32::new(4).unwrap(),
            mode: ApprovalMode::ReadOnly,
            sandbox: SandboxMode::Workspace,
            show_reasoning: false,
            api_base: None,
            disable_thinking: false,
            plain: true,
        }
    }

    #[test]
    fn a_missing_credential_fails_before_any_request() {
        for (key, expected) in [
            ("claude-opus", "ANTHROPIC_API_KEY"),
            ("terra", "OPENAI_API_KEY"),
            ("qwen3-coder-480b-a35b", "QWEN_API_BASE"),
        ] {
            let settings = settings(builtin_profile(key).expect("built-in profile"));

            let Err(error) = build_session_from(&settings, String::new(), Vec::new(), &empty_env)
            else {
                panic!("{key}: a session without a credential must not be built");
            };

            assert!(
                error.to_string().contains(expected),
                "{key} should name {expected}, said: {error}"
            );
        }
    }

    #[test]
    fn the_default_model_needs_an_endpoint() {
        let settings = settings(builtin_profile(DEFAULT_PROFILE).expect("default profile"));

        let Err(error) = build_session_from(&settings, String::new(), Vec::new(), &empty_env)
        else {
            panic!("the default has no endpoint of its own");
        };
        assert!(error.to_string().contains("QWEN_API_BASE"), "{error}");
    }

    #[test]
    fn an_endpoint_flag_stands_in_for_the_environment() {
        let mut settings = settings(builtin_profile("qwen3-8b").expect("built-in profile"));
        settings.api_base = Some("http://thinkpad.local:11434/v1".into());

        let session = build_session_from(&settings, String::new(), Vec::new(), &empty_env)
            .expect("an explicit endpoint needs no environment variable");

        assert_eq!(session.profile().provider_model, "qwen3:8b");
    }

    #[test]
    fn an_unknown_model_key_is_rejected_with_the_catalogue() {
        let error = resolve_profile(Some("gpt-9-imaginary")).expect_err("unknown key");

        let message = error.to_string();
        assert!(message.contains("gpt-9-imaginary"), "{message}");
        assert!(message.contains("known models:"), "{message}");
    }
}
