//! Scored matching of a metadata-only track against candidates from a
//! playable source (port of Cadence's `source/match.go`).
//!
//! An ISRC match within ±5 s wins outright. Otherwise the score is
//! `0.65 × title-token overlap + 0.35 × artist-token overlap`, minus
//! [`VERSION_PENALTY`] for every version marker (remix, live, sped up…) that
//! one side has and the other does not. Candidates whose duration differs by
//! more than [`MATCH_DRIFT_MS`] are rejected.

use std::collections::HashSet;

use wavyzon_plugin::Track;

/// Max duration difference between a track and its mirror.
pub const MATCH_DRIFT_MS: i64 = 5000;
/// Minimum score of an accepted text match.
pub const MIN_MATCH_SCORE: f64 = 0.55;
/// Penalty per mismatched version marker.
pub const VERSION_PENALTY: f64 = 0.4;

const VERSION_MARKERS: &[&str] = &[
    "remix",
    "live",
    "cover",
    "karaoke",
    "instrumental",
    "acoustic",
    "sped up",
    "slowed",
    "nightcore",
    "8d",
    "edit",
    "remaster",
    "demo",
    "reverb",
];
const FAN_EDIT_MARKERS: &[&str] = &["sped up", "slowed", "nightcore", "8d", "reverb", "karaoke"];

/// Upper-case, without dashes or spaces.
pub fn normalize_isrc(raw: &str) -> String {
    raw.trim().replace('-', "").to_uppercase()
}

fn normalize(s: &str) -> String {
    let mapped: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn tokens(s: &str) -> HashSet<String> {
    normalize(s).split_whitespace().map(str::to_string).collect()
}

fn overlap(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() {
        return 0.0;
    }
    a.iter().filter(|t| b.contains(*t)).count() as f64 / a.len() as f64
}

fn markers(s: &str) -> HashSet<&'static str> {
    let n = format!(" {} ", normalize(s));
    VERSION_MARKERS
        .iter()
        .copied()
        .filter(|m| n.contains(&format!(" {m} ")))
        .collect()
}

/// Whether the title is a fan edit (sped up, slowed, nightcore, 8D, reverb,
/// karaoke) — dropped from recommendations.
pub fn fan_edit(t: &Track) -> bool {
    let found = markers(&t.title);
    FAN_EDIT_MARKERS.iter().any(|m| found.contains(m))
}

/// Whether two durations are within [`MATCH_DRIFT_MS`] (unknown = yes).
pub fn within_drift(a: i64, b: i64) -> bool {
    a <= 0 || b <= 0 || (a - b).abs() <= MATCH_DRIFT_MS
}

/// Score `candidate` as a mirror of `target` (2 = ISRC match).
pub fn match_score(target: &Track, candidate: &Track) -> f64 {
    if !target.isrc.is_empty() && normalize_isrc(&target.isrc) == normalize_isrc(&candidate.isrc) {
        return if within_drift(target.duration_ms, candidate.duration_ms) {
            2.0
        } else {
            0.0
        };
    }
    if !within_drift(target.duration_ms, candidate.duration_ms) {
        return 0.0;
    }
    let mut score = 0.65 * overlap(&tokens(&target.title), &tokens(&candidate.title))
        + 0.35
            * overlap(
                &tokens(&target.author),
                &tokens(&format!("{} {}", candidate.author, candidate.title)),
            );
    let (want, got) = (markers(&target.title), markers(&candidate.title));
    score -= VERSION_PENALTY * got.symmetric_difference(&want).count() as f64;
    score
}

/// The best candidate scoring at least [`MIN_MATCH_SCORE`] (ties go to the
/// later candidate, like Cadence).
pub fn best_match(target: &Track, candidates: &[Track]) -> Option<Track> {
    let mut best: Option<&Track> = None;
    let mut best_score = MIN_MATCH_SCORE;
    for c in candidates {
        let s = match_score(target, c);
        if s >= best_score {
            best = Some(c);
            best_score = s;
        }
    }
    best.cloned()
}

