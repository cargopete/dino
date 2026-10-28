//! A second provider that takes over when the first refuses the account.
//!
//! Only [`ModelError::Account`] moves the session across: a spent quota or a
//! lapsed subscription will not recover within a session, whereas a dropped
//! connection or a rate limit will, and the core already retries those. Once
//! across, the session stays there.

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
    primary: Box<dyn ModelSession>,
    backup: Box<dyn ModelSession>,
    on_backup: bool,
    notice: SwitchNotice,
}

impl FallbackSession {
    /// `notice` is told why the session moved, once, when it does.
    #[must_use]
    pub fn new(
        primary: Box<dyn ModelSession>,
        backup: Box<dyn ModelSession>,
        notice: SwitchNotice,
    ) -> Self {
        Self {
            primary,
            backup,
            on_backup: false,
            notice,
        }
    }

    fn active(&self) -> &dyn ModelSession {
        if self.on_backup {
            &*self.backup
        } else {
            &*self.primary
        }
    }

    fn active_mut(&mut self) -> &mut dyn ModelSession {
        if self.on_backup {
            &mut *self.backup
        } else {
            &mut *self.primary
        }
    }

    fn switch(&mut self, reason: &str) -> Result<(), ModelError> {
        let state = self.primary.export_state()?;
        let handed = hand_over(&state, self.backup.profile())?;
        self.backup.restore_state(&handed)?;
        self.on_backup = true;
        (self.notice)(&format!(
            "{} refused the account ({reason}); continuing on {}",
            self.primary.profile().display_name,
            self.backup.profile().display_name,
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
            if self.on_backup {
                return self.backup.next(input).await;
            }
            // An account refusal arrives before anything streams, so the
            // primary's history is untouched and the same input can go across.
            match self.primary.next(input.clone()).await {
                Err(ModelError::Account(reason)) => {
                    self.switch(&reason)?;
                    self.backup.next(input).await
                }
                other => other,
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

    fn restore_state(&mut self, state: &SessionState) -> Result<(), ModelError> {
        if state.provider == self.backup.profile().provider
            && state.provider != self.primary.profile().provider
        {
            self.backup.restore_state(state)?;
            self.on_backup = true;
            return Ok(());
        }
        self.on_backup = false;
        self.primary.restore_state(state)
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
    use pretty_assertions::assert_eq;

    use super::*;

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
