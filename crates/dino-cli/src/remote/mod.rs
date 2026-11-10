//! Driving a running session from another device.
//!
//! `dino serve` runs a session with no terminal and a small web page in front
//! of it. This module is the part that knows about the session: a [`Hub`] that
//! the agent's events flow into and a page's commands flow out of, the approval
//! prompt answered from the page, and the sink that feeds the hub. The network
//! half, and every check on who may connect, is in [`server`]. RFC-0020.

mod blurb;
mod dash;
mod places;
mod server;

pub use blurb::Blurb;
pub use dash::serve as serve_dash;
pub use places::Place;
pub use places::SavedSession;
pub use places::Workspaces;
pub use places::note_workspace;
pub use server::bind_address;
pub use server::new_token;
pub use server::serve;

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

use dino_core::Inbox;
use dino_core::approval::ApprovalReply;
use dino_core::approval::Approver;
use dino_core::approval::AskContext;
use dino_core::approval::ReplyFuture;
use dino_core::session::EventSink;
use dino_core::session::SinkError;
use dino_protocol::AgentEvent;
use dino_protocol::ModelEvent;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolClass;
use serde::Serialize;
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Frames kept for a page that connects late or reconnects. A frame is one
/// event, and a streamed answer is many, so this is minutes and not hours.
const BACKLOG: usize = 4_000;

/// Frames a connected page may fall behind by before it is cut off. It
/// reconnects and is brought up from the backlog.
const LIVE: usize = 1_024;

/// Prompts that may wait for the session to be free.
const WAITING: usize = 8;

/// The longest prompt taken from a page.
const MAX_PROMPT_BYTES: usize = 32 * 1_024;

/// The longest string a frame carries. A page on a phone has no use for a
/// whole file, and the session log has it.
const MAX_STRING_BYTES: usize = 16 * 1_024;

/// The most of a call's arguments an approval shows.
const MAX_ARGUMENT_BYTES: usize = 8 * 1_024;

/// What the page says about the session it controls.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Info {
    pub model: String,
    pub workspace: String,
    pub sandbox: String,
    pub approvals: String,
    /// The saved conversation this session continues, by id.
    pub conversation: String,
    /// This run's log, by id, for its trajectory.
    pub log: String,
    #[serde(skip)]
    pub root: PathBuf,
}

/// What a page asks of the session loop. Only ever sent while it is idle,
/// apart from a prompt, which then waits its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Prompt(String),
    /// Continue the saved conversation with this id, one the page was listed.
    Resume(String),
    /// End this session and start one in this workspace, one the page was
    /// offered.
    Workspace(PathBuf),
}

/// One message to a page: a sequence number and its JSON, built once.
#[derive(Debug, Clone)]
pub struct Frame {
    pub seq: u64,
    pub json: Arc<str>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Payload<'a> {
    Event {
        event: serde_json::Value,
    },
    Approval {
        approval: &'a Approval,
    },
    Resolved {
        id: u64,
        outcome: &'a str,
    },
    State {
        busy: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        info: Option<&'a Info>,
    },
    Notice {
        text: &'a str,
    },
}

#[derive(Serialize)]
struct Framed<'a> {
    seq: u64,
    #[serde(flatten)]
    payload: Payload<'a>,
}

/// A call waiting for a person, as the page sees it.
#[derive(Debug, Serialize)]
pub struct Approval {
    id: u64,
    call_id: String,
    tool: String,
    class: ToolClass,
    subject: String,
    summary: String,
    arguments: String,
    choices: Vec<WireChoice>,
    also_pending: usize,
    unconfined: Option<String>,
}

#[derive(Debug, Serialize)]
struct WireChoice {
    key: &'static str,
    label: String,
}

struct Pending {
    frame: Frame,
    offered: Vec<(&'static str, ApprovalReply)>,
    reply: oneshot::Sender<ApprovalReply>,
}

#[derive(Default)]
struct State {
    seq: u64,
    backlog: VecDeque<Frame>,
    pending: BTreeMap<u64, Pending>,
    next_approval: u64,
    busy: bool,
}

/// Why a prompt from a page was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    Empty,
    TooLong,
    /// Too many prompts are already waiting.
    Full,
}

