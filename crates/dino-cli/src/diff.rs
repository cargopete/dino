//! A change as a person reviews it: line numbers, a little context, and the
//! words that changed picked out.

use std::fmt::Write as _;
use std::path::Path;

use dino_protocol::ToolCall;
use serde_json::Value;
use similar::ChangeTag;
use similar::TextDiff;

use crate::style;
use crate::style::FAINT;
use crate::style::OK;
use crate::style::RUST;

/// Unchanged lines kept either side of a change.
const CONTEXT: usize = 2;

/// One line of a diff. A changed line's parts say which of them changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Same {
        number: Option<usize>,
        text: String,
    },
    Removed {
        number: Option<usize>,
        parts: Vec<(bool, String)>,
    },
    Added {
        number: Option<usize>,
        parts: Vec<(bool, String)>,
    },
    /// Unchanged lines left out between two changes.
    Gap,
}

/// What an `apply_patch` or `write_file` call would change. With `root`, the
/// file is read for line numbers and, for `write_file`, for what it replaces.
#[must_use]
pub fn of_call(call: &ToolCall, root: Option<&Path>) -> Option<Vec<Row>> {
    let path = call.arguments.get("path").and_then(Value::as_str);
    let file = root
        .zip(path)
        .and_then(|(root, path)| std::fs::read_to_string(root.join(path)).ok());
    let text = |edit: &Value, key: &str| {
        edit.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    match call.name.as_str() {
        "write_file" => {
            let content = text(&call.arguments, "content");
            let old = match (&file, root) {
                (Some(file), _) => file.clone(),
                (None, Some(_)) => String::new(),
                // Without the file there is nothing to compare against.
                (None, None) => return None,
            };
            Some(rows(&old, &content, Some(1)))
        }
        "apply_patch" => {
            let edits: Vec<&Value> = match call.arguments.get("edits").and_then(Value::as_array) {
                Some(edits) => edits.iter().collect(),
                None => vec![&call.arguments],
            };
            let mut all = Vec::new();
            for edit in edits {
                let (old, new) = (text(edit, "old_text"), text(edit, "new_text"));
                let edit_rows = match file.as_deref().and_then(|file| around(file, &old)) {
                    Some(Around {
                        before,
                        after,
                        first,
                    }) => rows(
                        &format!("{before}{old}{after}"),
                        &format!("{before}{new}{after}"),
                        Some(first),
                    ),
                    None => rows(&old, &new, None),
                };
                if edit_rows.is_empty() {
                    continue;
                }
                if !all.is_empty() {
                    all.push(Row::Gap);
                }
                all.extend(edit_rows);
            }
            Some(all)
        }
        _ => None,
    }
}

/// Where an edit's old text sits in its file, with the lines either side.
#[derive(Debug, PartialEq, Eq)]
struct Around<'a> {
    before: &'a str,
    after: &'a str,
    /// The line `before` starts on.
    first: usize,
}

/// The [`CONTEXT`] lines around `text` in `file`, and the rest of the lines it
/// starts and ends inside, when it is there exactly once.
fn around<'a>(file: &'a str, text: &str) -> Option<Around<'a>> {
    if text.is_empty() {
        return None;
    }
    let at = file.find(text)?;
    if file[at + 1..].contains(text) {
        return None;
    }
    let mut start = file[..at].rfind('\n').map_or(0, |newline| newline + 1);
    for _ in 0..CONTEXT {
        if start == 0 {
            break;
        }
        start = file[..start - 1]
            .rfind('\n')
            .map_or(0, |newline| newline + 1);
    }
    let mut end = at + text.len();
    if !text.ends_with('\n') {
        end = file[end..]
            .find('\n')
            .map_or(file.len(), |newline| end + newline + 1);
    }
    for _ in 0..CONTEXT {
        end = file[end..]
            .find('\n')
            .map_or(file.len(), |newline| end + newline + 1);
    }
    Some(Around {
        before: &file[start..at],
        after: &file[at + text.len()..end],
        first: file[..start].matches('\n').count() + 1,
    })
}

