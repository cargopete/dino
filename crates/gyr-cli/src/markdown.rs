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
}

impl Markdown {
    /// Starts a fresh message, so a fence left open by the last one cannot
    /// colour this one.
    pub fn reset(&mut self) {
        self.in_fence = false;
    }

    /// One line, without its newline, ready to print.
    pub fn line(&mut self, line: &str) -> String {
        if !style::enabled() {
            return line.to_owned();
        }
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
    fn visible(painted: &str) -> String {
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
            visible(&markdown.line("Added `fib(n)` and **one** test.")),
            "Added fib(n) and one test."
        );
        assert_eq!(visible(&markdown.line("## Changes")), "Changes");
        assert_eq!(
            visible(&markdown.line("  - nested item")),
            "  • nested item"
        );
        assert_eq!(visible(&markdown.line("2. second")), "2. second");
        assert_eq!(
            visible(&markdown.line("an unclosed `tick stays")),
            "an unclosed `tick stays"
        );
    }

    #[test]
    fn code_blocks_get_a_gutter_and_their_markup_is_left_alone() {
        style::force_colour_for_tests();
        let mut markdown = Markdown::default();
        let lines: Vec<String> = ["```rust", "let x = `y`;", "```", "after"]
            .iter()
            .map(|line| visible(&markdown.line(line)))
            .collect();
        assert_eq!(lines, ["  ╭ rust", "  │ let x = `y`;", "  ╰", "after"]);
    }
}
