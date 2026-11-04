//! Client sessions (port of Cadence's `internal/session`): one per WebSocket
//! client, holding a player per guild, with resume (players keep running
//! while the client is away, missed messages are replayed on reconnect) and
//! state persistence across restarts.

mod guild;
mod persist;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;
use wavyzon_analysis::{Analyzer, Offline, Registry as Fingerprints};
use wavyzon_audio::Frame;
use wavyzon_player::{
    FilterRegistry, Loudness, LoudnessLookup, Opener, Player, PlayerConfig, Resolver, State, Tap, TapFactory,
    UnmeasuredHook,
};
use wavyzon_plugin::{Event, EventType, Failure, Stream, Track, async_trait};
use wavyzon_sources::Sources;

pub use guild::{Guild, VoiceError};
pub use persist::{PersistError, RestoreReport};

use crate::lavalink::{self, event_name};
use crate::metrics::NodeMetrics;
use crate::peercache::Pool;
use crate::plugins::EventBus;

/// Messages queued per connected client before it counts as not reading.
const OUTBOUND_BUFFER: usize = 4096;
/// Messages kept for a resuming client while it is away.
const BACKLOG_LIMIT: usize = 2000;
/// How often a v4 session gets `stats`.
const STATS_INTERVAL: Duration = Duration::from_secs(60);

/// Which API a session speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    V1,
    V4,
}

impl Serialize for Protocol {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(match self {
            Protocol::V1 => 0,
            Protocol::V4 => 1,
        })
    }
}

impl<'de> Deserialize<'de> for Protocol {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match u8::deserialize(d)? {
            0 => Ok(Protocol::V1),
            1 => Ok(Protocol::V4),
            n => Err(serde::de::Error::custom(format!("unknown protocol {n}"))),
        }
    }
}

pub(crate) fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Everything sessions need from the node.
#[derive(Clone)]
pub struct Deps {
    pub sources: Sources,
    pub fingerprints: Arc<Fingerprints>,
    pub offline: Arc<Offline>,
    pub filters: Arc<FilterRegistry>,
    pub buffer_frames: usize,
    pub playout_delay: Duration,
    pub pause_fade: Duration,
    /// Automatic crossfade into the next track (`0` = gapless).
    pub crossfade: Duration,
    pub update_interval: Duration,
    /// Loudness normalisation target in LUFS; `None` disables it.
    pub normalize_lufs: Option<f64>,
    pub metrics: Arc<NodeMetrics>,
    pub bus: EventBus,
    pub peers: Option<Arc<Pool>>,
    pub runtime: Handle,
}

/// Opens tracks through the source registry (and asks peers for a copy).
struct SourcesOpener {
    sources: Sources,
    peers: Option<Arc<Pool>>,
}

#[async_trait]
impl Opener for SourcesOpener {
    async fn open(&self, track: &Track, start_ms: i64) -> Result<Stream, Failure> {
        if let Some(p) = &self.peers {
            p.maybe_pull(&Sources::disk_key(track));
        }
        self.sources.open(track, start_ms).await
    }
}

struct SourcesResolver(Sources);

#[async_trait]
impl Resolver for SourcesResolver {
    async fn playable(&self, track: &Track) -> Result<Track, Failure> {
        self.0.playable(track).await
    }
}

/// Feeds the live analyser while a track plays from its start.
struct AnalyzerTap(Analyzer);

