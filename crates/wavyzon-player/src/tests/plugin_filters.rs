use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use wavyzon_audio::SILENCE;
use wavyzon_plugin::{AudioFilter, EventType, FilterFactory};

use super::*;
use crate::{Filters, PlayerError, filter_registry};

/// Multiplies by `{"gain": x}`; counts creations, reconfigurations and resets.
#[derive(Default)]
struct GainFactory {
    created: AtomicUsize,
    reconfigured: Arc<AtomicUsize>,
    resets: Arc<AtomicUsize>,
}

struct Gain {
    gain: f32,
    reconfigured: Arc<AtomicUsize>,
    resets: Arc<AtomicUsize>,
}

fn gain_of(cfg: &Value) -> Result<f32, Failure> {
    cfg.get("gain")
        .and_then(Value::as_f64)
        .map(|g| g as f32)
        .ok_or_else(|| Failure::common("gain", "gain is required", None))
}

impl AudioFilter for Gain {
    fn process(&mut self, frame: &mut [f32]) {
        for s in frame {
            *s *= self.gain;
        }
    }
    fn reconfigure(&mut self, config: &Value) -> bool {
        match gain_of(config) {
            Ok(g) => {
                self.gain = g;
                self.reconfigured.fetch_add(1, Ordering::SeqCst);
                true
            }
            Err(_) => false,
        }
    }
    fn reset(&mut self) {
        self.resets.fetch_add(1, Ordering::SeqCst);
    }
}

impl FilterFactory for GainFactory {
    fn name(&self) -> &str {
        "gain"
    }
    fn create(&self, config: &Value) -> Result<Box<dyn AudioFilter>, Failure> {
        self.created.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Gain {
            gain: gain_of(config)?,
            reconfigured: self.reconfigured.clone(),
            resets: self.resets.clone(),
        }))
    }
}

fn with_gain(cfg: Value) -> Filters {
    let mut f = Filters::default();
    f.plugin_filters.insert("gain".into(), cfg);
    f
}

async fn gain_player() -> (Arc<Player>, Recorder, Arc<GainFactory>) {
    let rec = Recorder::default();
    let factory = Arc::new(GainFactory::default());
    let mut cfg = config(&[("a", 6.0)], &rec);
    let dynf: Arc<dyn FilterFactory> = factory.clone();
    cfg.plugin_filters = Arc::new(filter_registry([&dynf]));
    (Player::new(cfg), rec, factory)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_and_invalid_plugin_filters_are_refused() {
    let (p, _, _) = gain_player().await;
    let mut f = Filters::default();
    f.plugin_filters.insert("nope".into(), json!({}));
    assert_eq!(p.set_filters(f), Err(PlayerError::UnknownFilter("nope".into())));
    assert!(matches!(
        p.set_filters(with_gain(json!({}))),
        Err(PlayerError::FilterConfig { .. })
    ));
    assert!(
        p.state().filters.plugin_filters.is_empty(),
        "a refused update must not change the filters"
    );
    let parsed: Filters = serde_json::from_value(json!({ "pluginFilters": { "gain": { "gain": 0.5 } } })).unwrap();
    p.set_filters(parsed).unwrap();
    assert_eq!(
        serde_json::to_value(p.state().filters).unwrap()["pluginFilters"]["gain"]["gain"],
        0.5
    );
    p.destroy();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_filters_process_glide_reconfigure_and_reset() {
    let (p, rec, factory) = gain_player().await;
    p.play(track("a"), 0, false);
    rec.wait(EventType::TrackStartEvent).await;
    let mut out = SILENCE;
    for _ in 0..5 {
        frame(&p, &mut out).await;
    }
    let full = peak(&out);

    // Added on a running deck: fades in rather than jumping to half level.
    p.set_filters(with_gain(json!({ "gain": 0.5 }))).unwrap();
    frame(&p, &mut out).await;
    let first = peak(&out);
    assert!(
        first > full * 6 / 10,
        "the filter must fade in, first frame peak {first} of {full}"
    );
    for _ in 0..60 {
        frame(&p, &mut out).await;
    }
    let half = peak(&out);
    assert!(
        (half - full / 2).abs() <= full / 50,
        "settled peak {half}, want about {}",
        full / 2
    );

    // A new config reconfigures the running instance.
    let created = factory.created.load(Ordering::SeqCst);
    p.set_filters(with_gain(json!({ "gain": 0.25 }))).unwrap();
    assert_eq!(factory.reconfigured.load(Ordering::SeqCst), 1);
    assert_eq!(
        factory.created.load(Ordering::SeqCst),
        created + 1,
        "only the validation instance is built"
    );
    frame(&p, &mut out).await;
    let quarter = peak(&out);
    assert!(
        (quarter - full / 4).abs() <= full / 50,
        "reconfigured peak {quarter}, want about {}",
        full / 4
    );

    // Seek keeps the instance and resets it.
    p.seek(1000).unwrap();
    assert_eq!(factory.resets.load(Ordering::SeqCst), 1);
    frame(&p, &mut out).await;
    assert!((peak(&out) - full / 4).abs() <= full / 50);

    // Removing it fades out, then the chain empties.
    p.set_filters(Filters::default()).unwrap();
    for _ in 0..60 {
        frame(&p, &mut out).await;
    }
    assert!(
        (peak(&out) - full).abs() <= full / 50,
        "after removal the level returns to {full}, got {}",
        peak(&out)
    );
    assert!(p.inner.lock().active.as_ref().unwrap().plugins.names().is_empty());
    p.destroy();
}
