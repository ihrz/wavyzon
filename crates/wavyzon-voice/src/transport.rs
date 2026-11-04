//! UDP side of a voice connection: IP discovery, RTP headers and the
//! `*_rtpsize` AEAD transport encryption.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::XChaCha20Poly1305;

use crate::Error;
use crate::protocol::EncryptionMode;

/// Size of the fixed RTP header Discord expects.
pub const RTP_HEADER_SIZE: usize = 12;
const RTP_VERSION_FLAGS: u8 = 0x80;
const RTP_PAYLOAD_TYPE: u8 = 0x78;
const DISCOVERY_LENGTH: usize = 74;
const DISCOVERY_REQUEST: u16 = 0x1;
const DISCOVERY_RESPONSE: u16 = 0x2;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const DISCOVERY_RESEND: Duration = Duration::from_secs(1);
const NONCE_COUNTER_SIZE: usize = 4;
const TAG_SIZE: usize = 16;

enum Cipher {
    Aes(Box<Aes256Gcm>),
    XChaCha(Box<XChaCha20Poly1305>),
}

/// Seals (and, for tests and diagnostics, opens) RTP packets with the
/// session's secret key: `header | AEAD(payload, aad = header) | nonce[..4]`,
/// the nonce being a big-endian 32-bit counter padded with zeros.
pub struct Sealer {
    cipher: Cipher,
    counter: u32,
    buf: Vec<u8>,
}

impl Sealer {
    /// Build a sealer for `mode` with the 32-byte key from the session description.
    pub fn new(mode: EncryptionMode, key: &[u8]) -> Result<Sealer, Error> {
        let bad = |_| Error::BadKey(key.len());
        let cipher = match mode {
            EncryptionMode::Aes256Gcm => Cipher::Aes(Box::new(Aes256Gcm::new_from_slice(key).map_err(bad)?)),
            EncryptionMode::XChaCha20Poly1305 => {
                Cipher::XChaCha(Box::new(XChaCha20Poly1305::new_from_slice(key).map_err(bad)?))
            }
        };
        Ok(Sealer {
            cipher,
            counter: 0,
            buf: Vec::with_capacity(2048),
        })
    }

    fn nonce_size(&self) -> usize {
        match self.cipher {
            Cipher::Aes(_) => 12,
            Cipher::XChaCha(_) => 24,
        }
    }

    /// Encrypt `payload` behind `header`. The returned slice is valid until the next call.
    pub fn seal(&mut self, header: &[u8], payload: &[u8]) -> &[u8] {
        let mut nonce = [0u8; 24];
        nonce[..NONCE_COUNTER_SIZE].copy_from_slice(&self.counter.to_be_bytes());
        self.counter = self.counter.wrapping_add(1);
        let n = self.nonce_size();
        self.buf.clear();
        self.buf.extend_from_slice(header);
        self.buf.extend_from_slice(payload);
        let (aad, body) = self.buf.split_at_mut(header.len());
        let tag = match &self.cipher {
            Cipher::Aes(c) => c
                .encrypt_in_place_detached(nonce[..n].into(), aad, body)
                .map(<[u8; TAG_SIZE]>::from),
            Cipher::XChaCha(c) => c
                .encrypt_in_place_detached(nonce[..n].into(), aad, body)
                .map(<[u8; TAG_SIZE]>::from),
        }
        .expect("AEAD encryption of a voice packet cannot fail");
        self.buf.extend_from_slice(&tag);
        self.buf.extend_from_slice(&nonce[..NONCE_COUNTER_SIZE]);
        &self.buf
    }

    /// Decrypt a packet produced by [`Sealer::seal`] (fixed 12-byte header).
    pub fn open(&self, packet: &[u8]) -> Result<Vec<u8>, Error> {
        if packet.len() < RTP_HEADER_SIZE + NONCE_COUNTER_SIZE + TAG_SIZE {
            return Err(Error::ShortPacket);
        }
        let mut nonce = [0u8; 24];
        nonce[..NONCE_COUNTER_SIZE].copy_from_slice(&packet[packet.len() - NONCE_COUNTER_SIZE..]);
        let n = self.nonce_size();
        let header = &packet[..RTP_HEADER_SIZE];
        let mut body = packet[RTP_HEADER_SIZE..packet.len() - NONCE_COUNTER_SIZE].to_vec();
        let res = match &self.cipher {
            Cipher::Aes(c) => c.decrypt_in_place(nonce[..n].into(), header, &mut body),
            Cipher::XChaCha(c) => c.decrypt_in_place(nonce[..n].into(), header, &mut body),
        };
        res.map_err(|_| Error::Decrypt)?;
        Ok(body)
    }
}

