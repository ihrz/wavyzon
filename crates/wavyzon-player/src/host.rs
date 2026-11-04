//! Host transitions: a short voice clip (a DJ drop, an announcement) played
//! between two tracks. The outgoing track fades out under the start of the
//! voice, the incoming one opens ducked shortly before the voice ends and
//! rises to full level after it. With nothing playing it is a cold open.

use std::io::Cursor;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use wavyzon_audio::dsp::{FadeCurve, HostShape, Resampler, TransitionProfile, duck_gain, scale};
use wavyzon_audio::{
    CHANNELS, FRAME_MS, FRAME_SAMPLES, Frame, FrameInfo, SAMPLE_RATE, SILENCE, clip, codec, ms_for, peak,
};
use wavyzon_plugin::{EventType, Format};

use crate::events::ev;
use crate::player::{Inner, PendingMix};
use crate::{Player, PlayerError};

/// Largest accepted voice clip (decoded base64), bytes.
pub const MAX_VOICE_BYTES: usize = 512 << 10;
/// Longest accepted voice clip, ms.
pub const MAX_VOICE_MS: i64 = 25_000;
pub(crate) const DEFAULT_OUT_FADE_MS: i64 = 2500;
pub(crate) const DEFAULT_LEAD_MS: i64 = 1800;
pub(crate) const DEFAULT_RAMP_MS: i64 = 1200;
pub(crate) const DEFAULT_DUCK_DB: f64 = 9.0;
pub(crate) const MAX_HOST_FADE_MS: i64 = 8000;
const VOICE_TARGET_RMS: f64 = 0.158;
pub(crate) const VOICE_PEAK_CEILING: f64 = 0.891;

/// Body of a host request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HostRequest {
    /// Base64 (standard alphabet) MP3, at most [`MAX_VOICE_BYTES`].
    pub audio: String,
    /// Arm the transition for this deck A position; `None` starts now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_position_ms: Option<i64>,
    /// Outgoing fade under the voice (default 2500, max 8000).
    pub out_fade_ms: i64,
    /// How long before the voice ends the incoming track opens (default 1800).
    pub lead_ms: i64,
    /// Rise of the incoming track after the voice (default 1200).
    pub ramp_ms: i64,
    /// Duck of the incoming track under the voice, dB (default 9, max 30).
    pub duck_db: f64,
}

/// A decoded, levelled voice clip in 48 kHz frames.
#[derive(Debug, Clone)]
pub(crate) struct VoiceClip {
    pub frames: Vec<Frame>,
}

impl VoiceClip {
    pub(crate) fn duration_ms(&self) -> i64 {
        ms_for(self.frames.len() as i64)
    }
}

/// A running (or armed) host transition.
#[derive(Debug, Clone)]
pub(crate) struct HostRun {
    pub clip: VoiceClip,
    pub shape: HostShape,
    pub elapsed: i64,
    pub voice_ended: bool,
}

fn or_default(v: i64, fallback: i64, ceiling: i64) -> i64 {
    if v <= 0 { fallback } else { v.min(ceiling) }
}

impl HostRequest {
    /// The gain shape for a voice of `voice_ms`.
    pub(crate) fn shape(&self, voice_ms: i64) -> HostShape {
        let duck = if self.duck_db == 0.0 {
            DEFAULT_DUCK_DB
        } else {
            self.duck_db
        };
        HostShape {
            voice_ms,
            out_fade_ms: or_default(self.out_fade_ms, DEFAULT_OUT_FADE_MS, MAX_HOST_FADE_MS),
            lead_ms: or_default(self.lead_ms, DEFAULT_LEAD_MS, MAX_HOST_FADE_MS),
            ramp_ms: or_default(self.ramp_ms, DEFAULT_RAMP_MS, MAX_HOST_FADE_MS),
            duck: duck_gain(duck.abs().min(30.0)),
        }
    }
}

