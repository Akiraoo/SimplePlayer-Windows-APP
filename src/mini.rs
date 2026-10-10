//! Mini player: `SimplePlayer.exe --mini <files…>` (double-clicked audio files, or files
//! dropped on the main window). A separate program run with its own window and a one-off
//! play list: it never touches the main player's queue, session or settings, has no tray
//! and quits when closed. Only one mini player runs; opening more files hands them over.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::library::{self, Track};
use crate::player::{self, Player, PlayerEvent};
use crate::{config, ffdec, i18n, single, Lang, LyricLine, MiniRow, MiniWindow, Palette};

/// Window and volume of the mini player (its own file, so it never overwrites the main
/// player's config.json).
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct MiniState {
    volume: f32,
    geom: Option<[i32; 4]>,
    show_list: bool,
    /// height of the window with the list open (physical px)
    list_height: u32,
    pinned: bool,
    /// the panel shows the play list (else the lyrics)
    list_mode: bool,
}

impl Default for MiniState {
    fn default() -> Self {
        MiniState {
            volume: 0.8,
            geom: None,
            show_list: true,
            list_height: 0,
            pinned: false,
            list_mode: false,
        }
    }
}

/// Window height with the panel closed: title bar 34 + song 104 + progress 24 + buttons 54
/// (logical px; keep in step with ui/mini.slint).
const COMPACT_H: f32 = 218.0;

fn state_path() -> PathBuf {
    config::cache_dir().join("mini.json")
}

/// Files waiting for the mini player: one small file per launch, then a named event.
fn inbox() -> PathBuf {
    config::cache_dir().join("mini-inbox")
}

fn post_request(paths: &[PathBuf]) {
    let dir = inbox();
    let _ = std::fs::create_dir_all(&dir);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let body: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    let _ = std::fs::write(
        dir.join(format!("{stamp}-{}.txt", std::process::id())),
        body.join("\n"),
    );
}

/// Takes all pending requests (recent ones only; leftovers from a crash are dropped).
fn take_requests() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(inbox())
        .map(|it| it.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let fresh = std::fs::metadata(&f)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|age| age < Duration::from_secs(30))
            .unwrap_or(false);
        if fresh {
            if let Ok(text) = std::fs::read_to_string(&f) {
                out.extend(text.lines().filter(|l| !l.trim().is_empty()).map(PathBuf::from));
            }
        }
        let _ = std::fs::remove_file(&f);
    }
    out
}

/* ---------------- building the play list ---------------- */

fn audio_in_dir(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|it| {
            it.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file() && library::audio_ext(p).is_some())
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| name_cmp(a, b));
    v
}

fn name_cmp(a: &Path, b: &Path) -> std::cmp::Ordering {
    let n = |p: &Path| p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    library::natural_cmp(&n(a), &n(b))
}

fn starts_with_number(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.trim_start().chars().next())
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
}

/// One opened file → the songs that belong with it in its folder: the same album (by tag,
/// in disc/track order), else files numbered like "01. …", else just that file.
fn related(file: &Path) -> Vec<Track> {
    let Some((me, _, _)) = library::track_from_path(file) else {
        return Vec::new();
    };
    let Some(dir) = file.parent() else {
        return vec![me];
    };
    let siblings = audio_in_dir(dir);
    if siblings.len() <= 1 || siblings.len() > 500 {
        return vec![me];
    }
    if !me.album.trim().is_empty() {
        let mut same: Vec<(Track, u32, u32)> = siblings
            .iter()
            .filter_map(|p| library::track_from_path(p))
            .filter(|(t, _, _)| t.album == me.album)
            .collect();
        if same.len() > 1 {
            same.sort_by(|a, b| {
                (a.1, a.2)
                    .cmp(&(b.1, b.2))
                    .then_with(|| name_cmp(a.0.path.as_deref().unwrap(), b.0.path.as_deref().unwrap()))
            });
            return same.into_iter().map(|x| x.0).collect();
        }
    }
    if starts_with_number(file) {
        let numbered: Vec<Track> = siblings
            .iter()
            .filter(|p| starts_with_number(p))
            .filter_map(|p| library::track_from_path(p).map(|x| x.0))
            .collect();
        if numbered.len() > 1 {
            return numbered;
        }
    }
    vec![me]
}

