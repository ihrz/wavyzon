//! The player: deck A (playing), deck B (cued / incoming), the gapless next
//! deck, crossfades, pause fades, and the wire clock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;
use wavyzon_audio::dsp::{Bus, Crossfade, DeckDsp, FadeCurve, TransitionProfile, mix, ramp, scale};
use wavyzon_audio::{FRAME_MS, Frame, FrameInfo, FrameSource, SILENCE};
use wavyzon_plugin::{EndReason, Event, EventType, Failure, Track};

use crate::PlayerError;
use crate::deck::{Deck, STUCK_AFTER, spawn_producer};
use crate::events::{Dispatcher, ev, tev};
use crate::filters::Filters;
use crate::gapless::Adoption;
use crate::host::HostRun;
use crate::loudness::normalization_gain;
use crate::plugin_chain::{self, FilterRegistry, PluginChain, Pool};
use crate::traits::{EventHandler, LoudnessLookup, Opener, Resolver, TapFactory, UnmeasuredHook};

/// How long [`Resolver::playable`] may take.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);
/// Highest accepted volume (percent).
pub const MAX_VOLUME: i32 = 1000;
/// A frame sent within this window means the player is audibly playing.
const LIVE_WINDOW: Duration = Duration::from_millis(200);
/// How long deck B glides from its beat-matched tempo back to normal after a
/// crossfade promoted it.
const TEMPO_RAMP_DURATION: Duration = Duration::from_secs(11);

/// Everything a [`Player`] needs. Build with [`PlayerConfig::new`] and adjust
/// the public fields.
#[derive(Clone)]
pub struct PlayerConfig {
    /// Guild the player belongs to; stamped on every event.
    pub guild_id: String,
    /// Decoded audio buffered per deck, in 20 ms frames (default 500 = 10 s).
    pub buffer_frames: usize,
    /// Measured delay from Discord receiving audio to listeners hearing it,
    /// added to `audibleAt` / `anchorAudibleAt`.
    pub playout_delay: Duration,
    /// Fade out on pause and in on resume (`0` = pause instantly).
    pub pause_fade: Duration,
    /// Opens audio streams.
    pub opener: Arc<dyn Opener>,
    /// Resolves requested tracks to playable ones.
    pub resolver: Arc<dyn Resolver>,
    /// Receives every event, in order (see [`EventHandler`]).
    pub on_event: EventHandler,
    /// Loudness measurements for normalisation; `None` disables it.
    pub loudness: Option<LoudnessLookup>,
    /// Called for tracks the loudness lookup did not know.
    pub on_unmeasured: Option<UnmeasuredHook>,
    /// Normalisation target in LUFS (default -14).
    pub target_lufs: f64,
    /// Plugin filters usable under `filters.pluginFilters`.
    pub plugin_filters: Arc<FilterRegistry>,
    /// Runtime that runs loads, stream reads, decoders and event delivery.
    pub runtime: Handle,
    /// How long a gapless next track waits for the client's `play` of it
    /// before it is announced anyway (default 5 s).
    pub adopt_window: Duration,
    /// Automatic crossfade into the gapless next track: when one is preloaded
    /// (`nextTrack`), deck A fades into it this long before its end instead of
    /// cutting over on the last frame. `0` (the default) keeps strict gapless.
    pub crossfade: Duration,
}

impl PlayerConfig {
    /// A configuration with defaults for everything optional.
    pub fn new(
        guild_id: impl Into<String>,
        opener: Arc<dyn Opener>,
        resolver: Arc<dyn Resolver>,
        on_event: EventHandler,
        runtime: Handle,
    ) -> Self {
        Self {
            guild_id: guild_id.into(),
            buffer_frames: 500,
            playout_delay: Duration::ZERO,
            pause_fade: Duration::ZERO,
            opener,
            resolver,
            on_event,
            loudness: None,
            on_unmeasured: None,
            target_lufs: -14.0,
            plugin_filters: Arc::new(FilterRegistry::new()),
            runtime,
            adopt_window: Duration::from_secs(5),
            crossfade: Duration::ZERO,
        }
    }
}

/// Body of `POST …/mix/crossfade`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CrossfadeRequest {
    /// Fade length (must be positive).
    pub duration_ms: i64,
    /// `EQUAL_POWER` (default) or `LINEAR`.
    pub curve: String,
    /// Arm the fade to start when deck A reaches this position; `None` starts
    /// it on the next frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_position_ms: Option<i64>,
    /// Tempo of deck B relative to normal while mixing (beat matching); after
    /// promotion it glides back to `1` over 11 s. `0` means `1`.
    pub tempo_ratio: f64,
    /// Gain of deck B (loudness matching); ignored when deck B has a loudness
    /// measurement. `0` means `1`.
    pub gain_ratio: f64,
    /// Bass swap and high-pass sweeps.
    pub profile: TransitionProfile,
}

