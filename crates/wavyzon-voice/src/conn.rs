//! The voice gateway connection: dial, session loop, reconnects and teardown.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU16, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::{Interval, MissedTickBehavior, interval_at, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, info_span, warn};
use wavyzon_audio::FrameSource;
use wavyzon_audio::codec::{clamp_bitrate, new_encoder};

use crate::Error;
use crate::dave::{Dave, DaveOut, should_hold};
use crate::protocol::*;
use crate::sender::{self, SenderHandle, SenderParts};
use crate::transport::{Sealer, discover};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const MAX_RECONNECTS: u32 = 10;
pub(crate) const MAX_BACKOFF: Duration = Duration::from_secs(16);
const READ_LIMIT: usize = 1 << 20;

const CLOSE_NORMAL: u16 = 1000;
const CLOSE_GOING_AWAY: u16 = 1001;
const CLOSE_NO_STATUS: u16 = 1005;
const CLOSE_INTERNAL_ERROR: u16 = 1011;

/// Wait before reconnect attempt `attempt` (1-based): 1 s, 2 s, 4 s, 8 s, then 16 s.
pub(crate) fn reconnect_delay(attempt: u32) -> Duration {
    let exp = attempt.saturating_sub(1).min(5);
    MAX_BACKOFF.min(Duration::from_secs(1 << exp))
}

/// Close codes after which Discord will not take the session back.
pub(crate) fn terminal_reason(code: u16) -> Option<&'static str> {
    Some(match code {
        4001 => "unknown opcode",
        4002 => "failed to decode payload",
        4003 => "not authenticated",
        4004 => "authentication failed",
        4005 => "already authenticated",
        4006 => "session no longer valid",
        4011 => "server not found",
        4012 => "unknown protocol",
        4014 => "disconnected",
        4016 => "unknown encryption mode",
        4020 => "bad request",
        4021 => "rate limited",
        4022 => "call terminated",
        _ => return None,
    })
}

fn is_terminal(code: u16) -> bool {
    terminal_reason(code).is_some() || code == CLOSE_NORMAL
}

/// Everything needed to join a voice server, as relayed by the bot from
/// Discord's `VOICE_STATE_UPDATE` / `VOICE_SERVER_UPDATE`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Server {
    /// Guild id (snowflake).
    pub guild_id: String,
    /// The bot's user id (snowflake).
    pub user_id: String,
    /// Voice channel id (snowflake); also the DAVE MLS group id.
    pub channel_id: String,
    /// Voice session id.
    pub session_id: String,
    /// Voice server token.
    pub token: String,
    /// Voice server endpoint, e.g. `c-dfw09-1a2b.discord.media:443`. A
    /// `wss://` prefix is accepted; a `ws://` prefix dials plain WebSocket
    /// (local testing only).
    pub endpoint: String,
    /// Opus bitrate in bit/s; 0 means the default.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bitrate: i32,
}

fn is_zero(v: &i32) -> bool {
    *v == 0
}

impl Server {
    /// Whether every field needed to dial is present.
    pub fn valid(&self) -> bool {
        !self.session_id.is_empty()
            && !self.token.is_empty()
            && !self.endpoint.is_empty()
            && !self.guild_id.is_empty()
            && !self.user_id.is_empty()
            && !self.channel_id.is_empty()
    }

    /// The voice region from the endpoint host (`c-dfw09-…` → `dfw`).
    pub fn region(&self) -> String {
        let e = self.endpoint.as_str();
        let e = e
            .strip_prefix("wss://")
            .or_else(|| e.strip_prefix("ws://"))
            .unwrap_or(e);
        let host = e.split(':').next().unwrap_or("");
        let label = host.split('.').next().unwrap_or("");
        let label = label.strip_prefix("c-").unwrap_or(label);
        label.chars().take_while(|c| c.is_ascii_lowercase()).collect()
    }

