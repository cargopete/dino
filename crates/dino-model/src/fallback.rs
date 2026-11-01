//! Further providers that take over, in order, when the one in use refuses
//! the account.
//!
//! Only [`ModelError::Account`] moves the session along: a spent quota or a
//! lapsed subscription will not recover within a session, whereas a dropped
//! connection or a rate limit will, and the core already retries those. Once
//! moved, the session does not go back.

use dino_protocol::ModelProfile;
use dino_protocol::ProviderKind;
use dino_protocol::TurnInput;
use serde_json::Value;
use serde_json::json;

use crate::Elision;
use crate::ModelError;
use crate::ModelEventStream;
use crate::ModelFuture;
use crate::ModelSession;
use crate::SessionState;

pub type SwitchNotice = Box<dyn Fn(&str) + Send>;

pub struct FallbackSession {
    /// The primary, then each fallback in the order it is tried.
    chain: Vec<Box<dyn ModelSession>>,
    active: usize,
    notice: SwitchNotice,
}

impl FallbackSession {
    /// `notice` is told why the session moved, each time it does.
    #[must_use]
    pub fn new(
        primary: Box<dyn ModelSession>,
        fallbacks: Vec<Box<dyn ModelSession>>,
        notice: SwitchNotice,
    ) -> Self {
        let mut chain = vec![primary];
        chain.extend(fallbacks);
        Self {
            chain,
            active: 0,
            notice,
        }
    }

    fn active(&self) -> &dyn ModelSession {
        &*self.chain[self.active]
    }

    fn active_mut(&mut self) -> &mut dyn ModelSession {
        &mut *self.chain[self.active]
    }

    fn switch(&mut self, reason: &str) -> Result<(), ModelError> {
        let (from, to) = (self.active, self.active + 1);
        let state = self.chain[from].export_state()?;
        let handed = hand_over(&state, self.chain[to].profile())?;
        self.chain[to].restore_state(&handed)?;
        self.active = to;
        (self.notice)(&format!(
            "{} refused the account ({reason}); continuing on {}",
            self.chain[from].profile().display_name,
            self.chain[to].profile().display_name,
        ));
        Ok(())
    }
}

impl ModelSession for FallbackSession {
    fn profile(&self) -> &ModelProfile {
        self.active().profile()
    }

    fn next(&mut self, input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
        Box::pin(async move {
            loop {
                // An account refusal arrives before anything streams, so the
                // history is untouched and the same input can go across.
                match self.chain[self.active].next(input.clone()).await {
                    Err(ModelError::Account(reason)) if self.active + 1 < self.chain.len() => {
                        self.switch(&reason)?;
                    }
                    other => return other,
                }
            }
        })
    }

    fn discover_limits(&mut self) -> ModelFuture<'_, ()> {
        self.active_mut().discover_limits()
    }

    fn elide_tool_results(&mut self, keep_recent: usize) -> Result<Elision, ModelError> {
        self.active_mut().elide_tool_results(keep_recent)
    }

    fn export_state(&self) -> Result<SessionState, ModelError> {
        self.active().export_state()
    }

    /// A saved session continues on the model that wrote it, or failing that
    /// the first of the chain on its provider.
    fn restore_state(&mut self, state: &SessionState) -> Result<(), ModelError> {
        let index = self
            .chain
            .iter()
            .position(|session| session.profile().key == state.model_key)
            .or_else(|| {
                self.chain
                    .iter()
                    .position(|session| session.profile().provider == state.provider)
            })
            .unwrap_or(0);
        self.chain[index].restore_state(state)?;
        self.active = index;
        Ok(())
    }
}

/// Rewrites one adapter's exported history in another's native shape.
///
/// Reasoning is dropped both ways: Anthropic only accepts thinking blocks it
/// signed, and Qwen has no use for Anthropic's.
///
/// # Errors
///
/// Returns [`ModelError::Configuration`] for a pair of providers it cannot
/// translate between, and [`ModelError::Protocol`] for a malformed history.
pub fn hand_over(state: &SessionState, to: &ModelProfile) -> Result<SessionState, ModelError> {
    let (payload, version) = match (state.provider, to.provider) {
        (from, to) if from == to => (state.payload.clone(), state.version),
        (ProviderKind::Qwen, ProviderKind::Anthropic) => (
            chat_to_messages(&state.payload)?,
            crate::anthropic::STATE_VERSION,
        ),
        (ProviderKind::Anthropic, ProviderKind::Qwen) => (
            messages_to_chat(&state.payload)?,
            crate::qwen::STATE_VERSION,
        ),
        (from, to) => {
            return Err(ModelError::Configuration(format!(
                "cannot hand a {from:?} conversation to {to:?}"
            )));
        }
    };
    Ok(SessionState {
        provider: to.provider,
        model_key: to.key.clone(),
        version,
        payload,
    })
}

