//! Lyrics out of a Genius song page, without an HTML parser dependency.
//!
//! The page holds the lyrics in one or more `<div data-lyrics-container="true">`
//! (split where ads are inserted). Line breaks are `<br>`; raw newlines in the
//! markup are formatting only. Inside a container, elements marked
//! `data-exclude-from-selection="true"` or with a `LyricsHeader` class (the
//! "N Contributors · Translations · Song Lyrics" header of the current layout)
//! are not lyrics. Same rules as the `lyricsgenius` Python library.

/// Elements with no closing tag.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr",
];

/// The lyrics of a song page: `None` when the page has no lyrics container
/// (not a song page, or a layout this parser does not know), otherwise the
/// tidied text (empty for a song without lyrics yet).
pub fn extract_lyrics(html: &str) -> Option<String> {
    const MARK: &str = "data-lyrics-container=\"true\"";
    let mut out = String::new();
    let mut found = false;
    let mut pos = 0;
    while let Some(off) = html[pos..].find(MARK) {
        let at = pos + off;
        // The attribute sits in the container's start tag: its `<` is the
        // last one before the attribute.
        let Some(start) = html[..at].rfind('<') else {
            pos = at + MARK.len();
            continue;
        };
        found = true;
        pos = walk(html, start, &mut out).max(at + MARK.len());
        out.push('\n');
    }
    found.then(|| tidy(&out))
}

/// One parsed tag.
struct Tag<'a> {
    name: String,
    attrs: &'a str,
    closing: bool,
    self_closing: bool,
    /// Index just past the `>`.
    end: usize,
}

/// The tag starting at `html[at] == '<'`, or `None` for a lone `<`.
fn parse_tag(html: &str, at: usize) -> Option<Tag<'_>> {
    let b = html.as_bytes();
    let mut i = at + 1;
    let closing = b.get(i) == Some(&b'/');
    if closing {
        i += 1;
    }
    let name_start = i;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'-') {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name = html[name_start..i].to_ascii_lowercase();
    let attrs_start = i;
    // Find the closing `>`, skipping quoted attribute values.
    let mut quote = None;
    while i < b.len() {
        match (quote, b[i]) {
            (None, b'"' | b'\'') => quote = Some(b[i]),
            (Some(q), c) if c == q => quote = None,
            (None, b'>') => break,
            _ => {}
        }
        i += 1;
    }
    let attrs = &html[attrs_start..i.min(b.len())];
    Some(Tag {
        name,
        attrs,
        closing,
        self_closing: attrs.trim_end().ends_with('/'),
        end: (i + 1).min(b.len()),
    })
}

fn excluded(attrs: &str) -> bool {
    attrs.contains("data-exclude-from-selection=\"true\"") || attrs.contains("LyricsHeader")
}

/// Append the text of the element starting at `start` to `out`; returns the
/// index just past its end tag (or the end of the page if it never closes).
fn walk(html: &str, start: usize, out: &mut String) -> usize {
    let Some(first) = parse_tag(html, start) else {
        return start + 1;
    };
    let mut depth = 1usize;
    // Depth at which an excluded element was opened.
    let mut skip_at: Option<usize> = None;
    let mut i = first.end;
    while i < html.len() {
        let Some(off) = html[i..].find('<') else {
            if skip_at.is_none() {
                push_text(out, &html[i..]);
            }
            break;
        };
        if skip_at.is_none() {
            push_text(out, &html[i..i + off]);
        }
        let at = i + off;
        if html[at..].starts_with("<!--") {
            i = html[at..].find("-->").map_or(html.len(), |e| at + e + 3);
            continue;
        }
        let Some(tag) = parse_tag(html, at) else {
            if skip_at.is_none() {
                out.push('<');
            }
            i = at + 1;
            continue;
        };
        i = tag.end;
        if tag.closing {
            if VOID.contains(&tag.name.as_str()) {
                continue;
            }
            depth -= 1;
            if skip_at.is_some_and(|d| depth < d) {
                skip_at = None;
            }
            if depth == 0 {
                return i;
            }
        } else if tag.name == "br" {
            if skip_at.is_none() {
                out.push('\n');
            }
        } else if tag.name == "script" || tag.name == "style" {
            let close = format!("</{}", tag.name);
            i = html[i..].find(&close).map_or(html.len(), |e| i + e);
        } else if !tag.self_closing && !VOID.contains(&tag.name.as_str()) {
            depth += 1;
            if skip_at.is_none() && excluded(tag.attrs) {
                skip_at = Some(depth);
            }
        }
    }
    html.len()
}