/// Decode, resample and level a base64 MP3 voice clip.
pub(crate) fn decode_voice(encoded: &str) -> Result<VoiceClip, PlayerError> {
    if encoded.len() / 4 * 3 > MAX_VOICE_BYTES + 3 {
        return Err(PlayerError::VoiceTooLarge);
    }
    let raw = STANDARD
        .decode(encoded.trim())
        .map_err(|e| PlayerError::VoiceInvalid(format!("not base64: {e}")))?;
    if raw.len() > MAX_VOICE_BYTES {
        return Err(PlayerError::VoiceTooLarge);
    }
    let mut pcm = codec::decode(&Format::mp3(), Box::new(Cursor::new(raw)))
        .map_err(|e| PlayerError::VoiceInvalid(format!("not mp3: {e}")))?;
    let rate = pcm.sample_rate().max(1);
    let limit = (MAX_VOICE_MS as usize / 1000 + 1) * rate as usize * CHANNELS;
    let mut samples = Vec::new();
    let mut buf = vec![0i16; 8192];
    loop {
        let n = pcm
            .read(&mut buf)
            .map_err(|e| PlayerError::VoiceInvalid(format!("decode: {e}")))?;
        if n == 0 {
            break;
        }
        samples.extend_from_slice(&buf[..n]);
        if samples.len() > limit {
            return Err(PlayerError::VoiceTooLong);
        }
    }
    let frames = to_frames(&resampled(&samples, rate));
    if frames.is_empty() {
        return Err(PlayerError::VoiceEmpty);
    }
    let mut clip = VoiceClip { frames };
    if clip.duration_ms() > MAX_VOICE_MS {
        return Err(PlayerError::VoiceTooLong);
    }
    level(&mut clip.frames);
    Ok(clip)
}

fn resampled(samples: &[i16], rate: u32) -> Vec<f64> {
    if rate == SAMPLE_RATE {
        return samples.iter().map(|&s| s as f64).collect();
    }
    let mut r = Resampler::new(rate, 1.0);
    r.write(samples);
    r.write(&[0i16; 64 * CHANNELS]);
    let mut out = vec![0.0; 2 * (r.available() + 1)];
    let n = r.read(&mut out);
    out.truncate(2 * n);
    out
}

fn to_frames(interleaved: &[f64]) -> Vec<Frame> {
    let count = interleaved.len().div_ceil(FRAME_SAMPLES);
    let mut frames = vec![SILENCE; count];
    for (i, v) in interleaved.iter().enumerate() {
        frames[i / FRAME_SAMPLES][i % FRAME_SAMPLES] = clip(*v);
    }
    frames
}

/// Bring speech to a fixed RMS without clipping (gain kept in `0.25..=4`).
pub(crate) fn level(frames: &mut [Frame]) {
    let (mut sum, mut pk, mut n) = (0.0, 0, 0usize);
    for f in frames.iter() {
        for &s in f.iter() {
            let v = s as f64 / 32768.0;
            sum += v * v;
            n += 1;
        }
        pk = pk.max(peak(f));
    }
    if n == 0 || sum == 0.0 || pk == 0 {
        return;
    }
    let gain = (VOICE_TARGET_RMS / (sum / n as f64).sqrt())
        .min(VOICE_PEAK_CEILING * 32768.0 / pk as f64)
        .clamp(0.25, 4.0);
    for f in frames.iter_mut() {
        let src = *f;
        scale(&src, gain, f);
    }
}

impl Player {
    /// Start (or arm, with `at_position_ms`) a host transition into the cued
    /// deck B. With nothing on deck A it is a cold open: the voice plays, then
    /// deck B starts. Errors: [`PlayerError::NoDeckB`],
    /// [`PlayerError::TrackLoading`] and the voice clip errors.
    pub async fn start_host(&self, req: HostRequest) -> Result<(), PlayerError> {
        let audio = req.audio.clone();
        let clip = tokio::task::spawn_blocking(move || decode_voice(&audio))
            .await
            .map_err(|e| PlayerError::VoiceInvalid(e.to_string()))?;
        match clip {
            Ok(clip) => self.arm_host(&req, clip),
            Err(e) => {
                tracing::debug!(guild = %self.cfg.guild_id, error = %e, "host transition refused");
                Err(e)
            }
        }
    }

