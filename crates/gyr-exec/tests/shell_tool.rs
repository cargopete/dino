//! The shell tool, unconfined so these measure the tool rather than the sandbox.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use gyr_core::ToolRuntime;
use gyr_exec::ExecLimits;
use gyr_exec::ShellTool;
use gyr_protocol::ToolCall;
use gyr_protocol::ToolClass;
use gyr_sandbox::Unconfined;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "gyrfalcon-shell-test-{}-{name}",
            std::process::id()
        ));
        fs::create_dir_all(path.join("sub")).unwrap();
        Self {
            path: fs::canonicalize(&path).unwrap(),
        }
    }

    fn tool(&self) -> ShellTool {
        ShellTool::new(&self.path, ExecLimits::default(), Arc::new(Unconfined)).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn call(arguments: Value) -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: "shell".into(),
        arguments,
    }
}

async fn output(tool: &ShellTool, arguments: Value) -> Value {
    let output = tool.execute(&call(arguments)).await.unwrap();
    serde_json::from_str(&output.content).unwrap()
}

#[tokio::test]
async fn pipes_redirects_and_and_work() {
    let fixture = Fixture::new("pipes");
    let tool = fixture.tool();

    let result = output(
        &tool,
        json!({"command": "printf 'b\\na\\n' | sort > out.txt && cat out.txt"}),
    )
    .await;

    assert_eq!(result["stdout"], "a\nb\n");
    assert_eq!(result["exit_code"], 0);
    assert_eq!(
        result["command"],
        "printf 'b\\na\\n' | sort > out.txt && cat out.txt"
    );
}

#[tokio::test]
async fn a_directory_inside_the_workspace_is_the_working_directory() {
    let fixture = Fixture::new("dir");
    let result = output(
        &fixture.tool(),
        json!({"command": "pwd", "directory": "sub"}),
    )
    .await;

    assert_eq!(
        result["stdout"].as_str().unwrap().trim(),
        fixture.path.join("sub").display().to_string()
    );
}

#[test]
fn the_whole_line_is_the_approval_subject() {
    let fixture = Fixture::new("class");
    let action = fixture
        .tool()
        .classify(&call(json!({"command": "cargo test 2>&1 | tail -5"})))
        .unwrap();

    assert_eq!(action.class, ToolClass::Process);
    assert_eq!(action.subject.as_deref(), Some("cargo test 2>&1 | tail -5"));
}

#[test]
fn empty_lines_and_escaping_directories_are_refused() {
    let fixture = Fixture::new("refuse");
    let tool = fixture.tool();

    for arguments in [
        json!({"command": "  "}),
        json!({"command": "ls", "directory": "../.."}),
        json!({"command": ["ls"]}),
    ] {
        assert!(
            tool.classify(&call(arguments.clone())).is_err(),
            "{arguments}"
        );
    }
}
