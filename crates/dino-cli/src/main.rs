//! The `dino` executable.

mod approve;
mod calls;
mod config;
mod diff;
mod evals;
mod handover;
mod listen;
mod markdown;
mod probe;
mod recall;
mod render;
mod replay;
mod session;
mod settings;
mod spinner;
mod style;
mod summary;

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
use dino_core::Agent;
use dino_core::AgentConfig;
use dino_core::ToolRuntime;
use dino_core::ToolSet;
use dino_core::approval::AllowAll;
use dino_core::approval::ApprovalPolicy;
use dino_core::approval::Interactive;
use dino_core::approval::ReadOnly;
use dino_core::prompt::PromptContext;
use dino_core::prompt::system_prompt;
use dino_core::resume;
use dino_core::session::JsonlSessionLog;
use dino_core::session::SessionId;
use dino_core::session::SessionMeta;
use dino_model::ModelSession;
use dino_model::builtin_profiles;
use dino_protocol::ProfileStatus;
use dino_protocol::StopReason;
use dino_protocol::ToolDefinition;
use dino_sandbox::Sandbox;
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
    name = "dino",
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
    /// Review uncommitted changes, or a branch against a base, without editing.
    Review(ReviewArgs),
    /// Run the eval corpus against a model.
    Eval(crate::evals::EvalArgs),
    /// Probe an endpoint: tool parser, thinking, prompt delivery, throughput, context.
    Probe(probe::ProbeArgs),
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
    /// Model key from `dino models`. Defaults to `DINO_MODEL`.
    #[arg(long)]
    model: Option<String>,
    /// Workspace root. Defaults to the current directory.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Session log path. Defaults to .dino/sessions/<id>.jsonl in the workspace.
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
    /// Model key from `dino models`. Defaults to `DINO_MODEL`.
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
    /// Split a long task across fresh sessions, carried by a progress record.
    /// RFC-0018.
    #[arg(long)]
    handover: bool,
    /// With --handover, the turns one session may take before it hands over.
    #[arg(long, default_value_t = 16)]
    session_turns: u32,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Debug, clap::Args)]
struct ReviewArgs {
    /// Review the commits since this base, as `git diff <base>...HEAD`, rather
    /// than the uncommitted changes.
    #[arg(long)]
    base: Option<String>,
    /// What to look for, in place of correctness bugs.
    #[arg(long)]
    focus: Option<String>,
    #[command(flatten)]
    common: CommonArgs,
}

fn main() -> ExitCode {
    match dispatch() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("dino: {error:#}");
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
        Some(Command::Review(args)) => on_runtime(review(args)),
        Some(Command::Eval(args)) => on_runtime(evals::run(args)),
        Some(Command::Probe(args)) => {
            let common = CommonArgs {
                model: args.model.clone(),
                workspace: args.workspace.clone(),
                api_base: args.api_base.clone(),
                plain: args.plain,
                ..CommonArgs::default()
            };
            let run_settings = settings(&common, &SessionId::generate())?;
            on_runtime(probe::execute(run_settings, args))
        }
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
    let sandbox = config::build_sandbox(args.sandbox, &workspace, &[])?;
    let tools = tool_set(
        &workspace,
        sandbox,
        &interactive_options(&workspace, Vec::new(), None, Vec::new()),
        Vec::new(),
    )?;
    let context = prompt_context(
        &workspace,
        ApprovalMode::Interactive,
        &tools.definitions(),
        None,
    );
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
/// Deferred to `dino-eval` so the corpus and the executable cannot drift. They
/// did drift once, for exactly one commit, and the eval run said so.
fn tool_set(
    workspace: &Path,
    sandbox: Arc<dyn Sandbox>,
    options: &dino_eval::ToolOptions,
    extra: Vec<Box<dyn ToolRuntime>>,
) -> Result<ToolSet> {
    dino_eval::session_tools_with(workspace, sandbox, options, extra)
        .map_err(|error| anyhow::anyhow!("{error}"))
}

/// What must be true before the model may end its turn: the user's
/// before-end-turn hooks, and for a run nobody watches, a clean build.
fn finish_checks(
    settings: &RunSettings,
    sandbox: &Arc<dyn Sandbox>,
    unattended: bool,
) -> Result<Vec<Box<dyn dino_core::FinishCheck>>> {
    let mut checks: Vec<Box<dyn dino_core::FinishCheck>> = Vec::new();
    if !settings.end_turn_hooks.is_empty() {
        checks.push(Box::new(dino_exec::EndTurnHooks::new(
            settings.end_turn_hooks.clone(),
            &settings.workspace,
            Arc::clone(sandbox),
        )));
    }
    if unattended
        && let Some(build) = dino_eval::build_check(&settings.workspace, Arc::clone(sandbox))?
    {
        checks.push(Box::new(build));
    }
    Ok(checks)
}

/// What each session-start hook printed, as instructions under its command.
fn start_hook_output(
    settings: &RunSettings,
    sandbox: &Arc<dyn Sandbox>,
) -> Vec<dino_core::prompt::Instructions> {
    if settings.start_hooks.is_empty() {
        return Vec::new();
    }
    let said = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(dino_exec::run_start_hooks(
            &settings.start_hooks,
            &settings.workspace,
            sandbox,
        ))
    });
    said.into_iter()
        .map(|(command, text)| dino_core::prompt::Instructions {
            source: format!("session_start: {command}"),
            text,
            truncated: false,
        })
        .collect()
}