impl Tap for AnalyzerTap {
    fn feed(&mut self, frame: &Frame) {
        self.0.feed(frame);
    }
    fn finish(mut self: Box<Self>) {
        self.0.finish();
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AttachError {
    #[error("session already has a connected client")]
    AlreadyAttached,
    #[error("session expired")]
    Closed,
}

struct Inner {
    out: Option<mpsc::Sender<String>>,
    attach_id: u64,
    connected: bool,
    resuming: bool,
    timeout: Duration,
    backlog: Vec<String>,
    overflowed: bool,
    closed: bool,
    expiry: Option<(u64, AbortHandle)>,
    expiry_gen: u64,
    guilds: HashMap<String, Arc<Guild>>,
}

pub struct Session {
    pub id: String,
    pub user_id: String,
    pub client_name: String,
    pub protocol: Protocol,
    me: Weak<Session>,
    manager: Weak<Manager>,
    deps: Deps,
    inner: Mutex<Inner>,
    extras: AtomicBool,
    stop: CancellationToken,
}

/// A receiver of a session's outgoing messages, from [`Session::attach`].
pub struct Attached {
    pub rx: mpsc::Receiver<String>,
    pub resumed: bool,
    pub attach_id: u64,
}

impl Session {
    fn manager(&self) -> Option<Arc<Manager>> {
        self.manager.upgrade()
    }

    /// Queue `v` for the client (or the resume backlog).
    pub fn send(&self, v: &impl Serialize) {
        let Some(m) = self.manager() else { return };
        if m.quiet.load(Ordering::SeqCst) {
            return;
        }
        let raw = match serde_json::to_string(v) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(session = %self.id, error = %e, "unencodable message");
                return;
            }
        };
        let mut st = self.inner.lock();
        if !st.connected {
            self.buffer_locked(&mut st, raw);
            return;
        }
        let Some(out) = &st.out else { return };
        let raw = match out.try_send(raw) {
            Ok(()) => return,
            Err(mpsc::error::TrySendError::Full(r) | mpsc::error::TrySendError::Closed(r)) => r,
        };
        tracing::warn!(session = %self.id, client = %self.client_name, "client is not reading, disconnecting it so it can resume");
        self.buffer_locked(&mut st, raw);
        let remove = self.detach_locked(&mut st);
        drop(st);
        if remove {
            m.remove(&self.id);
        }
    }

    fn buffer_locked(&self, st: &mut Inner, raw: String) {
        if !st.resuming {
            return;
        }
        if st.backlog.len() >= BACKLOG_LIMIT {
            if !st.overflowed {
                st.overflowed = true;
                tracing::warn!(session = %self.id, limit = BACKLOG_LIMIT, "resume backlog is full, further messages are dropped");
            }
            return;
        }
        st.backlog.push(raw);
    }

    /// Connect a client. `resumed` is true when the session was waiting for
    /// one (after a disconnect or a restore); the backlog comes first.
    pub fn attach(&self) -> Result<Attached, AttachError> {
        let mut st = self.inner.lock();
        if st.closed {
            return Err(AttachError::Closed);
        }
        if st.connected {
            return Err(AttachError::AlreadyAttached);
        }
        let resumed = st.expiry.is_some();
        if let Some((_, h)) = st.expiry.take() {
            h.abort();
        }
        let (tx, rx) = mpsc::channel(OUTBOUND_BUFFER);
        for raw in st.backlog.drain(..) {
            let _ = tx.try_send(raw);
        }
        st.overflowed = false;
        st.out = Some(tx);
        st.connected = true;
        st.attach_id += 1;
        Ok(Attached {
            rx,
            resumed,
            attach_id: st.attach_id,
        })
    }

    fn detach_locked(&self, st: &mut Inner) -> bool {
        st.connected = false;
        st.out = None;
        if !st.resuming {
            return true;
        }
        self.schedule_expiry_locked(st, st.timeout);
        tracing::info!(session = %self.id, timeout_seconds = st.timeout.as_secs(), "client disconnected, keeping players for resume");
        false
    }

    fn schedule_expiry_locked(&self, st: &mut Inner, after: Duration) {
        if let Some((_, h)) = st.expiry.take() {
            h.abort();
        }
        st.expiry_gen += 1;
        let generation = st.expiry_gen;
        let me = self.me.clone();
        let task = self.deps.runtime.spawn(async move {
            tokio::time::sleep(after).await;
            if let Some(s) = me.upgrade() {
                s.expire(generation);
            }
        });
        st.expiry = Some((generation, task.abort_handle()));
    }

    fn expire(&self, generation: u64) {
        {
            let mut st = self.inner.lock();
            if st.connected || st.expiry.as_ref().map(|e| e.0) != Some(generation) {
                return;
            }
            st.expiry = None;
            st.closed = true;
        }
        tracing::info!(session = %self.id, "resume timeout elapsed, destroying the session");
        if let Some(m) = self.manager() {
            m.remove(&self.id);
        }
    }

