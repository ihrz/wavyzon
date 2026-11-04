use std::time::{Duration, Instant};

use wavyzon_audio::SILENCE;
use wavyzon_plugin::{EndReason, EventType};

use super::*;
use crate::player::unix_ms;
use crate::{CrossfadeRequest, Filters, PlayerError, Timescale};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn position_advances_twenty_ms_per_frame() {
    let (p, rec) = new_test_player(&[("a", 5.0)]);
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    let mut out = SILENCE;
    let first = frame(&p, &mut out).await;
    let mut last = first;
    for _ in 0..50 {
        last = frame(&p, &mut out).await;
    }
    let got = last.position_ms - first.position_ms;
    assert!((got - 1000.0).abs() <= 1.0, "50 frames advanced {got}ms, want 1000");
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_offset_is_exact() {
    let (p, rec) = new_test_player(&[("a", 5.0)]);
    p.play(track("a"), 1500, false);
    rec.wait(EventType::TrackStartEvent).await;
    let mut out = SILENCE;
    let info = frame(&p, &mut out).await;
    assert!(
        (info.position_ms - 1500.0).abs() <= 1.0,
        "first frame at {}, want 1500",
        info.position_ms
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timescale_moves_position_faster() {
    let (p, rec) = new_test_player(&[("a", 8.0)]);
    p.set_filters(Filters {
        timescale: Some(Timescale {
            speed: 1.5,
            pitch: 1.0,
            rate: 1.0,
        }),
        ..Default::default()
    })
    .unwrap();
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    let mut out = SILENCE;
    let first = frame(&p, &mut out).await;
    let mut last = first;
    for _ in 0..100 {
        last = frame(&p, &mut out).await;
    }
    let ratio = (last.position_ms - first.position_ms) / 2000.0;
    assert!(
        (ratio - 1.5).abs() <= 0.05,
        "position advanced {ratio:.3}x real time, want 1.5"
    );
    assert_eq!(p.state().rate, 1.5);
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn track_ends_finished() {
    let (p, rec) = new_test_player(&[("a", 0.2)]);
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    let mut out = SILENCE;
    for _ in 0..200 {
        if rec.has(EventType::TrackEndEvent) {
            break;
        }
        p.next_frame(&mut out);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        rec.wait(EventType::TrackEndEvent).await.reason,
        Some(EndReason::Finished)
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_failure_reports_exception() {
    let (p, rec) = new_test_player(&[]);
    p.play(track("missing"), 0, false);
    let e = rec.wait(EventType::TrackExceptionEvent).await;
    assert_eq!(
        e.exception.as_ref().map(|f| f.cause.as_str()),
        Some("boom"),
        "exception {:?}",
        e.exception
    );
    assert_eq!(
        rec.wait(EventType::TrackEndEvent).await.reason,
        Some(EndReason::LoadFailed)
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audible_fires_once_at_first_send() {
    let (p, rec) = new_test_player(&[("a", 5.0)]);
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    assert!(
        !rec.has(EventType::TrackAudibleEvent),
        "audible before any frame was sent"
    );
    let mut out = SILENCE;
    let now = Instant::now();
    let info = frame(&p, &mut out).await;
    p.sent(info, now);
    let info = frame(&p, &mut out).await;
    p.sent(info, now + Duration::from_millis(20));
    let e = rec.wait(EventType::TrackAudibleEvent).await;
    assert!(
        (e.sent_at - unix_ms(now)).abs() <= 2 && e.position_ms == 0,
        "audible {e:?}"
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(rec.count(EventType::TrackAudibleEvent), 1);
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_audio_without_voice_means_no_position_drift() {
    let (p, rec) = new_test_player(&[("a", 5.0)]);
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let s = p.state();
    assert!(
        s.position_ms == 0 && !s.sounding,
        "position moved with no sender: {s:?}"
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn armed_crossfade_promotes_at_cue() {
    let (p, rec) = new_test_player(&[("a", 6.0), ("b", 6.0)]);
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    p.load_deck_b(track("b"), 2000).await.unwrap();
    p.start_crossfade(CrossfadeRequest {
        duration_ms: 200,
        curve: "LINEAR".into(),
        at_position_ms: Some(400),
        ..Default::default()
    })
    .unwrap();
    let mut out = SILENCE;
    for _ in 0..60 {
        frame(&p, &mut out).await;
    }
    let started = rec.wait(EventType::MixStartedEvent).await;
    assert_eq!(started.into.as_ref().map(|t| t.identifier.as_str()), Some("b"));
    let promoted = rec.wait(EventType::MixPromotedEvent).await;
    assert!(
        promoted.into_ms >= 2000,
        "deck b position {}, want >= 2000",
        promoted.into_ms
    );
    let s = p.state();
    assert!(
        s.track.as_ref().is_some_and(|t| t.identifier == "b") && !s.mixing,
        "state after promote {s:?}"
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crossfade_without_deck_b_fails() {
    let (p, _) = new_test_player(&[("a", 5.0)]);
    let err = p.start_crossfade(CrossfadeRequest {
        duration_ms: 100,
        ..Default::default()
    });
    assert_eq!(err, Err(PlayerError::NoDeckB));
    let err = p.start_crossfade(CrossfadeRequest {
        duration_ms: 100,
        curve: "bounce".into(),
        ..Default::default()
    });
    assert!(matches!(err, Err(PlayerError::InvalidCurve(_))));
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_carry_guild_and_instance_and_state_serializes() {
    let (p, rec) = new_test_player(&[("a", 5.0)]);
    let inst = p.play(track("a"), 0, false);
    let e = rec.wait(EventType::TrackStartEvent).await;
    assert_eq!((e.guild_id.as_str(), e.instance), ("1", inst));
    assert!(!e.encoded.is_empty());
    let v = serde_json::to_value(p.state()).unwrap();
    for key in [
        "anchorMs",
        "anchorSentAt",
        "anchorAudibleAt",
        "rate",
        "sounding",
        "deckBLoaded",
        "hosting",
        "filters",
    ] {
        assert!(v.get(key).is_some(), "state is missing {key}: {v}");
    }
    p.stop();
    assert_eq!(
        rec.wait(EventType::TrackEndEvent).await.reason,
        Some(EndReason::Stopped)
    );
    p.destroy();
}
