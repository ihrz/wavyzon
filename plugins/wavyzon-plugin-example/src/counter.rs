//! An [`EventListener`] that counts player events by type.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use wavyzon_plugin::serde_json::{self, Value};
use wavyzon_plugin::{EndReason, Event, EventListener, EventType, async_trait};

/// Counts every event the node delivers. Listeners run on a dedicated task,
/// so a short lock here never delays audio.
#[derive(Default)]
pub struct EventCounter {
    by_type: Mutex<BTreeMap<String, u64>>,
    sessions: Mutex<HashSet<String>>,
    total: AtomicU64,
}

impl EventCounter {
    /// The name an event type has on the wire (`PositionJump` →
    /// `"PositionEvent"`), taken from its serde representation.
    pub fn wire_name(kind: EventType) -> String {
        match serde_json::to_value(kind) {
            Ok(Value::String(s)) => s,
            _ => format!("{kind:?}"),
        }
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Distinct client sessions seen since the last reset.
    pub fn sessions(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    pub fn count(&self, kind: EventType) -> u64 {
        self.by_type
            .lock()
            .unwrap()
            .get(&Self::wire_name(kind))
            .copied()
            .unwrap_or(0)
    }

    /// `{ "TrackStartEvent": 3, … }`
    pub fn snapshot(&self) -> BTreeMap<String, u64> {
        self.by_type.lock().unwrap().clone()
    }

    pub fn reset(&self) {
        self.by_type.lock().unwrap().clear();
        self.sessions.lock().unwrap().clear();
        self.total.store(0, Ordering::Relaxed);
    }
}

#[async_trait]
impl EventListener for EventCounter {
    async fn on_event(&self, session_id: &str, event: &Event) {
        *self
            .by_type
            .lock()
            .unwrap()
            .entry(Self::wire_name(event.kind))
            .or_default() += 1;
        self.sessions.lock().unwrap().insert(session_id.to_string());
        self.total.fetch_add(1, Ordering::Relaxed);

        match event.kind {
            EventType::TrackExceptionEvent => {
                if let Some(f) = &event.exception {
                    tracing::warn!(session_id, guild = %event.guild_id, severity = ?f.severity, "track failed: {f}");
                }
            }
            EventType::TrackEndEvent if event.reason == Some(EndReason::LoadFailed) => {
                tracing::debug!(session_id, guild = %event.guild_id, "track ended: load failed");
            }
            _ => tracing::trace!(session_id, guild = %event.guild_id, kind = ?event.kind, "event"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn counts_by_wire_name() {
        let c = EventCounter::default();
        c.on_event("a", &Event::new(EventType::TrackAudibleEvent, "1")).await;
        c.on_event("a", &Event::new(EventType::TrackAudibleEvent, "1")).await;
        c.on_event("b", &Event::new(EventType::PositionJump, "1")).await;
        assert_eq!(c.total(), 3);
        assert_eq!(c.count(EventType::TrackAudibleEvent), 2);
        assert_eq!(c.snapshot().get("PositionEvent"), Some(&1));
        assert_eq!(c.sessions(), 2);
        c.reset();
        assert_eq!(c.total(), 0);
        assert!(c.snapshot().is_empty());
    }
}
