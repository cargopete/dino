//! Running a case through another agent's own harness, so its result is scored
//! by the same hidden tests and scorer as a dino run.
//!
//! The harness runs as installed and logged in, without the person's own
//! settings, on its own model or, given a [`Provider`], on that provider's, so
//! the harness is the only variable. Codex keeps its own sandbox; the others,
//! which approve their own calls unattended, run inside dino's: writes only to
//! the case, the harness's own state and the Cargo and temporary directories,
//! network only to HTTPS and DNS. What is comparable is the outcome and the
//! claims; turns and tool errors are read from each harness's event stream and
//! are close to, not identical with, dino's.

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use dino_protocol::ReasoningEffort;
use dino_sandbox::Sandbox;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::Case;
use crate::EarlyStop;
use crate::EvalError;
use crate::Metrics;
use crate::Outcome;
use crate::Variant;
use crate::case::fingerprint_tree;
use crate::runner::Finished;
use crate::runner::Tripwire;
use crate::runner::conclude;
use crate::runner::prompt_for;
use crate::runner::protected_failure;
use crate::runner::wall_outcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    ClaudeCode,
    Codex,
    Kimi,
    Grok,
    Pi,
}

/// An API serving one model in the wire formats of several harnesses, so they
/// can be compared on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    Moonshot,
    Anthropic,
}

impl Provider {
    const fn name(self) -> &'static str {
        match self {
            Self::Moonshot => "moonshot",
            Self::Anthropic => "anthropic",
        }
    }

    const fn openai_base(self) -> Option<&'static str> {
        match self {
            Self::Moonshot => Some("https://api.moonshot.ai/v1"),
            Self::Anthropic => None,
        }
    }

    const fn anthropic_base(self) -> &'static str {
        match self {
            Self::Moonshot => "https://api.moonshot.ai/anthropic",
            Self::Anthropic => "https://api.anthropic.com",
        }
    }

    const fn key_variable(self) -> &'static str {
        match self {
            Self::Moonshot => "MOONSHOT_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
        }
    }
}

/// A harness's command, what it needs in its environment, and the files and
/// directories it is given for its own state.
#[derive(Debug, Default)]
struct Invocation {
    program: String,
    arguments: Vec<String>,
    environment: Vec<(String, String)>,
    removed: Vec<&'static str>,
    files: Vec<(PathBuf, String)>,
    /// Logins copied in from the person's own state, as (from, to).
    logins: Vec<(PathBuf, PathBuf)>,
    writable: Vec<PathBuf>,
}

/// Login copies that are removed when the run ends, however it ends.
struct Logins(Vec<PathBuf>);

impl Drop for Logins {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Harness {
    /// Home-relative paths the harness writes besides the case: its own state
    /// and logins. `None` for Codex, whose own sandbox cannot run inside another.
    fn state(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Codex => None,
            Self::ClaudeCode => Some(&[
                ".claude",
                ".claude.json",
                "Library/Caches/claude-cli-nodejs",
            ]),
            Self::Kimi => Some(&[".kimi-code", ".kimi"]),
            Self::Grok => Some(&[".grok"]),
            // Pi's state is the run's own directory, in the invocation.
            Self::Pi => Some(&[]),
        }
    }

    /// The reasoning effort the harness is told to run at, recorded with the
    /// run. `None` is the harness's own default, which none of them reports.
    #[must_use]
    pub const fn effort(self, provider: Option<Provider>) -> Option<ReasoningEffort> {
        match (self, provider) {
            // Low, as dino runs Claude.
            (Self::Pi, Some(Provider::Anthropic)) => Some(ReasoningEffort::Low),
            // Pi's own default, as the runs before this was recorded had.
            (Self::Pi, _) => Some(ReasoningEffort::Medium),
            _ => None,
        }
    }

    /// The command wrapped in a sandbox for this workspace, where one applies.
    fn contained(
        self,
        invocation: Invocation,
        workspace: &Path,
        confine: bool,
    ) -> Result<Invocation, EvalError> {
        let Some(state) = self.state().filter(|_| confine) else {
            return Ok(invocation);
        };
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let mut writable: Vec<PathBuf> = state
            .iter()
            .chain(&[".cargo", ".rustup"])
            .map(|path| home.join(path))
            .collect();
        writable.extend(invocation.writable.iter().cloned());
        writable.push(std::env::temp_dir());
        // Claude Code's shell keeps its state in /tmp/claude-<uid>; without it
        // no command runs at all, and the model works blind.
        if self == Self::ClaudeCode {
            writable.push("/private/tmp".into());
        }
        writable.retain(|path| path.exists());
        let sandbox = dino_sandbox::detect(workspace)
            .map_err(|error| EvalError::Setup(error.to_string()))?
            .for_fetching(&writable)
            .ok_or_else(|| EvalError::Setup("this sandbox cannot allow HTTPS".into()))?;
        let wrapped = sandbox
            .wrap(&invocation.program, &invocation.arguments)
            .map_err(|error| EvalError::Setup(error.to_string()))?;
        Ok(Invocation {
            program: wrapped.program,
            arguments: wrapped.arguments,
            ..invocation
        })
    }

