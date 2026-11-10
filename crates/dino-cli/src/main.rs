//! The `dino` executable.

mod approve;
mod ask;
mod calls;
mod commands;
mod config;
mod context;
mod diff;
mod evals;
mod expand;
mod greeting;
mod handover;
mod hooks;
mod keys;
mod listen;
mod markdown;
mod plugin;
mod preset;
mod probe;
mod recall;
mod remote;
mod render;
mod replay;
mod rewind;
mod search;
mod session;
mod settings;
mod shell;
mod spend;
mod spinner;
mod statusline;
mod style;
mod subagent;
mod summary;
mod task;
mod trace;
mod webhook;
mod worktree;

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
use dino_core::approval::Approver;
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
    Prompt(PrintPromptArgs),
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
    /// Show a past session's turns and calls with their inputs, outputs and
    /// durations.
    Trace(crate::trace::TraceArgs),
    /// Run a session with no terminal, driven from a web page over Tailscale or
    /// an SSH tunnel. RFC-0020.
    Serve(ServeArgs),
    /// Review pull requests as GitHub reports them ready, read-only, from a
    /// signed webhook. Configured under [webhook] in your own config file.
    Webhook(WebhookArgs),
    /// Show the palette and the opening drawing as this terminal draws them.
    Palette,
    /// Install, list, create and find plugins: bundles of skills, hooks, MCP
    /// servers and instructions.
    Plugin(plugin::command::PluginArgs),
}

// These are switches rather than a state machine because each one means one
// thing to a person reading `--help`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default, Clone, clap::Args)]
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
    /// Safety limit on model turns in one submission. Default 100.
    #[arg(long)]
    max_turns: Option<NonZeroU32>,
    /// Refuse every mutation and every process instead of asking.
    #[arg(long, conflicts_with = "dangerously_allow_all")]
    read_only: bool,
    /// Make edits to workspace files without asking; processes still ask.
    #[arg(long, conflicts_with_all = ["dangerously_allow_all", "read_only"])]
    accept_edits: bool,
    /// Approve everything without asking. Consider this carefully. Also
    /// spelled --dangerously-skip-permissions.
    #[arg(long, alias = "dangerously-skip-permissions")]
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
    /// Reasoning effort for a model that offers a choice: none, low, medium,
    /// high, xhigh or max. Leaves the model's default when absent.
    #[arg(long)]
    effort: Option<dino_protocol::ReasoningEffort>,
    /// See every submission through, as if each said "keep going". `/auto`
    /// switches it in a session.
    #[arg(long)]
    keep_going: bool,
    /// Continue a previous session by id or /rename name. Bare, the most recent
    /// in this workspace.
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,
    /// With --resume, carry on in a copy and leave the original as it was.
    #[arg(long, requires = "resume")]
    fork: bool,
    /// Start in a new git worktree under .dino/worktrees, on a branch of its
    /// own, removed at the end if nothing changed. Named, or a generated name.
    #[arg(long, num_args = 0..=1, default_missing_value = "", conflicts_with = "resume")]
    worktree: Option<String>,
    /// The tools offered and the limits started under: standard, minimal or
    /// review. Default standard.
    #[arg(long, value_enum)]
    preset: Option<preset::Preset>,
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
    /// The tools offered, as a run's: standard, minimal or review. Defaults to
    /// the user file, else standard.
    #[arg(long, value_enum)]
    preset: Option<preset::Preset>,
}

#[derive(Debug, clap::Args)]
struct PrintPromptArgs {
    #[command(flatten)]
    args: PromptArgs,
    /// Print an estimate of the tokens each part and each tool's schema takes
    /// instead of the prompt.
    #[arg(long)]
    tokens: bool,
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
    /// Write the run to standard output as JSON lines, the session log's
    /// records, in place of the transcript. The protocol in docs/sdk.md.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// Address to listen on: a loopback or Tailscale address, and nothing else.
    /// Defaults to this machine's Tailscale address, or localhost without one.
    #[arg(long)]
    bind: Option<std::net::IpAddr>,
    /// Port to listen on. Defaults to a free one, so several can run at once.
    #[arg(long, default_value_t = 0)]
    port: u16,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Debug, clap::Args)]
