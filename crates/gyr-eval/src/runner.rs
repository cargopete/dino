//! Running one case and deciding whether it passed.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use gyr_core::Agent;
use gyr_core::AgentConfig;
use gyr_core::ToolError;
use gyr_core::ToolFuture;
use gyr_core::ToolRuntime;
use gyr_core::ToolSet;
use gyr_core::approval::AllowAll;
use gyr_core::prompt::PromptContext;
use gyr_core::prompt::system_prompt;
use gyr_core::session::JsonlSessionLog;
use gyr_core::session::SessionId;
use gyr_core::session::SessionMeta;
use gyr_exec::ExecLimits;
use gyr_exec::ExecTool;
use gyr_exec::GithubLimits;
use gyr_exec::GithubTool;
use gyr_exec::ShellTool;
use gyr_exec::WebFetchTool;
use gyr_exec::WebLimits;
use gyr_model::ModelSession;
use gyr_protocol::ToolAction;
use gyr_protocol::ToolCall;
use gyr_protocol::ToolDefinition;
use gyr_rust::BuildCheck;
use gyr_rust::CargoLimits;
use gyr_rust::CargoTool;
use gyr_sandbox::Sandbox;
use gyr_tools::ToolLimits;
use gyr_tools::WorkspaceTools;
use serde::Deserialize;
use serde::Serialize;

use crate::Case;
use crate::CheckExpectation;
use crate::EvalError;
use crate::Metrics;
use crate::case::fingerprint_tree;
use crate::metrics;

/// What a run of one case produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub case: String,
    pub passed: bool,
    /// Every assertion that did not hold, in the order checked.
    pub failures: Vec<String>,
    pub files_changed: Vec<String>,
    pub metrics: Metrics,
    pub duration_ms: u128,
    pub log_path: String,
    /// Tools withheld from this run, so a comparison against another run cannot
    /// mistake an ablation for a behaviour change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub without: Vec<String>,
    /// The diagnostic cap the run was given, when it was not the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_diagnostics: Option<usize>,
    /// The reasoning budget the run was given, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u32>,
    /// The run's score, when scoring was asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<crate::score::Score>,
    /// Everything the score was computed from, so a later scorer version can
    /// rescore the run without running it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured: Option<crate::score::Measured>,
}

/// How a run differs from the harness as shipped, for ablations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Variant {
    /// Tools withheld.
    pub without: Vec<String>,
    /// How many Cargo diagnostics the model is shown, in place of the default.
    pub max_diagnostics: Option<usize>,
    /// The reasoning budget the session was built with. Recorded here; the
    /// session itself is the caller's to build.
    pub thinking_budget: Option<u32>,
    /// Ask for a `gyr-report` and score the run. RFC-0019.
    pub score: bool,
}

/// What shapes the tools a session gets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolOptions {
    /// Workspace-relative paths no tool may write.
    pub protected: Vec<std::path::PathBuf>,
    /// How many Cargo diagnostics the model is shown, in place of the default.
    pub max_diagnostics: Option<usize>,
    /// Offer the read-only `github` tool. Off for evals, which must not depend
    /// on the network.
    pub github: bool,
    /// Offer `web_fetch`. Off for evals, for the same reason.
    pub web: bool,
    /// Skills offered through the `skill` tool. None in evals.
    pub skills: Vec<gyr_core::skills::Skill>,
    /// Where `remember` writes. None in evals, which keep no memory.
    pub memory: Option<gyr_core::memory::MemoryFiles>,
    /// After-edit hooks around the file tools. None in evals.
    pub hooks: Vec<gyr_exec::Hook>,
}

/// A tool set with some tools hidden, for ablation.
///
/// A hidden tool behaves exactly as one that was never built: it is absent from
/// the definitions the model is given, and naming it is an unknown-tool error.
/// Anything less would be measuring a tool that is present but discouraged,
/// which is a different experiment.
struct Without {
    inner: ToolSet,
    hidden: HashSet<String>,
}

