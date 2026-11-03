//! After-edit hooks, around a stand-in file tool.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_exec::AfterEdit;
use dino_exec::Hook;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use dino_sandbox::Unconfined;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

/// Writes `edited` into the named file, as `write_file` would.
struct Writer {
    root: PathBuf,
    rewritten: Arc<Mutex<Vec<String>>>,
}

impl ToolRuntime for Writer {
    fn definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    fn classify(&self, _call: &ToolCall) -> Result<ToolAction, ToolError> {
        Ok(ToolAction::mutating("x"))
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let path = self.root.join(call.arguments["path"].as_str().unwrap());
        Box::pin(async move {
            fs::write(path, "edited").unwrap();
            Ok(ToolOutput::success(json!({"written": true}).to_string()))
        })
    }

    fn rewritten(&self, path: &str) {
        self.rewritten.lock().unwrap().push(path.to_owned());
    }
}

fn fixture(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("dino-hooks-{}-{name}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    fs::canonicalize(root).unwrap()
}

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: name.into(),
        arguments: json!({"path": "a.txt"}),
    }
}

fn hooked(root: &PathBuf, hooks: Vec<Hook>) -> AfterEdit {
    hooked_telling(root, hooks, Arc::default())
}

fn hooked_telling(
    root: &PathBuf,
    hooks: Vec<Hook>,
    rewritten: Arc<Mutex<Vec<String>>>,
) -> AfterEdit {
    AfterEdit::new(
        Box::new(Writer {
            root: root.clone(),
            rewritten,
        }),
        hooks,
        root,
        Arc::new(Unconfined),
    )
}

fn argv(parts: &[&str]) -> Hook {
    Hook {
        command: parts.iter().map(|part| (*part).to_owned()).collect(),
        extension: None,
    }
}

