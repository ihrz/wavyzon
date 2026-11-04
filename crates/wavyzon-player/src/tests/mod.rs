//! Ports of Cadence's player tests, plus plugin filter tests.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::time::{Instant, sleep};
use wavyzon_audio::{Frame, FrameInfo, SAMPLE_RATE};
use wavyzon_plugin::{Event, EventType, Failure, Format, Stream, Track};

use crate::{IdentityResolver, Opener, Player, PlayerConfig, Resolver};

mod auto_crossfade;
mod filter_ramp;
mod gapless;
mod host;
mod loudness;
mod pause_fade;
mod player;
mod plugin_filters;
mod reverb;
mod seek_mix;
mod stream_resume;

/// Interleaved s16le stereo sine at 48 kHz, amplitude 8000.
pub(crate) fn tone(seconds: f64, hz: f64) -> Vec<u8> {
    let n = (seconds * SAMPLE_RATE as f64) as usize;
    let mut buf = Vec::with_capacity(n * 4);
    for i in 0..n {
        let v = (8000.0 * (2.0 * std::f64::consts::PI * hz * i as f64 / SAMPLE_RATE as f64).sin()) as i16;
        buf.extend_from_slice(&v.to_le_bytes());
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf
}

/// Serves a tone of the given length per identifier; unknown ones fail with
/// cause "boom".
pub(crate) struct ToneOpener(pub HashMap<String, f64>);

pub(crate) fn opener(lengths: &[(&str, f64)]) -> Arc<dyn Opener> {
    Arc::new(ToneOpener(lengths.iter().map(|(k, v)| (k.to_string(), *v)).collect()))
}

#[async_trait]
impl Opener for ToneOpener {
    async fn open(&self, t: &Track, _start_ms: i64) -> Result<Stream, Failure> {
        let Some(secs) = self.0.get(&t.identifier) else {
            return Err(Failure::common(&t.source, "open failed", Some("boom".into())));
        };
        Ok(Stream {
            body: Box::new(std::io::Cursor::new(tone(*secs, 440.0))),
            format: Format::Pcm48,
            start_sample: 0,
            live: false,
        })
    }
}

/// Collects every event.
#[derive(Clone, Default)]
pub(crate) struct Recorder(pub Arc<Mutex<Vec<Event>>>);

impl Recorder {
    pub(crate) fn has(&self, t: EventType) -> bool {
        self.0.lock().iter().any(|e| e.kind == t)
    }

    pub(crate) fn count(&self, t: EventType) -> usize {
        self.0.lock().iter().filter(|e| e.kind == t).count()
    }

    pub(crate) fn events(&self) -> Vec<Event> {
        self.0.lock().clone()
    }

    /// The first event of type `t`, waiting up to 3 s.
    pub(crate) async fn wait(&self, t: EventType) -> Event {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Some(e) = self.0.lock().iter().find(|e| e.kind == t) {
                return e.clone();
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("no {t:?} event");
    }

    /// Wait until at least `n` events of type `t` arrived (up to 1 s).
    pub(crate) async fn wait_count(&self, t: EventType, n: usize) -> usize {
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.count(t) < n && Instant::now() < deadline {
            sleep(Duration::from_millis(5)).await;
        }
        self.count(t)
    }
}

pub(crate) fn config(lengths: &[(&str, f64)], rec: &Recorder) -> PlayerConfig {
    let r = rec.clone();
    let mut cfg = PlayerConfig::new(
        "1",
        opener(lengths),
        Arc::new(IdentityResolver) as Arc<dyn Resolver>,
        Arc::new(move |e| r.0.lock().push(e)),
        tokio::runtime::Handle::current(),
    );
    cfg.buffer_frames = 200;
    cfg
}

pub(crate) fn new_test_player(lengths: &[(&str, f64)]) -> (Arc<Player>, Recorder) {
    let rec = Recorder::default();
    (Player::new(config(lengths, &rec)), rec)
}

/// Pull a frame, retrying for up to 2 s while the deck buffers.
pub(crate) async fn frame(p: &Player, out: &mut Frame) -> FrameInfo {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(info) = p.next_frame(out) {
            return info;
        }
        sleep(Duration::from_millis(1)).await;
    }
    panic!("no frame produced");
}

pub(crate) fn track(id: &str) -> Track {
    Track {
        source: "test".into(),
        identifier: id.into(),
        title: id.into(),
        ..Default::default()
    }
}

pub(crate) fn peak(f: &Frame) -> i32 {
    wavyzon_audio::peak(f)
}
