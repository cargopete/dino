//! The `gyr eval` command.

use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use anyhow::bail;
use gyr_eval::Case;
use gyr_eval::Outcome;
use gyr_eval::run_case;
use gyr_model::ModelSession;

use crate::config;
use crate::config::RunSettings;
use crate::config::SandboxMode;
use crate::style;
use crate::style::FAINT;
use crate::style::MUTED;
use crate::style::OK;
use crate::style::SLATE;
use crate::style::WARN;

#[derive(Debug, clap::Args)]
pub struct EvalArgs {
    /// Model key from `gyr models`. Defaults to `GYR_MODEL`.
    #[arg(long)]
    pub model: Option<String>,
    /// Corpus directory. Defaults to `evals` beside the current directory.
    #[arg(long, default_value = "evals")]
    pub corpus: PathBuf,
    /// Run only these cases. Repeatable. Defaults to all of them.
    #[arg(long = "case")]
    pub cases: Vec<String>,
    /// Where case workspaces and logs are written.
    #[arg(long)]
    pub scratch: Option<PathBuf>,
    /// Containment for the processes a case starts.
    #[arg(long, value_enum, default_value_t = SandboxMode::Workspace)]
    pub sandbox: SandboxMode,
    /// Withhold a tool from every case, for ablation. Repeatable.
    #[arg(long = "without")]
    pub without: Vec<String>,
    /// Show the model at most this many Cargo diagnostics per run of the cargo
    /// tool, in place of the default, to measure what a small model can use.
    #[arg(long)]
    pub max_diagnostics: Option<usize>,
    /// Cap the model's reasoning per turn, where the server honours a budget.
    /// Qwen defaults to 4096.
    #[arg(long)]
    pub thinking_budget: Option<u32>,
    /// Emit the whole record, metrics included, as JSON.
    #[arg(long)]
    pub json: bool,
    /// Endpoint for a self-served model, standing in for `QWEN_API_BASE`.
    #[arg(long)]
    pub api_base: Option<String>,
    /// Ask a toggling model not to think.
    #[arg(long)]
    pub no_thinking: bool,
    /// Never emit terminal colour.
    #[arg(long)]
    pub plain: bool,
}

/// Runs a corpus and reports.
///
/// # Errors
///
/// Returns an error when the corpus cannot be read, the model cannot be built,
/// or a case is broken. A case that fails its assertions is not an error: it is
/// the result, and it decides the exit code.
pub async fn run(args: EvalArgs) -> Result<ExitCode> {
    style::enable(args.plain);
    let profile = config::resolve_profile(args.model.as_deref())?;
    let cases = select(&args)?;
    let scratch = match &args.scratch {
        Some(path) => path.clone(),
        None => std::env::temp_dir().join(format!("gyrfalcon-evals-{}", std::process::id())),
    };
    std::fs::create_dir_all(&scratch)?;

    if !args.json {
        eprintln!(
            "{}",
            style::paint(
                FAINT,
                &format!(
                    "{} · {} case(s){} · scratch {}",
                    profile.display_name,
                    cases.len(),
                    if args.without.is_empty() {
                        String::new()
                    } else {
                        format!(" · without {}", args.without.join(", "))
                    },
                    scratch.display()
                )
            )
        );
    }

    let mut outcomes = Vec::with_capacity(cases.len());
    for case in &cases {
        // Per case, because a sandbox is built around one workspace root and
        // each case gets its own. An unattended run is exactly when the
        // containment has to be real.
        let workspace = case.materialise(&scratch)?;
        let sandbox = config::build_sandbox(args.sandbox, &workspace, &case.protected)?;

        let settings = eval_settings(&args, &profile, &workspace);
        let build = |prompt: String, tools: Vec<gyr_protocol::ToolDefinition>| {
            config::build_session(&settings, prompt, tools)
                .map_err(|error| gyr_eval::EvalError::Setup(error.to_string()))
                .map(|session| session as Box<dyn ModelSession>)
        };

        let variant = gyr_eval::Variant {
            without: args.without.clone(),
            max_diagnostics: args.max_diagnostics,
            thinking_budget: args.thinking_budget,
        };
        let outcome = run_case(case, &scratch, Arc::clone(&sandbox), &variant, &build).await?;
        if !args.json {
            print_case(&outcome);
        }
        outcomes.push(outcome);
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&outcomes)?);
    } else {
        print_summary(&outcomes);
    }

    let failed = outcomes.iter().filter(|outcome| !outcome.passed).count();
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn select(args: &EvalArgs) -> Result<Vec<Case>> {
    let all = Case::load_corpus(&args.corpus)?;
    if args.cases.is_empty() {
        if all.is_empty() {
            bail!("no cases found in {}", args.corpus.display());
        }
        return Ok(all);
    }
    let mut chosen = Vec::with_capacity(args.cases.len());
    for wanted in &args.cases {
        let Some(case) = all.iter().find(|case| &case.name == wanted) else {
            bail!("no case named {wanted:?} in {}", args.corpus.display());
        };
        chosen.push(case.clone());
    }
    Ok(chosen)
}

