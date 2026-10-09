// Simple Player for Windows: local library + Simple Player Web Server, Discord Rich Presence.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod discord;
mod httpsrc;
mod library;
mod media;
mod player;
mod server;
mod session;
mod single;
mod tray;
#[cfg(windows)]
mod wasapi_out;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use config::Config;
use library::{Source, Track};
use player::{Player, PlayerEvent};
use server::ServerData;

slint::include_modules!();

#[derive(Clone, PartialEq, Debug)]
enum View {
    Queue,
    Local,
    LocalFolder(String),
    Server,
    Playlist(String),
    Setup,
}

impl View {
    fn key(&self) -> String {
        match self {
            View::Queue => "queue".into(),
            View::Local => "local".into(),
            View::LocalFolder(n) => format!("lf:{n}"),
            View::Server => "server".into(),
            View::Playlist(n) => format!("pl:{n}"),
            View::Setup => String::new(),
        }
    }
}

/// Results of the background loading before a song starts.
enum Loaded {
    Cover(Option<PathBuf>),
    Lyrics(Vec<(Option<f64>, String)>),
}

struct App {
    ui: slint::Weak<MainWindow>,
    /// The detachable player window.
    pw: slint::Weak<PlayerWindow>,
    pw_shown: bool,
    /// Bumped on every song switch; stale background loads are dropped.
    load_gen: u64,
    switching: bool,
    /// Play order before shuffling (restored when shuffle is turned off).
    unshuffled: Vec<Track>,
    /// The user's queue ("up next"): a temporary playlist that plays before the rest of the
    /// current list. A queued song is removed once it has been played through (or skipped).
    upnext: Vec<Track>,
    /// Index in `upnext` of the song playing now, if it came from the queue.
    upnext_cur: Option<usize>,
    /// Queue view: row -> index into `queue`.
    visible_qidx: Vec<usize>,
    rng: u64,
    /// Restored session: open the song paused at this position.
    resume_at: Option<f64>,
    /// After a restore, stay quiet (no Discord) until the user presses play.
    idle_restore: bool,
    last_session_save: Instant,
    tray: Option<tray::Tray>,
    /// Restore the last opened list / highlighted song once they exist (libraries load late).
    pending_view: Option<String>,
    pending_select: Option<String>,
    /// Output device names as last listed (settings → 音訊輸出).
    output_names: Vec<String>,
    cfg: Config,
    client: reqwest::blocking::Client,
    local: Vec<Track>,
    server: Option<ServerData>,
    source_views: Vec<Option<View>>,
    view: View,
    query: String,
    sort: Option<(i32, bool)>,
    visible: Vec<Track>,
    queue: Vec<Track>,
    queue_pos: usize,
    current: Option<Track>,
    cover_file: Option<PathBuf>,
    player: Player,
    discord: discord::Discord,
    media: Option<media::Media>,
    last_playing: bool,
    last_presence: Instant,
    /// Set by user actions; the presence is sent ~0.3 s later (lets seeks settle, merges bursts).
    presence_dirty: Option<Instant>,
    opened_at: Instant,
    scanning: bool,
    status: String,
    lyrics: Vec<(Option<f64>, String)>,
    active_lyric: i32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` with the app state (UI thread only). Never call it re-entrantly.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| match a.try_borrow_mut() {
        Ok(mut a) => a.as_mut().map(f),
        Err(_) => None,
    })
}

/// Runs `f` on the UI thread with the app state.
fn post(f: impl FnOnce(&mut App) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        with_app(f);
    });
}

fn fmt_time(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        return "0:00".into();
    }
    let s = secs as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

impl App {
    fn ui(&self) -> Option<MainWindow> {
        self.ui.upgrade()
    }

    /* ---------- detached player ---------- */

    fn set_detached(&mut self, on: bool) {
        let was_shown = self.pw_shown;
        self.cfg.detached = on;
        config::save(&self.cfg);
        let (Some(ui), Some(pw)) = (self.ui(), self.pw.upgrade()) else {
            return;
        };
        ui.set_detached(on);
        if on {
            self.mirror_player(true);
            let main = ui.window();
            let scale = main.scale_factor();
            // Same place and size as last time; the first time it pops out where the panel was.
            let (pos, w, h) = match self.cfg.pw_geom {
                Some([x, y, w, h]) if w > 0 && h > 0 => {
                    (slint::PhysicalPosition::new(x, y), w as u32, h as u32)
                }
                _ => {
                    let w = (360.0 * scale) as u32;
                    let h = ((main.size().height as f32) - 80.0 * scale)
                        .clamp(520.0 * scale, 760.0 * scale) as u32;
                    let p = main.position();
                    let pos = slint::PhysicalPosition::new(
                        p.x + main.size().width as i32 - w as i32 - (20.0 * scale) as i32,
                        p.y + (52.0 * scale) as i32,
                    );
                    (pos, w, h)
                }
            };
            pw.window().set_size(slint::PhysicalSize::new(w, h));
            pw.window().set_position(pos);
            let _ = pw.show();
            self.pw_shown = true;
            let weak = pw.as_weak();
            slint::Timer::single_shot(Duration::from_millis(60), move || {
                if let Some(pw) = weak.upgrade() {
                    if let Some(hwnd) = window_hwnd(pw.window()) {
                        window_chrome(pw.window(), hwnd);
                    }
                }
            });
        } else {
            if was_shown {
                self.remember_pw_geometry();
                config::save(&self.cfg);
            }
            // Re-attached: activate the main window *before* hiding the player. If the player
            // were hidden while still active, Windows would hand the focus to whatever window
            // lies behind it (the main window flashes, then drops back).
            bring_to_front(ui.window());
            let _ = pw.hide();
            self.pw_shown = false;
            let weak = ui.as_weak();
            slint::Timer::single_shot(Duration::from_millis(80), move || {
                if let Some(ui) = weak.upgrade() {
                    bring_to_front(ui.window());
                }
            });
        }
    }

    /// Stores where the detached player is and how big (kept in the config across runs).
    fn remember_pw_geometry(&mut self) {
        if !self.pw_shown {
            return;
        }
        if let Some(pw) = self.pw.upgrade() {
            let p = pw.window().position();
            let sz = pw.window().size();
            if sz.width > 0 && sz.height > 0 && !pw.window().is_minimized() {
                self.cfg.pw_geom = Some([p.x, p.y, sz.width as i32, sz.height as i32]);
            }
        }
    }

    /// Copies the now-playing state to the detached player window.
    /// `full` also copies the cover image and the lyrics model.
    fn mirror_player(&self, full: bool) {
        if !self.cfg.detached {
            return;
        }
        let (Some(ui), Some(pw)) = (self.ui(), self.pw.upgrade()) else {
            return;
        };
        if full {
            pw.set_cover_image(ui.get_cover_image());
            pw.set_lyrics(ui.get_lyrics());
        }
        macro_rules! copy {
            ($get:ident, $set:ident) => {
                let v = ui.$get();
                if pw.$get() != v {
                    pw.$set(v);
                }
            };
        }
        copy!(get_has_cover, set_has_cover);
        copy!(get_now_title, set_now_title);
        copy!(get_now_artist, set_now_artist);
        copy!(get_now_album, set_now_album);
        copy!(get_now_detail, set_now_detail);
        copy!(get_active_lyric, set_active_lyric);
        copy!(get_lyrics_state, set_lyrics_state);
        copy!(get_playing, set_playing);
        copy!(get_progress, set_progress);
        copy!(get_position_text, set_position_text);
        copy!(get_duration_text, set_duration_text);
    }

    /* ---------- sources & list ---------- */

    fn rebuild_sources(&mut self) {
        let mut items: Vec<SourceItem> = Vec::new();
        let mut views: Vec<Option<View>> = Vec::new();
        let header = |items: &mut Vec<SourceItem>, views: &mut Vec<Option<View>>, label: &str| {
            items.push(SourceItem {
                label: label.into(),
                count: SharedString::new(),
                icon: SharedString::new(),
                header: true,
            });
            views.push(None);
        };

        {
            header(&mut items, &mut views, "正在播放");
            items.push(SourceItem {
                label: "播放佇列".into(),
                count: self.upnext.len().to_string().into(),
                icon: "list".into(),
                header: false,
            });
            views.push(Some(View::Queue));
        }
        if !self.cfg.local_folders.is_empty() || !self.local.is_empty() {
            header(&mut items, &mut views, "本地音樂");
            items.push(SourceItem {
                label: "全部本地歌曲".into(),
                count: self.local.len().to_string().into(),
                icon: "disk".into(),
                header: false,
            });
            views.push(Some(View::Local));
            let mut folders: std::collections::BTreeMap<&str, usize> = Default::default();
            for t in &self.local {
                *folders.entry(t.folder.as_str()).or_default() += 1;
            }
            if folders.len() > 1 {
                for (name, n) in folders {
                    items.push(SourceItem {
                        label: name.into(),
                        count: n.to_string().into(),
                        icon: "folder".into(),
                        header: false,
                    });
                    views.push(Some(View::LocalFolder(name.to_string())));
                }
            }
        }
        if let Some(s) = &self.server {
            header(&mut items, &mut views, "SIMPLE PLAYER 伺服器");
            items.push(SourceItem {
                label: "全部歌曲".into(),
                count: s.tracks.len().to_string().into(),
                icon: "cloud".into(),
                header: false,
            });
            views.push(Some(View::Server));
            for (name, ids) in &s.playlists {
                items.push(SourceItem {
                    label: name.into(),
                    count: ids.len().to_string().into(),
                    icon: "playlist".into(),
                    header: false,
                });
                views.push(Some(View::Playlist(name.clone())));
            }
        }
        if views.iter().all(|v| v.is_none()) {
            header(&mut items, &mut views, "開始使用");
            items.push(SourceItem {
                label: "開啟設定…".into(),
                count: SharedString::new(),
                icon: "settings".into(),
                header: false,
            });
            views.push(Some(View::Setup));
        }

        // The list that was open last time, as soon as it exists (server lists load later).
        if let Some(want) = self.pending_view.clone() {
            if let Some(v) = views.iter().flatten().find(|v| v.key() == want) {
                self.view = v.clone();
                self.pending_view = None;
            }
        }
        // Keep the current view if it still exists, otherwise pick the first one.
        if !views.iter().any(|v| v.as_ref() == Some(&self.view)) {
            let wanted = self.cfg.last_view.clone();
            self.view = views
                .iter()
                .flatten()
                .find(|v| v.key() == wanted)
                .or_else(|| {
                    views
                        .iter()
                        .flatten()
                        .find(|v| **v != View::Setup && **v != View::Queue)
                })
                .or_else(|| views.iter().flatten().next())
                .cloned()
                .unwrap_or(View::Setup);
        }
        self.source_views = views;
        let empty = self.local.is_empty()
            && self
                .server
                .as_ref()
                .map(|s| s.tracks.is_empty())
                .unwrap_or(true);
        if let Some(ui) = self.ui() {
            ui.set_empty_library(empty);
        }
        if let Some(ui) = self.ui() {
            let sel = self
                .source_views
                .iter()
                .position(|v| v.as_ref() == Some(&self.view))
                .map(|i| i as i32);
            ui.set_sources(ModelRc::new(VecModel::from(items)));
            ui.set_selected_source(sel.unwrap_or(-1));
        }
        self.refresh_list();
    }

    fn select_source(&mut self, index: usize) {
        let Some(Some(view)) = self.source_views.get(index).cloned() else {
            return;
        };
        if view == View::Setup {
            if let Some(ui) = self.ui() {
                ui.set_show_settings(true);
            }
            return;
        }
        self.view = view;
        self.pending_view = None;
        self.pending_select = None;
        self.cfg.last_view = self.view.key();
        config::save(&self.cfg);
        if let Some(ui) = self.ui() {
            ui.set_selected_source(index as i32);
            ui.set_selected_row(-1);
        }
        self.refresh_list();
    }

    fn tracks_for_view(&self) -> Vec<Track> {
        match &self.view {
            View::Queue => self.upnext.clone(),
            View::Local => self.local.clone(),
            View::LocalFolder(f) => self
                .local
                .iter()
                .filter(|t| &t.folder == f)
                .cloned()
                .collect(),
            View::Server => self
                .server
                .as_ref()
                .map(|s| s.tracks.clone())
                .unwrap_or_default(),
            View::Playlist(name) => {
                let Some(s) = &self.server else {
                    return Vec::new();
                };
                let by_id: HashMap<&str, &Track> = s
                    .tracks
                    .iter()
                    .filter_map(|t| t.server_id.as_deref().map(|id| (id, t)))
                    .collect();
                s.playlists
                    .get(name)
                    .map(|ids| {
                        ids.iter()
                            .filter_map(|id| by_id.get(id.as_str()).map(|t| (*t).clone()))
                            .collect()
                    })
                    .unwrap_or_default()
            }
            View::Setup => Vec::new(),
        }
    }

    fn refresh_list(&mut self) {
        let q = self.query.trim().to_lowercase();
        let queue_view = self.view == View::Queue;
        self.visible_qidx.clear();
        let mut rows: Vec<Track> = Vec::new();
        for (i, t) in self.tracks_for_view().into_iter().enumerate() {
            if library::matches(&t, &q) {
                if queue_view {
                    self.visible_qidx.push(i);
                }
                rows.push(t);
            }
        }
        if let (Some((col, asc)), false) = (self.sort, queue_view) {
            let key = |t: &Track| -> String {
                match col {
                    1 => t.title.to_lowercase(),
                    2 => format!("{}\u{1}{}", t.artist.to_lowercase(), t.album.to_lowercase()),
                    3 => format!("{}\u{1}{}", t.album.to_lowercase(), t.title.to_lowercase()),
                    4 => format!("{:012}", t.duration_ms),
                    5 => t.ext.clone(),
                    6 => format!("{:?}", t.source),
                    _ => String::new(),
                }
            };
            if col > 0 {
                rows.sort_by_cached_key(key);
                if !asc {
                    rows.reverse();
                }
            }
        }
        self.visible = rows;
        let title = match &self.view {
            View::Local => "全部本地歌曲".to_string(),
            View::Server => "全部歌曲".to_string(),
            View::Playlist(n) | View::LocalFolder(n) => n.clone(),
            View::Queue => "播放佇列".to_string(),
            View::Setup => "Simple Player".to_string(),
        };
        if let Some(ui) = self.ui() {
            ui.set_list_title(title.into());
            ui.set_queue_view(queue_view);
            ui.set_list_info(format!("{} 首", self.visible.len()).into());
            ui.set_sort_column(self.sort.map(|s| s.0).unwrap_or(-1));
            ui.set_sort_asc(self.sort.map(|s| s.1).unwrap_or(true));
        }
        self.push_rows();
        // highlight (and scroll to) the song that was selected last time
        if self.pending_view.is_none() {
            if let Some(key) = self.pending_select.clone() {
                if let Some(i) = self.visible.iter().position(|t| t.key == key) {
                    self.pending_select = None;
                    if let Some(ui) = self.ui() {
                        ui.set_selected_row(i as i32);
                        let weak = ui.as_weak();
                        slint::Timer::single_shot(Duration::from_millis(150), move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.invoke_scroll_to_row(i as i32);
                            }
                        });
                    }
                }
            }
        }
    }

    fn push_rows(&self) {
        let cur = self.current.as_ref().map(|t| t.key.as_str());
        let rows: Vec<TrackRow> = self
            .visible
            .iter()
            .enumerate()
            .map(|(i, t)| TrackRow {
                num: (i + 1).to_string().into(),
                title: t.title.as_str().into(),
                artist: t.artist.as_str().into(),
                album: t.album.as_str().into(),
                format: t.ext.to_uppercase().into(),
                duration: if t.duration_ms > 0 {
                    fmt_time(t.duration_ms as f64 / 1000.0)
                } else {
                    String::new()
                }
                .into(),
                origin: match t.source {
                    Source::Local => "disk",
                    Source::Server => "cloud",
                }
                .into(),
                playing: self.row_is_current(i, t, cur),
            })
            .collect();
        if let Some(ui) = self.ui() {
            ui.set_tracks(ModelRc::new(VecModel::from(rows)));
        }
    }

    /// Updates only the "playing" marker without rebuilding the whole list.
    fn mark_playing(&self) {
        let Some(ui) = self.ui() else { return };
        let model = ui.get_tracks();
        let cur = self.current.as_ref().map(|t| t.key.as_str());
        for (i, t) in self.visible.iter().enumerate() {
            let playing = self.row_is_current(i, t, cur);
            if let Some(mut row) = model.row_data(i) {
                if row.playing != playing {
                    row.playing = playing;
                    model.set_row_data(i, row);
                }
            }
        }
    }

    /// In the queue view only the actual queue position is "playing" (a song can be queued twice).
    fn row_is_current(&self, row: usize, t: &Track, cur: Option<&str>) -> bool {
        if self.view == View::Queue {
            self.current.is_some()
                && self.upnext_cur.is_some()
                && self.visible_qidx.get(row) == self.upnext_cur.as_ref()
        } else {
            Some(t.key.as_str()) == cur
        }
    }

    fn sort_by(&mut self, col: i32) {
        self.sort = match self.sort {
            Some((c, true)) if c == col => Some((col, false)),
            Some((c, false)) if c == col => None,
            _ => Some((col, true)),
        };
        if col == 0 {
            self.sort = None;
        }
        self.refresh_list();
    }

    /* ---------- playback ---------- */

    fn play_row(&mut self, index: usize) {
        if index >= self.visible.len() {
            return;
        }
        self.pending_select = None;
        self.cfg.last_view = self.view.key();
        self.cfg.last_selected = self.visible[index].key.clone();
        self.idle_restore = false;
        if self.view == View::Queue {
            // play that queued song; the queue itself keeps its order
            if let Some(&qi) = self.visible_qidx.get(index) {
                self.play_upnext(qi);
            }
            return;
        }
        // Any other list becomes the play order; the user's queue is left alone.
        self.unshuffled = self.visible.clone();
        self.queue = self.visible.clone();
        self.queue_pos = index;
        if self.cfg.shuffle {
            self.shuffle_rest();
        }
        self.play_current();
    }

    fn play_upnext(&mut self, qi: usize) {
        let Some(t) = self.upnext.get(qi).cloned() else {
            return;
        };
        self.idle_restore = false;
        self.upnext_cur = Some(qi);
        self.start(t);
        self.queue_changed();
    }

    /* ---------- queue, shuffle, repeat ---------- */

    fn rand(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545f4914f6cdd1d)
    }

    /// Puts the current song first and shuffles everything after it.
    fn shuffle_rest(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let cur = self.queue.remove(self.queue_pos.min(self.queue.len() - 1));
        for i in (1..self.queue.len()).rev() {
            let j = (self.rand() % (i as u64 + 1)) as usize;
            self.queue.swap(i, j);
        }
        self.queue.insert(0, cur);
        self.queue_pos = 0;
    }

    fn toggle_shuffle(&mut self) {
        self.cfg.shuffle = !self.cfg.shuffle;
        config::save(&self.cfg);
        if self.cfg.shuffle {
            self.unshuffled = self.queue.clone();
            self.shuffle_rest();
        } else if !self.unshuffled.is_empty() {
            let key = self.queue.get(self.queue_pos).map(|t| t.key.clone());
            self.queue = std::mem::take(&mut self.unshuffled);
            self.queue_pos = key
                .and_then(|k| self.queue.iter().position(|t| t.key == k))
                .unwrap_or(0);
        }
        self.push_modes();
        self.queue_changed();
    }

    fn cycle_repeat(&mut self) {
        self.cfg.repeat = (self.cfg.repeat + 1) % 3;
        config::save(&self.cfg);
        self.push_modes();
    }

    fn push_modes(&self) {
        if let Some(ui) = self.ui() {
            ui.set_shuffle(self.cfg.shuffle);
            ui.set_repeat_mode(self.cfg.repeat as i32);
            ui.set_close_to_tray(self.cfg.close_to_tray);
        }
        if let Some(pw) = self.pw.upgrade() {
            pw.set_shuffle(self.cfg.shuffle);
            pw.set_repeat_mode(self.cfg.repeat as i32);
        }
    }

    /// Sidebar count and the queue view after the queue changed.
    fn queue_changed(&mut self) {
        if self.view == View::Queue {
            self.refresh_list();
        }
        if let (Some(ui), Some(i)) = (
            self.ui(),
            self.source_views
                .iter()
                .position(|v| v == &Some(View::Queue)),
        ) {
            let model = ui.get_sources();
            if let Some(mut row) = model.row_data(i) {
                row.count = self.upnext.len().to_string().into();
                model.set_row_data(i, row);
            }
        }
    }

    fn queue_add(&mut self, row: usize) {
        let Some(t) = self.visible.get(row).cloned() else {
            return;
        };
        self.flash(format!("已加入佇列：{}", t.title));
        self.upnext.push(t);
        self.queue_changed();
    }

    fn queue_remove(&mut self, row: usize) {
        let Some(&qi) = self.visible_qidx.get(row) else {
            return;
        };
        if Some(qi) == self.upnext_cur || qi >= self.upnext.len() {
            return; // the playing song stays until it is done
        }
        self.upnext.remove(qi);
        if let Some(k) = self.upnext_cur.as_mut() {
            if *k > qi {
                *k -= 1;
            }
        }
        self.queue_changed();
    }

    /* ---------- session (remember last playback) ---------- */

    fn save_session(&mut self) {
        self.last_session_save = Instant::now();
        let position = if self.player.is_active() {
            self.player.position()
        } else {
            self.resume_at.unwrap_or(0.0)
        };
        session::save(&session::Session {
            queue: self.queue.iter().map(|t| t.key.clone()).collect(),
            unshuffled: if self.cfg.shuffle {
                self.unshuffled.iter().map(|t| t.key.clone()).collect()
            } else {
                Vec::new()
            },
            pos: self.queue_pos,
            position,
            upnext: self.upnext.iter().map(|t| t.key.clone()).collect(),
            upnext_cur: self.upnext_cur,
        });
    }

    /// Reopens the last play order and queue, paused at the last position
    /// (no sound, no Discord until the user presses play).
    fn restore_session(&mut self) {
        let Some(sess) = session::load() else { return };
        let mut by_key: HashMap<&str, &Track> = HashMap::new();
        for t in &self.local {
            by_key.insert(t.key.as_str(), t);
        }
        if let Some(s) = &self.server {
            for t in &s.tracks {
                by_key.insert(t.key.as_str(), t);
            }
        }
        let pick = |keys: &[String]| -> Vec<Track> {
            keys.iter()
                .filter_map(|k| by_key.get(k.as_str()).map(|t| (*t).clone()))
                .collect()
        };
        // The saved position counts duplicates: find the same occurrence again.
        let locate = |keys: &[String], list: &[Track], pos: usize| -> Option<usize> {
            let key = keys.get(pos)?;
            let nth = keys[..pos].iter().filter(|k| *k == key).count();
            list.iter()
                .enumerate()
                .filter(|(_, t)| &t.key == key)
                .map(|(i, _)| i)
                .nth(nth)
                .or_else(|| list.iter().position(|t| &t.key == key))
        };
        let queue = pick(&sess.queue);
        let unshuffled = pick(&sess.unshuffled);
        let upnext = pick(&sess.upnext);
        let ctx_pos = locate(&sess.queue, &queue, sess.pos);
        let up_pos = sess
            .upnext_cur
            .and_then(|i| locate(&sess.upnext, &upnext, i));
        self.unshuffled = if unshuffled.is_empty() {
            queue.clone()
        } else {
            unshuffled
        };
        self.queue = queue;
        self.queue_pos = ctx_pos.unwrap_or(0);
        self.upnext = upnext;
        let resume = Some(sess.position.max(0.0));
        if let Some(k) = up_pos {
            self.upnext_cur = Some(k);
            self.resume_at = resume;
            self.idle_restore = true;
            let t = self.upnext[k].clone();
            self.start(t);
        } else if ctx_pos.is_some() {
            self.resume_at = resume;
            self.idle_restore = true;
            self.play_current();
        }
        self.queue_changed();
    }

    /* ---------- tray ---------- */

    fn hide_to_tray(&mut self) {
        let Some(ui) = self.ui() else { return };
        if self.tray.is_none() {
            // no tray icon: hiding would make the app unreachable, so just minimize
            ui.window().set_minimized(true);
            return;
        }
        use slint::winit_030::WinitWindowAccessor;
        ui.window().with_winit_window(|w| w.set_visible(false));
        self.save_session();
    }

    fn show_from_tray(&mut self) {
        let Some(ui) = self.ui() else { return };
        use slint::winit_030::WinitWindowAccessor;
        ui.window().with_winit_window(|w| w.set_visible(true));
        bring_to_front(ui.window());
    }

    fn poll_tray(&mut self) {
        let cmds = match &self.tray {
            Some(t) => t.poll(),
            None => return,
        };
        for c in cmds {
            match c {
                tray::TrayCmd::Show => self.show_from_tray(),
                tray::TrayCmd::TogglePlay => self.toggle_play(),
                tray::TrayCmd::Prev => self.prev(),
                tray::TrayCmd::Next => self.next(),
                tray::TrayCmd::Quit => {
                    let _ = slint::quit_event_loop();
                }
            }
        }
        let tip = match &self.current {
            Some(t) if !t.artist.is_empty() => format!("{} – {}", t.title, t.artist),
            Some(t) => t.title.clone(),
            None => "Simple Player".to_string(),
        };
        // Windows limits tray tooltips to 127 characters
        let tip: String = tip.chars().take(120).collect();
        if let Some(t) = self.tray.as_mut() {
            t.set_tooltip(&tip);
        }
    }

    /* ---------- audio output ---------- */

    /// Fills the device list in the settings (index 0 = system default).
    fn refresh_output_devices(&mut self) {
        let names = self.player.output_devices();
        let mut items: Vec<SharedString> = vec!["系統預設".into()];
        items.extend(names.iter().map(|n| SharedString::from(n.as_str())));
        let want = self.cfg.output_device.clone();
        let index = if want.is_empty() {
            0
        } else if let Some(i) = names.iter().position(|n| *n == want) {
            i + 1
        } else {
            // chosen before but not plugged in now: keep showing it
            items.push(format!("{want}（未連接）").into());
            items.len() - 1
        };
        self.output_names = names;
        if let Some(ui) = self.ui() {
            ui.set_output_devices(ModelRc::new(VecModel::from(items)));
            ui.set_output_index(index as i32);
            ui.set_exclusive(self.cfg.exclusive);
            ui.set_buffer_level(self.cfg.buffer_level as i32);
            ui.set_output_status(self.player.output_status().into());
        }
    }

    fn pick_output(&mut self, index: usize) {
        let name = if index == 0 {
            None
        } else if let Some(n) = self.output_names.get(index - 1) {
            Some(n.clone())
        } else {
            return; // the "not connected" entry
        };
        let ok = self.player.set_output_device(name.clone());
        self.cfg.output_device = name.clone().unwrap_or_default();
        config::save(&self.cfg);
        self.flash(match (&name, ok) {
            (None, _) => "音訊輸出：系統預設".to_string(),
            (Some(n), true) => format!("音訊輸出：{n}"),
            (Some(n), false) => format!("無法開啟「{n}」，暫時改用系統預設"),
        });
        self.refresh_output_devices();
    }

    fn set_exclusive(&mut self, on: bool) {
        self.cfg.exclusive = on;
        config::save(&self.cfg);
        let msg = match self.player.set_exclusive(on) {
            Ok(d) if on => format!("WASAPI 獨佔：{d}"),
            Ok(_) => "已改回共享模式（Windows 混音）".to_string(),
            Err(e) => e,
        };
        self.flash(msg);
        self.refresh_output_devices();
    }

    /// Short message in the status line.
    fn flash(&mut self, msg: String) {
        self.status = msg;
        self.update_status();
    }

    /// Plays `queue[queue_pos]` (the current list, not the user's queue).
    fn play_current(&mut self) {
        let Some(t) = self.queue.get(self.queue_pos).cloned() else {
            return;
        };
        self.upnext_cur = None;
        self.start(t);
    }

    /// Switches to `t`: the now-playing area fades out, cover and lyrics are loaded in the
    /// background, and only then the new song is shown and starts playing.
    fn start(&mut self, t: Track) {
        self.load_gen += 1;
        self.switching = true;
        let gen = self.load_gen;
        self.player.stop();
        self.current = Some(t.clone());
        self.cover_file = None;
        // a restored session opens paused: don't flash the pause icon meanwhile
        self.last_playing = self.resume_at.is_none();
        self.lyrics.clear();
        self.active_lyric = -1;
        if let Some(ui) = self.ui() {
            ui.set_switching(true);
            ui.set_playing(self.resume_at.is_none());
            ui.set_progress(0.0);
            ui.set_position_text("0:00".into());
            ui.set_active_lyric(-1);
        }
        self.mark_playing();
        if let Some(pw) = self.pw.upgrade() {
            pw.set_switching(true);
        }

        let client = self.client.clone();
        let base = self
            .server
            .as_ref()
            .map(|s| s.base.clone())
            .unwrap_or_default();
        let started = Instant::now();
        std::thread::spawn(move || {
            // Cover and lyrics load in parallel; a slow server can delay playback by 5 s at most.
            let (tx, rx) = std::sync::mpsc::channel::<Loaded>();
            {
                let (t, client, base, tx) = (t.clone(), client.clone(), base.clone(), tx.clone());
                std::thread::spawn(move || {
                    let file = match t.source {
                        Source::Local => library::local_cover(&t),
                        Source::Server => t
                            .server_id
                            .as_deref()
                            .and_then(|id| server::cover_file(&client, &base, id)),
                    };
                    let _ = tx.send(Loaded::Cover(file));
                });
            }
            {
                let (t, tx) = (t.clone(), tx);
                std::thread::spawn(move || {
                    let lines = match t.source {
                        Source::Local => library::local_lyrics(&t),
                        Source::Server => t
                            .server_id
                            .as_deref()
                            .map(|id| server::lyrics(&client, &base, id))
                            .unwrap_or_default(),
                    };
                    let _ = tx.send(Loaded::Lyrics(lines));
                });
            }
            let deadline = started + Duration::from_secs(5);
            let (mut cover, mut lyrics) = (None, None);
            while cover.is_none() || lyrics.is_none() {
                let left = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(left) {
                    Ok(Loaded::Cover(f)) => cover = Some(f),
                    Ok(Loaded::Lyrics(l)) => lyrics = Some(l),
                    Err(_) => break,
                }
            }
            // Let the fade-out finish so the switch always reads as one motion.
            let min = Duration::from_millis(220);
            if started.elapsed() < min {
                std::thread::sleep(min - started.elapsed());
            }
            let cover = cover.flatten();
            let lyrics = lyrics.unwrap_or_default();
            post(move |app| app.finish_switch(gen, t, cover, lyrics));
        });
    }

    fn finish_switch(
        &mut self,
        gen: u64,
        t: Track,
        cover: Option<PathBuf>,
        lyrics: Vec<(Option<f64>, String)>,
    ) {
        if gen != self.load_gen {
            return; // the user already picked another song
        }
        self.switching = false;
        if let Some(ui) = self.ui() {
            ui.set_now_title(t.title.as_str().into());
            ui.set_now_artist(
                if t.artist.is_empty() {
                    t.album.as_str()
                } else {
                    t.artist.as_str()
                }
                .into(),
            );
            ui.set_now_album(t.album.as_str().into());
            ui.set_now_detail(
                format!(
                    "{}  ·  {}",
                    t.ext.to_uppercase(),
                    match t.source {
                        Source::Local => "本地",
                        Source::Server => "伺服器",
                    }
                )
                .into(),
            );
            let img = cover
                .as_ref()
                .and_then(|f| slint::Image::load_from_path(f).ok());
            ui.set_has_cover(img.is_some());
            if let Some(img) = img {
                ui.set_cover_image(img);
            }
            ui.set_duration_text(
                if t.duration_ms > 0 {
                    fmt_time(t.duration_ms as f64 / 1000.0)
                } else {
                    "0:00".into()
                }
                .into(),
            );
        }
        self.cover_file = cover;
        self.set_lyrics(lyrics); // also mirrors to the detached player

        if let Some(ui) = self.ui() {
            ui.set_switching(false);
        }
        if let Some(pw) = self.pw.upgrade() {
            pw.set_switching(false);
        }

        // Now start the audio (a restored session opens paused at the saved position).
        let resume = self.resume_at.take();
        let paused = resume.is_some();
        if paused {
            self.last_playing = false;
            if let Some(ui) = self.ui() {
                ui.set_playing(false);
            }
        }
        let ext = Some(t.ext.clone());
        match t.source {
            Source::Local => {
                let Some(path) = t.path.clone() else { return };
                self.player.open(
                    Box::new(move || {
                        std::fs::File::open(&path)
                            .map(|f| Box::new(f) as Box<dyn symphonia::core::io::MediaSource>)
                            .map_err(|e| format!("無法開啟檔案：{e}"))
                    }),
                    ext,
                    paused,
                );
            }
            Source::Server => {
                let (Some(s), Some(id)) = (&self.server, t.server_id.clone()) else {
                    return;
                };
                let url = server::stream_url(&s.base, &id);
                let client = self.client.clone();
                self.player.open(
                    Box::new(move || {
                        httpsrc::HttpSource::open(client, url)
                            .map(|h| Box::new(h) as Box<dyn symphonia::core::io::MediaSource>)
                            .map_err(|e| format!("無法串流：{e}"))
                    }),
                    ext,
                    paused,
                );
            }
        }
        if let Some(at) = resume.filter(|p| *p > 1.0) {
            self.player.seek(at);
        }
        if let Some(m) = self.media.as_mut() {
            let cover = self
                .cover_file
                .as_ref()
                .map(|f| format!("file://{}", f.display()));
            m.set_track(
                &t.title,
                &t.artist,
                &t.album,
                cover.as_deref(),
                t.duration_ms as f64 / 1000.0,
            );
            m.set_state(!paused, resume.unwrap_or(0.0));
        }
        if self.view == View::Queue {
            self.refresh_list();
        }
        // Presence is pushed as soon as the decoder knows the duration (see sync_state).
        self.opened_at = Instant::now();
        self.presence_dirty.get_or_insert_with(Instant::now);
    }

    fn set_lyrics(&mut self, lines: Vec<(Option<f64>, String)>) {
        let synced = lines.iter().any(|(t, _)| t.is_some());
        self.lyrics = lines;
        self.active_lyric = -1;
        let Some(ui) = self.ui() else { return };
        let rows: Vec<LyricLine> = self
            .lyrics
            .iter()
            .map(|(t, text)| LyricLine {
                text: if text.trim().is_empty() {
                    "…".into()
                } else {
                    text.as_str().into()
                },
                timed: t.is_some(),
            })
            .collect();
        ui.set_lyrics_state(
            if self.lyrics.is_empty() {
                "NO LYRICS"
            } else if synced {
                "SYNCED"
            } else {
                "UNSYNCED"
            }
            .into(),
        );
        ui.set_lyrics(ModelRc::new(VecModel::from(rows)));
        ui.set_active_lyric(-1);
        self.mirror_player(true);
    }

    fn seek_lyric(&mut self, index: usize) {
        if let Some((Some(t), _)) = self.lyrics.get(index) {
            self.player.seek(*t);
            self.presence_dirty.get_or_insert_with(Instant::now);
        }
    }

    fn toggle_play(&mut self) {
        if self.switching {
            return; // a song is being prepared and will start by itself
        }
        if !self.player.is_active() {
            self.idle_restore = false;
            if let Some(t) = self.current.clone() {
                self.start(t);
            } else if let Some(ui) = self.ui() {
                let sel = ui.get_selected_row();
                self.play_row(if sel >= 0 { sel as usize } else { 0 });
            }
            return;
        }
        if self.player.is_paused() {
            self.idle_restore = false;
            self.player.play();
        } else {
            self.player.pause();
        }
        self.sync_state(true);
    }

    fn next(&mut self) {
        self.advance(false);
    }

    /// Next song. `auto` = the current one ended by itself (repeat-one replays it).
    /// Queued songs come first; after them the current list continues where it was.
    fn advance(&mut self, auto: bool) {
        if self.queue.is_empty() && self.upnext_cur.is_none() {
            return;
        }
        self.idle_restore = false;
        if auto && self.cfg.repeat == 2 {
            if let Some(t) = self.current.clone() {
                self.start(t);
            }
            return;
        }
        // Playing from the queue (a temporary playlist): a song that was listened to the end is
        // removed; skipping keeps it. The queue only plays while you are in it, so other lists
        // never touch it.
        if let Some(k) = self.upnext_cur {
            let mut next = k + 1;
            if auto && k < self.upnext.len() {
                self.upnext.remove(k);
                next = k;
            }
            self.upnext_cur = None;
            let n = self.upnext.len();
            let pick = if n == 0 {
                None
            } else if self.cfg.shuffle && n > 1 {
                let mut r = (self.rand() % n as u64) as usize;
                if !auto && r == k {
                    r = (r + 1) % n; // a skip should move on
                }
                Some(r)
            } else if next < n {
                Some(next)
            } else if self.cfg.repeat >= 1 {
                Some(0) // repeat all: the songs that were skipped come round again
            } else {
                None
            };
            match pick {
                Some(i) => self.play_upnext(i),
                None => {
                    self.player.stop();
                    self.current = None;
                    self.mark_playing();
                    self.sync_state(true);
                }
            }
            self.queue_changed();
            return;
        }
        if self.queue.is_empty() {
            self.player.stop();
            self.sync_state(true);
        } else if self.queue_pos + 1 < self.queue.len() {
            self.queue_pos += 1;
            self.play_current();
        } else if self.cfg.repeat >= 1 {
            // repeat all: start over (a fresh order when shuffling)
            self.queue_pos = 0;
            if self.cfg.shuffle && self.queue.len() > 2 {
                let last = self.queue.last().map(|t| t.key.clone());
                self.shuffle_rest();
                // don't play the song that just ended right away again
                if self.queue.first().map(|t| &t.key) == last.as_ref() {
                    self.queue.swap(0, 1);
                }
            }
            self.play_current();
        } else {
            self.player.stop();
            self.sync_state(true);
        }
        self.queue_changed();
    }

    fn prev(&mut self) {
        if self.queue.is_empty() && self.upnext_cur.is_none() {
            return;
        }
        if let Some(k) = self.upnext_cur {
            if self.player.position() > 3.0 || k == 0 {
                self.player.seek(0.0);
                self.presence_dirty.get_or_insert_with(Instant::now);
            } else {
                self.play_upnext(k - 1);
            }
            return;
        }
        if self.player.position() > 3.0 || self.queue_pos == 0 {
            self.player.seek(0.0);
            self.presence_dirty.get_or_insert_with(Instant::now);
            return;
        }
        self.queue_pos -= 1;
        self.idle_restore = false;
        self.play_current();
    }

    fn seek_by(&mut self, delta: f64) {
        if !self.player.is_active() {
            return;
        }
        let d = self.player.duration();
        let mut p = self.player.position() + delta;
        if d > 0.0 {
            p = p.min(d - 0.5);
        }
        self.player.seek(p.max(0.0));
        self.presence_dirty.get_or_insert_with(Instant::now);
    }

    fn volume_by(&mut self, delta: f32) {
        let v = (self.cfg.volume + delta).clamp(0.0, 1.0);
        self.cfg.volume = v;
        self.player.set_volume(v);
        if let Some(ui) = self.ui() {
            ui.set_volume(v);
        }
    }

    fn seek_fraction(&mut self, f: f32) {
        let d = self.player.duration();
        if d > 0.0 {
            self.player.seek(d * f as f64);
            self.presence_dirty.get_or_insert_with(Instant::now);
        }
    }

    fn on_player_event(&mut self, ev: PlayerEvent) {
        match ev {
            PlayerEvent::Ended => self.advance(true),
            PlayerEvent::Error(e) => {
                self.status = e;
                self.update_status();
                if let Some(ui) = self.ui() {
                    ui.set_playing(false);
                }
            }
        }
    }

    /* ---------- periodic updates ---------- */

    fn tick(&mut self) {
        let Some(ui) = self.ui() else { return };
        let pos = self.player.position();
        let dur = self.player.duration();
        let playing = self.player.is_active() && !self.player.is_paused();
        ui.set_playing(
            playing || (self.current.is_some() && !self.player.is_active() && self.last_playing),
        );
        if self.player.is_active() {
            ui.set_progress(if dur > 0.0 { (pos / dur) as f32 } else { 0.0 });
            ui.set_position_text(fmt_time(pos).into());
            if dur > 0.0 {
                ui.set_duration_text(fmt_time(dur).into());
            }
        }
        if dur > 0.0 {
            self.learn_duration(dur);
        }
        let active = active_lyric(&self.lyrics, pos + 0.15);
        if active != self.active_lyric {
            self.active_lyric = active;
            ui.set_active_lyric(active);
        }
        self.mirror_player(false);
        if self.last_session_save.elapsed() > Duration::from_secs(15) {
            self.save_session();
        }
        if playing != self.last_playing {
            self.last_playing = playing;
            self.sync_state(true);
        } else {
            self.sync_state(false);
        }
    }

    /// Stores the decoder's duration for tracks whose tags/server gave none.
    fn learn_duration(&mut self, dur: f64) {
        let Some(cur) = self.current.as_mut() else {
            return;
        };
        if cur.duration_ms > 0 {
            return;
        }
        let ms = (dur * 1000.0) as u64;
        cur.duration_ms = ms;
        let key = cur.key.clone();
        let lists = [&mut self.visible, &mut self.queue, &mut self.local];
        for list in lists {
            for t in list.iter_mut().filter(|t| t.key == key) {
                t.duration_ms = ms;
            }
        }
        if let Some(s) = self.server.as_mut() {
            for t in s.tracks.iter_mut().filter(|t| t.key == key) {
                t.duration_ms = ms;
            }
        }
        if let Some(i) = self.visible.iter().position(|t| t.key == key) {
            if let Some(ui) = self.ui() {
                let model = ui.get_tracks();
                if let Some(mut row) = model.row_data(i) {
                    row.duration = fmt_time(dur).into();
                    model.set_row_data(i, row);
                }
            }
        }
        if let (Some(m), Some(t)) = (self.media.as_mut(), self.current.as_ref()) {
            let cover = self
                .cover_file
                .as_ref()
                .map(|f| format!("file://{}", f.display()));
            m.set_track(&t.title, &t.artist, &t.album, cover.as_deref(), dur);
            m.set_state(!self.player.is_paused(), self.player.position());
        }
    }

    /// Pushes the play state to Windows media controls and Discord.
    fn sync_state(&mut self, changed: bool) {
        let active = self.player.is_active();
        let paused = self.player.is_paused();
        let pos = self.player.position();
        if changed {
            if let Some(m) = self.media.as_mut() {
                m.set_state(active && !paused, pos);
            }
        }
        if changed {
            self.presence_dirty.get_or_insert_with(Instant::now);
        }
        if !self.cfg.discord {
            self.presence_dirty = None;
            return;
        }
        // A new track: wait (briefly) until the decoder knows the length, so the
        // progress bar Discord draws is right from the first update.
        if active
            && self.player.duration() <= 0.0
            && self.opened_at.elapsed() < Duration::from_secs(3)
        {
            return;
        }
        // Any user action is sent at once. Otherwise Discord animates the bar on its own:
        // playing = an occasional resync only; paused = re-anchor every 5 s so it looks frozen.
        let every = if paused {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(300)
        };
        match self.presence_dirty {
            Some(at) if at.elapsed() < Duration::from_millis(300) => return,
            None if self.last_presence.elapsed() < every => return,
            _ => {}
        }
        self.presence_dirty = None;
        self.last_presence = Instant::now();
        let Some(t) = self
            .current
            .as_ref()
            .filter(|_| active && !self.idle_restore)
        else {
            self.discord.set_activity(None);
            return;
        };
        let (cover_url, share_url) = self.public_links(t);
        let act = discord::activity(&discord::NowPlaying {
            title: &t.title,
            artist: &t.artist,
            album: &t.album,
            position: pos,
            duration: self.player.duration(),
            paused,
            cover_url,
            share_url,
        });
        self.discord.set_activity(Some(act));
    }

    /// Public https origin Discord can reach: the user's setting, else the server's publicOrigin,
    /// else the server address itself when it already is https.
    fn public_origin(&self) -> Option<String> {
        let own = self.cfg.public_url.trim().trim_end_matches('/');
        if own.starts_with("https://") {
            return Some(own.to_string());
        }
        let s = self.server.as_ref()?;
        if s.public_origin.starts_with("https://") {
            Some(s.public_origin.clone())
        } else if s.base.starts_with("https://") {
            Some(s.base.clone())
        } else {
            None
        }
    }

    /// Cover and share links for Discord. Server tracks use their own cover; local tracks
    /// (no public URL) show the Simple Player icon from the server instead.
    fn public_links(&self, t: &Track) -> (String, String) {
        let Some(origin) = self.public_origin() else {
            return (String::new(), String::new());
        };
        match t.server_id.as_deref() {
            Some(id) => (
                server::cover_url(&origin, id),
                format!("{origin}/s/{}", server::enc(id)),
            ),
            None => (format!("{origin}/icon-512.png"), String::new()),
        }
    }

    fn update_status(&self) {
        let Some(ui) = self.ui() else { return };
        let mut parts = Vec::new();
        if self.scanning {
            parts.push("掃描本地資料夾中…".to_string());
        }
        parts.push(format!("本地 {} 首", self.local.len()));
        if let Some(s) = &self.server {
            parts.push(format!("伺服器 {} 首", s.tracks.len()));
        }
        if !self.status.is_empty() {
            parts.push(self.status.clone());
        }
        ui.set_status_text(parts.join("  ·  ").into());
    }

    fn refresh_discord_status(&self) {
        let Some(ui) = self.ui() else { return };
        let text = if !self.cfg.discord {
            "已關閉".to_string()
        } else {
            let s = self.discord.status();
            if s.connected {
                format!(
                    "已連線{}",
                    if s.user.is_empty() {
                        String::new()
                    } else {
                        format!("（{}）", s.user)
                    }
                )
            } else if !s.error.is_empty() {
                s.error
            } else {
                "播放時自動連線".to_string()
            }
        };
        ui.set_discord_status(text.into());
    }

    /* ---------- library loading ---------- */

    fn start_local_scan(&mut self) {
        if self.cfg.local_folders.is_empty() {
            self.local.clear();
            self.rebuild_sources();
            self.update_status();
            return;
        }
        self.scanning = true;
        self.update_status();
        let folders = self.cfg.local_folders.clone();
        std::thread::spawn(move || {
            let tracks = library::scan_local(&folders, |_| {});
            post(move |app| {
                app.scanning = false;
                app.local = tracks;
                app.rebuild_sources();
                app.update_status();
            });
        });
    }

    fn connect_server(&mut self) {
        let base = server::normalize(&self.cfg.server);
        if base.is_empty() {
            self.server = None;
            self.rebuild_sources();
            self.set_server_status("未設定".into());
            return;
        }
        if self.server.as_ref().map(|s| s.base != base).unwrap_or(true) {
            self.server = server::load_cache(&base);
            self.rebuild_sources();
        }
        self.set_server_status("連線中…".into());
        let client = self.client.clone();
        std::thread::spawn(move || {
            let res = server::fetch(&client, &base);
            post(move |app| match res {
                Ok(data) => {
                    let n = data.tracks.len();
                    app.server = Some(data);
                    app.rebuild_sources();
                    app.update_status();
                    app.set_server_status(format!("已連線，{n} 首歌"));
                }
                Err(e) => app.set_server_status(e),
            });
        });
    }

    fn set_server_status(&self, s: String) {
        if let Some(ui) = self.ui() {
            ui.set_server_status(s.into());
        }
    }

    fn push_folders(&self) {
        if let Some(ui) = self.ui() {
            let items: Vec<SharedString> = self
                .cfg
                .local_folders
                .iter()
                .map(|p| p.display().to_string().into())
                .collect();
            ui.set_folders(ModelRc::new(VecModel::from(items)));
        }
    }

    fn handle_media_key(&mut self, key: media::MediaKey) {
        use media::MediaKey::*;
        match key {
            Play => {
                if !self.player.is_active() || self.player.is_paused() {
                    self.toggle_play();
                }
            }
            Pause => {
                if self.player.is_active() && !self.player.is_paused() {
                    self.toggle_play();
                }
            }
            Toggle => self.toggle_play(),
            Next => self.next(),
            Previous => self.prev(),
            Stop => {
                self.player.pause();
                self.sync_state(true);
            }
        }
    }
}

/// Index of the lyric line being sung at `pos` (timed lyrics only).
fn active_lyric(lines: &[(Option<f64>, String)], pos: f64) -> i32 {
    let mut best = -1;
    for (i, (t, _)) in lines.iter().enumerate() {
        match t {
            Some(t) if *t <= pos => best = i as i32,
            Some(_) => break,
            None => {}
        }
    }
    best
}

fn window_hwnd(win: &slint::Window) -> Option<*mut std::ffi::c_void> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = win.window_handle();
    let wh = handle.window_handle().ok()?;
    match wh.as_raw() {
        RawWindowHandle::Win32(w) => Some(w.hwnd.get() as *mut std::ffi::c_void),
        _ => None,
    }
}

/// Parses "#rrggbb" / "rrggbb" / "#rgb".
fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.trim().trim_start_matches('#');
    let h: String = match h.len() {
        3 => h.chars().flat_map(|c| [c, c]).collect(),
        6 => h.to_string(),
        _ => return None,
    };
    let v = u32::from_str_radix(&h, 16).ok()?;
    Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// Relative luminance (sRGB), 0..1.
fn luminance((r, g, b): (u8, u8, u8)) -> f32 {
    let lin = |c: u8| {
        let c = c as f32 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// Applies the theme colour to a window's palette: a variant readable on the dark and on
/// the light background, plus a text colour that stays legible on top of it.
fn apply_accent(p: &Palette, hex: &str) {
    let Some(base) = parse_hex(hex) else { return };
    let scale = |(r, g, b): (u8, u8, u8), f: f32| {
        let m = |c: u8| (c as f32 * f).round().clamp(0.0, 255.0) as u8;
        (m(r), m(g), m(b))
    };
    let mix_white = |(r, g, b): (u8, u8, u8), t: f32| {
        let m = |c: u8| (c as f32 + (255.0 - c as f32) * t).round() as u8;
        (m(r), m(g), m(b))
    };
    // dark theme: lift very dark picks so they still show up
    let mut on_dark = base;
    for _ in 0..12 {
        if luminance(on_dark) >= 0.16 {
            break;
        }
        on_dark = mix_white(on_dark, 0.12);
    }
    // light theme: deepen bright picks (yellow, mint…) so they read on near-white
    let mut on_light = base;
    for _ in 0..16 {
        if luminance(on_light) <= 0.30 {
            break;
        }
        on_light = scale(on_light, 0.92);
    }
    let text = |c: (u8, u8, u8)| {
        if luminance(c) > 0.40 {
            slint::Color::from_rgb_u8(0x1a, 0x16, 0x10)
        } else {
            slint::Color::from_rgb_u8(0xff, 0xff, 0xff)
        }
    };
    let col = |(r, g, b): (u8, u8, u8)| slint::Color::from_rgb_u8(r, g, b);
    p.set_accent_base(col(base));
    p.set_accent_on_dark(col(on_dark));
    p.set_accent_on_light(col(on_light));
    p.set_accent_text_dark(text(on_dark));
    p.set_accent_text_light(text(on_light));
}

/// Restores (if minimized) and activates a window.
fn bring_to_front(win: &slint::Window) {
    use slint::winit_030::WinitWindowAccessor;
    win.set_minimized(false);
    win.with_winit_window(|w| w.focus_window());
    #[cfg(windows)]
    if let Some(hwnd) = window_hwnd(win) {
        #[link(name = "user32")]
        extern "system" {
            fn SetForegroundWindow(hwnd: *mut std::ffi::c_void) -> i32;
            fn BringWindowToTop(hwnd: *mut std::ffi::c_void) -> i32;
        }
        // Allowed here: the click that triggered this came from our own (foreground) window.
        unsafe {
            BringWindowToTop(hwnd);
            SetForegroundWindow(hwnd);
        }
    }
}

/// Starts a system move of a frameless window (call while the mouse button is down).
fn drag_window(win: &slint::Window) {
    use slint::winit_030::WinitWindowAccessor;
    win.with_winit_window(|w| {
        let _ = w.drag_window();
    });
}

/// Starts a system resize from an edge: 0 N, 1 NE, 2 E, 3 SE, 4 S, 5 SW, 6 W, 7 NW.
fn resize_window(win: &slint::Window, dir: i32) {
    use slint::winit_030::winit::window::ResizeDirection as D;
    use slint::winit_030::WinitWindowAccessor;
    let d = match dir {
        0 => D::North,
        1 => D::NorthEast,
        2 => D::East,
        3 => D::SouthEast,
        4 => D::South,
        5 => D::SouthWest,
        6 => D::West,
        _ => D::NorthWest,
    };
    win.with_winit_window(|w| {
        let _ = w.drag_resize_window(d);
    });
}

/// Frameless window: ask Windows for the drop shadow and (Windows 11) rounded corners.
/// Windows itself drops the rounding while the window is maximized.
fn window_chrome(win: &slint::Window, hwnd: *mut std::ffi::c_void) {
    #[cfg(windows)]
    {
        use slint::winit_030::winit::platform::windows::WindowExtWindows;
        use slint::winit_030::WinitWindowAccessor;
        // Icons from the .exe resource (id 1, see build.rs): Windows then picks the hand-tuned
        // 16/20/24/32 px layers instead of shrinking one big PNG, so the taskbar icon stays sharp.
        win.with_winit_window(|w| {
            use slint::winit_030::winit::dpi::PhysicalSize;
            use slint::winit_030::winit::platform::windows::IconExtWindows;
            use slint::winit_030::winit::window::Icon;
            w.set_undecorated_shadow(true);
            let scale = w.scale_factor();
            let px = |n: f64| {
                let v = (n * scale).round() as u32;
                Some(PhysicalSize::new(v, v))
            };
            if let Ok(icon) = Icon::from_resource(1, px(16.0)) {
                w.set_window_icon(Some(icon));
            }
            if let Ok(icon) = Icon::from_resource(1, px(32.0)) {
                w.set_taskbar_icon(Some(icon));
            }
        });

        #[link(name = "dwmapi")]
        extern "system" {
            fn DwmSetWindowAttribute(
                hwnd: *mut std::ffi::c_void,
                attr: u32,
                value: *const std::ffi::c_void,
                size: u32,
            ) -> i32;
        }
        const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
        const DWMWCP_ROUND: u32 = 2;
        let pref = DWMWCP_ROUND;
        // Fails harmlessly on Windows 10.
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const u32 as *const std::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            );
        }
    }
    #[cfg(not(windows))]
    let _ = (win, hwnd);
}

fn install_crash_log() {
    // Release builds have no console: write panics to %LOCALAPPDATA%\SimplePlayer\crash.log.
    std::panic::set_hook(Box::new(|info| {
        let msg = format!(
            "[{:?}] {}\n{}\n\n",
            std::time::SystemTime::now(),
            info,
            std::backtrace::Backtrace::force_capture()
        );
        eprintln!("{msg}");
        let dir = config::cache_dir();
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("crash.log"))
        {
            use std::io::Write;
            let _ = f.write_all(msg.as_bytes());
        }
    }));
}

fn main() -> Result<(), slint::PlatformError> {
    install_crash_log();
    // Already running (maybe hidden in the tray)? Wake that copy and quit.
    let Some(instance) = single::acquire() else {
        return Ok(());
    };
    let ui = MainWindow::new()?;
    let pw = PlayerWindow::new()?;
    let cfg = config::load();

    let player = Player::new(
        |ev| post(move |app| app.on_player_event(ev)),
        (!cfg.output_device.is_empty()).then(|| cfg.output_device.clone()),
        cfg.exclusive,
    );
    player.set_buffer_level(cfg.buffer_level as u32);
    player.set_volume(cfg.volume);
    let discord = discord::Discord::new(cfg.discord_client_id.clone());

    ui.set_volume(cfg.volume);
    ui.set_server_url(cfg.server.as_str().into());
    ui.set_public_url(cfg.public_url.as_str().into());
    ui.set_discord_enabled(cfg.discord);
    ui.global::<Palette>().set_light(cfg.light);
    pw.global::<Palette>().set_light(cfg.light);
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    ui.set_pinned(cfg.pin_main);
    apply_accent(&ui.global::<Palette>(), &cfg.accent);
    apply_accent(&pw.global::<Palette>(), &cfg.accent);
    ui.set_accent_hex(cfg.accent.as_str().into());
    pw.set_pinned(cfg.pin_player);

    let app = App {
        ui: ui.as_weak(),
        pw: pw.as_weak(),
        pw_shown: false,
        load_gen: 0,
        switching: false,
        unshuffled: Vec::new(),
        upnext: Vec::new(),
        upnext_cur: None,
        visible_qidx: Vec::new(),
        rng: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e3779b97f4a7c15)
            | 1,
        resume_at: None,
        idle_restore: false,
        last_session_save: Instant::now(),
        tray: None,
        pending_view: None,
        pending_select: None,
        output_names: Vec::new(),
        local: library::load_local_cache(),
        server: None,
        client: server::client(),
        source_views: Vec::new(),
        view: View::Setup,
        query: String::new(),
        sort: None,
        visible: Vec::new(),
        queue: Vec::new(),
        queue_pos: 0,
        current: None,
        cover_file: None,
        player,
        discord,
        media: None,
        last_playing: false,
        last_presence: Instant::now(),
        presence_dirty: None,
        opened_at: Instant::now(),
        scanning: false,
        status: String::new(),
        lyrics: Vec::new(),
        active_lyric: -1,
        cfg,
    };
    APP.with(|a| *a.borrow_mut() = Some(app));

    with_app(|app| {
        if !app.cfg.last_view.is_empty() {
            app.pending_view = Some(app.cfg.last_view.clone());
        }
        if !app.cfg.last_selected.is_empty() {
            app.pending_select = Some(app.cfg.last_selected.clone());
        }
        app.push_folders();
        app.push_modes();
        app.rebuild_sources();
        app.update_status();
        app.start_local_scan();
        app.connect_server();
        app.refresh_discord_status();
        // local and server libraries come from their caches at this point
        app.restore_session();
    });

    /* ---------- callbacks ---------- */
    ui.on_source_clicked(|i| {
        with_app(|a| a.select_source(i.max(0) as usize));
    });
    ui.on_row_activated(|i| {
        with_app(|a| a.play_row(i.max(0) as usize));
    });
    ui.on_sort_by(|c| {
        with_app(|a| a.sort_by(c));
    });
    ui.on_search_changed(|q| {
        with_app(|a| {
            a.query = q.to_string();
            a.refresh_list();
        });
    });
    ui.on_toggle_play(|| {
        with_app(|a| a.toggle_play());
    });
    ui.on_next(|| {
        with_app(|a| a.next());
    });
    ui.on_prev(|| {
        with_app(|a| a.prev());
    });
    ui.on_seek(|f| {
        with_app(|a| a.seek_fraction(f));
    });
    ui.on_volume_changed(|v| {
        with_app(|a| {
            a.player.set_volume(v);
            a.cfg.volume = v;
        });
    });
    ui.on_save_server(|url| {
        with_app(|a| {
            a.cfg.server = server::normalize(&url);
            config::save(&a.cfg);
            if let Some(ui) = a.ui() {
                ui.set_server_url(a.cfg.server.as_str().into());
            }
            a.connect_server();
        });
    });
    ui.on_save_public_url(|url| {
        with_app(|a| {
            a.cfg.public_url = url.trim().trim_end_matches('/').to_string();
            config::save(&a.cfg);
            if let Some(ui) = a.ui() {
                ui.set_public_url(a.cfg.public_url.as_str().into());
            }
            a.sync_state(true);
        });
    });
    ui.on_add_folder(|| {
        // The folder picker runs its own message loop: keep the app state unborrowed meanwhile.
        let picked = rfd::FileDialog::new()
            .set_title("選擇音樂資料夾")
            .pick_folder();
        if let Some(dir) = picked {
            with_app(|a| {
                if !a.cfg.local_folders.contains(&dir) {
                    a.cfg.local_folders.push(dir);
                    config::save(&a.cfg);
                    a.push_folders();
                    a.start_local_scan();
                }
            });
        }
    });
    ui.on_remove_folder(|i| {
        with_app(|a| {
            let i = i.max(0) as usize;
            if i < a.cfg.local_folders.len() {
                a.cfg.local_folders.remove(i);
                config::save(&a.cfg);
                a.push_folders();
                a.start_local_scan();
            }
        });
    });
    ui.on_rescan(|| {
        with_app(|a| {
            a.start_local_scan();
            a.connect_server();
        });
    });
    ui.on_theme_toggled(|light| {
        with_app(|a| {
            a.cfg.light = light;
            config::save(&a.cfg);
            if let Some(pw) = a.pw.upgrade() {
                pw.global::<Palette>().set_light(light);
            }
        });
    });
    ui.on_accent_picked(|hex| {
        with_app(|a| {
            let Some((r, g, b)) = parse_hex(&hex) else {
                // invalid input: show the current colour again
                if let Some(ui) = a.ui() {
                    ui.set_accent_hex(a.cfg.accent.as_str().into());
                }
                return;
            };
            let hex = format!("#{r:02x}{g:02x}{b:02x}");
            a.cfg.accent = hex.clone();
            config::save(&a.cfg);
            if let Some(ui) = a.ui() {
                apply_accent(&ui.global::<Palette>(), &hex);
                ui.set_accent_hex(hex.as_str().into());
            }
            if let Some(pw) = a.pw.upgrade() {
                apply_accent(&pw.global::<Palette>(), &hex);
            }
        });
    });
    ui.on_lyric_clicked(|i| {
        with_app(|a| a.seek_lyric(i.max(0) as usize));
    });
    ui.on_discord_toggled(|on| {
        with_app(|a| {
            a.cfg.discord = on;
            config::save(&a.cfg);
            if on {
                a.sync_state(true);
            } else {
                a.discord.set_activity(None);
            }
            a.refresh_discord_status();
        });
    });

    /* ---------- frameless windows ---------- */
    {
        let weak = ui.as_weak();
        ui.on_win_drag(move || {
            if let Some(ui) = weak.upgrade() {
                drag_window(ui.window());
            }
        });
        let weak = ui.as_weak();
        ui.on_win_resize(move |dir| {
            if let Some(ui) = weak.upgrade() {
                resize_window(ui.window(), dir);
            }
        });
        let weak = ui.as_weak();
        ui.on_win_minimize(move || {
            if let Some(ui) = weak.upgrade() {
                ui.window().set_minimized(true);
            }
        });
        let weak = ui.as_weak();
        ui.on_win_maximize(move || {
            if let Some(ui) = weak.upgrade() {
                let max = !ui.window().is_maximized();
                ui.window().set_maximized(max);
                ui.set_is_max(max);
            }
        });
        // ✕ / Alt+F4: hide to the tray (playback goes on) or quit, depending on the setting.
        ui.on_win_close(|| {
            let to_tray = with_app(|a| a.cfg.close_to_tray && a.tray.is_some()).unwrap_or(false);
            if to_tray {
                with_app(|a| a.hide_to_tray());
            } else {
                let _ = slint::quit_event_loop();
            }
        });
        ui.window().on_close_requested(|| {
            let to_tray = with_app(|a| a.cfg.close_to_tray && a.tray.is_some()).unwrap_or(false);
            if to_tray {
                with_app(|a| a.hide_to_tray());
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                let _ = slint::quit_event_loop();
                slint::CloseRequestResponse::HideWindow
            }
        });
        ui.on_settings_opened(|| {
            // deferred: the dialog can be opened from inside other app code
            post(|a| a.refresh_output_devices());
        });
        ui.on_buffer_picked(|level| {
            with_app(|a| {
                let level = level.clamp(0, 2) as u8;
                a.cfg.buffer_level = level;
                config::save(&a.cfg);
                a.player.set_buffer_level(level as u32);
                if let Some(ui) = a.ui() {
                    ui.set_buffer_level(level as i32);
                }
            });
        });
        ui.on_exclusive_toggled(|on| {
            with_app(|a| a.set_exclusive(on));
        });
        ui.on_output_picked(|i| {
            with_app(|a| a.pick_output(i.max(0) as usize));
        });
        ui.on_close_to_tray_toggled(|on| {
            with_app(|a| {
                a.cfg.close_to_tray = on;
                config::save(&a.cfg);
            });
        });
        ui.on_toggle_shuffle(|| {
            with_app(|a| a.toggle_shuffle());
        });
        ui.on_cycle_repeat(|| {
            with_app(|a| a.cycle_repeat());
        });
        ui.on_seek_by(|d| {
            with_app(|a| a.seek_by(d as f64));
        });
        ui.on_volume_by(|d| {
            with_app(|a| a.volume_by(d));
        });
        ui.on_queue_add(|i| {
            with_app(|a| a.queue_add(i.max(0) as usize));
        });
        ui.on_queue_remove(|i| {
            with_app(|a| a.queue_remove(i.max(0) as usize));
        });
        pw.on_toggle_shuffle(|| {
            with_app(|a| a.toggle_shuffle());
        });
        pw.on_cycle_repeat(|| {
            with_app(|a| a.cycle_repeat());
        });
        pw.on_seek_by(|d| {
            with_app(|a| a.seek_by(d as f64));
        });
        ui.on_pin_toggled(|on| {
            with_app(|a| {
                a.cfg.pin_main = on;
                config::save(&a.cfg);
            });
        });
        ui.on_detach_toggled(|on| {
            with_app(|a| a.set_detached(on));
        });

        // detached player
        pw.on_toggle_play(|| {
            with_app(|a| a.toggle_play());
        });
        pw.on_next(|| {
            with_app(|a| a.next());
        });
        pw.on_prev(|| {
            with_app(|a| a.prev());
        });
        pw.on_seek(|f| {
            with_app(|a| a.seek_fraction(f));
        });
        pw.on_lyric_clicked(|i| {
            with_app(|a| a.seek_lyric(i.max(0) as usize));
        });
        pw.on_attach(|| {
            with_app(|a| a.set_detached(false));
        });
        pw.window().on_close_requested(|| {
            post(|a| a.set_detached(false));
            slint::CloseRequestResponse::KeepWindowShown
        });
        pw.on_pin_toggled(|on| {
            with_app(|a| {
                a.cfg.pin_player = on;
                config::save(&a.cfg);
            });
        });
        let weak = pw.as_weak();
        pw.on_win_drag(move || {
            if let Some(pw) = weak.upgrade() {
                drag_window(pw.window());
            }
        });
        let weak = pw.as_weak();
        pw.on_win_resize(move |dir| {
            if let Some(pw) = weak.upgrade() {
                resize_window(pw.window(), dir);
            }
        });
        let weak = pw.as_weak();
        pw.on_win_minimize(move || {
            if let Some(pw) = weak.upgrade() {
                pw.window().set_minimized(true);
            }
        });
    }

    /* ---------- timers ---------- */
    let tick = slint::Timer::default();
    tick.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        || {
            with_app(|a| a.tick());
        },
    );
    let slow = slint::Timer::default();
    slow.start(slint::TimerMode::Repeated, Duration::from_secs(1), || {
        with_app(|a| {
            a.refresh_discord_status();
            // live output format while the settings are open (it follows each song)
            if let Some(ui) = a.ui() {
                if ui.get_show_settings() {
                    ui.set_output_status(a.player.output_status().into());
                }
            }
            // Keep the maximize/restore icon right after Win+Up, snapping, etc.
            if let Some(ui) = a.ui() {
                let max = ui.window().is_maximized();
                if ui.get_is_max() != max {
                    ui.set_is_max(max);
                }
            }
        });
    });

    ui.show()?;

    // Windows media controls need the native window handle, which exists once shown.
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(300), move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some(hwnd) = window_hwnd(ui.window()) else {
            return;
        };
        window_chrome(ui.window(), hwnd);
        let media = media::Media::new(hwnd, |key| post(move |app| app.handle_media_key(key)));
        with_app(|a| {
            a.media = media;
            // a song restored before the media controls existed
            if let (Some(m), Some(t)) = (a.media.as_mut(), a.current.as_ref()) {
                let cover = a
                    .cover_file
                    .as_ref()
                    .map(|f| format!("file://{}", f.display()));
                m.set_track(
                    &t.title,
                    &t.artist,
                    &t.album,
                    cover.as_deref(),
                    t.duration_ms as f64 / 1000.0,
                );
                m.set_state(
                    a.player.is_active() && !a.player.is_paused(),
                    a.player.position(),
                );
            }
            if a.cfg.detached {
                a.set_detached(true);
            }
        });
    });

    with_app(|a| a.tray = tray::Tray::new());
    let tray_timer = slint::Timer::default();
    tray_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(120),
        move || {
            let show = instance.show_requested();
            with_app(|a| {
                if show {
                    a.show_from_tray();
                }
                a.poll_tray();
            });
        },
    );

    // Keep running while the main window is hidden in the tray; quit_event_loop() ends it.
    slint::run_event_loop_until_quit()?;

    with_app(|a| {
        a.discord.set_activity(None);
        a.save_session();
        a.remember_pw_geometry();
        config::save(&a.cfg);
        a.tray = None; // removes the tray icon now (the app state is leaked below)
    });
    // Give the Discord thread a moment to clear the status.
    std::thread::sleep(Duration::from_millis(150));
    // Take the app state out of thread-local storage and leak it: destroying it during
    // thread-local teardown (media controls, audio, window handles) after the event loop has
    // ended panics ("thread local panicked on drop"). The process is exiting anyway.
    if let Some(app) = APP.with(|a| a.borrow_mut().take()) {
        std::mem::forget(app);
    }
    Ok(())
}
