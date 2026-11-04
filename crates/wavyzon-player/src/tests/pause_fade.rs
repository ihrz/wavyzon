use wavyzon_audio::{FRAME_SAMPLES, SILENCE};
use wavyzon_plugin::EventType;

use super::*;

fn fading_player(fade: Duration) -> (Arc<Player>, Recorder) {
    let rec = Recorder::default();
    let mut cfg = config(&[("a", 5.0)], &rec);
    cfg.pause_fade = fade;
    (Player::new(cfg), rec)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_fades_out_then_stops() {
    let (p, rec) = fading_player(Duration::from_millis(100));
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    let mut out = SILENCE;
    frame(&p, &mut out).await;
    let full = peak(&out);
    p.pause(true);
    assert!(
        p.state().paused,
        "state must report paused as soon as the pause is requested"
    );
    let mut prev = full + 1;
    let mut frames = 0;
    while p.next_frame(&mut out).is_some() {
        frames += 1;
        let pk = peak(&out);
        assert!(pk < prev, "fade frame {frames} peak {pk} did not fall below {prev}");
        prev = pk;
        assert!(frames <= 10, "fade-out never finished");
    }
    for s in &out[FRAME_SAMPLES - 4..] {
        assert!(
            (*s as i32).abs() <= full / 100,
            "fade-out ended on sample {s}, want silence"
        );
    }
    assert_eq!(frames, 5, "fade-out took {frames} frames, want 5");
    assert!(
        p.next_frame(&mut out).is_none(),
        "a paused player kept producing frames after its fade-out"
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_fades_in() {
    let (p, rec) = fading_player(Duration::from_millis(100));
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    let mut out = SILENCE;
    frame(&p, &mut out).await;
    let full = peak(&out);
    p.pause(true);
    for _ in 0..10 {
        p.next_frame(&mut out);
    }
    p.pause(false);
    frame(&p, &mut out).await;
    assert!(
        peak(&out) < full / 2,
        "first frame after resume peaked at {} of {full}, want a fade-in",
        peak(&out)
    );
    for _ in 0..5 {
        frame(&p, &mut out).await;
    }
    assert!(
        peak(&out) >= full * 9 / 10,
        "after the fade-in the peak is {} of {full}",
        peak(&out)
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_during_fade_out_reverses_it() {
    let (p, rec) = fading_player(Duration::from_millis(200));
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    let mut out = SILENCE;
    frame(&p, &mut out).await;
    p.pause(true);
    p.next_frame(&mut out);
    p.next_frame(&mut out);
    p.pause(false);
    let low = p.inner.lock().env;
    assert!(low > 0.0 && low < 1.0, "expected to be mid fade-out, envelope {low}");
    frame(&p, &mut out).await;
    let env = p.inner.lock().env;
    assert!(
        env > low,
        "resume mid fade-out did not turn the fade around: {env} after {low}"
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_fade_configured_pauses_immediately() {
    let (p, rec) = fading_player(Duration::ZERO);
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    let mut out = SILENCE;
    frame(&p, &mut out).await;
    p.pause(true);
    assert!(
        p.next_frame(&mut out).is_none(),
        "without a pause fade a pause must stop on the next frame"
    );
    p.destroy();
}
