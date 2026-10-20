use std::cell::{Cell, RefCell};

use crate::event::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

type Callback = Box<dyn Fn(&Event)>;

struct Subscriber {
    id: SubscriptionId,
    callback: Callback,
    active: Cell<bool>,
}

#[derive(Default)]
pub(crate) struct Bus {
    next: Cell<u64>,
    subscribers: RefCell<Vec<Subscriber>>,
}

impl Bus {
    pub(crate) fn subscribe(&self, callback: Callback) -> SubscriptionId {
        let id = SubscriptionId(self.next.get());
        self.next.set(id.0 + 1);
        let mut subscribers = self.subscribers.borrow_mut();
        subscribers.retain(|subscriber| subscriber.active.get());
        subscribers.push(Subscriber {
            id,
            callback,
            active: Cell::new(true),
        });
        id
    }

    /// Only marks the subscriber inactive, so a callback can unsubscribe
    /// itself while `emit` is walking the list.
    pub(crate) fn unsubscribe(&self, id: SubscriptionId) -> bool {
        for subscriber in self.subscribers.borrow().iter() {
            if subscriber.id == id && subscriber.active.get() {
                subscriber.active.set(false);
                return true;
            }
        }
        false
    }

    /// Calls every active subscriber and returns how many were called.
    pub(crate) fn emit(&self, event: &Event) -> u64 {
        let mut delivered = 0;
        for subscriber in self.subscribers.borrow().iter() {
            if subscriber.active.get() {
                (subscriber.callback)(event);
                delivered += 1;
            }
        }
        delivered
    }
}
