//! Player events, as sent over the WebSocket (`op: "event"`) and delivered to
//! plugin [`EventListener`](crate::EventListener)s.

use serde::{Deserialize, Serialize};

use crate::{Failure, Track};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventType {
    TrackLoadingEvent,
    TrackStartEvent,
    TrackAudibleEvent,
    TrackEndEvent,
    TrackExceptionEvent,
    TrackStuckEvent,
    MixArmedEvent,
    MixStartedEvent,
    MixPromotedEvent,
    MixDroppedEvent,
    HostStartedEvent,
    HostEndedEvent,
    SendStallEvent,
    #[serde(rename = "PositionEvent")]
    PositionJump,
    WebSocketClosedEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EndReason {
    Finished,
    LoadFailed,
    Stopped,
    Replaced,
    Cleanup,
}

impl EndReason {
    /// Whether a client may start the next queued track (Lavalink semantics).
    pub fn may_start_next(self) -> bool {
        matches!(self, EndReason::Finished | EndReason::LoadFailed)
    }
}

/// One event. Optional fields are omitted from JSON when unset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub op: String,
    #[serde(rename = "type")]
    pub kind: EventType,
    pub guild_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<Track>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub encoded: String,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub instance: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<EndReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exception: Option<Failure>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub position_ms: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub sent_at: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub audible_at: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub duration_ms: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stall_ms: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub threshold_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub into: Option<Track>,
    #[serde(default, rename = "intoPositionMs", skip_serializing_if = "is_zero")]
    pub into_ms: i64,
    /// Voice gateway close code (`WebSocketClosedEvent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
    #[serde(default, rename = "closeReason", skip_serializing_if = "String::is_empty")]
    pub close_reason: String,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

impl Event {
    /// A blank event of `kind` for `guild_id`; fill the relevant fields after.
    pub fn new(kind: EventType, guild_id: impl Into<String>) -> Self {
        Self {
            op: "event".into(),
            kind,
            guild_id: guild_id.into(),
            track: None,
            encoded: String::new(),
            instance: 0,
            reason: None,
            exception: None,
            position_ms: 0,
            sent_at: 0,
            audible_at: 0,
            duration_ms: 0,
            stall_ms: 0,
            threshold_ms: 0,
            into: None,
            into_ms: 0,
            code: None,
            terminal: None,
            close_reason: String::new(),
        }
    }

    /// Attach a track (and its encoded form).
    pub fn with_track(mut self, t: &Track) -> Self {
        self.encoded = t.encode();
        self.track = Some(t.clone());
        self
    }

    pub fn with_instance(mut self, instance: u64) -> Self {
        self.instance = instance;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_compactly() {
        let e = Event::new(EventType::TrackEndEvent, "1").with_instance(3);
        let mut e = e;
        e.reason = Some(EndReason::LoadFailed);
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "TrackEndEvent");
        assert_eq!(v["reason"], "loadFailed");
        assert_eq!(v["op"], "event");
        assert!(v.get("positionMs").is_none());
        let p = serde_json::to_value(Event::new(EventType::PositionJump, "1")).unwrap();
        assert_eq!(p["type"], "PositionEvent");
    }
}
