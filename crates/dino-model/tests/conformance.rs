//! The conformance suite RFC-0003 section 7 asked for.
//!
//! Every adapter is driven over a real socket with a provider-shaped SSE
//! script, and every scenario asserts the same **normalised** events. Three
//! different wires, one semantics: that is the whole claim the `ModelSession`
//! boundary makes, and until now nothing checked it. Each adapter tested
//! whatever its author thought of, and `OpenAI` had no wire test at all.
//!
//! What this cannot do is prove a provider accepts what we send. That needs a
//! credential, and RFC-0003 says schema conformance is necessary and
//! insufficient. This is the necessary half.

use std::collections::HashMap;
use std::fmt::Write as _;

use dino_model::ModelError;
use dino_model::ModelSession;
use dino_model::anthropic::AnthropicConfig;
use dino_model::anthropic::AnthropicSession;
use dino_model::openai::OpenAiConfig;
use dino_model::openai::OpenAiSession;
use dino_model::qwen::QwenConfig;
use dino_model::qwen::QwenSession;
use dino_protocol::ModelEvent;
use dino_protocol::StopReason;
use dino_protocol::TurnInput;
use futures_util::TryStreamExt;
use pretty_assertions::assert_eq;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

/// Which adapter a scenario is being run against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Provider {
    Anthropic,
    OpenAi,
    Qwen,
}

impl Provider {
    const ALL: [Self; 3] = [Self::Anthropic, Self::OpenAi, Self::Qwen];

    fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Qwen => "qwen",
        }
    }
}

/// Serves one SSE script per connection and hands back what was asked for.
///
/// A second turn needs the request body the adapter sent, because the property
/// scenario 2 is really about is not what came back but what went out next.
async fn serve_turns(
    scripts: Vec<Vec<String>>,
) -> (String, tokio::sync::oneshot::Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut bodies = Vec::new();
        for script in scripts {
            let (mut socket, _) = listener.accept().await.unwrap();
            bodies.push(read_request(&mut socket).await);
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

/// Reads one HTTP request and returns its body.
async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4_096];
    loop {
        let read = socket.read(&mut buffer).await.unwrap();
        if read == 0 {
            break;
        }
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
            if request.len() >= headers_end + 4 + length {
                return String::from_utf8_lossy(&request[headers_end + 4..]).into_owned();
            }
        }
    }
    String::new()
}

fn build(provider: Provider, base: String) -> Box<dyn ModelSession> {
    match provider {
        Provider::Anthropic => {
            let mut config =
                AnthropicConfig::new("key", dino_model::builtin_profile("claude-opus").unwrap());
            config.api_base = base;
            Box::new(AnthropicSession::new(config).unwrap())
        }
        Provider::OpenAi => {
            let mut config =
                OpenAiConfig::new("key", dino_model::builtin_profile("terra").unwrap());
            config.api_base = base;
            Box::new(OpenAiSession::new(config).unwrap())
        }
        Provider::Qwen => Box::new(
            QwenSession::new(QwenConfig::new(
                format!("{base}/v1"),
                dino_model::builtin_profile("qwen3-coder-next").unwrap(),
            ))
            .unwrap(),
        ),
    }
}

/// Serves one SSE script on the loopback interface and hangs up.
///
/// Shared, because two adapters had grown their own copy of this and the third
/// had none, which is how `OpenAI` ended up with no wire coverage at all.
async fn serve(chunks: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4_096];
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            if read == 0 {
                return;
            }
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
                if request.len() >= headers_end + 4 + length {
                    break;
                }
            }
        }
        let mut body = String::from(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
        );
        for chunk in chunks {
            let _ = writeln!(&mut body, "data: {chunk}\n");
        }
        let _ = socket.write_all(body.as_bytes()).await;
        let _ = socket.shutdown().await;
    });
    format!("http://{address}")
}

