use wavyzon_audio::FRAME_MS;

use crate::filter_ramp::{FilterRamp, WET_ROTATION};
use crate::{Band, Filters, Rotation, Timescale};

fn frames_for(ms: i64) -> i64 {
    ms / FRAME_MS
}

fn ts(speed: f64, pitch: f64) -> Filters {
    Filters {
        timescale: Some(Timescale {
            speed,
            pitch,
            rate: 1.0,
        }),
        ..Default::default()
    }
}

#[test]
fn starts_at_the_first_filters_without_gliding() {
    let mut r = FilterRamp::default();
    let f = ts(1.3, 1.3);
    r.advance(&f);
    assert!(
        r.resample == f.resample_factor() && r.tempo == f.tempo(),
        "resample {} tempo {}",
        r.resample,
        r.tempo
    );
}

#[test]
fn glides_instead_of_jumping() {
    let mut r = FilterRamp::default();
    r.advance(&Filters::default());
    let nightcore = Filters {
        equalizer: vec![Band { band: 0, gain: 0.2 }],
        ..ts(1.3, 1.3)
    };
    r.advance(&nightcore);
    assert!(
        r.resample < 1.3 && r.resample > 1.0,
        "one frame in, pitch should be part-way, got {}",
        r.resample
    );
    assert!(
        r.eq[0] < 0.2 && r.eq[0] > 0.0,
        "one frame in, the bass band should be part-way, got {}",
        r.eq[0]
    );
    for _ in 0..frames_for(1000) {
        r.advance(&nightcore);
    }
    assert!(
        r.resample == 1.3 && r.eq[0] == 0.2,
        "after a second the ramp should have settled, got {} {}",
        r.resample,
        r.eq[0]
    );
}

#[test]
fn mostly_arrives_within_half_a_second() {
    let mut r = FilterRamp::default();
    r.advance(&Filters::default());
    let target = ts(0.8, 0.8);
    for _ in 0..frames_for(500) {
        r.advance(&target);
    }
    assert!(
        (r.resample - 0.8).abs() / 0.2 <= 0.1,
        "half a second in, got {}",
        r.resample
    );
}

#[test]
fn fades_an_effect_out_with_its_last_settings() {
    let mut r = FilterRamp::default();
    let on = Filters {
        rotation: Some(Rotation { rotation_hz: 0.2 }),
        ..Default::default()
    };
    r.advance(&on);
    assert_eq!(
        r.wet[WET_ROTATION], 1.0,
        "an effect present from the start is fully wet"
    );
    r.advance(&Filters::default());
    assert!(
        r.wet[WET_ROTATION] > 0.0 && r.wet[WET_ROTATION] < 1.0,
        "turning it off should fade, got {}",
        r.wet[WET_ROTATION]
    );
    assert_eq!(
        r.effective().rotation.map(|r| r.rotation_hz),
        Some(0.2),
        "while fading out the effect keeps its settings"
    );
    for _ in 0..frames_for(1500) {
        r.advance(&Filters::default());
    }
    assert!(r.effective().rotation.is_none(), "once faded out the effect is gone");
}