/// What became of a prompt from a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    /// The session was idle and starts on it.
    Started,
    /// The session was working, so it goes to the model with the next tool
    /// results, as a line typed during a run does.
    Queued,
}

/// Why a resume or a change of workspace was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveError {
    /// Not among what the page was offered.
    NotOffered,
    /// A run is under way; it would be lost.
    Busy,
    /// Too many requests are already waiting.
    Full,
}

/// Why an answer to an approval was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerError {
    /// Answered already, withdrawn, or never asked.
    NoSuchApproval,
    /// Not among the answers that approval offered.
    NotOffered,
}

/// The meeting point of a session and the pages watching it.
pub struct Hub {
    state: Mutex<State>,
    live: broadcast::Sender<Frame>,
    commands: mpsc::Sender<Request>,
    inbox: Inbox,
    cancel: Notify,
    info: Mutex<Option<Info>>,
    workspaces: OnceLock<Workspaces>,
}

/// What a page that has just connected is sent first, and then what follows.
pub struct Subscription {
    pub backlog: VecDeque<Frame>,
    pub live: broadcast::Receiver<Frame>,
}

impl Hub {
    /// A hub, and the receiving end of the prompts sent to it.
    #[must_use]
    pub fn new() -> (Arc<Self>, mpsc::Receiver<Request>) {
        Self::with_inbox(Inbox::default())
    }

    /// As [`Hub::new`], delivering lines typed during a run to `inbox`: for a
    /// session already under way, whose agent reads that one.
    #[must_use]
    pub fn with_inbox(inbox: Inbox) -> (Arc<Self>, mpsc::Receiver<Request>) {
        let (commands, prompts) = mpsc::channel(WAITING);
        let (live, _) = broadcast::channel(LIVE);
        let hub = Arc::new(Self {
            state: Mutex::new(State::default()),
            live,
            commands,
            inbox,
            cancel: Notify::new(),
            info: Mutex::new(None),
            workspaces: OnceLock::new(),
        });
        (hub, prompts)
    }

    /// Where lines typed during a run wait, shared with the agent.
    #[must_use]
    pub fn inbox(&self) -> Inbox {
        Arc::clone(&self.inbox)
    }

    /// Says what the session is, for the page's heading. Again after a
    /// resume or a change of workspace; pages hear it with the next state.
    pub fn describe(&self, info: Info) {
        *self
            .info
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(info);
    }

