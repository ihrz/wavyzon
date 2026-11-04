//! A stream that breaks mid-track is reopened where the decoder stopped.

use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};
use wavyzon_audio::SILENCE;

use super::*;

/// Serves the bytes up to `cut`, then fails like a reset connection.
struct Breaking {
    data: std::io::Cursor<Vec<u8>>,
    cut: u64,
}

impl AsyncRead for Breaking {
    fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.data.position() >= this.cut {
            return Poll::Ready(Err(std::io::Error::other("connection reset by peer")));
        }
        let left = (this.cut - this.data.position()) as usize;
        let mut tmp = vec![0u8; buf.remaining().min(left).min(4096)];
        let n = std::io::Read::read(&mut this.data, &mut tmp).unwrap();
        buf.put_slice(&tmp[..n]);
        Poll::Ready(Ok(()))
    }
}

/// A tone source whose first `breaks` opens cut off at 40 % of the bytes.
/// Like most sources it ignores `start_ms` (the decoder skips).
struct Flaky {
    seconds: f64,
    breaks: AtomicU32,
    live: bool,
    opens: Mutex<Vec<i64>>,
}

#[async_trait]
impl Opener for Flaky {
    async fn open(&self, _: &Track, start_ms: i64) -> Result<Stream, Failure> {
        self.opens.lock().push(start_ms);
        let data = tone(self.seconds, 440.0);
        let breaking = self
            .breaks
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |b| b.checked_sub(1))
            .is_ok();
        let cut = if breaking { data.len() as u64 * 2 / 5 } else { u64::MAX };
        Ok(Stream {
            body: Box::new(Breaking {
                data: std::io::Cursor::new(data),
                cut,
            }),
            format: Format::Pcm48,
            start_sample: 0,
            live: self.live,
        })
    }
}

/// Play one track through `flaky` and collect everything it sent.
async fn play_all(flaky: Arc<Flaky>) -> (Vec<i16>, Recorder) {
    let rec = Recorder::default();
    let mut cfg = config(&[], &rec);
    cfg.opener = flaky;
    let p = Player::new(cfg);
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    let mut out = SILENCE;
    let mut pcm = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rec.has(EventType::TrackEndEvent) {
        assert!(Instant::now() < deadline, "the track never ended");
        match p.next_frame(&mut out) {
            Some(_) => pcm.extend_from_slice(&out),
            None => sleep(Duration::from_millis(1)).await,
        }
    }
    p.destroy();
    (pcm, rec)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_stream_resumes_on_the_exact_sample() {
    let flaky = Arc::new(Flaky {
        seconds: 1.0,
        breaks: AtomicU32::new(1),
        live: false,
        opens: Mutex::new(vec![]),
    });
    let (pcm, rec) = play_all(flaky.clone()).await;
    assert!(!rec.has(EventType::TrackExceptionEvent), "the break was reported");
    let opens = flaky.opens.lock().clone();
    assert_eq!(opens.len(), 2, "{opens:?}");
    assert!(opens[1] > 0, "reopened from the start: {opens:?}");
    // Same output as a stream that never broke: no gap and no repeat at the
    // seam.
    let (unbroken, _) = play_all(Arc::new(Flaky {
        seconds: 1.0,
        breaks: AtomicU32::new(0),
        live: false,
        opens: Mutex::new(vec![]),
    }))
    .await;
    assert_eq!(pcm.len(), unbroken.len(), "samples were lost or repeated");
    let first_diff = pcm.iter().zip(&unbroken).position(|(a, b)| a != b);
    assert_eq!(first_diff, None, "the audio differs at the seam");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_that_keeps_breaking_fails_after_three_reopens() {
    let flaky = Arc::new(Flaky {
        seconds: 1.0,
        breaks: AtomicU32::new(u32::MAX),
        live: false,
        opens: Mutex::new(vec![]),
    });
    let (_, rec) = play_all(flaky.clone()).await;
    let e = rec.wait(EventType::TrackExceptionEvent).await;
    assert_eq!(e.exception.unwrap().message, "stream interrupted");
    assert_eq!(flaky.opens.lock().len(), 4, "one open and three reopens");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_stream_is_not_reopened() {
    let flaky = Arc::new(Flaky {
        seconds: 1.0,
        breaks: AtomicU32::new(1),
        live: true,
        opens: Mutex::new(vec![]),
    });
    let (_, rec) = play_all(flaky.clone()).await;
    assert!(rec.has(EventType::TrackExceptionEvent));
    assert_eq!(flaky.opens.lock().len(), 1);
}
