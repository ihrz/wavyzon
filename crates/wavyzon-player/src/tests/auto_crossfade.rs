//! Automatic crossfade into the gapless next track (`PlayerConfig::crossfade`).

use std::time::Duration;

use tokio::time::Instant;
use wavyzon_audio::SILENCE;
use wavyzon_plugin::{EndReason, EventType, Track};

use super::*;

/// A test track with a known duration (the auto crossfade needs one).
fn timed(id: &str, ms: i64) -> Track {
    Track {
        duration_ms: ms,
        ..track(id)
    }
}

fn player(lengths: &[(&str, f64)], crossfade_ms: u64) -> (Arc<Player>, Recorder) {
    let rec = Recorder::default();
    let mut cfg = config(lengths, &rec);
    cfg.crossfade = Duration::from_millis(crossfade_ms);
    (Player::new(cfg), rec)
}

/// Play `a`, preload `b` as the next track, then pull frames until `b` is on
/// deck A. Returns `b`'s instance and whether any frame was a mix.
async fn run_into_next(p: &Player, rec: &Recorder, a: Track, b: Track) -> (u64, bool) {
    p.play(a, 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    let b_inst = p.set_next(b).await.unwrap();
    sleep(Duration::from_millis(150)).await;
    let mut out = SILENCE;
    let mut mixed = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match p.next_frame(&mut out) {
            None => tokio::task::yield_now().await,
            Some(info) if info.instance == b_inst => return (b_inst, mixed),
            Some(info) => mixed |= info.mixing,
        }
    }
    panic!("never reached the next track");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn next_track_fades_in_before_the_end() {
    let (p, rec) = player(&[("a", 1.0), ("b", 3.0)], 300);
    let (b_inst, mixed) = run_into_next(&p, &rec, timed("a", 1000), timed("b", 3000)).await;
    assert!(mixed, "no crossfaded frame before the next track took over");
    let started = rec.wait(EventType::MixStartedEvent).await;
    assert_eq!(started.duration_ms, 300);
    assert_eq!(started.into.as_ref().map(|t| t.identifier.as_str()), Some("b"));
    let end = rec.wait(EventType::TrackEndEvent).await;
    assert_eq!(end.track.as_ref().unwrap().identifier, "a");
    assert_eq!(
        end.reason,
        Some(EndReason::Finished),
        "the client must advance its queue"
    );
    sleep(Duration::from_millis(20)).await;
    assert_eq!(
        rec.count(EventType::TrackStartEvent),
        1,
        "next track announced before the client adopted it"
    );
    assert_eq!(
        p.play(timed("b", 3000), 0, false),
        b_inst,
        "play of the next track restarted it"
    );
    assert_eq!(rec.wait_count(EventType::TrackStartEvent, 2).await, 2);
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_tracks_stay_gapless() {
    let (p, rec) = player(&[("a", 0.5), ("b", 3.0)], 300);
    let (_, mixed) = run_into_next(&p, &rec, timed("a", 500), timed("b", 3000)).await;
    assert!(!mixed, "a track shorter than two fades was crossfaded");
    assert!(!rec.has(EventType::MixStartedEvent));
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_duration_the_swap_stays_gapless() {
    let (p, rec) = player(&[("a", 0.8), ("b", 3.0)], 300);
    let (_, mixed) = run_into_next(&p, &rec, track("a"), timed("b", 3000)).await;
    assert!(!mixed, "a track of unknown length was crossfaded");
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seek_during_the_fade_stays_on_the_current_track() {
    let (p, rec) = player(&[("a", 2.0), ("b", 3.0)], 500);
    p.play(timed("a", 2000), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    p.set_next(timed("b", 3000)).await.unwrap();
    sleep(Duration::from_millis(150)).await;
    let mut out = SILENCE;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rec.has(EventType::MixStartedEvent) {
        assert!(Instant::now() < deadline, "the crossfade never started");
        if p.next_frame(&mut out).is_none() {
            tokio::task::yield_now().await;
        }
    }
    p.seek(200).unwrap();
    rec.wait(EventType::MixDroppedEvent).await;
    let st = p.state();
    assert_eq!(st.track.map(|t| t.identifier), Some("a".to_string()));
    assert!(!rec.has(EventType::TrackEndEvent), "the seek ended the current track");
    p.destroy();
}
