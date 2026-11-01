//! Anthropic Messages sessions with provider-native content block history.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use async_stream::try_stream;
use dino_protocol::ModelEvent;
use dino_protocol::ModelProfile;
use dino_protocol::ProviderKind;
use dino_protocol::ReasoningEffort;
use dino_protocol::StopReason;
use dino_protocol::TokenUsage;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::TurnInput;
use futures_util::StreamExt;
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
use crate::RetryPolicy;
use crate::SessionState;
use crate::elide_arguments;
use crate::elided_marker;
use crate::sse::SseDecoder;

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Bump when the persisted history shape changes. RFC-0014 section 3.
pub(crate) const STATE_VERSION: u32 = 1;
const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingMode {
    AdaptiveSummarized,
    AdaptiveOmitted,
    Disabled,
}

#[derive(Clone)]
pub struct AnthropicConfig {
    pub api_base: String,
    pub api_key: Option<String>,
    pub profile: ModelProfile,
    pub system_prompt: String,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: u32,
    pub effort: ReasoningEffort,
    pub thinking: ThinkingMode,
    /// Fast mode: the same model at up to 2.5 times the output speed, at twice
    /// the price. Opus 5 and 5.5 only.
    pub fast: bool,
    pub retry: RetryPolicy,
}

impl AnthropicConfig {
    #[must_use]
    pub fn new(api_key: impl Into<String>, profile: ModelProfile) -> Self {
        Self {
            api_base: "https://api.anthropic.com/v1".into(),
            api_key: Some(api_key.into()),
            profile,
            system_prompt: String::new(),
            tools: Vec::new(),
            max_output_tokens: 32_768,
            effort: ReasoningEffort::High,
            thinking: ThinkingMode::AdaptiveSummarized,
            fast: false,
            retry: RetryPolicy::default(),
        }
    }
}

pub struct AnthropicSession {
    client: Client,
    endpoint: Url,
    config: AnthropicConfig,
    history: Arc<Mutex<Vec<AnthropicMessage>>>,
}

impl AnthropicSession {
    /// Creates an Anthropic Messages session.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Configuration`] when the selected profile is not
    /// an Anthropic profile or the API base is not a valid URL.
    pub fn new(config: AnthropicConfig) -> Result<Self, ModelError> {
        if config.profile.provider != ProviderKind::Anthropic {
            return Err(ModelError::Configuration(format!(
                "profile {} is not an Anthropic profile",
                config.profile.key
            )));
        }
        let endpoint = format!("{}/messages", config.api_base.trim_end_matches('/'));
        let endpoint = Url::parse(&endpoint).map_err(|error| {
            ModelError::Configuration(format!("invalid Anthropic API base: {error}"))
        })?;

        Ok(Self {
            client: crate::http_client()?,
            endpoint,
            config,
            history: Arc::new(Mutex::new(Vec::new())),
        })
    }

    async fn send(&self, body: &Value) -> Result<reqwest::Response, ModelError> {
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(body);
        if body.get("speed").is_some() {
            request = request.header("anthropic-beta", FAST_MODE_BETA);
        }
        if let Some(api_key) = &self.config.api_key {
            request = request.header("x-api-key", api_key);
        }
        request
            .send()
            .await
            .map_err(|error| crate::send_error(&error))
    }

    /// One attempt at a request. Once fast mode is refused, `fast` is cleared
    /// and the turn's later attempts go at standard speed.
    async fn attempt(
        &self,
        body: &Value,
        fast: &AtomicBool,
    ) -> Result<reqwest::Response, ModelError> {
        let slow = || {
            let mut slow = body.clone();
            if let Some(fields) = slow.as_object_mut() {
                fields.remove("speed");
            }
            slow
        };
        let fast_asked = body.get("speed").is_some();
        let mut response = if fast_asked && !fast.load(Ordering::Relaxed) {
            self.send(&slow()).await?
        } else {
            self.send(body).await?
        };
        // Fast mode has a rate limit of its own; standard speed may still
        // have room, and switching only costs the prompt cache.
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
            && fast_asked
            && fast.swap(false, Ordering::Relaxed)
        {
            response = self.send(&slow()).await?;
        }
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(crate::http_error("Anthropic", response).await)
        }
    }

    fn pending_message(input: TurnInput) -> AnthropicMessage {
        match input {
            TurnInput::User { content } => AnthropicMessage {
                role: Role::User,
                content: vec![json!({"type": "text", "text": content})],
            },
            TurnInput::UserWithImages { content, images } => AnthropicMessage {
                role: Role::User,
                content: images
                    .into_iter()
                    .map(|image| {
                        json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": image.media_type,
                                "data": image.data,
                            },
                        })
                    })
                    .chain(std::iter::once(json!({"type": "text", "text": content})))
                    .collect(),
            },
            TurnInput::ToolResults { results, note } => AnthropicMessage {
                role: Role::User,
                content: results
                    .into_iter()
                    .map(|result| {
                        json!({
                            "type": "tool_result",
                            "tool_use_id": result.call_id,
                            "content": result.output.content,
                            "is_error": result.output.is_error,
                        })
                    })
                    // The results must come first in the message, so the note follows.
                    .chain(note.map(|note| json!({"type": "text", "text": note})))
                    .collect(),
            },
        }
    }

    fn request_body(&self, pending: &AnthropicMessage) -> Result<Value, ModelError> {
        validate_request_settings(&self.config)?;
        let history = self
            .history
            .lock()
            .map_err(|_| ModelError::Protocol("Anthropic history lock was poisoned".into()))?;
        let mut messages = Vec::with_capacity(history.len() + 1);
        messages.extend(history.iter().cloned());
        messages.push(pending.clone());
        drop(history);

        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(self.config.profile.provider_model));
        body.insert("max_tokens".into(), json!(self.config.max_output_tokens));
        body.insert("messages".into(), json!(messages));
        body.insert("stream".into(), json!(true));
        // Automatic caching: the breakpoint follows the last block, so each turn
        // reads the whole earlier conversation from cache.
        body.insert("cache_control".into(), json!({"type": "ephemeral"}));
        body.insert(
            "output_config".into(),
            json!({"effort": effort_name(self.config.effort)?}),
        );
        body.insert("thinking".into(), thinking_config(self.config.thinking));
        if self.config.fast {
            body.insert("speed".into(), json!("fast"));
        }
        if !self.config.system_prompt.is_empty() {
            body.insert("system".into(), json!(self.config.system_prompt));
        }
        if !self.config.tools.is_empty() {
            body.insert(
                "tools".into(),
                Value::Array(
                    self.config
                        .tools
                        .iter()
                        .map(|tool| {
                            json!({
                                "name": tool.name,
                                "description": tool.description,
                                "input_schema": tool.input_schema,
                            })
                        })
                        .collect(),
                ),
            );
            body.insert("tool_choice".into(), json!({"type": "auto"}));
        }
        Ok(Value::Object(body))
    }
}

