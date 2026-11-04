//! [`MockVoiceGateway`]: an in-process fake Discord voice server.
//!
//! It runs a plain WebSocket server (`ws://127.0.0.1:<port>`) speaking the
//! voice gateway **v8** JSON protocol and a UDP socket that answers IP
//! discovery and decrypts the RTP packets it receives.
//!
//! The default script is the happy path Discord follows:
//!
//! ```text
//! client                         mock
//!   | <------ Hello (8) {heartbeat_interval}
//!   | Identify (0) ------>
//!   | <------ Ready (2) {ssrc, ip, port, modes}
//!   | UDP discovery req ------>  (UDP)
//!   | <------ UDP discovery resp {your ip:port}
//!   | SelectProtocol (1) ------>
//!   | <------ SessionDescription (4) {mode, secret_key, dave_protocol_version: 0}
//!   | Speaking (5) ------>        (recorded)
//!   | RTP (sealed) ------>        (UDP, decrypted and recorded)
//!   | Heartbeat (3) {t, seq_ack} ------>
//!   | <------ HeartbeatACK (6) {t}
//!   | ... reconnect ...
//!   | Resume (7) ------>
//!   | <------ Resumed (9)
//! ```
//!
//! Faults ([`Faults`]) bend that script (withhold heartbeat ACKs, ignore
//! discovery, drop RTP, stall the handshake, reject resumes), and
//! [`MockVoiceGateway::close`], [`MockVoiceGateway::send_op`],
//! [`MockVoiceGateway::send_binary_op`] and [`MockVoiceGateway::set_hook`]
//! let a test script anything else — including DAVE opcodes later on.
//!
//! ```no_run
//! # async fn demo() {
//! use std::time::Duration;
//! use wavyzon_testkit::gateway::MockVoiceGateway;
//!
//! let gw = MockVoiceGateway::start().await.unwrap();
//! // Hand `gw.endpoint()` ("ws://127.0.0.1:PORT") to the voice connection as
//! // the voice server endpoint, then:
//! let packets = gw.wait_for_packets(50, Duration::from_secs(3)).await.unwrap();
//! assert!(packets.iter().all(|p| p.ssrc == gw.ssrc()));
//! gw.close(4014, "disconnected").await; // terminal close code
//! # }
//! ```

pub mod wire;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message};

use crate::WaitTimeout;
use wire::{PacketOpener, RtpHeader, TransportMode};

/// Voice gateway opcodes (v8, including DAVE).
pub mod op {
    /// Client → server: start a new session.
    pub const IDENTIFY: u8 = 0;
    /// Client → server: chosen UDP address and encryption mode.
    pub const SELECT_PROTOCOL: u8 = 1;
    /// Server → client: SSRC, UDP address and offered modes.
    pub const READY: u8 = 2;
    /// Client → server: keep-alive `{t, seq_ack}`.
    pub const HEARTBEAT: u8 = 3;
    /// Server → client: mode and secret key.
    pub const SESSION_DESCRIPTION: u8 = 4;
    /// Both ways: speaking flags.
    pub const SPEAKING: u8 = 5;
    /// Server → client: heartbeat acknowledged `{t}`.
    pub const HEARTBEAT_ACK: u8 = 6;
    /// Client → server: resume a session.
    pub const RESUME: u8 = 7;
    /// Server → client: first message, heartbeat interval.
    pub const HELLO: u8 = 8;
    /// Server → client: resume succeeded.
    pub const RESUMED: u8 = 9;
    /// Server → client: users joined `{user_ids}`.
    pub const CLIENTS_CONNECT: u8 = 11;
    /// Server → client: a user left `{user_id}`.
    pub const CLIENT_DISCONNECT: u8 = 13;
    /// DAVE: prepare a protocol transition.
    pub const DAVE_PREPARE_TRANSITION: u8 = 21;
    /// DAVE: execute a protocol transition.
    pub const DAVE_EXECUTE_TRANSITION: u8 = 22;
    /// DAVE: client is ready for a transition.
    pub const DAVE_TRANSITION_READY: u8 = 23;
    /// DAVE: prepare a new MLS epoch.
    pub const DAVE_PREPARE_EPOCH: u8 = 24;
    /// DAVE (binary): MLS external sender.
    pub const DAVE_EXTERNAL_SENDER: u8 = 25;
    /// DAVE (binary, client): MLS key package.
    pub const DAVE_KEY_PACKAGE: u8 = 26;
    /// DAVE (binary): MLS proposals.
    pub const DAVE_PROPOSALS: u8 = 27;
    /// DAVE (binary, client): MLS commit + welcome.
    pub const DAVE_COMMIT_WELCOME: u8 = 28;
    /// DAVE (binary): announce commit transition.
    pub const DAVE_ANNOUNCE_COMMIT: u8 = 29;
    /// DAVE (binary): MLS welcome.
    pub const DAVE_WELCOME: u8 = 30;
    /// DAVE (client): invalid commit/welcome.
    pub const DAVE_INVALID_COMMIT_WELCOME: u8 = 31;
}

