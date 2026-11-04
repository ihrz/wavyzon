//! Wire-level building blocks of the Discord voice transport: RTP headers,
//! UDP IP discovery packets and the two `*_rtpsize` AEAD transport modes.
//!
//! The mock gateway uses these to *receive*; they are public so tests can also
//! *send* (a scripted client) or check bytes produced by `wavyzon-voice`.
//!
//! # `rtpsize` layout
//!
//! ```text
//! | RTP header (12 B + 4·CC + 4 if X) | ciphertext (ext body + opus) | tag (16 B) | nonce counter (4 B, BE) |
//! \_____________ AAD ______________/
//! ```
//!
//! The AEAD nonce is the 4-byte big-endian counter followed by zeroes
//! (12 bytes for AES-256-GCM, 24 bytes for XChaCha20-Poly1305).

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::XChaCha20Poly1305;

/// Size of a fixed RTP header.
pub const RTP_HEADER_SIZE: usize = 12;
/// RTP version 2, no padding, no extension, no CSRC.
pub const RTP_VERSION_FLAGS: u8 = 0x80;
/// The payload type Discord uses for Opus.
pub const RTP_PAYLOAD_TYPE_OPUS: u8 = 0x78;
/// Length of an IP discovery packet (request and response).
pub const DISCOVERY_LENGTH: usize = 74;
/// Bytes of nonce counter appended to every `rtpsize` packet.
pub const NONCE_SUFFIX: usize = 4;
/// AEAD tag size of both supported modes.
pub const TAG_SIZE: usize = 16;

/// The voice transport encryption modes Wavyzon supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportMode {
    /// `aead_aes256_gcm_rtpsize` (preferred by Discord and Cadence).
    Aes256Gcm,
    /// `aead_xchacha20_poly1305_rtpsize` (mandatory fallback).
    XChaCha20Poly1305,
}

impl TransportMode {
    /// Both modes, in Discord's preference order.
    pub const ALL: [TransportMode; 2] = [TransportMode::Aes256Gcm, TransportMode::XChaCha20Poly1305];

    /// The protocol string used in `Ready.modes` and `SelectProtocol.data.mode`.
    pub fn as_str(self) -> &'static str {
        match self {
            TransportMode::Aes256Gcm => "aead_aes256_gcm_rtpsize",
            TransportMode::XChaCha20Poly1305 => "aead_xchacha20_poly1305_rtpsize",
        }
    }

    /// Parse a protocol string; `None` for unknown or deprecated modes.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "aead_aes256_gcm_rtpsize" => Some(TransportMode::Aes256Gcm),
            "aead_xchacha20_poly1305_rtpsize" => Some(TransportMode::XChaCha20Poly1305),
            _ => None,
        }
    }

    fn nonce(self, counter: u32) -> Vec<u8> {
        let mut n = vec![0u8; if self == TransportMode::Aes256Gcm { 12 } else { 24 }];
        n[..4].copy_from_slice(&counter.to_be_bytes());
        n
    }
}

impl std::fmt::Display for TransportMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a packet could not be parsed or opened.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    /// Fewer bytes than the layout requires.
    #[error("packet too short ({0} bytes)")]
    TooShort(usize),
    /// Not an RTP version 2 packet.
    #[error("not an RTP v2 packet (first byte {0:#04x})")]
    NotRtp(u8),
    /// AEAD authentication failed (wrong key, mode, AAD or nonce).
    #[error("decryption failed")]
    Decrypt,
    /// Not a valid IP discovery packet.
    #[error("invalid ip discovery packet")]
    Discovery,
}

/// A parsed RTP header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    /// RTP version (always 2 for valid packets).
    pub version: u8,
    /// Padding bit.
    pub padding: bool,
    /// Extension bit (`X`).
    pub extension: bool,
    /// Number of CSRC identifiers.
    pub csrc_count: u8,
    /// Marker bit.
    pub marker: bool,
    /// Payload type (`0x78` for Discord Opus).
    pub payload_type: u8,
    /// Sequence number.
    pub sequence: u16,
    /// RTP timestamp (48 kHz units).
    pub timestamp: u32,
    /// Synchronisation source.
    pub ssrc: u32,
    /// Bytes of the unencrypted part under `rtpsize`: the fixed header, the
    /// CSRCs and, with `X` set, the 4-byte extension *header*.
    pub aad_len: usize,
}