    /// The command for one run. `state` is a fresh directory of the run's own,
    /// for a harness whose usual one would carry the person's setup.
    fn command(
        self,
        model: &str,
        prompt: &str,
        workspace: &Path,
        provider: Option<(Provider, &str)>,
        state: &Path,
    ) -> Result<Invocation, EvalError> {
        let owned = |parts: &[&str]| parts.iter().map(|part| (*part).to_owned()).collect();
        let (program, arguments) = match self {
            Self::ClaudeCode => (
                "claude".to_owned(),
                owned(&[
                    "-p",
                    prompt,
                    "--model",
                    model,
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--dangerously-skip-permissions",
                    "--no-session-persistence",
                    // Stock Claude Code, as Codex's --ignore-user-config gives
                    // stock Codex: no personal CLAUDE.md, hooks, MCP or skills.
                    "--setting-sources",
                    "project",
                    "--strict-mcp-config",
                    "--disable-slash-commands",
                    "--no-chrome",
                ]),
            ),
            Self::Codex => (
                "codex".to_owned(),
                [
                    "exec",
                    "--json",
                    "-m",
                    model,
                    "--skip-git-repo-check",
                    "--ephemeral",
                    "--ignore-user-config",
                    "-s",
                    "workspace-write",
                    // Dependencies have to be fetchable, as dino's cargo fetch allows.
                    "-c",
                    "sandbox_workspace_write.network_access=true",
                ]
                .iter()
                .map(|part| (*part).to_owned())
                .chain(
                    provider
                        .map(|(provider, _)| codex_provider(provider))
                        .transpose()?
                        .unwrap_or_default(),
                )
                .chain([
                    "-C".to_owned(),
                    workspace.display().to_string(),
                    prompt.to_owned(),
                ])
                .collect(),
            ),
            // Kimi Code runs every call unasked in prompt mode, so dino contains it.
            Self::Kimi => (
                "kimi".to_owned(),
                owned(&["-p", prompt, "-m", model, "--output-format", "stream-json"]),
            ),
            // Shell is held to cargo: approving everything unattended was refused here.
            Self::Grok => (
                "grok".to_owned(),
                owned(&[
                    "-p",
                    prompt,
                    "-m",
                    model,
                    "--output-format",
                    "streaming-json",
                    "--permission-mode",
                    "acceptEdits",
                    "--allow",
                    "Bash(cargo:*)",
                ]),
            ),
            // Pi has no approvals at all, so dino contains it.
            Self::Pi => {
                let mut arguments: Vec<String> =
                    owned(&["-p", "--mode", "json", "--no-session", "--offline"]);
                if let Some((provider, _)) = provider {
                    arguments.extend(["--provider".to_owned(), provider.name().to_owned()]);
                }
                if let Some(effort) = self.effort(provider.map(|(provider, _)| provider)) {
                    arguments.extend(["--thinking".to_owned(), effort.name().to_owned()]);
                }
                if let Some((Provider::Anthropic, _)) = provider {
                    arguments.extend([
                        "-e".to_owned(),
                        state.join(PI_ADAPTIVE).display().to_string(),
                    ]);
                }
                arguments.extend(["--model".to_owned(), model.to_owned(), prompt.to_owned()]);
                ("pi".to_owned(), arguments)
            }
        };
        self.provisioned(
            Invocation {
                program,
                arguments,
                ..Invocation::default()
            },
            model,
            provider,
            state,
        )
    }

    /// `invocation` with the environment and state that keep the person's
    /// setup out and point the harness at `provider`.
    fn provisioned(
        self,
        mut invocation: Invocation,
        model: &str,
        provider: Option<(Provider, &str)>,
        state: &Path,
    ) -> Result<Invocation, EvalError> {
        match (self, provider) {
            (Self::Pi, _) => {
                // A directory of its own is stock Pi: no personal settings,
                // extensions, skills or AGENTS.md.
                invocation.environment.push((
                    "PI_CODING_AGENT_DIR".to_owned(),
                    state.display().to_string(),
                ));
                invocation.writable.push(state.to_path_buf());
                if let Some((provider, _)) = provider {
                    invocation
                        .files
                        .push((state.join("models.json"), pi_models(provider, model)));
                }
                if let (Some((Provider::Anthropic, _)), Some(effort)) = (
                    provider,
                    self.effort(provider.map(|(provider, _)| provider)),
                ) {
                    invocation
                        .files
                        .push((state.join(PI_ADAPTIVE), pi_adaptive(effort)));
                }
            }
            (Self::Codex, _) => {
                // --ignore-user-config still loads ~/.codex's AGENTS.md, skills
                // and memories. A provider keyed from the environment needs no login.
                invocation
                    .environment
                    .push(("CODEX_HOME".to_owned(), state.display().to_string()));
                if provider.is_none() {
                    invocation
                        .logins
                        .push((codex_home().join("auth.json"), state.join("auth.json")));
                }
            }
            (Self::ClaudeCode, Some((provider, key))) => {
                invocation.environment = claude_code_environment(provider, model, key);
                invocation.removed.push("ANTHROPIC_AUTH_TOKEN");
            }
            (Self::Grok, None) => {
                // Grok reads Claude Code's and Cursor's personal instructions,
                // skills, MCP servers and hooks as its own unless told not to.
                invocation.environment = ["CLAUDE", "CURSOR"]
                    .iter()
                    .flat_map(|vendor| {
                        ["RULES", "SKILLS", "AGENTS", "MCPS", "HOOKS"].map(|part| {
                            (format!("GROK_{vendor}_{part}_ENABLED"), "false".to_owned())
                        })
                    })
                    .chain([("GROK_MEMORY".to_owned(), "0".to_owned())])
                    .collect();
            }
            (Self::Kimi | Self::Grok, Some((provider, _))) => {
                return Err(EvalError::Setup(format!(
                    "{self:?} cannot be pointed at {}",
                    provider.name()
                )));
            }
            (_, None) => {}
        }
        Ok(invocation)
    }
}

/// The person's own Codex state, where its login lives.
fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME").map_or_else(
        || {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".codex")
        },
        PathBuf::from,
    )
}

/// Codex's overrides adding `provider` and selecting it. Codex reads the key
/// from its variable itself.
fn codex_provider(provider: Provider) -> Result<Vec<String>, EvalError> {
    let name = provider.name();
    let base = provider
        .openai_base()
        .ok_or_else(|| EvalError::Setup(format!("Codex cannot be pointed at {name}")))?;
    Ok([
        format!("model_provider=\"{name}\""),
        format!("model_providers.{name}.name=\"{name}\""),
        format!("model_providers.{name}.base_url=\"{base}\""),
        format!(
            "model_providers.{name}.env_key=\"{}\"",
            provider.key_variable()
        ),
        // The only wire format Codex still speaks.
        format!("model_providers.{name}.wire_api=\"responses\""),
    ]
    .into_iter()
    .flat_map(|setting| ["-c".to_owned(), setting])
    .collect())
}

/// Claude Code's environment for `provider`. Every model alias is the one
/// model, or background calls would ask the provider for Haiku.
fn claude_code_environment(provider: Provider, model: &str, key: &str) -> Vec<(String, String)> {
    let mut environment = vec![
        (
            "ANTHROPIC_BASE_URL".to_owned(),
            provider.anthropic_base().to_owned(),
        ),
        // In place of the person's own key, which would otherwise go to the provider.
        ("ANTHROPIC_API_KEY".to_owned(), key.to_owned()),
        (
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_owned(),
            "1".to_owned(),
        ),
    ];
    environment.extend(
        [
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "CLAUDE_CODE_SUBAGENT_MODEL",
        ]
        .map(|variable| (variable.to_owned(), model.to_owned())),
    );
    environment
}