impl ToolRuntime for Without {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner
            .definitions()
            .into_iter()
            .filter(|definition| !self.hidden.contains(&definition.name))
            .collect()
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if self.hidden.contains(&call.name) {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        self.inner.classify(call)
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        if self.hidden.contains(&call.name) {
            let name = call.name.clone();
            return Box::pin(async move { Err(ToolError::new(format!("unknown tool {name:?}"))) });
        }
        self.inner.execute(call)
    }
}

/// Builds the provider session for one case.
pub type BuildSession<'a> =
    &'a dyn Fn(String, Vec<ToolDefinition>) -> Result<Box<dyn ModelSession>, EvalError>;

/// Runs one case to completion and checks its assertions.
///
/// Cases run with the allow-all policy because nobody is there to approve
/// anything, which is exactly why the caller must supply a confining sandbox.
///
/// # Errors
///
/// Returns [`EvalError`] when the case cannot be set up or its log cannot be
/// read. A case that fails its assertions is not an error: it is the result.
pub async fn run_case(
    case: &Case,
    scratch: &Path,
    sandbox: Arc<dyn Sandbox>,
    variant: &Variant,
    build_session: BuildSession<'_>,
) -> Result<Outcome, EvalError> {
    let workspace = case.materialise(scratch)?;
    let before = fingerprint_tree(&workspace)?;

    let (session, tools) = eval_session(case, &workspace, &sandbox, variant, build_session)?;

    let log_path = scratch.join(format!("{}.jsonl", case.name));
    let log = case_log(case, &log_path, &workspace, sandbox.as_ref())?;

    let finish = build_check(&workspace, Arc::clone(&sandbox))?;
    let mut agent = eval_agent(case, session, tools, log, finish)?;

    let started = Instant::now();
    let run = agent.run(prompt_for(case, variant.score)).await;
    let duration_ms = started.elapsed().as_millis();

    let mut failures = Vec::new();
    let answer = match &run {
        Ok(result) => result.text.clone(),
        Err(error) => {
            failures.push(format!("the run did not complete: {error}"));
            String::new()
        }
    };

    let blocked =
        matches!(&run, Ok(result) if result.stop_reason == gyr_protocol::StopReason::Blocked);
    match (blocked, case.expect.blocked) {
        (true, false) => failures.push(format!("the model reported a blocker: {answer}")),
        (false, true) => failures.push("the model did not report the blocker".to_owned()),
        _ => {}
    }

    let metrics = metrics::from_log(&log_path)?;
    conclude(
        case,
        scratch,
        sandbox,
        variant,
        Finished {
            workspace,
            before,
            answer,
            failures,
            metrics,
            duration_ms,
            log_path,
        },
    )
    .await
}

/// A run that has ended, by whichever harness, ready to be checked.
pub(crate) struct Finished {
    pub workspace: std::path::PathBuf,
    pub before: std::collections::BTreeMap<String, String>,
    pub answer: String,
    pub failures: Vec<String>,
    pub metrics: Metrics,
    pub duration_ms: u128,
    pub log_path: std::path::PathBuf,
}

/// Checks a finished run's assertions, measures it, lays the hidden tests
/// over it and scores it. The same for gyr and for an external harness.
pub(crate) async fn conclude(
    case: &Case,
    scratch: &Path,
    sandbox: Arc<dyn Sandbox>,
    variant: &Variant,
    finished: Finished,
) -> Result<Outcome, EvalError> {
    let Finished {
        workspace,
        before,
        answer,
        mut failures,
        metrics,
        duration_ms,
        log_path,
    } = finished;
    let after = fingerprint_tree(&workspace)?;
    let files_changed = changed_files(&before, &after);
    check(case, &workspace, &files_changed, &answer, &mut failures);
    if let Some(expectation) = case.expect.cargo_check
        && let Some(failure) = check_cargo(&workspace, Arc::clone(&sandbox), expectation).await?
    {
        failures.push(failure);
    }
    let mut measured = if variant.score {
        let measured = measure(
            case,
            &workspace,
            &sandbox,
            &files_changed,
            &answer,
            failures.is_empty(),
            &metrics,
        )
        .await?;
        Some(measured)
    } else {
        None
    };
    // Last, because the overlay changes the workspace every check above reads.
    if case.expect.hidden_tests {
        lay_hidden(
            case,
            scratch,
            &workspace,
            sandbox,
            measured.as_mut(),
            &mut failures,
        )
        .await?;
    }

    Ok(Outcome {
        case: case.name.clone(),
        passed: failures.is_empty(),
        failures,
        files_changed,
        metrics,
        duration_ms,
        log_path: log_path.display().to_string(),
        without: variant.without.clone(),
        max_diagnostics: variant.max_diagnostics,
        thinking_budget: variant.thinking_budget,
        score: measured
            .as_ref()
            .map(|measured| crate::score::score(case, measured)),
        measured,
    })
}

