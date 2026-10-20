//! Qwen sessions over an OpenAI-compatible Chat Completions endpoint.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_stream::try_stream;
use futures_util::StreamExt;
use gyr_protocol::ModelEvent;
use gyr_protocol::ModelProfile;
use gyr_protocol::ProviderKind;
use gyr_protocol::ReasoningSupport;
use gyr_protocol::SamplingDefaults;
use gyr_protocol::StopReason;
use gyr_protocol::TokenUsage;
use gyr_protocol::ToolCall;
use gyr_protocol::ToolDefinition;
use gyr_protocol::TurnInput;
use reqwest::Client;
use reqwest::Url;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::ELIDED_MARKER_FLOOR;
use crate::Elision;
use crate::ModelError;
use crate::ModelEventStream;
use crate::ModelFuture;
use crate::ModelSession;
use crate::SessionState;
use crate::elided_marker;
use crate::sse::SseDecoder;

const ERROR_BODY_LIMIT: usize = 4_096;

/// Bump when the persisted history shape changes. RFC-0014 section 3.
const STATE_VERSION: u32 = 1;

#[derive(Clone)]
pub struct QwenConfig {
    pub api_base: String,
    pub api_key: Option<String>,
    pub profile: ModelProfile,
    pub system_prompt: String,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: Option<u32>,
    pub sampling: Option<SamplingDefaults>,
    pub enable_thinking: Option<bool>,
    pub preserve_thinking: bool,
    pub retry: RetryPolicy,
    /// Most reasoning tokens per turn, through vLLM's `thinking_token_budget`.
    /// Measured on vLLM 0.32 with Qwen3.8 27B: reasoning stops one token short
    /// of the budget and the model still answers. Servers that do not know the
    /// field ignore it.
    pub thinking_budget: Option<u32>,
}

/// How often a request that never reached the model is tried again.
///
/// Only before anything has streamed: a request that failed there produced
/// nothing, so sending it again cannot duplicate a turn.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts, the first included.
    pub attempts: u32,
    /// Doubled after each failure.
    pub first_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 5,
            first_delay: Duration::from_secs(2),
        }
    }
}

impl QwenConfig {
    #[must_use]
    pub fn new(api_base: impl Into<String>, profile: ModelProfile) -> Self {
        Self {
            api_base: api_base.into(),
            api_key: None,
            profile,
            system_prompt: String::new(),
            tools: Vec::new(),
            max_output_tokens: None,
            sampling: None,
            enable_thinking: None,
            preserve_thinking: true,
            retry: RetryPolicy::default(),
            thinking_budget: None,
        }
    }
}

pub struct QwenSession {
    client: Client,
    endpoint: Url,
    config: QwenConfig,
    history: Arc<Mutex<Vec<ChatMessage>>>,
    limits_discovered: bool,
}

impl QwenSession {
    /// Creates a Qwen session for a vLLM, `SGLang`, or compatible endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Configuration`] when the selected profile is not
    /// a Qwen profile or the API base is not a valid URL.
    pub fn new(config: QwenConfig) -> Result<Self, ModelError> {
        if config.profile.provider != ProviderKind::Qwen {
            return Err(ModelError::Configuration(format!(
                "profile {} is not a Qwen profile",
                config.profile.key
            )));
        }

        let endpoint = format!("{}/chat/completions", config.api_base.trim_end_matches('/'));
        let endpoint = Url::parse(&endpoint).map_err(|error| {
            ModelError::Configuration(format!("invalid Qwen API base: {error}"))
        })?;

        Ok(Self {
            client: Client::new(),
            endpoint,
            config,
            history: Arc::new(Mutex::new(Vec::new())),
            limits_discovered: false,
        })
    }

    fn pending_messages(input: TurnInput) -> Vec<ChatMessage> {
        match input {
            TurnInput::User { content } => vec![ChatMessage::user(content)],
            TurnInput::ToolResults { results } => results
                .into_iter()
                .map(|result| {
                    let content = if result.output.is_error {
                        format!("Tool error: {}", result.output.content)
                    } else {
                        result.output.content
                    };
                    ChatMessage::tool(result.call_id, content)
                })
                .collect(),
        }
    }

