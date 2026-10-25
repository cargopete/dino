//! The act-observe loop, its approval layer and its cancellation behaviour.

mod support;

use std::future::pending;
use std::sync::Arc;
use std::sync::Mutex;

use futures_util::StreamExt;
use futures_util::stream;
use gyr_core::Agent;
use gyr_core::AgentConfig;
use gyr_core::ToolRuntime;
use gyr_core::ToolSet;
use gyr_core::approval::AllowAll;
use gyr_core::approval::ApprovalReply;
use gyr_core::approval::Interactive;
use gyr_core::approval::ReadOnly;
use gyr_model::ModelError;
use gyr_model::ModelEventStream;
use gyr_protocol::AgentEvent;
use gyr_protocol::ApprovalDecision;
use gyr_protocol::DecisionSource;
use gyr_protocol::ModelEvent;
use gyr_protocol::StopReason;
use gyr_protocol::ToolClass;
use gyr_protocol::ToolOutput;
use gyr_protocol::ToolResult;
use gyr_protocol::TurnInput;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::support::EchoTool;
use crate::support::FailingSink;
use crate::support::ScriptedApprover;
use crate::support::ScriptedSession;
use crate::support::ScriptedTools;
use crate::support::Turn;
use crate::support::text_turn;
use crate::support::tool_call;
use crate::support::tool_turn;
use crate::support::usage_turn;

fn agent(turns: Vec<Turn>) -> Agent<ScriptedSession, ScriptedTools> {
    Agent::new(
        ScriptedSession::new(turns),
        ScriptedTools::default(),
        AgentConfig::default(),
    )
}

fn decisions(result: &gyr_core::RunResult) -> Vec<(String, ApprovalDecision)> {
    result
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolDecided {
                tool,
                action,
                decision,
                ..
            } => Some((action.rule_key(tool), decision.clone())),
            _ => None,
        })
        .collect()
}

