//! The Cargo tool against a real toolchain and a throwaway workspace.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use dino_core::ToolRuntime;
use dino_protocol::ToolCall;
use dino_protocol::ToolClass;
use dino_protocol::ToolOutput;
use dino_rust::CargoLimits;
use dino_rust::CargoTool;
use dino_sandbox::Sandbox;
use dino_sandbox::Unconfined;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

static WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A single-package Cargo project with no dependencies, so a check costs a
/// fraction of a second and needs no network.
struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(body: &str) -> Self {
        let serial = WORKSPACE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("dino-cargo-test-{}-{serial}", std::process::id()));
        fs::create_dir_all(path.join("src")).unwrap();
        fs::write(
            path.join("Cargo.toml"),
            // An explicit empty workspace table detaches the fixture from any
            // Cargo workspace that happens to contain the temporary directory.
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

    /// Unconfined, so these tests measure the Cargo tool rather than the
    /// sandbox. Confinement gets its own test below.
    fn tool(&self) -> CargoTool {
        self.tool_with(CargoLimits::default(), Arc::new(Unconfined))
    }

    fn tool_with(&self, limits: CargoLimits, sandbox: Arc<dyn Sandbox>) -> CargoTool {
        CargoTool::new(&self.path, limits, sandbox).unwrap()
    }

    // Gated with its callers. Left ungated it is dead code everywhere the
    // sandbox is unimplemented, which is what CI caught on its first Linux run.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn sandboxed(&self) -> CargoTool {
        let sandbox = dino_sandbox::detect(&self.path).expect("a sandbox on this platform");
        self.tool_with(CargoLimits::default(), Arc::from(sandbox))
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
        name: "cargo".into(),
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
async fn a_clean_check_reports_no_errors() {
    let fixture = Fixture::new("pub fn two() -> u32 { 2 }\n");

    let output = fixture
        .tool()
        .execute(&call(json!({"command": "check"})))
        .await
        .unwrap();
    let output = parse(&output);

    assert_eq!(output["exit_code"], 0);
    assert_eq!(output["counts"]["errors"], 0);
    assert_eq!(output["timed_out"], false);
    assert_eq!(output["dropped_diagnostics"], 0);
    assert!(
        output["command"]
            .as_str()
            .unwrap()
            .contains("--manifest-path Cargo.toml"),
        "the manifest must be explicit: {}",
        output["command"]
    );
}

#[tokio::test]
async fn a_type_error_arrives_parsed_with_its_code_and_location() {
    let fixture = Fixture::new("pub fn two() -> u32 {\n    \"two\"\n}\n");

    let output = fixture
        .tool()
        .execute(&call(json!({"command": "check"})))
        .await
        .unwrap();
    let output = parse(&output);

    assert_ne!(output["exit_code"], 0);
    assert_eq!(output["counts"]["errors"], 1);
    let diagnostic = &output["diagnostics"][0];
    assert_eq!(diagnostic["level"], "error");
    assert_eq!(diagnostic["code"], "E0308");
    assert_eq!(diagnostic["file"], "src/lib.rs");
    assert_eq!(diagnostic["line"], 2);
    assert!(
        diagnostic["rendered"]
            .as_str()
            .unwrap()
            .contains("mismatched types")
    );
}

#[tokio::test]
async fn a_build_that_fails_before_compiling_counts_as_an_error() {
    let fixture = Fixture::new("pub fn two() -> u32 { 2 }\n");
    let manifest = fixture.path.join("Cargo.toml");
    let mut text = fs::read_to_string(&manifest).unwrap();
    text.push_str("[dependencies]\nmissing = { path = \"no-such-crate\" }\n");
    fs::write(&manifest, text).unwrap();

    let output = fixture
        .tool()
        .execute(&call(json!({"command": "build"})))
        .await
        .unwrap();
    let output = parse(&output);

    assert_ne!(output["exit_code"], 0);
    assert_eq!(output["counts"]["errors"], 1);
    assert!(
        output["output"].as_str().unwrap().contains("no-such-crate"),
        "cargo's own error must be returned: {}",
        output["output"]
    );
}

#[tokio::test]
async fn a_long_failing_test_run_keeps_its_summary() {
    let fixture = Fixture::new(
        "#[test]\nfn noisy() {\n    for n in 0..4_000 { println!(\"line {n} of noise\"); }\n    panic!(\"the failure\");\n}\n",
    );

    let output = fixture
        .tool()
        .execute(&call(json!({"command": "test"})))
        .await
        .unwrap();
    let output = parse(&output);
    let text = output["output"].as_str().unwrap();

    assert_eq!(output["truncated"], true);
    assert!(text.contains("bytes omitted"), "{text}");
    assert!(
        text.contains("test result: FAILED"),
        "the summary was cut: {text}"
    );
}

#[tokio::test]
async fn metadata_is_summarised_rather_than_returned_whole() {
    let fixture = Fixture::new("pub fn two() -> u32 { 2 }\n");

    let output = fixture
        .tool()
        .execute(&call(json!({"command": "metadata"})))
        .await
        .unwrap();
    let output = parse(&output);

    let packages = output["packages"].as_array().unwrap();
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0]["name"], "fixture");
    assert_eq!(packages[0]["version"], "0.1.0");
    assert_eq!(packages[0]["edition"], "2021");
    assert_eq!(packages[0]["manifest_path"], "Cargo.toml");
    assert!(output.get("output").unwrap().as_str().unwrap().is_empty());
}

