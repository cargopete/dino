//! Approval policy for classified tool calls.
//!
//! The tool runtime classifies, a policy decides, and the agent core records
//! the decision. Approval attaches to a tool name and a resolved subject, never
//! to a rendered description of what a call probably does.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use dino_protocol::ApprovalDecision;
use dino_protocol::DecisionSource;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolClass;

pub type DecisionFuture<'a> = Pin<Box<dyn Future<Output = ApprovalDecision> + Send + 'a>>;

pub type ReplyFuture<'a> = Pin<Box<dyn Future<Output = ApprovalReply> + Send + 'a>>;

pub trait ApprovalPolicy: Send + Sync {
    fn decide(&self, call: &ToolCall, action: &ToolAction) -> DecisionFuture<'_>;

    /// Told every classified call of a model turn before the first is decided,
    /// so a person can be asked about them together.
    fn begin_turn(&self, _calls: &[(ToolCall, ToolAction)]) {}
}

impl ApprovalPolicy for Box<dyn ApprovalPolicy> {
    fn decide(&self, call: &ToolCall, action: &ToolAction) -> DecisionFuture<'_> {
        (**self).decide(call, action)
    }

    fn begin_turn(&self, calls: &[(ToolCall, ToolAction)]) {
        (**self).begin_turn(calls);
    }
}

/// What a person answered when asked about one proposed action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalReply {
    /// Allow this call and nothing else.
    Once,
    /// Allow this call and later calls with the same tool and subject.
    ForSession,
    /// As [`ApprovalReply::ForSession`], and keep the rule for later sessions.
    Remember,
    /// Allow this call and the others of this model turn that would ask.
    ForTurn,
    /// Allow, for this session, every call of this tool running this program,
    /// whatever its arguments. Offered only where [`AskContext::program`] is set.
    ForProgram,
    /// Allow, for this session, every process call, still inside the sandbox.
    AllProcesses,
    /// Allow, for this session, every edit of a workspace file outside the
    /// guarded paths.
    AllEdits,
    /// Refuse, optionally saying why.
    Reject(Option<String>),
}

/// What else a person may want to know when asked about one call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AskContext {
    /// The other calls of this model turn that will ask, by rule subject.
    pub also_pending: Vec<String>,
    /// The program a [`ApprovalReply::ForProgram`] rule would cover, when the
    /// tool runs one program per call.
    pub program: Option<String>,
    /// The model's reason, when the call asks to run outside the sandbox. Only
    /// a one-off yes or a refusal is offered then.
    pub unconfined: Option<String>,
    /// Set when the subject is a guarded path, which asks every time. No rule
    /// can be kept for it, so only a one-off yes or a refusal is offered.
    pub guarded: bool,
}

/// A frontend that can ask a person about a proposed action.
pub trait Approver: Send + Sync {
    fn ask(&self, call: &ToolCall, action: &ToolAction) -> ReplyFuture<'_>;

    fn ask_in(
        &self,
        call: &ToolCall,
        action: &ToolAction,
        _context: &AskContext,
    ) -> ReplyFuture<'_> {
        self.ask(call, action)
    }
}

/// Tools whose subject starts with the one program they run, so a rule for
/// that program means what it says. `shell` is not one: a line can run anything.
const PROGRAM_TOOLS: [&str; 2] = ["exec", "cargo"];

fn program_of(call: &ToolCall, action: &ToolAction) -> Option<String> {
    if action.class != ToolClass::Process || !PROGRAM_TOOLS.contains(&call.name.as_str()) {
        return None;
    }
    let program = action.subject.as_deref()?.split_whitespace().next()?;
    Some(program.to_owned())
}

fn program_key(tool: &str, program: &str) -> String {
    format!("{tool}\u{1f}{program} *")
}

/// Allows everything, including mutation, without asking.
///
/// Intended for tests and for an explicit opt-out flag. It is not a default.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAll;

