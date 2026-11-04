//! Deezer's `BF_CBC_STRIPE` stream cipher, port of Cadence's
//! `internal/source/deezer/stripeCipher.go`.
//!
//! A media file is cut into [`STRIPE_CHUNK`]-byte chunks; every
//! [`STRIPE_INTERVAL`]th chunk (indexes 0, 3, 6…) is Blowfish-CBC encrypted
//! with a per-track key and a fixed IV, the others are plaintext. The
//! per-track key derives from the MD5 of the track id, XOR-folded with the
//! operator-provided 16-byte master key ([`StripeDecryptor::track_key`]).

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use blowfish::Blowfish;
#[cfg(test)]
use blowfish::cipher::BlockEncrypt;
use blowfish::cipher::{BlockDecrypt, KeyInit, generic_array::GenericArray};
use md5::{Digest, Md5};
use tokio::io::{AsyncRead, ReadBuf};

use crate::util::Body;

/// Decrypted 2048-byte chunk size.
pub const STRIPE_CHUNK: usize = 2048;
/// One chunk out of this many is encrypted.
pub const STRIPE_INTERVAL: usize = 3;
/// Size of `DEEZER_MASTER_KEY` and of the derived per-track key, in bytes.
pub const MASTER_KEY_SIZE: usize = 16;

/// The IV each encrypted chunk starts from (Cadence's `stripeIV`).
const STRIPE_IV: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

/// `DEEZER_MASTER_KEY` is not exactly [`MASTER_KEY_SIZE`] bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MasterKeySizeError;

impl std::fmt::Display for MasterKeySizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DEEZER_MASTER_KEY must be exactly {MASTER_KEY_SIZE} bytes")
    }
}

impl std::error::Error for MasterKeySizeError {}

type BlowfishBlock = GenericArray<u8, <Blowfish as blowfish::cipher::BlockSizeUser>::BlockSize>;

/// Decrypts Deezer media streams (Cadence's `StripeDecryptor`).
#[derive(Clone)]
pub struct StripeDecryptor {
    master: [u8; MASTER_KEY_SIZE],
}

impl StripeDecryptor {
    /// `master_key` is the raw `DEEZER_MASTER_KEY` value.
    pub fn new(master_key: &str) -> Result<Self, MasterKeySizeError> {
        let bytes = master_key.as_bytes();
        if bytes.len() != MASTER_KEY_SIZE {
            return Err(MasterKeySizeError);
        }
        let mut master = [0u8; MASTER_KEY_SIZE];
        master.copy_from_slice(bytes);
        Ok(Self { master })
    }

    /// The Blowfish key of one track (Cadence's `trackKey`): the two halves of
    /// the track id's MD5 hex digest, XORed together and with the master key.
    pub fn track_key(&self, track_id: &str) -> [u8; MASTER_KEY_SIZE] {
        let digest = hex::encode(Md5::digest(track_id.as_bytes()));
        let d = digest.as_bytes();
        let mut key = [0u8; MASTER_KEY_SIZE];
        for (i, k) in key.iter_mut().enumerate() {
            *k = d[i] ^ d[i + MASTER_KEY_SIZE] ^ self.master[i];
        }
        key
    }

    /// Decrypt a whole stream from its first chunk.
    pub fn decrypt(&self, track_id: &str, encrypted: Body) -> StripeReader {
        self.decrypt_from(track_id, encrypted, 0)
    }

    /// Decrypt a stream that starts at chunk `first_chunk` (a byte range that
    /// is not chunk-aligned cannot be decrypted).
    pub fn decrypt_from(&self, track_id: &str, encrypted: Body, first_chunk: usize) -> StripeReader {
        StripeReader {
            src: encrypted,
            cipher: Blowfish::new_from_slice(&self.track_key(track_id)).expect("blowfish accepts 16-byte keys"),
            chunk: vec![0u8; STRIPE_CHUNK],
            filled: 0,
            pos: 0,
            index: first_chunk,
            finished: false,
        }
    }
}