    async fn send_with_retry(
        &self,
        body: &serde_json::Map<String, Value>,
    ) -> Result<reqwest::Response, ModelError> {
        let mut delay = self.config.retry.first_delay;
        let mut attempt = 1;
        loop {
            let mut request = self.client.post(self.endpoint.clone()).json(body);
            if let Some(api_key) = &self.config.api_key {
                request = request.bearer_auth(api_key);
            }
            let (error, retryable) = match request.send().await {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let status = response.status();
                    let response_body = response
                        .text()
                        .await
                        .unwrap_or_else(|error| format!("could not read response body: {error}"));
                    // A proxy in front of the server answers 404 with nothing
                    // while the server is away; the server's own 404, for a
                    // model it does not have, says so and will not change.
                    let retryable = matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
                        || (status.as_u16() == 404 && response_body.trim().is_empty());
                    let message = format!(
                        "Qwen endpoint returned {status}: {}",
                        truncate_for_error(&response_body)
                    );
                    (message, retryable)
                }
                Err(error) => (error.to_string(), error.is_connect() || error.is_timeout()),
            };
            if !retryable || attempt >= self.config.retry.attempts {
                return Err(ModelError::Transport(error));
            }
            tokio::time::sleep(delay).await;
            delay = delay.saturating_mul(2);
            attempt += 1;
        }
    }

    fn request_body(
        &self,
        pending: &[ChatMessage],
    ) -> Result<serde_json::Map<String, Value>, ModelError> {
        let history = self
            .history
            .lock()
            .map_err(|_| ModelError::Protocol("Qwen history lock was poisoned".into()))?;
        let mut messages = Vec::with_capacity(history.len() + pending.len() + 1);
        if !self.config.system_prompt.is_empty() {
            messages.push(ChatMessage::system(self.config.system_prompt.clone()));
        }
        // Reasoning is most of what a thinking model writes, so replaying it
        // all would fill a self-hosted context window in a few turns. It goes
        // back only when the profile asks the template to preserve it.
        let replay_reasoning = matches!(self.config.profile.reasoning, ReasoningSupport::Toggle)
            && self.config.preserve_thinking;
        messages.extend(history.iter().map(|message| {
            let mut message = message.clone();
            if !replay_reasoning {
                message.reasoning_content = None;
            }
            message
        }));
        messages.extend_from_slice(pending);
        drop(history);

        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(self.config.profile.provider_model));
        body.insert("messages".into(), json!(messages));
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({ "include_usage": true }));

        if let Some(max_tokens) = self.config.max_output_tokens {
            body.insert("max_tokens".into(), json!(max_tokens));
        }
        if let Some(budget) = self.config.thinking_budget {
            body.insert("thinking_token_budget".into(), json!(budget));
        }
        let profile_sampling = if matches!(self.config.profile.reasoning, ReasoningSupport::Toggle)
            && self.config.enable_thinking == Some(false)
        {
            Some(SamplingDefaults {
                temperature: 0.7,
                top_p: 0.8,
                top_k: Some(20),
                min_p: Some(0.0),
                presence_penalty: Some(1.5),
                repetition_penalty: Some(1.0),
            })
        } else {
            self.config.profile.sampling
        };
        if let Some(sampling) = self.config.sampling.or(profile_sampling) {
            body.insert("temperature".into(), json!(sampling.temperature));
            body.insert("top_p".into(), json!(sampling.top_p));
            if let Some(top_k) = sampling.top_k {
                body.insert("top_k".into(), json!(top_k));
            }
            if let Some(min_p) = sampling.min_p {
                body.insert("min_p".into(), json!(min_p));
            }
            if let Some(presence_penalty) = sampling.presence_penalty {
                body.insert("presence_penalty".into(), json!(presence_penalty));
            }
            if let Some(repetition_penalty) = sampling.repetition_penalty {
                body.insert("repetition_penalty".into(), json!(repetition_penalty));
            }
        }
        if !self.config.tools.is_empty() {
            let tools = self
                .config
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.input_schema,
                        }
                    })
                })
                .collect::<Vec<_>>();
            body.insert("tools".into(), Value::Array(tools));
            body.insert("tool_choice".into(), json!("auto"));
            // An adapter may expose less than a model can do and never more,
            // per RFC-0001 section 5. Asking a profile that says no for
            // parallel calls would be asking for more.
            body.insert(
                "parallel_tool_calls".into(),
                json!(self.config.profile.parallel_tool_calls),
            );
        }

        if matches!(self.config.profile.reasoning, ReasoningSupport::Toggle) {
            let mut template = serde_json::Map::new();
            if let Some(enable_thinking) = self.config.enable_thinking {
                template.insert("enable_thinking".into(), json!(enable_thinking));
            }
            if self.config.preserve_thinking {
                template.insert("preserve_thinking".into(), json!(true));
            }
            if !template.is_empty() {
                body.insert("chat_template_kwargs".into(), Value::Object(template));
            }
        }

        Ok(body)
    }
}

