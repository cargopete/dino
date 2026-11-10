//! The `agent` tool: a bounded job handed to a fresh session in this process,
//! which returns only its conclusion and a line saying what it cost.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use anyhow::Result;
use dino_core::Agent;
use dino_core::AgentConfig;
use dino_core::AgentError;
use dino_core::RunResult;
use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_core::ToolSet;
use dino_core::approval::ApprovalPolicy;
use dino_core::approval::Approver;
use dino_core::approval::AskContext;
use dino_core::approval::ReadOnly;
use dino_core::approval::ReplyFuture;
use dino_core::prompt::PromptContext;
use dino_core::session::EventSink;
use dino_core::session::JsonlSessionLog;
use dino_core::session::SessionId;
use dino_core::session::SessionMeta;
use dino_core::session::SinkError;
use dino_core::subagent::AGENT;
use dino_model::ModelSession;
use dino_protocol::AgentEvent;
use dino_protocol::ModelEvent;
use dino_protocol::ModelProfile;
use dino_protocol::StopReason;
use dino_protocol::TokenUsage;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use dino_sandbox::Sandbox;
use serde::Deserialize;
use serde_json::json;

use crate::config::ApprovalMode;
use crate::config::RunSettings;
use crate::preset::Preset;
use crate::spinner::Status;

/// Told to every child, after its job: it has no one to ask, and its last
/// message is all that goes back.
const BRIEF: &str = "You are a subagent. Another session handed you this job and will see \
    only your final message, none of your calls. Do the job, then end with a final message \
    that is the whole answer: the conclusion, and the paths, lines and evidence it rests on. \
    Nobody can answer a question from you, so do not ask one.";

pub type OpenSession = Box<
    dyn Fn(&RunSettings, String, Vec<ToolDefinition>) -> Result<Box<dyn ModelSession>>
        + Send
        + Sync,
>;
pub type OpenTools = Box<dyn Fn(&RunSettings) -> Result<ToolSet> + Send + Sync>;
pub type OpenPolicy = Box<dyn Fn(ApprovalMode) -> Box<dyn ApprovalPolicy> + Send + Sync>;

/// What a child is built from: the parent's settings and prompt, and how it
/// opens a model, tools and approvals.
pub struct Kit {
    pub settings: RunSettings,
    pub session_id: SessionId,
    pub sandbox: Arc<dyn Sandbox>,
    pub context: PromptContext,
    pub status: Arc<Mutex<Status>>,
    pub open: OpenSession,
    pub tools: OpenTools,
    /// Asked only for a preset that can change anything; `review` is read-only.
    pub policy: OpenPolicy,
}

impl Kit {
    /// The models a child may be put on: those the person configured.
    fn models(&self) -> impl Iterator<Item = &ModelProfile> {
        std::iter::once(&self.settings.profile)
            .chain(&self.settings.explore)
            .chain(&self.settings.review)
            .chain(&self.settings.fallback)
    }
}

/// The `agent` tool. Its kit is set once the session it belongs to is built,
/// since a child needs that session's prompt, meter and status row.
pub struct Delegate {
    kit: Arc<OnceLock<Kit>>,
    started: AtomicU32,
}

