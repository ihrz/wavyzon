//! Prometheus text exposition (port of Cadence's `internal/metrics`).
//!
//! Counters live in a [`NodeMetrics`] owned by the node (not globals), so
//! tests running several nodes in one process don't mix their numbers.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicI64, Ordering};

use parking_lot::Mutex;

/// Prefix of every exported metric.
pub const PREFIX: &str = "wavyzon_";

/// A counter keyed by one label value.
#[derive(Debug, Default)]
pub struct Labeled(Mutex<BTreeMap<String, i64>>);

impl Labeled {
    pub fn inc(&self, label: &str) {
        self.add(label, 1);
    }
    pub fn add(&self, label: &str, n: i64) {
        *self.0.lock().entry(label.to_string()).or_default() += n;
    }
    pub fn get(&self, label: &str) -> i64 {
        self.0.lock().get(label).copied().unwrap_or(0)
    }
    pub fn snapshot(&self) -> Vec<(String, i64)> {
        self.0.lock().iter().map(|(k, v)| (k.clone(), *v)).collect()
    }
}

/// Counters the node itself maintains (sources and analysis keep their own).
#[derive(Debug, Default)]
pub struct NodeMetrics {
    /// `events_total{type}`.
    pub events: Labeled,
    /// `track_exceptions_total{kind="source:severity"}`.
    pub exceptions: Labeled,
    /// `voice_closes_total{code}`.
    pub voice_closes: Labeled,
    /// `host_total{outcome}`: `started`, `ended`, `refused`.
    pub host: Labeled,
    /// `peer_cache_total{outcome}`.
    pub peer_cache: Labeled,
    /// `send_stall_ms_total`.
    pub stall_ms: AtomicI64,
}

/// Builds an exposition document.
#[derive(Default)]
pub struct Exposition(String);

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

impl Exposition {
    pub fn new() -> Self {
        Self::default()
    }

    fn header(&mut self, name: &str, help: &str, kind: &str) {
        let _ = writeln!(self.0, "# HELP {PREFIX}{name} {help}\n# TYPE {PREFIX}{name} {kind}");
    }

    pub fn gauge(&mut self, name: &str, help: &str, value: f64) {
        self.header(name, help, "gauge");
        let _ = writeln!(self.0, "{PREFIX}{name} {value}");
    }

    pub fn counter(&mut self, name: &str, help: &str, value: f64) {
        self.header(name, help, "counter");
        let _ = writeln!(self.0, "{PREFIX}{name} {value}");
    }

    pub fn labeled<S: AsRef<str>>(&mut self, name: &str, help: &str, label: &str, values: &[(S, i64)]) {
        self.header(name, help, "counter");
        for (k, v) in values {
            let _ = writeln!(self.0, "{PREFIX}{name}{{{label}=\"{}\"}} {v}", escape(k.as_ref()));
        }
    }

    pub fn finish(self) -> String {
        self.0
    }
}

/// Resident memory of this process in bytes (Linux; `0` elsewhere).
pub fn resident_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok()))
        .map_or(0, |pages| pages * 4096)
}

impl NodeMetrics {
    pub fn stall_ms(&self) -> i64 {
        self.stall_ms.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposition_format() {
        let m = NodeMetrics::default();
        m.events.inc("TrackStartEvent");
        m.events.inc("TrackStartEvent");
        m.exceptions.inc("tone:\"weird\"");
        let mut e = Exposition::new();
        e.gauge("players", "Players held.", 3.0);
        e.labeled("events_total", "Player events emitted.", "type", &m.events.snapshot());
        e.labeled(
            "track_exceptions_total",
            "Track failures.",
            "kind",
            &m.exceptions.snapshot(),
        );
        let out = e.finish();
        assert!(out.contains("# TYPE wavyzon_players gauge\nwavyzon_players 3\n"));
        assert!(out.contains("wavyzon_events_total{type=\"TrackStartEvent\"} 2\n"));
        assert!(out.contains("kind=\"tone:\\\"weird\\\"\""));
    }
}
