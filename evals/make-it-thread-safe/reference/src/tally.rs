use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::bus::SubscriptionId;
use crate::event::EventKind;
use crate::registry::Registry;

/// Counts the events a registry publishes, by kind, from the moment it is
/// attached until it is detached.
pub struct Tally {
    registry: Registry,
    subscription: SubscriptionId,
    counts: Arc<Mutex<BTreeMap<EventKind, u64>>>,
}

impl Tally {
    #[must_use]
    pub fn attach(registry: &Registry) -> Self {
        let counts: Arc<Mutex<BTreeMap<EventKind, u64>>> = Arc::default();
        let sink = Arc::clone(&counts);
        let subscription = registry.subscribe(move |event| {
            *sink.lock().unwrap_or_else(PoisonError::into_inner).entry(event.kind()).or_insert(0) += 1;
        });
        Self {
            registry: registry.clone(),
            subscription,
            counts,
        }
    }

    #[must_use]
    pub fn count(&self, kind: EventKind) -> u64 {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner).get(&kind).copied().unwrap_or(0)
    }

    #[must_use]
    pub fn total(&self) -> u64 {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner).values().sum()
    }

    /// Stops counting and returns what was counted.
    pub fn detach(self) -> BTreeMap<EventKind, u64> {
        self.registry.unsubscribe(self.subscription);
        self.counts.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}
