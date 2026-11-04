use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use wavyzon_audio::dsp::duck_gain;
use wavyzon_audio::{FRAME_SAMPLES, SILENCE, frames_for};
use wavyzon_plugin::EventType;

use super::*;
use crate::host::{
    DEFAULT_DUCK_DB, DEFAULT_LEAD_MS, DEFAULT_RAMP_MS, MAX_HOST_FADE_MS, VOICE_PEAK_CEILING, VoiceClip, decode_voice,
    level,
};
use crate::{HostRequest, MAX_VOICE_BYTES, PlayerError};

fn constant_clip(ms: i64, value: i16) -> VoiceClip {
    VoiceClip {
        frames: vec![[value; FRAME_SAMPLES]; frames_for(ms) as usize],
    }
}

async fn hosted_player(voice_ms: i64) -> (Arc<Player>, Recorder) {
    let (p, rec) = new_test_player(&[("a", 8.0), ("b", 8.0)]);
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    p.load_deck_b(track("b"), 2000).await.unwrap();
    let req = HostRequest {
        at_position_ms: Some(200),
        out_fade_ms: 300,
        lead_ms: 400,
        ramp_ms: 200,
        duck_db: 9.0,
        ..Default::default()
    };
    p.arm_host(&req, constant_clip(voice_ms, 6000)).unwrap();
    (p, rec)
}

async fn until_host_started(p: &Player, rec: &Recorder, out: &mut Frame) {
    while !rec.has(EventType::HostStartedEvent) {
        frame(p, out).await;
        sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_holds_the_incoming_deck_until_its_window_opens() {
    let (p, rec) = hosted_player(1000).await;
    let mut out = SILENCE;
    until_host_started(&p, &rec, &mut out).await;
    for _ in 0..20 {
        frame(&p, &mut out).await;
    }
    let s = p.state();
    assert!(
        s.mixing && s.hosting && s.mix_into_ms == 2000,
        "while the dj talks deck b must not move: {s:?}"
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_plays_the_voice_over_silence_once_the_outgoing_track_has_faded() {
    let (p, rec) = hosted_player(1000).await;
    let mut out = SILENCE;
    until_host_started(&p, &rec, &mut out).await;
    for _ in 0..20 {
        frame(&p, &mut out).await;
    }
    assert!(
        out[0] >= 1000 && out[0] == out[FRAME_SAMPLES - 1],
        "only the levelled voice should sound, got {}…{}",
        out[0],
        out[FRAME_SAMPLES - 1]
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_promotes_the_incoming_track_from_where_it_opened() {
    let (p, rec) = hosted_player(1000).await;
    let mut out = SILENCE;
    for _ in 0..120 {
        frame(&p, &mut out).await;
    }
    let ended = rec.wait(EventType::HostEndedEvent).await;
    assert_eq!(
        ended.duration_ms, 1000,
        "host ended after {}ms, want the voice length",
        ended.duration_ms
    );
    let promoted = rec.wait(EventType::MixPromotedEvent).await;
    assert!(
        promoted.into_ms >= 2000 && promoted.into_ms <= 2000 + 400 + 200 + 100,
        "deck b promoted at {}ms, want just past its start",
        promoted.into_ms
    );
    let s = p.state();
    assert!(
        s.track.as_ref().is_some_and(|t| t.identifier == "b") && !s.mixing && !s.hosting,
        "state after the dj handed over {s:?}"
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_without_deck_b_is_refused() {
    let (p, _) = new_test_player(&[("a", 5.0)]);
    assert_eq!(
        p.arm_host(&HostRequest::default(), constant_clip(500, 1)),
        Err(PlayerError::NoDeckB)
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_during_the_voice_drops_the_host() {
    let (p, rec) = hosted_player(2000).await;
    let mut out = SILENCE;
    until_host_started(&p, &rec, &mut out).await;
    p.stop();
    let s = p.state();
    assert!(!s.mixing && !s.hosting, "a stop must clear the dj, got {s:?}");
    let ended = rec.wait(EventType::HostEndedEvent).await;
    assert!(
        ended.duration_ms < 2000,
        "a dropped dj must report how long it actually spoke, got {}ms",
        ended.duration_ms
    );
    p.destroy();
}

#[test]
fn host_shape_uses_defaults_and_caps() {
    let s = HostRequest {
        out_fade_ms: 60_000,
        ..Default::default()
    }
    .shape(5000);
    assert!(
        s.out_fade_ms == MAX_HOST_FADE_MS
            && s.lead_ms == DEFAULT_LEAD_MS
            && s.ramp_ms == DEFAULT_RAMP_MS
            && s.voice_ms == 5000,
        "shape {s:?}"
    );
    assert_eq!(s.duck, duck_gain(DEFAULT_DUCK_DB));
}

#[test]
fn decode_voice_rejects_bad_input() {
    assert!(decode_voice("%%%not base64").is_err());
    let big = STANDARD.encode(vec![0u8; MAX_VOICE_BYTES + 1]);
    assert!(matches!(decode_voice(&big), Err(PlayerError::VoiceTooLarge)));
    assert!(
        decode_voice(&STANDARD.encode("x".repeat(200))).is_err(),
        "bytes that are not mp3 must be rejected"
    );
}

#[test]
fn level_brings_quiet_speech_up_without_clipping() {
    let mut clip = constant_clip(1000, 500);
    level(&mut clip.frames);
    let v = clip.frames[0][0] as f64;
    assert!(
        v > 500.0 && v <= VOICE_PEAK_CEILING * 32768.0 + 1.0,
        "levelled sample {v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_open_talks_first_then_starts_the_first_song() {
    let (p, rec) = new_test_player(&[("b", 8.0)]);
    p.load_deck_b(track("b"), 1500).await.unwrap();
    p.arm_host(
        &HostRequest {
            lead_ms: 400,
            ramp_ms: 200,
            ..Default::default()
        },
        constant_clip(1000, 6000),
    )
    .expect("a cold open with nothing playing must be accepted");
    let mut out = SILENCE;
    frame(&p, &mut out).await;
    rec.wait(EventType::HostStartedEvent).await;
    assert!(out[0] >= 1000, "the voice must be heard straight away, got {}", out[0]);
    let s = p.state();
    assert!(
        s.hosting && s.mix_into_ms == 1500,
        "while the dj opens deck b waits at its cue: {s:?}"
    );
    for _ in 0..80 {
        frame(&p, &mut out).await;
    }
    let start = rec.wait(EventType::TrackStartEvent).await;
    assert_eq!(start.track.map(|t| t.identifier), Some("b".to_string()));
    assert!(
        !rec.has(EventType::MixPromotedEvent) && !rec.has(EventType::TrackEndEvent),
        "a cold open has no outgoing track"
    );
    let s = p.state();
    assert!(
        s.track.as_ref().is_some_and(|t| t.identifier == "b") && !s.hosting && s.position_ms >= 1500,
        "after the opener the first song plays from its cue: {s:?}"
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_open_without_a_cued_song_is_refused() {
    let (p, _) = new_test_player(&[]);
    assert_eq!(
        p.arm_host(&HostRequest::default(), constant_clip(500, 1)),
        Err(PlayerError::NoDeckB)
    );
    p.destroy();
}
