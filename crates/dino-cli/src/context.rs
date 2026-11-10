//! `/context`: what is filling the window.
//!
//! Providers report one figure per request, the whole input, so every part is
//! estimated from its size and labelled so; the provider's figure, where there
//! is one, is the total to believe.

use std::fmt::Write as _;
use std::path::Path;

use dino_core::prompt::PromptPart;
use dino_protocol::ProviderKind;
use dino_protocol::ToolDefinition;
use serde::Serialize;
use serde_json::Value;

use crate::render::compact;
use crate::style;
use crate::style::FAINT;
use crate::style::MUTED;

/// Bytes to a token, near enough for English and code alike.
const BYTES_PER_TOKEN: usize = 4;

/// Keys whose strings in a provider's history are not text the model reads as
/// tokens: ids, signatures, base64 images.
const NOT_TEXT: [&str; 8] = [
    "id",
    "tool_use_id",
    "tool_call_id",
    "signature",
    "type",
    "data",
    "role",
    "images",
];

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub window: Option<u32>,
    /// Input tokens of the latest request, as the provider counted them.
    pub reported: Option<u64>,
    pub groups: Vec<Group>,
    pub marks: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Group {
    pub name: String,
    pub rows: Vec<Row>,
}

#[derive(Debug, Serialize)]
pub struct Row {
    pub label: String,
    pub tokens: u64,
    pub note: Option<String>,
}

/// What the window holds, as far as Dino can see it.
pub struct Contents<'a> {
    pub prompt: &'a [PromptPart],
    pub tools: &'a [ToolDefinition],
    /// The provider's native history, or `None` when it keeps the history.
    pub history: Option<(ProviderKind, &'a Value)>,
    pub window: Option<u32>,
    pub reported: Option<u64>,
    pub marks: Vec<String>,
}

pub fn estimate(bytes: usize) -> u64 {
    bytes.div_ceil(BYTES_PER_TOKEN) as u64
}

fn counted_tools(count: usize) -> String {
    if count == 1 {
        "1 tool".to_owned()
    } else {
        format!("{count} tools")
    }
}

fn row(label: impl Into<String>, bytes: usize, note: Option<String>) -> Row {
    Row {
        label: label.into(),
        tokens: estimate(bytes),
        note,
    }
}

/// The system prompt and the tool schemas, which every request carries.
pub fn fixed(prompt: &[PromptPart], tools: &[ToolDefinition]) -> Vec<Group> {
    let mut parts: Vec<Row> = Vec::new();
    for part in prompt {
        // A few lines of Dino's own between two larger parts.
        match parts.first_mut() {
            Some(rules) if part.label == "approval note" => {
                rules.tokens += estimate(part.text.len());
            }
            _ => parts.push(row(
                crate::render::short_path(Path::new(&part.label), Path::new("")),
                part.text.len(),
                None,
            )),
        }
    }
    let mut servers: Vec<(String, usize, usize)> = Vec::new();
    let mut builtin = (0_usize, 0_usize);
    for tool in tools {
        let bytes = serde_json::to_string(tool).map_or(0, |text| text.len());
        let server = tool
            .name
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
            .map(|(server, _)| server);
        if let Some(server) = server {
            match servers.iter_mut().find(|(name, ..)| name == server) {
                Some(entry) => {
                    entry.1 += 1;
                    entry.2 += bytes;
                }
                None => servers.push((server.to_owned(), 1, bytes)),
            }
        } else {
            builtin.0 += 1;
            builtin.1 += bytes;
        }
    }
    let mut schemas = vec![row(
        format!("built in, {}", counted_tools(builtin.0)),
        builtin.1,
        None,
    )];
    for (server, count, bytes) in servers {
        schemas.push(row(
            format!("mcp {server}, {}", counted_tools(count)),
            bytes,
            None,
        ));
    }
    vec![
        Group {
            name: "system prompt, estimated".to_owned(),
            rows: parts,
        },
        Group {
            name: "tool schemas, estimated".to_owned(),
            rows: schemas,
        },
    ]
}

impl Report {
    pub fn new(contents: &Contents<'_>) -> Self {
        let mut groups = fixed(contents.prompt, contents.tools);
        let rows = match contents.history {
            Some((provider, payload)) => conversation(provider, payload).rows(),
            None => vec![Row {
                label: "held by the provider".to_owned(),
                tokens: 0,
                note: Some("not itemised".to_owned()),
            }],
        };
        groups.push(Group {
            name: "conversation, estimated".to_owned(),
            rows,
        });
        Self {
            window: contents.window,
            reported: contents.reported,
            groups,
            marks: contents.marks.clone(),
        }
    }

    pub fn estimated(&self) -> u64 {
        self.groups
            .iter()
            .flat_map(|group| &group.rows)
            .map(|row| row.tokens)
            .sum()
    }