/// Pi's `models.json` adding `model` to `provider`. Pi takes the provider's
/// quirks from its address; the window and output limit come from dino's
/// profile, where it has one.
fn pi_models(provider: Provider, model: &str) -> String {
    let profile = dino_model::builtin_profile(model).or_else(|| {
        dino_model::builtin_profiles()
            .into_iter()
            .find(|profile| profile.provider_model == model)
    });
    let mut entry = serde_json::json!({ "id": model, "reasoning": true });
    if let Some(window) = profile
        .as_ref()
        .and_then(|profile| profile.context_window_tokens)
    {
        entry["contextWindow"] = window.into();
    }
    if let Some(output) = profile
        .as_ref()
        .and_then(|profile| profile.max_output_tokens)
    {
        entry["maxTokens"] = output.into();
    }
    let (base, api) = provider
        .openai_base()
        .map_or((provider.anthropic_base(), "anthropic-messages"), |base| {
            (base, "openai-completions")
        });
    serde_json::json!({
        "providers": {
            provider.name(): {
                "baseUrl": base,
                "api": api,
                "apiKey": provider.key_variable(),
                "models": [entry],
            }
        }
    })
    .to_string()
}

/// The Pi extension that turns its thinking requests to Anthropic adaptive.
const PI_ADAPTIVE: &str = "adaptive-thinking.ts";

/// Pi 0.73.1 predates Claude 5 and asks it for budgeted thinking, which it
/// refuses; this sends adaptive thinking at `effort` in its place.
fn pi_adaptive(effort: ReasoningEffort) -> String {
    format!(
        r#"export default function (pi) {{
  pi.on("before_provider_request", (event) => {{
    const payload = event.payload;
    if (payload?.thinking?.type !== "enabled") return undefined;
    const {{ budget_tokens, ...thinking }} = payload.thinking;
    return {{
      ...payload,
      thinking: {{ ...thinking, type: "adaptive" }},
      output_config: {{ ...payload.output_config, effort: "{effort}" }},
    }};
  }});
}}
"#
    )
}

/// What a harness's event stream says happened.
#[derive(Debug, Default, PartialEq)]
pub struct Parsed {
    pub answer: String,
    pub metrics: Metrics,
    pub completed: bool,
}

/// Reads Claude Code's `stream-json` output. A call counts as an error only
/// when it could not run; a command that ran and exited non-zero is a result,
/// as it is in dino.
#[must_use]
pub fn parse_claude_code(stream: &str) -> Parsed {
    let mut parsed = Parsed::default();
    for line in stream.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("assistant") => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_use" {
                        let name = block["name"].as_str().unwrap_or("?").to_owned();
                        *parsed.metrics.tool_calls.entry(name).or_insert(0) += 1;
                    }
                }
            }
            Some("user") => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    let content = block["content"].to_string();
                    if block["is_error"] == true && !content.contains("Exit code") {
                        parsed.metrics.tool_errors += 1;
                    }
                }
            }
            Some("result") => {
                parsed.completed = event["terminal_reason"] == "completed";
                event["result"]
                    .as_str()
                    .unwrap_or_default()
                    .clone_into(&mut parsed.answer);
                parsed.metrics.model_turns =
                    u32::try_from(event["num_turns"].as_u64().unwrap_or(0)).unwrap_or(u32::MAX);
                let usage = &event["usage"];
                let count = |field: &str| usage[field].as_u64().unwrap_or(0);
                parsed.metrics.tokens.cached_input_tokens = count("cache_read_input_tokens");
                parsed.metrics.tokens.cache_write_input_tokens =
                    count("cache_creation_input_tokens");
                parsed.metrics.tokens.input_tokens = count("input_tokens")
                    + count("cache_creation_input_tokens")
                    + count("cache_read_input_tokens");
                parsed.metrics.tokens.output_tokens = count("output_tokens");
            }
            _ => {}
        }
    }
    parsed
}

/// Reads `codex exec --json` output. Codex reports no model-turn count, so each
/// tool step counts as one, plus one for the closing answer.
#[must_use]
pub fn parse_codex(stream: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let mut steps = 0_u32;
    for line in stream.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("item.completed") => {
                let item = &event["item"];
                match item["type"].as_str() {
                    Some("agent_message") => {
                        item["text"]
                            .as_str()
                            .unwrap_or_default()
                            .clone_into(&mut parsed.answer);
                    }
                    // An `error` item is Codex's own warning, such as a model it
                    // has no metadata for, not a step.
                    Some("reasoning" | "error") | None => {}
                    Some(kind) => {
                        steps += 1;
                        *parsed
                            .metrics
                            .tool_calls
                            .entry(kind.to_owned())
                            .or_insert(0) += 1;
                        let unrunnable = matches!(item["exit_code"].as_i64(), Some(126 | 127))
                            || item["status"] == "declined";
                        if unrunnable {
                            parsed.metrics.tool_errors += 1;
                        }
                    }
                }
            }
            Some("turn.completed") => {
                parsed.completed = true;
                let usage = &event["usage"];
                let count = |field: &str| usage[field].as_u64().unwrap_or(0);
                parsed.metrics.tokens.input_tokens += count("input_tokens");
                parsed.metrics.tokens.cached_input_tokens += count("cached_input_tokens");
                parsed.metrics.tokens.cache_write_input_tokens += count("cache_write_input_tokens");
                parsed.metrics.tokens.output_tokens += count("output_tokens");
                parsed.metrics.tokens.reasoning_tokens += count("reasoning_output_tokens");
            }
            _ => {}
        }
    }
    parsed.metrics.model_turns = steps + 1;
    parsed
}

/// Reads Kimi Code's `stream-json` output: one chat message per line. It
/// reports no usage, so tokens stay at zero.
#[must_use]
pub fn parse_kimi(stream: &str) -> Parsed {
    let mut parsed = Parsed::default();
    for line in stream.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if event["role"] != "assistant" {
            continue;
        }
        parsed.metrics.model_turns += 1;
        let calls = event["tool_calls"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        for call in calls {
            let name = call["function"]["name"].as_str().unwrap_or("?").to_owned();
            *parsed.metrics.tool_calls.entry(name).or_insert(0) += 1;
        }
        if let Some(text) = event["content"].as_str().filter(|text| !text.is_empty()) {
            text.clone_into(&mut parsed.answer);
        }
        parsed.completed = calls.is_empty() && !parsed.answer.is_empty();
    }
    parsed
}