impl ModelSession for QwenSession {
    fn profile(&self) -> &ModelProfile {
        &self.config.profile
    }

    fn export_state(&self) -> Result<SessionState, ModelError> {
        let history = self
            .history
            .lock()
            .map_err(|_| ModelError::Protocol("Qwen history lock was poisoned".into()))?;
        Ok(SessionState {
            provider: ProviderKind::Qwen,
            model_key: self.config.profile.key.clone(),
            version: STATE_VERSION,
            payload: serde_json::to_value(&*history).map_err(|error| {
                ModelError::Protocol(format!("cannot serialise Qwen history: {error}"))
            })?,
        })
    }

    fn restore_state(&mut self, state: &SessionState) -> Result<(), ModelError> {
        state.check(&self.config.profile, STATE_VERSION)?;
        let restored: Vec<ChatMessage> =
            serde_json::from_value(state.payload.clone()).map_err(|error| {
                ModelError::Configuration(format!("cannot read Qwen history: {error}"))
            })?;
        let mut history = self
            .history
            .lock()
            .map_err(|_| ModelError::Protocol("Qwen history lock was poisoned".into()))?;
        *history = restored;
        Ok(())
    }

    fn elide_tool_results(&mut self, keep_recent: usize) -> Result<Elision, ModelError> {
        let mut history = self
            .history
            .lock()
            .map_err(|_| ModelError::Protocol("Qwen history lock was poisoned".into()))?;

        let mut seen = 0_usize;
        let mut elision = Elision::default();
        for message in history.iter_mut().rev() {
            if message.role != Role::Tool {
                continue;
            }
            seen += 1;
            if seen <= keep_recent {
                continue;
            }
            let Some(content) = message.content.as_ref() else {
                continue;
            };
            let bytes = content.len();
            if bytes <= ELIDED_MARKER_FLOOR {
                continue;
            }
            let replacement = elided_marker(bytes);
            elision.bytes_reclaimed += bytes.saturating_sub(replacement.len());
            elision.results_elided += 1;
            // The tool_call_id is left alone: the provider needs every call to
            // keep a matching result, and an elided one is still a result.
            message.content = Some(replacement);
        }
        Ok(elision)
    }

    fn discover_limits(&mut self) -> ModelFuture<'_, ()> {
        Box::pin(async move {
            if std::mem::replace(&mut self.limits_discovered, true) {
                return Ok(());
            }
            // The profile records the model's native window, but a server is
            // often started with less, and only the server knows how much.
            let url = format!("{}/models", self.config.api_base.trim_end_matches('/'));
            let mut request = self.client.get(url).timeout(Duration::from_secs(10));
            if let Some(api_key) = &self.config.api_key {
                request = request.bearer_auth(api_key);
            }
            let Ok(response) = request.send().await else {
                return Ok(());
            };
            let Ok(models) = response.json::<ModelList>().await else {
                return Ok(());
            };
            let served = models
                .data
                .into_iter()
                .find(|model| model.id == self.config.profile.provider_model)
                .and_then(|model| model.max_model_len);
            if let Some(served) = served {
                let window = &mut self.config.profile.context_window_tokens;
                *window = Some(window.map_or(served, |native| native.min(served)));
            }
            Ok(())
        })
    }

    fn next(&mut self, input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
        Box::pin(async move {
            let pending = Self::pending_messages(input);
            let body = self.request_body(&pending)?;
            let response = self.send_with_retry(&body).await?;

            let history = Arc::clone(&self.history);
            let stream = try_stream! {
                let mut chunks = response.bytes_stream();
                let mut decoder = SseDecoder::default();
                let mut parser = QwenStreamParser::default();
                let mut done = false;

                while let Some(chunk) = chunks.next().await {
                    let chunk = chunk.map_err(|error| ModelError::Transport(error.to_string()))?;
                    for data in decoder.push(&chunk)? {
                        if data == "[DONE]" {
                            let (final_events, assistant) = parser.finish()?;
                            commit_history(&history, &pending, assistant)?;
                            for event in final_events {
                                yield event;
                            }
                            done = true;
                            break;
                        }

                        for event in parser.consume(&data)? {
                            yield event;
                        }
                    }
                    if done {
                        break;
                    }
                }

                if !done {
                    Err(ModelError::Protocol(
                        "Qwen stream ended before the [DONE] marker".into(),
                    ))?;
                }
            };

            Ok(Box::pin(stream) as ModelEventStream)
        })
    }
}

