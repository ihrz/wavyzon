//! One guild of a session: its player and its voice connection (port of
//! Cadence's `internal/session/guild.go`).

use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Map, Value};
use wavyzon_audio::FrameSource;
use wavyzon_audio::codec::clamp_bitrate;
use wavyzon_player::Player;
use wavyzon_plugin::{Event, EventType};
use wavyzon_voice::{CloseEvent, Conn, Server};

use super::Session;

/// Why a voice update was refused.
#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("incomplete voice server state")]
    Incomplete,
    #[error("voice dial failed: {0}")]
    Dial(#[from] wavyzon_voice::Error),
}

#[derive(Default)]
struct State {
    server: Server,
    conn: Option<Conn>,
    bitrate: i32,
    user_encoded: String,
    user_data: Option<Map<String, Value>>,
}

pub struct Guild {
    pub id: String,
    pub player: Arc<Player>,
    pub(crate) session: Weak<Session>,
    /// Serialises voice updates (closing the old connection is awaited).
    voice_lock: tokio::sync::Mutex<()>,
    state: Mutex<State>,
}

fn same_server(a: &Server, b: &Server) -> bool {
    a.session_id == b.session_id && a.token == b.token && a.endpoint == b.endpoint && a.channel_id == b.channel_id
}

impl Guild {
    pub(crate) fn new(id: String, player: Arc<Player>, session: Weak<Session>) -> Self {
        Self {
            id,
            player,
            session,
            voice_lock: tokio::sync::Mutex::new(()),
            state: Mutex::new(State::default()),
        }
    }

    fn live(&self) -> Option<Conn> {
        self.state.lock().conn.clone().filter(|c| !c.dead())
    }

    /// The voice server state, whether a live connection exists, and its ping.
    pub fn voice_state(&self) -> (Server, bool, Duration) {
        let server = self.state.lock().server.clone();
        match self.live() {
            Some(c) => (server, true, c.ping()),
            None => (server, false, Duration::ZERO),
        }
    }

    /// Join (or move to) a voice server. The same server again is a no-op
    /// (only a new bitrate is applied); a new one closes the old connection,
    /// waits for it to be gone, then dials.
    pub async fn update_voice(self: &Arc<Self>, s: Server) -> Result<(), VoiceError> {
        let _serial = self.voice_lock.lock().await;
        {
            let mut st = self.state.lock();
            if let Some(c) = st.conn.clone().filter(|c| !c.dead())
                && same_server(&st.server, &s)
            {
                if s.bitrate > 0 && s.bitrate != st.server.bitrate {
                    c.set_bitrate(s.bitrate);
                    st.server.bitrate = s.bitrate;
                }
                return Ok(());
            }
        }
        if !s.valid() {
            tracing::warn!(
                guild = %self.id,
                has_session = !s.session_id.is_empty(),
                has_token = !s.token.is_empty(),
                has_endpoint = !s.endpoint.is_empty(),
                has_channel = !s.channel_id.is_empty(),
                connected = self.live().is_some(),
                "ignored an incomplete voice update, keeping the current connection"
            );
            return Err(VoiceError::Incomplete);
        }
        let old = self.state.lock().conn.take();
        if let Some(old) = old {
            old.close().await;
        }
        let weak = Arc::downgrade(self);
        let source: Arc<dyn FrameSource> = self.player.clone();
        let conn = wavyzon_voice::dial(s.clone(), source, move |c, e| {
            if let Some(g) = weak.upgrade() {
                g.on_voice_close(c, e);
            }
        })
        .inspect_err(|e| tracing::warn!(guild = %self.id, error = %e, "voice dial failed"))?;
        let mut st = self.state.lock();
        st.conn = Some(conn);
        st.server = s;
        Ok(())
    }

    fn on_voice_close(&self, c: &Conn, e: CloseEvent) {
        let mut ev = Event::new(EventType::WebSocketClosedEvent, self.id.clone());
        ev.code = Some(e.code);
        ev.close_reason = e.reason.clone();
        ev.terminal = Some(e.terminal);
        if let Some(s) = self.session.upgrade() {
            s.voice_closed(self, &ev);
        }
        if e.terminal {
            let mut st = self.state.lock();
            if st.conn.as_ref() == Some(c) {
                st.conn = None;
            }
        }
    }

    /// Leave voice, waiting until the connection is gone.
    pub async fn disconnect(&self) {
        let _serial = self.voice_lock.lock().await;
        let conn = self.state.lock().conn.take();
        if let Some(c) = conn {
            c.close().await;
        }
    }

    /// Leave voice and destroy the player.
    pub async fn destroy(&self) {
        self.disconnect().await;
        self.player.destroy();
    }

    /// Remember the bitrate (clamped) and apply it to a live connection.
    pub fn set_bitrate(&self, bps: i32) {
        let bps = clamp_bitrate(bps);
        let conn = {
            let mut st = self.state.lock();
            st.bitrate = bps;
            st.server.bitrate = bps;
            st.conn.clone().filter(|c| !c.dead())
        };
        if let Some(c) = conn {
            c.set_bitrate(bps);
        }
    }

    /// The bitrate set with [`Guild::set_bitrate`] (`0` = none).
    pub fn preferred_bitrate(&self) -> i32 {
        self.state.lock().bitrate
    }

    /// Attach `data` to the track encoded as `encoded` (v4 `userData`).
    pub fn set_user_data(&self, encoded: &str, data: Option<Map<String, Value>>) {
        let mut st = self.state.lock();
        st.user_encoded = encoded.to_string();
        st.user_data = data;
    }

    /// The `userData` of `encoded`, or an empty object.
    pub fn user_data(&self, encoded: &str) -> Map<String, Value> {
        let st = self.state.lock();
        match &st.user_data {
            Some(d) if st.user_encoded == encoded => d.clone(),
            _ => Map::new(),
        }
    }
}
