use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::event::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

type Callback = Box<dyn Fn(&Event) + Send + Sync>;

struct Subscriber {
    id: SubscriptionId,
    callback: Callback,
    active: AtomicBool,
}

#[derive(Default)]
pub(crate) struct Bus {
    next: AtomicU64,
    subscribers: Mutex<Vec<Arc<Subscriber>>>,
}

impl Bus {
    pub(crate) fn subscribe(&self, callback: Callback) -> SubscriptionId {
        let id = SubscriptionId(self.next.fetch_add(1, Ordering::Relaxed));
        self.list().push(Arc::new(Subscriber {
            id,
            callback,
            active: AtomicBool::new(true),
        }));
        id
    }

    pub(crate) fn unsubscribe(&self, id: SubscriptionId) -> bool {
        let mut subscribers = self.list();
        match subscribers.iter().position(|subscriber| subscriber.id == id) {
            Some(index) => {
                subscribers.remove(index).active.store(false, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    /// Calls every active subscriber and returns how many were called. The
    /// list is copied out first so callbacks run with no lock held and may
    /// call back into the registry.
    pub(crate) fn emit(&self, event: &Event) -> u64 {
        let snapshot: Vec<Arc<Subscriber>> = self.list().clone();
        let mut delivered = 0;
        for subscriber in snapshot {
            if subscriber.active.load(Ordering::SeqCst) {
                (subscriber.callback)(event);
                delivered += 1;
            }
        }
        delivered
    }

    fn list(&self) -> MutexGuard<'_, Vec<Arc<Subscriber>>> {
        self.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
