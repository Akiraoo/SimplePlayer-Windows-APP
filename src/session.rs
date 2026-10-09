//! Remembers the play queue and position between runs (%LOCALAPPDATA%\SimplePlayer\session.json).

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config;

#[derive(Default, Serialize, Deserialize)]
pub struct Session {
    /// Track keys in play order.
    pub queue: Vec<String>,
    /// Order before shuffling (empty when not shuffled).
    #[serde(default)]
    pub unshuffled: Vec<String>,
    pub pos: usize,
    /// Seconds into the current song.
    pub position: f64,
    /// The user's queue, and which of its songs is playing (if the current one came from it).
    #[serde(default)]
    pub upnext: Vec<String>,
    #[serde(default)]
    pub upnext_cur: Option<usize>,
}

fn path() -> PathBuf {
    config::cache_dir().join("session.json")
}

pub fn load() -> Option<Session> {
    serde_json::from_slice(&fs::read(path()).ok()?).ok()
}

pub fn save(s: &Session) {
    if let Ok(json) = serde_json::to_vec(s) {
        let _ = fs::create_dir_all(config::cache_dir());
        let tmp = path().with_extension("json.tmp");
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, path());
        }
    }
}
