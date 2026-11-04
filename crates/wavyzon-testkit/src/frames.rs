//! [`MockFrameSource`]: a scripted [`FrameSource`] for voice sender tests.
//!
//! It counts pulls, records every `sent(info, at)` and `stall(gap)` call, and
//! can simulate the end of a track (`next_frame` returning `None`).
//!
//! ```
//! use std::sync::Arc;
//! use wavyzon_audio::{FrameSource, SILENCE};
//! use wavyzon_testkit::MockFrameSource;
//!
//! let src = Arc::new(MockFrameSource::tone(440.0, 0.5).with_limit(3));
//! let mut f = SILENCE;
//! let mut n = 0;
//! while let Some(info) = src.next_frame(&mut f) {
//!     src.sent(info, std::time::Instant::now());
//!     n += 1;
//! }
//! assert_eq!((n, src.pulled(), src.sent_count()), (3, 3, 3));
//! ```

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use wavyzon_audio::{FRAME_MS, FRAME_SAMPLES_PER_CHANNEL, Frame, FrameInfo, FrameSource, SILENCE};

use crate::WaitTimeout;
use crate::audio::sine_sample;

/// What a [`MockFrameSource`] plays.
#[derive(Debug, Clone)]
pub enum FrameContent {
    /// Digital silence.
    Silence,
    /// A continuous sine.
    Tone {
        /// Frequency in Hz.
        freq: f64,
        /// Peak amplitude, `0.0..=1.0`.
        amplitude: f64,
    },
    /// These frames in order; the track ends when they run out (more can be
    /// added with [`MockFrameSource::push_frames`]).
    Frames(VecDeque<Frame>),
}

/// One recorded `sent` call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SentFrame {
    /// What `next_frame` said about the frame.
    pub info: FrameInfo,
    /// The wire instant the sender reported.
    pub at: Instant,
}

struct State {
    content: FrameContent,
    limit: Option<u64>,
    instance: u64,
    mixing: bool,
    /// Frames handed out for the current instance.
    position: u64,
    /// Frames handed out in total.
    pulled: u64,
    /// `next_frame` calls, including those that returned `None`.
    calls: u64,
    ended: bool,
    paused: bool,
    pulls: Vec<Instant>,
    sent: Vec<SentFrame>,
    stalls: Vec<Duration>,
}

/// A scripted [`FrameSource`]. Share it as `Arc<MockFrameSource>`.
pub struct MockFrameSource {
    state: Mutex<State>,
}

impl MockFrameSource {
    /// Plays `content` for ever (until [`end_track`](Self::end_track) or the limit).
    pub fn new(content: FrameContent) -> Self {
        Self {
            state: Mutex::new(State {
                content,
                limit: None,
                instance: 1,
                mixing: false,
                position: 0,
                pulled: 0,
                calls: 0,
                ended: false,
                paused: false,
                pulls: Vec::new(),
                sent: Vec::new(),
                stalls: Vec::new(),
            }),
        }
    }

    /// Endless silence.
    pub fn silence() -> Self {
        Self::new(FrameContent::Silence)
    }

    /// An endless sine at `freq` Hz.
    pub fn tone(freq: f64, amplitude: f64) -> Self {
        Self::new(FrameContent::Tone { freq, amplitude })
    }

    /// Exactly these frames, then end of track.
    pub fn from_frames(frames: Vec<Frame>) -> Self {
        Self::new(FrameContent::Frames(frames.into()))
    }

    /// End the track after `frames` frames of the current instance.
    pub fn with_limit(self, frames: u64) -> Self {
        self.state.lock().limit = Some(frames);
        self
    }

    /// Report this track instance in [`FrameInfo::instance`] (default 1).
    pub fn with_instance(self, instance: u64) -> Self {
        self.state.lock().instance = instance;
        self
    }

    /// Report [`FrameInfo::mixing`].
    pub fn with_mixing(self, mixing: bool) -> Self {
        self.state.lock().mixing = mixing;
        self
    }

    // ----- runtime control ----------------------------------------------------

    /// Simulate the end of the track: `next_frame` returns `None` from now on.
    pub fn end_track(&self) {
        self.state.lock().ended = true;
    }

    /// Start a new track instance: position back to 0, not ended.
    pub fn start_instance(&self, instance: u64) {
        let mut st = self.state.lock();
        st.instance = instance;
        st.position = 0;
        st.ended = false;
    }

    /// While paused `next_frame` returns `None` (without ending the track).
    pub fn set_paused(&self, paused: bool) {
        self.state.lock().paused = paused;
    }

    /// Replace the content.
    pub fn set_content(&self, content: FrameContent) {
        self.state.lock().content = content;
    }

    /// Append frames (switches to [`FrameContent::Frames`] if needed) and
    /// clear the ended flag.
    pub fn push_frames(&self, frames: impl IntoIterator<Item = Frame>) {
        let mut st = self.state.lock();
        if !matches!(st.content, FrameContent::Frames(_)) {
            st.content = FrameContent::Frames(VecDeque::new());
        }
        if let FrameContent::Frames(q) = &mut st.content {
            q.extend(frames);
        }
        st.ended = false;
    }

    // ----- inspection -----------------------------------------------------------

    /// Frames handed out (calls that returned `Some`).
    pub fn pulled(&self) -> u64 {
        self.state.lock().pulled
    }

