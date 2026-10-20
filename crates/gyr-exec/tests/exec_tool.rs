//! The exec tool, with and without containment.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gyr_core::ToolRuntime;
use gyr_exec::ExecLimits;
use gyr_exec::ExecTool;
use gyr_protocol::ToolCall;
use gyr_protocol::ToolClass;
use gyr_protocol::ToolOutput;
use gyr_sandbox::Sandbox;
use gyr_sandbox::Unconfined;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

static WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let serial = WORKSPACE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gyrfalcon-exec-test-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(path.join("scripts")).unwrap();
        Self {
            path: fs::canonicalize(&path).unwrap(),
        }
    }

    /// Unconfined, so these tests measure the tool rather than the sandbox.
    fn tool(&self) -> ExecTool {
        self.tool_with(ExecLimits::default(), Arc::new(Unconfined))
    }

    fn tool_with(&self, limits: ExecLimits, sandbox: Arc<dyn Sandbox>) -> ExecTool {
        ExecTool::new(&self.path, limits, sandbox).unwrap()
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn sandboxed(&self) -> ExecTool {
        let sandbox = gyr_sandbox::detect(&self.path).expect("a sandbox on this platform");
        self.tool_with(ExecLimits::default(), Arc::from(sandbox))
    }

    fn script(&self, name: &str, body: &str) -> String {
        let path = self.path.join("scripts").join(name);
        fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        format!("scripts/{name}")
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
        name: "exec".into(),
        arguments,
    }
}

fn parse(output: &ToolOutput) -> Value {
    assert!(
        !output.is_error,
        "tool reported an error: {}",
        output.content
    );
    serde_json::from_str(&output.content).unwrap()
}

#[tokio::test]
async fn a_successful_command_returns_its_output_and_exit_code() {
    let fixture = Fixture::new();

    let output = fixture
        .tool()
        .execute(&call(json!({"command": ["echo", "hello there"]})))
        .await
        .unwrap();
    let output = parse(&output);

    assert_eq!(output["exit_code"], 0);
    assert_eq!(output["stdout"], "hello there\n");
    assert_eq!(output["stderr"], "");
    assert_eq!(output["timed_out"], false);
    assert_eq!(output["command"], "echo hello there");
}

#[tokio::test]
async fn the_two_streams_are_kept_apart() {
    let fixture = Fixture::new();
    let script = fixture.script(
        "streams.sh",
        "#!/bin/sh\necho to-stdout\necho to-stderr >&2\nexit 3\n",
    );

    let output = fixture
        .tool()
        .execute(&call(json!({"command": [script]})))
        .await
        .unwrap();
    let output = parse(&output);

    assert_eq!(output["exit_code"], 3);
    assert_eq!(output["stdout"], "to-stdout\n");
    assert_eq!(output["stderr"], "to-stderr\n");
}

#[tokio::test]
async fn a_working_directory_is_resolved_inside_the_workspace() {
    let fixture = Fixture::new();

    let output = fixture
        .tool()
        .execute(&call(json!({"command": ["pwd"], "directory": "scripts"})))
        .await
        .unwrap();
    let output = parse(&output);

    assert_eq!(
        output["stdout"].as_str().unwrap().trim(),
        fixture.path.join("scripts").display().to_string()
    );
}

#[test]
fn a_working_directory_outside_the_workspace_never_reaches_a_policy() {
    let fixture = Fixture::new();

    let error = fixture
        .tool()
        .classify(&call(
            json!({"command": ["pwd"], "directory": "../elsewhere"}),
        ))
        .unwrap_err();

    assert!(
        error.to_string().contains("remain inside the workspace"),
        "said: {error}"
    );
}

#[test]
fn a_relative_program_path_cannot_climb_out_of_the_workspace() {
    let fixture = Fixture::new();

    let error = fixture
        .tool()
        .classify(&call(json!({"command": ["../../bin/sh"]})))
        .unwrap_err();

    assert!(
        error.to_string().contains("remain inside the workspace"),
        "said: {error}"
    );
}

#[test]
fn an_absolute_program_path_is_passed_through() {
    let fixture = Fixture::new();

    let action = fixture
        .tool()
        .classify(&call(json!({"command": ["/usr/bin/env"]})))
        .unwrap();

    // Refusing this while allowing `env` by way of PATH would be theatre: the
    // same binary, reached two ways. The sandbox governs what it may do.
    assert_eq!(action.subject.as_deref(), Some("/usr/bin/env"));
}

#[test]
fn an_empty_command_is_refused_before_anything_is_spawned() {
    let fixture = Fixture::new();

    let error = fixture
        .tool()
        .classify(&call(json!({"command": []})))
        .unwrap_err();

    assert!(
        error.to_string().contains("at least a program"),
        "said: {error}"
    );
}

