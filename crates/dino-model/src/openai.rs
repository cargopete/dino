//! `OpenAI` Responses sessions for GPT-5.6 Terra.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

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
use serde_json::Value;
use serde_json::json;

use crate::ModelError;
use crate::ModelEventStream;
use crate::ModelFuture;
use crate::ModelSession;
use crate::SessionState;
use crate::sse::SseDecoder;

const ERROR_BODY_LIMIT: usize = 4_096;

/// Bump when the persisted state shape changes. RFC-0014 section 3.
const STATE_VERSION: u32 = 1;

#[derive(Clone)]
pub struct OpenAiConfig {
    pub api_base: String,
    pub api_key: Option<String>,
    pub organization: Option<String>,
    pub project: Option<String>,
    pub profile: ModelProfile,
    pub instructions: String,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_effort: ReasoningEffort,
}

impl OpenAiConfig {
    #[must_use]
    pub fn new(api_key: impl Into<String>, profile: ModelProfile) -> Self {
        Self {
            api_base: "https://api.openai.com/v1".into(),
            api_key: Some(api_key.into()),
            organization: None,
            project: None,
            profile,
            instructions: String::new(),
            tools: Vec::new(),
            max_output_tokens: None,
            reasoning_effort: ReasoningEffort::High,
        }
    }
}

pub struct OpenAiSession {
    client: Client,
    endpoint: Url,
    config: OpenAiConfig,
    previous_response_id: Arc<Mutex<Option<String>>>,
}

impl OpenAiSession {
    /// Creates an `OpenAI` Responses session.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Configuration`] when the selected profile is not
    /// an `OpenAI` profile or the API base is not a valid URL.
    pub fn new(config: OpenAiConfig) -> Result<Self, ModelError> {
        if config.profile.provider != ProviderKind::OpenAi {
            return Err(ModelError::Configuration(format!(
                "profile {} is not an OpenAI profile",
                config.profile.key
            )));
        }
        let endpoint = format!("{}/responses", config.api_base.trim_end_matches('/'));
        let endpoint = Url::parse(&endpoint).map_err(|error| {
            ModelError::Configuration(format!("invalid OpenAI API base: {error}"))
        })?;

        Ok(Self {
            client: crate::http_client()?,
            endpoint,
            config,
            previous_response_id: Arc::new(Mutex::new(None)),
        })
    }

    fn request_body(&self, input: TurnInput) -> Result<Value, ModelError> {
        let input =
            match input {
                TurnInput::User { content } => json!([{
                    "role": "user",
                    "content": [{"type": "input_text", "text": content}],
                }]),
                TurnInput::UserWithImages { content, images } => {
                    let mut parts = vec![json!({"type": "input_text", "text": content})];
                    parts.extend(images.iter().map(
                        |image| json!({"type": "input_image", "image_url": image.data_url()}),
                    ));
                    json!([{"role": "user", "content": parts}])
                }
                TurnInput::ToolResults { results } => Value::Array(
                    results
                        .into_iter()
                        .map(|result| {
                            let output = if result.output.is_error {
                                format!("Tool error: {}", result.output.content)
                            } else {
                                result.output.content
                            };
                            json!({
                                "type": "function_call_output",
                                "call_id": result.call_id,
                                "output": output,
                            })
                        })
                        .collect(),
                ),
            };
        let previous_response_id = self
            .previous_response_id
            .lock()
            .map_err(|_| ModelError::Protocol("OpenAI response ID lock was poisoned".into()))?
            .clone();
        let tools = self
            .config
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                    // Strict mode needs every property required, and the tools have optional ones.
                    "strict": false,
                })
            })
            .collect::<Vec<_>>();

        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(self.config.profile.provider_model));
        body.insert("input".into(), input);
        body.insert("instructions".into(), json!(self.config.instructions));
        body.insert("stream".into(), json!(true));
        body.insert("store".into(), json!(true));
        body.insert("parallel_tool_calls".into(), json!(true));
        body.insert(
            "reasoning".into(),
            json!({
                "effort": reasoning_effort_name(self.config.reasoning_effort),
                "summary": "auto",
            }),
        );
        if let Some(previous_response_id) = previous_response_id {
            body.insert("previous_response_id".into(), json!(previous_response_id));
        }
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
            body.insert("tool_choice".into(), json!("auto"));
        }
        if let Some(max_output_tokens) = self.config.max_output_tokens {
            body.insert("max_output_tokens".into(), json!(max_output_tokens));
        }
        Ok(Value::Object(body))
    }
}

