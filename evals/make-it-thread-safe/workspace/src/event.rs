use crate::job::JobId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EventKind {
    Submitted,
    Progressed,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Submitted { id: JobId },
    Progressed { id: JobId, done: u32, total: u32 },
    Completed { id: JobId },
    Cancelled { id: JobId },
}

impl Event {
    #[must_use]
    pub fn kind(&self) -> EventKind {
        match self {
            Event::Submitted { .. } => EventKind::Submitted,
            Event::Progressed { .. } => EventKind::Progressed,
            Event::Completed { .. } => EventKind::Completed,
            Event::Cancelled { .. } => EventKind::Cancelled,
        }
    }

    #[must_use]
    pub fn job(&self) -> JobId {
        match self {
            Event::Submitted { id }
            | Event::Progressed { id, .. }
            | Event::Completed { id }
            | Event::Cancelled { id } => *id,
        }
    }
}