#[test]
fn an_unknown_field_is_refused_rather_than_ignored() {
    let fixture = Fixture::new();

    let error = fixture
        .tool()
        .classify(&call(json!({"command": ["true"], "shell": true})))
        .unwrap_err();

    assert!(
        error.to_string().contains("invalid exec arguments"),
        "said: {error}"
    );
}

#[test]
fn exec_is_a_process_and_its_subject_is_the_argument_vector() {
    let fixture = Fixture::new();

    let action = fixture
        .tool()
        .classify(&call(
            json!({"command": ["git", "log", "--oneline", "-20"]}),
        ))
        .unwrap();

    assert_eq!(action.class, ToolClass::Process);
    assert_eq!(action.subject.as_deref(), Some("git log --oneline -20"));
}

#[test]
fn a_session_rule_for_one_command_does_not_cover_another() {
    let fixture = Fixture::new();
    let tool = fixture.tool();

    let status = tool
        .classify(&call(json!({"command": ["git", "status"]})))
        .unwrap();
    let push = tool
        .classify(&call(json!({"command": ["git", "push"]})))
        .unwrap();

    assert_ne!(status.rule_key("exec"), push.rule_key("exec"));
}

#[tokio::test]
async fn the_wall_clock_kills_a_sleeping_child() {
    let fixture = Fixture::new();
    let limits = ExecLimits {
        timeout: Duration::from_secs(2),
        ..ExecLimits::default()
    };

    let output = fixture
        .tool_with(limits, Arc::new(Unconfined))
        .execute(&call(json!({"command": ["sleep", "120"]})))
        .await
        .unwrap();
    let output = parse(&output);

    assert_eq!(output["timed_out"], true);
    assert_eq!(output["exit_code"], Value::Null);
    assert!(
        output["stderr"]
            .as_str()
            .unwrap()
            .contains("killed after 2")
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn a_confined_command_cannot_write_outside_the_workspace() {
    let fixture = Fixture::new();
    let escape = std::env::temp_dir().join(format!("gyrfalcon-exec-escape-{}", std::process::id()));
    let _ = fs::remove_file(&escape);
    let script = fixture.script(
        "escape.sh",
        &format!("#!/bin/sh\necho escaped > '{}'\n", escape.display()),
    );

    let output = fixture
        .sandboxed()
        .execute(&call(json!({"command": [script]})))
        .await
        .unwrap();
    let output = parse(&output);

    assert_ne!(output["exit_code"], 0, "the write must have failed");
    // Seatbelt refuses with EPERM and Landlock with EACCES, so the test asks
    // for a refusal rather than for one platform's wording.
    let stderr = output["stderr"].as_str().unwrap();
    assert!(
        stderr.contains("Operation not permitted") || stderr.contains("Permission denied"),
        "it failed for some other reason: {stderr}"
    );
    assert!(
        !escape.exists(),
        "a confined command wrote to {}",
        escape.display()
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn a_confined_command_cannot_reach_the_network() {
    let fixture = Fixture::new();

    let output = fixture
        .sandboxed()
        .execute(&call(json!({
            "command": ["/usr/bin/curl", "-sS", "-m", "5", "https://example.com"]
        })))
        .await
        .unwrap();
    let output = parse(&output);

    // This is what makes automatic pushes and purchases impossible under
    // confinement, rather than a list of forbidden program names.
    assert_ne!(output["exit_code"], 0, "curl reached the network");
    assert_eq!(output["stdout"], "");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn a_confined_command_may_still_write_inside_the_workspace() {
    let fixture = Fixture::new();
    let script = fixture.script("inside.sh", "#!/bin/sh\necho written > ./made-by-exec\n");

    let output = fixture
        .sandboxed()
        .execute(&call(json!({"command": [script]})))
        .await
        .unwrap();
    let output = parse(&output);

    assert_eq!(output["exit_code"], 0, "stderr: {}", output["stderr"]);
    assert!(fixture.path.join("made-by-exec").is_file());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn the_temporary_directory_survives_the_workspace_being_cleaned() {
    let fixture = Fixture::new();
    let tool = fixture.sandboxed();
    // An eval replaces the workspace after the sandbox is built, taking this
    // directory with it. Everything that honours TMPDIR then failed.
    fs::remove_dir_all(fixture.path.join(".gyr")).unwrap();
    let script = fixture.script("temp.sh", "#!/bin/sh\necho scratch > \"$TMPDIR/note\"\n");

    let output = tool
        .execute(&call(json!({"command": [script]})))
        .await
        .unwrap();
    let output = parse(&output);

    assert_eq!(output["exit_code"], 0, "stderr: {}", output["stderr"]);
    assert!(fixture.path.join(".gyr/tmp/note").is_file());
}