/// Lays the hidden tests over the workspace, records each one's result for
/// scoring, and fails the run when they do not pass.
async fn lay_hidden(
    case: &Case,
    scratch: &Path,
    workspace: &Path,
    sandbox: Arc<dyn Sandbox>,
    measured: Option<&mut crate::score::Measured>,
    failures: &mut Vec<String>,
) -> Result<(), EvalError> {
    crate::case::overlay_hidden(case, workspace)?;
    if let Some(measured) = measured {
        measured.baseline_passing = baseline_passing(case, scratch).await?;
        hidden_results(case, workspace, &sandbox, measured).await;
    }
    if let Some(failure) = check_hidden_tests(workspace, sandbox).await? {
        failures.push(failure);
    }
    Ok(())
}

/// Hidden tests that already pass on the untouched workspace, as
/// `binary::name`. Measured once per case and remembered for later trials.
async fn baseline_passing(case: &Case, scratch: &Path) -> Result<Vec<String>, EvalError> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(known) = cache
        .lock()
        .ok()
        .and_then(|cache| cache.get(&case.name).cloned())
    {
        return Ok(known);
    }
    let workspace = case.materialise(&scratch.join("baseline"))?;
    crate::case::overlay_hidden(case, &workspace)?;
    let sandbox: Arc<dyn Sandbox> = Arc::from(
        gyr_sandbox::detect_protecting(&workspace, &case.protected)
            .map_err(|error| EvalError::Setup(error.to_string()))?,
    );
    let mut measured = crate::score::Measured::default();
    hidden_results(case, &workspace, &sandbox, &mut measured).await;
    let passing = passing_keys(&measured);
    if let Ok(mut cache) = cache.lock() {
        cache.insert(case.name.clone(), passing.clone());
    }
    Ok(passing)
}

fn passing_keys(measured: &crate::score::Measured) -> Vec<String> {
    measured
        .hidden
        .iter()
        .filter(|test| test.passed)
        .map(|test| format!("{}::{}", test.binary, test.name))
        .collect()
}

/// The model session and the tool set one run of a case gets: the shipped
/// tools, less any withheld, and a prompt with nothing personal in it.
fn eval_session(
    case: &Case,
    workspace: &Path,
    sandbox: &Arc<dyn Sandbox>,
    variant: &Variant,
    build_session: BuildSession<'_>,
) -> Result<(Box<dyn ModelSession>, Without), EvalError> {
    let tools = Without {
        inner: session_tools(
            workspace,
            Arc::clone(sandbox),
            &ToolOptions {
                protected: case.protected.clone(),
                max_diagnostics: variant.max_diagnostics,
                github: false,
                web: false,
                hooks: Vec::new(),
                memory: None,
                skills: Vec::new(),
            },
        )?,
        hidden: variant.without.iter().cloned().collect(),
    };
    let definitions = tools.definitions();
    let context = PromptContext {
        workspace_root: workspace.display().to_string(),
        tools: definitions
            .iter()
            .map(|definition| definition.name.clone())
            .collect(),
        approval_mode: "eval (unattended, everything allowed, sandbox enforcing)".to_owned(),
        max_model_turns: Some(case.max_turns),
        git: None,
        // None, so a user's own instructions cannot move a measurement.
        instructions: Vec::new(),
        memory: Vec::new(),
        skills: Vec::new(),
    };
    let session = build_session(system_prompt(&context), definitions)?;
    Ok((session, tools))
}

