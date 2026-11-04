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

/// JSON decode failure.
pub fn bad_json(source: &str, e: serde_json::Error) -> Failure {
    Failure::fault(source, "unexpected response", Some(e.to_string()))
}

/// Percent-encode `s` for a path segment or query value.
pub fn esc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

/// Encode query pairs.
pub fn query(pairs: &[(&str, &str)]) -> String {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        q.append_pair(k, v);
    }
    q.finish()
}