impl ModelSession for OpenAiSession {
    fn profile(&self) -> &ModelProfile {
        &self.config.profile
    }

    /// Exports the continuation handle, which is all the state there is.
    ///
    /// The provider may have expired it by the time it is used. That failure
    /// arrives on the next request, in the provider's own words, and is the
    /// case RFC-0001 section 9 anticipated when it called continuation IDs an
    /// optimisation.
    fn export_state(&self) -> Result<SessionState, ModelError> {
        let previous = self
            .previous_response_id
            .lock()
            .map_err(|_| ModelError::Protocol("OpenAI response lock was poisoned".into()))?;
        Ok(SessionState {
            provider: ProviderKind::OpenAi,
            model_key: self.config.profile.key.clone(),
            version: STATE_VERSION,
            payload: serde_json::json!({ "previous_response_id": *previous }),
        })
    }

    fn restore_state(&mut self, state: &SessionState) -> Result<(), ModelError> {
        state.check(&self.config.profile, STATE_VERSION)?;
        let restored = state
            .payload
            .get("previous_response_id")
            .ok_or_else(|| {
                ModelError::Configuration("OpenAI state has no previous_response_id".into())
            })?
            .as_str()
            .map(ToOwned::to_owned);
        let mut previous = self
            .previous_response_id
            .lock()
            .map_err(|_| ModelError::Protocol("OpenAI response lock was poisoned".into()))?;
        *previous = restored;
        Ok(())
    }

    fn next(&mut self, input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
        Box::pin(async move {
            let body = self.request_body(input)?;
            let mut request = self.client.post(self.endpoint.clone()).json(&body);
            if let Some(api_key) = &self.config.api_key {
                request = request.bearer_auth(api_key);
            }
            if let Some(organization) = &self.config.organization {
                request = request.header("OpenAI-Organization", organization);
            }
            if let Some(project) = &self.config.project {
                request = request.header("OpenAI-Project", project);
            }

            let response = request
                .send()
                .await
                .map_err(|error| ModelError::Transport(error.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                let body = response
                    .text()
                    .await
                    .unwrap_or_else(|error| format!("could not read response body: {error}"));
                return Err(ModelError::Transport(format!(
                    "OpenAI returned {status}: {}",
                    truncate_for_error(&body)
                )));
            }

            let response_state = Arc::clone(&self.previous_response_id);
            let stream = try_stream! {
                let mut chunks = response.bytes_stream();
                let mut decoder = SseDecoder::default();
                let mut parser = ResponsesStreamParser::default();
                let mut done = false;

                while let Some(chunk) = chunks.next().await {
                    let chunk = chunk.map_err(|error| ModelError::Transport(error.to_string()))?;
                    for data in decoder.push(&chunk)? {
                        if data == "[DONE]" {
                            continue;
                        }
                        let events = parser.consume(&data)?;
                        let terminal = events
                            .iter()
                            .any(|event| matches!(event, ModelEvent::Finished { .. }));
                        if terminal {
                            set_response_id(&response_state, parser.response_id()?)?;
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
                        "OpenAI stream ended without a terminal response event".into(),
                    ))?;
                }
            };

            Ok(Box::pin(stream) as ModelEventStream)
        })
    }
}

fn set_response_id(state: &Mutex<Option<String>>, response_id: String) -> Result<(), ModelError> {
    let mut state = state
        .lock()
        .map_err(|_| ModelError::Protocol("OpenAI response ID lock was poisoned".into()))?;
    *state = Some(response_id);
    Ok(())
}

pub(crate) const fn reasoning_effort_name(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::None => "none",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::XHigh => "xhigh",
        ReasoningEffort::Max => "max",
    }
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

#[derive(Debug, Default)]
struct ResponsesStreamParser {
    response_id: Option<String>,
    started: bool,
    refusal: bool,
    tool_calls: BTreeMap<String, FunctionCallAccumulator>,
}

impl ResponsesStreamParser {
    fn response_id(&self) -> Result<String, ModelError> {
        self.response_id
            .clone()
            .ok_or_else(|| ModelError::Protocol("OpenAI completed a response without an ID".into()))
    }