fn messages_to_chat(history: &Value) -> Result<Value, ModelError> {
    let messages = history.as_array().ok_or_else(|| {
        ModelError::Protocol("a message history to hand over was not an array".into())
    })?;
    let mut out = Vec::new();
    for message in messages {
        let blocks = message["content"].as_array().into_iter().flatten();
        let mut text = String::new();
        let mut images = Vec::new();
        let mut calls = Vec::new();
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
                Some("image") => {
                    let source = &block["source"];
                    if let (Some(media), Some(data)) =
                        (source["media_type"].as_str(), source["data"].as_str())
                    {
                        images.push(json!(format!("data:{media};base64,{data}")));
                    }
                }
                Some("tool_use") => calls.push(json!({
                    "id": block["id"],
                    "type": "function",
                    "function": {"name": block["name"], "arguments": block["input"].to_string()},
                })),
                // Each result is its own message in the chat shape.
                Some("tool_result") => out.push(json!({
                    "role": "tool",
                    "tool_call_id": block["tool_use_id"],
                    "content": result_text(&block["content"]),
                })),
                _ => {}
            }
        }
        let role = message["role"].as_str().unwrap_or("user");
        if text.is_empty() && images.is_empty() && calls.is_empty() {
            continue;
        }
        let mut converted = json!({"role": role, "content": text});
        if !images.is_empty() {
            converted["images"] = Value::Array(images);
        }
        if !calls.is_empty() {
            converted["tool_calls"] = Value::Array(calls);
        }
        out.push(converted);
    }
    Ok(Value::Array(out))
}

/// A tool result's content may be a string or a list of text blocks.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn chat_to_messages(history: &Value) -> Result<Value, ModelError> {
    let messages = history.as_array().ok_or_else(|| {
        ModelError::Protocol("a chat history to hand over was not an array".into())
    })?;
    let mut out: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for message in messages {
        let text = message["content"].as_str().unwrap_or_default();
        let (role, blocks) = match message["role"].as_str() {
            Some("user") => {
                let mut blocks: Vec<Value> = message["images"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .filter_map(image_block)
                    .collect();
                blocks.push(json!({"type": "text", "text": text}));
                ("user", blocks)
            }
            Some("assistant") => {
                let mut blocks = Vec::new();
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let arguments = call["function"]["arguments"].as_str().unwrap_or("{}");
                    let input = serde_json::from_str::<Value>(arguments)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call_id(call["id"].as_str().unwrap_or_default()),
                        "name": call["function"]["name"],
                        "input": input,
                    }));
                }
                if blocks.is_empty() {
                    blocks.push(json!({"type": "text", "text": "(no reply)"}));
                }
                ("assistant", blocks)
            }
            Some("tool") => (
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": call_id(message["tool_call_id"].as_str().unwrap_or_default()),
                    "content": text,
                    "is_error": false,
                })],
            ),
            _ => continue,
        };
        // Anthropic wants roles to alternate, and the results of one turn's
        // calls in a single user message.
        match out.last_mut() {
            Some((last, existing)) if *last == role => existing.extend(blocks),
            _ => out.push((role, blocks)),
        }
    }
    Ok(Value::Array(
        out.into_iter()
            .map(|(role, content)| json!({"role": role, "content": content}))
            .collect(),
    ))
}

fn image_block(data_url: &str) -> Option<Value> {
    let (header, data) = data_url.strip_prefix("data:")?.split_once(',')?;
    let media_type = header.strip_suffix(";base64")?;
    Some(json!({
        "type": "image",
        "source": {"type": "base64", "media_type": media_type, "data": data},
    }))
}

