// Simple Player for Windows: local library + Simple Player Web Server, Discord Rich Presence.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod discord;
mod httpsrc;
mod library;
mod media;
mod player;
mod server;

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
    Local,
    LocalFolder(String),
    Server,
    Playlist(String),
    Setup,
}

impl View {
    fn key(&self) -> String {
        match self {
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
    pw_pos: Option<slint::PhysicalPosition>,
    pw_shown: bool,
    /// Bumped on every song switch; stale background loads are dropped.
    load_gen: u64,
    switching: bool,
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
            let w = (360.0 * scale) as u32;
            let h = ((main.size().height as f32) - 80.0 * scale).clamp(520.0 * scale, 760.0 * scale)
                as u32;
            pw.window().set_size(slint::PhysicalSize::new(w, h));
            let pos = self.pw_pos.unwrap_or_else(|| {
                // Pop out right where the panel was.
                let p = main.position();
                slint::PhysicalPosition::new(
                    p.x + main.size().width as i32 - w as i32 - (20.0 * scale) as i32,
                    p.y + (52.0 * scale) as i32,
                )
            });
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
                self.pw_pos = Some(pw.window().position());
            }
            let _ = pw.hide();
            self.pw_shown = false;
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

        // Keep the current view if it still exists, otherwise pick the first one.
        if !views.iter().any(|v| v.as_ref() == Some(&self.view)) {
            let wanted = self.cfg.last_view.clone();
            self.view = views
                .iter()
                .flatten()
                .find(|v| v.key() == wanted)
                .or_else(|| views.iter().flatten().find(|v| **v != View::Setup))
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
        let mut rows: Vec<Track> = self
            .tracks_for_view()
            .into_iter()
            .filter(|t| library::matches(t, &q))
            .collect();
        if let Some((col, asc)) = self.sort {
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
            View::Setup => "Simple Player".to_string(),
        };
        if let Some(ui) = self.ui() {
            ui.set_list_title(title.into());
            ui.set_list_info(format!("{} 首", self.visible.len()).into());
            ui.set_sort_column(self.sort.map(|s| s.0).unwrap_or(-1));
            ui.set_sort_asc(self.sort.map(|s| s.1).unwrap_or(true));
        }
        self.push_rows();
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
                playing: Some(t.key.as_str()) == cur,
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
            let playing = Some(t.key.as_str()) == cur;
            if let Some(mut row) = model.row_data(i) {
                if row.playing != playing {
                    row.playing = playing;
                    model.set_row_data(i, row);
                }
            }
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
        self.queue = self.visible.clone();
        self.queue_pos = index;
        self.play_current();
    }

    /// Switches to `queue[queue_pos]`: the now-playing area fades out, cover and lyrics are
    /// loaded in the background, and only then the new song is shown and starts playing.
    fn play_current(&mut self) {
        let Some(t) = self.queue.get(self.queue_pos).cloned() else {
            return;
        };
        self.load_gen += 1;
        self.switching = true;
        let gen = self.load_gen;
        self.player.stop();
        self.current = Some(t.clone());
        self.cover_file = None;
        self.last_playing = true;
        self.lyrics.clear();
        self.active_lyric = -1;
        if let Some(ui) = self.ui() {
            ui.set_switching(true);
            ui.set_playing(true);
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

        // Now start the audio.
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
                    false,
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
                    false,
                );
            }
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
            m.set_state(true, 0.0);
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
                    "♪".into()
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
            if self.current.is_none() {
                if let Some(ui) = self.ui() {
                    let sel = ui.get_selected_row();
                    self.play_row(if sel >= 0 { sel as usize } else { 0 });
                }
            } else {
                self.play_current();
            }
            return;
        }
        if self.player.is_paused() {
            self.player.play();
        } else {
            self.player.pause();
        }
        self.sync_state(true);
    }

    fn next(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if self.queue_pos + 1 < self.queue.len() {
            self.queue_pos += 1;
            self.play_current();
        } else {
            self.player.stop();
            self.sync_state(true);
        }
    }

    fn prev(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if self.player.position() > 3.0 || self.queue_pos == 0 {
            self.player.seek(0.0);
            self.presence_dirty.get_or_insert_with(Instant::now);
            return;
        }
        self.queue_pos -= 1;
        self.play_current();
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
            PlayerEvent::Ended => self.next(),
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
        let Some(t) = self.current.as_ref().filter(|_| active) else {
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
    let ui = MainWindow::new()?;
    let pw = PlayerWindow::new()?;
    let cfg = config::load();

    let player = Player::new(|ev| post(move |app| app.on_player_event(ev)));
    player.set_volume(cfg.volume);
    let discord = discord::Discord::new(cfg.discord_client_id.clone());

    ui.set_volume(cfg.volume);
    ui.set_server_url(cfg.server.as_str().into());
    ui.set_public_url(cfg.public_url.as_str().into());
    ui.set_discord_enabled(cfg.discord);
    ui.global::<Palette>().set_light(cfg.light);
    pw.global::<Palette>().set_light(cfg.light);
    ui.set_pinned(cfg.pin_main);
    pw.set_pinned(cfg.pin_player);

    let app = App {
        ui: ui.as_weak(),
        pw: pw.as_weak(),
        pw_pos: None,
        pw_shown: false,
        load_gen: 0,
        switching: false,
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
        app.push_folders();
        app.rebuild_sources();
        app.update_status();
        app.start_local_scan();
        app.connect_server();
        app.refresh_discord_status();
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
        ui.on_win_close(|| {
            let _ = slint::quit_event_loop();
        });
        // Alt+F4 on the main window closes the whole app, even with the player detached.
        ui.window().on_close_requested(|| {
            let _ = slint::quit_event_loop();
            slint::CloseRequestResponse::HideWindow
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
            if a.cfg.detached {
                a.set_detached(true);
            }
        });
    });

    slint::run_event_loop()?;

    with_app(|a| {
        a.discord.set_activity(None);
        config::save(&a.cfg);
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
