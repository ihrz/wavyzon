use crate::Reverb;
use crate::reverb::ReverbState;

#[test]
fn leaves_a_tail_that_decays() {
    let mut s = ReverbState::default();
    let rv = Reverb {
        mix: 0.5,
        room: 0.8,
        damping: 0.4,
    };
    let mut energy = |from: usize, to: usize, first: bool| {
        let mut total = 0.0;
        for i in from..to {
            let input = if first && i == 0 { 1.0 } else { 0.0 };
            let (l, r) = s.process(input, input, &rv, 1.0);
            total += l * l + r * r;
        }
        total
    };
    let early = energy(0, 9600, true);
    let late = energy(9600, 96_000, false);
    let tail = energy(96_000, 192_000, false);
    assert!(
        early > 0.0 && late > 0.0,
        "an impulse should ring on, got early {early} late {late}"
    );
    assert!(tail < late, "the tail should decay, got {tail} after {late}");
}

#[test]
fn with_no_mix_is_dry() {
    let mut s = ReverbState::default();
    let rv = Reverb {
        mix: 0.0,
        room: 0.9,
        damping: 0.2,
    };
    for i in 0..2000 {
        let input = (i as f64 / 9.0).sin();
        let (l, r) = s.process(input, -input, &rv, 1.0);
        assert!(
            l == input && r == -input,
            "mix 0 must pass the signal through, sample {i} got {l} {r}"
        );
    }
}

#[test]
fn stays_finite_at_the_largest_room() {
    let mut s = ReverbState::default();
    let rv = Reverb {
        mix: 1.0,
        room: 1.0,
        damping: 0.0,
    };
    for i in 0..480_000 {
        let input = (i as f64 / 7.0).sin();
        let (l, r) = s.process(input, input, &rv, 1.0);
        assert!(
            l.is_finite() && l.abs() <= 50.0 && r.abs() <= 50.0,
            "reverb ran away at sample {i}: {l} {r}"
        );
    }
}
