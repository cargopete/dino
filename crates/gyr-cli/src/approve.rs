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

use gyr_core::approval::ApprovalReply;
use gyr_core::approval::Approver;
use gyr_core::approval::ReplyFuture;
use gyr_core::approval::RuleStore;
use gyr_protocol::ToolAction;
use gyr_protocol::ToolCall;
use gyr_protocol::ToolClass;

use crate::render::describe_call;
use crate::style;
use crate::style::BOLD;
use crate::style::FAINT;
use crate::style::RUST;

#[derive(Debug, Clone)]
pub struct TerminalApprover {
    sandbox: String,
}

impl TerminalApprover {
    #[must_use]
    pub fn new(sandbox: impl Into<String>) -> Self {
        Self {
            sandbox: sandbox.into(),
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
        Box::pin(async move {
            let prompt = move || ask_on_terminal(&summary, &tool, &subject, class, &sandbox);
            match tokio::task::spawn_blocking(prompt).await {
                Ok(reply) => reply,
                Err(error) => {
                    ApprovalReply::Reject(Some(format!("approval prompt failed: {error}")))
                }
            }
        })
    }
}

/// Asks on the terminal and refuses on anything that is not a clear yes.
///
/// A closed or non-interactive standard input reaches the same place: refusal.
/// An approval that a person did not give is not an approval.
fn ask_on_terminal(
    summary: &str,
    tool: &str,
    subject: &str,
    class: ToolClass,
    sandbox: &str,
) -> ApprovalReply {
    let mut err = stderr().lock();
    // The one moment a person is directly addressed.
    let header = style::paint_with(RUST, &[BOLD], "  approval");
    let detail = style::paint(FAINT, &format!("      rule scope: {tool} on {subject}"));
    let (always, caveat) = match class {
        ToolClass::Process => (
            "always for this exact command",
            // Worth saying every time. A rule approves an argument vector, and
            // the code that vector compiles and runs can change afterwards.
            Some(format!(
                "      runs code on your machine · containment: {sandbox} · what it runs may change"
            )),
        ),
        _ => ("always for this file", None),
    };
    let question = style::paint(
        FAINT,
        &format!("      [y] once   [a] {always}   [r] remember for this workspace   [n] refuse: "),
    );
    let written = writeln!(err, "\n{header}  {summary}")
        .and_then(|()| writeln!(err, "{detail}"))
        .and_then(|()| match &caveat {
            Some(caveat) => writeln!(err, "{}", style::paint(FAINT, caveat)),
            None => Ok(()),
        })
        .and_then(|()| write!(err, "{question}"))
        .and_then(|()| err.flush());
    if written.is_err() {
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
        let dir = std::env::temp_dir().join(format!("gyr-rules-{}", std::process::id()));
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