fn marker_mismatch(want: &HashSet<&'static str>, t: &Track) -> (usize, usize) {
    let got = markers(&t.title);
    (want.difference(&got).count(), got.difference(want).count())
}

/// Stable-sort search results so versions the query did not ask for (live,
/// karaoke…) come last.
pub fn rank_for_query(query: &str, tracks: Vec<Track>) -> Vec<Track> {
    let want = markers(query);
    let mut keyed: Vec<((usize, usize), Track)> = tracks.into_iter().map(|t| (marker_mismatch(&want, &t), t)).collect();
    keyed.sort_by_key(|(k, _)| *k);
    keyed.into_iter().map(|(_, t)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, title: &str, author: &str, dur: i64) -> Track {
        Track {
            identifier: id.into(),
            title: title.into(),
            author: author.into(),
            duration_ms: dur,
            ..Default::default()
        }
    }

    #[test]
    fn match_prefers_isrc_and_rejects_wrong_version() {
        let target = Track {
            isrc: "USUG11904206".into(),
            ..t("", "Blinding Lights", "The Weeknd", 200_000)
        };
        let candidates = vec![
            t("remix", "Blinding Lights (Remix)", "The Weeknd", 201_000),
            t("live", "Blinding Lights - Live", "The Weeknd", 199_000),
            t("orig", "Blinding Lights", "The Weeknd", 200_500),
        ];
        assert_eq!(best_match(&target, &candidates).unwrap().identifier, "orig");
        let mut with_isrc = candidates.clone();
        with_isrc.push(Track {
            isrc: "US-UG1-19-04206".into(),
            ..t("isrc", "Totally different", "", 200_000)
        });
        assert_eq!(best_match(&target, &with_isrc).unwrap().identifier, "isrc");
        assert!(
            best_match(&target, &[t("", "Blinding Lights", "The Weeknd", 260_000)]).is_none(),
            "60 s drift accepted"
        );
        let sped = t("", "Blinding Lights (Sped Up)", "The Weeknd", 170_000);
        assert!(
            best_match(&sped.clone(), &[sped]).is_some(),
            "explicitly requested version rejected"
        );
    }

    #[test]
    fn search_demotes_versions_the_query_did_not_ask_for() {
        let input = vec![
            t("k", "Let The Light In (Instrumental Karaoke Version)", "", 0),
            t("o", "Let The Light In", "", 0),
            t("l", "Let The Light In (Live)", "", 0),
            t("o2", "Let The Light In (feat. Father John Misty)", "", 0),
        ];
        let order: Vec<String> = rank_for_query("let the light in", input.clone())
            .into_iter()
            .map(|t| t.identifier)
            .collect();
        assert_eq!(order, ["o", "o2", "l", "k"]);
        assert_eq!(rank_for_query("let the light in karaoke", input)[0].identifier, "k");
    }

    #[test]
    fn fan_edits_are_recognised() {
        for title in [
            "Gilded Lily (Sped Up)",
            "This Side of Paradise (slowed)",
            "i met sarah in the bathroom {slow//reverb}",
            "Swing Lynn (Slowed Version)",
            "Song - Nightcore",
            "Track (8D Audio)",
            "Hit (Karaoke Version)",
        ] {
            assert!(fan_edit(&t("", title, "", 0)), "{title} should be a fan edit");
        }
        for title in [
            "Sweater Weather",
            "Song (Live)",
            "Song (Remix)",
            "Song - Acoustic",
            "Speedway",
            "Slowdive",
        ] {
            assert!(!fan_edit(&t("", title, "", 0)), "{title} is not a fan edit");
        }
    }

    #[test]
    fn isrc_normalization() {
        assert_eq!(normalize_isrc(" us-abc-12-34567 "), "USABC1234567");
        assert!(within_drift(0, 100_000));
        assert!(!within_drift(1, 5002));
    }
}