fn commit_history(
    history: &Mutex<Vec<ChatMessage>>,
    pending: &[ChatMessage],
    assistant: ChatMessage,
) -> Result<(), ModelError> {
    let mut history = history
        .lock()
        .map_err(|_| ModelError::Protocol("Qwen history lock was poisoned".into()))?;
    history.extend(pending.iter().cloned());
    history.push(assistant);
    Ok(())
}

fn truncate_for_error(body: &str) -> &str {
    if body.len() <= ERROR_BODY_LIMIT {
        return body;
    }
    let mut boundary = ERROR_BODY_LIMIT;
    while !body.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &body[..boundary]
}

/// A closed set rather than a string, for the same reason as Anthropic's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ChatMessage {
    role: Role,
    // Every optional field is skipped when empty on the way out and defaulted on
    // the way back in. Without the defaults a message that legitimately has no
    // tool calls serialises fine and then fails to read, which is what the
    // round-trip test caught.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ChatToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

impl ChatMessage {
    fn system(content: String) -> Self {
        Self::plain(Role::System, content)
    }

    fn user(content: String) -> Self {
        Self::plain(Role::User, content)
    }

    fn plain(role: Role, content: String) -> Self {
        Self {
            role,
            content: Some(content),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    fn tool(tool_call_id: String, content: String) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id),
        }
    }
}

/// The only kind of tool call this surface defines. An enum rather than a
/// literal so a restored history is validated rather than trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CallKind {
    Function,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct ChatToolCall {
    id: String,
    r#type: CallKind,
    function: ChatFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct ChatFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Default)]
struct QwenStreamParser {
    started: bool,
    content: String,
    reasoning: String,
    tool_calls: BTreeMap<u32, ToolCallAccumulator>,
    finish_reason: Option<StopReason>,
}

impl QwenStreamParser {
    fn consume(&mut self, data: &str) -> Result<Vec<ModelEvent>, ModelError> {
        let chunk: ChatCompletionChunk = serde_json::from_str(data)
            .map_err(|error| ModelError::Protocol(format!("invalid Qwen stream JSON: {error}")))?;
        let mut events = Vec::new();

        if !self.started {
            events.push(ModelEvent::Started {
                response_id: chunk.id.clone(),
            });
            self.started = true;
        }

        if let Some(usage) = chunk.usage {
            events.push(ModelEvent::Usage {
                usage: TokenUsage {
                    input_tokens: usage.prompt_tokens,
                    cached_input_tokens: usage
                        .prompt_tokens_details
                        .map_or(0, |details| details.cached_tokens),
                    output_tokens: usage.completion_tokens,
                    reasoning_tokens: usage
                        .completion_tokens_details
                        .map_or(0, |details| details.reasoning_tokens),
                },
            });
        }

        for choice in chunk.choices {
            if choice.index != 0 {
                return Err(ModelError::Protocol(format!(
                    "Qwen returned unsupported choice index {}",
                    choice.index
                )));
            }
            if let Some(content) = choice.delta.content {
                self.content.push_str(&content);
                if !content.is_empty() {
                    events.push(ModelEvent::TextDelta { text: content });
                }
            }
            if let Some(reasoning) = choice.delta.reasoning_content.or(choice.delta.reasoning) {
                self.reasoning.push_str(&reasoning);
                if !reasoning.is_empty() {
                    events.push(ModelEvent::ReasoningDelta { text: reasoning });
                }
            }
            for delta in choice.delta.tool_calls {
                let accumulator = self.tool_calls.entry(delta.index).or_default();
                let mut argument_delta = None;
                if let Some(id) = delta.id {
                    accumulator.id.push_str(&id);
                }
                if let Some(function) = delta.function {
                    if let Some(name) = function.name {
                        accumulator.name.push_str(&name);
                    }
                    if let Some(arguments) = function.arguments {
                        accumulator.arguments.push_str(&arguments);
                        argument_delta = Some(arguments);
                    }
                }
                if !accumulator.started
                    && !accumulator.id.is_empty()
                    && !accumulator.name.is_empty()
                {
                    events.push(ModelEvent::ToolCallStarted {
                        id: accumulator.id.clone(),
                        name: accumulator.name.clone(),
                    });
                    accumulator.started = true;
                }
                if let Some(delta) = argument_delta {
                    events.push(ModelEvent::ToolCallArgumentsDelta {
                        id: accumulator.id.clone(),
                        delta,
                    });
                }
            }
            if let Some(reason) = choice.finish_reason {
                let reason = parse_finish_reason(&reason)?;
                if self.finish_reason.replace(reason).is_some() {
                    return Err(ModelError::Protocol(
                        "Qwen returned more than one finish reason".into(),
                    ));
                }
            }
        }

        Ok(events)
    }

