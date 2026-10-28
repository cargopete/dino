//! Append-only session records.
//!
//! Presentation and the eval corpus are derived from this log. Native provider
//! continuation state deliberately stays inside its adapter and is not written
//! here, so a log is not yet a replay source.

use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dino_protocol::AgentEvent;
use dino_protocol::ProviderKind;
use dino_protocol::StopReason;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("session sink failed: {message}")]
pub struct SinkError {
    message: String,
}

impl SinkError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// A destination for agent events.
///
/// `emit` is fallible on purpose. A log that quietly stops recording reports a
/// healthy run it did not observe, which is the failure this project keeps
/// promising not to ship.
pub trait EventSink: Send {
    /// Records one agent event.
    ///
    /// # Errors
    ///
    /// Returns [`SinkError`] when the event could not be recorded. The agent
    /// treats this as terminal.
    fn emit(&mut self, event: &AgentEvent) -> Result<(), SinkError>;

    /// Records how the run ended.
    ///
    /// # Errors
    ///
    /// Returns [`SinkError`] when the outcome could not be recorded.
    fn finish(&mut self, outcome: &RunOutcome) -> Result<(), SinkError> {
        let _ = outcome;
        Ok(())
    }
}

/// Discards every event. Used where a run genuinely wants no record.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _event: &AgentEvent) -> Result<(), SinkError> {
        Ok(())
    }
}

impl<A, B> EventSink for (A, B)
where
    A: EventSink,
    B: EventSink,
{
    fn emit(&mut self, event: &AgentEvent) -> Result<(), SinkError> {
        self.0.emit(event)?;
        self.1.emit(event)
    }

    fn finish(&mut self, outcome: &RunOutcome) -> Result<(), SinkError> {
        self.0.finish(outcome)?;
        self.1.finish(outcome)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionId(String);

impl SessionId {
    /// Builds an identifier from the wall clock and the process ID.
    ///
    /// Unique enough to name a log file on one machine, and deliberately not a
    /// cryptographic identifier.
    #[must_use]
    pub fn generate() -> Self {
        let millis = unix_millis();
        let pid = u128::from(std::process::id());
        Self(format!("{millis:x}-{pid:x}"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: SessionId,
    #[serde(alias = "gyrfalcon_version")]
    pub dino_version: String,
    pub model_key: String,
    pub provider: ProviderKind,
    /// The canonical workspace root. Credentials are never recorded.
    pub workspace_root: String,
    pub approval_mode: String,
    /// The containment in force. A person running unconfined should be able to
    /// tell from the log that they did.
    pub sandbox: String,
    pub max_model_turns: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOutcome {
    /// Absent when the run ended with an error rather than a stop reason.
    pub stop_reason: Option<StopReason>,
    pub model_turns: u32,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum SessionRecord {
    Started {
        seq: u64,
        unix_millis: u128,
        session: Box<SessionMeta>,
    },
    Event {
        seq: u64,
        unix_millis: u128,
        event: AgentEvent,
    },
    Finished {
        seq: u64,
        unix_millis: u128,
        outcome: RunOutcome,
    },
}

/// An append-only JSONL log, one record per line, flushed per record.
///
/// Records are flushed but not synchronised, so sudden power loss may lose the
/// tail of a log. Atomic durability is not claimed.
#[derive(Debug)]
pub struct JsonlSessionLog {
    writer: BufWriter<File>,
    path: PathBuf,
    seq: u64,
}

impl JsonlSessionLog {
    /// Creates or appends to a JSONL log and writes its opening record.
    ///
    /// # Errors
    ///
    /// Returns [`SinkError`] when the parent directory cannot be created, the
    /// file cannot be opened, or the opening record cannot be written.
    pub fn create(path: impl AsRef<Path>, session: SessionMeta) -> Result<Self, SinkError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                SinkError::new(format!(
                    "cannot create session log directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| {
                SinkError::new(format!(
                    "cannot open session log {}: {error}",
                    path.display()
                ))
            })?;
        let mut log = Self {
            writer: BufWriter::new(file),
            path,
            seq: 0,
        };
        let record = SessionRecord::Started {
            seq: log.next_seq(),
            unix_millis: unix_millis(),
            session: Box::new(session),
        };
        log.write(&record)?;
        Ok(log)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self.seq;
        self.seq += 1;
        seq
    }

    fn write(&mut self, record: &SessionRecord) -> Result<(), SinkError> {
        let line = serde_json::to_string(record)
            .map_err(|error| SinkError::new(format!("cannot encode session record: {error}")))?;
        writeln!(self.writer, "{line}").map_err(|error| {
            SinkError::new(format!(
                "cannot write session log {}: {error}",
                self.path.display()
            ))
        })?;
        self.writer.flush().map_err(|error| {
            SinkError::new(format!(
                "cannot flush session log {}: {error}",
                self.path.display()
            ))
        })
    }
}

impl EventSink for JsonlSessionLog {
    fn emit(&mut self, event: &AgentEvent) -> Result<(), SinkError> {
        let record = SessionRecord::Event {
            seq: self.next_seq(),
            unix_millis: unix_millis(),
            event: event.clone(),
        };
        self.write(&record)
    }

    fn finish(&mut self, outcome: &RunOutcome) -> Result<(), SinkError> {
        let record = SessionRecord::Finished {
            seq: self.next_seq(),
            unix_millis: unix_millis(),
            outcome: outcome.clone(),
        };
        self.write(&record)
    }
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}
