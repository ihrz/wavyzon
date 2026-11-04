//! Gapless playback: a pre-buffered next track takes over on the exact frame
//! the current one ends.
//!
//! The client usually reacts to `TrackEndEvent{finished}` by playing the next
//! track of its queue. When that is the track that already took over, the
//! `play` *adopts* it (no restart); its `TrackStartEvent` (and a held-back
//! `TrackAudibleEvent`) are sent then, or when the adoption window closes.

use std::time::Instant;

use wavyzon_plugin::{EndReason, Event, EventType, Track};

use crate::events::tev;
use crate::player::{Inner, end_event};
use crate::{Player, PlayerError};

/// A next track that took over and waits for the client to acknowledge it.
pub(crate) struct Adoption {
    pub encoded: String,
    pub inst: u64,
    pub until: Instant,
    /// `TrackAudibleEvent` held until the start is announced.
    pub audible: Option<Event>,
}

impl Player {
    /// Pre-buffer `t` to start gaplessly after the current track. Returns its
    /// instance. Fails if nothing plays, or if a newer `set_next` /
    /// `clear_next` (or a new track) superseded this call while it resolved.
    pub async fn set_next(&self, t: Track) -> Result<u64, PlayerError> {
        let generation = {
            let mut g = self.inner.lock();
            g.next_gen += 1;
            g.next_gen
        };
        let playable = self.resolve(&t).await.map_err(PlayerError::Resolve)?;
        let mut g = self.inner.lock();
        let s = &mut *g;
        if self.is_destroyed() {
            return Err(PlayerError::Destroyed);
        }
        if generation != s.next_gen {
            return Err(PlayerError::NextSuperseded);
        }
        if s.active.is_none() && s.loading.is_none() {
            return Err(PlayerError::NothingPlaying);
        }
        s.next = None;
        s.instance += 1;
        s.next_inst = s.instance;
        s.next = Some(self.new_deck_locked(s, t, playable, 0, None));
        Ok(s.next_inst)
    }

    /// Forget the gapless next track (and any `set_next` still resolving).
    pub fn clear_next(&self) {
        let mut g = self.inner.lock();
        g.next_gen += 1;
        g.next = None;
        g.next_inst = 0;
    }

    /// Swap the next deck in if deck A just ended cleanly.
    pub(crate) fn gapless_locked(&self, s: &mut Inner) -> bool {
        let ready = matches!((&s.next, &s.active), (Some(_), Some(d)) if d.ended() && d.failure().is_none());
        if !ready {
            return false;
        }
        if s.pending.is_some() || s.fade.is_some() || s.next.as_ref().is_some_and(|n| n.failure().is_some()) {
            s.next = None;
            s.next_inst = 0;
            return false;
        }
        let (Some(old), Some(next)) = (s.active.take(), s.next.take()) else {
            return false;
        };
        let old_inst = s.active_inst;
        s.active_inst = s.next_inst;
        s.next_inst = 0;
        s.active_gain = 1.0;
        s.adopt = Some(Adoption {
            encoded: next.track.encode(),
            inst: s.active_inst,
            until: Instant::now() + self.cfg.adopt_window,
            audible: None,
        });
        s.active = Some(next);
        let tr = old.track.clone();
        drop(old);
        self.emit(end_event(&tr, old_inst, EndReason::Finished));
        true
    }

    fn announce_locked(&self, s: &Inner, a: Adoption) {
        if let Some(d) = &s.active {
            self.emit(tev(EventType::TrackStartEvent, &d.track, a.inst));
        }
        if let Some(e) = a.audible {
            self.emit(e);
        }
    }

    pub(crate) fn adopt_locked(&self, s: &mut Inner, t: &Track, start_ms: i64, paused: bool) -> Option<u64> {
        let a = s.adopt.as_ref()?;
        if start_ms != 0 || s.active_inst != a.inst || Instant::now() > a.until || t.encode() != a.encoded {
            return None;
        }
        let a = s.adopt.take()?;
        let inst = a.inst;
        s.paused = paused;
        self.announce_locked(s, a);
        Some(inst)
    }

    pub(crate) fn expire_adoption_locked(&self, s: &mut Inner) {
        match &s.adopt {
            Some(a) if Instant::now() >= a.until => {}
            _ => return,
        }
        let Some(a) = s.adopt.take() else { return };
        if s.active.is_some() && s.active_inst == a.inst {
            self.announce_locked(s, a);
        }
    }
}
