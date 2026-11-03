//! The github tool, against a stand-in `gh` that prints its arguments.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use dino_core::ToolRuntime;
use dino_exec::GithubLimits;
use dino_exec::GithubTool;
use dino_protocol::ToolCall;
use dino_protocol::ToolClass;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("dino-github-test-{}-{name}", std::process::id()));
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

#[tokio::test]
async fn ci_commands_map_to_read_only_gh_invocations() {
    let fixture = Fixture::new("ci");
    let tool = fixture.tool();

    assert_eq!(
        ran(&tool, json!({"command": "pr_checks", "number": 240})).await,
        "pr checks 240"
    );
    assert_eq!(
        ran(
            &tool,
            json!({"command": "run_list", "branch": "pete/long-tasks"})
        )
        .await,
        "run list --limit 30 --branch=pete/long-tasks"
    );
    assert_eq!(
        ran(
            &tool,
            json!({"command": "run_view", "number": 19_034_567_890_u64, "log_failed": true})
        )
        .await,
        "run view 19034567890 --log-failed"
    );
    for arguments in [
        json!({"command": "run_list", "branch": "--repo=someone/else"}),
        json!({"command": "pr_checks", "number": 1, "branch": "main"}),
        json!({"command": "pr_view", "number": 1, "log_failed": true}),
        json!({"command": "pr_checks", "number": 1, "timeout": 60}),
        json!({"command": "checks_wait", "number": 1, "timeout": 7_200}),
        json!({"command": "checks_wait"}),
    ] {
        assert!(
            tool.classify(&call(arguments.clone())).is_err(),
            "{arguments}"
        );
    }
}

#[tokio::test]
async fn checks_wait_polls_until_no_check_is_pending() {
    let fixture = Fixture::new("wait");
    let gh = fixture.path.join("pending-gh");
    // Pending, which gh says by exiting 8, until the third poll.
    fs::write(
        &gh,
        "#!/bin/sh\nn=$(cat polls 2>/dev/null || echo 0); n=$((n+1)); echo $n > polls\n\
         echo \"poll $n\"; [ $n -ge 3 ] || exit 8\n",
    )
    .unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let limits = GithubLimits {
        poll: Duration::from_millis(10),
        ..GithubLimits::default()
    };
    let tool = GithubTool::new(&fixture.path, limits)
        .unwrap()
        .with_program(gh.display().to_string());
    let wait = |arguments: Value| {
        let tool = &tool;
        async move {
            let output = tool.execute(&call(arguments)).await.unwrap();
            serde_json::from_str::<Value>(&output.content).unwrap()
        }
    };

    let passed = wait(json!({"command": "checks_wait", "number": 7})).await;
    assert_eq!(passed["stdout"], "poll 3\n");
    assert_eq!(passed["exit_code"], 0);
    assert!(passed.get("note").is_none());

    fs::write(fixture.path.join("polls"), "-1000000").unwrap();
    let pending = wait(json!({"command": "checks_wait", "number": 7, "timeout": 1})).await;
    assert_eq!(pending["exit_code"], 8);
    assert!(pending["note"].as_str().unwrap().contains("still pending"));
}