/// A fresh session log for one run of a case.
fn case_log(
    case: &Case,
    log_path: &Path,
    workspace: &Path,
    sandbox: &dyn Sandbox,
) -> Result<JsonlSessionLog, EvalError> {
    let _ = std::fs::remove_file(log_path);
    JsonlSessionLog::create(
        log_path,
        SessionMeta {
            session_id: SessionId::generate(),
            gyrfalcon_version: env!("CARGO_PKG_VERSION").to_owned(),
            model_key: "eval".to_owned(),
            provider: gyr_protocol::ProviderKind::Qwen,
            workspace_root: workspace.display().to_string(),
            approval_mode: "eval (unattended, everything allowed)".to_owned(),
            sandbox: sandbox.label(),
            max_model_turns: case.max_turns,
        },
    )
    .map_err(|error| EvalError::Setup(error.to_string()))
}

/// Builds the sandbox for one workspace root.
pub type SandboxFor<'a> = dyn Fn(&Path) -> Result<Arc<dyn Sandbox>, EvalError> + 'a;

/// The floor and ceiling a case must score between before it is trusted, with
/// no model involved. RFC-0019 section 5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    pub case: String,
    /// The untouched workspace, which must score under 10.
    pub floor: crate::score::Score,
    /// The reference solution with an honest report, which must reach 95.
    pub ceiling: crate::score::Score,
}

impl Calibration {
    #[must_use]
    pub fn holds(&self) -> bool {
        self.floor.total < 10.0 && self.ceiling.total >= 95.0
    }
}

/// Scores a case's untouched workspace and its reference solution through the
/// same sandboxed measurement a model's run gets.
///
/// # Errors
///
/// Returns [`EvalError`] when the case cannot be materialised, has no
/// reference, or Cargo cannot be run at all.
pub async fn calibrate(
    case: &Case,
    scratch: &Path,
    sandbox_for: &SandboxFor<'_>,
) -> Result<Calibration, EvalError> {
    let measure_workspace = async |workspace: &Path, reference: bool| {
        let before = fingerprint_tree(workspace)?;
        if reference {
            crate::case::overlay_reference(case, workspace)?;
        }
        let sandbox = sandbox_for(workspace)?;
        let files_changed = changed_files(&before, &fingerprint_tree(workspace)?);
        let mut failures = Vec::new();
        check(case, workspace, &files_changed, "", &mut failures);
        let metrics = Metrics {
            model_turns: if reference {
                case.score
                    .reference_turns
                    .unwrap_or_else(|| case.max_turns.div_ceil(3))
            } else {
                1
            },
            ..Metrics::default()
        };
        let mut measured = measure(
            case,
            workspace,
            &sandbox,
            &files_changed,
            "",
            failures.is_empty(),
            &metrics,
        )
        .await?;
        if case.expect.hidden_tests {
            crate::case::overlay_hidden(case, workspace)?;
            hidden_results(case, workspace, &sandbox, &mut measured).await;
        }
        Ok::<_, EvalError>(measured)
    };
    let mut floor = measure_workspace(&case.materialise(&scratch.join("floor"))?, false).await?;
    let mut ceiling = measure_workspace(&case.materialise(&scratch.join("ceiling"))?, true).await?;
    // The untouched workspace is the baseline: tests passing there are guards.
    let baseline = passing_keys(&floor);
    floor.baseline_passing.clone_from(&baseline);
    ceiling.baseline_passing = baseline;
    // The report a model would give if it told the truth.
    ceiling.report = Some(crate::score::Report {
        task: Some(crate::score::Status::Done),
        build: Some(if ceiling.builds { "pass" } else { "fail" }.to_owned()),
        tests: Some(
            match ceiling.own_tests_pass {
                Some(true) => "pass",
                Some(false) => "fail",
                None => "none",
            }
            .to_owned(),
        ),
        requirements: ceiling
            .requirements
            .iter()
            .map(|(name, _, met)| {
                let status = if *met {
                    crate::score::Status::Done
                } else {
                    crate::score::Status::NotDone
                };
                (name.clone(), status)
            })
            .collect(),
    });
    Ok(Calibration {
        case: case.name.clone(),
        floor: crate::score::score(case, &floor),
        ceiling: crate::score::score(case, &ceiling),
    })
}

