//! Client for Simple Player Web Server (the same API the Android app uses).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::library::{Source, Track};

pub fn client() -> Client {
    Client::builder()
        .connect_timeout(Duration::from_secs(6))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("SimplePlayer-Windows/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client")
}

/// "192.168.0.10:55555" -> "http://192.168.0.10:55555"; trailing slashes removed.
pub fn normalize(base: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    if b.is_empty() {
        return String::new();
    }
    if b.starts_with("http://") || b.starts_with("https://") {
        b.to_string()
    } else {
        format!("http://{b}")
    }
}

#[derive(Deserialize)]
struct LibraryResp {
    tracks: Vec<ServerTrack>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerTrack {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    artist: String,
    #[serde(default)]
    album: String,
    #[serde(default)]
    folder: String,
    #[serde(default)]
    ext: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    has_cover: bool,
    /// Seconds (sent by newer servers).
    #[serde(default)]
    duration: f64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ServerData {
    pub base: String,
    pub tracks: Vec<Track>,
    pub playlists: BTreeMap<String, Vec<String>>, // name -> server ids
    #[serde(default)]
    pub public_origin: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ConfigResp {
    #[serde(default)]
    public_origin: String,
}

fn cache_path() -> PathBuf {
    config::cache_dir().join("server-library.json")
}

pub fn load_cache(base: &str) -> Option<ServerData> {
    let data: ServerData = serde_json::from_slice(&fs::read(cache_path()).ok()?).ok()?;
    (data.base == base).then_some(data)
}

pub fn fetch(client: &Client, base: &str) -> Result<ServerData, String> {
    let get = |path: &str| -> Result<String, String> {
        let r = client
            .get(format!("{base}{path}"))
            .send()
            .map_err(|e| format!("連線失敗：{e}"))?;
        if !r.status().is_success() {
            return Err(format!("伺服器回應 HTTP {}", r.status()));
        }
        r.text().map_err(|e| e.to_string())
    };
    let lib: LibraryResp =
        serde_json::from_str(&get("/api/library")?).map_err(|e| format!("曲庫格式錯誤：{e}"))?;
    let playlists: BTreeMap<String, Vec<String>> =
        serde_json::from_str(&get("/api/playlists")?).unwrap_or_default();
    let cfg: ConfigResp = get("/api/config")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let tracks = lib
        .tracks
        .into_iter()
        .map(|t| Track {
            key: format!("server:{}", t.id),
            source: Source::Server,
            title: if t.title.trim().is_empty() {
                t.name.clone()
            } else {
                t.title
            },
            artist: t.artist,
            album: t.album,
            folder: t.folder,
            ext: t.ext.trim_start_matches('.').to_ascii_lowercase(),
            size: t.size,
            duration_ms: if t.duration.is_finite() && t.duration > 0.0 {
                (t.duration * 1000.0) as u64
            } else {
                0
            },
            has_cover: t.has_cover,
            path: None,
            server_id: Some(t.id),
            mtime: 0,
        })
        .collect();
    let data = ServerData {
        base: base.to_string(),
        tracks,
        playlists,
        public_origin: cfg.public_origin.trim_end_matches('/').to_string(),
    };
    if let Ok(json) = serde_json::to_vec(&data) {
        let _ = fs::create_dir_all(config::cache_dir());
        let _ = fs::write(cache_path(), json);
    }
    Ok(data)
}

/// Asks the server to rescan its music folder (POST /api/scan) and waits for it to finish,
/// so new or changed songs show up right away. Returns a short summary.
pub fn rescan(client: &Client, base: &str) -> Result<String, String> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct ScanResp {
        tracks: u64,
        added: u64,
        changed: u64,
        removed: u64,
        cached: bool,
    }
    let r = client
        .post(format!("{base}/api/scan"))
        // a big library can take a while on the first scan after many changes
        .timeout(Duration::from_secs(600))
        .send()
        .map_err(|e| format!("連線失敗：{e}"))?;
    if !r.status().is_success() {
        return Err(format!("伺服器掃描失敗（HTTP {}）", r.status()));
    }
    let s: ScanResp = r.json().unwrap_or_default();
    if s.cached {
        return Ok("伺服器剛掃描過".into());
    }
    Ok(if s.added + s.changed + s.removed == 0 {
        format!("伺服器掃描完成，沒有變更（{} 首）", s.tracks)
    } else {
        format!(
            "伺服器掃描完成：新增 {}、變更 {}、移除 {}",
            s.added, s.changed, s.removed
        )
    })
}

pub fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn stream_url(base: &str, id: &str) -> String {
    format!("{base}/stream/{}", enc(id))
}

pub fn cover_url(base: &str, id: &str) -> String {
    format!("{base}/api/track/{}/cover", enc(id))
}

/// Downloads (once) and returns the cached cover file of a server track.
pub fn cover_file(client: &Client, base: &str, id: &str) -> Option<PathBuf> {
    let stem = config::cache_dir().join("covers").join(format!(
        "s{:016x}",
        crate::library::hash(&format!("{base}|{id}"))
    ));
    if let Some(f) = crate::library::cached_image(&stem) {
        return Some(f);
    }
    let r = client.get(cover_url(base, id)).send().ok()?;
    if !r.status().is_success() {
        return None;
    }
    let bytes = r.bytes().ok()?;
    crate::library::save_image(&stem, &bytes)
}

#[derive(Deserialize)]
struct MetaResp {
    #[serde(default)]
    lyrics: Vec<LyricJson>,
}

#[derive(Deserialize)]
struct LyricJson {
    time: Option<f64>,
    #[serde(default)]
    text: String,
}

/// Embedded lyrics of a server track (`time` is null for unsynced lines).
pub fn lyrics(client: &Client, base: &str, id: &str) -> Vec<(Option<f64>, String)> {
    let url = format!("{base}/api/track/{}/metadata", enc(id));
    let Ok(resp) = client.get(url).send() else {
        return Vec::new();
    };
    let Ok(meta) = resp.json::<MetaResp>() else {
        return Vec::new();
    };
    meta.lyrics
        .into_iter()
        .filter(|l| l.time.is_some() || !l.text.trim().is_empty())
        .map(|l| (l.time.filter(|t| t.is_finite()), l.text))
        .collect()
}
