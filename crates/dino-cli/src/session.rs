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
use dino_core::Agent;
use dino_core::ToolSet;
use dino_model::ModelSession;
use dino_protocol::StopReason;
use dino_protocol::TokenUsage;
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
struct Continuation {
    /// For completing `@path` against the workspace.
    root: std::path::PathBuf,
}

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

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _context: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        Ok(complete_at(line, pos, &self.root))
    }
}

/// Completes the `@path` word ending at `pos` against the workspace.
fn complete_at(line: &str, pos: usize, root: &std::path::Path) -> (usize, Vec<String>) {
    let before = &line[..pos.min(line.len())];
    let start = before
        .rfind(char::is_whitespace)
        .map_or(0, |index| index + 1);
    let Some(partial) = before[start..].strip_prefix('@') else {
        return (pos, Vec::new());
    };
    let (directory, prefix) = match partial.rfind('/') {
        Some(index) => (&partial[..=index], &partial[index + 1..]),
        None => ("", partial),
    };
    if directory.starts_with('/') || directory.split('/').any(|part| part == "..") {
        return (pos, Vec::new());
    }
    let Ok(entries) = std::fs::read_dir(root.join(directory)) else {
        return (pos, Vec::new());
    };
    let mut candidates: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let hidden = name.starts_with('.') && !prefix.starts_with('.');
            if hidden || !name.starts_with(prefix) {
                return None;
            }
            let slash = if entry.path().is_dir() { "/" } else { "" };
            Some(format!("@{directory}{name}{slash}"))
        })
        .collect();
    candidates.sort();
    (start, candidates)
}

/// Extensions read as images rather than as text, with their media types.
const IMAGE_TYPES: [(&str, &str); 5] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
];

/// The most an attached image may weigh; providers refuse more.
const MAX_IMAGE_BYTES: u64 = 5 * 1_024 * 1_024;

fn image_type(path: &std::path::Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    IMAGE_TYPES
        .iter()
        .find(|(known, _)| *known == extension)
        .map(|(_, media)| *media)
}

/// Images named as `@path`, which unlike text may sit outside the workspace:
/// a screenshot is usually on the desktop, and the person named it.
fn image_attachments(text: &str, root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let mut found: Vec<std::path::PathBuf> = Vec::new();
    for word in text.split_whitespace() {
        let Some(path) = word.strip_prefix('@') else {
            continue;
        };
        let path = path.trim_end_matches([',', ';', ':', ')', '!', '?', '"', '\'']);
        let resolved = match (path.strip_prefix("~/"), &home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ if path.starts_with('/') => std::path::PathBuf::from(path),
            _ => root.join(path),
        };
        if image_type(&resolved).is_some() && resolved.is_file() && !found.contains(&resolved) {
            found.push(resolved);
        }
    }
    found
}

/// Workspace-relative paths named as `@path` in `text` that exist, in order and
/// once each. Anything else with an `@` in it, like an address, is left alone.
fn attachments(text: &str, root: &std::path::Path) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        let Some(path) = word.strip_prefix('@') else {
            continue;
        };
        let path = path.trim_end_matches([',', '.', ';', ':', ')', '!', '?', '"', '\'']);
        let path = path.trim_end_matches('/');
        let escapes = path.starts_with('/') || path.split('/').any(|part| part == "..");
        if path.is_empty() || escapes || found.iter().any(|seen| seen == path) {
            continue;
        }
        if image_type(std::path::Path::new(path)).is_some() {
            continue;
        }
        if root.join(path).exists() {
            found.push(path.to_owned());
        }
    }
    found
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
    Plan(Option<String>),
    Compact,
    Init,
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
        "compact" => Input::Compact,
        "init" => Input::Init,
        "model" => Input::Model(argument),
        "resume" => Input::Resume(argument),
        "plan" => {
            Input::Plan(Some(rest[word.len()..].trim().to_owned()).filter(|task| !task.is_empty()))
        }
        "log" => Input::Log,
        "exit" | "quit" | "q" => Input::Exit,
        // A lone word is a mistyped command; a word with more after it is prose.
        other if rest.trim() == other => Input::Unknown(other.to_owned()),
        _ => submission(),
    }
}

/// What `/init` asks for: the instructions file a later session reads first.
const INIT_REQUEST: &str = "Survey this repository and write AGENTS.md at its root, for a coding \
    agent that works here later. Include what the project is, in two sentences; the exact \
    commands to build, test, lint and format it, checked by running them where that is safe; \
    which directories hold what; and the conventions you can see in the code, such as error \
    handling, naming and how tests are written. Leave out anything true of every project in \
    this language. Keep it under 60 lines, plain and specific. If AGENTS.md already exists, \
    improve it and keep what is still true.";