/// The rows from `old` to `new`, numbered from `first` when the old text's
/// place in its file is known.
#[must_use]
pub fn rows(old: &str, new: &str, first: Option<usize>) -> Vec<Row> {
    // Else a last line without its newline differs from the same line with one.
    let ended = |text: &str| {
        if text.is_empty() || text.ends_with('\n') {
            text.to_owned()
        } else {
            format!("{text}\n")
        }
    };
    let (old, new) = (ended(old), ended(new));
    let diff = TextDiff::from_lines(&old, &new);
    let number = |index: Option<usize>| first.zip(index).map(|(first, index)| first + index);
    let mut rows = Vec::new();
    for (group, ops) in diff.grouped_ops(CONTEXT).iter().enumerate() {
        if group > 0 {
            rows.push(Row::Gap);
        }
        for op in ops {
            for change in diff.iter_inline_changes(op) {
                let mut parts: Vec<(bool, String)> = change
                    .iter_strings_lossy()
                    .map(|(changed, text)| (changed, text.trim_end_matches('\n').to_owned()))
                    .filter(|(_, text)| !text.is_empty())
                    .collect();
                // A line changed throughout is told by its sign alone.
                if parts
                    .iter()
                    .all(|(changed, text)| *changed || text.trim().is_empty())
                {
                    for part in &mut parts {
                        part.0 = false;
                    }
                }
                rows.push(match change.tag() {
                    ChangeTag::Equal => Row::Same {
                        number: number(change.old_index()),
                        text: parts.into_iter().map(|(_, text)| text).collect(),
                    },
                    ChangeTag::Delete => Row::Removed {
                        number: number(change.old_index()),
                        parts,
                    },
                    ChangeTag::Insert => Row::Added {
                        number: number(change.new_index()),
                        parts,
                    },
                });
            }
        }
    }
    rows
}

/// At most `limit` rows, indented by `indent` and cut to `width` columns.
#[must_use]
pub fn paint(rows: &[Row], limit: usize, indent: &str, width: usize) -> String {
    let shown = crate::calls::shown(rows.len(), limit);
    let digits = rows
        .iter()
        .filter_map(|row| match row {
            Row::Same { number, .. } | Row::Removed { number, .. } | Row::Added { number, .. } => {
                *number
            }
            Row::Gap => None,
        })
        .max()
        .map_or(0, |widest| widest.to_string().len());
    let gutter = |number: Option<usize>| match (digits, number) {
        (0, _) => String::new(),
        (_, Some(number)) => format!("{number:>digits$} "),
        (_, None) => " ".repeat(digits + 1),
    };
    let room = width.saturating_sub(indent.len() + digits + 3);
    let mut block = String::new();
    for row in &rows[..shown] {
        let line = match row {
            Row::Same { number, text } => format!(
                "{}{}",
                style::paint(FAINT, &gutter(*number)),
                style::paint(FAINT, &format!("  {}", cut(&expand(text), room)))
            ),
            Row::Removed { number, parts } => format!(
                "{}{}",
                style::paint(FAINT, &gutter(*number)),
                changed(parts, RUST, false, room)
            ),
            Row::Added { number, parts } => format!(
                "{}{}",
                style::paint(FAINT, &gutter(*number)),
                changed(parts, OK, true, room)
            ),
            Row::Gap => style::paint(FAINT, &format!("{}  ⋮", " ".repeat(digits))),
        };
        let _ = writeln!(block, "{indent}{line}");
    }
    if shown < rows.len() {
        let more = rows.len() - shown;
        let _ = writeln!(
            block,
            "{indent}{}",
            style::paint(FAINT, &format!("… {more} more lines"))
        );
    }
    block
}

/// A removed or added line, its changed words picked out, cut to `room`.
fn changed(parts: &[(bool, String)], colour: style::Colour, added: bool, room: usize) -> String {
    let sign = if added { "+ " } else { "- " };
    let mut line = style::paint(colour, sign);
    let mut left = room;
    for (emphasis, text) in parts {
        if left == 0 {
            break;
        }
        let text = cut(&expand(text), left);
        left = left.saturating_sub(text.chars().count());
        line.push_str(&if *emphasis {
            style::paint_changed(colour, added, &text)
        } else {
            style::paint(colour, &text)
        });
    }
    line
}

/// Tabs as spaces, since a terminal's tab stops would push the line past the
/// width it was cut to.
fn expand(text: &str) -> String {
    text.replace('\t', "    ")
}