/// Reads Grok's `streaming-json` output. The answer is the text streamed after
/// the last tool call, and each `usage` event is one model turn.
#[must_use]
pub fn parse_grok(stream: &str) -> Parsed {
    let mut parsed = Parsed::default();
    for line in stream.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("text") => parsed
                .answer
                .push_str(event["data"].as_str().unwrap_or_default()),
            Some("tool_call") => {
                parsed.answer.clear();
                let name = event["toolName"].as_str().unwrap_or("?").to_owned();
                *parsed.metrics.tool_calls.entry(name).or_insert(0) += 1;
            }
            Some("tool_call_update") if event["status"] == "failed" => {
                parsed.metrics.tool_errors += 1;
            }
            Some("usage") => parsed.metrics.model_turns += 1,
            Some("end") => {
                parsed.completed = event["stopReason"] == "end_turn";
                let usage = &event["usage"];
                let count = |field: &str| usage[field].as_u64().unwrap_or(0);
                parsed.metrics.tokens.cached_input_tokens = count("cache_read_input_tokens");
                parsed.metrics.tokens.cache_write_input_tokens =
                    count("cache_creation_input_tokens");
                parsed.metrics.tokens.input_tokens = count("input_tokens")
                    + count("cache_creation_input_tokens")
                    + count("cache_read_input_tokens");
                parsed.metrics.tokens.output_tokens = count("output_tokens");
                parsed.metrics.tokens.reasoning_tokens = count("reasoning_tokens");
            }
            _ => {}
        }
    }
    parsed
}

/// Reads `pi --mode json` output. Each assistant message is a turn, and the
/// answer is the text of the last. Pi's bash tool fails a command that ran and
/// exited non-zero; that is a result, as it is in dino, unless the shell could
/// not run it.
#[must_use]
pub fn parse_pi(stream: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let mut stopped = false;
    for line in stream.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("message_end") if event["message"]["role"] == "assistant" => {
                let message = &event["message"];
                parsed.metrics.model_turns += 1;
                parsed.answer = message["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|block| block["type"] == "text")
                    .filter_map(|block| block["text"].as_str())
                    .collect();
                stopped = message["stopReason"] == "stop";
                let usage = &message["usage"];
                let count = |field: &str| usage[field].as_u64().unwrap_or(0);
                let tokens = &mut parsed.metrics.tokens;
                tokens.input_tokens += count("input") + count("cacheRead") + count("cacheWrite");
                tokens.cached_input_tokens += count("cacheRead");
                tokens.cache_write_input_tokens += count("cacheWrite");
                tokens.output_tokens += count("output");
            }
            Some("tool_execution_start") => {
                let name = event["toolName"].as_str().unwrap_or("?").to_owned();
                *parsed.metrics.tool_calls.entry(name).or_insert(0) += 1;
            }
            Some("tool_execution_end") if event["isError"] == true => {
                let text = event["result"]["content"].to_string();
                let exit = text
                    .rsplit_once("Command exited with code ")
                    .and_then(|(_, rest)| {
                        rest.split(|c: char| !c.is_ascii_digit())
                            .next()?
                            .parse::<u32>()
                            .ok()
                    });
                if !matches!(exit, Some(code) if code != 126 && code != 127) {
                    parsed.metrics.tool_errors += 1;
                }
            }
            Some("agent_end") => parsed.completed = stopped,
            _ => {}
        }
    }
    parsed
}

/// The harness's command for `case`, with a fresh state directory beside the
/// workspace and the files it is given there written, and the logins copied
/// there, which are removed when the returned [`Logins`] is dropped.
fn prepare(
    case: &Case,
    scratch: &Path,
    workspace: &Path,
    harness: Harness,
    model: &str,
    provider: Option<Provider>,
) -> Result<(Invocation, Logins), EvalError> {
    let key = provider
        .map(|provider| {
            std::env::var(provider.key_variable())
                .map(|key| (provider, key))
                .map_err(|_| EvalError::Setup(format!("{} is not set", provider.key_variable())))
        })
        .transpose()?;
    let state = scratch.join(format!("{}.{harness:?}-state", case.name));
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state)
        .map_err(|error| EvalError::Setup(format!("cannot create {}: {error}", state.display())))?;
    let invocation = harness.command(
        model,
        &prompt_for(case, true),
        workspace,
        key.as_ref()
            .map(|(provider, key)| (*provider, key.as_str())),
        &state,
    )?;
    for (path, contents) in &invocation.files {
        std::fs::write(path, contents).map_err(|error| {
            EvalError::Setup(format!("cannot write {}: {error}", path.display()))
        })?;
    }
    let logins = copy_logins(harness, &invocation.logins)?;
    Ok((invocation, logins))
}

fn copy_logins(harness: Harness, logins: &[(PathBuf, PathBuf)]) -> Result<Logins, EvalError> {
    let mut copied = Logins(Vec::new());
    for (from, to) in logins {
        std::fs::copy(from, to).map_err(|error| {
            EvalError::Setup(format!(
                "{harness:?} needs its login, {}: {error}; log in first",
                from.display()
            ))
        })?;
        copied.0.push(to.clone());
    }
    Ok(copied)
}

/// How often a watched harness's workspace is compared with how it began.
const TRIPWIRE_POLL: Duration = Duration::from_secs(2);

enum Ended {
    Exited,
    Wall,
    Tripped,
}

