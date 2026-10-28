//! The harness's own correctness, driven by a scripted provider.
//!
//! No credential, no network, no model. A harness whose correctness depended on
//! a paid non-deterministic service could not be trusted to report anything.

use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use dino_eval::Case;
use dino_eval::run_case;
use dino_model::ModelError;
use dino_model::ModelEventStream;
use dino_model::ModelFuture;
use dino_model::ModelSession;
use dino_protocol::ModelEvent;
use dino_protocol::ModelProfile;
use dino_protocol::StopReason;
use dino_protocol::ToolCall;
use dino_protocol::TurnInput;
use dino_sandbox::Unconfined;
use futures_util::stream;
use pretty_assertions::assert_eq;
use serde_json::json;

static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let serial = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("dino-eval-test-{}-{serial}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A provider that makes a fixed sequence of moves.
struct Scripted {
    profile: ModelProfile,
    turns: VecDeque<Vec<ModelEvent>>,
}

impl ModelSession for Scripted {
    fn profile(&self) -> &ModelProfile {
        &self.profile
    }

    fn next(&mut self, _input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
        let turn = self.turns.pop_front();
        Box::pin(async move {
            let events =
                turn.ok_or_else(|| ModelError::Protocol("the script has no next turn".into()))?;
            Ok(Box::pin(stream::iter(events.into_iter().map(Ok))) as ModelEventStream)
        })
    }
}

fn tool_turn(id: &str, name: &str, arguments: serde_json::Value) -> Vec<ModelEvent> {
    vec![
        ModelEvent::ToolCallCompleted {
            call: ToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            },
        },
        ModelEvent::Finished {
            reason: StopReason::ToolUse,
        },
    ]
}

fn text_turn(text: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::TextDelta { text: text.into() },
        ModelEvent::Usage {
            usage: dino_protocol::TokenUsage {
                input_tokens: 120,
                cached_input_tokens: 20,
                output_tokens: 8,
                reasoning_tokens: 0,
            },
        },
        ModelEvent::Finished {
            reason: StopReason::EndTurn,
        },
    ]
}

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("evals")
}

/// The exact patch that fixes the `fix-type-errors` fixture, in two edits.
fn patch(path: &str, sha: &str, old: &str, new: &str) -> serde_json::Value {
    json!({"path": path, "expected_sha256": sha, "old_text": old, "new_text": new})
}

fn sha_of(workspace: &Path, relative: &str) -> String {
    digest(&fs::read(workspace.join(relative)).unwrap())
}

fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    use sha2::Digest as _;

    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut encoded, byte| {
            let _ = write!(&mut encoded, "{byte:02x}");
            encoded
        })
}