    fn info(&self) -> Option<Info> {
        self.info
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The current workspace, once a session has said which it is. Never the
    /// process's directory, which an empty path would read as.
    fn root(&self) -> Option<PathBuf> {
        self.info()
            .map(|info| info.root)
            .filter(|root| root.is_absolute())
    }

    /// The workspaces a page may move the session to. Once, at start.
    pub fn offer(&self, workspaces: Workspaces) {
        let _ = self.workspaces.set(workspaces);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn publish(&self, state: &mut State, payload: Payload<'_>) -> Frame {
        state.seq += 1;
        let seq = state.seq;
        let json = serde_json::to_string(&Framed { seq, payload }).unwrap_or_else(|_| {
            format!(r#"{{"seq":{seq},"kind":"notice","text":"a frame could not be encoded"}}"#)
        });
        let frame = Frame {
            seq,
            json: json.into(),
        };
        state.backlog.push_back(frame.clone());
        if state.backlog.len() > BACKLOG {
            state.backlog.pop_front();
        }
        // No receivers is a session nobody is watching, which is allowed.
        let _ = self.live.send(frame.clone());
        frame
    }

    /// Passes one agent event to the pages.
    pub fn event(&self, event: &AgentEvent) {
        if let Some(event) = forwarded(event) {
            let mut state = self.lock();
            self.publish(&mut state, Payload::Event { event });
        }
    }

    /// Says something that is not an agent event, such as why a run stopped.
    pub fn notice(&self, text: &str) {
        let mut state = self.lock();
        self.publish(&mut state, Payload::Notice { text });
    }

    /// Marks the session as working on a prompt, or free for one.
    pub fn set_busy(&self, busy: bool) {
        let mut state = self.lock();
        state.busy = busy;
        let info = self.info();
        self.publish(
            &mut state,
            Payload::State {
                busy,
                info: info.as_ref(),
            },
        );
    }

    /// What a page connecting after `after` is owed: the frames since, every
    /// approval still waiting (a page drops one it already shows), and where
    /// the session stands now.
    #[must_use]
    pub fn subscribe(&self, after: u64) -> Subscription {
        let state = self.lock();
        let live = self.live.subscribe();
        let mut backlog: VecDeque<Frame> = state
            .backlog
            .iter()
            .filter(|frame| frame.seq > after)
            .cloned()
            .collect();
        for pending in state.pending.values() {
            if !backlog.iter().any(|frame| frame.seq == pending.frame.seq) {
                backlog.push_back(pending.frame.clone());
            }
        }
        let info = self.info();
        let now = Framed {
            seq: state.seq,
            payload: Payload::State {
                busy: state.busy,
                info: info.as_ref(),
            },
        };
        if let Ok(json) = serde_json::to_string(&now) {
            backlog.push_back(Frame {
                seq: state.seq,
                json: json.into(),
            });
        }
        Subscription { backlog, live }
    }

    /// Takes a prompt from a page.
    ///
    /// # Errors
    ///
    /// Returns why the prompt was not taken.
    pub fn submit(&self, text: &str) -> Result<Submitted, SubmitError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(SubmitError::Empty);
        }
        if text.len() > MAX_PROMPT_BYTES {
            return Err(SubmitError::TooLong);
        }
        if self.lock().busy {
            if let Ok(mut inbox) = self.inbox.lock() {
                inbox.push(text.to_owned());
            }
            return Ok(Submitted::Queued);
        }
        self.commands
            .try_send(Request::Prompt(text.to_owned()))
            .map(|()| Submitted::Started)
            .map_err(|_| SubmitError::Full)
    }

    /// Asks the idle session to continue the saved conversation `id`.
    ///
    /// # Errors
    ///
    /// Returns why not: `id` is not a saved conversation of this workspace, or
    /// a run is under way.
    pub fn resume(&self, id: &str) -> Result<(), MoveError> {
        if !self
            .sessions()
            .iter()
            .any(|session| session.resumable && session.id == id)
        {
            return Err(MoveError::NotOffered);
        }
        self.request(Request::Resume(id.to_owned()))
    }

    /// Asks the idle session to make way for one in `path`.
    ///
    /// # Errors
    ///
    /// Returns why not: `path` is not among the workspaces offered, or a run is
    /// under way.
    pub fn switch(&self, path: &str) -> Result<(), MoveError> {
        let offered = self.workspaces();
        let place = offered
            .iter()
            .find(|place| place.path.to_str() == Some(path))
            .ok_or(MoveError::NotOffered)?;
        self.request(Request::Workspace(place.path.clone()))
    }

    /// The workspaces offered now, the current one first.
    #[must_use]
    pub fn workspaces(&self) -> Vec<Place> {
        match (self.root(), self.workspaces.get()) {
            (Some(root), Some(workspaces)) => workspaces.offered(&root),
            _ => Vec::new(),
        }
    }

    /// The saved conversations and logs of the current workspace.
    #[must_use]
    pub fn sessions(&self) -> Vec<SavedSession> {
        self.root()
            .map_or_else(Vec::new, |root| places::sessions(&root))
    }

    /// Where the log `id` of the current workspace is, when it is one.
    #[must_use]
    pub fn log_path(&self, id: &str) -> Option<PathBuf> {
        places::log_path(&self.root()?, id)
    }

    fn request(&self, request: Request) -> Result<(), MoveError> {
        if self.lock().busy {
            return Err(MoveError::Busy);
        }
        self.commands.try_send(request).map_err(|_| MoveError::Full)
    }

    /// Asks the run under way to stop. False when none is.
    pub fn cancel(&self) -> bool {
        let busy = self.lock().busy;
        if busy {
            self.cancel.notify_waiters();
        }
        busy
    }

    /// Resolves when a page asks to cancel. Created before the run it watches
    /// is awaited, so a request cannot fall between the two.
    pub async fn cancelled(&self) {
        self.cancel.notified().await;
    }

    /// Takes a page's answer to the approval `id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the approval is not waiting or did not offer
    /// `choice`. Nothing is decided then.
    pub fn answer(&self, id: u64, choice: &str) -> Result<(), AnswerError> {
        let mut state = self.lock();
        let pending = state.pending.get(&id).ok_or(AnswerError::NoSuchApproval)?;
        let reply = pending
            .offered
            .iter()
            .find(|(key, _)| *key == choice)
            .map(|(_, reply)| reply.clone())
            .ok_or(AnswerError::NotOffered)?;
        if let Some(pending) = state.pending.remove(&id) {
            let outcome = if matches!(reply, ApprovalReply::Reject(_)) {
                "refused"
            } else {
                "allowed"
            };
            // The asker is gone when its run was cancelled; nothing to tell.
            let _ = pending.reply.send(reply);
            self.publish(&mut state, Payload::Resolved { id, outcome });
        }
        Ok(())
    }

    /// Puts a call to the pages and waits for an answer.
    async fn ask(
        &self,
        mut approval: Approval,
        offered: Vec<(&'static str, ApprovalReply)>,
    ) -> ApprovalReply {
        let (reply, answer) = oneshot::channel();
        let id = {
            let mut state = self.lock();
            state.next_approval += 1;
            let id = state.next_approval;
            approval.id = id;
            let frame = self.publish(
                &mut state,
                Payload::Approval {
                    approval: &approval,
                },
            );
            state.pending.insert(
                id,
                Pending {
                    frame,
                    offered,
                    reply,
                },
            );
            id
        };
        let _withdraw = Withdraw { hub: self, id };
        answer
            .await
            .unwrap_or_else(|_| ApprovalReply::Reject(Some("the remote session ended".to_owned())))
    }
}

/// Takes a question back from the pages if the run asking it is cancelled, so
/// a card nobody can act on does not stay on a phone.
struct Withdraw<'a> {
    hub: &'a Hub,
    id: u64,
}

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        let mut state = self.hub.lock();
        if state.pending.remove(&self.id).is_some() {
            self.hub.publish(
                &mut state,
                Payload::Resolved {
                    id: self.id,
                    outcome: "withdrawn",
                },
            );
        }
    }
}

