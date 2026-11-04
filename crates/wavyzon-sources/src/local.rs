//! The `local` source: audio files under one configured root directory.
//!
//! Disabled unless a root is configured. Identifiers are `local:<path>`,
//! `file://<absolute path>` or an absolute path; relative paths are taken from
//! the root. Every path is canonicalized (symlinks resolved) and refused if it
//! escapes the root — both when loading and when opening, since encoded tracks
//! come from clients. A directory loads as a playlist of its audio files.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use wavyzon_plugin::{Failure, Format, LoadResult, Playable, Playlist, Source, Stream, Track};

use crate::probe::{SNIFF_BYTES, hint_for_path, sniff};

pub const NAME: &str = "local";
/// Identifier prefix.
pub const PREFIX: &str = "local:";
const DIR_LIMIT: usize = 1000;

/// The local file source.
pub struct LocalSource {
    root: PathBuf,
}

impl LocalSource {
    /// Fails when `root` does not exist or is not a directory.
    pub fn new(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let root = std::fs::canonicalize(root)?;
        if !root.is_dir() {
            return Err(std::io::Error::other("local source root is not a directory"));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Map an identifier to a canonical path inside the root.
    pub async fn resolve(&self, identifier: &str) -> Result<PathBuf, Failure> {
        let raw = identifier
            .strip_prefix(PREFIX)
            .or_else(|| identifier.strip_prefix("file://"))
            .unwrap_or(identifier)
            .trim();
        let raw = url::form_urlencoded::parse(format!("p={}", raw.replace('+', "%2B")).as_bytes())
            .next()
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        if raw.is_empty() || raw.contains('\0') {
            return Err(Failure::common(NAME, "empty path", None));
        }
        let p = Path::new(&raw);
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        let canon = tokio::fs::canonicalize(&joined)
            .await
            .map_err(|e| Failure::common(NAME, "file not found", Some(e.to_string())))?;
        if !canon.starts_with(&self.root) {
            return Err(Failure::common(NAME, "path escapes the local root", None));
        }
        Ok(canon)
    }

    fn relative(&self, p: &Path) -> String {
        p.strip_prefix(&self.root)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/")
    }

    async fn file_track(&self, path: &Path) -> Result<Track, Failure> {
        let hint = hint_for_path(&path.to_string_lossy());
        let meta = tokio::fs::metadata(path)
            .await
            .map_err(|e| Failure::common(NAME, "file not found", Some(e.to_string())))?;
        let mut head = vec![0u8; SNIFF_BYTES];
        let mut f = tokio::fs::File::open(path)
            .await
            .map_err(|e| Failure::common(NAME, "cannot open file", Some(e.to_string())))?;
        let n = read_up_to(&mut f, &mut head).await;
        head.truncate(n);
        let s = sniff(&head, Some(meta.len()), hint);
        let rel = self.relative(path);
        Ok(Track {
            source: NAME.into(),
            identifier: rel.clone(),
            title: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| rel.clone()),
            author: path
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Local file".into()),
            duration_ms: s.duration_ms,
            uri: format!("{PREFIX}{rel}"),
            ..Default::default()
        })
    }
}

async fn read_up_to(f: &mut tokio::fs::File, buf: &mut [u8]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]).await {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    n
}

#[async_trait]
impl Source for LocalSource {
    fn name(&self) -> &str {
        NAME
    }

    fn matches_url(&self, url: &str) -> bool {
        url.starts_with(PREFIX) || url.starts_with("file://") || url.starts_with('/')
    }

    async fn load_url(&self, url: &str) -> Result<LoadResult, Failure> {
        let path = self.resolve(url).await?;
        if !path.is_dir() {
            if hint_for_path(&path.to_string_lossy()).is_none() {
                return Err(Failure::common(NAME, "not an audio file", None));
            }
            return Ok(LoadResult::single(self.file_track(&path).await?));
        }
        let mut entries = tokio::fs::read_dir(&path)
            .await
            .map_err(|e| Failure::common(NAME, "cannot list directory", Some(e.to_string())))?;
        let mut files = Vec::new();
        while let Ok(Some(e)) = entries.next_entry().await {
            let p = e.path();
            if p.is_file() && hint_for_path(&p.to_string_lossy()).is_some() {
                files.push(p);
            }
        }
        files.sort();
        files.truncate(DIR_LIMIT);
        let mut tracks = Vec::with_capacity(files.len());
        for f in files {
            // re-check: entries may be symlinks pointing outside the root
            if let Ok(canon) = self.resolve(&f.to_string_lossy()).await {
                if let Ok(t) = self.file_track(&canon).await {
                    tracks.push(t);
                }
            }
        }
        if tracks.is_empty() {
            return Ok(LoadResult::empty());
        }
        let rel = self.relative(&path);
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| rel.clone());
        Ok(LoadResult::playlist(
            Playlist {
                name,
                uri: format!("{PREFIX}{rel}"),
                total: tracks.len(),
                ..Default::default()
            },
            tracks,
        ))
    }

    async fn track(&self, identifier: &str) -> Result<Track, Failure> {
        let p = self.resolve(identifier).await?;
        self.file_track(&p).await
    }

    fn playable(&self) -> Option<&dyn Playable> {
        Some(self)
    }
}