/// The `AsyncRead` adapter returned by [`StripeDecryptor::decrypt`]: reads
/// [`STRIPE_CHUNK`]-byte chunks, decrypting every [`STRIPE_INTERVAL`]th one.
pub struct StripeReader {
    src: Body,
    cipher: Blowfish,
    chunk: Vec<u8>,
    filled: usize,
    pos: usize,
    index: usize,
    finished: bool,
}

impl StripeReader {
    /// Fill `chunk` up to [`STRIPE_CHUNK`], keeping partial progress across
    /// polls, then decrypt it. A short final chunk is left as it is (as in
    /// Cadence).
    fn poll_fill(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.filled < STRIPE_CHUNK {
            let mut buf = ReadBuf::new(&mut self.chunk[self.filled..]);
            ready!(Pin::new(&mut self.src).poll_read(cx, &mut buf))?;
            let n = buf.filled().len();
            if n == 0 {
                self.finished = true;
                return Poll::Ready(Ok(()));
            }
            self.filled += n;
        }
        if self.index % STRIPE_INTERVAL == 0 {
            cbc_decrypt(&self.cipher, &STRIPE_IV, &mut self.chunk[..self.filled]);
        }
        self.index += 1;
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for StripeReader {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.pos < this.filled {
                // Serve only a complete chunk (or the short final one): a
                // half-filled one must keep filling first, or its bytes would
                // go out un-decrypted and un-aligned.
                if this.filled == STRIPE_CHUNK || this.finished {
                    let n = (this.filled - this.pos).min(buf.remaining());
                    buf.put_slice(&this.chunk[this.pos..this.pos + n]);
                    this.pos += n;
                    return Poll::Ready(Ok(()));
                }
            } else {
                if this.finished {
                    return Poll::Ready(Ok(()));
                }
                if this.filled == STRIPE_CHUNK {
                    // The previous chunk was served entirely.
                    this.pos = 0;
                    this.filled = 0;
                }
            }
            ready!(this.poll_fill(cx))?;
        }
    }
}

/// Blowfish-CBC decrypt `data` in place, resetting to `iv` at every
/// [`STRIPE_CHUNK`]'s-worth of blocks (each chunk stands alone).
fn cbc_decrypt(cipher: &Blowfish, iv: &[u8; 8], data: &mut [u8]) {
    let mut prev = *iv;
    for block in data.chunks_exact_mut(8) {
        let plain_block = BlowfishBlock::clone_from_slice(block);
        let mut work = plain_block;
        cipher.decrypt_block(&mut work);
        for i in 0..8 {
            block[i] = work[i] ^ prev[i];
        }
        prev = plain_block.into();
    }
}

/// The encryption counterpart of [`cbc_decrypt`], used by tests to build
/// encrypted fixtures.
#[cfg(test)]
pub(crate) fn cbc_encrypt(cipher: &Blowfish, iv: &[u8; 8], data: &mut [u8]) {
    let mut prev = *iv;
    for block in data.chunks_exact_mut(8) {
        for i in 0..8 {
            block[i] ^= prev[i];
        }
        let mut work = BlowfishBlock::clone_from_slice(block);
        cipher.encrypt_block(&mut work);
        block.copy_from_slice(&work);
        prev = work.into();
    }
}