async fn run(case: &Case, scratch: &Scratch, turns: Vec<Vec<ModelEvent>>) -> dino_eval::Outcome {
    let build = |_prompt: String, _tools: Vec<dino_protocol::ToolDefinition>| {
        Ok(Box::new(Scripted {
            profile: dino_model::builtin_profiles().remove(0),
            turns: VecDeque::from(turns.clone()),
        }) as Box<dyn ModelSession>)
    };
    run_case(
        case,
        &scratch.path,
        Arc::new(Unconfined),
        &dino_eval::Variant::default(),
        &build,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_case_that_fixes_the_fixture_passes() {
    let scratch = Scratch::new();
    let case = Case::load(corpus().join("fix-type-errors")).unwrap();
    // The workspace has to exist before its fingerprint can be quoted back, so
    // it is materialised once here and again inside the run. Both copies are
    // byte-identical, which is the point of a fixture.
    let staged = case.materialise(&scratch.path).unwrap();
    let first = sha_of(&staged, "src/lib.rs");
    let fixed_once = fs::read_to_string(staged.join("src/lib.rs"))
        .unwrap()
        .replacen("\"one\"", "1", 1);
    let second = digest(fixed_once.as_bytes());

    let outcome = run(
        &case,
        &scratch,
        vec![
            tool_turn(
                "c1",
                "apply_patch",
                patch("src/lib.rs", &first, "\"one\"", "1"),
            ),
            tool_turn(
                "c2",
                "apply_patch",
                patch("src/lib.rs", &second, "\"two\"", "2"),
            ),
            text_turn("Both fixed."),
        ],
    )
    .await;

    assert!(outcome.passed, "failures: {:?}", outcome.failures);
    assert_eq!(outcome.files_changed, vec!["src/lib.rs".to_owned()]);
}

#[tokio::test]
async fn a_run_that_changes_nothing_fails_even_when_the_code_compiles() {
    let scratch = Scratch::new();
    // This fixture already compiles, so only the harness's own rule can catch
    // a model that did nothing.
    let case = Case::load(corpus().join("add-a-function")).unwrap();

    let outcome = run(&case, &scratch, vec![text_turn("Looks fine to me.")]).await;

    assert!(!outcome.passed);
    assert!(
        outcome
            .failures
            .iter()
            .any(|failure| failure.contains("nothing in the workspace changed")),
        "failures: {:?}",
        outcome.failures
    );
}

#[tokio::test]
async fn a_run_that_exceeds_its_turn_limit_fails_and_names_the_limit() {
    let scratch = Scratch::new();
    let mut case = Case::load(corpus().join("add-a-function")).unwrap();
    case.max_turns = 2;
    case.expect = dino_eval::Expectations::default();
    let staged = case.materialise(&scratch.path).unwrap();
    let sha = sha_of(&staged, "src/lib.rs");

    // Three tool turns against a budget of two.
    let outcome = run(
        &case,
        &scratch,
        vec![
            tool_turn("c1", "read", json!({"path": "src/lib.rs"})),
            tool_turn("c2", "read", json!({"path": "Cargo.toml"})),
            tool_turn(
                "c3",
                "apply_patch",
                patch("src/lib.rs", &sha, "pub fn one", "pub fn uno"),
            ),
        ],
    )
    .await;

    assert!(!outcome.passed);
    assert_eq!(outcome.stopped, None);
    assert!(
        outcome
            .failures
            .iter()
            .any(|failure| failure.contains("model-turn limit reached after 2")),
        "failures: {:?}",
        outcome.failures
    );
}

#[tokio::test]
async fn editing_a_file_the_case_declared_unchanged_fails() {
    let scratch = Scratch::new();
    let case = Case::load(corpus().join("add-a-function")).unwrap();
    let staged = case.materialise(&scratch.path).unwrap();
    let sha = sha_of(&staged, "Cargo.toml");

    let outcome = run(
        &case,
        &scratch,
        vec![
            tool_turn(
                "c1",
                "apply_patch",
                patch("Cargo.toml", &sha, "0.1.0", "0.2.0"),
            ),
            text_turn("Bumped the version."),
        ],
    )
    .await;

    assert!(!outcome.passed);
    assert!(
        outcome
            .failures
            .iter()
            .any(|failure| failure.contains("Cargo.toml to be left alone")),
        "failures: {:?}",
        outcome.failures
    );
}

#[tokio::test]
async fn the_metrics_match_the_moves_the_script_made() {
    let scratch = Scratch::new();
    let mut case = Case::load(corpus().join("add-a-function")).unwrap();
    case.expect = dino_eval::Expectations::default();
    let staged = case.materialise(&scratch.path).unwrap();
    let sha = sha_of(&staged, "src/lib.rs");

    let outcome = run(
        &case,
        &scratch,
        vec![
            tool_turn("c1", "read", json!({"path": "src/lib.rs"})),
            tool_turn("c2", "search", json!({"query": "pub fn"})),
            tool_turn(
                "c3",
                "apply_patch",
                patch("src/lib.rs", &sha, "pub fn one", "pub fn uno"),
            ),
            text_turn("Renamed it."),
        ],
    )
    .await;

    // Every one of these came out of the session log rather than out of the
    // agent, which is also the test that the log is sufficient.
    assert_eq!(outcome.metrics.model_turns, 4);
    assert_eq!(outcome.metrics.tool_calls.get("read"), Some(&1));
    assert_eq!(outcome.metrics.tool_calls.get("search"), Some(&1));
    assert_eq!(outcome.metrics.tool_calls.get("apply_patch"), Some(&1));
    assert_eq!(outcome.metrics.refusals, 0);
    assert_eq!(outcome.metrics.tool_errors, 0);
    assert_eq!(outcome.metrics.tokens.input_tokens, 120);
    assert_eq!(outcome.metrics.tokens.cached_input_tokens, 20);
    assert_eq!(outcome.metrics.stop_reason, Some(StopReason::EndTurn));
    assert!(outcome.passed, "failures: {:?}", outcome.failures);
}

#[test]
fn the_shipped_corpus_parses() {
    let cases = Case::load_corpus(corpus()).unwrap();

    assert!(cases.len() >= 2, "found {} cases", cases.len());
    for case in &cases {
        assert!(
            !case.prompt.trim().is_empty(),
            "{} has no prompt",
            case.name
        );
        assert!(case.max_turns > 0, "{} allows zero turns", case.name);
    }
}

#[test]
fn a_malformed_case_names_the_file() {
    let scratch = Scratch::new();
    let directory = scratch.path.join("broken");
    fs::create_dir_all(directory.join("workspace")).unwrap();
    fs::write(directory.join("case.toml"), "name = 3\n").unwrap();

    let error = Case::load(&directory).unwrap_err();

    assert!(error.to_string().contains("case.toml"), "said: {error}");
}

#[test]
fn a_case_without_a_workspace_is_refused() {
    let scratch = Scratch::new();
    let directory = scratch.path.join("no-workspace");
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("case.toml"),
        "name = \"x\"\nprompt = \"y\"\nmax_turns = 1\n",
    )
    .unwrap();

    let error = Case::load(&directory).unwrap_err();

    assert!(error.to_string().contains("workspace/"), "said: {error}");
}

fn hidden_test_case(scratch: &Scratch) -> Case {
    let directory = scratch.path.join("case-with-hidden-tests");
    fs::create_dir_all(directory.join("workspace/src")).unwrap();
    fs::create_dir_all(directory.join("hidden/tests")).unwrap();
    fs::write(
        directory.join("case.toml"),
        "name = \"hidden\"\nprompt = \"make two return 2\"\nmax_turns = 4\n\n\
         [expect]\ncargo_check = \"clean\"\nhidden_tests = true\n",
    )
    .unwrap();
    fs::write(
        directory.join("workspace/Cargo.toml"),
        "[workspace]\n[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        directory.join("workspace/src/lib.rs"),
        "pub fn two() -> u32 {\n    1\n}\n",
    )
    .unwrap();
    fs::write(
        directory.join("hidden/tests/two.rs"),
        "#[test]\nfn two_is_two() {\n    assert_eq!(fixture::two(), 2);\n}\n",
    )
    .unwrap();
    Case::load(&directory).unwrap()
}

async fn answer_two_with(case: &Case, scratch: &Scratch, value: &str) -> dino_eval::Outcome {
    let workspace = scratch.path.join("work");
    fs::create_dir_all(&workspace).unwrap();
    let source = case.directory.join("workspace/src/lib.rs");
    let sha = digest(&fs::read(source).unwrap());
    run(
        case,
        &Scratch {
            path: workspace.clone(),
        },
        vec![
            tool_turn(
                "c1",
                "apply_patch",
                patch("src/lib.rs", &sha, "    1\n", &format!("    {value}\n")),
            ),
            text_turn("Done."),
        ],
    )
    .await
}

#[tokio::test]
async fn hidden_tests_decide_a_case_the_compiler_cannot() {
    let scratch = Scratch::new();
    let case = hidden_test_case(&scratch);

    let right = answer_two_with(&case, &scratch, "2").await;
    assert!(right.passed, "failures: {:?}", right.failures);

    let wrong = answer_two_with(&case, &scratch, "3").await;
    assert!(!wrong.passed);
    assert!(
        wrong
            .failures
            .iter()
            .any(|failure| failure.starts_with("hidden tests failed")),
        "failures: {:?}",
        wrong.failures
    );
}

#[test]
fn a_case_asking_for_hidden_tests_must_have_them() {
    let scratch = Scratch::new();
    let case = hidden_test_case(&scratch);
    fs::remove_dir_all(case.directory.join("hidden")).unwrap();

    let error = Case::load(&case.directory).unwrap_err();

    assert!(error.to_string().contains("hidden/"), "said: {error}");
}

fn blocker_turn(reason: &str) -> Vec<ModelEvent> {
    tool_turn(
        "b1",
        "report_blocker",
        json!({"reason": reason, "evidence": ["tests/two.rs"]}),
    )
}

#[tokio::test]
async fn a_blocker_the_case_expects_passes_and_one_it_does_not_fails() {
    let scratch = Scratch::new();
    let directory = scratch.path.join("cases").join("contradiction");
    fs::create_dir_all(directory.join("workspace/src")).unwrap();
    fs::write(
        directory.join("case.toml"),
        "name = \"contradiction\"\nprompt = \"make two return 2\"\nmax_turns = 4\n\n\
         [expect]\nblocked = true\nanswer_contains = [\"tests/two.rs\"]\n",
    )
    .unwrap();
    fs::write(directory.join("workspace/src/lib.rs"), "pub fn two() {}\n").unwrap();
    let expecting = Case::load(&directory).unwrap();
    let mut not_expecting = expecting.clone();
    not_expecting.expect.blocked = false;

    let reason = "tests/two.rs wants 3 from a function named two";
    let blocked = run(&expecting, &scratch, vec![blocker_turn(reason)]).await;
    assert!(blocked.passed, "failures: {:?}", blocked.failures);

    let unexpected = run(&not_expecting, &scratch, vec![blocker_turn(reason)]).await;
    assert!(
        unexpected
            .failures
            .iter()
            .any(|failure| failure.contains("reported a blocker")),
        "failures: {:?}",
        unexpected.failures
    );

    let carried_on = run(
        &expecting,
        &scratch,
        vec![text_turn("tests/two.rs is fine")],
    )
    .await;
    assert!(
        carried_on
            .failures
            .iter()
            .any(|failure| failure.contains("did not report the blocker")),
        "failures: {:?}",
        carried_on.failures
    );
}

#[test]
fn a_case_can_name_its_own_wall_and_zero_is_refused() {
    let scratch = Scratch::new();
    let directory = scratch.path.join("walled");
    fs::create_dir_all(directory.join("workspace")).unwrap();
    fs::write(
        directory.join("case.toml"),
        "name = \"walled\"\nprompt = \"y\"\nmax_turns = 4\nmax_wall_secs = 90\n",
    )
    .unwrap();

    let case = Case::load(&directory).unwrap();
    assert_eq!(case.max_wall_secs, Some(90));
    assert_eq!(
        case.wall_limit().unwrap(),
        std::time::Duration::from_secs(90)
    );

    let mut zero = case;
    zero.max_wall_secs = Some(0);
    let error = zero.wall_limit().unwrap_err();
    assert!(error.to_string().contains("zero"), "said: {error}");
}

/// A provider that never answers, so only the wall can end the run.
struct Stalling {
    profile: ModelProfile,
}

impl ModelSession for Stalling {
    fn profile(&self) -> &ModelProfile {
        &self.profile
    }

    fn next(&mut self, _input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(Box::pin(stream::iter(std::iter::empty())) as ModelEventStream)
        })
    }
}

