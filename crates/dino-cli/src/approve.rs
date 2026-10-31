//! The terminal approval prompt.
//!
//! The prompt shows the tool name and the subject the runtime resolved, not a
//! description the model wrote. Answering approves that exact action, or a rule
//! narrowed to that exact tool and subject.

use std::fmt::Write as _;
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
use dino_core::approval::AskContext;
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
    /// Where a proposed edit's file is read from, to show what it changes.
    root: PathBuf,
    caveat_shown: Arc<AtomicBool>,
}

impl TerminalApprover {
    #[must_use]
    pub fn new(sandbox: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            sandbox: sandbox.into(),
            root: root.into(),
            caveat_shown: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// The most diff lines the prompt shows before counting the rest.
const DIFF_LINES: usize = 30;

impl Approver for TerminalApprover {
    fn ask(&self, call: &ToolCall, action: &ToolAction) -> ReplyFuture<'_> {
        self.ask_in(call, action, &AskContext::default())
    }

    fn ask_in(
        &self,
        call: &ToolCall,
        action: &ToolAction,
        context: &AskContext,
    ) -> ReplyFuture<'_> {
        let summary = describe_call(call);
        let subject = action.subject.clone().unwrap_or_else(|| call.name.clone());
        let tool = call.name.clone();
        let class = action.class;
        let sandbox = self.sandbox.clone();
        let caveat_shown = Arc::clone(&self.caveat_shown);
        let context = context.clone();
        let call = call.clone();
        let root = self.root.clone();
        Box::pin(async move {
            let prompt = move || {
                let change = crate::diff::of_call(&call, Some(&root))
                    .filter(|rows| !rows.is_empty())
                    .map(|rows| {
                        crate::diff::paint(
                            &rows,
                            DIFF_LINES,
                            "      ",
                            crate::markdown::terminal_width().saturating_sub(1),
                        )
                    });
                let asking = Asking {
                    summary: &summary,
                    tool: &tool,
                    subject: &subject,
                    class,
                    context: &context,
                    change: change.as_deref(),
                };
                ask_on_terminal(&asking, &caveat_shown, &sandbox)
            };
            match tokio::task::spawn_blocking(prompt).await {
                Ok(reply) => reply,
                Err(error) => {
                    ApprovalReply::Reject(Some(format!("approval prompt failed: {error}")))
                }
            }
        })
    }
}

struct Asking<'a> {
    summary: &'a str,
    tool: &'a str,
    subject: &'a str,
    class: ToolClass,
    context: &'a AskContext,
    /// The diff of an edit, drawn and ready.
    change: Option<&'a str>,
}

/// One answer the selector offers: its key, its label and what it means.
struct Choice {
    key: char,
    label: String,
    reply: ApprovalReply,
}

/// How many of a turn's other pending calls are named in the prompt.
const NAMED_PENDING: usize = 3;

fn choices(asking: &Asking<'_>, always: &str) -> Vec<Choice> {
    let choice = |key, label: &str, reply| Choice {
        key,
        label: label.to_owned(),
        reply,
    };
    if asking.context.unconfined.is_some() {
        return vec![
            choice('y', "run it once, unconfined", ApprovalReply::Once),
            choice('n', "refuse", ApprovalReply::Reject(None)),
        ];
    }
    let mut choices = vec![choice('y', "once", ApprovalReply::Once)];
    let others = asking.context.also_pending.len();
    if others > 0 {
        choices.push(choice(
            't',
            &format!("all {} in this turn", others + 1),
            ApprovalReply::ForTurn,
        ));
    }
    if let Some(program) = &asking.context.program {
        choices.push(choice(
            'p',
            &format!("always for {program} *"),
            ApprovalReply::ForProgram,
        ));
    }
    choices.push(choice('a', always, ApprovalReply::ForSession));
    choices.push(choice('r', "remember", ApprovalReply::Remember));
    if asking.class == ToolClass::Process {
        choices.push(choice(
            's',
            "every command this session",
            ApprovalReply::AllProcesses,
        ));
    }
    choices.push(choice('n', "refuse", ApprovalReply::Reject(None)));
    choices
}

