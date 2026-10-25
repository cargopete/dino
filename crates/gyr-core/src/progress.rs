//! The progress record a handover chain is carried by. RFC-0018 section 3.
//!
//! A tool rather than a file the model edits, because the harness reads it to
//! decide whether to hand over, and a tool call cannot be broken by a bad patch.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use gyr_protocol::ToolAction;
use gyr_protocol::ToolCall;
use gyr_protocol::ToolDefinition;
use gyr_protocol::ToolOutput;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::ToolError;
use crate::ToolFuture;
use crate::ToolRuntime;

pub const PROGRESS: &str = "progress";
const MAX_ITEMS: usize = 12;
const MAX_TEXT_BYTES: usize = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Done,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub text: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub items: Vec<Item>,
    pub notes: Vec<String>,
}

impl Record {
    #[must_use]
    pub fn pending(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.status == Status::Pending)
            .count()
    }

    #[must_use]
    pub fn done(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.status == Status::Done)
            .count()
    }

    /// The record as a fresh session reads it.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = String::new();
        for (index, item) in self.items.iter().enumerate() {
            let mark = match item.status {
                Status::Done => "[x]",
                Status::Blocked => "[!]",
                Status::Pending => "[ ]",
            };
            let _ = write!(&mut text, "{mark} {}. {}", index + 1, item.text);
            if let Some(note) = &item.note {
                let _ = write!(&mut text, " ({note})");
            }
            text.push('\n');
        }
        if !self.notes.is_empty() {
            text.push_str("Notes:\n");
            for note in &self.notes {
                let _ = writeln!(&mut text, "- {note}");
            }
        }
        text
    }
}

/// The record, shared with the harness, and written to `path` after each call.
#[derive(Debug, Clone)]
pub struct Progress {
    record: Arc<Mutex<Record>>,
    path: PathBuf,
}

impl Progress {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            record: Arc::new(Mutex::new(Record::default())),
            path: path.into(),
        }
    }

    /// The record as it stands.
    ///
    /// # Panics
    ///
    /// Panics if the record's lock was poisoned by an earlier panic.
    #[must_use]
    pub fn snapshot(&self) -> Record {
        self.record.lock().expect("progress lock").clone()
    }

    fn apply(&self, arguments: &Value) -> Result<String, ToolError> {
        let op = Op::parse(arguments)?;
        let mut record = self
            .record
            .lock()
            .map_err(|_| ToolError::new("progress record lock was poisoned"))?;
        let clean = |text: &str| -> Result<String, ToolError> {
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if text.is_empty() || text.len() > MAX_TEXT_BYTES {
                return Err(ToolError::new(format!(
                    "text must be 1 to {MAX_TEXT_BYTES} bytes"
                )));
            }
            Ok(text)
        };
        let done = matches!(op, Op::Done { .. });
        match op {
            Op::Plan { items } => {
                if items.is_empty() || items.len() > MAX_ITEMS {
                    return Err(ToolError::new(format!("a plan has 1 to {MAX_ITEMS} items")));
                }
                if record.done() > 0 {
                    return Err(ToolError::new(
                        "the plan already has done items; mark items blocked or add notes instead",
                    ));
                }
                record.items = items
                    .iter()
                    .map(|text| {
                        Ok(Item {
                            text: clean(text)?,
                            status: Status::Pending,
                            note: None,
                        })
                    })
                    .collect::<Result<_, ToolError>>()?;
            }
            Op::Done { item, note } | Op::Blocked { item, note } => {
                let status = if done { Status::Done } else { Status::Blocked };
                let count = record.items.len();
                let note = note.as_deref().map(clean).transpose()?;
                let numbers = item.numbers();
                // All or none, so a bad number leaves the record as it was.
                if let Some(bad) = numbers
                    .iter()
                    .find(|&&number| number == 0 || number > count)
                {
                    return Err(ToolError::new(format!(
                        "there is no item {bad}; the plan has {count}"
                    )));
                }
                for number in numbers {
                    let entry = &mut record.items[number - 1];
                    entry.status = status;
                    entry.note.clone_from(&note);
                }
            }
            Op::Note { text } => {
                let text = clean(&text)?;
                record.notes.push(text);
            }
        }
        let encoded = serde_json::to_vec_pretty(&*record)
            .map_err(|error| ToolError::new(format!("cannot encode the record: {error}")))?;
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&self.path, encoded).map_err(|error| {
            ToolError::new(format!("cannot write {}: {error}", self.path.display()))
        })?;
        Ok(format!(
            "Recorded. {} of {} done.\n{}",
            record.done(),
            record.items.len(),
            record.render()
        ))
    }
}