#[tokio::test]
async fn a_run_that_hits_its_wall_fails_as_a_wall_and_nothing_else() {
    let scratch = Scratch::new();
    let mut case = Case::load(corpus().join("add-a-function")).unwrap();
    case.max_wall_secs = Some(1);
    case.expect = dino_eval::Expectations::default();
    let build = |_prompt: String, _tools: Vec<dino_protocol::ToolDefinition>| {
        Ok(Box::new(Stalling {
            profile: dino_model::builtin_profiles().remove(0),
        }) as Box<dyn ModelSession>)
    };

    let outcome = run_case(
        &case,
        &scratch.path,
        Arc::new(Unconfined),
        &dino_eval::Variant::default(),
        &build,
    )
    .await
    .unwrap();

    assert!(!outcome.passed);
    assert_eq!(outcome.stopped, Some(dino_eval::EarlyStop::WallTime));
    assert_eq!(
        outcome.failures,
        vec!["hit the wall-time limit after 1 second".to_owned()]
    );
    assert!(outcome.score.is_none());
    assert!(
        outcome.duration_ms < 10_000,
        "the wall did not stop the run: {} ms",
        outcome.duration_ms
    );
}

fn tool_text(input: &TurnInput) -> String {
    match input {
        TurnInput::ToolResults { results } => results
            .last()
            .map(|result| result.output.content.clone())
            .unwrap_or_default(),
        TurnInput::User { content } | TurnInput::UserWithImages { content, .. } => content.clone(),
    }
}

