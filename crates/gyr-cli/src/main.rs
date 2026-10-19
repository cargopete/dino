//! The `gyr` executable.

mod approve;
mod config;
mod evals;
mod render;
mod replay;
mod session;
mod settings;
mod style;

use std::io::IsTerminal;
use std::io::Read;
use std::io::stdin;
use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use clap::Subcommand;
use gyr_core::Agent;
use gyr_core::AgentConfig;
use gyr_core::ToolRuntime;
use gyr_core::ToolSet;
use gyr_core::approval::AllowAll;
use gyr_core::approval::ApprovalPolicy;
use gyr_core::approval::Interactive;
use gyr_core::approval::ReadOnly;
use gyr_core::prompt::PromptContext;
use gyr_core::prompt::system_prompt;
use gyr_core::resume;
use gyr_core::session::JsonlSessionLog;
use gyr_core::session::SessionId;
use gyr_core::session::SessionMeta;
use gyr_model::ModelSession;
use gyr_model::builtin_profiles;
use gyr_protocol::ProfileStatus;
use gyr_protocol::StopReason;
use gyr_protocol::ToolDefinition;
use gyr_sandbox::Sandbox;
use tokio_util::sync::CancellationToken;

use crate::approve::TerminalApprover;
use crate::config::ApprovalMode;
use crate::config::RunSettings;
use crate::config::SandboxMode;
use crate::render::Renderer;
use crate::session::Session;
use crate::settings::Layers;
use crate::settings::Source;

#[derive(Debug, Parser)]
#[command(
    name = "gyr",
    version,
    about = "A Rust-first terminal coding agent",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// With no subcommand, these open an interactive session.
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List the deliberately bounded built-in model catalogue.
    Models {
        /// Emit the full catalogue as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the system prompt a run would send, so its cost is inspectable.
    Prompt(PromptArgs),
    /// Run one request and exit. For scripts, CI and evals.
    Run(RunArgs),
    /// Run the eval corpus against a model.
    Eval(crate::evals::EvalArgs),
    /// Show every setting, its value, and where that value came from.
    Config(PromptArgs),
    /// Print a past session from its log.
    Replay(crate::replay::ReplayArgs),
}

// These are switches rather than a state machine because each one means one
// thing to a person reading `--help`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default, clap::Args)]
struct CommonArgs {
    /// Model key from `gyr models`. Defaults to `GYR_MODEL`.
    #[arg(long)]
    model: Option<String>,
    /// Workspace root. Defaults to the current directory.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Session log path. Defaults to .gyr/sessions/<id>.jsonl in the workspace.
    #[arg(long)]
    log: Option<PathBuf>,
    /// Safety limit on model turns in one submission. Default 32.
    #[arg(long)]
    max_turns: Option<NonZeroU32>,
    /// Refuse every mutation and every process instead of asking.
    #[arg(long, conflicts_with = "dangerously_allow_all")]
    read_only: bool,
    /// Approve everything without asking. Consider this carefully.
    #[arg(long)]
    dangerously_allow_all: bool,
    /// Operating-system containment for processes this run starts. Default
    /// workspace.
    #[arg(long, value_enum)]
    sandbox: Option<SandboxMode>,
    /// Show streamed reasoning summaries where the provider sends them.
    #[arg(long)]
    show_reasoning: bool,
    /// Never emit terminal colour.
    #[arg(long)]
    plain: bool,
    /// Endpoint for a self-served model, standing in for `QWEN_API_BASE`.
    #[arg(long)]
    api_base: Option<String>,
    /// Ask a toggling model not to think. Leaves the server's default alone
    /// when absent.
    #[arg(long)]
    no_thinking: bool,
    /// Continue a previous session. Bare, the most recent in this workspace.
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,
}

#[derive(Debug, clap::Args)]
struct PromptArgs {
    /// Model key from `gyr models`. Defaults to `GYR_MODEL`.
    #[arg(long)]
    model: Option<String>,
    /// Workspace root. Defaults to the current directory.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Containment to describe, so a printed prompt matches a real run.
    #[arg(long, value_enum, default_value_t = SandboxMode::Workspace)]
    sandbox: SandboxMode,
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// The request. Read from standard input when omitted.
    prompt: Option<String>,
    #[command(flatten)]
    common: CommonArgs,
}

fn main() -> ExitCode {
    match dispatch() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("gyr: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Models { json }) => {
            list_models(json)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Prompt(args)) => {
            print_prompt(&args)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Run(args)) => on_runtime(run(args)),
        Some(Command::Eval(args)) => on_runtime(evals::run(args)),
        Some(Command::Config(args)) => {
            print_config(&args)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Replay(args)) => replay::run(&args),
        None => on_runtime(session(cli.common)),
    }
}