impl RtpHeader {
    /// Parse the header at the start of `packet`.
    pub fn parse(packet: &[u8]) -> Result<Self, WireError> {
        if packet.len() < RTP_HEADER_SIZE {
            return Err(WireError::TooShort(packet.len()));
        }
        let b0 = packet[0];
        if b0 >> 6 != 2 {
            return Err(WireError::NotRtp(b0));
        }
        let csrc_count = b0 & 0x0f;
        let extension = b0 & 0x10 != 0;
        let aad_len = RTP_HEADER_SIZE + 4 * csrc_count as usize + if extension { 4 } else { 0 };
        if packet.len() < aad_len {
            return Err(WireError::TooShort(packet.len()));
        }
        Ok(Self {
            version: 2,
            padding: b0 & 0x20 != 0,
            extension,
            csrc_count,
            marker: packet[1] & 0x80 != 0,
            payload_type: packet[1] & 0x7f,
            sequence: u16::from_be_bytes([packet[2], packet[3]]),
            timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
            ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
            aad_len,
        })
    }

    /// Number of 32-bit words of extension body, read from the extension
    /// header (`0` without extension).
    pub fn extension_words(&self, packet: &[u8]) -> usize {
        if !self.extension || packet.len() < self.aad_len {
            return 0;
        }
        u16::from_be_bytes([packet[self.aad_len - 2], packet[self.aad_len - 1]]) as usize
    }
}

/// Build the 12-byte header Discord expects (`0x80 0x78 seq ts ssrc`).
pub fn rtp_header(sequence: u16, timestamp: u32, ssrc: u32) -> [u8; RTP_HEADER_SIZE] {
    let mut h = [0u8; RTP_HEADER_SIZE];
    h[0] = RTP_VERSION_FLAGS;
    h[1] = RTP_PAYLOAD_TYPE_OPUS;
    h[2..4].copy_from_slice(&sequence.to_be_bytes());
    h[4..8].copy_from_slice(&timestamp.to_be_bytes());
    h[8..12].copy_from_slice(&ssrc.to_be_bytes());
    h
}

enum Cipher {
    Gcm(Box<Aes256Gcm>),
    XChaCha(Box<XChaCha20Poly1305>),
}

impl Cipher {
    fn new(mode: TransportMode, key: &[u8; 32]) -> Self {
        match mode {
            TransportMode::Aes256Gcm => Cipher::Gcm(Box::new(Aes256Gcm::new(key.into()))),
            TransportMode::XChaCha20Poly1305 => Cipher::XChaCha(Box::new(XChaCha20Poly1305::new(key.into()))),
        }
    }

    fn seal(&self, nonce: &[u8], msg: &[u8], aad: &[u8]) -> Vec<u8> {
        let p = Payload { msg, aad };
        match self {
            Cipher::Gcm(c) => c.encrypt(nonce.into(), p),
            Cipher::XChaCha(c) => c.encrypt(nonce.into(), p),
        }
        .expect("AEAD encryption cannot fail for in-range lengths")
    }

    fn open(&self, nonce: &[u8], msg: &[u8], aad: &[u8]) -> Result<Vec<u8>, WireError> {
        let p = Payload { msg, aad };
        match self {
            Cipher::Gcm(c) => c.decrypt(nonce.into(), p),
            Cipher::XChaCha(c) => c.decrypt(nonce.into(), p),
        }
        .map_err(|_| WireError::Decrypt)
    }
}

/// A decrypted voice packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedPacket {
    /// The RTP header.
    pub header: RtpHeader,
    /// The nonce counter carried in the last 4 bytes.
    pub nonce: u32,
    /// Decrypted RTP header extension body (empty without `X`).
    pub extension: Vec<u8>,
    /// The Opus payload (decrypted, extension body stripped).
    pub payload: Vec<u8>,
}