/// RTP header state for one SSRC: sequence and timestamp advance per frame.
pub struct Rtp {
    header: [u8; RTP_HEADER_SIZE],
    sequence: u16,
    timestamp: u32,
}

impl Rtp {
    /// Start a stream for `ssrc` at sequence 0, timestamp 0.
    pub fn new(ssrc: u32) -> Rtp {
        let mut header = [0u8; RTP_HEADER_SIZE];
        header[0] = RTP_VERSION_FLAGS;
        header[1] = RTP_PAYLOAD_TYPE;
        header[8..].copy_from_slice(&ssrc.to_be_bytes());
        Rtp {
            header,
            sequence: 0,
            timestamp: 0,
        }
    }

    /// The header for the next packet, then advance by `samples_per_frame`.
    pub fn next(&mut self, samples_per_frame: u32) -> [u8; RTP_HEADER_SIZE] {
        self.header[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        self.header[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(samples_per_frame);
        self.header
    }
}

/// The 74-byte IP discovery request for `ssrc`.
pub fn discovery_packet(ssrc: u32) -> [u8; DISCOVERY_LENGTH] {
    let mut p = [0u8; DISCOVERY_LENGTH];
    p[..2].copy_from_slice(&DISCOVERY_REQUEST.to_be_bytes());
    p[2..4].copy_from_slice(&((DISCOVERY_LENGTH - 4) as u16).to_be_bytes());
    p[4..8].copy_from_slice(&ssrc.to_be_bytes());
    p
}

/// Parse an IP discovery response into our external `(ip, port)`.
pub fn parse_discovery(p: &[u8]) -> Result<(String, u16), Error> {
    if p.len() < DISCOVERY_LENGTH || u16::from_be_bytes([p[0], p[1]]) != DISCOVERY_RESPONSE {
        return Err(Error::BadDiscovery);
    }
    let addr = &p[8..72];
    let end = addr.iter().position(|&b| b == 0).unwrap_or(addr.len());
    let ip = String::from_utf8_lossy(&addr[..end]).into_owned();
    Ok((ip, u16::from_be_bytes([p[72], p[73]])))
}

/// Result of IP discovery.
#[derive(Debug, Clone)]
pub struct Discovery {
    /// Our external IP as the voice server sees it.
    pub ip: String,
    /// Our external port.
    pub port: u16,
    /// Where the reply came from (may differ from the server address).
    pub from: SocketAddr,
    /// How many requests were sent.
    pub attempts: u32,
}

/// Run IP discovery on a blocking socket: resend every second for up to 5 s,
/// accepting a well-formed reply for `ssrc` from any address.
pub fn discover(socket: &UdpSocket, remote: SocketAddr, ssrc: u32) -> Result<Discovery, Error> {
    let request = discovery_packet(ssrc);
    let deadline = Instant::now() + DISCOVERY_TIMEOUT;
    let mut buf = [0u8; 1500];
    let mut attempts = 0;
    let result = (|| {
        while Instant::now() < deadline {
            socket
                .send_to(&request, remote)
                .map_err(|e| Error::Discovery(format!("send: {e}")))?;
            attempts += 1;
            let resend = (Instant::now() + DISCOVERY_RESEND).min(deadline);
            loop {
                let left = resend.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                socket
                    .set_read_timeout(Some(left))
                    .map_err(|e| Error::Discovery(e.to_string()))?;
                match socket.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        if n < DISCOVERY_LENGTH || buf[4..8] != ssrc.to_be_bytes() {
                            continue;
                        }
                        if let Ok((ip, port)) = parse_discovery(&buf[..n]) {
                            return Ok(Discovery {
                                ip,
                                port,
                                from,
                                attempts,
                            });
                        }
                    }
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => break,
                    Err(e) => return Err(Error::Discovery(e.to_string())),
                }
            }
        }
        Err(Error::Discovery(format!(
            "no reply from {remote} after {attempts} attempts in {}s",
            DISCOVERY_TIMEOUT.as_secs()
        )))
    })();
    let _ = socket.set_read_timeout(None);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealer_round_trip() {
        for mode in [EncryptionMode::Aes256Gcm, EncryptionMode::XChaCha20Poly1305] {
            let key = [7u8; 32];
            let mut s = Sealer::new(mode, &key).unwrap();
            let r = Sealer::new(mode, &key).unwrap();
            let mut rtp = Rtp::new(42);
            for i in 0..3u8 {
                let payload = [1, 2, 3, i];
                let packet = s.seal(&rtp.next(960), &payload).to_vec();
                assert_eq!(
                    packet.len(),
                    RTP_HEADER_SIZE + payload.len() + TAG_SIZE + NONCE_COUNTER_SIZE
                );
                assert_eq!(&packet[packet.len() - 4..], &(i as u32).to_be_bytes());
                assert_eq!(r.open(&packet).unwrap(), payload, "{mode}");
            }
        }
    }

