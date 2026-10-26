//! The model's Markdown, drawn one line at a time.
//!
//! Only what reads better drawn than shown: inline code, bold, headings,
//! list markers, quotes and code blocks. Anything else is left as written. With
//! colour off the text passes through untouched, because whatever reads a pipe
//! may want the Markdown itself.

use crate::style;
use crate::style::BOLD;
use crate::style::FAINT;
use crate::style::MUTED;
use crate::style::RUST;
use crate::style::SLATE;
use crate::style::TEXT;

#[derive(Debug, Default)]
pub struct Markdown {
    in_fence: bool,
    /// Table rows held until the table ends, because a column's width is not
    /// known until its last row has arrived.
    table: Vec<String>,
}

impl Markdown {
    /// Starts a fresh message, so a fence left open by the last one cannot
    /// colour this one.
    pub fn reset(&mut self) {
        self.in_fence = false;
        self.table.clear();
    }

    /// What one line of input becomes: nothing yet while a table is being
    /// held, a whole table when the line after it arrives, or the line drawn.
    pub fn lines(&mut self, line: &str) -> Vec<String> {
        if !style::enabled() {
            return vec![line.to_owned()];
        }
        if !self.in_fence && line.trim_start().starts_with('|') {
            self.table.push(line.to_owned());
            return Vec::new();
        }
        let mut out = self.flush();
        out.push(self.line(line));
        out
    }

    /// A table still held, drawn, for when the text ends with it.
    pub fn flush(&mut self) -> Vec<String> {
        if self.table.is_empty() {
            return Vec::new();
        }
        let rows = std::mem::take(&mut self.table);
        table(&rows, terminal_width())
    }

    /// One line, without its newline, ready to print.
    fn line(&mut self, line: &str) -> String {
        if line.trim_start().starts_with("```") {
            self.in_fence = !self.in_fence;
            let language = line.trim_start().trim_start_matches('`').trim();
            return if self.in_fence && !language.is_empty() {
                style::paint(FAINT, &format!("  ╭ {language}"))
            } else if self.in_fence {
                style::paint(FAINT, "  ╭")
            } else {
                style::paint(FAINT, "  ╰")
            };
        }
        if self.in_fence {
            return format!(
                "{}{}",
                style::paint(FAINT, "  │ "),
                style::paint(SLATE, line)
            );
        }
        let indent = &line[..line.len() - line.trim_start().len()];
        let body = line.trim_start();
        if let Some(heading) = heading(body) {
            return style::paint_with(TEXT, &[BOLD], heading);
        }
        if let Some(rest) = body.strip_prefix("> ") {
            return format!("{}{}", style::paint(FAINT, "│ "), inline(rest, MUTED));
        }
        if let Some(rest) = ["- ", "* ", "+ "]
            .iter()
            .find_map(|marker| body.strip_prefix(marker))
        {
            return format!("{indent}{} {}", style::paint(RUST, "•"), inline(rest, TEXT));
        }
        if let Some((number, rest)) = numbered(body) {
            return format!(
                "{indent}{} {}",
                style::paint(RUST, number),
                inline(rest, TEXT)
            );
        }
        format!("{indent}{}", inline(body, TEXT))
    }
}

fn terminal_width() -> usize {
    terminal_size::terminal_size().map_or(100, |(width, _)| usize::from(width.0))
}

/// A table's cells, with its `|---|` separator rows dropped.
fn cells(rows: &[String]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            let row = row.trim();
            let row = row.strip_prefix('|').unwrap_or(row);
            let row = row.strip_suffix('|').unwrap_or(row);
            row.split('|')
                .map(|cell| cell.trim().to_owned())
                .collect::<Vec<_>>()
        })
        .filter(|cells| {
            !cells.iter().all(|cell| {
                !cell.is_empty() && cell.chars().all(|character| matches!(character, '-' | ':'))
            })
        })
        .collect()
}

/// How wide a cell reads once its markup is drawn.
fn visible(cell: &str) -> usize {
    cell.replace("**", "").replace('`', "").chars().count()
}

/// Words of `text` in lines of at most `width` visible characters.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in text.split_whitespace() {
        let current = lines.last_mut().expect("never empty");
        if !current.is_empty() && visible(current) + 1 + visible(word) > width {
            lines.push(word.to_owned());
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    lines
}

/// A table laid out for a terminal `width` columns wide: aligned when it fits,
/// its last column wrapped in place when that is enough, and otherwise one
/// block per row.
fn table(rows: &[String], width: usize) -> Vec<String> {
    const GAP: usize = 2;
    let rows = cells(rows);
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return Vec::new();
    }
    let cell = |row: &Vec<String>, column: usize| row.get(column).cloned().unwrap_or_default();
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .map(|row| visible(&cell(row, column)))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let pad = |text: &str, width: usize| " ".repeat(width.saturating_sub(visible(text)));
    let total: usize = widths.iter().sum::<usize>() + GAP * (columns - 1);
    let lead: usize = widths[..columns - 1].iter().sum::<usize>() + GAP * (columns - 1);
    let last = if total <= width {
        widths[columns - 1]
    } else {
        width.saturating_sub(lead)
    };
    if total > width && columns > 1 && last < 24 {
        return stacked(&rows, columns);
    }

    let mut out = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let draw = |text: &str| {
            if index == 0 {
                style::paint_with(TEXT, &[BOLD], &text.replace("**", "").replace('`', ""))
            } else {
                inline(text, TEXT)
            }
        };
        let mut prefix = String::new();
        for (column, &column_width) in widths.iter().enumerate().take(columns - 1) {
            let text = cell(row, column);
            prefix.push_str(&draw(&text));
            prefix.push_str(&pad(&text, column_width + GAP));
        }
        for (line, piece) in wrap(&cell(row, columns - 1), last).iter().enumerate() {
            let start = if line == 0 {
                prefix.clone()
            } else {
                " ".repeat(lead)
            };
            out.push(format!("{start}{}", draw(piece)));
        }
        if index == 0 {
            out.push(style::paint(FAINT, &"─".repeat(total.min(width))));
        }
    }
    out
}