    /// The gateway URL: `wss://<endpoint>/?v=8` (or `ws://…` for a `ws://` endpoint).
    pub fn gateway_url(&self) -> String {
        if let Some(rest) = self.endpoint.strip_prefix("ws://") {
            return format!("ws://{}/?v={GATEWAY_VERSION}", rest.trim_end_matches('/'));
        }
        let host = self.endpoint.strip_prefix("wss://").unwrap_or(&self.endpoint);
        format!("wss://{}/?v={GATEWAY_VERSION}", host.trim_end_matches('/'))
    }
}

/// A voice gateway close, reported to the `on_close` callback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseEvent {
    /// WebSocket close code (0 when the connection dropped without one).
    pub code: u16,
    /// Close reason.
    pub reason: String,
    /// Whether the remote side closed (always true for reported closes).
    pub by_remote: bool,
    /// The connection is dead and will not reconnect.
    pub terminal: bool,
}

type OnClose = Box<dyn Fn(&Conn, CloseEvent) + Send + Sync>;

enum Outgoing {
    Message(Message),
    Close(u16, String),
}

struct Udp {
    socket: Arc<UdpSocket>,
    remote: SocketAddr,
    ssrc: u32,
    drain: AbortHandle,
}

pub(crate) struct Inner {
    server: Server,
    pub(crate) source: Arc<dyn FrameSource>,
    on_close: OnClose,
    span: tracing::Span,
    cancel: CancellationToken,
    run: Mutex<Option<JoinHandle<()>>>,

    ws: Mutex<Option<UnboundedSender<Outgoing>>>,
    seq: AtomicI64,
    pub(crate) dave: Mutex<Option<Dave>>,
    udp: Mutex<Option<Udp>>,
    resumable: AtomicBool,
    healthy: AtomicBool,
    dead: AtomicBool,
    awaiting_ack: AtomicBool,
    ping_ns: AtomicU64,
    sent_at: Mutex<Option<Instant>>,
    bitrate: AtomicI32,
    sender: Mutex<Option<SenderHandle>>,
    dave_version: AtomicU16,
    pending: Mutex<HashMap<u16, u16>>,
}

/// A live voice connection. Cheap to clone; all clones are the same connection.
///
/// The connection keeps running (and reconnecting) until it dies on a
/// terminal close or [`Conn::close`] is awaited; dropping the handles does not
/// stop it.
#[derive(Clone)]
pub struct Conn(Arc<Inner>);

impl PartialEq for Conn {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for Conn {}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conn")
            .field("guild", &self.0.server.guild_id)
            .field("region", &self.0.server.region())
            .field("dead", &self.dead())
            .finish()
    }
}

/// Start a voice connection to `server` in the background and return at once.
///
/// The sender pulls a frame from `source` every 20 ms. `on_close` is called
/// from the connection's task on every gateway close: non-terminal ones are
/// followed by a reconnect (resuming when possible), terminal ones mean the
/// connection is dead and its resources are already released.
///
/// Must be called within a Tokio runtime.
pub fn dial<F>(server: Server, source: Arc<dyn FrameSource>, on_close: F) -> Result<Conn, Error>
where
    F: Fn(&Conn, CloseEvent) + Send + Sync + 'static,
{
    if !server.valid() {
        return Err(Error::IncompleteServer);
    }
    let rt = tokio::runtime::Handle::try_current().map_err(|_| Error::NoRuntime)?;
    // tokio-tungstenite's rustls enables no crypto provider of its own.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let span = info_span!("voice", guild = %server.guild_id, region = %server.region());
    let inner = Arc::new(Inner {
        bitrate: AtomicI32::new(clamp_bitrate(server.bitrate)),
        server,
        source,
        on_close: Box::new(on_close),
        span: span.clone(),
        cancel: CancellationToken::new(),
        run: Mutex::new(None),
        ws: Mutex::new(None),
        seq: AtomicI64::new(-1),
        dave: Mutex::new(None),
        udp: Mutex::new(None),
        resumable: AtomicBool::new(false),
        healthy: AtomicBool::new(false),
        dead: AtomicBool::new(false),
        awaiting_ack: AtomicBool::new(false),
        ping_ns: AtomicU64::new(0),
        sent_at: Mutex::new(None),
        sender: Mutex::new(None),
        dave_version: AtomicU16::new(0),
        pending: Mutex::new(HashMap::new()),
    });
    let task = rt.spawn(inner.clone().run().instrument(span));
    *inner.run.lock() = Some(task);
    Ok(Conn(inner))
}

