//! Plugin filters ([`wavyzon_plugin::AudioFilter`]) of one deck, run after
//! the built-in chain on `f32` frames. Filters that are added or removed on
//! a running deck fade their wet amount in or out, like built-in effects.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::Value;
use wavyzon_audio::{FRAME_SAMPLES, Frame, clip};
use wavyzon_plugin::{AudioFilter, FilterFactory};

use crate::PlayerError;
use crate::filter_ramp::glide;

/// Registered plugin filter factories, keyed by [`FilterFactory::name`].
pub type FilterRegistry = HashMap<String, Arc<dyn FilterFactory>>;

/// Build a [`FilterRegistry`] from factories (later names win).
pub fn filter_registry<'a>(factories: impl IntoIterator<Item = &'a Arc<dyn FilterFactory>>) -> FilterRegistry {
    factories
        .into_iter()
        .map(|f| (f.name().to_string(), f.clone()))
        .collect()
}

struct Entry {
    name: String,
    config: Value,
    filter: Box<dyn AudioFilter>,
    wet: f64,
    target: f64,
}

/// Fresh instances created while validating a `set_filters` call, handed to
/// the first deck that needs one.
pub(crate) type Pool = HashMap<String, Box<dyn AudioFilter>>;

/// Check every changed plugin filter of `next` against the registry and build
/// one instance of each (so factory errors reach the client).
pub(crate) fn validate(
    registry: &FilterRegistry,
    current: &BTreeMap<String, Value>,
    next: &BTreeMap<String, Value>,
) -> Result<Pool, PlayerError> {
    let mut pool = Pool::new();
    for (name, cfg) in next {
        let factory = registry
            .get(name)
            .ok_or_else(|| PlayerError::UnknownFilter(name.clone()))?;
        if current.get(name) == Some(cfg) {
            continue;
        }
        let f = factory.create(cfg).map_err(|failure| PlayerError::FilterConfig {
            name: name.clone(),
            failure,
        })?;
        pool.insert(name.clone(), f);
    }
    Ok(pool)
}

/// The plugin filters of one deck.
#[derive(Default)]
pub(crate) struct PluginChain {
    entries: Vec<Entry>,
    buf: Vec<f32>,
    wet_buf: Vec<f32>,
}

fn build(registry: &FilterRegistry, pool: &mut Pool, name: &str, cfg: &Value) -> Option<Box<dyn AudioFilter>> {
    if let Some(f) = pool.remove(name) {
        return Some(f);
    }
    let factory = registry.get(name)?;
    match factory.create(cfg) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!(filter = name, error = %e, "plugin filter refused its configuration");
            None
        }
    }
}

impl PluginChain {
    /// A chain for a new deck: filters start fully wet (no fade-in).
    pub(crate) fn new(registry: &FilterRegistry, pool: &mut Pool, cfg: &BTreeMap<String, Value>) -> Self {
        let mut chain = Self::default();
        for (name, c) in cfg {
            if let Some(filter) = build(registry, pool, name, c) {
                chain.entries.push(Entry {
                    name: name.clone(),
                    config: c.clone(),
                    filter,
                    wet: 1.0,
                    target: 1.0,
                });
            }
        }
        chain
    }

    /// Move a running deck's chain to `cfg`: unchanged filters stay,
    /// changed ones are reconfigured (or rebuilt and crossfaded), removed ones
    /// fade out, new ones fade in.
    pub(crate) fn update(&mut self, registry: &FilterRegistry, pool: &mut Pool, cfg: &BTreeMap<String, Value>) {
        for e in &mut self.entries {
            if !cfg.contains_key(&e.name) {
                e.target = 0.0;
            }
        }
        for (name, c) in cfg {
            let live = self.entries.iter().position(|e| &e.name == name && e.target > 0.0);
            if let Some(i) = live {
                let e = &mut self.entries[i];
                if &e.config == c {
                    continue;
                }
                if e.filter.reconfigure(c) {
                    e.config = c.clone();
                    continue;
                }
                e.target = 0.0;
            }
            if let Some(filter) = build(registry, pool, name, c) {
                self.entries.push(Entry {
                    name: name.clone(),
                    config: c.clone(),
                    filter,
                    wet: 0.0,
                    target: 1.0,
                });
            }
        }
    }

    /// Clear every filter's internal state (seek, track change).
    pub(crate) fn reset(&mut self) {
        for e in &mut self.entries {
            e.filter.reset();
        }
    }

    /// Run the chain over `out`.
    pub(crate) fn process(&mut self, out: &mut Frame) {
        if self.entries.is_empty() {
            return;
        }
        self.buf.clear();
        self.buf.extend(out.iter().map(|&s| s as f32 / 32768.0));
        for e in &mut self.entries {
            if e.wet >= 1.0 && e.target >= 1.0 {
                e.filter.process(&mut self.buf);
                continue;
            }
            self.wet_buf.clear();
            self.wet_buf.extend_from_slice(&self.buf);
            e.filter.process(&mut self.wet_buf);
            let (from, to) = (e.wet, glide(e.wet, e.target));
            let frames = FRAME_SAMPLES / 2;
            for i in 0..frames {
                let g = (from + (to - from) * (i + 1) as f64 / frames as f64) as f32;
                for c in 0..2 {
                    let k = 2 * i + c;
                    self.buf[k] += (self.wet_buf[k] - self.buf[k]) * g;
                }
            }
            e.wet = to;
        }
        self.entries.retain(|e| e.target > 0.0 || e.wet > 0.0);
        for (o, v) in out.iter_mut().zip(&self.buf) {
            *o = clip(*v as f64 * 32768.0);
        }
    }

    /// Names of the filters currently running (including fading ones).
    #[cfg(test)]
    pub(crate) fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }
}
