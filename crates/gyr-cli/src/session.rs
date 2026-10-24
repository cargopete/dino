//! The interactive session.
//!
//! Line-based rather than an alternate-screen interface, on purpose. The
//! transcript belongs in the terminal's scrollback, where a person goes to find
//! what the agent did an hour ago and where selection and copying still work.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context;
use anyhow::Result;
use gyr_core::Agent;
use gyr_core::ToolSet;
use gyr_model::ModelSession;
use gyr_protocol::StopReason;
use gyr_protocol::TokenUsage;
use rustyline::error::ReadlineError;
use tokio_util::sync::CancellationToken;

use crate::render::describe_usage;
use crate::render::status_block;
use crate::style;
use crate::style::FAINT;
use crate::style::MUTED;
use crate::style::SLATE;

/// Lets a line ending in a backslash continue onto the next.
///
/// Pasting already works: rustyline enables bracketed paste by default and
/// delivers a whole multi-line block as one buffer, which was measured rather
/// than assumed. What did not work was *composing* several lines, which is what
/// this is for, and the shell's convention is the one fingers already know.
#[derive(Default)]
struct Continuation;

impl rustyline::validate::Validator for Continuation {
    fn validate(
        &self,
        context: &mut rustyline::validate::ValidationContext<'_>,
    ) -> rustyline::Result<rustyline::validate::ValidationResult> {
        Ok(if ends_open(context.input()) {
            rustyline::validate::ValidationResult::Incomplete
        } else {
            rustyline::validate::ValidationResult::Valid(None)
        })
    }
}

/// True when the input ends in an odd number of backslashes, so the last one is
/// a continuation rather than an escaped backslash a person meant to keep.
fn ends_open(input: &str) -> bool {
    input.chars().rev().take_while(|c| *c == '\\').count() % 2 == 1
}

/// Removes the continuations a person typed, joining the lines they held apart.
fn join_continuations(input: &str) -> String {
    input.replace("\\\n", "\n")
}

impl rustyline::completion::Completer for Continuation {
    type Candidate = String;
}
impl rustyline::hint::Hinter for Continuation {
    type Hint = String;
}
impl rustyline::highlight::Highlighter for Continuation {}
impl rustyline::Helper for Continuation {}

/// What a person typed at the prompt.
#[derive(Debug, PartialEq, Eq)]
enum Input {
    Submit(String),
    Help,
    Status,
    Clear,
    Diff,
    Cost,
    Model(Option<String>),
    Resume(Option<String>),
    Log,
    Exit,
    Unknown(String),
    Blank,
}

/// Reads one line as either a submission or a command.
///
/// A leading slash is not enough on its own: `/usr/bin/env is on PATH, is that
/// a problem?` is a perfectly reasonable thing to ask an agent, and an earlier
/// rule ate it. A first word containing a second slash is therefore a path, and
/// a path is a submission.
///
/// A lone unknown word does become [`Input::Unknown`], so a mistyped `/exti`
/// gets a correction rather than being quietly sent to a model.
fn parse(line: &str) -> Input {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Input::Blank;
    }
    let submission = || Input::Submit(trimmed.to_owned());
    let Some(rest) = trimmed.strip_prefix('/') else {
        return submission();
    };
    let word = rest.split_whitespace().next().unwrap_or_default();
    if word.is_empty() || word.contains('/') {
        return submission();
    }
    // A known command word wins over whatever follows it, as in any shell.
    let argument = rest[word.len()..]
        .split_whitespace()
        .next()
        .map(str::to_owned);
    match word {
        "help" | "h" | "?" => Input::Help,
        "status" => Input::Status,
        "clear" => Input::Clear,
        "diff" => Input::Diff,
        "cost" => Input::Cost,
        "model" => Input::Model(argument),
        "resume" => Input::Resume(argument),
        "log" => Input::Log,
        "exit" | "quit" | "q" => Input::Exit,
        // A lone word is a mistyped command; a word with more after it is prose.
        other if rest.trim() == other => Input::Unknown(other.to_owned()),
        _ => submission(),
    }
}

/// Builds a fresh model session for a profile key, with its display name.
pub type SessionFactory<S> = Box<dyn Fn(&str) -> Result<(S, String)>>;