/// Drives one adapter over the socket and returns its normalised events.
async fn run(provider: Provider, chunks: Vec<String>) -> Result<Vec<ModelEvent>, ModelError> {
    let base = serve(chunks).await;
    let mut session: Box<dyn ModelSession> = match provider {
        Provider::Anthropic => {
            let mut config =
                AnthropicConfig::new("key", dino_model::builtin_profile("claude-opus").unwrap());
            config.api_base = base;
            Box::new(AnthropicSession::new(config)?)
        }
        Provider::OpenAi => {
            let mut config =
                OpenAiConfig::new("key", dino_model::builtin_profile("terra").unwrap());
            config.api_base = base;
            Box::new(OpenAiSession::new(config)?)
        }
        Provider::Qwen => Box::new(QwenSession::new(QwenConfig::new(
            format!("{base}/v1"),
            dino_model::builtin_profile("qwen3-coder-next").unwrap(),
        ))?),
    };
    let stream = session
        .next(TurnInput::User {
            content: "go".into(),
        })
        .await?;
    stream.try_collect::<Vec<_>>().await
}

/// The events a scenario cares about, with streamed text joined.
///
/// Providers differ in how finely they chop a stream, and a suite that asserted
/// delta boundaries would be testing chunking rather than semantics.
fn shape(events: &[ModelEvent]) -> (String, Vec<(String, String)>, Option<StopReason>) {
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut stop = None;
    for event in events {
        match event {
            ModelEvent::TextDelta { text: delta } => text.push_str(delta),
            ModelEvent::ToolCallCompleted { call } => {
                calls.push((call.name.clone(), call.arguments.to_string()));
            }
            ModelEvent::Finished { reason } => stop = Some(*reason),
            _ => {}
        }
    }
    (text, calls, stop)
}

fn usage(events: &[ModelEvent]) -> Option<dino_protocol::TokenUsage> {
    events.iter().find_map(|event| match event {
        ModelEvent::Usage { usage } => Some(*usage),
        _ => None,
    })
}

// ---------------------------------------------------------------- scripts ---

/// One semantic scenario, written once per wire.
struct Script {
    scripts: HashMap<Provider, Vec<String>>,
}

impl Script {
    fn new(anthropic: &[&str], openai: &[&str], qwen: &[&str]) -> Self {
        let own = |lines: &[&str]| lines.iter().map(|line| (*line).to_owned()).collect();
        Self {
            scripts: HashMap::from([
                (Provider::Anthropic, own(anthropic)),
                (Provider::OpenAi, own(openai)),
                (Provider::Qwen, own(qwen)),
            ]),
        }
    }

    fn for_provider(&self, provider: Provider) -> Vec<String> {
        self.scripts[&provider].clone()
    }
}

/// A plain streamed answer, twenty input tokens of which five were cached.
///
/// **The three scripts are not the same numbers, on purpose.** Anthropic's
/// `input_tokens` excludes the cached portion and its adapter sums
/// `input + cache_creation + cache_read`; `OpenAI`'s and Qwen's already include
/// it and their adapters pass it through. So fifteen-plus-five on one wire is
/// twenty-including-five on the others, and all three normalise to twenty.
/// Writing this fixture wrong is how the difference was noticed.
fn plain_answer() -> Script {
    Script::new(
        &[
            r#"{"type":"message_start","message":{"id":"m1","usage":{"input_tokens":15,"cache_read_input_tokens":5,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"the answer"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
            r#"{"type":"message_stop"}"#,
        ],
        &[
            r#"{"type":"response.created","response":{"id":"r1"}}"#,
            r#"{"type":"response.output_text.delta","delta":"the answer"}"#,
            r#"{"type":"response.completed","response":{"id":"r1","usage":{"input_tokens":20,"output_tokens":7,"input_tokens_details":{"cached_tokens":5}}}}"#,
        ],
        &[
            r#"{"id":"c1","choices":[{"index":0,"delta":{"content":"the answer"}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":20,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":5}}}"#,
            "[DONE]",
        ],
    )
}