impl Delegate {
    pub fn new() -> (Self, Arc<OnceLock<Kit>>) {
        let kit = Arc::new(OnceLock::new());
        (
            Self {
                kit: Arc::clone(&kit),
                started: AtomicU32::new(0),
            },
            kit,
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    prompt: String,
    preset: Option<Preset>,
    model: Option<String>,
    max_turns: Option<NonZeroU32>,
}

/// A call resolved against the kit: what the child will run on.
struct Job {
    prompt: String,
    preset: Preset,
    profile: ModelProfile,
    turns: NonZeroU32,
}

fn arguments(call: &ToolCall) -> Result<Arguments, ToolError> {
    let arguments: Arguments = serde_json::from_value(call.arguments.clone())
        .map_err(|error| ToolError::new(format!("invalid {AGENT} arguments: {error}")))?;
    if arguments.prompt.trim().is_empty() {
        return Err(ToolError::new("an agent needs a prompt saying what to do"));
    }
    Ok(arguments)
}

fn job(kit: &Kit, arguments: Arguments) -> Result<Job, ToolError> {
    let preset = arguments.preset.unwrap_or(Preset::Review);
    let profile = match arguments.model.as_deref() {
        Some(key) => kit
            .models()
            .find(|profile| profile.key == key)
            .cloned()
            .ok_or_else(|| {
                let mut keys: Vec<&str> =
                    kit.models().map(|profile| profile.key.as_str()).collect();
                keys.dedup();
                ToolError::new(format!(
                    "model {key:?} is not one this session may use; leave model out, or use \
                     one of: {}",
                    keys.join(", ")
                ))
            })?,
        None => match (&kit.settings.explore, preset) {
            (Some(explore), Preset::Review) => explore.clone(),
            _ => kit.settings.profile.clone(),
        },
    };
    let most = kit.settings.max_turns;
    let turns = arguments
        .max_turns
        .or_else(|| preset.max_turns())
        .unwrap_or(most)
        .min(most);
    Ok(Job {
        prompt: arguments.prompt,
        preset,
        profile,
        turns,
    })
}

impl ToolRuntime for Delegate {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: AGENT.into(),
            description: "Hand a bounded job to a fresh session and get back only its final \
                          message: a sweep of many files, or a question that needs a lot of \
                          reading, so the reading stays out of this conversation. It sees the \
                          system prompt and instructions but none of this conversation, so say \
                          in prompt everything it needs and what to report. preset review, the \
                          default, only reads; minimal and standard may edit and run commands \
                          under this session's approvals. Several agent calls in one response \
                          run at once. A subagent cannot start agents of its own."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "minLength": 1},
                    "preset": {"type": "string", "enum": ["review", "minimal", "standard"]},
                    "model": {
                        "type": "string",
                        "description": "A model key this session is configured with. Leave \
                                        out for the default."
                    },
                    "max_turns": {"type": "integer", "minimum": 1}
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
        }]
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if call.name != AGENT {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        let arguments = arguments(call)?;
        let preset = match self.kit.get() {
            Some(kit) => job(kit, arguments)?.preset,
            None => arguments.preset.unwrap_or(Preset::Review),
        };
        Ok(match preset {
            Preset::Review => ToolAction::read_only(),
            // Each of its own calls is decided as well; this asks first whether
            // to start something that may change the workspace at all.
            Preset::Minimal | Preset::Standard => {
                ToolAction::mutating(format!("a {} subagent", preset.name()))
            }
        })
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let call = call.clone();
        Box::pin(async move {
            let kit = self
                .kit
                .get()
                .ok_or_else(|| ToolError::new("subagents are not available in this session"))?;
            let job = job(kit, arguments(&call)?)?;
            let nth = self.started.fetch_add(1, Ordering::Relaxed) + 1;
            Ok(run(kit, job, nth).await)
        })
    }
}

/// Asks a person through `inner` for one subagent at a time, with the status
/// row held off the prompt, since children run alongside each other.
pub struct Asking<A> {
    inner: A,
    hold: crate::spinner::Hold,
    turn: Arc<tokio::sync::Mutex<()>>,
}

impl<A> Asking<A> {
    pub fn new(inner: A, hold: crate::spinner::Hold, turn: Arc<tokio::sync::Mutex<()>>) -> Self {
        Self { inner, hold, turn }
    }
}

impl<A: Approver> Approver for Asking<A> {
    fn ask(&self, call: &ToolCall, action: &ToolAction) -> ReplyFuture<'_> {
        self.ask_in(call, action, &AskContext::default())
    }

    fn ask_in(
        &self,
        call: &ToolCall,
        action: &ToolAction,
        context: &AskContext,
    ) -> ReplyFuture<'_> {
        let (call, action, context) = (call.clone(), action.clone(), context.clone());
        Box::pin(async move {
            let _turn = self.turn.lock().await;
            let held = self.hold.take();
            let reply = self.inner.ask_in(&call, &action, &context).await;
            self.hold.release(held);
            reply
        })
    }
}