fn push_text(out: &mut String, raw: &str) {
    if raw.is_empty() {
        return;
    }
    out.push_str(&decode_entities(&raw.replace(['\n', '\r'], "")));
}

/// Decode the HTML entities Genius uses (named basics and numeric ones).
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest.find(';').filter(|&semi| semi <= 10).and_then(|semi| {
            let ent = &rest[1..semi];
            let c = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                "rsquo" => Some('\u{2019}'),
                "lsquo" => Some('\u{2018}'),
                "rdquo" => Some('\u{201d}'),
                "ldquo" => Some('\u{201c}'),
                "hellip" => Some('\u{2026}'),
                "mdash" => Some('\u{2014}'),
                "ndash" => Some('\u{2013}'),
                _ => ent
                    .strip_prefix("#x")
                    .or_else(|| ent.strip_prefix("#X"))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            };
            c.map(|c| (c, semi))
        });
        match decoded {
            Some((c, semi)) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Trailing spaces trimmed, at most one blank line in a row, no blank edges.
pub fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
            out.push('\n');
            continue;
        }
        blank = 0;
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like a current Genius page: a header inside the first
    /// container, an ad between two containers, links and formatting.
    const PAGE: &str = r#"<html><head><script>var x = "<div data-lyrics-container=\"false\">";</script></head><body>
<div class="Lyrics__Root"><div data-lyrics-container="true" class="Lyrics__Container-sc-1">
<div data-exclude-from-selection="true" class="LyricsHeader__Container-sc"><div>24 Contributors</div><span>Under Pressure Lyrics</span></div>[Intro]<br/>Mm ba ba de<br><br>[Verse 1: Freddie Mercury]<br/><a href="/123" class="ReferentFragment"><span>Pressure pushing down on me</span></a><br/>Pressing down on you, no man ask&#x27;d for<br/><i>Under</i> pressure &amp; that&#39;s it</div>
<div class="RightSidebar"><div>Ad here</div></div>
<div data-lyrics-container="true" class="Lyrics__Container-sc-1">[Chorus]<br/>It&rsquo;s the terror<!-- comment <br> --> of knowing<br/><img src="x.png"/>What this world is about</div>
</div><div>Footer</div></body></html>"#;

    #[test]
    fn extracts_every_container_without_the_header() {
        let got = extract_lyrics(PAGE).unwrap();
        assert_eq!(
            got,
            "[Intro]\nMm ba ba de\n\n[Verse 1: Freddie Mercury]\nPressure pushing down on me\nPressing down on you, no man ask'd for\nUnder pressure & that's it\n[Chorus]\nIt\u{2019}s the terror of knowing\nWhat this world is about"
        );
    }

    #[test]
    fn a_page_without_containers_is_none() {
        assert_eq!(extract_lyrics("<html><body><div>Sign in</div></body></html>"), None);
        assert_eq!(
            extract_lyrics(r#"<div data-lyrics-container="true"></div>"#).as_deref(),
            Some("")
        );
    }

    #[test]
    fn an_unclosed_container_stops_at_the_end_of_the_page() {
        assert_eq!(
            extract_lyrics(r#"<div data-lyrics-container="true">one<br>two"#).as_deref(),
            Some("one\ntwo")
        );
    }

    #[test]
    fn entities() {
        assert_eq!(
            decode_entities("a &amp; b &lt;c&gt; &#233;t&#xE9; &bogus; & x"),
            "a & b <c> été &bogus; & x"
        );
        assert_eq!(decode_entities("&#x110000;"), "&#x110000;");
    }

    #[test]
    fn tidy_collapses_blank_lines() {
        assert_eq!(tidy("\n\n a  \n\n\n\nb\n \n"), "a\n\nb");
    }
}
