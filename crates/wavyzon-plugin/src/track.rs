//! The track model shared by every source, the player and the HTTP API.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use crate::Failure;

/// A playable (or mirrorable) recording.
///
/// Tracks are encoded as unpadded base64url JSON, so **any Wavyzon node can
/// play any track**, whichever node resolved it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    /// Name of the source that owns `identifier` (`soundcloud`, `http`, …).
    pub source: String,
    /// Source-specific identifier.
    pub identifier: String,
    /// Missing decodes as empty, as in Cadence (hand-built encoded tracks).
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub author: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub album: String,
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub isrc: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artwork_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub uri: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_stream: bool,
}

impl Track {
    /// `source:identifier`, unique across the node.
    pub fn key(&self) -> String {
        format!("{}:{}", self.source, self.identifier)
    }

    /// Encode as unpadded base64url JSON.
    pub fn encode(&self) -> String {
        let raw = serde_json::to_vec(self).expect("track serializes");
        URL_SAFE_NO_PAD.encode(raw)
    }

    /// Decode a track produced by [`Track::encode`].
    pub fn decode(encoded: &str) -> Result<Track, Failure> {
        let raw = URL_SAFE_NO_PAD
            .decode(encoded.trim_end_matches('='))
            .map_err(|e| Failure::common("decode", "decode track", Some(e.to_string())))?;
        let t: Track =
            serde_json::from_slice(&raw).map_err(|e| Failure::common("decode", "decode track", Some(e.to_string())))?;
        if t.source.is_empty() || t.identifier.is_empty() {
            return Err(Failure::common(
                "decode",
                "decode track: missing source or identifier",
                None,
            ));
        }
        Ok(t)
    }
}

/// What a `loadtracks` call produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoadType {
    Track,
    Playlist,
    Search,
    Empty,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Playlist {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artwork_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub uri: String,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadResult {
    #[serde(rename = "loadType")]
    pub kind: LoadType,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<Track>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist: Option<Playlist>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
}

impl LoadResult {
    pub fn empty() -> Self {
        Self {
            kind: LoadType::Empty,
            tracks: vec![],
            playlist: None,
            error: None,
        }
    }
    pub fn single(t: Track) -> Self {
        Self {
            kind: LoadType::Track,
            tracks: vec![t],
            playlist: None,
            error: None,
        }
    }
    pub fn results(tracks: Vec<Track>) -> Self {
        if tracks.is_empty() {
            return Self::empty();
        }
        Self {
            kind: LoadType::Search,
            tracks,
            playlist: None,
            error: None,
        }
    }
    pub fn playlist(p: Playlist, tracks: Vec<Track>) -> Self {
        Self {
            kind: LoadType::Playlist,
            tracks,
            playlist: Some(p),
            error: None,
        }
    }
    pub fn error(f: Failure) -> Self {
        Self {
            kind: LoadType::Error,
            tracks: vec![],
            playlist: None,
            error: Some(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let t = Track {
            source: "http".into(),
            identifier: "https://example.com/a.mp3".into(),
            title: "A".into(),
            author: "B".into(),
            duration_ms: 1000,
            ..Default::default()
        };
        let enc = t.encode();
        assert!(!enc.contains('='));
        assert_eq!(Track::decode(&enc).unwrap(), t);
        assert_eq!(t.key(), "http:https://example.com/a.mp3");
    }

    #[test]
    fn rejects_incomplete() {
        let enc = URL_SAFE_NO_PAD.encode(br#"{"source":"","identifier":"x","title":"","author":""}"#);
        assert!(Track::decode(&enc).is_err());
        assert!(Track::decode("!!!").is_err());
    }

    #[test]
    fn title_and_author_are_optional() {
        let enc = URL_SAFE_NO_PAD.encode(br#"{"source":"soundcloud","identifier":"123"}"#);
        let t = Track::decode(&enc).unwrap();
        assert_eq!((t.title.as_str(), t.author.as_str()), ("", ""));
    }
}