struct WebhookArgs {
    /// Address to listen on: a loopback or Tailscale address, and nothing else.
    /// Default localhost, for `tailscale funnel` or another proxy that ends
    /// HTTPS in front of it.
    #[arg(long)]
    bind: Option<std::net::IpAddr>,
    /// Port to listen on.
    #[arg(long, default_value_t = 7879)]
    port: u16,
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
        Some(Command::Prompt(PrintPromptArgs { args, tokens })) => {
            print_prompt(&args, tokens)?;
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
        Some(Command::Trace(args)) => trace::run(&args),
        Some(Command::Serve(args)) => on_runtime(serve(args)),
        Some(Command::Webhook(args)) => on_runtime(webhook(args)),
        Some(Command::Plugin(args)) => plugin::command::run(args),
        Some(Command::Palette) => {
            style::enable(false);
            print!("{}", greeting::palette());
            Ok(ExitCode::SUCCESS)
        }
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
    print!("{}", models_table(&builtin_profiles()));
    Ok(())
}

/// The model table, its key and provider columns sized to the widest row so
/// every line stays in step, however long a model key runs.
fn models_table(profiles: &[dino_protocol::ModelProfile]) -> String {
    use std::fmt::Write as _;
    let key_width = profiles.iter().map(|p| p.key.len()).max().unwrap_or(0);
    let provider_width = profiles
        .iter()
        .map(|p| format!("{:?}", p.provider).len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for profile in profiles {
        let note = match profile.status {
            ProfileStatus::Supported => "",
            ProfileStatus::Development => "  (development)",
        };
        let _ = writeln!(
            out,
            "{:<key_width$} {:<provider_width$} {}{note}",
            profile.key,
            format!("{:?}", profile.provider).to_lowercase(),
            profile.provider_model,
        );
    }
    out
}

fn print_prompt(args: &PromptArgs, tokens: bool) -> Result<()> {
    let workspace = config::resolve_workspace(args.workspace.as_deref())?;
    // Resolve the model and preset the layered way a session does, so the
    // printed prompt is the one a run would send, not a default.
    let common = CommonArgs {
        model: args.model.clone(),
        workspace: args.workspace.clone(),
        preset: args.preset,
        ..CommonArgs::default()
    };
    let resolved = resolve(&common, &workspace)?;
    let profile = config::resolve_profile(resolved.model.0.as_deref())?;
    let preset = resolved.preset.0;
    let sandbox = config::build_sandbox(args.sandbox, &workspace, &[])?;
    let tools = tool_set(
        &workspace,
        sandbox,
        &interactive_options(&workspace, Vec::new(), None, Vec::new(), None, None),
        vec![Box::new(dino_core::ask::AskTool::unattended())],
    )?
    .keeping(|name| preset.offers(name));
    let context = prompt_context(
        &workspace,
        ApprovalMode::Interactive,
        &tools.definitions(),
        None,
        &profile.key,
    );
    let prompt = system_prompt(&context);
    eprintln!(
        "model {} · preset {} · {} bytes of system prompt",
        profile.key,
        preset.name(),
        prompt.len()
    );
    if tokens {
        let parts = dino_core::prompt::prompt_parts(&context);
        let report = context::Report {
            window: profile.context_window_tokens,
            groups: context::fixed(&parts, &tools.definitions()),
            ..context::Report::default()
        };
        print!("{}", report.render());
        return Ok(());
    }
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
/// before-end-turn hooks, and for a run nobody watches, a clean build and the
/// workspace's verify command.
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
    if unattended && let Some(verify) = verify_check(settings, sandbox) {
        checks.push(Box::new(verify));
    }
    Ok(checks)
}

fn verify_check(settings: &RunSettings, sandbox: &Arc<dyn Sandbox>) -> Option<dino_exec::Verify> {
    let command = settings.verify.clone()?;
    Some(dino_exec::Verify::new(
        command,
        &settings.workspace,
        Arc::clone(sandbox),
    ))
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
        .map(|(command, text)| {
            dino_core::prompt::Instructions::whole(format!("session_start: {command}"), text)
        })
        .collect()
}

/// Everything a person's session can call, MCP servers included.
fn session_tool_set(
    settings: &RunSettings,
    sandbox: &Arc<dyn Sandbox>,
    extra: Vec<Box<dyn ToolRuntime>>,
) -> Result<ToolSet> {
    let mut mcp = if settings.preset.connects_mcp() {
        connect_mcp(&settings.mcp, &settings.workspace, sandbox)
    } else {
        Vec::new()
    };
    mcp.extend(extra);
    let preset = settings.preset;
    tool_set(
        &settings.workspace,
        Arc::clone(sandbox),
        &dino_eval::ToolOptions {
            after_call: settings.call_hooks.after_call.clone(),
            ..interactive_options(
                &settings.workspace,
                settings.protected.clone(),
                settings.max_diagnostics,
                settings.hooks.clone(),
                settings.verify.clone(),
                settings.web_search.backend(),
            )
        },
        mcp,
    )
    .map(|tools| tools.keeping(|name| preset.offers(name)))
}

/// A subagent's tools: the built-in ones under its settings, no MCP server
/// and nothing a mode of the parent's added.
fn child_tools(settings: &RunSettings, sandbox: &Arc<dyn Sandbox>) -> Result<ToolSet> {
    tool_set(
        &settings.workspace,
        Arc::clone(sandbox),
        &dino_eval::ToolOptions {
            after_call: settings.call_hooks.after_call.clone(),
            ..interactive_options(
                &settings.workspace,
                settings.protected.clone(),
                settings.max_diagnostics,
                settings.hooks.clone(),
                settings.verify.clone(),
                None,
            )
        },
        Vec::new(),
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
    model: &str,
) -> PromptContext {
    PromptContext {
        workspace_root: workspace.display().to_string(),
        model: Some(model.to_owned()),
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
        instructions: plugin::active()
            .iter()
            .filter_map(plugin::Active::instructions)
            .chain(dino_core::prompt::load_instructions(
                settings::user_instructions_path().as_deref(),
                workspace,
            ))
            .collect(),
    }
}

/// The tools a person's own session gets, as opposed to an eval's.
fn interactive_options(
    workspace: &Path,
    protected: Vec<PathBuf>,
    max_diagnostics: Option<usize>,
    hooks: Vec<dino_exec::Hook>,
    verify: Option<Vec<String>>,
    search: Option<dino_exec::SearchBackend>,
) -> dino_eval::ToolOptions {
    dino_eval::ToolOptions {
        protected,
        max_diagnostics,
        hooks,
        after_call: Vec::new(),
        memory: memory_files(workspace),
        skills: skills(workspace),
        github: dino_exec::GithubTool::available(),
        web: true,
        search,
        code: true,
        skip_edit_check: false,
        escalation: true,
        verify,
    }
}

/// The user's skills, then the plugins', then the repository's, then those
/// built in; the first to claim a name keeps it.
fn skills(workspace: &Path) -> Vec<dino_core::skills::Skill> {
    let mut dirs: Vec<PathBuf> = settings::user_dir()
        .map(|dir| dir.join("skills"))
        .into_iter()
        .collect();
    dirs.extend(plugin::active().iter().map(plugin::Active::skills_dir));
    dirs.push(workspace.join(".dino").join("skills"));
    let mut found = dino_core::skills::discover(&dirs);
    if let Some(skill) = dino_core::skills::builtin(plugin::PLUGIN_DEV_SKILL)
        && found.iter().all(|known| known.name != skill.name)
    {
        found.push(skill);
    }
    found
}

/// The plugins' commands, then yours, then the workspace's, each shadowing
/// the last; at a terminal, says what was shadowed or left out.
fn custom_commands(workspace: &Path, announce: bool) -> commands::Commands {
    let mut dirs: Vec<(commands::Source, PathBuf)> = plugin::active()
        .iter()
        .map(|plugin| {
            (
                commands::Source::Plugin(plugin.name().to_owned()),
                plugin.commands_dir(),
            )
        })
        .collect();
    dirs.extend(settings::user_dir().map(|dir| (commands::Source::User, dir.join("commands"))));
    dirs.push((
        commands::Source::Workspace,
        workspace.join(".dino").join("commands"),
    ));
    let loaded = commands::load(&dirs, session::is_builtin);
    if announce {
        for note in &loaded.notes {
            eprintln!(
                "{}",
                style::paint(style::WARN, &format!("commands: {note}"))
            );
        }
    }
    loaded.commands.register();
    loaded.commands
}

/// The user's memory file and this workspace's, beside the user config.
fn memory_files(workspace: &Path) -> Option<dino_core::memory::MemoryFiles> {
    let dir = settings::user_dir()?.join("memory");
    let name = worktree::home(workspace)
        .display()
        .to_string()
        .replace('/', "%");
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
                format!(
                    "{source} (cut at {} of {} bytes)",
                    render::compact(item.text.len() as u64),
                    render::compact(item.bytes as u64)
                )
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
    context: PromptContext,
    definitions: Vec<ToolDefinition>,
    requests: Arc<std::sync::atomic::AtomicU64>,
    planning: Arc<std::sync::atomic::AtomicBool>,
    context_full: Arc<std::sync::atomic::AtomicBool>,
    status: Arc<std::sync::Mutex<spinner::Status>>,
    sandbox: Arc<dyn Sandbox>,
    spending: spend::Spending,
    /// Set while someone is there to answer the model's questions.
    attended: Arc<std::sync::atomic::AtomicBool>,
}

fn prompt_hooks(settings: &RunSettings, sandbox: &Arc<dyn Sandbox>) -> dino_exec::PromptHooks {
    dino_exec::PromptHooks::new(
        settings.call_hooks.prompt_submit.clone(),
        &settings.workspace,
        Arc::clone(sandbox),
    )
}

impl Prepared {
    /// A fresh session for `settings`, priced into the same meter.
    pub(crate) fn build_session(&self, settings: &RunSettings) -> Result<Box<dyn ModelSession>> {
        metered(
            settings,
            self.system_prompt.clone(),
            self.definitions.clone(),
            &self.spending,
        )
    }
}

fn metered(
    settings: &RunSettings,
    prompt: String,
    definitions: Vec<ToolDefinition>,
    spending: &spend::Spending,
) -> Result<Box<dyn ModelSession>> {
    let model = config::build_session(settings, prompt, definitions)?;
    let asks = settings
        .web_search
        .switch(settings.profile.provider)
        .is_some();
    let model: Box<dyn ModelSession> = match &settings.web_search {
        search::WebSearch::Native(gate) if asks => {
            Box::new(search::Gated::new(model, Arc::clone(gate)))
        }
        _ => model,
    };
    Ok(Box::new(spend::Metered::new(model, spending)))
}

/// `agent` with what must hold before it ends a run: the finish checks, and
/// the review when one is configured.
fn guarded(
    agent: Agent<Box<dyn ModelSession>, ToolSet>,
    settings: &RunSettings,
    sandbox: &Arc<dyn Sandbox>,
    unattended: bool,
    spending: &spend::Spending,
) -> Result<Agent<Box<dyn ModelSession>, ToolSet>> {
    let checks = finish_checks(settings, sandbox, unattended)?;
    let agent = if checks.is_empty() {
        agent
    } else {
        agent.with_finish_check(dino_core::FirstUnfinished::new(checks))
    };
    let agent = match verify_check(settings, sandbox).filter(|_| !unattended) {
        Some(verify) => agent.with_task_check(verify),
        None => agent,
    };
    Ok(match &settings.review {
        Some(review) => agent.with_reviewer(Arc::new(dino_core::review::Reviewer::new(
            settings.workspace.clone(),
            review_sessions(settings, review, spending)?,
        ))),
        // Without a review model, a task the person said to finish is reviewed
        // by the model running it, which sees only the task and the diff.
        None => agent.with_task_reviewer(Arc::new(dino_core::review::Reviewer::new(
            settings.workspace.clone(),
            review_sessions(settings, &settings.profile, spending)?,
        ))),
    })
}

/// The reviewer's sessions, priced into the session's meter.
fn review_sessions(
    settings: &RunSettings,
    review: &dino_protocol::ModelProfile,
    spending: &spend::Spending,
) -> Result<dino_core::review::OpenSession> {
    let settings = config::review_settings(settings, review)?;
    let spending = spending.clone();
    Ok(Box::new(move |system| {
        metered(&settings, system, Vec::new(), &spending).map_err(|error| error.to_string())
    }))
}

/// `unattended` is for a one-shot run, which has no person to hand an unfinished
/// build back to.
fn warn_unprotected(settings: &RunSettings, sandbox: &dyn Sandbox) {
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
}

/// Where a run is shown: the terminal, or with `json` standard output as the
/// session log's records.
fn shown(
    json: bool,
    renderer: Renderer,
    meta: SessionMeta,
) -> Result<Box<dyn dino_core::session::EventSink>> {
    Ok(if json {
        Box::new(JsonlSessionLog::to_writer(
            std::io::stdout(),
            "standard output",
            meta,
        )?)
    } else {
        Box::new(renderer)
    })
}

/// The `ask` tool, asking on the pages when there are any, and the switch
/// that says whether anyone is there.
fn ask_tool(
    attended: bool,
    remote: Option<&Arc<remote::Hub>>,
) -> (Arc<std::sync::atomic::AtomicBool>, Box<dyn ToolRuntime>) {
    let attended = Arc::new(std::sync::atomic::AtomicBool::new(attended));
    let asker: Arc<dyn dino_core::ask::Asker> = match remote {
        Some(hub) => Arc::new(remote::RemoteAsker::new(Arc::clone(hub))),
        None => Arc::new(ask::TerminalAsker),
    };
    let tool = dino_core::ask::AskTool::new(asker, Arc::clone(&attended));
    (attended, Box::new(tool))
}

/// Names how the session searches, and gives the provider's search the
/// policy that decides whether it runs.
fn open_search(settings: &RunSettings, sandbox: &str, remote: Option<&Arc<remote::Hub>>) {
    if let search::WebSearch::Native(gate) = &settings.web_search {
        gate.decide_with(policy(
            settings.mode,
            sandbox,
            &settings.workspace,
            remote,
            &settings.call_hooks,
        ));
    }
    if !matches!(settings.web_search, search::WebSearch::Off) {
        eprintln!(
            "{}",
            style::paint(
                style::FAINT,
                &format!("web search: {}", settings.web_search.label())
            )
        );
    }
}

/// Restoring before the first turn, so the conversation continues rather than
/// starting. A mismatch refuses here rather than confusing the provider later.
fn restore(
    common: &CommonArgs,
    settings: &RunSettings,
    session_id: &SessionId,
    model: &mut dyn ModelSession,
) -> Result<PathBuf> {
    let workspace = &settings.workspace;
    let _ = dino_core::checkpoint::prune(workspace, dino_core::checkpoint::KEPT_FOR);
    let Some((id, path)) = resume_from(common.resume.as_deref(), workspace)? else {
        return Ok(resume::state_path(workspace, session_id.as_str()));
    };
    let state = resume::load(&path)?;
    model.restore_state(&state)?;
    if !common.fork {
        eprintln!(
            "{}",
            style::paint(style::FAINT, &format!("resumed session {id}"))
        );
        return Ok(path);
    }
    // With --fork, a copy of it, and the original left as it was.
    let child = session_id.as_str();
    resume::fork(workspace, &id, child, &state, None)?;
    eprintln!(
        "{}",
        style::paint(style::FAINT, &format!("forked session {id} as {child}"))
    );
    Ok(resume::state_path(workspace, child))
}

fn prepare(
    common: &CommonArgs,
    unattended: bool,
    mut extra: Vec<Box<dyn ToolRuntime>>,
    remote: Option<&Arc<remote::Hub>>,
    json: bool,
) -> Result<Prepared> {
    let session_id = SessionId::generate();
    let settings = settings(common, &session_id)?;
    // After resolution rather than before, so a config file can ask for plain
    // output as well as the flag.
    style::enable(settings.plain);

    let sandbox =
        config::build_sandbox(settings.sandbox, &settings.workspace, &settings.protected)?;
    let sandbox_label = sandbox.label();
    warn_unprotected(&settings, sandbox.as_ref());
    let (attended, ask) = ask_tool(!unattended && !settings.keep_going, remote);
    extra.push(ask);
    open_search(&settings, &sandbox_label, remote);
    let (delegate, kit) = subagent::Delegate::new();
    extra.push(Box::new(delegate));
    let tools = session_tool_set(&settings, &sandbox, extra)?;
    let definitions = tools.definitions();
    let mut context = prompt_context(
        &settings.workspace,
        settings.mode,
        &definitions,
        Some(settings.max_turns.get()),
        &settings.profile.key,
    );
    context
        .instructions
        .extend(start_hook_output(&settings, &sandbox));
    announce_instructions(&context);
    let prompt = system_prompt(&context);
    let spending = spend::Spending {
        meter: Arc::new(
            spend::Meter::for_session(session_id.as_str()).with_limits(settings.task_limits),
        ),
        cap: settings
            .budget
            .map(|budget| Arc::new(spend::Cap::new(budget, Box::new(approve::TerminalPurse)))),
    };
    let mut model = metered(&settings, prompt.clone(), definitions.clone(), &spending)?;

    let meta = SessionMeta {
        session_id: session_id.clone(),
        dino_version: env!("CARGO_PKG_VERSION").to_owned(),
        model_key: settings.profile.key.clone(),
        provider: settings.profile.provider,
        workspace_root: settings.workspace.display().to_string(),
        approval_mode: settings.mode.label().to_owned(),
        sandbox: sandbox_label.clone(),
        max_model_turns: settings.max_turns.get(),
    };
    let log = JsonlSessionLog::create(&settings.log_path, meta.clone())?;

    let state_path = restore(common, &settings, &session_id, model.as_mut())?;

    let renderer = Renderer::new(settings.show_reasoning);
    let usage = renderer.usage_handle();
    let requests = renderer.requests_handle();
    let context_full = renderer.context_handle();
    let status = renderer.status_handle();
    if let Ok(mut status) = status.lock() {
        status.answering(&settings.profile);
        status.meter = Some(Arc::clone(&spending.meter));
        status.line = statusline::StatusLine::configured(&settings);
    }
    let hold = renderer.hold_handle();
    let shown = shown(json, renderer, meta)?;
    let planning = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let agent = Agent::new(
        model,
        tools,
        AgentConfig {
            max_model_turns: settings.max_turns,
            ..AgentConfig::default()
        },
    )
    .with_policy(gated_policy(
        &settings,
        &sandbox_label,
        &sandbox,
        remote,
        &planning,
    ))
    .with_sink(((shown, log), remote::Tap(remote.map(Arc::clone))));
    let agent = guarded(agent, &settings, &sandbox, unattended, &spending)?;

    let prepared = Prepared {
        agent,
        state_path,
        usage,
        settings,
        sandbox_label,
        session_id,
        system_prompt: prompt,
        context,
        definitions,
        requests,
        planning,
        context_full,
        status,
        sandbox,
        spending,
        attended,
    };
    let _ = kit.set(subagent_kit(&prepared, remote, hold));
    Ok(prepared)
}

/// What the session's `agent` tool builds children from.
fn subagent_kit(
    prepared: &Prepared,
    remote: Option<&Arc<remote::Hub>>,
    hold: spinner::Hold,
) -> subagent::Kit {
    let spending = prepared.spending.clone();
    let sandbox = Arc::clone(&prepared.sandbox);
    subagent::Kit {
        settings: prepared.settings.clone(),
        session_id: prepared.session_id.clone(),
        sandbox: Arc::clone(&prepared.sandbox),
        context: prepared.context.clone(),
        status: Arc::clone(&prepared.status),
        open: Box::new(move |settings, prompt, definitions| {
            metered(settings, prompt, definitions, &spending)
        }),
        tools: Box::new(move |settings| child_tools(settings, &sandbox)),
        policy: child_policy(
            &prepared.sandbox_label,
            &prepared.sandbox,
            &prepared.settings.workspace,
            remote,
            hold,
            Arc::clone(&prepared.planning),
            prepared.settings.call_hooks.clone(),
        ),
    }
}

async fn session(mut common: CommonArgs) -> Result<ExitCode> {
    // Each is tidied as Dino exits, so a session can still go back to the last.
    let mut made: Vec<worktree::Worktree> = enter_worktree(&mut common)?.into_iter().collect();
    // Where to go back to when a session will not start in a new worktree.
    let mut previous: Option<CommonArgs> = None;
    let ended = loop {
        match drive(common.clone(), None).await {
            Ok(Next::Worktree(next)) => {
                announce_worktree(&next);
                let mut moved = common.clone();
                moved.workspace = Some(next.workspace.clone());
                moved.resume = None;
                moved.log = None;
                made.push(next);
                previous = Some(std::mem::replace(&mut common, moved));
            }
            Ok(_) => break Ok(ExitCode::SUCCESS),
            Err(error) => match previous.take() {
                Some(back) => {
                    eprintln!("dino: {error:#}");
                    common = back;
                }
                None => break Err(error),
            },
        }
    };
    made.iter().for_each(leave);
    ended
}

/// Makes the worktree `--worktree` asks for and points the session at it.
fn enter_worktree(common: &mut CommonArgs) -> Result<Option<worktree::Worktree>> {
    let Some(name) = common.worktree.take() else {
        return Ok(None);
    };
    let workspace = config::resolve_workspace(common.workspace.as_deref())?;
    let made = worktree::Worktree::create(&workspace, &name)?;
    announce_worktree(&made);
    common.workspace = Some(made.workspace.clone());
    Ok(Some(made))
}

fn announce_worktree(worktree: &worktree::Worktree) {
    eprintln!(
        "{}",
        style::paint(
            style::FAINT,
            &format!(
                "worktree {} on branch {}",
                render::short_path(&worktree.root, Path::new("")),
                worktree.branch
            )
        )
    );
}

/// Removes a worktree nothing changed in, and says where one that did is kept.
fn leave(worktree: &worktree::Worktree) {
    let path = render::short_path(&worktree.root, Path::new(""));
    match worktree.finish() {
        Ok(worktree::Ended::Removed) => eprintln!(
            "{}",
            style::paint(
                style::FAINT,
                &format!("  removed worktree {path}: nothing changed")
            )
        ),
        Ok(worktree::Ended::Kept) => eprintln!(
            "{}",
            style::paint(
                style::PURPLE,
                &format!("  kept worktree {path} on branch {}", worktree.branch)
            )
        ),
        Err(error) => eprintln!(
            "{}",
            style::paint(
                style::WARN,
                &format!("  could not tidy worktree {path}: {error:#}")
            )
        ),
    }
}

/// What `dino serve` adds to a session: the hub the pages talk to, and what
/// they ask of it.
struct Remote {
    hub: Arc<remote::Hub>,
    requests: tokio::sync::mpsc::Receiver<remote::Request>,
}

/// Listens for pages, then runs a session for them until the person at the
/// terminal interrupts it.
async fn serve(args: ServeArgs) -> Result<ExitCode> {
    if args.common.worktree.is_some() {
        bail!("--worktree is for `dino` and `dino run`, not `dino serve`");
    }
    let address = remote::bind_address(args.bind, args.port)?;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("cannot listen on {address}"))?;
    let address = listener.local_addr().context("cannot read the address")?;
    if address.ip().is_loopback() {
        let port = address.port();
        eprintln!("dino: from another machine: ssh -L {port}:localhost:{port} <this machine>");
    }
    let token = remote::new_token()?;
    let (hub, requests) = remote::Hub::new();
    hub.offer(remote::Workspaces {
        configured: settings::configured_workspaces()?,
        recent: settings::recent_workspaces_path(),
    });
    // The token is in the fragment, which a browser keeps to itself.
    eprintln!(
        "{}",
        style::paint(style::PURPLE, &format!("remote: http://{address}/#{token}"))
    );
    eprintln!(
        "{}",
        style::paint(
            style::FAINT,
            "the token is the only key: anyone who has it can drive this session"
        )
    );
    let server = tokio::spawn(remote::serve(listener, Arc::clone(&hub), token));
    let mut remote = Remote {
        hub: Arc::clone(&hub),
        requests,
    };
    let mut common = args.common;
    // Where to go back to when the workspace a page picked will not start.
    let mut previous: Option<CommonArgs> = None;
    let ended = loop {
        match drive(common.clone(), Some(&mut remote)).await {
            Ok(Next::Workspace(workspace)) => {
                hub.notice(&format!("moving to {}", workspace.display()));
                let mut next = common.clone();
                next.workspace = Some(workspace);
                next.resume = None;
                next.log = None;
                previous = Some(std::mem::replace(&mut common, next));
            }
            Ok(_) => break Ok(ExitCode::SUCCESS),
            Err(error) => match previous.take() {
                Some(back) => {
                    eprintln!("dino: {error:#}");
                    hub.notice(&format!("could not start there: {error:#}"));
                    common = back;
                }
                None => break Err(error),
            },
        }
    };
    server.abort();
    ended
}

/// Reviews waiting their turn. Past it, a delivery is told to come back.
const WEBHOOK_QUEUE: usize = 4;

/// Listens for GitHub, and reviews what it admits one at a time until
/// interrupted.
async fn webhook(args: WebhookArgs) -> Result<ExitCode> {
    let layers = Layers::user_only()?;
    let config = webhook::Config::resolve(
        layers.user.webhook.as_ref(),
        layers.user.review_model.clone(),
        &|name| std::env::var(name).ok(),
    )?;
    for (repo, clone) in &config.repos {
        if !clone.join(".git").exists() {
            bail!("{repo}: {} is not a git clone", clone.display());
        }
    }
    let requested = args
        .bind
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let address = remote::bind_address(Some(requested), args.port)?;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("cannot listen on {address}"))?;
    let address = listener.local_addr().context("cannot read the address")?;
    let repos = config.repos.keys().cloned().collect::<Vec<_>>().join(", ");
    eprintln!(
        "{}",
        style::paint(
            style::PURPLE,
            &format!("webhook: http://{address}/github for {repos}")
        )
    );
    eprintln!(
        "{}",
        style::paint(
            style::FAINT,
            if config.post {
                "reviews are recorded and posted as pull request comments"
            } else {
                "reviews are recorded, not posted; set webhook.post to post them"
            }
        )
    );
    let config = Arc::new(config);
    let gate = Arc::new(std::sync::Mutex::new(webhook::Gate::new(config.per_hour)));
    let (jobs, mut queue) = tokio::sync::mpsc::channel(WEBHOOK_QUEUE);
    let server = tokio::spawn(webhook::serve(
        listener,
        Arc::clone(&config),
        Arc::clone(&gate),
        jobs,
    ));
    loop {
        let job = tokio::select! {
            job = queue.recv() => job,
            _ = tokio::signal::ctrl_c() => None,
        };
        let Some(job) = job else { break };
        match webhook::review(&job, &config).await {
            Ok(record) => eprintln!(
                "dino: webhook: reviewed {}#{} at {}: {}",
                job.repo,
                job.number,
                job.short_head(),
                record.display()
            ),
            Err(error) => {
                // Forget it so a redelivery is taken; a failed fetch, outage or
                // full disk should not wedge the head until restart.
                gate.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .forget(&job);
                eprintln!(
                    "{}",
                    style::paint(
                        style::WARN,
                        &format!(
                            "dino: webhook: {}#{} at {}: {error:#}",
                            job.repo,
                            job.number,
                            job.short_head()
                        )
                    )
                );
            }
        }
    }
    server.abort();
    Ok(ExitCode::SUCCESS)
}

/// The workspace's schedules. Only a session that stays open can be sent what
/// comes due, so a one-shot run, a handover, a review and an eval never get them.
fn schedules(common: &CommonArgs) -> Result<(session::Due, Box<dyn ToolRuntime>)> {
    let workspace = config::resolve_workspace(common.workspace.as_deref())?;
    let due = session::Due {
        store: dino_core::schedule::Schedules::in_workspace(&workspace),
        conversation: dino_core::schedule::Conversation::default(),
        clock: dino_core::schedule::system_clock(),
        reported: None,
    };
    let tools =
        dino_core::schedule::ScheduleTools::new(due.store.clone(), Arc::clone(&due.conversation));
    Ok((due, Box::new(tools)))
}

/// Opens a fresh conversation on another model, for `/model` and `/clear`.
fn model_factory(prepared: &Prepared) -> session::SessionFactory<Box<dyn ModelSession>> {
    let base = prepared.settings.clone();
    let context = prepared.context.clone();
    let definitions = prepared.definitions.clone();
    let spending = prepared.spending.clone();
    Box::new(move |key| {
        let mut settings = base.clone();
        settings.profile = config::resolve_profile(Some(key))?;
        // Switching to a model without the configured effort is not a refusal.
        settings.reasoning_effort = settings
            .reasoning_effort
            .filter(|effort| config::check_effort(&settings.profile, *effort).is_ok());
        let name = settings.profile.display_name.clone();
        // The prompt names the model, so a new conversation gets its own.
        let mut context = context.clone();
        context.model = Some(key.to_owned());
        let prompt = system_prompt(&context);
        let model = metered(&settings, prompt, definitions.clone(), &spending)?;
        Ok((model, name))
    })
}

/// Where a session went when it ended.
enum Next {
    Leave,
    /// A page picked another workspace.
    Workspace(PathBuf),
    /// `/worktree` made one.
    Worktree(worktree::Worktree),
}

/// Runs one session, and returns where it is to move, if anywhere. A move
/// starts a new session there, with that workspace's schedules.
async fn drive(common: CommonArgs, remote: Option<&mut Remote>) -> Result<Next> {
    let hub = remote.as_ref().map(|remote| Arc::clone(&remote.hub));
    let (due, tools) = schedules(&common)?;
    let prepared = prepare(&common, false, vec![tools], hub.as_ref(), false)?;
    // Kept only when the preset offers the tools.
    let schedules = prepared
        .definitions
        .iter()
        .any(|definition| definition.name == dino_core::schedule::CREATE)
        .then_some(due);
    if let Some(file) = settings::recent_workspaces_path() {
        let _ = remote::note_workspace(&file, &prepared.settings.workspace);
    }
    if hub.is_some() && matches!(prepared.settings.mode, ApprovalMode::AllowAll) {
        eprintln!(
            "{}",
            style::paint(
                style::WARN,
                "approvals are off, so a page may run anything the sandbox allows"
            )
        );
    }
    let factory = model_factory(&prepared);
    let inbox = hub
        .as_ref()
        .map_or_else(dino_core::Inbox::default, |hub| hub.inbox());
    let commands = custom_commands(&prepared.settings.workspace, hub.is_none());
    let mut session = Session {
        agent: prepared.agent.with_inbox(std::sync::Arc::clone(&inbox)),
        inbox,
        typed_ahead: String::new(),
        queued: std::collections::VecDeque::new(),
        usage: prepared.usage,
        requests: prepared.requests,
        meter: Arc::clone(&prepared.spending.meter),
        factory,
        model_key: prepared.settings.profile.key.clone(),
        workspace_root: prepared.settings.workspace.clone(),
        run_id: prepared.session_id.as_str().to_owned(),
        conversations: 0,
        planning: prepared.planning,
        awaiting_plan: false,
        carried: None,
        recall: crate::recall::Recall::default(),
        compaction: prepared.settings.compaction,
        keep_going: prepared.settings.keep_going,
        proposed: false,
        max_turns: prepared.settings.max_turns.get(),
        context_full: prepared.context_full,
        status: prepared.status,
        model: prepared.settings.profile.display_name.clone(),
        workspace: prepared.settings.workspace.display().to_string(),
        sandbox: prepared.sandbox_label,
        approvals: prepared.settings.mode.label().to_owned(),
        log_path: prepared.settings.log_path.display().to_string(),
        end_hooks: prepared.settings.task_end_hooks.clone(),
        prompt_hooks: prompt_hooks(&prepared.settings, &prepared.sandbox),
        history_path: prepared.settings.workspace.join(".dino").join("history"),
        name: resume::label(&prepared.state_path)
            .ok()
            .and_then(|label| label.name),
        rewinding: None,
        state_path: prepared.state_path,
        greeting: greeting::shows(
            prepared.settings.greeting,
            style::enabled(),
            stdin().is_terminal(),
        ),
        remote: hub,
        schedules,
        commands,
        prompt: prepared.context,
        ran: Vec::new(),
        web_search: prepared.settings.web_search.clone(),
        attended: prepared.attended,
        worktree: worktree::describe(&prepared.settings.workspace),
        moving_to: None,
    };
    session.note_conversation();
    let moving = if let Some(remote) = remote {
        session
            .serve(&mut remote.requests)
            .await?
            .map_or(Next::Leave, Next::Workspace)
    } else {
        session.run().await?;
        session.moving_to.take().map_or(Next::Leave, Next::Worktree)
    };
    eprintln!(
        "{}",
        style::paint(
            style::FAINT,
            &format!("  {}", prepared.spending.meter.closing())
        )
    );
    Ok(moving)
}

/// The largest diff put into a review request; past it the model reads files.
const MAX_REVIEW_DIFF_BYTES: usize = 200 * 1_024;

/// Runs a read-only review. The diff is taken here and handed over, so the
/// model needs no process and the session can refuse every change.
async fn review(mut args: ReviewArgs) -> Result<ExitCode> {
    if args.common.worktree.is_some() {
        bail!("--worktree is for `dino` and `dino run`, not `dino review`");
    }
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
    args.common.preset.get_or_insert(preset::Preset::Review);
    run(RunArgs {
        prompt: Some(request),
        handover: false,
        // Read only with --handover, which a review never uses.
        session_turns: 0,
        json: false,
        common: args.common,
    })
    .await
}

/// Starts the clock on a one-shot run's task, and says when the message set its
/// limits. Returns the request, with a note added when the branch is behind
/// and what the `prompt_submit` hooks added, or an error when one refused it.
async fn begin_task(prepared: &mut Prepared, request: String) -> Result<String> {
    let added = hooks::submitted(
        &prompt_hooks(&prepared.settings, &prepared.sandbox),
        &request,
    )
    .await
    .map_err(|reason| anyhow::anyhow!("not sent: {reason}"))?;
    let (limits, named) = prepared.spending.meter.begin_task(&request);
    if named {
        eprintln!(
            "{}",
            style::paint(
                style::FAINT,
                &format!("  task budget: {}", limits.describe())
            )
        );
    }
    // A read-only run is a review or a question, not a task to see through.
    let finishing = (prepared.settings.keep_going || task::wants_to_finish(&request))
        && !matches!(prepared.settings.mode, ApprovalMode::ReadOnly);
    prepared.agent.set_keep_going(finishing);
    let request = if finishing {
        task::note_if_behind(&prepared.settings.workspace, request).await
    } else {
        request
    };
    Ok(dino_exec::with_context(request, &added))
}

/// Ends the task a handover chain ran and pings for it.
async fn conclude_handover(prepared: &Prepared, ended: &Result<handover::Ended>) {
    let outcome = match ended {
        Ok(handover::Ended::Stopped(StopReason::EndTurn)) => task::Outcome {
            status: "finished",
            detail: String::new(),
        },
        Ok(handover::Ended::Stopped(StopReason::Cancelled)) => task::Outcome {
            status: "cancelled",
            detail: String::new(),
        },
        Ok(handover::Ended::OverBudget(reason)) => task::Outcome {
            status: "stopped",
            detail: reason.clone(),
        },
        Ok(_) => task::Outcome {
            status: "stopped",
            detail: "the plan is unfinished".to_owned(),
        },
        Err(error) => task::Outcome {
            status: "failed",
            detail: error.to_string(),
        },
    };
    if let Some(elapsed) = prepared.spending.meter.task().end() {
        task::ping(
            &prepared.settings.task_end_hooks,
            elapsed,
            &prepared.settings.workspace,
            &prepared.settings.profile.key,
            prepared.spending.meter.spent(),
            &outcome,
        )
        .await;
    }
}

/// Ends a one-shot run's task. When a limit stopped it, asks once what is left
/// and returns that answer in place of the error, with the reason.
async fn conclude_run(
    prepared: &mut Prepared,
    result: Result<dino_core::RunResult, dino_core::AgentError>,
    cancel: &CancellationToken,
) -> (
    Result<dino_core::RunResult, dino_core::AgentError>,
    Option<String>,
) {
    let stopped = result
        .is_err()
        .then(|| prepared.spending.meter.task().stopped())
        .flatten();
    let mut result = result;
    if let Some(reason) = &stopped {
        eprintln!(
            "{}",
            style::paint(style::WARN, &format!("  stopped: {reason}"))
        );
        if let Some(run) = task::wrap_up(
            &mut prepared.agent,
            &prepared.spending.meter,
            &prepared.planning,
            reason,
            cancel,
        )
        .await
        {
            result = Ok(run);
        }
    }
    task::conclude(
        &prepared.settings.task_end_hooks,
        &prepared.settings.workspace,
        &prepared.settings.profile.key,
        &prepared.spending.meter,
        &result,
        stopped.clone(),
    )
    .await;
    (result, stopped)
}

async fn run(mut args: RunArgs) -> Result<ExitCode> {
    let worktree = enter_worktree(&mut args.common)?;
    let ran = run_in(args).await;
    worktree.iter().for_each(leave);
    ran
}

async fn run_in(args: RunArgs) -> Result<ExitCode> {
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
    let mut prepared = prepare(&args.common, true, extra, None, args.json)?;
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

    let request = begin_task(&mut prepared, request).await?;
    if let Some(progress) = &progress {
        let ended = handover::run(
            &mut prepared,
            progress,
            &request,
            args.session_turns,
            &cancel,
        )
        .await;
        conclude_handover(&prepared, &ended).await;
        eprintln!(
            "{}",
            style::paint(
                style::FAINT,
                &format!("  {}", prepared.spending.meter.closing())
            )
        );
        return Ok(match ended? {
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
            handover::Ended::OverBudget(reason) => {
                eprintln!(
                    "{}",
                    style::paint(style::WARN, &format!("  stopped: {reason}"))
                );
                ExitCode::FAILURE
            }
            _ => ExitCode::FAILURE,
        });
    }
    let result = prepared.agent.run_cancellable(request, &cancel).await;
    let (result, stopped) = conclude_run(&mut prepared, result, &cancel).await;
    eprintln!(
        "{}",
        style::paint(
            style::FAINT,
            &format!("  {}", prepared.spending.meter.closing())
        )
    );
    let result = result?;
    save_state(&prepared.agent, &prepared.state_path)?;
    if stopped.is_some() {
        return Ok(ExitCode::FAILURE);
    }
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
    match resume::find(workspace, requested)? {
        Some(found) => Ok(Some(found)),
        None => bail!("no session {requested:?} in {}", workspace.display()),
    }
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
        Ok(state) => {
            // A resumed conversation keeps the name it was given.
            let name = resume::label(path).ok().and_then(|label| label.name);
            resume::save(path, &state, name.as_deref()).map_err(Into::into)
        }
        Err(_) => Ok(()),
    }
}

