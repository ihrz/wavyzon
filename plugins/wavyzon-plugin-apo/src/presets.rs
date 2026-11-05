//! The presets directory: every `.txt` configuration and every impulse
//! response under it, read and compiled at startup and on reload (on a
//! blocking thread, never on the audio path).
//!
//! Paths are normalised with `/` and stay inside the directory; symbolic
//! links are skipped so a preset can't reach files outside it. Lookups fall
//! back to a case-insensitive match because APO configurations come from
//! Windows.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::ir::{self, Impulse};
use crate::parse::MAX_TEXT_BYTES;
use crate::program::{self, Program, Resolver};

/// Deepest directory level scanned.
pub const MAX_DEPTH: usize = 6;
/// Most directory entries looked at.
pub const MAX_ENTRIES: usize = 10_000;
/// Most configuration files loaded.
pub const MAX_TEXT_FILES: usize = 1024;
/// Most impulse responses loaded.
pub const MAX_IMPULSES: usize = 64;

/// A configuration file usable as `{"preset": name}`.
#[derive(Debug)]
pub struct Preset {
    /// Path under the directory (`headphones/hd600.txt`).
    pub path: String,
    pub program: Result<Arc<Program>, String>,
}

#[derive(Debug, Default)]
struct Files {
    texts: HashMap<String, Arc<str>>,
    irs: HashMap<String, Result<Arc<Impulse>, String>>,
    /// Lowercased path → path, for both maps.
    lower: HashMap<String, String>,
}

impl Files {
    fn key<'a>(&'a self, path: &'a str) -> &'a str {
        if self.texts.contains_key(path) || self.irs.contains_key(path) {
            return path;
        }
        self.lower.get(&path.to_lowercase()).map_or(path, String::as_str)
    }
}

impl Resolver for Files {
    fn text(&self, path: &str) -> Option<Arc<str>> {
        self.texts.get(self.key(path)).cloned()
    }

    fn impulse(&self, path: &str) -> Option<Result<Arc<Impulse>, String>> {
        self.irs.get(self.key(path)).cloned()
    }
}

/// Everything loaded from one directory.
#[derive(Debug, Default)]
pub struct Store {
    pub dir: PathBuf,
    files: Files,
    /// By name: the path without `.txt`.
    pub presets: BTreeMap<String, Preset>,
    /// Problems with the directory itself (limits, unreadable files).
    pub warnings: Vec<String>,
}

impl Resolver for Store {
    fn text(&self, path: &str) -> Option<Arc<str>> {
        self.files.text(path)
    }

    fn impulse(&self, path: &str) -> Option<Result<Arc<Impulse>, String>> {
        self.files.impulse(path)
    }
}

fn walk(
    dir: &Path,
    rel: &str,
    depth: usize,
    seen: &mut usize,
    out: &mut Vec<(String, PathBuf)>,
    warnings: &mut Vec<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        warnings.push(format!("cannot read directory {}", dir.display()));
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        *seen += 1;
        if *seen > MAX_ENTRIES {
            if *seen == MAX_ENTRIES + 1 {
                warnings.push(format!("more than {MAX_ENTRIES} files; the rest is ignored"));
            }
            return;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let path = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            warnings.push(format!("{path}: symbolic links are not followed"));
        } else if ft.is_dir() {
            if depth < MAX_DEPTH {
                walk(&e.path(), &path, depth + 1, seen, out, warnings);
            }
        } else if ft.is_file() {
            out.push((path, e.path()));
        }
    }
}

fn extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn read_text(path: &Path) -> Result<String, String> {
    let len = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if len > MAX_TEXT_BYTES as u64 {
        return Err(format!("larger than {} KiB", MAX_TEXT_BYTES / 1024));
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    // APO files may be UTF-8 with a BOM, or ANSI: keep what decodes.
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.trim_start_matches('\u{feff}').to_string())
}

