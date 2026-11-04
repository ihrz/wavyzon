//! # wavyzon-voice
//!
//! Discord voice for Wavyzon, ported from Cadence's `internal/voice`:
//!
//! - voice gateway **v8** over WSS with `seq_ack` heartbeats, resume,
//!   exponential-backoff reconnects and terminal/non-terminal close codes;
//! - UDP IP discovery, RTP, and `aead_aes256_gcm_rtpsize` transport
//!   encryption with `aead_xchacha20_poly1305_rtpsize` as the fallback (the
//!   deprecated modes are never selected);
//! - **DAVE** end-to-end encryption through [`davey`](https://docs.rs/davey)
//!   (cargo feature `dave`, on by default). When a call negotiates E2EE, audio
//!   is held (never sent unencrypted) until the epoch is ready; a hold of 15 s
//!   restarts the voice session. Protocol 0 calls play straight away;
//! - a send loop on a dedicated OS thread with drift-free 20 ms pacing.
//!
//! Each [`Conn`] owns its Opus encoder, sender thread, UDP socket and DAVE
//! session, and tears them down together.

mod conn;
mod dave;
pub mod protocol;
mod sender;
pub mod transport;

pub use conn::{CloseEvent, Conn, Server, dial};
pub use dave::dave_available;
pub use protocol::{EncryptionMode, GATEWAY_VERSION, Opcode, choose_mode};
pub use wavyzon_audio::{FrameInfo, FrameSource};

/// Errors from dialing and running a voice connection.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// [`Server::valid`] is false.
    #[error("incomplete voice server state")]
    IncompleteServer,
    /// [`dial`] was called outside a Tokio runtime.
    #[error("voice dial needs a running Tokio runtime")]
    NoRuntime,
    /// Built without the `dave` feature; Discord requires DAVE and will reject voice.
    #[error("wavyzon-voice was built without the `dave` feature, so DAVE is unavailable and Discord will reject voice")]
    NoDave,
    /// The voice server offered no non-deprecated encryption mode.
    #[error("voice server offered no supported encryption mode")]
    NoSupportedMode,
    /// The session description named a mode we never selected.
    #[error("unsupported encryption mode {0:?}")]
    UnsupportedMode(String),
    /// The transport key has the wrong length.
    #[error("bad transport key length {0}")]
    BadKey(usize),
    /// A packet too short to open.
    #[error("packet too short")]
    ShortPacket,
    /// AEAD open failed.
    #[error("packet decryption failed")]
    Decrypt,
    /// A binary gateway frame too short for its opcode.
    #[error("binary voice frame too short")]
    ShortFrame,
    /// Malformed IP discovery response.
    #[error("invalid ip discovery response")]
    BadDiscovery,
    /// IP discovery failed or timed out.
    #[error("ip discovery: {0}")]
    Discovery(String),
    /// A user or channel id is not a snowflake.
    #[error("voice {0} id {1:?} is not a snowflake")]
    BadSnowflake(&'static str, String),
    /// The DAVE session has no key yet.
    #[error("DAVE session is not ready")]
    DaveNotReady,
    /// DAVE encryption failed.
    #[error("DAVE encrypt: {0}")]
    DaveEncrypt(String),
    /// No gateway session is connected.
    #[error("voice gateway not connected")]
    NotConnected,
    /// Opus encoder error.
    #[error(transparent)]
    Codec(#[from] wavyzon_audio::codec::CodecError),
    /// WebSocket error.
    #[error("voice gateway: {0}")]
    WebSocket(String),
    /// JSON error.
    #[error("voice payload: {0}")]
    Json(#[from] serde_json::Error),
    /// Socket error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