/// Everything a run needs, with each value's origin, for `dino config`.
struct Resolved {
    model: (Option<String>, Source),
    preset: (preset::Preset, Source),
    max_turns: (NonZeroU32, Source),
    approvals: (ApprovalMode, Source),
    sandbox: (SandboxMode, Source),
    api_base: (Option<String>, Source),
    show_reasoning: (bool, Source),
    no_thinking: (bool, Source),
    plain: (bool, Source),
    greeting: (bool, Source),
    protected: (Vec<PathBuf>, Source),
    max_diagnostics: (Option<usize>, Source),
    thinking_budget: (Option<u32>, Source),
    reasoning_effort: (Option<dino_protocol::ReasoningEffort>, Source),
    compaction: config::Compaction,
    keep_going: (bool, Source),
    fallback: Vec<String>,
    review_model: Option<String>,
    explore_model: Option<String>,
    budget: Option<spend::Budget>,
    hooks: (Vec<dino_exec::Hook>, Source),
    end_turn_hooks: Vec<dino_exec::Hook>,
    start_hooks: Vec<dino_exec::Hook>,
    task_end_hooks: Vec<task::EndHook>,
    verify: Option<(Vec<String>, Source)>,
    task_limits: task::Limits,
    mcp: Vec<dino_exec::McpServer>,
    search: SearchSettings,
    layers: Layers,
}