/// Runs one case through `harness` with `model` and scores the result,
/// stopping the harness once it changes a protected path when `early_stop`.
///
/// # Errors
///
/// Returns [`EvalError`] when the case cannot be set up or the harness cannot
/// be started. A harness that runs and fails is a result, not an error.
pub async fn run_external(
    case: &Case,
    scratch: &Path,
    sandbox: Arc<dyn Sandbox>,
    harness: Harness,
    model: &str,
    provider: Option<Provider>,
    early_stop: bool,
) -> Result<Outcome, EvalError> {
    let workspace = case.materialise(scratch)?;
    let before = fingerprint_tree(&workspace)?;
    let tripwire = early_stop
        .then(|| Tripwire::new(case, &workspace, &before, CancellationToken::new()))
        .flatten();
    let (invocation, logins) = prepare(case, scratch, &workspace, harness, model, provider)?;
    let invocation = harness.contained(invocation, &workspace, sandbox.confines_writes())?;
    // Model names such as `kimi-code/k3.1-pro` would otherwise name a directory.
    let log_path = scratch.join(format!("{}.{}.jsonl", case.name, model.replace('/', "_")));

    let started = Instant::now();
    let mut command = tokio::process::Command::new(&invocation.program);
    command
        .args(&invocation.arguments)
        .envs(invocation.environment.iter().cloned());
    for variable in &invocation.removed {
        command.env_remove(variable);
    }
    let child = command
        .current_dir(&workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            EvalError::Setup(format!("cannot start {}: {error}", invocation.program))
        })?;
    // The case's own clock. A turn budget is not enforceable on another
    // harness, and it would not stop one turn that never returns.
    let limit = case.wall_limit()?;
    let (ended, stream) = await_harness(child, limit, tripwire.as_ref()).await?;
    let duration_ms = started.elapsed().as_millis();
    drop(logins);
    let _ = std::fs::write(&log_path, &stream);
    let stream = String::from_utf8_lossy(&stream);

    let parsed = match harness {
        Harness::ClaudeCode => parse_claude_code(&stream),
        Harness::Codex => parse_codex(&stream),
        Harness::Kimi => parse_kimi(&stream),
        Harness::Grok => parse_grok(&stream),
        Harness::Pi => parse_pi(&stream),
    };
    let variant = Variant {
        score: true,
        reasoning_effort: harness.effort(provider),
        early_stop,
        ..Variant::default()
    };
    if matches!(ended, Ended::Wall) {
        return wall_outcome(
            case,
            &variant,
            Finished {
                workspace,
                before,
                answer: parsed.answer,
                failures: Vec::new(),
                metrics: parsed.metrics,
                duration_ms,
                log_path,
            },
            limit,
        );
    }
    let mut failures = Vec::new();
    match tripwire.as_ref().and_then(Tripwire::tripped) {
        Some(path) if matches!(ended, Ended::Tripped) => failures.push(protected_failure(path)),
        _ if !parsed.completed => failures.push("the harness did not complete the run".to_owned()),
        _ => {}
    }
    let mut outcome = conclude(
        case,
        scratch,
        sandbox,
        &variant,
        Finished {
            workspace,
            before,
            answer: parsed.answer,
            failures,
            metrics: parsed.metrics,
            duration_ms,
            log_path,
        },
    )
    .await?;
    if matches!(ended, Ended::Tripped) {
        outcome.stopped = Some(EarlyStop::ProtectedPath);
    }
    Ok(outcome)
}

/// The harness's event stream, once it exits or is stopped.
async fn await_harness(
    mut child: tokio::process::Child,
    limit: Duration,
    tripwire: Option<&Tripwire>,
) -> Result<(Ended, Vec<u8>), EvalError> {
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| EvalError::Setup("the harness had no stdout".into()))?;
    // Bytes, not a String: a dropped `read_to_string` keeps nothing it read.
    let mut stream = Vec::new();
    let ended = tokio::select! {
        finished = tokio::time::timeout(limit, async {
            let _ = stdout.read_to_end(&mut stream).await;
            let _ = child.wait().await;
        }) => if finished.is_ok() { Ended::Exited } else { Ended::Wall },
        () = tripped(tripwire) => Ended::Tripped,
    };
    if !matches!(ended, Ended::Exited) {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    Ok((ended, stream))
}

