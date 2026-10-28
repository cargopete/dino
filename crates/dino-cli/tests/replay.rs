//! Replaying a log, through the shipped binary.
//!
//! The log is written by hand rather than produced by a run, so this tests the
//! reader against a fixed input instead of against whatever the writer happened
//! to emit that day.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Workspace {
    path: PathBuf,
}

impl Workspace {
    fn new(log: &str) -> Self {
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("dino-replay-test-{}-{serial}", std::process::id()));
        let sessions = path.join(".dino").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("abc123.jsonl"), log).unwrap();
        Self { path }
    }

    fn replay(&self, extra: &[&str]) -> (String, bool) {
        let output = Command::new(env!("CARGO_BIN_EXE_dino"))
            .arg("replay")
            .arg("--workspace")
            .arg(&self.path)
            .arg("--plain")
            .args(extra)
            .output()
            .unwrap();
        (
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
            output.status.success(),
        )
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn line(record: &str) -> String {
    format!("{record}\n")
}

fn session_log(submissions: &[&str]) -> String {
    let mut log = line(
        r#"{"record":"started","seq":0,"unix_millis":1,"session":{"session_id":"abc123",
        "dino_version":"0.1.0","model_key":"claude-sonnet","provider":"anthropic",
        "workspace_root":"/tmp/w","approval_mode":"interactive","sandbox":"seatbelt",
        "max_model_turns":32}}"#
            .replace('\n', "")
            .replace("        ", "")
            .as_str(),
    );
    let mut seq = 1;
    for text in submissions {
        log.push_str(&line(&format!(
            r#"{{"record":"event","seq":{seq},"unix_millis":2,"event":{{"type":"submitted","text":"{text}"}}}}"#
        )));
        seq += 1;
        log.push_str(&line(&format!(
            r#"{{"record":"event","seq":{seq},"unix_millis":3,"event":{{"type":"tool_started","model_turn":1,"call":{{"id":"c1","name":"read","arguments":{{"path":"src/lib.rs"}}}}}}}}"#
        )));
        seq += 1;
        log.push_str(&line(&format!(
            r#"{{"record":"event","seq":{seq},"unix_millis":4,"event":{{"type":"model","model_turn":1,"event":{{"type":"text_delta","text":"answered {text}"}}}}}}"#
        )));
        seq += 1;
    }
    log
}

#[test]
fn a_log_replays_as_a_transcript() {
    let workspace = Workspace::new(&session_log(&["first question"]));

    let (output, ok) = workspace.replay(&[]);

    assert!(ok, "{output}");
    // The header comes from the opening record.
    assert!(output.contains("claude-sonnet"), "{output}");
    // The question, which the log did not hold at all until RFC-0016.
    assert!(output.contains("› first question"), "{output}");
    // The tool, rendered the way the live session renders it.
    assert!(output.contains("read src/lib.rs"), "{output}");
    assert!(output.contains("answered first question"), "{output}");
}

#[test]
fn last_shows_only_the_recent_submissions_and_says_what_it_hid() {
    let workspace = Workspace::new(&session_log(&["one", "two", "three"]));

    let (output, ok) = workspace.replay(&["--last", "1"]);

    assert!(ok, "{output}");
    assert!(output.contains("› three"), "{output}");
    assert!(!output.contains("› one"), "{output}");
    assert!(
        output.contains("2 earlier submission(s) not shown"),
        "{output}"
    );
}

#[test]
fn a_log_that_predates_submissions_still_replays() {
    let mut log = session_log(&[]);
    log.push_str(&line(
        r#"{"record":"event","seq":1,"unix_millis":2,"event":{"type":"model","model_turn":1,"event":{"type":"text_delta","text":"an older session"}}}"#,
    ));
    let workspace = Workspace::new(&log);

    let (output, ok) = workspace.replay(&["--last", "1"]);

    // Showing nothing would look like an empty session rather than an old one.
    assert!(ok, "{output}");
    assert!(output.contains("an older session"), "{output}");
}

#[test]
fn a_malformed_line_names_the_file_and_the_line() {
    let mut log = session_log(&["fine"]);
    log.push_str("not json at all\n");
    let workspace = Workspace::new(&log);

    let (output, ok) = workspace.replay(&[]);

    assert!(!ok, "{output}");
    assert!(output.contains("abc123.jsonl:5"), "{output}");
}

#[test]
fn a_named_session_that_does_not_exist_says_so() {
    let workspace = Workspace::new(&session_log(&["fine"]));

    let (output, ok) = workspace.replay(&["nonexistent"]);

    assert!(!ok, "{output}");
    assert!(output.contains("nonexistent"), "{output}");
}

#[test]
fn a_log_outside_any_workspace_replays_by_its_path() {
    // Eval runs write their logs beside the scratch workspaces, not inside them.
    let workspace = Workspace::new("");
    let log = workspace.path.join("interval-set.jsonl");
    fs::write(&log, session_log(&["from an eval"])).unwrap();

    let (output, ok) = workspace.replay(&[log.to_str().unwrap()]);

    assert!(ok, "{output}");
    assert!(output.contains("answered from an eval"), "{output}");
}
