//! Lavalink v4 wire shapes (port of Cadence's `internal/lavalink`).

use serde::Serialize;
use serde_json::{Map, Value, json};
use wavyzon_player::{Filters, State};
use wavyzon_plugin::{Event, EventType, Failure, LoadResult, LoadType, Playlist, Track};

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub identifier: String,
    pub is_seekable: bool,
    pub author: String,
    pub length: i64,
    pub is_stream: bool,
    pub position: i64,
    pub title: String,
    pub uri: Option<String>,
    pub artwork_url: Option<String>,
    pub isrc: Option<String>,
    pub source_name: String,
}

/// A track in Lavalink's form.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LavalinkTrack {
    pub encoded: String,
    pub info: TrackInfo,
    pub plugin_info: Map<String, Value>,
    pub user_data: Map<String, Value>,
}

fn optional(v: &str) -> Option<String> {
    (!v.is_empty()).then(|| v.to_string())
}

pub fn from_track(t: &Track) -> LavalinkTrack {
    LavalinkTrack {
        encoded: t.encode(),
        info: TrackInfo {
            identifier: t.identifier.clone(),
            is_seekable: !t.is_stream,
            author: t.author.clone(),
            length: t.duration_ms,
            is_stream: t.is_stream,
            position: 0,
            title: t.title.clone(),
            uri: optional(&t.uri),
            artwork_url: optional(&t.artwork_url),
            isrc: optional(&t.isrc),
            source_name: t.source.clone(),
        },
        plugin_info: Map::new(),
        user_data: Map::new(),
    }
}

/// Lavalink's exception object.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Exception {
    pub message: String,
    pub severity: wavyzon_plugin::Severity,
    pub cause: String,
    pub cause_stack_trace: String,
}

pub fn from_failure(f: &Failure) -> Exception {
    Exception {
        message: f.message.clone(),
        severity: f.severity,
        cause: f.cause.clone(),
        cause_stack_trace: String::new(),
    }
}

fn playlist_plugin_info(p: Option<&Playlist>) -> Value {
    let mut out = Map::new();
    if let Some(p) = p {
        if !p.artwork_url.is_empty() {
            out.insert("artworkUrl".into(), p.artwork_url.clone().into());
        }
        if !p.uri.is_empty() {
            out.insert("url".into(), p.uri.clone().into());
        }
        out.insert("totalTracks".into(), p.total.into());
    }
    Value::Object(out)
}

/// A `loadtracks` result in Lavalink's shape.
pub fn load_result(res: &LoadResult) -> Value {
    let tracks: Vec<LavalinkTrack> = res.tracks.iter().map(from_track).collect();
    match res.kind {
        LoadType::Track if !tracks.is_empty() => json!({ "loadType": "track", "data": tracks[0] }),
        LoadType::Playlist => json!({
            "loadType": "playlist",
            "data": {
                "info": { "name": res.playlist.as_ref().map(|p| p.name.as_str()).unwrap_or(""), "selectedTrack": -1 },
                "pluginInfo": playlist_plugin_info(res.playlist.as_ref()),
                "tracks": tracks,
            }
        }),
        LoadType::Search => json!({ "loadType": "search", "data": tracks }),
        LoadType::Error => {
            let f = res
                .error
                .clone()
                .unwrap_or_else(|| Failure::fault("load", "unknown error", None));
            json!({ "loadType": "error", "data": from_failure(&f) })
        }
        _ => json!({ "loadType": "empty", "data": {} }),
    }
}

/// `state` of a v4 player / `playerUpdate`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct PlayerState {
    pub time: i64,
    pub position: i64,
    pub connected: bool,
    pub ping: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VoiceState {
    pub token: String,
    pub endpoint: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub channel_id: String,
}

/// A v4 player.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Player {
    pub guild_id: String,
    pub track: Option<LavalinkTrack>,
    pub volume: i32,
    pub paused: bool,
    pub state: PlayerState,
    pub voice: VoiceState,
    pub filters: Filters,
}