fn two_tool_calls() -> Script {
    Script::new(
        &[
            r#"{"type":"message_start","message":{"id":"m1","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t2","name":"read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"b\"}"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}"#,
            r#"{"type":"message_stop"}"#,
        ],
        &[
            r#"{"type":"response.created","response":{"id":"r1"}}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"f1","call_id":"t1","name":"read","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"f1","arguments":"{\"path\":\"a\"}"}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"f2","call_id":"t2","name":"read","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"f2","arguments":"{\"path\":\"b\"}"}"#,
            r#"{"type":"response.completed","response":{"id":"r1","usage":{"input_tokens":1,"output_tokens":4}}}"#,
        ],
        &[
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"t1","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"t2","function":{"name":"read","arguments":"{\"path\":\"b\"}"}}]}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ],
    )
}

fn text_then_tool() -> Script {
    Script::new(
        &[
            r#"{"type":"message_start","message":{"id":"m1","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"looking now"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a\"}"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}"#,
            r#"{"type":"message_stop"}"#,
        ],
        &[
            r#"{"type":"response.created","response":{"id":"r1"}}"#,
            r#"{"type":"response.output_text.delta","delta":"looking now"}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"f1","call_id":"t1","name":"read","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"f1","arguments":"{\"path\":\"a\"}"}"#,
            r#"{"type":"response.completed","response":{"id":"r1","usage":{"input_tokens":1,"output_tokens":4}}}"#,
        ],
        &[
            r#"{"id":"c1","choices":[{"index":0,"delta":{"content":"looking now"}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"t1","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ],
    )
}

fn malformed_arguments() -> Script {
    Script::new(
        &[
            r#"{"type":"message_start","message":{"id":"m1","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}"#,
            r#"{"type":"message_stop"}"#,
        ],
        &[
            r#"{"type":"response.created","response":{"id":"r1"}}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"f1","call_id":"t1","name":"read","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"f1","arguments":"{\"path\":"}"#,
            r#"{"type":"response.completed","response":{"id":"r1","usage":{"input_tokens":1,"output_tokens":2}}}"#,
        ],
        &[
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"t1","function":{"name":"read","arguments":"{\"path\":"}}]}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ],
    )
}

