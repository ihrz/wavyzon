//! Background analysis of tracks that have not been played yet (so their
//! loudness and fingerprint are known before they start).
//!
//! A fixed pool of workers drains a bounded queue; when the queue is full new
//! requests are dropped (and counted). Each job opens the track through an
//! [`Opener`], decodes it on a blocking thread, resamples it to 48 kHz and
//! stores the [`Fingerprint`] in the shared [`Registry`].

use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use wavyzon_audio::dsp::Resampler;
use wavyzon_audio::{FRAME_SAMPLES, FRAME_SAMPLES_PER_CHANNEL, SAMPLE_RATE, clip, codec};
use wavyzon_plugin::{Failure, Stream, Track};

use crate::analyzer::Analyzer;
use crate::fingerprint::{Fingerprint, Registry, key};

/// Longest one offline analysis may take, opening included.
pub const OFFLINE_TIMEOUT: Duration = Duration::from_secs(3 * 60);
/// Only the first 20 minutes of a track are analysed.
pub const MAX_TRACK_LENGTH: Duration = Duration::from_secs(20 * 60);
/// How long a track that failed to analyse is not retried.
pub const FAIL_RETRY: Duration = Duration::from_secs(30 * 60);
const MAX_FAILURES: usize = 4096;

/// Opens a track's audio stream for analysis. Usually resolves the playable
/// track and opens it at position 0 through the source registry.
pub type Opener = Arc<dyn Fn(Track) -> BoxFuture<'static, Result<Stream, Failure>> + Send + Sync>;

/// Counters exposed on `/v1/info` and `/metrics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Stats {
    /// Jobs waiting in the queue (not counting the ones being analysed).
    pub queued: i64,
    /// Requests refused because the queue was full.
    pub dropped: i64,
    /// Tracks analysed successfully.
    pub done: i64,
    /// Analyses that failed (open, decode, timeout or panic).
    pub failed: i64,
}

struct Job {
    track: Track,
    key: String,
}

#[derive(Default)]
struct State {
    in_flight: HashSet<String>,
    failures: HashMap<String, Instant>,
}

struct Shared {
    registry: Arc<Registry>,
    open: Opener,
    state: Mutex<State>,
    dropped: AtomicI64,
    done: AtomicI64,
    failed: AtomicI64,
}

/// The offline analyser: a bounded worker pool fed by a bounded queue.
///
/// Dropping it stops the workers once they finish their current job.
pub struct Offline {
    shared: Arc<Shared>,
    tx: mpsc::Sender<Job>,
    workers: usize,
}

impl std::fmt::Debug for Offline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Offline")
            .field("workers", &self.workers)
            .field("stats", &self.stats())
            .finish()
    }
}

impl Offline {
    /// Start `workers` workers (at least 1) behind a queue of `queue_size`
    /// jobs (at least 1). Results go into `registry`.
    ///
    /// Must be called from within a Tokio runtime.
    pub fn new(registry: Arc<Registry>, open: Opener, workers: usize, queue_size: usize) -> Self {
        let workers = workers.max(1);
        let (tx, rx) = mpsc::channel(queue_size.max(1));
        let shared = Arc::new(Shared {
            registry,
            open,
            state: Mutex::default(),
            dropped: AtomicI64::new(0),
            done: AtomicI64::new(0),
            failed: AtomicI64::new(0),
        });
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        for _ in 0..workers {
            tokio::spawn(worker(shared.clone(), rx.clone()));
        }
        Self { shared, tx, workers }
    }

    /// The registry results are stored in.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.shared.registry
    }

    /// Number of workers.
    pub fn workers(&self) -> usize {
        self.workers
    }

    /// Queue capacity.
    pub fn capacity(&self) -> usize {
        self.tx.max_capacity()
    }

    /// Jobs currently waiting in the queue.
    pub fn queue_len(&self) -> usize {
        self.tx.max_capacity() - self.tx.capacity()
    }

    /// Current counters.
    pub fn stats(&self) -> Stats {
        Stats {
            queued: self.queue_len() as i64,
            dropped: self.shared.dropped.load(Ordering::Relaxed),
            done: self.shared.done.load(Ordering::Relaxed),
            failed: self.shared.failed.load(Ordering::Relaxed),
        }
    }

    /// Queue `track` for analysis. Returns whether it was queued.
    ///
    /// Skipped when it is already analysed (unless `force`), already queued
    /// or being analysed, failed less than [`FAIL_RETRY`] ago (even with
    /// `force`), or the queue is full (counted in [`Stats::dropped`]).
    pub fn enqueue(&self, track: Track, force: bool) -> bool {
        let k = key(&track.source, &track.identifier);
        if !force && self.shared.registry.has(&track.source, &track.identifier) {
            return false;
        }
        {
            let mut st = self.shared.state.lock();
            if st.failures.get(&k).is_some_and(|at| at.elapsed() < FAIL_RETRY) {
                return false;
            }
            if !st.in_flight.insert(k.clone()) {
                return false;
            }
        }
        match self.tx.try_send(Job { track, key: k }) {
            Ok(()) => true,
            Err(e) => {
                let job = match e {
                    mpsc::error::TrySendError::Full(j) | mpsc::error::TrySendError::Closed(j) => j,
                };
                self.shared.release(&job.key);
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(component = "analysis", track = %job.key, "analysis queue full, dropping");
                false
            }
        }
    }
}

