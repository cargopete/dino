//! After-edit hooks, around a stand-in file tool.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

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
    AfterEdit::new(
        Box::new(Writer { root: root.clone() }),
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
    let tool = hooked(
        &root,
        vec![argv(&["sh", "-c", "printf formatted > {path}"])],
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
            .contains("read it again")
    );
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "formatted");
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