pub fn from_player(guild_id: &str, st: &State, state: PlayerState, voice: VoiceState) -> Player {
    Player {
        guild_id: guild_id.to_string(),
        track: st.track.as_ref().map(from_track),
        volume: st.volume,
        paused: st.paused,
        state,
        voice,
        filters: st.filters.clone(),
    }
}

pub fn player_update(guild_id: &str, state: PlayerState) -> Value {
    json!({ "op": "playerUpdate", "guildId": guild_id, "state": state })
}

/// Events every Lavalink client understands; the rest need `wavyzonEvents`.
pub fn is_standard_event(t: EventType) -> bool {
    matches!(
        t,
        EventType::TrackStartEvent
            | EventType::TrackEndEvent
            | EventType::TrackExceptionEvent
            | EventType::TrackStuckEvent
            | EventType::WebSocketClosedEvent
    )
}

/// Wire name of an event type (`PositionEvent` for [`EventType::PositionJump`]).
pub fn event_name(t: EventType) -> String {
    match serde_json::to_value(t) {
        Ok(Value::String(s)) => s,
        _ => format!("{t:?}"),
    }
}

/// An event in Lavalink's form. `user_data` fills the track's `userData`.
pub fn event(e: &Event, user_data: impl Fn(&str) -> Map<String, Value>) -> Value {
    let mut out = Map::new();
    out.insert("op".into(), "event".into());
    out.insert("type".into(), event_name(e.kind).into());
    out.insert("guildId".into(), e.guild_id.clone().into());
    let lt = |t: &Track| {
        let mut l = from_track(t);
        l.user_data = user_data(&l.encoded);
        serde_json::to_value(l).unwrap_or(Value::Null)
    };
    if let Some(t) = &e.track {
        out.insert("track".into(), lt(t));
    }
    match e.kind {
        EventType::TrackEndEvent => {
            let reason = e
                .reason
                .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
                .unwrap_or("finished".into());
            out.insert("reason".into(), reason);
        }
        EventType::TrackExceptionEvent => {
            let f = e
                .exception
                .clone()
                .unwrap_or_else(|| Failure::fault("player", "unknown failure", None));
            out.insert(
                "exception".into(),
                serde_json::to_value(from_failure(&f)).unwrap_or(Value::Null),
            );
        }
        EventType::TrackStuckEvent => {
            out.insert("thresholdMs".into(), e.threshold_ms.into());
        }
        EventType::WebSocketClosedEvent => {
            out.insert("code".into(), e.code.unwrap_or(0).into());
            out.insert("reason".into(), e.close_reason.clone().into());
            out.insert("byRemote".into(), true.into());
            if let Some(t) = e.terminal {
                out.insert("terminal".into(), t.into());
            }
        }
        _ => {
            out.insert("instance".into(), e.instance.into());
            if e.position_ms != 0 {
                out.insert("positionMs".into(), e.position_ms.into());
            }
            if e.sent_at != 0 {
                out.insert("sentAt".into(), e.sent_at.into());
                out.insert("audibleAt".into(), e.audible_at.into());
            }
            if e.stall_ms != 0 {
                out.insert("stallMs".into(), e.stall_ms.into());
            }
            if e.duration_ms != 0 {
                out.insert("durationMs".into(), e.duration_ms.into());
            }
            if let Some(into) = &e.into {
                out.insert("into".into(), lt(into));
                out.insert("intoPositionMs".into(), e.into_ms.into());
            }
        }
    }
    Value::Object(out)
}