fn eval_settings(
    args: &EvalArgs,
    profile: &gyr_protocol::ModelProfile,
    workspace: &Path,
) -> RunSettings {
    RunSettings {
        profile: profile.clone(),
        workspace: workspace.to_path_buf(),
        log_path: workspace.join("unused.jsonl"),
        max_turns: std::num::NonZeroU32::new(1).expect("1 is non-zero"),
        mode: crate::config::ApprovalMode::AllowAll,
        sandbox: args.sandbox,
        protected: Vec::new(),
        max_diagnostics: args.max_diagnostics,
        hooks: Vec::new(),
        end_turn_hooks: Vec::new(),
        start_hooks: Vec::new(),
        compaction: config::Compaction::Manual,
        mcp: Vec::new(),
        thinking_budget: args.thinking_budget,
        show_reasoning: false,
        api_base: args.api_base.clone(),
        disable_thinking: args.no_thinking,
        plain: args.plain,
    }
}

fn print_case(outcome: &Outcome) {
    let (colour, mark) = if outcome.passed {
        (OK, "pass")
    } else {
        (WARN, "fail")
    };
    println!(
        "  {}  {}  {}",
        style::paint(colour, mark),
        style::paint(SLATE, &format!("{:<20}", outcome.case)),
        style::paint(
            FAINT,
            &format!(
                "{} turn(s) · {} ms · {}",
                outcome.metrics.model_turns,
                outcome.duration_ms,
                tool_histogram(outcome)
            )
        )
    );
    for failure in &outcome.failures {
        println!("        {}", style::paint(WARN, failure));
    }
}

/// The metric the open questions in RFC-0010 and RFC-0011 are actually asking
/// for: what a model reached for, and how often.
fn tool_histogram(outcome: &Outcome) -> String {
    if outcome.metrics.tool_calls.is_empty() {
        return "no tools".to_owned();
    }
    outcome
        .metrics
        .tool_calls
        .iter()
        .map(|(name, count)| format!("{name} x{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_summary(outcomes: &[Outcome]) {
    let passed = outcomes.iter().filter(|outcome| outcome.passed).count();
    let verdicts: Vec<&str> = outcomes
        .iter()
        .flat_map(|outcome| outcome.metrics.gate_verdicts.iter().map(String::as_str))
        .collect();
    let gate_calls: usize = outcomes
        .iter()
        .filter_map(|outcome| outcome.metrics.tool_calls.get("gate"))
        .sum();
    // Called and silent is not the same as never called. `gate start` returns
    // no verdict, and a summary that conflated the two would contradict the
    // histogram printed directly above it.
    let gate = match (gate_calls, verdicts.is_empty()) {
        (0, _) => "the gate was never called".to_owned(),
        (calls, true) => format!("the gate was called {calls} time(s) and returned no verdict"),
        (_, false) => format!("gate: {}", verdicts.join(", ")),
    };
    // Tokens in the summary because a person running a paid corpus should be
    // able to see what it cost without going and asking the invoice.
    let input: u64 = outcomes
        .iter()
        .map(|outcome| outcome.metrics.tokens.input_tokens)
        .sum();
    let output: u64 = outcomes
        .iter()
        .map(|outcome| outcome.metrics.tokens.output_tokens)
        .sum();
    println!(
        "\n  {}  {}\n  {}",
        style::paint(
            if passed == outcomes.len() { OK } else { WARN },
            &format!("{passed}/{} passed", outcomes.len())
        ),
        style::paint(MUTED, &gate),
        style::paint(
            FAINT,
            &format!("{input} input token(s), {output} output token(s) across the corpus")
        )
    );
}