/// Voice gateway close codes the mock uses.
pub mod close_code {
    /// Unknown opcode.
    pub const UNKNOWN_OPCODE: u16 = 4001;
    /// Failed to decode payload.
    pub const DECODE_ERROR: u16 = 4002;
    /// Not authenticated (a message before Identify).
    pub const NOT_AUTHENTICATED: u16 = 4003;
    /// Authentication failed (wrong token).
    pub const AUTHENTICATION_FAILED: u16 = 4004;
    /// Already authenticated (second Identify).
    pub const ALREADY_AUTHENTICATED: u16 = 4005;
    /// Session no longer valid (rejected Resume).
    pub const SESSION_INVALID: u16 = 4006;
    /// Session timeout.
    pub const SESSION_TIMEOUT: u16 = 4009;
    /// Server not found.
    pub const SERVER_NOT_FOUND: u16 = 4011;
    /// Unknown protocol (SelectProtocol.protocol != "udp").
    pub const UNKNOWN_PROTOCOL: u16 = 4012;
    /// Disconnected (kicked / channel deleted) — terminal.
    pub const DISCONNECTED: u16 = 4014;
    /// Voice server crashed — resumable.
    pub const VOICE_SERVER_CRASHED: u16 = 4015;
    /// Unknown encryption mode.
    pub const UNKNOWN_ENCRYPTION_MODE: u16 = 4016;
}

/// Static behaviour of a [`MockVoiceGateway`].
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// SSRC assigned in Ready.
    pub ssrc: u32,
    /// `heartbeat_interval` sent in Hello, in ms.
    pub heartbeat_interval_ms: f64,
    /// Modes offered in Ready (strings, so tests can offer unknown ones).
    pub modes: Vec<String>,
    /// The session key sent in SessionDescription.
    pub secret_key: [u8; 32],
    /// When set, Identify/Resume with another token closes with 4004/4006.
    pub expected_token: Option<String>,
    /// `dave_protocol_version` in SessionDescription (keep `0`: no DAVE).
    pub dave_protocol_version: u16,
    /// `ip` sent in Ready (where the client sends UDP). Defaults to `127.0.0.1`.
    pub ready_ip: String,
    /// IP reported back in discovery; `None` echoes the sender's IP.
    pub discovery_ip: Option<String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        let mut secret_key = [0u8; 32];
        for (i, b) in secret_key.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        Self {
            ssrc: 0x5EED_0001,
            heartbeat_interval_ms: 13_750.0,
            modes: TransportMode::ALL.iter().map(|m| m.as_str().to_string()).collect(),
            secret_key,
            expected_token: None,
            dave_protocol_version: 0,
            ready_ip: "127.0.0.1".into(),
            discovery_ip: None,
        }
    }
}

/// Runtime fault injection, changed with [`MockVoiceGateway::update_faults`].
/// The default injects nothing.
#[derive(Debug, Clone, Default)]
pub struct Faults {
    /// Do not answer heartbeats (the client should detect a zombie link).
    pub withhold_heartbeat_acks: bool,
    /// Delay every scripted reply (Ready, SessionDescription, ACK, Resumed).
    pub reply_delay: Duration,
    /// Ignore this many upcoming IP discovery requests (client must resend).
    pub ignore_discovery: u32,
    /// Discard every received RTP packet (counted in `dropped_packets`).
    pub drop_all_rtp: bool,
    /// Discard every n-th received RTP packet (`0` = none).
    pub drop_every_nth_rtp: u32,
    /// Close Resume attempts with 4006 instead of sending Resumed.
    pub reject_resume: bool,
    /// Never answer Identify with Ready (handshake stall).
    pub stall_after_identify: bool,
    /// Never answer SelectProtocol with SessionDescription.
    pub stall_after_select_protocol: bool,
}