fn profile() -> ModelProfile {
    dino_model::builtin_profiles().remove(0)
}

/// Tries a byte-identical patch, and the real edit only when that is refused.
struct FixesAfterRefusal {
    profile: ModelProfile,
    step: u32,
}

impl ModelSession for FixesAfterRefusal {
    fn profile(&self) -> &ModelProfile {
        &self.profile
    }

    fn next(&mut self, input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
        let step = self.step;
        self.step += 1;
        let heard = tool_text(&input);
        let events = match step {
            0 => tool_turn("c1", "read", json!({"path": "src/lib.rs"})),
            1 => tool_turn(
                "c2",
                "apply_patch",
                json!({"path": "src/lib.rs", "old_text": "100", "new_text": "100"}),
            ),
            2 if heard.contains("would not change the file") => tool_turn(
                "c3",
                "apply_patch",
                json!({"path": "src/lib.rs", "old_text": "100", "new_text": "250"}),
            ),
            _ => text_turn("Done."),
        };
        Box::pin(async move {
            Ok(Box::pin(stream::iter(events.into_iter().map(Ok))) as ModelEventStream)
        })
    }
}

/// Reads the same file until the tool says that range was already returned.
struct RereadsUntilTold {
    profile: ModelProfile,
    step: u32,
}

impl ModelSession for RereadsUntilTold {
    fn profile(&self) -> &ModelProfile {
        &self.profile
    }