/// Everything a person's session can call, MCP servers included.
fn session_tool_set(
    settings: &RunSettings,
    sandbox: &Arc<dyn Sandbox>,
    extra: Vec<Box<dyn ToolRuntime>>,
) -> Result<ToolSet> {
    let mut mcp = connect_mcp(&settings.mcp, &settings.workspace, sandbox);
    mcp.extend(extra);
    tool_set(
        &settings.workspace,
        Arc::clone(sandbox),
        &interactive_options(
            &settings.workspace,
            settings.protected.clone(),
            settings.max_diagnostics,
            settings.hooks.clone(),
        ),
        mcp,
    )
}

/// Starts each configured MCP server and lists its tools. A server that fails
/// is named and left out; the session goes on without it.
fn connect_mcp(
    servers: &[dino_exec::McpServer],
    workspace: &Path,
    sandbox: &Arc<dyn Sandbox>,
) -> Vec<Box<dyn ToolRuntime>> {
    let mut connected: Vec<Box<dyn ToolRuntime>> = Vec::new();
    let mut names = Vec::new();
    for server in servers {
        let name = server.name.clone();
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(dino_exec::McpTools::connect(
                server.clone(),
                workspace,
                sandbox,
            ))
        });
        match result {
            Ok(tools) => {
                names.push(tools.summary());
                connected.push(Box::new(tools));
            }
            Err(error) => eprintln!(
                "{}",
                style::paint(style::WARN, &format!("mcp {name}: {error}"))
            ),
        }
    }
    if !names.is_empty() {
        eprintln!(
            "{}",
            style::paint(style::FAINT, &format!("mcp: {}", names.join(", ")))
        );
    }
    connected
}

fn prompt_context(
    workspace: &Path,
    mode: ApprovalMode,
    definitions: &[ToolDefinition],
    max_model_turns: Option<u32>,
) -> PromptContext {
    PromptContext {
        workspace_root: workspace.display().to_string(),
        tools: definitions
            .iter()
            .map(|definition| definition.name.clone())
            .collect(),
        approval_mode: mode.label().to_owned(),
        max_model_turns,
        git: dino_core::git::GitSnapshot::take(workspace),
        outline: dino_rust::outline::outline(workspace),
        memory: memory_files(workspace)
            .map(|files| dino_core::prompt::load_memory(&files))
            .unwrap_or_default(),
        skills: skills(workspace),
        instructions: dino_core::prompt::load_instructions(
            settings::user_instructions_path().as_deref(),
            workspace,
        ),
    }
}

/// The tools a person's own session gets, as opposed to an eval's.
fn interactive_options(
    workspace: &Path,
    protected: Vec<PathBuf>,
    max_diagnostics: Option<usize>,
    hooks: Vec<dino_exec::Hook>,
) -> dino_eval::ToolOptions {
    dino_eval::ToolOptions {
        protected,
        max_diagnostics,
        hooks,
        memory: memory_files(workspace),
        skills: skills(workspace),
        github: dino_exec::GithubTool::available(),
        web: true,
        skip_edit_check: false,
        escalation: true,
    }
}