#[derive(Debug)]
enum Op {
    Plan { items: Vec<String> },
    Done { item: Numbers, note: Option<String> },
    Blocked { item: Numbers, note: Option<String> },
    Note { text: String },
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Plan,
    Done,
    Blocked,
    Note,
}

/// The arguments as sent. Models put `text` where `note` belongs and the other
/// way round, and name an item under `items`; refusing those cost one run
/// every call it made, so the fields are read by what the op needs.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    op: Kind,
    items: Option<Value>,
    item: Option<Numbers>,
    note: Option<String>,
    text: Option<String>,
}

impl Op {
    fn parse(arguments: &Value) -> Result<Self, ToolError> {
        let invalid = |why: &str| ToolError::new(format!("invalid {PROGRESS} arguments: {why}"));
        let raw: Arguments = serde_json::from_value(arguments.clone())
            .map_err(|error| invalid(&error.to_string()))?;
        let words = raw.note.or(raw.text);
        let numbers = || {
            raw.item
                .or_else(|| {
                    raw.items
                        .clone()
                        .and_then(|items| serde_json::from_value(items).ok())
                })
                .ok_or_else(|| invalid("done and blocked need item, the item's number"))
        };
        Ok(match raw.op {
            Kind::Plan => Self::Plan {
                items: raw
                    .items
                    .and_then(|items| serde_json::from_value(items).ok())
                    .ok_or_else(|| invalid("plan needs items, a list of short strings"))?,
            },
            Kind::Done => Self::Done {
                item: numbers()?,
                note: words,
            },
            Kind::Blocked => Self::Blocked {
                item: numbers()?,
                note: words,
            },
            Kind::Note => Self::Note {
                text: words.ok_or_else(|| invalid("note needs text"))?,
            },
        })
    }
}

/// One item number, or several. The schema offers an integer only: a union
/// type there made Qwen send `"1"` and `"[1, 2]"` as strings, so those are
/// read too.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Numbers {
    One(usize),
    Many(Vec<usize>),
    Text(String),
}

impl Numbers {
    fn numbers(&self) -> Vec<usize> {
        match self {
            Self::One(number) => vec![*number],
            Self::Many(numbers) => numbers.clone(),
            Self::Text(text) => {
                let text = text.trim();
                serde_json::from_str::<Vec<usize>>(text)
                    .ok()
                    .or_else(|| text.parse().ok().map(|number| vec![number]))
                    .unwrap_or_else(|| vec![0])
            }
        }
    }
}