/// Asks on the terminal and refuses on anything that is not a clear yes.
///
/// A closed or non-interactive standard input reaches the same place: refusal.
/// An approval that a person did not give is not an approval.
fn ask_on_terminal(asking: &Asking<'_>, caveat_shown: &AtomicBool, sandbox: &str) -> ApprovalReply {
    // The keyboard is the prompt's alone until it has its answer.
    let _keyboard = crate::listen::hold();
    crate::render::others_wrote();
    let mut err = stderr().lock();
    // The one moment a person is directly addressed.
    let header = style::paint_with(RUST, &[BOLD], "  approval");
    let detail = style::paint(
        FAINT,
        &format!("      rule scope: {} on {}", asking.tool, asking.subject),
    );
    let (always, caveat) = match (asking.class, &asking.context.unconfined) {
        (_, Some(reason)) => (
            "",
            Some(format!(
                "      OUTSIDE THE SANDBOX: full network, your credentials and every file · \
                 reason: {reason}"
            )),
        ),
        _ => match asking.class {
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
                "      sends a request to {} · the URL can carry workspace text with it",
                asking.subject
            )),
        ),
        _ => ("always for this file", None),
        },
    };
    let pending = &asking.context.also_pending;
    let also = (!pending.is_empty()).then(|| {
        let mut named: Vec<&str> = pending
            .iter()
            .take(NAMED_PENDING)
            .map(String::as_str)
            .collect();
        if pending.len() > NAMED_PENDING {
            named.push("…");
        }
        format!("      also asking this turn: {}", named.join(" · "))
    });
    let written = writeln!(err, "\n{header}  {}", asking.summary)
        .and_then(|()| writeln!(err, "{detail}"))
        .and_then(|()| match &caveat {
            // Outside the sandbox is the one caveat that must not read as small print.
            Some(caveat) if asking.context.unconfined.is_some() => {
                writeln!(err, "{}", style::paint(style::WARN, caveat))
            }
            Some(caveat) => writeln!(err, "{}", style::paint(FAINT, caveat)),
            None => Ok(()),
        })
        .and_then(|()| match &also {
            Some(also) => writeln!(err, "{}", style::paint(FAINT, also)),
            None => Ok(()),
        })
        .and_then(|()| match asking.change {
            Some(change) => write!(err, "\n{change}\n"),
            None => Ok(()),
        })
        .and_then(|()| err.flush());
    if written.is_err() {
        return ApprovalReply::Reject(Some("could not present the approval prompt".into()));
    }
    drop(err);

    let choices = choices(asking, always);
    if stdin().is_terminal()
        && stderr().is_terminal()
        && let Some(reply) = select(&choices)
    {
        return reply;
    }
    read_typed_answer(&choices)
}

/// The selector: arrows or tab move, enter confirms, a choice's key answers at
/// once, and esc or ctrl-c refuses. Keys typed before the prompt appeared are
/// discarded, and any other key is ignored rather than taken as a refusal.
/// `None` when the terminal cannot be put into raw mode.
fn select(choices: &[Choice]) -> Option<ApprovalReply> {
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

    let refuse = choices.len() - 1;
    let mut current = 0usize;
    let mut drawn = false;
    let chosen = loop {
        draw_choices(choices, current, drawn);
        drawn = true;
        let Ok(Event::Key(key)) = read() else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break refuse,
            KeyCode::Esc => break refuse,
            KeyCode::Enter => break current,
            KeyCode::Left | KeyCode::Up | KeyCode::BackTab => {
                current = (current + choices.len() - 1) % choices.len();
            }
            KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                current = (current + 1) % choices.len();
            }
            KeyCode::Char(typed) => {
                if let Some(index) = choices
                    .iter()
                    .position(|choice| choice.key == typed.to_ascii_lowercase())
                {
                    break index;
                }
            }
            _ => {}
        }
    };
    // The list gives way to the answer, so the scrollback keeps one line.
    let answer = style::paint(FAINT, &format!("      › {}", choices[chosen].label));
    let mut err = stderr();
    let _ = write!(err, "\x1b[{}A\r\x1b[J{answer}\r\n", choices.len());
    let _ = err.flush();
    drop(raw);

    Some(match &choices[chosen].reply {
        ApprovalReply::Reject(_) => ApprovalReply::Reject(Some(refusal_reason())),
        reply => reply.clone(),
    })
}

