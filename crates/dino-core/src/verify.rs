//! Whether Rust source changed in this run after the tests last passed.

use dino_protocol::AgentEvent;
use dino_protocol::ModelEvent;
use dino_protocol::ToolCall;
use serde_json::Value;

const EDIT_TOOLS: [&str; 2] = ["write_file", "apply_patch"];

/// A reason to send the model back when it edited a `.rs` file and no
/// `cargo test` has passed since, or `None`.
#[must_use]
pub fn untested_edits(events: &[AgentEvent]) -> Option<String> {
    let mut calls: Vec<&ToolCall> = Vec::new();
    let mut edited: Vec<String> = Vec::new();
    for event in events {
        match event {
            AgentEvent::Model {
                event: ModelEvent::ToolCallCompleted { call },
                ..
            } => calls.push(call),
            AgentEvent::ToolFinished { result, .. } if !result.output.is_error => {
                let Some(call) = calls.iter().find(|call| call.id == result.call_id) else {
                    continue;
                };
                if let Some(path) = edited_rust_file(call) {
                    if !edited.contains(&path) {
                        edited.push(path);
                    }
                } else if passing_test_run(call, &result.output.content) {
                    edited.clear();
                }
            }
            _ => {}
        }
    }
    (!edited.is_empty()).then(|| {
        format!(
            "you changed {} Rust file(s) ({}) and no `cargo test` has passed since. Run the \
             tests, adding tests for new behaviour, or say plainly why they cannot run here.",
            edited.len(),
            edited.join(", ")
        )
    })
}

fn edited_rust_file(call: &ToolCall) -> Option<String> {
    if !EDIT_TOOLS.contains(&call.name.as_str()) {
        return None;
    }
    let path = call.arguments.get("path")?.as_str()?;
    std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
        .then(|| path.to_owned())
}

fn passing_test_run(call: &ToolCall, output: &str) -> bool {
    let runs_tests = match call.name.as_str() {
        "cargo" => call.arguments.get("command").and_then(Value::as_str) == Some("test"),
        "exec" => {
            let words: Vec<&str> = call
                .arguments
                .get("command")
                .and_then(Value::as_array)
                .map(|words| words.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            words.windows(2).any(|pair| pair == ["cargo", "test"])
        }
        "shell" => call
            .arguments
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|line| line.contains("cargo test")),
        _ => false,
    };
    runs_tests
        && serde_json::from_str::<Value>(output)
            .ok()
            .and_then(|value| value.get("exit_code").and_then(Value::as_i64))
            == Some(0)
}

#[cfg(test)]
mod tests {
    use super::untested_edits;
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
    fn an_edit_with_no_test_run_is_unfinished() {
        let mut events = Vec::new();
        call(
            &mut events,
            "1",
            "write_file",
            json!({"path": "src/lib.rs"}),
            "{}",
        );
        let reason = untested_edits(&events).unwrap();
        assert!(reason.contains("src/lib.rs"), "{reason}");
    }

    #[test]
    fn a_passing_test_run_after_the_edit_settles_it() {
        let mut events = Vec::new();
        call(
            &mut events,
            "1",
            "apply_patch",
            json!({"path": "src/lib.rs"}),
            "{}",
        );
        call(
            &mut events,
            "2",
            "cargo",
            json!({"command": "test"}),
            r#"{"exit_code":0}"#,
        );
        assert_eq!(untested_edits(&events), None);
    }

    #[test]
    fn a_failing_run_or_a_later_edit_does_not() {
        let mut events = Vec::new();
        call(
            &mut events,
            "1",
            "apply_patch",
            json!({"path": "src/a.rs"}),
            "{}",
        );
        call(
            &mut events,
            "2",
            "exec",
            json!({"command": ["cargo", "test"]}),
            r#"{"exit_code":101}"#,
        );
        assert!(untested_edits(&events).is_some());

        call(
            &mut events,
            "3",
            "shell",
            json!({"command": "cargo test --workspace 2>&1"}),
            r#"{"exit_code":0}"#,
        );
        call(
            &mut events,
            "4",
            "write_file",
            json!({"path": "src/b.rs"}),
            "{}",
        );
        let reason = untested_edits(&events).unwrap();
        assert!(
            reason.contains("src/b.rs") && !reason.contains("src/a.rs"),
            "{reason}"
        );
    }

    #[test]
    fn other_files_do_not_count() {
        let mut events = Vec::new();
        call(
            &mut events,
            "1",
            "write_file",
            json!({"path": "README.md"}),
            "{}",
        );
        assert_eq!(untested_edits(&events), None);
    }
}