/// Everything a session needs that does not change between submissions.
pub struct Session<S> {
    pub agent: Agent<S, ToolSet>,
    pub usage: Arc<Mutex<TokenUsage>>,
    pub requests: Arc<std::sync::atomic::AtomicU64>,
    pub factory: SessionFactory<S>,
    pub model_key: String,
    pub workspace_root: std::path::PathBuf,
    pub run_id: String,
    /// Conversations started in this process after the first, for state names.
    pub conversations: u32,
    pub model: String,
    pub workspace: String,
    pub sandbox: String,
    pub approvals: String,
    pub log_path: String,
    pub history_path: std::path::PathBuf,
    /// Where this conversation is persisted after each submission.
    pub state_path: std::path::PathBuf,
}

impl<S> Session<S>
where
    S: ModelSession,
{
    /// Runs until the person leaves.
    ///
    /// # Errors
    ///
    /// Returns an error when the line editor cannot be started. A failed
    /// submission is reported and the session continues, because one bad turn
    /// is not a reason to throw away the conversation.
    pub async fn run(&mut self) -> Result<()> {
        let mut editor: rustyline::Editor<Continuation, rustyline::history::FileHistory> =
            rustyline::Editor::new().context("cannot start the line editor")?;
        editor.set_helper(Some(Continuation));
        let _ = editor.load_history(&self.history_path);

        println!("{}", self.banner());
        // The prompt is the person's own line, so it wears the person's colour.
        let prompt = match style::RUST.sequence() {
            sequence if sequence.is_empty() => "› ".to_owned(),
            sequence => format!("{sequence}› {}", style::RESET),
        };

        loop {
            // Blocking on purpose. Nothing else is running while a person is
            // typing, and moving the editor across a spawn_blocking boundary
            // every keystroke would buy nothing.
            match editor.readline(&prompt) {
                Ok(line) => {
                    let _ = editor.add_history_entry(line.as_str());
                    if self.handle(parse(&join_continuations(&line))).await? {
                        break;
                    }
                }
                // At an idle prompt Ctrl-C clears the line, as a shell does.
                Err(ReadlineError::Interrupted) => {}
                Err(ReadlineError::Eof) => break,
                Err(error) => return Err(error).context("cannot read from the terminal"),
            }
        }

        let _ = editor.save_history(&self.history_path);
        Ok(())
    }

    /// Acts on one input. Returns true when the session should end.
    async fn handle(&mut self, input: Input) -> Result<bool> {
        match input {
            Input::Blank => {}
            Input::Exit => return Ok(true),
            Input::Help => print!("{}", help()),
            Input::Status => println!("{}", self.banner()),
            Input::Clear => {
                let key = self.model_key.clone();
                self.start_conversation(&key);
            }
            Input::Model(None) => print!("{}", self.models()),
            Input::Model(Some(key)) => self.start_conversation(&key),
            Input::Resume(None) => print!("{}", self.saved()),
            Input::Resume(Some(id)) => self.resume(&id),
            Input::Diff => print!("{}", diff(&self.workspace_root)),
            Input::Cost => println!("{}", self.cost()),
            Input::Log => println!("{}", style::paint(MUTED, &self.log_path)),
            Input::Unknown(name) => println!(
                "{}",
                style::paint(FAINT, &format!("no such command: /{name}. Try /help."))
            ),
            Input::Submit(text) => self.submit(text).await,
        }
        Ok(false)
    }

    /// Runs one submission, cancellable on its own for its own lifetime.
    async fn submit(&mut self, text: String) {
        let cancel = CancellationToken::new();
        let signal = cancel.clone();
        let interrupt = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signal.cancel();
            }
        });

        let result = self.agent.run_cancellable(text, &cancel).await;

        // Aborted so a stale handler cannot cancel the next submission.
        interrupt.abort();

        // After each submission rather than continuously: a submission is the
        // unit that either happened or did not.
        if let Ok(state) = self.agent.session().export_state()
            && let Err(error) = gyr_core::resume::save(&self.state_path, &state)
        {
            println!(
                "{}",
                style::paint(
                    style::WARN,
                    &format!("  could not save the session: {error}")
                )
            );
        }

        match result {
            Ok(run) if run.stop_reason == StopReason::Cancelled => {
                println!(
                    "{}",
                    style::paint(FAINT, "  cancelled. The conversation is intact.")
                );
            }
            Ok(_) => {}
            Err(error) => {
                println!("{}", style::paint(style::WARN, &format!("  {error}")));
            }
        }
    }

    /// A new, empty conversation on `key`. The previous one stays saved and
    /// can be resumed by its id.
    fn start_conversation(&mut self, key: &str) {
        match (self.factory)(key) {
            Ok((session, name)) => {
                self.agent.replace_session(session);
                self.conversations += 1;
                let id = format!("{}-{}", self.run_id, self.conversations);
                // An empty conversation is never written, so there may be nothing to name.
                let previous = self.state_path.is_file().then(|| self.current_id());
                self.state_path = gyr_core::resume::state_path(&self.workspace_root, &id);
                key.clone_into(&mut self.model_key);
                self.model = name;
                let note = previous.map_or_else(String::new, |previous| {
                    format!(" The last one is /resume {previous}.")
                });
                println!(
                    "{}",
                    style::paint(
                        FAINT,
                        &format!("  new conversation on {}.{note}", self.model)
                    )
                );
            }
            Err(error) => println!("{}", style::paint(style::WARN, &format!("  {error}"))),
        }
    }

    fn resume(&mut self, id: &str) {
        let path = gyr_core::resume::state_path(&self.workspace_root, id);
        let restored = gyr_core::resume::load(&path)
            .map_err(anyhow::Error::from)
            .and_then(|state| {
                let (mut session, name) = (self.factory)(&state.model_key)?;
                session.restore_state(&state)?;
                Ok((session, name, state.model_key))
            });
        match restored {
            Ok((session, name, key)) => {
                self.agent.replace_session(session);
                self.state_path = path;
                self.model_key = key;
                self.model = name;
                println!(
                    "{}",
                    style::paint(FAINT, &format!("  resumed {id} on {}", self.model))
                );
            }
            Err(error) => println!("{}", style::paint(style::WARN, &format!("  {error}"))),
        }
    }

    fn current_id(&self) -> String {
        self.state_path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".state.json"))
            .unwrap_or_default()
            .to_owned()
    }

    fn models(&self) -> String {
        let mut block = String::new();
        for profile in gyr_model::builtin_profiles() {
            let marker = if profile.key == self.model_key {
                "›"
            } else {
                " "
            };
            let _ = writeln!(
                &mut block,
                "  {marker} {}  {}",
                style::paint(SLATE, &format!("{:<24}", profile.key)),
                style::paint(MUTED, &profile.display_name)
            );
        }
        block.push_str(&style::paint(
            FAINT,
            "  /model <key> starts a new conversation on that model.\n",
        ));
        block
    }

    fn saved(&self) -> String {
        let saved = match gyr_core::resume::recent(&self.workspace_root) {
            Ok(saved) if saved.is_empty() => return "  nothing saved yet\n".to_owned(),
            Ok(saved) => saved,
            Err(error) => return format!("  {error}\n"),
        };
        let current = self.current_id();
        let now = std::time::SystemTime::now();
        let mut block = String::new();
        for item in saved.iter().take(10) {
            let age = now
                .duration_since(item.modified)
                .map_or_else(|_| "now".to_owned(), |age| describe_age(age.as_secs()));
            let model = gyr_core::resume::load(&item.path)
                .map_or_else(|_| "?".to_owned(), |state| state.model_key);
            let marker = if item.id == current { "›" } else { " " };
            let _ = writeln!(
                &mut block,
                "  {marker} {}  {}",
                style::paint(SLATE, &format!("{:<22}", item.id)),
                style::paint(MUTED, &format!("{age:>8} ago  {model}"))
            );
        }
        block
    }

    fn cost(&self) -> String {
        let usage = self
            .usage
            .lock()
            .map_or_else(|_| TokenUsage::default(), |total| *total);
        let requests = self.requests.load(std::sync::atomic::Ordering::Relaxed);
        status_block(&[
            ("requests", requests.to_string()),
            ("tokens", describe_usage(usage)),
        ])
    }

    fn banner(&self) -> String {
        let usage = self
            .usage
            .lock()
            .map_or_else(|_| TokenUsage::default(), |total| *total);
        status_block(&[
            ("model", self.model.clone()),
            ("workspace", self.workspace.clone()),
            ("approvals", self.approvals.clone()),
            ("sandbox", self.sandbox.clone()),
            ("log", self.log_path.clone()),
            ("state", self.state_path.display().to_string()),
            ("tokens", describe_usage(usage)),
        ])
    }
}

