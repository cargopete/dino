use std::sync::Arc;

use crate::event::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub(crate) u64);

impl JobId {
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct Job {
    id: JobId,
    name: Arc<str>,
    done: u32,
    total: u32,
    state: JobState,
}

impl Job {
    pub(crate) fn new(id: JobId, name: &str, steps: u32) -> Self {
        Self {
            id,
            name: Arc::from(name),
            done: 0,
            total: steps.max(1),
            state: JobState::Running,
        }
    }

    #[must_use]
    pub fn id(&self) -> JobId {
        self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn done(&self) -> u32 {
        self.done
    }

    #[must_use]
    pub fn total(&self) -> u32 {
        self.total
    }

    #[must_use]
    pub fn state(&self) -> JobState {
        self.state
    }

    /// Moves a running job on by one step. Returns the events that produced,
    /// which is none at all for a job that has already finished.
    pub(crate) fn step(&mut self) -> Vec<Event> {
        if self.state != JobState::Running {
            return Vec::new();
        }
        self.done += 1;
        let mut events = vec![Event::Progressed {
            id: self.id,
            done: self.done,
            total: self.total,
        }];
        if self.done == self.total {
            self.state = JobState::Completed;
            events.push(Event::Completed { id: self.id });
        }
        events
    }

    pub(crate) fn cancel(&mut self) -> bool {
        if self.state != JobState::Running {
            return false;
        }
        self.state = JobState::Cancelled;
        true
    }
}
