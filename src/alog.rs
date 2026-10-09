//! Small diagnostic log for the audio engine.
//! Debug builds write `audio.log` next to the exe (e.g. target\debug\audio.log);
//! release builds write %LOCALAPPDATA%\SimplePlayer\audio.log. Truncated at start-up.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static LOG: OnceLock<Mutex<Option<(File, Instant)>>> = OnceLock::new();

fn path() -> PathBuf {
    if cfg!(debug_assertions) {
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        {
            return dir.join("audio.log");
        }
    }
    crate::config::cache_dir().join("audio.log")
}

fn cell() -> &'static Mutex<Option<(File, Instant)>> {
    LOG.get_or_init(|| {
        let p = path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(p)
            .ok();
        Mutex::new(f.map(|f| (f, Instant::now())))
    })
}

pub fn write(msg: &str) {
    eprintln!("{msg}");
    if let Ok(mut g) = cell().lock() {
        if let Some((f, t0)) = g.as_mut() {
            let _ = writeln!(f, "[{:9.3}] {msg}", t0.elapsed().as_secs_f64());
            let _ = f.flush();
        }
    }
}

#[macro_export]
macro_rules! alog {
    ($($t:tt)*) => { $crate::alog::write(&format!($($t)*)) };
}
