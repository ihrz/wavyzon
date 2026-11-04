//! Bounded on-disk cache of complete audio files (port of Cadence's
//! `internal/audiocache`).
//!
//! Files are named `<key>.audio`; writes go to a temp file and are renamed
//! into place only once complete, so a crash never leaves a truncated entry.
//! The least recently *played* files are removed first once the size limit is
//! exceeded; on startup the index is rebuilt from modification times (which
//! [`Store::get`] refreshes) and leftover temp files are deleted.
//!
//! Operations are synchronous `std::fs` calls on small metadata or on chunks
//! already in memory; callers in async code accept that, like Cadence did.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use parking_lot::Mutex;
use regex::Regex;

use crate::metrics::{Metrics, inc};

const FILE_EXT: &str = ".audio";
const TEMP_EXT: &str = ".tmp";
/// Largest file the cache accepts.
pub const MAX_FILE_SIZE: u64 = 64 << 20;

fn key_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_-]{1,96}$").unwrap())
}

/// Whether `key` is a valid cache key (`[A-Za-z0-9_-]{1,96}`).
pub fn valid_key(key: &str) -> bool {
    key_pattern().is_match(key)
}

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("audio cache key must be 1-96 characters of [A-Za-z0-9_-]")]
    InvalidKey,
    #[error("audio file exceeds the {} MiB cache limit", MAX_FILE_SIZE >> 20)]
    TooLarge,
    #[error("audio download did not complete: {written} of {expected} bytes")]
    Incomplete { written: u64, expected: u64 },
    #[error("cache writer already committed or aborted")]
    Spent,
    #[error("audio cache limit must be positive")]
    BadLimit,
    #[error("audio cache io: {0}")]
    Io(#[from] io::Error),
}

struct Index {
    used: u64,
    /// key → (size, tick)
    items: HashMap<String, (u64, u64)>,
    /// tick → key; highest tick = most recently played.
    order: BTreeMap<u64, String>,
    tick: u64,
}

impl Index {
    fn touch(&mut self, key: &str) {
        if let Some((_, t)) = self.items.get_mut(key) {
            self.order.remove(t);
            *t = self.tick;
            self.order.insert(self.tick, key.to_string());
            self.tick += 1;
        }
    }
    fn insert(&mut self, key: &str, size: u64) {
        if let Some((old, t)) = self.items.remove(key) {
            self.order.remove(&t);
            self.used -= old;
        }
        self.items.insert(key.to_string(), (size, self.tick));
        self.order.insert(self.tick, key.to_string());
        self.tick += 1;
        self.used += size;
    }
    fn remove(&mut self, key: &str) -> bool {
        match self.items.remove(key) {
            Some((size, t)) => {
                self.order.remove(&t);
                self.used -= size;
                true
            }
            None => false,
        }
    }
}

/// The on-disk audio cache.
pub struct Store {
    dir: PathBuf,
    limit: u64,
    metrics: Arc<Metrics>,
    index: Mutex<Index>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("dir", &self.dir)
            .field("limit", &self.limit)
            .finish()
    }
}

