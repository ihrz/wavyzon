//! A deck: one track being opened, decoded, resampled, time-stretched and
//! filtered into 20 ms frames.
//!
//! The producer side runs off the real-time path: the [`Opener`]'s async
//! body is read on the runtime (with an idle timeout), decoded on a blocking
//! thread and pushed into a bounded chunk queue. [`Deck::read`], called from
//! the send loop, never blocks: when the queue is empty it reports a miss.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use tokio::io::AsyncReadExt;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;
use wavyzon_audio::dsp::{Resampler, Stretcher};
use wavyzon_audio::{CHANNELS, FRAME_SAMPLES, FRAME_SAMPLES_PER_CHANNEL, Frame, SAMPLE_RATE, clip, codec};
use wavyzon_plugin::{Failure, Format, Stream, Track};

use crate::filter_ramp::FilterRamp;
use crate::filters::{Effects, Filters};
use crate::plugin_chain::PluginChain;
use crate::traits::{Opener, Tap};

/// Decoded samples per channel in one queued chunk.
const CHUNK_SAMPLES: usize = 4096;
/// A deck with no audio for this long while not finished is reported stuck.
pub(crate) const STUCK_AFTER: Duration = Duration::from_secs(10);
/// A source that sends nothing for this long fails with "stream stalled".
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const DECODE_CHUNK_SIZE: usize = CHUNK_SAMPLES * CHANNELS;
/// Raw byte chunks buffered between the async reader and the decoder.
const PUMP_CHUNKS: usize = 16;
const PUMP_BYTES: usize = 32 << 10;

/// Decoded PCM at the source's native rate.
pub(crate) struct Chunk {
    samples: Vec<i16>,
    /// Native sample index (per channel) of the first sample.
    start: i64,
    rate: u32,
}

struct QueueState {
    chunks: VecDeque<Chunk>,
    /// The producer finished (successfully or not).
    done: bool,
    /// The deck was closed; the producer must stop.
    closed: bool,
}

enum Pop {
    Chunk(Chunk),
    Empty,
    Drained,
}

/// State shared between a deck and its producer.
pub(crate) struct DeckShared {
    queue: Mutex<QueueState>,
    cv: Condvar,
    cap: usize,
    failure: Mutex<Option<Failure>>,
    pub(crate) cancel: CancellationToken,
}

impl DeckShared {
    fn new(cap: usize, cancel: CancellationToken) -> Self {
        Self {
            queue: Mutex::new(QueueState {
                chunks: VecDeque::new(),
                done: false,
                closed: false,
            }),
            cv: Condvar::new(),
            cap,
            failure: Mutex::new(None),
            cancel,
        }
    }

    /// Blocking push (producer thread). `false` once the deck is closed.
    fn push(&self, c: Chunk) -> bool {
        let mut q = self.queue.lock();
        while q.chunks.len() >= self.cap && !q.closed {
            self.cv.wait(&mut q);
        }
        if q.closed {
            return false;
        }
        q.chunks.push_back(c);
        true
    }

    fn try_pop(&self) -> Pop {
        let mut q = self.queue.lock();
        match q.chunks.pop_front() {
            Some(c) => {
                self.cv.notify_one();
                Pop::Chunk(c)
            }
            None if q.done => Pop::Drained,
            None => Pop::Empty,
        }
    }

    fn finish(&self) {
        self.queue.lock().done = true;
    }

    fn close(&self) {
        self.queue.lock().closed = true;
        self.cv.notify_all();
        self.cancel.cancel();
    }

    /// The producer is done and every chunk was consumed.
    pub(crate) fn drained(&self) -> bool {
        let q = self.queue.lock();
        q.done && q.chunks.is_empty()
    }

    /// Record `f` unless a failure is already recorded.
    fn fail(&self, f: Failure) {
        let mut slot = self.failure.lock();
        if slot.is_none() {
            *slot = Some(f);
        }
    }

