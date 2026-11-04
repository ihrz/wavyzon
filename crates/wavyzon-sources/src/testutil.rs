//! Test helpers: in-process axum servers standing in for the real services,
//! and a self-deleting temp dir.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;

/// Serve `router` on an ephemeral localhost port; returns `http://127.0.0.1:port`.
pub async fn serve(router: Router) -> String {
    serve_with(|_| router).await
}

/// Like [`serve`], but the router is built knowing its own base URL.
pub async fn serve_with(build: impl FnOnce(String) -> Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = build(base.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    base
}

/// A unique directory under the system temp dir, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "wavyzon-sources-test-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