/// The `player` object of `playerUpdate` / `GET …/players/{guild}`.
///
/// The live position is `anchorMs + (now - anchorSentAt) * rate` while
/// `sounding`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    /// The requested track on deck A (or the one still loading).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<Track>,
    /// What actually plays on deck A.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playable: Option<Track>,
    /// Instance of deck A's track.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub instance: u64,
    /// Whether playback is paused.
    pub paused: bool,
    /// Volume, percent.
    pub volume: i32,
    /// Current filters.
    pub filters: Filters,
    /// Best estimate of what listeners hear now, in ms.
    pub position_ms: i64,
    /// Source position of the last frame that left the socket.
    pub anchor_ms: f64,
    /// When it left (unix ms).
    pub anchor_sent_at: i64,
    /// When it will be heard (unix ms, `anchorSentAt + playout delay`).
    pub anchor_audible_at: i64,
    /// Source ms per wall ms (timescale × tempo ratio).
    pub rate: f64,
    /// Whether frames are being sent right now.
    pub sounding: bool,
    /// Whether a crossfade is running.
    pub mixing: bool,
    /// The incoming track during a crossfade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mix_into: Option<Track>,
    /// Position of the incoming track.
    #[serde(default, rename = "mixIntoPositionMs", skip_serializing_if = "is_zero_i64")]
    pub mix_into_ms: i64,
    /// Crossfade progress `0..=1`.
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub mix_progress: f64,
    /// Whether a track is cued on deck B.
    #[serde(rename = "deckBLoaded")]
    pub deck_b_loaded: bool,
    /// Deck A position an armed crossfade waits for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mix_armed_at_ms: Option<i64>,
    /// Whether a host voice is talking.
    pub hosting: bool,
    /// The gapless next track.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_track: Option<Track>,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}
fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}
fn is_zero_f64(v: &f64) -> bool {
    *v == 0.0
}

/// The last frame that left the socket.
#[derive(Debug, Clone, Copy)]
struct SentMark {
    instance: u64,
    pos_ms: f64,
    at: Instant,
}

/// An armed crossfade (or host transition) waiting for its cue.
pub(crate) struct PendingMix {
    pub deck: Box<Deck>,
    pub instance: u64,
    pub duration_ms: i64,
    pub curve: FadeCurve,
    pub at_ms: i64,
    pub profile: TransitionProfile,
    pub gain: f64,
    pub host: Option<HostRun>,
}

pub(crate) struct Inner {
    pub active: Option<Box<Deck>>,
    pub active_inst: u64,
    pub incoming: Option<Box<Deck>>,
    pub inc_inst: u64,
    pub cued: Option<Box<Deck>>,
    pub cued_inst: u64,
    pub pending: Option<PendingMix>,
    pub fade: Option<Crossfade>,
    pub host: Option<HostRun>,
    pub host_pos_a: f64,
    pub bus: Bus,
    profile: TransitionProfile,
    pub active_gain: f64,
    pub inc_gain: f64,
    out_dsp: DeckDsp,
    in_dsp: DeckDsp,
    pub buf_a: Frame,
    pub buf_b: Frame,

    pub instance: u64,
    pub paused: bool,
    volume: i32,
    filters: Filters,
    last: Option<SentMark>,
    audible: u64,
    analysis: Option<TapFactory>,
    ramp_until: Option<Instant>,
    ramp_from: f64,
    ramp_start: Instant,
    paused_at: Option<Instant>,
    pub env: f64,
    pub fading_out: bool,

    pub loading: Option<Track>,
    loading_from: i64,

    pub next: Option<Box<Deck>>,
    pub next_inst: u64,
    pub adopt: Option<Adoption>,
    pub next_gen: u64,
    /// The running fade is an automatic crossfade into the next track: its
    /// promotion ends deck A with `finished` and waits for adoption, like a
    /// gapless swap, instead of `replaced`.
    pub auto_mix: bool,
}

/// One guild's player.
///
/// Created with [`Player::new`] as an `Arc`, and handed to the voice sender
/// as a [`FrameSource`]. Commands (`play`, `pause`, …) may be called from any
/// thread; events are delivered through [`PlayerConfig::on_event`].
pub struct Player {
    pub(crate) cfg: PlayerConfig,
    me: Weak<Player>,
    ctx: CancellationToken,
    events: Dispatcher,
    pub(crate) inner: Mutex<Inner>,
    deck_ids: AtomicU64,
}

/// Unix milliseconds of an [`Instant`].
pub(crate) fn unix_ms(at: Instant) -> i64 {
    let now = Instant::now();
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    if at <= now {
        wall - (now - at).as_millis() as i64
    } else {
        wall + (at - now).as_millis() as i64
    }
}