#[derive(Debug, Clone, Copy, Serialize, Default)]
pub struct Memory {
    pub free: u64,
    pub used: u64,
    pub allocated: u64,
    pub reservable: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Cpu {
    pub cores: usize,
    pub system_load: f64,
    pub lavalink_load: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub players: usize,
    pub playing_players: usize,
    pub uptime: i64,
    pub memory: Memory,
    pub cpu: Cpu,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Version {
    pub semver: String,
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre_release: Option<String>,
    pub build: Option<String>,
}

pub fn parse_version(v: &str) -> Version {
    let (core, build) = match v.split_once('+') {
        Some((c, b)) => (c, Some(b.to_string())),
        None => (v, None),
    };
    let (core, pre) = match core.split_once('-') {
        Some((c, p)) => (c, Some(p.to_string())),
        None => (core, None),
    };
    let mut nums = core.splitn(3, '.').map(|p| p.parse().unwrap_or(0));
    Version {
        semver: v.to_string(),
        major: nums.next().unwrap_or(0),
        minor: nums.next().unwrap_or(0),
        patch: nums.next().unwrap_or(0),
        pre_release: pre,
        build,
    }
}

#[cfg(test)]
mod tests {
    use wavyzon_plugin::EndReason;

    use super::*;

    fn track() -> Track {
        Track {
            source: "tone".into(),
            identifier: "440:1000".into(),
            title: "Tone".into(),
            author: "a".into(),
            duration_ms: 1000,
            ..Default::default()
        }
    }

    #[test]
    fn track_shape() {
        let v = serde_json::to_value(from_track(&track())).unwrap();
        assert_eq!(v["info"]["length"], 1000);
        assert_eq!(v["info"]["isSeekable"], true);
        assert_eq!(v["info"]["sourceName"], "tone");
        assert!(v["info"]["uri"].is_null());
        assert!(v["pluginInfo"].is_object() && v["userData"].is_object());
        assert_eq!(Track::decode(v["encoded"].as_str().unwrap()).unwrap(), track());
    }

    #[test]
    fn load_shapes() {
        assert_eq!(
            load_result(&LoadResult::empty()),
            json!({"loadType": "empty", "data": {}})
        );
        let p = Playlist {
            name: "P".into(),
            uri: "https://x".into(),
            total: 9,
            ..Default::default()
        };
        let v = load_result(&LoadResult::playlist(p, vec![track()]));
        assert_eq!(v["data"]["info"], json!({"name": "P", "selectedTrack": -1}));
        assert_eq!(v["data"]["pluginInfo"], json!({"url": "https://x", "totalTracks": 9}));
        let e = load_result(&LoadResult::error(Failure::suspicious(
            "sc",
            "refused",
            Some("429".into()),
        )));
        assert_eq!(
            e["data"],
            json!({"message": "refused", "severity": "suspicious", "cause": "429", "causeStackTrace": ""})
        );
    }

    #[test]
    fn events_are_filtered_and_shaped() {
        assert!(is_standard_event(EventType::TrackEndEvent));
        assert!(!is_standard_event(EventType::TrackAudibleEvent));
        let mut e = Event::new(EventType::TrackEndEvent, "1")
            .with_track(&track())
            .with_instance(3);
        e.reason = Some(EndReason::LoadFailed);
        let v = event(&e, |_| Map::from_iter([("a".to_string(), Value::from(1))]));
        assert_eq!(v["reason"], "loadFailed");
        assert_eq!(v["track"]["userData"]["a"], 1);
        assert!(v.get("instance").is_none());
        let mut a = Event::new(EventType::TrackAudibleEvent, "1").with_instance(4);
        a.sent_at = 10;
        a.audible_at = 20;
        let v = event(&a, |_| Map::new());
        assert_eq!((v["instance"].as_u64(), v["audibleAt"].as_i64()), (Some(4), Some(20)));
        assert_eq!(event_name(EventType::PositionJump), "PositionEvent");
    }

    #[test]
    fn versions() {
        let v = parse_version("1.2.3-rc.1+abc");
        assert_eq!((v.major, v.minor, v.patch), (1, 2, 3));
        assert_eq!(v.pre_release.as_deref(), Some("rc.1"));
        assert_eq!(v.build.as_deref(), Some("abc"));
    }
}