    fn finish(&mut self) -> Result<(Vec<ModelEvent>, ChatMessage), ModelError> {
        let reason = self.finish_reason.ok_or_else(|| {
            ModelError::Protocol("Qwen sent [DONE] without a finish reason".into())
        })?;
        let mut events = Vec::new();
        let mut wire_calls = Vec::with_capacity(self.tool_calls.len());

        for accumulator in self.tool_calls.values() {
            if accumulator.id.is_empty() || accumulator.name.is_empty() {
                return Err(ModelError::Protocol(
                    "Qwen returned an incomplete tool identity".into(),
                ));
            }
            if !accumulator.started {
                events.push(ModelEvent::ToolCallStarted {
                    id: accumulator.id.clone(),
                    name: accumulator.name.clone(),
                });
            }
            let arguments = serde_json::from_str(&accumulator.arguments).map_err(|error| {
                ModelError::Protocol(format!(
                    "Qwen returned invalid arguments for {}: {error}",
                    accumulator.name
                ))
            })?;
            events.push(ModelEvent::ToolCallCompleted {
                call: ToolCall {
                    id: accumulator.id.clone(),
                    name: accumulator.name.clone(),
                    arguments,
                },
            });
            wire_calls.push(ChatToolCall {
                id: accumulator.id.clone(),
                r#type: CallKind::Function,
                function: ChatFunction {
                    name: accumulator.name.clone(),
                    arguments: accumulator.arguments.clone(),
                },
            });
        }
        events.push(ModelEvent::Finished { reason });

        let assistant = ChatMessage {
            role: Role::Assistant,
            content: (!self.content.is_empty()).then(|| self.content.clone()),
            reasoning_content: (!self.reasoning.is_empty()).then(|| self.reasoning.clone()),
            tool_calls: wire_calls,
            tool_call_id: None,
        };
        Ok((events, assistant))
    }
}

