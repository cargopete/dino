//! What a tool result says, in the few words a person wants beside the call.
//!
//! The full result is in the session log. This is the gist: lines read, lines
//! changed, tests passed, an exit code. A summary that would need the result's
//! own words to be true falls back to its size.

use gyr_protocol::ToolCall;
use gyr_protocol::ToolOutput;
use serde_json::Value;

/// How a result should read: as routine, or as something that went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Fine,
    Failed,
}

/// The gist of one result, or `None` where the call line says it all.
#[must_use]
pub fn summarise(call: &ToolCall, output: &ToolOutput) -> Option<(Tone, String)> {
    if output.is_error {
        let first = output.content.lines().next().unwrap_or_default();
        return Some((Tone::Failed, first.to_owned()));
    }
    let value: Value = serde_json::from_str(&output.content).unwrap_or(Value::Null);
    let number = |key: &str| value.get(key).and_then(Value::as_u64);
    let fine = |text: String| Some((Tone::Fine, text));
    match call.name.as_str() {
        "todo" | "report_blocker" => None,
        "read" => match (
            number("start_line"),
            number("end_line"),
            number("total_lines"),
        ) {
            (Some(1), Some(end), Some(total)) if end == total => fine(lines(total)),
            (Some(start), Some(end), Some(total)) => {
                fine(format!("lines {start}-{end} of {total}"))
            }
            _ => fine(size(output)),
        },
        "write_file" => {
            let written = text_lines(call, "content");
            let verb = if value.get("created").and_then(Value::as_bool) == Some(true) {
                "created"
            } else {
                "rewritten"
            };
            fine(format!("{verb}, {}", lines(written)))
        }
        "apply_patch" => {
            let changes = patch_preview(call, usize::MAX);
            let added = changes
                .iter()
                .filter(|change| matches!(change, Change::Added(_)))
                .count();
            fine(format!("+{added} −{}", changes.len() - added))
        }
        "search" => {
            number("total_matches").and_then(|matches| fine(count(matches, "match", "matches")))
        }
        "list" => {
            number("total_entries").and_then(|entries| fine(count(entries, "entry", "entries")))
        }
        "progress" => {
            let first = output.content.lines().next().unwrap_or_default();
            let done = first
                .strip_prefix("Recorded. ")
                .and_then(|rest| rest.strip_suffix('.'))
                .unwrap_or(first);
            fine(done.to_owned())
        }
        "cargo" => Some(cargo(call, &value)),
        "exec" | "shell" | "packages" | "github" => Some(process(&value)),
        _ => fine(size(output)),
    }
}

/// One line of a patch preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Removed(String),
    Added(String),
    /// Lines not shown, to keep the preview short.
    More(usize),
}