    /// All `next_frame` calls, including `None` results.
    pub fn next_frame_calls(&self) -> u64 {
        self.state.lock().calls
    }

    /// Instants at which frames were handed out.
    pub fn pull_instants(&self) -> Vec<Instant> {
        self.state.lock().pulls.clone()
    }

    /// Recorded `sent` calls.
    pub fn sent_frames(&self) -> Vec<SentFrame> {
        self.state.lock().sent.clone()
    }

    /// Number of `sent` calls.
    pub fn sent_count(&self) -> usize {
        self.state.lock().sent.len()
    }

    /// Gaps between consecutive `sent` instants (≈ 20 ms for a healthy sender).
    pub fn send_intervals(&self) -> Vec<Duration> {
        let st = self.state.lock();
        st.sent
            .windows(2)
            .map(|w| w[1].at.saturating_duration_since(w[0].at))
            .collect()
    }

    /// Recorded `stall` calls.
    pub fn stalls(&self) -> Vec<Duration> {
        self.state.lock().stalls.clone()
    }

    /// Whether the track has ended (explicitly, by limit or out of frames).
    pub fn ended(&self) -> bool {
        self.state.lock().ended
    }

    /// Wait until `n` frames were reported sent.
    pub async fn wait_for_sent(&self, n: usize, timeout: Duration) -> Result<Vec<SentFrame>, WaitTimeout> {
        crate::poll_until(timeout, &format!("{n} sent frames"), || self.sent_count() >= n).await?;
        Ok(self.sent_frames())
    }

    /// Wait until `n` frames were pulled.
    pub async fn wait_for_pulled(&self, n: u64, timeout: Duration) -> Result<(), WaitTimeout> {
        crate::poll_until(timeout, &format!("{n} pulled frames"), || self.pulled() >= n).await
    }
}

impl FrameSource for MockFrameSource {
    fn next_frame(&self, out: &mut Frame) -> Option<FrameInfo> {
        let mut st = self.state.lock();
        st.calls += 1;
        if st.ended || st.paused {
            return None;
        }
        if st.limit.is_some_and(|l| st.position >= l) {
            st.ended = true;
            return None;
        }
        let position = st.position;
        match &mut st.content {
            FrameContent::Silence => *out = SILENCE,
            FrameContent::Tone { freq, amplitude } => {
                let base = position * FRAME_SAMPLES_PER_CHANNEL as u64;
                for (i, pair) in out.chunks_exact_mut(2).enumerate() {
                    let s = sine_sample(*freq, *amplitude, 48_000, base + i as u64);
                    pair[0] = s;
                    pair[1] = s;
                }
            }
            FrameContent::Frames(q) => match q.pop_front() {
                Some(f) => *out = f,
                None => {
                    st.ended = true;
                    return None;
                }
            },
        }
        st.position += 1;
        st.pulled += 1;
        st.pulls.push(Instant::now());
        Some(FrameInfo {
            instance: st.instance,
            position_ms: (position as i64 * FRAME_MS) as f64,
            mixing: st.mixing,
        })
    }

    fn sent(&self, info: FrameInfo, at: Instant) {
        self.state.lock().sent.push(SentFrame { info, at });
    }

    fn stall(&self, gap: Duration) {
        self.state.lock().stalls.push(gap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{assert_frequency, channel, frames_to_samples};

    #[test]
    fn tone_frames_are_continuous() {
        let src = MockFrameSource::tone(1000.0, 0.5).with_limit(10).with_instance(7);
        let mut frames = Vec::new();
        let mut f = SILENCE;
        while let Some(info) = src.next_frame(&mut f) {
            assert_eq!(info.instance, 7);
            assert_eq!(info.position_ms, frames.len() as f64 * 20.0);
            frames.push(f);
        }
        assert_eq!(frames.len(), 10);
        assert!(src.ended());
        assert_frequency(&channel(&frames_to_samples(&frames), 2, 0), 48_000, 1000.0, 2.0);
        assert_eq!(src.next_frame_calls(), 11);
    }

    #[test]
    fn end_pause_and_new_instance() {
        let src = MockFrameSource::silence();
        let mut f = SILENCE;
        assert!(src.next_frame(&mut f).is_some());
        src.set_paused(true);
        assert!(src.next_frame(&mut f).is_none());
        assert!(!src.ended());
        src.set_paused(false);
        src.end_track();
        assert!(src.next_frame(&mut f).is_none());
        src.start_instance(2);
        let info = src.next_frame(&mut f).unwrap();
        assert_eq!((info.instance, info.position_ms), (2, 0.0));
        assert_eq!(src.pulled(), 2);
    }

    #[test]
    fn scripted_frames_sent_and_stalls() {
        let mut a = SILENCE;
        a[0] = 123;
        let src = MockFrameSource::from_frames(vec![a]);
        let mut f = SILENCE;
        let info = src.next_frame(&mut f).unwrap();
        assert_eq!(f[0], 123);
        assert!(src.next_frame(&mut f).is_none());
        let t = Instant::now();
        src.sent(info, t);
        src.sent(info, t + Duration::from_millis(20));
        src.stall(Duration::from_millis(80));
        assert_eq!(src.send_intervals(), vec![Duration::from_millis(20)]);
        assert_eq!(src.stalls(), vec![Duration::from_millis(80)]);
        src.push_frames([SILENCE]);
        assert!(src.next_frame(&mut f).is_some());
    }
}
