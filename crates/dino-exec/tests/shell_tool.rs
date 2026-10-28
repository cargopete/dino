//! The shell tool, unconfined so these measure the tool rather than the sandbox.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use dino_core::ToolRuntime;
use dino_exec::ExecLimits;
use dino_exec::ShellTool;
use dino_protocol::ToolCall;
use dino_protocol::ToolClass;
use dino_sandbox::Unconfined;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("dino-shell-test-{}-{name}", std::process::id()));
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

#[tokio::test]
async fn a_backgrounded_process_ends_with_the_call_and_does_not_hold_it_open() {
    let fixture = Fixture::new("orphan");
    let tool = fixture.tool();
    // A killed process can linger briefly as a zombie until it is reaped, and
    // `kill -0` still finds a zombie, so give it a moment to go.
    let alive = |pid: &str| {
        (0..20).all(|_| {
            let found = std::process::Command::new("kill")
                .args(["-0", pid])
                .status()
                .unwrap()
                .success();
            if found {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            found
        })
    };

    for command in [
        // Detached from the pipes, as a server started in the background is.
        "sleep 60 > /dev/null 2>&1 & echo $!",
        // Still holding the pipes, which used to keep the call waiting.
        "sleep 60 & echo $!",
    ] {
        let started = std::time::Instant::now();
        let result = output(&tool, json!({ "command": command })).await;
        let pid = result["stdout"].as_str().unwrap().trim().to_owned();

        assert!(!pid.is_empty(), "{result}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{command} held the call for {:?}",
            started.elapsed()
        );
        assert!(!alive(&pid), "{command}: sleep {pid} outlived the call");
    }
}

/// Unconfined, but saying the network is denied, so the policy's effect on a
/// command's environment and result can be checked without a real sandbox.
#[derive(Debug)]
struct NetworkDenied;

impl dino_sandbox::Sandbox for NetworkDenied {
    fn label(&self) -> String {
        "network denied".to_owned()
    }
    fn confines_writes(&self) -> bool {
        false
    }
    fn denies_network(&self) -> bool {
        true
    }
    fn temp_dir(&self) -> Option<&std::path::Path> {
        None
    }
    fn wrap(
        &self,
        program: &str,
        arguments: &[String],
    ) -> Result<dino_sandbox::WrappedCommand, dino_sandbox::SandboxError> {
        Unconfined.wrap(program, arguments)
    }
}

#[tokio::test]
async fn a_denied_download_says_it_is_policy_and_names_the_way_through() {
    let fixture = Fixture::new("network-note");
    let tool = ShellTool::new(
        &fixture.path,
        ExecLimits::default(),
        Arc::new(NetworkDenied),
    )
    .unwrap();

    let failed = output(
        &tool,
        json!({"command": "echo 'npm error code ENOTFOUND' >&2; exit 1"}),
    )
    .await;
    let note = failed["note"]
        .as_str()
        .expect("a note on a denied download");
    assert!(note.contains("policy") && note.contains("fetch"), "{note}");

    let fine = output(&tool, json!({"command": "echo 'ENOTFOUND is just a word'"})).await;
    assert!(
        fine.get("note").is_none(),
        "a command that succeeded needs no note"
    );
}

#[tokio::test]
async fn package_managers_are_told_to_work_offline_only_when_the_network_is_denied() {
    let fixture = Fixture::new("offline-env");
    let probe = json!({"command": "echo $CARGO_NET_OFFLINE,$npm_config_offline,$GOPROXY"});

    let denied = ShellTool::new(
        &fixture.path,
        ExecLimits::default(),
        Arc::new(NetworkDenied),
    )
    .unwrap();
    assert_eq!(
        output(&denied, probe.clone()).await["stdout"],
        "true,true,off\n"
    );

    let open = fixture.tool();
    assert_eq!(output(&open, probe.clone()).await["stdout"], ",,\n");
    let failed = output(&open, json!({"command": "echo ENOTFOUND >&2; exit 1"})).await;
    assert!(
        failed.get("note").is_none(),
        "no policy to explain without a sandbox"
    );
}
