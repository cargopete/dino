//! The github tool, against a stand-in `gh` that prints its arguments.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use gyr_core::ToolRuntime;
use gyr_exec::GithubLimits;
use gyr_exec::GithubTool;
use gyr_protocol::ToolCall;
use gyr_protocol::ToolClass;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "gyrfalcon-github-test-{}-{name}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        let gh = path.join("fake-gh");
        fs::write(&gh, "#!/bin/sh\necho \"$@\"\n").unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            path: fs::canonicalize(&path).unwrap(),
        }
    }

    fn tool(&self) -> GithubTool {
        GithubTool::new(&self.path, GithubLimits::default())
            .unwrap()
            .with_program(self.path.join("fake-gh").display().to_string())
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
        name: "github".into(),
        arguments,
    }
}

async fn ran(tool: &GithubTool, arguments: Value) -> String {
    let output = tool.execute(&call(arguments)).await.unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();
    output["stdout"].as_str().unwrap().trim().to_owned()
}

#[tokio::test]
async fn each_command_maps_to_one_fixed_gh_invocation() {
    let fixture = Fixture::new("map");
    let tool = fixture.tool();

    assert_eq!(
        ran(&tool, json!({"command": "issue_list"})).await,
        "issue list --state open --limit 30"
    );
    assert_eq!(
        ran(&tool, json!({"command": "pr_list", "state": "merged"})).await,
        "pr list --state merged --limit 30"
    );
    assert_eq!(
        ran(
            &tool,
            json!({"command": "issue_view", "number": 58, "comments": true})
        )
        .await,
        "issue view 58 --comments"
    );
    assert_eq!(
        ran(&tool, json!({"command": "pr_diff", "number": 57})).await,
        "pr diff 57"
    );
}

#[test]
fn calls_are_read_only() {
    let fixture = Fixture::new("class");
    let action = fixture
        .tool()
        .classify(&call(json!({"command": "pr_view", "number": 1})))
        .unwrap();

    assert_eq!(action.class, ToolClass::ReadOnly);
}

#[test]
fn malformed_requests_are_refused_before_anything_runs() {
    let fixture = Fixture::new("refuse");
    let tool = fixture.tool();

    for arguments in [
        json!({"command": "issue_view"}),
        json!({"command": "issue_view", "number": 0}),
        json!({"command": "issue_list", "number": 3}),
        json!({"command": "issue_list", "state": "merged"}),
        json!({"command": "issue_close", "number": 3}),
        json!({"command": "pr_view", "number": 1, "repo": "someone/else"}),
    ] {
        assert!(
            tool.classify(&call(arguments.clone())).is_err(),
            "{arguments}"
        );
    }
}
