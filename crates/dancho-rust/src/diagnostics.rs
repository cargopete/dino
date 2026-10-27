//! Parsing `--message-format=json` into something an agent can act on.

use std::collections::HashSet;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub level: String,
    pub code: Option<String>,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub column: Option<u64>,
    /// The compiler's own rendering. Omitted when the byte budget ran out; the
    /// diagnostic itself is still returned, because losing the location would
    /// be worse than losing the pretty version of it.
    pub rendered: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticCounts {
    pub errors: u32,
    pub warnings: u32,
}

/// Every distinct diagnostic Cargo emitted, and the counts for the whole run.
///
/// The counts describe everything seen, not everything returned, so an agent
/// reading `errors: 0` can trust it even when the list was capped.
///
/// "Distinct" is doing work here. `--all-targets` compiles a library once as a
/// library and again as its own test target, so a single mistake in `src/lib.rs`
/// arrives twice. One mistake is one diagnostic, and counting it twice would
/// make a clean fix look like a half-finished one.
#[derive(Debug, Default)]
pub struct ParsedDiagnostics {
    pub diagnostics: Vec<Diagnostic>,
    pub counts: DiagnosticCounts,
}

/// Everything that identifies one mistake, ignoring which target reported it.
type Identity = (
    String,
    Option<String>,
    Option<String>,
    Option<u64>,
    Option<u64>,
    String,
);

fn identity(diagnostic: &Diagnostic) -> Identity {
    (
        diagnostic.level.clone(),
        diagnostic.code.clone(),
        diagnostic.file.clone(),
        diagnostic.line,
        diagnostic.column,
        diagnostic.message.clone(),
    )
}

pub fn parse(stdout: &str) -> ParsedDiagnostics {
    let mut parsed = ParsedDiagnostics::default();
    let mut seen: HashSet<Identity> = HashSet::new();
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        let level = message
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // Notes and help lines arrive as their own messages and are already
        // present inside the rendered text of the diagnostic they belong to.
        if level != "error" && level != "warning" {
            continue;
        }
        let diagnostic = diagnostic(message, level);
        if !seen.insert(identity(&diagnostic)) {
            continue;
        }
        if level == "error" {
            parsed.counts.errors += 1;
        } else {
            parsed.counts.warnings += 1;
        }
        parsed.diagnostics.push(diagnostic);
    }
    parsed
}

fn diagnostic(message: &Value, level: &str) -> Diagnostic {
    let primary = message
        .get("spans")
        .and_then(Value::as_array)
        .and_then(|spans| {
            spans
                .iter()
                .find(|span| span.get("is_primary").and_then(Value::as_bool) == Some(true))
                .or_else(|| spans.first())
        });
    Diagnostic {
        level: level.to_owned(),
        code: message
            .get("code")
            .and_then(|code| code.get("code"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        message: message
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        file: primary
            .and_then(|span| span.get("file_name"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        line: primary
            .and_then(|span| span.get("line_start"))
            .and_then(Value::as_u64),
        column: primary
            .and_then(|span| span.get("column_start"))
            .and_then(Value::as_u64),
        rendered: message
            .get("rendered")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    }
}

/// Applies the diagnostic and byte caps, keeping errors ahead of warnings.
///
/// Returns how many diagnostics were not returned, so the caller can say so
/// rather than presenting a truncated list as the whole story.
pub fn cap(
    mut diagnostics: Vec<Diagnostic>,
    max_count: usize,
    max_rendered_bytes: usize,
) -> (Vec<Diagnostic>, usize) {
    // A stable sort keeps compiler order within each level, so the first error
    // returned is still the first error the compiler reported.
    diagnostics.sort_by_key(|diagnostic| u8::from(diagnostic.level != "error"));
    let dropped = diagnostics.len().saturating_sub(max_count);
    diagnostics.truncate(max_count);

    let mut spent = 0_usize;
    for diagnostic in &mut diagnostics {
        let Some(rendered) = &diagnostic.rendered else {
            continue;
        };
        if spent + rendered.len() > max_rendered_bytes {
            diagnostic.rendered = None;
        } else {
            spent += rendered.len();
        }
    }
    (diagnostics, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(level: &str, code: &str, rendered: &str) -> String {
        serde_json::json!({
            "reason": "compiler-message",
            "message": {
                "level": level,
                "code": {"code": code},
                "message": "mismatched types",
                "rendered": rendered,
                "spans": [
                    {"file_name": "src/other.rs", "line_start": 1, "column_start": 1,
                     "is_primary": false},
                    {"file_name": "src/lib.rs", "line_start": 42, "column_start": 9,
                     "is_primary": true}
                ]
            }
        })
        .to_string()
    }

    #[test]
    fn the_same_mistake_reported_by_two_targets_counts_once() {
        let one = message("error", "E0308", "error[E0308]: mismatched types");
        let stdout = [one.clone(), one].join("\n");

        let parsed = parse(&stdout);

        assert_eq!(
            parsed.counts,
            DiagnosticCounts {
                errors: 1,
                warnings: 0
            }
        );
        assert_eq!(parsed.diagnostics.len(), 1);
    }

    #[test]
    fn parses_the_primary_span_and_counts_every_level() {
        let stdout = [
            message("warning", "unused_variables", "warning: unused"),
            message("error", "E0308", "error[E0308]: mismatched types"),
            r#"{"reason":"compiler-artifact","target":{}}"#.to_owned(),
            r#"{"reason":"compiler-message","message":{"level":"note","message":"aside"}}"#
                .to_owned(),
            "not json at all".to_owned(),
        ]
        .join("\n");

        let parsed = parse(&stdout);

        assert_eq!(
            parsed.counts,
            DiagnosticCounts {
                errors: 1,
                warnings: 1
            }
        );
        assert_eq!(parsed.diagnostics.len(), 2);
        assert_eq!(parsed.diagnostics[1].code.as_deref(), Some("E0308"));
        assert_eq!(parsed.diagnostics[1].file.as_deref(), Some("src/lib.rs"));
        assert_eq!(parsed.diagnostics[1].line, Some(42));
        assert_eq!(parsed.diagnostics[1].column, Some(9));
    }

    #[test]
    fn capping_keeps_errors_ahead_of_warnings_and_says_what_it_dropped() {
        let stdout = [
            message("warning", "w1", "warning one"),
            message("warning", "w2", "warning two"),
            message("error", "E0001", "the error"),
        ]
        .join("\n");
        let parsed = parse(&stdout);

        let (kept, dropped) = cap(parsed.diagnostics, 2, 1_024);

        assert_eq!(dropped, 1);
        assert_eq!(kept[0].level, "error");
        assert_eq!(kept[1].level, "warning");
        // The counts still describe all three, which is the point of them.
        assert_eq!(
            parsed.counts,
            DiagnosticCounts {
                errors: 1,
                warnings: 2
            }
        );
    }

    #[test]
    fn a_rendered_budget_drops_text_rather_than_the_diagnostic() {
        let stdout = [
            message("error", "E0001", &"x".repeat(64)),
            message("error", "E0002", &"y".repeat(64)),
        ]
        .join("\n");
        let parsed = parse(&stdout);

        let (kept, dropped) = cap(parsed.diagnostics, 10, 70);

        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 2);
        assert!(kept[0].rendered.is_some());
        assert!(kept[1].rendered.is_none());
        assert_eq!(kept[1].code.as_deref(), Some("E0002"));
    }
}