impl Player {
    /// Create a player. Must be called within (or with a handle to) a Tokio
    /// runtime: the event dispatcher starts on [`PlayerConfig::runtime`].
    pub fn new(cfg: PlayerConfig) -> Arc<Player> {
        let events = Dispatcher::new(&cfg.runtime, cfg.on_event.clone());
        Arc::new_cyclic(|me| Player {
            cfg,
            me: me.clone(),
            ctx: CancellationToken::new(),
            events,
            deck_ids: AtomicU64::new(0),
            inner: Mutex::new(Inner {
                active: None,
                active_inst: 0,
                incoming: None,
                inc_inst: 0,
                cued: None,
                cued_inst: 0,
                pending: None,
                fade: None,
                host: None,
                host_pos_a: 0.0,
                bus: Bus::default(),
                profile: TransitionProfile::default(),
                active_gain: 1.0,
                inc_gain: 1.0,
                out_dsp: DeckDsp::new(),
                in_dsp: DeckDsp::new(),
                buf_a: SILENCE,
                buf_b: SILENCE,
                instance: 0,
                paused: false,
                volume: 100,
                filters: Filters::default(),
                last: None,
                audible: 0,
                analysis: None,
                ramp_until: None,
                ramp_from: 1.0,
                ramp_start: Instant::now(),
                paused_at: None,
                env: 1.0,
                fading_out: false,
                loading: None,
                loading_from: 0,
                next: None,
                next_inst: 0,
                adopt: None,
                next_gen: 0,
                auto_mix: false,
            }),
        })
    }

    /// The guild this player belongs to.
    pub fn guild_id(&self) -> &str {
        &self.cfg.guild_id
    }

    pub(crate) fn emit(&self, mut e: Event) {
        e.guild_id.clone_from(&self.cfg.guild_id);
        self.events.push(e);
    }

    /// Install (or remove) the analysis tap factory. Only decks that start
    /// from the beginning are tapped.
    pub fn set_analysis(&self, f: Option<TapFactory>) {
        self.inner.lock().analysis = f;
    }

    pub(crate) fn is_destroyed(&self) -> bool {
        self.ctx.is_cancelled()
    }

    /// Create and start a deck. `chain` carries plugin filters over (seek).
    pub(crate) fn new_deck_locked(
        &self,
        s: &mut Inner,
        t: Track,
        playable: Track,
        start_ms: i64,
        chain: Option<PluginChain>,
    ) -> Box<Deck> {
        let id = self.deck_ids.fetch_add(1, Ordering::Relaxed) + 1;
        let mut d = Box::new(Deck::new(
            id,
            t,
            playable,
            self.cfg.buffer_frames,
            start_ms,
            self.ctx.child_token(),
        ));
        self.level_deck(&mut d);
        d.filters = s.filters.clone();
        d.plugins = chain
            .unwrap_or_else(|| PluginChain::new(&self.cfg.plugin_filters, &mut Pool::new(), &s.filters.plugin_filters));
        if start_ms == 0
            && let Some(f) = &s.analysis
            && let Some(tap) = f(&d.playable)
        {
            d.attach_tap(tap);
        }
        let weak = self.me.clone();
        spawn_producer(
            &self.cfg.runtime,
            d.shared.clone(),
            self.cfg.opener.clone(),
            d.playable.clone(),
            start_ms,
            Box::new(move || {
                if let Some(p) = weak.upgrade() {
                    p.deck_done(id);
                }
            }),
        );
        d
    }

    fn level_deck(&self, d: &mut Deck) {
        d.norm = 1.0;
        let Some(lookup) = &self.cfg.loudness else { return };
        if let Some(l) = lookup(&d.track, &d.playable) {
            d.norm = normalization_gain(l, self.cfg.target_lufs);
            d.measured = true;
            return;
        }
        if let Some(hook) = &self.cfg.on_unmeasured {
            hook(&d.track);
        }
    }

    /// A deck's producer finished: report a failure of deck A right away,
    /// even when no frames are being pulled.
    fn deck_done(&self, id: u64) {
        let mut g = self.inner.lock();
        let s = &mut *g;
        let failed = s
            .active
            .as_ref()
            .is_some_and(|d| d.id == id && d.shared.drained() && d.failure().is_some());
        if failed {
            self.check_end_locked(s);
        }
    }

    pub(crate) async fn resolve(&self, t: &Track) -> Result<Track, Failure> {
        tokio::select! {
            _ = self.ctx.cancelled() => Err(Failure::common(&t.source, "player destroyed", None)),
            r = tokio::time::timeout(RESOLVE_TIMEOUT, self.cfg.resolver.playable(t)) => match r {
                Ok(r) => r,
                Err(_) => Err(Failure::common(&t.source, "resolve timed out", Some(format!("{}s", RESOLVE_TIMEOUT.as_secs())))),
            },
        }
    }

