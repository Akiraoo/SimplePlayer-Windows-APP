use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Simple Player Web Server address (mobile API port), e.g. http://192.168.0.10:55555
    pub server: String,
    pub local_folders: Vec<PathBuf>,
    pub discord: bool,
    pub discord_client_id: String,
    pub volume: f32,
    pub last_view: String,
    pub light: bool,
    /// Public https address of the server (e.g. a Cloudflare Tunnel domain). Discord can
    /// only show covers from public https URLs; empty = use the server's publicOrigin.
    pub public_url: String,
    /// The player panel is in its own window.
    pub detached: bool,
    pub pin_main: bool,
    pub pin_player: bool,
    /// Theme (accent) colour, "#rrggbb".
    pub accent: String,
    pub shuffle: bool,
    /// 0 off, 1 repeat all, 2 repeat one
    pub repeat: u8,
    /// The ✕ button hides the window to the tray (playback continues).
    pub close_to_tray: bool,
    /// Key of the song highlighted last (restored on start).
    pub last_selected: String,
    /// Detached player window: x, y, width, height (physical pixels).
    pub pw_geom: Option<[i32; 4]>,
    /// Audio output device name; empty = system default.
    pub output_device: String,
    /// WASAPI exclusive mode (bit-perfect, the song's own format; other apps go silent).
    pub exclusive: bool,
    /// Settings → 緩衝大小: 0 標準, 1 大, 2 超大.
    pub buffer_level: u8,
    /// Interface language: "zh-TW", "en", or empty = follow Windows on first start.
    pub lang: String,
    /// Look for a newer release on GitHub at start-up.
    pub auto_update: bool,
    /// ReplayGain: 0 off, 1 track, 2 album.
    pub replaygain: u8,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            server: String::new(),
            local_folders: Vec::new(),
            discord: true,
            discord_client_id: "1552923544420225046".into(),
            volume: 0.8,
            last_view: String::new(),
            light: false,
            public_url: String::new(),
            detached: false,
            pin_main: false,
            pin_player: false,
            accent: "#f2a65a".into(),
            shuffle: false,
            repeat: 0,
            close_to_tray: true,
            last_selected: String::new(),
            pw_geom: None,
            output_device: String::new(),
            exclusive: false,
            buffer_level: 1,
            lang: String::new(),
            auto_update: true,
            replaygain: 0,
        }
    }
}

fn app_dir(base: Option<PathBuf>) -> PathBuf {
    base.unwrap_or_else(std::env::temp_dir).join("SimplePlayer")
}

pub fn config_path() -> PathBuf {
    app_dir(dirs::config_dir()).join("config.json")
}

pub fn cache_dir() -> PathBuf {
    app_dir(dirs::cache_dir())
}

pub fn load() -> Config {
    fs::read(config_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(cfg: &Config) {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec_pretty(cfg) {
        let _ = fs::write(path, json);
    }
}
