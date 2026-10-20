use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::bus::SubscriptionId;
use crate::event::EventKind;
use crate::registry::Registry;

/// Counts the events a registry publishes, by kind, from the moment it is
/// attached until it is detached.
pub struct Tally {
    registry: Registry,
    subscription: SubscriptionId,
    counts: Rc<RefCell<BTreeMap<EventKind, u64>>>,
}

impl Tally {
    #[must_use]
    pub fn attach(registry: &Registry) -> Self {
        let counts: Rc<RefCell<BTreeMap<EventKind, u64>>> = Rc::default();
        let sink = Rc::clone(&counts);
        let subscription = registry.subscribe(move |event| {
            *sink.borrow_mut().entry(event.kind()).or_insert(0) += 1;
        });
        Self {
            registry: registry.clone(),
            subscription,
            counts,
        }
    }

    #[must_use]
    pub fn count(&self, kind: EventKind) -> u64 {
        self.counts.borrow().get(&kind).copied().unwrap_or(0)
    }

    #[must_use]
    pub fn total(&self) -> u64 {
        self.counts.borrow().values().sum()
    }

    /// Stops counting and returns what was counted.
    pub fn detach(self) -> BTreeMap<EventKind, u64> {
        self.registry.unsubscribe(self.subscription);
        self.counts.borrow().clone()
    }
}