/// A message the mock can send to the connected client.
#[derive(Debug, Clone, PartialEq)]
pub enum GatewayMessage {
    /// `{"op": op, "d": d, "seq": n}`; `seq` is added for every op except
    /// Hello and HeartbeatACK.
    Json {
        /// Opcode.
        op: u8,
        /// Payload.
        d: Value,
    },
    /// A raw text frame, sent as is.
    Text(String),
    /// A binary frame `[seq u16 BE][op][payload]` (DAVE MLS framing).
    Binary {
        /// Opcode.
        op: u8,
        /// Payload after the opcode.
        payload: Vec<u8>,
    },
    /// A raw binary frame, sent as is.
    RawBinary(Vec<u8>),
    /// Close the WebSocket with `code`.
    Close {
        /// Close code (`4014`, `4006`, `1000`…).
        code: u16,
        /// Close reason.
        reason: String,
    },
    /// Drop the TCP connection without a close frame (network loss).
    Abort,
}

/// One message received from the client.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientMessage {
    /// When it arrived.
    pub at: Instant,
    /// Which connection (1-based, in accept order) it arrived on.
    pub connection: usize,
    /// Opcode (`op` field, or the first byte of a binary frame).
    pub op: u8,
    /// The `d` field (`Null` for binary frames).
    pub d: Value,
    /// Payload after the opcode byte, for binary frames.
    pub binary: Option<Vec<u8>>,
}

/// A Speaking (op 5) message received from the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeakingEvent {
    /// When it arrived.
    pub at: Instant,
    /// Speaking flags (1 = microphone, 0 = stopped).
    pub speaking: u64,
    /// `delay` field.
    pub delay: u64,
    /// `ssrc` field.
    pub ssrc: u32,
}

/// A Heartbeat (op 3) received from the client.
#[derive(Debug, Clone, PartialEq)]
pub struct HeartbeatEvent {
    /// When it arrived.
    pub at: Instant,
    /// The nonce (`d.t`, or `d` itself for pre-v8 clients).
    pub nonce: Value,
    /// `d.seq_ack`, if present.
    pub seq_ack: Option<i64>,
}

/// A decrypted RTP packet received on the UDP socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedPacket {
    /// When it arrived.
    pub at: Instant,
    /// The sender's UDP address.
    pub from: SocketAddr,
    /// RTP sequence number.
    pub sequence: u16,
    /// RTP timestamp.
    pub timestamp: u32,
    /// RTP SSRC.
    pub ssrc: u32,
    /// RTP payload type (`0x78`).
    pub payload_type: u8,
    /// AEAD nonce counter.
    pub nonce: u32,
    /// The Opus payload.
    pub opus: Vec<u8>,
}

impl ReceivedPacket {
    /// Whether the payload is the 3-byte Opus silence frame `F8 FF FE`.
    pub fn is_opus_silence(&self) -> bool {
        self.opus == [0xF8, 0xFF, 0xFE]
    }
}

/// A close observed on the gateway WebSocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseRecord {
    /// When it happened.
    pub at: Instant,
    /// Connection number.
    pub connection: usize,
    /// Close code, if the closing side sent one.
    pub code: Option<u16>,
    /// Close reason.
    pub reason: String,
    /// `true` if the mock closed, `false` if the client did.
    pub by_server: bool,
}

/// Hook called with every client message, before the default script runs;
/// whatever it returns is sent right away.
pub type Hook = Arc<dyn Fn(&ClientMessage) -> Vec<GatewayMessage> + Send + Sync>;

#[derive(Default)]
struct State {
    faults: Faults,
    identify: Option<Value>,
    resume: Option<Value>,
    select_protocol: Option<Value>,
    session_id: Option<String>,
    token: Option<String>,
    seq: i64,
    messages: Vec<ClientMessage>,
    speaking: Vec<SpeakingEvent>,
    heartbeats: Vec<HeartbeatEvent>,
    packets: Vec<ReceivedPacket>,
    rtp_seen: u64,
    dropped: u64,
    undecryptable: u64,
    discovery_requests: u64,
    opener: Option<Arc<PacketOpener>>,
    mode: Option<TransportMode>,
    sessions: usize,
    resumes: usize,
    connections: usize,
    connect_uris: Vec<String>,
    closes: Vec<CloseRecord>,
    current: Option<(usize, mpsc::UnboundedSender<GatewayMessage>)>,
    hook: Option<Hook>,
}

struct Inner {
    config: GatewayConfig,
    udp_addr: SocketAddr,
    state: Mutex<State>,
}

/// An in-process fake Discord voice server (WebSocket + UDP). See the
/// [module docs](self). Background tasks stop when it is dropped.
pub struct MockVoiceGateway {
    inner: Arc<Inner>,
    ws_addr: SocketAddr,
    tasks: Vec<AbortHandle>,
}

impl Drop for MockVoiceGateway {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        if let Some((_, tx)) = self.inner.state.lock().current.take() {
            let _ = tx.send(GatewayMessage::Abort);
        }
    }
}