/// Tells and asks the person at the terminal about a budget. With nobody
/// there to answer, the run stops.
pub struct TerminalPurse;

impl crate::spend::Purse for TerminalPurse {
    fn warn(&self, message: &str) {
        let _keyboard = crate::listen::hold();
        crate::render::others_wrote();
        eprintln!(
            "\n{}",
            style::paint(style::WARN, &format!("  budget  {message}"))
        );
    }

    fn approve(&self, message: &str) -> bool {
        if !stdin().is_terminal() {
            return false;
        }
        let _keyboard = crate::listen::hold();
        crate::render::others_wrote();
        eprintln!(
            "\n{}  {message}",
            style::paint_with(RUST, &[BOLD], "  budget")
        );
        let choices = [
            Choice {
                key: 'y',
                label: "carry on, as far again".to_owned(),
                reply: ApprovalReply::Once,
            },
            Choice {
                key: 'n',
                label: "stop here".to_owned(),
                reply: ApprovalReply::Reject(None),
            },
        ];
        let reply = match stderr().is_terminal().then(|| select(&choices)).flatten() {
            Some(reply) => reply,
            None => read_typed_answer(&choices),
        };
        matches!(reply, ApprovalReply::Once)
    }
}

/// Restores the terminal however the selector exits.
struct RawGuard;

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// One choice per line, so a narrow terminal never wraps the list. A redraw
/// moves back up over the list it drew last time.
fn draw_choices(choices: &[Choice], current: usize, redraw: bool) {
    let mut text = String::new();
    if redraw {
        let _ = write!(text, "\x1b[{}A", choices.len());
    }
    for (index, choice) in choices.iter().enumerate() {
        let line = format!("[{}] {}", choice.key, choice.label);
        let line = if index == current {
            style::paint_with(RUST, &[BOLD], &format!("    › {line}"))
        } else {
            style::paint(FAINT, &format!("      {line}"))
        };
        let _ = write!(text, "\r\x1b[2K{line}\r\n");
    }
    let mut err = stderr();
    let _ = write!(err, "{text}");
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
    let _ = crate::listen::cooked(|| stdin().lock().read_line(&mut reason));
    match reason.trim() {
        "" => "the operator refused this action".to_owned(),
        reason => format!("the operator refused this action: {reason}"),
    }
}

/// For a pipe or a terminal that cannot go raw: one typed line, where only a
/// clear answer approves.
fn read_typed_answer(choices: &[Choice]) -> ApprovalReply {
    let mut err = stderr().lock();
    let options: Vec<String> = choices
        .iter()
        .map(|choice| format!("[{}] {}", choice.key, choice.label))
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

    let answer = answer.trim().to_ascii_lowercase();
    let typed = match answer.as_str() {
        "yes" => "y",
        "always" => "a",
        "remember" => "r",
        other => other,
    };
    match choices
        .iter()
        .find(|choice| typed.len() == 1 && typed.starts_with(choice.key))
        .map(|choice| &choice.reply)
    {
        Some(ApprovalReply::Reject(_)) | None => {
            ApprovalReply::Reject(Some("the operator refused this action".into()))
        }
        Some(reply) => reply.clone(),
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
        crate::render::others_wrote();
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