    /// For a person at the terminal.
    pub fn render(&self) -> String {
        let estimated = self.estimated();
        let used = self.reported.unwrap_or(estimated);
        let mut block = String::new();
        let headline = match (self.window, self.reported) {
            (Some(window), Some(reported)) => format!(
                "{} of {} tokens ({}%), as the provider counted the last request",
                compact(reported),
                compact(u64::from(window)),
                reported * 100 / u64::from(window).max(1)
            ),
            (Some(window), None) => format!(
                "about {} of {} tokens, estimated",
                compact(estimated),
                compact(u64::from(window))
            ),
            (None, Some(reported)) => format!(
                "{} tokens in the last request; the window is not known",
                compact(reported)
            ),
            (None, None) => format!("about {} tokens, estimated", compact(estimated)),
        };
        block.push_str(&crate::render::status_block(&[("context", headline)]));
        let width = self
            .groups
            .iter()
            .flat_map(|group| &group.rows)
            .map(|row| row.label.chars().count())
            .max()
            .unwrap_or(0)
            .max(12);
        for group in &self.groups {
            let _ = writeln!(block, "  {}", style::paint(style::TEXT, &group.name));
            for row in &group.rows {
                let figure = if row.tokens == 0 && row.note.is_some() {
                    String::new()
                } else {
                    compact(row.tokens)
                };
                let note = row
                    .note
                    .as_ref()
                    .map(|note| style::paint(FAINT, &format!("  {note}")))
                    .unwrap_or_default();
                let _ = writeln!(
                    block,
                    "    {}  {}{note}",
                    style::paint(MUTED, &format!("{:<width$}", row.label)),
                    style::paint(style::TEXT, &format!("{figure:>7}"))
                );
            }
        }
        let mut rows = Vec::new();
        if let Some(window) = self.window {
            rows.push(("free", compact(u64::from(window).saturating_sub(used))));
        }
        if !self.marks.is_empty() {
            rows.push(("marks", self.marks.join(" · ")));
        }
        block.push_str(&crate::render::status_block(&rows));
        let gap = match self.reported {
            Some(reported) => format!(
                "; the parts come to {} against the {} reported",
                compact(estimated),
                compact(reported)
            ),
            None => String::new(),
        };
        let _ = writeln!(
            block,
            "{}",
            style::paint(
                FAINT,
                &format!("  each part is estimated at {BYTES_PER_TOKEN} bytes a token{gap}")
            )
        );
        block
    }
}

