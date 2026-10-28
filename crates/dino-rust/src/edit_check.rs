//! `cargo check` after a turn's Rust edits, with the errors in the edited files
//! added to the last edit's result, so the model sees a broken build at once
//! instead of at its next check.

use std::collections::HashSet;
use std::sync::Mutex;

use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use serde_json::Value;
use serde_json::json;

use crate::CargoTool;

const EDITS: [&str; 2] = ["write_file", "apply_patch"];
/// Errors from the edited files shown, at most. The counts cover the rest.
const SHOWN_ERRORS: usize = 20;

/// Wraps the file tools.
pub struct EditCheck {
    inner: Box<dyn ToolRuntime>,
    cargo: CargoTool,
    turn: Mutex<Turn>,
}

#[derive(Default)]
struct Turn {
    /// Rust edits announced for this model turn and not yet run.
    left: usize,
    /// Paths of this turn's Rust edits that succeeded.
    edited: HashSet<String>,
}

impl EditCheck {
    #[must_use]
    pub fn new(inner: Box<dyn ToolRuntime>, cargo: CargoTool) -> Self {
        Self {
            inner,
            cargo,
            turn: Mutex::new(Turn::default()),
        }
    }

    /// Counts one Rust edit as run, and returns the turn's edited paths when it
    /// was the last.
    fn settle(&self, path: &str, succeeded: bool) -> Option<HashSet<String>> {
        let mut turn = self.turn.lock().ok()?;
        turn.left = turn.left.saturating_sub(1);
        if succeeded {
            turn.edited.insert(path.trim_start_matches("./").to_owned());
        }
        (turn.left == 0).then(|| std::mem::take(&mut turn.edited))
    }

    async fn report(&self, edited: &HashSet<String>) -> Option<Value> {
        let parsed = self.cargo.parsed_check().await.ok()?;
        let errors: Vec<&crate::Diagnostic> = parsed
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.level == "error")
            .collect();
        let shown: Vec<Value> = errors
            .iter()
            .filter(|diagnostic| {
                diagnostic
                    .file
                    .as_deref()
                    .is_some_and(|file| edited.contains(file))
            })
            .take(SHOWN_ERRORS)
            .map(|diagnostic| {
                json!({
                    "file": diagnostic.file,
                    "line": diagnostic.line,
                    "code": diagnostic.code,
                    "message": diagnostic.message,
                })
            })
            .collect();
        Some(json!({
            "errors": parsed.counts.errors,
            "warnings": parsed.counts.warnings,
            "in_edited_files": shown,
        }))
    }
}

fn rust_path(call: &ToolCall) -> Option<&str> {
    if !EDITS.contains(&call.name.as_str()) {
        return None;
    }
    let path = call.arguments.get("path")?.as_str()?;
    std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
        .then_some(path)
}

impl ToolRuntime for EditCheck {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        self.inner.classify(call)
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let call = call.clone();
        Box::pin(async move {
            let output = self.inner.execute(&call).await?;
            let Some(path) = rust_path(&call) else {
                return Ok(output);
            };
            let Some(edited) = self.settle(path, !output.is_error) else {
                return Ok(output);
            };
            if edited.is_empty() {
                return Ok(output);
            }
            let Some(report) = self.report(&edited).await else {
                return Ok(output);
            };
            let content = match serde_json::from_str::<Value>(&output.content) {
                Ok(Value::Object(mut fields)) => {
                    fields.insert("cargo_check".into(), report);
                    Value::Object(fields).to_string()
                }
                _ => format!("{}\n\ncargo_check: {report}", output.content),
            };
            Ok(ToolOutput {
                content,
                is_error: output.is_error,
            })
        })
    }

    fn history_lost(&self) {
        self.inner.history_lost();
    }

    fn begin_turn(&self, calls: &[ToolCall]) {
        if let Ok(mut turn) = self.turn.lock() {
            *turn = Turn {
                left: calls
                    .iter()
                    .filter(|call| rust_path(call).is_some())
                    .count(),
                edited: HashSet::new(),
            };
        }
        self.inner.begin_turn(calls);
    }
}