/// Whether to search, and the engine for a model whose provider has none.
struct SearchSettings {
    enabled: (bool, Source),
    backend: Option<settings::SearchBackendConfig>,
}

fn search_settings(layers: &Layers) -> SearchSettings {
    SearchSettings {
        enabled: layers
            .pick(|file| file.web_search)
            .unwrap_or((true, Source::Default)),
        backend: layers
            .pick_user_only(|file| file.search_backend.clone())
            .map(|(backend, _)| backend),
    }
}

/// The verify command, if it may run: the user's own, or a project's the
/// person has trusted, now or before, in exactly this form.
fn trusted_verify(
    proposed: Option<(Vec<String>, Source)>,
    project_file: Option<&Path>,
    workspace: &Path,
) -> Option<Vec<String>> {
    let (command, source) = proposed?;
    let Some(project_file) = project_file.filter(|_| source == Source::ProjectFile) else {
        return Some(command);
    };
    let digest = settings::verify_digest(workspace, &command);
    let store = settings::trusted_verify_path()?;
    if settings::is_trusted(&store, &digest) {
        return Some(command);
    }
    if approve::trust_verify(project_file, &command) {
        if let Err(error) = settings::trust(&store, &digest) {
            eprintln!(
                "{}",
                style::paint(style::WARN, &format!("could not record the trust: {error}"))
            );
        }
        return Some(command);
    }
    eprintln!(
        "{}",
        style::paint(
            style::WARN,
            &format!(
                "{} proposes the verify command `{}`, which is not trusted and will not run; \
                 start an interactive session here to trust it",
                project_file.display(),
                command.join(" ")
            )
        )
    );
    None
}