    pub(crate) fn arm_host(&self, req: &HostRequest, clip: VoiceClip) -> Result<(), PlayerError> {
        let shape = req.shape(clip.duration_ms());
        let mut g = self.inner.lock();
        let s = &mut *g;
        if s.cued.is_none() {
            return Err(PlayerError::NoDeckB);
        }
        if s.active.is_none() && s.loading.is_some() {
            return Err(PlayerError::TrackLoading);
        }
        let Some(deck) = s.cued.take() else {
            return Err(PlayerError::NoDeckB);
        };
        let inst = s.cued_inst;
        let mut run = HostRun {
            clip,
            shape,
            elapsed: 0,
            voice_ended: false,
        };
        let total = shape.total_ms();
        if s.active.is_none() {
            run.shape.out_fade_ms = 0;
            s.host_pos_a = 0.0;
            s.paused = false;
            s.env = 1.0;
            s.fading_out = false;
        }
        if let (Some(at), Some(_)) = (req.at_position_ms, &s.active) {
            if let Some(p) = s.pending.take() {
                self.drop_deck_locked(p.deck, p.instance);
            }
            let tr = deck.track.clone();
            s.pending = Some(PendingMix {
                deck,
                instance: inst,
                duration_ms: total,
                curve: FadeCurve::Linear,
                at_ms: at,
                profile: TransitionProfile::default(),
                gain: 1.0,
                host: Some(run),
            });
            drop(g);
            let mut e = ev(EventType::MixArmedEvent, Some(&tr), inst);
            e.position_ms = at;
            e.duration_ms = total;
            self.emit(e);
            return Ok(());
        }
        self.begin_fade_locked(
            s,
            deck,
            inst,
            total,
            FadeCurve::Linear,
            TransitionProfile::default(),
            1.0,
            Some(run),
        );
        Ok(())
    }

    pub(crate) fn host_frame_locked(&self, s: &mut Inner, out: &mut Frame, vol: f64) -> Option<FrameInfo> {
        let h = s.host.as_mut()?;
        let (t0, t1) = (h.elapsed, h.elapsed + FRAME_MS);
        s.bus.reset();
        let mut pos_a = s.host_pos_a;
        let (from, to) = (h.shape.out_gain(t0), h.shape.out_gain(t1));
        if let Some(a) = s.active.as_mut()
            && (from > 0.0 || to > 0.0)
        {
            let (pos, ok) = a.read(&mut s.buf_a);
            if ok {
                pos_a = pos;
                let g = s.active_gain * a.norm * vol;
                s.bus.add(&s.buf_a, from * g, to * g);
            }
        }
        if t1 > h.shape.in_open_ms()
            && let Some(inc) = s.incoming.as_mut()
        {
            let (_, ok) = inc.read(&mut s.buf_b);
            if ok {
                let g = s.inc_gain * inc.norm * vol;
                s.bus.add(&s.buf_b, h.shape.in_gain(t0) * g, h.shape.in_gain(t1) * g);
            }
        }
        if let Some(f) = h.clip.frames.get((t0 / FRAME_MS) as usize) {
            s.bus.add(f, vol, vol);
        }
        s.bus.write(out);
        s.host_pos_a = pos_a;
        h.elapsed = t1;
        if let Some(f) = &mut s.fade {
            f.advance();
        }
        let info = FrameInfo {
            instance: s.active_inst,
            position_ms: pos_a,
            mixing: true,
        };
        let (voice_ms, total) = (h.shape.voice_ms, h.shape.total_ms());
        if !h.voice_ended && t1 >= voice_ms {
            h.voice_ended = true;
            let mut e = ev(
                EventType::HostEndedEvent,
                s.active.as_ref().map(|d| &d.track),
                s.active_inst,
            );
            e.duration_ms = voice_ms;
            self.emit(e);
        }
        if t1 >= total {
            tracing::debug!(guild = %self.cfg.guild_id, "host transition played");
            self.promote_locked(s);
        }
        Some(info)
    }
}
