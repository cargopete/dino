//! The terminal approval prompt.
//!
//! The prompt shows the tool name and the subject the runtime resolved, not a
//! description the model wrote. Answering approves that exact action, or a rule
//! narrowed to that exact tool and subject.

use std::io::BufRead;
use std::io::IsTerminal;
use std::io::Write;
use std::io::stderr;
use std::io::stdin;

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use dino_core::approval::ApprovalReply;
use dino_core::approval::Approver;
use dino_core::approval::ReplyFuture;
use dino_core::approval::RuleStore;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolClass;

use crate::render::describe_call;
use crate::style;
use crate::style::BOLD;
use crate::style::FAINT;
use crate::style::RUST;

#[derive(Debug, Clone)]
pub struct TerminalApprover {
    sandbox: String,
    caveat_shown: Arc<AtomicBool>,
}

impl TerminalApprover {
    #[must_use]
    pub fn new(sandbox: impl Into<String>) -> Self {
        Self {
            sandbox: sandbox.into(),
            caveat_shown: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Approver for TerminalApprover {
    fn ask(&self, call: &ToolCall, action: &ToolAction) -> ReplyFuture<'_> {
        let summary = describe_call(call);
        let subject = action.subject.clone().unwrap_or_else(|| call.name.clone());
        let tool = call.name.clone();
        let class = action.class;
        let sandbox = self.sandbox.clone();
        let caveat_shown = Arc::clone(&self.caveat_shown);
        Box::pin(async move {
            let prompt =
                move || ask_on_terminal(&summary, &tool, &subject, class, &caveat_shown, &sandbox);
            match tokio::task::spawn_blocking(prompt).await {
                Ok(reply) => reply,
                Err(error) => {
                    ApprovalReply::Reject(Some(format!("approval prompt failed: {error}")))
                }
            }
        })
    }
}

/// The choices, in the order the selector shows them, with their keys.
const CHOICES: [(char, &str); 4] = [('y', "once"), ('a', ""), ('r', "remember"), ('n', "refuse")];

/// Asks on the terminal and refuses on anything that is not a clear yes.
///
/// A closed or non-interactive standard input reaches the same place: refusal.
/// An approval that a person did not give is not an approval.
fn ask_on_terminal(
    summary: &str,
    tool: &str,
    subject: &str,
    class: ToolClass,
    caveat_shown: &AtomicBool,
    sandbox: &str,
) -> ApprovalReply {
    let mut err = stderr().lock();
    // The one moment a person is directly addressed.
    let header = style::paint_with(RUST, &[BOLD], "  approval");
    let detail = style::paint(FAINT, &format!("      rule scope: {tool} on {subject}"));
    let (always, caveat) = match class {
        // The containment line is the same every time, so it is said once.
        ToolClass::Process => (
            "always for this command",
            (!caveat_shown.swap(true, Ordering::Relaxed)).then(|| {
                format!(
                    "      runs code on your machine · containment: {sandbox} · what it runs may change"
                )
            }),
        ),
        ToolClass::Network => (
            "always for this host",
            Some(format!(
                "      sends a request to {subject} · the URL can carry workspace text with it"
            )),
        ),
        _ => ("always for this file", None),
    };
    let written = writeln!(err, "\n{header}  {summary}")
        .and_then(|()| writeln!(err, "{detail}"))
        .and_then(|()| match &caveat {
            Some(caveat) => writeln!(err, "{}", style::paint(FAINT, caveat)),
            None => Ok(()),
        })
        .and_then(|()| err.flush());
    if written.is_err() {
        return ApprovalReply::Reject(Some("could not present the approval prompt".into()));
    }
    drop(err);

    let labels = CHOICES.map(|(key, label)| (key, if label.is_empty() { always } else { label }));
    if stdin().is_terminal()
        && stderr().is_terminal()
        && let Some(reply) = select(&labels)
    {
        return reply;
    }
    read_typed_answer(&labels)
}

/// The selector: arrows or tab move, enter confirms, a choice's key answers at
/// once, and esc or ctrl-c refuses. Keys typed before the prompt appeared are
/// discarded, and any other key is ignored rather than taken as a refusal.
/// `None` when the terminal cannot be put into raw mode.
fn select(labels: &[(char, &str); 4]) -> Option<ApprovalReply> {
    use crossterm::event::Event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEventKind;
    use crossterm::event::KeyModifiers;
    use crossterm::event::poll;
    use crossterm::event::read;
    use crossterm::terminal;

    terminal::enable_raw_mode().ok()?;
    let raw = RawGuard;
    while poll(Duration::ZERO).unwrap_or(false) {
        let _ = read();
    }

    let mut current = 0usize;
    let chosen = loop {
        draw_choices(labels, current);
        let Ok(Event::Key(key)) = read() else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break 3,
            KeyCode::Esc => break 3,
            KeyCode::Enter => break current,
            KeyCode::Left | KeyCode::Up | KeyCode::BackTab => {
                current = (current + labels.len() - 1) % labels.len();
            }
            KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                current = (current + 1) % labels.len();
            }
            KeyCode::Char(typed) => {
                if let Some(index) = labels
                    .iter()
                    .position(|(key, _)| *key == typed.to_ascii_lowercase())
                {
                    break index;
                }
            }
            _ => {}
        }
    };
    draw_choices(labels, chosen);
    let mut err = stderr();
    let _ = write!(err, "\r\n");
    let _ = err.flush();
    drop(raw);