impl ToolRuntime for Progress {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: PROGRESS.into(),
            description: "Keep the progress record this task is carried by. It may continue in \
                          a fresh session that sees only the record and the workspace. op plan \
                          sets 2 to 6 items before you start; op done or blocked marks items by \
                          number, with a short note; op note records what a fresh session would \
                          need: what you tried, why it failed, where things are."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["plan", "done", "blocked", "note"]},
                    "items": {"type": "array", "items": {"type": "string"}},
                    "item": {"type": "integer", "minimum": 1},
                    "note": {"type": "string"},
                    "text": {"type": "string"}
                },
                "required": ["op"],
                "additionalProperties": false
            }),
        }]
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if call.name != PROGRESS {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        Op::parse(&call.arguments)?;
        Ok(ToolAction::read_only())
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let result = self.apply(&call.arguments).map(ToolOutput::success);
        Box::pin(async move { result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(arguments: Value) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: PROGRESS.into(),
            arguments,
        }
    }

    #[tokio::test]
    async fn the_record_follows_its_ops_and_lands_on_disk() {
        let path = std::env::temp_dir()
            .join(format!("gyr-progress-{}", std::process::id()))
            .join("progress.json");
        let progress = Progress::new(&path);

        for arguments in [
            json!({"op": "plan", "items": ["parse integers", "precedence", "error offsets"]}),
            json!({"op": "done", "item": 1, "note": "tests in tests/int.rs"}),
            json!({"op": "blocked", "item": 3, "note": "spec unclear"}),
            json!({"op": "note", "text": "recursive descent failed on right associativity"}),
        ] {
            progress.execute(&call(arguments)).await.unwrap();
        }

        let record = progress.snapshot();
        assert_eq!(record.pending(), 1);
        assert_eq!(
            record.render(),
            "[x] 1. parse integers (tests in tests/int.rs)\n[ ] 2. precedence\n\
             [!] 3. error offsets (spec unclear)\nNotes:\n\
             - recursive descent failed on right associativity\n"
        );
        let saved: Record = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved, record);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn misplaced_fields_are_read_by_what_the_op_needs() {
        let progress = Progress::new(
            std::env::temp_dir().join(format!("gyr-progress-loose-{}.json", std::process::id())),
        );
        for arguments in [
            json!({"op": "plan", "items": ["a", "b", "c"]}),
            json!({"op": "done", "item": 1, "text": "parser done"}),
            json!({"op": "done", "items": [2, 3]}),
            json!({"op": "note", "note": "all green"}),
        ] {
            progress
                .execute(&call(arguments.clone()))
                .await
                .unwrap_or_else(|error| panic!("{arguments}: {error}"));
        }
        let record = progress.snapshot();
        assert_eq!(record.done(), 3);
        assert_eq!(record.items[0].note.as_deref(), Some("parser done"));
        assert_eq!(record.notes, ["all green"]);
    }

    #[tokio::test]
    async fn several_items_are_closed_in_one_call() {
        let progress = Progress::new(
            std::env::temp_dir().join(format!("gyr-progress-many-{}.json", std::process::id())),
        );
        for arguments in [
            json!({"op": "plan", "items": ["a", "b", "c"]}),
            json!({"op": "done", "item": [1, 2], "note": "all verified"}),
            json!({"op": "done", "item": "3", "note": "all verified"}),
        ] {
            progress.execute(&call(arguments)).await.unwrap();
        }
        let record = progress.snapshot();
        assert_eq!(record.done(), 3);
        assert_eq!(record.items[2].note.as_deref(), Some("all verified"));
    }

    #[tokio::test]
    async fn a_plan_with_done_items_cannot_be_replaced_and_bad_items_are_refused() {
        let progress = Progress::new(
            std::env::temp_dir().join(format!("gyr-progress-refuse-{}.json", std::process::id())),
        );
        progress
            .execute(&call(json!({"op": "plan", "items": ["a", "b"]})))
            .await
            .unwrap();
        progress
            .execute(&call(json!({"op": "done", "item": 1})))
            .await
            .unwrap();

        for arguments in [
            json!({"op": "plan", "items": ["c"]}),
            json!({"op": "done", "item": 0}),
            json!({"op": "done", "item": 3}),
            json!({"op": "done", "item": [2, 3]}),
            json!({"op": "done", "item": "two"}),
            json!({"op": "note", "text": "  "}),
        ] {
            assert!(
                progress.execute(&call(arguments.clone())).await.is_err(),
                "{arguments}"
            );
        }
    }
}