fn on_runtime<F>(future: F) -> Result<ExitCode>
where
    F: Future<Output = Result<ExitCode>>,
{
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?
        .block_on(future)
}

fn list_models(json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&builtin_profiles())?);
        return Ok(());
    }
    for profile in builtin_profiles() {
        let note = match profile.status {
            ProfileStatus::Supported => "",
            ProfileStatus::Development => "  (development)",
        };
        println!(
            "{:<24} {:<12} {}{note}",
            profile.key,
            format!("{:?}", profile.provider).to_lowercase(),
            profile.provider_model
        );
    }
    Ok(())
}

fn print_prompt(args: &PromptArgs) -> Result<()> {
    let profile = config::resolve_profile(args.model.as_deref())?;
    let workspace = config::resolve_workspace(args.workspace.as_deref())?;
    let sandbox = config::build_sandbox(args.sandbox, &workspace)?;
    let tools = tool_set(&workspace, sandbox)?;
    let context = prompt_context(&workspace, ApprovalMode::Interactive, &tools.definitions());
    let prompt = system_prompt(&context);
    eprintln!(
        "model {} · {} bytes of system prompt",
        profile.key,
        prompt.len()
    );
    println!("{prompt}");
    Ok(())
}

/// The tools one session offers.
///
/// Deferred to `gyr-eval` so the corpus and the executable cannot drift. They
/// did drift once, for exactly one commit, and the eval run said so.
fn tool_set(workspace: &Path, sandbox: Arc<dyn Sandbox>) -> Result<ToolSet> {
    gyr_eval::session_tools(workspace, sandbox).map_err(|error| anyhow::anyhow!("{error}"))
}

fn prompt_context(
    workspace: &Path,
    mode: ApprovalMode,
    definitions: &[ToolDefinition],
) -> PromptContext {
    PromptContext {
        workspace_root: workspace.display().to_string(),
        tools: definitions
            .iter()
            .map(|definition| definition.name.clone())
            .collect(),
        approval_mode: mode.label().to_owned(),
    }
}

/// Everything a submission needs, assembled once.
struct Prepared {
    agent: Agent<Box<dyn ModelSession>, ToolSet>,
    state_path: PathBuf,
    usage: std::sync::Arc<std::sync::Mutex<gyr_protocol::TokenUsage>>,
    settings: RunSettings,
    sandbox_label: String,
    session_id: SessionId,
}

fn prepare(common: &CommonArgs) -> Result<Prepared> {
    let session_id = SessionId::generate();
    let settings = settings(common, &session_id)?;
    // After resolution rather than before, so a config file can ask for plain
    // output as well as the flag.
    style::enable(settings.plain);

    let sandbox = config::build_sandbox(settings.sandbox, &settings.workspace)?;
    let sandbox_label = sandbox.label();
    let tools = tool_set(&settings.workspace, Arc::clone(&sandbox))?;
    let definitions = tools.definitions();
    let context = prompt_context(&settings.workspace, settings.mode, &definitions);
    let mut model = config::build_session(&settings, system_prompt(&context), definitions)?;

    let log = JsonlSessionLog::create(
        &settings.log_path,
        SessionMeta {
            session_id: session_id.clone(),
            gyrfalcon_version: env!("CARGO_PKG_VERSION").to_owned(),
            model_key: settings.profile.key.clone(),
            provider: settings.profile.provider,
            workspace_root: settings.workspace.display().to_string(),
            approval_mode: settings.mode.label().to_owned(),
            sandbox: sandbox_label.clone(),
            max_model_turns: settings.max_turns.get(),
        },
    )?;

    // Restoring before the first turn, so the conversation continues rather than
    // starting. A mismatch refuses here rather than confusing the provider later.
    let state_path = match resume_from(common.resume.as_deref(), &settings.workspace)? {
        Some((id, path)) => {
            let state = resume::load(&path)?;
            model.restore_state(&state)?;
            eprintln!(
                "{}",
                style::paint(style::FAINT, &format!("resumed session {id}"))
            );
            path
        }
        None => resume::state_path(&settings.workspace, session_id.as_str()),
    };

    let renderer = Renderer::new(settings.show_reasoning);
    let usage = renderer.usage_handle();
    let agent = Agent::new(
        model,
        tools,
        AgentConfig {
            max_model_turns: settings.max_turns,
            ..AgentConfig::default()
        },
    )
    .with_policy(policy(settings.mode, &sandbox_label))
    .with_sink((renderer, log));

    Ok(Prepared {
        agent,
        state_path,
        usage,
        settings,
        sandbox_label,
        session_id,
    })
}