/// What `/compact` asks for. Specific, because a vague summary loses exactly
/// the file paths and decisions the next turn needs.
const COMPACT_REQUEST: &str = "This conversation is about to be compacted: you will continue \
    in a fresh one that sees only what you write now. Do not call any tool. Write a summary \
    under 400 words with these headings: Task (what was asked, in the person's words where it \
    matters), Done (what changed, with file paths), Build and tests (what passes, what fails, \
    the exact commands and failing test names), Code state (anything half-finished, and where), \
    Decisions (and why), Remaining (next steps), Open questions. Include exact names, paths, \
    commands and error messages that the next turn will need.";

/// Sent after a compaction that interrupted a task, so the work goes on.
pub const CONTINUE_AFTER_COMPACTION: &str = "The conversation was compacted in the middle of the \
    task. Carry on from the summary below.";

/// Sent ahead of a planned task. The gate enforces it; this explains it.
const PLAN_MODE: &str = "Plan mode. Investigate with read-only tools: read, search, list, \
    github and web_fetch. Edits and processes are refused until the person accepts a plan. \
    End with a short numbered plan, each step naming the files it touches, and stop there.";

/// Builds a fresh model session for a profile key, with its display name.
pub type SessionFactory<S> = Box<dyn Fn(&str) -> Result<(S, String)>>;

/// Everything a session needs that does not change between submissions.
pub struct Session<S> {
    pub agent: Agent<S, ToolSet>,
    pub usage: Arc<Mutex<TokenUsage>>,
    pub requests: Arc<std::sync::atomic::AtomicU64>,
    pub meter: Arc<crate::spend::Meter>,
    pub factory: SessionFactory<S>,
    pub model_key: String,
    pub workspace_root: std::path::PathBuf,
    pub run_id: String,
    /// Conversations started in this process after the first, for state names.
    pub conversations: u32,
    /// Shared with the approval policy: while set, nothing changes or runs.
    pub planning: Arc<std::sync::atomic::AtomicBool>,
    /// A plan has been proposed and the next line decides it.
    pub awaiting_plan: bool,
    /// A compacted summary waiting to go ahead of the next message.
    pub carried: Option<String>,
    /// What this conversation did, from its events, for the next compaction.
    pub recall: crate::recall::Recall,
    /// Compact after any submission that filled the context past the mark.
    pub auto_compact: bool,
    /// Set by the renderer when the core warns the context is filling.
    pub context_full: Arc<std::sync::atomic::AtomicBool>,
    /// The status row's figures, told which model is answering.
    pub status: Arc<Mutex<crate::spinner::Status>>,
    /// Messages typed during a run, shared with the agent, which delivers them.
    pub inbox: dino_core::Inbox,
    /// Typed during the last run and not entered, to start the next prompt with.
    pub typed_ahead: String,
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
        editor.set_helper(Some(Continuation {
            root: self.workspace_root.clone(),
        }));
        // A new line without sending: alt-enter, which is also what shift-enter
        // sends once a terminal is set to send ESC CR for it. Not ctrl-j: Enter
        // typed before the editor takes the terminal arrives as the same byte.
        editor.bind_sequence(
            rustyline::KeyEvent(rustyline::KeyCode::Enter, rustyline::Modifiers::ALT),
            rustyline::Cmd::Newline,
        );
        let _ = editor.load_history(&self.history_path);

        println!("{}", self.opening());
        // The prompt is the person's own line, so it wears the person's colour.
        let prompt = match style::RUST.sequence() {
            sequence if sequence.is_empty() => "› ".to_owned(),
            sequence => format!("{sequence}› {}", style::RESET),
        };

