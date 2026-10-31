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
use dino_model::DEFAULT_PROFILE;
use dino_model::ModelSession;
use dino_model::anthropic::AnthropicConfig;
use dino_model::anthropic::AnthropicSession;
use dino_model::builtin_profile;
use dino_model::builtin_profiles;
use dino_model::fallback::FallbackSession;
use dino_model::openai::OpenAiConfig;
use dino_model::openai::OpenAiSession;
use dino_model::qwen::QwenConfig;
use dino_model::qwen::QwenSession;
use dino_protocol::ModelProfile;
use dino_protocol::ProviderKind;
use dino_protocol::ReasoningEffort;
use dino_protocol::ReasoningSupport;
use dino_protocol::ToolDefinition;
use dino_sandbox::Sandbox;
use dino_sandbox::Unconfined;

/// Reasoning tokens per Qwen turn when nothing else is set. Measured on the
/// hard bench (vLLM 0.32, Qwen3.8 27B): the same pass rate as no budget, and
/// pratt-expression-parser in 92 minutes over three trials instead of 157.
pub const DEFAULT_THINKING_BUDGET: u32 = 4_096;

/// Models that offer Anthropic's fast mode.
const FAST_MODELS: [&str; 2] = ["claude-opus-5", "claude-opus-5-5"];

/// How much the operating system confines a process Dino starts.
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

/// Whether a session compacts only when asked, or also on its own once the
/// context window passes the warning mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compaction {
    Manual,
    Automatic,
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
    /// Workspace-relative paths no tool or process may write.
    pub protected: Vec<PathBuf>,
    /// Cargo diagnostics shown to the model per run, when not the default.
    pub max_diagnostics: Option<usize>,
    /// After-edit hooks, as argument vectors.
    pub hooks: Vec<dino_exec::Hook>,
    pub end_turn_hooks: Vec<dino_exec::Hook>,
    pub start_hooks: Vec<dino_exec::Hook>,
    pub mcp: Vec<dino_exec::McpServer>,
    /// Most reasoning tokens per turn, for a server that honours a budget.
    pub thinking_budget: Option<u32>,
    pub compaction: Compaction,
    /// Overrides the endpoint for a self-served model.
    pub api_base: Option<String>,
    /// Asks a toggling model not to think. Absent leaves the server's default.
    pub disable_thinking: bool,
    /// Forces thinking on or off. `None` leaves [`RunSettings::disable_thinking`]
    /// in charge, which is every run except a probe comparing the two.
    pub enable_thinking: Option<bool>,
    /// Set on every model in the chain that offers it. `None` keeps each
    /// adapter's own default.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Emit no terminal colour. Resolved like everything else, so a config file
    /// can ask for it.
    pub plain: bool,
    /// Where to continue, in order, when the provider refuses the account.
    /// Always empty for evals and probes, whose results would otherwise mix
    /// models.
    pub fallback: Vec<ModelProfile>,
    /// Spending ceilings. Always `None` for evals, which are not metered.
    pub budget: Option<crate::spend::Budget>,
}

/// Resolves the model profile named by a flag or `DINO_MODEL`.
pub fn resolve_profile(flag: Option<&str>) -> Result<ModelProfile> {
    let key = match flag {
        Some(key) => key.to_owned(),
        None => std::env::var("DINO_MODEL").unwrap_or_else(|_| DEFAULT_PROFILE.to_owned()),
    };
    builtin_profile(&key).with_context(|| format!("unknown model {key:?} ({})", known_keys()))
}

/// The fallback profiles in order, without the model already in use or a
/// model named twice.
pub fn resolve_fallback(keys: &[String], profile: &ModelProfile) -> Result<Vec<ModelProfile>> {
    let mut chain: Vec<ModelProfile> = Vec::new();
    for key in keys {
        if key != &profile.key && chain.iter().all(|kept| &kept.key != key) {
            chain.push(resolve_profile(Some(key))?);
        }
    }
    Ok(chain)
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
    if settings.fallback.is_empty() {
        return build_provider(settings, &settings.profile, system_prompt, tools, env);
    }
    let primary = build_provider(
        settings,
        &settings.profile,
        system_prompt.clone(),
        tools.clone(),
        env,
    )?;
    let fallbacks = settings
        .fallback
        .iter()
        .map(|profile| build_provider(settings, profile, system_prompt.clone(), tools.clone(), env))
        .collect::<Result<Vec<_>>>()?;
    Ok(Box::new(FallbackSession::new(
        primary,
        fallbacks,
        Box::new(|message| eprintln!("dino: {message}")),
    )))
}