impl MockVoiceGateway {
    /// Start with the default [`GatewayConfig`] on random local ports.
    pub async fn start() -> std::io::Result<Self> {
        Self::start_with(GatewayConfig::default()).await
    }

    /// Start with `config` on random local ports. Must run inside a Tokio
    /// runtime.
    pub async fn start_with(config: GatewayConfig) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let ws_addr = listener.local_addr()?;
        let udp = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let udp_addr = udp.local_addr()?;
        let inner = Arc::new(Inner {
            config,
            udp_addr,
            state: Mutex::new(State::default()),
        });

        let accept = {
            let inner = inner.clone();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let id = {
                        let mut st = inner.state.lock();
                        st.connections += 1;
                        st.connections
                    };
                    tokio::spawn(connection(inner.clone(), stream, id));
                }
            })
        };
        let udp_task = tokio::spawn(udp_loop(inner.clone(), udp));
        Ok(Self {
            inner,
            ws_addr,
            tasks: vec![accept.abort_handle(), udp_task.abort_handle()],
        })
    }

    // ----- addresses and config -------------------------------------------

    /// The voice server endpoint to hand to the voice connection:
    /// `ws://127.0.0.1:PORT` (append `/?v=8` when dialing).
    pub fn endpoint(&self) -> String {
        format!("ws://{}", self.ws_addr)
    }

    /// `127.0.0.1:PORT`, the scheme-less form Discord puts in
    /// `VOICE_SERVER_UPDATE.endpoint`.
    pub fn host_port(&self) -> String {
        self.ws_addr.to_string()
    }

    /// The full URL a v8 client dials: `ws://127.0.0.1:PORT/?v=8`.
    pub fn ws_url(&self) -> String {
        format!("ws://{}/?v=8", self.ws_addr)
    }

    /// The WebSocket listener address.
    pub fn ws_addr(&self) -> SocketAddr {
        self.ws_addr
    }

    /// The UDP address announced in Ready.
    pub fn udp_addr(&self) -> SocketAddr {
        self.inner.udp_addr
    }

    /// The SSRC announced in Ready.
    pub fn ssrc(&self) -> u32 {
        self.inner.config.ssrc
    }

    /// The secret key sent in SessionDescription.
    pub fn secret_key(&self) -> [u8; 32] {
        self.inner.config.secret_key
    }

    /// The configuration this mock was started with.
    pub fn config(&self) -> &GatewayConfig {
        &self.inner.config
    }

    // ----- inspection ------------------------------------------------------

    /// The `d` of the last Identify.
    pub fn identify_payload(&self) -> Option<Value> {
        self.inner.state.lock().identify.clone()
    }

    /// The `d` of the last Resume.
    pub fn resume_payload(&self) -> Option<Value> {
        self.inner.state.lock().resume.clone()
    }

    /// The `d` of the last SelectProtocol.
    pub fn select_protocol_payload(&self) -> Option<Value> {
        self.inner.state.lock().select_protocol.clone()
    }

    /// The mode negotiated by the last SelectProtocol.
    pub fn negotiated_mode(&self) -> Option<TransportMode> {
        self.inner.state.lock().mode
    }

    /// Every message received from clients, in order.
    pub fn client_messages(&self) -> Vec<ClientMessage> {
        self.inner.state.lock().messages.clone()
    }

    /// Received messages with opcode `op`.
    pub fn messages_with_op(&self, op: u8) -> Vec<ClientMessage> {
        self.inner
            .state
            .lock()
            .messages
            .iter()
            .filter(|m| m.op == op)
            .cloned()
            .collect()
    }

    /// Decrypted RTP packets, in arrival order.
    pub fn received_packets(&self) -> Vec<ReceivedPacket> {
        self.inner.state.lock().packets.clone()
    }

    /// Number of decrypted RTP packets.
    pub fn packet_count(&self) -> usize {
        self.inner.state.lock().packets.len()
    }

    /// RTP packets discarded by [`Faults`].
    pub fn dropped_packets(&self) -> u64 {
        self.inner.state.lock().dropped
    }

    /// RTP packets that failed to decrypt (wrong key/mode, before
    /// SessionDescription, corrupted).
    pub fn undecryptable_packets(&self) -> u64 {
        self.inner.state.lock().undecryptable
    }

    /// IP discovery requests received (including ignored ones).
    pub fn discovery_requests(&self) -> u64 {
        self.inner.state.lock().discovery_requests
    }

    /// Speaking messages received.
    pub fn speaking_events(&self) -> Vec<SpeakingEvent> {
        self.inner.state.lock().speaking.clone()
    }

    /// Heartbeats received.
    pub fn heartbeats(&self) -> Vec<HeartbeatEvent> {
        self.inner.state.lock().heartbeats.clone()
    }

    /// WebSocket connections accepted so far.
    pub fn connection_count(&self) -> usize {
        self.inner.state.lock().connections
    }

    /// Request URIs of accepted connections (`/?v=8`).
    pub fn connect_uris(&self) -> Vec<String> {
        self.inner.state.lock().connect_uris.clone()
    }

    /// Whether a client is currently connected.
    pub fn is_connected(&self) -> bool {
        self.inner.state.lock().current.is_some()
    }

    /// SessionDescriptions sent (fresh sessions established).
    pub fn sessions_established(&self) -> usize {
        self.inner.state.lock().sessions
    }

    /// Resumed messages sent (successful resumes).
    pub fn resumes(&self) -> usize {
        self.inner.state.lock().resumes
    }

    /// Closes seen on the gateway (by either side).
    pub fn closes(&self) -> Vec<CloseRecord> {
        self.inner.state.lock().closes.clone()
    }

    /// The last `seq` the mock sent.
    pub fn last_seq(&self) -> i64 {
        self.inner.state.lock().seq
    }

    /// Forget recorded packets, messages, speaking events and heartbeats
    /// (session state is kept).
    pub fn clear_recordings(&self) {
        let mut st = self.inner.state.lock();
        st.messages.clear();
        st.packets.clear();
        st.speaking.clear();
        st.heartbeats.clear();
    }

    // ----- control ---------------------------------------------------------

    /// Change fault injection, e.g.
    /// `gw.update_faults(|f| f.withhold_heartbeat_acks = true)`.
    pub fn update_faults(&self, f: impl FnOnce(&mut Faults)) {
        f(&mut self.inner.state.lock().faults);
    }

    /// Install a hook run on every client message (see [`Hook`]). Use it to
    /// script replies the default flow does not know (DAVE…).
    pub fn set_hook(&self, hook: impl Fn(&ClientMessage) -> Vec<GatewayMessage> + Send + Sync + 'static) {
        self.inner.state.lock().hook = Some(Arc::new(hook));
    }

    /// Remove the hook.
    pub fn clear_hook(&self) {
        self.inner.state.lock().hook = None;
    }

    /// Send a message to the current client. Returns `false` if none is
    /// connected.
    pub async fn send(&self, msg: GatewayMessage) -> bool {
        match &self.inner.state.lock().current {
            Some((_, tx)) => tx.send(msg).is_ok(),
            None => false,
        }
    }

    /// Send `{"op": op, "d": d, "seq": n}` to the current client.
    pub async fn send_op(&self, op: u8, d: Value) -> bool {
        self.send(GatewayMessage::Json { op, d }).await
    }

    /// Send a binary `[seq][op][payload]` frame (DAVE MLS framing).
    pub async fn send_binary_op(&self, op: u8, payload: Vec<u8>) -> bool {
        self.send(GatewayMessage::Binary { op, payload }).await
    }

    /// Send Clients Connect (op 11).
    pub async fn send_clients_connect(&self, user_ids: &[&str]) -> bool {
        self.send_op(op::CLIENTS_CONNECT, json!({ "user_ids": user_ids })).await
    }

    /// Send Client Disconnect (op 13).
    pub async fn send_client_disconnect(&self, user_id: &str) -> bool {
        self.send_op(op::CLIENT_DISCONNECT, json!({ "user_id": user_id })).await
    }

    /// Force-close the current connection with `code` (e.g. `4014`
    /// terminal, `4015` resumable, `4006` session invalid).
    pub async fn close(&self, code: u16, reason: &str) -> bool {
        self.send(GatewayMessage::Close {
            code,
            reason: reason.into(),
        })
        .await
    }

    /// Drop the TCP connection without a close frame.
    pub async fn abort_connection(&self) -> bool {
        self.send(GatewayMessage::Abort).await
    }

    // ----- waiting ---------------------------------------------------------

    /// Wait until `cond(self)` holds, polling every 5 ms.
    pub async fn wait_until(
        &self,
        timeout: Duration,
        what: &str,
        cond: impl Fn(&Self) -> bool,
    ) -> Result<(), WaitTimeout> {
        crate::poll_until(timeout, what, || cond(self)).await
    }

    /// Wait until at least `n` packets were decrypted; returns all of them.
    pub async fn wait_for_packets(&self, n: usize, timeout: Duration) -> Result<Vec<ReceivedPacket>, WaitTimeout> {
        self.wait_until(timeout, &format!("{n} RTP packets"), |g| g.packet_count() >= n)
            .await?;
        Ok(self.received_packets())
    }

    /// Wait for the `nth` (1-based) client message with opcode `op`.
    pub async fn wait_for_op_count(&self, op: u8, nth: usize, timeout: Duration) -> Result<ClientMessage, WaitTimeout> {
        self.wait_until(timeout, &format!("client op {op} #{nth}"), |g| {
            g.messages_with_op(op).len() >= nth
        })
        .await?;
        Ok(self.messages_with_op(op)[nth - 1].clone())
    }

    /// Wait for the first client message with opcode `op`.
    pub async fn wait_for_op(&self, op: u8, timeout: Duration) -> Result<ClientMessage, WaitTimeout> {
        self.wait_for_op_count(op, 1, timeout).await
    }

    /// Wait until `n` sessions were established (SessionDescription sent).
    pub async fn wait_for_sessions(&self, n: usize, timeout: Duration) -> Result<(), WaitTimeout> {
        self.wait_until(timeout, &format!("{n} sessions"), |g| g.sessions_established() >= n)
            .await
    }

    /// Wait until `n` Speaking messages arrived.
    pub async fn wait_for_speaking(&self, n: usize, timeout: Duration) -> Result<Vec<SpeakingEvent>, WaitTimeout> {
        self.wait_until(timeout, &format!("{n} speaking events"), |g| {
            g.speaking_events().len() >= n
        })
        .await?;
        Ok(self.speaking_events())
    }
}