/// The user file's hooks for one event, then the plugins'. A project file
/// cannot set any.
fn hooks_for(layers: &Layers, event: settings::HookEvent) -> Vec<dino_exec::Hook> {
    let mut hooks: Vec<dino_exec::Hook> = layers
        .pick_user_only(|file| file.hooks.clone())
        .map(|(hooks, _)| {
            hooks
                .into_iter()
                .filter(|hook| hook.event == event)
                .map(|hook| dino_exec::Hook {
                    command: hook.command,
                    extension: hook.extension,
                    tool: hook.tool,
                    program: hook.program,
                })
                .collect()
        })
        .unwrap_or_default();
    hooks.extend(
        plugin::active()
            .iter()
            .flat_map(|active| active.hooks(event)),
    );
    hooks
}

/// How long a task must run, or an approval wait, before a ping fires.
fn ping_after(layers: &Layers) -> std::time::Duration {
    std::time::Duration::from_secs(
        layers
            .pick_user_only(|file| file.ping_after_seconds)
            .map_or(300, |(seconds, _)| seconds),
    )
}

/// The hooks around calls, submissions and unanswered questions.
fn call_hooks(layers: &Layers) -> config::CallHooks {
    config::CallHooks {
        before_call: hooks_for(layers, settings::HookEvent::BeforeCall),
        after_call: hooks_for(layers, settings::HookEvent::AfterCall),
        prompt_submit: hooks_for(layers, settings::HookEvent::PromptSubmit),
        awaiting_approval: hooks_for(layers, settings::HookEvent::AwaitingApproval),
        ping_after: ping_after(layers),
    }
}

