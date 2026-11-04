//! The generated signals go through the real `wavyzon-audio` decoders.

use wavyzon_plugin::{Format, Source};
use wavyzon_testkit::ToneSource;
use wavyzon_testkit::audio::*;

#[test]
fn wav_builder_decodes_via_codec() {
    let wav = WavBuilder::new(48_000, 2)
        .tone(440.0, 500, 0.5)
        .silence(250)
        .clicks(120.0, 1000, 0.9)
        .build();
    let (pcm, rate) = decode_bytes(
        &Format::Encoded {
            hint: Some("wav".into()),
        },
        wav,
    )
    .unwrap();
    assert_eq!(rate, 48_000);
    assert_eq!(pcm.len(), (500 + 250 + 1000) * 48 * 2);
    let left = channel(&pcm, 2, 0);
    let (tone, rest) = left.split_at(500 * 48);
    let (quiet, beats) = rest.split_at(250 * 48);
    assert_frequency(tone, 48_000, 440.0, 1.0);
    assert_within(goertzel(tone, 48_000, 440.0), 0.5, 0.02, "tone amplitude");
    assert_silent(quiet, 0.0);
    let on = onsets(beats, 48_000, 0.5, 50.0);
    assert_eq!(on.len(), 2);
    assert_within(estimate_bpm(&on).unwrap(), 120.0, 0.5, "bpm");
}

#[test]
fn wav_at_44100_mono_keeps_native_rate() {
    let wav = WavBuilder::new(44_100, 1).tone(1000.0, 300, 0.25).build();
    let (pcm, rate) = decode_bytes(&Format::Encoded { hint: None }, wav).unwrap();
    assert_eq!(rate, 44_100);
    // The decoder hands out interleaved stereo.
    assert_frequency(&channel(&pcm, 2, 0), 44_100, 1000.0, 1.0);
    assert_within(peak(&pcm), 0.25, 0.01, "peak");
}

#[tokio::test]
async fn tone_source_through_pcm48_decoder() {
    let src = ToneSource::new().with_amplitude(0.3);
    let track = Source::track(&src, "523.25:400").await.unwrap();
    let mut stream = src.playable().unwrap().open(&track, 100).await.unwrap();
    assert_eq!(stream.format, Format::Pcm48);
    assert_eq!(stream.start_sample, 4_800);
    let bytes = read_body(&mut stream, usize::MAX).await.unwrap();
    let (pcm, rate) = decode_bytes(&stream.format, bytes).unwrap();
    assert_eq!(rate, 48_000);
    assert_eq!(pcm.len(), 300 * 48 * 2);
    let left = channel(&pcm, 2, 0);
    assert_eq!(left, channel(&pcm, 2, 1));
    assert_frequency(&left, 48_000, 523.25, 1.0);
    assert_within(goertzel(&left, 48_000, 523.25), 0.3, 0.02, "goertzel amplitude");
    assert_within(
        dominant_frequency(&left, 48_000, 400.0, 700.0, 0.25),
        523.25,
        0.5,
        "dominant",
    );
}