impl ModelSession for AnthropicSession {
    fn profile(&self) -> &ModelProfile {
        &self.config.profile
    }

    fn export_state(&self) -> Result<SessionState, ModelError> {
        let history = self
            .history
            .lock()
            .map_err(|_| ModelError::Protocol("Anthropic history lock was poisoned".into()))?;
        Ok(SessionState {
            provider: ProviderKind::Anthropic,
            model_key: self.config.profile.key.clone(),
            version: STATE_VERSION,
            payload: serde_json::to_value(&*history).map_err(|error| {
                ModelError::Protocol(format!("cannot serialise Anthropic history: {error}"))
            })?,
        })
    }

    fn restore_state(&mut self, state: &SessionState) -> Result<(), ModelError> {
        state.check(&self.config.profile, STATE_VERSION)?;
        let restored: Vec<AnthropicMessage> = serde_json::from_value(state.payload.clone())
            .map_err(|error| {
                ModelError::Configuration(format!("cannot read Anthropic history: {error}"))
            })?;
        let mut history = self
            .history
            .lock()
            .map_err(|_| ModelError::Protocol("Anthropic history lock was poisoned".into()))?;
        *history = restored;
        Ok(())
    }

    fn elide_tool_results(&mut self, keep_recent: usize) -> Result<Elision, ModelError> {
        let mut history = self
            .history
            .lock()
            .map_err(|_| ModelError::Protocol("Anthropic history lock was poisoned".into()))?;

        // Newest first, so the count of results kept is a count of the most
        // recent ones rather than of whichever came first.
        let mut seen = 0_usize;
        let mut elision = Elision::default();
        for message in history.iter_mut().rev() {
            for block in &mut message.content {
                if block.get("type").and_then(Value::as_str) == Some("tool_use")
                    && seen > keep_recent
                {
                    if let Some(input) = block.get_mut("input") {
                        let reclaimed = elide_arguments(input);
                        if reclaimed > 0 {
                            elision.bytes_reclaimed += reclaimed;
                            elision.calls_elided += 1;
                        }
                    }
                    continue;
                }
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                seen += 1;
                if seen <= keep_recent {
                    continue;
                }
                let Some(content) = block.get("content").and_then(Value::as_str) else {
                    continue;
                };
                let bytes = content.len();
                if bytes <= ELIDED_MARKER_FLOOR {
                    continue;
                }
                let replacement = elided_marker(bytes);
                elision.bytes_reclaimed += bytes.saturating_sub(replacement.len());
                elision.results_elided += 1;
                block["content"] = json!(replacement);
            }
        }
        Ok(elision)
    }

    fn next(&mut self, input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
        Box::pin(async move {
            let pending = Self::pending_message(input);
            let body = self.request_body(&pending)?;
            let fast = AtomicBool::new(true);
            let response =
                crate::send_with_retry(self.config.retry, || self.attempt(&body, &fast)).await?;

            let history = Arc::clone(&self.history);
            let stream = try_stream! {
                let mut chunks = response.bytes_stream();
                let mut decoder = SseDecoder::default();
                let mut parser = MessagesStreamParser::default();
                let mut done = false;

                while let Some(chunk) = chunks.next().await {
                    let chunk = chunk.map_err(|error| ModelError::Transport(error.to_string()))?;
                    for data in decoder.push(&chunk)? {
                        let events = parser.consume(&data)?;
                        let terminal = events
                            .iter()
                            .any(|event| matches!(event, ModelEvent::Finished { .. }));
                        if terminal {
                            commit_history(
                                &history,
                                &pending,
                                parser.assistant_message()?,
                            )?;
                        }
                        for event in events {
                            yield event;
                        }
                        if terminal {
                            done = true;
                            break;
                        }
                    }
                    if done {
                        break;
                    }
                }

                if !done {
                    Err(ModelError::Protocol(
                        "Anthropic stream ended before message_stop".into(),
                    ))?;
                }
            };

            Ok(Box::pin(stream) as ModelEventStream)
        })
    }
}

