//! Notes the model keeps for later sessions, in plain files the user owns.
//!
//! Two files: one for the user across every workspace, one for this
//! workspace. Both live with the user's config rather than in the repository,
//! and both are read back into the system prompt when a session starts.

use std::io::Write as _;
use std::path::PathBuf;

use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::ToolError;
use crate::ToolFuture;
use crate::ToolRuntime;

pub const REMEMBER: &str = "remember";
const MAX_NOTE_BYTES: usize = 500;

/// Where the two memory files are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryFiles {
    pub user: PathBuf,
    pub workspace: PathBuf,
}

#[derive(Debug, Clone)]
pub struct MemoryTool {
    files: MemoryFiles,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Scope {
    User,
    #[default]
    Workspace,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    note: String,
    #[serde(default)]
    scope: Scope,
}

impl MemoryTool {
    #[must_use]
    pub fn new(files: MemoryFiles) -> Self {
        Self { files }
    }

    fn path(&self, scope: Scope) -> &PathBuf {
        match scope {
            Scope::User => &self.files.user,
            Scope::Workspace => &self.files.workspace,
        }
    }
}

fn parse(arguments: &Value) -> Result<(String, Scope), ToolError> {
    let request: Request = serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::new(format!("invalid {REMEMBER} arguments: {error}")))?;
    let note = request
        .note
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if note.is_empty() || note.len() > MAX_NOTE_BYTES {
        return Err(ToolError::new(format!(
            "a note must be 1 to {MAX_NOTE_BYTES} bytes"
        )));
    }
    Ok((note, request.scope))
}

/// Today's date as `YYYY-MM-DD`, in UTC.
#[must_use]
pub fn today() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() / 86_400);
    let (year, month, day) = civil_from_days(i64::try_from(days).unwrap_or_default());
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's days-to-civil algorithm, so a date needs no dependency.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

impl ToolRuntime for MemoryTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: REMEMBER.into(),
            description: "Keep one short note for later sessions: a preference the person \
                          stated, a correction they made, or a fact about this repository that \
                          took effort to find. Not progress on the current task, and nothing \
                          the code or git history already says. scope user is for notes about \
                          the person, workspace (the default) for notes about this repository."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "note": {"type": "string", "minLength": 1, "maxLength": MAX_NOTE_BYTES},
                    "scope": {"type": "string", "enum": ["user", "workspace"]}
                },
                "required": ["note"],
                "additionalProperties": false
            }),
        }]
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if call.name != REMEMBER {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        let (_, scope) = parse(&call.arguments)?;
        Ok(ToolAction::mutating(self.path(scope).display().to_string()))
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let result = parse(&call.arguments).and_then(|(note, scope)| {
            let path = self.path(scope);
            let append = || -> std::io::Result<()> {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?;
                writeln!(file, "- {}: {note}", today())
            };
            append()
                .map(|()| ToolOutput::success(format!("remembered in {}", path.display())))
                .map_err(|error| {
                    ToolError::new(format!("cannot write {}: {error}", path.display()))
                })
        });
        Box::pin(async move { result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_are_right_either_side_of_a_leap_day() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(20_749), (2026, 10, 23));
    }

    #[tokio::test]
    async fn notes_append_to_the_file_for_their_scope() {
        let base = std::env::temp_dir().join(format!("dino-memory-{}", std::process::id()));
        let tool = MemoryTool::new(MemoryFiles {
            user: base.join("user.md"),
            workspace: base.join("repo.md"),
        });
        let call = |arguments: Value| ToolCall {
            id: "call-1".into(),
            name: REMEMBER.into(),
            arguments,
        };

        tool.execute(&call(json!({"note": "Prefers   thiserror\nover anyhow"})))
            .await
            .unwrap();
        tool.execute(&call(json!({"note": "Call them Chief", "scope": "user"})))
            .await
            .unwrap();

        let workspace = std::fs::read_to_string(base.join("repo.md")).unwrap();
        assert!(
            workspace.ends_with(": Prefers thiserror over anyhow\n"),
            "{workspace}"
        );
        assert!(
            std::fs::read_to_string(base.join("user.md"))
                .unwrap()
                .contains("Call them Chief")
        );
        assert!(tool.classify(&call(json!({"note": " "}))).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