    /// The client of `attach_id` went away.
    pub fn detach(&self, attach_id: u64) {
        let mut st = self.inner.lock();
        if !st.connected || st.attach_id != attach_id {
            return;
        }
        let remove = self.detach_locked(&mut st);
        drop(st);
        if remove && let Some(m) = self.manager() {
            m.remove(&self.id);
        }
    }

    pub fn configure(&self, resuming: bool, timeout: Duration) {
        let mut st = self.inner.lock();
        st.resuming = resuming;
        if !timeout.is_zero() {
            st.timeout = timeout;
        }
    }

    /// `(resuming, timeout)`.
    pub fn settings(&self) -> (bool, Duration) {
        let st = self.inner.lock();
        (st.resuming, st.timeout)
    }

    pub fn is_connected(&self) -> bool {
        self.inner.lock().connected
    }

    /// v4: also send Wavyzon's extra events.
    pub fn set_extra_events(&self, on: bool) {
        self.extras.store(on, Ordering::SeqCst);
    }

    pub fn extra_events(&self) -> bool {
        self.extras.load(Ordering::SeqCst)
    }

    /// The player of `id`, created when `create` is set.
    pub fn guild(&self, id: &str, create: bool) -> Option<Arc<Guild>> {
        let mut st = self.inner.lock();
        if let Some(g) = st.guilds.get(id) {
            return Some(g.clone());
        }
        if !create || st.closed {
            return None;
        }
        let g = self.new_guild(id);
        st.guilds.insert(id.to_string(), g.clone());
        Some(g)
    }

    fn new_guild(&self, id: &str) -> Arc<Guild> {
        let d = &self.deps;
        Arc::new_cyclic(|weak: &Weak<Guild>| {
            let (sess, guild) = (self.me.clone(), weak.clone());
            let on_event = Arc::new(move |e: Event| {
                if let (Some(s), Some(g)) = (sess.upgrade(), guild.upgrade()) {
                    s.on_event(&g, e);
                }
            });
            let opener = Arc::new(SourcesOpener {
                sources: d.sources.clone(),
                peers: d.peers.clone(),
            });
            let mut cfg = PlayerConfig::new(
                id,
                opener,
                Arc::new(SourcesResolver(d.sources.clone())),
                on_event,
                d.runtime.clone(),
            );
            cfg.buffer_frames = d.buffer_frames;
            cfg.playout_delay = d.playout_delay;
            cfg.pause_fade = d.pause_fade;
            cfg.crossfade = d.crossfade;
            cfg.plugin_filters = d.filters.clone();
            if let Some(target) = d.normalize_lufs {
                cfg.target_lufs = target;
                let fps = d.fingerprints.clone();
                let lookup: LoudnessLookup = Arc::new(move |t: &Track, playable: &Track| {
                    [playable, t]
                        .into_iter()
                        .find_map(|k| fps.loudness(&k.source, &k.identifier))
                        .map(|l| Loudness {
                            integrated_lufs: l.integrated_lufs,
                            peak_amplitude: l.peak_amplitude,
                        })
                });
                let offline = d.offline.clone();
                let hook: UnmeasuredHook = Arc::new(move |t: &Track| {
                    offline.enqueue(t.clone(), false);
                });
                cfg.loudness = Some(lookup);
                cfg.on_unmeasured = Some(hook);
            }
            Guild::new(id.to_string(), Player::new(cfg), self.me.clone())
        })
    }

    pub fn guilds(&self) -> Vec<Arc<Guild>> {
        self.inner.lock().guilds.values().cloned().collect()
    }

    /// Destroy one player and leave voice.
    pub async fn destroy_guild(&self, id: &str) -> bool {
        let g = self.inner.lock().guilds.remove(id);
        match g {
            Some(g) => {
                g.destroy().await;
                true
            }
            None => false,
        }
    }

    /// A tap factory that analyses tracks not fingerprinted yet.
    pub fn analysis_tap(&self) -> TapFactory {
        let fps = self.deps.fingerprints.clone();
        Arc::new(move |t: &Track| {
            if fps.has(&t.source, &t.identifier) {
                return None;
            }
            let tap: Box<dyn Tap> = Box::new(AnalyzerTap(Analyzer::storing_into(
                &t.source,
                &t.identifier,
                fps.clone(),
            )));
            Some(tap)
        })
    }

