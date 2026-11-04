//! In-order, non-blocking event delivery.
//!
//! The player pushes events from the real-time path, so pushing never
//! blocks: events go into an unbounded channel drained by one task that
//! calls the configured [`EventHandler`](crate::EventHandler) in order.

use parking_lot::Mutex;
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use wavyzon_plugin::{Event, EventType, Track};

use crate::traits::EventHandler;

pub(crate) struct Dispatcher {
    tx: Mutex<Option<UnboundedSender<Event>>>,
}

impl Dispatcher {
    pub(crate) fn new(rt: &Handle, handler: EventHandler) -> Self {
        let (tx, mut rx) = unbounded_channel::<Event>();
        rt.spawn(async move {
            while let Some(e) = rx.recv().await {
                handler(e);
            }
        });
        Self {
            tx: Mutex::new(Some(tx)),
        }
    }

    /// Queue `e`. Dropped after [`close`](Self::close).
    pub(crate) fn push(&self, e: Event) {
        if let Some(tx) = &*self.tx.lock() {
            let _ = tx.send(e);
        }
    }

    /// Deliver what is queued, then stop.
    pub(crate) fn close(&self) {
        self.tx.lock().take();
    }
}

/// An event naming `track` without its encoded form (mix, stall and
/// position events).
pub(crate) fn ev(kind: EventType, track: Option<&Track>, instance: u64) -> Event {
    let mut e = Event::new(kind, "").with_instance(instance);
    e.track = track.cloned();
    e
}

/// An event carrying `track` and its encoded form (lifecycle events).
pub(crate) fn tev(kind: EventType, track: &Track, instance: u64) -> Event {
    Event::new(kind, "").with_track(track).with_instance(instance)
}