fn cut(text: &str, room: usize) -> String {
    crate::calls::cut(text, room)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Lines `{prefix}1` to `{prefix}{count}`.
    fn numbered(count: usize, prefix: &str) -> String {
        (1..=count).fold(String::new(), |mut text, n| {
            let _ = writeln!(text, "{prefix}{n}");
            text
        })
    }

    fn unchanged(text: &str) -> Vec<(bool, String)> {
        vec![(false, text.to_owned())]
    }

    #[test]
    fn a_changed_line_picks_out_the_words_that_changed() {
        let rows = rows("let x = 1;\n", "let x = 2;\n", Some(7));
        assert_eq!(rows.len(), 2);
        let Row::Removed { number, parts } = &rows[0] else {
            panic!("{rows:?}");
        };
        assert_eq!(*number, Some(7));
        assert!(parts.contains(&(true, "1;".to_owned())), "{parts:?}");
        assert!(parts.contains(&(false, "let x = ".to_owned())), "{parts:?}");
        assert!(matches!(
            &rows[1],
            Row::Added {
                number: Some(7),
                ..
            }
        ));
    }

    #[test]
    fn context_is_kept_near_a_change_and_cut_between_two() {
        let old = numbered(12, "line ");
        let new = old
            .replace("line 2\n", "line two\n")
            .replace("line 11\n", "line eleven\n");
        let rows = rows(&old, &new, Some(1));
        assert!(rows.contains(&Row::Gap), "{rows:?}");
        assert_eq!(
            rows[0],
            Row::Same {
                number: Some(1),
                text: "line 1".into()
            }
        );
        // Two above and two below each change, and the gap between them.
        assert_eq!(rows.len(), 3 + 2 + 1 + 2 + 2 + 1);
    }

    #[test]
    fn a_wholly_new_line_is_not_picked_out() {
        let rows = rows("", "fn main() {}\n", Some(1));
        assert_eq!(
            rows,
            [Row::Added {
                number: Some(1),
                parts: unchanged("fn main() {}")
            }]
        );
    }

    #[test]
    fn a_patch_takes_its_numbers_and_context_from_the_file() {
        let file = numbered(9, "line ");
        assert_eq!(
            around(&file, "ne 5\nline 6"),
            Some(Around {
                before: "line 3\nline 4\nli",
                after: "\nline 7\nline 8\n",
                first: 3,
            })
        );
        assert_eq!(
            around(&file, "line 1\n"),
            Some(Around {
                before: "",
                after: "line 2\nline 3\n",
                first: 1,
            })
        );
        assert_eq!(around("a\nb\nb\n", "b"), None, "ambiguous");
        assert_eq!(around("a\n", "z"), None);
    }

    #[test]
    fn edits_are_set_apart_and_an_empty_one_is_left_out() {
        let call = ToolCall {
            id: "c".into(),
            name: "apply_patch".into(),
            arguments: json!({"path": "x", "edits": [
                {"old_text": "a", "new_text": "b"},
                {"old_text": "same", "new_text": "same"},
                {"old_text": "c", "new_text": "d"}
            ]}),
        };
        let rows = of_call(&call, None).unwrap();
        assert_eq!(rows.len(), 5, "two edits and the gap between: {rows:?}");
        assert_eq!(rows[2], Row::Gap);
    }

    #[test]
    fn a_long_diff_is_collapsed_and_every_line_fits() {
        let new = numbered(30, &"x".repeat(200));
        let painted = visible(&paint(&rows("", &new, Some(1)), 10, "  ", 80));
        let lines: Vec<&str> = painted.lines().collect();
        assert_eq!(lines.len(), 11);
        assert!(lines[10].contains("20 more lines"), "{painted}");
        assert!(lines[0].starts_with("   1 + xxx"), "{painted}");
        assert!(lines[0].ends_with('…'), "{painted}");
        assert!(
            lines.iter().all(|line| line.chars().count() <= 80),
            "{painted}"
        );
    }

    /// Another test may have turned colour on for the whole process.
    fn visible(painted: &str) -> String {
        let mut out = String::new();
        let mut chars = painted.chars();
        while let Some(character) = chars.next() {
            if character == '\u{1b}' {
                chars.by_ref().find(|next| *next == 'm');
            } else {
                out.push(character);
            }
        }
        out
    }
}
