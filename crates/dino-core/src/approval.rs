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
}

impl ApprovalPolicy for Box<dyn ApprovalPolicy> {
    fn decide(&self, call: &ToolCall, action: &ToolAction) -> DecisionFuture<'_> {
        (**self).decide(call, action)
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
    /// Refuse, optionally saying why.
    Reject(Option<String>),
}

/// A frontend that can ask a person about a proposed action.
pub trait Approver: Send + Sync {
    fn ask(&self, call: &ToolCall, action: &ToolAction) -> ReplyFuture<'_>;
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
    saved: HashSet<String>,
    store: Option<Box<dyn RuleStore>>,
}

impl<A> Interactive<A> {
    pub fn new(approver: A) -> Self {
        Self {
            approver,
            rules: Mutex::new(HashSet::new()),
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
}

impl<A> ApprovalPolicy for Interactive<A>
where
    A: Approver,
{
    fn decide(&self, call: &ToolCall, action: &ToolAction) -> DecisionFuture<'_> {
        if action.class == ToolClass::ReadOnly {
            return Box::pin(async { ApprovalDecision::allowed(DecisionSource::Policy) });
        }

        let key = action.rule_key(&call.name);
        if self.saved.contains(&key) {
            return Box::pin(async { ApprovalDecision::allowed(DecisionSource::SavedRule) });
        }
        if self.holds_rule(&key) {
            return Box::pin(async { ApprovalDecision::allowed(DecisionSource::SessionRule) });
        }

        let call = call.clone();
        let action = action.clone();
        Box::pin(async move {
            match self.approver.ask(&call, &action).await {
                ApprovalReply::Once => ApprovalDecision::allowed(DecisionSource::User),
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
}