/// The play list for what was opened, and where to start in it.
fn build_list(paths: &[PathBuf]) -> (Vec<Track>, usize) {
    let files: Vec<&PathBuf> = paths.iter().filter(|p| p.is_file()).collect();
    if paths.len() == 1 && files.len() == 1 {
        let list = related(files[0]);
        let key = format!("local:{}", files[0].display());
        let start = list.iter().position(|t| t.key == key).unwrap_or(0);
        return (list, start);
    }
    // several files and/or folders: exactly those (folders: their audio files)
    let mut all: Vec<PathBuf> = Vec::new();
    for p in paths {
        if p.is_dir() {
            all.extend(audio_in_dir(p));
        } else if library::audio_ext(p).is_some() {
            all.push(p.clone());
        }
    }
    let list = all
        .iter()
        .filter_map(|p| library::track_from_path(p).map(|x| x.0))
        .collect();
    (list, 0)
}

/* ---------------- the running mini player ---------------- */

struct Mini {
    ui: slint::Weak<MiniWindow>,
    player: Player,
    list: Vec<Track>,
    pos: usize,
    /// changes with every new list; part of the preload token
    list_gen: u64,
    preload_dirty: bool,
    info_gen: u64,
    load_gen: u64,
    lyrics: Vec<(Option<f64>, String)>,
    active_lyric: i32,
    state: MiniState,
    last_open: Instant,
    dropped: Vec<PathBuf>,
    dropped_at: Option<Instant>,
    instance: single::Instance,
    status_until: Option<Instant>,
}

thread_local! {
    static MINI: RefCell<Option<Mini>> = const { RefCell::new(None) };
}

fn with_mini<R>(f: impl FnOnce(&mut Mini) -> R) -> Option<R> {
    MINI.with(|m| m.borrow_mut().as_mut().map(f))
}

fn post(f: impl FnOnce(&mut Mini) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        with_mini(f);
    });
}

impl Mini {
    fn ui(&self) -> Option<MiniWindow> {
        self.ui.upgrade()
    }

    /// New files: replace the list, or add to it when they arrive right after the last
    /// ones (Explorer starts one copy per selected file).
    fn open(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let append = self.last_open.elapsed() < Duration::from_millis(1500) && !self.list.is_empty();
        self.last_open = Instant::now();
        if append {
            let cur = self.list.get(self.pos).map(|t| t.key.clone());
            for p in &paths {
                if let Some((t, _, _)) = library::track_from_path(p) {
                    if !self.list.iter().any(|x| x.key == t.key) {
                        self.list.push(t);
                    }
                }
            }
            self.list.sort_by(|a, b| match (&a.path, &b.path) {
                (Some(x), Some(y)) => x
                    .parent()
                    .cmp(&y.parent())
                    .then_with(|| name_cmp(x, y)),
                _ => std::cmp::Ordering::Equal,
            });
            self.pos = cur
                .and_then(|k| self.list.iter().position(|t| t.key == k))
                .unwrap_or(0);
            self.list_gen += 1;
            self.preload_dirty = true;
            self.push_rows();
            return;
        }
        let (list, start) = build_list(&paths);
        if list.is_empty() {
            self.flash(crate::tr!("沒有可以播放的音訊檔", "No playable audio files"));
            return;
        }
        self.list = list;
        self.list_gen += 1;
        self.play(start);
    }

    fn item(t: &Track) -> Option<player::Item> {
        let path = t.path.clone()?;
        let alt = Some(ffdec::Input::Path(path.clone()));
        Some(player::Item {
            open: Box::new(move || {
                std::fs::File::open(&path)
                    .map(|f| Box::new(f) as Box<dyn symphonia::core::io::MediaSource>)
                    .map_err(|e| crate::tr!("無法開啟檔案：{}", "Can't open the file: {}", e))
            }),
            ext: Some(t.ext.clone()),
            alt,
            duration_hint: t.duration_ms as f64 / 1000.0,
        })
    }

    fn play(&mut self, i: usize) {
        let Some(t) = self.list.get(i).cloned() else {
            return;
        };
        self.pos = i;
        if let Some(item) = Self::item(&t) {
            self.player.open(item, false);
        }
        self.show_track(&t);
    }