async fn session(common: CommonArgs) -> Result<ExitCode> {
    let prepared = prepare(&common)?;
    let mut session = Session {
        agent: prepared.agent,
        usage: prepared.usage,
        model: prepared.settings.profile.display_name.clone(),
        workspace: prepared.settings.workspace.display().to_string(),
        sandbox: prepared.sandbox_label,
        approvals: prepared.settings.mode.label().to_owned(),
        log_path: prepared.settings.log_path.display().to_string(),
        history_path: prepared.settings.workspace.join(".gyr").join("history"),
        state_path: prepared.state_path,
    };
    let _ = prepared.session_id;
    session.run().await?;
    Ok(ExitCode::SUCCESS)
}

async fn run(args: RunArgs) -> Result<ExitCode> {
    let mut prepared = prepare(&args.common)?;
    let request = request_text(args.prompt)?;
    eprintln!(
        "{}",
        style::paint(
            style::FAINT,
            &format!(
                "{} · {} · {} · log {}",
                prepared.settings.profile.display_name,
                prepared.settings.mode.label(),
                prepared.sandbox_label,
                prepared.settings.log_path.display()
            )
        )
    );

    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });

    let result = prepared.agent.run_cancellable(request, &cancel).await?;
    save_state(&prepared.agent, &prepared.state_path)?;
    Ok(match result.stop_reason {
        StopReason::EndTurn => ExitCode::SUCCESS,
        StopReason::Cancelled => ExitCode::from(130),
        _ => ExitCode::FAILURE,
    })
}

/// Resolves `--resume`, bare or named, into a state file.
///
/// Bare means the most recent in this workspace. A named session that does not
/// exist is an error rather than a silent new session, because a person who
/// asked to continue something has said what they want.
fn resume_from(requested: Option<&str>, workspace: &Path) -> Result<Option<(String, PathBuf)>> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    if requested.is_empty() {
        let Some(found) = resume::most_recent(workspace)? else {
            bail!(
                "no session to resume in {}; run without --resume to start one",
                workspace.display()
            );
        };
        return Ok(Some(found));
    }
    let path = resume::state_path(workspace, requested);
    if !path.is_file() {
        bail!("no session {requested:?} in {}", workspace.display());
    }
    Ok(Some((requested.to_owned(), path)))
}

/// Persists the session's native state, so it can be continued later.
fn save_state<S: ModelSession, T: gyr_core::ToolRuntime>(
    agent: &Agent<S, T>,
    path: &Path,
) -> Result<()> {
    // A provider that cannot export is not an error: OpenAI's continuation may
    // have expired anyway, and a one-shot against a provider without state is
    // still a complete run.
    match agent.session().export_state() {
        Ok(state) => resume::save(path, &state).map_err(Into::into),
        Err(_) => Ok(()),
    }
}

/// Everything a run needs, with each value's origin, for `gyr config`.
struct Resolved {
    model: (Option<String>, Source),
    max_turns: (NonZeroU32, Source),
    approvals: (ApprovalMode, Source),
    sandbox: (SandboxMode, Source),
    api_base: (Option<String>, Source),
    show_reasoning: (bool, Source),
    no_thinking: (bool, Source),
    plain: (bool, Source),
    layers: Layers,
}

/// Applies RFC-0015's precedence: flag, environment, project file, user file,
/// default.
fn resolve(args: &CommonArgs, workspace: &Path) -> Result<Resolved> {
    let layers = Layers::load(workspace)?;

    let model = if let Some(model) = args.model.clone() {
        (Some(model), Source::Flag)
    } else if let Ok(model) = std::env::var("GYR_MODEL") {
        (Some(model), Source::Environment)
    } else if let Some((model, source)) = layers.pick(|file| file.model.clone()) {
        (Some(model), source)
    } else {
        (None, Source::Default)
    };

    let approvals = if args.dangerously_allow_all {
        (ApprovalMode::AllowAll, Source::Flag)
    } else if args.read_only {
        (ApprovalMode::ReadOnly, Source::Flag)
    } else if let Some(found) = layers.pick_user_only(|file| file.approvals) {
        found
    } else {
        (ApprovalMode::Interactive, Source::Default)
    };

    let api_base = if let Some(base) = args.api_base.clone() {
        (Some(base), Source::Flag)
    } else if let Ok(base) = std::env::var("QWEN_API_BASE") {
        (Some(base), Source::Environment)
    } else if let Some((base, source)) = layers.pick_user_only(|file| file.api_base.clone()) {
        (Some(base), source)
    } else {
        (None, Source::Default)
    };

    // A flag can only turn these on, so a set flag is unambiguous and an unset
    // one falls through to the files.
    let switch = |flag: bool, choose: fn(&crate::settings::FileConfig) -> Option<bool>| {
        if flag {
            (true, Source::Flag)
        } else {
            layers.pick(choose).unwrap_or((false, Source::Default))
        }
    };

    Ok(Resolved {
        model,
        max_turns: args.max_turns.map_or_else(
            || {
                layers.pick(|file| file.max_turns).unwrap_or((
                    NonZeroU32::new(32).expect("32 is non-zero"),
                    Source::Default,
                ))
            },
            |turns| (turns, Source::Flag),
        ),
        approvals,
        sandbox: args.sandbox.map_or_else(
            || {
                layers
                    .pick_user_only(|file| file.sandbox)
                    .unwrap_or((SandboxMode::Workspace, Source::Default))
            },
            |mode| (mode, Source::Flag),
        ),
        api_base,
        show_reasoning: switch(args.show_reasoning, |file| file.show_reasoning),
        no_thinking: switch(args.no_thinking, |file| file.no_thinking),
        plain: switch(args.plain, |file| file.plain),
        layers,
    })
}

