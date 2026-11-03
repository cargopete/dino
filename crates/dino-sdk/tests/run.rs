//! The SDK's surface, driven by a scripted model.

use std::collections::VecDeque;
use std::path::PathBuf;

use dino_model::ModelEventStream;
use dino_model::ModelFuture;
use dino_protocol::ModelProfile;
use dino_protocol::TurnInput;
use dino_sdk::AgentConfig;
use dino_sdk::AgentEvent;
use dino_sdk::DinoAgent;
use dino_sdk::ModelEvent;
use dino_sdk::ModelSession;
use dino_sdk::ReadOnly;
use dino_sdk::StopReason;
use dino_sdk::ToolCall;
use dino_sdk::Update;
use futures_util::stream;
use serde_json::json;

struct Scripted {
    profile: ModelProfile,
    turns: VecDeque<Vec<ModelEvent>>,
}

impl ModelSession for Scripted {
    fn profile(&self) -> &ModelProfile {
        &self.profile
    }

    fn next(&mut self, _input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
        let events = self.turns.pop_front().unwrap_or_default();
        Box::pin(async move {
            Ok(Box::pin(stream::iter(events.into_iter().map(Ok))) as ModelEventStream)
        })
    }
}

fn call(id: &str, name: &str, arguments: serde_json::Value) -> Vec<ModelEvent> {
    vec![
        ModelEvent::ToolCallCompleted {
            call: ToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            },
        },
        ModelEvent::Finished {
            reason: StopReason::ToolUse,
        },
    ]
}

fn workspace() -> PathBuf {
    let path = std::env::temp_dir().join(format!("dino-sdk-run-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("notes.txt"), "the answer is 42\n").unwrap();
    path
}

#[tokio::test]
async fn a_read_only_agent_streams_its_events_and_refuses_an_edit() {
    let root = workspace();
    let session = Scripted {
        profile: dino_model::builtin_profiles().remove(0),
        turns: VecDeque::from([
            call("1", "read", json!({ "path": "notes.txt" })),
            call(
                "2",
                "write_file",
                json!({ "path": "notes.txt", "content": "changed\n" }),
            ),
            vec![
                ModelEvent::TextDelta { text: "42".into() },
                ModelEvent::Finished {
                    reason: StopReason::EndTurn,
                },
            ],
        ]),
    };
    let tools = dino_sdk::workspace_tools(&root).unwrap();
    let (sink, mut updates) = dino_sdk::updates();
    let mut agent = DinoAgent::new(Box::new(session), tools, AgentConfig::default())
        .with_policy(ReadOnly)
        .with_sink(sink);

    let result = agent.run("what is the answer?").await.unwrap();
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.text, "42");

    let mut seen = Vec::new();
    while let Ok(update) = updates.try_recv() {
        seen.push(update);
    }
    assert!(matches!(
        seen.first(),
        Some(Update::Event(AgentEvent::Submitted { .. }))
    ));
    assert!(matches!(seen.last(), Some(Update::Finished(outcome)) if outcome.error.is_none()));
    let read = seen.iter().find_map(|update| match update {
        Update::Event(AgentEvent::ToolFinished { result, .. }) if result.call_id == "1" => {
            Some(result.output.content.clone())
        }
        _ => None,
    });
    assert!(read.unwrap().contains("the answer is 42"));
    let refused = seen.iter().any(|update| {
        matches!(update, Update::Event(AgentEvent::ToolDecided { call_id, decision, .. })
            if call_id == "2" && !decision.is_allowed())
    });
    assert!(refused, "{seen:#?}");
    assert_eq!(
        std::fs::read_to_string(root.join("notes.txt")).unwrap(),
        "the answer is 42\n"
    );
    let _ = std::fs::remove_dir_all(&root);
}