    #[test]
    fn sealer_rejects_tampering_and_bad_keys() {
        let mut s = Sealer::new(EncryptionMode::Aes256Gcm, &[1; 32]).unwrap();
        let mut packet = s.seal(&Rtp::new(1).next(960), b"hello").to_vec();
        packet[3] ^= 1;
        assert!(s.open(&packet).is_err());
        assert!(Sealer::new(EncryptionMode::Aes256Gcm, &[1; 16]).is_err());
    }

    #[test]
    fn rtp_header_advances() {
        let mut r = Rtp::new(0xAABBCCDD);
        let h1 = r.next(960);
        let h2 = r.next(960);
        assert_eq!((h1[0], h1[1]), (0x80, 0x78));
        assert_eq!(h1[8..], 0xAABBCCDDu32.to_be_bytes());
        assert_eq!(u16::from_be_bytes([h2[2], h2[3]]), 1);
        assert_eq!(u32::from_be_bytes([h2[4], h2[5], h2[6], h2[7]]), 960);
    }

    #[test]
    fn discovery_round_trip() {
        let p = discovery_packet(99);
        assert_eq!(p.len(), 74);
        assert_eq!(u16::from_be_bytes([p[2], p[3]]), 70);
        assert_eq!(u32::from_be_bytes([p[4], p[5], p[6], p[7]]), 99);
        let mut res = [0u8; 74];
        res[..2].copy_from_slice(&2u16.to_be_bytes());
        res[8..8 + 11].copy_from_slice(b"203.0.113.9");
        res[72..].copy_from_slice(&50001u16.to_be_bytes());
        assert_eq!(parse_discovery(&res).unwrap(), ("203.0.113.9".to_string(), 50001));
    }

    pub(crate) fn discovery_reply(ssrc: u32) -> [u8; 74] {
        let mut res = [0u8; 74];
        res[..2].copy_from_slice(&2u16.to_be_bytes());
        res[2..4].copy_from_slice(&70u16.to_be_bytes());
        res[4..8].copy_from_slice(&ssrc.to_be_bytes());
        res[8..8 + 12].copy_from_slice(b"198.51.100.7");
        res[72..].copy_from_slice(&40000u16.to_be_bytes());
        res
    }

    fn listen_local() -> UdpSocket {
        UdpSocket::bind("127.0.0.1:0").unwrap()
    }

    #[test]
    fn discovery_accepts_reply_from_another_address() {
        let (server, other, client) = (listen_local(), listen_local(), listen_local());
        let server_addr = server.local_addr().unwrap();
        let other_port = other.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 128];
            let (_, from) = server.recv_from(&mut buf).unwrap();
            other.send_to(&discovery_reply(1), from).unwrap();
            other.send_to(&discovery_reply(7), from).unwrap();
        });
        let got = discover(&client, server_addr, 7).unwrap();
        assert_eq!(
            (got.ip.as_str(), got.port, got.from.port()),
            ("198.51.100.7", 40000, other_port)
        );
        t.join().unwrap();
    }

    #[test]
    fn discovery_resends_when_the_first_request_is_lost() {
        let (server, client) = (listen_local(), listen_local());
        let server_addr = server.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 128];
            server.recv_from(&mut buf).unwrap();
            let (_, from) = server.recv_from(&mut buf).unwrap();
            server.send_to(&discovery_reply(3), from).unwrap();
        });
        let got = discover(&client, server_addr, 3).unwrap();
        assert_eq!(got.attempts, 2);
        t.join().unwrap();
    }
}