impl ApprovalPolicy for AllowAll {
    fn decide(&self, _call: &ToolCall, _action: &ToolAction) -> DecisionFuture<'_> {
        Box::pin(async { ApprovalDecision::allowed(DecisionSource::Policy) })
    }
}

/// Refuses changes and processes while a plan is being made, and otherwise
/// defers to the policy it wraps. Network reads pass through, because research
/// is most of planning, and still ask where the inner policy asks.
pub struct PlanGate<P> {
    inner: P,
    planning: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<P> PlanGate<P> {
    pub fn new(inner: P, planning: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self { inner, planning }
    }
}

impl<P: ApprovalPolicy> ApprovalPolicy for PlanGate<P> {
    fn decide(&self, call: &ToolCall, action: &ToolAction) -> DecisionFuture<'_> {
        let planning = self.planning.load(std::sync::atomic::Ordering::Relaxed);
        if planning && matches!(action.class, ToolClass::Mutating | ToolClass::Process) {
            return Box::pin(async {
                ApprovalDecision::denied(
                    "plan mode: nothing changes and nothing runs until the plan is accepted; \
                     finish with a numbered plan",
                )
            });
        }
        self.inner.decide(call, action)
    }

    fn begin_turn(&self, calls: &[(ToolCall, ToolAction)]) {
        self.inner.begin_turn(calls);
    }
}

/// Allows read-only calls and refuses everything else.
///
/// This is the default policy, so an agent constructed without an explicit
/// choice can neither write to the workspace nor run anything on the host.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReadOnly;

impl ApprovalPolicy for ReadOnly {
    fn decide(&self, _call: &ToolCall, action: &ToolAction) -> DecisionFuture<'_> {
        let decision = match action.class {
            ToolClass::ReadOnly => ApprovalDecision::allowed(DecisionSource::Policy),
            ToolClass::Mutating | ToolClass::Process | ToolClass::Network => {
                ApprovalDecision::denied("this session runs in read-only mode")
            }
        };
        Box::pin(async move { decision })
    }
}

/// Rules a person chose to keep beyond one session, keyed as
/// [`ToolAction::rule_key`] keys them.
pub trait RuleStore: Send + Sync {
    fn load(&self) -> Vec<String>;

    /// Keeps one more rule. A store that cannot write should say so to the
    /// person itself: the call is allowed either way, because they said yes.
    fn save(&self, key: &str);
}

/// Allows read-only calls, asks a person about anything else, and remembers the
/// narrow rules that person granted.
pub struct Interactive<A> {
    approver: A,
    rules: Mutex<HashSet<String>>,
    /// Calls of the current model turn that will ask, by call id and subject.
    pending: Mutex<Vec<(String, String)>>,
    /// Call ids allowed by a [`ApprovalReply::ForTurn`] answer.
    turn_allowed: Mutex<HashSet<String>>,
    all_processes: std::sync::atomic::AtomicBool,
    all_edits: std::sync::atomic::AtomicBool,
    /// Workspace-relative paths an edit to which asks whatever else was granted.
    guarded: Vec<std::path::PathBuf>,
    saved: HashSet<String>,
    store: Option<Box<dyn RuleStore>>,
}

impl<A> Interactive<A> {
    pub fn new(approver: A) -> Self {
        Self {
            approver,
            rules: Mutex::new(HashSet::new()),
            pending: Mutex::new(Vec::new()),
            turn_allowed: Mutex::new(HashSet::new()),
            all_processes: std::sync::atomic::AtomicBool::new(false),
            all_edits: std::sync::atomic::AtomicBool::new(false),
            guarded: Vec::new(),
            saved: HashSet::new(),
            store: None,
        }
    }

    /// Consults and adds to rules saved for this workspace.
    #[must_use]
    pub fn with_store(mut self, store: impl RuleStore + 'static) -> Self {
        self.saved = store.load().into_iter().collect();
        self.store = Some(Box::new(store));
        self
    }