fn no_terminal_event() -> Script {
    Script::new(
        &[
            r#"{"type":"message_start","message":{"id":"m1","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"cut off"}}"#,
        ],
        &[
            r#"{"type":"response.created","response":{"id":"r1"}}"#,
            r#"{"type":"response.output_text.delta","delta":"cut off"}"#,
        ],
        &[r#"{"id":"c1","choices":[{"index":0,"delta":{"content":"cut off"}}]}"#],
    )
}

// -------------------------------------------------------------- scenarios ---

#[tokio::test]
async fn every_adapter_streams_a_plain_answer_the_same_way() {
    for provider in Provider::ALL {
        let events = run(provider, plain_answer().for_provider(provider))
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", provider.name()));
        let (text, calls, stop) = shape(&events);

        assert_eq!(text, "the answer", "{}", provider.name());
        assert!(calls.is_empty(), "{}", provider.name());
        assert_eq!(stop, Some(StopReason::EndTurn), "{}", provider.name());
    }
}

#[tokio::test]
async fn every_adapter_reports_usage_from_one_plain_answer() {
    for provider in Provider::ALL {
        let events = run(provider, plain_answer().for_provider(provider))
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", provider.name()));
        let usage =
            usage(&events).unwrap_or_else(|| panic!("{}: no usage reported", provider.name()));

        // Twenty total, five of them cached, whatever the wire called them.
        assert_eq!(usage.input_tokens, 20, "{}", provider.name());
        assert_eq!(usage.cached_input_tokens, 5, "{}", provider.name());
        assert_eq!(usage.output_tokens, 7, "{}", provider.name());
    }
}

#[tokio::test]
async fn every_adapter_reports_two_tool_calls_in_one_turn() {
    for provider in Provider::ALL {
        let events = run(provider, two_tool_calls().for_provider(provider))
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", provider.name()));
        let (_, calls, stop) = shape(&events);

        assert_eq!(
            calls,
            vec![
                ("read".to_owned(), r#"{"path":"a"}"#.to_owned()),
                ("read".to_owned(), r#"{"path":"b"}"#.to_owned()),
            ],
            "{}",
            provider.name()
        );
        assert_eq!(stop, Some(StopReason::ToolUse), "{}", provider.name());
    }
}

#[tokio::test]
async fn every_adapter_keeps_text_and_a_tool_call_from_one_turn() {
    for provider in Provider::ALL {
        let events = run(provider, text_then_tool().for_provider(provider))
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", provider.name()));
        let (text, calls, stop) = shape(&events);

        assert_eq!(text, "looking now", "{}", provider.name());
        assert_eq!(calls.len(), 1, "{}", provider.name());
        assert_eq!(stop, Some(StopReason::ToolUse), "{}", provider.name());
    }
}

#[tokio::test]
async fn every_adapter_marks_truncated_tool_arguments_instead_of_building_a_call() {
    for provider in Provider::ALL {
        let events = run(provider, malformed_arguments().for_provider(provider))
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", provider.name()));

        // Never a usable call built from an unparsable fragment: the arguments
        // are marked, so the tool set refuses the call and the model can retry.
        let call = events
            .iter()
            .find_map(|event| match event {
                ModelEvent::ToolCallCompleted { call } => Some(call),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{}: no call completed", provider.name()));
        assert!(
            call.arguments
                .get(dino_protocol::UNPARSED_ARGUMENTS)
                .is_some(),
            "{}: {}",
            provider.name(),
            call.arguments
        );
    }
}

#[tokio::test]
async fn every_adapter_refuses_a_stream_with_no_terminal_event() {
    for provider in Provider::ALL {
        let error = run(provider, no_terminal_event().for_provider(provider))
            .await
            .err()
            .unwrap_or_else(|| panic!("{}: a truncated stream was accepted", provider.name()));

        assert!(
            matches!(error, ModelError::Protocol(_)),
            "{}: {error}",
            provider.name()
        );
    }
}

#[tokio::test]
async fn every_adapter_returns_a_tool_result_under_its_own_call_id() {
    // Scenario 2, the half a single-turn test cannot reach. The three wires
    // carry a result three different ways: a `tool_result` content block, a
    // `function_call_output` item, and a message with `role: "tool"`. What must
    // be true of all three is that the next request carries the output, tied to
    // the ID the provider issued.
    let answer = plain_answer();
    for provider in Provider::ALL {
        let (base, bodies) = serve_turns(vec![
            two_tool_calls().for_provider(provider),
            answer.for_provider(provider),
        ])
        .await;
        let mut session = build(provider, base);

        let first = session
            .next(TurnInput::User {
                content: "go".into(),
            })
            .await
            .unwrap();
        let events = first.try_collect::<Vec<_>>().await.unwrap();
        let (_, calls, _) = shape(&events);
        assert_eq!(calls.len(), 2, "{}", provider.name());

        let results = vec![
            dino_protocol::ToolResult {
                call_id: "t1".into(),
                output: dino_protocol::ToolOutput::success("contents of a"),
            },
            dino_protocol::ToolResult {
                call_id: "t2".into(),
                output: dino_protocol::ToolOutput::error("b is missing"),
            },
        ];
        let second = session
            .next(TurnInput::ToolResults {
                results,
                note: None,
            })
            .await
            .unwrap();
        let _ = second.try_collect::<Vec<_>>().await.unwrap();

        let sent = bodies.await.unwrap();
        assert_eq!(sent.len(), 2, "{}", provider.name());
        let request = &sent[1];

        for (id, output) in [("t1", "contents of a"), ("t2", "b is missing")] {
            assert!(
                request.contains(id),
                "{}: call id {id} never reached the next request",
                provider.name()
            );
            assert!(
                request.contains(output),
                "{}: output for {id} never reached the next request",
                provider.name()
            );
        }
    }
}