/// The lines a patch changes, without the lines its old and new text share at
/// either end, at most `limit` of them.
#[must_use]
pub fn patch_preview(call: &ToolCall, limit: usize) -> Vec<Change> {
    let text = |key: &str| {
        call.arguments
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let (old, new) = (text("old_text"), text("new_text"));
    let head = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let tail = old[head..]
        .iter()
        .rev()
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let changes: Vec<Change> = old[head..old.len() - tail]
        .iter()
        .cloned()
        .map(Change::Removed)
        .chain(
            new[head..new.len() - tail]
                .iter()
                .cloned()
                .map(Change::Added),
        )
        .collect();
    if changes.len() <= limit {
        return changes;
    }
    let hidden = changes.len() - limit;
    changes
        .into_iter()
        .take(limit)
        .chain(std::iter::once(Change::More(hidden)))
        .collect()
}

fn cargo(call: &ToolCall, value: &Value) -> (Tone, String) {
    let command = call
        .arguments
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let counts = |key: &str| {
        value
            .get("counts")
            .and_then(|counts| counts.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let exit = value.get("exit_code").and_then(Value::as_i64);
    if value.get("timed_out").and_then(Value::as_bool) == Some(true) {
        return (Tone::Failed, "timed out".to_owned());
    }
    let (errors, warnings) = (counts("errors"), counts("warnings"));
    if errors > 0 {
        return (
            Tone::Failed,
            format!(
                "{}, {}",
                count(errors, "error", "errors"),
                count(warnings, "warning", "warnings")
            ),
        );
    }
    let output = value
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let summary = match command {
        "test" => {
            let (passed, failed) = test_counts(output);
            if failed > 0 || exit.is_some_and(|code| code != 0) {
                return (
                    Tone::Failed,
                    if failed > 0 {
                        format!("{failed} failed, {passed} passed")
                    } else {
                        exit_text(exit)
                    },
                );
            }
            format!("{passed} passed")
        }
        "add" => {
            let added: Vec<&str> = value
                .get("added")
                .and_then(Value::as_array)
                .map(|added| added.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if added.is_empty() {
                exit_text(exit)
            } else {
                added.join(", ")
            }
        }
        _ if exit.is_some_and(|code| code != 0) => {
            return (
                Tone::Failed,
                if command == "fmt_check" {
                    "needs formatting".to_owned()
                } else {
                    exit_text(exit)
                },
            );
        }
        "check" | "build" | "clippy" if warnings > 0 => count(warnings, "warning", "warnings"),
        "check" | "build" | "clippy" | "fmt_check" => "clean".to_owned(),
        "fmt" => "formatted".to_owned(),
        _ => exit_text(exit),
    };
    (Tone::Fine, summary)
}

fn process(value: &Value) -> (Tone, String) {
    if value.get("timed_out").and_then(Value::as_bool) == Some(true) {
        return (Tone::Failed, "timed out".to_owned());
    }
    let exit = value.get("exit_code").and_then(Value::as_i64);
    let tone = if exit == Some(0) {
        Tone::Fine
    } else {
        Tone::Failed
    };
    (tone, exit_text(exit))
}

/// Passed and failed, summed over every `test result:` line in Cargo's output.
fn test_counts(output: &str) -> (u64, u64) {
    let field = |line: &str, label: &str| {
        line.split(';')
            .find_map(|part| part.trim().strip_suffix(label))
            .and_then(|number| number.trim().rsplit(' ').next())
            .and_then(|number| number.parse::<u64>().ok())
            .unwrap_or(0)
    };
    output
        .lines()
        .filter(|line| line.starts_with("test result:"))
        .fold((0, 0), |(passed, failed), line| {
            (
                passed + field(line, "passed"),
                failed + field(line, "failed"),
            )
        })
}

fn exit_text(exit: Option<i64>) -> String {
    match exit {
        Some(0) => "ok".to_owned(),
        Some(code) => format!("exit {code}"),
        None => "killed".to_owned(),
    }
}

fn text_lines(call: &ToolCall, key: &str) -> u64 {
    let text = call
        .arguments
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default();
    u64::try_from(text.lines().count()).unwrap_or(u64::MAX)
}

fn lines(count_of: u64) -> String {
    count(count_of, "line", "lines")
}

fn count(number: u64, one: &str, many: &str) -> String {
    if number == 1 {
        format!("1 {one}")
    } else {
        format!("{number} {many}")
    }
}

fn size(output: &ToolOutput) -> String {
    #[allow(clippy::cast_precision_loss)]
    let bytes = output.content.len() as f64;
    if bytes < 1_024.0 {
        format!("{bytes} bytes")
    } else {
        format!("{:.1} kB", bytes / 1_024.0)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments,
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn gist(name: &str, arguments: Value, content: Value) -> Option<(Tone, String)> {
        summarise(
            &call(name, arguments),
            &ToolOutput::success(content.to_string()),
        )
    }

    #[test]
    fn file_tools_say_how_much_was_read_or_changed() {
        assert_eq!(
            gist(
                "read",
                json!({}),
                json!({"start_line": 1, "end_line": 62, "total_lines": 62})
            ),
            Some((Tone::Fine, "62 lines".into()))
        );
        assert_eq!(
            gist(
                "read",
                json!({}),
                json!({"start_line": 40, "end_line": 80, "total_lines": 300})
            ),
            Some((Tone::Fine, "lines 40-80 of 300".into()))
        );
        assert_eq!(
            gist(
                "apply_patch",
                json!({"old_text": "a\nb", "new_text": "a\nb\nc\nd"}),
                json!({})
            ),
            Some((Tone::Fine, "+2 −0".into()))
        );
        assert_eq!(
            gist(
                "write_file",
                json!({"content": "x\n"}),
                json!({"created": true})
            ),
            Some((Tone::Fine, "created, 1 line".into()))
        );
    }

    #[test]
    fn a_patch_preview_shows_only_the_changed_lines() {
        let patch = call(
            "apply_patch",
            json!({"old_text": "fn a() {\n    1\n}", "new_text": "fn a() {\n    2\n    + 3\n}"}),
        );
        assert_eq!(
            patch_preview(&patch, 10),
            [
                Change::Removed("    1".into()),
                Change::Added("    2".into()),
                Change::Added("    + 3".into())
            ]
        );
        assert_eq!(
            patch_preview(&patch, 2),
            [
                Change::Removed("    1".into()),
                Change::Added("    2".into()),
                Change::More(1)
            ]
        );
    }

    #[test]
    fn cargo_says_what_a_person_would_look_for() {
        let test = json!({"exit_code": 0, "counts": {"errors": 0, "warnings": 0},
            "output": "test result: ok. 4 passed; 0 failed; 0 ignored\ntest result: ok. 3 passed; 0 failed"});
        assert_eq!(
            gist("cargo", json!({"command": "test"}), test),
            Some((Tone::Fine, "7 passed".into()))
        );

        let failing = json!({"exit_code": 101, "counts": {"errors": 0, "warnings": 0},
            "output": "test result: FAILED. 2 passed; 1 failed; 0 ignored"});
        assert_eq!(
            gist("cargo", json!({"command": "test"}), failing),
            Some((Tone::Failed, "1 failed, 2 passed".into()))
        );

        let broken = json!({"exit_code": 101, "counts": {"errors": 2, "warnings": 1}});
        assert_eq!(
            gist("cargo", json!({"command": "check"}), broken),
            Some((Tone::Failed, "2 errors, 1 warning".into()))
        );
        assert_eq!(
            gist(
                "cargo",
                json!({"command": "clippy"}),
                json!({"exit_code": 0, "counts": {"errors": 0, "warnings": 0}})
            ),
            Some((Tone::Fine, "clean".into()))
        );
        assert_eq!(
            gist(
                "cargo",
                json!({"command": "add"}),
                json!({"exit_code": 0, "added": ["axum 0.8.9 (dependencies)"]})
            ),
            Some((Tone::Fine, "axum 0.8.9 (dependencies)".into()))
        );
    }

    #[test]
    fn processes_report_their_exit_and_errors_their_first_line() {
        assert_eq!(
            gist("exec", json!({}), json!({"exit_code": 1})),
            Some((Tone::Failed, "exit 1".into()))
        );
        assert_eq!(
            gist("shell", json!({}), json!({"exit_code": 0})),
            Some((Tone::Fine, "ok".into()))
        );
        let refused = summarise(
            &call("exec", json!({})),
            &ToolOutput::error("refused by approval policy: no\nmore"),
        );
        assert_eq!(
            refused,
            Some((Tone::Failed, "refused by approval policy: no".into()))
        );
    }
}
