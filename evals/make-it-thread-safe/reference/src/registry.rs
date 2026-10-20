use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::bus::{Bus, SubscriptionId};
use crate::event::Event;
use crate::job::{Job, JobId, JobState};
use crate::stats::Stats;

/// An in-memory job registry. Clones share the same jobs and subscribers.
#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    jobs: Mutex<BTreeMap<JobId, Job>>,
    next_id: AtomicU64,
    stats: Mutex<Stats>,
    bus: Bus,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn submit(&self, name: &str, steps: u32) -> JobId {
        let id = JobId(self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        self.jobs().insert(id, Job::new(id, name, steps));
        self.update_stats(|stats| stats.submitted += 1);
        self.publish(&Event::Submitted { id });
        id
    }

    #[must_use]
    pub fn get(&self, id: JobId) -> Option<Job> {
        self.jobs().get(&id).cloned()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.jobs().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Ids of the jobs still running, in ascending order.
    #[must_use]
    pub fn running(&self) -> Vec<JobId> {
        self.jobs()
            .values()
            .filter(|job| job.state() == JobState::Running)
            .map(Job::id)
            .collect()
    }

    /// Moves a job on by one step and returns its state afterwards, or `None`
    /// for an unknown id. A finished job is left as it is.
    pub fn advance(&self, id: JobId) -> Option<JobState> {
        self.step(id).map(|(state, _)| state)
    }

    /// Cancels a running job. False if the job is unknown or already finished.
    pub fn cancel(&self, id: JobId) -> bool {
        let cancelled = match self.jobs().get_mut(&id) {
            Some(job) => job.cancel(),
            None => false,
        };
        if cancelled {
            self.update_stats(|stats| stats.cancelled += 1);
            self.publish(&Event::Cancelled { id });
        }
        cancelled
    }

    pub fn subscribe<F>(&self, callback: F) -> SubscriptionId
    where
        F: Fn(&Event) + Send + Sync + 'static,
    {
        self.inner.bus.subscribe(Box::new(callback))
    }

    pub fn unsubscribe(&self, id: SubscriptionId) -> bool {
        self.inner.bus.unsubscribe(id)
    }

    #[must_use]
    pub fn stats(&self) -> Stats {
        *self.inner.stats.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The state after the step, and whether the job actually moved.
    pub(crate) fn step(&self, id: JobId) -> Option<(JobState, bool)> {
        let (state, events) = {
            let mut jobs = self.jobs();
            let job = jobs.get_mut(&id)?;
            let events = job.step();
            (job.state(), events)
        };
        let moved = !events.is_empty();
        for event in &events {
            if let Event::Completed { .. } = event {
                self.update_stats(|stats| stats.completed += 1);
            }
            self.publish(event);
        }
        Some((state, moved))
    }

    fn jobs(&self) -> MutexGuard<'_, BTreeMap<JobId, Job>> {
        self.inner.jobs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, event: &Event) {
        let delivered = self.inner.bus.emit(event);
        self.update_stats(|stats| stats.delivered += delivered);
    }

    fn update_stats(&self, change: impl FnOnce(&mut Stats)) {
        change(&mut self.inner.stats.lock().unwrap_or_else(PoisonError::into_inner));
    }
}