// ----- WebSocket side --------------------------------------------------------

fn next_seq(inner: &Inner) -> i64 {
    let mut st = inner.state.lock();
    st.seq += 1;
    st.seq
}

fn render(inner: &Inner, msg: GatewayMessage) -> Message {
    match msg {
        GatewayMessage::Json { op, d } => {
            let v = if op == op::HELLO || op == op::HEARTBEAT_ACK {
                json!({ "op": op, "d": d })
            } else {
                json!({ "op": op, "d": d, "seq": next_seq(inner) })
            };
            Message::Text(v.to_string().into())
        }
        GatewayMessage::Text(t) => Message::Text(t.into()),
        GatewayMessage::Binary { op, payload } => {
            let seq = next_seq(inner) as u16;
            Message::Binary(wire::encode_server_binary(seq, op, &payload).into())
        }
        GatewayMessage::RawBinary(b) => Message::Binary(b.into()),
        GatewayMessage::Close { code, reason } => Message::Close(Some(CloseFrame {
            code: CloseCode::from(code),
            reason: reason.into(),
        })),
        GatewayMessage::Abort => unreachable!("handled by the connection loop"),
    }
}

struct Conn {
    id: usize,
    authed: bool,
    identified: bool,
}

// The handshake callback's error type is tungstenite's, not ours.
#[allow(clippy::result_large_err)]
async fn connection(inner: Arc<Inner>, stream: TcpStream, id: usize) {
    let uri = Arc::new(Mutex::new(String::new()));
    let cb = {
        let uri = uri.clone();
        move |req: &tungstenite::handshake::server::Request, resp| {
            *uri.lock() = req.uri().to_string();
            Ok(resp)
        }
    };
    let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, cb).await else {
        return;
    };
    let (mut sink, mut source) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<GatewayMessage>();
    {
        let mut st = inner.state.lock();
        st.connect_uris.push(uri.lock().clone());
        st.current = Some((id, tx.clone()));
    }
    let hello = GatewayMessage::Json {
        op: op::HELLO,
        d: json!({ "heartbeat_interval": inner.config.heartbeat_interval_ms }),
    };
    if sink.send(render(&inner, hello)).await.is_err() {
        return;
    }
    let mut conn = Conn {
        id,
        authed: false,
        identified: false,
    };
    let mut server_closed = false;
    loop {
        tokio::select! {
            out = rx.recv() => {
                let Some(out) = out else { break };
                match out {
                    GatewayMessage::Abort => break,
                    GatewayMessage::Close { code, reason } => {
                        inner.state.lock().closes.push(CloseRecord {
                            at: Instant::now(), connection: id, code: Some(code), reason: reason.clone(), by_server: true,
                        });
                        let _ = sink.send(render(&inner, GatewayMessage::Close { code, reason })).await;
                        server_closed = true;
                        break;
                    }
                    other => {
                        if sink.send(render(&inner, other)).await.is_err() {
                            break;
                        }
                    }
                }
            }
            frame = source.next() => {
                let replies = match frame {
                    Some(Ok(Message::Text(t))) => handle_text(&inner, &mut conn, t.as_str()),
                    Some(Ok(Message::Binary(b))) => handle_binary(&inner, &conn, &b),
                    Some(Ok(Message::Close(frame))) => {
                        inner.state.lock().closes.push(CloseRecord {
                            at: Instant::now(),
                            connection: id,
                            code: frame.as_ref().map(|f| u16::from(f.code)),
                            reason: frame.map(|f| f.reason.to_string()).unwrap_or_default(),
                            by_server: false,
                        });
                        break;
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => break,
                };
                let delay = inner.state.lock().faults.reply_delay;
                for r in replies {
                    // Closes go through the channel so they are recorded and
                    // ordered after earlier replies.
                    if delay.is_zero() && !matches!(r, GatewayMessage::Close { .. } | GatewayMessage::Abort) {
                        if sink.send(render(&inner, r)).await.is_err() {
                            break;
                        }
                    } else if delay.is_zero() {
                        let _ = tx.send(r);
                    } else {
                        let tx = tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            let _ = tx.send(r);
                        });
                    }
                }
            }
        }
    }
    {
        let mut st = inner.state.lock();
        if st.current.as_ref().is_some_and(|(cid, _)| *cid == id) {
            st.current = None;
        }
    }
    if server_closed {
        // Let the client read the close frame and answer it.
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(Ok(_)) = source.next().await {}
        })
        .await;
    }
}