impl Store {
    /// Open (creating if needed) the cache in `dir`, capped at `limit` bytes.
    pub fn open(dir: impl Into<PathBuf>, limit: u64, metrics: Arc<Metrics>) -> Result<Self, CacheError> {
        if limit == 0 {
            return Err(CacheError::BadLimit);
        }
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        }
        let s = Self {
            dir,
            limit,
            metrics,
            index: Mutex::new(Index {
                used: 0,
                items: HashMap::new(),
                order: BTreeMap::new(),
                tick: 0,
            }),
        };
        s.scan()?;
        {
            let mut idx = s.index.lock();
            s.evict_locked(&mut idx);
            s.publish_locked(&idx);
            tracing::info!(dir = %s.dir.display(), files = idx.items.len(), used_bytes = idx.used, limit_bytes = limit, "audio cache ready");
        }
        Ok(s)
    }

    fn scan(&self) -> Result<(), CacheError> {
        let mut found: Vec<(String, u64, SystemTime)> = Vec::new();
        for d in fs::read_dir(&self.dir)? {
            let d = d?;
            let name = d.file_name().to_string_lossy().into_owned();
            let path = d.path();
            if name.ends_with(TEMP_EXT) {
                if let Err(e) = fs::remove_file(&path) {
                    tracing::warn!(file = %name, error = %e, "could not remove leftover temp file");
                }
                continue;
            }
            let Some(key) = name.strip_suffix(FILE_EXT) else {
                continue;
            };
            if !valid_key(key) {
                continue;
            }
            match d.metadata() {
                Ok(m) if m.is_file() => {
                    found.push((key.to_string(), m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)))
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(file = %name, error = %e, "could not stat cached file"),
            }
        }
        // oldest first, so the newest gets the highest tick
        found.sort_by_key(|(_, _, m)| *m);
        let mut idx = self.index.lock();
        for (key, size, _) in found {
            idx.insert(&key, size);
        }
        Ok(())
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}{FILE_EXT}"))
    }

    /// The cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Open a cached file and mark it recently played. Returns the file and
    /// its size.
    pub fn get(&self, key: &str) -> Option<(File, u64)> {
        if !valid_key(key) {
            return None;
        }
        let size = {
            let mut idx = self.index.lock();
            let size = idx.items.get(key).map(|(s, _)| *s);
            if size.is_some() {
                idx.touch(key);
            }
            size
        };
        let Some(size) = size else {
            inc(&self.metrics.audio_cache.miss);
            return None;
        };
        let path = self.path(key);
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(key, error = %e, "cached file unreadable, dropping it");
                self.drop_key(key);
                inc(&self.metrics.audio_cache.read_failed);
                return None;
            }
        };
        match file.metadata() {
            Ok(m) if m.len() == size => {}
            other => {
                tracing::warn!(key, error = ?other.err(), "cached file changed size, dropping it");
                self.drop_key(key);
                inc(&self.metrics.audio_cache.read_failed);
                return None;
            }
        }
        if let Err(e) = file.set_modified(SystemTime::now()) {
            tracing::debug!(key, error = %e, "could not touch cached file");
        }
        inc(&self.metrics.audio_cache.hit);
        Some((file, size))
    }

    /// Whether `key` is indexed.
    pub fn has(&self, key: &str) -> bool {
        self.index.lock().items.contains_key(key)
    }

    fn drop_key(&self, key: &str) {
        let mut idx = self.index.lock();
        if idx.remove(key) {
            self.delete_file(key);
            self.publish_locked(&idx);
        }
    }

    fn delete_file(&self, key: &str) {
        match fs::remove_file(self.path(key)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                tracing::warn!(key, error = %e, "could not delete cached file")
            }
            _ => {}
        }
    }

    fn evict_locked(&self, idx: &mut Index) {
        while idx.used > self.limit {
            let Some((_, key)) = idx.order.first_key_value().map(|(t, k)| (*t, k.clone())) else {
                break;
            };
            idx.remove(&key);
            self.delete_file(&key);
            inc(&self.metrics.audio_cache.evicted);
        }
    }

    fn publish_locked(&self, idx: &Index) {
        self.metrics.audio_cache_bytes.store(idx.used as i64, Ordering::Relaxed);
        self.metrics
            .audio_cache_files
            .store(idx.items.len() as i64, Ordering::Relaxed);
    }

    /// Start writing `key`. `expected` is the full size if known (0 = unknown).
    pub fn create(self: &Arc<Self>, key: &str, expected: u64) -> Result<Writer, CacheError> {
        if !valid_key(key) {
            return Err(CacheError::InvalidKey);
        }
        if expected > MAX_FILE_SIZE {
            return Err(CacheError::TooLarge);
        }
        let nonce: u64 = rand_nonce();
        let tmp = self.dir.join(format!("{key}-{nonce:016x}{TEMP_EXT}"));
        let file = fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        Ok(Writer {
            store: self.clone(),
            key: key.to_string(),
            tmp,
            file: Some(file),
            written: 0,
            expected,
            err: false,
        })
    }

    /// `(files, used bytes, limit bytes)`.
    pub fn stats(&self) -> (usize, u64, u64) {
        let idx = self.index.lock();
        (idx.items.len(), idx.used, self.limit)
    }

    /// Every indexed key.
    pub fn keys(&self) -> Vec<String> {
        self.index.lock().items.keys().cloned().collect()
    }

    /// The first indexed key that starts with `prefix`.
    pub fn find_prefix(&self, prefix: &str) -> Option<String> {
        self.index.lock().items.keys().find(|k| k.starts_with(prefix)).cloned()
    }
}

fn rand_nonce() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.finish()
}

/// An in-progress cache write. Dropping it without [`Writer::commit`] aborts.
pub struct Writer {
    store: Arc<Store>,
    key: String,
    tmp: PathBuf,
    file: Option<File>,
    written: u64,
    expected: u64,
    err: bool,
}

impl Writer {
    /// Bytes written so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Move the file into the cache. Fails (and discards it) when empty,
    /// shorter than `expected`, or after a write error.
    pub fn commit(mut self) -> Result<(), CacheError> {
        let Some(file) = self.file.take() else {
            return Err(CacheError::Spent);
        };
        let flushed = file.sync_all();
        drop(file);
        let fail = |e: CacheError, tmp: &Path| {
            let _ = fs::remove_file(tmp);
            Err(e)
        };
        if self.err {
            return fail(CacheError::Io(io::Error::other("write failed")), &self.tmp);
        }
        if let Err(e) = flushed {
            return fail(e.into(), &self.tmp);
        }
        if self.written == 0 || (self.expected > 0 && self.written != self.expected) {
            return fail(
                CacheError::Incomplete {
                    written: self.written,
                    expected: self.expected,
                },
                &self.tmp,
            );
        }
        let s = &self.store;
        let mut idx = s.index.lock();
        if let Err(e) = fs::rename(&self.tmp, s.path(&self.key)) {
            return fail(e.into(), &self.tmp);
        }
        idx.insert(&self.key, self.written);
        s.evict_locked(&mut idx);
        s.publish_locked(&idx);
        inc(&s.metrics.audio_cache.stored);
        Ok(())
    }