    Some(match chosen {
        0 => ApprovalReply::Once,
        1 => ApprovalReply::ForSession,
        2 => ApprovalReply::Remember,
        _ => ApprovalReply::Reject(Some(refusal_reason())),
    })
}

/// Restores the terminal however the selector exits.
struct RawGuard;

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn draw_choices(labels: &[(char, &str); 4], current: usize) {
    let mut line = String::from("      ");
    for (index, (key, label)) in labels.iter().enumerate() {
        let text = format!("[{key}] {label}");
        if index == current {
            line.push_str(&style::paint_with(RUST, &[BOLD], &format!("› {text}")));
        } else {
            line.push_str(&style::paint(FAINT, &format!("  {text}")));
        }
        line.push_str("  ");
    }
    let mut err = stderr();
    let _ = write!(err, "\r\x1b[2K{line}");
    let _ = err.flush();
}

/// What the person wants done instead, passed to the model with the refusal.
fn refusal_reason() -> String {
    let mut err = stderr();
    let _ = write!(
        err,
        "{}",
        style::paint(FAINT, "      what should it do instead? (enter to skip): ")
    );
    let _ = err.flush();
    let mut reason = String::new();
    let _ = stdin().lock().read_line(&mut reason);
    match reason.trim() {
        "" => "the operator refused this action".to_owned(),
        reason => format!("the operator refused this action: {reason}"),
    }
}

/// For a pipe or a terminal that cannot go raw: one typed line, where only a
/// clear answer approves.
fn read_typed_answer(labels: &[(char, &str); 4]) -> ApprovalReply {
    let mut err = stderr().lock();
    let options: Vec<String> = labels
        .iter()
        .map(|(key, label)| format!("[{key}] {label}"))
        .collect();
    let question = style::paint(FAINT, &format!("      {}: ", options.join("   ")));
    if write!(err, "{question}")
        .and_then(|()| err.flush())
        .is_err()
    {
        return ApprovalReply::Reject(Some("could not present the approval prompt".into()));
    }

    let mut answer = String::new();
    if stdin().lock().read_line(&mut answer).is_err() || answer.is_empty() {
        let _ = writeln!(err);
        return ApprovalReply::Reject(Some(
            "standard input is not available to answer the prompt".into(),
        ));
    }

    // A terminal echoes the operator's newline; a pipe does not, and the next
    // line would otherwise begin halfway along the question.
    if !stdin().is_terminal() {
        let _ = writeln!(err);
    }

    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalReply::Once,
        "a" | "always" => ApprovalReply::ForSession,
        "r" | "remember" => ApprovalReply::Remember,
        _ => ApprovalReply::Reject(Some("the operator refused this action".into())),
    }
}

/// Saved approval rules for one workspace, kept with the user's config rather
/// than in the repository, so a checkout cannot approve its own commands.
#[derive(Debug, Clone)]
pub struct FileRuleStore {
    path: PathBuf,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct SavedRules {
    workspace: String,
    allow: Vec<SavedRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SavedRule {
    tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
}

const SEPARATOR: char = '\u{1f}';

impl FileRuleStore {
    /// The store for `workspace` beneath `user_dir`.
    #[must_use]
    pub fn for_workspace(user_dir: &Path, workspace: &Path) -> Self {
        let name = workspace.display().to_string().replace('/', "%");
        Self {
            path: user_dir.join("approvals").join(format!("{name}.json")),
        }
    }

    fn read(&self) -> SavedRules {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
}

impl RuleStore for FileRuleStore {
    fn load(&self) -> Vec<String> {
        self.read()
            .allow
            .into_iter()
            .map(|rule| match rule.subject {
                Some(subject) => format!("{}{SEPARATOR}{subject}", rule.tool),
                None => rule.tool,
            })
            .collect()
    }

    fn save(&self, key: &str) {
        let mut saved = self.read();
        let (tool, subject) = match key.split_once(SEPARATOR) {
            Some((tool, subject)) => (tool.to_owned(), Some(subject.to_owned())),
            None => (key.to_owned(), None),
        };
        let rule = SavedRule { tool, subject };
        if !saved.allow.contains(&rule) {
            saved.allow.push(rule);
        }
        saved.workspace = self
            .path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(|stem| stem.replace('%', "/"))
            .unwrap_or_default();
        let written = self
            .path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| serde_json::to_vec_pretty(&saved).map_err(std::io::Error::other))
            .and_then(|bytes| std::fs::write(&self.path, bytes));
        let note = match written {
            Ok(()) => format!("      saved to {}", self.path.display()),
            Err(error) => format!("      could not save the rule: {error}"),
        };
        let _ = writeln!(stderr(), "{}", style::paint(FAINT, &note));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_round_trip_through_the_file_and_are_kept_per_workspace() {
        let dir = std::env::temp_dir().join(format!("dino-rules-{}", std::process::id()));
        let store = FileRuleStore::for_workspace(&dir, Path::new("/repo/one"));
        let other = FileRuleStore::for_workspace(&dir, Path::new("/repo/two"));

        store.save("cargo\u{1f}cargo test --workspace");
        store.save("cargo\u{1f}cargo test --workspace");
        store.save("report_blocker");

        assert_eq!(
            store.load(),
            ["cargo\u{1f}cargo test --workspace", "report_blocker"]
        );
        assert!(other.load().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
