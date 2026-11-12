//! Plain atomic counters the server can export (Prometheus or otherwise).
//!
//! Names follow Cadence's `/metrics`: `prefetch_total{outcome}`,
//! `fallback_total{kind}`, `audio_cache_total{outcome}`, `audio_cache_bytes`,
//! `audio_cache_files`, `deezer_accounts_total{outcome}`, `seek_total{outcome}`
//! and `cdn_range_ignored_total`, plus Wavyzon's `mirror_total{outcome}`,
//! `recommendations_total{outcome}`, `deezer_recommendations_total{source}` and
//! `lyrics_total{outcome}`, `lyrics_plugin_total{outcome}`, `lrclib_total{outcome}`.

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
counters!(
    /// Deezer accounts rejected, and streams served by a later account.
    DeezerAccountCounters, "deezer_accounts_total", "outcome" { switched, rejected }
);
counters!(
    /// How Deezer seeks were served.
    SeekCounters, "seek_total", "outcome" { range, fallback }
);
counters!(
    /// Lyrics lookups (lyrics.ovh): found, served from the cache, none, or failed.
    LyricsCounters, "lyrics_total", "outcome" { found, cached, not_found, failed }
);
counters!(
    /// LRCLIB lookups: found (`synced` counts those with timed lines too),
    /// served from the cache, none, or failed.
    LrclibCounters, "lrclib_total", "outcome" { found, synced, cached, not_found, failed }
);
counters!(
    /// Lookups by plugin lyrics providers (Genius), asked before lyrics.ovh:
    /// found, none (lyrics.ovh is asked next), or failed (lyrics.ovh is asked next).
    LyricsPluginCounters, "lyrics_plugin_total", "outcome" { found, not_found, failed }
);
counters!(
    /// Where Deezer recommendation lists came from.
    DeezerMixCounters, "deezer_recommendations_total", "source" { track_mix, artist_radio }
);

/// All counters of a [`crate::Sources`].
#[derive(Debug, Default)]
pub struct Metrics {
    pub prefetch: PrefetchCounters,
    pub fallback: FallbackCounters,
    pub audio_cache: AudioCacheCounters,
    pub mirror: MirrorCounters,
    pub recommend: RecommendCounters,
    /// Deezer account walks (stream requests that switched or benched).
    pub deezer_accounts: DeezerAccountCounters,
    /// Deezer range seeks (served by a range, or streamed from the start).
    pub seek: SeekCounters,
    /// Where Deezer's recommendation lists came from.
    pub deezer_recommend: DeezerMixCounters,
    /// Lyrics lookups.
    pub lyrics: LyricsCounters,
    /// LRCLIB lookups.
    pub lrclib: LrclibCounters,
    /// Lyrics lookups by plugin providers.
    pub lyrics_plugins: LyricsPluginCounters,
    /// Range requests a CDN answered with the whole file.
    pub cdn_range_ignored: AtomicU64,
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
        push(
            DeezerAccountCounters::METRIC,
            DeezerAccountCounters::LABEL,
            self.deezer_accounts.snapshot(),
        );
        push(SeekCounters::METRIC, SeekCounters::LABEL, self.seek.snapshot());
        push(
            DeezerMixCounters::METRIC,
            DeezerMixCounters::LABEL,
            self.deezer_recommend.snapshot(),
        );
        push(LyricsCounters::METRIC, LyricsCounters::LABEL, self.lyrics.snapshot());
        push(LrclibCounters::METRIC, LrclibCounters::LABEL, self.lrclib.snapshot());
        push(
            LyricsPluginCounters::METRIC,
            LyricsPluginCounters::LABEL,
            self.lyrics_plugins.snapshot(),
        );
        out.push(Sample {
            metric: "cdn_range_ignored_total",
            label: None,
            value: self.cdn_range_ignored.load(Ordering::Relaxed) as i64,
        });
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
