//! The zero-context review before a run ends, with scripted models.

#[allow(dead_code, reason = "shared with agent.rs, which uses the rest")]
mod support;

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use dino_core::Agent;
use dino_core::AgentConfig;
use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_core::approval::AllowAll;
use dino_core::review::Reviewer;
use dino_model::ModelEventStream;
use dino_model::ModelFuture;
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
use dino_protocol::TurnInput;
use futures_util::stream;
use pretty_assertions::assert_eq;
use serde_json::json;

use crate::support::ScriptedSession;
use crate::support::Turn;
use crate::support::text_turn;
use crate::support::tool_turn;

struct Workspace(PathBuf);

impl Workspace {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("dino-review-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("lib.rs"), "fn old() {}\n").unwrap();
        Self(root)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `write` puts its `text` in its `path`; `cargo` answers with `output`.
struct Files {
    root: PathBuf,
}

impl ToolRuntime for Files {
    fn definitions(&self) -> Vec<ToolDefinition> {
        ["write", "cargo"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: name.into(),
                input_schema: json!({"type": "object"}),
            })
            .collect()
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        match call.name.as_str() {
            "write" => Ok(ToolAction::mutating("lib.rs")),
            "cargo" => Ok(ToolAction::process("cargo test")),
            name => Err(ToolError::new(format!("unknown tool {name:?}"))),
        }
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let argument = |key: &str| call.arguments[key].as_str().unwrap_or_default().to_owned();
        let output = if call.name == "write" {
            std::fs::write(self.root.join(argument("path")), argument("text")).unwrap();
            "written".to_owned()
        } else {
            argument("output")
        };
        Box::pin(async move { Ok(ToolOutput::success(output)) })
    }
}

fn write(id: &str, text: &str) -> Turn {
    tool_turn(ToolCall {
        id: id.into(),
        name: "write".into(),
        arguments: json!({"path": "lib.rs", "text": text}),
    })
}

fn cargo_test(id: &str, output: &str) -> Turn {
    tool_turn(ToolCall {
        id: id.into(),
        name: "cargo".into(),
        arguments: json!({"command": "test", "output": output}),
    })
}

/// A reviewer's session: one reply, and what it was sent kept where the test
/// can read it after the session is dropped.
struct ReviewModel {
    profile: ModelProfile,
    reply: String,
    asked: Arc<Mutex<Vec<(String, String)>>>,
    system: String,
}

impl ModelSession for ReviewModel {
    fn profile(&self) -> &ModelProfile {
        &self.profile
    }