    fn next(&mut self, input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
        let step = self.step;
        self.step += 1;
        let heard = tool_text(&input);
        let id = format!("c{step}");
        let events = if step == 0 || !heard.contains("Pass again to read it once more") {
            tool_turn(&id, "read", json!({"path": "src/lib.rs"}))
        } else {
            text_turn("1847")
        };
        Box::pin(async move {
            Ok(Box::pin(stream::iter(events.into_iter().map(Ok))) as ModelEventStream)
        })
    }
}

#[tokio::test]
async fn a_noop_patch_does_not_pass_until_it_is_refused() {
    let scratch = Scratch::new();
    let case = Case::load(corpus().join("change-one-number")).unwrap();
    let build = |_prompt: String, _tools: Vec<dino_protocol::ToolDefinition>| {
        Ok(Box::new(FixesAfterRefusal {
            profile: profile(),
            step: 0,
        }) as Box<dyn ModelSession>)
    };

    let outcome = run_case(
        &case,
        &scratch.path,
        Arc::new(Unconfined),
        &dino_eval::Variant::default(),
        &build,
    )
    .await
    .unwrap();

    // Drop the refusal and this script stops after the identical patch.
    assert_eq!(outcome.metrics.tool_errors, 1);
    assert_eq!(outcome.metrics.model_turns, 4);
    assert!(outcome.passed, "failures: {:?}", outcome.failures);
}

#[tokio::test]
async fn a_reread_does_not_pass_until_the_range_is_withheld() {
    let scratch = Scratch::new();
    let case = Case::load(corpus().join("answer-from-the-file")).unwrap();
    let build = |_prompt: String, _tools: Vec<dino_protocol::ToolDefinition>| {
        Ok(Box::new(RereadsUntilTold {
            profile: profile(),
            step: 0,
        }) as Box<dyn ModelSession>)
    };

    let outcome = run_case(
        &case,
        &scratch.path,
        Arc::new(Unconfined),
        &dino_eval::Variant::default(),
        &build,
    )
    .await
    .unwrap();

    // Without the note the script reads until the turn limit and never answers.
    assert_eq!(outcome.metrics.tool_calls.get("read"), Some(&2));
    assert_eq!(outcome.metrics.tool_errors, 0);
    assert_eq!(outcome.metrics.model_turns, 3);
    assert!(outcome.passed, "failures: {:?}", outcome.failures);
}