    fn consume(&mut self, data: &str) -> Result<Vec<ModelEvent>, ModelError> {
        let event: ResponsesEvent = serde_json::from_str(data).map_err(|error| {
            ModelError::Protocol(format!("invalid OpenAI stream JSON: {error}"))
        })?;
        match event.kind.as_str() {
            "response.created" | "response.in_progress" => self.start(event.response),
            "response.output_text.delta" => Ok(vec![ModelEvent::TextDelta {
                text: required(event.delta, "output text delta")?,
            }]),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                Ok(vec![ModelEvent::ReasoningDelta {
                    text: required(event.delta, "reasoning delta")?,
                }])
            }
            "response.refusal.delta" => {
                self.refusal = true;
                Ok(vec![ModelEvent::TextDelta {
                    text: required(event.delta, "refusal delta")?,
                }])
            }
            "response.output_item.added" => self.add_output_item(event.item),
            "response.function_call_arguments.delta" => {
                self.add_argument_delta(event.item_id, event.delta)
            }
            "response.function_call_arguments.done" => {
                self.complete_arguments(event.item_id, event.arguments)
            }
            "response.completed" => self.complete_response(event.response),
            "response.incomplete" => self.terminal_response(event.response, StopReason::MaxTokens),
            "response.cancelled" => self.terminal_response(event.response, StopReason::Cancelled),
            "response.failed" | "error" => Err(provider_failure(event)),
            _ => Ok(Vec::new()),
        }
    }

    fn start(&mut self, response: Option<ResponseData>) -> Result<Vec<ModelEvent>, ModelError> {
        let response = required(response, "created response")?;
        let id = required(response.id, "created response ID")?;
        self.response_id = Some(id.clone());
        if self.started {
            return Ok(Vec::new());
        }
        self.started = true;
        Ok(vec![ModelEvent::Started {
            response_id: Some(id),
        }])
    }

    fn add_output_item(
        &mut self,
        item: Option<ResponseItem>,
    ) -> Result<Vec<ModelEvent>, ModelError> {
        let Some(item) = item else {
            return Err(ModelError::Protocol(
                "OpenAI output-item event omitted its item".into(),
            ));
        };
        if item.kind != "function_call" {
            return Ok(Vec::new());
        }
        let item_id = required(item.id, "function-call item ID")?;
        let call_id = required(item.call_id, "function-call call ID")?;
        let name = required(item.name, "function-call name")?;
        let call = FunctionCallAccumulator {
            call_id: call_id.clone(),
            name: name.clone(),
            arguments: item.arguments.unwrap_or_default(),
            completed: false,
        };
        if self.tool_calls.insert(item_id, call).is_some() {
            return Err(ModelError::Protocol(
                "OpenAI reused a function-call item ID".into(),
            ));
        }
        Ok(vec![ModelEvent::ToolCallStarted { id: call_id, name }])
    }

    fn add_argument_delta(
        &mut self,
        item_id: Option<String>,
        delta: Option<String>,
    ) -> Result<Vec<ModelEvent>, ModelError> {
        let item_id = required(item_id, "argument-delta item ID")?;
        let delta = required(delta, "function-call argument delta")?;
        let call = self.tool_calls.get_mut(&item_id).ok_or_else(|| {
            ModelError::Protocol(format!("OpenAI sent arguments for unknown item {item_id}"))
        })?;
        call.arguments.push_str(&delta);
        Ok(vec![ModelEvent::ToolCallArgumentsDelta {
            id: call.call_id.clone(),
            delta,
        }])
    }