/// Encrypt `plain` the way the Deezer CDN serves it (test fixture).
#[cfg(test)]
pub(crate) fn stripe_encrypt(dec: &StripeDecryptor, track_id: &str, plain: &[u8]) -> Vec<u8> {
    let cipher = Blowfish::new_from_slice(&dec.track_key(track_id)).unwrap();
    let mut out = plain.to_vec();
    for (index, off) in (0..out.len()).step_by(STRIPE_CHUNK).enumerate() {
        let end = (off + STRIPE_CHUNK).min(out.len());
        if end - off == STRIPE_CHUNK && index % STRIPE_INTERVAL == 0 {
            cbc_encrypt(&cipher, &STRIPE_IV, &mut out[off..end]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MASTER: &str = "0123456789abcdef";

    fn cursor(data: Vec<u8>) -> Body {
        Box::new(std::io::Cursor::new(data))
    }

    async fn read_all(mut r: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut r, &mut out).await?;
        Ok(out)
    }

    #[tokio::test]
    async fn stripe_decrypt_round_trips() {
        let d = StripeDecryptor::new(TEST_MASTER).unwrap();
        for size in [0, 100, STRIPE_CHUNK, STRIPE_CHUNK * 3 + 7, STRIPE_CHUNK * 7, 9000] {
            let plain: Vec<u8> = (0..size).map(|i| (i * 31 + i / 7) as u8).collect();
            let enc = stripe_encrypt(&d, "3135553", &plain);
            if size >= STRIPE_CHUNK {
                assert_ne!(enc, plain, "size {size}: nothing was encrypted");
            }
            let r = d.decrypt("3135553", cursor(enc));
            let got = read_all(r).await.unwrap();
            assert_eq!(got, plain, "size {size}: decrypted output differs");
        }
    }

    #[tokio::test]
    async fn decrypt_from_resumes_at_a_chunk_index() {
        let d = StripeDecryptor::new(TEST_MASTER).unwrap();
        let plain: Vec<u8> = (0..STRIPE_CHUNK * 6).map(|i| (i * 13) as u8).collect();
        let enc = stripe_encrypt(&d, "3135553", &plain);
        // A tail that starts at chunk 3 (not encrypted) and chunk 4 (encrypted).
        for first in [3usize, 4] {
            let r = d.decrypt_from("3135553", cursor(enc[first * STRIPE_CHUNK..].to_vec()), first);
            let got = read_all(r).await.unwrap();
            assert_eq!(got, plain[first * STRIPE_CHUNK..], "chunk {first}");
        }
    }

    #[test]
    fn stripe_key_depends_on_the_track() {
        let d = StripeDecryptor::new(TEST_MASTER).unwrap();
        assert_ne!(d.track_key("1"), d.track_key("2"), "tracks share a key");
    }

    #[test]
    fn stripe_rejects_a_bad_master_key() {
        assert!(matches!(StripeDecryptor::new("short"), Err(MasterKeySizeError)));
    }

    #[tokio::test]
    async fn stripe_surfaces_read_errors() {
        struct Boom;
        impl AsyncRead for Boom {
            fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
                Poll::Ready(Err(io::Error::other("reset")))
            }
        }
        let d = StripeDecryptor::new(TEST_MASTER).unwrap();
        let r = d.decrypt("1", Box::new(Boom));
        let err = read_all(r).await.unwrap_err();
        assert_eq!(err.to_string(), "reset");
    }

    /// A body that returns `Pending` on every other poll and short reads,
    /// like a real network stream (in-memory readers never trigger this).
    struct Trickle {
        data: std::io::Cursor<Vec<u8>>,
        step: usize,
        pending: bool,
    }

    impl AsyncRead for Trickle {
        fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            if !this.pending {
                this.pending = true;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            this.pending = false;
            let mut tmp = vec![0u8; this.step.min(buf.remaining()).max(1)];
            let n = std::io::Read::read(&mut this.data, &mut tmp).unwrap();
            buf.put_slice(&tmp[..n]);
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn stripe_decrypt_survives_trickling_reads() {
        let d = StripeDecryptor::new(TEST_MASTER).unwrap();
        let plain: Vec<u8> = (0..9000).map(|i| (i * 31 + i / 7) as u8).collect();
        let enc = stripe_encrypt(&d, "3135553", &plain);
        for step in [1usize, 700, 2048] {
            let src = Trickle {
                data: std::io::Cursor::new(enc.clone()),
                step,
                pending: false,
            };
            let r = d.decrypt("3135553", Box::new(src));
            let got = read_all(r).await.unwrap();
            assert_eq!(got, plain, "step {step}");
        }
    }
}