/// The event as a page should see it, or `None` for the ones it does not.
///
/// Reasoning and argument deltas are left out: the first is not for showing
/// and the second repeats the completed call that follows.
fn forwarded(event: &AgentEvent) -> Option<serde_json::Value> {
    if matches!(
        event,
        AgentEvent::Model {
            event: ModelEvent::ReasoningDelta { .. } | ModelEvent::ToolCallArgumentsDelta { .. },
            ..
        }
    ) {
        return None;
    }
    let mut value = serde_json::to_value(event).ok()?;
    cap_strings(&mut value);
    Some(value)
}

fn cap_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) if text.len() > MAX_STRING_BYTES => {
            let mut end = MAX_STRING_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let cut = text.len() - end;
            text.truncate(end);
            let _ = write!(text, " … [{cut} bytes not sent to the remote view]");
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(cap_strings),
        serde_json::Value::Object(fields) => fields.values_mut().for_each(cap_strings),
        _ => {}
    }
}

/// A hub being served to pages for a session that was already running.
pub struct Serving {
    pub hub: Arc<Hub>,
    pub requests: mpsc::Receiver<Request>,
    /// Where the page is, token included.
    pub url: String,
    pub local_only: bool,
    task: tokio::task::JoinHandle<std::convert::Infallible>,
}

impl Serving {
    /// Stops accepting pages. One already connected keeps its stream, which goes
    /// quiet because the session no longer feeds the hub.
    pub fn stop(self) {
        self.task.abort();
    }
}

