//! The `web_fetch` tool, against a local server.

use gyr_core::ToolRuntime;
use gyr_exec::WebFetchTool;
use gyr_exec::WebLimits;
use gyr_protocol::ToolCall;
use gyr_protocol::ToolClass;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

fn call(arguments: Value) -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: "web_fetch".into(),
        arguments,
    }
}

/// Answers each connection with the next response, in order.
async fn serve(responses: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 4_096];
            let _ = socket.read(&mut buffer).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    base
}

fn ok(content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn html_comes_back_as_text() {
    let base = serve(vec![ok(
        "text/html; charset=utf-8",
        "<html><head><script>var x=1;</script></head><body><h1>Serde</h1><p>A framework for <b>serializing</b>.</p></body></html>",
    )])
    .await;
    let tool = WebFetchTool::new(WebLimits::default()).unwrap();

    let output = tool
        .execute(&call(json!({"url": format!("{base}/serde")})))
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();

    assert_eq!(output["status"], 200);
    let text = output["text"].as_str().unwrap();
    assert!(text.contains("Serde"), "{text}");
    assert!(text.contains("serializing"), "{text}");
    assert!(!text.contains("var x"), "{text}");
}

#[tokio::test]
async fn a_redirect_to_another_host_is_refused() {
    let base = serve(vec![
        "HTTP/1.1 302 Found\r\nlocation: http://localhost:9/elsewhere\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            .to_owned(),
    ])
    .await;
    let tool = WebFetchTool::new(WebLimits::default()).unwrap();

    let error = tool
        .execute(&call(json!({"url": format!("{base}/start")})))
        .await
        .unwrap_err();

    assert!(error.to_string().contains("another host"), "{error}");
}

#[test]
fn approval_is_keyed_on_the_host() {
    let tool = WebFetchTool::new(WebLimits::default()).unwrap();

    let action = tool
        .classify(&call(
            json!({"url": "https://docs.rs/serde/latest/serde/?q=x"}),
        ))
        .unwrap();

    assert_eq!(action.class, ToolClass::Network);
    assert_eq!(action.subject.as_deref(), Some("docs.rs"));
    for url in ["file:///etc/passwd", "docs.rs/serde", "https://"] {
        assert!(tool.classify(&call(json!({"url": url}))).is_err(), "{url}");
    }
}
