//! `/v1/info`, `/v1/stats`, `/v1/clock`, `/v1/plugins` and `/metrics`.

use axum::extract::State;
use axum::response::Response;
use serde_json::json;
use wavyzon_analysis::ANALYSIS_VERSION;

use super::{Api, FILTERS, St, VERSION, ok_json, typed};
use crate::metrics::{Exposition, resident_bytes};
use crate::session::unix_ms;

pub(super) async fn info(State(api): St) -> Response {
    ok_json(json!({
        "version": VERSION,
        "sources": api.sources.names(),
        "dave": wavyzon_voice::dave_available(),
        "filters": FILTERS,
        "pluginFilters": api.plugin_filters,
        "plugins": api.plugins,
        "mixing": true,
        "analysis": ANALYSIS_VERSION,
        "wireClock": true,
        "lyrics": api.sources.lyrics().is_some(),
    }))
}

pub(super) async fn stats(State(api): St) -> Response {
    let (players, playing) = api.sessions.counts();
    ok_json(json!({
        "players": players,
        "playingPlayers": playing,
        "sessions": api.sessions.sessions().len(),
        "uptime": api.started.elapsed().as_millis() as i64,
        "analysis": api.offline.stats(),
    }))
}

pub(super) async fn clock() -> Response {
    ok_json(json!({ "now": unix_ms() }))
}

pub(super) async fn plugins(State(api): St) -> Response {
    ok_json(&api.plugins)
}

pub(super) async fn metrics(State(api): St) -> Response {
    typed("text/plain; version=0.0.4", render_metrics(&api))
}

/// The `/metrics` document.
pub fn render_metrics(api: &Api) -> String {
    let (players, playing) = api.sessions.counts();
    let analysis = api.offline.stats();
    let m = &api.metrics;
    let mut e = Exposition::new();
    e.gauge("players", "Players held.", players as f64);
    e.gauge("playing_players", "Players currently sending audio.", playing as f64);
    e.gauge("sessions", "Client sessions.", api.sessions.sessions().len() as f64);
    e.gauge("plugins", "Loaded plugins.", api.plugins.len() as f64);
    e.gauge("analysis_queued", "Offline analyses waiting.", analysis.queued as f64);
    e.counter(
        "analysis_dropped_total",
        "Offline analyses dropped because the queue was full.",
        analysis.dropped as f64,
    );
    e.counter("analysis_done_total", "Tracks analysed offline.", analysis.done as f64);
    e.counter(
        "analysis_failed_total",
        "Offline analyses that failed.",
        analysis.failed as f64,
    );
    e.gauge(
        "fingerprints",
        "Track analyses held in memory.",
        api.fingerprints.len() as f64,
    );
    e.counter(
        "send_stall_ms_total",
        "Milliseconds the send loop fell behind its 20ms schedule.",
        m.stall_ms() as f64,
    );
    e.labeled("events_total", "Player events emitted.", "type", &m.events.snapshot());
    e.labeled(
        "track_exceptions_total",
        "Track failures by source and severity.",
        "kind",
        &m.exceptions.snapshot(),
    );
    e.labeled(
        "voice_closes_total",
        "Voice gateway closes by code.",
        "code",
        &m.voice_closes.snapshot(),
    );
    e.labeled(
        "host_total",
        "DJ voice transitions started, ended or refused.",
        "outcome",
        &m.host.snapshot(),
    );
    e.labeled(
        "peer_cache_total",
        "Cross-node peer cache outcomes.",
        "outcome",
        &m.peer_cache.snapshot(),
    );
    // Source counters (prefetch, fallback, audio cache, mirroring, radio).
    let mut labeled: Vec<(&'static str, &'static str, Vec<(&'static str, i64)>)> = Vec::new();
    for s in api.sources.metrics().samples() {
        match s.label {
            Some((label, value)) => match labeled.iter_mut().find(|(m, _, _)| *m == s.metric) {
                Some((_, _, v)) => v.push((value, s.value)),
                None => labeled.push((s.metric, label, vec![(value, s.value)])),
            },
            None if s.metric.ends_with("_total") => e.counter(s.metric, &source_help(s.metric), s.value as f64),
            None => e.gauge(s.metric, &source_help(s.metric), s.value as f64),
        }
    }
    for (metric, label, values) in labeled {
        e.labeled(metric, &source_help(metric), label, &values);
    }
    if let Ok(h) = tokio::runtime::Handle::try_current() {
        e.gauge("tasks", "Live tokio tasks.", h.metrics().num_alive_tasks() as f64);
    }
    e.gauge(
        "resident_bytes",
        "Resident memory of the process.",
        resident_bytes() as f64,
    );
    e.finish()
}

fn source_help(metric: &str) -> String {
    match metric {
        "prefetch_total" => "Track-start prefetch outcomes.",
        "fallback_total" => "Answers given by a playback source other than the first one.",
        "audio_cache_total" => "Disk audio cache outcomes.",
        "mirror_total" => "How metadata-only tracks were mirrored to a playable source.",
        "recommendations_total" => "Recommendation (radio) outcomes.",
        "deezer_recommendations_total" => "Where Deezer recommendation lists came from.",
        "deezer_accounts_total" => "Deezer accounts rejected, and streams served by a later account.",
        "seek_total" => "How Deezer seeks were served.",
        "cdn_range_ignored_total" => "Range requests the CDN answered with the whole file.",
        "audio_cache_bytes" => "Bytes of audio held in the disk cache.",
        "audio_cache_files" => "Tracks held in the disk cache.",
        _ => "Source counter.",
    }
    .to_string()
}