#[tokio::test]
async fn a_hook_that_changes_the_file_says_so() {
    let root = fixture("change");
    let rewritten = Arc::default();
    let tool = hooked_telling(
        &root,
        vec![argv(&["sh", "-c", "printf formatted > {path}"])],
        Arc::clone(&rewritten),
    );

    let output = tool.execute(&call("write_file")).await.unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();

    assert_eq!(output["written"], true);
    assert_eq!(output["hooks"]["after_edit"][0]["exit_code"], 0);
    assert_eq!(output["hooks"]["changed_file"], true);
    assert!(
        output["hooks"]["note"]
            .as_str()
            .unwrap()
            .contains("as it is now")
    );
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "formatted");
    assert_eq!(
        *rewritten.lock().unwrap(),
        ["a.txt"],
        "the file tool is told"
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_failing_hook_reports_its_output_and_the_edit_stands() {
    let root = fixture("fail");
    let tool = hooked(
        &root,
        vec![argv(&["sh", "-c", "echo bad syntax >&2; exit 3"])],
    );

    let output = tool.execute(&call("apply_patch")).await.unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();

    assert_eq!(output["hooks"]["after_edit"][0]["exit_code"], 3);
    assert_eq!(output["hooks"]["after_edit"][0]["output"], "bad syntax");
    assert_eq!(output["hooks"]["changed_file"], false);
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "edited");
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_hook_runs_only_on_a_file_that_was_already_clean_under_it() {
    let root = fixture("clean");
    let tool = hooked(
        &root,
        vec![argv(&[
            "sh",
            "-c",
            "printf formatted > \"$1\"",
            "sh",
            "{path}",
        ])],
    );

    fs::write(root.join("a.txt"), "messy").unwrap();
    let output = tool.execute(&call("apply_patch")).await.unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();
    assert!(
        output["hooks"]["after_edit"][0]["skipped"]
            .as_str()
            .unwrap()
            .contains("not clean"),
        "{output}"
    );
    assert_eq!(output["hooks"]["changed_file"], false);
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "edited");

    fs::write(root.join("a.txt"), "formatted").unwrap();
    let output = tool.execute(&call("apply_patch")).await.unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(output["hooks"]["after_edit"][0]["exit_code"], 0, "{output}");
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "formatted");
    let leftovers = fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("dino-hook-{}-", std::process::id()))
        })
        .count();
    assert_eq!(leftovers, 0, "the copies are removed");
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn the_edition_placeholder_is_the_packages_edition_and_only_ever_a_year() {
    let root = fixture("edition");
    let hook = argv(&[
        "sh",
        "-c",
        "printf \"$2\" > \"$1\"",
        "sh",
        "{path}",
        "{edition}",
    ]);
    let tool = hooked(&root, vec![hook]);

    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"a\"\nedition = \"2021\"\n",
    )
    .unwrap();
    tool.execute(&call("write_file")).await.unwrap();
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "2021");

    fs::remove_file(root.join("a.txt")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"a\"\nedition = \"2021; rm -rf ~\"\n",
    )
    .unwrap();
    let output = tool.execute(&call("write_file")).await.unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();
    assert!(
        output["hooks"]["after_edit"][0]["error"]
            .as_str()
            .unwrap()
            .contains("not a year"),
        "{output}"
    );
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "edited");
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn other_tools_run_no_hooks() {
    let root = fixture("other");
    let tool = hooked(&root, vec![argv(&["sh", "-c", "exit 1"])]);

    let output = tool.execute(&call("read")).await.unwrap();

    assert!(!output.content.contains("hooks"), "{}", output.content);
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_hook_for_another_extension_does_not_run() {
    let root = fixture("extension");
    let mut hook = argv(&["sh", "-c", "exit 1"]);
    hook.extension = Some("rs".into());
    let tool = hooked(&root, vec![hook]);

    let output = tool.execute(&call("write_file")).await.unwrap();

    assert!(!output.content.contains("hooks"), "{}", output.content);
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_failing_end_turn_hook_calls_the_work_unfinished() {
    use dino_core::FinishCheck;
    use dino_exec::EndTurnHooks;

    let root = fixture("endturn");
    let failing = EndTurnHooks::new(
        vec![argv(&["sh", "-c", "echo 'clippy: 2 warnings' >&2; exit 1"])],
        &root,
        Arc::new(Unconfined),
    );
    let passing = EndTurnHooks::new(vec![argv(&["true"])], &root, Arc::new(Unconfined));

    let unfinished = failing.unfinished().await.unwrap();
    assert!(unfinished.contains("clippy: 2 warnings"), "{unfinished}");
    assert!(unfinished.contains("exited 1"), "{unfinished}");
    assert_eq!(passing.unfinished().await, None);
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn start_hooks_report_their_output() {
    let root = fixture("start");
    let said = dino_exec::run_start_hooks(
        &[argv(&["sh", "-c", "echo today is tuesday"])],
        &root,
        &(Arc::new(Unconfined) as Arc<dyn dino_sandbox::Sandbox>),
    )
    .await;

    assert_eq!(
        said,
        [(
            "sh -c echo today is tuesday".to_owned(),
            "today is tuesday".to_owned()
        )]
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn task_end_hooks_are_told_how_the_run_ended() {
    let root = fixture("task-end");
    let ended = dino_exec::TaskEnd {
        status: "failed".into(),
        message: "dino failed after 36m; boom".into(),
    };
    let hook = argv(&[
        "sh",
        "-c",
        "printf '%s|%s|%s' \"$DINO_STATUS\" \"$DINO_MESSAGE\" \"$1\" > told.txt",
        "sh",
        "{status}:{message}",
    ]);

    let problems = dino_exec::run_task_end_hooks(&[hook], &root, &ended).await;

    assert_eq!(problems, Vec::<String>::new());
    assert_eq!(
        fs::read_to_string(root.join("told.txt")).unwrap(),
        "failed|dino failed after 36m; boom|failed:dino failed after 36m; boom"
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_failing_task_end_hook_is_reported_without_its_arguments() {
    let root = fixture("task-end-fails");
    let ended = dino_exec::TaskEnd {
        status: "finished".into(),
        message: "done".into(),
    };
    let failing = argv(&[
        "sh",
        "-c",
        "exit 22",
        "sh",
        "https://example.test/hook/SECRET",
    ]);
    let missing = argv(&["dino-no-such-program", "SECRET"]);
    let passing = argv(&["true"]);

    let problems = dino_exec::run_task_end_hooks(&[failing, missing, passing], &root, &ended).await;

    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(problems[0].contains("`sh` exited 22"), "{problems:?}");
    assert!(problems[1].contains("dino-no-such-program"), "{problems:?}");
    assert!(
        problems.iter().all(|problem| !problem.contains("SECRET")),
        "{problems:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn hooks_wait_for_the_turns_last_edit_to_a_file() {
    let root = fixture("deferred");
    let tool = hooked(&root, vec![argv(&["sh", "-c", "echo ran >> runs.log"])]);
    let first = call("apply_patch");
    let second = ToolCall {
        id: "call-2".into(),
        ..call("apply_patch")
    };
    tool.begin_turn(&[first.clone(), second.clone()]);

    let output = tool.execute(&first).await.unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();
    assert!(output["hooks"]["deferred"].is_string(), "{output}");
    assert!(!root.join("runs.log").exists(), "the hook ran early");

    let output = tool.execute(&second).await.unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(output["hooks"]["after_edit"][0]["exit_code"], 0, "{output}");
    assert_eq!(fs::read_to_string(root.join("runs.log")).unwrap(), "ran\n");
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_commit_waits_for_the_verify_command_and_a_failure_refuses_it() {
    use dino_core::FinishCheck;
    use dino_exec::BeforeCommit;
    use dino_exec::Verify;

    let root = fixture("verify");
    let verify = |script: &str| {
        Verify::new(
            vec!["sh".into(), "-c".into(), script.into()],
            &root,
            Arc::new(Unconfined),
        )
    };
    let commit = ToolCall {
        id: "call-1".into(),
        name: "shell".into(),
        arguments: json!({"path": "a.txt", "command": "git add -A && git commit -m x"}),
    };

    let failing = BeforeCommit::new(
        Box::new(Writer {
            root: root.clone(),
            rewritten: Arc::default(),
        }),
        verify("echo '2 warnings' >&2; exit 1"),
    );
    let refused = failing.execute(&commit).await.unwrap();
    assert!(refused.is_error);
    assert!(
        refused.content.contains("Not committed") && refused.content.contains("2 warnings"),
        "{}",
        refused.content
    );
    assert!(!root.join("a.txt").exists(), "the commit did not run");
    let unfinished = verify("exit 1").unfinished().await.unwrap();
    assert!(unfinished.contains("verify command"), "{unfinished}");

    let passing = BeforeCommit::new(
        Box::new(Writer {
            root: root.clone(),
            rewritten: Arc::default(),
        }),
        verify("true"),
    );
    assert!(!passing.execute(&commit).await.unwrap().is_error);
    assert!(root.join("a.txt").exists());
    assert_eq!(verify("true").unfinished().await, None);
    let _ = fs::remove_dir_all(&root);
}