    fn failure(&self) -> Option<Failure> {
        self.failure.lock().clone()
    }
}

/// One deck.
pub(crate) struct Deck {
    /// Unique per player, to recognise the deck in producer callbacks.
    pub id: u64,
    /// The track the client asked for.
    pub track: Track,
    /// What actually plays (after resolving/mirroring).
    pub playable: Track,
    pub(crate) shared: Arc<DeckShared>,

    resampler: Option<Resampler>,
    rate: u32,
    stretch: Stretcher,
    float_buf: Vec<f64>,
    pos_buf: Vec<f64>,
    hop: Vec<f64>,

    pub filters: Filters,
    fx: Effects,
    pub(crate) ramp: FilterRamp,
    pub(crate) plugins: PluginChain,
    tempo_ratio: f64,
    position_ms: f64,
    starved_at: Option<Instant>,
    stuck_sent: bool,
    tap: Option<Box<dyn Tap>>,
    /// Loudness normalisation gain.
    pub norm: f64,
    /// Whether `norm` comes from a measurement.
    pub measured: bool,
    ended: bool,
}

impl Drop for Deck {
    fn drop(&mut self) {
        self.shared.close();
    }
}

impl Deck {
    /// A deck that will start at `start_ms` once [`spawn_producer`] runs.
    pub(crate) fn new(
        id: u64,
        track: Track,
        playable: Track,
        buffer_frames: usize,
        start_ms: i64,
        cancel: CancellationToken,
    ) -> Self {
        let chunks = (buffer_frames * FRAME_SAMPLES_PER_CHANNEL / CHUNK_SAMPLES).max(4);
        Self {
            id,
            track,
            playable,
            shared: Arc::new(DeckShared::new(chunks, cancel)),
            resampler: None,
            rate: 0,
            stretch: Stretcher::new(),
            float_buf: Vec::new(),
            pos_buf: Vec::new(),
            hop: vec![0.0; FRAME_SAMPLES],
            filters: Filters::default(),
            fx: Effects::default(),
            ramp: FilterRamp::default(),
            plugins: PluginChain::default(),
            tempo_ratio: 1.0,
            position_ms: start_ms as f64,
            starved_at: None,
            stuck_sent: false,
            tap: None,
            norm: 1.0,
            measured: false,
            ended: false,
        }
    }

    pub(crate) fn failure(&self) -> Option<Failure> {
        self.shared.failure()
    }

    pub(crate) fn set_tempo_ratio(&mut self, r: f64) {
        self.tempo_ratio = if r <= 0.0 { 1.0 } else { r };
    }

    pub(crate) fn tempo_ratio(&self) -> f64 {
        self.tempo_ratio
    }

    pub(crate) fn attach_tap(&mut self, t: Box<dyn Tap>) {
        self.tap = Some(t);
    }

    pub(crate) fn position_ms(&self) -> i64 {
        self.position_ms as i64
    }

    pub(crate) fn ended(&self) -> bool {
        self.ended
    }

    /// Whether the deck has starved for [`STUCK_AFTER`]; reports once per stall.
    pub(crate) fn stuck(&mut self) -> bool {
        match self.starved_at {
            Some(at) if !self.stuck_sent && at.elapsed() >= STUCK_AFTER => {
                self.stuck_sent = true;
                true
            }
            _ => false,
        }
    }

    /// Feed one queued chunk into the resampler.
    fn feed(&mut self) -> Fed {
        match self.shared.try_pop() {
            Pop::Chunk(c) => {
                let factor = self.ramp.resample_factor(&self.filters);
                match &mut self.resampler {
                    Some(r) if c.rate == self.rate => r.write(&c.samples),
                    slot => {
                        self.rate = c.rate;
                        let mut r = Resampler::new(c.rate, factor);
                        r.reset(c.start);
                        r.write(&c.samples);
                        *slot = Some(r);
                    }
                }
                Fed::Chunk
            }
            Pop::Empty => Fed::Empty,
            Pop::Drained => Fed::Drained,
        }
    }