/// Resolves once the harness has changed a protected path; never without a
/// tripwire.
async fn tripped(tripwire: Option<&Tripwire>) {
    let Some(tripwire) = tripwire else {
        return std::future::pending().await;
    };
    while tripwire.check().is_none() {
        tokio::time::sleep(TRIPWIRE_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    fn invocation(program: &str, arguments: &[&str]) -> Invocation {
        Invocation {
            program: program.to_owned(),
            arguments: arguments.iter().map(|part| (*part).to_owned()).collect(),
            ..Invocation::default()
        }
    }

    #[tokio::test]
    async fn a_watched_harness_trips_on_its_case_tests_and_nothing_else() {
        let case = Case::load(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/contradictory-tests"),
        )
        .unwrap();
        let scratch = std::env::temp_dir().join(format!("dino-tripwire-{}", std::process::id()));
        let workspace = case.materialise(&scratch).unwrap();
        let before = fingerprint_tree(&workspace).unwrap();
        let cancel = CancellationToken::new();
        let wire = Tripwire::new(&case, &workspace, &before, cancel.clone()).unwrap();

        std::fs::write(workspace.join("src/lib.rs"), "pub fn round_half() {}").unwrap();
        let unprotected =
            tokio::time::timeout(Duration::from_millis(100), tripped(Some(&wire))).await;
        let rounding = workspace.join("tests/rounding.rs");
        let original = std::fs::read(&rounding).unwrap();
        std::fs::write(&rounding, "").unwrap();
        let protected = tokio::time::timeout(Duration::from_secs(1), tripped(Some(&wire))).await;
        std::fs::write(&rounding, original).unwrap();
        let after_restore = wire.check().map(str::to_owned);
        let unwatched = tokio::time::timeout(Duration::from_millis(50), tripped(None)).await;
        let _ = std::fs::remove_dir_all(&scratch);

        assert!(unprotected.is_err(), "tripped on src/lib.rs");
        assert!(protected.is_ok(), "did not trip on the case's tests");
        assert!(cancel.is_cancelled());
        assert_eq!(after_restore.as_deref(), Some("tests/rounding.rs"));
        assert!(unwatched.is_err());
    }

    fn setting<'a>(arguments: &'a [String], prefix: &str) -> Option<&'a str> {
        arguments
            .windows(2)
            .find(|pair| pair[0] == "-c" && pair[1].starts_with(prefix))
            .map(|pair| pair[1].as_str())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn harnesses_that_approve_their_own_calls_run_contained() {
        let workspace = std::env::temp_dir();
        for harness in [
            Harness::Kimi,
            Harness::ClaudeCode,
            Harness::Grok,
            Harness::Pi,
        ] {
            let contained = harness
                .contained(invocation("tool", &["-p"]), &workspace, true)
                .unwrap();
            assert_eq!(contained.program, "/usr/bin/sandbox-exec", "{harness:?}");
            assert!(
                contained.arguments[1].contains("remote tcp \"*:443\""),
                "{harness:?}"
            );
            assert_eq!(contained.arguments[2], "tool");
        }
        let contained = Harness::Codex
            .contained(invocation("codex", &[]), &workspace, true)
            .unwrap();
        assert_eq!(contained.program, "codex", "Codex keeps its own sandbox");
        let contained = Harness::Kimi
            .contained(invocation("kimi", &[]), &workspace, false)
            .unwrap();
        assert_eq!(contained.program, "kimi", "--sandbox none is respected");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pi_may_write_its_own_state_directory() {
        // Any directory that exists outside the temporary one will do.
        let state = Path::new(env!("CARGO_MANIFEST_DIR"));
        let pi = Harness::Pi
            .command(
                "kimi-k3.1-pro",
                "fix it",
                &std::env::temp_dir(),
                None,
                state,
            )
            .unwrap();
        assert!(pi.environment.contains(&(
            "PI_CODING_AGENT_DIR".to_owned(),
            state.display().to_string()
        )));
        let contained = Harness::Pi
            .contained(pi, &std::env::temp_dir(), true)
            .unwrap();
        assert!(
            contained.arguments[1].contains(&state.display().to_string()),
            "{}",
            contained.arguments[1]
        );
    }

    #[test]
    fn codex_is_pointed_at_a_provider_without_the_persons_codex_home() {
        let state = Path::new("/scratch/case.Codex-state");
        let codex = Harness::Codex
            .command(
                "kimi-k3.1-pro",
                "fix it",
                Path::new("/scratch/case"),
                Some((Provider::Moonshot, "secret")),
                state,
            )
            .unwrap();

        assert_eq!(
            setting(&codex.arguments, "model_provider="),
            Some("model_provider=\"moonshot\"")
        );
        assert_eq!(
            setting(&codex.arguments, "model_providers.moonshot.base_url"),
            Some("model_providers.moonshot.base_url=\"https://api.moonshot.ai/v1\"")
        );
        assert_eq!(
            setting(&codex.arguments, "model_providers.moonshot.env_key"),
            Some("model_providers.moonshot.env_key=\"MOONSHOT_API_KEY\"")
        );
        assert!(
            !codex.arguments.iter().any(|part| part.contains("secret")),
            "Codex reads the key itself"
        );
        assert_eq!(
            codex.arguments[codex.arguments.len() - 3..],
            ["-C", "/scratch/case", "fix it"]
        );
        assert!(
            codex
                .environment
                .contains(&("CODEX_HOME".to_owned(), state.display().to_string()))
        );
        assert!(codex.logins.is_empty(), "the provider's key is the login");
    }

    #[test]
    fn native_codex_gets_a_fresh_home_with_only_the_persons_login() {
        let state = Path::new("/scratch/case.Codex-state");
        let native = Harness::Codex
            .command("gpt-6.1", "fix it", Path::new("/scratch/case"), None, state)
            .unwrap();
        assert!(setting(&native.arguments, "model_provider").is_none());
        assert_eq!(
            native.environment,
            [("CODEX_HOME".to_owned(), state.display().to_string())]
        );
        assert_eq!(
            native.logins,
            [(codex_home().join("auth.json"), state.join("auth.json"))]
        );
    }

    #[test]
    fn logins_are_copied_into_the_state_and_removed_with_the_run() {
        let base = std::env::temp_dir().join(format!("dino-logins-{}", std::process::id()));
        let (home, state) = (base.join("home"), base.join("state"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let login = home.join("auth.json");
        let copy = state.join("auth.json");
        std::fs::write(&login, "{}").unwrap();

        let logins = copy_logins(Harness::Codex, &[(login.clone(), copy.clone())]).unwrap();
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "{}");
        drop(logins);
        assert!(!copy.exists());
        assert!(login.exists(), "the person's own login is left alone");

        std::fs::remove_file(&login).unwrap();
        let Err(EvalError::Setup(message)) = copy_logins(Harness::Codex, &[(login, copy)]) else {
            panic!("a missing login is a setup error");
        };
        assert!(
            message.contains("auth.json") && message.contains("log in"),
            "{message}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn grok_is_kept_off_the_persons_claude_and_cursor_setup_and_memory() {
        let grok = Harness::Grok
            .command(
                "grok-4",
                "fix it",
                Path::new("/scratch/case"),
                None,
                Path::new("/scratch/state"),
            )
            .unwrap();
        for variable in [
            "GROK_CLAUDE_RULES_ENABLED",
            "GROK_CLAUDE_SKILLS_ENABLED",
            "GROK_CLAUDE_MCPS_ENABLED",
            "GROK_CURSOR_RULES_ENABLED",
        ] {
            assert!(
                grok.environment
                    .contains(&(variable.to_owned(), "false".to_owned())),
                "{variable}"
            );
        }
        assert!(
            grok.environment
                .contains(&("GROK_MEMORY".to_owned(), "0".to_owned()))
        );
    }

    #[test]
    fn claude_code_on_a_provider_sends_it_the_providers_key_for_every_model() {
        let claude = Harness::ClaudeCode
            .command(
                "kimi-k3.1-pro",
                "fix it",
                Path::new("/scratch/case"),
                Some((Provider::Moonshot, "secret")),
                Path::new("/scratch/state"),
            )
            .unwrap();
        let variable = |name: &str| {
            claude
                .environment
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };

        assert_eq!(
            variable("ANTHROPIC_BASE_URL"),
            Some("https://api.moonshot.ai/anthropic")
        );
        assert_eq!(variable("ANTHROPIC_API_KEY"), Some("secret"));
        assert_eq!(
            variable("ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            Some("kimi-k3.1-pro")
        );
        assert_eq!(claude.removed, ["ANTHROPIC_AUTH_TOKEN"]);
    }

    #[test]
    fn pi_on_a_provider_is_given_the_model_in_its_own_directory() {
        let state = Path::new("/scratch/case.Pi-state");
        let pi = Harness::Pi
            .command(
                "kimi-k3.1-pro",
                "fix it",
                Path::new("/scratch/case"),
                Some((Provider::Moonshot, "secret")),
                state,
            )
            .unwrap();

        assert_eq!(
            pi.arguments,
            [
                "-p",
                "--mode",
                "json",
                "--no-session",
                "--offline",
                "--provider",
                "moonshot",
                "--thinking",
                "medium",
                "--model",
                "kimi-k3.1-pro",
                "fix it"
            ]
        );
        assert_eq!(pi.files.len(), 1, "no thinking shim off Anthropic");
        assert_eq!(pi.writable, [state]);
        let (path, models) = &pi.files[0];
        assert_eq!(path, &state.join("models.json"));
        let models: Value = serde_json::from_str(models).unwrap();
        let moonshot = &models["providers"]["moonshot"];
        assert_eq!(moonshot["baseUrl"], "https://api.moonshot.ai/v1");
        assert_eq!(
            moonshot["apiKey"], "MOONSHOT_API_KEY",
            "a variable, not the key"
        );
        assert_eq!(moonshot["models"][0]["id"], "kimi-k3.1-pro");
        assert_eq!(moonshot["models"][0]["contextWindow"], 1_048_576);
    }

    #[test]
    fn pi_on_anthropic_thinks_adaptively_at_low_effort() {
        let state = Path::new("/scratch/case.Pi-state");
        let pi = Harness::Pi
            .command(
                "claude-haiku-5-5",
                "fix it",
                Path::new("/scratch/case"),
                Some((Provider::Anthropic, "secret")),
                state,
            )
            .unwrap();

        let shim = state.join(PI_ADAPTIVE).display().to_string();
        assert_eq!(
            pi.arguments,
            [
                "-p",
                "--mode",
                "json",
                "--no-session",
                "--offline",
                "--provider",
                "anthropic",
                "--thinking",
                "low",
                "-e",
                shim.as_str(),
                "--model",
                "claude-haiku-5-5",
                "fix it"
            ]
        );
        assert_eq!(
            Harness::Pi.effort(Some(Provider::Anthropic)),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(pi.writable, [state]);
        let models = pi
            .files
            .iter()
            .find(|(path, _)| path == &state.join("models.json"))
            .map(|(_, models)| serde_json::from_str::<Value>(models).unwrap())
            .unwrap();
        let anthropic = &models["providers"]["anthropic"];
        assert_eq!(anthropic["baseUrl"], "https://api.anthropic.com");
        assert_eq!(anthropic["api"], "anthropic-messages");
        assert_eq!(anthropic["apiKey"], "ANTHROPIC_API_KEY");
        assert_eq!(anthropic["models"][0]["id"], "claude-haiku-5-5");
        assert_eq!(anthropic["models"][0]["reasoning"], true);
        assert_eq!(
            anthropic["models"][0]["contextWindow"], 1_000_000,
            "found by the provider's model name"
        );
        let (_, extension) = pi
            .files
            .iter()
            .find(|(path, _)| path == &state.join(PI_ADAPTIVE))
            .unwrap();
        assert!(extension.contains(r#"type: "adaptive""#), "{extension}");
        assert!(extension.contains(r#"effort: "low""#), "{extension}");
    }

    #[test]
    fn codex_cannot_be_pointed_at_anthropic() {
        let Err(EvalError::Setup(message)) = Harness::Codex.command(
            "claude-haiku-5-5",
            "fix it",
            Path::new("/scratch/case"),
            Some((Provider::Anthropic, "secret")),
            Path::new("/scratch/state"),
        ) else {
            panic!("Codex speaks no Anthropic API");
        };
        assert!(message.contains("anthropic"), "{message}");
    }

    #[test]
    fn harnesses_with_their_own_provider_refuse_another() {
        for harness in [Harness::Kimi, Harness::Grok] {
            assert!(
                harness
                    .command(
                        "k",
                        "fix it",
                        Path::new("/scratch/case"),
                        Some((Provider::Moonshot, "secret")),
                        Path::new("/scratch/state"),
                    )
                    .is_err(),
                "{harness:?}"
            );
        }
    }

    /// Recorded from Pi 0.73.1 on Kimi K3.1 Pro, trimmed to the events read,
    /// with two bash calls added: one exiting 1, one not found.
    #[test]
    fn pi_streams_give_turns_tokens_errors_and_the_answer() {
        let stream = r#"{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"Use the read tool on notes.txt, then try to read missing.txt, then reply with the number of lines in notes.txt."}]}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"I need use read notes then try missing then reply lines. Should obey sequencing. I should read notes.","thinkingSignature":"reasoning_content"},{"type":"toolCall","id":"read_0_644eb4af","name":"read","arguments":{"path":"notes.txt"}}],"provider":"moonshotai","model":"kimi-k3.1-pro","usage":{"input":160,"output":73,"cacheRead":0,"cacheWrite":1280},"stopReason":"toolUse"}}
{"type":"tool_execution_start","toolCallId":"read_0_644eb4af","toolName":"read","args":{"path":"notes.txt"}}
{"type":"tool_execution_end","toolCallId":"read_0_644eb4af","toolName":"read","result":{"content":[{"type":"text","text":"a\nb\n"}]},"isError":false}
{"type":"message_end","message":{"role":"toolResult","toolCallId":"read_0_644eb4af","toolName":"read","content":[{"type":"text","text":"a\nb\n"}],"isError":false}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Should try missing.","thinkingSignature":"reasoning_content"},{"type":"toolCall","id":"read_1_238f5c4b","name":"read","arguments":{"path":"missing.txt"}}],"provider":"moonshotai","model":"kimi-k3.1-pro","usage":{"input":263,"output":56,"cacheRead":1024,"cacheWrite":256},"stopReason":"toolUse"}}
{"type":"tool_execution_start","toolCallId":"read_1_238f5c4b","toolName":"read","args":{"path":"missing.txt"}}
{"type":"tool_execution_end","toolCallId":"read_1_238f5c4b","toolName":"read","result":{"content":[{"type":"text","text":"ENOENT: no such file or directory, access '/case/missing.txt'"}],"details":{}},"isError":true}
{"type":"tool_execution_start","toolCallId":"b1","toolName":"bash","args":{"command":"cargo test"}}
{"type":"tool_execution_end","toolCallId":"b1","toolName":"bash","result":{"content":[{"type":"text","text":"test failed\n\nCommand exited with code 101"}]},"isError":true}
{"type":"tool_execution_start","toolCallId":"b2","toolName":"bash","args":{"command":"rg x"}}
{"type":"tool_execution_end","toolCallId":"b2","toolName":"bash","result":{"content":[{"type":"text","text":"bash: rg: command not found\n\nCommand exited with code 127"}]},"isError":true}
{"type":"message_end","message":{"role":"toolResult","toolCallId":"read_1_238f5c4b","toolName":"read","content":[{"type":"text","text":"ENOENT: no such file or directory, access '/case/missing.txt'"}],"details":{},"isError":true}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Respond 2. Should think notes has a b output blank likely 2 lines yes.","thinkingSignature":"reasoning_content"},{"type":"text","text":"2"}],"provider":"moonshotai","model":"kimi-k3.1-pro","usage":{"input":146,"output":35,"cacheRead":1536,"cacheWrite":0},"stopReason":"stop"}}
{"type":"agent_end","messages":[]}"#;

        let parsed = parse_pi(stream);

        assert!(parsed.completed);
        assert_eq!(parsed.answer, "2");
        assert_eq!(parsed.metrics.model_turns, 3);
        assert_eq!(parsed.metrics.tool_calls["read"], 2);
        assert_eq!(parsed.metrics.tool_calls["bash"], 2);
        assert_eq!(
            parsed.metrics.tool_errors, 2,
            "the missing file and the missing command; a failing test is a result"
        );
        assert_eq!(parsed.metrics.tokens.input_tokens, 4_665);
        assert_eq!(parsed.metrics.tokens.cached_input_tokens, 2_560);
        assert_eq!(parsed.metrics.tokens.cache_write_input_tokens, 1_536);
        assert_eq!(parsed.metrics.tokens.output_tokens, 164);
    }

    #[test]
    fn pi_streams_that_end_in_an_error_are_not_complete() {
        let stream = r#"{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"input":10,"output":0,"cacheRead":0,"cacheWrite":0},"stopReason":"error","errorMessage":"429 rate limited"}}
{"type":"agent_end","messages":[]}"#;

        assert!(!parse_pi(stream).completed);
        assert!(!parse_pi("").completed, "no agent_end, no completion");
    }

    #[test]
    fn claude_code_streams_give_turns_tokens_errors_and_the_answer() {
        let stream = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read"}]}}
{"type":"user","message":{"content":[{"type":"tool_result","is_error":null,"content":"ok"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}
{"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":"Exit code 1\ncat: x: No such file"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit"}]}}
{"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":"File has not been read yet"}]}}
{"type":"result","terminal_reason":"completed","num_turns":4,"result":"Done.","usage":{"input_tokens":4,"cache_creation_input_tokens":100,"cache_read_input_tokens":900,"output_tokens":50}}"#;

        let parsed = parse_claude_code(stream);

        assert!(parsed.completed);
        assert_eq!(parsed.answer, "Done.");
        assert_eq!(parsed.metrics.model_turns, 4);
        assert_eq!(parsed.metrics.tool_calls["Bash"], 1);
        assert_eq!(
            parsed.metrics.tool_errors, 1,
            "a non-zero exit is a result, not an error"
        );
        assert_eq!(parsed.metrics.tokens.input_tokens, 1_004);
        assert_eq!(parsed.metrics.tokens.cached_input_tokens, 900);
        assert_eq!(parsed.metrics.tokens.cache_write_input_tokens, 100);
    }

    #[test]
    fn codex_streams_count_steps_and_only_unrunnable_commands_as_errors() {
        let stream = r#"{"type":"thread.started"}
{"type":"item.completed","item":{"id":"item_0","type":"error","message":"Model metadata for `kimi-k3.1-pro` not found. Defaulting to fallback metadata; this can degrade performance and cause issues."}}
{"type":"item.completed","item":{"type":"agent_message","text":"Reading first."}}
{"type":"item.completed","item":{"type":"command_execution","exit_code":0,"status":"completed"}}
{"type":"item.completed","item":{"type":"command_execution","exit_code":1,"status":"failed"}}
{"type":"item.completed","item":{"type":"command_execution","exit_code":127,"status":"failed"}}
{"type":"item.completed","item":{"type":"file_change","status":"completed"}}
{"type":"item.completed","item":{"type":"agent_message","text":"All done."}}
{"type":"turn.completed","usage":{"input_tokens":5000,"cached_input_tokens":4000,"cache_write_input_tokens":600,"output_tokens":300,"reasoning_output_tokens":40}}"#;

        let parsed = parse_codex(stream);

        assert!(parsed.completed);
        assert_eq!(parsed.answer, "All done.");
        assert_eq!(
            parsed.metrics.model_turns, 5,
            "Codex's own warning is not a step"
        );
        assert!(!parsed.metrics.tool_calls.contains_key("error"));
        assert_eq!(parsed.metrics.tool_errors, 1);
        assert_eq!(parsed.metrics.tokens.cached_input_tokens, 4_000);
        assert_eq!(parsed.metrics.tokens.cache_write_input_tokens, 600);
    }

    #[test]
    fn kimi_streams_count_assistant_turns_and_keep_the_last_answer() {
        let stream = r#"{"role":"assistant","tool_calls":[{"type":"function","id":"a","function":{"name":"Read","arguments":"{}"}}]}
{"role":"tool","tool_call_id":"a","content":"1\tfn main(){}"}
{"role":"assistant","content":"Editing now.","tool_calls":[{"type":"function","id":"b","function":{"name":"Edit","arguments":"{}"}}]}
{"role":"tool","tool_call_id":"b","content":"Replaced 1 occurrence in main.rs"}
{"role":"assistant","content":"ready"}
{"role":"meta","type":"session.resume_hint","session_id":"s"}"#;

        let parsed = parse_kimi(stream);

        assert!(parsed.completed);
        assert_eq!(parsed.answer, "ready");
        assert_eq!(parsed.metrics.model_turns, 3);
        assert_eq!(parsed.metrics.tool_calls["Edit"], 1);
    }

    #[test]
    fn kimi_streams_ending_on_a_tool_call_are_not_complete() {
        let stream = r#"{"role":"assistant","content":"Looking.","tool_calls":[{"type":"function","id":"a","function":{"name":"Read","arguments":"{}"}}]}"#;

        assert!(!parse_kimi(stream).completed);
    }

    #[test]
    fn grok_streams_answer_with_the_text_after_the_last_call() {
        let stream = r#"{"type":"available_commands","tools":["read_file"]}
{"type":"text","data":"I'll read it."}
{"type":"usage","usage":{"input_tokens":10}}
{"type":"tool_call","toolCallId":"1","toolName":"read_file","status":"pending"}
{"type":"tool_call_update","toolCallId":"1","status":"completed"}
{"type":"tool_call","toolCallId":"2","toolName":"run_terminal_command","status":"pending"}
{"type":"tool_call_update","toolCallId":"2","status":"failed"}
{"type":"usage","usage":{"input_tokens":10}}
{"type":"text","data":"rea"}
{"type":"text","data":"dy"}
{"type":"usage","usage":{"input_tokens":10}}
{"type":"end","stopReason":"end_turn","usage":{"input_tokens":20,"cache_read_input_tokens":100,"cache_creation_input_tokens":0,"output_tokens":7,"reasoning_tokens":3}}"#;

        let parsed = parse_grok(stream);

        assert!(parsed.completed);
        assert_eq!(parsed.answer, "ready");
        assert_eq!(parsed.metrics.model_turns, 3);
        assert_eq!(parsed.metrics.tool_errors, 1);
        assert_eq!(parsed.metrics.tool_calls["read_file"], 1);
        assert_eq!(parsed.metrics.tokens.input_tokens, 120);
        assert_eq!(parsed.metrics.tokens.cached_input_tokens, 100);
    }
}