/// The case's prompt, with the report instruction when the run is scored.
pub(crate) fn prompt_for(case: &Case, scored: bool) -> String {
    if !scored {
        return case.prompt.clone();
    }
    let mut prompt = format!("{}\n\n{}", case.prompt, crate::score::REPORT_INSTRUCTION);
    if !case.score.requirements.is_empty() {
        let names: Vec<&str> = case
            .score
            .requirements
            .iter()
            .map(|requirement| requirement.name.as_str())
            .collect();
        let _ = write!(
            prompt,
            " The requirements to report on: {}.",
            names.join(", ")
        );
    }
    prompt
}

/// What scoring needs that the assertions do not: the build, the model's own
/// tests, each declared requirement, the report and protected paths. Read
/// before the hidden tests are laid over the workspace.
async fn measure(
    case: &Case,
    workspace: &Path,
    sandbox: &Arc<dyn Sandbox>,
    files_changed: &[String],
    answer: &str,
    assertions_passed: bool,
    metrics: &Metrics,
) -> Result<crate::score::Measured, EvalError> {
    let cargo = workspace.join("Cargo.toml").is_file();
    let (builds, own_tests_pass) = if cargo {
        let tool = CargoTool::new(workspace, CargoLimits::default(), Arc::clone(sandbox))
            .map_err(|error| EvalError::Setup(error.to_string()))?;
        let builds = tool
            .parsed_check()
            .await
            .is_ok_and(|parsed| parsed.counts.errors == 0);
        let tests = tool.run_tests().await.ok();
        // No `test result:` line with a test in it means the model wrote none.
        let ran_any = tests.as_ref().is_some_and(|run| {
            run.summary.split(" | ").any(|line| {
                line.starts_with("test result:") && !line.contains(" 0 passed; 0 failed")
            })
        });
        (builds, ran_any.then(|| tests.is_some_and(|run| run.passed)))
    } else {
        (true, None)
    };
    let mut requirements = Vec::new();
    for requirement in &case.score.requirements {
        let met = match requirement.command.split_first() {
            Some((program, arguments)) => gyr_exec::process::run(
                program,
                arguments,
                workspace,
                sandbox.as_ref(),
                256 * 1_024,
                std::time::Duration::from_secs(600),
            )
            .await
            .is_ok_and(|execution| execution.exit_code == Some(0)),
            None => false,
        };
        requirements.push((requirement.name.clone(), requirement.weight, met));
    }
    let protected_touched = files_changed.iter().any(|file| {
        case.protected
            .iter()
            .any(|protected| Path::new(file).starts_with(protected))
    });
    Ok(crate::score::Measured {
        builds,
        own_tests_pass,
        hidden: Vec::new(),
        hidden_broken: Vec::new(),
        baseline_passing: Vec::new(),
        assertions_passed,
        requirements,
        protected_touched,
        report: crate::score::parse_report(answer),
        metrics: metrics.clone(),
    })
}

