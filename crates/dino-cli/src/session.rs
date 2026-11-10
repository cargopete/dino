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
use crate::style::PURPLE;

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
    /// Switch, when a key is given, and start later sessions on the model.
    SaveModel(Option<String>),
    Resume(Option<String>),
    Rename(Option<String>),
    Rewind(Option<String>),
    Fork(Option<String>),
    Plan(Option<String>),
    Compact,
    Auto,
    Init,
    Log,
    Schedules,
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
    // Said to a shell, never meant for a model.
    if matches!(trimmed, "exit" | "quit") {
        return Input::Exit;
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
    let remainder = || Some(rest[word.len()..].trim().to_owned()).filter(|text| !text.is_empty());
    match word {
        "help" | "h" | "?" => Input::Help,
        "status" => Input::Status,
        "clear" => Input::Clear,
        "diff" => Input::Diff,
        "cost" => Input::Cost,
        "compact" => Input::Compact,
        "auto" => Input::Auto,
        "init" => Input::Init,
        "model" => {
            let mut words: Vec<&str> = rest[word.len()..].split_whitespace().collect();
            let default = words.contains(&"--default");
            words.retain(|word| *word != "--default");
            let key = words.first().map(|key| (*key).to_owned());
            if default {
                Input::SaveModel(key)
            } else {
                Input::Model(key)
            }
        }
        "resume" => Input::Resume(remainder()),
        "rename" => Input::Rename(remainder()),
        "rewind" => Input::Rewind(remainder()),
        "fork" => Input::Fork(remainder()),
        "plan" => Input::Plan(remainder()),
        "log" => Input::Log,
        "schedules" => Input::Schedules,
        "exit" | "quit" | "q" => Input::Exit,
        // A lone word is a mistyped command; a word with more after it is prose.
        other if rest.trim() == other => Input::Unknown(other.to_owned()),
        _ => submission(),
    }
}

/// Whether a line is for the session rather than the model.
pub(crate) fn is_command(line: &str) -> bool {
    !matches!(parse(line), Input::Submit(_) | Input::Blank)
}

