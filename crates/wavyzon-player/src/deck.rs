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
use std::sync::atomic::{AtomicBool, Ordering};
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
/// How many times a deck reopens its stream after it broke mid-track.
const MAX_RESUMES: u32 = 3;

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
    /// Native `(sample index, rate)` right after the last decoded chunk:
    /// where a reopened stream resumes.
    decoded_end: Mutex<Option<(i64, u32)>>,
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
            decoded_end: Mutex::new(None),
            cancel,
        }
    }

    fn mark_decoded(&self, end: i64, rate: u32) {
        *self.decoded_end.lock() = Some((end, rate));
    }

    fn decoded_end(&self) -> Option<(i64, u32)> {
        *self.decoded_end.lock()
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

/// Open the stream and decode it; when the connection breaks mid-track
/// (an error or no data for [`STREAM_IDLE_TIMEOUT`]), reopen it where the
/// decoder stopped, up to [`MAX_RESUMES`] times.
///
/// Not in Cadence. A body nobody reads for a while is cut by the CDN or by
/// the HTTP client's read timeout: a preloaded next track (gapless /
/// crossfade) waits whole minutes with a full buffer, and so does a long
/// pause. Lavaplayer's persistent HTTP streams reconnect the same way.
async fn produce(rt: &Handle, shared: &Arc<DeckShared>, opener: Arc<dyn Opener>, playable: Track, start_ms: i64) {
    let cancel = shared.cancel.clone();
    let mut resume_at: Option<(i64, u32)> = None;
    let mut resumes = 0;
    loop {
        let open_ms = resume_at.map_or(start_ms, |(index, rate)| index * 1000 / rate.max(1) as i64);
        // The idle timer covers opening too (as in Cadence), so a source that
        // never answers fails the track instead of only reporting it stuck.
        let opened = tokio::select! {
            _ = cancel.cancelled() => return,
            r = tokio::time::timeout(STREAM_IDLE_TIMEOUT, opener.open(&playable, open_ms)) => match r {
                Ok(r) => r,
                Err(_) => Err(Failure::common(
                    &playable.source,
                    "stream stalled",
                    Some(format!("no data for {}s", STREAM_IDLE_TIMEOUT.as_secs())),
                )),
            },
        };
        let stream = match opened {
            Ok(s) => s,
            Err(f) => {
                if !cancel.is_cancelled() {
                    shared.fail(f);
                }
                return;
            }
        };
        let live = stream.live || playable.is_stream;
        let cause = match pump(rt, shared, &playable.source, stream, open_ms, resume_at).await {
            Pumped::Done => return,
            Pumped::Broken(cause) => cause,
        };
        if cancel.is_cancelled() || shared.failure().is_some() {
            return;
        }
        if live || resumes >= MAX_RESUMES {
            shared.fail(Failure::common(&playable.source, "stream interrupted", Some(cause)));
            return;
        }
        resumes += 1;
        resume_at = shared.decoded_end().or(resume_at);
        let at_ms = resume_at.map_or(start_ms, |(index, rate)| index * 1000 / rate.max(1) as i64);
        tracing::warn!(
            track = %playable.key(),
            at_ms,
            attempt = resumes,
            cause = %cause,
            "stream interrupted, reopening it where it stopped"
        );
    }
}

/// How one opened stream ended.
enum Pumped {
    /// It ended, the deck was closed, or it failed for good (recorded).
    Done,
    /// The connection broke mid-track; the decoder stopped where it got.
    Broken(String),
}

/// Read one opened stream into a decoder until it ends or breaks.
async fn pump(
    rt: &Handle,
    shared: &Arc<DeckShared>,
    source: &str,
    stream: Stream,
    start_ms: i64,
    resume_at: Option<(i64, u32)>,
) -> Pumped {
    let cancel = shared.cancel.clone();
    let Stream {
        mut body,
        format,
        start_sample,
        ..
    } = stream;
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(PUMP_CHUNKS);
    // Set when the body broke: the decoder then stops on the cut-off data
    // without reporting it (the stream is reopened).
    let abandoned = Arc::new(AtomicBool::new(false));
    let decoder = {
        let shared = shared.clone();
        let source = source.to_string();
        let abandoned = abandoned.clone();
        rt.spawn_blocking(move || {
            decode_loop(
                &shared,
                &source,
                &format,
                ChannelReader::new(rx),
                Skip {
                    start_ms,
                    start_sample,
                    resume_at,
                },
                &abandoned,
            )
        })
    };
    let mut buf = vec![0u8; PUMP_BYTES];
    let outcome = loop {
        let read = tokio::select! {
            _ = cancel.cancelled() => break Pumped::Done,
            r = tokio::time::timeout(STREAM_IDLE_TIMEOUT, body.read(&mut buf)) => r,
        };
        match read {
            Err(_) => break Pumped::Broken(format!("no data for {}s", STREAM_IDLE_TIMEOUT.as_secs())),
            Ok(Ok(0)) => break Pumped::Done,
            Ok(Ok(n)) => {
                let sent = tokio::select! {
                    _ = cancel.cancelled() => false,
                    r = tx.send(buf[..n].to_vec()) => r.is_ok(),
                };
                if !sent {
                    break Pumped::Done;
                }
            }
            Ok(Err(e)) => break Pumped::Broken(e.to_string()),
        }
    };
    if matches!(outcome, Pumped::Broken(_)) {
        abandoned.store(true, Ordering::SeqCst);
    }
    drop(tx);
    // A preloaded deck's decoder may wait here until playback frees room
    // in its queue: the reopen then happens with that buffer still ahead.
    let _ = decoder.await;
    outcome
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

/// Where decoding output starts.
struct Skip {
    /// Requested start (ms).
    start_ms: i64,
    /// Where the stream actually starts, in 48 kHz samples.
    start_sample: i64,
    /// Exact native `(index, rate)` to resume at after a reopen.
    resume_at: Option<(i64, u32)>,
}

fn decode_loop(
    shared: &DeckShared,
    source: &str,
    format: &Format,
    reader: ChannelReader,
    from: Skip,
    abandoned: &AtomicBool,
) {
    let mut pcm = match codec::decode(format, Box::new(reader)) {
        Ok(p) => p,
        Err(e) => {
            if !shared.cancel.is_cancelled() && !abandoned.load(Ordering::SeqCst) {
                shared.fail(Failure::fault(source, "decode failed", Some(e.to_string())));
            }
            return;
        }
    };
    let rate = pcm.sample_rate().max(1);
    // A resumed stream continues on the exact sample the previous one
    // stopped at (no gap, no repeat).
    let skip = match from.resume_at {
        Some((index, r)) if r == rate => index,
        _ => from.start_ms * rate as i64 / 1000,
    };
    let mut index = from.start_sample * rate as i64 / SAMPLE_RATE as i64;
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
                    let end = start + (buf.len() / CHANNELS) as i64;
                    if !shared.push(Chunk {
                        samples: buf,
                        start,
                        rate,
                    }) {
                        return;
                    }
                    shared.mark_decoded(end, rate);
                }
                index += frames;
            }
            Err(e) => {
                if !shared.cancel.is_cancelled() && !abandoned.load(Ordering::SeqCst) {
                    shared.fail(Failure::common(source, "stream interrupted", Some(e.to_string())));
                }
                return;
            }
        }
    }
}