    fn complete_arguments(
        &mut self,
        item_id: Option<String>,
        arguments: Option<String>,
    ) -> Result<Vec<ModelEvent>, ModelError> {
        let item_id = required(item_id, "arguments-done item ID")?;
        let arguments = required(arguments, "final function-call arguments")?;
        let call = self.tool_calls.get_mut(&item_id).ok_or_else(|| {
            ModelError::Protocol(format!(
                "OpenAI completed arguments for unknown item {item_id}"
            ))
        })?;
        if !call.arguments.is_empty() && call.arguments != arguments {
            return Err(ModelError::Protocol(format!(
                "OpenAI final arguments disagreed with deltas for {}",
                call.name
            )));
        }
        let parsed = serde_json::from_str(&arguments)
            .unwrap_or_else(|error| crate::unparsed(&arguments, &error));
        call.arguments = arguments;
        call.completed = true;
        Ok(vec![ModelEvent::ToolCallCompleted {
            call: ToolCall {
                id: call.call_id.clone(),
                name: call.name.clone(),
                arguments: parsed,
            },
        }])
    }

    fn complete_response(
        &mut self,
        response: Option<ResponseData>,
    ) -> Result<Vec<ModelEvent>, ModelError> {
        self.ensure_calls_complete()?;
        let reason = if self.refusal {
            StopReason::Refusal
        } else if self.tool_calls.is_empty() {
            StopReason::EndTurn
        } else {
            StopReason::ToolUse
        };
        self.terminal_response(response, reason)
    }

    fn terminal_response(
        &mut self,
        response: Option<ResponseData>,
        reason: StopReason,
    ) -> Result<Vec<ModelEvent>, ModelError> {
        let response = required(response, "terminal response")?;
        let id = required(response.id, "terminal response ID")?;
        self.response_id = Some(id.clone());
        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            events.push(ModelEvent::Started {
                response_id: Some(id),
            });
        }
        if let Some(usage) = response.usage {
            events.push(ModelEvent::Usage {
                usage: usage.into(),
            });
        }
        events.push(ModelEvent::Finished { reason });
        Ok(events)
    }

    fn ensure_calls_complete(&self) -> Result<(), ModelError> {
        if let Some(call) = self.tool_calls.values().find(|call| !call.completed) {
            return Err(ModelError::Protocol(format!(
                "OpenAI completed before arguments for {} were done",
                call.name
            )));
        }
        Ok(())
    }
}

fn provider_failure(event: ResponsesEvent) -> ModelError {
    let message = event
        .response
        .and_then(|response| response.error)
        .and_then(|error| error.message)
        .or_else(|| event.error.and_then(|error| error.message))
        .unwrap_or_else(|| "unknown provider error".into());
    ModelError::Transport(format!("OpenAI response failed: {message}"))
}

fn required<T>(value: Option<T>, field: &str) -> Result<T, ModelError> {
    value.ok_or_else(|| ModelError::Protocol(format!("OpenAI omitted {field}")))
}

#[derive(Debug, Default)]
struct FunctionCallAccumulator {
    call_id: String,
    name: String,
    arguments: String,
    completed: bool,
}

#[derive(Debug, Deserialize)]
struct ResponsesEvent {
    #[serde(rename = "type")]
    kind: String,
    delta: Option<String>,
    arguments: Option<String>,
    item_id: Option<String>,
    item: Option<ResponseItem>,
    response: Option<ResponseData>,
    error: Option<ResponseError>,
}

