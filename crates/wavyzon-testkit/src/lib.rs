//! # wavyzon-testkit
//!
//! Test doubles and assertions for [Wavyzon](https://gitlab.com/ihrz/wavyzon).
//! Not published; use it as a `[dev-dependencies]` entry:
//!
//! ```toml
//! [dev-dependencies]
//! wavyzon-testkit = { path = "../wavyzon-testkit" }
//! ```
//!
//! | Module | What |
//! | --- | --- |
//! | [`gateway`] | [`MockVoiceGateway`]: fake Discord voice server (WS v8 + UDP, AEAD `rtpsize`) |
//! | [`gateway::wire`] | RTP / IP discovery / AEAD helpers ([`PacketSealer`], [`PacketOpener`]) |
//! | [`sources`] | [`ToneSource`], [`FailingSource`], [`SlowSource`], [`ScriptedSource`] |
//! | [`frames`] | [`MockFrameSource`], a scripted [`wavyzon_audio::FrameSource`] |
//! | [`audio`] | WAV builder, signal generators, RMS/peak, Goertzel, tolerance asserts |
//! | [`client`] | [`TestClient`], a Lavalink-style WS + REST client for end-to-end tests |
//!
//! See the crate `README.md` for worked examples.

#![warn(missing_docs)]

pub mod audio;
pub mod client;
pub mod frames;
pub mod gateway;
pub mod sources;

use std::time::{Duration, Instant};

pub use client::{TestClient, TestClientBuilder, TestClientError};
pub use frames::MockFrameSource;
pub use gateway::wire::{PacketOpener, PacketSealer, TransportMode};
pub use gateway::{Faults, GatewayConfig, GatewayMessage, MockVoiceGateway};
pub use sources::{FailAt, FailingSource, ScriptedSource, SlowSource, ToneSource};

/// A `wait_*` helper gave up.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("timed out after {timeout:?} waiting for {what}")]
pub struct WaitTimeout {
    /// What was awaited.
    pub what: String,
    /// How long we waited.
    pub timeout: Duration,
}

/// Poll `cond` every 5 ms until it holds or `timeout` elapses.
pub async fn poll_until(timeout: Duration, what: &str, mut cond: impl FnMut() -> bool) -> Result<(), WaitTimeout> {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(WaitTimeout {
                what: what.to_string(),
                timeout,
            });
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