        loop {
            // Blocking on purpose. Nothing else is running while a person is
            // typing, and moving the editor across a spawn_blocking boundary
            // every keystroke would buy nothing.
            let mut typed = std::mem::take(&mut self.typed_ahead);
            typed.push_str(&crate::style::take_typeahead());
            match editor.readline_with_initial(&prompt, (&typed, "")) {
                Ok(line) => {
                    let _ = editor.add_history_entry(line.as_str());
                    let input = parse(&join_continuations(&line));
                    let input = if self.awaiting_plan {
                        self.decide_plan(input).await
                    } else {
                        input
                    };
                    if self.handle(input).await? {
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
            Input::Plan(None) => println!(
                "{}",
                style::paint(FAINT, "  /plan <task> plans it read-only, then asks you")
            ),
            Input::Plan(Some(task)) => {
                self.set_planning(true);
                self.submit(format!("{task}\n\n{PLAN_MODE}")).await;
                Self::ask_about_plan();
            }
            Input::Cost => println!("{}", self.cost()),
            Input::Compact => {
                self.compact().await;
            }
            Input::Log => println!("{}", style::paint(MUTED, &self.log_path)),
            Input::Unknown(name) => println!(
                "{}",
                style::paint(FAINT, &format!("no such command: /{name}. Try /help."))
            ),
            Input::Submit(text) => self.submit(text).await,
            Input::Init => self.submit(INIT_REQUEST.to_owned()).await,
        }
        Ok(false)
    }

    /// The text with each `@path` it names appended, read by the workspace's
    /// own `read` and `list` tools so the model sees exactly what those return,
    /// and the read counts as one before an edit.
    async fn with_attachments(&mut self, text: String) -> String {
        let text = self.attach_images(text);
        let paths = attachments(&text, &self.workspace_root);
        if paths.is_empty() {
            return text;
        }
        let mut message = text;
        message.push_str("\n\nAttached by the user:");
        for (index, path) in paths.iter().enumerate() {
            let tool = if self.workspace_root.join(path).is_dir() {
                "list"
            } else {
                "read"
            };
            let call = dino_protocol::ToolCall {
                id: format!("attach-{index}"),
                name: tool.to_owned(),
                arguments: serde_json::json!({ "path": path }),
            };
            let attached = dino_core::ToolRuntime::execute(self.agent.tools(), &call).await;
            match attached {
                Ok(output) => {
                    let _ = write!(
                        &mut message,
                        "\n\n### @{path} ({tool})\n\n{}",
                        output.content
                    );
                    println!(
                        "{}",
                        style::paint(
                            FAINT,
                            &format!("  attached {path} ({} bytes)", output.content.len())
                        )
                    );
                }
                Err(error) => println!(
                    "{}",
                    style::paint(style::WARN, &format!("  could not attach {path}: {error}"))
                ),
            }
        }
        message
    }

    /// Hands any `@image` to the agent for the next turn, noting each in the
    /// text, or says why it could not be sent.
    fn attach_images(&mut self, text: String) -> String {
        let paths = image_attachments(&text, &self.workspace_root);
        if paths.is_empty() {
            return text;
        }
        if !self.agent.session().profile().image_input {
            println!(
                "{}",
                style::paint(
                    style::WARN,
                    &format!(
                        "  {} does not take images, so none were attached",
                        self.model
                    )
                )
            );
            return text;
        }
        let mut message = text;
        let mut images = Vec::new();
        for path in paths {
            let shown = path.display();
            let size = std::fs::metadata(&path).map_or(0, |metadata| metadata.len());
            if size > MAX_IMAGE_BYTES {
                println!(
                    "{}",
                    style::paint(
                        style::WARN,
                        &format!("  {shown} is over 5 MiB; not attached")
                    )
                );
                continue;
            }
            match std::fs::read(&path) {
                Ok(bytes) => {
                    use base64::Engine as _;
                    images.push(dino_protocol::Image {
                        media_type: image_type(&path).unwrap_or("image/png").to_owned(),
                        data: base64::engine::general_purpose::STANDARD.encode(bytes),
                    });
                    let _ = write!(&mut message, "\n\n[image attached: {shown}]");
                    println!(
                        "{}",
                        style::paint(FAINT, &format!("  attached image {shown} ({size} bytes)"))
                    );
                }
                Err(error) => println!(
                    "{}",
                    style::paint(style::WARN, &format!("  could not read {shown}: {error}"))
                ),
            }
        }
        self.agent.attach_images(images);
        message
    }

    /// Runs one submission, cancellable on its own for its own lifetime.
    async fn submit(&mut self, text: String) {
        let text = self.with_attachments(text).await;
        // The person's words first, so the transcript shows what they typed.
        let text = match self.carried.take() {
            Some(summary) => format!("{text}{}{summary}", crate::recall::SUMMARY_DIVIDER),
            None => text,
        };
        let run = self.run_submission(text).await;
        if let Some(run) = &run {
            self.recall.absorb(&run.events);
        }
        // Entered during the run but never delivered, because no tool result
        // came after it: the next submission, in the order typed.
        let undelivered = self
            .inbox
            .lock()
            .map(|mut inbox| std::mem::take(&mut *inbox))
            .unwrap_or_default();
        if !undelivered.is_empty() {
            Box::pin(self.submit(undelivered.join("\n\n"))).await;
            return;
        }
        let full = self
            .context_full
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        if full && self.auto_compact {
            println!(
                "{}",
                style::paint(FAINT, "  the context is filling, so compacting now")
            );
            let compacted = self.compact().await;
            self.context_full
                .store(false, std::sync::atomic::Ordering::Relaxed);
            // A run the window cut short is resumed rather than left waiting.
            let interrupted = run.is_none_or(|run| run.stop_reason == StopReason::MaxTokens);
            if compacted && interrupted {
                Box::pin(self.submit(CONTINUE_AFTER_COMPACTION.to_owned())).await;
            }
        }
    }

    /// Runs one prepared message and saves the conversation after it.
    async fn run_submission(&mut self, text: String) -> Option<dino_core::RunResult> {
        // Each time, since `/model` and `/resume` change it and a run can learn
        // the window the server really serves.
        if let Ok(mut status) = self.status.lock() {
            status.answering(self.agent.session().profile());
        }
        let cancel = CancellationToken::new();
        let signal = cancel.clone();
        let interrupt = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signal.cancel();
            }
        });