#[derive(Debug, Deserialize)]
struct ResponseItem {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponseData {
    id: Option<String>,
    usage: Option<ResponseUsage>,
    error: Option<ResponseError>,
}

#[derive(Debug, Deserialize)]
struct ResponseError {
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponseUsage {
    input_tokens: u64,
    output_tokens: u64,
    input_tokens_details: Option<ResponseInputTokenDetails>,
    output_tokens_details: Option<ResponseOutputTokenDetails>,
}

impl From<ResponseUsage> for TokenUsage {
    fn from(usage: ResponseUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage
                .input_tokens_details
                .map_or(0, |details| details.cached_tokens),
            cache_write_input_tokens: 0,
            output_tokens: usage.output_tokens,
            reasoning_tokens: usage
                .output_tokens_details
                .map_or(0, |details| details.reasoning_tokens),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ResponseInputTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ResponseOutputTokenDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn the_continuation_handle_round_trips() {
        let profile = crate::builtin_profile("terra").unwrap();
        let original = OpenAiSession::new(OpenAiConfig::new("key", profile.clone())).unwrap();
        *original.previous_response_id.lock().unwrap() = Some("resp_abc123".into());

        let state = original.export_state().unwrap();
        let mut restored = OpenAiSession::new(OpenAiConfig::new("key", profile)).unwrap();
        restored.restore_state(&state).unwrap();

        assert_eq!(
            *restored.previous_response_id.lock().unwrap(),
            Some("resp_abc123".to_owned())
        );
    }

    #[test]
    fn openai_says_it_cannot_elide_rather_than_appearing_to() {
        let profile = crate::builtin_profile("terra").unwrap();
        let mut session = OpenAiSession::new(OpenAiConfig::new("key", profile)).unwrap();

        let error = session.elide_tool_results(4).unwrap_err();

        // It continues with previous_response_id and keeps no local history to
        // reduce. Returning a successful no-op would let a caller believe the
        // window had been reclaimed when nothing had happened.
        assert!(
            error.to_string().contains("no local history"),
            "said: {error}"
        );
    }

    #[test]
    fn request_repeats_instructions_and_correlates_tool_outputs() {
        let profile = crate::builtin_profile("terra").unwrap();
        let mut config = OpenAiConfig::new("test-key", profile);
        config.instructions = "Work carefully.".into();
        let session = OpenAiSession::new(config).unwrap();
        set_response_id(&session.previous_response_id, "resp-1".into()).unwrap();
        let body = session
            .request_body(TurnInput::ToolResults {
                results: vec![dino_protocol::ToolResult {
                    call_id: "call-1".into(),
                    output: dino_protocol::ToolOutput::success("file contents"),
                }],
            })
            .unwrap();

        assert_eq!(body["previous_response_id"], "resp-1");
        assert_eq!(body["instructions"], "Work carefully.");
        assert_eq!(body["input"][0]["type"], "function_call_output");
        assert_eq!(body["input"][0]["call_id"], "call-1");
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn parser_handles_interleaved_reasoning_and_function_arguments() {
        let mut parser = ResponsesStreamParser::default();
        let chunks = [
            r#"{"type":"response.created","response":{"id":"resp-1"}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"inspect"}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc-1","call_id":"call-1","name":"read_file","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc-1","delta":"{\"path\":\"src/lib.rs\"}"}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"fc-1","arguments":"{\"path\":\"src/lib.rs\"}"}"#,
            r#"{"type":"response.completed","response":{"id":"resp-1","usage":{"input_tokens":50,"output_tokens":12,"input_tokens_details":{"cached_tokens":10},"output_tokens_details":{"reasoning_tokens":4}}}}"#,
        ];
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(parser.consume(chunk).unwrap());
        }

        assert_eq!(parser.response_id().unwrap(), "resp-1");
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallCompleted { call }
                if call.arguments == json!({"path": "src/lib.rs"})
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::Usage { usage }
                if usage.cached_input_tokens == 10 && usage.reasoning_tokens == 4
        )));
        assert_eq!(
            events.last(),
            Some(&ModelEvent::Finished {
                reason: StopReason::ToolUse
            })
        );
    }

    #[test]
    fn parser_rejects_a_terminal_response_with_partial_arguments() {
        let mut parser = ResponsesStreamParser::default();
        parser
            .consume(
                r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc-1","call_id":"call-1","name":"read_file","arguments":""}}"#,
            )
            .unwrap();
        let error = parser
            .consume(r#"{"type":"response.completed","response":{"id":"resp-1"}}"#)
            .unwrap_err();

        assert!(error.to_string().contains("arguments for read_file"));
    }

    #[test]
    fn malformed_stream_json_is_not_silently_discarded() {
        let mut parser = ResponsesStreamParser::default();
        assert!(parser.consume("{not json").is_err());
    }
}
