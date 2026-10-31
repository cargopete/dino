//! A second look at finished work by a model that saw none of the conversation
//! that produced it: only the task, the change since the run began, and the
//! latest check results. One call, no tools. Issue #209.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use dino_model::ModelSession;
use dino_protocol::AgentEvent;
use dino_protocol::ModelEvent;
use dino_protocol::TokenUsage;
use dino_protocol::ToolCall;
use dino_protocol::TurnInput;
use futures_util::StreamExt as _;
use serde_json::Value;

use crate::text::capped_head;
use crate::text::capped_output;

/// The largest diff the reviewer is shown.
pub const MAX_DIFF_BYTES: usize = 64 * 1_024;
const MAX_TASK_BYTES: usize = 16 * 1_024;
const MAX_CHECK_BYTES: usize = 3 * 1_024;
const MAX_OBJECTIONS: usize = 5;
/// Files past this size are not snapshotted, so a change to one goes unseen.
const MAX_FILE_BYTES: u64 = 512 * 1_024;
const MAX_SNAPSHOT_BYTES: u64 = 64 * 1_024 * 1_024;
const SKIPPED_DIRECTORIES: [&str; 3] = [".git", "target", ".dino"];
const NO_OBJECTIONS: &str = "NO OBJECTIONS";

pub const SYSTEM_PROMPT: &str = "You review a change to a code repository. You did not \
write it, and you see none of the conversation that produced it: only the task as it was \
given, the change made since the work began, and the latest test and check results. Look \
for what the change gets wrong against the task: a requirement it misses, an invariant or \
existing behaviour it breaks, an edit outside what the task asked for. Raise only what you \
can point to in the task or the diff, each concrete enough to act on. Leave out style, \
naming, and anything you cannot confirm from what you were given. If you find nothing of \
that kind, reply with exactly NO OBJECTIONS. Otherwise reply with at most five objections, \
one per line, each starting with \"- \", the most serious first, and nothing else.";

/// Opens a fresh provider session with the given system prompt and no tools.
pub type OpenSession =
    Box<dyn Fn(String) -> Result<Box<dyn ModelSession>, String> + Send + Sync + 'static>;

/// What a review came to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verdict {
    pub objections: Vec<String>,
    /// Set when the review failed, in which case it objects to nothing.
    pub error: Option<String>,
    pub usage: TokenUsage,
}

/// Reviews a workspace's change since the run began.
pub struct Reviewer {
    workspace: PathBuf,
    open: OpenSession,
    max_diff_bytes: usize,
    start: Mutex<Snapshot>,
    /// The whole diff the last review saw, to tell whether the work changed after.
    reviewed: Mutex<Option<String>>,
}

