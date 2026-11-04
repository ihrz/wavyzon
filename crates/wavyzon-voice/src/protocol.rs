//! Voice gateway v8 wire format: opcodes, JSON payloads and DAVE binary frames.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::Error;

/// A voice gateway opcode (JSON `op` field, or the opcode byte of a binary frame).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Opcode(pub u8);

#[allow(missing_docs)]
impl Opcode {
    pub const IDENTIFY: Opcode = Opcode(0);
    pub const SELECT_PROTOCOL: Opcode = Opcode(1);
    pub const READY: Opcode = Opcode(2);
    pub const HEARTBEAT: Opcode = Opcode(3);
    pub const SESSION_DESCRIPTION: Opcode = Opcode(4);
    pub const SPEAKING: Opcode = Opcode(5);
    pub const HEARTBEAT_ACK: Opcode = Opcode(6);
    pub const RESUME: Opcode = Opcode(7);
    pub const HELLO: Opcode = Opcode(8);
    pub const RESUMED: Opcode = Opcode(9);
    pub const CLIENTS_CONNECT: Opcode = Opcode(11);
    pub const CLIENT_DISCONNECT: Opcode = Opcode(13);
    pub const DAVE_PREPARE_TRANSITION: Opcode = Opcode(21);
    pub const DAVE_EXECUTE_TRANSITION: Opcode = Opcode(22);
    pub const DAVE_TRANSITION_READY: Opcode = Opcode(23);
    pub const DAVE_PREPARE_EPOCH: Opcode = Opcode(24);
    pub const DAVE_EXTERNAL_SENDER: Opcode = Opcode(25);
    pub const DAVE_KEY_PACKAGE: Opcode = Opcode(26);
    pub const DAVE_PROPOSALS: Opcode = Opcode(27);
    pub const DAVE_COMMIT_WELCOME: Opcode = Opcode(28);
    pub const DAVE_ANNOUNCE_COMMIT: Opcode = Opcode(29);
    pub const DAVE_WELCOME: Opcode = Opcode(30);
    pub const DAVE_INVALID_COMMIT_WELCOME: Opcode = Opcode(31);
}

/// The voice gateway version this crate speaks (`?v=8`).
pub const GATEWAY_VERSION: u8 = 8;

/// A transport encryption mode. Only the two non-deprecated AEAD modes are
/// ever selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EncryptionMode {
    /// `aead_aes256_gcm_rtpsize` (preferred).
    Aes256Gcm,
    /// `aead_xchacha20_poly1305_rtpsize` (fallback).
    XChaCha20Poly1305,
}

impl EncryptionMode {
    /// The mode's name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            EncryptionMode::Aes256Gcm => "aead_aes256_gcm_rtpsize",
            EncryptionMode::XChaCha20Poly1305 => "aead_xchacha20_poly1305_rtpsize",
        }
    }

    /// Parse a wire name; deprecated and unknown modes give `None`.
    pub fn parse(s: &str) -> Option<EncryptionMode> {
        match s {
            "aead_aes256_gcm_rtpsize" => Some(EncryptionMode::Aes256Gcm),
            "aead_xchacha20_poly1305_rtpsize" => Some(EncryptionMode::XChaCha20Poly1305),
            _ => None,
        }
    }
}

impl std::fmt::Display for EncryptionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Pick AES-256-GCM if offered, else XChaCha20-Poly1305, else fail.
pub fn choose_mode<S: AsRef<str>>(offered: &[S]) -> Result<EncryptionMode, Error> {
    let has = |m: EncryptionMode| offered.iter().any(|o| o.as_ref() == m.as_str());
    if has(EncryptionMode::Aes256Gcm) {
        return Ok(EncryptionMode::Aes256Gcm);
    }
    if has(EncryptionMode::XChaCha20Poly1305) {
        return Ok(EncryptionMode::XChaCha20Poly1305);
    }
    Err(Error::NoSupportedMode)
}

#[derive(Debug, Deserialize)]
pub(crate) struct Inbound<'a> {
    pub op: Opcode,
    #[serde(borrow, default)]
    pub d: Option<&'a RawValue>,
    #[serde(default)]
    pub s: Option<i64>,
    #[serde(default)]
    pub seq: Option<i64>,
}

impl Inbound<'_> {
    /// The sequence number, whichever spelling (`seq` or `s`) the gateway used.
    pub fn sequence(&self) -> Option<i64> {
        self.seq.or(self.s)
    }

    pub fn data<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_str(self.d.map(RawValue::get).unwrap_or("null"))
    }
}