fn validate_request_settings(config: &AnthropicConfig) -> Result<(), ModelError> {
    if config.max_output_tokens == 0 {
        return Err(ModelError::Configuration(
            "Anthropic max output tokens must be non-zero".into(),
        ));
    }
    if config.effort == ReasoningEffort::None {
        return Err(ModelError::Configuration(
            "Anthropic does not define a none effort level".into(),
        ));
    }
    if config.thinking == ThinkingMode::Disabled
        && matches!(config.effort, ReasoningEffort::XHigh | ReasoningEffort::Max)
    {
        return Err(ModelError::Configuration(
            "Claude cannot disable thinking at xhigh or max effort".into(),
        ));
    }
    if config.thinking == ThinkingMode::Disabled
        && matches!(
            config.profile.provider_model.as_str(),
            "claude-opus-5-5" | "claude-sonnet-5-5"
        )
    {
        return Err(ModelError::Configuration(format!(
            "{} cannot disable thinking at any effort",
            config.profile.display_name
        )));
    }
    Ok(())
}

fn effort_name(effort: ReasoningEffort) -> Result<&'static str, ModelError> {
    match effort {
        ReasoningEffort::Low => Ok("low"),
        ReasoningEffort::Medium => Ok("medium"),
        ReasoningEffort::High => Ok("high"),
        ReasoningEffort::XHigh => Ok("xhigh"),
        ReasoningEffort::Max => Ok("max"),
        ReasoningEffort::None => Err(ModelError::Configuration(
            "Anthropic does not define a none effort level".into(),
        )),
    }
}

fn thinking_config(mode: ThinkingMode) -> Value {
    match mode {
        ThinkingMode::AdaptiveSummarized => {
            json!({"type": "adaptive", "display": "summarized"})
        }
        ThinkingMode::AdaptiveOmitted => json!({"type": "adaptive", "display": "omitted"}),
        ThinkingMode::Disabled => json!({"type": "disabled"}),
    }
}

fn commit_history(
    history: &Mutex<Vec<AnthropicMessage>>,
    pending: &AnthropicMessage,
    assistant: AnthropicMessage,
) -> Result<(), ModelError> {
    let mut history = history
        .lock()
        .map_err(|_| ModelError::Protocol("Anthropic history lock was poisoned".into()))?;
    history.push(pending.clone());
    history.push(assistant);
    Ok(())
}

/// A closed set rather than a string, so a history read back from disk is
/// validated on the way in instead of being accepted and sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct AnthropicMessage {
    role: Role,
    content: Vec<Value>,
}

#[derive(Debug, Default)]
struct MessagesStreamParser {
    message_id: Option<String>,
    blocks: BTreeMap<u32, ContentBlock>,
    usage: AnthropicUsage,
    stop_reason: Option<StopReason>,
    stopped: bool,
}

impl MessagesStreamParser {
    fn consume(&mut self, data: &str) -> Result<Vec<ModelEvent>, ModelError> {
        let event: MessagesEvent = serde_json::from_str(data).map_err(|error| {
            ModelError::Protocol(format!("invalid Anthropic stream JSON: {error}"))
        })?;
        match event.kind.as_str() {
            "message_start" => self.start_message(event.message),
            "content_block_start" => self.start_block(event.index, event.content_block),
            "content_block_delta" => {
                let delta = event.content_delta()?;
                self.apply_delta(event.index, delta)
            }
            "content_block_stop" => self.stop_block(event.index),
            "message_delta" => {
                let delta = event.message_delta()?;
                self.apply_message_delta(delta, event.usage)
            }
            "message_stop" => self.stop_message(),
            "error" => Err(provider_failure(event.error)),
            _ => Ok(Vec::new()),
        }
    }

    fn start_message(
        &mut self,
        message: Option<MessageStart>,
    ) -> Result<Vec<ModelEvent>, ModelError> {
        if self.message_id.is_some() {
            return Err(ModelError::Protocol(
                "Anthropic sent more than one message_start".into(),
            ));
        }
        let message = required(message, "message_start message")?;
        self.message_id = Some(message.id.clone());
        if let Some(usage) = message.usage {
            self.usage.update(&usage);
        }
        Ok(vec![ModelEvent::Started {
            response_id: Some(message.id),
        }])
    }

