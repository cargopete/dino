//! Rendering a past session from its log.
//!
//! Through the same [`Renderer`] the live session uses, fed recorded events
//! instead of streamed ones. A second renderer would drift, and the first thing
//! it would drift on is the thing a person is replaying to check.

use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use dino_core::session::EventSink;
use dino_protocol::AgentEvent;
use serde_json::Value;

use crate::config;
use crate::render::Renderer;
use crate::render::status_block;
use crate::style;
use crate::style::FAINT;

#[derive(Debug, clap::Args)]
pub struct ReplayArgs {
    /// Session id, or the path of a session log. Defaults to the most recent
    /// session in this workspace.
    pub session: Option<String>,
    /// Workspace root. Defaults to the current directory.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// Show only the last N submissions.
    #[arg(long)]
    pub last: Option<usize>,
    /// Never emit terminal colour.
    #[arg(long)]
    pub plain: bool,
}

/// Prints a past session.
///
/// # Errors
///
/// Returns an error when there is no such session, or its log cannot be read.
pub fn run(args: &ReplayArgs) -> Result<ExitCode> {
    style::enable(args.plain);
    let workspace = config::resolve_workspace(args.workspace.as_deref())?;
    let (id, path) = locate(args.session.as_deref(), &workspace)?;

    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let mut records = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let record: Value = serde_json::from_str(line)
            .with_context(|| format!("{}:{}: not a session record", path.display(), number + 1))?;
        records.push(record);
    }

    print_header(&id, &records);
    let events = collect_events(&records, &path)?;
    let events = trim_to_last(events, args.last);

    let mut renderer = Renderer::new(true).replaying();
    for event in &events {
        renderer
            .emit(event)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
    }
    // A log can end mid-line, with no finish record to draw the last of it.
    renderer
        .flush()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    println!();
    Ok(ExitCode::SUCCESS)
}

fn locate(requested: Option<&str>, workspace: &Path) -> Result<(String, PathBuf)> {
    let directory = workspace.join(".dino").join("sessions");
    if let Some(id) = requested {
        let given = Path::new(id);
        if given
            .extension()
            .is_some_and(|extension| extension == "jsonl")
            && given.is_file()
        {
            let stem = given
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or(id);
            return Ok((stem.to_owned(), given.to_path_buf()));
        }
        let path = directory.join(format!("{id}.jsonl"));
        if !path.is_file() {
            bail!("no session {id:?} in {}", workspace.display());
        }
        return Ok((id.to_owned(), path));
    }

    let mut best: Option<(std::time::SystemTime, String, PathBuf)> = None;
    let entries = std::fs::read_dir(&directory)
        .with_context(|| format!("no sessions in {}", workspace.display()))?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Some(id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".jsonl"))
        else {
            continue;
        };
        let Ok(modified) = entry.metadata().and_then(|data| data.modified()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(when, _, _)| modified > *when) {
            best = Some((modified, id.to_owned(), path));
        }
    }
    match best {
        Some((_, id, path)) => Ok((id, path)),
        None => bail!("no sessions in {}", workspace.display()),
    }
}

fn print_header(id: &str, records: &[Value]) {
    let Some(session) = records.first().and_then(|record| record.get("session")) else {
        return;
    };
    let field = |name: &str| {
        session
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned()
    };
    print!(
        "{}",
        status_block(&[
            ("session", id.to_owned()),
            ("model", field("model_key")),
            ("workspace", field("workspace_root")),
            ("approvals", field("approval_mode")),
            ("sandbox", field("sandbox")),
        ])
    );
}

fn collect_events(records: &[Value], path: &Path) -> Result<Vec<AgentEvent>> {
    let mut events = Vec::new();
    for (number, record) in records.iter().enumerate() {
        if record.get("record").and_then(Value::as_str) != Some("event") {
            continue;
        }
        let event: AgentEvent = serde_json::from_value(record["event"].clone())
            .with_context(|| format!("{}:{}: not an agent event", path.display(), number + 1))?;
        events.push(event);
    }
    Ok(events)
}

/// Keeps only the last `count` submissions and everything after each.
///
/// A log with no submissions is left whole rather than emptied: an older log
/// predates the event, and showing nothing would look like an empty session.
fn trim_to_last(events: Vec<AgentEvent>, count: Option<usize>) -> Vec<AgentEvent> {
    let Some(count) = count.filter(|count| *count > 0) else {
        return events;
    };
    let starts: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event, AgentEvent::Submitted { .. }))
        .map(|(index, _)| index)
        .collect();
    if starts.len() <= count {
        return events;
    }
    let from = starts[starts.len() - count];
    println!(
        "{}",
        style::paint(
            FAINT,
            &format!(
                "  … {} earlier submission(s) not shown",
                starts.len() - count
            )
        )
    );
    events.into_iter().skip(from).collect()
}

#[cfg(test)]
mod tests {
    use dino_protocol::ModelEvent;

    use super::*;

    fn submitted(text: &str) -> AgentEvent {
        AgentEvent::Submitted { text: text.into() }
    }

    fn spoke(text: &str) -> AgentEvent {
        AgentEvent::Model {
            model_turn: 1,
            event: ModelEvent::TextDelta { text: text.into() },
        }
    }

    #[test]
    fn trimming_keeps_the_last_submissions_and_what_followed_them() {
        let events = vec![
            submitted("first"),
            spoke("one"),
            submitted("second"),
            spoke("two"),
            submitted("third"),
            spoke("three"),
        ];

        let kept = trim_to_last(events, Some(2));

        assert_eq!(kept.len(), 4);
        assert!(matches!(&kept[0], AgentEvent::Submitted { text } if text == "second"));
    }

    #[test]
    fn trimming_leaves_a_short_session_whole() {
        let events = vec![submitted("only"), spoke("answer")];

        assert_eq!(trim_to_last(events.clone(), Some(5)).len(), 2);
        assert_eq!(trim_to_last(events.clone(), None).len(), 2);
        assert_eq!(trim_to_last(events, Some(0)).len(), 2);
    }

    #[test]
    fn a_log_that_predates_submissions_is_left_whole() {
        // Showing nothing would look like an empty session rather than an old
        // one.
        let events = vec![spoke("an answer with no recorded question")];

        assert_eq!(trim_to_last(events, Some(1)).len(), 1);
    }
}
