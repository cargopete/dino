//! The check after a turn's Rust edits, against a real Cargo project.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use dino_rust::CargoLimits;
use dino_rust::CargoTool;
use dino_rust::EditCheck;
use dino_sandbox::Unconfined;
use serde_json::Value;
use serde_json::json;

/// Writes the call's `content` to its `path`, as `write_file` would.
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
        let content = call.arguments["content"].as_str().unwrap().to_owned();
        Box::pin(async move {
            fs::write(path, content).unwrap();
            Ok(ToolOutput::success(json!({"written": true}).to_string()))
        })
    }
}

fn write(id: &str, path: &str, content: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "write_file".into(),
        arguments: json!({"path": path, "content": content}),
    }
}

#[tokio::test]
async fn the_last_rust_edit_of_a_turn_carries_the_errors_in_the_edited_files() {
    let root = std::env::temp_dir().join(format!("dino-edit-check-{}", std::process::id()));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\n[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub mod a;\npub mod b;\n").unwrap();
    let cargo = CargoTool::new(&root, CargoLimits::default(), Arc::new(Unconfined)).unwrap();
    let tools = EditCheck::new(Box::new(Writer { root: root.clone() }), cargo);

    let first = write("1", "src/a.rs", "pub fn a() -> u32 { \"one\" }\n");
    let second = write("2", "src/b.rs", "pub fn b() -> u32 { 2 }\n");
    tools.begin_turn(&[first.clone(), second.clone()]);

    let output: Value =
        serde_json::from_str(&tools.execute(&first).await.unwrap().content).unwrap();
    assert!(
        output.get("cargo_check").is_none(),
        "checked too early: {output}"
    );

    let output: Value =
        serde_json::from_str(&tools.execute(&second).await.unwrap().content).unwrap();
    let check = &output["cargo_check"];
    assert_eq!(check["errors"], 1, "{output}");
    assert_eq!(check["in_edited_files"][0]["file"], "src/a.rs", "{output}");
    let _ = fs::remove_dir_all(&root);
}