fn tool_results(result: &gyr_core::RunResult) -> Vec<ToolResult> {
    result
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolFinished { result, .. } => Some(result.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn completes_a_tool_round_trip() {
    let call = tool_call("call-1", "read", "src/main.rs");
    let mut agent = agent(vec![
        tool_turn(call.clone()),
        text_turn("The file is small."),
    ]);

    let result = agent.run("inspect the entry point").await.unwrap();

    assert_eq!(result.text, "The file is small.");
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.model_turns, 2);
    assert_eq!(
        agent.session().inputs,
        vec![
            TurnInput::User {
                content: "inspect the entry point".into(),
            },
            TurnInput::ToolResults {
                results: vec![ToolResult {
                    call_id: "call-1".into(),
                    output: ToolOutput::success("fn main() {}"),
                }],
            },
        ]
    );
}

#[tokio::test]
async fn rejects_a_stream_without_a_terminal_event() {
    let mut agent = agent(vec![Turn::Events(vec![ModelEvent::TextDelta {
        text: "unfinished".into(),
    }])]);

    let error = agent.run("hello").await.unwrap_err();

    assert_eq!(
        error.to_string(),
        "model stream ended without a terminal event"
    );
}

#[tokio::test]
async fn rejects_duplicate_tool_call_ids_across_model_turns() {
    let call = tool_call("duplicate", "read", "Cargo.toml");
    let mut agent = agent(vec![tool_turn(call.clone()), tool_turn(call)]);

    let error = agent.run("read twice").await.unwrap_err();

    assert_eq!(error.to_string(), "provider reused tool call id duplicate");
}

#[tokio::test]
async fn allows_read_only_calls_without_asking_anybody() {
    let (approver, asked) = ScriptedApprover::new(Vec::new());
    let mut agent = agent(vec![
        tool_turn(tool_call("call-1", "read", "src/lib.rs")),
        text_turn("done"),
    ])
    .with_policy(Interactive::new(approver));

    let result = agent.run("read a file").await.unwrap();

    assert!(
        asked.lock().unwrap().is_empty(),
        "a read-only call must not reach a person"
    );
    assert_eq!(
        decisions(&result),
        vec![(
            "read".to_owned(),
            ApprovalDecision::allowed(DecisionSource::Policy)
        )]
    );
}

#[tokio::test]
async fn a_refusal_reaches_the_model_as_an_ordinary_tool_result() {
    let mut agent = agent(vec![
        tool_turn(tool_call("call-1", "apply_patch", "src/lib.rs")),
        text_turn("understood, I will not."),
    ])
    .with_policy(ReadOnly);

    let result = agent.run("edit a file").await.unwrap();

    let results = tool_results(&result);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].call_id, "call-1");
    assert!(results[0].output.is_error);
    assert!(
        results[0]
            .output
            .content
            .contains("refused by approval policy"),
        "said: {}",
        results[0].output.content
    );
    assert!(
        agent.session().inputs.contains(&TurnInput::ToolResults {
            results: results.clone(),
        }),
        "the refusal must be sent back to the provider under its own call ID"
    );
    assert!(
        agent.tools().executed_names().is_empty(),
        "a refused call must never execute"
    );
    assert_eq!(result.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn a_session_rule_covers_its_own_subject_and_no_other() {
    let (approver, asked) = ScriptedApprover::new(vec![
        ApprovalReply::ForSession,
        ApprovalReply::Reject(Some("not that one".into())),
    ]);
    let mut agent = agent(vec![
        tool_turn(tool_call("call-1", "apply_patch", "src/lib.rs")),
        tool_turn(tool_call("call-2", "apply_patch", "src/lib.rs")),
        tool_turn(tool_call("call-3", "apply_patch", "src/main.rs")),
        text_turn("finished"),
    ])
    .with_policy(Interactive::new(approver));

    let result = agent.run("edit two files").await.unwrap();

    assert_eq!(
        *asked.lock().unwrap(),
        vec![
            "apply_patch\u{1f}src/lib.rs".to_owned(),
            "apply_patch\u{1f}src/main.rs".to_owned(),
        ],
        "the second edit to the same file is covered by the rule; a different file is not"
    );
    assert_eq!(
        decisions(&result)
            .into_iter()
            .map(|(_, decision)| decision)
            .collect::<Vec<_>>(),
        vec![
            ApprovalDecision::allowed(DecisionSource::User),
            ApprovalDecision::allowed(DecisionSource::SessionRule),
            ApprovalDecision::denied("not that one"),
        ]
    );
    assert_eq!(agent.tools().executed_names().len(), 2);
}

#[tokio::test]
async fn an_unclassifiable_call_is_never_decided_and_never_executed() {
    let mut agent = agent(vec![
        tool_turn(tool_call("call-1", "launch_missiles", "silo/1")),
        text_turn("fair enough"),
    ])
    .with_policy(AllowAll);

    let result = agent.run("do something unwise").await.unwrap();

    assert!(
        decisions(&result).is_empty(),
        "a call that cannot be classified must not reach a policy"
    );
    let results = tool_results(&result);
    assert!(results[0].output.is_error);
    assert!(
        results[0].output.content.contains("unknown tool"),
        "said: {}",
        results[0].output.content
    );
    assert!(agent.tools().executed_names().is_empty());
}

#[tokio::test]
async fn a_call_carrying_an_elision_marker_is_refused_unrun() {
    let mut call = tool_call("call-1", "read", "src/lib.rs");
    call.arguments["old_text"] = json!(
        "[gyrfalcon elided 352 bytes of this earlier argument. The call ran as sent; read the \
         file if you need what it wrote.]"
    );
    let mut agent = agent(vec![tool_turn(call), text_turn("I will read it")]).with_policy(AllowAll);

    let result = agent.run("patch it").await.unwrap();

    let results = tool_results(&result);
    assert!(results[0].output.is_error);
    assert!(results[0].output.content.contains("placeholder"));
    assert!(decisions(&result).is_empty());
    assert!(agent.tools().executed_names().is_empty());
}

#[tokio::test]
async fn cancellation_stops_the_run_mid_stream() {
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let stream = stream::once(async {
        Ok(ModelEvent::TextDelta {
            text: "thinking abou".into(),
        })
    })
    .chain(stream::once(async move {
        // Cancel, then never resolve: only the token can end this turn.
        trigger.cancel();
        pending::<Result<ModelEvent, ModelError>>().await
    }));
    let mut agent = agent(vec![Turn::Stream(Box::pin(stream) as ModelEventStream)]);

    let result = agent
        .run_cancellable("start something long", &cancel)
        .await
        .unwrap();

    assert_eq!(result.stop_reason, StopReason::Cancelled);
    assert_eq!(result.text, "thinking abou");
    assert!(
        !result.events.iter().any(|event| matches!(
            event,
            AgentEvent::Model {
                event: ModelEvent::Finished { .. },
                ..
            }
        )),
        "the agent must not invent a terminal event the provider never sent"
    );
}

#[tokio::test]
async fn cancellation_between_tool_calls_stops_before_the_next_turn() {
    let cancel = CancellationToken::new();
    let mut agent = agent(vec![
        tool_turn(tool_call("call-1", "read", "src/lib.rs")),
        text_turn("never reached"),
    ]);
    cancel.cancel();

    let result = agent.run_cancellable("read a file", &cancel).await.unwrap();

    assert_eq!(result.stop_reason, StopReason::Cancelled);
    // The question was asked and then cancelled, so the log holds the question
    // and nothing else. Recording nothing at all would be tidier and less true.
    assert_eq!(
        result.events,
        vec![AgentEvent::Submitted {
            text: "read a file".into()
        }]
    );
    assert!(agent.session().inputs.is_empty());
}

#[tokio::test]
async fn a_sink_failure_fails_the_run() {
    let mut agent = agent(vec![text_turn("hello")]).with_sink(FailingSink {
        fail_at: 1,
        seen: 0,
    });

    let error = agent.run("say hello").await.unwrap_err();

    assert_eq!(
        error.to_string(),
        "session sink failed: the disk is imaginary and also full"
    );
}

#[tokio::test]
async fn a_mutating_call_is_classified_as_such() {
    use gyr_core::ToolRuntime;

    let tools = ScriptedTools::default();

    let read = tools
        .classify(&tool_call("call-1", "read", "src/lib.rs"))
        .unwrap();
    let patch = tools
        .classify(&tool_call("call-2", "apply_patch", "src/lib.rs"))
        .unwrap();

    assert_eq!(read.class, ToolClass::ReadOnly);
    assert_eq!(read.subject, None);
    assert_eq!(patch.class, ToolClass::Mutating);
    assert_eq!(patch.subject.as_deref(), Some("src/lib.rs"));
    assert_eq!(json!(patch.class), json!("mutating"));
}

#[tokio::test]
async fn a_process_call_is_refused_in_read_only_mode() {
    let mut agent = agent(vec![
        tool_turn(tool_call("call-1", "cargo", "unused")),
        text_turn("very well"),
    ])
    .with_policy(ReadOnly);

    let result = agent.run("run the tests").await.unwrap();

    let results = tool_results(&result);
    assert!(results[0].output.is_error);
    assert!(
        results[0].output.content.contains("read-only mode"),
        "said: {}",
        results[0].output.content
    );
    assert!(agent.tools().executed_names().is_empty());
}

#[tokio::test]
async fn a_process_call_still_asks_a_person_in_interactive_mode() {
    let (approver, asked) = ScriptedApprover::new(vec![ApprovalReply::Once]);
    let mut agent = agent(vec![
        tool_turn(tool_call("call-1", "cargo", "unused")),
        text_turn("done"),
    ])
    .with_policy(Interactive::new(approver));

    let result = agent.run("run the tests").await.unwrap();

    assert_eq!(
        *asked.lock().unwrap(),
        vec!["cargo\u{1f}cargo check --workspace".to_owned()],
        "a process call is never auto-allowed"
    );
    assert_eq!(
        decisions(&result),
        vec![(
            "cargo\u{1f}cargo check --workspace".to_owned(),
            ApprovalDecision::allowed(DecisionSource::User)
        )]
    );
}

#[test]
fn a_tool_set_refuses_two_runtimes_claiming_one_name() {
    let Err(error) = ToolSet::new(vec![
        Box::new(EchoTool { name: "build" }),
        Box::new(EchoTool { name: "build" }),
    ]) else {
        panic!("a duplicated tool name must not build a tool set");
    };

    assert_eq!(error.to_string(), "two tool runtimes both offer \"build\"");
}

#[tokio::test]
async fn a_tool_set_dispatches_by_tool_name() {
    let tools = ToolSet::new(vec![
        Box::new(EchoTool { name: "first" }),
        Box::new(EchoTool { name: "second" }),
    ])
    .unwrap();

    let names: Vec<String> = tools
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    let chosen = tools
        .execute(&tool_call("call-1", "second", "unused"))
        .await
        .unwrap();
    let unknown = tools
        .execute(&tool_call("call-2", "third", "unused"))
        .await
        .unwrap_err();

    assert_eq!(names, vec!["first".to_owned(), "second".to_owned()]);
    assert_eq!(chosen.content, "second");
    assert_eq!(unknown.to_string(), "unknown tool \"third\"");
}

/// A window small enough that the fractions land on readable numbers.
const WINDOW: u32 = 1_000;

fn budgeted(turns: Vec<Turn>) -> Agent<ScriptedSession, ScriptedTools> {
    Agent::new(
        ScriptedSession::new(turns).with_window(Some(WINDOW)),
        ScriptedTools::default(),
        AgentConfig::default(),
    )
}

fn warnings(result: &gyr_core::RunResult) -> Vec<(u64, u32)> {
    result
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ContextWarning {
                input_tokens,
                window_tokens,
                ..
            } => Some((*input_tokens, *window_tokens)),
            _ => None,
        })
        .collect()
}