/// Where a child's log goes: beside its parent's, in a directory named after it.
pub fn child_log(parent_log: &std::path::Path, child: &SessionId) -> PathBuf {
    parent_log
        .with_extension("")
        .join(format!("{}.jsonl", child.as_str()))
}

async fn run(kit: &Kit, job: Job, nth: u32) -> ToolOutput {
    let id = kit.session_id.child(nth);
    let tally = Arc::new(Mutex::new(Tally::default()));
    let outcome = start(kit, &job, &id, &tally).await;
    let tally = tally.lock().map(|tally| tally.clone()).unwrap_or_default();
    let footer = footer(&job.profile, &tally, &id);
    match outcome {
        // A blocker's reason is not text the model wrote, so only the run has it.
        Ok(result) if result.stop_reason == StopReason::Blocked => {
            ToolOutput::success(format!("{}\n\n{footer}", result.text.trim()))
        }
        Ok(_) => ToolOutput::success(format!("{}\n\n{footer}", tally.said.trim())),
        Err(Failed::Run(AgentError::ModelTurnLimit(turns))) => ToolOutput::success(format!(
            "{}\n\n[the subagent stopped at its limit of {turns} turns, so this may be \
             unfinished]\n{footer}",
            tally.said.trim()
        )),
        Err(Failed::Run(error)) => {
            ToolOutput::error(format!("the subagent failed: {error}\n{footer}"))
        }
        Err(Failed::Start(error)) => {
            ToolOutput::error(format!("the subagent could not start: {error:#}"))
        }
    }
}

enum Failed {
    Start(anyhow::Error),
    Run(AgentError),
}

async fn start(
    kit: &Kit,
    job: &Job,
    id: &SessionId,
    tally: &Arc<Mutex<Tally>>,
) -> Result<RunResult, Failed> {
    let parent = &kit.settings;
    let same_model = job.profile.key == parent.profile.key;
    let mode = job.preset.approvals().unwrap_or(parent.mode);
    let settings = RunSettings {
        profile: job.profile.clone(),
        log_path: child_log(&parent.log_path, id),
        max_turns: job.turns,
        mode,
        preset: job.preset,
        // Another model's effort or fallbacks may not apply to this one.
        reasoning_effort: parent.reasoning_effort.filter(|_| same_model),
        fallback: if same_model {
            parent.fallback.clone()
        } else {
            Vec::new()
        },
        review: None,
        explore: None,
        ..parent.clone()
    };
    let tools = (kit.tools)(&settings)
        .map_err(Failed::Start)?
        .keeping(|name| name != AGENT && job.preset.offers(name));
    let definitions = tools.definitions();
    let context = PromptContext {
        model: Some(job.profile.key.clone()),
        tools: definitions
            .iter()
            .map(|definition| definition.name.clone())
            .collect(),
        approval_mode: mode.label().to_owned(),
        max_model_turns: Some(job.turns.get()),
        ..kit.context.clone()
    };
    let prompt = dino_core::prompt::system_prompt(&context);
    let model = (kit.open)(&settings, prompt, definitions).map_err(Failed::Start)?;
    let meta = SessionMeta {
        session_id: id.clone(),
        dino_version: env!("CARGO_PKG_VERSION").to_owned(),
        model_key: job.profile.key.clone(),
        provider: job.profile.provider,
        workspace_root: settings.workspace.display().to_string(),
        approval_mode: mode.label().to_owned(),
        sandbox: kit.sandbox.label(),
        max_model_turns: job.turns.get(),
    };
    let log = JsonlSessionLog::create(&settings.log_path, meta)
        .map_err(|error| Failed::Start(error.into()))?;
    let policy: Box<dyn ApprovalPolicy> = match job.preset {
        Preset::Review => Box::new(ReadOnly),
        Preset::Minimal | Preset::Standard => (kit.policy)(mode),
    };
    let watch = Watch {
        status: Arc::clone(&kit.status),
        slot: id.as_str().to_owned(),
        tally: Arc::clone(tally),
    };
    let mut agent = Agent::new(
        model,
        tools,
        AgentConfig {
            max_model_turns: job.turns,
            ..AgentConfig::default()
        },
    )
    .with_policy(policy)
    .with_sink((log, watch));
    agent
        .run(format!("{}\n\n{BRIEF}", job.prompt.trim()))
        .await
        .map_err(Failed::Run)
}