    fn start_block(
        &mut self,
        index: Option<u32>,
        block: Option<ContentBlockStart>,
    ) -> Result<Vec<ModelEvent>, ModelError> {
        let index = required(index, "content block index")?;
        let block = required(block, "content block")?;
        let (block, events) = match block.kind.as_str() {
            "text" => {
                let text = block.text.unwrap_or_default();
                let events = (!text.is_empty())
                    .then(|| ModelEvent::TextDelta { text: text.clone() })
                    .into_iter()
                    .collect();
                (
                    ContentBlock::Text {
                        text,
                        stopped: false,
                    },
                    events,
                )
            }
            "thinking" => {
                let thinking = block.thinking.unwrap_or_default();
                let events = (!thinking.is_empty())
                    .then(|| ModelEvent::ReasoningDelta {
                        text: thinking.clone(),
                    })
                    .into_iter()
                    .collect();
                (
                    ContentBlock::Thinking {
                        thinking,
                        signature: block.signature.unwrap_or_default(),
                        stopped: false,
                    },
                    events,
                )
            }
            "redacted_thinking" => (
                ContentBlock::RedactedThinking {
                    data: required(block.data, "redacted thinking data")?,
                    stopped: false,
                },
                Vec::new(),
            ),
            "tool_use" => {
                let id = required(block.id, "tool-use ID")?;
                let name = required(block.name, "tool-use name")?;
                let event = ModelEvent::ToolCallStarted {
                    id: id.clone(),
                    name: name.clone(),
                };
                (
                    ContentBlock::ToolUse {
                        id,
                        name,
                        initial_input: block.input.unwrap_or_else(|| json!({})),
                        caller: block.caller,
                        partial_json: String::new(),
                        final_input: None,
                        stopped: false,
                    },
                    vec![event],
                )
            }
            other => {
                return Err(ModelError::Protocol(format!(
                    "unsupported Anthropic content block {other:?}"
                )));
            }
        };
        if self.blocks.insert(index, block).is_some() {
            return Err(ModelError::Protocol(format!(
                "Anthropic reused content block index {index}"
            )));
        }
        Ok(events)
    }

    fn apply_delta(
        &mut self,
        index: Option<u32>,
        delta: Option<ContentBlockDelta>,
    ) -> Result<Vec<ModelEvent>, ModelError> {
        let index = required(index, "content delta index")?;
        let delta = required(delta, "content block delta")?;
        let block = self.blocks.get_mut(&index).ok_or_else(|| {
            ModelError::Protocol(format!("Anthropic sent a delta for unknown block {index}"))
        })?;
        match (block, delta.kind.as_str()) {
            (ContentBlock::Text { text, .. }, "text_delta") => {
                let delta = required(delta.text, "text delta")?;
                text.push_str(&delta);
                Ok(vec![ModelEvent::TextDelta { text: delta }])
            }
            (ContentBlock::Thinking { thinking, .. }, "thinking_delta") => {
                let delta = required(delta.thinking, "thinking delta")?;
                thinking.push_str(&delta);
                Ok(vec![ModelEvent::ReasoningDelta { text: delta }])
            }
            (ContentBlock::Thinking { signature, .. }, "signature_delta") => {
                signature.push_str(&required(delta.signature, "thinking signature delta")?);
                Ok(Vec::new())
            }
            (
                ContentBlock::ToolUse {
                    id, partial_json, ..
                },
                "input_json_delta",
            ) => {
                let delta = required(delta.partial_json, "partial tool JSON")?;
                partial_json.push_str(&delta);
                Ok(vec![ModelEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    delta,
                }])
            }
            (_, other) => Err(ModelError::Protocol(format!(
                "Anthropic delta {other:?} did not match its content block"
            ))),
        }
    }

    fn stop_block(&mut self, index: Option<u32>) -> Result<Vec<ModelEvent>, ModelError> {
        let index = required(index, "stopped content block index")?;
        let block = self.blocks.get_mut(&index).ok_or_else(|| {
            ModelError::Protocol(format!("Anthropic stopped unknown block {index}"))
        })?;
        if block.stopped() {
            return Err(ModelError::Protocol(format!(
                "Anthropic stopped content block {index} twice"
            )));
        }
        block.mark_stopped();
        let ContentBlock::ToolUse {
            id,
            name,
            initial_input,
            partial_json,
            final_input,
            ..
        } = block
        else {
            return Ok(Vec::new());
        };
        // Unparsable arguments go to the tool set to refuse; history keeps an
        // empty object, which the API accepts back.
        let (input, kept) = if partial_json.is_empty() {
            (initial_input.clone(), initial_input.clone())
        } else {
            match serde_json::from_str(partial_json) {
                Ok(input) => (input, Value::Null),
                Err(error) => (
                    crate::unparsed(partial_json, &error),
                    Value::Object(serde_json::Map::new()),
                ),
            }
        };
        *final_input = Some(if kept.is_null() { input.clone() } else { kept });
        Ok(vec![ModelEvent::ToolCallCompleted {
            call: ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: input,
            },
        }])
    }

    fn apply_message_delta(
        &mut self,
        delta: Option<MessageDelta>,
        usage: Option<UsageDelta>,
    ) -> Result<Vec<ModelEvent>, ModelError> {
        if let Some(usage) = usage {
            self.usage.update(&usage);
        }
        if let Some(reason) = required(delta, "message delta")?.stop_reason {
            if self.stop_reason.is_some() {
                return Err(ModelError::Protocol(
                    "Anthropic sent more than one stop reason".into(),
                ));
            }
            self.stop_reason = Some(parse_stop_reason(&reason)?);
        }
        Ok(Vec::new())
    }

    fn stop_message(&mut self) -> Result<Vec<ModelEvent>, ModelError> {
        if self.stopped {
            return Err(ModelError::Protocol(
                "Anthropic sent message_stop twice".into(),
            ));
        }
        if self.message_id.is_none() {
            return Err(ModelError::Protocol(
                "Anthropic stopped a message which never started".into(),
            ));
        }
        if let Some((index, _)) = self.blocks.iter().find(|(_, block)| !block.stopped()) {
            return Err(ModelError::Protocol(format!(
                "Anthropic stopped the message before content block {index}"
            )));
        }
        ensure_contiguous_indices(&self.blocks)?;
        let reason = required(self.stop_reason, "message stop reason")?;
        self.stopped = true;
        Ok(vec![
            ModelEvent::Usage {
                usage: self.usage.normalized(),
            },
            ModelEvent::Finished { reason },
        ])
    }

    fn assistant_message(&self) -> Result<AnthropicMessage, ModelError> {
        if !self.stopped {
            return Err(ModelError::Protocol(
                "Anthropic assistant message is not complete".into(),
            ));
        }
        let content = self
            .blocks
            .values()
            .map(ContentBlock::wire_value)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(AnthropicMessage {
            role: Role::Assistant,
            content,
        })
    }
}

