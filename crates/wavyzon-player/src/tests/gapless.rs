use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio::time::Instant;
use wavyzon_audio::{Frame, SILENCE};
use wavyzon_plugin::{EndReason, EventType, Failure, Track};

use super::*;
use crate::{PlayerError, Resolver};

async fn play_into_next(p: &Player, rec: &Recorder) -> (u64, u64, usize, Frame) {
    let a_inst = p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    let b_inst = p.set_next(track("b")).await.unwrap();
    sleep(Duration::from_millis(150)).await;
    let mut out = SILENCE;
    frame(p, &mut out).await;
    let mut gaps = 0;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match p.next_frame(&mut out) {
            None => {
                gaps += 1;
                tokio::task::yield_now().await;
            }
            Some(info) if info.instance == b_inst => return (a_inst, b_inst, gaps, out),
            Some(_) => {}
        }
    }
    panic!("never reached the next track");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn next_track_starts_on_the_very_next_frame() {
    let (p, rec) = new_test_player(&[("a", 0.5), ("b", 3.0)]);
    let (_, _, gaps, _) = play_into_next(&p, &rec).await;
    assert_eq!(gaps, 0, "{gaps} empty frames between tracks");
    let e = rec.wait(EventType::TrackEndEvent).await;
    assert!(
        e.reason == Some(EndReason::Finished) && e.track.as_ref().unwrap().identifier == "a",
        "end {e:?}"
    );
    sleep(Duration::from_millis(20)).await;
    assert_eq!(
        rec.count(EventType::TrackStartEvent),
        1,
        "next track announced before the client adopted it"
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn play_of_the_next_track_adopts_it_without_restarting() {
    let (p, rec) = new_test_player(&[("a", 0.5), ("b", 3.0)]);
    let (_, b_inst, _, mut out) = play_into_next(&p, &rec).await;
    for _ in 0..10 {
        frame(&p, &mut out).await;
    }
    let before = p.state().position_ms;
    assert_eq!(p.play(track("b"), 0, false), b_inst);
    let got = p.state().position_ms;
    assert!(got >= before, "adopting restarted the track: {before} -> {got}");
    assert_eq!(
        rec.wait_count(EventType::TrackStartEvent, 2).await,
        2,
        "adoption did not announce the track"
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn play_of_another_track_replaces_the_gapless_one() {
    let (p, rec) = new_test_player(&[("a", 0.5), ("b", 3.0), ("c", 3.0)]);
    let (_, b_inst, _, _) = play_into_next(&p, &rec).await;
    assert_ne!(p.play(track("c"), 0, false), b_inst, "a different track was adopted");
    assert_eq!(p.state().track.map(|t| t.identifier), Some("c".to_string()));
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unadopted_next_track_is_announced_when_the_window_closes() {
    let rec = Recorder::default();
    let mut cfg = config(&[("a", 0.5), ("b", 3.0)], &rec);
    cfg.adopt_window = Duration::from_millis(50);
    let p = Player::new(cfg);
    let (_, _, _, mut out) = play_into_next(&p, &rec).await;
    sleep(Duration::from_millis(80)).await;
    frame(&p, &mut out).await;
    assert_eq!(
        rec.wait_count(EventType::TrackStartEvent, 2).await,
        2,
        "next track never announced"
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audible_of_the_gapless_track_follows_its_start() {
    let (p, rec) = new_test_player(&[("a", 0.5), ("b", 3.0)]);
    let (_, b_inst, _, mut out) = play_into_next(&p, &rec).await;
    if let Some(info) = p.next_frame(&mut out) {
        p.sent(info, std::time::Instant::now());
    }
    sleep(Duration::from_millis(20)).await;
    assert!(
        rec.count(EventType::TrackAudibleEvent) <= 1,
        "audible sent for a track the client has not been told about"
    );
    p.play(track("b"), 0, false);
    rec.wait_count(EventType::TrackAudibleEvent, 1).await;
    let events = rec.events();
    let mine = || events.iter().enumerate().filter(|(_, e)| e.instance == b_inst);
    let start = mine()
        .find(|(_, e)| e.kind == EventType::TrackStartEvent)
        .map(|(i, _)| i);
    let audible = mine()
        .find(|(_, e)| e.kind == EventType::TrackAudibleEvent)
        .map(|(i, _)| i);
    assert!(
        matches!((start, audible), (Some(s), Some(a)) if a > s),
        "start at {start:?}, audible at {audible:?}"
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_next_track_is_not_swapped_in() {
    let (p, rec) = new_test_player(&[("a", 0.4)]);
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    p.set_next(track("missing")).await.unwrap();
    sleep(Duration::from_millis(100)).await;
    let mut out = SILENCE;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !rec.has(EventType::TrackEndEvent) {
        p.next_frame(&mut out);
        tokio::task::yield_now().await;
    }
    let e = rec.wait(EventType::TrackEndEvent).await;
    assert_eq!(e.track.unwrap().identifier, "a");
    assert!(p.state().track.is_none(), "a failed next track became active");
    p.destroy();
}

struct Gate(Arc<Notify>);

#[async_trait]
impl Resolver for Gate {
    async fn playable(&self, t: &Track) -> Result<Track, Failure> {
        if t.identifier == "slow" {
            self.0.notified().await;
        }
        Ok(t.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleared_next_is_not_revived_by_a_slow_preload() {
    let rec = Recorder::default();
    let gate = Arc::new(Notify::new());
    let mut cfg = config(&[("a", 5.0), ("slow", 5.0)], &rec);
    cfg.resolver = Arc::new(Gate(gate.clone()));
    let p = Player::new(cfg);
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    let p2 = p.clone();
    let task = tokio::spawn(async move { p2.set_next(track("slow")).await });
    sleep(Duration::from_millis(20)).await;
    p.clear_next();
    gate.notify_one();
    assert_eq!(task.await.unwrap(), Err(PlayerError::NextSuperseded));
    assert!(p.state().next_track.is_none(), "cleared next track came back");
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_acknowledges_only_the_track_that_took_over() {
    let (p, rec) = new_test_player(&[("a", 0.5), ("b", 3.0), ("c", 3.0)]);
    let (_, b_inst, _, _) = play_into_next(&p, &rec).await;
    assert_eq!(p.adopt(&track("c"), 0), None, "another track was adopted");
    assert_eq!(p.adopt(&track("b"), 500), None, "a start other than 0 was adopted");
    assert_eq!(p.adopt(&track("b"), 0), Some(b_inst));
    assert_eq!(
        rec.wait_count(EventType::TrackStartEvent, 2).await,
        2,
        "adoption did not announce the track"
    );
    assert_eq!(p.adopt(&track("b"), 0), None, "adopted twice");
    assert_eq!(p.state().track.map(|t| t.identifier), Some("b".to_string()));
    p.destroy();
}