fn describe_age(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// `git diff --stat` against HEAD, and how many files git does not track.
fn diff(workspace: &std::path::Path) -> String {
    let git = |arguments: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(arguments)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let Some(stat) = git(&["diff", "--stat", "HEAD"]) else {
        return "  not a git repository, or nothing committed yet\n".to_owned();
    };
    let untracked = git(&["status", "--porcelain"])
        .unwrap_or_default()
        .lines()
        .filter(|line| line.starts_with("??"))
        .count();
    let mut block = if stat.trim().is_empty() {
        "  no changes to tracked files\n".to_owned()
    } else {
        stat.lines().fold(String::new(), |mut block, line| {
            let _ = writeln!(&mut block, "  {line}");
            block
        })
    };
    if untracked > 0 {
        let _ = writeln!(&mut block, "  {untracked} untracked file(s)");
    }
    block
}

fn help() -> String {
    let rows = [
        ("/help", "these commands"),
        ("/status", "model, workspace, containment, approvals, log"),
        (
            "/clear",
            "start a new conversation; the old one stays resumable",
        ),
        (
            "/model",
            "list models; /model <key> switches, in a new conversation",
        ),
        (
            "/resume",
            "list saved conversations; /resume <id> continues one",
        ),
        ("/diff", "what has changed in the working tree"),
        ("/cost", "requests and tokens so far"),
        ("/log", "the path of this session's log"),
        ("/exit", "leave; Ctrl-D does the same"),
    ];
    let mut block = String::new();
    for (command, description) in rows {
        let _ = writeln!(
            &mut block,
            "  {}  {}",
            style::paint(SLATE, &format!("{command:<8}")),
            style::paint(MUTED, description)
        );
    }
    block.push_str(&style::paint(
        FAINT,
        "  Ctrl-C during a turn cancels it and keeps the conversation.\n",
    ));
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailing_backslash_keeps_the_line_open() {
        assert!(ends_open("first line \\"));
        assert!(ends_open("path\\\\ then \\"));
        // An even number is an escaped backslash a person meant to keep, not a
        // continuation. Getting this wrong swallows the next line.
        assert!(!ends_open("a windows path C:\\\\"));
        assert!(!ends_open("no backslash at all"));
    }

    #[test]
    fn continuations_are_removed_and_the_lines_joined() {
        assert_eq!(
            join_continuations("first\\\nsecond\\\nthird"),
            "first\nsecond\nthird"
        );
        // A pasted block has no continuations and must survive untouched.
        assert_eq!(
            join_continuations("thread panicked\nstack backtrace:"),
            "thread panicked\nstack backtrace:"
        );
    }

    #[test]
    fn a_leading_slash_with_a_letter_is_a_command() {
        assert_eq!(parse("/help"), Input::Help);
        assert_eq!(parse("  /status  "), Input::Status);
        assert_eq!(parse("/log"), Input::Log);
        assert_eq!(parse("/exit"), Input::Exit);
        assert_eq!(parse("/q"), Input::Exit);
        assert_eq!(parse("/nonsense"), Input::Unknown("nonsense".into()));
    }

    #[test]
    fn session_commands_take_at_most_one_word() {
        assert_eq!(parse("/clear"), Input::Clear);
        assert_eq!(parse("/diff"), Input::Diff);
        assert_eq!(parse("/cost"), Input::Cost);
        assert_eq!(parse("/model"), Input::Model(None));
        assert_eq!(
            parse("/model  claude-opus "),
            Input::Model(Some("claude-opus".into()))
        );
        assert_eq!(parse("/resume"), Input::Resume(None));
        assert_eq!(
            parse("/resume 1a16-2"),
            Input::Resume(Some("1a16-2".into()))
        );
    }

    #[test]
    fn a_lone_mistyped_command_is_corrected_rather_than_sent_to_a_model() {
        assert_eq!(parse("/exti"), Input::Unknown("exti".into()));
    }

    #[test]
    fn a_submission_that_merely_looks_like_a_path_is_a_submission() {
        // A person asking about a path should not have it eaten as a command.
        assert_eq!(
            parse("/usr/bin/env is on PATH, is that a problem?"),
            Input::Submit("/usr/bin/env is on PATH, is that a problem?".into())
        );
        assert_eq!(
            parse("what does src/lib.rs do?"),
            Input::Submit("what does src/lib.rs do?".into())
        );
        assert_eq!(
            parse("/etc/hosts"),
            Input::Submit("/etc/hosts".into()),
            "a bare path is a submission, not a command"
        );
        assert_eq!(
            parse("/log the whole thing for me"),
            Input::Log,
            "a known command wins over whatever follows it, as in any shell"
        );
    }

    #[test]
    fn an_empty_line_does_nothing() {
        assert_eq!(parse(""), Input::Blank);
        assert_eq!(parse("   \t "), Input::Blank);
    }
}