/// Starts serving `inbox`'s session to pages, on the address `dino serve` would
/// choose for `bind` and `port`.
///
/// # Errors
///
/// Returns an error when the address is not one the page may be served on, it
/// cannot be bound, or the token cannot be made.
pub async fn start(
    bind: Option<std::net::IpAddr>,
    port: u16,
    inbox: Inbox,
    workspaces: Workspaces,
) -> anyhow::Result<Serving> {
    use anyhow::Context as _;
    let address = bind_address(bind, port)?;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("cannot listen on {address}"))?;
    let address = listener.local_addr().context("cannot read the address")?;
    let token = new_token()?;
    let (hub, requests) = Hub::with_inbox(inbox);
    hub.offer(workspaces);
    // The token is in the fragment, which a browser keeps to itself.
    let url = format!("http://{address}/#{token}");
    let task = tokio::spawn(serve(listener, Arc::clone(&hub), token));
    Ok(Serving {
        hub,
        requests,
        url,
        local_only: address.ip().is_loopback(),
        task,
    })
}

/// The hub of a session that `/serve` may start serving after it has begun, and
/// whether the turn under way came from a page.
#[derive(Clone, Default)]
pub struct Late {
    hub: Arc<Mutex<Option<Arc<Hub>>>>,
    page_turn: Arc<std::sync::atomic::AtomicBool>,
}

impl Late {
    #[must_use]
    pub fn new(hub: Option<Arc<Hub>>) -> Self {
        Self {
            hub: Arc::new(Mutex::new(hub)),
            page_turn: Arc::default(),
        }
    }

    #[must_use]
    pub fn get(&self) -> Option<Arc<Hub>> {
        self.hub
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set(&self, hub: Option<Arc<Hub>>) {
        *self
            .hub
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = hub;
    }

    /// Marks the turn about to run as one a page started, so its approvals are
    /// asked there and not at a terminal nobody is watching.
    pub fn page_turn(&self, page: bool) {
        self.page_turn
            .store(page, std::sync::atomic::Ordering::Relaxed);
    }

    fn asking_page(&self) -> Option<Arc<Hub>> {
        if self.page_turn.load(std::sync::atomic::Ordering::Relaxed) {
            self.get()
        } else {
            None
        }
    }
}

/// Feeds the hub the events the terminal and the log get.
pub struct Tap(pub Late);

impl EventSink for Tap {
    fn emit(&mut self, event: &AgentEvent) -> Result<(), SinkError> {
        if let Some(hub) = self.0.get() {
            hub.event(event);
        }
        Ok(())
    }
}

/// Asks at the terminal, except for a turn a page started, which asks there.
pub struct RoutedApprover {
    terminal: crate::approve::TerminalApprover,
    late: Late,
}

impl RoutedApprover {
    #[must_use]
    pub fn new(terminal: crate::approve::TerminalApprover, late: Late) -> Self {
        Self { terminal, late }
    }
}

impl Approver for RoutedApprover {
    fn ask(&self, call: &ToolCall, action: &ToolAction) -> ReplyFuture<'_> {
        self.ask_in(call, action, &AskContext::default())
    }

    fn ask_in(
        &self,
        call: &ToolCall,
        action: &ToolAction,
        context: &AskContext,
    ) -> ReplyFuture<'_> {
        match self.late.asking_page() {
            Some(hub) => ask_page(hub, call, action, context),
            None => self.terminal.ask_in(call, action, context),
        }
    }
}

/// Asks about a call on a page, and offers it less than the terminal would.
///
/// A page may give every answer the terminal can except a rule kept for later
/// sessions: whoever holds the token holds a phone that can be lost, and that is
/// the one answer that outlasts this session.
pub struct RemoteApprover {
    hub: Arc<Hub>,
}

impl RemoteApprover {
    #[must_use]
    pub fn new(hub: Arc<Hub>) -> Self {
        Self { hub }
    }
}