/// Runs each hidden test file on its own and records every test's result. A
/// file that does not compile counts as one failed test, so breaking the build
/// cannot hide the tests it would have run.
async fn hidden_results(
    case: &Case,
    workspace: &Path,
    sandbox: &Arc<dyn Sandbox>,
    measured: &mut crate::score::Measured,
) {
    let Ok(entries) = std::fs::read_dir(case.directory.join("hidden").join("tests")) else {
        return;
    };
    let mut stems: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().is_some_and(|extension| extension == "rs"))
                .then(|| path.file_stem()?.to_str().map(str::to_owned))
                .flatten()
        })
        .collect();
    stems.sort();
    let manifest = workspace.join("Cargo.toml").display().to_string();
    for stem in stems {
        let arguments: Vec<String> = [
            "test",
            "--offline",
            "--no-fail-fast",
            "--test",
            stem.as_str(),
            "--manifest-path",
            manifest.as_str(),
        ]
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect();
        let run = gyr_exec::process::run(
            "cargo",
            &arguments,
            workspace,
            sandbox.as_ref(),
            4 * 1_024 * 1_024,
            std::time::Duration::from_secs(600),
        )
        .await;
        let Ok(execution) = run else {
            measured.hidden_broken.push(stem);
            continue;
        };
        let before = measured.hidden.len();
        for line in execution.stdout.lines() {
            let Some(rest) = line.strip_prefix("test ") else {
                continue;
            };
            let Some((name, verdict)) = rest.rsplit_once(" ... ") else {
                continue;
            };
            let passed = match verdict.trim() {
                "ok" => true,
                "FAILED" => false,
                _ => continue,
            };
            measured.hidden.push(crate::score::TestResult {
                binary: stem.clone(),
                name: name.to_owned(),
                passed,
            });
        }
        if measured.hidden.len() == before {
            // Why it produced nothing, so a case that cannot run says so.
            let reason = execution
                .stderr
                .lines()
                .chain(execution.stdout.lines())
                .find(|line| line.contains("error") || line.contains("panicked"))
                .unwrap_or("no test results")
                .trim()
                .chars()
                .take(200)
                .collect::<String>();
            measured.hidden_broken.push(format!("{stem} ({reason})"));
        }
    }
}

/// The tools a session gets, for the harness and for the executable alike.
///
/// It lives here rather than in `gyr-cli` because a library cannot depend on a
/// binary, and it must live in exactly one place: a corpus that measured a
/// different tool surface from the one the product ships would be measuring
/// nothing anyone uses. That is not hypothetical. Removing the gate from the
/// executable left it in the harness, and the next eval run still reported the
/// gate being called.
///
/// # Errors
///
/// Returns [`EvalError::Setup`] when a tool cannot be built for this workspace.
pub fn session_tools(
    workspace: &Path,
    sandbox: Arc<dyn Sandbox>,
    options: &ToolOptions,
) -> Result<ToolSet, EvalError> {
    session_tools_with(workspace, sandbox, options, Vec::new())
}

/// As [`session_tools`], with runtimes built elsewhere, such as connected MCP
/// servers, added after the built-in tools.
///
/// # Errors
///
/// As [`session_tools`], and when an extra runtime reuses a tool name.
pub fn session_tools_with(
    workspace: &Path,
    sandbox: Arc<dyn Sandbox>,
    options: &ToolOptions,
    extra: Vec<Box<dyn ToolRuntime>>,
) -> Result<ToolSet, EvalError> {
    let setup = |error: gyr_core::ToolError| EvalError::Setup(error.to_string());
    // A handover run's progress record is its checklist; two checklists with
    // different arguments are one too many, and models mix them up.
    let has_progress = extra.iter().any(|runtime| {
        runtime
            .definitions()
            .iter()
            .any(|definition| definition.name == gyr_core::progress::PROGRESS)
    });
    let mut runtimes: Vec<Box<dyn ToolRuntime>> = vec![
        file_tools(workspace, options, &sandbox)?,
        Box::new(gyr_core::blocker::BlockerTool),
        Box::new(
            ExecTool::new(workspace, ExecLimits::default(), Arc::clone(&sandbox)).map_err(setup)?,
        ),
        Box::new(
            ShellTool::new(workspace, ExecLimits::default(), Arc::clone(&sandbox))
                .map_err(setup)?,
        ),
        Box::new(
            gyr_exec::PackagesTool::new(workspace, ExecLimits::default(), Arc::clone(&sandbox))
                .map_err(setup)?,
        ),
    ];
    if !options.skills.is_empty() {
        runtimes.push(Box::new(gyr_core::skills::SkillTool::new(
            options.skills.clone(),
        )));
    }
    if let Some(files) = &options.memory {
        runtimes.push(Box::new(gyr_core::memory::MemoryTool::new(files.clone())));
    }
    if options.web {
        runtimes.push(Box::new(
            WebFetchTool::new(WebLimits::default()).map_err(setup)?,
        ));
    }
    if options.github {
        runtimes.push(Box::new(
            GithubTool::new(workspace, GithubLimits::default()).map_err(setup)?,
        ));
    }
    // Offered before a Cargo.toml exists too: a new project needs it to fetch.
    // The gate is deliberately absent. RFC-0011 section 13.
    let limits = CargoLimits {
        max_diagnostics: options
            .max_diagnostics
            .unwrap_or(CargoLimits::default().max_diagnostics),
        ..CargoLimits::default()
    };
    runtimes.push(Box::new(
        CargoTool::new(workspace, limits, sandbox).map_err(setup)?,
    ));
    if !has_progress {
        runtimes.push(Box::new(gyr_core::todo::TodoTool));
    }
    runtimes.extend(extra);
    ToolSet::new(runtimes).map_err(setup)
}