impl Shared {
    fn release(&self, k: &str) {
        self.state.lock().in_flight.remove(k);
    }

    fn remember(&self, k: &str) {
        let mut st = self.state.lock();
        if st.failures.len() >= MAX_FAILURES {
            st.failures.retain(|_, at| at.elapsed() < FAIL_RETRY);
        }
        if st.failures.len() >= MAX_FAILURES {
            tracing::warn!(
                component = "analysis",
                entries = st.failures.len(),
                "analysis failure memory full, forgetting it"
            );
            st.failures.clear();
        }
        st.failures.insert(k.to_owned(), Instant::now());
    }
}

async fn worker(shared: Arc<Shared>, rx: Arc<tokio::sync::Mutex<mpsc::Receiver<Job>>>) {
    loop {
        let job = rx.lock().await.recv().await;
        let Some(job) = job else { return };
        let k = job.key.clone();
        // Run in its own task so a panic is contained and reported.
        let outcome = tokio::spawn(analyze(shared.clone(), job.track)).await;
        match outcome {
            Ok(Ok(fp)) => {
                shared.registry.store(Arc::new(fp));
                shared.done.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(component = "analysis", track = %k, "offline analysis done");
            }
            Ok(Err(error)) => {
                shared.failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(component = "analysis", track = %k, %error, retry_after_s = FAIL_RETRY.as_secs(), "analysis failed");
                shared.remember(&k);
            }
            Err(e) => {
                shared.failed.fetch_add(1, Ordering::Relaxed);
                tracing::error!(component = "analysis", track = %k, panic = %e, "analysis panicked");
            }
        }
        shared.release(&k);
    }
}

async fn analyze(shared: Arc<Shared>, track: Track) -> Result<Fingerprint, String> {
    let deadline = Instant::now() + OFFLINE_TIMEOUT;
    let stream = tokio::time::timeout_at(deadline.into(), (shared.open)(track.clone()))
        .await
        .map_err(|_| "timed out opening the stream".to_owned())?
        .map_err(|f| f.to_string())?;
    let handle = Handle::current();
    match tokio::task::spawn_blocking(move || decode_and_analyze(stream, &track, handle, deadline)).await {
        Ok(r) => r,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(e.to_string()),
    }
}

/// Blocking reader over an async body that gives up at `deadline`.
struct DeadlineReader {
    body: Box<dyn AsyncRead + Send + Unpin>,
    handle: Handle,
    deadline: Instant,
}

impl Read for DeadlineReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let deadline = self.deadline.into();
        let body = &mut self.body;
        self.handle
            .block_on(tokio::time::timeout_at(deadline, body.read(buf)))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "analysis timed out"))?
    }
}

