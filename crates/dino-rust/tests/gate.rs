//! The diagnostic gate against a real toolchain.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use dino_core::ToolRuntime;
use dino_protocol::ToolCall;
use dino_protocol::ToolClass;
use dino_protocol::ToolOutput;
use dino_rust::CargoLimits;
use dino_rust::GateTool;
use dino_sandbox::Unconfined;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

static WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(body: &str) -> Self {
        let serial = WORKSPACE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("dino-gate-test-{}-{serial}", std::process::id()));
        fs::create_dir_all(path.join("src")).unwrap();
        fs::write(
            path.join("Cargo.toml"),
            "[workspace]\n\
             [package]\n\
             name = \"fixture\"\n\
             version = \"0.1.0\"\n\
             edition = \"2021\"\n",
        )
        .unwrap();
        fs::write(path.join("src/lib.rs"), body).unwrap();
        Self { path }
    }

    fn edit(&self, body: &str) {
        fs::write(self.path.join("src/lib.rs"), body).unwrap();
    }

    fn gate(&self) -> GateTool {
        GateTool::new(&self.path, CargoLimits::default(), Arc::new(Unconfined)).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn call(command: &str) -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: "gate".into(),
        arguments: json!({"command": command}),
    }
}

fn parse(output: &ToolOutput) -> Value {
    assert!(
        !output.is_error,
        "gate reported an error: {}",
        output.content
    );
    serde_json::from_str(&output.content).unwrap()
}

/// Two independent errors, so fixing one is measurable progress.
const TWO_ERRORS: &str = "pub fn one() -> u32 {\n    \"one\"\n}\n\
                          pub fn two() -> u32 {\n    \"two\"\n}\n";
const ONE_ERROR: &str = "pub fn one() -> u32 {\n    1\n}\n\
                         pub fn two() -> u32 {\n    \"two\"\n}\n";
const NO_ERRORS: &str = "pub fn one() -> u32 {\n    1\n}\n\
                         pub fn two() -> u32 {\n    2\n}\n";

#[tokio::test]
async fn a_batch_reports_improving_then_green() {
    let fixture = Fixture::new(TWO_ERRORS);
    let gate = fixture.gate();

    let start = parse(&gate.execute(&call("start")).await.unwrap());
    assert_eq!(start["baseline"]["errors"], 2);
    assert!(start["files_fingerprinted"].as_u64().unwrap() >= 1);

    fixture.edit(ONE_ERROR);
    let improving = parse(&gate.execute(&call("check")).await.unwrap());
    assert_eq!(improving["verdict"], "improving");
    assert_eq!(improving["current"]["errors"], 1);
    assert_eq!(improving["resolved_since_last_check"], 1);
    assert_eq!(improving["files_changed"], 1);

    fixture.edit(NO_ERRORS);
    let green = parse(&gate.execute(&call("check")).await.unwrap());
    assert_eq!(green["verdict"], "green");
    assert_eq!(green["current"]["errors"], 0);
}

/// Two errors of different kinds, so the distinct-identity path is measured as
/// well as the multiplicity one that `TWO_ERRORS` exercises.
const TWO_KINDS: &str = "pub fn one() -> u32 {\n    \"one\"\n}\n\
                         pub fn two() -> u32 {\n    missing\n}\n";

#[tokio::test]
async fn errors_of_different_kinds_are_counted_separately() {
    let fixture = Fixture::new(TWO_KINDS);
    let gate = fixture.gate();
    let start = parse(&gate.execute(&call("start")).await.unwrap());
    assert_eq!(start["baseline"]["errors"], 2);

    fixture.edit("pub fn one() -> u32 {\n    \"one\"\n}\npub fn two() -> u32 {\n    2\n}\n");
    let report = parse(&gate.execute(&call("check")).await.unwrap());

    assert_eq!(report["verdict"], "improving");
    assert_eq!(report["resolved_since_last_check"], 1);
    assert_eq!(report["introduced_since_last_check"], 0);
    let resolved = report["resolved"].as_array().unwrap();
    assert_eq!(resolved.len(), 1);
    assert!(
        resolved[0].as_str().unwrap().contains("E0425"),
        "the resolved one should be named: {resolved:?}"
    );
}

#[tokio::test]
async fn a_check_with_no_edits_in_between_stalls() {
    let fixture = Fixture::new(TWO_ERRORS);
    let gate = fixture.gate();
    gate.execute(&call("start")).await.unwrap();

    let first = parse(&gate.execute(&call("check")).await.unwrap());

    assert_eq!(first["verdict"], "stalled");
    assert_eq!(first["files_changed"], 0);
    assert_eq!(first["resolved_since_last_check"], 0);
}

#[tokio::test]
async fn a_second_consecutive_stall_is_exhausted() {
    let fixture = Fixture::new(TWO_ERRORS);
    let gate = fixture.gate();
    gate.execute(&call("start")).await.unwrap();

    let first = parse(&gate.execute(&call("check")).await.unwrap());
    let second = parse(&gate.execute(&call("check")).await.unwrap());

    // Two, not one: a single stalled check is an ordinary step in a multi-site
    // change where the next edit unblocks a cascade.
    assert_eq!(first["verdict"], "stalled");
    assert_eq!(second["verdict"], "exhausted");
}