/// The file tools, wrapped in the user's after-edit hooks when there are any.
fn file_tools(
    workspace: &Path,
    options: &ToolOptions,
    sandbox: &Arc<dyn Sandbox>,
) -> Result<Box<dyn ToolRuntime>, EvalError> {
    let tools = WorkspaceTools::new(workspace, ToolLimits::default())
        .map_err(|error| EvalError::Setup(error.to_string()))?
        .protecting(options.protected.clone());
    if options.hooks.is_empty() {
        return Ok(Box::new(tools));
    }
    Ok(Box::new(gyr_exec::AfterEdit::new(
        Box::new(tools),
        options.hooks.clone(),
        workspace,
        Arc::clone(sandbox),
    )))
}

/// An agent for one unattended case: everything allowed, inside the sandbox.
fn eval_agent<S: ModelSession, T: ToolRuntime>(
    case: &Case,
    session: S,
    tools: T,
    log: JsonlSessionLog,
    check: Option<BuildCheck>,
) -> Result<Agent<S, T>, EvalError> {
    let turns = std::num::NonZeroU32::new(case.max_turns)
        .ok_or_else(|| EvalError::Case(format!("case {} allows zero turns", case.name)))?;
    let config = AgentConfig {
        max_model_turns: turns,
        ..AgentConfig::default()
    };
    let agent = Agent::new(session, tools, config)
        .with_policy(AllowAll)
        .with_sink(log);
    Ok(match check {
        Some(check) => agent.with_finish_check(check),
        None => agent,
    })
}

/// The check that sends a model back when it stops with the build broken, for a
/// Cargo workspace run unattended.
///
/// # Errors
///
/// Returns [`EvalError::Setup`] when the Cargo tool cannot be built.
pub fn build_check(
    workspace: &Path,
    sandbox: Arc<dyn Sandbox>,
) -> Result<Option<BuildCheck>, EvalError> {
    if !workspace.join("Cargo.toml").is_file() {
        return Ok(None);
    }
    let cargo = CargoTool::new(workspace, CargoLimits::default(), sandbox)
        .map_err(|error| EvalError::Setup(error.to_string()))?;
    Ok(Some(BuildCheck::new(cargo)))
}

fn changed_files(
    before: &std::collections::BTreeMap<String, String>,
    after: &std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    let mut changed: Vec<String> = after
        .iter()
        .filter(|(name, digest)| before.get(*name) != Some(*digest))
        .map(|(name, _)| name.clone())
        .collect();
    changed.extend(
        before
            .keys()
            .filter(|name| !after.contains_key(*name))
            .cloned(),
    );
    changed.sort();
    changed.dedup();
    changed
}

