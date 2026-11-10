//! The `code` tool against a real rust-analyzer, in the real sandbox, on a
//! small crate. Skipped where rust-analyzer cannot be run.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use dino_core::ToolRuntime;
use dino_exec::CodeLimits;
use dino_exec::CodeTool;
use dino_protocol::ToolCall;
use dino_protocol::ToolClass;
use serde_json::Value;
use serde_json::json;

const LIB: &str = "pub mod shapes;

pub use shapes::Square as Block;

pub fn total(items: &[Block]) -> u32 {
    items.iter().map(shapes::Shape::area).sum()
}
";

const SHAPES: &str = "/// Something with an area.
pub trait Shape {
    fn area(&self) -> u32;
}

pub struct Square(pub u32);

impl Shape for Square {
    fn area(&self) -> u32 {
        self.0 * self.0
    }
}
";

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("dino-code-{}", std::process::id()));
        fs::create_dir_all(path.join("src")).unwrap();
        fs::write(
            path.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(path.join("src/lib.rs"), LIB).unwrap();
        fs::write(path.join("src/shapes.rs"), SHAPES).unwrap();
        Self {
            path: fs::canonicalize(path).unwrap(),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

async fn ask(tool: &CodeTool, arguments: Value) -> Value {
    let call = ToolCall {
        id: "call".into(),
        name: "code".into(),
        arguments,
    };
    let output = tool.execute(&call).await.unwrap();
    assert!(!output.is_error, "{}", output.content);
    serde_json::from_str(&output.content).unwrap()
}

fn places(answer: &Value) -> Vec<String> {
    answer["locations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|found| format!("{}:{}", found["path"].as_str().unwrap(), found["line"]))
        .collect()
}

// One test, so one server indexes the fixture once.
#[tokio::test(flavor = "multi_thread")]
async fn rust_analyzer_answers_through_the_sandbox() {
    let fixture = Fixture::new();
    let sandbox = dino_sandbox::detect(&fixture.path).expect("a sandbox on this platform");
    let Some(tool) =
        CodeTool::new(&fixture.path, CodeLimits::default(), Arc::from(sandbox)).unwrap()
    else {
        eprintln!("skipped: rust-analyzer cannot be run here");
        return;
    };
    let call = ToolCall {
        id: "call".into(),
        name: "code".into(),
        arguments: json!({"operation": "symbols", "query": "Square"}),
    };
    assert_eq!(tool.classify(&call).unwrap().class, ToolClass::ReadOnly);

    // Through the re-export, to the struct it renames.
    let definition = ask(
        &tool,
        json!({"operation": "definition", "path": "src/lib.rs", "line": 5, "name": "Block"}),
    )
    .await;
    assert_eq!(definition.get("note"), None, "{definition}");
    assert_eq!(places(&definition), ["src/shapes.rs:6"], "{definition}");
    assert_eq!(definition["locations"][0]["column"], 12);
    assert_eq!(
        definition["locations"][0]["text"],
        "pub struct Square(pub u32);"
    );

    let references = ask(
        &tool,
        json!({"operation": "references", "path": "src/shapes.rs", "line": 3, "name": "area"}),
    )
    .await;
    assert!(
        places(&references).contains(&"src/lib.rs:6".to_owned()),
        "{references}"
    );

    let implementations = ask(
        &tool,
        json!({"operation": "implementations", "path": "src/shapes.rs", "line": 2, "name": "Shape"}),
    )
    .await;
    assert_eq!(
        places(&implementations),
        ["src/shapes.rs:8"],
        "{implementations}"
    );

    let hover = ask(
        &tool,
        json!({"operation": "hover", "path": "src/lib.rs", "line": 6, "name": "Shape"}),
    )
    .await;
    let text = hover["hover"].as_str().unwrap();
    assert!(text.contains("trait Shape"), "{text}");
    assert!(text.contains("Something with an area."), "{text}");

    let symbols = ask(&tool, json!({"operation": "symbols", "query": "Square"})).await;
    let square = symbols["locations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|found| found["name"] == "Square")
        .unwrap_or_else(|| panic!("{symbols}"));
    assert_eq!(square["kind"], "struct");
    assert_eq!(square["path"], "src/shapes.rs");

    // Edits on disk are what the next question sees: to a file already asked
    // about, and in a new file no question has named.
    fs::write(
        fixture.path.join("src/lib.rs"),
        format!(
            "{LIB}mod extra;\n\npub fn one() -> u32 {{\n    shapes::Shape::area(&Block(1))\n}}\n"
        ),
    )
    .unwrap();
    fs::write(
        fixture.path.join("src/extra.rs"),
        "pub fn two() -> u32 {\n    crate::shapes::Shape::area(&crate::Block(2))\n}\n",
    )
    .unwrap();
    let references = ask(
        &tool,
        json!({"operation": "references", "path": "src/shapes.rs", "line": 3, "name": "area"}),
    )
    .await;
    let found = places(&references);
    assert!(found.contains(&"src/lib.rs:11".to_owned()), "{references}");
    assert!(found.contains(&"src/extra.rs:2".to_owned()), "{references}");

    let wrong = ToolCall {
        id: "call".into(),
        name: "code".into(),
        arguments: json!({"operation": "hover", "path": "src/lib.rs", "line": 6, "name": "nothing"}),
    };
    let error = tool.execute(&wrong).await.unwrap_err().to_string();
    assert!(error.contains("is not on line 6"), "{error}");
}