    /// Allows every edit outside the guarded paths from the start, as an
    /// [`ApprovalReply::AllEdits`] answer would.
    #[must_use]
    pub fn accepting_edits(self) -> Self {
        self.all_edits
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self
    }

    /// Edits beneath these workspace-relative paths ask every time, whatever
    /// [`ApprovalReply::AllEdits`] covers.
    #[must_use]
    pub fn guarding(mut self, paths: Vec<std::path::PathBuf>) -> Self {
        self.guarded = paths;
        self
    }

    /// Session rules granted so far, sorted, for display and tests.
    ///
    /// # Panics
    ///
    /// Panics if the rule lock was poisoned by an earlier panic.
    #[must_use]
    pub fn granted_rules(&self) -> Vec<String> {
        let mut rules: Vec<String> = self
            .rules
            .lock()
            .expect("approval rule lock")
            .iter()
            .cloned()
            .collect();
        rules.sort();
        rules
    }

    fn holds_rule(&self, key: &str) -> bool {
        self.rules.lock().is_ok_and(|rules| rules.contains(key))
    }

    fn grant_rule(&self, key: String) {
        if let Ok(mut rules) = self.rules.lock() {
            rules.insert(key);
        }
    }

    /// Asks about a call outside the sandbox. Whatever the person answers, at
    /// most this one call is allowed: no rule is granted, saved or consulted.
    fn ask_unconfined(
        &self,
        call: &ToolCall,
        action: &ToolAction,
        reason: String,
    ) -> DecisionFuture<'_>
    where
        A: Approver,
    {
        let context = AskContext {
            unconfined: Some(reason),
            ..AskContext::default()
        };
        let call = call.clone();
        let action = action.clone();
        Box::pin(async move {
            match self.approver.ask_in(&call, &action, &context).await {
                ApprovalReply::Reject(reason) => ApprovalDecision::denied(
                    reason.unwrap_or_else(|| "the operator refused this action".to_owned()),
                ),
                _ => ApprovalDecision::allowed(DecisionSource::User),
            }
        })
    }

    /// Whether a rule already covers the call, and by what.
    fn covered(&self, call: &ToolCall, action: &ToolAction) -> Option<DecisionSource> {
        if action.unconfined.is_some() {
            return None;
        }
        // A note appended to the user's own memory file, which they read back.
        if call.name == crate::memory::REMEMBER && action.class == ToolClass::Mutating {
            return Some(DecisionSource::Policy);
        }
        // A guarded path asks every time, whatever rule was granted or saved:
        // the schedule store lives under one, and the README promises every
        // schedule change asks.
        let guarded = action.class == ToolClass::Mutating && self.is_guarded(action);
        let key = action.rule_key(&call.name);
        if !guarded && self.saved.contains(&key) {
            return Some(DecisionSource::SavedRule);
        }
        let by_program = program_of(call, action).map(|program| program_key(&call.name, &program));
        let by_process = action.class == ToolClass::Process
            && self
                .all_processes
                .load(std::sync::atomic::Ordering::Relaxed);
        let by_edits = action.class == ToolClass::Mutating
            && self.all_edits.load(std::sync::atomic::Ordering::Relaxed)
            && !guarded;
        let by_turn = !guarded
            && self
                .turn_allowed
                .lock()
                .is_ok_and(|allowed| allowed.contains(&call.id));
        if (!guarded && self.holds_rule(&key))
            || by_program.is_some_and(|key| self.holds_rule(&key))
            || by_process
            || by_edits
            || by_turn
        {
            return Some(DecisionSource::SessionRule);
        }
        None
    }

    fn is_guarded(&self, action: &ToolAction) -> bool {
        action.subject.as_deref().is_none_or(|subject| {
            self.guarded
                .iter()
                .any(|guarded| std::path::Path::new(subject).starts_with(guarded))
        })
    }
}

