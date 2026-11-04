//! The contract between the player (producer) and the voice sender (consumer).

use std::time::{Duration, Instant};

use crate::Frame;

/// What the producer says about a frame it handed out.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FrameInfo {
    /// Track instance the frame belongs to.
    pub instance: u64,
    /// Source position of the frame's first sample, in ms (fractional, so it
    /// stays exact under tempo changes).
    pub position_ms: f64,
    /// Whether a crossfade was running.
    pub mixing: bool,
}

/// Pulled by the voice sender once per 20 ms tick.
pub trait FrameSource: Send + Sync + 'static {
    /// Fill `out` with the next frame. `None` means "nothing to play": the
    /// sender sends a short silence trailer and stops speaking.
    fn next_frame(&self, out: &mut Frame) -> Option<FrameInfo>;

    /// The frame described by `info` left the socket at `at` (wire clock).
    fn sent(&self, info: FrameInfo, at: Instant);

    /// The send loop fell behind schedule by `gap`.
    fn stall(&self, gap: Duration);
}