/// Decrypts `rtpsize` voice packets (what the mock gateway does on receive).
pub struct PacketOpener {
    mode: TransportMode,
    cipher: Cipher,
}

impl PacketOpener {
    /// An opener for `mode` with the 32-byte session `key`.
    pub fn new(mode: TransportMode, key: &[u8; 32]) -> Self {
        Self {
            mode,
            cipher: Cipher::new(mode, key),
        }
    }

    /// The mode this opener decrypts.
    pub fn mode(&self) -> TransportMode {
        self.mode
    }

    /// Parse and decrypt one packet.
    pub fn open(&self, packet: &[u8]) -> Result<OpenedPacket, WireError> {
        let header = RtpHeader::parse(packet)?;
        if packet.len() < header.aad_len + TAG_SIZE + NONCE_SUFFIX {
            return Err(WireError::TooShort(packet.len()));
        }
        let split = packet.len() - NONCE_SUFFIX;
        let nonce = u32::from_be_bytes(packet[split..].try_into().expect("4 bytes"));
        let aad = &packet[..header.aad_len];
        let mut plain = self
            .cipher
            .open(&self.mode.nonce(nonce), &packet[header.aad_len..split], aad)?;
        let ext_len = (header.extension_words(packet) * 4).min(plain.len());
        let payload = plain.split_off(ext_len);
        Ok(OpenedPacket {
            header,
            nonce,
            extension: plain,
            payload,
        })
    }
}

/// Encrypts `rtpsize` voice packets the way a Discord client must, with an
/// incrementing nonce counter. Used by scripted clients in tests.
pub struct PacketSealer {
    mode: TransportMode,
    cipher: Cipher,
    counter: u32,
}

impl PacketSealer {
    /// A sealer for `mode` with the 32-byte session `key`, counter at 0.
    pub fn new(mode: TransportMode, key: &[u8; 32]) -> Self {
        Self {
            mode,
            cipher: Cipher::new(mode, key),
            counter: 0,
        }
    }

    /// Seal `payload` behind `header` (which becomes the AAD), returning the
    /// full UDP datagram.
    pub fn seal(&mut self, header: &[u8], payload: &[u8]) -> Vec<u8> {
        let nonce = self.counter;
        self.counter = self.counter.wrapping_add(1);
        let mut out = Vec::with_capacity(header.len() + payload.len() + TAG_SIZE + NONCE_SUFFIX);
        out.extend_from_slice(header);
        out.extend_from_slice(&self.cipher.seal(&self.mode.nonce(nonce), payload, header));
        out.extend_from_slice(&nonce.to_be_bytes());
        out
    }
}

/// An IP discovery request for `ssrc` (type `0x1`, length 70).
pub fn discovery_request(ssrc: u32) -> [u8; DISCOVERY_LENGTH] {
    let mut p = [0u8; DISCOVERY_LENGTH];
    p[0..2].copy_from_slice(&1u16.to_be_bytes());
    p[2..4].copy_from_slice(&70u16.to_be_bytes());
    p[4..8].copy_from_slice(&ssrc.to_be_bytes());
    p
}

/// An IP discovery response (type `0x2`) telling the client its external
/// address is `ip:port`.
pub fn discovery_response(ssrc: u32, ip: &str, port: u16) -> [u8; DISCOVERY_LENGTH] {
    let mut p = [0u8; DISCOVERY_LENGTH];
    p[0..2].copy_from_slice(&2u16.to_be_bytes());
    p[2..4].copy_from_slice(&70u16.to_be_bytes());
    p[4..8].copy_from_slice(&ssrc.to_be_bytes());
    let ip = ip.as_bytes();
    let n = ip.len().min(63);
    p[8..8 + n].copy_from_slice(&ip[..n]);
    p[72..74].copy_from_slice(&port.to_be_bytes());
    p
}