    /// Produce the next frame into `out`. Returns the deck position (ms) and
    /// whether a frame was produced. Never blocks.
    pub(crate) fn read(&mut self, out: &mut Frame) -> (f64, bool) {
        if self.ended {
            return (self.position_ms, false);
        }
        self.ramp.advance(&self.filters);
        self.stretch
            .set_tempo(self.ramp.tempo_factor(&self.filters) * self.tempo_ratio);
        loop {
            if let Some(pos) = self.stretch.next(&mut self.hop) {
                self.emit(out, pos);
                return (self.position_ms, true);
            }
            if self.resampler.is_some() {
                let factor = self.ramp.resample_factor(&self.filters);
                let rate = self.rate;
                if let Some(r) = &mut self.resampler {
                    r.configure(rate, factor);
                }
                if self.pump() {
                    continue;
                }
            }
            match self.feed() {
                Fed::Chunk => continue,
                Fed::Drained => {
                    self.ended = true;
                    if let Some(t) = self.tap.take() {
                        t.finish();
                    }
                    return (self.position_ms, false);
                }
                Fed::Empty => {
                    if self.starved_at.is_none() {
                        self.starved_at = Some(Instant::now());
                    }
                    return (self.position_ms, false);
                }
            }
        }
    }

    /// Move resampled audio (with per-sample source positions) into the
    /// stretcher.
    fn pump(&mut self) -> bool {
        let Some(r) = &mut self.resampler else { return false };
        let avail = r.available();
        if avail == 0 {
            return false;
        }
        let n = avail.min(4096);
        if self.float_buf.len() < 2 * n {
            self.float_buf.resize(2 * n, 0.0);
            self.pos_buf.resize(n, 0.0);
        }
        let first = r.position();
        let step = r.step();
        let got = r.read(&mut self.float_buf[..2 * n]);
        let rate = self.rate as f64;
        for i in 0..got {
            self.pos_buf[i] = (first + i as f64 * step) * 1000.0 / rate;
        }
        self.stretch.write(&self.float_buf[..2 * got], &self.pos_buf[..got]);
        got > 0
    }

    fn emit(&mut self, out: &mut Frame, pos_ms: f64) {
        for (o, v) in out.iter_mut().zip(&self.hop) {
            *o = clip(*v);
        }
        let eff = self.ramp.effective();
        self.fx.apply(&eff, &self.ramp.wet, out);
        self.plugins.process(out);
        self.position_ms = pos_ms;
        self.starved_at = None;
        self.stuck_sent = false;
        if let Some(t) = &mut self.tap {
            t.feed(out);
        }
    }
}

enum Fed {
    Chunk,
    Empty,
    Drained,
}

/// Start producing audio for a deck: open the stream on `rt`, read it
/// asynchronously, decode on a blocking thread. `on_done` runs once the
/// producer has finished (after any failure was recorded).
pub(crate) fn spawn_producer(
    rt: &Handle,
    shared: Arc<DeckShared>,
    opener: Arc<dyn Opener>,
    playable: Track,
    start_ms: i64,
    on_done: Box<dyn FnOnce() + Send>,
) {
    let handle = rt.clone();
    rt.spawn(async move {
        produce(&handle, &shared, opener, playable, start_ms).await;
        shared.finish();
        on_done();
    });
}