/// What a child has done so far, kept outside it so a run that fails still
/// says what it cost and what it last said.
#[derive(Debug, Clone, Default)]
struct Tally {
    turns: u32,
    usage: TokenUsage,
    /// The text of the latest turn that wrote any.
    said: String,
    writing: String,
}

/// Shows a child's turn in the parent's status row and keeps its tally.
struct Watch {
    status: Arc<Mutex<Status>>,
    slot: String,
    tally: Arc<Mutex<Tally>>,
}

impl EventSink for Watch {
    fn emit(&mut self, event: &AgentEvent) -> Result<(), SinkError> {
        let AgentEvent::Model { model_turn, event } = event else {
            return Ok(());
        };
        let Ok(mut tally) = self.tally.lock() else {
            return Ok(());
        };
        match event {
            ModelEvent::Started { .. } => {
                tally.turns = *model_turn;
                tally.writing.clear();
                if let Ok(mut status) = self.status.lock() {
                    status.agents.insert(self.slot.clone(), *model_turn);
                }
            }
            ModelEvent::TextDelta { text } => {
                tally.writing.push_str(text);
                if !tally.writing.trim().is_empty() {
                    let Tally { said, writing, .. } = &mut *tally;
                    said.clone_from(writing);
                }
            }
            ModelEvent::Usage { usage } => {
                let total = &mut tally.usage;
                total.input_tokens += usage.input_tokens;
                total.cached_input_tokens += usage.cached_input_tokens;
                total.cache_write_input_tokens += usage.cache_write_input_tokens;
                total.output_tokens += usage.output_tokens;
                total.reasoning_tokens += usage.reasoning_tokens;
            }
            _ => {}
        }
        Ok(())
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Ok(mut status) = self.status.lock() {
            status.agents.remove(&self.slot);
        }
    }
}

/// The line an `agent` result ends with, read back by [`gist`] and
/// [`child_session`].
fn footer(profile: &ModelProfile, tally: &Tally, id: &SessionId) -> String {
    let mut parts = vec![
        format!("{} turn(s)", tally.turns),
        format!(
            "{} in · {} out",
            crate::render::compact(tally.usage.input_tokens),
            crate::render::compact(tally.usage.output_tokens)
        ),
    ];
    if let Some(price) = profile.price {
        parts.push(crate::spend::usd(price.cost(&tally.usage)));
    }
    parts.push(profile.key.clone());
    parts.push(format!("session {}", id.as_str()));
    format!("[agent: {}]", parts.join(" · "))
}

fn read_footer(content: &str) -> Option<(&str, &str)> {
    content
        .lines()
        .last()?
        .strip_prefix("[agent: ")?
        .strip_suffix(']')?
        .rsplit_once(" · session ")
}

/// What an `agent` result cost, as its footer says.
pub fn gist(content: &str) -> Option<&str> {
    read_footer(content).map(|(gist, _)| gist)
}

/// The child session an `agent` result names, for following it in a trace.
pub fn child_session(content: &str) -> Option<&str> {
    read_footer(content).map(|(_, session)| session)
}