        let listener = crate::listen::Listener::start(
            cancel.clone(),
            Arc::clone(&self.inbox),
            Arc::clone(&self.status),
        );
        let result = self.agent.run_cancellable(text, &cancel).await;
        self.typed_ahead = listener
            .map(crate::listen::Listener::stop)
            .unwrap_or_default();
        if let Ok(mut status) = self.status.lock() {
            status.typed.clear();
            status.queued = 0;
        }

        // Aborted so a stale handler cannot cancel the next submission.
        interrupt.abort();

        // After each submission rather than continuously: a submission is the
        // unit that either happened or did not.
        if let Ok(state) = self.agent.session().export_state()
            && let Err(error) = dino_core::resume::save(&self.state_path, &state)
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
                None
            }
            Ok(run) => Some(run),
            Err(error) => {
                println!("{}", style::paint(style::WARN, &format!("  {error}")));
                None
            }
        }
    }

    /// Asks for a summary with changes gated off, starts a new conversation,
    /// and carries the summary into the next message. The old conversation
    /// stays saved in full.
    async fn compact(&mut self) -> bool {
        let was_planning = self.planning.load(std::sync::atomic::Ordering::Relaxed);
        self.planning
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let run = self.run_submission(COMPACT_REQUEST.to_owned()).await;
        self.planning
            .store(was_planning, std::sync::atomic::Ordering::Relaxed);
        let summary = run
            .map(|run| run.text.trim().to_owned())
            .unwrap_or_default();
        if summary.is_empty() {
            println!(
                "{}",
                style::paint(
                    style::WARN,
                    "  no summary came back, so nothing was compacted"
                )
            );
            return false;
        }
        let summary = match self.recall.render() {
            Some(recalled) => format!("{summary}\n\n{recalled}"),
            None => summary,
        };
        let key = self.model_key.clone();
        self.start_conversation(&key);
        let words = summary.split_whitespace().count();
        self.carried = Some(summary);
        println!(
            "{}",
            style::paint(
                FAINT,
                &format!("  compacted into {words} words, which go ahead of your next message")
            )
        );
        true
    }

    fn set_planning(&mut self, planning: bool) {
        self.planning
            .store(planning, std::sync::atomic::Ordering::Relaxed);
        self.awaiting_plan = planning;
    }

    fn ask_about_plan() {
        println!(
            "{}",
            style::paint(
                SLATE,
                "  plan: [y] carry it out   [n] drop it   or type what to change"
            )
        );
    }

    /// Reads the line after a plan as the decision on it. Commands still work;
    /// anything else is feedback, and the plan is revised in plan mode.
    async fn decide_plan(&mut self, input: Input) -> Input {
        let Input::Submit(text) = input else {
            return input;
        };
        match text.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => {
                self.set_planning(false);
                Input::Submit("The plan is accepted. Carry it out, step by step.".to_owned())
            }
            "n" | "no" => {
                self.set_planning(false);
                println!(
                    "{}",
                    style::paint(FAINT, "  plan dropped; nothing was changed")
                );
                Input::Blank
            }
            _ => {
                self.submit(format!(
                    "{text}\n\n(Still in plan mode: revise the plan; do not change anything.)"
                ))
                .await;
                Self::ask_about_plan();
                Input::Blank
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
                self.recall = crate::recall::Recall::default();
                let id = format!("{}-{}", self.run_id, self.conversations);
                // An empty conversation is never written, so there may be nothing to name.
                let previous = self.state_path.is_file().then(|| self.current_id());
                self.state_path = dino_core::resume::state_path(&self.workspace_root, &id);
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
        let path = dino_core::resume::state_path(&self.workspace_root, id);
        let restored = dino_core::resume::load(&path)
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
        for profile in dino_model::builtin_profiles() {
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
        let saved = match dino_core::resume::recent(&self.workspace_root) {
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
            let model = dino_core::resume::load(&item.path)
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
            ("cost", self.meter.closing()),
        ])
    }

    /// What a person needs as a session opens: what is answering, where, and
    /// under which rules. The rest is in `/status`.
    fn opening(&self) -> String {
        let workspace = crate::render::short_path(
            std::path::Path::new(&self.workspace),
            std::path::Path::new(""),
        );
        let rule = style::paint(crate::style::FAINT, "  ────────");
        format!(
            "\n{}  {}\n{}\n  {}\n  {}\n  {}\n",
            style::paint_with(crate::style::RUST, &[crate::style::BOLD], "  DINO"),
            style::paint(crate::style::FAINT, env!("CARGO_PKG_VERSION")),
            rule,
            style::paint(crate::style::TEXT, &format!("{} · {workspace}", self.model)),
            style::paint(
                crate::style::MUTED,
                &format!(
                    "{} · {}",
                    crate::render::short_label(&self.approvals),
                    crate::render::short_label(&self.sandbox)
                )
            ),
            style::paint(crate::style::FAINT, "/help for commands · ctrl-d to leave"),
        )
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
        (
            "/plan",
            "/plan <task>: plan read-only, then accept, revise or drop it",
        ),
        ("/diff", "what has changed in the working tree"),
        (
            "/init",
            "survey the repository and write or improve AGENTS.md",
        ),
        ("/cost", "requests, tokens and list price so far"),
        (
            "/compact",
            "summarise, then carry on in a fresh conversation",
        ),
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
    fn plan_takes_the_rest_of_the_line_as_its_task() {
        assert_eq!(
            parse("/plan add a --json flag to dino models"),
            Input::Plan(Some("add a --json flag to dino models".into()))
        );
        assert_eq!(parse("/plan"), Input::Plan(None));
    }

    #[test]
    fn session_commands_take_at_most_one_word() {
        assert_eq!(parse("/clear"), Input::Clear);
        assert_eq!(parse("/diff"), Input::Diff);
        assert_eq!(parse("/cost"), Input::Cost);
        assert_eq!(parse("/compact"), Input::Compact);
        assert_eq!(parse("/init"), Input::Init);
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

    fn scratch(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("dino-at-{}-{name}", std::process::id()));
        std::fs::create_dir_all(root.join("src/bin")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        root
    }

    #[test]
    fn only_at_words_naming_something_in_the_workspace_are_attached() {
        let root = scratch("attach");

        let found = attachments(
            "fix @src/lib.rs, see @Cargo.toml and @src/lib.rs again; mail me@example.com \
             or @missing.rs or @../etc/passwd or @/etc/hosts and look in @src/bin.",
            &root,
        );

        assert_eq!(found, ["src/lib.rs", "Cargo.toml", "src/bin"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn image_words_resolve_inside_or_outside_the_workspace() {
        let root = scratch("images");
        std::fs::write(root.join("shot.png"), b"\x89PNG").unwrap();
        let outside = std::env::temp_dir().join(format!("dino-outside-{}.jpg", std::process::id()));
        std::fs::write(&outside, b"\xff\xd8").unwrap();

        let text = format!(
            "look at @shot.png and @{} but not @missing.png or @src/lib.rs",
            outside.display()
        );
        let found = image_attachments(&text, &root);

        assert_eq!(found, [root.join("shot.png"), outside.clone()]);
        assert!(
            !attachments(&text, &root)
                .iter()
                .any(|path| path == "shot.png")
        );
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn completion_offers_workspace_paths_after_an_at() {
        let root = scratch("complete");

        let (start, candidates) = complete_at("look at @sr", 11, &root);
        assert_eq!((start, candidates), (8, vec!["@src/".to_owned()]));
        let (_, candidates) = complete_at("@src/l", 6, &root);
        assert_eq!(candidates, ["@src/lib.rs"]);
        let (_, candidates) = complete_at("no at here", 10, &root);
        assert!(candidates.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_line_does_nothing() {
        assert_eq!(parse(""), Input::Blank);
        assert_eq!(parse("   \t "), Input::Blank);
    }
}