#[tokio::test]
async fn an_edit_that_makes_it_worse_reports_regressing() {
    let fixture = Fixture::new(ONE_ERROR);
    let gate = fixture.gate();
    gate.execute(&call("start")).await.unwrap();

    fixture.edit(TWO_ERRORS);
    let report = parse(&gate.execute(&call("check")).await.unwrap());

    assert_eq!(report["verdict"], "regressing");
    assert_eq!(report["introduced_since_last_check"], 1);
    assert!(
        report["message"].as_str().unwrap().contains("revert"),
        "a regression should say what to do: {}",
        report["message"]
    );
}

#[tokio::test]
async fn a_green_build_that_changed_nothing_is_not_success() {
    let fixture = Fixture::new(NO_ERRORS);
    let gate = fixture.gate();
    gate.execute(&call("start")).await.unwrap();

    let report = parse(&gate.execute(&call("check")).await.unwrap());

    // The whole point of section 5. A model that reports this as success is
    // reporting somebody else's.
    assert_eq!(report["verdict"], "unchanged");
    assert_eq!(report["current"]["errors"], 0);
    assert_eq!(report["files_changed"], 0);
    // The message must name its own blind spot. A live run caught a model
    // taking its baseline after finishing the work, where the old wording
    // ("nothing was fixed here") was flatly untrue.
    assert!(
        report["message"]
            .as_str()
            .unwrap()
            .contains("cannot see edits made before start"),
        "said: {}",
        report["message"]
    );
}

#[tokio::test]
async fn an_edit_and_its_exact_reversal_is_no_change() {
    let fixture = Fixture::new(NO_ERRORS);
    let gate = fixture.gate();
    gate.execute(&call("start")).await.unwrap();

    fixture.edit(TWO_ERRORS);
    fixture.edit(NO_ERRORS);
    let report = parse(&gate.execute(&call("check")).await.unwrap());

    // Fingerprints rather than an edit count: two edits, no change.
    assert_eq!(report["files_changed"], 0);
    assert_eq!(report["verdict"], "unchanged");
}

#[tokio::test]
async fn a_diagnostic_that_only_moved_is_the_same_diagnostic() {
    // The second function keeps its error; the first gains a line above it, so
    // the surviving error's line number shifts.
    let fixture = Fixture::new(ONE_ERROR);
    let gate = fixture.gate();
    gate.execute(&call("start")).await.unwrap();

    fixture.edit(
        "// a new line at the top\n\
         pub fn one() -> u32 {\n    1\n}\n\
         pub fn two() -> u32 {\n    \"two\"\n}\n",
    );
    let report = parse(&gate.execute(&call("check")).await.unwrap());

    assert_eq!(report["current"]["errors"], 1);
    assert_eq!(
        report["resolved_since_last_check"], 0,
        "the error moved down a line; it was not resolved and reintroduced"
    );
    assert_eq!(report["introduced_since_last_check"], 0);
    assert_eq!(report["files_changed"], 1);
    assert_eq!(report["verdict"], "stalled");
}

#[tokio::test]
async fn check_before_start_is_refused() {
    let fixture = Fixture::new(NO_ERRORS);

    let error = fixture.gate().execute(&call("check")).await.unwrap_err();

    assert!(
        error.to_string().contains("call the gate with start"),
        "said: {error}"
    );
}

#[tokio::test]
async fn status_reports_the_last_verdict_without_running_anything() {
    let fixture = Fixture::new(NO_ERRORS);
    let gate = fixture.gate();
    gate.execute(&call("start")).await.unwrap();
    gate.execute(&call("check")).await.unwrap();

    let status = parse(&gate.execute(&call("status")).await.unwrap());

    assert_eq!(status["phase"], "check");
    assert_eq!(status["verdict"], "unchanged");
}

#[test]
fn start_and_check_are_processes_and_status_is_not() {
    let fixture = Fixture::new(NO_ERRORS);
    let gate = fixture.gate();

    let start = gate.classify(&call("start")).unwrap();
    let check = gate.classify(&call("check")).unwrap();
    let status = gate.classify(&call("status")).unwrap();

    assert_eq!(start.class, ToolClass::Process);
    assert_eq!(check.class, ToolClass::Process);
    // It runs nothing, so it is nobody's business but the model's.
    assert_eq!(status.class, ToolClass::ReadOnly);
    let subject = check.subject.unwrap();
    assert!(subject.starts_with("gate check ("), "{subject}");
    assert!(subject.contains("cargo check --workspace"), "{subject}");
}

#[test]
fn an_unknown_command_is_refused_rather_than_ignored() {
    let fixture = Fixture::new(NO_ERRORS);

    let error = fixture.gate().classify(&call("finish")).unwrap_err();

    assert!(
        error.to_string().contains("invalid gate arguments"),
        "said: {error}"
    );
}

#[tokio::test]
async fn a_broken_build_is_unfinished_work_and_a_clean_one_is_not() {
    use dino_core::FinishCheck;

    let fixture =
        Fixture::new("pub fn one() -> u32 { \"one\" }\npub fn two() -> u32 { \"two\" }\n");
    let cargo =
        dino_rust::CargoTool::new(&fixture.path, CargoLimits::default(), Arc::new(Unconfined))
            .unwrap();
    let check = dino_rust::BuildCheck::new(cargo);

    let note = check.unfinished().await.expect("two type errors");
    assert!(note.contains("reports 2 error(s)"), "{note}");
    assert!(note.contains("src/lib.rs:1: mismatched types"), "{note}");

    fixture.edit("pub fn one() -> u32 { 1 }\npub fn two() -> u32 { 2 }\n");
    assert_eq!(check.unfinished().await, None);
}
