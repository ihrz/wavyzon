//! Small helpers shared by the sources.

use std::io;

use futures::TryStreamExt;
use tokio::io::AsyncRead;
use tokio_util::io::StreamReader;
use wavyzon_plugin::Failure;

/// Boxed body of a [`wavyzon_plugin::Stream`].
pub type Body = Box<dyn AsyncRead + Send + Unpin>;

/// Turn a streaming response into an `AsyncRead`.
pub fn response_body(res: reqwest::Response) -> Body {
    Box::new(StreamReader::new(Box::pin(
        res.bytes_stream().map_err(io::Error::other),
    )))
}

/// JSON decode failure: the source changed its answer, not a node fault, so
/// it is `common` like Cadence's fallback classification.
pub fn bad_json(source: &str, e: serde_json::Error) -> Failure {
    Failure::common(source, format!("{source} failed"), Some(e.to_string()))
}

/// Percent-encode `s` for a path segment. Like Go's `url.PathEscape`, `:` and
/// `@` stay literal, so Deezer's `/track/isrc:XXXX` lookups go out as
/// Cadence sent them.
pub fn esc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
        .replace("%3A", ":")
        .replace("%40", "@")
}

/// Encode query pairs.
pub fn query(pairs: &[(&str, &str)]) -> String {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        q.append_pair(k, v);
    }
    q.finish()
}

#[cfg(test)]
mod tests {
    #[test]
    fn esc_is_a_path_segment_escape() {
        assert_eq!(super::esc("isrc:USRC17607839"), "isrc:USRC17607839");
        assert_eq!(super::esc("a b/c?d+e@f"), "a%20b%2Fc%3Fd%2Be@f");
    }
}