fn parse_finish_reason(reason: &str) -> Result<StopReason, ModelError> {
    match reason {
        "stop" => Ok(StopReason::EndTurn),
        "tool_calls" => Ok(StopReason::ToolUse),
        "length" => Ok(StopReason::MaxTokens),
        "content_filter" => Ok(StopReason::Refusal),
        other => Err(ModelError::Protocol(format!(
            "unknown Qwen finish reason {other:?}"
        ))),
    }
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChunk {
    id: Option<String>,
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct ModelList {
    data: Vec<ServedModel>,
}

#[derive(Debug, Deserialize)]
struct ServedModel {
    id: String,
    max_model_len: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    index: u32,
    delta: Delta,
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    content: Option<String>,
    reasoning_content: Option<String>,
    // vLLM renamed the field; 0.32 sends only `reasoning`. Kept apart rather
    // than aliased, because a server sending both would fail as a duplicate.
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    index: u32,
    id: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    prompt_tokens: u64,
    completion_tokens: u64,
    prompt_tokens_details: Option<PromptTokenDetails>,
    completion_tokens_details: Option<CompletionTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct CompletionTokenDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

#[cfg(test)]
mod tests {
    use futures_util::TryStreamExt;
    use pretty_assertions::assert_eq;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn state_round_trips_into_a_request_that_is_byte_identical() {
        let profile = crate::builtin_profile("qwen3-coder-next").unwrap();
        let build = || {
            QwenSession::new(QwenConfig::new("http://127.0.0.1:8000/v1", profile.clone())).unwrap()
        };
        let original = build();
        original.history.lock().unwrap().extend([
            ChatMessage::plain(Role::User, "read the file".to_owned()),
            ChatMessage {
                role: Role::Assistant,
                content: None,
                reasoning_content: None,
                tool_calls: vec![ChatToolCall {
                    id: "call-1".into(),
                    r#type: CallKind::Function,
                    function: ChatFunction {
                        name: "read".into(),
                        arguments: "{\"path\":\"src/lib.rs\"}".into(),
                    },
                }],
                tool_call_id: None,
            },
            ChatMessage {
                role: Role::Tool,
                content: Some("fn main() {}".to_owned()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                tool_call_id: Some("call-1".into()),
            },
        ]);

        let state = original.export_state().unwrap();
        let mut restored = build();
        restored.restore_state(&state).unwrap();

        let pending = QwenSession::pending_messages(TurnInput::User {
            content: "and now?".into(),
        });
        assert_eq!(
            restored.request_body(&pending).unwrap(),
            original.request_body(&pending).unwrap()
        );
    }

    #[test]
    fn elision_hollows_the_oldest_tool_messages_and_keeps_their_ids() {
        let profile = crate::builtin_profile("qwen3-coder-next").unwrap();
        let mut session =
            QwenSession::new(QwenConfig::new("http://127.0.0.1:8000/v1", profile)).unwrap();
        {
            let mut history = session.history.lock().unwrap();
            for index in 0..4 {
                history.push(ChatMessage {
                    role: Role::Tool,
                    content: Some("y".repeat(1_000)),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    tool_call_id: Some(format!("call-{index}")),
                });
            }
            // An assistant turn between them must survive untouched.
            history.push(ChatMessage::plain(
                Role::Assistant,
                "carrying on".to_owned(),
            ));
        }

        let elision = session.elide_tool_results(2).unwrap();

        let history = session.history.lock().unwrap();
        assert_eq!(elision.results_elided, 2);
        for (index, message) in history.iter().take(4).enumerate() {
            let content = message.content.as_deref().unwrap();
            if index < 2 {
                assert!(content.starts_with("[gyrfalcon elided"), "{index}");
            } else {
                assert_eq!(content.len(), 1_000, "{index}");
            }
            assert_eq!(
                message.tool_call_id.as_deref(),
                Some(&*format!("call-{index}"))
            );
        }
        assert_eq!(history[4].content.as_deref(), Some("carrying on"));
    }

    #[test]
    fn parser_reads_reasoning_in_the_shape_vllm_0_32_sends() {
        let mut parser = QwenStreamParser::default();
        let chunks = [
            r#"{"id":"chat-1","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
            r#"{"id":"chat-1","choices":[{"index":0,"delta":{"reasoning":"check the "},"finish_reason":null}]}"#,
            r#"{"id":"chat-1","choices":[{"index":0,"delta":{"reasoning":"borrow"},"finish_reason":null}]}"#,
            r#"{"id":"chat-1","choices":[{"index":0,"delta":{"content":"Done."},"finish_reason":"stop"}]}"#,
        ];
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(parser.consume(chunk).unwrap());
        }
        let (_, assistant) = parser.finish().unwrap();

        let reasoning: String = events
            .iter()
            .filter_map(|event| match event {
                ModelEvent::ReasoningDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, "check the borrow");
        // Carried on the assistant message, so the next turn replays it.
        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some("check the borrow")
        );
    }

    #[test]
    fn parser_accumulates_reasoning_text_tools_and_usage() {
        let mut parser = QwenStreamParser::default();
        let chunks = [
            r#"{"id":"chat-1","choices":[{"index":0,"delta":{"reasoning_content":"inspect "},"finish_reason":null}]}"#,
            r#"{"id":"chat-1","choices":[{"index":0,"delta":{"content":"I will read.","tool_calls":[{"index":0,"id":"call-1","function":{"name":"read_file","arguments":"{\"path\":"}}]},"finish_reason":null}]}"#,
            r#"{"id":"chat-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"src/lib.rs\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            r#"{"id":"chat-1","choices":[],"usage":{"prompt_tokens":20,"completion_tokens":8,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":2}}}"#,
        ];
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(parser.consume(chunk).unwrap());
        }
        let (final_events, assistant) = parser.finish().unwrap();
        events.extend(final_events);

        assert_eq!(
            events.last(),
            Some(&ModelEvent::Finished {
                reason: StopReason::ToolUse
            })
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallCompleted { call }
                if call.arguments == json!({"path": "src/lib.rs"})
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::Usage { usage }
                if usage.cached_input_tokens == 4 && usage.reasoning_tokens == 2
        )));
        assert_eq!(assistant.reasoning_content.as_deref(), Some("inspect "));
        assert_eq!(assistant.tool_calls.len(), 1);
        let started = events
            .iter()
            .position(|event| matches!(event, ModelEvent::ToolCallStarted { .. }))
            .unwrap();
        let arguments = events
            .iter()
            .position(|event| matches!(event, ModelEvent::ToolCallArgumentsDelta { .. }))
            .unwrap();
        assert!(started < arguments);
    }

    #[test]
    fn parser_rejects_malformed_tool_arguments() {
        let mut parser = QwenStreamParser::default();
        parser
            .consume(
                r#"{"id":"chat-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read_file","arguments":"{"}}]},"finish_reason":"tool_calls"}]}"#,
            )
            .unwrap();

        assert!(matches!(parser.finish(), Err(ModelError::Protocol(_))));
    }

    #[test]
    fn request_preserves_provider_native_tool_history_shape() {
        let profile = crate::builtin_profile("qwen3.6-27b").unwrap();
        let mut config = QwenConfig::new("http://127.0.0.1:8000/v1", profile);
        config.enable_thinking = Some(true);
        config.tools.push(ToolDefinition {
            name: "read_file".into(),
            description: "Read a file".into(),
            input_schema: json!({"type": "object"}),
        });
        let session = QwenSession::new(config).unwrap();
        let pending = QwenSession::pending_messages(TurnInput::ToolResults {
            results: vec![gyr_protocol::ToolResult {
                call_id: "call-1".into(),
                output: gyr_protocol::ToolOutput::success("fn main() {}"),
            }],
        });
        let body = session.request_body(&pending).unwrap();

        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["messages"][0]["role"], "tool");
        assert_eq!(
            body["chat_template_kwargs"],
            json!({"enable_thinking": true, "preserve_thinking": true})
        );
    }

    #[test]
    fn a_thinking_budget_is_sent_only_when_set() {
        let profile = crate::builtin_profile("qwen3.8-27b").unwrap();
        let mut config = QwenConfig::new("http://127.0.0.1:8000/v1", profile);
        let unset = QwenSession::new(config.clone()).unwrap();
        assert!(
            !unset
                .request_body(&[])
                .unwrap()
                .contains_key("thinking_token_budget")
        );

        config.thinking_budget = Some(4_096);
        let set = QwenSession::new(config).unwrap();
        assert_eq!(
            set.request_body(&[]).unwrap()["thinking_token_budget"],
            4_096
        );
    }

    #[test]
    fn reasoning_is_replayed_only_when_the_profile_preserves_thinking() {
        let replayed = |key: &str| {
            let profile = crate::builtin_profile(key).unwrap();
            let session =
                QwenSession::new(QwenConfig::new("http://127.0.0.1:8000/v1", profile)).unwrap();
            session.history.lock().unwrap().push(ChatMessage {
                role: Role::Assistant,
                content: Some("reading".into()),
                reasoning_content: Some("the bug is in wrap".into()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            });
            let body = session.request_body(&[]).unwrap();
            let assistant = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["role"] == "assistant")
                .unwrap()
                .clone();
            assistant.get("reasoning_content").cloned()
        };

        assert_eq!(replayed("qwen3.8-27b"), None);
        assert_eq!(replayed("qwen3.6-27b"), Some(json!("the bug is in wrap")));
    }

    #[test]
    fn parallel_tool_calls_follows_the_profile_rather_than_the_adapter() {
        let profile = crate::builtin_profile("qwen3-8b").unwrap();
        assert!(
            !profile.parallel_tool_calls,
            "fixture assumes a serial model"
        );
        let mut config = QwenConfig::new("http://127.0.0.1:8000/v1", profile);
        config.tools = vec![ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: json!({"type": "object"}),
        }];
        let session = QwenSession::new(config).unwrap();
        let pending = QwenSession::pending_messages(TurnInput::User {
            content: "hello".into(),
        });

        let body = session.request_body(&pending).unwrap();

        assert_eq!(body["parallel_tool_calls"], false);
    }

    #[test]
    fn non_thinking_qwen_3_6_uses_its_own_sampling_profile() {
        let profile = crate::builtin_profile("qwen3.6-35b-a3b").unwrap();
        let mut config = QwenConfig::new("http://127.0.0.1:8000/v1", profile);
        config.enable_thinking = Some(false);
        let session = QwenSession::new(config).unwrap();
        let pending = QwenSession::pending_messages(TurnInput::User {
            content: "hello".into(),
        });
        let body = session.request_body(&pending).unwrap();

        assert_eq!(body["temperature"], 0.7);
        assert_eq!(body["top_p"], 0.8);
        assert_eq!(body["presence_penalty"], 1.5);
    }

    #[tokio::test]
    async fn session_posts_and_streams_a_plain_answer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 2_048];
            let expected_length = loop {
                let read = socket.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0, "request ended before its body");
                request.extend_from_slice(&buffer[..read]);
                if let Some(headers_end) = find_bytes(&request, b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..headers_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    let expected_length = headers_end + 4 + content_length;
                    if request.len() >= expected_length {
                        break expected_length;
                    }
                }
            };

            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            socket
                .write_all(
                    b"data: {\"id\":\"chat-live\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\n",
                )
                .await
                .unwrap();
            socket.write_all(b"data: [DONE]\n\n").await.unwrap();

            request.truncate(expected_length);
            String::from_utf8(request).unwrap()
        });

        let profile = crate::builtin_profile("qwen3-coder-next").unwrap();
        let config = QwenConfig::new(format!("http://{address}/v1"), profile);
        let mut session = QwenSession::new(config).unwrap();
        let stream = session
            .next(TurnInput::User {
                content: "say hello".into(),
            })
            .await
            .unwrap();
        let events = stream.try_collect::<Vec<_>>().await.unwrap();
        let request = server.await.unwrap();

        assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(request.contains("\"content\":\"say hello\""));
        assert!(
            events.iter().any(|event| {
                matches!(event, ModelEvent::TextDelta { text } if text == "hello")
            })
        );
        assert_eq!(
            events.last(),
            Some(&ModelEvent::Finished {
                reason: StopReason::EndTurn
            })
        );
    }

    /// Answers one request with `body` as JSON, and returns the request line.
    async fn serve_json_once(body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 2_048];
            while find_bytes(&request, b"\r\n\r\n").is_none() {
                let read = socket.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&buffer[..read]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request)
                .lines()
                .next()
                .unwrap()
                .to_owned()
        });
        (base, server)
    }

    const SSE_HELLO: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"id\":\"chat-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

    /// Answers one connection per response, in order, and counts the requests.
    async fn serve_in_turn(responses: Vec<String>) -> (String, tokio::task::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut served = 0;
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4_096];
                while find_bytes(&request, b"\r\n\r\n").is_none() {
                    let read = socket.read(&mut buffer).await.unwrap();
                    assert_ne!(read, 0);
                    request.extend_from_slice(&buffer[..read]);
                }
                socket.write_all(response.as_bytes()).await.unwrap();
                served += 1;
            }
            served
        });
        (base, server)
    }

    fn quick_retries(base: String) -> QwenSession {
        let profile = crate::builtin_profile("qwen3.8-27b").unwrap();
        let mut config = QwenConfig::new(base, profile);
        config.retry = RetryPolicy {
            attempts: 3,
            first_delay: Duration::from_millis(1),
        };
        QwenSession::new(config).unwrap()
    }

    #[tokio::test]
    async fn a_proxy_that_briefly_loses_the_server_is_retried() {
        let (base, server) = serve_in_turn(vec![
            "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(),
            "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                .into(),
            SSE_HELLO.into(),
        ])
        .await;
        let mut session = quick_retries(base);

        let events: Vec<_> = session
            .next(TurnInput::User {
                content: "hi".into(),
            })
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();

        assert!(events.contains(&ModelEvent::TextDelta {
            text: "hello".into()
        }));
        assert_eq!(server.await.unwrap(), 3);
    }

    #[tokio::test]
    async fn the_server_saying_the_model_does_not_exist_is_not_retried() {
        let body =
            r#"{"object":"error","message":"The model `qwen-typo` does not exist.","code":404}"#;
        let (base, server) = serve_in_turn(vec![format!(
            "HTTP/1.1 404 Not Found\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )])
        .await;
        let mut session = quick_retries(base);

        let error = session
            .next(TurnInput::User {
                content: "hi".into(),
            })
            .await
            .err()
            .unwrap();

        assert!(error.to_string().contains("does not exist"), "{error}");
        assert_eq!(server.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn the_window_shrinks_to_what_the_server_serves() {
        let (base, server) = serve_json_once(
            r#"{"object":"list","data":[{"id":"other","max_model_len":8192},{"id":"qwen3.8-27b","max_model_len":65536}]}"#,
        )
        .await;
        let profile = crate::builtin_profile("qwen3.8-27b").unwrap();
        let mut session = QwenSession::new(QwenConfig::new(base, profile)).unwrap();

        session.discover_limits().await.unwrap();

        assert_eq!(session.profile().context_window_tokens, Some(65_536));
        assert_eq!(server.await.unwrap(), "GET /v1/models HTTP/1.1");
    }

    #[tokio::test]
    async fn a_server_that_reports_no_limit_leaves_the_profile_alone() {
        let (base, _server) =
            serve_json_once(r#"{"object":"list","data":[{"id":"qwen3.8-27b"}]}"#).await;
        let profile = crate::builtin_profile("qwen3.8-27b").unwrap();
        let mut session = QwenSession::new(QwenConfig::new(base, profile)).unwrap();

        session.discover_limits().await.unwrap();

        assert_eq!(session.profile().context_window_tokens, Some(262_144));
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }
}