/// Parse a discovery packet: `(type, ssrc, ip, port)`.
pub fn parse_discovery(p: &[u8]) -> Result<(u16, u32, String, u16), WireError> {
    if p.len() < DISCOVERY_LENGTH {
        return Err(WireError::Discovery);
    }
    let kind = u16::from_be_bytes([p[0], p[1]]);
    if !(kind == 1 || kind == 2) || u16::from_be_bytes([p[2], p[3]]) != 70 {
        return Err(WireError::Discovery);
    }
    let ssrc = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
    let addr = &p[8..72];
    let end = addr.iter().position(|b| *b == 0).unwrap_or(addr.len());
    let ip = String::from_utf8_lossy(&addr[..end]).into_owned();
    Ok((kind, ssrc, ip, u16::from_be_bytes([p[72], p[73]])))
}

/// Encode a client → server binary gateway message (`[op][payload]`).
pub fn encode_client_binary(op: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + payload.len());
    out.push(op);
    out.extend_from_slice(payload);
    out
}

/// Encode a server → client binary gateway message (`[seq u16 BE][op][payload]`),
/// the framing DAVE MLS opcodes 25, 27, 29 and 30 use.
pub fn encode_server_binary(seq: u16, op: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + payload.len());
    out.extend_from_slice(&seq.to_be_bytes());
    out.push(op);
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_both_modes() {
        for mode in TransportMode::ALL {
            let key = [7u8; 32];
            let mut s = PacketSealer::new(mode, &key);
            let o = PacketOpener::new(mode, &key);
            for i in 0..3u16 {
                let h = rtp_header(i, i as u32 * 960, 42);
                let pkt = s.seal(&h, &[1, 2, 3, i as u8]);
                let got = o.open(&pkt).unwrap();
                assert_eq!(got.payload, vec![1, 2, 3, i as u8]);
                assert_eq!(got.nonce, i as u32);
                assert_eq!(got.header.sequence, i);
                assert_eq!(got.header.ssrc, 42);
            }
            let mut pkt = s.seal(&rtp_header(9, 0, 42), &[5]);
            pkt[1] ^= 1; // tamper with the AAD
            assert_eq!(o.open(&pkt), Err(WireError::Decrypt));
        }
    }

    #[test]
    fn wrong_mode_or_key_fails() {
        let mut s = PacketSealer::new(TransportMode::Aes256Gcm, &[1; 32]);
        let pkt = s.seal(&rtp_header(0, 0, 1), b"opus");
        assert!(
            PacketOpener::new(TransportMode::XChaCha20Poly1305, &[1; 32])
                .open(&pkt)
                .is_err()
        );
        assert!(
            PacketOpener::new(TransportMode::Aes256Gcm, &[2; 32])
                .open(&pkt)
                .is_err()
        );
    }

    #[test]
    fn extension_body_is_stripped() {
        let key = [3u8; 32];
        let mut h = rtp_header(1, 2, 3).to_vec();
        h[0] |= 0x10;
        h.extend_from_slice(&[0xBE, 0xDE, 0x00, 0x01]); // one word of extension
        let mut s = PacketSealer::new(TransportMode::Aes256Gcm, &key);
        let pkt = s.seal(&h, &[9, 9, 9, 9, 0xAA, 0xBB]);
        let got = PacketOpener::new(TransportMode::Aes256Gcm, &key).open(&pkt).unwrap();
        assert_eq!(got.header.aad_len, 16);
        assert_eq!(got.extension, vec![9, 9, 9, 9]);
        assert_eq!(got.payload, vec![0xAA, 0xBB]);
    }

    #[test]
    fn discovery_roundtrip() {
        let req = discovery_request(99);
        assert_eq!(parse_discovery(&req).unwrap(), (1, 99, String::new(), 0));
        let res = discovery_response(99, "203.0.113.9", 50001);
        assert_eq!(parse_discovery(&res).unwrap(), (2, 99, "203.0.113.9".into(), 50001));
        assert!(parse_discovery(&[0; 10]).is_err());
    }

    #[test]
    fn modes_parse() {
        for m in TransportMode::ALL {
            assert_eq!(TransportMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(TransportMode::parse("xsalsa20_poly1305"), None);
    }
}
