//! The `dino eval` command.

use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use anyhow::bail;
use dino_eval::Case;
use dino_eval::EarlyStop;
use dino_eval::Outcome;
use dino_eval::RunRate;
use dino_eval::run_case;
use dino_eval::total_cost;
use dino_eval::wilson;
use dino_eval::wilson_interval;
use dino_model::ModelSession;
use futures_util::StreamExt as _;

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
    /// Model key from `dino models`. Defaults to `DINO_MODEL`.
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
    /// Run this many cases at once. Each makes its own model calls, so the
    /// provider's rate limit is the ceiling. Results keep the serial order.
    #[arg(long, default_value_t = 1)]
    pub jobs: usize,
    /// Stop each case after this many seconds, instead of the case's own
    /// wall-time limit. A turn budget does not stop one turn that never returns.
    #[arg(long)]
    pub max_wall_secs: Option<u64>,
    /// USD per million input tokens, with `--usd-per-million-out`. Cached
    /// tokens are priced as input. List price, not a measured bill.
    #[arg(long)]
    pub usd_per_million_in: Option<f64>,
    /// USD per million output tokens, with `--usd-per-million-in`.
    #[arg(long)]
    pub usd_per_million_out: Option<f64>,
    /// Draw of the machine, in watts. Wall time becomes estimated watt-hours.
    #[arg(long)]
    pub watts: Option<f64>,
    /// GPU hire, in USD per hour of wall time.
    #[arg(long)]
    pub dollars_per_hour: Option<f64>,
    /// Score each case's untouched workspace and its reference solution, with
    /// no model, and fail unless every floor is under 10 and every ceiling 95
    /// or more. RFC-0019 section 5.
    #[arg(long)]
    pub calibrate: bool,
    /// Rescore a record written by `--json --score` and print it again, with no
    /// model. Hidden tests are run afresh where the run's workspace remains.
    #[arg(long, value_name = "RECORD")]
    pub rescore: Option<PathBuf>,
    /// Run the cases through another agent's own CLI instead of dino, scored the
    /// same way. `--model` is then that harness's model name, passed through.
    #[arg(long, value_enum)]
    pub harness: Option<ExternalHarness>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ExternalHarness {
    ClaudeCode,
    Codex,
    Kimi,
    Grok,
}