fn elisions(result: &gyr_core::RunResult) -> Vec<(usize, usize)> {
    result
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Elided {
                results_elided,
                bytes_reclaimed,
                ..
            } => Some((*results_elided, *bytes_reclaimed)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_filling_window_is_reported_once_and_not_on_every_turn() {
    // 750 of 1000 crosses the 70% mark. The second turn is higher still and
    // must not produce a second warning: a warning repeated every turn is
    // noise, and noise is how a real one gets missed.
    let mut agent = budgeted(vec![
        Turn::Events(vec![
            ModelEvent::Usage {
                usage: gyr_protocol::TokenUsage {
                    input_tokens: 750,
                    ..gyr_protocol::TokenUsage::default()
                },
            },
            ModelEvent::ToolCallCompleted {
                call: tool_call("call-1", "read", "src/lib.rs"),
            },
            ModelEvent::Finished {
                reason: StopReason::ToolUse,
            },
        ]),
        usage_turn(800, "done"),
    ]);

    let result = agent.run("keep going").await.unwrap();

    assert_eq!(warnings(&result), vec![(750, WINDOW)]);
}

#[tokio::test]
async fn the_window_the_server_reports_is_the_one_measured_against() {
    // The profile claims ten times the room; the server says otherwise.
    let mut session =
        ScriptedSession::new(vec![usage_turn(750, "done")]).with_window(Some(WINDOW * 10));
    session.served_window = Some(WINDOW);
    let mut agent = Agent::new(session, ScriptedTools::default(), AgentConfig::default());

    let result = agent.run("keep going").await.unwrap();

    assert_eq!(warnings(&result), vec![(750, WINDOW)]);
}

#[tokio::test]
async fn a_window_below_the_mark_says_nothing() {
    let mut agent = budgeted(vec![usage_turn(500, "plenty of room")]);

    let result = agent.run("hello").await.unwrap();

    assert!(warnings(&result).is_empty());
    assert!(elisions(&result).is_empty());
}

#[tokio::test]
async fn crossing_the_higher_mark_elides_and_records_it() {
    let mut agent = Agent::new(
        ScriptedSession::new(vec![usage_turn(900, "still here")])
            .with_window(Some(WINDOW))
            .eliding(3, 4_096),
        ScriptedTools::default(),
        AgentConfig::default(),
    );

    let result = agent.run("a long conversation").await.unwrap();

    assert_eq!(elisions(&result), vec![(3, 4_096)]);
    assert_eq!(warnings(&result).len(), 1, "it crossed both marks at once");
    assert_eq!(
        agent.session().elide_requests,
        vec![AgentConfig::default().keep_recent_results]
    );
}

#[tokio::test]
async fn a_provider_that_cannot_elide_does_not_fail_the_run() {
    // The scripted session refuses, as OpenAI's server-side continuation would.
    let mut agent = budgeted(vec![usage_turn(900, "still here")]);

    let result = agent.run("a long conversation").await.unwrap();

    assert!(elisions(&result).is_empty());
    assert_eq!(warnings(&result).len(), 1);
    assert_eq!(result.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn a_model_with_no_documented_window_is_left_alone() {
    let mut agent = Agent::new(
        ScriptedSession::new(vec![usage_turn(9_999_999, "enormous")])
            .with_window(None)
            .eliding(3, 4_096),
        ScriptedTools::default(),
        AgentConfig::default(),
    );

    let result = agent.run("hello").await.unwrap();

    // Guessing a window and cutting history against the guess would be worse
    // than an honest refusal from the provider later.
    assert!(warnings(&result).is_empty());
    assert!(elisions(&result).is_empty());
    assert!(agent.session().elide_requests.is_empty());
}

/// A turn that reports its usage and stops for the given reason.
fn measured_turn(input: u64, output: u64, mut events: Vec<ModelEvent>, reason: StopReason) -> Turn {
    events.push(ModelEvent::Usage {
        usage: gyr_protocol::TokenUsage {
            input_tokens: input,
            output_tokens: output,
            ..gyr_protocol::TokenUsage::default()
        },
    });
    events.push(ModelEvent::Finished { reason });
    Turn::Events(events)
}

fn cut_offs(result: &gyr_core::RunResult) -> Vec<u32> {
    result
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TurnCutOff { model_turn, .. } => Some(*model_turn),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_turn_cut_off_by_a_full_window_elides_and_continues() {
    // A self-hosted server shrinks the output allowance to what the window has
    // left, so a long prompt ends the turn with a length stop.
    let mut agent = Agent::new(
        ScriptedSession::new(vec![
            measured_turn(
                900,
                100,
                vec![ModelEvent::TextDelta {
                    text: "half a tho".into(),
                }],
                StopReason::MaxTokens,
            ),
            usage_turn(300, "done"),
        ])
        .with_window(Some(WINDOW))
        .eliding(3, 4_096),
        ScriptedTools::default(),
        AgentConfig::default(),
    );

    let result = agent.run("a long task").await.unwrap();

    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.model_turns, 2);
    assert_eq!(cut_offs(&result), vec![1]);
    assert!(!elisions(&result).is_empty());
    let TurnInput::User { content } = &agent.session().inputs[1] else {
        panic!(
            "expected a note to the model, got {:?}",
            agent.session().inputs[1]
        );
    };
    assert!(content.contains("cut off"), "{content}");
}

#[tokio::test]
async fn calls_completed_before_the_window_filled_are_answered() {
    let mut agent = Agent::new(
        ScriptedSession::new(vec![
            measured_turn(
                900,
                100,
                vec![ModelEvent::ToolCallCompleted {
                    call: tool_call("call-1", "read", "src/lib.rs"),
                }],
                StopReason::MaxTokens,
            ),
            usage_turn(300, "done"),
        ])
        .with_window(Some(WINDOW))
        .eliding(3, 4_096),
        ScriptedTools::default(),
        AgentConfig::default(),
    );

    let result = agent.run("a long task").await.unwrap();

    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(agent.tools().executed_names(), vec!["read".to_owned()]);
    let TurnInput::ToolResults { results } = &agent.session().inputs[1] else {
        panic!("expected tool results, got {:?}", agent.session().inputs[1]);
    };
    assert!(results[0].output.content.contains("cut off"));
}

#[tokio::test]
async fn a_turn_cut_off_with_nothing_to_elide_ends_the_run() {
    let mut agent = budgeted(vec![measured_turn(
        900,
        100,
        Vec::new(),
        StopReason::MaxTokens,
    )]);

    let result = agent.run("a long task").await.unwrap();

    assert_eq!(result.stop_reason, StopReason::MaxTokens);
    assert_eq!(result.model_turns, 1);
}

#[tokio::test]
async fn a_length_stop_with_room_left_is_the_model_s_own_limit() {
    let mut agent = Agent::new(
        ScriptedSession::new(vec![measured_turn(
            100,
            100,
            Vec::new(),
            StopReason::MaxTokens,
        )])
        .with_window(Some(WINDOW))
        .eliding(3, 4_096),
        ScriptedTools::default(),
        AgentConfig::default(),
    );

    let result = agent.run("a short task").await.unwrap();

    assert_eq!(result.stop_reason, StopReason::MaxTokens);
    assert!(cut_offs(&result).is_empty());
    assert!(elisions(&result).is_empty());
}

#[tokio::test]
async fn history_is_reduced_while_the_next_turn_still_fits() {
    // 600 of 1000 is under the 85% mark, but a model that has written 250 in
    // one turn needs room for that again, twice: once replayed, once new.
    let mut agent = Agent::new(
        ScriptedSession::new(vec![
            measured_turn(
                400,
                250,
                vec![ModelEvent::ToolCallCompleted {
                    call: tool_call("call-1", "read", "a.rs"),
                }],
                StopReason::ToolUse,
            ),
            measured_turn(
                600,
                50,
                vec![ModelEvent::ToolCallCompleted {
                    call: tool_call("call-2", "read", "b.rs"),
                }],
                StopReason::ToolUse,
            ),
            usage_turn(300, "done"),
        ])
        .with_window(Some(WINDOW))
        .eliding(3, 4_096),
        ScriptedTools::default(),
        AgentConfig::default(),
    );

    let result = agent.run("a long task").await.unwrap();

    let elided_at: Vec<u32> = result
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Elided { model_turn, .. } => Some(*model_turn),
            _ => None,
        })
        .collect();
    assert_eq!(elided_at, vec![2]);
}

/// Keeps the outcome a run reports, which is what the session log and eval read.
struct OutcomeSink(std::sync::Arc<std::sync::Mutex<Option<gyr_core::session::RunOutcome>>>);

impl gyr_core::session::EventSink for OutcomeSink {
    fn emit(&mut self, _event: &AgentEvent) -> Result<(), gyr_core::session::SinkError> {
        Ok(())
    }

    fn finish(
        &mut self,
        outcome: &gyr_core::session::RunOutcome,
    ) -> Result<(), gyr_core::session::SinkError> {
        *self.0.lock().unwrap() = Some(outcome.clone());
        Ok(())
    }
}

#[tokio::test]
async fn a_run_cut_off_by_the_turn_limit_reports_the_turns_it_spent() {
    let outcome = std::sync::Arc::default();
    let mut agent = Agent::new(
        ScriptedSession::new(vec![
            tool_turn(tool_call("call-1", "read", "a.rs")),
            tool_turn(tool_call("call-2", "read", "b.rs")),
            tool_turn(tool_call("call-3", "read", "c.rs")),
        ]),
        ScriptedTools::default(),
        AgentConfig {
            max_model_turns: std::num::NonZeroU32::new(2).unwrap(),
            ..AgentConfig::default()
        },
    )
    .with_sink(OutcomeSink(std::sync::Arc::clone(&outcome)));

    let error = agent.run("read everything").await.unwrap_err();

    assert_eq!(error.to_string(), "model-turn limit reached after 2 turns");
    let outcome = outcome.lock().unwrap().clone().unwrap();
    assert_eq!(outcome.model_turns, 2);
}

fn broken_stream(before: Vec<ModelEvent>) -> Turn {
    let events = before
        .into_iter()
        .map(Ok)
        .chain(std::iter::once(Err(ModelError::Transport(
            "error decoding response body".into(),
        ))));
    Turn::Stream(Box::pin(stream::iter(events)) as ModelEventStream)
}

#[tokio::test]
async fn a_stream_that_breaks_mid_turn_is_sent_again_and_its_fragment_dropped() {
    let mut agent = agent(vec![
        broken_stream(vec![
            ModelEvent::TextDelta {
                text: "half a tho".into(),
            },
            ModelEvent::ToolCallCompleted {
                call: tool_call("call-1", "read", "src/lib.rs"),
            },
        ]),
        tool_turn(tool_call("call-1", "read", "src/lib.rs")),
        text_turn("done"),
    ]);

    let result = agent.run("read it").await.unwrap();

    // The repeated call id is the same call sent again, not a duplicate.
    assert_eq!(result.text, "done");
    assert_eq!(result.model_turns, 2);
    assert_eq!(agent.tools().executed_names(), vec!["read"]);
    let inputs = &agent.session().inputs;
    assert_eq!(inputs[0], inputs[1], "the broken turn is resent unchanged");
    assert!(result.events.iter().any(|event| matches!(
        event,
        AgentEvent::TurnRetried { model_turn: 1, attempt: 1, error }
            if error.contains("decoding response body")
    )));
}

#[tokio::test]
async fn a_stream_that_keeps_breaking_fails_the_run() {
    let mut agent = Agent::new(
        ScriptedSession::new(vec![broken_stream(vec![]), broken_stream(vec![])]),
        ScriptedTools::default(),
        AgentConfig {
            turn_attempts: 2,
            ..AgentConfig::default()
        },
    );

    let error = agent.run("hello").await.unwrap_err();

    assert!(
        error.to_string().contains("decoding response body"),
        "{error}"
    );
}

fn last_result(input: &TurnInput) -> String {
    match input {
        TurnInput::ToolResults { results } => results.last().unwrap().output.content.clone(),
        TurnInput::User { content } | TurnInput::UserWithImages { content, .. } => content.clone(),
    }
}

#[tokio::test]
async fn the_last_turns_carry_a_countdown() {
    let mut agent = Agent::new(
        ScriptedSession::new(vec![
            tool_turn(tool_call("call-1", "read", "a.rs")),
            tool_turn(tool_call("call-2", "read", "b.rs")),
            tool_turn(tool_call("call-3", "read", "c.rs")),
            text_turn("done"),
        ]),
        ScriptedTools::default(),
        AgentConfig {
            max_model_turns: std::num::NonZeroU32::new(4).unwrap(),
            turn_notice_at: 2,
            ..AgentConfig::default()
        },
    );

    let result = agent.run("read them").await.unwrap();

    let inputs = &agent.session().inputs;
    assert!(!last_result(&inputs[1]).contains("turns left"));
    assert!(
        last_result(&inputs[2]).contains("2 model turns left"),
        "{}",
        last_result(&inputs[2])
    );
    assert!(
        last_result(&inputs[3]).contains("1 model turn left"),
        "{}",
        last_result(&inputs[3])
    );
    let notices: Vec<u32> = result
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TurnsRunningOut { turns_left, .. } => Some(*turns_left),
            _ => None,
        })
        .collect();
    assert_eq!(notices, vec![2, 1]);
}