impl Conn {
    /// The server state this connection was dialed with.
    pub fn server(&self) -> &Server {
        &self.0.server
    }

    /// Whether the connection died (terminal close or reconnects exhausted).
    pub fn dead(&self) -> bool {
        self.0.dead.load(Ordering::SeqCst)
    }

    /// Last heartbeat round-trip time (zero before the first ack).
    pub fn ping(&self) -> Duration {
        Duration::from_nanos(self.0.ping_ns.load(Ordering::Relaxed))
    }

    /// The DAVE protocol version currently in effect (0 = no E2EE).
    pub fn dave_protocol_version(&self) -> u16 {
        self.0.dave_version()
    }

    /// Change the Opus bitrate (clamped); the sender applies it on its next tick.
    pub fn set_bitrate(&self, bps: i32) {
        self.0.bitrate.store(clamp_bitrate(bps), Ordering::SeqCst);
        if let Some(s) = self.0.sender.lock().as_ref() {
            s.bitrate_changed.store(true, Ordering::SeqCst);
        }
    }

    /// Close the connection (normal closure) and wait until its task, sender
    /// thread, socket, encoder and DAVE session are gone. `on_close` is not
    /// called for a local close.
    pub async fn close(&self) {
        self.0.cancel.cancel();
        let task = self.0.run.lock().take();
        if let Some(task) = task {
            let _ = task.await;
        }
        self.0.teardown();
    }
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Ended {
    code: u16,
    reason: String,
    error: Option<String>,
}

impl Ended {
    fn new(code: u16, reason: impl Into<String>, error: Option<String>) -> Ended {
        Ended {
            code,
            reason: reason.into(),
            error,
        }
    }
}

impl Inner {
    pub(crate) fn bitrate(&self) -> i32 {
        self.bitrate.load(Ordering::SeqCst)
    }

    pub(crate) fn dave_version(&self) -> u16 {
        self.dave_version.load(Ordering::SeqCst)
    }

    /// Whether the sender must hold audio (E2EE call whose epoch is not ready).
    pub(crate) fn hold_audio(&self) -> bool {
        let version = self.dave_version();
        let dave = self.dave.lock();
        should_hold(dave.is_some(), version, dave.as_ref().is_some_and(Dave::ready))
    }

    fn set_dave_version(&self, v: u16, why: &str) {
        let old = self.dave_version.swap(v, Ordering::SeqCst);
        if old != v {
            info!(from = old, to = v, via = why, "dave protocol version changed");
        }
    }

    /// Drop the session (no resume) and let the run loop dial a new one.
    pub(crate) fn restart_fresh(&self, reason: &str) {
        warn!(reason, "restarting the voice session from scratch");
        self.resumable.store(false, Ordering::SeqCst);
        self.close_ws(CLOSE_GOING_AWAY, reason);
    }

    fn close_ws(&self, code: u16, reason: &str) {
        if let Some(tx) = self.ws.lock().as_ref() {
            let _ = tx.send(Outgoing::Close(code, reason.to_string()));
        }
    }

    fn write(&self, msg: Message) -> Result<(), Error> {
        match self.ws.lock().as_ref() {
            Some(tx) => tx.send(Outgoing::Message(msg)).map_err(|_| Error::NotConnected),
            None => Err(Error::NotConnected),
        }
    }

