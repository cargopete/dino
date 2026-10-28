//! What a compacted conversation should not have to trust the model to
//! remember: the person's latest words, the files read and changed, and how
//! the tests last went. Read from the event log, not from the summary.

use std::fmt::Write as _;

use dino_protocol::AgentEvent;
use dino_protocol::ModelEvent;
use dino_protocol::ToolCall;
use serde_json::Value;

/// Between a message and the summary carried ahead of it.
pub const SUMMARY_DIVIDER: &str =
    "\n\n---\n\nSummary of this session so far, written before it was compacted:\n\n";

/// The person's messages kept word for word.
const KEPT_MESSAGES: usize = 3;
/// Paths listed per kind, at most.
const LISTED_PATHS: usize = 30;

#[derive(Debug, Default)]
pub struct Recall {
    messages: Vec<String>,
    read: Vec<String>,
    changed: Vec<String>,
    tests: Option<String>,
}

impl Recall {
    /// Takes in one run's events.
    pub fn absorb(&mut self, events: &[AgentEvent]) {
        let mut calls: Vec<&ToolCall> = Vec::new();
        for event in events {
            match event {
                AgentEvent::Submitted { text } => {
                    // Only the person's own words: not a carried summary, nor
                    // the message that resumes a task after compaction.
                    let own = text.split(SUMMARY_DIVIDER).next().unwrap_or_default();
                    if own != crate::session::CONTINUE_AFTER_COMPACTION && !own.is_empty() {
                        self.messages.push(own.to_owned());
                    }
                }
                AgentEvent::Model {
                    event: ModelEvent::ToolCallCompleted { call },
                    ..
                } => calls.push(call),
                AgentEvent::ToolFinished { result, .. } if !result.output.is_error => {
                    let Some(call) = calls.iter().find(|call| call.id == result.call_id) else {
                        continue;
                    };
                    self.absorb_call(call, &result.output.content);
                }
                _ => {}
            }
        }
    }

    fn absorb_call(&mut self, call: &ToolCall, output: &str) {
        let path = call.arguments.get("path").and_then(Value::as_str);
        match (call.name.as_str(), path) {
            ("read", Some(path)) => add(&mut self.read, path),
            ("write_file" | "apply_patch", Some(path)) => add(&mut self.changed, path),
            ("cargo", _)
                if call.arguments.get("command").and_then(Value::as_str) == Some("test") =>
            {
                let exit = serde_json::from_str::<Value>(output)
                    .ok()
                    .and_then(|value| value.get("exit_code").and_then(Value::as_i64));
                self.tests = Some(match exit {
                    Some(0) => "the last `cargo test` passed".to_owned(),
                    Some(code) => format!("the last `cargo test` failed (exit {code})"),
                    None => "the last `cargo test` did not finish".to_owned(),
                });
            }
            _ => {}
        }
    }

    /// The appendix to a compacted summary, or `None` when nothing was seen.
    #[must_use]
    pub fn render(&self) -> Option<String> {
        if self.messages.is_empty() && self.read.is_empty() && self.changed.is_empty() {
            return None;
        }
        let mut text = String::from("From the session log, not from the summary:\n");
        let start = self.messages.len().saturating_sub(KEPT_MESSAGES);
        for message in &self.messages[start..] {
            let _ = writeln!(
                text,
                "\nThe person wrote:\n> {}",
                message.replace('\n', "\n> ")
            );
        }
        for (label, paths) in [("Changed", &self.changed), ("Read", &self.read)] {
            if paths.is_empty() {
                continue;
            }
            let shown: Vec<&str> = paths
                .iter()
                .take(LISTED_PATHS)
                .map(String::as_str)
                .collect();
            let more = paths.len().saturating_sub(LISTED_PATHS);
            let more = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            let _ = writeln!(text, "\n{label}: {}{more}", shown.join(", "));
        }
        if let Some(tests) = &self.tests {
            let _ = writeln!(text, "\nTests: {tests}.");
        }
        Some(text)
    }
}

fn add(paths: &mut Vec<String>, path: &str) {
    if !paths.iter().any(|known| known == path) {
        paths.push(path.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::Recall;
    use dino_protocol::AgentEvent;
    use dino_protocol::ModelEvent;
    use dino_protocol::ToolCall;
    use dino_protocol::ToolOutput;
    use dino_protocol::ToolResult;
    use serde_json::Value;
    use serde_json::json;

    fn call(events: &mut Vec<AgentEvent>, id: &str, name: &str, arguments: Value, output: &str) {
        events.push(AgentEvent::Model {
            model_turn: 1,
            event: ModelEvent::ToolCallCompleted {
                call: ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments,
                },
            },
        });
        events.push(AgentEvent::ToolFinished {
            model_turn: 1,
            result: ToolResult {
                call_id: id.into(),
                output: ToolOutput::success(output),
            },
        });
    }

    #[test]
    fn the_appendix_keeps_words_paths_and_the_test_verdict() {
        let mut events = vec![AgentEvent::Submitted {
            text: "add thorough tests".into(),
        }];
        call(&mut events, "1", "read", json!({"path": "src/a.rs"}), "{}");
        call(
            &mut events,
            "2",
            "apply_patch",
            json!({"path": "src/a.rs"}),
            "{}",
        );
        call(
            &mut events,
            "3",
            "cargo",
            json!({"command": "test"}),
            r#"{"exit_code":101}"#,
        );
        let mut recall = Recall::default();
        recall.absorb(&events);

        let text = recall.render().unwrap();
        assert!(text.contains("> add thorough tests"), "{text}");
        assert!(text.contains("Changed: src/a.rs"), "{text}");
        assert!(text.contains("Read: src/a.rs"), "{text}");
        assert!(text.contains("failed (exit 101)"), "{text}");
    }

    #[test]
    fn a_carried_summary_and_the_resume_message_are_not_the_persons_words() {
        let events = vec![
            AgentEvent::Submitted {
                text: format!("next step{}old summary", super::SUMMARY_DIVIDER),
            },
            AgentEvent::Submitted {
                text: crate::session::CONTINUE_AFTER_COMPACTION.to_owned(),
            },
        ];
        let mut recall = Recall::default();
        recall.absorb(&events);
        let text = recall.render().unwrap();
        assert!(text.contains("> next step"), "{text}");
        assert!(
            !text.contains("old summary") && !text.contains("compacted in the middle"),
            "{text}"
        );
    }

    #[test]
    fn only_the_latest_messages_are_kept() {
        let events: Vec<AgentEvent> = (1..=5)
            .map(|n| AgentEvent::Submitted {
                text: format!("message {n}"),
            })
            .collect();
        let mut recall = Recall::default();
        recall.absorb(&events);
        let text = recall.render().unwrap();
        assert!(
            !text.contains("message 2") && text.contains("message 3"),
            "{text}"
        );
    }
}