/// Applies the case's assertions, and the one the harness applies to every case.
fn check(
    case: &Case,
    workspace: &Path,
    files_changed: &[String],
    answer: &str,
    failures: &mut Vec<String>,
) {
    // The harness's own rule, so a case cannot forget it. RFC-0011 made "a
    // green build with no material diff is not success" a verdict a model reads;
    // here it is a rule the corpus applies to itself, because a corpus that can
    // pass by doing nothing measures nothing.
    //
    // A case that asserts on the answer is exempt, because its deliverable is
    // the answer. The rule is that a case must assert something material
    // happened, in the workspace or in what was said, not that every task ends
    // in an edit.
    if files_changed.is_empty()
        && case.expect.answer_contains.is_empty()
        && case.expect.answer_mentions.is_empty()
    {
        failures.push("nothing in the workspace changed".to_owned());
    }

    for wanted in &case.expect.answer_contains {
        if !answer.contains(wanted.as_str()) {
            failures.push(format!("the answer does not mention {wanted:?}"));
        }
    }
    let lowered = answer.to_lowercase();
    for idea in &case.expect.answer_mentions {
        if !idea
            .iter()
            .any(|wording| lowered.contains(&wording.to_lowercase()))
        {
            failures.push(format!("the answer mentions none of {idea:?}"));
        }
    }

    for expected in &case.expect.files_changed {
        if !files_changed.iter().any(|name| name == expected) {
            failures.push(format!("expected {expected} to change, and it did not"));
        }
    }
    for expected in &case.expect.files_unchanged {
        if files_changed.iter().any(|name| name == expected) {
            failures.push(format!(
                "expected {expected} to be left alone, and it was not"
            ));
        }
    }
    for expectation in &case.expect.contains {
        match read(workspace, &expectation.file) {
            Some(body) if body.contains(&expectation.text) => {}
            Some(_) => failures.push(format!(
                "{} does not contain {:?}",
                expectation.file, expectation.text
            )),
            None => failures.push(format!("{} could not be read", expectation.file)),
        }
    }
    for expectation in &case.expect.not_contains {
        if read(workspace, &expectation.file).is_some_and(|body| body.contains(&expectation.text)) {
            failures.push(format!(
                "{} still contains {:?}",
                expectation.file, expectation.text
            ));
        }
    }
}

fn read(workspace: &Path, relative: &str) -> Option<String> {
    std::fs::read_to_string(workspace.join(relative)).ok()
}

/// Runs `cargo test` over a workspace with the hidden tests laid in.
///
/// # Errors
///
/// Returns [`EvalError`] when Cargo cannot be run at all.
async fn check_hidden_tests(
    workspace: &Path,
    sandbox: Arc<dyn Sandbox>,
) -> Result<Option<String>, EvalError> {
    let cargo = CargoTool::new(workspace, CargoLimits::default(), sandbox)
        .map_err(|error| EvalError::Setup(error.to_string()))?;
    let run = cargo
        .run_tests()
        .await
        .map_err(|error| EvalError::Setup(error.to_string()))?;
    Ok((!run.passed).then(|| format!("hidden tests failed: {}", run.summary)))
}

/// Runs `cargo check` over a finished case and reports whether it matched.
///
/// Returns the failing message, or `None` when the expectation held.
///
/// # Errors
///
/// Returns [`EvalError`] when Cargo cannot be run at all, which is a broken
/// case rather than a failed one.
async fn check_cargo(
    workspace: &Path,
    sandbox: Arc<dyn Sandbox>,
    expectation: CheckExpectation,
) -> Result<Option<String>, EvalError> {
    let cargo = CargoTool::new(workspace, CargoLimits::default(), sandbox)
        .map_err(|error| EvalError::Setup(error.to_string()))?;
    let parsed = cargo
        .parsed_check()
        .await
        .map_err(|error| EvalError::Setup(error.to_string()))?;
    Ok(match (expectation, parsed.counts.errors) {
        (CheckExpectation::Clean, 0) | (CheckExpectation::Errors, 1..) => None,
        (CheckExpectation::Clean, errors) => {
            Some(format!("expected a clean build, found {errors} error(s)"))
        }
        (CheckExpectation::Errors, 0) => {
            Some("expected the build to still fail, and it did not".to_owned())
        }
    })
}