fn record(inner: &Inner, msg: ClientMessage) -> Vec<GatewayMessage> {
    let hook = {
        let mut st = inner.state.lock();
        st.messages.push(msg.clone());
        st.hook.clone()
    };
    hook.map(|h| h(&msg)).unwrap_or_default()
}

fn handle_binary(inner: &Inner, conn: &Conn, raw: &[u8]) -> Vec<GatewayMessage> {
    let Some((&op, payload)) = raw.split_first() else {
        return vec![];
    };
    record(
        inner,
        ClientMessage {
            at: Instant::now(),
            connection: conn.id,
            op,
            d: Value::Null,
            binary: Some(payload.to_vec()),
        },
    )
}

fn close(code: u16, reason: &str) -> GatewayMessage {
    GatewayMessage::Close {
        code,
        reason: reason.into(),
    }
}

fn str_field<'a>(d: &'a Value, key: &str) -> Option<&'a str> {
    d.get(key).and_then(Value::as_str)
}

fn handle_text(inner: &Inner, conn: &mut Conn, text: &str) -> Vec<GatewayMessage> {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return vec![close(close_code::DECODE_ERROR, "failed to decode payload")];
    };
    let Some(opcode) = v.get("op").and_then(Value::as_u64) else {
        return vec![close(close_code::DECODE_ERROR, "failed to decode payload")];
    };
    let op = opcode as u8;
    let d = v.get("d").cloned().unwrap_or(Value::Null);
    let mut out = record(
        inner,
        ClientMessage {
            at: Instant::now(),
            connection: conn.id,
            op,
            d: d.clone(),
            binary: None,
        },
    );
    let cfg = &inner.config;
    let mut st = inner.state.lock();
    match op {
        op::IDENTIFY => {
            if conn.identified {
                out.push(close(close_code::ALREADY_AUTHENTICATED, "already authenticated"));
                return out;
            }
            let fields = ["server_id", "user_id", "session_id", "token"];
            if fields.iter().any(|k| str_field(&d, k).is_none()) {
                out.push(close(close_code::DECODE_ERROR, "failed to decode payload"));
                return out;
            }
            let token = str_field(&d, "token").unwrap_or_default().to_string();
            if cfg.expected_token.as_ref().is_some_and(|t| *t != token) {
                out.push(close(close_code::AUTHENTICATION_FAILED, "authentication failed"));
                return out;
            }
            conn.identified = true;
            conn.authed = true;
            st.identify = Some(d.clone());
            st.session_id = str_field(&d, "session_id").map(str::to_string);
            st.token = Some(token);
            st.seq = 0;
            st.opener = None;
            st.mode = None;
            if !st.faults.stall_after_identify {
                out.push(GatewayMessage::Json {
                    op: op::READY,
                    d: json!({
                        "ssrc": cfg.ssrc,
                        "ip": cfg.ready_ip,
                        "port": inner.udp_addr.port(),
                        "modes": cfg.modes,
                        "experiments": [],
                    }),
                });
            }
        }
        op::SELECT_PROTOCOL => {
            if !conn.authed {
                out.push(close(close_code::NOT_AUTHENTICATED, "not authenticated"));
                return out;
            }
            if str_field(&d, "protocol") != Some("udp") {
                out.push(close(close_code::UNKNOWN_PROTOCOL, "unknown protocol"));
                return out;
            }
            let data = d.get("data").cloned().unwrap_or(Value::Null);
            let mode_str = str_field(&data, "mode").unwrap_or_default().to_string();
            let mode = TransportMode::parse(&mode_str).filter(|_| cfg.modes.contains(&mode_str));
            let Some(mode) = mode else {
                out.push(close(close_code::UNKNOWN_ENCRYPTION_MODE, "unknown encryption mode"));
                return out;
            };
            st.select_protocol = Some(d.clone());
            st.mode = Some(mode);
            st.opener = Some(Arc::new(PacketOpener::new(mode, &cfg.secret_key)));
            if !st.faults.stall_after_select_protocol {
                st.sessions += 1;
                out.push(GatewayMessage::Json {
                    op: op::SESSION_DESCRIPTION,
                    d: json!({
                        "mode": mode.as_str(),
                        "secret_key": cfg.secret_key.to_vec(),
                        "audio_codec": "opus",
                        "media_session_id": format!("mock-{}", conn.id),
                        "dave_protocol_version": cfg.dave_protocol_version,
                    }),
                });
            }
        }
        op::HEARTBEAT => {
            let (nonce, seq_ack) = match &d {
                Value::Object(m) => (
                    m.get("t").cloned().unwrap_or(Value::Null),
                    m.get("seq_ack").and_then(Value::as_i64),
                ),
                other => (other.clone(), None),
            };
            st.heartbeats.push(HeartbeatEvent {
                at: Instant::now(),
                nonce: nonce.clone(),
                seq_ack,
            });
            if !st.faults.withhold_heartbeat_acks {
                out.push(GatewayMessage::Json {
                    op: op::HEARTBEAT_ACK,
                    d: json!({ "t": nonce }),
                });
            }
        }
        op::SPEAKING => {
            if !conn.authed {
                out.push(close(close_code::NOT_AUTHENTICATED, "not authenticated"));
                return out;
            }
            st.speaking.push(SpeakingEvent {
                at: Instant::now(),
                speaking: d.get("speaking").and_then(Value::as_u64).unwrap_or(0),
                delay: d.get("delay").and_then(Value::as_u64).unwrap_or(0),
                ssrc: d.get("ssrc").and_then(Value::as_u64).unwrap_or(0) as u32,
            });
        }
        op::RESUME => {
            st.resume = Some(d.clone());
            let known = st.session_id.is_some() && st.session_id.as_deref() == str_field(&d, "session_id");
            let token_ok = st.token.as_deref().is_none_or(|t| Some(t) == str_field(&d, "token"));
            if st.faults.reject_resume || !known || !token_ok {
                out.push(close(close_code::SESSION_INVALID, "session no longer valid"));
                return out;
            }
            conn.authed = true;
            st.resumes += 1;
            out.push(GatewayMessage::Json {
                op: op::RESUMED,
                d: Value::Null,
            });
        }
        _ => {}
    }
    out
}