fn decode_and_analyze(stream: Stream, track: &Track, handle: Handle, deadline: Instant) -> Result<Fingerprint, String> {
    let reader = DeadlineReader {
        body: stream.body,
        handle,
        deadline,
    };
    let mut pcm = codec::decode(&stream.format, Box::new(reader)).map_err(|e| e.to_string())?;
    // Cadence's windowed-sinc resampler, the one the player uses, so offline
    // and live fingerprints of the same track agree.
    let mut resampler = Resampler::new(pcm.sample_rate(), 1.0);
    let mut analyzer = Analyzer::new(track.source.clone(), track.identifier.clone(), None);
    let limit = MAX_TRACK_LENGTH.as_millis() as i64 * SAMPLE_RATE as i64 / 1000;
    let mut input = vec![0i16; 8192];
    let mut out = [0f64; FRAME_SAMPLES];
    let mut frame = [0i16; FRAME_SAMPLES];
    let mut pending = 0;
    while analyzer.samples() < limit {
        if Instant::now() >= deadline {
            return Err("analysis timed out".into());
        }
        let n = pcm.read(&mut input).map_err(|e| format!("decode: {e}"))?;
        if n == 0 {
            break;
        }
        resampler.write(&input[..n]);
        loop {
            pending += resampler.read(&mut out[pending * 2..]);
            if pending < FRAME_SAMPLES_PER_CHANNEL {
                break;
            }
            for (f, o) in frame.iter_mut().zip(&out) {
                *f = clip(*o);
            }
            analyzer.feed_samples(&frame);
            pending = 0;
        }
    }
    Ok(Arc::unwrap_or_clone(analyzer.result()))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tokio::sync::Semaphore;
    use wavyzon_plugin::Format;

    use super::*;
    use crate::tests_support::{click_samples, wav_bytes};

    fn track(id: &str) -> Track {
        Track {
            source: "s".into(),
            identifier: id.into(),
            ..Default::default()
        }
    }

    async fn wait_for(cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn offline_queue_drops_when_full() {
        let gate = Arc::new(Semaphore::new(0));
        let g = gate.clone();
        let open: Opener = Arc::new(move |_t| {
            let g = g.clone();
            Box::pin(async move {
                let _permit = g.acquire().await;
                Err(Failure::common("s", "closed", None))
            })
        });
        let o = Offline::new(Arc::new(Registry::new()), open, 1, 2);
        let accepted = (0..10)
            .filter(|i| o.enqueue(track(&format!("{}", (b'a' + *i as u8) as char)), false))
            .count();
        let dropped = o.stats().dropped;
        assert!(accepted <= 3 && dropped >= 7, "accepted {accepted} dropped {dropped}");
        assert!(o.stats().queued >= 1);
        assert!(!o.enqueue(track("a"), false), "in-flight track queued twice");
        gate.add_permits(100);
        wait_for(|| o.stats().failed >= accepted as i64).await;
        assert_eq!(o.stats().failed, accepted as i64);
        wait_for(|| o.shared.state.lock().in_flight.is_empty()).await;
        assert!(
            !o.enqueue(track("a"), false),
            "a track that just failed was queued again before FAIL_RETRY"
        );
        assert!(!o.enqueue(track("a"), true), "force bypassed the failure memory");
        o.shared
            .state
            .lock()
            .failures
            .insert("s:a".into(), Instant::now() - FAIL_RETRY);
        assert!(
            o.enqueue(track("a"), false),
            "failed track was not released after FAIL_RETRY"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn offline_analyses_and_caches() {
        let wav = Arc::new(wav_bytes(44_100, &click_samples(44_100, 120.0, 4, 20)));
        let opens = Arc::new(AtomicI64::new(0));
        let (w, n) = (wav.clone(), opens.clone());
        let open: Opener = Arc::new(move |_t| {
            let body = Cursor::new((*w).clone());
            n.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                Ok(Stream {
                    body: Box::new(body),
                    format: Format::Encoded {
                        hint: Some("wav".into()),
                    },
                    start_sample: 0,
                    live: false,
                })
            })
        });
        let registry = Arc::new(Registry::new());
        let o = Offline::new(registry.clone(), open, 2, 8);
        assert!(o.enqueue(track("clicks"), false));
        wait_for(|| registry.has("s", "clicks")).await;
        let fp = registry.get("s", "clicks").expect("analysed");
        assert!((fp.duration_ms - 20_000).abs() <= 25, "duration {}", fp.duration_ms);
        assert!((fp.rhythm.bpm - 120.0).abs() <= 2.0, "bpm {}", fp.rhythm.bpm);
        assert!(fp.loudness.integrated_lufs > crate::FLOOR_LUFS);
        assert_eq!(o.stats().done, 1);
        assert!(!o.enqueue(track("clicks"), false), "cached track analysed twice");
        wait_for(|| o.shared.state.lock().in_flight.is_empty()).await;
        assert!(o.enqueue(track("clicks"), true), "force must re-analyse");
        wait_for(|| o.stats().done == 2).await;
        assert_eq!(opens.load(Ordering::Relaxed), 2);
    }

    /// The same audio at 44.1 kHz and 48 kHz analyses alike: the sinc
    /// resampler (unlike linear interpolation) keeps the spectrum, so the
    /// K-weighted loudness and the tempo don't depend on the source rate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn source_rate_does_not_change_the_analysis() {
        let analyse = |rate: u32| {
            let wav = wav_bytes(rate, &click_samples(rate, 120.0, 4, 20));
            let handle = Handle::current();
            tokio::task::spawn_blocking(move || {
                let stream = Stream {
                    body: Box::new(Cursor::new(wav)),
                    format: Format::Encoded {
                        hint: Some("wav".into()),
                    },
                    start_sample: 0,
                    live: false,
                };
                decode_and_analyze(stream, &track("r"), handle, Instant::now() + OFFLINE_TIMEOUT)
            })
        };
        let a = analyse(44_100).await.unwrap().expect("44.1 kHz");
        let b = analyse(48_000).await.unwrap().expect("48 kHz");
        let (la, lb) = (a.loudness.integrated_lufs, b.loudness.integrated_lufs);
        assert!((la - lb).abs() <= 0.5, "44.1 kHz {la} LUFS vs 48 kHz {lb} LUFS");
        assert!((a.rhythm.bpm - b.rhythm.bpm).abs() <= 1.0);
        assert!((a.duration_ms - b.duration_ms).abs() <= 25);
    }
}