    /// Queue a JSON message on the current gateway session.
    pub(crate) fn send_json<T: Serialize>(&self, op: Opcode, d: &T) -> Result<(), Error> {
        let raw = serde_json::to_string(&Outbound { op, d })?;
        self.write(Message::Text(raw.into()))
    }

    fn send_dave(&self, out: Vec<DaveOut>) {
        for o in out {
            let res = match o {
                DaveOut::Binary(op, payload) => self.write(Message::Binary(encode_binary(op, &payload).into())),
                DaveOut::TransitionReady(id) => {
                    self.send_json(Opcode::DAVE_TRANSITION_READY, &TransitionId { transition_id: id })
                }
                DaveOut::InvalidCommitWelcome(id) => {
                    self.send_json(Opcode::DAVE_INVALID_COMMIT_WELCOME, &TransitionId { transition_id: id })
                }
            };
            if let Err(e) = res {
                warn!(error = %e, "dave message not sent");
            }
        }
    }

    fn stop_sender(&self) {
        let s = self.sender.lock().take();
        if let Some(s) = s {
            s.stop();
        }
    }

    fn close_udp(&self) {
        if let Some(udp) = self.udp.lock().take() {
            udp.drain.abort();
        }
    }

    fn teardown(&self) {
        self.stop_sender();
        self.close_udp();
        self.dave.lock().take();
    }

    fn emit_close(self: &Arc<Self>, ev: CloseEvent) {
        (self.on_close)(&Conn(self.clone()), ev);
    }

    async fn run(self: Arc<Self>) {
        let mut attempts = 0u32;
        while !self.cancel.is_cancelled() {
            let ended = self.session(self.resumable.load(Ordering::SeqCst)).await;
            if self.cancel.is_cancelled() {
                return;
            }
            if self.healthy.swap(false, Ordering::SeqCst) {
                attempts = 0;
            }
            let Ended { code, reason, error } = ended;
            if is_terminal(code) {
                warn!(code, reason, "voice gateway closed for good");
                self.dead.store(true, Ordering::SeqCst);
                self.teardown();
                self.emit_close(CloseEvent {
                    code,
                    reason,
                    by_remote: true,
                    terminal: true,
                });
                return;
            }
            attempts += 1;
            if attempts > MAX_RECONNECTS {
                error!(attempts = MAX_RECONNECTS, code, error = ?error, "voice gateway gave up reconnecting");
                self.dead.store(true, Ordering::SeqCst);
                self.teardown();
                let reason = format!("gave up after {MAX_RECONNECTS} reconnects: {}", error.unwrap_or(reason));
                self.emit_close(CloseEvent {
                    code,
                    reason,
                    by_remote: true,
                    terminal: true,
                });
                return;
            }
            let resume = self.resumable.load(Ordering::SeqCst);
            warn!(code, reason, error = ?error, attempt = attempts, resume, "voice gateway dropped, reconnecting");
            self.emit_close(CloseEvent {
                code,
                reason,
                by_remote: true,
                terminal: false,
            });
            if !resume {
                self.teardown();
            }
            tokio::select! {
                _ = tokio::time::sleep(reconnect_delay(attempts)) => {}
                _ = self.cancel.cancelled() => return,
            }
        }
    }

    async fn session(self: &Arc<Self>, resuming: bool) -> Ended {
        let config = WebSocketConfig::default()
            .max_message_size(Some(READ_LIMIT))
            .max_frame_size(Some(READ_LIMIT));
        let url = self.server.gateway_url();
        let connect = timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async_with_config(url, Some(config), false),
        );
        let ws = tokio::select! {
            r = connect => match r {
                Ok(Ok((ws, _))) => ws,
                Ok(Err(e)) => return Ended::new(0, "", Some(e.to_string())),
                Err(_) => return Ended::new(0, "", Some("voice gateway connect timed out".into())),
            },
            _ = self.cancel.cancelled() => return Ended::new(CLOSE_NORMAL, "", None),
        };
        let (tx, rx) = unbounded_channel();
        *self.ws.lock() = Some(tx);
        let ended = self.session_loop(ws, rx, resuming).await;
        *self.ws.lock() = None;
        ended
    }

