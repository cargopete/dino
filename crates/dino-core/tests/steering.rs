//! What the person types mid-run, and dino's countdown, on the real wires.
//!
//! The core tests show these leave the tool results; these show each adapter
//! then sends them as a message of their own, and that a restored session
//! still has them there.

use std::fmt::Write as _;
use std::sync::Arc;

use dino_core::Agent;
use dino_core::AgentConfig;
use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_model::ModelSession;
use dino_model::SessionState;
use dino_model::anthropic::AnthropicConfig;
use dino_model::anthropic::AnthropicSession;
use dino_model::qwen::QwenConfig;
use dino_model::qwen::QwenSession;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use dino_protocol::TurnInput;
use futures_util::TryStreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

const TYPED: &str = "also fix b.rs";

struct Read;

impl ToolRuntime for Read {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "read".into(),
            description: "reads a file".into(),
            input_schema: json!({"type": "object"}),
        }]
    }

    fn classify(&self, _call: &ToolCall) -> Result<ToolAction, ToolError> {
        Ok(ToolAction::read_only())
    }

    fn execute(&self, _call: &ToolCall) -> ToolFuture<'_> {
        Box::pin(async { Ok(ToolOutput::success("fn main() {}")) })
    }
}

/// Serves one SSE script per model request and returns the request bodies.
/// Anything fetched with GET, such as a model list, is answered 404.
async fn serve(
    scripts: Vec<&'static [&'static str]>,
) -> (String, tokio::sync::oneshot::Receiver<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut bodies = Vec::new();
        for script in scripts {
            let mut socket = loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (head, body) = read_request(&mut socket).await;
                if !head.starts_with("GET") {
                    bodies.push(serde_json::from_str(&body).unwrap());
                    break socket;
                }
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await;
            };
            let mut response = String::from(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            );
            for chunk in script {
                let _ = writeln!(&mut response, "data: {chunk}\n");
            }
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
        let _ = sender.send(bodies);
    });
    (format!("http://{address}"), receiver)
}

/// Reads one HTTP request and returns its head and body.
async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, String) {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4_096];
    loop {
        let read = socket.read(&mut buffer).await.unwrap();
        request.extend_from_slice(&buffer[..read]);
        let text = String::from_utf8_lossy(&request);
        if let Some(headers_end) = text.find("\r\n\r\n") {
            let length: usize = text
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())?
                })
                .unwrap_or(0);
            if read == 0 || request.len() >= headers_end + 4 + length {
                let body = String::from_utf8_lossy(&request[headers_end + 4..]).into_owned();
                return (text[..headers_end].to_owned(), body);
            }
        }
        assert_ne!(read, 0, "the request ended before its headers did");
    }
}

const ANTHROPIC_READ: &[&str] = &[
    r#"{"type":"message_start","message":{"id":"m1","usage":{"input_tokens":1,"output_tokens":1}}}"#,
    r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"read","input":{}}}"#,
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.rs\"}"}}"#,
    r#"{"type":"content_block_stop","index":0}"#,
    r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}"#,
    r#"{"type":"message_stop"}"#,
];

const ANTHROPIC_DONE: &[&str] = &[
    r#"{"type":"message_start","message":{"id":"m2","usage":{"input_tokens":1,"output_tokens":1}}}"#,
    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}"#,
    r#"{"type":"content_block_stop","index":0}"#,
    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
    r#"{"type":"message_stop"}"#,
];

const CHAT_READ: &[&str] = &[
    r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"t1","function":{"name":"read","arguments":"{\"path\":\"a.rs\"}"}}]}}]}"#,
    r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    "[DONE]",
];

const CHAT_DONE: &[&str] = &[
    r#"{"id":"c2","choices":[{"index":0,"delta":{"content":"done"}}]}"#,
    r#"{"id":"c2","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    "[DONE]",
];

fn anthropic(base: String) -> AnthropicSession {
    let mut config =
        AnthropicConfig::new("key", dino_model::builtin_profile("claude-opus").unwrap());
    config.api_base = base;
    AnthropicSession::new(config).unwrap()
}

fn chat(base: &str) -> QwenSession {
    QwenSession::new(QwenConfig::new(
        format!("{base}/v1"),
        dino_model::builtin_profile("qwen3-coder-next").unwrap(),
    ))
    .unwrap()
}

/// Runs one read and an answer, with the person typing during the read and
/// two turns left after it, and returns the session's exported history.
async fn steered_run(session: impl ModelSession) -> SessionState {
    let inbox = dino_core::Inbox::default();
    let mut agent = Agent::new(
        session,
        Read,
        AgentConfig {
            max_model_turns: std::num::NonZeroU32::new(3).unwrap(),
            turn_notice_at: 2,
            ..AgentConfig::default()
        },
    )
    .with_inbox(Arc::clone(&inbox));
    inbox.lock().unwrap().push(TYPED.into());

    agent.run("read a.rs").await.unwrap();
    agent.session().export_state().unwrap()
}

/// Sends one more turn from a restored history.
async fn restored_request(mut session: impl ModelSession, state: &SessionState) {
    session.restore_state(state).unwrap();
    let stream = session
        .next(TurnInput::User {
            content: "and now?".into(),
        })
        .await
        .unwrap();
    let _ = stream.try_collect::<Vec<_>>().await.unwrap();
}

fn assert_said_after(note: &Value) {
    let note = note.as_str().unwrap();
    assert!(
        note.starts_with(&format!(
            "The person says:\n{TYPED}\n\n[dino: 2 model turns left"
        )),
        "{note}"
    );
}

fn check_anthropic(body: &Value) {
    let content = &body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["content"][0]["type"] == "tool_result")
        .unwrap()["content"];
    assert_eq!(content.as_array().unwrap().len(), 2, "{content}");
    assert_eq!(
        content[0],
        json!({"type": "tool_result", "tool_use_id": "t1", "content": "fn main() {}", "is_error": false})
    );
    assert_eq!(content[1]["type"], "text");
    assert_said_after(&content[1]["text"]);
}

fn check_chat(body: &Value) {
    let messages = body["messages"].as_array().unwrap();
    let at = messages
        .iter()
        .position(|message| message["role"] == "tool")
        .unwrap();
    assert_eq!(messages[at]["content"], "fn main() {}");
    assert_eq!(messages[at + 1]["role"], "user");
    assert_said_after(&messages[at + 1]["content"]);
}

#[tokio::test]
async fn typed_steering_and_the_countdown_reach_anthropic_outside_the_results() {
    let (base, bodies) = serve(vec![ANTHROPIC_READ, ANTHROPIC_DONE]).await;
    let state = steered_run(anthropic(base)).await;
    check_anthropic(&bodies.await.unwrap()[1]);

    let (base, bodies) = serve(vec![ANTHROPIC_DONE]).await;
    restored_request(anthropic(base), &state).await;
    check_anthropic(&bodies.await.unwrap()[0]);
}

#[tokio::test]
async fn typed_steering_and_the_countdown_reach_a_chat_server_outside_the_results() {
    let (base, bodies) = serve(vec![CHAT_READ, CHAT_DONE]).await;
    let state = steered_run(chat(&base)).await;
    check_chat(&bodies.await.unwrap()[1]);

    let (base, bodies) = serve(vec![CHAT_DONE]).await;
    restored_request(chat(&base), &state).await;
    check_chat(&bodies.await.unwrap()[0]);
}
