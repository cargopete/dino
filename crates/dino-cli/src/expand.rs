//! `/expand`: a call's whole result, which the transcript folds to one line.
//!
//! Read back from this session's log, which keeps every result as the tool
//! returned it, so nothing extra is held in memory for a call nobody opens.

use std::fmt::Write as _;
use std::path::Path;

use crate::style;
use crate::style::FAINT;
use crate::style::MUTED;
use crate::style::PURPLE;

/// The call `back` from the latest, counting the latest as 1, in full.
pub fn expand(log: &Path, back: usize) -> String {
    let trajectory = match crate::trace::read(log, usize::MAX) {
        Ok(trajectory) => trajectory,
        Err(error) => return style::paint(style::WARN, &format!("  {error}\n")),
    };
    let calls: Vec<&crate::trace::Call> = trajectory
        .submissions
        .iter()
        .flat_map(|submission| &submission.turns)
        .flat_map(|turn| &turn.calls)
        .collect();
    if calls.is_empty() {
        return style::paint(FAINT, "  no calls yet in this session\n");
    }
    let Some(call) = back
        .checked_sub(1)
        .and_then(|index| calls.len().checked_sub(index + 1))
        .map(|index| calls[index])
    else {
        return style::paint(
            FAINT,
            &format!(
                "  this session has made {} call(s); /expand {} is the first\n",
                calls.len(),
                calls.len()
            ),
        );
    };
    let mut block = format!(
        "  {} {}",
        style::paint(PURPLE, &call.tool),
        style::paint(MUTED, &call.summary)
    );
    if let Some(gist) = &call.gist {
        let _ = write!(block, "{}", style::paint(FAINT, &format!(" · {gist}")));
    }
    block.push('\n');
    if let Some(refusal) = &call.refusal {
        let _ = writeln!(
            block,
            "{}",
            style::paint(style::WARN, &format!("  refused: {refusal}"))
        );
    }
    match call.output.as_deref().map(str::trim_end) {
        Some(output) if !output.is_empty() => {
            let _ = writeln!(block, "{output}");
        }
        _ => block.push_str(&style::paint(FAINT, "  (no output)\n")),
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(event: &serde_json::Value) -> String {
        serde_json::json!({ "record": "event", "seq": 1, "unix_millis": 1, "event": event })
            .to_string()
    }

    fn call(id: &str, path: &str, content: &str) -> [String; 2] {
        [
            record(&serde_json::json!({
                "type": "model", "model_turn": 1,
                "event": { "type": "tool_call_completed",
                    "call": { "id": id, "name": "read", "arguments": { "path": path } } }
            })),
            record(&serde_json::json!({
                "type": "tool_finished", "model_turn": 1,
                "result": { "call_id": id, "output": { "content": content, "is_error": false } }
            })),
        ]
    }

    #[test]
    fn the_latest_call_or_one_further_back_is_shown_whole() {
        let log = std::env::temp_dir().join(format!("dino-expand-{}.jsonl", std::process::id()));
        let big = "line\n".repeat(5_000);
        let lines: Vec<String> = [
            record(&serde_json::json!({ "type": "submitted", "text": "look" })),
            call("a", "src/a.rs", "first file").join("\n"),
            call("b", "src/b.rs", &big).join("\n"),
        ]
        .to_vec();
        std::fs::write(&log, lines.join("\n")).unwrap();

        let latest = expand(&log, 1);
        let earlier = expand(&log, 2);
        let beyond = expand(&log, 3);

        assert!(latest.contains("src/b.rs"), "{latest}");
        assert_eq!(latest.matches("line\n").count(), 5_000, "nothing is cut");
        assert!(earlier.contains("first file"), "{earlier}");
        assert!(beyond.contains("made 2 call(s)"), "{beyond}");
        let _ = std::fs::remove_file(&log);
    }
}