#[derive(Serialize)]
pub(crate) struct Outbound<T: Serialize> {
    pub op: Opcode,
    pub d: T,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct Identify {
    pub server_id: String,
    pub user_id: String,
    pub session_id: String,
    pub token: String,
    pub max_dave_protocol_version: u16,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct Resume {
    pub server_id: String,
    pub session_id: String,
    pub token: String,
    pub seq_ack: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Heartbeat {
    pub t: i64,
    pub seq_ack: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Hello {
    pub heartbeat_interval: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Ready {
    pub ssrc: u32,
    pub ip: String,
    pub port: u16,
    #[serde(default)]
    pub modes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SelectProtocol {
    pub protocol: String,
    pub data: SelectProtocolData,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SelectProtocolData {
    pub address: String,
    pub port: u16,
    pub mode: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SessionDescription {
    pub mode: String,
    pub secret_key: Vec<u8>,
    #[serde(default)]
    pub dave_protocol_version: u16,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Speaking {
    pub speaking: u8,
    pub delay: u32,
    pub ssrc: u32,
}

/// The `speaking` flag for regular voice audio.
pub const SPEAKING_MICROPHONE: u8 = 1;

#[derive(Debug, Deserialize)]
pub(crate) struct ClientsConnect {
    pub user_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClientDisconnect {
    pub user_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Transition {
    pub transition_id: u16,
    #[serde(default)]
    pub protocol_version: u16,
}

#[derive(Debug, Serialize)]
pub(crate) struct TransitionId {
    pub transition_id: u16,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PrepareEpoch {
    pub epoch: u64,
    pub protocol_version: u16,
}

/// A server-to-client binary (DAVE) frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryFrame {
    /// Gateway sequence number.
    pub seq: u16,
    /// DAVE opcode.
    pub op: Opcode,
    /// Transition id, present for announce-commit (29) and welcome (30).
    pub transition_id: u16,
    /// The rest of the frame.
    pub payload: Vec<u8>,
}

/// Parse a server binary frame: `seq u16 | op u8 | [transition_id u16] | payload`.
pub fn parse_binary(raw: &[u8]) -> Result<BinaryFrame, Error> {
    if raw.len() < 3 {
        return Err(Error::ShortFrame);
    }
    let mut f = BinaryFrame {
        seq: u16::from_be_bytes([raw[0], raw[1]]),
        op: Opcode(raw[2]),
        transition_id: 0,
        payload: Vec::new(),
    };
    let mut body = &raw[3..];
    if f.op == Opcode::DAVE_ANNOUNCE_COMMIT || f.op == Opcode::DAVE_WELCOME {
        if body.len() < 2 {
            return Err(Error::ShortFrame);
        }
        f.transition_id = u16::from_be_bytes([body[0], body[1]]);
        body = &body[2..];
    }
    f.payload = body.to_vec();
    Ok(f)
}

/// Encode a client binary frame: `op u8 | payload` (clients send no sequence).
pub fn encode_binary(op: Opcode, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + payload.len());
    out.push(op.0);
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choose_mode_prefers_aes_gcm() {
        let m = choose_mode(&[
            "xsalsa20_poly1305",
            "aead_xchacha20_poly1305_rtpsize",
            "aead_aes256_gcm_rtpsize",
        ])
        .unwrap();
        assert_eq!(m, EncryptionMode::Aes256Gcm);
        let m = choose_mode(&["aead_xchacha20_poly1305_rtpsize"]).unwrap();
        assert_eq!(m, EncryptionMode::XChaCha20Poly1305);
        assert!(
            choose_mode(&["xsalsa20_poly1305_lite"]).is_err(),
            "deprecated mode accepted"
        );
    }

    #[test]
    fn parse_binary_commit_carries_transition() {
        let raw = [0x00, 0x05, Opcode::DAVE_ANNOUNCE_COMMIT.0, 0x01, 0x02, 0xAA, 0xBB];
        let f = parse_binary(&raw).unwrap();
        assert_eq!(
            (f.seq, f.transition_id, f.payload.as_slice()),
            (5, 0x0102, &[0xAA, 0xBB][..])
        );
        let f = parse_binary(&[0, 1, Opcode::DAVE_EXTERNAL_SENDER.0, 9]).unwrap();
        assert_eq!((f.transition_id, f.payload.as_slice()), (0, &[9][..]));
        assert!(
            parse_binary(&[0, 1, Opcode::DAVE_WELCOME.0, 1]).is_err(),
            "short welcome accepted"
        );
        assert!(parse_binary(&[0, 1]).is_err());
    }

    #[test]
    fn encode_binary_has_no_sequence() {
        assert_eq!(encode_binary(Opcode::DAVE_KEY_PACKAGE, &[1, 2]), vec![26, 1, 2]);
    }

    #[test]
    fn inbound_sequence_accepts_both_spellings() {
        let a: Inbound = serde_json::from_str(r#"{"op":5,"d":{},"seq":12}"#).unwrap();
        let b: Inbound = serde_json::from_str(r#"{"op":5,"d":{},"s":7}"#).unwrap();
        let c: Inbound = serde_json::from_str(r#"{"op":6,"d":null}"#).unwrap();
        assert_eq!(a.sequence(), Some(12));
        assert_eq!(b.sequence(), Some(7));
        assert_eq!(c.sequence(), None);
    }

    #[test]
    fn session_description_reads_the_key_array() {
        let m: Inbound = serde_json::from_str(
            r#"{"op":4,"d":{"mode":"aead_aes256_gcm_rtpsize","secret_key":[1,2,3],"dave_protocol_version":1}}"#,
        )
        .unwrap();
        let sd: SessionDescription = m.data().unwrap();
        assert_eq!((sd.secret_key, sd.dave_protocol_version), (vec![1, 2, 3], 1));
    }
}