#[tokio::test]
async fn reporting_a_blocker_ends_the_run_with_the_reason() {
    let mut agent = Agent::new(
        ScriptedSession::new(vec![
            Turn::Events(vec![
                ModelEvent::ToolCallCompleted {
                    call: gyr_protocol::ToolCall {
                        id: "call-1".into(),
                        name: gyr_core::blocker::REPORT_BLOCKER.into(),
                        arguments: json!({
                            "reason": "tests/basic.rs expects 3 but the spec says 4",
                            "evidence": ["tests/basic.rs:12"]
                        }),
                    },
                },
                ModelEvent::Finished {
                    reason: StopReason::ToolUse,
                },
            ]),
            text_turn("never asked for"),
        ]),
        gyr_core::blocker::BlockerTool,
        AgentConfig::default(),
    );

    let result = agent.run("make it pass").await.unwrap();

    assert_eq!(result.stop_reason, StopReason::Blocked);
    assert!(
        result.text.contains("expects 3 but the spec says 4"),
        "{}",
        result.text
    );
    assert_eq!(agent.session().inputs.len(), 1, "no turn after the blocker");
}

/// Says the work is unfinished every time it is asked, and counts the asking.
struct AlwaysBroken(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl gyr_core::FinishCheck for AlwaysBroken {
    fn unfinished(&self) -> gyr_core::CheckFuture<'_> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Some("the build is broken.".to_owned()) })
    }
}