impl Reviewer {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>, open: OpenSession) -> Self {
        Self {
            workspace: workspace.into(),
            open,
            max_diff_bytes: MAX_DIFF_BYTES,
            start: Mutex::new(Snapshot::default()),
            reviewed: Mutex::new(None),
        }
    }

    #[must_use]
    pub fn with_max_diff_bytes(mut self, bytes: usize) -> Self {
        self.max_diff_bytes = bytes;
        self
    }

    /// Takes the state the next review's diff starts from.
    pub fn begin(&self) {
        let snapshot = Snapshot::take(&self.workspace);
        if let Ok(mut start) = self.start.lock() {
            *start = snapshot;
        }
        if let Ok(mut reviewed) = self.reviewed.lock() {
            *reviewed = None;
        }
    }

    /// The change since [`Reviewer::begin`], as a unified diff.
    #[must_use]
    pub fn diff(&self) -> String {
        let now = Snapshot::take(&self.workspace);
        self.start
            .lock()
            .map(|start| start.diff(&now))
            .unwrap_or_default()
    }

    /// Whether the workspace changed after the last review, or `None` when
    /// there has been none since the run began.
    #[must_use]
    pub fn changed_since_review(&self) -> Option<bool> {
        let reviewed = self.reviewed.lock().ok()?.clone()?;
        Some(self.diff() != reviewed)
    }

    /// Reviews the change against `task`, or `None` when nothing changed.
    pub async fn review(&self, task: &str, checks: &str) -> Option<Verdict> {
        let diff = self.diff();
        if diff.is_empty() {
            return None;
        }
        let request = request(task, &diff, checks, self.max_diff_bytes);
        if let Ok(mut reviewed) = self.reviewed.lock() {
            *reviewed = Some(diff);
        }
        let mut verdict = Verdict::default();
        match self.ask(request, &mut verdict.usage).await {
            Ok(reply) => match objections(&reply) {
                Ok(objections) => verdict.objections = objections,
                Err(error) => verdict.error = Some(error),
            },
            Err(error) => verdict.error = Some(error),
        }
        Some(verdict)
    }

    async fn ask(&self, request: String, usage: &mut TokenUsage) -> Result<String, String> {
        let mut session = (self.open)(SYSTEM_PROMPT.to_owned())?;
        let mut stream = session
            .next(TurnInput::User { content: request })
            .await
            .map_err(|error| error.to_string())?;
        let mut reply = String::new();
        while let Some(event) = stream.next().await {
            match event.map_err(|error| error.to_string())? {
                ModelEvent::TextDelta { text } => reply.push_str(&text),
                ModelEvent::Usage { usage: used } => {
                    usage.input_tokens += used.input_tokens;
                    usage.cached_input_tokens += used.cached_input_tokens;
                    usage.cache_write_input_tokens += used.cache_write_input_tokens;
                    usage.output_tokens += used.output_tokens;
                    usage.reasoning_tokens += used.reasoning_tokens;
                }
                _ => {}
            }
        }
        Ok(reply)
    }
}

fn request(task: &str, diff: &str, checks: &str, max_diff_bytes: usize) -> String {
    let mut request = format!(
        "## Task\n\n{}\n\n## Change since the work began\n\n```diff\n{}\n```\n",
        capped_head(task, MAX_TASK_BYTES),
        capped_head(diff, max_diff_bytes)
    );
    if diff.len() > max_diff_bytes {
        let _ = writeln!(
            request,
            "\nThe diff is {} bytes; only the first {max_diff_bytes} are shown, so do not \
             object to what may be in the rest.",
            diff.len()
        );
    }
    request.push_str("\n## Latest checks\n\n");
    if checks.is_empty() {
        request.push_str("No tests or checks were run.\n");
    } else {
        request.push_str(checks);
    }
    request
}

/// The objections in a reply, or why the reply is neither a list nor a pass.
fn objections(reply: &str) -> Result<Vec<String>, String> {
    let listed: Vec<String> = reply
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("- "))
        .map(str::trim)
        .filter(|objection| !objection.is_empty())
        .take(MAX_OBJECTIONS)
        .map(str::to_owned)
        .collect();
    if !listed.is_empty() {
        return Ok(listed);
    }
    let reply = reply.trim();
    if reply
        .get(..NO_OBJECTIONS.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(NO_OBJECTIONS))
    {
        return Ok(Vec::new());
    }
    Err(format!(
        "the reviewer replied neither {NO_OBJECTIONS} nor a list: {}",
        capped_head(reply, 200)
    ))
}

/// What the model is told when the reviewer objects.
#[must_use]
pub fn objections_note(objections: &[String]) -> String {
    let mut note = "[dino: before you finish, a reviewer who saw only the task, your change and \
                    the latest check results objected:\n"
        .to_owned();
    for objection in objections {
        let _ = writeln!(note, "- {objection}");
    }
    note.push_str(
        "Fix those that are right and check again; for any that are wrong, say why in a \
         sentence. Then finish. There is no second review.]",
    );
    note
}

