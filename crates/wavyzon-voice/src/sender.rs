//! The 20 ms send loop, on a dedicated OS thread.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};
use wavyzon_audio::codec::{Encoder, MAX_OPUS_BYTES, OPUS_SILENCE};
use wavyzon_audio::{FRAME_MS, FRAME_SAMPLES_PER_CHANNEL, Frame, SILENCE};

use crate::Error;
use crate::conn::Inner;
use crate::protocol::{Opcode, SPEAKING_MICROPHONE, Speaking};
use crate::transport::{Rtp, Sealer};

pub(crate) const FRAME_INTERVAL: Duration = Duration::from_millis(FRAME_MS as u64);
/// The loop reports a stall when it is more than this late.
pub(crate) const STALL_THRESHOLD: Duration = Duration::from_millis(3 * FRAME_MS as u64);
/// Silence frames sent after the source runs dry, before speaking goes off.
pub(crate) const SILENCE_TRAILER: u32 = 5;
const DAVE_WAIT_LOG_EVERY: u32 = 50;
/// 750 ticks of 20 ms: a 15 s DAVE hold restarts the session.
pub(crate) const DAVE_HOLD_LIMIT: u32 = 750;

/// Handle to a running sender thread. Stopping joins the thread, so the
/// encoder, socket and sealer it owns are gone when [`SenderHandle::stop`] returns.
pub(crate) struct SenderHandle {
    stop: Arc<AtomicBool>,
    pub bitrate_changed: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl SenderHandle {
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.thread().unpark();
        if self.thread.join().is_err() {
            warn!("voice sender thread panicked");
        }
    }
}

/// Everything one sender owns.
pub(crate) struct SenderParts {
    pub sealer: Sealer,
    pub encoder: Box<dyn Encoder>,
    pub socket: Arc<UdpSocket>,
    pub remote: SocketAddr,
    pub ssrc: u32,
}

pub(crate) fn spawn(inner: Arc<Inner>, parts: SenderParts, span: tracing::Span) -> Result<SenderHandle, Error> {
    let stop = Arc::new(AtomicBool::new(false));
    let bitrate_changed = Arc::new(AtomicBool::new(false));
    let mut s = Sender {
        inner,
        rtp: Rtp::new(parts.ssrc),
        parts,
        stop: stop.clone(),
        bitrate_changed: bitrate_changed.clone(),
        speaking: false,
        silence_left: 0,
        opus: vec![0u8; MAX_OPUS_BYTES],
        dave_buf: Vec::with_capacity(MAX_OPUS_BYTES * 2),
        frame: Box::new(SILENCE),
    };
    let thread = thread::Builder::new().name("wavyzon-voice-tx".into()).spawn(move || {
        let _e = span.enter();
        s.run();
    })?;
    Ok(SenderHandle {
        stop,
        bitrate_changed,
        thread,
    })
}

struct Sender {
    inner: Arc<Inner>,
    parts: SenderParts,
    rtp: Rtp,
    stop: Arc<AtomicBool>,
    bitrate_changed: Arc<AtomicBool>,
    speaking: bool,
    silence_left: u32,
    opus: Vec<u8>,
    dave_buf: Vec<u8>,
    frame: Box<Frame>,
}

impl Sender {
    fn run(&mut self) {
        let mut next = Instant::now();
        let mut waits = 0u32;
        while !self.stop.load(Ordering::SeqCst) {
            if self.bitrate_changed.swap(false, Ordering::SeqCst)
                && let Err(e) = self.parts.encoder.set_bitrate(self.inner.bitrate())
            {
                warn!(error = %e, "bitrate change failed");
            }
            if self.inner.hold_audio() {
                waits += 1;
                if waits % DAVE_WAIT_LOG_EVERY == 1 {
                    info!("holding audio until the DAVE epoch is ready");
                }
                if waits >= DAVE_HOLD_LIMIT {
                    waits = 0;
                    self.inner.restart_fresh("DAVE epoch never became ready");
                }
                next = self.sleep(next);
                continue;
            }
            waits = 0;
            self.tick();
            next = self.sleep(next);
        }
    }

    fn tick(&mut self) {
        if let Some(info) = self.inner.source.next_frame(&mut self.frame) {
            if !self.speaking {
                self.set_speaking(true);
            }
            let n = match self.parts.encoder.encode(&self.frame[..], &mut self.opus) {
                Ok(n) if n > 0 => n,
                Ok(_) => {
                    warn!("opus encode produced nothing, frame dropped");
                    return;
                }
                Err(e) => {
                    warn!(error = %e, "opus encode failed, frame dropped");
                    return;
                }
            };
            let opus = std::mem::take(&mut self.opus);
            let res = self.write(&opus[..n]);
            self.opus = opus;
            if let Err(e) = res {
                warn!(error = %e, "voice packet dropped");
                return;
            }
            self.inner.source.sent(info, Instant::now());
            self.silence_left = SILENCE_TRAILER;
            return;
        }
        if !self.speaking {
            return;
        }
        if self.silence_left > 0 {
            self.silence_left -= 1;
            if let Err(e) = self.write(&OPUS_SILENCE) {
                debug!(error = %e, "silence frame dropped");
            }
            return;
        }
        self.set_speaking(false);
    }

    fn set_speaking(&mut self, on: bool) {
        let speaking = if on { SPEAKING_MICROPHONE } else { 0 };
        let msg = Speaking {
            speaking,
            delay: 0,
            ssrc: self.parts.ssrc,
        };
        if let Err(e) = self.inner.send_json(Opcode::SPEAKING, &msg) {
            warn!(error = %e, "speaking update failed");
            return;
        }
        self.speaking = on;
    }

    /// DAVE-encrypt (when the call is E2EE), transport-seal and send one Opus frame.
    fn write(&mut self, opus: &[u8]) -> Result<(), Error> {
        let mut e2ee = false;
        if self.inner.dave_version() > 0
            && let Some(d) = self.inner.dave.lock().as_mut()
        {
            d.encrypt_opus(opus, &mut self.dave_buf)?;
            e2ee = true;
        }
        let payload = if e2ee { &self.dave_buf[..] } else { opus };
        let header = self.rtp.next(FRAME_SAMPLES_PER_CHANNEL as u32);
        let packet = self.parts.sealer.seal(&header, payload);
        self.parts.socket.send_to(packet, self.parts.remote)?;
        Ok(())
    }

    /// Wait for the next 20 ms slot. Slots are computed from the previous
    /// slot, not from "now", so the cadence does not drift; when the loop is
    /// more than [`STALL_THRESHOLD`] late it reports a stall and re-anchors.
    fn sleep(&self, next: Instant) -> Instant {
        let next = next + FRAME_INTERVAL;
        let now = Instant::now();
        let late = now.saturating_duration_since(next);
        if late > STALL_THRESHOLD {
            if self.speaking {
                self.inner.source.stall(late);
                warn!(late_ms = late.as_millis() as u64, "send loop fell behind");
            }
            return now;
        }
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return next;
            }
            let now = Instant::now();
            if now >= next {
                return next;
            }
            thread::park_timeout(next - now);
        }
    }
}