/// The status row's part for the children running, by their turn.
pub fn status_part(agents: &BTreeMap<String, u32>) -> Option<String> {
    let turns: Vec<String> = agents.values().map(u32::to_string).collect();
    match turns.len() {
        0 => None,
        1 => Some(format!("agent at turn {}", turns[0])),
        n => Some(format!("{n} agents at turns {}", turns.join(", "))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use dino_core::approval::AllowAll;
    use dino_model::ModelError;
    use dino_model::ModelEventStream;
    use dino_model::ModelFuture;
    use dino_protocol::TurnInput;

    use super::*;

    /// A model that plays its turns in order, after meeting the others at
    /// `meet` on its first, and keeps what it was sent.
    struct Scripted {
        profile: ModelProfile,
        turns: VecDeque<Vec<ModelEvent>>,
        meet: Option<Arc<tokio::sync::Barrier>>,
        inputs: Arc<Mutex<Vec<TurnInput>>>,
    }

    impl Scripted {
        fn new(key: &str, turns: Vec<Vec<ModelEvent>>) -> Self {
            Self {
                profile: dino_model::builtin_profile(key).unwrap(),
                turns: turns.into(),
                meet: None,
                inputs: Arc::default(),
            }
        }
    }

    impl ModelSession for Scripted {
        fn profile(&self) -> &ModelProfile {
            &self.profile
        }

        fn next(&mut self, input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
            self.inputs.lock().unwrap().push(input);
            let turn = self.turns.pop_front();
            let meet = self.meet.take();
            Box::pin(async move {
                if let Some(meet) = meet {
                    meet.wait().await;
                }
                let events =
                    turn.ok_or_else(|| ModelError::Protocol("no turn left in the script".into()))?;
                Ok(
                    Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)))
                        as ModelEventStream,
                )
            })
        }
    }

    fn said(text: &str) -> Vec<ModelEvent> {
        vec![
            ModelEvent::Started { response_id: None },
            ModelEvent::TextDelta { text: text.into() },
            ModelEvent::Usage {
                usage: TokenUsage {
                    input_tokens: 10_000,
                    output_tokens: 2_000,
                    ..TokenUsage::default()
                },
            },
            ModelEvent::Finished {
                reason: StopReason::EndTurn,
            },
        ]
    }

    fn calls(calls: Vec<ToolCall>) -> Vec<ModelEvent> {
        let mut events = vec![ModelEvent::Started { response_id: None }];
        events.extend(
            calls
                .into_iter()
                .map(|call| ModelEvent::ToolCallCompleted { call }),
        );
        events.push(ModelEvent::Finished {
            reason: StopReason::ToolUse,
        });
        events
    }

    fn agent_call(id: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: AGENT.into(),
            arguments,
        }
    }

    fn workspace(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("dino-subagent-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn settings(root: &std::path::Path) -> RunSettings {
        RunSettings {
            profile: dino_model::builtin_profile("claude-sonnet-5.5").unwrap(),
            workspace: root.to_path_buf(),
            log_path: root.join(".dino/sessions/parent.jsonl"),
            max_turns: NonZeroU32::new(30).unwrap(),
            mode: ApprovalMode::AllowAll,
            show_reasoning: false,
            sandbox: crate::config::SandboxMode::None,
            protected: Vec::new(),
            max_diagnostics: None,
            hooks: Vec::new(),
            end_turn_hooks: Vec::new(),
            start_hooks: Vec::new(),
            task_end_hooks: Vec::new(),
            verify: None,
            task_limits: crate::task::Limits::default(),
            mcp: Vec::new(),
            thinking_budget: None,
            compaction: crate::config::Compaction::Manual,
            keep_going: false,
            api_base: None,
            disable_thinking: false,
            enable_thinking: None,
            reasoning_effort: None,
            plain: true,
            greeting: false,
            fallback: Vec::new(),
            budget: None,
            review: None,
            explore: Some(dino_model::builtin_profile("claude-haiku-5.5").unwrap()),
            preset: Preset::Standard,
        }
    }

    /// A tool named like a read that says it writes, to show the policy, not
    /// the tool list, is what keeps a review child from changing anything.
    struct Sly;

    impl ToolRuntime for Sly {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "read".into(),
                description: "reads, it says".into(),
                input_schema: json!({"type": "object"}),
            }]
        }

        fn classify(&self, _call: &ToolCall) -> Result<ToolAction, ToolError> {
            Ok(ToolAction::mutating("src/lib.rs"))
        }

        fn execute(&self, _call: &ToolCall) -> ToolFuture<'_> {
            Box::pin(async { Ok(ToolOutput::success("written")) })
        }
    }

    type Opened = Arc<Mutex<Vec<(String, String, Vec<String>)>>>;

    /// A kit whose children play `script` and whose tools are `Sly`, recording
    /// each model opened: its key, system prompt and tool names.
    fn kit(
        root: &std::path::Path,
        script: Vec<Vec<ModelEvent>>,
        meet: Option<Arc<tokio::sync::Barrier>>,
    ) -> (Kit, Opened, Arc<Mutex<Vec<TurnInput>>>) {
        let opened = Opened::default();
        let inputs = Arc::<Mutex<Vec<TurnInput>>>::default();
        let (record, sent) = (Arc::clone(&opened), Arc::clone(&inputs));
        let kit = Kit {
            settings: settings(root),
            session_id: SessionId::generate(),
            sandbox: Arc::new(dino_sandbox::Unconfined),
            context: PromptContext {
                workspace_root: root.display().to_string(),
                model: None,
                tools: Vec::new(),
                approval_mode: String::new(),
                max_model_turns: None,
                git: None,
                outline: None,
                instructions: Vec::new(),
                memory: Vec::new(),
                skills: Vec::new(),
            },
            status: Arc::default(),
            open: Box::new(move |settings, prompt, definitions| {
                record.lock().unwrap().push((
                    settings.profile.key.clone(),
                    prompt,
                    definitions.into_iter().map(|tool| tool.name).collect(),
                ));
                let mut session = Scripted::new(&settings.profile.key, script.clone());
                session.meet = meet.clone();
                session.inputs = Arc::clone(&sent);
                Ok(Box::new(session))
            }),
            tools: Box::new(|_| {
                ToolSet::new(vec![Box::new(Sly)]).map_err(|error| anyhow::anyhow!("{error}"))
            }),
            policy: Box::new(|_| Box::new(AllowAll)),
        };
        (kit, opened, inputs)
    }

    fn delegate(kit: Kit) -> Delegate {
        let (delegate, slot) = Delegate::new();
        assert!(slot.set(kit).is_ok());
        delegate
    }

    #[tokio::test]
    async fn two_children_run_at_once_and_the_parent_gets_only_their_conclusions() {
        let root = workspace("two");
        let meet = Arc::new(tokio::sync::Barrier::new(2));
        let (kit, opened, inputs) = kit(&root, vec![said("It is in src/a.rs:12.")], Some(meet));
        let parent_id = kit.session_id.clone();
        let status = Arc::clone(&kit.status);
        let log = kit.settings.log_path.clone();
        let parent = Scripted::new(
            "claude-sonnet-5.5",
            vec![
                calls(vec![
                    agent_call("a", json!({"prompt": "Where is the parser?"})),
                    agent_call("b", json!({"prompt": "Where is the lexer?"})),
                ]),
                said("Both found."),
            ],
        );
        let parent_inputs = Arc::clone(&parent.inputs);
        let tools = ToolSet::new(vec![Box::new(delegate(kit))]).unwrap();
        let mut agent = Agent::new(parent, tools, AgentConfig::default());

        // Each child waits on its first turn for the other, so one at a time hangs.
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), agent.run("look"))
            .await
            .expect("the children ran at once")
            .unwrap();

        assert_eq!(result.text, "Both found.");
        let sent = parent_inputs.lock().unwrap();
        let TurnInput::ToolResults { results, .. } = &sent[1] else {
            panic!("unexpected {:?}", sent[1]);
        };
        for (result, nth) in results.iter().zip(1..) {
            let child = parent_id.child(nth);
            assert!(!result.output.is_error, "{}", result.output.content);
            assert_eq!(
                result.output.content,
                format!(
                    "It is in src/a.rs:12.\n\n[agent: 1 turn(s) · 10.0k in · 2.0k out · $0.0020 · \
                     claude-haiku-5.5 · session {}]",
                    child.as_str()
                )
            );
            assert_eq!(child_session(&result.output.content), Some(child.as_str()));
            let records = std::fs::read_to_string(child_log(&log, &child)).unwrap();
            assert!(records.contains("Where is the"), "{records}");
        }
        assert!(status.lock().unwrap().agents.is_empty());

        // Read-only children run on the explore model, without the agent tool,
        // and are told their last message is all that goes back.
        for (key, prompt, tools) in opened.lock().unwrap().iter() {
            assert_eq!(key, "claude-haiku-5.5");
            assert!(prompt.contains("Approval mode: read-only"), "{prompt}");
            assert_eq!(tools, &vec!["read".to_owned()]);
        }
        let TurnInput::User { content } = &inputs.lock().unwrap()[0] else {
            panic!("the child's first input is the job");
        };
        assert!(content.ends_with(BRIEF), "{content}");
    }

    #[tokio::test]
    async fn a_review_child_is_read_only_whatever_the_parent_allows() {
        let root = workspace("read-only");
        let (kit, _, inputs) = kit(
            &root,
            vec![
                calls(vec![ToolCall {
                    id: "w".into(),
                    name: "read".into(),
                    arguments: json!({}),
                }]),
                said("I could not write it."),
            ],
            None,
        );
        assert_eq!(kit.settings.mode, ApprovalMode::AllowAll);
        let delegate = delegate(kit);
        let call = agent_call("a", json!({"prompt": "Write src/lib.rs"}));

        assert_eq!(delegate.classify(&call).unwrap(), ToolAction::read_only());
        let output = delegate.execute(&call).await.unwrap();

        assert!(output.content.starts_with("I could not write it."));
        let sent = inputs.lock().unwrap();
        let TurnInput::ToolResults { results, .. } = &sent[1] else {
            panic!("unexpected {:?}", sent[1]);
        };
        assert_eq!(
            results[0].output.content,
            "refused by approval policy: this session runs in read-only mode"
        );
    }

    #[test]
    fn a_child_that_may_edit_asks_first_and_names_only_configured_models() {
        let root = workspace("classify");
        let (kit, _, _) = kit(&root, Vec::new(), None);
        let delegate = delegate(kit);

        let standard = agent_call("a", json!({"prompt": "fix it", "preset": "standard"}));
        assert_eq!(
            delegate.classify(&standard).unwrap(),
            ToolAction::mutating("a standard subagent")
        );
        let elsewhere = agent_call("b", json!({"prompt": "look", "model": "terra"}));
        let error = delegate.classify(&elsewhere).unwrap_err().to_string();
        assert!(
            error.contains("claude-sonnet-5.5, claude-haiku-5.5"),
            "{error}"
        );
        let empty = agent_call("c", json!({"prompt": "  "}));
        assert!(delegate.classify(&empty).is_err());
    }

    #[test]
    fn turns_default_to_the_preset_and_never_pass_the_parent() {
        let root = workspace("turns");
        let (kit, _, _) = kit(&root, Vec::new(), None);
        let turns = |arguments: serde_json::Value| {
            job(&kit, serde_json::from_value(arguments).unwrap())
                .unwrap()
                .turns
                .get()
        };
        // The review preset's 40 is past this parent's 30.
        assert_eq!(turns(json!({"prompt": "p"})), 30);
        assert_eq!(turns(json!({"prompt": "p", "max_turns": 8})), 8);
        assert_eq!(turns(json!({"prompt": "p", "max_turns": 99})), 30);
        let model = |arguments: serde_json::Value| {
            job(&kit, serde_json::from_value(arguments).unwrap())
                .unwrap()
                .profile
                .key
        };
        assert_eq!(model(json!({"prompt": "p"})), "claude-haiku-5.5");
        assert_eq!(
            model(json!({"prompt": "p", "preset": "minimal"})),
            "claude-sonnet-5.5"
        );
    }

    #[tokio::test]
    async fn a_child_out_of_turns_says_so_and_returns_what_it_had() {
        let root = workspace("limit");
        let read = |id: &str| {
            calls(vec![ToolCall {
                id: id.into(),
                name: "read".into(),
                arguments: json!({}),
            }])
        };
        let mut first = read("r1");
        first.insert(
            1,
            ModelEvent::TextDelta {
                text: "Half way: src/a.rs.".into(),
            },
        );
        let (kit, _, _) = kit(&root, vec![first, read("r2")], None);
        let delegate = delegate(kit);
        let call = agent_call("a", json!({"prompt": "look", "max_turns": 2}));

        let output = delegate.execute(&call).await.unwrap();

        assert!(!output.is_error);
        assert!(
            output.content.starts_with(
                "Half way: src/a.rs.\n\n[the subagent stopped at its limit of 2 turns"
            ),
            "{}",
            output.content
        );
        assert!(child_session(&output.content).is_some());
    }
}
