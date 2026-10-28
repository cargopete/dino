//! A checklist the model keeps for a multi-step task.
//!
//! Each call sends the whole list, so the latest call is the list, and it
//! survives `--resume` by being part of the conversation like any other call.

use std::fmt::Write as _;

use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::ToolError;
use crate::ToolFuture;
use crate::ToolRuntime;

pub const TODO: &str = "todo";
const MAX_ITEMS: usize = 50;

#[derive(Debug, Clone, Copy, Default)]
pub struct TodoTool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Done,
    /// Cannot be done here; the item carries the reason.
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub text: String,
    pub status: Status,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    items: Vec<Item>,
}

/// The list a `todo` call carries, validated.
///
/// # Errors
///
/// Returns [`ToolError`] when the arguments do not parse, the list is too long,
/// or an item has no text.
pub fn items(arguments: &Value) -> Result<Vec<Item>, ToolError> {
    let request: Request = serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::new(format!("invalid {TODO} arguments: {error}")))?;
    if request.items.len() > MAX_ITEMS {
        return Err(ToolError::new(format!(
            "a list of {} items is too long; keep it under {MAX_ITEMS}",
            request.items.len()
        )));
    }
    if request.items.iter().any(|item| item.text.trim().is_empty()) {
        return Err(ToolError::new("every item needs text"));
    }
    if request.items.iter().any(|item| {
        item.status == Status::Blocked
            && item
                .reason
                .as_deref()
                .is_none_or(|reason| reason.trim().is_empty())
    }) {
        return Err(ToolError::new(
            "a blocked item needs a reason saying what stops it",
        ));
    }
    Ok(request.items)
}

/// One line per item: `[x]` done, `[~]` in progress, `[ ]` pending, `[!]` blocked.
#[must_use]
pub fn render(items: &[Item]) -> String {
    let mut text = String::new();
    for item in items {
        let mark = match item.status {
            Status::Done => "[x]",
            Status::InProgress => "[~]",
            Status::Pending => "[ ]",
            Status::Blocked => "[!]",
        };
        let _ = write!(&mut text, "{mark} {}", item.text.trim());
        if let (Status::Blocked, Some(reason)) = (item.status, &item.reason) {
            let _ = write!(&mut text, " (blocked: {})", reason.trim());
        }
        text.push('\n');
    }
    text
}

impl ToolRuntime for TodoTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: TODO.into(),
            description: "Keep a short checklist for a task with several steps. Send the whole \
                          list every time, with each item pending, in_progress, done or blocked. Mark \
                          one in_progress while you work on it and done as soon as it is finished. \
                          Mark an item blocked, with a reason, only when it cannot be done here. \
                          The person sees the list as it changes."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "maxItems": MAX_ITEMS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": {"type": "string", "minLength": 1},
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "done", "blocked"]
                                },
                                "reason": {
                                    "type": "string",
                                    "description": "For a blocked item: what stops it."
                                }
                            },
                            "required": ["text", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["items"],
                "additionalProperties": false
            }),
        }]
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if call.name != TODO {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        items(&call.arguments)?;
        Ok(ToolAction::read_only())
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let result = items(&call.arguments).map(|items| {
            let count = |status| items.iter().filter(|item| item.status == status).count();
            let blocked = match count(Status::Blocked) {
                0 => String::new(),
                n => format!(", {n} blocked"),
            };
            ToolOutput::success(format!(
                "Checklist updated: {} of {} done{blocked}.\n{}",
                count(Status::Done),
                items.len(),
                render(&items)
            ))
        });
        Box::pin(async move { result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(arguments: Value) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: TODO.into(),
            arguments,
        }
    }

    #[tokio::test]
    async fn the_list_is_rendered_with_a_count() {
        let output = TodoTool
            .execute(&call(json!({"items": [
                {"text": "Read the parser", "status": "done"},
                {"text": "Fix the off-by-one", "status": "in_progress"},
                {"text": "Add a test", "status": "pending"}
            ]})))
            .await
            .unwrap();

        assert_eq!(
            output.content,
            "Checklist updated: 1 of 3 done.\n[x] Read the parser\n[~] Fix the off-by-one\n[ ] Add a test\n"
        );
    }

    #[tokio::test]
    async fn a_blocked_item_shows_its_reason() {
        let output = TodoTool
            .execute(&call(json!({"items": [
                {"text": "Build it", "status": "done"},
                {"text": "Push it", "status": "blocked", "reason": "no network"}
            ]})))
            .await
            .unwrap();

        assert_eq!(
            output.content,
            "Checklist updated: 1 of 2 done, 1 blocked.\n[x] Build it\n[!] Push it (blocked: no network)\n"
        );
    }

    #[test]
    fn empty_text_and_unknown_statuses_are_refused() {
        for arguments in [
            json!({"items": [{"text": " ", "status": "done"}]}),
            json!({"items": [{"text": "x", "status": "blocked"}]}),
            json!({"items": [{"text": "x", "status": "blocked", "reason": " "}]}),
            json!({"items": [{"text": "x", "status": "stuck"}]}),
            json!({"items": "x"}),
        ] {
            assert!(
                TodoTool.classify(&call(arguments.clone())).is_err(),
                "{arguments}"
            );
        }
    }
}