impl<A> ApprovalPolicy for Interactive<A>
where
    A: Approver,
{
    fn decide(&self, call: &ToolCall, action: &ToolAction) -> DecisionFuture<'_> {
        if action.class == ToolClass::ReadOnly {
            return Box::pin(async { ApprovalDecision::allowed(DecisionSource::Policy) });
        }
        if let Some(reason) = &action.unconfined {
            return self.ask_unconfined(call, action, reason.clone());
        }

        if let Some(source) = self.covered(call, action) {
            return Box::pin(async move { ApprovalDecision::allowed(source) });
        }

        let key = action.rule_key(&call.name);
        let program = program_of(call, action);
        let also_pending: Vec<String> = self
            .pending
            .lock()
            .map(|pending| {
                pending
                    .iter()
                    .skip_while(|(id, _)| *id != call.id)
                    .skip(1)
                    .map(|(_, subject)| subject.clone())
                    .collect()
            })
            .unwrap_or_default();
        let context = AskContext {
            also_pending,
            program: program.clone(),
            unconfined: None,
            guarded: action.class == ToolClass::Mutating && self.is_guarded(action),
        };
        let call = call.clone();
        let action = action.clone();
        Box::pin(async move {
            match self.approver.ask_in(&call, &action, &context).await {
                ApprovalReply::Once => ApprovalDecision::allowed(DecisionSource::User),
                ApprovalReply::ForTurn => {
                    if let (Ok(pending), Ok(mut allowed)) =
                        (self.pending.lock(), self.turn_allowed.lock())
                    {
                        allowed.extend(pending.iter().map(|(id, _)| id.clone()));
                    }
                    ApprovalDecision::allowed(DecisionSource::User)
                }
                ApprovalReply::ForProgram => {
                    match program {
                        Some(program) => self.grant_rule(program_key(&call.name, &program)),
                        None => self.grant_rule(key),
                    }
                    ApprovalDecision::allowed(DecisionSource::User)
                }
                ApprovalReply::AllProcesses => {
                    if action.class == ToolClass::Process {
                        self.all_processes
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        self.grant_rule(key);
                    }
                    ApprovalDecision::allowed(DecisionSource::User)
                }
                ApprovalReply::AllEdits => {
                    if action.class == ToolClass::Mutating {
                        self.all_edits
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        self.grant_rule(key);
                    }
                    ApprovalDecision::allowed(DecisionSource::User)
                }
                ApprovalReply::ForSession => {
                    self.grant_rule(key);
                    ApprovalDecision::allowed(DecisionSource::User)
                }
                ApprovalReply::Remember => {
                    if let Some(store) = &self.store {
                        store.save(&key);
                    }
                    self.grant_rule(key);
                    ApprovalDecision::allowed(DecisionSource::User)
                }
                ApprovalReply::Reject(reason) => ApprovalDecision::denied(
                    reason.unwrap_or_else(|| "the operator refused this action".to_owned()),
                ),
            }
        })
    }

    fn begin_turn(&self, calls: &[(ToolCall, ToolAction)]) {
        let asking: Vec<(String, String)> = calls
            .iter()
            .filter(|(call, action)| {
                action.class != ToolClass::ReadOnly
                    && action.unconfined.is_none()
                    && self.covered(call, action).is_none()
            })
            .map(|(call, action)| {
                let subject = action.subject.clone().unwrap_or_else(|| call.name.clone());
                (call.id.clone(), format!("{} {subject}", call.name))
            })
            .collect();
        if let Ok(mut pending) = self.pending.lock() {
            *pending = asking;
        }
        if let Ok(mut allowed) = self.turn_allowed.lock() {
            allowed.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// Answers from a script and records what it was asked, with its context.
    struct Script {
        replies: Mutex<VecDeque<ApprovalReply>>,
        asked: Mutex<Vec<(String, AskContext)>>,
    }

    impl Script {
        fn new(replies: Vec<ApprovalReply>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    impl Approver for Script {
        fn ask(&self, call: &ToolCall, action: &ToolAction) -> ReplyFuture<'_> {
            self.ask_in(call, action, &AskContext::default())
        }

        fn ask_in(
            &self,
            call: &ToolCall,
            action: &ToolAction,
            context: &AskContext,
        ) -> ReplyFuture<'_> {
            self.asked
                .lock()
                .unwrap()
                .push((action.rule_key(&call.name), context.clone()));
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ApprovalReply::Reject(None));
            Box::pin(async move { reply })
        }
    }

    fn exec(id: &str, subject: &str) -> (ToolCall, ToolAction) {
        (
            ToolCall {
                id: id.into(),
                name: "exec".into(),
                arguments: serde_json::Value::Null,
            },
            ToolAction::process(subject),
        )
    }

    async fn allowed(policy: &Interactive<Script>, call: &(ToolCall, ToolAction)) -> bool {
        policy.decide(&call.0, &call.1).await.is_allowed()
    }

    #[tokio::test]
    async fn one_answer_covers_the_turn_and_no_further() {
        let policy = Interactive::new(Script::new(vec![ApprovalReply::ForTurn]));
        let turn = [exec("1", "git add ."), exec("2", "git commit -m x")];
        policy.begin_turn(&turn);
        assert!(allowed(&policy, &turn[0]).await);
        assert!(allowed(&policy, &turn[1]).await);

        let asked = policy.approver.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert_eq!(
            asked[0].1.also_pending,
            vec!["exec git commit -m x".to_owned()]
        );
        assert_eq!(asked[0].1.program.as_deref(), Some("git"));

        let next = [exec("3", "git push")];
        policy.begin_turn(&next);
        assert!(
            !allowed(&policy, &next[0]).await,
            "the next turn asks again"
        );
    }

    #[tokio::test]
    async fn a_program_rule_covers_that_program_only() {
        let policy = Interactive::new(Script::new(vec![ApprovalReply::ForProgram]));
        assert!(allowed(&policy, &exec("1", "git status")).await);
        assert!(allowed(&policy, &exec("2", "git log -1")).await);
        assert!(!allowed(&policy, &exec("3", "gitk")).await);
        assert_eq!(policy.approver.asked.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_shell_line_gets_no_program_rule() {
        let policy = Interactive::new(Script::new(vec![ApprovalReply::ForProgram]));
        let shell = ToolCall {
            id: "1".into(),
            name: "shell".into(),
            arguments: serde_json::Value::Null,
        };
        let first = ToolAction::process("git status; rm -rf src");
        assert!(policy.decide(&shell, &first).await.is_allowed());
        let other = ToolAction::process("git log");
        let shell2 = ToolCall {
            id: "2".into(),
            ..shell.clone()
        };
        assert!(!policy.decide(&shell2, &other).await.is_allowed());
        assert_eq!(policy.approver.asked.lock().unwrap()[0].1.program, None);
    }

    #[tokio::test]
    async fn an_unconfined_call_is_asked_every_time_and_never_remembered() {
        let policy = Interactive::new(Script::new(vec![
            ApprovalReply::AllProcesses,
            ApprovalReply::Remember,
            ApprovalReply::Reject(None),
        ]));
        let call = ToolCall {
            id: "1".into(),
            name: "exec".into(),
            arguments: serde_json::Value::Null,
        };
        let push = ToolAction::unconfined_process("git push", "publish the branch");
        assert!(policy.decide(&call, &push).await.is_allowed());
        assert!(policy.decide(&call, &push).await.is_allowed());
        assert!(!policy.decide(&call, &push).await.is_allowed());

        let asked = policy.approver.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 3);
        assert_eq!(asked[0].1.unconfined.as_deref(), Some("publish the branch"));
        assert!(policy.granted_rules().is_empty());
        // Nor did the first answer reach sandboxed processes.
        assert!(!allowed(&policy, &exec("2", "git status")).await);
    }

    #[tokio::test]
    async fn all_processes_covers_processes_but_not_edits() {
        let policy = Interactive::new(Script::new(vec![ApprovalReply::AllProcesses]));
        assert!(allowed(&policy, &exec("1", "git status")).await);
        assert!(allowed(&policy, &exec("2", "cargo test")).await);
        let edit = ToolCall {
            id: "3".into(),
            name: "write_file".into(),
            arguments: serde_json::Value::Null,
        };
        assert!(
            !policy
                .decide(&edit, &ToolAction::mutating("src/lib.rs"))
                .await
                .is_allowed()
        );
    }

    fn edit(id: &str, path: &str) -> (ToolCall, ToolAction) {
        (
            ToolCall {
                id: id.into(),
                name: "apply_patch".into(),
                arguments: serde_json::Value::Null,
            },
            ToolAction::mutating(path),
        )
    }

    #[tokio::test]
    async fn every_edit_covers_later_edits_but_not_guarded_paths_or_processes() {
        let policy = Interactive::new(Script::new(vec![ApprovalReply::AllEdits]))
            .guarding(vec![".cargo/config.toml".into()]);
        assert!(allowed(&policy, &edit("1", "src/lib.rs")).await);
        assert!(allowed(&policy, &edit("2", "src/main.rs")).await);
        assert!(!allowed(&policy, &edit("3", ".cargo/config.toml")).await);
        assert!(!allowed(&policy, &exec("4", "cargo test")).await);
        assert_eq!(policy.approver.asked.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn accepting_edits_asks_only_for_guarded_paths_and_processes() {
        let policy = Interactive::new(Script::new(vec![]))
            .accepting_edits()
            .guarding(vec![".git/hooks".into()]);
        assert!(allowed(&policy, &edit("1", "src/lib.rs")).await);
        assert!(!allowed(&policy, &edit("2", ".git/hooks/pre-commit")).await);
        assert!(!allowed(&policy, &exec("3", "cargo test")).await);
        assert_eq!(policy.approver.asked.lock().unwrap().len(), 2);
    }

    fn schedule_create(id: &str) -> (ToolCall, ToolAction) {
        (
            ToolCall {
                id: id.into(),
                name: "schedule_create".into(),
                arguments: serde_json::Value::Null,
            },
            ToolAction::mutating(".dino/schedules/daily.json"),
        )
    }

    #[tokio::test]
    async fn a_session_rule_never_covers_a_guarded_schedule_create() {
        let policy = Interactive::new(Script::new(vec![
            ApprovalReply::ForSession,
            ApprovalReply::Once,
        ]))
        .guarding(vec![".dino/schedules".into()]);
        // The person says "always for this file" on the first create.
        assert!(allowed(&policy, &schedule_create("1")).await);
        // The next create still asks, and is allowed only by the fresh answer:
        // a guarded path is never covered by a remembered rule.
        assert!(allowed(&policy, &schedule_create("2")).await);
        let asked = policy.approver.asked.lock().unwrap();
        assert_eq!(asked.len(), 2, "the second create asked again");
        assert!(asked[0].1.guarded, "the ask carries the guarded flag");
    }

    #[tokio::test]
    async fn a_saved_rule_never_covers_a_guarded_schedule_create() {
        let mut policy = Interactive::new(Script::new(vec![ApprovalReply::Reject(None)]))
            .guarding(vec![".dino/schedules".into()]);
        let create = schedule_create("1");
        // Even with the matching key already saved from a past session, it asks.
        policy.saved.insert(create.1.rule_key(&create.0.name));
        assert!(!allowed(&policy, &create).await);
        assert_eq!(policy.approver.asked.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_note_to_memory_is_kept_without_asking() {
        let policy = Interactive::new(Script::new(vec![]));
        let remember = ToolCall {
            id: "1".into(),
            name: crate::memory::REMEMBER.into(),
            arguments: serde_json::Value::Null,
        };
        let note = ToolAction::mutating("/home/me/.config/dino/memory/repo.md");
        let decision = policy.decide(&remember, &note).await;
        assert_eq!(decision, ApprovalDecision::allowed(DecisionSource::Policy));
        assert!(policy.approver.asked.lock().unwrap().is_empty());
    }
}
