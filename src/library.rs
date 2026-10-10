//! Songs from both sources (local folders and Simple Player Web Server) share one
//! `Track` type, so lists, search and the play queue do not care where a song lives.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use lofty::prelude::*;
use lofty::probe::Probe;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config;

/// Symphonia reads most of these; the rest (Opus, APE, WavPack, DSD, WMA, Dolby, DTS…)
/// play through FFmpeg when it is available.
const AUDIO_EXTS: &[&str] = &[
    "mp3", "mp2", "flac", "m4a", "m4b", "aac", "alac", "ogg", "oga", "opus", "wav", "w64",
    "webm", "mka", "aiff", "aif", "aifc", "caf", "ape", "wv", "tta", "mpc", "dsf", "dff", "wma",
    "ac3", "eac3", "ec3", "dts", "thd", "mlp",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    Local,
    Server,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    /// Unique key: "local:<path>" or "server:<id>".
    pub key: String,
    pub source: Source,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub folder: String,
    pub ext: String,
    pub size: u64,
    pub duration_ms: u64,
    pub has_cover: bool,
    /// Local file path (local tracks).
    pub path: Option<PathBuf>,
    /// Server track id (server tracks).
    pub server_id: Option<String>,
    #[serde(default)]
    pub mtime: u64,
}

/* ---------------- local library ---------------- */

#[derive(Default, Serialize, Deserialize)]
struct LocalCache {
    tracks: Vec<Track>,
}

fn local_cache_path() -> PathBuf {
    config::cache_dir().join("local-library.json")
}

/// Previously scanned local tracks (instant start-up).
pub fn load_local_cache() -> Vec<Track> {
    fs::read(local_cache_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<LocalCache>(&b).ok())
        .map(|c| c.tracks)
        .unwrap_or_default()
}

/// Walks the folders, re-reading tags only for new or changed files.
pub fn scan_local(folders: &[PathBuf], progress: impl Fn(usize)) -> Vec<Track> {
    let old: HashMap<String, Track> = load_local_cache()
        .into_iter()
        .map(|t| (t.key.clone(), t))
        .collect();
    let mut out = Vec::new();
    for root in folders {
        for entry in WalkDir::new(root)
            .follow_links(true)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .unwrap_or_default();
            if !AUDIO_EXTS.contains(&ext.as_str()) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let size = meta.len();
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let key = format!("local:{}", path.display());
            if let Some(t) = old.get(&key) {
                if t.size == size && t.mtime == mtime {
                    let mut t = t.clone();
                    t.folder = folder_name(path, root);
                    out.push(t);
                    continue;
                }
            }
            out.push(read_local(path, root, &ext, size, mtime, key));
            if out.len() % 50 == 0 {
                progress(out.len());
            }
        }
    }
    out.sort_by(|a, b| a.folder.cmp(&b.folder).then(a.path.cmp(&b.path)));
    if let Ok(json) = serde_json::to_vec(&LocalCache {
        tracks: out.clone(),
    }) {
        let _ = fs::create_dir_all(config::cache_dir());
        let _ = fs::write(local_cache_path(), json);
    }
    out
}

/// Playlist name of a file: its folder relative to the library root
/// ("Anime / OST"); files directly in the root use the root folder's name.
fn folder_name(path: &Path, root: &Path) -> String {
    let rel = path
        .parent()
        .and_then(|p| p.strip_prefix(root).ok())
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" / ")
        })
        .unwrap_or_default();
    if !rel.is_empty() {
        return rel;
    }
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string())
}

fn read_local(path: &Path, root: &Path, ext: &str, size: u64, mtime: u64, key: String) -> Track {
    read_local_full(path, root, ext, size, mtime, key).0
}

/// Like `read_local`, plus (disc, track number) from the tags (0 when missing).
fn read_local_full(
    path: &Path,
    root: &Path,
    ext: &str,
    size: u64,
    mtime: u64,
    key: String,
) -> (Track, u32, u32) {
    let mut numbers = (0u32, 0u32);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let folder = folder_name(path, root);
    let mut t = Track {
        key,
        source: Source::Local,
        title: stem,
        artist: String::new(),
        album: String::new(),
        folder,
        ext: ext.to_string(),
        size,
        duration_ms: 0,
        has_cover: false,
        path: Some(path.to_path_buf()),
        server_id: None,
        mtime,
    };
    if let Ok(tagged) = Probe::open(path).and_then(|p| p.read()) {
        t.duration_ms = tagged.properties().duration().as_millis() as u64;
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            if let Some(v) = tag.title() {
                if !v.trim().is_empty() {
                    t.title = v.to_string();
                }
            }
            if let Some(v) = tag.artist() {
                t.artist = v.to_string();
            }
            if let Some(v) = tag.album() {
                t.album = v.to_string();
            }
            numbers = (tag.disk().unwrap_or(0), tag.track().unwrap_or(0));
        }
        t.has_cover = tagged.tags().iter().any(|tag| !tag.pictures().is_empty());
    }
    (t, numbers.0, numbers.1)
}

/// File extension (lower case) when it is an audio file we play.
pub fn audio_ext(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    AUDIO_EXTS.contains(&ext.as_str()).then_some(ext)
}