impl From<ExternalHarness> for dino_eval::external::Harness {
    fn from(harness: ExternalHarness) -> Self {
        match harness {
            ExternalHarness::ClaudeCode => Self::ClaudeCode,
            ExternalHarness::Codex => Self::Codex,
            ExternalHarness::Kimi => Self::Kimi,
            ExternalHarness::Grok => Self::Grok,
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
    if let Some(record) = &args.rescore {
        return rescore(&args, record).await;
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
    let mut cases = select(&args)?;
    apply_wall(&mut cases, args.max_wall_secs)?;
    let rate = run_rate(&args)?;
    let scratch = match &args.scratch {
        Some(path) => path.clone(),
        None => std::env::temp_dir().join(format!("dino-evals-{}", std::process::id())),
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

    let trials = args.trials.max(1);
    let runs = (1..=trials)
        .flat_map(|trial| cases.iter().map(move |case| (trial, case)))
        .map(|(trial, case)| run_one(&args, profile.as_ref(), &scratch, trials, trial, case));
    let mut outcomes = Vec::with_capacity(cases.len());
    let mut finished = std::pin::pin!(scheduled(runs, args.jobs));
    while let Some(outcome) = finished.next().await {
        outcomes.push(record(outcome?, rate.as_ref(), args.json));
    }

    report(&args, &label, trials, &cases, &outcomes)?;

    let failed = outcomes.iter().filter(|outcome| !outcome.passed).count();
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Runs up to `jobs` of `runs` at once, yielding each result in the order given.
fn scheduled<F: Future>(
    runs: impl IntoIterator<Item = F>,
    jobs: usize,
) -> impl futures_util::Stream<Item = F::Output> {
    futures_util::stream::iter(runs).buffered(jobs.max(1))
}

/// One trial of one case, in its own workspace under the trial's scratch.
async fn run_one(
    args: &EvalArgs,
    profile: Option<&dino_protocol::ModelProfile>,
    scratch: &Path,
    trials: u32,
    trial: u32,
    case: &Case,
) -> Result<Outcome> {
    let scratch = if trials > 1 {
        scratch.join(format!("trial-{trial}"))
    } else {
        scratch.to_path_buf()
    };
    std::fs::create_dir_all(&scratch)?;
    // Per case, because a sandbox is built around one workspace root and
    // each case gets its own. An unattended run is exactly when the
    // containment has to be real.
    let workspace = case.materialise(&scratch)?;
    let sandbox = config::build_sandbox(args.sandbox, &workspace, &case.protected)?;

    if let (Some(harness), Some(model)) = (args.harness, &args.model) {
        return Ok(dino_eval::external::run_external(
            case,
            &scratch,
            Arc::clone(&sandbox),
            harness.into(),
            model,
        )
        .await?);
    }
    let Some(profile) = profile else {
        unreachable!("a profile is resolved whenever there is no harness");
    };
    let settings = eval_settings(args, profile, &workspace);
    let build = |prompt: String, tools: Vec<dino_protocol::ToolDefinition>| {
        config::build_session(&settings, prompt, tools)
            .map_err(|error| dino_eval::EvalError::Setup(error.to_string()))
            .map(|session| session as Box<dyn ModelSession>)
    };

    let variant = dino_eval::Variant {
        without: args.without.clone(),
        max_diagnostics: args.max_diagnostics,
        thinking_budget: args.thinking_budget,
        score: args.score,
    };
    Ok(run_case(case, &scratch, Arc::clone(&sandbox), &variant, &build).await?)
}

/// Prints the runs, and the score when there is one, as a table or as JSON.
fn report(
    args: &EvalArgs,
    label: &str,
    trials: u32,
    cases: &[Case],
    outcomes: &[dino_eval::Outcome],
) -> Result<()> {
    let summary = (args.score || args.harness.is_some() || args.rescore.is_some())
        .then(|| summarise(cases, outcomes));
    if args.json {
        match &summary {
            Some(summary) => println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "model": label,
                    "trials": trials,
                    "summary": summary,
                    "rates": rates_json(outcomes),
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

fn run_rate(args: &EvalArgs) -> Result<Option<RunRate>> {
    dino_eval::parse_rate(
        args.usd_per_million_in,
        args.usd_per_million_out,
        args.watts,
        args.dollars_per_hour,
    )
    .map_err(anyhow::Error::msg)
}

fn record(mut outcome: Outcome, rate: Option<&RunRate>, json: bool) -> Outcome {
    if let Some(rate) = rate {
        outcome.cost = Some(dino_eval::cost_of(
            &outcome.metrics.tokens,
            outcome.duration_ms,
            *rate,
        ));
    }
    if !json {
        print_case(&outcome);
    }
    outcome
}

fn apply_wall(cases: &mut [Case], seconds: Option<u64>) -> Result<()> {
    if seconds == Some(0) {
        bail!("--max-wall-secs must be at least 1");
    }
    if let Some(seconds) = seconds {
        for case in cases {
            case.max_wall_secs = Some(seconds);
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
    profile: &dino_protocol::ModelProfile,
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
        fallback: Vec::new(),
    }
}

/// Rescores a stored record and reports it as a run would have.
async fn rescore(args: &EvalArgs, record: &Path) -> Result<ExitCode> {
    #[derive(serde::Deserialize)]
    struct Record {
        model: String,
        trials: u32,
        outcomes: Vec<Outcome>,
    }
    let Record {
        model,
        trials,
        mut outcomes,
    } = serde_json::from_str(&std::fs::read_to_string(record)?)?;
    let all = Case::load_corpus(&args.corpus)?;
    let scratch = match &args.scratch {
        Some(path) => path.clone(),
        None => std::env::temp_dir().join(format!("dino-rescore-{}", std::process::id())),
    };
    std::fs::create_dir_all(&scratch)?;
    let scratch = std::path::absolute(&scratch)?;
    let sandbox_for = |workspace: &std::path::Path| {
        config::build_sandbox(args.sandbox, workspace, &[])
            .map_err(|error| dino_eval::EvalError::Setup(error.to_string()))
    };
    let mut cases: Vec<Case> = Vec::new();
    for outcome in &mut outcomes {
        let Some(case) = all.iter().find(|case| case.name == outcome.case) else {
            bail!(
                "no case named {:?} in {}",
                outcome.case,
                args.corpus.display()
            );
        };
        dino_eval::rescore(case, outcome, &scratch, &sandbox_for).await?;
        if !cases.iter().any(|known| known.name == case.name) {
            cases.push(case.clone());
        }
        if !args.json {
            print_case(outcome);
        }
    }
    report(args, &model, trials, &cases, &outcomes)?;
    Ok(ExitCode::SUCCESS)
}

/// Checks every selected case against the scorer's floor and ceiling.
async fn calibrate(args: &EvalArgs) -> Result<ExitCode> {
    let cases = select(args)?;
    let scratch = match &args.scratch {
        Some(path) => path.clone(),
        None => std::env::temp_dir().join(format!("dino-calibrate-{}", std::process::id())),
    };
    let sandbox_for = |workspace: &std::path::Path| {
        config::build_sandbox(args.sandbox, workspace, &[])
            .map_err(|error| dino_eval::EvalError::Setup(error.to_string()))
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
        let result = dino_eval::calibrate(case, &scratch.join(&case.name), &sandbox_for).await?;
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
fn summarise(cases: &[dino_eval::Case], outcomes: &[Outcome]) -> dino_eval::score::Summary {
    let runs: Vec<(String, dino_eval::score::Tier, f64)> = outcomes
        .iter()
        .filter_map(|outcome| {
            let score = outcome.score.as_ref()?;
            let case = cases.iter().find(|case| case.name == outcome.case)?;
            Some((
                outcome.case.clone(),
                dino_eval::score::tier(case),
                score.total,
            ))
        })
        .collect();
    dino_eval::score::summarise(&runs)
}

fn print_score(model: &str, summary: &dino_eval::score::Summary) {
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
    let (colour, mark) = match (outcome.passed, outcome.stopped) {
        (true, _) => (OK, "pass"),
        (_, Some(EarlyStop::WallTime)) => (WARN, "wall"),
        _ => (WARN, "fail"),
    };
    println!(
        "  {}  {}  {}",
        style::paint(colour, mark),
        style::paint(SLATE, &format!("{:<20}", outcome.case)),
        style::paint(
            FAINT,
            &format!(
                "{} turn(s) · {} ms{} · {}",
                outcome.metrics.model_turns,
                outcome.duration_ms,
                outcome
                    .cost
                    .as_ref()
                    .map(|cost| format!(" · {}", cost.phrase()))
                    .unwrap_or_default(),
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
    let walls = outcomes
        .iter()
        .filter(|outcome| outcome.stopped == Some(EarlyStop::WallTime))
        .count();
    let wall = if walls > 0 {
        format!(", {walls} hit the wall")
    } else {
        String::new()
    };
    let headline = format!(
        "{}/{} passed {}{wall}",
        passed,
        outcomes.len(),
        wilson_interval(as_count(passed), as_count(outcomes.len()))
    );
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
    let priced = total_cost(outcomes.iter().filter_map(|outcome| outcome.cost.as_ref()))
        .map(|cost| format!(" · {}", cost.phrase()))
        .unwrap_or_default();
    println!(
        "\n  {}  {}\n  {}",
        style::paint(if passed == outcomes.len() { OK } else { WARN }, &headline),
        style::paint(MUTED, &gate),
        style::paint(
            FAINT,
            &format!("{input} input token(s), {output} output token(s) across the corpus{priced}")
        )
    );
    let rows = case_counts(outcomes);
    if rows.iter().any(|(_, _, total)| *total > 1) {
        for (name, successes, total) in rows {
            println!(
                "  {}  {}",
                style::paint(MUTED, &format!("{name:<28}")),
                style::paint(
                    FAINT,
                    &format!(
                        "{successes}/{total} passed {}",
                        wilson_interval(successes, total)
                    )
                )
            );
        }
    }
}

fn as_count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Passes and trials per case, in the order the cases were first run.
fn case_counts(outcomes: &[Outcome]) -> Vec<(String, u64, u64)> {
    let mut rows: Vec<(String, u64, u64)> = Vec::new();
    for outcome in outcomes {
        let passed = u64::from(outcome.passed);
        if let Some(row) = rows.iter_mut().find(|row| row.0 == outcome.case) {
            row.1 += passed;
            row.2 += 1;
        } else {
            rows.push((outcome.case.clone(), passed, 1));
        }
    }
    rows
}

fn rates_json(outcomes: &[Outcome]) -> serde_json::Value {
    let passed = outcomes.iter().filter(|outcome| outcome.passed).count();
    let (low, high) = wilson(as_count(passed), as_count(outcomes.len()));
    let cases: Vec<serde_json::Value> = case_counts(outcomes)
        .into_iter()
        .map(|(name, successes, total)| {
            let (low, high) = wilson(successes, total);
            serde_json::json!({
                "case": name,
                "passed": successes,
                "total": total,
                "wilson95": [low, high],
            })
        })
        .collect();
    serde_json::json!({
        "passed": as_count(passed),
        "total": as_count(outcomes.len()),
        "wilson95": [low, high],
        "cases": cases,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use futures_util::StreamExt as _;

    use super::scheduled;

    async fn ran_with(jobs: usize) -> (Vec<usize>, usize) {
        let running = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        // Earlier runs take longer, so finishing order is the reverse of the given one.
        let runs = (0..6).map(|index| {
            let (running, peak) = (&running, &peak);
            async move {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10 * (6 - index as u64))).await;
                running.fetch_sub(1, Ordering::SeqCst);
                index
            }
        });
        let order = scheduled(runs, jobs).collect().await;
        (order, peak.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn runs_keep_their_order_and_never_exceed_the_jobs() {
        assert_eq!(ran_with(3).await, ((0..6).collect(), 3));
        assert_eq!(ran_with(1).await, ((0..6).collect(), 1));
        assert_eq!(ran_with(0).await, ((0..6).collect(), 1));
    }
}