    fn on_event(&self, g: &Guild, e: Event) {
        let m = &self.deps.metrics;
        m.events.inc(&event_name(e.kind));
        match e.kind {
            EventType::TrackExceptionEvent => {
                if let Some(f) = &e.exception {
                    let severity = serde_json::to_value(f.severity)
                        .ok()
                        .and_then(|v| v.as_str().map(String::from));
                    m.exceptions
                        .inc(&format!("{}:{}", f.source, severity.unwrap_or_default()));
                    tracing::warn!(session = %self.id, guild = %g.id, source = %f.source, severity = ?f.severity, message = %f.message, cause = %f.cause, "track failed");
                }
            }
            EventType::SendStallEvent => {
                m.stall_ms.fetch_add(e.stall_ms, Ordering::Relaxed);
            }
            EventType::HostStartedEvent => m.host.inc("started"),
            EventType::HostEndedEvent => m.host.inc("ended"),
            _ => {}
        }
        match self.protocol {
            Protocol::V4 => {
                if lavalink::is_standard_event(e.kind) || self.extra_events() {
                    self.send(&lavalink::event(&e, |enc| g.user_data(enc)));
                }
                self.send(&self.update_v4(g));
            }
            Protocol::V1 => {
                self.send(&e);
                self.send(&self.update(g));
            }
        }
        self.deps.bus.publish(&self.id, &e);
    }

    pub(crate) fn voice_closed(&self, g: &Guild, e: &Event) {
        self.deps.metrics.voice_closes.inc(&e.code.unwrap_or(0).to_string());
        tracing::info!(session = %self.id, guild = %g.id, code = e.code.unwrap_or(0), reason = %e.close_reason, terminal = e.terminal.unwrap_or(false), "voice closed");
        match self.protocol {
            Protocol::V4 => self.send(&lavalink::event(e, |_| Default::default())),
            Protocol::V1 => self.send(e),
        }
        self.deps.bus.publish(&self.id, e);
    }

    /// The `/v1` player object (`playerUpdate`).
    pub fn update(&self, g: &Guild) -> Value {
        let (server, connected, ping) = g.voice_state();
        let mut state = json!({
            "time": unix_ms(),
            "connected": connected,
            "ping": ping.as_millis() as i64,
            "player": g.player.state(),
        });
        let region = server.region();
        if !region.is_empty() {
            state["region"] = region.into();
        }
        json!({ "op": "playerUpdate", "guildId": g.id, "state": state })
    }

    fn state_v4(&self, g: &Guild, st: &State) -> lavalink::PlayerState {
        let (_, connected, ping) = g.voice_state();
        lavalink::PlayerState {
            time: unix_ms(),
            position: st.position_ms,
            connected,
            ping: if connected { ping.as_millis() as i64 } else { -1 },
        }
    }

    /// The `/v4` player object.
    pub fn player_v4(&self, g: &Guild) -> lavalink::Player {
        let (server, _, _) = g.voice_state();
        let st = g.player.state();
        let voice = lavalink::VoiceState {
            token: server.token,
            endpoint: server.endpoint,
            session_id: server.session_id,
            channel_id: server.channel_id,
        };
        let mut out = lavalink::from_player(&g.id, &st, self.state_v4(g, &st), voice);
        if let Some(t) = &mut out.track {
            t.user_data = g.user_data(&t.encoded);
        }
        out
    }

    fn update_v4(&self, g: &Guild) -> Value {
        lavalink::player_update(&g.id, self.state_v4(g, &g.player.state()))
    }

    /// Send a v4 `stats` message.
    pub fn send_stats(&self) {
        if let Some(m) = self.manager()
            && let Some(f) = m.stats.get()
        {
            let mut v = serde_json::to_value(f()).unwrap_or_default();
            v["op"] = "stats".into();
            self.send(&v);
        }
    }

