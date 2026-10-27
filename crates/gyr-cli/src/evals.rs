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

// Command-line flags are booleans by nature.
#[allow(clippy::struct_excessive_bools)]
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
    /// Score each run from 1 to 100 and the model across the corpus. RFC-0019.
    #[arg(long)]
    pub score: bool,
    /// Run every case this many times.
    #[arg(long, default_value_t = 1)]
    pub trials: u32,
    /// Score each case's untouched workspace and its reference solution, with
    /// no model, and fail unless every floor is under 10 and every ceiling 95
    /// or more. RFC-0019 section 5.
    #[arg(long)]
    pub calibrate: bool,
    /// Run the cases through another agent's own CLI instead of gyr, scored the
    /// same way. `--model` is then that harness's model name, passed through.
    #[arg(long, value_enum)]
    pub harness: Option<ExternalHarness>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ExternalHarness {
    ClaudeCode,
    Codex,
}

impl From<ExternalHarness> for gyr_eval::external::Harness {
    fn from(harness: ExternalHarness) -> Self {
        match harness {
            ExternalHarness::ClaudeCode => Self::ClaudeCode,
            ExternalHarness::Codex => Self::Codex,
        }
    }
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
    if args.calibrate {
        return calibrate(&args).await;
    }
    let profile = match args.harness {
        None => Some(config::resolve_profile(args.model.as_deref())?),
        Some(_) => None,
    };
    let label = match (&profile, args.harness, &args.model) {
        (Some(profile), _, _) => profile.key.clone(),
        (None, Some(harness), Some(model)) => format!("{model} ({harness:?})"),
        (None, _, _) => bail!("--harness needs --model, the harness's own model name"),
    };
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
                    profile
                        .as_ref()
                        .map_or(&label, |profile| &profile.display_name),
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
    let trials = args.trials.max(1);
    for (trial, case) in (1..=trials).flat_map(|trial| cases.iter().map(move |case| (trial, case)))
    {
        let scratch = if trials > 1 {
            scratch.join(format!("trial-{trial}"))
        } else {
            scratch.clone()
        };
        std::fs::create_dir_all(&scratch)?;
        // Per case, because a sandbox is built around one workspace root and
        // each case gets its own. An unattended run is exactly when the
        // containment has to be real.
        let workspace = case.materialise(&scratch)?;
        let sandbox = config::build_sandbox(args.sandbox, &workspace, &case.protected)?;

        if let (Some(harness), Some(model)) = (args.harness, &args.model) {
            let outcome = gyr_eval::external::run_external(
                case,
                &scratch,
                Arc::clone(&sandbox),
                harness.into(),
                model,
            )
            .await?;
            if !args.json {
                print_case(&outcome);
            }
            outcomes.push(outcome);
            continue;
        }
        let Some(profile) = &profile else {
            unreachable!("a profile is resolved whenever there is no harness");
        };
        let settings = eval_settings(&args, profile, &workspace);
        let build = |prompt: String, tools: Vec<gyr_protocol::ToolDefinition>| {
            config::build_session(&settings, prompt, tools)
                .map_err(|error| gyr_eval::EvalError::Setup(error.to_string()))
                .map(|session| session as Box<dyn ModelSession>)
        };

        let variant = gyr_eval::Variant {
            without: args.without.clone(),
            max_diagnostics: args.max_diagnostics,
            thinking_budget: args.thinking_budget,
            score: args.score,
        };
        let outcome = run_case(case, &scratch, Arc::clone(&sandbox), &variant, &build).await?;
        if !args.json {
            print_case(&outcome);
        }
        outcomes.push(outcome);
    }

    report(&args, &label, trials, &cases, &outcomes)?;

    let failed = outcomes.iter().filter(|outcome| !outcome.passed).count();
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Prints the runs, and the score when there is one, as a table or as JSON.
fn report(
    args: &EvalArgs,
    label: &str,
    trials: u32,
    cases: &[Case],
    outcomes: &[gyr_eval::Outcome],
) -> Result<()> {
    let summary = (args.score || args.harness.is_some()).then(|| summarise(cases, outcomes));
    if args.json {
        match &summary {
            Some(summary) => println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "model": label,
                    "trials": trials,
                    "summary": summary,
                    "outcomes": outcomes,
                }))?
            ),
            None => println!("{}", serde_json::to_string_pretty(&outcomes)?),
        }
    } else {
        print_summary(outcomes);
        if let Some(summary) = &summary {
            print_score(label, summary);
        }
    }
    Ok(())
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
        // The flag, else the user's own config: an endpoint is where the person
        // runs models, not a property of the case. A project file cannot set it.
        api_base: args.api_base.clone().or_else(|| {
            crate::settings::Layers::load(workspace)
                .ok()?
                .pick_user_only(|file| file.api_base.clone())
                .map(|(base, _)| base)
        }),
        disable_thinking: args.no_thinking,
        enable_thinking: None,
        plain: args.plain,
    }
}

