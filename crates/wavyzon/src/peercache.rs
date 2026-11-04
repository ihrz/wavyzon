//! Cross-node disk cache sharing (port of Cadence's `internal/peercache`).
//!
//! Every 30 s each node fetches its peers' `/v1/cache/index`. When a track is
//! about to be opened and this node has no cached copy but a peer does, the
//! file is pulled in the background (at most 3 at a time), so the next play
//! is served from disk. It never blocks or slows the current play.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use parking_lot::{Mutex, RwLock};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use wavyzon_sources::audiocache::{MAX_FILE_SIZE, Store, valid_key};

use crate::metrics::NodeMetrics;

const GOSSIP_INTERVAL: Duration = Duration::from_secs(30);
const GOSSIP_TIMEOUT: Duration = Duration::from_secs(5);
const PULL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PULLS: usize = 3;
const MAX_INDEX_KEYS: usize = 200_000;
const MAX_INDEX_BYTES: usize = 16 << 20;

pub struct Pool {
    peers: Vec<String>,
    token: String,
    store: Arc<Store>,
    client: reqwest::Client,
    metrics: Arc<NodeMetrics>,
    /// Hash prefix (`<40 hex>`) → (full key, peer).
    index: RwLock<HashMap<String, (String, String)>>,
    slots: Arc<Semaphore>,
    inflight: Mutex<HashSet<String>>,
}

impl Pool {
    /// `None` unless there are peers, a token and a store.
    pub fn new(
        peers: &[String],
        token: &str,
        store: Option<Arc<Store>>,
        metrics: Arc<NodeMetrics>,
    ) -> Option<Arc<Self>> {
        let peers: Vec<String> = peers
            .iter()
            .map(|p| p.trim().trim_end_matches('/').to_string())
            .filter(|p| !p.is_empty())
            .collect();
        let store = store?;
        if peers.is_empty() || token.is_empty() {
            return None;
        }
        let client = reqwest::Client::builder().timeout(PULL_TIMEOUT).build().ok()?;
        Some(Arc::new(Self {
            peers,
            token: token.to_string(),
            store,
            client,
            metrics,
            index: RwLock::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(MAX_PULLS)),
            inflight: Mutex::new(HashSet::new()),
        }))
    }

    /// Refresh the peer index until `stop` fires.
    pub async fn run(self: Arc<Self>, stop: CancellationToken) {
        let mut tick = tokio::time::interval(GOSSIP_INTERVAL);
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tick.tick() => self.refresh().await,
            }
        }
    }

    pub async fn refresh(&self) {
        let mut next: HashMap<String, (String, String)> = HashMap::new();
        for peer in &self.peers {
            match self.fetch_index(peer).await {
                Ok(keys) => {
                    for k in keys {
                        if next.len() >= MAX_INDEX_KEYS {
                            break;
                        }
                        if !valid_key(&k) {
                            continue;
                        }
                        let hash = k.split('-').next().unwrap_or(&k).to_string();
                        next.entry(hash).or_insert_with(|| (k, peer.clone()));
                    }
                }
                Err(e) => {
                    self.metrics.peer_cache.inc("gossip_failed");
                    tracing::debug!(peer, error = %e, "peer index refresh failed");
                }
            }
        }
        *self.index.write() = next;
    }

    async fn fetch_index(&self, peer: &str) -> Result<Vec<String>, String> {
        let res = self
            .client
            .get(format!("{peer}/v1/cache/index"))
            .header("Authorization", &self.token)
            .timeout(GOSSIP_TIMEOUT)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("peer index status {}", res.status()));
        }
        // Stop reading at the limit (like Cadence's LimitReader), so a bad
        // peer can't make us buffer an unbounded body.
        if res.content_length().is_some_and(|n| n > MAX_INDEX_BYTES as u64) {
            return Err("peer index too large".into());
        }
        let mut body = Vec::new();
        let mut chunks = res.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            if body.len() + chunk.len() > MAX_INDEX_BYTES {
                return Err("peer index too large".into());
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|e| e.to_string())
    }

    /// Number of keys known from peers.
    pub fn known(&self) -> usize {
        self.index.read().len()
    }

    /// If this node has no copy of `hash` but a peer does, pull it in the
    /// background. Never blocks.
    pub fn maybe_pull(self: &Arc<Self>, hash: &str) {
        if self.store.find_prefix(&format!("{hash}-")).is_some() {
            return;
        }
        let Some((key, peer)) = self.index.read().get(hash).cloned() else {
            return;
        };
        self.metrics.peer_cache.inc("index_hit");
        if !self.inflight.lock().insert(key.clone()) {
            return;
        }
        let Ok(permit) = self.slots.clone().try_acquire_owned() else {
            self.inflight.lock().remove(&key);
            self.metrics.peer_cache.inc("too_busy");
            return;
        };
        let me = self.clone();
        tokio::spawn(async move {
            let ok = me.pull(&key, &peer).await;
            me.metrics.peer_cache.inc(if ok { "pulled" } else { "failed" });
            me.inflight.lock().remove(&key);
            drop(permit);
        });
    }

    async fn pull(&self, key: &str, peer: &str) -> bool {
        let res = match self
            .client
            .get(format!("{peer}/v1/cache/file"))
            .query(&[("key", key)])
            .header("Authorization", &self.token)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                tracing::debug!(key, peer, status = %r.status(), "peer cache pull refused");
                return false;
            }
            Err(e) => {
                tracing::debug!(key, peer, error = %e, "peer cache pull failed");
                return false;
            }
        };
        let len = res.content_length().unwrap_or(0);
        if len == 0 || len > MAX_FILE_SIZE {
            return false;
        }
        let Ok(mut w) = self.store.create(key, len) else {
            return false;
        };
        let mut body = res.bytes_stream();
        while let Some(chunk) = body.next().await {
            let ok = match chunk {
                Ok(c) => std::io::Write::write_all(&mut w, &c).is_ok(),
                Err(_) => false,
            };
            if !ok {
                w.abort();
                return false;
            }
        }
        match w.commit() {
            Ok(()) => {
                tracing::debug!(key, peer, bytes = len, "pulled a track from a peer cache");
                true
            }
            Err(e) => {
                tracing::debug!(key, error = %e, "peer cache commit failed");
                false
            }
        }
    }
}