/// The task this run was given: the first submission among `events`.
#[must_use]
pub fn task(events: &[AgentEvent]) -> String {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::Submitted { text } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The last result of each kind of test or check the model ran, in the order
/// each kind was first run.
#[must_use]
pub fn latest_checks(events: &[AgentEvent]) -> String {
    let mut calls: Vec<&ToolCall> = Vec::new();
    let mut latest: Vec<(String, String)> = Vec::new();
    for event in events {
        match event {
            AgentEvent::Model {
                event: ModelEvent::ToolCallCompleted { call },
                ..
            } => calls.push(call),
            AgentEvent::ToolFinished { result, .. } => {
                let Some(kind) = calls
                    .iter()
                    .find(|call| call.id == result.call_id)
                    .and_then(|call| check_kind(call))
                else {
                    continue;
                };
                let output = capped_output(&result.output.content, MAX_CHECK_BYTES);
                match latest.iter_mut().find(|(known, _)| *known == kind) {
                    Some((_, last)) => *last = output,
                    None => latest.push((kind, output)),
                }
            }
            _ => {}
        }
    }
    latest
        .iter()
        .fold(String::new(), |mut text, (kind, output)| {
            let _ = write!(text, "`{kind}`, latest run:\n```\n{output}\n```\n\n");
            text
        })
}

const CHECKS: [&str; 5] = ["test", "clippy", "check", "fmt", "build"];

fn check_kind(call: &ToolCall) -> Option<String> {
    let words: Vec<String> = match call.name.as_str() {
        "cargo" => {
            let command = call.arguments.get("command")?.as_str()?;
            return CHECKS
                .contains(&command)
                .then(|| format!("cargo {command}"));
        }
        "exec" => call
            .arguments
            .get("command")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        "shell" => call
            .arguments
            .get("command")?
            .as_str()?
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
        _ => return None,
    };
    words.windows(2).find_map(|pair| {
        (pair[0] == "cargo" && CHECKS.contains(&pair[1].as_str()))
            .then(|| format!("cargo {}", pair[1]))
    })
}

/// The workspace's files, keyed by relative path: text, or the bytes of a
/// file that is not UTF-8.
#[derive(Debug, Default)]
struct Snapshot {
    files: BTreeMap<String, Result<String, Vec<u8>>>,
}

impl Snapshot {
    fn take(root: &Path) -> Self {
        let mut files = BTreeMap::new();
        let mut total = 0;
        let walk = ignore::WalkBuilder::new(root)
            .hidden(false)
            .require_git(false)
            .filter_entry(|entry| {
                !(entry.file_type().is_some_and(|kind| kind.is_dir())
                    && SKIPPED_DIRECTORIES
                        .iter()
                        .any(|skipped| entry.file_name() == *skipped))
            })
            .build();
        for entry in walk.flatten() {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let Ok(size) = entry.metadata().map(|metadata| metadata.len()) else {
                continue;
            };
            if size > MAX_FILE_BYTES || total + size > MAX_SNAPSHOT_BYTES {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(root) else {
                continue;
            };
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            total += size;
            files.insert(
                relative.to_string_lossy().replace('\\', "/"),
                String::from_utf8(bytes).map_err(std::string::FromUtf8Error::into_bytes),
            );
        }
        Self { files }
    }

    fn diff(&self, now: &Self) -> String {
        let mut paths: Vec<&String> = self.files.keys().chain(now.files.keys()).collect();
        paths.sort();
        paths.dedup();
        let mut diff = String::new();
        for path in paths {
            let (before, after) = (self.files.get(path), now.files.get(path));
            // Cargo writes a lockfile on its first run, which is not the work.
            if before == after || (before.is_none() && path.ends_with("Cargo.lock")) {
                continue;
            }
            let (Some(old), Some(new)) = (
                before.map_or(Some(""), |file| file.as_deref().ok()),
                after.map_or(Some(""), |file| file.as_deref().ok()),
            ) else {
                let _ = writeln!(diff, "Binary file {path} changed");
                continue;
            };
            let from = if before.is_some() {
                format!("a/{path}")
            } else {
                "/dev/null".to_owned()
            };
            let to = if after.is_some() {
                format!("b/{path}")
            } else {
                "/dev/null".to_owned()
            };
            let _ = write!(
                diff,
                "{}",
                similar::TextDiff::from_lines(old, new)
                    .unified_diff()
                    .context_radius(3)
                    .header(&from, &to)
            );
        }
        diff
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dino_protocol::ToolOutput;
    use dino_protocol::ToolResult;
    use serde_json::json;

    #[test]
    fn a_reply_is_a_pass_a_list_or_neither() {
        assert_eq!(objections("NO OBJECTIONS"), Ok(Vec::new()));
        assert_eq!(objections("  no objections.\n"), Ok(Vec::new()));
        assert_eq!(
            objections("- parse drops the sign\n  - the README edit is outside the task\n"),
            Ok(vec![
                "parse drops the sign".to_owned(),
                "the README edit is outside the task".to_owned()
            ])
        );
        let many = (0..9).fold(String::new(), |mut reply, n| {
            let _ = writeln!(reply, "- objection {n}");
            reply
        });
        assert_eq!(objections(&many).unwrap().len(), MAX_OBJECTIONS);
        assert!(objections("Looks fine to me").is_err());
    }

    fn call(events: &mut Vec<AgentEvent>, id: &str, name: &str, arguments: Value, output: &str) {
        events.push(AgentEvent::Model {
            model_turn: 1,
            event: ModelEvent::ToolCallCompleted {
                call: ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments,
                },
            },
        });
        events.push(AgentEvent::ToolFinished {
            model_turn: 1,
            result: ToolResult {
                call_id: id.into(),
                output: ToolOutput::success(output),
            },
        });
    }

    #[test]
    fn only_the_latest_run_of_each_check_is_shown() {
        let mut events = vec![AgentEvent::Submitted {
            text: "fix it".into(),
        }];
        call(
            &mut events,
            "1",
            "cargo",
            json!({"command": "test"}),
            "1 failed",
        );
        call(
            &mut events,
            "2",
            "read_file",
            json!({"path": "a"}),
            "contents",
        );
        call(
            &mut events,
            "3",
            "shell",
            json!({"command": "cargo clippy --all-targets"}),
            "clean",
        );
        call(
            &mut events,
            "4",
            "exec",
            json!({"command": ["cargo", "test"]}),
            "ok",
        );
        let checks = latest_checks(&events);
        assert!(
            checks.contains("`cargo test`") && checks.contains("ok"),
            "{checks}"
        );
        assert!(!checks.contains("1 failed") && !checks.contains("contents"));
        assert!(checks.find("cargo test") < checks.find("cargo clippy"));
        assert_eq!(task(&events), "fix it");
    }

    #[test]
    fn a_large_diff_is_cut_and_said_to_be() {
        let diff = "+x\n".repeat(100);
        let request = request("task", &diff, "", 30);
        assert!(request.contains("only the first 30"), "{request}");
        assert!(request.contains("No tests or checks were run."));
        assert!(request.len() < 600);
    }

    #[test]
    fn the_diff_covers_edits_new_files_and_removals_and_skips_target() {
        let root = std::env::temp_dir().join(format!("dino-review-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        std::fs::write(root.join("gone.txt"), "old\n").unwrap();
        let start = Snapshot::take(&root);

        std::fs::write(root.join("src/lib.rs"), "fn a() {}\nfn c() {}\n").unwrap();
        std::fs::write(root.join("new.txt"), "fresh\n").unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        std::fs::write(root.join("target/junk"), "built\n").unwrap();
        std::fs::write(root.join("Cargo.lock"), "version = 4\n").unwrap();
        let diff = start.diff(&Snapshot::take(&root));
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            diff.contains("--- a/src/lib.rs\n+++ b/src/lib.rs"),
            "{diff}"
        );
        assert!(diff.contains("-fn b() {}\n+fn c() {}"), "{diff}");
        assert!(diff.contains("--- /dev/null\n+++ b/new.txt"), "{diff}");
        assert!(diff.contains("--- a/gone.txt\n+++ /dev/null"), "{diff}");
        assert!(
            !diff.contains("junk") && !diff.contains("Cargo.lock"),
            "{diff}"
        );
    }
}