/// The user file's hooks for the end of a task, each with how long a task must
/// have run before it fires.
fn end_hooks(layers: &Layers) -> Vec<task::EndHook> {
    let after = ping_after(layers);
    hooks_for(layers, settings::HookEvent::TaskEnd)
        .into_iter()
        .map(|hook| task::EndHook { hook, after })
        .collect()
}

/// Applies RFC-0015's precedence: flag, environment, project file, user file,
/// default.
fn resolve(args: &CommonArgs, workspace: &Path) -> Result<Resolved> {
    resolve_layers(args, Layers::load(workspace)?)
}

/// The flag, then `QWEN_API_BASE`, then the user file.
fn api_base(args: &CommonArgs, layers: &Layers) -> (Option<String>, Source) {
    if let Some(base) = args.api_base.clone() {
        (Some(base), Source::Flag)
    } else if let Ok(base) = std::env::var("QWEN_API_BASE") {
        (Some(base), Source::Environment)
    } else if let Some((base, source)) = layers.pick_user_only(|file| file.api_base.clone()) {
        (Some(base), source)
    } else {
        (None, Source::Default)
    }
}

fn resolve_layers(args: &CommonArgs, layers: Layers) -> Result<Resolved> {
    let preset = args.preset.map_or_else(
        || {
            layers
                .pick(|file| file.preset)
                .unwrap_or((preset::Preset::Standard, Source::Default))
        },
        |preset| (preset, Source::Flag),
    );
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
    } else if args.accept_edits {
        (ApprovalMode::AcceptEdits, Source::Flag)
    } else if let Some(found) = layers.pick_user_only(|file| file.approvals) {
        found
    } else {
        (ApprovalMode::Interactive, Source::Default)
    };
    let approvals = with_preset(preset, approvals)?;

    let api_base = api_base(args, &layers);

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
        max_turns: max_turns(args, &layers, preset),
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
        greeting: layers
            .pick(|file| file.greeting)
            .unwrap_or((true, Source::Default)),
        protected: layers
            .pick(|file| file.protected.clone())
            .unwrap_or((Vec::new(), Source::Default)),
        max_diagnostics: layers
            .pick(|file| file.max_diagnostics.map(Some))
            .unwrap_or((None, Source::Default)),
        compaction: compaction(&layers),
        keep_going: switch(args.keep_going, |file| file.keep_going),
        fallback: layers
            .pick_user_only(|file| file.fallback.clone())
            .map(|(fallback, _)| fallback.keys())
            .unwrap_or_default(),
        review_model: layers
            .pick_user_only(|file| file.review_model.clone())
            .map(|(key, _)| key),
        explore_model: layers.user.explore_model.clone(),
        budget: layers.budget()?,
        thinking_budget: layers
            .pick(|file| file.thinking_budget.map(Some))
            .unwrap_or((None, Source::Default)),
        reasoning_effort: reasoning_effort(args, &layers),
        hooks: (
            hooks_for(&layers, settings::HookEvent::AfterEdit),
            Source::UserFile,
        ),
        end_turn_hooks: hooks_for(&layers, settings::HookEvent::BeforeEndTurn),
        start_hooks: hooks_for(&layers, settings::HookEvent::SessionStart),
        task_end_hooks: end_hooks(&layers),
        verify: layers
            .pick(|file| file.verify.clone())
            .filter(|(command, _)| !command.is_empty()),
        task_limits: layers.task_limits()?,
        mcp: mcp_servers(&layers),
        search: search_settings(&layers),
        preset,
        layers,
    })
}

/// The flag, then a file, then the preset, then 100.
fn max_turns(
    args: &CommonArgs,
    layers: &Layers,
    preset: (preset::Preset, Source),
) -> (NonZeroU32, Source) {
    args.max_turns.map_or_else(
        || {
            layers
                .pick(|file| file.max_turns)
                .or_else(|| preset.0.max_turns().map(|turns| (turns, preset.1)))
                .unwrap_or((
                    NonZeroU32::new(100).expect("100 is non-zero"),
                    Source::Default,
                ))
        },
        |turns| (turns, Source::Flag),
    )
}

/// The approvals a preset requires in place of looser ones from a file. A flag
/// that contradicts it is refused rather than quietly overruled.
fn with_preset(
    preset: (preset::Preset, Source),
    approvals: (ApprovalMode, Source),
) -> Result<(ApprovalMode, Source)> {
    match preset.0.approvals() {
        Some(required) if approvals.0 != required && approvals.1 == Source::Flag => bail!(
            "the {} preset requires {}; drop the approval flag",
            preset.0.name(),
            required.label()
        ),
        Some(required) => Ok((required, preset.1)),
        None => Ok(approvals),
    }
}

fn compaction(layers: &Layers) -> config::Compaction {
    let at_tokens = layers
        .pick(|file| file.auto_compact_tokens)
        .map_or(config::AUTO_COMPACT_TOKENS, |(tokens, _)| tokens.get());
    match layers.pick(|file| file.auto_compact) {
        Some((true, _)) => config::Compaction::Automatic { at_tokens },
        Some((false, _)) => config::Compaction::Manual,
        None => config::Compaction::Tasks { at_tokens },
    }
}

/// The user file's MCP servers, then the plugins'. A project file cannot set
/// any, and a plugin's server cannot take the name of one already set.
fn mcp_servers(layers: &Layers) -> Vec<dino_exec::McpServer> {
    let mut servers: Vec<dino_exec::McpServer> = layers
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
        .unwrap_or_default();
    for active in plugin::active() {
        for server in active.mcp() {
            if servers.iter().any(|known| known.name == server.name) {
                eprintln!(
                    "{}",
                    style::paint(
                        style::WARN,
                        &format!(
                            "plugin {}: mcp server {} is already configured; the plugin's is left out",
                            active.name(),
                            server.name
                        )
                    )
                );
            } else {
                servers.push(server);
            }
        }
    }
    servers
}

fn reasoning_effort(
    args: &CommonArgs,
    layers: &Layers,
) -> (Option<dino_protocol::ReasoningEffort>, Source) {
    match args.effort {
        Some(effort) => (Some(effort), Source::Flag),
        None => layers
            .pick(|file| file.reasoning_effort.map(Some))
            .unwrap_or((None, Source::Default)),
    }
}

