//! The append-only JSONL session log.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use dancho_core::session::EventSink;
use dancho_core::session::JsonlSessionLog;
use dancho_core::session::RunOutcome;
use dancho_core::session::SessionId;
use dancho_core::session::SessionMeta;
use dancho_protocol::AgentEvent;
use dancho_protocol::ModelEvent;
use dancho_protocol::ProviderKind;
use dancho_protocol::StopReason;
use pretty_assertions::assert_eq;
use serde_json::Value;

static DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn new() -> Self {
        let serial = DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("danchoai-log-test-{}-{serial}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}

fn meta() -> SessionMeta {
    SessionMeta {
        session_id: SessionId::generate(),
        danchoai_version: "0.1.0".into(),
        model_key: "claude-opus".into(),
        provider: ProviderKind::Anthropic,
        workspace_root: "/tmp/workspace".into(),
        approval_mode: "read-only (mutations are refused)".into(),
        sandbox: "workspace (seatbelt: writes confined, network denied)".into(),
        max_model_turns: 32,
    }
}

#[test]
fn records_are_ordered_and_their_sequence_is_unbroken() {
    let directory = TempDirectory::new();
    // A nested path proves the log creates its own parents.
    let path = directory.path.join("sessions").join("run.jsonl");
    let mut log = JsonlSessionLog::create(&path, meta()).unwrap();

    for index in 0..3_u32 {
        log.emit(&AgentEvent::Model {
            model_turn: 1,
            event: ModelEvent::TextDelta {
                text: format!("delta {index}"),
            },
        })
        .unwrap();
    }
    log.finish(&RunOutcome {
        stop_reason: Some(StopReason::EndTurn),
        model_turns: 1,
        error: None,
    })
    .unwrap();
    drop(log);

    let contents = fs::read_to_string(&path).unwrap();
    let records: Vec<Value> = contents
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    assert_eq!(records.len(), 5);
    let kinds: Vec<&str> = records
        .iter()
        .map(|record| record["record"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["started", "event", "event", "event", "finished"]);
    let sequence: Vec<u64> = records
        .iter()
        .map(|record| record["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(sequence, [0, 1, 2, 3, 4]);
    assert_eq!(records[0]["session"]["model_key"], "claude-opus");
    assert_eq!(records[4]["outcome"]["stop_reason"], "end_turn");
}

#[test]
fn a_credential_is_never_written_to_the_log() {
    let directory = TempDirectory::new();
    let path = directory.path.join("run.jsonl");
    let log = JsonlSessionLog::create(&path, meta()).unwrap();
    drop(log);

    let contents = fs::read_to_string(&path).unwrap();

    // The meta type has no field for one, and this test exists so that adding
    // one would have to be a deliberate act with a failing test attached.
    for forbidden in ["api_key", "sk-", "authorization"] {
        assert!(
            !contents.to_ascii_lowercase().contains(forbidden),
            "the opening record mentioned {forbidden}: {contents}"
        );
    }
}

#[test]
fn an_unwritable_path_fails_at_creation_rather_than_later() {
    let directory = TempDirectory::new();
    let occupied = directory.path.join("not-a-directory");
    fs::write(&occupied, "in the way").unwrap();

    let error = JsonlSessionLog::create(occupied.join("run.jsonl"), meta())
        .expect_err("a log under a regular file cannot be created");

    assert!(error.to_string().contains("session log"), "said: {error}");
}
