//! Scored matching of a metadata-only track against candidates from a
//! playable source (port of Cadence's `source/match.go`).
//!
//! An ISRC match within ±5 s wins outright. Otherwise the score is
//! `0.65 × title match + 0.35 × artist-token overlap`, minus
//! [`VERSION_PENALTY`] for every version marker (remix, live, sped up…) that
//! one side has and the other does not. Candidates whose duration differs by
//! more than [`MATCH_DRIFT_MS`] are rejected.
//!
//! Two deliberate departures from Cadence, which let a SoundCloud upload
//! called "LUTHER - NOTORIOUS UET" stand in for Luther's "uet": the title
//! must match in both directions (see `title_match`), and a version the
//! request did not ask for is disqualifying rather than a mild penalty.

use std::collections::HashSet;

use wavyzon_plugin::Track;

/// Max duration difference between a track and its mirror.
pub const MATCH_DRIFT_MS: i64 = 5000;
/// Minimum score of an accepted text match.
pub const MIN_MATCH_SCORE: f64 = 0.55;
/// Penalty per mismatched version marker. Large enough that one mismatch
/// alone drops a perfect title and artist match below [`MIN_MATCH_SCORE`]:
/// a remix is never played for the original (Cadence used 0.4, which let a
/// lone remix through when nothing better was found).
pub const VERSION_PENALTY: f64 = 0.5;
/// Below this, a candidate's title does not name the same song (see
/// `title_match`).
pub const MIN_TITLE_MATCH: f64 = 0.75;

/// Words uploads add around a title that say nothing about which song it is.
const TITLE_NOISE: &[&str] = &[
    "official",
    "audio",
    "video",
    "clip",
    "lyric",
    "lyrics",
    "visualizer",
    "visualiser",
    "hq",
    "hd",
    "explicit",
    "music",
    "mv",
    "clean",
];

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
    let title = title_match(target, candidate);
    if title < MIN_TITLE_MATCH {
        return 0.0;
    }
    let mut score = 0.65 * title
        + 0.35
            * overlap(
                &tokens(&target.author),
                &tokens(&format!("{} {}", candidate.author, candidate.title)),
            );
    let (want, got) = (markers(&target.title), markers(&candidate.title));
    score -= VERSION_PENALTY * got.symmetric_difference(&want).count() as f64;
    score
}

/// The part of a title that names the song: bracketed text, and anything
/// from a `feat`/`ft`/`prod` credit onwards, are dropped.
fn core_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let padded = format!(" {} ", normalize(&out));
    let cut = [" feat ", " ft ", " featuring ", " prod ", " produced by "]
        .iter()
        .filter_map(|m| padded.find(m))
        .min()
        .unwrap_or(padded.len());
    padded[..cut].trim().to_string()
}

/// How well `candidate`'s title names the same song as `target`'s, in both
/// directions (0..=1): every word of the wanted title must appear in the
/// candidate, and the candidate's remaining words, once credits, artist
/// names and upload noise are removed, must belong to the wanted title.
fn title_match(target: &Track, candidate: &Track) -> f64 {
    let want = tokens(&core_title(&target.title));
    if want.is_empty() {
        return overlap(&tokens(&target.title), &tokens(&candidate.title));
    }
    let forward = overlap(&want, &tokens(&candidate.title));
    let artists = tokens(&format!("{} {}", target.author, candidate.author));
    let rest: HashSet<String> = tokens(&core_title(&candidate.title))
        .into_iter()
        .filter(|t| want.contains(t) || !(artists.contains(t) || TITLE_NOISE.contains(&t.as_str())))
        .collect();
    let reverse = if rest.is_empty() {
        1.0
    } else {
        rest.iter().filter(|t| want.contains(*t)).count() as f64 / rest.len() as f64
    };
    forward * reverse
}

/// The best candidate scoring at least [`MIN_MATCH_SCORE`]. On a tie the
/// earlier candidate wins, so the source's own search ranking decides
/// (Cadence kept the later one).
pub fn best_match(target: &Track, candidates: &[Track]) -> Option<Track> {
    let mut best: Option<(&Track, f64)> = None;
    for c in candidates {
        let s = match_score(target, c);
        if s >= MIN_MATCH_SCORE && best.is_none_or(|(_, b)| s > b) {
            best = Some((c, s));
        }
    }
    best.map(|(t, _)| t.clone())
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

    /// Luther's "uet" (Deezer, 150 s) is not on SoundCloud: only remixes,
    /// edits and an unrelated "NOTORIOUS UET" are. Nothing may be chosen,
    /// instead of the one-way title overlap picking that upload.
    #[test]
    fn unrelated_title_containing_the_words_is_not_a_match() {
        let target = t("dz", "uet", "Luther", 150_000);
        let soundcloud = vec![
            t("kaza", "LUTHER - NOTORIOUS UET", "#KAZA", 151_000),
            t("flip", "Luther - uet (saili flip #2)", "saili, Luther", 179_000),
            t("speed", "Luther - uet ( speed up )", "RÊVE", 124_000),
            t("gabber", "LUTHER UET: HARD GABBER REMIX", "T_Bug", 147_000),
            t(
                "hood",
                "LUTHER - UET (HOODTRAP REMIX) (Prod. by Haïse)",
                "Haïse",
                120_000,
            ),
            t("swoop", "UET (Swoop remix)", "Luther", 179_000),
            t("edit", "Luther - Uet (LB aka LABAT Edit)", "LB aka LABAT", 217_000),
            t("sped", "Uet - Luther, Amne  (Sped Up)", "Zorze", 137_000),
            t("remix", "Luther - uet (Remix)", "©2006, LUTHER", 166_000),
        ];
        assert!(best_match(&target, &soundcloud).is_none());
        // Once the real upload is there (any common shape), it is picked.
        for (title, author) in [
            ("uet", "Luther"),
            ("Luther - uet", "LUTHER"),
            ("Luther - uet (Official Audio)", "Some Label"),
            ("uet (feat. Amne)", "Luther"),
            ("UET [Clip Officiel]", "Luther"),
        ] {
            let mut with_real = soundcloud.clone();
            with_real.push(t("real", title, author, 150_400));
            assert_eq!(
                best_match(&target, &with_real).map(|t| t.identifier).as_deref(),
                Some("real"),
                "{title} / {author}"
            );
        }
    }

    #[test]
    fn a_lone_remix_is_not_played_for_the_original() {
        let target = t("", "Blinding Lights", "The Weeknd", 200_000);
        let only_remix = [t("remix", "Blinding Lights (Remix)", "The Weeknd", 200_500)];
        assert!(best_match(&target, &only_remix).is_none());
        let asked = t("", "Blinding Lights (Remix)", "The Weeknd", 200_000);
        assert!(
            best_match(&asked, &only_remix).is_some(),
            "the remix itself was asked for"
        );
    }

    #[test]
    fn ties_go_to_the_better_ranked_result() {
        let target = t("", "Song", "Band", 100_000);
        let c = [
            t("first", "Song", "Band", 100_000),
            t("second", "Song", "Band", 100_000),
        ];
        assert_eq!(best_match(&target, &c).unwrap().identifier, "first");
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