    async fn session_loop(self: &Arc<Self>, mut ws: Ws, mut rx: UnboundedReceiver<Outgoing>, resuming: bool) -> Ended {
        let mut heartbeat: Option<Interval> = None;
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    close_ws(&mut ws, CLOSE_NORMAL, "").await;
                    return Ended::new(CLOSE_NORMAL, "", None);
                }
                Some(out) = rx.recv() => match out {
                    Outgoing::Message(m) => {
                        match timeout(WRITE_TIMEOUT, ws.send(m)).await {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => warn!(error = %e, "voice gateway write failed"),
                            Err(_) => warn!("voice gateway write timed out"),
                        }
                    }
                    Outgoing::Close(code, reason) => {
                        close_ws(&mut ws, code, &reason).await;
                        return Ended::new(code, reason, None);
                    }
                },
                _ = tick(&mut heartbeat) => {
                    if self.awaiting_ack.load(Ordering::SeqCst) {
                        warn!("voice heartbeat was not acknowledged, reconnecting");
                        close_ws(&mut ws, CLOSE_GOING_AWAY, "heartbeat timeout").await;
                        return Ended::new(CLOSE_GOING_AWAY, "heartbeat timeout", None);
                    }
                    *self.sent_at.lock() = Some(Instant::now());
                    self.awaiting_ack.store(true, Ordering::SeqCst);
                    let hb = Heartbeat { t: unix_ms(), seq_ack: self.seq.load(Ordering::SeqCst) };
                    if let Err(e) = self.send_json(Opcode::HEARTBEAT, &hb) {
                        warn!(error = %e, "heartbeat failed");
                    }
                }
                msg = ws.next() => {
                    let msg = match msg {
                        None => return Ended::new(0, "", Some("voice gateway connection closed".into())),
                        Some(Err(e)) => return Ended::new(0, "", Some(e.to_string())),
                        Some(Ok(m)) => m,
                    };
                    match msg {
                        Message::Close(frame) => {
                            let (code, reason) = frame.map_or((CLOSE_NO_STATUS, String::new()), |f| (u16::from(f.code), f.reason.to_string()));
                            return Ended::new(code, reason, None);
                        }
                        Message::Binary(raw) => self.binary(&raw),
                        Message::Text(text) => {
                            let m: Inbound = match serde_json::from_str(text.as_str()) {
                                Ok(m) => m,
                                Err(e) => {
                                    warn!(error = %e, "undecodable voice message");
                                    continue;
                                }
                            };
                            if let Some(s) = m.sequence() {
                                self.seq.store(s, Ordering::SeqCst);
                            }
                            let res = tokio::select! {
                                r = self.handle(&m, &mut heartbeat, resuming) => r,
                                _ = self.cancel.cancelled() => {
                                    close_ws(&mut ws, CLOSE_NORMAL, "").await;
                                    return Ended::new(CLOSE_NORMAL, "", None);
                                }
                            };
                            if let Err(e) = res {
                                close_ws(&mut ws, CLOSE_INTERNAL_ERROR, "setup failed").await;
                                return Ended::new(0, e.to_string(), Some(e.to_string()));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn handle(
        self: &Arc<Self>,
        m: &Inbound<'_>,
        heartbeat: &mut Option<Interval>,
        resuming: bool,
    ) -> Result<(), Error> {
        match m.op {
            Opcode::HELLO => {
                let h: Hello = m.data()?;
                self.awaiting_ack.store(false, Ordering::SeqCst);
                let every = Duration::from_secs_f64((h.heartbeat_interval / 1000.0).max(0.001));
                let mut iv = interval_at(tokio::time::Instant::now() + every, every);
                iv.set_missed_tick_behavior(MissedTickBehavior::Delay);
                *heartbeat = Some(iv);
                let s = &self.server;
                if resuming {
                    let r = Resume {
                        server_id: s.guild_id.clone(),
                        session_id: s.session_id.clone(),
                        token: s.token.clone(),
                        seq_ack: self.seq.load(Ordering::SeqCst),
                    };
                    return self.send_json(Opcode::RESUME, &r);
                }
                self.stop_sender();
                self.dave.lock().take();
                let dave = Dave::new(&s.user_id, &s.channel_id)?;
                *self.dave.lock() = Some(dave);
                let id = Identify {
                    server_id: s.guild_id.clone(),
                    user_id: s.user_id.clone(),
                    session_id: s.session_id.clone(),
                    token: s.token.clone(),
                    max_dave_protocol_version: Dave::max_protocol_version(),
                };
                self.send_json(Opcode::IDENTIFY, &id)
            }
            Opcode::READY => {
                let r: Ready = m.data()?;
                self.ready(r).await
            }
            Opcode::SESSION_DESCRIPTION => {
                let sd: SessionDescription = m.data()?;
                let mode = EncryptionMode::parse(&sd.mode).ok_or_else(|| Error::UnsupportedMode(sd.mode.clone()))?;
                let sealer = Sealer::new(mode, &sd.secret_key)?;
                let v = sd.dave_protocol_version;
                let out = self.dave.lock().as_mut().map(|d| d.on_select_protocol_ack(v));
                self.send_dave(out.unwrap_or_default());
                self.set_dave_version(v, "session description");
                self.resumable.store(true, Ordering::SeqCst);
                self.healthy.store(true, Ordering::SeqCst);
                info!(mode = %mode, dave = v, "voice session ready");
                self.start_sender(sealer)
            }
            Opcode::HEARTBEAT_ACK => {
                self.awaiting_ack.store(false, Ordering::SeqCst);
                if let Some(at) = *self.sent_at.lock() {
                    self.ping_ns.store(at.elapsed().as_nanos() as u64, Ordering::Relaxed);
                }
                Ok(())
            }
            Opcode::RESUMED => {
                self.healthy.store(true, Ordering::SeqCst);
                info!("voice session resumed");
                Ok(())
            }
            Opcode::CLIENTS_CONNECT => {
                if let Ok(cc) = m.data::<ClientsConnect>()
                    && let Some(d) = self.dave.lock().as_mut()
                {
                    cc.user_ids.iter().for_each(|id| d.add_user(id));
                }
                Ok(())
            }
            Opcode::CLIENT_DISCONNECT => {
                if let Ok(cd) = m.data::<ClientDisconnect>()
                    && let Some(d) = self.dave.lock().as_mut()
                {
                    d.remove_user(&cd.user_id);
                }
                Ok(())
            }
            Opcode::DAVE_PREPARE_TRANSITION => {
                if let Ok(t) = m.data::<Transition>() {
                    let out = self
                        .dave
                        .lock()
                        .as_mut()
                        .map(|d| d.on_prepare_transition(t.transition_id, t.protocol_version));
                    if let Some(out) = out {
                        self.send_dave(out);
                        let mut pending = self.pending.lock();
                        if pending.len() < 64 {
                            pending.insert(t.transition_id, t.protocol_version);
                        }
                    }
                }
                Ok(())
            }
            Opcode::DAVE_EXECUTE_TRANSITION => {
                if let Ok(t) = m.data::<Transition>() {
                    let had = self
                        .dave
                        .lock()
                        .as_mut()
                        .map(|d| d.on_execute_transition(t.transition_id))
                        .is_some();
                    if had {
                        let v = self.pending.lock().remove(&t.transition_id);
                        if let Some(v) = v {
                            self.set_dave_version(v, "transition");
                        }
                    }
                }
                Ok(())
            }
            Opcode::DAVE_PREPARE_EPOCH => {
                if let Ok(e) = m.data::<PrepareEpoch>() {
                    let out = self
                        .dave
                        .lock()
                        .as_mut()
                        .map(|d| d.on_prepare_epoch(e.epoch, e.protocol_version));
                    self.send_dave(out.unwrap_or_default());
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn binary(&self, raw: &[u8]) {
        let f = match parse_binary(raw) {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, "bad binary voice frame");
                return;
            }
        };
        self.seq.store(i64::from(f.seq), Ordering::SeqCst);
        let out = {
            let mut dave = self.dave.lock();
            let Some(d) = dave.as_mut() else { return };
            match f.op {
                Opcode::DAVE_EXTERNAL_SENDER => {
                    d.on_external_sender(&f.payload);
                    Vec::new()
                }
                Opcode::DAVE_PROPOSALS => d.on_proposals(&f.payload),
                Opcode::DAVE_ANNOUNCE_COMMIT => d.on_commit(f.transition_id, &f.payload),
                Opcode::DAVE_WELCOME => d.on_welcome(f.transition_id, &f.payload),
                op => {
                    debug!(op = op.0, "unhandled binary voice op");
                    Vec::new()
                }
            }
        };
        self.send_dave(out);
    }

    async fn ready(self: &Arc<Self>, r: Ready) -> Result<(), Error> {
        self.stop_sender();
        let mode = choose_mode(&r.modes)?;
        let addr = tokio::net::lookup_host((r.ip.as_str(), r.port))
            .await?
            .next()
            .ok_or_else(|| Error::Discovery(format!("cannot resolve {}", r.ip)))?;
        self.close_udp();
        let bind: SocketAddr = if addr.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        let socket = UdpSocket::bind(bind)?;
        let ssrc = r.ssrc;
        let (socket, found) = tokio::task::spawn_blocking(move || {
            let found = discover(&socket, addr, ssrc);
            (socket, found)
        })
        .await
        .map_err(|e| Error::Discovery(e.to_string()))?;
        let found = found?;
        if found.from != addr || found.attempts > 1 {
            info!(server = %addr, replied_from = %found.from, attempts = found.attempts, "ip discovery needed help");
        }
        // Drain whatever the server sends us so the receive buffer never fills.
        // The clone shares the file description, so the socket becomes
        // non-blocking for the sender too: a full send buffer drops a packet
        // instead of stalling the 20 ms loop.
        let drain = tokio::net::UdpSocket::from_std({
            let c = socket.try_clone()?;
            c.set_nonblocking(true)?;
            c
        })?;
        let drain = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while drain.recv_from(&mut buf).await.is_ok() {}
        })
        .abort_handle();
        *self.udp.lock() = Some(Udp {
            socket: Arc::new(socket),
            remote: addr,
            ssrc,
            drain,
        });
        let sp = SelectProtocol {
            protocol: "udp".into(),
            data: SelectProtocolData {
                address: found.ip,
                port: found.port,
                mode: mode.as_str().into(),
            },
        };
        self.send_json(Opcode::SELECT_PROTOCOL, &sp)
    }

    fn start_sender(self: &Arc<Self>, sealer: Sealer) -> Result<(), Error> {
        self.stop_sender();
        let encoder = new_encoder(self.bitrate())?;
        let (socket, remote, ssrc) = {
            let udp = self.udp.lock();
            let udp = udp.as_ref().ok_or(Error::NotConnected)?;
            (udp.socket.clone(), udp.remote, udp.ssrc)
        };
        let parts = SenderParts {
            sealer,
            encoder,
            socket,
            remote,
            ssrc,
        };
        let handle = sender::spawn(self.clone(), parts, self.span.clone())?;
        *self.sender.lock() = Some(handle);
        Ok(())
    }
}

async fn tick(iv: &mut Option<Interval>) {
    match iv {
        Some(iv) => {
            iv.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// Send a close frame and wait (briefly) for the peer's reply.
async fn close_ws(ws: &mut Ws, code: u16, reason: &str) {
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: reason.to_string().into(),
    };
    let _ = timeout(CLOSE_TIMEOUT, async {
        if ws.close(Some(frame)).await.is_err() {
            return;
        }
        while let Some(Ok(_)) = ws.next().await {}
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnects_outlast_a_short_network_outage() {
        let mut total = Duration::ZERO;
        for a in 1..=MAX_RECONNECTS {
            let d = reconnect_delay(a);
            assert!(d <= MAX_BACKOFF, "attempt {a} waits {d:?}, more than the cap");
            total += d;
        }
        assert!(
            total >= Duration::from_secs(90),
            "all reconnects span {total:?}; a 30-40 s outage must not exhaust them"
        );
        assert_eq!(reconnect_delay(1), Duration::from_secs(1));
        assert_eq!(reconnect_delay(5), MAX_BACKOFF);
    }

    #[test]
    fn region_from_endpoint() {
        let s = Server {
            endpoint: "c-dfw09-1a2b.discord.media:443".into(),
            ..Default::default()
        };
        assert_eq!(s.region(), "dfw");
        let s = Server {
            endpoint: "brazil11120.discord.media:443".into(),
            ..Default::default()
        };
        assert_eq!(s.region(), "brazil");
        let s = Server {
            endpoint: "wss://c-fra02-9.discord.media:443".into(),
            ..Default::default()
        };
        assert_eq!(s.region(), "fra");
    }

    #[test]
    fn gateway_url_is_v8() {
        let s = Server {
            endpoint: "c-dfw09-1a2b.discord.media:443".into(),
            ..Default::default()
        };
        assert_eq!(s.gateway_url(), "wss://c-dfw09-1a2b.discord.media:443/?v=8");
        let s = Server {
            endpoint: "wss://x.discord.media".into(),
            ..Default::default()
        };
        assert_eq!(s.gateway_url(), "wss://x.discord.media/?v=8");
        let s = Server {
            endpoint: "ws://127.0.0.1:9000".into(),
            ..Default::default()
        };
        assert_eq!(s.gateway_url(), "ws://127.0.0.1:9000/?v=8");
    }

    #[test]
    fn close_codes() {
        for c in [1000, 4004, 4006, 4014, 4022] {
            assert!(is_terminal(c), "{c}");
        }
        for c in [0, 1001, 1005, 1006, 4015, 4009] {
            assert!(!is_terminal(c), "{c}");
        }
    }

    #[test]
    fn server_json_shape() {
        let s: Server = serde_json::from_str(
            r#"{"guildId":"1","userId":"2","channelId":"3","sessionId":"s","token":"t","endpoint":"e"}"#,
        )
        .unwrap();
        assert!(s.valid());
        assert_eq!(s.bitrate, 0);
        assert!(
            !Server {
                token: String::new(),
                ..s.clone()
            }
            .valid()
        );
        let ev = serde_json::to_value(CloseEvent {
            code: 4014,
            reason: "x".into(),
            by_remote: true,
            terminal: true,
        })
        .unwrap();
        assert_eq!(ev["byRemote"], true);
    }

    #[test]
    fn dial_rejects_incomplete_servers() {
        struct Nothing;
        impl FrameSource for Nothing {
            fn next_frame(&self, _: &mut wavyzon_audio::Frame) -> Option<wavyzon_audio::FrameInfo> {
                None
            }
            fn sent(&self, _: wavyzon_audio::FrameInfo, _: Instant) {}
            fn stall(&self, _: Duration) {}
        }
        assert!(matches!(
            dial(Server::default(), Arc::new(Nothing), |_, _| {}),
            Err(Error::IncompleteServer)
        ));
        let s = Server {
            guild_id: "1".into(),
            user_id: "2".into(),
            channel_id: "3".into(),
            session_id: "s".into(),
            token: "t".into(),
            endpoint: "ws://127.0.0.1:1".into(),
            bitrate: 0,
        };
        assert!(matches!(dial(s, Arc::new(Nothing), |_, _| {}), Err(Error::NoRuntime)));
    }
}
