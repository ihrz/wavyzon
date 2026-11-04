//! Plain atomic counters the server can export (Prometheus or otherwise).
//!
//! Names follow Cadence's `/metrics`: `prefetch_total{outcome}`,
//! `fallback_total{kind}`, `audio_cache_total{outcome}`, `audio_cache_bytes`,
//! `audio_cache_files`, plus Wavyzon's `mirror_total{outcome}` and
//! `recommendations_total{outcome}`.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

macro_rules! counters {
    ($(#[$doc:meta])* $name:ident, $metric:literal, $label:literal { $($field:ident),* $(,)? }) => {
        $(#[$doc])*
        #[derive(Debug, Default)]
        pub struct $name {
            $(pub $field: AtomicU64,)*
        }

        impl $name {
            /// Metric name, without the node prefix.
            pub const METRIC: &'static str = $metric;
            /// Label name.
            pub const LABEL: &'static str = $label;

            /// `(label value, count)` pairs.
            pub fn snapshot(&self) -> Vec<(&'static str, u64)> {
                vec![$((stringify!($field), self.$field.load(Ordering::Relaxed)),)*]
            }
        }
    };
}

counters!(
    /// Prefetch (instant start) outcomes.
    PrefetchCounters, "prefetch_total", "outcome" { queued, hit, miss, failed, dropped }
);
counters!(
    /// Answers given by a playback source other than the first one.
    FallbackCounters, "fallback_total", "kind" { search, mirror }
);
counters!(
    /// Disk audio cache outcomes.
    AudioCacheCounters, "audio_cache_total", "outcome" { hit, miss, stored, evicted, read_failed, write_failed, incomplete }
);
counters!(
    /// How metadata-only tracks were mirrored to a playable source.
    MirrorCounters, "mirror_total", "outcome" { cached, isrc, search, miss, failed }
);
counters!(
    /// Recommendation (radio) outcomes.
    RecommendCounters, "recommendations_total", "outcome" { served, cached, empty, failed }
);

/// All counters of a [`crate::Sources`].
#[derive(Debug, Default)]
pub struct Metrics {
    pub prefetch: PrefetchCounters,
    pub fallback: FallbackCounters,
    pub audio_cache: AudioCacheCounters,
    pub mirror: MirrorCounters,
    pub recommend: RecommendCounters,
    /// Bytes currently stored in the disk audio cache.
    pub audio_cache_bytes: AtomicI64,
    /// Files currently stored in the disk audio cache.
    pub audio_cache_files: AtomicI64,
}

/// A single exported sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub metric: &'static str,
    /// `(label name, label value)`, if the metric is labelled.
    pub label: Option<(&'static str, &'static str)>,
    pub value: i64,
}

impl Metrics {
    /// Every counter and gauge as flat samples.
    pub fn samples(&self) -> Vec<Sample> {
        let mut out = Vec::new();
        let mut push = |metric, label, values: Vec<(&'static str, u64)>| {
            for (v, n) in values {
                out.push(Sample {
                    metric,
                    label: Some((label, v)),
                    value: n as i64,
                });
            }
        };
        push(
            PrefetchCounters::METRIC,
            PrefetchCounters::LABEL,
            self.prefetch.snapshot(),
        );
        push(
            FallbackCounters::METRIC,
            FallbackCounters::LABEL,
            self.fallback.snapshot(),
        );
        push(
            AudioCacheCounters::METRIC,
            AudioCacheCounters::LABEL,
            self.audio_cache.snapshot(),
        );
        push(MirrorCounters::METRIC, MirrorCounters::LABEL, self.mirror.snapshot());
        push(
            RecommendCounters::METRIC,
            RecommendCounters::LABEL,
            self.recommend.snapshot(),
        );
        out.push(Sample {
            metric: "audio_cache_bytes",
            label: None,
            value: self.audio_cache_bytes.load(Ordering::Relaxed),
        });
        out.push(Sample {
            metric: "audio_cache_files",
            label: None,
            value: self.audio_cache_files.load(Ordering::Relaxed),
        });
        out
    }
}

/// Increment a counter by one.
pub(crate) fn inc(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_cover_every_counter() {
        let m = Metrics::default();
        inc(&m.prefetch.hit);
        inc(&m.prefetch.hit);
        let s = m.samples();
        assert!(s.contains(&Sample {
            metric: "prefetch_total",
            label: Some(("outcome", "hit")),
            value: 2
        }));
        assert!(
            s.iter()
                .any(|x| x.metric == "fallback_total" && x.label == Some(("kind", "mirror")))
        );
        assert!(s.iter().any(|x| x.metric == "audio_cache_bytes"));
    }
}