#[tokio::test]
async fn a_wall_clock_kills_a_long_run_and_says_so() {
    let fixture = Fixture::new(
        "#[test]\nfn sleeps() { std::thread::sleep(std::time::Duration::from_secs(120)); }\n",
    );
    let limits = CargoLimits {
        timeout: Duration::from_secs(2),
        ..CargoLimits::default()
    };

    let output = fixture
        .tool_with(limits, Arc::new(Unconfined))
        .execute(&call(json!({"command": "test"})))
        .await
        .unwrap();
    let output = parse(&output);

    // The child is killed by dropping the future, which is the same path a
    // cancelled agent run takes.
    assert_eq!(output["timed_out"], true);
    assert_eq!(output["exit_code"], Value::Null);
    assert!(
        output["output"]
            .as_str()
            .unwrap()
            .contains("killed after 2"),
        "said: {}",
        output["output"]
    );
}

#[test]
fn every_cargo_call_is_classified_as_a_process() {
    let fixture = Fixture::new("pub fn two() -> u32 { 2 }\n");
    let tool = fixture.tool();

    for command in ["metadata", "check", "clippy", "test", "fmt"] {
        let action = tool.classify(&call(json!({"command": command}))).unwrap();

        assert_eq!(
            action.class,
            ToolClass::Process,
            "{command} must never be classified as read-only"
        );
        let subject = action.subject.unwrap();
        assert!(
            subject.starts_with(&format!("cargo {command}")),
            "{subject}"
        );
        assert!(subject.contains("--manifest-path Cargo.toml"), "{subject}");
    }
}

#[test]
fn a_session_rule_for_one_command_does_not_cover_another() {
    let fixture = Fixture::new("pub fn two() -> u32 { 2 }\n");
    let tool = fixture.tool();

    let check = tool.classify(&call(json!({"command": "check"}))).unwrap();
    let test = tool.classify(&call(json!({"command": "test"}))).unwrap();
    let scoped = tool
        .classify(&call(json!({"command": "check", "package": "fixture"})))
        .unwrap();

    assert_ne!(check.rule_key("cargo"), test.rule_key("cargo"));
    assert_ne!(check.rule_key("cargo"), scoped.rule_key("cargo"));
}

#[test]
fn an_argument_that_could_become_a_flag_is_rejected() {
    let fixture = Fixture::new("pub fn two() -> u32 { 2 }\n");
    let tool = fixture.tool();

    for package in ["--offline", "-p", "fixture --offline", "fixture;ls", ""] {
        let error = tool
            .classify(&call(json!({"command": "check", "package": package})))
            .unwrap_err();

        assert!(
            error.to_string().contains("package"),
            "{package:?} was refused for the wrong reason: {error}"
        );
    }
}

#[test]
fn a_filter_outside_the_test_command_is_rejected() {
    let fixture = Fixture::new("pub fn two() -> u32 { 2 }\n");

    let error = fixture
        .tool()
        .classify(&call(json!({"command": "check", "filter": "parses"})))
        .unwrap_err();

    assert!(error.to_string().contains("only to the test command"));
}

#[test]
fn an_empty_filter_through_a_tool_set_runs_every_test() {
    let fixture = Fixture::new("pub fn two() -> u32 { 2 }\n");
    let tools = dino_core::ToolSet::new(vec![Box::new(fixture.tool())]).unwrap();

    let action = tools
        .classify(&call(
            json!({"command": "test", "filter": "", "package": ""}),
        ))
        .unwrap();

    assert_eq!(
        action.subject,
        fixture
            .tool()
            .classify(&call(json!({"command": "test"})))
            .unwrap()
            .subject
    );
}

#[test]
fn an_unknown_field_is_rejected_rather_than_ignored() {
    let fixture = Fixture::new("pub fn two() -> u32 { 2 }\n");

    let error = fixture
        .tool()
        .classify(&call(json!({"command": "check", "flags": ["--offline"]})))
        .unwrap_err();

    assert!(error.to_string().contains("invalid cargo arguments"));
}