fn print_config(args: &PromptArgs) -> Result<()> {
    let workspace = config::resolve_workspace(args.workspace.as_deref())?;
    let common = CommonArgs {
        model: args.model.clone(),
        ..CommonArgs::default()
    };
    let resolved = resolve(&common, &workspace)?;

    let show = |value: String, source: Source| format!("{value}  ({})", source.label());
    let rows = vec![
        (
            "model",
            show(
                resolved.model.0.clone().unwrap_or_else(|| "unset".into()),
                resolved.model.1,
            ),
        ),
        (
            "workspace",
            format!("{}  (where you are)", workspace.display()),
        ),
        (
            "approvals",
            show(
                resolved.approvals.0.label().to_owned(),
                resolved.approvals.1,
            ),
        ),
        (
            "sandbox",
            show(
                format!("{:?}", resolved.sandbox.0).to_lowercase(),
                resolved.sandbox.1,
            ),
        ),
        (
            "max turns",
            show(resolved.max_turns.0.to_string(), resolved.max_turns.1),
        ),
        (
            "api base",
            show(
                resolved
                    .api_base
                    .0
                    .clone()
                    .unwrap_or_else(|| "unset".into()),
                resolved.api_base.1,
            ),
        ),
        (
            "reasoning",
            show(
                resolved.show_reasoning.0.to_string(),
                resolved.show_reasoning.1,
            ),
        ),
        (
            "thinking off",
            show(resolved.no_thinking.0.to_string(), resolved.no_thinking.1),
        ),
        (
            "plain",
            show(resolved.plain.0.to_string(), resolved.plain.1),
        ),
        (
            "project file",
            resolved
                .layers
                .project_path
                .as_ref()
                .map_or_else(|| "none".to_owned(), |path| path.display().to_string()),
        ),
        (
            "user file",
            resolved
                .layers
                .user_path
                .as_ref()
                .map_or_else(|| "none".to_owned(), |path| path.display().to_string()),
        ),
    ];
    print!("{}", crate::render::status_block(&rows));
    println!(
        "{}",
        style::paint(
            style::FAINT,
            "  a project file may not set approvals, sandbox or api base",
        )
    );
    Ok(())
}

fn settings(args: &CommonArgs, session_id: &SessionId) -> Result<RunSettings> {
    let workspace = config::resolve_workspace(args.workspace.as_deref())?;
    let resolved = resolve(args, &workspace)?;
    let profile = config::resolve_profile(resolved.model.0.as_deref())?;
    let mode = resolved.approvals.0;
    let log_path = match &args.log {
        Some(path) => path.clone(),
        None => config::default_log_path(&workspace, session_id.as_str()),
    };
    Ok(RunSettings {
        profile,
        workspace,
        log_path,
        max_turns: resolved.max_turns.0,
        mode,
        sandbox: resolved.sandbox.0,
        show_reasoning: resolved.show_reasoning.0,
        api_base: resolved.api_base.0,
        disable_thinking: resolved.no_thinking.0,
        plain: resolved.plain.0,
    })
}

fn policy(mode: ApprovalMode, sandbox: &str) -> Box<dyn ApprovalPolicy> {
    match mode {
        ApprovalMode::Interactive => Box::new(Interactive::new(TerminalApprover::new(sandbox))),
        ApprovalMode::ReadOnly => Box::new(ReadOnly),
        ApprovalMode::AllowAll => Box::new(AllowAll),
    }
}

fn request_text(argument: Option<String>) -> Result<String> {
    if let Some(text) = argument {
        return Ok(text);
    }
    if stdin().is_terminal() {
        bail!("no request given: pass one as an argument, pipe it in, or run `gyr` for a session");
    }
    let mut text = String::new();
    stdin()
        .read_to_string(&mut text)
        .context("cannot read the request from standard input")?;
    if text.trim().is_empty() {
        bail!("the request read from standard input was empty");
    }
    Ok(text)
}