    fn next(&mut self, input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
        let TurnInput::User { content } = input else {
            panic!("a review is one user message");
        };
        self.asked
            .lock()
            .unwrap()
            .push((self.system.clone(), content));
        let events = vec![
            ModelEvent::TextDelta {
                text: self.reply.clone(),
            },
            ModelEvent::Usage {
                usage: TokenUsage {
                    input_tokens: 900,
                    output_tokens: 30,
                    ..TokenUsage::default()
                },
            },
            ModelEvent::Finished {
                reason: StopReason::EndTurn,
            },
        ];
        Box::pin(async move {
            Ok(Box::pin(stream::iter(events.into_iter().map(Ok))) as ModelEventStream)
        })
    }
}

type Asked = Arc<Mutex<Vec<(String, String)>>>;

fn reviewer(root: &Path, reply: &str) -> (Arc<Reviewer>, Asked) {
    let asked: Asked = Arc::default();
    let shared = Arc::clone(&asked);
    let reply = reply.to_owned();
    let reviewer = Reviewer::new(
        root,
        Box::new(move |system| {
            Ok(Box::new(ReviewModel {
                profile: dino_model::builtin_profiles().remove(0),
                reply: reply.clone(),
                asked: Arc::clone(&shared),
                system,
            }) as Box<dyn ModelSession>)
        }),
    );
    (Arc::new(reviewer), asked)
}

fn agent(root: &Path, turns: Vec<Turn>, reviewer: &Arc<Reviewer>) -> Agent<ScriptedSession, Files> {
    Agent::new(
        ScriptedSession::new(turns),
        Files {
            root: root.to_path_buf(),
        },
        AgentConfig::default(),
    )
    .with_policy(AllowAll)
    .with_reviewer(Arc::clone(reviewer))
}

fn reviews(result: &dino_core::RunResult) -> Vec<(u32, Vec<String>, Option<String>)> {
    result
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Reviewed {
                model_turn,
                objections,
                error,
                ..
            } => Some((*model_turn, objections.clone(), error.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn objections_go_back_once_and_the_model_may_act_on_them() {
    let workspace = Workspace::new("objects");
    let (reviewer, asked) = reviewer(
        &workspace.0,
        "- `new` drops the old function the task kept\n- the test output shows a failure",
    );
    let mut agent = agent(
        &workspace.0,
        vec![
            write("1", "fn new() {}\n"),
            cargo_test("2", "test result: ok. 3 passed"),
            text_turn("done"),
            write("3", "fn old() {}\nfn new() {}\n"),
            text_turn("kept old; the tests passed"),
        ],
        &reviewer,
    );

    let result = agent.run("add `new` and keep `old`").await.unwrap();

    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.model_turns, 5);
    assert_eq!(
        reviews(&result),
        [(
            3,
            vec![
                "`new` drops the old function the task kept".to_owned(),
                "the test output shows a failure".to_owned()
            ],
            None
        )]
    );
    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 1, "one review a run");
    let (system, request) = &asked[0];
    assert_eq!(system, dino_core::review::SYSTEM_PROMPT);
    assert!(request.contains("add `new` and keep `old`"), "{request}");
    assert!(request.contains("-fn old() {}\n+fn new() {}"), "{request}");
    assert!(
        request.contains("`cargo test`") && request.contains("3 passed"),
        "{request}"
    );
    let TurnInput::User { content } = &agent.session().inputs[3] else {
        panic!("the objections are the model's next message");
    };
    assert!(
        content.contains("- `new` drops the old function") && content.contains("no second review"),
        "{content}"
    );
    assert_eq!(reviewer.changed_since_review(), Some(true));
}

#[tokio::test]
async fn no_objections_ends_the_run_and_nothing_changed_after() {
    let workspace = Workspace::new("passes");
    let (reviewer, asked) = reviewer(&workspace.0, "NO OBJECTIONS");
    let mut agent = agent(
        &workspace.0,
        vec![write("1", "fn old() {}\nfn new() {}\n"), text_turn("done")],
        &reviewer,
    );

    let result = agent.run("add `new`").await.unwrap();

    assert_eq!(result.model_turns, 2);
    assert_eq!(reviews(&result), [(2, Vec::new(), None)]);
    assert!(
        asked.lock().unwrap()[0]
            .1
            .contains("No tests or checks were run.")
    );
    assert_eq!(reviewer.changed_since_review(), Some(false));
    let Some(AgentEvent::Reviewed { usage, .. }) = result
        .events
        .iter()
        .find(|event| matches!(event, AgentEvent::Reviewed { .. }))
    else {
        unreachable!();
    };
    assert_eq!(usage.input_tokens, 900);
}

#[tokio::test]
async fn a_run_that_changed_nothing_is_not_reviewed() {
    let workspace = Workspace::new("unchanged");
    let (reviewer, asked) = reviewer(&workspace.0, "- anything");
    let mut agent = agent(&workspace.0, vec![text_turn("it says old")], &reviewer);

    let result = agent.run("what does lib.rs say?").await.unwrap();

    assert_eq!(result.model_turns, 1);
    assert!(reviews(&result).is_empty());
    assert!(asked.lock().unwrap().is_empty());
    assert_eq!(reviewer.changed_since_review(), None);
}

#[tokio::test]
async fn a_reviewer_that_fails_or_rambles_objects_to_nothing() {
    let workspace = Workspace::new("fails");
    let failing = Arc::new(Reviewer::new(
        &workspace.0,
        Box::new(|_| Err("REVIEW_KEY is not set".to_owned())),
    ));
    let mut first = agent(
        &workspace.0,
        vec![write("1", "fn new() {}\n"), text_turn("done")],
        &failing,
    );
    let result = first.run("add `new`").await.unwrap();
    assert_eq!(result.model_turns, 2);
    assert_eq!(
        reviews(&result),
        [(2, Vec::new(), Some("REVIEW_KEY is not set".to_owned()))]
    );

    let (rambling, _) = reviewer(&workspace.0, "It looks broadly sensible.");
    let mut second = agent(
        &workspace.0,
        vec![write("1", "fn newer() {}\n"), text_turn("done")],
        &rambling,
    );
    let result = second.run("add `newer`").await.unwrap();
    let [(_, objections, Some(error))] = &reviews(&result)[..] else {
        panic!("a reply that is neither a pass nor a list is an error");
    };
    assert!(
        objections.is_empty() && error.contains("NO OBJECTIONS"),
        "{error}"
    );
}

struct BrokenOnce(std::sync::atomic::AtomicBool);

impl dino_core::FinishCheck for BrokenOnce {
    fn unfinished(&self) -> dino_core::CheckFuture<'_> {
        let first = !self.0.swap(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move { first.then(|| "the build is broken.".to_owned()) })
    }
}

#[tokio::test]
async fn work_sent_back_unfinished_is_reviewed_once_it_is_finished() {
    let workspace = Workspace::new("after");
    let (reviewer, asked) = reviewer(&workspace.0, "NO OBJECTIONS");
    let mut agent = agent(
        &workspace.0,
        vec![
            write("1", "fn new( {}\n"),
            text_turn("done"),
            write("2", "fn new() {}\n"),
            text_turn("fixed"),
        ],
        &reviewer,
    )
    .with_finish_check(BrokenOnce(std::sync::atomic::AtomicBool::new(false)));

    let result = agent.run("add `new`").await.unwrap();

    assert_eq!(result.model_turns, 4);
    assert_eq!(reviews(&result), [(4, Vec::new(), None)]);
    assert_eq!(asked.lock().unwrap().len(), 1);
}