/// Checks every selected case against the scorer's floor and ceiling.
async fn calibrate(args: &EvalArgs) -> Result<ExitCode> {
    let cases = select(args)?;
    let scratch = match &args.scratch {
        Some(path) => path.clone(),
        None => std::env::temp_dir().join(format!("gyrfalcon-calibrate-{}", std::process::id())),
    };
    let sandbox_for = |workspace: &std::path::Path| {
        config::build_sandbox(args.sandbox, workspace, &[])
            .map_err(|error| gyr_eval::EvalError::Setup(error.to_string()))
    };
    let mut broken = 0;
    for case in &cases {
        if !case.directory.join("reference").is_dir() {
            println!(
                "  {}  {}",
                style::paint(FAINT, "skip"),
                style::paint(SLATE, &format!("{:<28} no reference/", case.name))
            );
            continue;
        }
        let result = gyr_eval::calibrate(case, &scratch.join(&case.name), &sandbox_for).await?;
        let holds = result.holds();
        if !holds {
            broken += 1;
        }
        println!(
            "  {}  {}  {}",
            style::paint(
                if holds { OK } else { WARN },
                if holds { "ok  " } else { "FAIL" }
            ),
            style::paint(SLATE, &format!("{:<28}", case.name)),
            style::paint(
                FAINT,
                &format!(
                    "floor {:.1} · ceiling {:.1}",
                    result.floor.total, result.ceiling.total
                )
            )
        );
        if !holds {
            for (which, notes) in [
                ("ceiling", &result.ceiling.notes),
                ("floor", &result.floor.notes),
            ] {
                for note in notes {
                    println!(
                        "        {}",
                        style::paint(FAINT, &format!("{which}: {note}"))
                    );
                }
            }
        }
    }
    Ok(if broken == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Every scored run as (case, tier, total), summarised.
fn summarise(cases: &[gyr_eval::Case], outcomes: &[Outcome]) -> gyr_eval::score::Summary {
    let runs: Vec<(String, gyr_eval::score::Tier, f64)> = outcomes
        .iter()
        .filter_map(|outcome| {
            let score = outcome.score.as_ref()?;
            let case = cases.iter().find(|case| case.name == outcome.case)?;
            Some((
                outcome.case.clone(),
                gyr_eval::score::tier(case),
                score.total,
            ))
        })
        .collect();
    gyr_eval::score::summarise(&runs)
}

fn print_score(model: &str, summary: &gyr_eval::score::Summary) {
    println!(
        "\n  {}  {}",
        style::paint(SLATE, &format!("{model}: {} / 100", summary.score)),
        style::paint(
            FAINT,
            &format!(
                "90% interval {} to {} · {} run(s) · score version {}",
                summary.low, summary.high, summary.runs, summary.version
            )
        )
    );
    for (case, result) in &summary.cases {
        println!(
            "  {}  {}",
            style::paint(MUTED, &format!("{case:<28}")),
            style::paint(
                FAINT,
                &format!(
                    "{:>5.1}  ({:.0} to {:.0}, {:?}, {} trial(s))",
                    result.mean, result.min, result.max, result.tier, result.trials
                )
            )
        );
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
    if let Some(score) = &outcome.score {
        println!(
            "        {}",
            style::paint(
                MUTED,
                &format!(
                    "score {:.0} · correctness {:.2} · requirements {:.2} · honesty {:.2} · \
                     efficiency {:.2} · discipline {:.2}",
                    score.total,
                    score.correctness,
                    score.requirements,
                    score.honesty,
                    score.efficiency,
                    score.discipline
                )
            )
        );
        for note in &score.notes {
            println!("        {}", style::paint(FAINT, note));
        }
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