/// The name a page answers by, for the replies a page may give.
fn wire_key(reply: &ApprovalReply) -> Option<&'static str> {
    match reply {
        ApprovalReply::Once => Some("once"),
        ApprovalReply::ForTurn => Some("turn"),
        ApprovalReply::ForSession => Some("session"),
        ApprovalReply::ForProgram => Some("program"),
        ApprovalReply::AllProcesses => Some("processes"),
        ApprovalReply::AllEdits => Some("edits"),
        ApprovalReply::Reject(_) => Some("reject"),
        ApprovalReply::Remember => None,
    }
}

impl Approver for RemoteApprover {
    fn ask(&self, call: &ToolCall, action: &ToolAction) -> ReplyFuture<'_> {
        self.ask_in(call, action, &AskContext::default())
    }

    fn ask_in(
        &self,
        call: &ToolCall,
        action: &ToolAction,
        context: &AskContext,
    ) -> ReplyFuture<'_> {
        ask_page(Arc::clone(&self.hub), call, action, context)
    }
}

fn ask_page(
    hub: Arc<Hub>,
    call: &ToolCall,
    action: &ToolAction,
    context: &AskContext,
) -> ReplyFuture<'static> {
    let summary = crate::render::describe_call(call);
    let subject = action.subject.clone().unwrap_or_else(|| call.name.clone());
    let asking = crate::approve::Asking {
        summary: &summary,
        tool: &call.name,
        subject: &subject,
        class: action.class,
        context,
        change: None,
    };
    let offered: Vec<(&'static str, ApprovalReply, String)> =
        crate::approve::choices(&asking, crate::approve::always_label(action.class))
            .into_iter()
            .filter_map(|choice| {
                wire_key(&choice.reply).map(|key| (key, choice.reply, choice.label))
            })
            .collect();
    let arguments = serde_json::to_string_pretty(&call.arguments).unwrap_or_default();
    let approval = Approval {
        id: 0,
        call_id: call.id.clone(),
        tool: call.name.clone(),
        class: action.class,
        subject,
        summary,
        arguments: dino_core::text::capped_head(&arguments, MAX_ARGUMENT_BYTES),
        choices: offered
            .iter()
            .map(|(key, _, label)| WireChoice {
                key,
                label: label.clone(),
            })
            .collect(),
        also_pending: context.also_pending.len(),
        unconfined: context.unconfined.clone(),
    };
    let offered = offered
        .into_iter()
        .map(|(key, reply, _)| (key, reply))
        .collect();
    Box::pin(async move { hub.ask(approval, offered).await })
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use serde_json::json;

    use super::*;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_owned(),
            name: "exec".to_owned(),
            arguments: json!({ "command": ["cargo", "test"] }),
        }
    }

    fn parsed(frame: &Frame) -> Value {
        serde_json::from_str(&frame.json).expect("a frame is JSON")
    }

    /// The approval frame the next `ask` publishes, read off the live channel.
    async fn next_approval(live: &mut broadcast::Receiver<Frame>) -> Value {
        loop {
            let frame = live.recv().await.expect("hub alive");
            let value = parsed(&frame);
            if value["kind"] == "approval" {
                return value["approval"].clone();
            }
        }
    }

    #[tokio::test]
    async fn a_page_answers_an_approval_and_the_session_gets_the_reply() {
        let (hub, _prompts) = Hub::new();
        let mut live = hub.subscribe(0).live;
        let approver = RemoteApprover::new(Arc::clone(&hub));
        let asked = tokio::spawn(async move {
            approver
                .ask_in(
                    &call("c1"),
                    &ToolAction::process("cargo test"),
                    &AskContext::default(),
                )
                .await
        });

        let approval = next_approval(&mut live).await;
        assert_eq!(approval["tool"], "exec");
        assert_eq!(approval["subject"], "cargo test");
        let keys: Vec<&str> = approval["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|choice| choice["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["once", "session", "processes", "reject"]);

        let id = approval["id"].as_u64().unwrap();
        hub.answer(id, "once").unwrap();
        assert_eq!(asked.await.unwrap(), ApprovalReply::Once);
        // Answered once; a second answer finds nothing waiting.
        assert_eq!(hub.answer(id, "once"), Err(AnswerError::NoSuchApproval));
    }

    #[tokio::test]
    async fn a_rule_for_later_sessions_is_never_offered_to_a_page() {
        let (hub, _prompts) = Hub::new();
        let mut live = hub.subscribe(0).live;
        let approver = RemoteApprover::new(Arc::clone(&hub));
        let context = AskContext {
            also_pending: vec!["other".to_owned()],
            program: Some("cargo".to_owned()),
            unconfined: None,
            ..AskContext::default()
        };
        let asked = tokio::spawn(async move {
            approver
                .ask_in(&call("c1"), &ToolAction::process("cargo test"), &context)
                .await
        });
        let approval = next_approval(&mut live).await;
        let id = approval["id"].as_u64().unwrap();
        for refused in ["remember", "nonsense"] {
            assert_eq!(hub.answer(id, refused), Err(AnswerError::NotOffered));
        }
        // The refusals decided nothing: the call is still waiting.
        hub.answer(id, "turn").unwrap();
        assert_eq!(asked.await.unwrap(), ApprovalReply::ForTurn);
    }

    #[tokio::test]
    async fn a_call_outside_the_sandbox_can_be_run_once_from_a_page() {
        let (hub, _prompts) = Hub::new();
        let mut live = hub.subscribe(0).live;
        let approver = RemoteApprover::new(Arc::clone(&hub));
        let context = AskContext {
            unconfined: Some("needs the network".to_owned()),
            ..AskContext::default()
        };
        let asked = tokio::spawn(async move {
            approver
                .ask_in(
                    &call("c1"),
                    &ToolAction::unconfined_process("curl x", "needs the network"),
                    &context,
                )
                .await
        });
        let approval = next_approval(&mut live).await;
        let keys: Vec<&str> = approval["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|choice| choice["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["once", "reject"]);
        assert_eq!(approval["unconfined"], "needs the network");
        hub.answer(approval["id"].as_u64().unwrap(), "once")
            .unwrap();
        assert_eq!(asked.await.unwrap(), ApprovalReply::Once);
    }

    #[tokio::test]
    async fn a_page_may_allow_every_command_but_never_remember_a_rule() {
        let (hub, _prompts) = Hub::new();
        let mut live = hub.subscribe(0).live;
        let approver = RemoteApprover::new(Arc::clone(&hub));
        let asked = tokio::spawn(async move {
            approver
                .ask(&call("c1"), &ToolAction::process("cargo test"))
                .await
        });
        let approval = next_approval(&mut live).await;
        let keys: Vec<&str> = approval["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|choice| choice["key"].as_str().unwrap())
            .collect();
        assert!(keys.contains(&"processes"), "{keys:?}");
        assert!(!keys.contains(&"remember"), "{keys:?}");
        let id = approval["id"].as_u64().unwrap();
        assert_eq!(hub.answer(id, "remember"), Err(AnswerError::NotOffered));
        hub.answer(id, "processes").unwrap();
        assert_eq!(asked.await.unwrap(), ApprovalReply::AllProcesses);
    }

    #[tokio::test]
    async fn a_cancelled_run_withdraws_its_question() {
        let (hub, _prompts) = Hub::new();
        let mut live = hub.subscribe(0).live;
        let approver = RemoteApprover::new(Arc::clone(&hub));
        let asked = tokio::spawn(async move {
            approver
                .ask(&call("c1"), &ToolAction::process("cargo test"))
                .await
        });
        let id = next_approval(&mut live).await["id"].as_u64().unwrap();
        asked.abort();
        loop {
            let frame = parsed(&live.recv().await.unwrap());
            if frame["kind"] == "resolved" {
                assert_eq!(frame["id"], id);
                assert_eq!(frame["outcome"], "withdrawn");
                break;
            }
        }
        assert_eq!(hub.answer(id, "once"), Err(AnswerError::NoSuchApproval));
    }

    #[tokio::test]
    async fn a_page_that_connects_late_is_owed_the_approval_still_waiting() {
        let (hub, _prompts) = Hub::new();
        let mut live = hub.subscribe(0).live;
        let approver = RemoteApprover::new(Arc::clone(&hub));
        let asked = tokio::spawn(async move {
            approver
                .ask(&call("c1"), &ToolAction::process("cargo test"))
                .await
        });
        let _ = next_approval(&mut live).await;
        // A later frame pushes the approval's own past what `after` asks for.
        hub.notice("later");
        let after = hub.lock().seq;

        let backlog = hub.subscribe(after).backlog;
        let kinds: Vec<String> = backlog
            .iter()
            .map(|frame| parsed(frame)["kind"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(kinds, ["approval", "state"]);
        asked.abort();
    }

    #[test]
    fn a_prompt_goes_to_the_session_when_idle_and_to_the_inbox_when_busy() {
        let (hub, mut prompts) = Hub::new();
        assert_eq!(hub.submit("  fix the build  "), Ok(Submitted::Started));
        assert_eq!(
            prompts.try_recv().unwrap(),
            Request::Prompt("fix the build".to_owned())
        );

        hub.set_busy(true);
        assert_eq!(hub.submit("and the tests"), Ok(Submitted::Queued));
        assert!(prompts.try_recv().is_err());
        assert_eq!(*hub.inbox().lock().unwrap(), ["and the tests"]);
    }

    #[test]
    fn empty_and_oversized_prompts_are_refused() {
        let (hub, _prompts) = Hub::new();
        assert_eq!(hub.submit("  \n "), Err(SubmitError::Empty));
        let long = "x".repeat(MAX_PROMPT_BYTES + 1);
        assert_eq!(hub.submit(&long), Err(SubmitError::TooLong));
        for _ in 0..WAITING {
            hub.submit("one").unwrap();
        }
        assert_eq!(hub.submit("too many"), Err(SubmitError::Full));
    }

    #[test]
    fn cancel_only_means_something_while_a_run_is() {
        let (hub, _prompts) = Hub::new();
        assert!(!hub.cancel());
        hub.set_busy(true);
        assert!(hub.cancel());
    }

    #[test]
    fn reasoning_stays_off_the_wire_and_long_output_is_cut() {
        let reasoning = AgentEvent::Model {
            model_turn: 1,
            event: ModelEvent::ReasoningDelta {
                text: "thinking".to_owned(),
            },
        };
        assert!(forwarded(&reasoning).is_none());

        let finished = AgentEvent::ToolFinished {
            model_turn: 1,
            result: dino_protocol::ToolResult {
                call_id: "c1".to_owned(),
                output: dino_protocol::ToolOutput::success("x".repeat(MAX_STRING_BYTES * 3)),
            },
        };
        let value = forwarded(&finished).unwrap();
        let content = value["result"]["output"]["content"].as_str().unwrap();
        assert!(content.len() < MAX_STRING_BYTES + 100, "{}", content.len());
        assert!(content.ends_with("bytes not sent to the remote view]"));
    }

    #[test]
    fn a_page_joining_mid_run_hears_how_things_stand_and_what_it_missed() {
        let (hub, _prompts) = Hub::new();
        hub.describe(Info {
            model: "m".to_owned(),
            workspace: "/w".to_owned(),
            sandbox: "s".to_owned(),
            approvals: "a".to_owned(),
            ..Info::default()
        });
        hub.notice("one");
        hub.notice("two");
        hub.set_busy(true);

        let seen = hub.subscribe(1).backlog;
        let kinds: Vec<(u64, String)> = seen
            .iter()
            .map(|frame| {
                let value = parsed(frame);
                (frame.seq, value["kind"].as_str().unwrap().to_owned())
            })
            .collect();
        assert_eq!(
            kinds,
            [
                (2, "notice".to_owned()),
                (3, "state".to_owned()),
                (3, "state".to_owned())
            ]
        );
        let state = parsed(seen.back().unwrap());
        assert_eq!(state["busy"], true);
        assert_eq!(state["info"]["workspace"], "/w");
    }
}