/// Anthropic accepts only `[A-Za-z0-9_-]` in a tool call ID.
fn call_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use pretty_assertions::assert_eq;

    use super::*;

    /// A session that refuses the account or answers with an empty stream, and
    /// records which model was asked and what was handed to it.
    struct Fake {
        profile: ModelProfile,
        refuses: bool,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl ModelSession for Fake {
        fn profile(&self) -> &ModelProfile {
            &self.profile
        }

        fn next(&mut self, _input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
            self.log
                .lock()
                .unwrap()
                .push(format!("ask {}", self.profile.key));
            let refuses = self.refuses;
            Box::pin(async move {
                if refuses {
                    return Err(ModelError::Account("insufficient balance".into()));
                }
                let stream: ModelEventStream = Box::pin(futures_util::stream::empty());
                Ok(stream)
            })
        }

        fn export_state(&self) -> Result<SessionState, ModelError> {
            Ok(SessionState {
                provider: self.profile.provider,
                model_key: self.profile.key.clone(),
                version: crate::qwen::STATE_VERSION,
                payload: json!([{"role": "user", "content": "fix it"}]),
            })
        }

        fn restore_state(&mut self, state: &SessionState) -> Result<(), ModelError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("{} takes {}", self.profile.key, state.model_key));
            Ok(())
        }
    }

    fn chain(refusing: &[&str], log: &Arc<Mutex<Vec<String>>>) -> FallbackSession {
        let fake = |key: &str| -> Box<dyn ModelSession> {
            Box::new(Fake {
                profile: crate::builtin_profile(key).unwrap(),
                refuses: refusing.contains(&key),
                log: Arc::clone(log),
            })
        };
        FallbackSession::new(
            fake("kimi-k3.1-pro"),
            vec![fake("qwen3.7-max"), fake("claude-opus-5.5")],
            Box::new(|_| {}),
        )
    }

    fn ask() -> TurnInput {
        TurnInput::User {
            content: "fix it".into(),
        }
    }

    #[tokio::test]
    async fn each_refusal_moves_the_session_one_model_along() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut session = chain(&["kimi-k3.1-pro", "qwen3.7-max"], &log);

        assert!(session.next(ask()).await.is_ok());
        assert!(session.next(ask()).await.is_ok());

        assert_eq!(session.profile().key, "claude-opus-5.5");
        assert_eq!(
            *log.lock().unwrap(),
            [
                "ask kimi-k3.1-pro",
                "qwen3.7-max takes qwen3.7-max",
                "ask qwen3.7-max",
                "claude-opus-5.5 takes claude-opus-5.5",
                "ask claude-opus-5.5",
                "ask claude-opus-5.5",
            ]
        );
    }

    #[tokio::test]
    async fn the_last_refusal_is_the_error() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut session = chain(&["kimi-k3.1-pro", "qwen3.7-max", "claude-opus-5.5"], &log);

        let Err(error) = session.next(ask()).await else {
            panic!("every model refused");
        };

        assert!(matches!(error, ModelError::Account(_)), "{error}");
        assert_eq!(session.profile().key, "claude-opus-5.5");
    }

    #[test]
    fn a_saved_session_resumes_on_the_model_that_wrote_it() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut session = chain(&[], &log);
        let saved = SessionState {
            provider: ProviderKind::Qwen,
            model_key: "qwen3.7-max".into(),
            version: crate::qwen::STATE_VERSION,
            payload: json!([]),
        };

        session.restore_state(&saved).unwrap();

        assert_eq!(session.profile().key, "qwen3.7-max");
    }

    #[test]
    fn a_history_survives_the_round_trip_back_to_chat() {
        let chat = json!([
            {"role": "user", "content": "fix it", "images": ["data:image/png;base64,AAAA"]},
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"a\"}"}},
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "one"},
            {"role": "assistant", "content": "done"},
        ]);

        let back = messages_to_chat(&chat_to_messages(&chat).unwrap()).unwrap();

        assert_eq!(back, chat);
    }

    #[test]
    fn a_note_after_tool_results_keeps_its_place_across_providers() {
        let messages = json!([
            {"role": "user", "content": [{"type": "text", "text": "fix it"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_1", "name": "cargo", "input": {"command": "test"}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_1", "content": "{\"exit_code\":0}", "is_error": false},
                {"type": "text", "text": "[dino: verified]"},
            ]},
        ]);

        let chat = messages_to_chat(&messages).unwrap();

        assert_eq!(
            chat[2],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "{\"exit_code\":0}"})
        );
        assert_eq!(
            chat[3],
            json!({"role": "user", "content": "[dino: verified]"})
        );
        assert_eq!(chat_to_messages(&chat).unwrap(), messages);
    }

    #[test]
    fn a_chat_history_becomes_alternating_messages() {
        let history = json!([
            {"role": "user", "content": "fix it", "images": ["data:image/png;base64,AAAA"]},
            {"role": "assistant", "reasoning_content": "hmm", "tool_calls": [
                {"id": "call:1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"a\"}"}},
                {"id": "call:2", "type": "function", "function": {"name": "read", "arguments": "not json"}},
            ]},
            {"role": "tool", "tool_call_id": "call:1", "content": "one"},
            {"role": "tool", "tool_call_id": "call:2", "content": "two"},
            {"role": "assistant", "content": "done"},
        ]);

        let messages = chat_to_messages(&history).unwrap();

        assert_eq!(
            messages,
            json!([
                {"role": "user", "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                    {"type": "text", "text": "fix it"},
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "read", "input": {"path": "a"}},
                    {"type": "tool_use", "id": "call_2", "name": "read", "input": {}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "one", "is_error": false},
                    {"type": "tool_result", "tool_use_id": "call_2", "content": "two", "is_error": false},
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "done"}]},
            ])
        );
    }
}