/// A single file outside the library (mini player). Returns (track, disc, track number).
pub fn track_from_path(path: &Path) -> Option<(Track, u32, u32)> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let ext = audio_ext(path)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = format!("local:{}", path.display());
    let root = path.parent().unwrap_or(path);
    Some(read_local_full(path, root, &ext, meta.len(), mtime, key))
}

/// "2 song" < "10 song": compares runs of digits by value, the rest case-insensitively.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let mut n1 = String::new();
                while let Some(c) = x.peek().copied().filter(|c| c.is_ascii_digit()) {
                    n1.push(c);
                    x.next();
                }
                let mut n2 = String::new();
                while let Some(d) = y.peek().copied().filter(|c| c.is_ascii_digit()) {
                    n2.push(d);
                    y.next();
                }
                let (t1, t2) = (n1.trim_start_matches('0'), n2.trim_start_matches('0'));
                let o = t1.len().cmp(&t2.len()).then(t1.cmp(t2));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(c), Some(d)) => {
                let o = c.to_lowercase().cmp(d.to_lowercase());
                if o != Ordering::Equal {
                    return o;
                }
                x.next();
                y.next();
            }
        }
    }
}

/// Writes the embedded cover of a local file to the cache and returns its path.
pub fn local_cover(t: &Track) -> Option<PathBuf> {
    let path = t.path.as_ref()?;
    let stem = config::cache_dir()
        .join("covers")
        .join(format!("{:016x}", hash(&t.key)));
    if let Some(f) = cached_image(&stem) {
        return Some(f);
    }
    let tagged = Probe::open(path).ok()?.read().ok()?;
    let pic = tagged
        .primary_tag()
        .and_then(|t| t.pictures().first())
        .or_else(|| tagged.tags().iter().find_map(|t| t.pictures().first()))?;
    save_image(&stem, pic.data())
}

const IMAGE_EXTS: [&str; 4] = ["png", "jpg", "webp", "gif"];

/// Finds an already cached image `<stem>.<ext>`.
pub fn cached_image(stem: &Path) -> Option<PathBuf> {
    IMAGE_EXTS
        .iter()
        .map(|e| stem.with_extension(e))
        .find(|f| f.is_file())
}

/// Detects the real image format from its first bytes.
pub fn image_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        Some("jpg")
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"GIF8") {
        Some("gif")
    } else {
        None
    }
}

/// Saves image bytes with an extension that matches their format, so the
/// UI image loader can decode it.
pub fn save_image(stem: &Path, bytes: &[u8]) -> Option<PathBuf> {
    let file = stem.with_extension(image_ext(bytes)?);
    fs::create_dir_all(file.parent()?).ok()?;
    fs::write(&file, bytes).ok()?;
    Some(file)
}

pub fn hash(s: &str) -> u64 {
    // FNV-1a: stable across runs, good enough for cache file names.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Case-insensitive search over the visible text of a track.
pub fn matches(t: &Track, q: &str) -> bool {
    if q.is_empty() {
        return true;
    }
    let hay = format!("{} {} {} {}", t.title, t.artist, t.album, t.folder).to_lowercase();
    q.split_whitespace().all(|w| hay.contains(w))
}

/// Lyrics of a local file: a sidecar .lrc file, else the embedded lyrics tag.
pub fn local_lyrics(t: &Track) -> Vec<(Option<f64>, String)> {
    let Some(path) = t.path.as_ref() else {
        return Vec::new();
    };
    if let Ok(text) = fs::read_to_string(path.with_extension("lrc")) {
        let lines = parse_lrc(&text);
        if !lines.is_empty() {
            return lines;
        }
    }
    let Ok(tagged) = Probe::open(path).and_then(|p| p.read()) else {
        return Vec::new();
    };
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return Vec::new();
    };
    let Some(text) = tag.get_string(&lofty::tag::ItemKey::Lyrics) else {
        return Vec::new();
    };
    parse_lrc(text)
}

/// Parses LRC ("[mm:ss.xx]text", several stamps per line allowed). Lines without a
/// time stamp are returned as unsynced text.
pub fn parse_lrc(text: &str) -> Vec<(Option<f64>, String)> {
    let mut timed: Vec<(f64, String)> = Vec::new();
    let mut plain: Vec<String> = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            let inner = &stripped[..end];
            match parse_stamp(inner) {
                Some(t) => stamps.push(t),
                None => {
                    // [ar:...] style metadata: skip the whole line
                    if inner.contains(':') && stamps.is_empty() {
                        rest = "";
                        break;
                    }
                }
            }
            rest = stripped[end + 1..].trim_start();
        }
        if stamps.is_empty() {
            if !rest.is_empty() {
                plain.push(rest.to_string());
            }
        } else {
            for t in stamps {
                timed.push((t, rest.to_string()));
            }
        }
    }
    if !timed.is_empty() {
        timed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        return timed.into_iter().map(|(t, s)| (Some(t), s)).collect();
    }
    plain.into_iter().map(|s| (None, s)).collect()
}

fn parse_stamp(s: &str) -> Option<f64> {
    let (m, sec) = s.split_once(':')?;
    let m: f64 = m.trim().parse().ok()?;
    let sec: f64 = sec.trim().replace(':', ".").parse().ok()?;
    Some(m * 60.0 + sec)
}