struct AllGood;

impl gyr_core::FinishCheck for AllGood {
    fn unfinished(&self) -> gyr_core::CheckFuture<'_> {
        Box::pin(async { None })
    }
}

fn sent_back(result: &gyr_core::RunResult) -> Vec<u32> {
    result
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::SentBack { model_turn, .. } => Some(*model_turn),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_model_that_stops_with_work_unfinished_is_sent_back_once() {
    let asked = std::sync::Arc::default();
    let mut agent = agent(vec![text_turn("done"), text_turn("still done")])
        .with_finish_check(AlwaysBroken(std::sync::Arc::clone(&asked)));

    let result = agent.run("fix it").await.unwrap();

    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.model_turns, 2);
    assert_eq!(sent_back(&result), vec![1]);
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    let note = last_result(&agent.session().inputs[1]);
    assert!(
        note.contains("not finished: the build is broken."),
        "{note}"
    );
}

#[tokio::test]
async fn the_countdown_says_what_is_unfinished() {
    let asked = std::sync::Arc::default();
    let mut agent = Agent::new(
        ScriptedSession::new(vec![
            tool_turn(tool_call("call-1", "read", "a.rs")),
            tool_turn(tool_call("call-2", "read", "b.rs")),
            tool_turn(tool_call("call-3", "read", "c.rs")),
            text_turn("done"),
        ]),
        ScriptedTools::default(),
        AgentConfig {
            max_model_turns: std::num::NonZeroU32::new(4).unwrap(),
            turn_notice_at: 2,
            ..AgentConfig::default()
        },
    )
    .with_finish_check(AlwaysBroken(std::sync::Arc::clone(&asked)));

    agent.run("read them").await.unwrap();

    let inputs = &agent.session().inputs;
    assert!(!last_result(&inputs[1]).contains("the build is broken"));
    for input in &inputs[2..4] {
        let note = last_result(input);
        assert!(note.contains("right now the build is broken."), "{note}");
    }
    // Twice in the countdown; never on the last turn, which is not sent back.
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn finished_work_ends_the_run() {
    let mut agent = agent(vec![text_turn("done")]).with_finish_check(AllGood);

    let result = agent.run("fix it").await.unwrap();

    assert_eq!(result.model_turns, 1);
    assert!(sent_back(&result).is_empty());
}

#[tokio::test]
async fn the_last_turn_is_not_sent_back() {
    let asked = std::sync::Arc::default();
    let mut agent = Agent::new(
        ScriptedSession::new(vec![text_turn("done")]),
        ScriptedTools::default(),
        AgentConfig {
            max_model_turns: std::num::NonZeroU32::new(1).unwrap(),
            ..AgentConfig::default()
        },
    )
    .with_finish_check(AlwaysBroken(std::sync::Arc::clone(&asked)));

    let result = agent.run("fix it").await.unwrap();

    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert!(sent_back(&result).is_empty());
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn each_submission_may_be_sent_back_once() {
    let mut agent = agent(vec![
        text_turn("done"),
        text_turn("done again"),
        text_turn("second task done"),
        text_turn("really"),
    ])
    .with_finish_check(AlwaysBroken(std::sync::Arc::default()));

    let first = agent.run("one").await.unwrap();
    let second = agent.run("two").await.unwrap();

    assert_eq!(sent_back(&first), vec![1]);
    assert_eq!(sent_back(&second), vec![1]);
}

#[derive(Default, Clone)]
struct MemoryStore {
    rules: Arc<Mutex<Vec<String>>>,
}

impl gyr_core::approval::RuleStore for MemoryStore {
    fn load(&self) -> Vec<String> {
        self.rules.lock().unwrap().clone()
    }

    fn save(&self, key: &str) {
        self.rules.lock().unwrap().push(key.to_owned());
    }
}

#[tokio::test]
async fn a_remembered_rule_is_saved_and_a_saved_rule_is_not_asked_again() {
    let store = MemoryStore::default();
    let (approver, asked) = ScriptedApprover::new(vec![ApprovalReply::Remember]);
    let mut first = agent(vec![
        tool_turn(tool_call("call-1", "apply_patch", "src/lib.rs")),
        text_turn("done"),
    ])
    .with_policy(Interactive::new(approver).with_store(store.clone()));
    first.run("edit").await.unwrap();
    assert_eq!(asked.lock().unwrap().len(), 1);
    assert_eq!(
        *store.rules.lock().unwrap(),
        vec!["apply_patch\u{1f}src/lib.rs".to_owned()]
    );

    // A later session, as after a restart: the store is all that carries over.
    let (approver, asked) = ScriptedApprover::new(Vec::new());
    let mut second = agent(vec![
        tool_turn(tool_call("call-1", "apply_patch", "src/lib.rs")),
        text_turn("done"),
    ])
    .with_policy(Interactive::new(approver).with_store(store));
    let result = second.run("edit again").await.unwrap();

    assert!(asked.lock().unwrap().is_empty());
    assert_eq!(
        decisions(&result)
            .into_iter()
            .map(|(_, decision)| decision)
            .collect::<Vec<_>>(),
        vec![ApprovalDecision::allowed(DecisionSource::SavedRule)]
    );
}

#[tokio::test]
async fn the_plan_gate_refuses_changes_while_planning_and_steps_aside_after() {
    use gyr_core::approval::ApprovalPolicy;
    use gyr_core::approval::PlanGate;
    use gyr_protocol::ToolAction;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    let planning = Arc::new(AtomicBool::new(true));
    let gate = PlanGate::new(AllowAll, Arc::clone(&planning));
    let call = tool_call("call-1", "apply_patch", "src/lib.rs");

    for action in [
        ToolAction::mutating("src/lib.rs"),
        ToolAction::process("cargo test"),
    ] {
        let decision = gate.decide(&call, &action).await;
        assert!(
            matches!(&decision, ApprovalDecision::Denied { reason } if reason.contains("plan")),
            "{decision:?}"
        );
    }
    for action in [ToolAction::read_only(), ToolAction::network("docs.rs")] {
        assert!(matches!(
            gate.decide(&call, &action).await,
            ApprovalDecision::Allowed { .. }
        ));
    }

    planning.store(false, Ordering::Relaxed);
    assert!(matches!(
        gate.decide(&call, &ToolAction::mutating("src/lib.rs"))
            .await,
        ApprovalDecision::Allowed { .. }
    ));
}

#[tokio::test]
async fn several_checks_report_the_first_that_finds_work_unfinished() {
    use gyr_core::CheckFuture;
    use gyr_core::FinishCheck;

    struct Says(Option<&'static str>);
    impl FinishCheck for Says {
        fn unfinished(&self) -> CheckFuture<'_> {
            let said = self.0.map(str::to_owned);
            Box::pin(async move { said })
        }
    }

    let checks = gyr_core::FirstUnfinished::new(vec![
        Box::new(Says(None)),
        Box::new(Says(Some("lint failed"))),
        Box::new(Says(Some("build failed"))),
    ]);

    assert_eq!(checks.unfinished().await.as_deref(), Some("lint failed"));
}

#[test]
fn a_call_with_unparsed_arguments_is_refused_with_what_was_sent() {
    let tools = ToolSet::new(vec![Box::new(gyr_core::todo::TodoTool)]).unwrap();
    let call = gyr_protocol::ToolCall {
        id: "call-1".into(),
        name: "todo".into(),
        arguments: serde_json::json!({
            gyr_protocol::UNPARSED_ARGUMENTS: {"raw": "{items: x}", "error": "key must be a string"}
        }),
    };

    let error = tools.classify(&call).unwrap_err().to_string();

    assert!(error.contains("not valid JSON"), "{error}");
    assert!(error.contains("{items: x}"), "{error}");
    assert!(error.contains("key must be a string"), "{error}");
}