fn build_provider(
    settings: &RunSettings,
    profile: &ModelProfile,
    system_prompt: String,
    tools: Vec<ToolDefinition>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Box<dyn ModelSession>> {
    let is_fallback = settings.profile.key != profile.key;
    let effort = effort_for(settings, profile, is_fallback)?;
    match profile.provider {
        ProviderKind::Anthropic => {
            let key = require_env(env, "ANTHROPIC_API_KEY", profile, is_fallback)?;
            let mut config = AnthropicConfig::new(key, profile.clone());
            config.system_prompt = system_prompt;
            config.tools = tools;
            if let Some(limit) = profile.max_output_tokens {
                config.max_output_tokens = limit;
            }
            // A backstop is there so work does not stop, so it should not crawl.
            config.fast = is_fallback && FAST_MODELS.contains(&profile.provider_model.as_str());
            if let Some(effort) = effort {
                config.effort = effort;
            }
            Ok(Box::new(AnthropicSession::new(config)?))
        }
        ProviderKind::OpenAi => {
            let key = require_env(env, "OPENAI_API_KEY", profile, is_fallback)?;
            let mut config = OpenAiConfig::new(key, profile.clone());
            config.instructions = system_prompt;
            config.tools = tools;
            config.max_output_tokens = profile.max_output_tokens;
            if let Some(effort) = effort {
                config.reasoning_effort = effort;
            }
            Ok(Box::new(OpenAiSession::new(config)?))
        }
        ProviderKind::Qwen => {
            // A model with an endpoint of its own never takes the Qwen one,
            // so its key cannot be sent to another provider.
            let hosted = dino_model::hosted_endpoint(&profile.key);
            let api_base = match (hosted, &settings.api_base) {
                (Some(hosted), _) => hosted.api_base.to_owned(),
                (None, Some(api_base)) => api_base.clone(),
                (None, None) => require_env(env, "QWEN_API_BASE", profile, is_fallback)?,
            };
            let mut config = QwenConfig::new(api_base, profile.clone());
            config.enable_thinking = match (settings.enable_thinking, settings.disable_thinking) {
                (Some(enabled), _) => Some(enabled),
                (None, true) => Some(false),
                (None, false) => None,
            };
            config.reasoning_effort = effort;
            config.api_key = match hosted {
                Some(hosted) => Some(require_env(env, hosted.key_variable, profile, is_fallback)?),
                None => env("QWEN_API_KEY").filter(|key| !key.is_empty()),
            };
            config.thinking_budget = Some(thinking_budget(settings.thinking_budget));
            // Experimental until measured on the Token Plan: #135.
            config.explicit_cache = env("DINO_EXPLICIT_CACHE").is_some_and(|value| value == "1");
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

/// Refuses an effort the profile does not list, naming the ones it does.
pub fn check_effort(profile: &ModelProfile, effort: ReasoningEffort) -> Result<()> {
    match &profile.reasoning {
        ReasoningSupport::Effort(offered) if offered.contains(&effort) => Ok(()),
        ReasoningSupport::Effort(offered) => bail!(
            "{} does not offer {effort} reasoning effort; it offers {}",
            profile.key,
            offered
                .iter()
                .map(|effort| effort.name())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => bail!("{} has no reasoning effort to set", profile.key),
    }
}

/// The effort a model in the chain is built with: the primary must offer it,
/// a fallback that does not keeps its default.
fn effort_for(
    settings: &RunSettings,
    profile: &ModelProfile,
    is_fallback: bool,
) -> Result<Option<ReasoningEffort>> {
    let Some(effort) = settings.reasoning_effort else {
        return Ok(None);
    };
    match check_effort(profile, effort) {
        Ok(()) => Ok(Some(effort)),
        Err(_) if is_fallback => Ok(None),
        Err(error) => Err(error),
    }
}

fn require_env(
    env: &dyn Fn(&str) -> Option<String>,
    name: &str,
    profile: &ModelProfile,
    is_fallback: bool,
) -> Result<String> {
    match env(name) {
        Some(value) if !value.is_empty() => Ok(value),
        _ if is_fallback => bail!(
            "fallback model {key} needs {name}, which is not set in the environment; set \
             it, or take {key} out of fallback in your config",
            key = profile.key
        ),
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
pub fn build_sandbox(
    mode: SandboxMode,
    workspace: &Path,
    protected: &[PathBuf],
) -> Result<Arc<dyn Sandbox>> {
    match mode {
        SandboxMode::None => Ok(Arc::new(Unconfined)),
        SandboxMode::Workspace => match dino_sandbox::detect_protecting(workspace, protected) {
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
        .join(".dino")
        .join("sessions")
        .join(format!("{session_id}.jsonl"))
}

fn thinking_budget(setting: Option<u32>) -> u32 {
    setting.unwrap_or(DEFAULT_THINKING_BUDGET)
}

#[cfg(test)]
mod tests {
    use dino_model::builtin_profile;

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
            protected: Vec::new(),
            max_diagnostics: None,
            hooks: Vec::new(),
            end_turn_hooks: Vec::new(),
            start_hooks: Vec::new(),
            mcp: Vec::new(),
            thinking_budget: None,
            compaction: Compaction::Manual,
            show_reasoning: false,
            api_base: None,
            disable_thinking: false,
            enable_thinking: None,
            reasoning_effort: None,
            plain: true,
            fallback: Vec::new(),
            budget: None,
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
    fn the_default_model_needs_its_own_key() {
        let settings = settings(builtin_profile(DEFAULT_PROFILE).expect("default profile"));

        let Err(error) = build_session_from(&settings, String::new(), Vec::new(), &empty_env)
        else {
            panic!("the default needs a Moonshot key");
        };
        assert!(error.to_string().contains("MOONSHOT_API_KEY"), "{error}");
    }

    #[test]
    fn each_model_in_the_chain_takes_its_own_endpoint_and_key() {
        let mut settings = settings(builtin_profile("kimi-k3.1-pro").expect("built-in profile"));
        settings.api_base = Some("https://token-plan.example/v1".into());
        settings.fallback = ["qwen3.7-max", "claude-opus-5.5"]
            .map(|key| builtin_profile(key).expect("built-in profile"))
            .to_vec();
        let keys = |missing: &'static str| {
            move |name: &str| (name != missing).then(|| format!("{name}-value"))
        };

        let session = build_session_from(&settings, String::new(), Vec::new(), &keys("none"))
            .expect("every key is set");
        assert_eq!(session.profile().key, "kimi-k3.1-pro");

        let Err(error) = build_session_from(
            &settings,
            String::new(),
            Vec::new(),
            &keys("ANTHROPIC_API_KEY"),
        ) else {
            panic!("a fallback without its key must not be built");
        };
        let message = error.to_string();
        assert!(
            message.contains("fallback model claude-opus-5.5 needs ANTHROPIC_API_KEY"),
            "{message}"
        );
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
    fn the_fallback_chain_skips_the_model_in_use_and_repeats() {
        let kimi = builtin_profile("kimi-k3.1-pro").expect("built-in profile");
        let keys = [
            "kimi-k3.1-pro",
            "qwen3.7-max",
            "qwen3.7-max",
            "claude-opus-5.5",
        ]
        .map(String::from);

        let chain = resolve_fallback(&keys, &kimi).unwrap();

        let chain: Vec<&str> = chain.iter().map(|profile| profile.key.as_str()).collect();
        assert_eq!(chain, ["qwen3.7-max", "claude-opus-5.5"]);
        assert!(resolve_fallback(&["gpt-9-imaginary".into()], &kimi).is_err());
    }

    #[test]
    fn an_unknown_model_key_is_rejected_with_the_catalogue() {
        let error = resolve_profile(Some("gpt-9-imaginary")).expect_err("unknown key");

        let message = error.to_string();
        assert!(message.contains("gpt-9-imaginary"), "{message}");
        assert!(message.contains("known models:"), "{message}");
    }

    #[test]
    fn an_effort_reaches_every_provider_that_offers_it() {
        let set = |key: &str, effort: &str| {
            let mut settings = settings(builtin_profile(key).expect("built-in profile"));
            settings.reasoning_effort = Some(effort.parse().expect("a known effort"));
            build_session_from(&settings, String::new(), Vec::new(), &|name: &str| {
                Some(format!("{name}-value"))
            })
            .map(|_| ())
        };

        for (key, effort) in [
            ("kimi-k3.1-pro", "low"),
            ("kimi-k3.1-pro", "medium"),
            ("kimi-k3.1-pro", "none"),
            ("claude-sonnet-5.5", "low"),
            ("terra-6.1", "none"),
        ] {
            set(key, effort).unwrap_or_else(|error| panic!("{key} at {effort}: {error}"));
        }

        let refused = set("kimi-k3.1-pro", "max").unwrap_err().to_string();
        assert!(
            refused.contains("kimi-k3.1-pro does not offer max")
                && refused.contains("none, low, medium, high"),
            "{refused}"
        );
        let refused = set("claude-sonnet-5.5", "none").unwrap_err().to_string();
        assert!(refused.contains("does not offer none"), "{refused}");
        let refused = set("qwen3.7-max", "low").unwrap_err().to_string();
        assert!(refused.contains("no reasoning effort to set"), "{refused}");
    }

    #[test]
    fn a_fallback_without_the_effort_keeps_its_default() {
        let mut settings = settings(builtin_profile("kimi-k3.1-pro").expect("built-in profile"));
        settings.reasoning_effort = Some(ReasoningEffort::None);
        settings.fallback = vec![builtin_profile("claude-sonnet-5.5").expect("built-in profile")];

        build_session_from(&settings, String::new(), Vec::new(), &|name: &str| {
            Some(format!("{name}-value"))
        })
        .expect("Sonnet has no none effort, so it keeps its own");
    }

    #[test]
    fn qwen_reasoning_is_budgeted_unless_set_otherwise() {
        assert_eq!(thinking_budget(None), DEFAULT_THINKING_BUDGET);
        assert_eq!(thinking_budget(Some(16_384)), 16_384);
    }
}
