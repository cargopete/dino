//! How a call reads on its one line: an icon for its kind, a mark for how it
//! went, and a run of reads, lists and searches folded into a single line.
//!
//! Everything here is text without colour, cut to a width, so the renderer
//! can paint it and a test can read it.

use std::fmt::Write as _;

use dino_protocol::ToolCall;
use serde_json::Value;

/// The kind of a call, at a glance.
#[must_use]
pub fn icon(tool: &str) -> char {
    match tool {
        "read" | "list" => '→',
        "search" => '✱',
        "apply_patch" | "write_file" => '←',
        "exec" | "shell" | "gate" | "cargo" | "background" | "packages" | "github" => '$',
        "web_fetch" => '%',
        "todo" => '#',
        _ => '•',
    }
}

/// How a call went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Running,
    Done,
    Failed,
    Refused,
}

impl Mark {
    #[must_use]
    pub fn glyph(self) -> char {
        match self {
            Self::Running => '●',
            Self::Done => '✓',
            Self::Failed | Self::Refused => '×',
        }
    }
}

/// Whether a call only looks, so that a run of them can share a line.
#[must_use]
pub fn explores(tool: &str) -> bool {
    matches!(tool, "read" | "list" | "search")
}

/// One look in a folded run: what it did, at what, and whether it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Look {
    pub verb: &'static str,
    pub target: String,
    pub failed: bool,
}

impl Look {
    #[must_use]
    pub fn of(call: &ToolCall, failed: bool) -> Self {
        let argument = |key: &str| call.arguments.get(key).and_then(Value::as_str);
        let (verb, target) = match call.name.as_str() {
            "search" => ("search", format!("{:?}", argument("query").unwrap_or(""))),
            "list" => ("list", argument("path").unwrap_or(".").to_owned()),
            _ => ("read", argument("path").unwrap_or("?").to_owned()),
        };
        Self {
            verb,
            target,
            failed,
        }
    }
}

/// A run of looks as one line, `explored · read a, b · search "x"`, with
/// consecutive looks of one kind sharing their verb. What does not fit in
/// `room` columns is counted at the end instead.
#[must_use]
pub fn explored(looks: &[Look], room: usize) -> String {
    let line = |shown: usize| {
        let mut text = String::from("explored");
        let mut verb = "";
        for look in &looks[..shown] {
            if look.verb == verb {
                text.push_str(", ");
            } else {
                verb = look.verb;
                text.push_str(" · ");
                text.push_str(verb);
                text.push(' ');
            }
            text.push_str(&look.target);
            if look.failed {
                text.push_str(" ×");
            }
        }
        if shown < looks.len() {
            let _ = write!(text, " · +{} more", looks.len() - shown);
        }
        text
    };
    (1..=looks.len())
        .rev()
        .map(line)
        .find(|text| width(text) <= room)
        .unwrap_or_else(|| cut(&line(1), room))
}

/// A call's description and its result's gist cut to share `room` columns,
/// the description first, since the gist is what a person scans for.
#[must_use]
pub fn fit(description: &str, gist: &str, room: usize) -> (String, String) {
    let separator = width(" · ");
    let gist_width = if gist.is_empty() {
        0
    } else {
        width(gist) + separator
    };
    let description = cut(
        description,
        room.saturating_sub(gist_width).max(room.min(24)),
    );
    let left = room.saturating_sub(width(&description) + separator);
    let gist = if left < 2 {
        String::new()
    } else {
        cut(gist, left)
    };
    (description, gist)
}

/// At most `room` columns of `text`, ending in `…` when it was cut.
#[must_use]
pub fn cut(text: &str, room: usize) -> String {
    if width(text) <= room {
        return text.to_owned();
    }
    let mut kept: String = text.chars().take(room.saturating_sub(1)).collect();
    if room > 0 {
        kept.push('…');
    }
    kept
}

/// Columns, counting a character as one: wide glyphs are rare in a call line.
#[must_use]
pub fn width(text: &str) -> usize {
    text.chars().count()
}

/// The lines to show of `total`: all of them, or `limit` when more than one
/// would be hidden, since a line saying one more is hidden costs that line.
#[must_use]
pub fn shown(total: usize, limit: usize) -> usize {
    if total <= limit.saturating_add(1) {
        total
    } else {
        limit
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

    #[test]
    fn each_kind_of_call_has_its_icon() {
        assert_eq!(icon("read"), '→');
        assert_eq!(icon("search"), '✱');
        assert_eq!(icon("apply_patch"), '←');
        assert_eq!(icon("cargo"), '$');
        assert_eq!(icon("web_fetch"), '%');
        assert_eq!(icon("mcp__thing"), '•');
        assert_eq!(Mark::Running.glyph(), '●');
        assert_eq!(Mark::Done.glyph(), '✓');
        assert_eq!(Mark::Failed.glyph(), '×');
    }

    #[test]
    fn a_run_of_looks_shares_one_line_and_its_verbs() {
        let looks = [
            Look::of(&call("read", json!({"path": "a.rs"})), false),
            Look::of(&call("read", json!({"path": "b.rs"})), false),
            Look::of(&call("search", json!({"query": "fn main"})), false),
            Look::of(&call("list", json!({})), false),
            Look::of(&call("read", json!({"path": "c.rs"})), true),
        ];
        assert_eq!(
            explored(&looks, 200),
            "explored · read a.rs, b.rs · search \"fn main\" · list . · read c.rs ×"
        );
    }

    #[test]
    fn looks_that_do_not_fit_are_counted() {
        let looks: Vec<Look> = (0..6)
            .map(|index| {
                Look::of(
                    &call("read", json!({"path": format!("file{index}.rs")})),
                    false,
                )
            })
            .collect();
        let line = explored(&looks, 50);
        assert_eq!(line, "explored · read file0.rs, file1.rs · +4 more");
        assert!(width(&line) <= 50);
        assert!(width(&explored(&looks, 10)) <= 10);
    }

    #[test]
    fn the_description_gives_way_before_the_gist() {
        assert_eq!(
            fit("read src/main.rs", "62 lines", 40),
            ("read src/main.rs".into(), "62 lines".into())
        );
        let (description, gist) = fit(&"x".repeat(100), "7 passed", 40);
        assert_eq!(gist, "7 passed");
        assert_eq!(width(&description) + 3 + width(&gist), 40);
        assert!(description.ends_with('…'));
        // Too narrow for both: the description keeps a readable stub.
        let (description, gist) = fit(&"x".repeat(100), &"y".repeat(100), 30);
        assert!(width(&description) + 3 + width(&gist) <= 30);
        assert_eq!(width(&description), 24);
    }

    #[test]
    fn hiding_a_single_line_is_not_worth_the_line_that_says_so() {
        assert_eq!(shown(10, 10), 10);
        assert_eq!(shown(11, 10), 11);
        assert_eq!(shown(12, 10), 10);
        assert_eq!(shown(3, 10), 3);
    }
}