    /// Title, cover and lyrics of `t` (the audio is already handled).
    fn show_track(&mut self, t: &Track) {
        self.preload_dirty = true;
        self.info_gen = 0;
        self.load_gen += 1;
        self.lyrics.clear();
        self.active_lyric = -1;
        if let Some(ui) = self.ui() {
            ui.set_now_title(t.title.as_str().into());
            ui.set_now_artist(
                if t.artist.is_empty() { t.album.as_str() } else { t.artist.as_str() }.into(),
            );
            ui.set_now_detail(t.ext.to_uppercase().into());
            ui.set_lyrics(ModelRc::new(VecModel::from(Vec::<LyricLine>::new())));
            ui.set_active_lyric(-1);
            ui.set_progress(0.0);
            ui.set_position_text("0:00".into());
            ui.set_duration_text(
                if t.duration_ms > 0 {
                    crate::fmt_time(t.duration_ms as f64 / 1000.0)
                } else {
                    "0:00".into()
                }
                .into(),
            );
        }
        self.push_rows();
        if let Some(ui) = self.ui() {
            ui.invoke_scroll_to_row(self.pos as i32);
        }
        let (t, gen) = (t.clone(), self.load_gen);
        std::thread::spawn(move || {
            let cover = library::local_cover(&t);
            let lyrics = library::local_lyrics(&t);
            post(move |m| {
                if m.load_gen != gen {
                    return;
                }
                if let Some(ui) = m.ui() {
                    let rows: Vec<LyricLine> = lyrics
                        .iter()
                        .map(|(t, text)| LyricLine {
                            text: if text.trim().is_empty() { "…".into() } else { text.as_str().into() },
                            timed: t.is_some(),
                        })
                        .collect();
                    ui.set_lyrics(ModelRc::new(VecModel::from(rows)));
                }
                m.lyrics = lyrics;
                if let Some(ui) = m.ui() {
                    let img = cover.and_then(|f| slint::Image::load_from_path(&f).ok());
                    ui.set_has_cover(img.is_some());
                    if let Some(img) = img {
                        ui.set_cover_image(img);
                    }
                }
            });
        });
    }

    fn push_rows(&self) {
        let Some(ui) = self.ui() else { return };
        let rows: Vec<MiniRow> = self
            .list
            .iter()
            .enumerate()
            .map(|(i, t)| MiniRow {
                title: t.title.as_str().into(),
                sub: if t.artist.is_empty() { t.album.as_str() } else { t.artist.as_str() }.into(),
                time: if t.duration_ms > 0 {
                    crate::fmt_time(t.duration_ms as f64 / 1000.0).into()
                } else {
                    SharedString::new()
                },
                playing: i == self.pos,
            })
            .collect();
        let total: u64 = self.list.iter().map(|t| t.duration_ms).sum();
        ui.set_list_info(
            crate::tr!(
                "{} 首 · {}",
                "{} songs · {}",
                self.list.len(),
                crate::fmt_time(total as f64 / 1000.0)
            )
            .into(),
        );
        ui.set_rows(ModelRc::new(VecModel::from(rows)));
    }

    fn mark_row(&self) {
        let Some(ui) = self.ui() else { return };
        let model = ui.get_rows();
        for i in 0..model.row_count() {
            if let Some(mut r) = model.row_data(i) {
                let on = i == self.pos;
                if r.playing != on {
                    r.playing = on;
                    model.set_row_data(i, r);
                }
            }
        }
    }

    fn flash(&mut self, msg: String) {
        if let Some(ui) = self.ui() {
            ui.set_status(msg.into());
        }
        self.status_until = Some(Instant::now() + Duration::from_secs(4));
    }

    fn next(&mut self) {
        if self.pos + 1 < self.list.len() {
            self.play(self.pos + 1);
        }
    }

    fn prev(&mut self) {
        if self.player.position() > 3.0 || self.pos == 0 {
            self.player.seek(0.0);
        } else {
            self.play(self.pos - 1);
        }
    }

    fn toggle_play(&mut self) {
        if !self.player.is_active() {
            let p = self.pos;
            self.play(p);
        } else if self.player.is_paused() {
            self.player.play();
        } else {
            self.player.pause();
        }
    }

    fn on_event(&mut self, ev: PlayerEvent) {
        match ev {
            PlayerEvent::Ended => {
                if self.pos + 1 < self.list.len() {
                    self.play(self.pos + 1);
                }
            }
            PlayerEvent::Advanced(token) => {
                let (gen, idx) = (token >> 32, (token & 0xffff_ffff) as usize);
                if gen == self.list_gen {
                    if let Some(t) = self.list.get(idx).cloned() {
                        self.pos = idx;
                        self.show_track(&t);
                    }
                }
            }
            PlayerEvent::Error(e) => {
                self.flash(e);
                // skip what can't be played
                if self.pos + 1 < self.list.len() {
                    let next = self.pos + 1;
                    slint::Timer::single_shot(Duration::from_millis(800), move || {
                        with_mini(|m| {
                            if m.pos + 1 == next {
                                m.play(next);
                            }
                        });
                    });
                }
            }
        }
    }

