//! Running one process outside the sandbox, when the person approves it.

use dino_core::ToolError;
use serde_json::Value;
use serde_json::json;

/// What the model is told about asking, in a tool description.
pub(crate) const DESCRIPTION: &str = " When a command fails only because of the \
     sandbox (no network, no credentials, no writes outside the workspace) and the task \
     needs it, as `gh` or `git push` do, send it again with `unconfined` true and a \
     one-line `justification`. The person approves each such command; never ask for it \
     to build or test.";

/// The schema properties for asking, added to a process tool's own.
pub(crate) fn properties() -> Value {
    json!({
        "unconfined": {
            "type": "boolean",
            "description": "Run outside the sandbox. The person is asked every time."
        },
        "justification": {
            "type": "string",
            "minLength": 1,
            "description": "Why this command needs to run unconfined, shown to the person."
        }
    })
}

/// The reason for an unconfined run, `None` for a sandboxed one.
///
/// # Errors
///
/// Returns [`ToolError`] when the call asks without a reason, or asks in a
/// session that does not offer it.
pub(crate) fn requested(
    allowed: bool,
    unconfined: bool,
    justification: Option<&str>,
) -> Result<Option<String>, ToolError> {
    if !unconfined {
        return Ok(None);
    }
    if !allowed {
        return Err(ToolError::new(
            "this session cannot run a command outside the sandbox",
        ));
    }
    match justification.map(str::trim) {
        Some(reason) if !reason.is_empty() => Ok(Some(reason.to_owned())),
        _ => Err(ToolError::new(
            "an unconfined command needs a justification for the person",
        )),
    }
}