fn ensure_contiguous_indices(blocks: &BTreeMap<u32, ContentBlock>) -> Result<(), ModelError> {
    for (expected, actual) in (0_u32..).zip(blocks.keys().copied()) {
        if expected != actual {
            return Err(ModelError::Protocol(format!(
                "Anthropic omitted content block index {expected}"
            )));
        }
    }
    Ok(())
}

fn parse_stop_reason(reason: &str) -> Result<StopReason, ModelError> {
    match reason {
        "end_turn" | "stop_sequence" => Ok(StopReason::EndTurn),
        "tool_use" => Ok(StopReason::ToolUse),
        "max_tokens" | "model_context_window_exceeded" => Ok(StopReason::MaxTokens),
        "refusal" => Ok(StopReason::Refusal),
        "pause_turn" => Err(ModelError::Protocol(
            "Anthropic pause_turn requires server-tool continuation, which is not enabled".into(),
        )),
        other => Err(ModelError::Protocol(format!(
            "unknown Anthropic stop reason {other:?}"
        ))),
    }
}

fn provider_failure(error: Option<ProviderError>) -> ModelError {
    let message = error
        .and_then(|error| error.message)
        .unwrap_or_else(|| "unknown provider error".into());
    ModelError::Transport(format!("Anthropic response failed: {message}"))
}

fn required<T>(value: Option<T>, field: &str) -> Result<T, ModelError> {
    value.ok_or_else(|| ModelError::Protocol(format!("Anthropic omitted {field}")))
}

#[derive(Debug)]
enum ContentBlock {
    Text {
        text: String,
        stopped: bool,
    },
    Thinking {
        thinking: String,
        signature: String,
        stopped: bool,
    },
    RedactedThinking {
        data: String,
        stopped: bool,
    },
    ToolUse {
        id: String,
        name: String,
        initial_input: Value,
        caller: Option<Value>,
        partial_json: String,
        final_input: Option<Value>,
        stopped: bool,
    },
}

impl ContentBlock {
    const fn stopped(&self) -> bool {
        match self {
            Self::Text { stopped, .. }
            | Self::Thinking { stopped, .. }
            | Self::RedactedThinking { stopped, .. }
            | Self::ToolUse { stopped, .. } => *stopped,
        }
    }

    const fn mark_stopped(&mut self) {
        match self {
            Self::Text { stopped, .. }
            | Self::Thinking { stopped, .. }
            | Self::RedactedThinking { stopped, .. }
            | Self::ToolUse { stopped, .. } => *stopped = true,
        }
    }

    fn wire_value(&self) -> Result<Value, ModelError> {
        match self {
            Self::Text { text, .. } => Ok(json!({"type": "text", "text": text})),
            Self::Thinking {
                thinking,
                signature,
                ..
            } => {
                if signature.is_empty() {
                    return Err(ModelError::Protocol(
                        "Anthropic thinking block omitted its signature".into(),
                    ));
                }
                Ok(json!({
                    "type": "thinking",
                    "thinking": thinking,
                    "signature": signature,
                }))
            }
            Self::RedactedThinking { data, .. } => {
                Ok(json!({"type": "redacted_thinking", "data": data}))
            }
            Self::ToolUse {
                id,
                name,
                caller,
                final_input,
                ..
            } => {
                let mut value = json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": required(final_input.clone(), "completed tool input")?,
                });
                if let Some(caller) = caller {
                    value["caller"] = caller.clone();
                }
                Ok(value)
            }
        }
    }
}

#[derive(Debug, Default)]
struct AnthropicUsage {
    input: u64,
    cache_creation_input: u64,
    cache_read_input: u64,
    output: u64,
}