#[async_trait]
impl Playable for LocalSource {
    async fn open(&self, track: &Track, start_ms: i64) -> Result<Stream, Failure> {
        let path = self.resolve(&track.identifier).await?;
        if path.is_dir() {
            return Err(Failure::common(NAME, "not a file", None));
        }
        let hint = hint_for_path(&path.to_string_lossy());
        let mut f = tokio::fs::File::open(&path)
            .await
            .map_err(|e| Failure::common(NAME, "cannot open file", Some(e.to_string())))?;
        let mut start_sample = 0;
        if start_ms > 0 && track.duration_ms > start_ms {
            let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
            let mut head = vec![0u8; SNIFF_BYTES];
            let n = read_up_to(&mut f, &mut head).await;
            head.truncate(n);
            let s = sniff(&head, Some(len), hint);
            let offset = if s.byte_seekable && len > s.audio_start {
                start_sample = start_ms * 48;
                s.audio_start + (len - s.audio_start) * start_ms as u64 / track.duration_ms as u64
            } else {
                0
            };
            f.seek(SeekFrom::Start(offset))
                .await
                .map_err(|e| Failure::common(NAME, "seek failed", Some(e.to_string())))?;
        }
        Ok(Stream {
            body: Box::new(f),
            format: Format::Encoded {
                hint: hint.map(str::to_string),
            },
            start_sample,
            live: false,
        })
    }

    fn seekable(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::tests::{fake_mp3, fake_wav};
    use crate::testutil::TempDir;
    use wavyzon_plugin::LoadType;

    fn setup() -> (TempDir, TempDir, LocalSource) {
        let root = TempDir::new();
        let outside = TempDir::new();
        std::fs::create_dir_all(root.path().join("album")).unwrap();
        std::fs::write(root.path().join("album/01 One.mp3"), fake_mp3(20 + 16_000 * 4)).unwrap();
        std::fs::write(root.path().join("album/02 Two.wav"), fake_wav(2)).unwrap();
        std::fs::write(root.path().join("album/notes.txt"), "x").unwrap();
        std::fs::write(outside.path().join("secret.mp3"), fake_mp3(1000)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path().join("secret.mp3"), root.path().join("album/03 link.mp3")).unwrap();
        let src = LocalSource::new(root.path()).unwrap();
        (root, outside, src)
    }

    #[tokio::test]
    async fn loads_files_and_directories() {
        let (_root, _o, src) = setup();
        let res = src.load_url("local:album/01%20One.mp3").await.unwrap();
        assert_eq!(res.kind, LoadType::Track);
        assert_eq!(res.tracks[0].title, "01 One");
        assert_eq!(res.tracks[0].author, "album");
        assert_eq!(res.tracks[0].duration_ms, 4000);
        let dir = src.load_url("local:album").await.unwrap();
        assert_eq!(dir.kind, LoadType::Playlist);
        let names: Vec<&str> = dir.tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(
            names,
            ["01 One", "02 Two"],
            "non-audio files and escaping symlinks are skipped"
        );
        assert_eq!(dir.tracks[1].duration_ms, 2000);
    }

    #[tokio::test]
    async fn refuses_paths_escaping_the_root() {
        let (_root, outside, src) = setup();
        for id in [
            "local:../x",
            "local:album/../../etc/passwd",
            &format!("file://{}", outside.path().join("secret.mp3").display()),
            "local:album/03 link.mp3",
        ] {
            assert!(src.load_url(id).await.is_err(), "{id} was accepted");
        }
        let crafted = Track {
            source: NAME.into(),
            identifier: outside.path().join("secret.mp3").to_string_lossy().into_owned(),
            ..Default::default()
        };
        assert!(src.open(&crafted, 0).await.is_err());
    }

    #[tokio::test]
    async fn open_seeks_mp3_by_bytes() {
        let (_root, _o, src) = setup();
        let t = src.track("album/01 One.mp3").await.unwrap();
        let mut s = src.open(&t, 2000).await.unwrap();
        assert_eq!(s.start_sample, 96_000);
        assert_eq!(s.format, Format::mp3());
        let mut rest = Vec::new();
        s.body.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest.len(), 32_000);
        let w = src.track("album/02 Two.wav").await.unwrap();
        let s = src.open(&w, 1000).await.unwrap();
        assert_eq!(s.start_sample, 0, "wav cannot start mid-file");
    }
}