/// One block per row, for a table too wide to lay out: the first cell as a
/// heading, then each other cell under its column's name.
fn stacked(rows: &[Vec<String>], columns: usize) -> Vec<String> {
    let Some((header, body)) = rows.split_first() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for row in body {
        out.push(style::paint_with(
            TEXT,
            &[BOLD],
            &row.first().cloned().unwrap_or_default(),
        ));
        for column in 1..columns {
            let name = header.get(column).cloned().unwrap_or_default();
            let value = row.get(column).cloned().unwrap_or_default();
            out.push(format!(
                "  {} {}",
                style::paint(FAINT, &format!("{name}:")),
                inline(&value, TEXT)
            ));
        }
    }
    out
}

fn heading(body: &str) -> Option<&str> {
    let hashes = body
        .chars()
        .take_while(|&character| character == '#')
        .count();
    (1..=6)
        .contains(&hashes)
        .then(|| body[hashes..].strip_prefix(' '))
        .flatten()
}

/// `1. text` as ("1.", "text").
fn numbered(body: &str) -> Option<(&str, &str)> {
    let digits = body.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = &body[digits..];
    let rest = rest
        .strip_prefix(". ")
        .or_else(|| rest.strip_prefix(") "))?;
    Some((&body[..=digits], rest))
}

/// Inline code in the machine's colour and bold as bold, the rest in `base`.
fn inline(text: &str, base: style::Colour) -> String {
    let mut out = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let code = rest.find('`');
        let bold = rest.find("**");
        let next = match (code, bold) {
            (Some(code), Some(bold)) => Some(code.min(bold)),
            (one, other) => one.or(other),
        };
        let Some(start) = next else {
            out.push_str(&style::paint(base, rest));
            break;
        };
        let marker = if Some(start) == code { "`" } else { "**" };
        let after = &rest[start + marker.len()..];
        let Some(end) = after.find(marker) else {
            out.push_str(&style::paint(base, rest));
            break;
        };
        out.push_str(&style::paint(base, &rest[..start]));
        let inner = &after[..end];
        out.push_str(&if marker == "`" {
            style::paint(SLATE, inner)
        } else {
            style::paint_with(base, &[BOLD], inner)
        });
        rest = &after[end + marker.len()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text a reader sees, escape sequences removed.
    fn visible_text(painted: &str) -> String {
        let mut out = String::new();
        let mut chars = painted.chars();
        while let Some(character) = chars.next() {
            if character == '\u{1b}' {
                for next in chars.by_ref() {
                    if next == 'm' {
                        break;
                    }
                }
            } else {
                out.push(character);
            }
        }
        out
    }

    #[test]
    fn markup_is_drawn_rather_than_shown() {
        style::force_colour_for_tests();
        let mut markdown = Markdown::default();
        assert_eq!(
            visible_text(&markdown.line("Added `fib(n)` and **one** test.")),
            "Added fib(n) and one test."
        );
        assert_eq!(visible_text(&markdown.line("## Changes")), "Changes");
        assert_eq!(
            visible_text(&markdown.line("  - nested item")),
            "  • nested item"
        );
        assert_eq!(visible_text(&markdown.line("2. second")), "2. second");
        assert_eq!(
            visible_text(&markdown.line("an unclosed `tick stays")),
            "an unclosed `tick stays"
        );
    }

    #[test]
    fn a_table_is_held_until_it_ends_then_aligned() {
        style::force_colour_for_tests();
        let mut markdown = Markdown::default();
        let mut out = Vec::new();
        for line in [
            "| Slice | Status |",
            "|---|---|",
            "| S0 | Done |",
            "| S10 | `ok` here |",
            "after",
        ] {
            out.extend(markdown.lines(line).iter().map(|line| visible_text(line)));
        }
        assert_eq!(
            out,
            [
                "Slice  Status",
                "──────────────",
                "S0     Done",
                "S10    ok here",
                "after"
            ]
        );
    }

    #[test]
    fn a_wide_table_wraps_its_last_column_in_place() {
        style::force_colour_for_tests();
        let rows = [
            "| S | What |".to_owned(),
            "|---|---|".to_owned(),
            format!("| S1 | {} |", "word ".repeat(12).trim()),
        ];
        let drawn: Vec<String> = table(&rows, 34)
            .iter()
            .map(|line| visible_text(line))
            .collect();
        assert_eq!(drawn[2], "S1  word word word word word word");
        assert_eq!(drawn[3], "    word word word word word word");
    }

    #[test]
    fn code_blocks_get_a_gutter_and_their_markup_is_left_alone() {
        style::force_colour_for_tests();
        let mut markdown = Markdown::default();
        let lines: Vec<String> = ["```rust", "let x = `y`;", "```", "after"]
            .iter()
            .map(|line| visible_text(&markdown.line(line)))
            .collect();
        assert_eq!(lines, ["  ╭ rust", "  │ let x = `y`;", "  ╰", "after"]);
    }
}