    /// Play `t` from `start_ms` on deck A, replacing whatever plays (which
    /// ends with `replaced`). Returns the new instance at once; the track is
    /// resolved and opened in the background (`TrackLoadingEvent`, then
    /// `TrackStartEvent` or `TrackExceptionEvent` + `TrackEndEvent{loadFailed}`).
    ///
    /// Playing the gapless next track that just took over (same encoded
    /// track, `start_ms == 0`) adopts it instead of restarting it.
    pub fn play(&self, t: Track, start_ms: i64, paused: bool) -> u64 {
        let mut g = self.inner.lock();
        let s = &mut *g;
        if let Some(inst) = self.adopt_locked(s, &t, start_ms, paused) {
            return inst;
        }
        s.adopt = None;
        s.instance += 1;
        let inst = s.instance;
        let old = s.active.take();
        let old_inst = s.active_inst;
        let was_loading = s.loading.take();
        self.clear_mix_locked(s);
        s.active_inst = inst;
        s.loading = Some(t.clone());
        s.loading_from = start_ms;
        s.paused = paused;
        s.env = 1.0;
        s.fading_out = false;
        drop(g);
        if let Some(old) = old {
            let tr = old.track.clone();
            drop(old);
            self.emit(end_event(&tr, old_inst, EndReason::Replaced));
        }
        if let Some(l) = was_loading {
            self.emit(end_event(&l, old_inst, EndReason::Replaced));
        }
        self.emit(tev(EventType::TrackLoadingEvent, &t, inst));
        if let Some(me) = self.me.upgrade() {
            self.cfg.runtime.spawn(async move { me.load(t, start_ms, inst).await });
        }
        inst
    }

    async fn load(&self, t: Track, start_ms: i64, inst: u64) {
        let res = self.resolve(&t).await;
        let mut g = self.inner.lock();
        let s = &mut *g;
        if s.active_inst != inst || self.is_destroyed() {
            return;
        }
        s.loading = None;
        match res {
            Err(f) => {
                s.active_inst = 0;
                drop(g);
                let mut e = tev(EventType::TrackExceptionEvent, &t, inst);
                e.exception = Some(f);
                self.emit(e);
                self.emit(end_event(&t, inst, EndReason::LoadFailed));
            }
            Ok(playable) => {
                let d = self.new_deck_locked(s, t, playable, start_ms, None);
                let e = tev(EventType::TrackStartEvent, &d.track, inst);
                s.active = Some(d);
                drop(g);
                self.emit(e);
            }
        }
    }

    pub(crate) fn drop_deck_locked(&self, d: Box<Deck>, inst: u64) {
        let tr = d.track.clone();
        drop(d);
        self.emit(ev(EventType::MixDroppedEvent, Some(&tr), inst));
    }

    fn drop_mix_locked(&self, s: &mut Inner) {
        let mut hosted = s.host.is_some();
        if let Some(d) = s.incoming.take() {
            self.drop_deck_locked(d, s.inc_inst);
        }
        if let Some(p) = s.pending.take() {
            hosted |= p.host.is_some();
            self.drop_deck_locked(p.deck, p.instance);
        }
        if let Some(d) = s.cued.take() {
            self.drop_deck_locked(d, s.cued_inst);
        }
        if hosted {
            tracing::debug!(guild = %self.cfg.guild_id, "host transition dropped");
        }
        if let Some(h) = &s.host
            && !h.voice_ended
        {
            let mut e = ev(
                EventType::HostEndedEvent,
                s.active.as_ref().map(|d| &d.track),
                s.active_inst,
            );
            e.duration_ms = h.elapsed;
            self.emit(e);
        }
        s.fade = None;
        s.host = None;
        s.auto_mix = false;
    }

    fn clear_mix_locked(&self, s: &mut Inner) {
        self.drop_mix_locked(s);
        s.next = None;
        s.next_inst = 0;
        s.next_gen += 1;
        s.inc_gain = 1.0;
        s.active_gain = 1.0;
        s.ramp_until = None;
    }

    /// Stop playback; the track ends with `stopped`. Mixes and the next track
    /// are dropped.
    pub fn stop(&self) {
        let mut g = self.inner.lock();
        let s = &mut *g;
        let d = s.active.take();
        let inst = s.active_inst;
        let was_loading = s.loading.take();
        self.clear_mix_locked(s);
        s.active_inst = 0;
        s.env = 1.0;
        s.fading_out = false;
        drop(g);
        if let Some(d) = d {
            let tr = d.track.clone();
            drop(d);
            self.emit(end_event(&tr, inst, EndReason::Stopped));
        }
        if let Some(l) = was_loading {
            self.emit(end_event(&l, inst, EndReason::Stopped));
        }
    }

    /// Pause or resume. With a [`PlayerConfig::pause_fade`] the audio fades
    /// out (frames keep coming until silent) and back in on resume; resuming
    /// mid fade-out turns the fade around.
    pub fn pause(&self, paused: bool) {
        let mut g = self.inner.lock();
        let s = &mut *g;
        let now = Instant::now();
        if paused && !s.paused && !self.cfg.pause_fade.is_zero() && s.active.is_some() && s.env > 0.0 {
            s.fading_out = true;
        } else if paused && !s.paused {
            s.paused_at = Some(now);
        } else if !paused && s.paused && s.fading_out {
            s.fading_out = false;
        } else if !paused
            && s.paused
            && let Some(at) = s.paused_at.take()
            && let Some(until) = s.ramp_until
        {
            let held = now - at;
            s.ramp_start += held;
            s.ramp_until = Some(until + held);
        }
        s.paused = paused;
    }

    /// Set the volume in percent, clamped to `0..=1000`.
    pub fn set_volume(&self, v: i32) {
        self.inner.lock().volume = v.clamp(0, MAX_VOLUME);
    }

