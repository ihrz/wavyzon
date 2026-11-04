//! `AsyncRead` wrappers used by the resolver:
//!
//! - [`HeadReader`] serves a prefetched head from memory immediately while
//!   the rest of the track is opened in the background and fast-forwarded
//!   past the head ("instant starts");
//! - [`TeeReader`] copies a stream into the disk audio cache as it plays and
//!   commits it only once it ended cleanly.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};
use tokio::sync::oneshot;

use crate::audiocache::Writer;
use crate::metrics::{Metrics, inc};
use crate::util::Body;

enum Rest {
    Pending(oneshot::Receiver<io::Result<Body>>),
    Ready(Body),
    Done,
}

/// Head bytes first, then the background-opened remainder.
pub struct HeadReader {
    head: Bytes,
    pos: usize,
    rest: Rest,
}

impl HeadReader {
    /// `rest` must yield the full stream from byte 0; the first `head.len()`
    /// bytes are skipped in the background. `None` = the head is the whole file.
    pub fn new<F>(head: Bytes, rest: Option<F>) -> Self
    where
        F: Future<Output = io::Result<Body>> + Send + 'static,
    {
        let rest = match rest {
            None => Rest::Done,
            Some(open) => {
                let (tx, rx) = oneshot::channel();
                let skip = head.len() as u64;
                tokio::spawn(async move {
                    let res = async {
                        let mut body = open.await?;
                        let skipped = tokio::io::copy(&mut (&mut body).take(skip), &mut tokio::io::sink()).await?;
                        if skipped != skip {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "stream is shorter than its prefetched head",
                            ));
                        }
                        Ok(body)
                    }
                    .await;
                    let _ = tx.send(res);
                });
                Rest::Pending(rx)
            }
        };
        Self { head, pos: 0, rest }
    }
}

impl AsyncRead for HeadReader {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.head.len() {
            let n = (this.head.len() - this.pos).min(buf.remaining());
            buf.put_slice(&this.head[this.pos..this.pos + n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        loop {
            match &mut this.rest {
                Rest::Done => return Poll::Ready(Ok(())),
                Rest::Ready(body) => return Pin::new(body).poll_read(cx, buf),
                Rest::Pending(rx) => {
                    let res = ready!(Pin::new(rx).poll(cx));
                    match res {
                        Ok(Ok(body)) => this.rest = Rest::Ready(body),
                        Ok(Err(e)) => {
                            this.rest = Rest::Done;
                            return Poll::Ready(Err(e));
                        }
                        Err(_) => {
                            this.rest = Rest::Done;
                            return Poll::Ready(Err(io::Error::other("background open was cancelled")));
                        }
                    }
                }
            }
        }
    }
}

/// Copies everything read into a cache [`Writer`].
pub struct TeeReader {
    inner: Body,
    writer: Option<Writer>,
    metrics: Arc<Metrics>,
    key: String,
}

impl TeeReader {
    pub fn new(inner: Body, writer: Writer, metrics: Arc<Metrics>, key: String) -> Self {
        Self {
            inner,
            writer: Some(writer),
            metrics,
            key,
        }
    }
}

impl AsyncRead for TeeReader {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let res = ready!(Pin::new(&mut this.inner).poll_read(cx, buf));
        match &res {
            Ok(()) => {
                let new = &buf.filled()[before..];
                if new.is_empty() {
                    if buf.remaining() > 0 {
                        if let Some(w) = this.writer.take() {
                            let written = w.written();
                            match w.commit() {
                                Ok(()) => tracing::debug!(key = %this.key, bytes = written, "track cached to disk"),
                                Err(e) => {
                                    inc(&this.metrics.audio_cache.incomplete);
                                    tracing::warn!(key = %this.key, error = %e, "audio cache skipped an incomplete download");
                                }
                            }
                        }
                    }
                } else if let Some(w) = this.writer.as_mut() {
                    if let Err(e) = io::Write::write_all(w, new) {
                        inc(&this.metrics.audio_cache.write_failed);
                        tracing::warn!(key = %this.key, error = %e, "audio cache write failed, playback continues uncached");
                        if let Some(w) = this.writer.take() {
                            w.abort();
                        }
                    }
                }
            }
            Err(_) => {
                if let Some(w) = this.writer.take() {
                    w.abort();
                }
            }
        }
        Poll::Ready(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audiocache::Store;
    use crate::testutil::TempDir;

    #[tokio::test]
    async fn head_reader_joins_head_and_skipped_rest() {
        let full: &'static [u8] = b"0123456789abcdef";
        let open = async move { Ok(Box::new(full) as Body) };
        let mut r = HeadReader::new(Bytes::from_static(&full[..6]), Some(open));
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, full);

        let mut whole = HeadReader::new(Bytes::from_static(b"all"), None::<std::future::Ready<io::Result<Body>>>);
        let mut out = Vec::new();
        whole.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"all");

        let short = async move { Ok(Box::new(&b"01"[..]) as Body) };
        let mut r = HeadReader::new(Bytes::from_static(b"0123"), Some(short));
        let mut out = Vec::new();
        assert!(r.read_to_end(&mut out).await.is_err());
    }

    #[tokio::test]
    async fn tee_commits_only_complete_streams() {
        let d = TempDir::new();
        let m = Arc::new(Metrics::default());
        let store = Arc::new(Store::open(d.path(), 1 << 20, m.clone()).unwrap());
        let mut t = TeeReader::new(
            Box::new(&b"audio bytes"[..]),
            store.create("k", 0).unwrap(),
            m.clone(),
            "k".into(),
        );
        let mut out = Vec::new();
        t.read_to_end(&mut out).await.unwrap();
        assert!(store.has("k"));
        let mut partial = TeeReader::new(
            Box::new(&b"audio bytes"[..]),
            store.create("p", 0).unwrap(),
            m,
            "p".into(),
        );
        let mut buf = [0u8; 4];
        partial.read_exact(&mut buf).await.unwrap();
        drop(partial);
        assert!(!store.has("p"), "a stream dropped before its end must not be cached");
    }
}
