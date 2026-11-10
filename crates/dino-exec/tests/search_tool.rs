//! The `web_search` tool, against a local server standing in for an engine.

use dino_core::ToolRuntime;
use dino_exec::SearchBackend;
use dino_exec::SearchEngine;
use dino_exec::WebSearchTool;
use dino_protocol::ToolCall;
use dino_protocol::ToolClass;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

fn call(arguments: Value) -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: "web_search".into(),
        arguments,
    }
}

/// Answers one request with `body`, and hands back the request's head.
async fn serve(status: &str, body: &str) -> (String, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let (seen, request) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut buffer = [0_u8; 4_096];
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut buffer).await.unwrap();
            head.extend_from_slice(&buffer[..read]);
        }
        let _ = seen.send(String::from_utf8_lossy(&head).into_owned());
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    (base, request)
}

#[tokio::test]
async fn brave_is_asked_with_its_key_and_answers_titles_urls_and_snippets() {
    let (base, request) = serve(
        "200 OK",
        r#"{"web":{"results":[{"title":"Tokio 2.0","url":"https://tokio.rs/blog/2","description":"What <strong>changed</strong>"}]}}"#,
    )
    .await;
    let backend = SearchBackend::new(
        SearchEngine::Brave,
        Some(&format!("{base}/res/v1/web/search")),
        Some("brave-key".into()),
    )
    .unwrap();
    let tool = WebSearchTool::new(backend).unwrap();

    let output = tool
        .execute(&call(json!({"query": "tokio 2 changelog", "count": 3})))
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();
    let head = request.await.unwrap();

    assert!(
        head.starts_with("GET /res/v1/web/search?q=tokio+2+changelog&count=3 "),
        "{head}"
    );
    assert!(
        head.to_ascii_lowercase()
            .contains("x-subscription-token: brave-key"),
        "{head}"
    );
    assert_eq!(
        output["results"],
        json!([{"title": "Tokio 2.0", "url": "https://tokio.rs/blog/2", "snippet": "What changed"}])
    );
}

#[tokio::test]
async fn searxng_is_asked_for_json_at_its_search_path() {
    let (base, request) = serve(
        "200 OK",
        r#"{"results":[{"title":"A","url":"https://a.example","content":"a"},{"title":"B","url":"https://b.example","content":"b"}]}"#,
    )
    .await;
    let backend = SearchBackend::new(SearchEngine::Searxng, Some(&base), None).unwrap();
    let tool = WebSearchTool::new(backend).unwrap();

    let output = tool
        .execute(&call(json!({"query": "serde", "count": 1})))
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();

    assert!(
        request
            .await
            .unwrap()
            .starts_with("GET /search?q=serde&format=json ")
    );
    assert_eq!(output["results"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn an_engine_that_refuses_is_an_error_naming_its_host() {
    let (base, _request) = serve("401 Unauthorized", r#"{"error":"bad key"}"#).await;
    let backend =
        SearchBackend::new(SearchEngine::Kagi, Some(&base), Some("wrong".into())).unwrap();
    let tool = WebSearchTool::new(backend).unwrap();

    let error = tool
        .execute(&call(json!({"query": "serde"})))
        .await
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("127.0.0.1") && error.contains("401"),
        "{error}"
    );
}

#[test]
fn approval_is_keyed_on_the_engine_whatever_the_query() {
    let backend = SearchBackend::new(SearchEngine::Kagi, None, Some("key".into())).unwrap();
    let tool = WebSearchTool::new(backend).unwrap();

    for query in ["serde derive", "tokio select"] {
        let action = tool.classify(&call(json!({"query": query}))).unwrap();
        assert_eq!(action.class, ToolClass::Network);
        assert_eq!(action.subject.as_deref(), Some("kagi.com"));
    }
    assert!(tool.classify(&call(json!({"query": "  "}))).is_err());
}