impl AnthropicUsage {
    fn update(&mut self, usage: &UsageDelta) {
        if let Some(value) = usage.input {
            self.input = value;
        }
        if let Some(value) = usage.cache_creation_input {
            self.cache_creation_input = value;
        }
        if let Some(value) = usage.cache_read_input {
            self.cache_read_input = value;
        }
        if let Some(value) = usage.output {
            self.output = value;
        }
    }

    const fn normalized(&self) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input + self.cache_creation_input + self.cache_read_input,
            cached_input_tokens: self.cache_read_input,
            cache_write_input_tokens: self.cache_creation_input,
            output_tokens: self.output,
            reasoning_tokens: 0,
        }
    }
}

#[derive(Debug, Deserialize)]
struct MessagesEvent {
    #[serde(rename = "type")]
    kind: String,
    index: Option<u32>,
    message: Option<MessageStart>,
    content_block: Option<ContentBlockStart>,
    delta: Option<Value>,
    usage: Option<UsageDelta>,
    error: Option<ProviderError>,
}

impl MessagesEvent {
    fn message_delta(&self) -> Result<Option<MessageDelta>, ModelError> {
        self.delta
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| {
                ModelError::Protocol(format!("invalid Anthropic message delta: {error}"))
            })
    }

    fn content_delta(&self) -> Result<Option<ContentBlockDelta>, ModelError> {
        self.delta
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| {
                ModelError::Protocol(format!("invalid Anthropic content delta: {error}"))
            })
    }
}

#[derive(Debug, Deserialize)]
struct MessageStart {
    id: String,
    usage: Option<UsageDelta>,
}