/// The user's skills, then the repository's; the user's win a shared name.
fn skills(workspace: &Path) -> Vec<dino_core::skills::Skill> {
    let mut dirs: Vec<PathBuf> = settings::user_dir()
        .map(|dir| dir.join("skills"))
        .into_iter()
        .collect();
    dirs.push(workspace.join(".dino").join("skills"));
    dino_core::skills::discover(&dirs)
}

/// The user's memory file and this workspace's, beside the user config.
fn memory_files(workspace: &Path) -> Option<dino_core::memory::MemoryFiles> {
    let dir = settings::user_dir()?.join("memory");
    let name = workspace.display().to_string().replace('/', "%");
    Some(dino_core::memory::MemoryFiles {
        user: dir.join("user.md"),
        workspace: dir.join(format!("{name}.md")),
    })
}

/// Names the instruction files a session read, so they are never a surprise.
fn announce_instructions(context: &PromptContext) {
    if context.instructions.is_empty() && context.memory.is_empty() {
        return;
    }
    let sources = context
        .instructions
        .iter()
        .chain(&context.memory)
        .map(|item| {
            let source = render::short_path(Path::new(&item.source), Path::new(""));
            if item.truncated {
                format!("{source} (truncated)")
            } else {
                source
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!(
        "{}",
        style::paint(style::FAINT, &format!("instructions: {sources}"))
    );
}

/// Everything a submission needs, assembled once.
pub(crate) struct Prepared {
    agent: Agent<Box<dyn ModelSession>, ToolSet>,
    state_path: PathBuf,
    usage: std::sync::Arc<std::sync::Mutex<dino_protocol::TokenUsage>>,
    settings: RunSettings,
    sandbox_label: String,
    session_id: SessionId,
    system_prompt: String,
    definitions: Vec<ToolDefinition>,
    requests: Arc<std::sync::atomic::AtomicU64>,
    planning: Arc<std::sync::atomic::AtomicBool>,
    context_full: Arc<std::sync::atomic::AtomicBool>,
    status: Arc<std::sync::Mutex<spinner::Status>>,
    sandbox: Arc<dyn Sandbox>,
}

/// `unattended` is for a one-shot run, which has no person to hand an unfinished
/// build back to.
fn prepare(
    common: &CommonArgs,
    unattended: bool,
    extra: Vec<Box<dyn ToolRuntime>>,
) -> Result<Prepared> {
    let session_id = SessionId::generate();
    let settings = settings(common, &session_id)?;
    // After resolution rather than before, so a config file can ask for plain
    // output as well as the flag.
    style::enable(settings.plain);

    let sandbox =
        config::build_sandbox(settings.sandbox, &settings.workspace, &settings.protected)?;
    let sandbox_label = sandbox.label();
    if !settings.protected.is_empty() && !sandbox.protects_paths() {
        eprintln!(
            "{}",
            style::paint(
                style::WARN,
                "protected paths are refused to the file tools only: this sandbox cannot \
                 stop a process writing them"
            )
        );
    }
    let tools = session_tool_set(&settings, &sandbox, extra)?;
    let definitions = tools.definitions();
    let mut context = prompt_context(
        &settings.workspace,
        settings.mode,
        &definitions,
        Some(settings.max_turns.get()),
    );
    context
        .instructions
        .extend(start_hook_output(&settings, &sandbox));
    announce_instructions(&context);
    let prompt = system_prompt(&context);
    let mut model = config::build_session(&settings, prompt.clone(), definitions.clone())?;

    let log = JsonlSessionLog::create(
        &settings.log_path,
        SessionMeta {
            session_id: session_id.clone(),
            dino_version: env!("CARGO_PKG_VERSION").to_owned(),
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
    let requests = renderer.requests_handle();
    let context_full = renderer.context_handle();
    let status = renderer.status_handle();
    if let Ok(mut status) = status.lock() {
        status.answering(&settings.profile);
    }
    let planning = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let agent = Agent::new(
        model,
        tools,
        AgentConfig {
            max_model_turns: settings.max_turns,
            ..AgentConfig::default()
        },
    )
    .with_policy(dino_core::approval::PlanGate::new(
        policy(settings.mode, &sandbox_label, &settings.workspace),
        Arc::clone(&planning),
    ))
    .with_sink((renderer, log));
    let checks = finish_checks(&settings, &sandbox, unattended)?;
    let agent = if checks.is_empty() {
        agent
    } else {
        agent.with_finish_check(dino_core::FirstUnfinished::new(checks))
    };

    Ok(Prepared {
        agent,
        state_path,
        usage,
        settings,
        sandbox_label,
        session_id,
        system_prompt: prompt,
        definitions,
        requests,
        planning,
        context_full,
        status,
        sandbox,
    })
}

async fn session(common: CommonArgs) -> Result<ExitCode> {
    let prepared = prepare(&common, false, Vec::new())?;
    let base = prepared.settings.clone();
    let prompt = prepared.system_prompt.clone();
    let definitions = prepared.definitions.clone();
    let factory: session::SessionFactory<Box<dyn ModelSession>> = Box::new(move |key| {
        let mut settings = base.clone();
        settings.profile = config::resolve_profile(Some(key))?;
        let name = settings.profile.display_name.clone();
        let model = config::build_session(&settings, prompt.clone(), definitions.clone())?;
        Ok((model, name))
    });
    let inbox = dino_core::Inbox::default();
    let mut session = Session {
        agent: prepared.agent.with_inbox(std::sync::Arc::clone(&inbox)),
        inbox,
        typed_ahead: String::new(),
        usage: prepared.usage,
        requests: prepared.requests,
        factory,
        model_key: prepared.settings.profile.key.clone(),
        workspace_root: prepared.settings.workspace.clone(),
        run_id: prepared.session_id.as_str().to_owned(),
        conversations: 0,
        planning: prepared.planning,
        awaiting_plan: false,
        carried: None,
        recall: crate::recall::Recall::default(),
        auto_compact: prepared.settings.compaction == config::Compaction::Automatic,
        context_full: prepared.context_full,
        status: prepared.status,
        model: prepared.settings.profile.display_name.clone(),
        workspace: prepared.settings.workspace.display().to_string(),
        sandbox: prepared.sandbox_label,
        approvals: prepared.settings.mode.label().to_owned(),
        log_path: prepared.settings.log_path.display().to_string(),
        history_path: prepared.settings.workspace.join(".dino").join("history"),
        state_path: prepared.state_path,
    };
    session.run().await?;
    Ok(ExitCode::SUCCESS)
}

/// The largest diff put into a review request; past it the model reads files.
const MAX_REVIEW_DIFF_BYTES: usize = 200 * 1_024;

/// Runs a read-only review. The diff is taken here and handed over, so the
/// model needs no process and the session can refuse every change.
async fn review(mut args: ReviewArgs) -> Result<ExitCode> {
    let workspace = config::resolve_workspace(args.common.workspace.as_deref())?;
    let git = |arguments: &[&str]| -> Result<String> {
        let output = std::process::Command::new("git")
            .args(arguments)
            .current_dir(&workspace)
            .output()
            .context("cannot run git")?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                arguments.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let (what, diff, untracked) = match &args.base {
        Some(base) => (
            format!("the commits since `{base}`"),
            git(&["diff", &format!("{base}...HEAD")])?,
            String::new(),
        ),
        None => (
            "the uncommitted changes".to_owned(),
            git(&["diff", "HEAD"])?,
            git(&["ls-files", "--others", "--exclude-standard"])?,
        ),
    };
    if diff.trim().is_empty() && untracked.trim().is_empty() {
        eprintln!("dino: nothing to review in {what}");
        return Ok(ExitCode::SUCCESS);
    }
    let shown = dino_core::text::capped_head(&diff, MAX_REVIEW_DIFF_BYTES);
    let focus = args.focus.take().unwrap_or_else(|| {
        "correctness bugs: wrong results, panics, lost errors, races, missed edge cases".to_owned()
    });
    let mut request = format!(
        "Review {what} in this repository for {focus}. Read the surrounding code where the \
         diff alone cannot settle a question. Nothing can be changed in this session. Report \
         each finding as `path:line`, one sentence saying what is wrong and one saying what \
         input or state shows it, most severe first. Leave out style and anything you could not \
         confirm from the code. If you find nothing, say so.\n\n```diff\n{shown}\n```\n"
    );
    if !untracked.trim().is_empty() {
        request.push_str("\nNew files, not in the diff; read them:\n");
        request.push_str(&untracked);
    }
    args.common.read_only = true;
    run(RunArgs {
        prompt: Some(request),
        handover: false,
        // Read only with --handover, which a review never uses.
        session_turns: 0,
        common: args.common,
    })
    .await
}

async fn run(args: RunArgs) -> Result<ExitCode> {
    let workspace = config::resolve_workspace(args.common.workspace.as_deref())?;
    let progress = args.handover.then(|| {
        dino_core::progress::Progress::new(
            workspace
                .join(".dino")
                .join("sessions")
                .join(format!("{}.progress.json", SessionId::generate().as_str())),
        )
    });
    let extra: Vec<Box<dyn ToolRuntime>> = progress
        .iter()
        .map(|progress| Box::new(progress.clone()) as Box<dyn ToolRuntime>)
        .collect();
    let mut prepared = prepare(&args.common, true, extra)?;
    let request = request_text(args.prompt)?;
    eprintln!(
        "{}",
        style::paint(
            style::FAINT,
            &format!(
                "{} · {} · {} · log {}",
                prepared.settings.profile.display_name,
                render::short_label(prepared.settings.mode.label()),
                render::short_label(&prepared.sandbox_label),
                render::short_path(&prepared.settings.log_path, &prepared.settings.workspace)
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

    if let Some(progress) = &progress {
        return Ok(
            match handover::run(
                &mut prepared,
                progress,
                &request,
                args.session_turns,
                &cancel,
            )
            .await?
            {
                handover::Ended::Stopped(StopReason::EndTurn) => ExitCode::SUCCESS,
                handover::Ended::Stopped(StopReason::Cancelled) => ExitCode::from(130),
                handover::Ended::Stalled => {
                    eprintln!(
                        "{}",
                        style::paint(
                            style::FAINT,
                            "  stopped: two sessions in a row ended with the plan unfinished"
                        )
                    );
                    ExitCode::FAILURE
                }
                _ => ExitCode::FAILURE,
            },
        );
    }
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
pub(crate) fn save_state<S: ModelSession, T: dino_core::ToolRuntime>(
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

/// Everything a run needs, with each value's origin, for `dino config`.
struct Resolved {
    model: (Option<String>, Source),
    max_turns: (NonZeroU32, Source),
    approvals: (ApprovalMode, Source),
    sandbox: (SandboxMode, Source),
    api_base: (Option<String>, Source),
    show_reasoning: (bool, Source),
    no_thinking: (bool, Source),
    plain: (bool, Source),
    protected: (Vec<PathBuf>, Source),
    max_diagnostics: (Option<usize>, Source),
    thinking_budget: (Option<u32>, Source),
    auto_compact: bool,
    fallback: Option<String>,
    hooks: (Vec<dino_exec::Hook>, Source),
    end_turn_hooks: Vec<dino_exec::Hook>,
    start_hooks: Vec<dino_exec::Hook>,
    mcp: Vec<dino_exec::McpServer>,
    layers: Layers,
}

/// The user file's hooks for one event. A project file cannot set any.
fn hooks_for(layers: &Layers, event: settings::HookEvent) -> Vec<dino_exec::Hook> {
    layers
        .pick_user_only(|file| file.hooks.clone())
        .map(|(hooks, _)| {
            hooks
                .into_iter()
                .filter(|hook| hook.event == event)
                .map(|hook| dino_exec::Hook {
                    command: hook.command,
                    extension: hook.extension,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Applies RFC-0015's precedence: flag, environment, project file, user file,
/// default.
fn resolve(args: &CommonArgs, workspace: &Path) -> Result<Resolved> {
    let layers = Layers::load(workspace)?;

    let model = if let Some(model) = args.model.clone() {
        (Some(model), Source::Flag)
    } else if let Ok(model) = std::env::var("DINO_MODEL") {
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
                    NonZeroU32::new(100).expect("100 is non-zero"),
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
        protected: layers
            .pick(|file| file.protected.clone())
            .unwrap_or((Vec::new(), Source::Default)),
        max_diagnostics: layers
            .pick(|file| file.max_diagnostics.map(Some))
            .unwrap_or((None, Source::Default)),
        auto_compact: layers
            .pick(|file| file.auto_compact)
            .is_some_and(|(on, _)| on),
        fallback: layers
            .pick_user_only(|file| file.fallback.clone())
            .map(|(key, _)| key),
        thinking_budget: layers
            .pick(|file| file.thinking_budget.map(Some))
            .unwrap_or((None, Source::Default)),
        hooks: (
            hooks_for(&layers, settings::HookEvent::AfterEdit),
            Source::UserFile,
        ),
        end_turn_hooks: hooks_for(&layers, settings::HookEvent::BeforeEndTurn),
        start_hooks: hooks_for(&layers, settings::HookEvent::SessionStart),
        mcp: layers
            .pick_user_only(|file| file.mcp.clone())
            .map(|(servers, _)| {
                servers
                    .into_iter()
                    .map(|(name, server)| dino_exec::McpServer {
                        name,
                        command: server.command,
                        pass_env: server.pass_env,
                        read_only_tools: server.read_only_tools,
                        sandboxed: server.sandbox,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        layers,
    })
}

fn listed(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return "nothing".to_owned();
    }
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_config(args: &PromptArgs) -> Result<()> {
    let workspace = config::resolve_workspace(args.workspace.as_deref())?;
    let common = CommonArgs {
        model: args.model.clone(),
        ..CommonArgs::default()
    };
    let resolved = resolve(&common, &workspace)?;

    let show = |value: String, source: Source| format!("{value}  ({})", source.label());
    let fallback = resolved.fallback.as_deref().unwrap_or("none").to_owned();
    let rows = vec![
        (
            "model",
            show(
                resolved
                    .model
                    .0
                    .clone()
                    .unwrap_or_else(|| dino_model::DEFAULT_PROFILE.into()),
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
            "protected",
            show(listed(&resolved.protected.0), resolved.protected.1),
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
        ("fallback", fallback),
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
        protected: resolved.protected.0,
        max_diagnostics: resolved.max_diagnostics.0,
        hooks: resolved.hooks.0,
        end_turn_hooks: resolved.end_turn_hooks,
        start_hooks: resolved.start_hooks,
        mcp: resolved.mcp,
        thinking_budget: resolved.thinking_budget.0,
        compaction: if resolved.auto_compact {
            config::Compaction::Automatic
        } else {
            config::Compaction::Manual
        },
        show_reasoning: resolved.show_reasoning.0,
        api_base: resolved.api_base.0,
        disable_thinking: resolved.no_thinking.0,
        enable_thinking: None,
        plain: resolved.plain.0,
        fallback: resolved
            .fallback
            .as_deref()
            .map(|key| config::resolve_profile(Some(key)))
            .transpose()?,
    })
}

fn policy(mode: ApprovalMode, sandbox: &str, workspace: &Path) -> Box<dyn ApprovalPolicy> {
    match mode {
        ApprovalMode::Interactive => {
            let interactive = Interactive::new(TerminalApprover::new(sandbox, workspace));
            match settings::user_dir() {
                Some(dir) => Box::new(
                    interactive.with_store(approve::FileRuleStore::for_workspace(&dir, workspace)),
                ),
                None => Box::new(interactive),
            }
        }
        ApprovalMode::ReadOnly => Box::new(ReadOnly),
        ApprovalMode::AllowAll => Box::new(AllowAll),
    }
}

fn request_text(argument: Option<String>) -> Result<String> {
    if let Some(text) = argument {
        return Ok(text);
    }
    if stdin().is_terminal() {
        bail!("no request given: pass one as an argument, pipe it in, or run `dino` for a session");
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
