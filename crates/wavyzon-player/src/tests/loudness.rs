use std::sync::atomic::{AtomicUsize, Ordering};

use wavyzon_audio::SILENCE;
use wavyzon_plugin::EventType;

use super::*;
use crate::loudness::NORM_PEAK_CEIL;
use crate::{Loudness, LoudnessLookup, UnmeasuredHook, normalization_gain};

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-3
}

#[test]
fn normalization_gain_cases() {
    let l = |lufs, peak| Loudness {
        integrated_lufs: lufs,
        peak_amplitude: peak,
    };
    let cases = [
        ("quiet track is boosted", l(-18.0, 0.0), 10f64.powf(4.0 / 20.0)),
        ("loud track is cut", l(-8.0, 0.0), 10f64.powf(-6.0 / 20.0)),
        ("boost is capped at 6 dB", l(-40.0, 0.0), 10f64.powf(6.0 / 20.0)),
        ("cut is capped at 12 dB", l(5.0, 0.0), 10f64.powf(-12.0 / 20.0)),
        (
            "boost never pushes the peak past -1 dBFS",
            l(-20.0, 0.8),
            NORM_PEAK_CEIL / 0.8,
        ),
        (
            "silence or a failed measurement is left alone",
            l(f64::NEG_INFINITY, 0.0),
            1.0,
        ),
    ];
    for (name, input, want) in cases {
        let got = normalization_gain(input, -14.0);
        assert!(near(got, want), "{name}: got {got:.4} want {want:.4}");
    }
}

fn rms(f: &Frame) -> f64 {
    (f.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / f.len() as f64).sqrt()
}

async fn level_of(lookup: LoudnessLookup, unmeasured: Option<UnmeasuredHook>) -> f64 {
    let rec = Recorder::default();
    let mut cfg = config(&[("a", 3.0)], &rec);
    cfg.loudness = Some(lookup);
    cfg.on_unmeasured = unmeasured;
    cfg.target_lufs = -14.0;
    let p = Player::new(cfg);
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    let mut out = SILENCE;
    for _ in 0..10 {
        frame(&p, &mut out).await;
    }
    p.destroy();
    rms(&out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn measured_track_is_levelled_and_unmeasured_is_queued() {
    let queued = Arc::new(AtomicUsize::new(0));
    let q = queued.clone();
    let unity = level_of(
        Arc::new(|_, _| None),
        Some(Arc::new(move |_| {
            q.fetch_add(1, Ordering::SeqCst);
        })),
    )
    .await;
    assert_eq!(
        queued.load(Ordering::SeqCst),
        1,
        "unmeasured track queued wrong number of times"
    );
    let boosted = level_of(
        Arc::new(|_, _| {
            Some(Loudness {
                integrated_lufs: -20.0,
                peak_amplitude: 0.25,
            })
        }),
        None,
    )
    .await;
    let ratio = boosted / unity;
    assert!(
        (ratio - 10f64.powf(6.0 / 20.0)).abs() <= 0.05,
        "boost ratio {ratio:.3}, want about 2.0"
    );
}
