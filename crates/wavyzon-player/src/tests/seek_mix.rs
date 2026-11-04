use wavyzon_audio::SILENCE;
use wavyzon_plugin::EventType;

use super::*;
use crate::CrossfadeRequest;

fn linear(ms: i64, at: Option<i64>) -> CrossfadeRequest {
    CrossfadeRequest {
        duration_ms: ms,
        curve: "LINEAR".into(),
        at_position_ms: at,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seek_during_armed_mix_drops_the_mix_and_seeks() {
    let (p, rec) = new_test_player(&[("a", 6.0), ("b", 6.0)]);
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    p.load_deck_b(track("b"), 0).await.unwrap();
    p.start_crossfade(linear(200, Some(4000))).unwrap();
    p.seek(1000).expect("seek while armed should succeed");
    rec.wait(EventType::MixDroppedEvent).await;
    let mut out = SILENCE;
    let info = frame(&p, &mut out).await;
    assert!(
        info.position_ms >= 990.0 && info.position_ms <= 1100.0,
        "position after seek {}, want about 1000",
        info.position_ms
    );
    let s = p.state();
    assert!(
        s.track.as_ref().is_some_and(|t| t.identifier == "a") && !s.mixing,
        "state after dropping the mix {s:?}"
    );
    rec.wait(EventType::PositionJump).await;
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seek_during_running_fade_lands_on_the_incoming_track() {
    let (p, rec) = new_test_player(&[("a", 6.0), ("b", 6.0)]);
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    p.load_deck_b(track("b"), 0).await.unwrap();
    p.start_crossfade(linear(3000, None)).unwrap();
    let mut out = SILENCE;
    for _ in 0..10 {
        frame(&p, &mut out).await;
    }
    rec.wait(EventType::MixStartedEvent).await;
    p.seek(2500).expect("seek while fading should succeed");
    rec.wait(EventType::MixPromotedEvent).await;
    let info = frame(&p, &mut out).await;
    let s = p.state();
    assert!(
        s.track.as_ref().is_some_and(|t| t.identifier == "b") && !s.mixing,
        "state after seek {s:?}"
    );
    assert!(
        info.position_ms >= 2490.0 && info.position_ms <= 2600.0,
        "incoming track position {}",
        info.position_ms
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_fade_promotes_the_running_incoming_deck() {
    let (p, rec) = new_test_player(&[("a", 6.0), ("b", 6.0), ("c", 6.0)]);
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    p.load_deck_b(track("b"), 0).await.unwrap();
    p.start_crossfade(linear(3000, None)).unwrap();
    let mut out = SILENCE;
    for _ in 0..5 {
        frame(&p, &mut out).await;
    }
    p.load_deck_b(track("c"), 0).await.unwrap();
    p.start_crossfade(linear(3000, None)).unwrap();
    rec.wait(EventType::MixPromotedEvent).await;
    let s = p.state();
    assert!(
        s.track.as_ref().is_some_and(|t| t.identifier == "b") && s.mixing,
        "state after second fade {s:?}"
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_pending_reports_the_dropped_mix() {
    let (p, rec) = new_test_player(&[("a", 6.0), ("b", 6.0)]);
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    p.load_deck_b(track("b"), 0).await.unwrap();
    p.cancel_pending();
    let e = rec.wait(EventType::MixDroppedEvent).await;
    assert_eq!(e.track.map(|t| t.identifier), Some("b".to_string()));
    p.destroy();
}
