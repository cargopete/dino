//! The MCP client, against a small stdio server written in Python.

#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use dino_core::ToolRuntime;
use dino_exec::McpServer;
use dino_exec::McpTools;
use dino_protocol::ToolCall;
use dino_protocol::ToolClass;
use dino_sandbox::Sandbox;
use dino_sandbox::Unconfined;
use pretty_assertions::assert_eq;
use serde_json::json;

/// Answers initialize, lists two tools over two pages, pings the client once,
/// and implements `add` and `fail`.
const SERVER: &str = r#"
import json, sys
def send(message):
    sys.stdout.write(json.dumps(message) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    message = json.loads(line)
    method, ident = message.get("method"), message.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": ident, "result": {"protocolVersion": "2025-06-18",
              "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}})
    elif method == "tools/list" and "cursor" not in message.get("params", {}):
        send({"jsonrpc": "2.0", "method": "notifications/message", "params": {}})
        send({"jsonrpc": "2.0", "id": ident, "result": {"nextCursor": "2", "tools": [
            {"name": "add", "description": "Add two numbers",
             "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}}}]}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": ident, "result": {"tools": [
            {"name": "fail", "description": "Always fails", "inputSchema": {"type": "object"}}]}})
    elif method == "tools/call":
        send({"jsonrpc": "2.0", "id": "server-ping", "method": "ping"})
        name, args = message["params"]["name"], message["params"]["arguments"]
        if name == "add":
            send({"jsonrpc": "2.0", "id": ident, "result": {"content": [{"type": "text", "text": str(args["a"] + args["b"])}]}})
        else:
            send({"jsonrpc": "2.0", "id": ident, "result": {"isError": True, "content": [{"type": "text", "text": "it broke"}]}})
    elif ident == "server-ping":
        pass
"#;

/// Its own directory per test: they run in parallel, and one rewriting the
/// script while another's server starts reading it left a half-written file.
fn fixture(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("dino-mcp-{}-{name}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("server.py"), SERVER).unwrap();
    fs::canonicalize(root).unwrap()
}

async fn connect(root: &Path) -> McpTools {
    let sandbox: Arc<dyn Sandbox> = Arc::new(Unconfined);
    McpTools::connect(
        McpServer {
            name: "fake".into(),
            command: vec![
                "python3".into(),
                root.join("server.py").display().to_string(),
            ],
            pass_env: Vec::new(),
            read_only_tools: vec!["add".into()],
            sandboxed: true,
        },
        root,
        &sandbox,
    )
    .await
    .unwrap()
}

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: name.into(),
        arguments,
    }
}

#[tokio::test]
async fn tools_from_every_page_are_offered_under_the_server_prefix() {
    let root = fixture("pages");
    let tools = connect(&root).await;

    let names: Vec<_> = tools
        .definitions()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(names, ["mcp__fake__add", "mcp__fake__fail"]);
    assert_eq!(tools.summary(), "fake (2 tools, sandboxed)");
}

#[tokio::test]
async fn calls_return_text_and_errors_as_errors() {
    let root = fixture("calls");
    let tools = connect(&root).await;

    let sum = tools
        .execute(&call("mcp__fake__add", json!({"a": 2, "b": 3})))
        .await
        .unwrap();
    assert_eq!(sum.content, "5");
    assert!(!sum.is_error);

    let failed = tools
        .execute(&call("mcp__fake__fail", json!({})))
        .await
        .unwrap();
    assert!(failed.is_error);
    assert_eq!(failed.content, "it broke");
}

#[tokio::test]
async fn listed_read_only_tools_skip_approval_and_the_rest_are_processes() {
    let root = fixture("classes");
    let tools = connect(&root).await;

    let add = tools.classify(&call("mcp__fake__add", json!({}))).unwrap();
    let fail = tools.classify(&call("mcp__fake__fail", json!({}))).unwrap();

    assert_eq!(add.class, ToolClass::ReadOnly);
    assert_eq!(fail.class, ToolClass::Process);
    assert_eq!(fail.subject.as_deref(), Some("MCP fake fail"));
    assert!(
        tools
            .classify(&call("mcp__fake__missing", json!({})))
            .is_err()
    );
}