fn verify_row(verify: Option<&(Vec<String>, Source)>) -> String {
    verify.map_or_else(
        || "unset".to_owned(),
        |(command, source)| format!("{}  ({})", command.join(" "), source.label()),
    )
}

fn plugins_row() -> String {
    let loaded: Vec<String> = plugin::active()
        .iter()
        .map(|active| format!("{} {}", active.name(), active.manifest.version))
        .collect();
    if loaded.is_empty() {
        "none  (dino plugin list)".to_owned()
    } else {
        format!("{}  (dino plugin list)", loaded.join(", "))
    }
}

fn compaction_row(compaction: config::Compaction) -> String {
    match compaction {
        config::Compaction::Manual => "only when asked".to_owned(),
        config::Compaction::Tasks { at_tokens } => format!(
            "a task seen through, past {} tokens",
            render::compact(at_tokens)
        ),
        config::Compaction::Automatic { at_tokens } => format!(
            "past {} tokens or 70% of the window",
            render::compact(at_tokens)
        ),
    }
}

fn budget_row(budget: Option<spend::Budget>) -> String {
    let Some(budget) = budget else {
        return "none".to_owned();
    };
    let each = [("a session", budget.session), ("a day", budget.daily)]
        .into_iter()
        .filter_map(|(per, usd)| Some(format!("{} {per}", spend::usd(usd?))))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{each}, warned at {:.0}%  (user file)",
        budget.warn_at * 100.0
    )
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

fn in_order(keys: &[String]) -> String {
    if keys.is_empty() {
        return "none".to_owned();
    }
    keys.join(", then ")
}

fn file_named(path: Option<&PathBuf>) -> String {
    path.map_or_else(|| "none".to_owned(), |path| path.display().to_string())
}

fn effort_row(
    (effort, source): (Option<dino_protocol::ReasoningEffort>, Source),
    model: Option<&str>,
) -> String {
    if let Some(effort) = effort {
        return format!("{effort}  ({})", source.label());
    }
    match config::resolve_profile(model).map(|profile| profile.default_effort) {
        Ok(Some(effort)) => format!("{effort}  (model default)"),
        _ => format!("provider default  ({})", source.label()),
    }
}

fn print_config(args: &PromptArgs) -> Result<()> {
    let workspace = config::resolve_workspace(args.workspace.as_deref())?;
    let common = CommonArgs {
        model: args.model.clone(),
        ..CommonArgs::default()
    };
    let resolved = resolve(&common, &workspace)?;
    print!(
        "{}",
        crate::render::status_block(&config_rows(&resolved, &workspace))
    );
    println!(
        "{}",
        style::paint(
            style::FAINT,
            "  a project file may not set approvals, sandbox or api base",
        )
    );
    Ok(())
}

/// What `dino config` shows: each setting, its value and where it came from.
fn config_rows(resolved: &Resolved, workspace: &Path) -> Vec<(&'static str, String)> {
    let show = |value: String, source: Source| format!("{value}  ({})", source.label());
    let switch = |(on, source): (bool, Source)| show(on.to_string(), source);
    vec![
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
            "preset",
            show(resolved.preset.0.label().to_owned(), resolved.preset.1),
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
        ("reasoning", switch(resolved.show_reasoning)),
        ("thinking off", switch(resolved.no_thinking)),
        (
            "reasoning effort",
            effort_row(resolved.reasoning_effort, resolved.model.0.as_deref()),
        ),
        ("plain", switch(resolved.plain)),
        ("greeting", switch(resolved.greeting)),
        ("keep going", switch(resolved.keep_going)),
        ("web search", search_row(resolved)),
        ("compaction", compaction_row(resolved.compaction)),
        ("fallback", in_order(&resolved.fallback)),
        (
            "review model",
            user_row(resolved.review_model.as_ref(), "off"),
        ),
        (
            "explore model",
            user_row(resolved.explore_model.as_ref(), "the session's"),
        ),
        ("budget", budget_row(resolved.budget)),
        ("verify", verify_row(resolved.verify.as_ref())),
        ("plugins", plugins_row()),
        (
            "branch prefix",
            user_row(
                resolved.layers.user.branch_prefix.as_ref(),
                "dino/  (default)",
            ),
        ),
        (
            "task budget",
            format!(
                "{}  (when a message names none)",
                resolved.task_limits.describe()
            ),
        ),
        (
            "project file",
            file_named(resolved.layers.project_path.as_ref()),
        ),
        ("user file", file_named(resolved.layers.user_path.as_ref())),
    ]
}

/// A setting only the user file may hold, or what applies without it.
fn user_row(value: Option<&String>, unset: &str) -> String {
    value.map_or_else(|| unset.to_owned(), |value| format!("{value}  (user file)"))
}

fn settings(args: &CommonArgs, session_id: &SessionId) -> Result<RunSettings> {
    let workspace = config::resolve_workspace(args.workspace.as_deref())?;
    let mut resolved = resolve(args, &workspace)?;
    let verify = trusted_verify(
        resolved.verify.take(),
        resolved.layers.project_path.as_deref(),
        &workspace,
    );
    run_settings(args, workspace, resolved, verify, session_id)
}

fn search_row(resolved: &Resolved) -> String {
    let (enabled, source) = resolved.search.enabled;
    if !enabled {
        return format!("off  ({})", source.label());
    }
    if !resolved.preset.0.offers(dino_exec::WEB_SEARCH) {
        return format!("off  (preset {})", resolved.preset.0.name());
    }
    let otherwise = resolved.search.backend.as_ref().map_or_else(
        || "none for other models".to_owned(),
        |backend| format!("{} for other models  (user file)", backend.engine.name()),
    );
    format!("at the provider for Claude and GPT, asked once; {otherwise}")
}

/// The settings `resolved` comes to, with the verify command already decided.
fn run_settings(
    args: &CommonArgs,
    workspace: PathBuf,
    resolved: Resolved,
    verify: Option<Vec<String>>,
    session_id: &SessionId,
) -> Result<RunSettings> {
    let profile = config::resolve_profile(resolved.model.0.as_deref())?;
    let fallback = config::resolve_fallback(&resolved.fallback, &profile)?;
    // An engine that cannot be used leaves the session without search rather
    // than without a session, as a failed MCP server does.
    let web_search = search::resolve(
        &profile,
        resolved.search.enabled.0
            && resolved.preset.0.offers(dino_exec::WEB_SEARCH)
            && resolved.approvals.0 != ApprovalMode::ReadOnly,
        resolved.search.backend.as_ref(),
        &|name| std::env::var(name).ok(),
    )
    .unwrap_or_else(|error| {
        eprintln!(
            "{}",
            style::paint(style::WARN, &format!("web search is off: {error}"))
        );
        search::WebSearch::Off
    });
    let review = resolved
        .review_model
        .as_deref()
        .map(|key| config::resolve_profile(Some(key)))
        .transpose()?;
    let explore = resolved
        .explore_model
        .as_deref()
        .map(|key| config::resolve_profile(Some(key)))
        .transpose()?;
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
        task_end_hooks: resolved.task_end_hooks,
        call_hooks: call_hooks(&resolved.layers),
        status_line: resolved
            .layers
            .pick_user_only(|file| file.status_line.clone())
            .map(|(command, _)| command)
            .filter(|command| !command.is_empty()),
        verify,
        task_limits: resolved.task_limits,
        mcp: resolved.mcp,
        thinking_budget: resolved.thinking_budget.0,
        compaction: resolved.compaction,
        keep_going: resolved.keep_going.0,
        show_reasoning: resolved.show_reasoning.0,
        api_base: resolved.api_base.0,
        disable_thinking: resolved.no_thinking.0,
        enable_thinking: None,
        reasoning_effort: resolved.reasoning_effort.0,
        plain: resolved.plain.0,
        greeting: resolved.greeting.0,
        fallback,
        budget: resolved.budget,
        web_search,
        review,
        explore,
        preset: resolved.preset.0,
    })
}

/// Asks `approver` about what the rules do not cover, and keeps the rules a
/// person chose to keep.
fn interactive_policy<A: Approver + 'static>(
    approver: A,
    workspace: &Path,
    accept_edits: bool,
) -> Box<dyn ApprovalPolicy> {
    let guarded = dino_sandbox::EXECUTABLE_CONFIG
        .iter()
        .chain([&dino_core::schedule::DIR])
        .map(PathBuf::from)
        .collect();
    let interactive = Interactive::new(approver).guarding(guarded);
    let interactive = if accept_edits {
        interactive.accepting_edits()
    } else {
        interactive
    };
    match settings::user_dir() {
        Some(dir) => Box::new(
            interactive.with_store(approve::FileRuleStore::for_workspace(
                &dir,
                &worktree::home(workspace),
            )),
        ),
        None => Box::new(interactive),
    }
}