    /// Discard the write.
    pub fn abort(mut self) {
        self.discard();
    }

    fn discard(&mut self) {
        if self.file.take().is_some() {
            let _ = fs::remove_file(&self.tmp);
        }
    }
}

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.err {
            return Err(io::Error::other("cache writer failed earlier"));
        }
        if self.written + buf.len() as u64 > MAX_FILE_SIZE {
            self.err = true;
            return Err(io::Error::other(CacheError::TooLarge));
        }
        let Some(f) = self.file.as_mut() else {
            return Err(io::Error::other(CacheError::Spent));
        };
        match f.write(buf) {
            Ok(n) => {
                self.written += n as u64;
                Ok(n)
            }
            Err(e) => {
                self.err = true;
                Err(e)
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().map_or(Ok(()), |f| f.flush())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.discard();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::io::Read;
    use std::time::Duration;

    fn open(dir: &Path, limit: u64) -> Arc<Store> {
        Arc::new(Store::open(dir, limit, Arc::new(Metrics::default())).unwrap())
    }

    fn put(s: &Arc<Store>, key: &str, data: &[u8]) {
        let mut w = s.create(key, data.len() as u64).unwrap();
        w.write_all(data).unwrap();
        w.commit().unwrap();
    }

    fn read(s: &Store, key: &str) -> Option<Vec<u8>> {
        let (mut f, _) = s.get(key)?;
        let mut b = Vec::new();
        f.read_to_end(&mut b).unwrap();
        Some(b)
    }

    #[test]
    fn commit_then_get_returns_the_bytes() {
        let d = TempDir::new();
        let s = open(d.path(), 1 << 20);
        put(&s, "123-mp3_128", b"hello");
        assert_eq!(read(&s, "123-mp3_128").as_deref(), Some(&b"hello"[..]));
        assert!(read(&s, "999-mp3_128").is_none());
        assert_eq!(s.find_prefix("123-").as_deref(), Some("123-mp3_128"));
    }

    #[test]
    fn aborted_and_incomplete_writes_leave_nothing() {
        let d = TempDir::new();
        let s = open(d.path(), 1 << 20);
        let mut w = s.create("a", 0).unwrap();
        w.write_all(b"partial").unwrap();
        w.abort();
        let mut short = s.create("b", 100).unwrap();
        short.write_all(b"too short").unwrap();
        assert!(matches!(short.commit(), Err(CacheError::Incomplete { .. })));
        let mut dropped = s.create("c", 0).unwrap();
        dropped.write_all(b"x").unwrap();
        drop(dropped);
        assert!(!s.has("a") && !s.has("b") && !s.has("c"));
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 0, "files left behind");
    }

    #[test]
    fn evicts_least_recently_played_past_the_limit() {
        let d = TempDir::new();
        let metrics = Arc::new(Metrics::default());
        let s = Arc::new(Store::open(d.path(), 25, metrics.clone()).unwrap());
        put(&s, "old", &[b'o'; 10]);
        put(&s, "mid", &[b'm'; 10]);
        read(&s, "old");
        put(&s, "new", &[b'n'; 10]);
        assert!(!s.has("mid"), "least recently played entry survived");
        assert!(s.has("old") && s.has("new"));
        assert_eq!(s.stats().1, 20);
        assert_eq!(metrics.audio_cache.evicted.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.audio_cache_bytes.load(Ordering::Relaxed), 20);
    }

    #[test]
    fn reopen_restores_index_and_clears_temp_files() {
        let d = TempDir::new();
        let s = open(d.path(), 1 << 20);
        put(&s, "older", b"1");
        put(&s, "newer", b"22");
        let old = SystemTime::now() - Duration::from_secs(3600);
        File::options()
            .write(true)
            .open(d.path().join("older.audio"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        fs::write(d.path().join("x-123.tmp"), b"junk").unwrap();
        fs::write(d.path().join("not a key!.audio"), b"junk").unwrap();
        let s2 = open(d.path(), 2);
        assert!(
            s2.has("newer") && !s2.has("older"),
            "reopen should keep the newest file within the limit"
        );
        assert!(!d.path().join("x-123.tmp").exists(), "leftover temp file not removed");
        let (files, used, _) = s2.stats();
        assert_eq!((files, used), (1, 2));
    }

    #[test]
    fn rejects_bad_keys_and_oversized_files() {
        let d = TempDir::new();
        let s = open(d.path(), 1 << 20);
        assert!(matches!(s.create("../escape", 0), Err(CacheError::InvalidKey)));
        assert!(matches!(s.create("big", MAX_FILE_SIZE + 1), Err(CacheError::TooLarge)));
        assert!(s.get("../escape").is_none());
        assert!(Store::open(d.path(), 0, Arc::new(Metrics::default())).is_err());
    }

    #[test]
    fn file_changed_on_disk_is_dropped() {
        let d = TempDir::new();
        let s = open(d.path(), 1 << 20);
        put(&s, "k", b"abc");
        fs::write(d.path().join("k.audio"), b"abcdef").unwrap();
        assert!(read(&s, "k").is_none(), "resized file was served");
        assert!(!s.has("k"));
    }
}