    /// Replace the filters. Built-in filters glide to their new values;
    /// plugin filters are validated first (unknown names and factory errors
    /// are returned and nothing changes), then reconfigured or rebuilt on
    /// every deck.
    pub fn set_filters(&self, f: Filters) -> Result<(), PlayerError> {
        let mut g = self.inner.lock();
        let s = &mut *g;
        let registry = &self.cfg.plugin_filters;
        let mut pool = plugin_chain::validate(registry, &s.filters.plugin_filters, &f.plugin_filters)?;
        let plugins_changed = s.filters.plugin_filters != f.plugin_filters;
        let decks = s
            .active
            .iter_mut()
            .chain(s.incoming.iter_mut())
            .chain(s.cued.iter_mut())
            .chain(s.next.iter_mut())
            .chain(s.pending.iter_mut().map(|p| &mut p.deck));
        for d in decks {
            d.filters = f.clone();
            if plugins_changed {
                d.plugins.update(registry, &mut pool, &f.plugin_filters);
            }
        }
        s.filters = f;
        Ok(())
    }

    /// Seek deck A to `ms` (a new deck opens there; plugin filters are kept
    /// and reset). A running crossfade is completed first, so the seek lands
    /// on the incoming track; an armed one is dropped, and so is an automatic
    /// crossfade into the next track (the seek is for the current track).
    pub fn seek(&self, ms: i64) -> Result<(), PlayerError> {
        let mut g = self.inner.lock();
        let s = &mut *g;
        if s.active.is_none() {
            return Err(PlayerError::NothingToMix);
        }
        // An automatic crossfade is dropped instead: the client does not know
        // the next track started, so its seek targets the current one.
        if s.fade.is_some() && s.incoming.is_some() && !s.auto_mix {
            self.promote_locked(s);
        }
        self.drop_mix_locked(s);
        let Some(mut old) = s.active.take() else {
            return Err(PlayerError::NothingToMix);
        };
        let inst = s.active_inst;
        let mut chain = std::mem::take(&mut old.plugins);
        chain.reset();
        let next = self.new_deck_locked(s, old.track.clone(), old.playable.clone(), ms.max(0), Some(chain));
        let tr = next.track.clone();
        s.active = Some(next);
        s.last = None;
        drop(g);
        drop(old);
        let mut e = ev(EventType::PositionJump, Some(&tr), inst);
        e.position_ms = ms;
        self.emit(e);
        Ok(())
    }

    /// Resolve and pre-buffer `t` on deck B from `start_ms`, replacing any
    /// cued deck. Returns its instance.
    pub async fn load_deck_b(&self, t: Track, start_ms: i64) -> Result<u64, PlayerError> {
        let playable = self.resolve(&t).await.map_err(PlayerError::Resolve)?;
        let mut g = self.inner.lock();
        let s = &mut *g;
        if self.is_destroyed() {
            return Err(PlayerError::Destroyed);
        }
        if let Some(c) = s.cued.take() {
            self.drop_deck_locked(c, s.cued_inst);
        }
        s.instance += 1;
        s.cued_inst = s.instance;
        s.cued = Some(self.new_deck_locked(s, t, playable, start_ms.max(0), None));
        Ok(s.cued_inst)
    }

