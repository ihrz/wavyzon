//! Which Genius search hit is the track. Strict on purpose: a wrong song's
//! lyrics are worse than none, and lyrics.ovh is asked when Genius misses.

use serde::Deserialize;
use wavyzon_sources::lyrics::{clean_artist, clean_title};

/// A song in Genius' search answers (both the API and the web search).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Hit {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub primary_artist: Artist,
    /// Every credited artist ("Queen & David Bowie").
    #[serde(default)]
    pub artist_names: Option<String>,
    /// `/Queen-and-david-bowie-under-pressure-lyrics`.
    #[serde(default)]
    pub path: String,
    /// `complete`, `unreleased`, …
    #[serde(default)]
    pub lyrics_state: Option<String>,
    #[serde(default)]
    pub instrumental: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Artist {
    #[serde(default)]
    pub name: String,
}

/// Lowercase words of letters and digits, single-spaced, padded with a space
/// on each side so `contains(" queen ")` matches whole words only.
pub fn words(s: &str) -> String {
    let mut out = String::from(" ");
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with(' ') {
            out.push(' ');
        }
    }
    if !out.ends_with(' ') {
        out.push(' ');
    }
    out
}

/// The title without any bracketed part.
fn bare_title(title: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    for c in title.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn same_title(track: &str, hit: &str) -> bool {
    let (t, h) = (words(&clean_title(track)), words(&clean_title(hit)));
    if t.trim().is_empty() {
        return false;
    }
    t == h || {
        let (t, h) = (words(&bare_title(track)), words(&bare_title(hit)));
        !t.trim().is_empty() && t == h
    }
}

fn same_artist(track: &str, hit: &Hit) -> bool {
    let a = words(&clean_artist(track));
    if a.trim().is_empty() {
        return false;
    }
    let names = [Some(hit.primary_artist.name.as_str()), hit.artist_names.as_deref()];
    names
        .into_iter()
        .flatten()
        .map(words)
        .any(|n| !n.trim().is_empty() && (n.contains(&a) || a.contains(&n)))
}

/// The first hit with the track's title and artist whose lyrics are
/// published (an unreleased song's page has none).
pub fn pick<'a>(artist: &str, title: &str, hits: &'a [Hit]) -> Option<&'a Hit> {
    hits.iter().find(|h| {
        h.path.starts_with('/')
            && h.lyrics_state.as_deref().is_none_or(|s| s == "complete")
            && same_title(title, &h.title)
            && same_artist(artist, h)
    })
}

/// The search text for a track: cleaned title and first artist.
pub fn query(artist: &str, title: &str) -> String {
    format!("{} {}", clean_title(title), clean_artist(artist))
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(title: &str, artist: &str, names: Option<&str>) -> Hit {
        Hit {
            title: title.into(),
            primary_artist: Artist { name: artist.into() },
            artist_names: names.map(Into::into),
            path: format!("/{}", title.replace(' ', "-")),
            lyrics_state: Some("complete".into()),
            instrumental: false,
        }
    }

    #[test]
    fn picks_the_same_song_by_the_same_artist() {
        let hits = [
            hit("Under Pressure", "Vanilla Ice", None),
            hit("Under Pressure", "Queen & David Bowie", Some("Queen & David Bowie")),
        ];
        let got = pick("Queen, David Bowie", "Under Pressure - Remastered 2011", &hits).unwrap();
        assert_eq!(got.primary_artist.name, "Queen & David Bowie");
        // A YouTube upload: channel suffix and video tag.
        assert!(pick("QueenVEVO", "Under Pressure (Official Video)", &hits).is_some());
        // Curly apostrophes and case.
        let hits = [hit("Don’t Stop Me Now", "Queen", None)];
        assert!(pick("queen", "Don't stop me now", &hits).is_some());
    }

    #[test]
    fn refuses_other_songs_and_artists() {
        let hits = [
            hit("Hello Again", "Adele", None),
            hit("Hello", "Lionel Richie", None),
            hit("Hello (Remix)", "Adelemma", None),
        ];
        assert!(pick("Adele", "Hello", &hits).is_none());
        let mut unreleased = hit("Hello", "Adele", None);
        unreleased.lyrics_state = Some("unreleased".into());
        assert!(pick("Adele", "Hello", &[unreleased]).is_none());
        let mut no_path = hit("Hello", "Adele", None);
        no_path.path = "https://evil.example/x".into();
        assert!(pick("Adele", "Hello", &[no_path]).is_none());
    }

    #[test]
    fn query_is_cleaned() {
        assert_eq!(
            query("Daft Punk - Topic", "One More Time (Official Video)"),
            "One More Time Daft Punk"
        );
        assert_eq!(words("Don’t  Stop!"), " don t stop ");
    }
}