    fn tick(&mut self) {
        if self.instance.show_requested() {
            let paths = take_requests();
            self.open(paths);
            if let Some(ui) = self.ui() {
                crate::bring_to_front(ui.window());
            }
        }
        if let Some(at) = self.dropped_at {
            if at.elapsed() > Duration::from_millis(200) {
                self.dropped_at = None;
                let paths = std::mem::take(&mut self.dropped);
                self.last_open = Instant::now() - Duration::from_secs(10); // a drop replaces
                self.open(paths);
            }
        }
        if self.status_until.is_some_and(|t| Instant::now() > t) {
            self.status_until = None;
            if let Some(ui) = self.ui() {
                ui.set_status(SharedString::new());
            }
        }
        let Some(ui) = self.ui() else { return };
        let pos = self.player.position();
        let dur = self.player.duration();
        ui.set_playing(self.player.is_active() && !self.player.is_paused());
        if self.player.is_active() {
            ui.set_progress(if dur > 0.0 { (pos / dur) as f32 } else { 0.0 });
            ui.set_position_text(crate::fmt_time(pos).into());
            if dur > 0.0 {
                ui.set_duration_text(crate::fmt_time(dur).into());
            }
        }
        let (gen, info) = self.player.stream_info();
        if gen != self.info_gen {
            self.info_gen = gen;
            if let (Some(info), Some(t)) = (info, self.list.get(self.pos)) {
                let badge = crate::format_badge(&info, t);
                // the source ("本地") says nothing in the mini player
                let badge = match badge.rsplit_once("  ·  ") {
                    Some((a, _)) => a.to_string(),
                    None => badge.clone(),
                };
                ui.set_now_detail(badge.into());
            }
            self.mark_row();
        }
        let i = crate::active_lyric(&self.lyrics, pos + 0.15);
        if i != self.active_lyric {
            self.active_lyric = i;
            ui.set_active_lyric(i);
        }
        if self.preload_dirty && self.player.is_active() {
            self.preload_dirty = false;
            let next = self.pos + 1;
            if let Some(item) = self.list.get(next).and_then(Self::item) {
                self.player.preload((self.list_gen << 32) | next as u64, item);
            }
        }
    }

    fn save_state(&mut self) {
        if let Some(ui) = self.ui() {
            let w = ui.window();
            let (p, sz) = (w.position(), w.size());
            if sz.width > 0 && sz.height > 0 && !w.is_minimized() {
                self.state.geom = Some([p.x, p.y, sz.width as i32, sz.height as i32]);
                if self.state.show_list && sz.height > ((COMPACT_H + 120.0) * w.scale_factor()) as u32 {
                    self.state.list_height = sz.height;
                }
            }
            self.state.volume = ui.get_volume();
            self.state.pinned = ui.get_pinned();
        }
        if let Ok(json) = serde_json::to_vec_pretty(&self.state) {
            let _ = std::fs::create_dir_all(config::cache_dir());
            let _ = std::fs::write(state_path(), json);
        }
    }

    /// List shown: back to the height it had; hidden: just the player part.
    fn set_show_list(&mut self, on: bool) {
        self.state.show_list = on;
        let Some(ui) = self.ui() else { return };
        let w = ui.window();
        let scale = w.scale_factor();
        let sz = w.size();
        let compact = (COMPACT_H * scale) as u32;
        // a remembered height is only useful if it really had room for the panel
        let roomy = |h: u32| h > compact + (120.0 * scale) as u32;
        if on {
            let h = if roomy(self.state.list_height) {
                self.state.list_height
            } else {
                (470.0 * scale) as u32
            };
            w.set_size(slint::PhysicalSize::new(sz.width, h));
        } else {
            if roomy(sz.height) {
                self.state.list_height = sz.height;
            }
            w.set_size(slint::PhysicalSize::new(sz.width, compact));
        }
    }
}

