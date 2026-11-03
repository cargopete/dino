//! Whether Rust source changed in this run after the tests last passed.

use std::collections::HashMap;

use dino_protocol::AgentEvent;
use dino_protocol::ModelEvent;
use dino_protocol::ToolCall;
use serde_json::Value;

use crate::outcomes::Ran;
use crate::outcomes::Tests;
use crate::outcomes::runs_tests;

const EDIT_TOOLS: [&str; 2] = ["write_file", "apply_patch"];

/// A reason to send the model back when it edited a `.rs` file and no
/// `cargo test` has passed since, or `None`.
#[must_use]
pub fn untested_edits(events: &[AgentEvent]) -> Option<String> {
    let mut calls: Vec<&ToolCall> = Vec::new();
    let mut edited: Vec<String> = Vec::new();
    let mut pieces = HashMap::new();
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
                } else if passing_test_run(call, &result.output.content, &mut pieces) {
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

/// Whether a call's result shows `cargo test` passing with at least one test
/// run. A command run by hand counts by its `test result:` lines, since a pipe
/// into `tail` exits 0 whatever the tests did. `background` output arrives in
/// pieces, so it is gathered per process until the process has exited.
fn passing_test_run(call: &ToolCall, output: &str, pieces: &mut HashMap<u64, String>) -> bool {
    let ran = Ran::parse(output);
    if !runs_tests(call, ran.command.as_deref()) {
        return false;
    }
    if call.name == "cargo" {
        let value: Value = serde_json::from_str(output).unwrap_or_default();
        return ran.exit_code == Some(0) && value.get("no_tests").is_none();
    }
    let text = if call.name == "background" {
        let Some(id) = serde_json::from_str::<Value>(output)
            .ok()
            .and_then(|value| value.get("id")?.as_u64())
        else {
            return false;
        };
        let gathered = pieces.entry(id).or_default();
        gathered.push_str(&ran.text);
        gathered.push('\n');
        if ran.running {
            return false;
        }
        pieces.remove(&id).unwrap_or_default()
    } else {
        ran.text
    };
    ran.exit_code == Some(0) && Tests::parse(&text).all_passed()
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
            r#"{"exit_code":0,"stdout":"test result: ok. 3 passed; 0 failed"}"#,
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

    #[test]
    fn a_passing_run_through_a_pipe_or_in_the_background_settles_it() {
        let mut events = Vec::new();
        call(
            &mut events,
            "1",
            "apply_patch",
            json!({"path": "src/lib.rs"}),
            "{}",
        );
        let command = "RUSTC=\"$(rustup which rustc)\" cargo test --lib seal:: 2>&1 | tail -40";
        call(
            &mut events,
            "2",
            "background",
            json!({"op": "start", "command": command}),
            &json!({"id": 3, "command": command, "running": true, "exit_code": null, "output": "   Compiling a\n"}).to_string(),
        );
        assert!(untested_edits(&events).is_some());
        call(
            &mut events,
            "3",
            "background",
            json!({"op": "output", "id": 3}),
            &json!({"id": 3, "command": command, "running": false, "exit_code": 0,
                "output": "running 3 tests\ntest result: ok. 3 passed; 0 failed; 0 ignored"})
            .to_string(),
        );
        assert_eq!(untested_edits(&events), None);

        call(
            &mut events,
            "4",
            "apply_patch",
            json!({"path": "src/lib.rs"}),
            "{}",
        );
        call(
            &mut events,
            "5",
            "shell",
            json!({"command": "cargo test 2>&1 | tail"}),
            r#"{"exit_code":0,"stdout":"error: could not compile `a`\n"}"#,
        );
        assert!(untested_edits(&events).is_some(), "a pipe hid the failure");
    }

    #[test]
    fn a_run_that_matched_no_tests_does_not_settle_it() {
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
            "shell",
            json!({"command": "cargo test --test skill_refs -- a b c"}),
            r#"{"exit_code":0,"stdout":"running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 6 filtered out"}"#,
        );
        call(
            &mut events,
            "3",
            "cargo",
            json!({"command": "test", "filter": "nothing"}),
            r#"{"exit_code":0,"no_tests":"no test ran"}"#,
        );
        assert!(untested_edits(&events).is_some());
    }
}
