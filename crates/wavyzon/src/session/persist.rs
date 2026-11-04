//! Saving resumable sessions on shutdown and restoring them on start (port of
//! Cadence's `internal/session/persist.go`).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use wavyzon_player::Filters;
use wavyzon_plugin::Track;
use wavyzon_voice::Server;

use super::{Manager, Protocol, Session, unix_ms};

const STATE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedPlayer {
    guild_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    track: String,
    position_ms: i64,
    paused: bool,
    volume: i32,
    #[serde(default)]
    filters: Filters,
    #[serde(default)]
    voice: Server,
    #[serde(default, skip_serializing_if = "is_zero")]
    bitrate: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_data: Option<Map<String, Value>>,
}

fn is_zero(v: &i32) -> bool {
    *v == 0
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedSession {
    id: String,
    user_id: String,
    client_name: String,
    protocol: Protocol,
    timeout_ms: i64,
    /// Extra v4 events (`wavyzonEvents`); Cadence's name is accepted.
    #[serde(alias = "cadenceEvents", default)]
    wavyzon_events: bool,
    #[serde(default)]
    players: Vec<SavedPlayer>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedState {
    version: u32,
    saved_at: i64,
    sessions: Vec<SavedSession>,
}

/// What [`Manager::restore`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RestoreReport {
    pub sessions: usize,
    pub players: usize,
    /// Sessions whose resume window had already passed.
    pub skipped: usize,
    pub age: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("state file: {0}")]
    Json(#[from] serde_json::Error),
    #[error("could not consume the state file, refusing to restore twice: {0}")]
    Consume(std::io::Error),
    #[error("state file version {0}, want {STATE_VERSION}")]
    Version(u32),
}

impl Manager {
    fn snapshot(&self) -> SavedState {
        let mut st = SavedState {
            version: STATE_VERSION,
            saved_at: unix_ms(),
            sessions: vec![],
        };
        for s in self.sessions() {
            let (resuming, timeout) = s.settings();
            if !resuming {
                continue;
            }
            let mut ss = SavedSession {
                id: s.id.clone(),
                user_id: s.user_id.clone(),
                client_name: s.client_name.clone(),
                protocol: s.protocol,
                timeout_ms: timeout.as_millis() as i64,
                wavyzon_events: s.extra_events(),
                players: vec![],
            };
            for g in s.guilds() {
                let (server, _, _) = g.voice_state();
                let ps = g.player.state();
                let mut sp = SavedPlayer {
                    guild_id: g.id.clone(),
                    track: String::new(),
                    position_ms: ps.position_ms,
                    paused: ps.paused,
                    volume: ps.volume,
                    filters: ps.filters,
                    voice: server,
                    bitrate: g.preferred_bitrate(),
                    user_data: None,
                };
                if let Some(t) = &ps.track {
                    sp.track = t.encode();
                    let ud = g.user_data(&sp.track);
                    sp.user_data = (!ud.is_empty()).then_some(ud);
                }
                ss.players.push(sp);
            }
            st.sessions.push(ss);
        }
        st
    }

    /// Write every resuming session to `path` (atomically) and stop sending
    /// messages, so clients don't see the shutdown's `cleanup` events.
    /// Returns how many sessions were saved.
    pub fn save(&self, path: &Path) -> Result<usize, PersistError> {
        let st = self.snapshot();
        let raw = serde_json::to_vec(&st)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            create_private_dir(dir)?;
        }
        let tmp = path.with_extension("tmp");
        write_private(&tmp, &raw)?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        self.quiet.store(true, Ordering::SeqCst);
        Ok(st.sessions.len())
    }

    /// Recreate the sessions saved in `path` (consuming the file). A missing
    /// file is not an error.
    pub async fn restore(&self, path: &Path) -> Result<RestoreReport, PersistError> {
        let raw = match std::fs::read(path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RestoreReport::default()),
            Err(e) => return Err(e.into()),
        };
        std::fs::remove_file(path).map_err(PersistError::Consume)?;
        let st: SavedState = serde_json::from_slice(&raw)?;
        if st.version != STATE_VERSION {
            return Err(PersistError::Version(st.version));
        }
        let age = Duration::from_millis((unix_ms() - st.saved_at).max(0) as u64);
        let mut rep = RestoreReport {
            age,
            ..Default::default()
        };
        for ss in st.sessions {
            let timeout = Duration::from_millis(ss.timeout_ms.max(0) as u64);
            if timeout.is_zero() || age >= timeout {
                rep.skipped += 1;
                continue;
            }
            let s = self.restore_session(&ss, timeout, timeout - age);
            rep.sessions += 1;
            for sp in ss.players {
                if restore_player(&s, sp).await {
                    rep.players += 1;
                }
            }
        }
        Ok(rep)
    }

    fn restore_session(&self, ss: &SavedSession, timeout: Duration, remaining: Duration) -> Arc<Session> {
        let s = self.new_session(ss.id.clone(), &ss.user_id, &ss.client_name, timeout, ss.protocol);
        s.set_extra_events(ss.wavyzon_events);
        let mut st = s.inner.lock();
        st.resuming = true;
        st.timeout = timeout;
        s.schedule_expiry_locked(&mut st, remaining);
        drop(st);
        s
    }
}

async fn restore_player(s: &Arc<Session>, sp: SavedPlayer) -> bool {
    let Some(g) = s.guild(&sp.guild_id, true) else {
        return false;
    };
    g.player.set_volume(sp.volume);
    if let Err(e) = g.player.set_filters(sp.filters) {
        tracing::warn!(guild = %sp.guild_id, error = %e, "restored player had filters this node does not accept");
    }
    if sp.bitrate > 0 {
        g.set_bitrate(sp.bitrate);
    }
    if sp.voice.valid()
        && let Err(e) = g.update_voice(sp.voice).await
    {
        tracing::warn!(guild = %sp.guild_id, error = %e, "restored player could not rejoin voice");
    }
    if sp.track.is_empty() {
        return true;
    }
    let t = match Track::decode(&sp.track) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(guild = %sp.guild_id, error = %e, "restored player had an unreadable track");
            return false;
        }
    };
    g.set_user_data(&sp.track, sp.user_data);
    g.player.play(t, sp.position_ms.max(0), sp.paused);
    true
}

/// Create `dir` and its parents, owner-only on unix (the state file holds
/// session ids and voice tokens). Existing directories keep their mode.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

fn write_private(path: &Path, raw: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    std::io::Write::write_all(&mut opts.open(path)?, raw)
}