/// `SimplePlayer.exe --mini <files>`; returns when the mini player is closed.
pub fn run(paths: Vec<PathBuf>) -> Result<(), slint::PlatformError> {
    // Hand the files to a mini player that is already open, if there is one.
    post_request(&paths);
    let Some(instance) = single::acquire_named("SimplePlayer.Mini", "SimplePlayer.MiniOpen") else {
        return Ok(());
    };
    let first = take_requests();

    let cfg = config::load(); // read only: the mini player never writes config.json
    i18n::set_english(match cfg.lang.as_str() {
        "en" => true,
        "zh-TW" => false,
        _ => !i18n::system_is_chinese(),
    });
    let state: MiniState = std::fs::read(state_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();

    let ui = MiniWindow::new()?;
    ui.global::<Lang>().set_en(i18n::english());
    ui.global::<Palette>().set_light(cfg.light);
    crate::apply_accent(&ui.global::<Palette>(), &cfg.accent);
    ui.set_volume(state.volume);
    ui.set_show_list(state.show_list);
    ui.set_pinned(state.pinned);
    ui.set_list_mode(state.list_mode);

    // Always shared mode: the main player may hold the device in exclusive mode.
    let player = Player::new(
        |ev| post(move |m| m.on_event(ev)),
        (!cfg.output_device.is_empty()).then(|| cfg.output_device.clone()),
        false,
    );
    player.set_buffer_level(cfg.buffer_level as u32);
    player.set_replaygain(cfg.replaygain as u32);
    player.set_volume(state.volume);

    if let Some([x, y, w, h]) = state.geom.filter(|g| g[2] > 0 && g[3] > 0) {
        ui.window().set_size(slint::PhysicalSize::new(w as u32, h as u32));
        ui.window().set_position(slint::PhysicalPosition::new(x, y));
    }

    MINI.with(|m| {
        *m.borrow_mut() = Some(Mini {
            ui: ui.as_weak(),
            player,
            list: Vec::new(),
            pos: 0,
            list_gen: 0,
            preload_dirty: false,
            info_gen: 0,
            load_gen: 0,
            lyrics: Vec::new(),
            active_lyric: -1,
            state,
            last_open: Instant::now() - Duration::from_secs(10),
            dropped: Vec::new(),
            dropped_at: None,
            instance,
            status_until: None,
        })
    });

    ui.on_toggle_play(|| {
        with_mini(|m| m.toggle_play());
    });
    ui.on_prev(|| {
        with_mini(|m| m.prev());
    });
    ui.on_next(|| {
        with_mini(|m| m.next());
    });
    ui.on_seek(|f| {
        with_mini(|m| {
            let d = m.player.duration();
            if d > 0.0 {
                m.player.seek(d * f as f64);
            }
        });
    });
    ui.on_seek_by(|delta| {
        with_mini(|m| {
            let d = m.player.duration();
            let mut p = m.player.position() + delta as f64;
            if d > 0.0 {
                p = p.min(d - 0.5);
            }
            m.player.seek(p.max(0.0));
        });
    });
    ui.on_volume_changed(|v| {
        with_mini(|m| m.player.set_volume(v));
    });
    ui.on_play_row(|i| {
        with_mini(|m| m.play(i.max(0) as usize));
    });
    ui.on_lyric_clicked(|i| {
        with_mini(|m| {
            if let Some((Some(t), _)) = m.lyrics.get(i.max(0) as usize) {
                m.player.seek(*t);
            }
        });
    });
    ui.on_list_mode_toggled(|on| {
        with_mini(|m| {
            m.state.list_mode = on;
            if on {
                if let Some(ui) = m.ui() {
                    ui.invoke_scroll_to_row(m.pos as i32);
                }
            }
        });
    });
    ui.on_list_toggled(|on| {
        with_mini(|m| m.set_show_list(on));
    });
    ui.on_pin_toggled(|_| {});
    {
        let weak = ui.as_weak();
        ui.on_win_drag(move || {
            if let Some(ui) = weak.upgrade() {
                crate::drag_window(ui.window());
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_win_resize(move |d| {
            if let Some(ui) = weak.upgrade() {
                crate::resize_window(ui.window(), d);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_win_minimize(move || {
            if let Some(ui) = weak.upgrade() {
                ui.window().set_minimized(true);
            }
        });
    }
    ui.on_win_close(|| {
        let _ = slint::quit_event_loop();
    });
    ui.window().on_close_requested(|| {
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });

    // Files dropped on the mini player replace its list.
    {
        use slint::winit_030::winit::event::WindowEvent;
        use slint::winit_030::{EventResult, WinitWindowAccessor};
        ui.window().on_winit_window_event(|_, ev| {
            if let WindowEvent::DroppedFile(p) = ev {
                let p = p.clone();
                with_mini(|m| {
                    m.dropped.push(p);
                    m.dropped_at = Some(Instant::now());
                });
            }
            EventResult::Propagate
        });
    }

    ui.show()?;
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(60), move || {
        if let Some(ui) = weak.upgrade() {
            if let Some(hwnd) = crate::window_hwnd(ui.window()) {
                crate::window_chrome(ui.window(), hwnd);
            }
        }
    });

    with_mini(|m| {
        if !m.state.show_list {
            m.set_show_list(false);
        }
        m.open(first);
    });

    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), || {
        with_mini(|m| m.tick());
    });

    slint::run_event_loop_until_quit()?;
    timer.stop();
    with_mini(|m| {
        m.player.stop();
        m.save_state();
    });
    // Leave without running destructors (audio threads, COM) in an odd order.
    std::process::exit(0);
}