    async fn updates(self: Arc<Self>) {
        let interval = self.deps.update_interval;
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        let mut stats = tokio::time::interval_at(tokio::time::Instant::now() + STATS_INTERVAL, STATS_INTERVAL);
        loop {
            tokio::select! {
                _ = self.stop.cancelled() => return,
                _ = stats.tick() => if self.protocol == Protocol::V4 { self.send_stats() },
                _ = tick.tick() => {
                    for g in self.guilds() {
                        let st = g.player.state();
                        if st.track.is_none() || st.paused {
                            continue;
                        }
                        match self.protocol {
                            Protocol::V4 => self.send(&lavalink::player_update(&g.id, self.state_v4(&g, &st))),
                            Protocol::V1 => self.send(&self.update(&g)),
                        }
                    }
                }
            }
        }
    }

    async fn destroy(&self) {
        let guilds: Vec<Arc<Guild>> = {
            let mut st = self.inner.lock();
            st.closed = true;
            st.out = None;
            if let Some((_, h)) = st.expiry.take() {
                h.abort();
            }
            st.guilds.drain().map(|(_, g)| g).collect()
        };
        self.stop.cancel();
        futures::future::join_all(guilds.iter().map(|g| g.destroy())).await;
    }
}

type StatsFn = Arc<dyn Fn() -> lavalink::Stats + Send + Sync>;

/// Every session of the node.
pub struct Manager {
    me: Weak<Manager>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    deps: Deps,
    quiet: AtomicBool,
    stats: OnceLock<StatsFn>,
}

fn new_id() -> String {
    rand::random::<[u8; 8]>().iter().map(|b| format!("{b:02x}")).collect()
}

impl Manager {
    pub fn new(deps: Deps) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            sessions: Mutex::new(HashMap::new()),
            deps,
            quiet: AtomicBool::new(false),
            stats: OnceLock::new(),
        })
    }

    pub fn deps(&self) -> &Deps {
        &self.deps
    }

    /// The function behind v4 `stats` messages.
    pub fn set_stats(&self, f: StatsFn) {
        let _ = self.stats.set(f);
    }

    pub fn create(&self, user_id: &str, client_name: &str, timeout: Duration, protocol: Protocol) -> Arc<Session> {
        self.new_session(new_id(), user_id, client_name, timeout, protocol)
    }

    fn new_session(
        &self,
        id: String,
        user_id: &str,
        client_name: &str,
        timeout: Duration,
        protocol: Protocol,
    ) -> Arc<Session> {
        let s = Arc::new_cyclic(|me| Session {
            id: id.clone(),
            user_id: user_id.to_string(),
            client_name: client_name.to_string(),
            protocol,
            me: me.clone(),
            manager: self.me.clone(),
            deps: self.deps.clone(),
            inner: Mutex::new(Inner {
                out: None,
                attach_id: 0,
                connected: false,
                resuming: false,
                timeout,
                backlog: Vec::new(),
                overflowed: false,
                closed: false,
                expiry: None,
                expiry_gen: 0,
                guilds: HashMap::new(),
            }),
            extras: AtomicBool::new(false),
            stop: CancellationToken::new(),
        });
        self.sessions.lock().insert(id, s.clone());
        self.deps.runtime.spawn(s.clone().updates());
        s
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().get(id).cloned()
    }

    /// The session `id` of `user_id`, if it exists and has no client.
    pub fn resume(&self, id: &str, user_id: &str) -> Option<Arc<Session>> {
        let s = self.get(id)?;
        (s.user_id == user_id && !s.is_connected()).then_some(s)
    }

    /// Forget a session and destroy its players in the background.
    pub(crate) fn remove(&self, id: &str) {
        let s = self.sessions.lock().remove(id);
        if let Some(s) = s {
            self.deps.runtime.spawn(async move { s.destroy().await });
        }
    }

    pub fn sessions(&self) -> Vec<Arc<Session>> {
        self.sessions.lock().values().cloned().collect()
    }

    /// `(players, playing players)`.
    pub fn counts(&self) -> (usize, usize) {
        let mut players = 0;
        let mut playing = 0;
        for s in self.sessions() {
            for g in s.guilds() {
                players += 1;
                if g.player.state().sounding {
                    playing += 1;
                }
            }
        }
        (players, playing)
    }

    /// Destroy every session, waiting until voice is closed.
    pub async fn shutdown(&self) {
        let all: Vec<Arc<Session>> = self.sessions.lock().drain().map(|(_, s)| s).collect();
        futures::future::join_all(all.iter().map(|s| s.destroy())).await;
    }
}