impl Store {
    /// Read `dir`. Never fails: problems are recorded on the presets or in
    /// [`warnings`](Self::warnings).
    pub fn load(dir: &Path) -> Self {
        let mut warnings = Vec::new();
        let mut found = Vec::new();
        let mut seen = 0;
        walk(dir, "", 0, &mut seen, &mut found, &mut warnings);
        let mut files = Files::default();
        let (mut texts, mut irs) = (0, 0);
        for (rel, path) in found {
            let ext = extension(&rel);
            if ext == "txt" {
                texts += 1;
                if texts > MAX_TEXT_FILES {
                    if texts == MAX_TEXT_FILES + 1 {
                        warnings.push(format!(
                            "more than {MAX_TEXT_FILES} configuration files; the rest is ignored"
                        ));
                    }
                    continue;
                }
                match read_text(&path) {
                    Ok(t) => {
                        files.lower.insert(rel.to_lowercase(), rel.clone());
                        files.texts.insert(rel, Arc::from(t));
                    }
                    Err(e) => warnings.push(format!("{rel}: {e}")),
                }
            } else if ir::EXTENSIONS.contains(&ext.as_str()) {
                irs += 1;
                if irs > MAX_IMPULSES {
                    if irs == MAX_IMPULSES + 1 {
                        warnings.push(format!(
                            "more than {MAX_IMPULSES} impulse responses; the rest is ignored"
                        ));
                    }
                    continue;
                }
                files.lower.insert(rel.to_lowercase(), rel.clone());
                files.irs.insert(rel, ir::load(&path).map(Arc::new));
            }
        }
        let mut presets = BTreeMap::new();
        for (path, text) in &files.texts {
            let name = path
                .strip_suffix(".txt")
                .or_else(|| path.strip_suffix(".TXT"))
                .unwrap_or(path);
            let program = program::compile(text, path, &files).map(Arc::new);
            presets.insert(
                name.to_string(),
                Preset {
                    path: path.clone(),
                    program,
                },
            );
        }
        Self {
            dir: dir.to_path_buf(),
            files,
            presets,
            warnings,
        }
    }

    /// A preset by name (`hd600`, `hd600.txt`, `headphones/HD600`).
    pub fn preset(&self, name: &str) -> Option<&Preset> {
        let name = name.replace('\\', "/");
        let name = name.strip_suffix(".txt").unwrap_or(&name);
        self.presets.get(name).or_else(|| {
            let lower = name.to_lowercase();
            self.presets
                .iter()
                .find(|(k, _)| k.to_lowercase() == lower)
                .map(|(_, p)| p)
        })
    }

    /// Impulse responses with their load result, sorted by path.
    pub fn impulses(&self) -> Vec<(&str, &Result<Arc<Impulse>, String>)> {
        let mut v: Vec<_> = self.files.irs.iter().map(|(k, v)| (k.as_str(), v)).collect();
        v.sort_by_key(|(k, _)| *k);
        v
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ir::tests::write_wav;

    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wavyzon-apo-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn loads_presets_includes_and_impulses() {
        let d = temp_dir("load");
        std::fs::create_dir_all(d.join("Headphones/common")).unwrap();
        std::fs::create_dir_all(d.join("ir")).unwrap();
        std::fs::write(
            d.join("Headphones/HD600.txt"),
            "\u{feff}Preamp: -3 dB\nInclude: common\\tilt.txt\nConvolution: ../ir/Room.wav\n",
        )
        .unwrap();
        std::fs::write(
            d.join("Headphones/common/tilt.txt"),
            "Filter: ON HS Fc 3000 Hz Gain 2 dB",
        )
        .unwrap();
        std::fs::write(d.join("broken.txt"), "Include: missing.txt").unwrap();
        let mut ir = vec![0.0; 960];
        ir[0] = 1.0;
        write_wav(&d.join("ir/Room.wav"), 48_000, &[ir]);

        let s = Store::load(&d);
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert_eq!(s.presets.len(), 3);
        let p = s.preset("headphones/hd600").expect("case-insensitive lookup");
        let prog = p.program.as_ref().unwrap();
        assert_eq!(prog.summary(), vec!["preamp", "filter", "convolution"]);
        assert!(s.preset("Headphones/HD600.txt").is_some());
        assert!(
            s.preset("broken")
                .unwrap()
                .program
                .as_ref()
                .unwrap_err()
                .contains("missing.txt")
        );
        assert!(s.preset("nope").is_none());
        assert_eq!(s.impulses().len(), 1);
        // Inline text resolves from the root.
        assert!(program::compile("Convolution: ir/room.wav", "", &s).is_ok());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn missing_directory_is_an_empty_store() {
        let s = Store::load(Path::new("/nonexistent/wavyzon-apo"));
        assert!(s.presets.is_empty());
        assert_eq!(s.warnings.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let d = temp_dir("symlink");
        let outside = temp_dir("outside");
        std::fs::write(outside.join("secret.txt"), "Preamp: -1 dB").unwrap();
        std::os::unix::fs::symlink(&outside, d.join("link")).unwrap();
        let s = Store::load(&d);
        assert!(s.presets.is_empty());
        assert!(s.warnings[0].contains("symbolic links"));
        std::fs::remove_dir_all(d).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }
}