/// The session's policy behind its plan gate, with the `before_call` hooks in
/// front of both.
fn gated_policy(
    settings: &RunSettings,
    sandbox_label: &str,
    sandbox: &Arc<dyn Sandbox>,
    remote: Option<&Arc<remote::Hub>>,
    planning: &Arc<std::sync::atomic::AtomicBool>,
) -> impl ApprovalPolicy + 'static {
    dino_exec::BeforeCall::new(
        dino_core::approval::PlanGate::new(
            policy(
                settings.mode,
                sandbox_label,
                &settings.workspace,
                remote,
                &settings.call_hooks,
            ),
            Arc::clone(planning),
        ),
        settings.call_hooks.before_call.clone(),
        &settings.workspace,
        Arc::clone(sandbox),
    )
}

/// With `remote`, approvals are asked on the pages and not at the terminal.
fn policy(
    mode: ApprovalMode,
    sandbox: &str,
    workspace: &Path,
    remote: Option<&Arc<remote::Hub>>,
    waiting: &config::CallHooks,
) -> Box<dyn ApprovalPolicy> {
    match mode {
        ApprovalMode::Interactive | ApprovalMode::AcceptEdits => {
            let accept_edits = mode == ApprovalMode::AcceptEdits;
            match remote {
                Some(hub) => interactive_policy(
                    hooks::Pinging::new(
                        remote::RemoteApprover::new(Arc::clone(hub)),
                        waiting.awaiting_approval.clone(),
                        workspace.to_path_buf(),
                        waiting.ping_after,
                    ),
                    workspace,
                    accept_edits,
                ),
                None => interactive_policy(
                    hooks::Pinging::new(
                        TerminalApprover::new(sandbox, workspace),
                        waiting.awaiting_approval.clone(),
                        workspace.to_path_buf(),
                        waiting.ping_after,
                    ),
                    workspace,
                    accept_edits,
                ),
            }
        }
        ApprovalMode::ReadOnly => Box::new(ReadOnly),
        ApprovalMode::AllowAll => Box::new(AllowAll),
    }
}

/// A subagent's approvals: the parent's mode, plan gate and `before_call`
/// hooks, none of the parent's session grants, and a person asked about one
/// child at a time.
fn child_policy(
    sandbox: &str,
    confined: &Arc<dyn Sandbox>,
    workspace: &Path,
    remote: Option<&Arc<remote::Hub>>,
    hold: spinner::Hold,
    planning: Arc<std::sync::atomic::AtomicBool>,
    calls: config::CallHooks,
) -> subagent::OpenPolicy {
    let (sandbox, workspace) = (sandbox.to_owned(), workspace.to_path_buf());
    let confined = Arc::clone(confined);
    let remote = remote.map(Arc::clone);
    let turn = Arc::new(tokio::sync::Mutex::new(()));
    Box::new(move |mode| {
        let (hold, turn) = (hold.clone(), Arc::clone(&turn));
        let accept_edits = mode == ApprovalMode::AcceptEdits;
        let inner = match (mode, &remote) {
            (ApprovalMode::Interactive | ApprovalMode::AcceptEdits, Some(hub)) => {
                interactive_policy(
                    hooks::Pinging::new(
                        subagent::Asking::new(
                            remote::RemoteApprover::new(Arc::clone(hub)),
                            hold,
                            turn,
                        ),
                        calls.awaiting_approval.clone(),
                        workspace.clone(),
                        calls.ping_after,
                    ),
                    &workspace,
                    accept_edits,
                )
            }
            (ApprovalMode::Interactive | ApprovalMode::AcceptEdits, None) => interactive_policy(
                hooks::Pinging::new(
                    subagent::Asking::new(
                        TerminalApprover::new(sandbox.as_str(), workspace.as_path()),
                        hold,
                        turn,
                    ),
                    calls.awaiting_approval.clone(),
                    workspace.clone(),
                    calls.ping_after,
                ),
                &workspace,
                accept_edits,
            ),
            (ApprovalMode::ReadOnly | ApprovalMode::AllowAll, _) => {
                policy(mode, &sandbox, &workspace, None, &calls)
            }
        };
        Box::new(dino_exec::BeforeCall::new(
            dino_core::approval::PlanGate::new(inner, Arc::clone(&planning)),
            calls.before_call.clone(),
            &workspace,
            Arc::clone(&confined),
        ))
    })
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

#[cfg(test)]
mod tests {
    use dino_protocol::ReasoningEffort;

    use super::*;

    fn parsed(arguments: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(["dino"].into_iter().chain(arguments.iter().copied()))
    }

    #[test]
    fn skip_permissions_is_the_same_flag_as_allow_all() {
        for flag in ["--dangerously-allow-all", "--dangerously-skip-permissions"] {
            let cli = parsed(&[flag]).unwrap();
            assert!(cli.common.dangerously_allow_all, "{flag}");
        }
        assert!(!parsed(&[]).unwrap().common.dangerously_allow_all);
    }

    #[test]
    fn json_output_is_a_run_flag_and_the_webhook_listens_on_its_own_port() {
        let Some(Command::Run(run)) = parsed(&["run", "--json", "--read-only", "hi"])
            .unwrap()
            .command
        else {
            panic!("not a run");
        };
        assert!(run.json && run.common.read_only);
        let Some(Command::Webhook(webhook)) = parsed(&["webhook"]).unwrap().command else {
            panic!("not the webhook");
        };
        assert_eq!((webhook.bind, webhook.port), (None, 7879));
    }

    #[test]
    fn skip_permissions_still_conflicts_with_read_only() {
        assert!(parsed(&["--dangerously-skip-permissions", "--read-only"]).is_err());
    }

    #[test]
    fn accept_edits_conflicts_with_the_other_approval_flags() {
        assert!(parsed(&["--accept-edits"]).unwrap().common.accept_edits);
        assert!(parsed(&["--accept-edits", "--read-only"]).is_err());
        assert!(parsed(&["--accept-edits", "--dangerously-allow-all"]).is_err());
    }

    #[test]
    fn config_names_where_the_effort_came_from() {
        let unset = (None, Source::Default);
        assert_eq!(
            effort_row(unset, Some("kimi-k3.1-pro")),
            "low  (model default)"
        );
        assert_eq!(
            effort_row(unset, Some("claude-sonnet-5.5")),
            "provider default  (default)"
        );
        assert_eq!(
            effort_row(
                (Some(ReasoningEffort::High), Source::Flag),
                Some("kimi-k3.1-pro")
            ),
            "high  (flag)"
        );
    }

    #[test]
    fn a_preset_is_chosen_by_name() {
        assert_eq!(
            parsed(&["--preset", "minimal"]).unwrap().common.preset,
            Some(preset::Preset::Minimal)
        );
        assert!(parsed(&["--preset", "huge"]).is_err());
    }

    #[test]
    fn prompt_resolves_the_user_file_model_and_preset() {
        // `dino prompt` with no flags resolves model and preset the layered way
        // a run does, so the user file's choices reach the printed prompt. The
        // model layer sits below `DINO_MODEL`, which the test environment leaves
        // unset; the preset layer has no environment override.
        let layers = Layers {
            user: crate::settings::FileConfig {
                model: Some("claude-haiku-5.5".to_owned()),
                preset: Some(preset::Preset::Minimal),
                ..Default::default()
            },
            ..Default::default()
        };
        let resolved = resolve_layers(&CommonArgs::default(), layers).unwrap();
        assert_eq!(resolved.preset.0, preset::Preset::Minimal);
        assert_eq!(resolved.preset.1, Source::UserFile);
        if std::env::var_os("DINO_MODEL").is_none() {
            assert_eq!(resolved.model.0.as_deref(), Some("claude-haiku-5.5"));
            assert_eq!(resolved.model.1, Source::UserFile);
        }
    }

    #[test]
    fn the_models_table_aligns_every_row_to_the_longest_key() {
        let profiles = builtin_profiles();
        assert!(
            profiles
                .iter()
                .any(|p| p.key == "qwen3.6-35b-a3b-abliterated"),
            "the longest-keyed model is still in the table"
        );
        let longest = profiles.iter().map(|p| p.key.len()).max().unwrap();
        let provider_column = |line: &str| {
            let gap = line.find(' ').expect("a key and a provider");
            gap + line[gap..].chars().take_while(|c| *c == ' ').count()
        };
        let table = models_table(&profiles);
        let starts: Vec<usize> = table.lines().map(provider_column).collect();
        let first = starts[0];
        assert!(
            starts.iter().all(|&start| start == first),
            "every provider column starts at the same offset"
        );
        assert!(first > longest, "the key column clears the longest key");
    }

    #[test]
    fn the_review_preset_overrules_a_file_but_refuses_a_contrary_flag() {
        let review = (preset::Preset::Review, Source::Flag);
        assert_eq!(
            with_preset(review, (ApprovalMode::AllowAll, Source::UserFile)).unwrap(),
            (ApprovalMode::ReadOnly, Source::Flag)
        );
        assert!(with_preset(review, (ApprovalMode::AcceptEdits, Source::Flag)).is_err());
        let standard = (preset::Preset::Standard, Source::Default);
        assert_eq!(
            with_preset(standard, (ApprovalMode::AcceptEdits, Source::Flag)).unwrap(),
            (ApprovalMode::AcceptEdits, Source::Flag)
        );
    }
}