    /// Crossfade from deck A into the cued deck B, now or (with
    /// `at_position_ms`) when deck A reaches the cue (`MixArmedEvent`).
    pub fn start_crossfade(&self, req: CrossfadeRequest) -> Result<(), PlayerError> {
        let curve: FadeCurve = req
            .curve
            .parse()
            .map_err(|_| PlayerError::InvalidCurve(req.curve.clone()))?;
        if req.duration_ms <= 0 {
            return Err(PlayerError::InvalidDuration);
        }
        let mut g = self.inner.lock();
        let s = &mut *g;
        if s.cued.is_none() {
            return Err(PlayerError::NoDeckB);
        }
        if s.active.is_none() {
            return Err(PlayerError::NothingToMix);
        }
        let Some(mut deck) = s.cued.take() else {
            return Err(PlayerError::NoDeckB);
        };
        let inst = s.cued_inst;
        deck.set_tempo_ratio(if req.tempo_ratio <= 0.0 { 1.0 } else { req.tempo_ratio });
        let gain = if req.gain_ratio <= 0.0 { 1.0 } else { req.gain_ratio };
        if let Some(at) = req.at_position_ms {
            if let Some(p) = s.pending.take() {
                self.drop_deck_locked(p.deck, p.instance);
            }
            let tr = deck.track.clone();
            s.pending = Some(PendingMix {
                deck,
                instance: inst,
                duration_ms: req.duration_ms,
                curve,
                at_ms: at,
                profile: req.profile,
                gain,
                host: None,
            });
            drop(g);
            let mut e = ev(EventType::MixArmedEvent, Some(&tr), inst);
            e.position_ms = at;
            e.duration_ms = req.duration_ms;
            self.emit(e);
            return Ok(());
        }
        self.begin_fade_locked(s, deck, inst, req.duration_ms, curve, req.profile, gain, None);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_fade_locked(
        &self,
        s: &mut Inner,
        deck: Box<Deck>,
        inst: u64,
        duration_ms: i64,
        curve: FadeCurve,
        profile: TransitionProfile,
        gain: f64,
        host: Option<HostRun>,
    ) {
        if let Some(p) = s.pending.take() {
            self.drop_deck_locked(p.deck, p.instance);
        }
        if s.fade.is_some() && s.incoming.is_some() {
            self.promote_locked(s);
        }
        s.auto_mix = false;
        if host.is_some()
            && let Some(a) = &s.active
        {
            s.host_pos_a = a.position_ms() as f64;
        }
        let voice_ms = host.as_ref().map(|h| h.shape.voice_ms);
        s.host = host;
        s.profile = profile;
        s.inc_gain = if deck.measured { 1.0 } else { gain };
        s.out_dsp.reset();
        s.in_dsp.reset();
        let into = deck.track.clone();
        let into_ms = deck.position_ms();
        s.incoming = Some(deck);
        s.inc_inst = inst;
        let fade = Crossfade::new(duration_ms, curve);
        let active_track = s.active.as_ref().map(|d| &d.track);
        let mut e = ev(EventType::MixStartedEvent, active_track, s.active_inst);
        e.into = Some(into.clone());
        e.into_ms = into_ms;
        e.duration_ms = fade.duration_ms();
        self.emit(e);
        if let Some(voice_ms) = voice_ms {
            let mut e = ev(EventType::HostStartedEvent, active_track, s.active_inst);
            e.into = Some(into);
            e.into_ms = into_ms;
            e.duration_ms = voice_ms;
            self.emit(e);
        }
        s.fade = Some(fade);
    }

    /// Drop the armed crossfade and the cued deck B (`MixDroppedEvent`).
    pub fn cancel_pending(&self) {
        let mut g = self.inner.lock();
        let s = &mut *g;
        if let Some(p) = s.pending.take() {
            if p.host.is_some() {
                tracing::debug!(guild = %self.cfg.guild_id, "host transition dropped");
            }
            self.drop_deck_locked(p.deck, p.instance);
        }
        if let Some(c) = s.cued.take() {
            self.drop_deck_locked(c, s.cued_inst);
        }
    }

    fn release_pending_locked(&self, s: &mut Inner) {
        let (Some(w), Some(a)) = (&s.pending, &s.active) else {
            return;
        };
        if a.ended() {
            if let Some(w) = s.pending.take() {
                self.drop_deck_locked(w.deck, w.instance);
            }
            return;
        }
        if a.position_ms() >= w.at_ms
            && let Some(w) = s.pending.take()
        {
            self.begin_fade_locked(s, w.deck, w.instance, w.duration_ms, w.curve, w.profile, w.gain, w.host);
        }
    }

    /// Pull the next 20 ms frame (see [`FrameSource::next_frame`]).
    pub fn next_frame(&self, out: &mut Frame) -> Option<FrameInfo> {
        let mut g = self.inner.lock();
        let s = &mut *g;
        if (s.active.is_none() && s.host.is_none()) || (s.paused && !s.fading_out) {
            return None;
        }
        let from = s.env;
        s.env = self.envelope(s);
        let info = self.frame_locked(s, out);
        if info.is_some() && (from != 1.0 || s.env != 1.0) {
            ramp(out, from * from, s.env * s.env);
        }
        if s.fading_out && s.env == 0.0 {
            s.fading_out = false;
            s.paused_at = Some(Instant::now());
        }
        info
    }

    fn envelope(&self, s: &Inner) -> f64 {
        if self.cfg.pause_fade.is_zero() {
            return 1.0;
        }
        let step = FRAME_MS as f64 / self.cfg.pause_fade.as_secs_f64() / 1000.0;
        if s.paused {
            let v = s.env - step;
            if v > 1e-9 { v } else { 0.0 }
        } else {
            let v = s.env + step;
            if v < 1.0 - 1e-9 { v } else { 1.0 }
        }
    }

    fn frame_locked(&self, s: &mut Inner, out: &mut Frame) -> Option<FrameInfo> {
        self.apply_ramp_locked(s);
        self.expire_adoption_locked(s);
        self.auto_crossfade_locked(s);
        self.release_pending_locked(s);
        let vol = s.volume as f64 / 100.0;
        if s.host.is_some() && s.fade.is_some() && s.incoming.is_some() {
            return self.host_frame_locked(s, out, vol);
        }
        if let (Some(fade), Some(inc), Some(active)) = (&mut s.fade, &mut s.incoming, &mut s.active) {
            let (pos_a, a_ok) = active.read(&mut s.buf_a);
            let (_, b_ok) = inc.read(&mut s.buf_b);
            fade.advance();
            let prog = fade.progress();
            let p = &s.profile;
            if a_ok && p.swaps_bass() {
                s.out_dsp.apply_bass_gain(&mut s.buf_a, p.outgoing_bass_gain(prog));
            }
            if a_ok && p.sweeps_out() {
                s.out_dsp.apply_high_pass(&mut s.buf_a, p.outgoing_sweep_hz(prog));
            }
            if b_ok && p.swaps_bass() {
                s.in_dsp.apply_bass_gain(&mut s.buf_b, p.incoming_bass_gain(prog));
            }
            if b_ok && p.sweeps_in() {
                s.in_dsp.apply_high_pass(&mut s.buf_b, p.incoming_sweep_hz(prog));
            }
            let a = if a_ok { &s.buf_a } else { &SILENCE };
            let b = if b_ok { &s.buf_b } else { &SILENCE };
            mix(
                a,
                fade.out_gain * s.active_gain * active.norm * vol,
                b,
                fade.in_gain * s.inc_gain * inc.norm * vol,
                out,
            );
            let info = FrameInfo {
                instance: s.active_inst,
                position_ms: pos_a,
                mixing: true,
            };
            if fade.finished() {
                self.promote_locked(s);
            }
            return Some(info);
        }
        let active = s.active.as_mut()?;
        let (mut pos, mut ok) = active.read(&mut s.buf_a);
        if !ok && self.gapless_locked(s) {
            let active = s.active.as_mut()?;
            (pos, ok) = active.read(&mut s.buf_a);
        }
        if !ok {
            self.check_end_locked(s);
            return None;
        }
        let norm = s.active.as_ref().map_or(1.0, |d| d.norm);
        scale(&s.buf_a, s.active_gain * norm * vol, out);
        Some(FrameInfo {
            instance: s.active_inst,
            position_ms: pos,
            mixing: false,
        })
    }

    fn check_end_locked(&self, s: &mut Inner) {
        let Some(d) = s.active.as_mut() else { return };
        let inst = s.active_inst;
        let failure = d.failure();
        if let Some(f) = &failure
            && s.fade.is_some()
            && s.incoming.is_some()
        {
            let mut e = tev(EventType::TrackExceptionEvent, &d.track, inst);
            e.exception = Some(f.clone());
            self.emit(e);
            if s.host.is_none() {
                self.promote_locked(s);
            }
            return;
        }
        if let Some(f) = failure {
            let Some(d) = s.active.take() else { return };
            s.active_inst = 0;
            self.clear_mix_locked(s);
            let tr = d.track.clone();
            drop(d);
            let mut e = tev(EventType::TrackExceptionEvent, &tr, inst);
            e.exception = Some(f);
            self.emit(e);
            self.emit(end_event(&tr, inst, EndReason::LoadFailed));
            return;
        }
        if d.ended() {
            let Some(d) = s.active.take() else { return };
            s.active_inst = 0;
            self.drop_mix_locked(s);
            let tr = d.track.clone();
            drop(d);
            self.emit(end_event(&tr, inst, EndReason::Finished));
            return;
        }
        if d.stuck() {
            let mut e = tev(EventType::TrackStuckEvent, &d.track, inst);
            e.threshold_ms = STUCK_AFTER.as_millis() as i64;
            self.emit(e);
        }
    }

    pub(crate) fn promote_locked(&self, s: &mut Inner) {
        let auto = std::mem::take(&mut s.auto_mix);
        if s.incoming.is_some() {
            self.flush_adoption_locked(s);
        }
        let old = s.active.take();
        let old_inst = s.active_inst;
        if let Some(h) = &s.host
            && !h.voice_ended
        {
            let mut e = ev(EventType::HostEndedEvent, old.as_ref().map(|d| &d.track), old_inst);
            e.duration_ms = h.elapsed;
            self.emit(e);
        }
        s.host = None;
        s.fade = None;
        let Some(new) = s.incoming.take() else {
            s.active = old;
            return;
        };
        s.active_inst = s.inc_inst;
        s.active_gain = s.inc_gain;
        s.inc_gain = 1.0;
        s.ramp_until = None;
        let r = new.tempo_ratio();
        if r != 1.0 {
            let now = Instant::now();
            s.ramp_from = r;
            s.ramp_start = now;
            s.ramp_until = Some(now + TEMPO_RAMP_DURATION);
        }
        let (into, into_ms) = (new.track.clone(), new.position_ms());
        s.active = Some(new);
        if let Some(old) = old {
            let tr = old.track.clone();
            drop(old);
            let mut e = ev(EventType::MixPromotedEvent, Some(&tr), old_inst);
            e.into = Some(into.clone());
            e.into_ms = into_ms;
            self.emit(e);
            let reason = if auto { EndReason::Finished } else { EndReason::Replaced };
            self.emit(end_event(&tr, old_inst, reason));
        }
        if auto {
            // As after a gapless swap: the client answers `finished` by
            // playing its next track, which adopts this one.
            self.hold_for_adoption_locked(s, &into);
        } else {
            self.emit(tev(EventType::TrackStartEvent, &into, s.active_inst));
        }
    }

    fn apply_ramp_locked(&self, s: &mut Inner) {
        let (Some(until), Some(active)) = (s.ramp_until, s.active.as_mut()) else {
            return;
        };
        let now = Instant::now();
        if now >= until {
            active.set_tempo_ratio(1.0);
            s.ramp_until = None;
            return;
        }
        let total = (until - s.ramp_start).as_secs_f64();
        let prog = if total > 0.0 {
            now.saturating_duration_since(s.ramp_start).as_secs_f64() / total
        } else {
            1.0
        };
        active.set_tempo_ratio(s.ramp_from + (1.0 - s.ramp_from) * prog);
    }

    /// A frame left the socket at `at` (see [`FrameSource::sent`]). Moves
    /// the wire-clock anchor and fires `TrackAudibleEvent` for the first
    /// frame of each instance.
    pub fn sent(&self, info: FrameInfo, at: Instant) {
        let mut g = self.inner.lock();
        let s = &mut *g;
        s.last = Some(SentMark {
            instance: info.instance,
            pos_ms: info.position_ms,
            at,
        });
        let first = s.audible != info.instance;
        if first {
            s.audible = info.instance;
        }
        let mut audible = None;
        if first
            && s.active_inst == info.instance
            && let Some(d) = &s.active
        {
            let mut e = tev(EventType::TrackAudibleEvent, &d.track, info.instance);
            e.position_ms = info.position_ms as i64;
            e.sent_at = unix_ms(at);
            e.audible_at = unix_ms(at + self.cfg.playout_delay);
            match &mut s.adopt {
                Some(a) if a.inst == info.instance => a.audible = Some(e),
                _ => audible = Some(e),
            }
        }
        drop(g);
        if let Some(e) = audible {
            self.emit(e);
        }
    }

    /// The send loop fell behind by `gap` (`SendStallEvent`).
    pub fn stall(&self, gap: Duration) {
        let g = self.inner.lock();
        let mut e = ev(
            EventType::SendStallEvent,
            g.active.as_ref().map(|d| &d.track),
            g.active_inst,
        );
        drop(g);
        e.stall_ms = gap.as_millis() as i64;
        self.emit(e);
    }

    /// A snapshot for `playerUpdate`.
    pub fn state(&self) -> State {
        let g = self.inner.lock();
        let s = &*g;
        let mut st = State {
            paused: s.paused,
            volume: s.volume,
            filters: s.filters.clone(),
            instance: s.active_inst,
            deck_b_loaded: s.cued.is_some(),
            ..Default::default()
        };
        let mut rate = s.filters.position_rate();
        if let Some(a) = &s.active {
            rate *= a.tempo_ratio();
            st.track = Some(a.track.clone());
            st.playable = Some(a.playable.clone());
            st.position_ms = a.position_ms();
        } else if let Some(l) = &s.loading {
            st.track = Some(l.clone());
            st.position_ms = s.loading_from;
        }
        st.rate = rate;
        if let Some(last) = s.last
            && last.instance == s.active_inst
        {
            st.anchor_ms = last.pos_ms;
            st.anchor_sent_at = unix_ms(last.at);
            st.anchor_audible_at = unix_ms(last.at + self.cfg.playout_delay);
            let since = last.at.elapsed();
            st.sounding = !s.paused && since < LIVE_WINDOW;
            let mut pos = last.pos_ms;
            if st.sounding {
                pos += since.as_millis() as f64 * rate;
            }
            st.position_ms = pos as i64;
        }
        if let (Some(_), Some(inc)) = (&s.fade, &s.incoming) {
            st.mixing = true;
            st.mix_into = Some(inc.track.clone());
            st.mix_into_ms = inc.position_ms();
            st.mix_progress = s.fade.as_ref().map_or(0.0, |f| f.progress());
            st.hosting = s.host.as_ref().is_some_and(|h| !h.voice_ended);
        }
        st.mix_armed_at_ms = s.pending.as_ref().map(|p| p.at_ms);
        st.next_track = s.next.as_ref().map(|d| d.track.clone());
        st
    }

    /// Tear the player down: the track ends with `cleanup`, every deck and
    /// background task stops, and no further events are delivered.
    pub fn destroy(&self) {
        let mut g = self.inner.lock();
        let s = &mut *g;
        let d = s.active.take();
        let inst = s.active_inst;
        let was_loading = s.loading.take();
        self.clear_mix_locked(s);
        drop(g);
        if let Some(d) = d {
            let tr = d.track.clone();
            drop(d);
            self.emit(end_event(&tr, inst, EndReason::Cleanup));
        }
        if let Some(l) = was_loading {
            self.emit(end_event(&l, inst, EndReason::Cleanup));
        }
        self.ctx.cancel();
        self.events.close();
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.ctx.cancel();
    }
}

pub(crate) fn end_event(t: &Track, inst: u64, reason: EndReason) -> Event {
    let mut e = tev(EventType::TrackEndEvent, t, inst);
    e.reason = Some(reason);
    e
}

impl FrameSource for Player {
    fn next_frame(&self, out: &mut Frame) -> Option<FrameInfo> {
        Player::next_frame(self, out)
    }

    fn sent(&self, info: FrameInfo, at: Instant) {
        Player::sent(self, info, at)
    }

    fn stall(&self, gap: Duration) {
        Player::stall(self, gap)
    }
}
