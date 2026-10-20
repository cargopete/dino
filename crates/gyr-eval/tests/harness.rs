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

use futures_util::stream;
use gyr_eval::Case;
use gyr_eval::run_case;
use gyr_model::ModelError;
use gyr_model::ModelEventStream;
use gyr_model::ModelFuture;
use gyr_model::ModelSession;
use gyr_protocol::ModelEvent;
use gyr_protocol::ModelProfile;
use gyr_protocol::StopReason;
use gyr_protocol::ToolCall;
use gyr_protocol::TurnInput;
use gyr_sandbox::Unconfined;
use pretty_assertions::assert_eq;
use serde_json::json;

static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let serial = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gyrfalcon-eval-test-{}-{serial}",
            std::process::id()
        ));
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
            usage: gyr_protocol::TokenUsage {
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

async fn run(case: &Case, scratch: &Scratch, turns: Vec<Vec<ModelEvent>>) -> gyr_eval::Outcome {
    let build = |_prompt: String, _tools: Vec<gyr_protocol::ToolDefinition>| {
        Ok(Box::new(Scripted {
            profile: gyr_model::builtin_profiles().remove(0),
            turns: VecDeque::from(turns.clone()),
        }) as Box<dyn ModelSession>)
    };
    run_case(
        case,
        &scratch.path,
        Arc::new(Unconfined),
        &gyr_eval::Variant::default(),
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
    case.expect = gyr_eval::Expectations::default();
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
    case.expect = gyr_eval::Expectations::default();
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

async fn answer_two_with(case: &Case, scratch: &Scratch, value: &str) -> gyr_eval::Outcome {
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