async fn produce(rt: &Handle, shared: &Arc<DeckShared>, opener: Arc<dyn Opener>, playable: Track, start_ms: i64) {
    let cancel = shared.cancel.clone();
    // The idle timer covers opening too (as in Cadence), so a source that
    // never answers fails the track instead of only reporting it stuck.
    let opened = tokio::select! {
        _ = cancel.cancelled() => return,
        r = tokio::time::timeout(STREAM_IDLE_TIMEOUT, opener.open(&playable, start_ms)) => match r {
            Ok(r) => r,
            Err(_) => Err(Failure::common(
                &playable.source,
                "stream stalled",
                Some(format!("no data for {}s", STREAM_IDLE_TIMEOUT.as_secs())),
            )),
        },
    };
    let Stream {
        mut body,
        format,
        start_sample,
        ..
    } = match opened {
        Ok(s) => s,
        Err(f) => {
            if !cancel.is_cancelled() {
                shared.fail(f);
            }
            return;
        }
    };
    let source = playable.source.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(PUMP_CHUNKS);
    let decoder = {
        let shared = shared.clone();
        let source = source.clone();
        rt.spawn_blocking(move || {
            decode_loop(
                &shared,
                &source,
                &format,
                ChannelReader::new(rx),
                start_ms,
                start_sample,
            )
        })
    };
    let mut buf = vec![0u8; PUMP_BYTES];
    loop {
        let read = tokio::select! {
            _ = cancel.cancelled() => break,
            r = tokio::time::timeout(STREAM_IDLE_TIMEOUT, body.read(&mut buf)) => r,
        };
        match read {
            Err(_) => {
                shared.fail(Failure::common(
                    &source,
                    "stream stalled",
                    Some(format!("no data for {}s", STREAM_IDLE_TIMEOUT.as_secs())),
                ));
                break;
            }
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let sent = tokio::select! {
                    _ = cancel.cancelled() => false,
                    r = tx.send(buf[..n].to_vec()) => r.is_ok(),
                };
                if !sent {
                    break;
                }
            }
            Ok(Err(e)) => {
                if !cancel.is_cancelled() {
                    shared.fail(Failure::common(&source, "stream interrupted", Some(e.to_string())));
                }
                break;
            }
        }
    }
    drop(tx);
    let _ = decoder.await;
}

/// Blocking `Read` over the byte channel fed by the async pump.
struct ChannelReader {
    rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    cur: Vec<u8>,
    off: usize,
}

impl ChannelReader {
    fn new(rx: tokio::sync::mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            cur: Vec::new(),
            off: 0,
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.off >= self.cur.len() {
            match self.rx.blocking_recv() {
                Some(b) => {
                    self.cur = b;
                    self.off = 0;
                }
                None => return Ok(0),
            }
        }
        let n = out.len().min(self.cur.len() - self.off);
        out[..n].copy_from_slice(&self.cur[self.off..self.off + n]);
        self.off += n;
        Ok(n)
    }
}

fn decode_loop(
    shared: &DeckShared,
    source: &str,
    format: &Format,
    reader: ChannelReader,
    start_ms: i64,
    start_sample: i64,
) {
    let mut pcm = match codec::decode(format, Box::new(reader)) {
        Ok(p) => p,
        Err(e) => {
            if !shared.cancel.is_cancelled() {
                shared.fail(Failure::fault(source, "decode failed", Some(e.to_string())));
            }
            return;
        }
    };
    let rate = pcm.sample_rate().max(1);
    let skip = start_ms * rate as i64 / 1000;
    let mut index = start_sample * rate as i64 / SAMPLE_RATE as i64;
    loop {
        if shared.cancel.is_cancelled() {
            return;
        }
        let mut buf = vec![0i16; DECODE_CHUNK_SIZE];
        match pcm.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                let frames = (n / CHANNELS) as i64;
                if frames == 0 {
                    continue;
                }
                buf.truncate(frames as usize * CHANNELS);
                if index + frames > skip {
                    let mut start = index;
                    if index < skip {
                        let cut = (skip - index) as usize;
                        buf.drain(..cut * CHANNELS);
                        start = skip;
                    }
                    if !shared.push(Chunk {
                        samples: buf,
                        start,
                        rate,
                    }) {
                        return;
                    }
                }
                index += frames;
            }
            Err(e) => {
                if !shared.cancel.is_cancelled() {
                    shared.fail(Failure::common(source, "stream interrupted", Some(e.to_string())));
                }
                return;
            }
        }
    }
}