#[derive(Debug, Deserialize)]
struct ContentBlockStart {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    thinking: Option<String>,
    signature: Option<String>,
    data: Option<String>,
    id: Option<String>,
    name: Option<String>,
    input: Option<Value>,
    caller: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ContentBlockDelta {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    thinking: Option<String>,
    signature: Option<String>,
    partial_json: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageDelta {
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageDelta {
    #[serde(rename = "input_tokens")]
    input: Option<u64>,
    #[serde(rename = "cache_creation_input_tokens")]
    cache_creation_input: Option<u64>,
    #[serde(rename = "cache_read_input_tokens")]
    cache_read_input: Option<u64>,
    #[serde(rename = "output_tokens")]
    output: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ProviderError {
    message: Option<String>,
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
        let profile = crate::builtin_profile("claude-opus").unwrap();
        let original = AnthropicSession::new(AnthropicConfig::new("key", profile.clone())).unwrap();
        original.history.lock().unwrap().extend([
            AnthropicMessage {
                role: Role::User,
                content: vec![json!({"type": "text", "text": "read the file"})],
            },
            AnthropicMessage {
                role: Role::Assistant,
                content: vec![json!({
                    "type": "tool_use",
                    "id": "call-1",
                    "name": "read",
                    "input": {"path": "src/lib.rs"},
                })],
            },
            AnthropicSession::pending_message(TurnInput::ToolResults {
                results: vec![dino_protocol::ToolResult {
                    call_id: "call-1".into(),
                    output: dino_protocol::ToolOutput::success("fn main() {}"),
                }],
                note: Some("[dino: verified]".into()),
            }),
        ]);

        let state = original.export_state().unwrap();
        let mut restored = AnthropicSession::new(AnthropicConfig::new("key", profile)).unwrap();
        restored.restore_state(&state).unwrap();
        assert_eq!(
            restored.history.lock().unwrap()[2].content[1],
            json!({"type": "text", "text": "[dino: verified]"})
        );

        // Serialising and deserialising is not the claim. The claim is that the
        // next request is the one the original would have sent.
        let pending = AnthropicSession::pending_message(TurnInput::User {
            content: "and now?".into(),
        });
        assert_eq!(
            restored.request_body(&pending).unwrap(),
            original.request_body(&pending).unwrap()
        );
    }

    #[test]
    fn state_from_another_provider_model_or_version_is_refused() {
        let profile = crate::builtin_profile("claude-opus").unwrap();
        let mut session = AnthropicSession::new(AnthropicConfig::new("key", profile)).unwrap();
        let good = session.export_state().unwrap();

        let wrong_provider = SessionState {
            provider: ProviderKind::Qwen,
            ..good.clone()
        };
        let wrong_model = SessionState {
            model_key: "claude-sonnet".into(),
            ..good.clone()
        };
        let wrong_version = SessionState {
            version: good.version + 1,
            ..good
        };

        for (state, expected) in [
            (wrong_provider, "not Anthropic"),
            (wrong_model, "not claude-opus"),
            (wrong_version, "this build reads version"),
        ] {
            let error = session.restore_state(&state).unwrap_err();
            assert!(error.to_string().contains(expected), "said: {error}");
        }
    }

    #[test]
    fn elision_hollows_the_oldest_results_and_keeps_the_recent_ones() {
        let profile = crate::builtin_profile("claude-opus").unwrap();
        let mut session = AnthropicSession::new(AnthropicConfig::new("key", profile)).unwrap();
        {
            let mut history = session.history.lock().unwrap();
            for index in 0..4 {
                history.push(AnthropicMessage {
                    role: Role::User,
                    content: vec![json!({
                        "type": "tool_result",
                        "tool_use_id": format!("call-{index}"),
                        "content": "x".repeat(1_000),
                        "is_error": false,
                    })],
                });
            }
        }

        let elision = session.elide_tool_results(2).unwrap();

        let history = session.history.lock().unwrap();
        assert_eq!(elision.results_elided, 2);
        assert!(elision.bytes_reclaimed > 1_500);
        // The two oldest are hollowed and the two newest are untouched.
        for (index, message) in history.iter().enumerate() {
            let content = message.content[0]["content"].as_str().unwrap();
            if index < 2 {
                assert!(content.starts_with("[dino elided"), "{index}: {content}");
            } else {
                assert_eq!(content.len(), 1_000, "{index}");
            }
            // Whatever else changed, the provider still needs a result for
            // every call it issued, matched by ID.
            assert_eq!(
                message.content[0]["tool_use_id"].as_str().unwrap(),
                format!("call-{index}")
            );
        }
    }

    #[test]
    fn elision_hollows_long_inputs_of_old_tool_uses() {
        let profile = crate::builtin_profile("claude-opus").unwrap();
        let mut session = AnthropicSession::new(AnthropicConfig::new("key", profile)).unwrap();
        let patch = json!({"path": "src/lib.rs", "old_text": "a a\n".repeat(200), "new_text": "b b\n".repeat(225)});
        session.history.lock().unwrap().extend([
            AnthropicMessage {
                role: Role::Assistant,
                content: vec![json!({"type": "tool_use", "id": "call-1", "name": "apply_patch", "input": patch})],
            },
            AnthropicMessage {
                role: Role::User,
                content: vec![json!({"type": "tool_result", "tool_use_id": "call-1", "content": "ok", "is_error": false})],
            },
        ]);

        let elision = session.elide_tool_results(0).unwrap();

        let history = session.history.lock().unwrap();
        let input = &history[0].content[0]["input"];
        assert_eq!(elision.calls_elided, 1);
        assert_eq!(input["path"], "src/lib.rs");
        assert!(
            input["old_text"]
                .as_str()
                .unwrap()
                .starts_with("[dino elided 800 bytes")
        );
        assert!(
            input["new_text"]
                .as_str()
                .unwrap()
                .starts_with("[dino elided 900 bytes")
        );
        assert_eq!(history[0].content[0]["id"], "call-1");
    }

    #[test]
    fn elision_leaves_a_short_result_alone() {
        let profile = crate::builtin_profile("claude-opus").unwrap();
        let mut session = AnthropicSession::new(AnthropicConfig::new("key", profile)).unwrap();
        session.history.lock().unwrap().push(AnthropicMessage {
            role: Role::User,
            content: vec![json!({
                "type": "tool_result",
                "tool_use_id": "call-1",
                "content": "ok",
                "is_error": false,
            })],
        });

        let elision = session.elide_tool_results(0).unwrap();

        // Replacing two bytes with a sentence reclaims nothing and loses the
        // only thing it had.
        assert_eq!(elision.results_elided, 0);
        assert_eq!(
            session.history.lock().unwrap()[0].content[0]["content"]
                .as_str()
                .unwrap(),
            "ok"
        );
    }

    #[test]
    fn request_places_tool_results_in_an_immediate_user_message() {
        let profile = crate::builtin_profile("claude-opus").unwrap();
        let mut config = AnthropicConfig::new("test-key", profile);
        config.system_prompt = "Work carefully.".into();
        let session = AnthropicSession::new(config).unwrap();
        let pending = AnthropicSession::pending_message(TurnInput::ToolResults {
            results: vec![dino_protocol::ToolResult {
                call_id: "toolu-1".into(),
                output: dino_protocol::ToolOutput::error("not found"),
            }],
            note: Some("[dino: verified]".into()),
        });
        let body = session.request_body(&pending).unwrap();

        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(
            body["messages"][0]["content"],
            json!([
                {"type": "tool_result", "tool_use_id": "toolu-1", "content": "not found", "is_error": true},
                {"type": "text", "text": "[dino: verified]"},
            ])
        );
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn parser_preserves_thinking_signature_and_tool_block() {
        let mut parser = MessagesStreamParser::default();
        let chunks = [
            r#"{"type":"message_start","message":{"id":"msg-1","usage":{"input_tokens":20,"cache_creation_input_tokens":3,"cache_read_input_tokens":5,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"inspect"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque-signature"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque-redacted-data"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu-1","name":"read_file","input":{},"caller":{"type":"direct"}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"src/lib.rs\"}"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":12}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(parser.consume(chunk).unwrap());
        }
        let assistant = parser.assistant_message().unwrap();

        assert_eq!(assistant.content[0]["signature"], "opaque-signature");
        assert_eq!(assistant.content[1]["data"], "opaque-redacted-data");
        assert_eq!(assistant.content[2]["input"], json!({"path": "src/lib.rs"}));
        assert_eq!(assistant.content[2]["caller"]["type"], "direct");
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallCompleted { call }
                if call.id == "toolu-1" && call.arguments == json!({"path": "src/lib.rs"})
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::Usage { usage }
                if usage.input_tokens == 28
                    && usage.cached_input_tokens == 5
                    && usage.cache_write_input_tokens == 3
        )));
        assert_eq!(
            events.last(),
            Some(&ModelEvent::Finished {
                reason: StopReason::ToolUse
            })
        );
    }

    #[test]
    fn invalid_partial_tool_json_is_marked_for_the_tool_set_to_refuse() {
        let mut parser = MessagesStreamParser::default();
        parser
            .consume(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu-1","name":"read_file","input":{}}}"#,
            )
            .unwrap();
        parser
            .consume(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{"}}"#,
            )
            .unwrap();

        let events = parser
            .consume(r#"{"type":"content_block_stop","index":0}"#)
            .unwrap();

        let Some(ModelEvent::ToolCallCompleted { call }) = events.first() else {
            panic!("no call completed: {events:?}");
        };
        assert_eq!(
            call.arguments[dino_protocol::UNPARSED_ARGUMENTS]["raw"],
            "{"
        );
    }

    #[test]
    fn disabled_thinking_is_rejected_at_max_effort() {
        let profile = crate::builtin_profile("claude-opus").unwrap();
        let mut config = AnthropicConfig::new("test-key", profile);
        config.effort = ReasoningEffort::Max;
        config.thinking = ThinkingMode::Disabled;
        let session = AnthropicSession::new(config).unwrap();
        let pending = AnthropicSession::pending_message(TurnInput::User {
            content: "hello".into(),
        });

        assert!(matches!(
            session.request_body(&pending),
            Err(ModelError::Configuration(_))
        ));
    }

    #[test]
    fn sonnet_5_5_refuses_disabled_thinking_at_any_effort() {
        let profile = crate::builtin_profile("claude-sonnet-5.5").unwrap();
        let mut config = AnthropicConfig::new("test-key", profile);
        config.effort = ReasoningEffort::Low;
        config.thinking = ThinkingMode::Disabled;
        let session = AnthropicSession::new(config).unwrap();
        let pending = AnthropicSession::pending_message(TurnInput::User {
            content: "hello".into(),
        });

        assert!(matches!(
            session.request_body(&pending),
            Err(ModelError::Configuration(_))
        ));
    }

    #[test]
    fn pause_turn_is_not_misreported_as_success() {
        assert!(parse_stop_reason("pause_turn").is_err());
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
            for event in [
                r#"{"type":"message_start","message":{"id":"msg-live","usage":{"input_tokens":4,"output_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
                r#"{"type":"message_stop"}"#,
            ] {
                socket
                    .write_all(format!("data: {event}\n\n").as_bytes())
                    .await
                    .unwrap();
            }

            request.truncate(expected_length);
            String::from_utf8(request).unwrap()
        });

        let profile = crate::builtin_profile("claude-opus").unwrap();
        let mut config = AnthropicConfig::new("test-key", profile);
        config.api_base = format!("http://{address}/v1");
        let mut session = AnthropicSession::new(config).unwrap();
        let stream = session
            .next(TurnInput::User {
                content: "say hello".into(),
            })
            .await
            .unwrap();
        let events = stream.try_collect::<Vec<_>>().await.unwrap();
        let request = server.await.unwrap();
        let request_lowercase = request.to_ascii_lowercase();

        assert!(request.starts_with("POST /v1/messages HTTP/1.1"));
        assert!(request_lowercase.contains("anthropic-version: 2023-06-01"));
        assert!(request_lowercase.contains("x-api-key: test-key"));
        assert!(request.contains("\"text\":\"say hello\""));
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

    fn quick_retries(base: String) -> AnthropicSession {
        let profile = crate::builtin_profile("claude-opus").unwrap();
        let mut config = AnthropicConfig::new("test-key", profile);
        config.api_base = base;
        config.retry = RetryPolicy {
            attempts: 3,
            first_delay: std::time::Duration::from_millis(1),
        };
        AnthropicSession::new(config).unwrap()
    }

    #[tokio::test]
    async fn a_rejected_request_is_typed_and_not_retried() {
        let (base, server) = crate::test_server::serve_in_turn(vec![crate::test_server::status(
            "400 Bad Request",
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"tool_use ids must be unique"}}"#,
        )])
        .await;
        let error = quick_retries(base)
            .next(TurnInput::User {
                content: "hi".into(),
            })
            .await
            .err()
            .unwrap();
        assert!(
            matches!(&error, ModelError::Http { provider: "Anthropic", status: 400, body } if body.contains("must be unique")),
            "{error}"
        );
        assert_eq!(server.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn an_unavailable_server_is_retried() {
        let (base, server) = crate::test_server::serve_in_turn(vec![
            crate::test_server::status("503 Service Unavailable", ""),
            crate::test_server::sse(&[
                r#"{"type":"message_start","message":{"id":"msg-1","usage":{"input_tokens":4,"output_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
                r#"{"type":"message_stop"}"#,
            ]),
        ])
        .await;
        let events: Vec<_> = quick_retries(base)
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
        assert_eq!(server.await.unwrap(), 2);
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }
}