#[tokio::test]
async fn a_new_project_gets_the_tool_and_is_told_to_write_a_manifest_first() {
    let serial = WORKSPACE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("dino-cargo-bare-{}-{serial}", std::process::id()));
    fs::create_dir_all(&path).unwrap();

    let tool = CargoTool::new(&path, CargoLimits::default(), Arc::new(Unconfined)).unwrap();
    let call = ToolCall {
        id: "call-1".into(),
        name: "cargo".into(),
        arguments: serde_json::json!({"command": "fetch"}),
    };
    let error = tool.execute(&call).await.unwrap_err();

    fs::remove_dir_all(&path).unwrap();
    assert!(error.to_string().contains("no Cargo.toml"), "{error}");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn a_confined_check_still_runs_a_build_script() {
    const BUILD_SCRIPT: &str = r#"
fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out}/generated.rs"), "pub const N: u32 = 7;\n").unwrap();
}
"#;
    let fixture = Fixture::new(
        "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\npub fn n() -> u32 { N }\n",
    );
    fs::write(fixture.path.join("build.rs"), BUILD_SCRIPT).unwrap();

    let output = fixture
        .sandboxed()
        .execute(&call(json!({"command": "check"})))
        .await
        .unwrap();
    let output = parse(&output);

    assert_eq!(output["exit_code"], 0, "output: {}", output["output"]);
    assert_eq!(output["counts"]["errors"], 0);
    assert!(
        output["command"].as_str().unwrap().contains("--offline"),
        "a confined run has no network and must say --offline: {}",
        output["command"]
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn a_confined_build_script_cannot_write_outside_the_workspace() {
    let escape = std::env::temp_dir().join(format!("dino-escape-{}", std::process::id()));
    let _ = fs::remove_file(&escape);
    let fixture = Fixture::new("pub fn n() -> u32 { 2 }\n");
    let build_script = format!(
        "fn main() {{\n    std::fs::write(r\"{}\", \"escaped\").expect(\"the sandbox let it through\");\n}}\n",
        escape.display()
    );
    fs::write(fixture.path.join("build.rs"), build_script).unwrap();

    let output = fixture
        .sandboxed()
        .execute(&call(json!({"command": "check"})))
        .await
        .unwrap();
    let output = parse(&output);

    assert_ne!(output["exit_code"], 0, "the build script must have failed");
    assert!(
        !escape.exists(),
        "a confined build script wrote to {}",
        escape.display()
    );
    // The build script must have failed because the write was refused, not
    // because it did not compile. A green-looking test for the wrong reason is
    // exactly what a sandbox test must not be.
    let rendered = output["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|diagnostic| diagnostic["rendered"].as_str())
        .collect::<String>();
    let evidence = format!(
        "{rendered}{}",
        output["output"].as_str().unwrap_or_default()
    );
    // Seatbelt refuses with EPERM and Landlock with EACCES.
    assert!(
        evidence.contains("the sandbox let it through")
            && (evidence.contains("Operation not permitted")
                || evidence.contains("Permission denied")),
        "the build script failed for some other reason: {evidence}"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn a_confined_run_gets_a_temporary_directory_inside_the_workspace() {
    let fixture = Fixture::new("pub fn n() -> u32 { 2 }\n");

    let output = fixture
        .sandboxed()
        .execute(&call(json!({"command": "test"})))
        .await
        .unwrap();
    let output = parse(&output);

    // Doctests build in TMPDIR, which a confining profile would otherwise deny.
    assert_eq!(output["exit_code"], 0, "output: {}", output["output"]);
    assert!(
        output["output"].as_str().unwrap().contains("Doc-tests"),
        "output: {}",
        output["output"]
    );
}

#[test]
fn add_and_fetch_say_they_use_the_network() {
    let fixture = Fixture::new("pub fn two() -> u32 { 2 }\n");
    let tool = fixture.tool();

    let add = tool
        .classify(&call(json!({
            "command": "add",
            "crates": ["serde@1", "thiserror"],
            "features": ["derive"],
            "dev": true
        })))
        .unwrap();
    let fetch = tool.classify(&call(json!({"command": "fetch"}))).unwrap();

    assert_eq!(
        add.subject.as_deref(),
        Some(
            "cargo add serde@1 thiserror --features derive --dev --manifest-path Cargo.toml \
             (network: HTTPS, Cargo cache)"
        )
    );
    assert_eq!(
        fetch.subject.as_deref(),
        Some("cargo fetch --manifest-path Cargo.toml (network: HTTPS, Cargo cache)")
    );
}

#[test]
fn add_needs_a_crate_and_its_fields_belong_to_it_alone() {
    let fixture = Fixture::new("pub fn two() -> u32 { 2 }\n");
    let tool = fixture.tool();

    for arguments in [
        json!({"command": "add"}),
        json!({"command": "add", "crates": ["--git"]}),
        json!({"command": "add", "crates": ["serde json"]}),
        json!({"command": "add", "crates": ["serde"], "features": ["a b"]}),
        json!({"command": "check", "crates": ["serde"]}),
        json!({"command": "fetch", "dev": true}),
    ] {
        assert!(
            tool.classify(&call(arguments.clone())).is_err(),
            "{arguments}"
        );
    }
}

#[test]
fn build_run_fmt_and_fmt_check_run_what_they_say() {
    let fixture = Fixture::new("pub fn two() -> u32 { 2 }\n");
    let tool = fixture.tool();
    let subject = |arguments: Value| tool.classify(&call(arguments)).unwrap().subject.unwrap();

    assert!(
        subject(json!({"command": "build"}))
            .starts_with("cargo build --workspace --all-targets --message-format=json")
    );
    assert_eq!(
        subject(json!({"command": "run", "args": ["--port", "0"]})),
        "cargo run --manifest-path Cargo.toml -- --port 0"
    );
    assert_eq!(
        subject(json!({"command": "fmt"})),
        "cargo fmt --all --manifest-path Cargo.toml"
    );
    assert_eq!(
        subject(json!({"command": "fmt_check"})),
        "cargo fmt --all --check --manifest-path Cargo.toml"
    );
    assert!(
        tool.classify(&call(json!({"command": "check", "args": ["x"]})))
            .is_err()
    );
}

#[tokio::test]
async fn fmt_formats_the_file() {
    let fixture = Fixture::new("pub fn  two( )->u32{2}\n");
    let tool = fixture.tool();

    tool.execute(&call(json!({"command": "fmt"})))
        .await
        .unwrap();

    let source = std::fs::read_to_string(fixture.path.join("src/lib.rs")).unwrap();
    assert_eq!(source, "pub fn two() -> u32 {\n    2\n}\n");
}

#[tokio::test]
async fn fmt_with_files_formats_only_those() {
    let fixture = Fixture::new("pub fn  two( )->u32{2}\n");
    let untouched = "#[test]\nfn two() { assert_eq!(fixture::two(), 2); }\n";
    fs::create_dir_all(fixture.path.join("tests")).unwrap();
    fs::write(fixture.path.join("tests/basics.rs"), untouched).unwrap();
    let tool = fixture.tool();
    let arguments = json!({"command": "fmt", "files": ["src/lib.rs"]});

    assert_eq!(
        tool.classify(&call(arguments.clone()))
            .unwrap()
            .subject
            .unwrap(),
        "rustfmt --edition 2021 src/lib.rs"
    );
    tool.execute(&call(arguments)).await.unwrap();

    assert_eq!(
        fs::read_to_string(fixture.path.join("src/lib.rs")).unwrap(),
        "pub fn two() -> u32 {\n    2\n}\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.path.join("tests/basics.rs")).unwrap(),
        untouched
    );
}

#[test]
fn fmt_files_take_an_inherited_edition_from_the_workspace() {
    let fixture = Fixture::new("pub fn two() -> u32 { 2 }\n");
    fs::write(
        fixture.path.join("Cargo.toml"),
        "[workspace.package]\nedition = \"2024\"\n\n[workspace]\n\n\
         [package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition.workspace = true\n",
    )
    .unwrap();
    let tool = fixture.tool();

    let action = tool
        .classify(&call(
            json!({"command": "fmt_check", "files": ["./src/lib.rs"]}),
        ))
        .unwrap();
    assert_eq!(
        action.subject.unwrap(),
        "rustfmt --edition 2024 --check ./src/lib.rs"
    );
}

#[test]
fn fmt_files_must_be_rust_files_inside_the_workspace() {
    let fixture = Fixture::new("pub fn two() -> u32 { 2 }\n");
    let tool = fixture.tool();

    for arguments in [
        json!({"command": "check", "files": ["src/lib.rs"]}),
        json!({"command": "fmt", "files": ["../outside.rs"]}),
        json!({"command": "fmt", "files": ["/tmp/outside.rs"]}),
        json!({"command": "fmt", "files": ["--config=x.rs"]}),
        json!({"command": "fmt", "files": ["Cargo.toml"]}),
        json!({"command": "fmt", "package": "fixture", "files": ["src/lib.rs"]}),
    ] {
        assert!(
            tool.classify(&call(arguments.clone())).is_err(),
            "{arguments}"
        );
    }
}
