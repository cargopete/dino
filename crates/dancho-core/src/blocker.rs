//! A way for the model to stop and say the task cannot be done as asked.
//!
//! Without one, a model facing a spec and tests that disagree can bend one to
//! fit the other or flail until the turn limit. The agent recognises a
//! successful call by name and ends the run with [`StopReason::Blocked`].
//!
//! [`StopReason::Blocked`]: dancho_protocol::StopReason::Blocked

use dancho_protocol::ToolAction;
use dancho_protocol::ToolCall;
use dancho_protocol::ToolDefinition;
use dancho_protocol::ToolOutput;
use serde::Deserialize;
use serde_json::json;

use crate::ToolError;
use crate::ToolFuture;
use crate::ToolRuntime;

pub const REPORT_BLOCKER: &str = "report_blocker";

#[derive(Debug, Clone, Copy, Default)]
pub struct BlockerTool;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockerArguments {
    reason: String,
    #[serde(default)]
    evidence: Vec<String>,
}

/// The reason and evidence of a blocker call, as the run's closing text.
///
/// # Errors
///
/// Returns [`ToolError`] when the arguments do not parse or give no reason.
pub fn describe(call: &ToolCall) -> Result<String, ToolError> {
    let arguments: BlockerArguments = serde_json::from_value(call.arguments.clone())
        .map_err(|error| ToolError::new(format!("invalid {REPORT_BLOCKER} arguments: {error}")))?;
    if arguments.reason.trim().is_empty() {
        return Err(ToolError::new("a blocker needs a reason"));
    }
    let mut text = format!("Blocked: {}", arguments.reason.trim());
    for item in &arguments.evidence {
        text.push_str("\n- ");
        text.push_str(item);
    }
    Ok(text)
}

impl ToolRuntime for BlockerTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: REPORT_BLOCKER.into(),
            description: "Stop, and report that the task cannot be done as asked: the \
                          request contradicts itself, or contradicts the tests or code it \
                          must keep. Give the reason and the evidence, such as the files and \
                          test names. This ends the run, so use it instead of changing a test \
                          or the spec to make the two agree."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "reason": {"type": "string", "minLength": 1},
                    "evidence": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["reason"],
                "additionalProperties": false
            }),
        }]
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if call.name != REPORT_BLOCKER {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        describe(call)?;
        Ok(ToolAction::read_only())
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let result = describe(call).map(|_| ToolOutput::success("recorded; the run ends here"));
        Box::pin(async move { result })
    }
}