// ----- UDP side --------------------------------------------------------------

async fn udp_loop(inner: Arc<Inner>, udp: Arc<UdpSocket>) {
    let mut buf = vec![0u8; 2048];
    loop {
        let Ok((n, from)) = udp.recv_from(&mut buf).await else {
            continue;
        };
        let pkt = &buf[..n];
        let discovery = if n == wire::DISCOVERY_LENGTH {
            wire::parse_discovery(pkt).ok()
        } else {
            None
        };
        if let Some((1, ssrc, _, _)) = discovery {
            let reply = {
                let mut st = inner.state.lock();
                st.discovery_requests += 1;
                if st.faults.ignore_discovery > 0 {
                    st.faults.ignore_discovery -= 1;
                    None
                } else {
                    let ip = inner
                        .config
                        .discovery_ip
                        .clone()
                        .unwrap_or_else(|| from.ip().to_string());
                    Some(wire::discovery_response(ssrc, &ip, from.port()))
                }
            };
            if let Some(reply) = reply {
                let _ = udp.send_to(&reply, from).await;
            }
            continue;
        }
        if RtpHeader::parse(pkt).is_err() {
            continue;
        }
        let at = Instant::now();
        let mut st = inner.state.lock();
        st.rtp_seen += 1;
        let nth = st.faults.drop_every_nth_rtp as u64;
        if st.faults.drop_all_rtp || (nth > 0 && st.rtp_seen % nth == 0) {
            st.dropped += 1;
            continue;
        }
        match st.opener.as_ref().map(|o| o.open(pkt)) {
            Some(Ok(p)) => st.packets.push(ReceivedPacket {
                at,
                from,
                sequence: p.header.sequence,
                timestamp: p.header.timestamp,
                ssrc: p.header.ssrc,
                payload_type: p.header.payload_type,
                nonce: p.nonce,
                opus: p.payload,
            }),
            _ => st.undecryptable += 1,
        }
    }
}