/// Lines entered during a run, as the inputs they become after it: a command
/// on its own, and prose between commands as one message.
fn queue_lines(lines: Vec<String>) -> Vec<String> {
    let mut queued = Vec::new();
    let mut prose: Vec<String> = Vec::new();
    for line in lines {
        if is_command(&line) {
            if !prose.is_empty() {
                queued.push(std::mem::take(&mut prose).join("\n\n"));
            }
            queued.push(line);
        } else {
            prose.push(line);
        }
    }
    if !prose.is_empty() {
        queued.push(prose.join("\n\n"));
    }
    queued
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

/// The summary a task seen through is compacted into: the report it ended on,
/// the branch's commits, and what the recall saw.
fn seeded_summary(
    report: &str,
    commits: Option<(String, String)>,
    recalled: Option<String>,
) -> String {
    let mut summary = format!(
        "A task seen through ended its last submission with this report:\n\n{}",
        report.trim()
    );
    if let Some((range, log)) = commits {
        let _ = write!(
            summary,
            "\n\nCommits on this branch (`git log {range}`):\n\n{log}"
        );
    }
    if let Some(recalled) = recalled {
        let _ = write!(summary, "\n\n{recalled}");
    }
    summary
}

/// Sent after a compaction that interrupted a task, so the work goes on.
pub const CONTINUE_AFTER_COMPACTION: &str = "The conversation was compacted in the middle of the \
    task. Carry on from the summary below.";

/// Sent ahead of a planned task. The gate enforces it; this explains it.
const PLAN_MODE: &str = "Plan mode. Investigate with read-only tools: read, search, list, \
    github and web_fetch. Edits and processes are refused until the person accepts a plan. \
    End with a short numbered plan, each step naming the files it touches, and stop there.";

/// How often a session looks for schedules come due.
const SCHEDULE_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// A session's schedules, and the conversation they are stamped with.
pub struct Due {
    pub store: dino_core::schedule::Schedules,
    /// Shared with the schedule tools, and kept to the conversation open.
    pub conversation: dino_core::schedule::Conversation,
    pub clock: dino_core::schedule::Clock,
    /// The last error reading them, so a poll does not repeat it.
    pub reported: Option<String>,
}

/// Says at the prompt, as it happens, that a schedule has come due. The line
/// editor cannot be interrupted, so it is sent once the person presses Enter.
struct PromptWatch {
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl PromptWatch {
    fn start(
        mut printer: impl rustyline::ExternalPrinter + Send + 'static,
        due: &Due,
        at_prompt: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        use std::sync::atomic::Ordering;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let (store, conversation, clock) = (
            due.store.clone(),
            Arc::clone(&due.conversation),
            Arc::clone(&due.clock),
        );
        std::thread::spawn(move || {
            let mut announced = std::collections::HashSet::new();
            while !stopping.load(Ordering::Relaxed) {
                std::thread::sleep(SCHEDULE_POLL);
                if !at_prompt.load(Ordering::Relaxed) {
                    continue;
                }
                let open = conversation.lock().map(|id| id.clone()).unwrap_or_default();
                for schedule in store.due(clock(), &open).unwrap_or_default() {
                    if announced.insert((schedule.id.clone(), schedule.next_due)) {
                        let line = format!(
                            "  {} is due: {}. Enter sends it.",
                            schedule.id,
                            dino_core::text::capped_head(schedule.message.trim(), 120)
                        );
                        let _ = printer.print(format!("{}\n", style::paint(PURPLE, &line)));
                    }
                }
            }
        });
        Self { stop }
    }
}

impl Drop for PromptWatch {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Builds a fresh model session for a profile key, with its display name.
pub type SessionFactory<S> = Box<dyn Fn(&str) -> Result<(S, String)>>;

/// Everything a session needs that does not change between submissions.
// Each switch is a separate thing a person turns on or off.
#[allow(clippy::struct_excessive_bools)]
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
    /// When to compact without being asked.
    pub compaction: crate::config::Compaction,
    /// Every submission is a task to see through, as `/auto` switches.
    pub keep_going: bool,
    /// The last run handed back with a next step in hand, so a go-ahead
    /// continues it as a task to see through.
    pub proposed: bool,
    /// Model turns one submission may take, for the banner.
    pub max_turns: u32,
    /// Set by the renderer when the core warns the context is filling.
    pub context_full: Arc<std::sync::atomic::AtomicBool>,
    /// The status row's figures, told which model is answering.
    pub status: Arc<Mutex<crate::spinner::Status>>,
    /// Messages typed during a run, shared with the agent, which delivers them.
    pub inbox: dino_core::Inbox,
    /// Typed during the last run and not entered, to start the next prompt with.
    pub typed_ahead: String,
    /// Entered during the last run and not delivered, run before the prompt.
    pub queued: std::collections::VecDeque<String>,
    pub model: String,
    pub workspace: String,
    pub sandbox: String,
    pub approvals: String,
    pub log_path: String,
    /// Pinged when a task ends, if it ran long enough.
    pub end_hooks: Vec<crate::task::EndHook>,
    pub history_path: std::path::PathBuf,
    /// Where this conversation is persisted after each submission.
    pub state_path: std::path::PathBuf,
    /// What `/rename` called this conversation, saved with it.
    pub name: Option<String>,
    /// A checkpoint `/rewind` previewed, and the next line says how to go back.
    pub rewinding: Option<u32>,
    /// Open with the dinosaur rather than the plain heading.
    pub greeting: bool,
    /// Set when pages are watching and driving this session, as `dino serve`.
    pub remote: Option<Arc<crate::remote::Hub>>,
    /// Set when the session offers the schedule tools.
    pub schedules: Option<Due>,
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
        if let Some(offer) = self.offer_resume() {
            println!("{offer}");
        }
        self.announce_waiting();
        let at_prompt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _watch = self.schedules.as_ref().and_then(|due| {
            let printer = editor.create_external_printer().ok()?;
            Some(PromptWatch::start(printer, due, Arc::clone(&at_prompt)))
        });
        // The prompt is the person's own line, so it wears the person's colour.
        let prompt = match style::EMBER.sequence() {
            sequence if sequence.is_empty() => "› ".to_owned(),
            sequence => format!("{sequence}› {}", style::RESET),
        };

        loop {
            if self.queued.is_empty() && !self.awaiting_plan {
                self.deliver_due().await;
            }
            let line = if let Some(queued) = self.queued.pop_front() {
                println!("{prompt}{queued}");
                queued
            } else {
                // Blocking on purpose. Nothing else is running while a person is
                // typing, and moving the editor across a spawn_blocking boundary
                // every keystroke would buy nothing.
                let mut typed = std::mem::take(&mut self.typed_ahead);
                typed.push_str(&crate::style::take_typeahead());
                at_prompt.store(true, std::sync::atomic::Ordering::Relaxed);
                let read = editor.readline_with_initial(&prompt, (&typed, ""));
                at_prompt.store(false, std::sync::atomic::Ordering::Relaxed);
                match read {
                    Ok(line) => {
                        let _ = editor.add_history_entry(line.as_str());
                        line
                    }
                    // At an idle prompt Ctrl-C clears the line, as a shell does.
                    Err(ReadlineError::Interrupted) => continue,
                    Err(ReadlineError::Eof) => break,
                    Err(error) => return Err(error).context("cannot read from the terminal"),
                }
            };
            let input = parse(&join_continuations(&line));
            let input = if let Some(number) = self.rewinding.take() {
                self.decide_rewind(number, input)
            } else if self.awaiting_plan {
                self.decide_plan(input).await
            } else {
                input
            };
            if self.handle(input).await? {
                break;
            }
        }

        let _ = editor.save_history(&self.history_path);
        Ok(())
    }

    /// Runs what the pages ask until the person at the terminal interrupts
    /// while it is idle, or a page picks another workspace, which is returned.
    /// There is no line editor and no slash command: a page sends prompts,
    /// answers approvals and picks from lists, and the rest stays at the
    /// terminal.
    ///
    /// # Errors
    ///
    /// Returns an error when this is not a remote session.
    pub async fn serve(
        &mut self,
        requests: &mut tokio::sync::mpsc::Receiver<crate::remote::Request>,
    ) -> Result<Option<std::path::PathBuf>> {
        let hub = self
            .remote
            .clone()
            .context("this session has no remote hub")?;
        self.describe_to(&hub);
        hub.set_busy(false);
        println!("{}", self.opening());
        self.announce_waiting();
        loop {
            // Busy while a schedule runs, so a page's resume or move waits its turn.
            if self.deliver_due().await {
                self.drain_queued_remotely().await;
                hub.set_busy(false);
            }
            let watching = self.schedules.is_some();
            let request = tokio::select! {
                request = requests.recv() => request,
                () = tokio::time::sleep(SCHEDULE_POLL), if watching => continue,
                _ = tokio::signal::ctrl_c() => None,
            };
            let Some(request) = request else { break };
            match request {
                crate::remote::Request::Prompt(prompt) => {
                    hub.set_busy(true);
                    self.submit(prompt).await;
                    self.drain_queued_remotely().await;
                }
                crate::remote::Request::Resume(id) => {
                    if self.resume(&id) {
                        self.describe_to(&hub);
                        hub.notice(&format!("resumed {id} on {}", self.model));
                    } else {
                        hub.notice(&format!("could not resume {id}; the terminal says why"));
                    }
                }
                crate::remote::Request::Workspace(path) => return Ok(Some(path)),
            }
            hub.set_busy(false);
        }
        Ok(None)
    }

    fn describe_to(&self, hub: &crate::remote::Hub) {
        let log = std::path::Path::new(&self.log_path)
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".jsonl"))
            .unwrap_or_default()
            .to_owned();
        hub.describe(crate::remote::Info {
            model: self.model.clone(),
            workspace: self.workspace.clone(),
            sandbox: self.sandbox.clone(),
            approvals: self.approvals.clone(),
            conversation: self.current_id(),
            log,
            root: self.workspace_root.clone(),
        });
    }

    /// Runs what was entered during a served run, which has no commands.
    async fn drain_queued_remotely(&mut self) {
        while let Some(line) = self.queued.pop_front() {
            if is_command(&line) {
                println!(
                    "{}",
                    style::paint(FAINT, &format!("  {line}: not a command while serving"))
                );
            } else {
                self.submit(line).await;
            }
        }
    }

    /// Sends what has come due for the open conversation, each as a submission
    /// of its own, in the order it fell due. Returns whether it sent any, having
    /// marked a served session busy first.
    async fn deliver_due(&mut self) -> bool {
        let open = self.current_id();
        let Some(due) = &mut self.schedules else {
            return false;
        };
        let found = due.store.due((due.clock)(), &open);
        let schedules = match found {
            Ok(schedules) => {
                due.reported = None;
                schedules
            }
            Err(error) => {
                let error = error.to_string();
                if due.reported.as_ref() != Some(&error) {
                    println!("{}", style::paint(style::WARN, &format!("  {error}")));
                    due.reported = Some(error);
                }
                return false;
            }
        };
        let (store, clock) = (due.store.clone(), Arc::clone(&due.clock));
        let mut sent = false;
        for schedule in schedules {
            match store.claim(&schedule, clock()) {
                Ok(Some(text)) => {
                    if let Some(hub) = &self.remote {
                        hub.set_busy(true);
                    }
                    sent = true;
                    let said = format!("{} came due; sending it", schedule.id);
                    self.tell(&said);
                    println!("{}", style::paint(PURPLE, &format!("  {said}")));
                    self.submit(text).await;
                }
                Ok(None) => {}
                Err(error) => {
                    println!("{}", style::paint(style::WARN, &format!("  {error}")));
                }
            }
        }
        sent
    }

    /// Names the schedules due for conversations other than this one, which
    /// wait until a session resumes theirs.
    fn announce_waiting(&self) {
        let Some(due) = &self.schedules else {
            return;
        };
        let waiting = match due
            .store
            .waiting_elsewhere((due.clock)(), &self.current_id())
        {
            Ok(waiting) => waiting,
            Err(error) => {
                println!("{}", style::paint(style::WARN, &format!("  {error}")));
                return;
            }
        };
        if waiting.is_empty() {
            return;
        }
        let named = waiting
            .iter()
            .map(|schedule| format!("{} for {}", schedule.id, schedule.conversation))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "{}",
            style::paint(
                style::WARN,
                &format!(
                    "  due for other conversations: {named}. /resume <id> sends them; \
                     /schedules lists every one."
                )
            )
        );
    }

    /// Every schedule in the workspace, and how many deliveries were recorded.
    fn schedules(&self) -> String {
        let Some(due) = &self.schedules else {
            return style::paint(FAINT, "  this session has no schedule tools\n");
        };
        let open = self.current_id();
        let mut block = String::new();
        match due.store.all() {
            Ok(schedules) if schedules.is_empty() => {
                block.push_str(&style::paint(FAINT, "  no schedules in this workspace\n"));
            }
            Ok(schedules) => {
                for schedule in schedules {
                    let _ = writeln!(
                        &mut block,
                        "  {}",
                        style::paint(MUTED, &schedule.summary(&open))
                    );
                }
            }
            Err(error) => {
                let _ = writeln!(
                    &mut block,
                    "  {}",
                    style::paint(style::WARN, &error.to_string())
                );
            }
        }
        let recorded = due.store.deliveries().len();
        if recorded > 0 {
            let _ = writeln!(
                &mut block,
                "{}",
                style::paint(
                    FAINT,
                    &format!(
                        "  {recorded} deliveries recorded in .dino/schedules/deliveries.jsonl; \
                         each says a message was sent, not that its work was done"
                    )
                )
            );
        }
        block
    }

    /// Tells the schedule tools which conversation is now open.
    pub fn note_conversation(&self) {
        if let Some(due) = &self.schedules
            && let Ok(mut open) = due.conversation.lock()
        {
            *open = self.current_id();
        }
    }

    /// Says something to the pages, when there are any.
    fn tell(&self, text: &str) {
        if let Some(hub) = &self.remote {
            hub.notice(text);
        }
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
            Input::SaveModel(key) => {
                if let Some(key) = key {
                    self.start_conversation(&key);
                    if self.model_key != key {
                        return Ok(false);
                    }
                }
                println!("{}", self.save_default_model());
            }
            Input::Resume(None) => print!("{}", self.saved()),
            Input::Resume(Some(wanted)) => {
                self.resume(&wanted);
            }
            Input::Rename(name) => self.rename(name),
            Input::Rewind(None) => print!("{}", self.checkpoints()),
            Input::Rewind(Some(number)) => self.preview_rewind(&number),
            Input::Fork(name) => self.fork(name),
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
            Input::Auto => {
                self.keep_going = !self.keep_going;
                println!("{}", style::paint(FAINT, &self.keep_going_banner()));
            }
            Input::Log => println!("{}", style::paint(MUTED, &self.log_path)),
            Input::Schedules => print!("{}", self.schedules()),
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
        let typed = text.clone();
        // Before attachments, which are file contents and name no limits.
        let (limits, named) = self.meter.begin_task(&text);
        if named {
            println!(
                "{}",
                style::paint(FAINT, &format!("  task budget: {}", limits.describe()))
            );
        }
        let go_ahead = self.proposed && crate::task::is_go_ahead(&text);
        // Not while planning: a plan is where that run is meant to stop.
        let finishing = (self.keep_going || go_ahead || crate::task::wants_to_finish(&text))
            && !self.planning.load(std::sync::atomic::Ordering::Relaxed);
        self.agent.set_keep_going(finishing);
        let text = if finishing {
            crate::task::note_if_behind(&self.workspace_root, text).await
        } else {
            text
        };
        let text = self.with_attachments(text).await;
        // The person's words first, so the transcript shows what they typed.
        let text = match self.carried.take() {
            Some(summary) => format!("{text}{}{summary}", crate::recall::SUMMARY_DIVIDER),
            None => text,
        };
        let checkpoint = self.checkpoint(&typed);
        let run = self.run_submission(text).await;
        if let Some(number) = checkpoint
            && let Err(error) =
                dino_core::checkpoint::end(&self.workspace_root, &self.current_id(), number)
        {
            println!(
                "{}",
                style::paint(style::WARN, &format!("  checkpoint {number}: {error}"))
            );
        }
        if let Some(run) = &run {
            self.recall.absorb(&run.events);
        }
        self.proposed = run.as_ref().is_some_and(|run| {
            run.stop_reason == StopReason::EndTurn
                && (finishing || crate::task::proposes_next_step(&run.text))
        });
        let full = self
            .context_full
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        let large = self.compaction.at_tokens().is_some_and(|at| {
            run.as_ref()
                .and_then(|run| dino_core::context_tokens(&run.events))
                .is_some_and(|tokens| tokens >= at)
        });
        // A task seen through has just written its report, which is the summary.
        if finishing
            && (full || large)
            && self.compaction != crate::config::Compaction::Manual
            && let Some(run) = run
                .as_ref()
                .filter(|run| run.stop_reason != StopReason::MaxTokens)
        {
            self.compact_from_report(&dino_core::final_message(&run.events));
            return;
        }
        let automatic = matches!(self.compaction, crate::config::Compaction::Automatic { .. });
        if (full || large) && automatic {
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

    /// Queues what was entered during the run but never delivered, because no
    /// tool result came after it, then what the listener held back: in the
    /// order typed.
    fn queue_undelivered(&mut self, held: Vec<String>) {
        let mut undelivered = self
            .inbox
            .lock()
            .map(|mut inbox| std::mem::take(&mut *inbox))
            .unwrap_or_default();
        undelivered.extend(held);
        self.queued.extend(queue_lines(undelivered));
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
        // A page's stop is the terminal's ctrl-c.
        let from_page = self.remote.clone().map(|hub| {
            let signal = cancel.clone();
            tokio::spawn(async move {
                hub.cancelled().await;
                signal.cancel();
            })
        });
        let result = self.agent.run_cancellable(text, &cancel).await;
        if let Some(from_page) = from_page {
            from_page.abort();
        }
        let stopped = listener
            .map(crate::listen::Listener::stop)
            .unwrap_or_default();
        self.typed_ahead = stopped.typed;
        self.queue_undelivered(stopped.held);
        if let Ok(mut status) = self.status.lock() {
            status.typed.clear();
            status.queued = 0;
        }

        // Aborted so a stale handler cannot cancel the next submission.
        interrupt.abort();

        // A limit stopped the task: ask once what is done and what is left.
        let stopped = result
            .is_err()
            .then(|| self.meter.task().stopped())
            .flatten();
        let mut result = result;
        if let Some(reason) = &stopped {
            self.tell(&format!("stopped: {reason}"));
            println!(
                "{}",
                style::paint(style::WARN, &format!("  stopped: {reason}"))
            );
            if let Some(run) = crate::task::wrap_up(
                &mut self.agent,
                &self.meter,
                &self.planning,
                reason,
                &cancel,
            )
            .await
            {
                result = Ok(run);
            }
        }
        crate::task::conclude(
            &self.end_hooks,
            &self.workspace_root,
            &self.model_key,
            &self.meter,
            &result,
            stopped,
        )
        .await;

        // After each submission rather than continuously: a submission is the
        // unit that either happened or did not.
        if let Ok(state) = self.agent.session().export_state()
            && let Err(error) =
                dino_core::resume::save(&self.state_path, &state, self.name.as_deref())
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
                self.tell("cancelled; the conversation is intact");
                println!(
                    "{}",
                    style::paint(FAINT, "  cancelled. The conversation is intact.")
                );
                None
            }
            Ok(run) => Some(run),
            Err(error) => {
                self.tell(&error.to_string());
                println!("{}", style::paint(style::WARN, &format!("  {error}")));
                None
            }
        }
    }

    /// Asks for a summary with changes gated off, starts a new conversation,
    /// and carries the summary into the next message. The old conversation
    /// stays saved in full.
    async fn compact(&mut self) -> bool {
        self.agent.set_keep_going(false);
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

    /// Compacts with no request to the model: the summary is the report a task
    /// seen through has just ended on, and the commits it made.
    fn compact_from_report(&mut self, report: &str) {
        let summary = seeded_summary(
            report,
            dino_core::git::branch_log(&self.workspace_root),
            self.recall.render(),
        );
        let key = self.model_key.clone();
        self.start_conversation(&key);
        let words = summary.split_whitespace().count();
        self.carried = Some(summary);
        println!(
            "{}",
            style::paint(
                FAINT,
                &format!(
                    "  compacted at the end of the task into its report and commits ({words} \
                     words), which go ahead of your next message"
                )
            )
        );
    }

    /// The one line that says whether every submission is seen through, and
    /// within what.
    fn keep_going_banner(&self) -> String {
        if self.keep_going {
            format!(
                "  keep going: every message is a task to see through, up to {} turns and {} \
                 each. /auto turns it off.",
                self.max_turns,
                self.meter.task().defaults().describe()
            )
        } else {
            "  keep going is off: a message ends when the model hands back. /auto turns it on."
                .to_owned()
        }
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
                PURPLE,
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
                self.name = None;
                let id = format!("{}-{}", self.run_id, self.conversations);
                // An empty conversation is never written, so there may be nothing to name.
                let previous = self.state_path.is_file().then(|| self.current_id());
                self.state_path = dino_core::resume::state_path(&self.workspace_root, &id);
                self.note_conversation();
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

    fn resume(&mut self, wanted: &str) -> bool {
        let found = match dino_core::resume::find(&self.workspace_root, wanted) {
            Ok(Some(found)) => found,
            Ok(None) => {
                println!(
                    "{}",
                    style::paint(
                        style::WARN,
                        &format!("  no saved conversation is called {wanted}; /resume lists them")
                    )
                );
                return false;
            }
            Err(error) => {
                println!("{}", style::paint(style::WARN, &format!("  {error}")));
                return false;
            }
        };
        let (id, path) = found;
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
                self.name = dino_core::resume::label(&path)
                    .ok()
                    .and_then(|label| label.name);
                self.state_path = path;
                self.note_conversation();
                self.model_key = key;
                self.model = name;
                println!(
                    "{}",
                    style::paint(FAINT, &format!("  resumed {id} on {}", self.model))
                );
                true
            }
            Err(error) => {
                println!("{}", style::paint(style::WARN, &format!("  {error}")));
                false
            }
        }
    }

    /// Names the conversation, or with no name clears it. A conversation
    /// already on disk is saved again now, so the name is not lost to an exit.
    fn rename(&mut self, name: Option<String>) {
        self.name = name;
        if self.state_path.is_file()
            && let Ok(state) = self.agent.session().export_state()
            && let Err(error) =
                dino_core::resume::save(&self.state_path, &state, self.name.as_deref())
        {
            println!(
                "{}",
                style::paint(style::WARN, &format!("  could not save the name: {error}"))
            );
            return;
        }
        let said = match &self.name {
            Some(name) => format!("  this conversation is {name}; /resume {name} continues it"),
            None => "  the name is cleared".to_owned(),
        };
        println!("{}", style::paint(FAINT, &said));
    }

    /// Reads the line after a preview as how to go back. A command runs as
    /// usual; anything else leaves everything as it is.
    fn decide_rewind(&mut self, number: u32, input: Input) -> Input {
        let Input::Submit(text) = &input else {
            return input;
        };
        let (files, conversation) = match text.trim().to_ascii_lowercase().as_str() {
            "c" | "conversation" => (false, true),
            "f" | "files" => (true, false),
            "b" | "both" => (true, true),
            _ => {
                println!("{}", style::paint(FAINT, "  nothing was rewound"));
                return Input::Blank;
            }
        };
        self.rewind(number, files, conversation);
        Input::Blank
    }

    /// Offers back the conversation the workspace's last session left in an
    /// error, such as a network outage mid-task.
    fn offer_resume(&self) -> Option<String> {
        let interrupted = dino_core::resume::interrupted(&self.workspace_root, &self.run_id)?;
        if interrupted.id == self.current_id() {
            return None;
        }
        Some(style::paint(
            style::WARN,
            &format!(
                "  the last session ended in an error ({}). /resume {} continues it.",
                dino_core::text::capped_head(
                    interrupted.error.lines().next().unwrap_or_default(),
                    160
                ),
                interrupted.id
            ),
        ))
    }

    pub(crate) fn current_id(&self) -> String {
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
                style::paint(PURPLE, &format!("{:<24}", profile.key)),
                style::paint(MUTED, &profile.display_name)
            );
        }
        block.push_str(&style::paint(
            FAINT,
            "  /model <key> starts a new conversation on that model; add --default to start \
             later sessions on it too.\n",
        ));
        block
    }

    /// Writes this session's model into the user's config, and says what
    /// would still start another.
    fn save_default_model(&self) -> String {
        let path = match crate::settings::save_default_model(&self.model_key) {
            Ok(path) => path,
            Err(error) => return style::paint(style::WARN, &format!("  {error}")),
        };
        let mut said = format!(
            "  later sessions start on {}: set in {}.",
            self.model_key,
            crate::render::short_path(&path, std::path::Path::new(""))
        );
        if std::env::var_os("DINO_MODEL").is_some() {
            said.push_str(" DINO_MODEL is set and still wins.");
        }
        let project = crate::settings::Layers::load(&self.workspace_root)
            .ok()
            .and_then(|layers| layers.project.model);
        if let Some(model) = project {
            let _ = write!(
                said,
                " This repository's .dino/config.toml names {model}, which wins here."
            );
        }
        style::paint(FAINT, &said)
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
        let saved = &saved[..saved.len().min(10)];
        for (index, depth) in dino_core::resume::family(&self.workspace_root, saved) {
            let item = &saved[index];
            let age = now
                .duration_since(item.modified)
                .map_or_else(|_| "now".to_owned(), |age| describe_age(age.as_secs()));
            let (model, name) = dino_core::resume::label(&item.path).map_or_else(
                |_| ("?".to_owned(), String::new()),
                |label| (label.model_key, label.name.unwrap_or_default()),
            );
            let marker = if item.id == current { "›" } else { " " };
            let _ = writeln!(
                &mut block,
                "  {marker} {}  {}{}",
                style::paint(
                    PURPLE,
                    &format!("{:<22}", format!("{}{}", "  ".repeat(depth), item.id))
                ),
                style::paint(MUTED, &format!("{age:>8} ago  {model}")),
                if name.is_empty() {
                    String::new()
                } else {
                    format!("  {}", style::paint(style::TEXT, &name))
                }
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
        let hello = if self.greeting {
            crate::greeting::greeting(&self.model, &workspace)
        } else {
            format!(
                "\n{}  {}\n{}\n  {}\n",
                style::paint_with(crate::style::EMBER, &[crate::style::BOLD], "  DINO"),
                style::paint(crate::style::FAINT, env!("CARGO_PKG_VERSION")),
                style::paint(crate::style::FAINT, "  ────────"),
                style::paint(crate::style::TEXT, &format!("{} · {workspace}", self.model)),
            )
        };
        let keep_going = if self.keep_going {
            format!("{}\n", style::paint(FAINT, &self.keep_going_banner()))
        } else {
            String::new()
        };
        format!(
            "{hello}  {}\n  {}\n{keep_going}",
            style::paint(
                crate::style::MUTED,
                &format!(
                    "{} · {}",
                    crate::render::short_label(&self.approvals),
                    crate::render::short_label(&self.sandbox)
                )
            ),
            style::paint(
                crate::style::FAINT,
                if self.remote.is_some() {
                    "driven from a page · ctrl-c to stop"
                } else {
                    "/help for commands · ctrl-d to leave"
                }
            ),
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

pub(crate) fn describe_age(seconds: u64) -> String {
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
            "list models; /model <key> switches, in a new conversation; \
             --default keeps it for later sessions",
        ),
        (
            "/resume",
            "list saved conversations; /resume <id or name> continues one",
        ),
        (
            "/rename",
            "/rename <name> names this conversation; alone, clears it",
        ),
        (
            "/rewind",
            "list checkpoints; /rewind <n> goes back to before that message",
        ),
        (
            "/fork",
            "/fork [name] carries on in a copy; the original stays resumable",
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
        ("/auto", "see every message through, or stop doing so"),
        ("/log", "the path of this session's log"),
        (
            "/schedules",
            "reminders and recurring messages in this workspace",
        ),
        ("/exit", "leave; Ctrl-D does the same"),
    ];
    let mut block = String::new();
    for (command, description) in rows {
        let _ = writeln!(
            &mut block,
            "  {}  {}",
            style::paint(PURPLE, &format!("{command:<10}")),
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
    fn a_task_is_compacted_into_its_report_and_the_commits_main_lacks() {
        let summary = seeded_summary(
            "S0 is met; S1 is next.\n",
            Some(("main..HEAD".into(), "3c7b6b2 Meet the gate".into())),
            Some("Files changed: src/shape.rs".into()),
        );

        assert!(
            summary.contains("report:\n\nS0 is met; S1 is next."),
            "{summary}"
        );
        assert!(
            summary.contains("(`git log main..HEAD`):\n\n3c7b6b2 Meet the gate"),
            "{summary}"
        );
        assert!(
            summary.ends_with("Files changed: src/shape.rs"),
            "{summary}"
        );
        assert!(!seeded_summary("done", None, None).contains("git log"));
    }

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
    fn a_lone_exit_or_quit_leaves_and_a_sentence_with_it_does_not() {
        assert_eq!(parse(" exit "), Input::Exit);
        assert_eq!(parse("quit"), Input::Exit);
        assert_eq!(
            parse("exit early when the cursor is closed"),
            Input::Submit("exit early when the cursor is closed".into())
        );
    }

    #[test]
    fn a_command_entered_during_a_run_is_run_on_its_own_after_it() {
        let lines = ["exit", "/q"].map(str::to_owned).to_vec();
        assert_eq!(queue_lines(lines), ["exit", "/q"]);
        let lines = ["also tests", "and docs", "/cost", "then commit"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(
            queue_lines(lines),
            ["also tests\n\nand docs", "/cost", "then commit"]
        );
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
        assert_eq!(parse("/auto"), Input::Auto);
        assert_eq!(parse("/init"), Input::Init);
        assert_eq!(parse("/model"), Input::Model(None));
        assert_eq!(
            parse("/model  claude-opus "),
            Input::Model(Some("claude-opus".into()))
        );
        assert_eq!(
            parse("/model claude-sonnet --default"),
            Input::SaveModel(Some("claude-sonnet".into()))
        );
        assert_eq!(parse("/model --default"), Input::SaveModel(None));
        assert_eq!(parse("/resume"), Input::Resume(None));
        assert_eq!(
            parse("/resume 1a16-2"),
            Input::Resume(Some("1a16-2".into()))
        );
        assert_eq!(
            parse("/resume rfc wave"),
            Input::Resume(Some("rfc wave".into()))
        );
        assert_eq!(
            parse("/rename  rfc wave "),
            Input::Rename(Some("rfc wave".into()))
        );
        assert_eq!(parse("/rename"), Input::Rename(None));
        assert_eq!(parse("/rewind"), Input::Rewind(None));
        assert_eq!(parse("/rewind 3"), Input::Rewind(Some("3".into())));
        assert_eq!(parse("/fork"), Input::Fork(None));
        assert_eq!(parse("/fork try b"), Input::Fork(Some("try b".into())));
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