/// The window's marks: where Dino warns, elides and compacts.
pub fn marks(window: Option<u32>, compaction: crate::config::Compaction) -> Vec<String> {
    let config = dino_core::AgentConfig::default();
    let mut marks = Vec::new();
    if let Some(window) = window {
        let at = |percent: u8| compact(u64::from(window) * u64::from(percent) / 100);
        marks.push(format!(
            "warns at {}% ({})",
            config.warn_at_percent,
            at(config.warn_at_percent)
        ));
        marks.push(format!(
            "elides past {}% ({})",
            config.elide_at_percent,
            at(config.elide_at_percent)
        ));
    }
    marks.push(match compaction {
        crate::config::Compaction::Manual => "compacts on /compact only".to_owned(),
        crate::config::Compaction::Tasks { at_tokens } => format!(
            "a task seen through compacts past the warning or {}",
            compact(at_tokens)
        ),
        crate::config::Compaction::Automatic { at_tokens } => {
            format!("compacts past the warning or {}", compact(at_tokens))
        }
    });
    marks
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Conversation {
    person: usize,
    model: usize,
    results: usize,
    elided: usize,
    images: usize,
}

impl Conversation {
    fn rows(&self) -> Vec<Row> {
        let mut rows = vec![
            row("your messages", self.person, None),
            row("model replies and calls", self.model, None),
            row(
                "tool results",
                self.results,
                (self.elided > 0).then(|| format!("{} elided", self.elided)),
            ),
        ];
        if self.images > 0 {
            rows.push(Row {
                label: "images".to_owned(),
                tokens: 0,
                note: Some(format!("{}, not estimated", self.images)),
            });
        }
        rows
    }
}

/// Sorts a provider's native history by who wrote what. Anthropic keeps tool
/// results as blocks in a user message; Qwen's chat history gives them a role.
fn conversation(provider: ProviderKind, payload: &Value) -> Conversation {
    let mut counted = Conversation::default();
    let Some(messages) = payload.as_array() else {
        return counted;
    };
    for message in messages {
        let role = message["role"].as_str().unwrap_or_default();
        match (provider, role) {
            (_, "system") => {}
            (_, "assistant") => counted.model += text_bytes(message, &mut 0),
            (ProviderKind::Qwen, "tool") => {
                counted.results += text_bytes(message, &mut counted.elided);
            }
            (_, _) => {
                if let Some(images) = message["images"].as_array() {
                    counted.images += images.len();
                }
                match &message["content"] {
                    Value::Array(blocks) => {
                        for block in blocks {
                            match block["type"].as_str() {
                                Some("tool_result") => {
                                    counted.results += text_bytes(block, &mut counted.elided);
                                }
                                Some("image") => counted.images += 1,
                                _ => counted.person += text_bytes(block, &mut 0),
                            }
                        }
                    }
                    content => counted.person += text_bytes(content, &mut 0),
                }
            }
        }
    }
    counted
}

/// The bytes of text in `value`, counting the elision markers met on the way.
fn text_bytes(value: &Value, elided: &mut usize) -> usize {
    match value {
        Value::String(text) => {
            if text.starts_with("[dino elided ") {
                *elided += 1;
            }
            if text.starts_with("data:") {
                0
            } else {
                text.len()
            }
        }
        Value::Array(items) => items.iter().map(|item| text_bytes(item, elided)).sum(),
        Value::Object(fields) => fields
            .iter()
            .filter(|(key, _)| !NOT_TEXT.contains(&key.as_str()))
            .map(|(_, field)| text_bytes(field, elided))
            .sum(),
        Value::Number(number) => number.to_string().len(),
        Value::Bool(_) | Value::Null => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_history_is_sorted_into_messages_replies_and_results() {
        let history = serde_json::json!([
            { "role": "user", "content": [{ "type": "text", "text": "fix the bug" }] },
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "hmm", "signature": "x".repeat(500) },
                { "type": "tool_use", "id": "t1", "name": "read", "input": { "path": "a.rs" } }
            ] },
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "t1", "content": "fn main() {}" },
                { "type": "tool_result", "tool_use_id": "t0",
                  "content": "[dino elided 900 bytes of this earlier tool result]" },
                { "type": "image", "source": { "type": "base64", "data": "A".repeat(9_000) } }
            ] }
        ]);

        let counted = conversation(ProviderKind::Anthropic, &history);

        assert_eq!(counted.person, "fix the bug".len());
        assert_eq!(counted.model, "hmm".len() + "read".len() + "a.rs".len());
        assert_eq!(counted.elided, 1);
        assert_eq!(counted.images, 1);
        assert!(counted.results < 100, "{counted:?}");
    }

    #[test]
    fn qwen_history_gives_tool_results_a_role_and_skips_the_system_prompt() {
        let history = serde_json::json!([
            { "role": "system", "content": "x".repeat(10_000) },
            { "role": "user", "content": "hello", "images": ["data:image/png;base64,AAAA"] },
            { "role": "assistant", "content": null, "reasoning_content": "think",
              "tool_calls": [{ "id": "c1", "type": "function",
                  "function": { "name": "list", "arguments": "{}" } }] },
            { "role": "tool", "tool_call_id": "c1", "content": "src/\nCargo.toml" }
        ]);

        let counted = conversation(ProviderKind::Qwen, &history);

        assert_eq!(
            counted,
            Conversation {
                person: 5,
                model: "think".len() + "list".len() + "{}".len(),
                results: "src/\nCargo.toml".len(),
                elided: 0,
                images: 1,
            }
        );
    }

    #[test]
    fn the_report_groups_mcp_tools_by_server_and_says_what_is_estimated() {
        let tool = |name: &str| ToolDefinition {
            name: name.to_owned(),
            description: "d".repeat(400),
            input_schema: serde_json::json!({ "type": "object" }),
        };
        let prompt = [
            PromptPart {
                label: "built-in rules".into(),
                text: "r".repeat(4_000),
            },
            PromptPart {
                label: "approval note".into(),
                text: "a".repeat(400),
            },
            PromptPart {
                label: "AGENTS.md".into(),
                text: "g".repeat(800),
            },
        ];
        let tools = [
            tool("read"),
            tool("mcp__github__issue"),
            tool("mcp__github__pr"),
        ];
        let report = Report::new(&Contents {
            prompt: &prompt,
            tools: &tools,
            history: None,
            window: Some(200_000),
            reported: Some(5_000),
            marks: marks(Some(200_000), crate::config::Compaction::Manual),
        });

        let labels: Vec<&str> = report
            .groups
            .iter()
            .flat_map(|group| &group.rows)
            .map(|row| row.label.as_str())
            .collect();
        assert_eq!(
            labels,
            [
                "built-in rules",
                "AGENTS.md",
                "built in, 1 tool",
                "mcp github, 2 tools",
                "held by the provider"
            ]
        );
        assert_eq!(report.groups[0].rows[0].tokens, 1_100);
        let text = report.render();
        assert!(text.contains("5.0k of 200.0k tokens (2%)"), "{text}");
        assert!(text.contains("system prompt, estimated"), "{text}");
        assert!(text.contains("warns at 70% (140.0k)"), "{text}");
        assert!(text.contains("estimated at 4 bytes a token"), "{text}");
    }
}
