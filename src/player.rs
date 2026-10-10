//! Audio engine: Symphonia (or FFmpeg, for formats Symphonia can't read) decodes on a
//! worker thread into a buffer that the output (cpal shared mode, or WASAPI exclusive)
//! drains. The next song can be handed over early, so it follows without a gap.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::resample::Resampler;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

/// Buffer sizes for settings → 緩衝大小: (decoded audio kept ahead in seconds,
/// exclusive-mode device buffer in ms). Index = level 0 標準, 1 大, 2 超大.
/// The device buffer stays at 40 ms: bigger exclusive buffers made some drivers play in
/// stutters ("頓頓頓"), while the decode-ahead buffer is what rides out slow reads.
pub const BUFFER_LEVELS: [(f32, u32); 3] = [(0.6, 40), (2.0, 40), (4.0, 40)];

pub enum PlayerEvent {
    /// The current track played to the end (and nothing was preloaded after it).
    Ended,
    /// Playback moved on to the preloaded track with this token, without a gap.
    Advanced(u64),
    /// The track could not be opened or decoded.
    Error(String),
}

pub type Opener = Box<dyn FnOnce() -> Result<Box<dyn MediaSource>, String> + Send>;

/// A song to play.
pub struct Item {
    /// Opens the bytes for Symphonia (file or HTTP stream).
    pub open: Opener,
    /// File extension, a hint for the format.
    pub ext: Option<String>,
    /// Where FFmpeg can read the song when Symphonia can't decode it.
    pub alt: Option<crate::ffdec::Input>,
    /// Length from the library (used when the decoder can't tell), seconds.
    pub duration_hint: f64,
}

/// What the song itself is (for the now-playing badge): codec and format as stored.
#[derive(Clone, Debug, Default)]
pub struct StreamInfo {
    /// decoder name, e.g. "flac", "eac3", "pcm_s16le", "dsd_lsbf_planar"
    pub codec: String,
    pub lossy: bool,
    /// bit depth of the stored samples (None for lossy / float formats)
    pub bits: Option<u32>,
    pub rate: u32,
    pub channels: u32,
    pub kbps: Option<u32>,
}

enum Cmd {
    Open { item: Item, start_paused: bool },
    /// The song to continue with when the current one ends (replaces an earlier one).
    Next { token: u64, item: Item },
    Seek(f64),
    Stop,
}

struct Shared {
    queue: Mutex<VecDeque<f32>>, // interleaved stereo at the output rate
    paused: AtomicBool,
    volume: AtomicU32,  // f32 bits
    played: AtomicU64,  // output frames played since the last load/seek
    base_ms: AtomicU64, // track position at the last load/seek
    duration_ms: AtomicU64,
    decoding_done: AtomicBool,
    active: AtomicBool, // a track is loaded
    out_rate: AtomicU32,
    /// Exclusive mode: the output follows the song's own format.
    exclusive: AtomicBool,
    want_rate: AtomicU32, // sample rate of the current song (0 = none)
    want_bits: AtomicU32, // bit depth of the current song
    excl_ack: AtomicU32,  // song rate the exclusive output has (re)opened for
    /// Diagnostics: output callbacks that found the buffer empty mid-song (= audible gaps).
    underruns: AtomicU64,
    /// Index into BUFFER_LEVELS.
    buffer_level: AtomicU32,
    /// Exclusive mode: device wake-ups that came too late (= gaps the underrun count misses).
    late: AtomicU64,
    /// What the output was opened as ("獨佔 · 44.1 kHz · 16-bit" / "共享 · 48 kHz"), written by
    /// the output thread so the UI can read it without asking that thread anything.
    desc: Mutex<String>,
    /// ReplayGain: 0 off, 1 track, 2 album.
    rg_mode: AtomicU32,
    /// The current samples are scaled by ReplayGain (so not bit-perfect).
    rg_active: AtomicBool,
    /// The song being heard; `info_gen` changes whenever it does.
    info: Mutex<Option<StreamInfo>>,
    info_gen: AtomicU64,
}

/// Commands for the audio-out thread (it owns the cpal stream).
enum OutCmd {
    /// Switch to the named output device (`None` = the system default); replies success.
    Use(Option<String>, Sender<bool>),
    /// WASAPI exclusive on/off; replies Ok(description) or why it could not be used.
    Exclusive(bool, Sender<Result<String, String>>),
    /// Buffer size changed: re-open an exclusive stream with the new device buffer.
    Rebuffer,
}

pub struct Player {
    tx: Sender<Cmd>,
    out_tx: Sender<OutCmd>,
    shared: Arc<Shared>,
}

impl Player {
    /// `device`: output device name, `None` = the system default.
    pub fn new(
        on_event: impl Fn(PlayerEvent) + Send + 'static,
        device: Option<String>,
        exclusive: bool,
    ) -> Player {
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            paused: AtomicBool::new(true),
            volume: AtomicU32::new(0.8f32.to_bits()),
            played: AtomicU64::new(0),
            base_ms: AtomicU64::new(0),
            duration_ms: AtomicU64::new(0),
            decoding_done: AtomicBool::new(false),
            active: AtomicBool::new(false),
            out_rate: AtomicU32::new(48_000),
            exclusive: AtomicBool::new(false),
            want_rate: AtomicU32::new(0),
            want_bits: AtomicU32::new(16),
            excl_ack: AtomicU32::new(0),
            underruns: AtomicU64::new(0),
            buffer_level: AtomicU32::new(1),
            late: AtomicU64::new(0),
            desc: Mutex::new(String::new()),
            rg_mode: AtomicU32::new(0),
            rg_active: AtomicBool::new(false),
            info: Mutex::new(None),
            info_gen: AtomicU64::new(0),
        });

        // The output stream lives on its own thread: WASAPI wants a COM apartment that
        // differs from the UI thread's, and cpal::Stream cannot move between threads.
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        let (out_tx, out_rx) = mpsc::channel::<OutCmd>();
        let out_shared = shared.clone();
        thread::Builder::new()
            .name("audio-out".into())
            .spawn(move || Output::run(out_shared, device, exclusive, out_rx, ready_tx))
            .expect("spawn audio thread");
        if !ready_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or(false)
        {
            eprintln!("no audio output device");
        }
        let (tx, rx) = mpsc::channel();
        let worker_shared = shared.clone();
        thread::Builder::new()
            .name("decoder".into())
            .spawn(move || decoder_thread(rx, worker_shared, on_event))
            .expect("spawn decoder thread");

        Player { tx, out_tx, shared }
    }

    /// Opens the source on the decoder thread (e.g. an HTTP stream), then plays it.
    pub fn open(&self, item: Item, start_paused: bool) {
        let _ = self.tx.send(Cmd::Open { item, start_paused });
    }

    /// The song after the current one: it is opened shortly before the current one ends and
    /// follows without a gap; `PlayerEvent::Advanced(token)` reports the switch.
    pub fn preload(&self, token: u64, item: Item) {
        let _ = self.tx.send(Cmd::Next { token, item });
    }

    /// ReplayGain mode: 0 off, 1 track, 2 album. Applies from the next decoded block.
    pub fn set_replaygain(&self, mode: u32) {
        self.shared.rg_mode.store(mode.min(2), Ordering::Relaxed);
    }

    pub fn play(&self) {
        if self.shared.active.load(Ordering::Relaxed) {
            self.shared.paused.store(false, Ordering::Relaxed);
        }
    }

    pub fn pause(&self) {
        self.shared.paused.store(true, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    pub fn is_active(&self) -> bool {
        self.shared.active.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn seek(&self, seconds: f64) {
        let _ = self.tx.send(Cmd::Seek(seconds.max(0.0)));
    }

    pub fn set_volume(&self, v: f32) {
        self.shared
            .volume
            .store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// Current position in seconds.
    pub fn position(&self) -> f64 {
        let rate = self.shared.out_rate.load(Ordering::Relaxed).max(1) as f64;
        let base = self.shared.base_ms.load(Ordering::Relaxed) as f64 / 1000.0;
        let played = self.shared.played.load(Ordering::Relaxed) as f64 / rate;
        let pos = base + played;
        let dur = self.duration();
        if dur > 0.0 {
            pos.min(dur)
        } else {
            pos
        }
    }

    /// Names of the output devices that can be picked.
    /// Listed on a throw-away thread, never on the audio thread: enumerating devices can take
    /// a while, and in exclusive mode the audio thread must not miss a single device request.
    pub fn output_devices(&self) -> Vec<String> {
        let (tx, rx) = mpsc::channel();
        let _ = thread::Builder::new()
            .name("list-devices".into())
            .spawn(move || {
                let _ = tx.send(device_names());
            });
        rx.recv_timeout(Duration::from_secs(3)).unwrap_or_default()
    }

    /// Moves playback to another output device (`None` = system default) and continues at
    /// the same position. Returns false when that device could not be opened (the system
    /// default is used instead).
    pub fn set_output_device(&self, name: Option<String>) -> bool {
        let pos = self.position();
        let (tx, rx) = mpsc::channel();
        if self.out_tx.send(OutCmd::Use(name, tx)).is_err() {
            return false;
        }
        let ok = rx.recv_timeout(Duration::from_secs(5)).unwrap_or(false);
        if self.is_active() {
            // drop what was buffered for the old device and carry on from here
            self.seek(pos);
        }
        ok
    }

    /// Turns WASAPI exclusive mode on or off. Ok = it is in the requested mode
    /// (with a short format description); Err = why exclusive mode could not be used.
    pub fn set_exclusive(&self, on: bool) -> Result<String, String> {
        let pos = self.position();
        let (tx, rx) = mpsc::channel();
        if self.out_tx.send(OutCmd::Exclusive(on, tx)).is_err() {
            return Err("audio thread gone".into());
        }
        let r = rx
            .recv_timeout(Duration::from_secs(8))
            .unwrap_or_else(|_| Err(crate::tr!("逾時", "Timed out")));
        if self.is_active() {
            self.seek(pos);
        }
        r
    }

    /// Settings → 緩衝大小 (0 標準, 1 大, 2 超大): how much audio is decoded ahead and how big
    /// the exclusive-mode device buffer is. Bigger = steadier on a busy PC, slower to react.
    pub fn set_buffer_level(&self, level: u32) {
        let pos = self.position();
        self.shared
            .buffer_level
            .store(level.min(2), Ordering::Relaxed);
        let _ = self.out_tx.send(OutCmd::Rebuffer);
        if self.shared.exclusive.load(Ordering::Relaxed) && self.is_active() {
            self.seek(pos);
        }
    }

    /// Short description of the current output ("共享" / "獨佔 · 96 kHz · 24-bit").
    /// Never blocks and never touches the audio thread (reads what it published).
    pub fn output_status(&self) -> String {
        let sh = &self.shared;
        let rate = sh.out_rate.load(Ordering::Relaxed).max(1) as f64;
        // try_lock: if the audio side holds the queue right now, just skip that number
        let buffered = sh
            .queue
            .try_lock()
            .map(|q| {
                let secs = format!("{:.2}", q.len() as f64 / 2.0 / rate);
                crate::tr!(" · 緩衝 {} 秒", " · buffer {} s", secs)
            })
            .unwrap_or_default();
        let detail = sh.desc.lock().map(|d| d.clone()).unwrap_or_default();
        let excl = sh.exclusive.load(Ordering::Relaxed);
        let desc = if detail.is_empty() {
            crate::tr!("沒有輸出", "No output")
        } else if excl {
            crate::tr!("獨佔 · {}", "Exclusive · {}", detail)
        } else {
            crate::tr!("共享 · {}", "Shared · {}", detail)
        };
        let mut s = desc + &buffered;
        s.push_str(&crate::tr!(
            " · 斷音 {} 次",
            " · dropouts {}",
            sh.underruns.load(Ordering::Relaxed)
        ));
        if excl {
            s.push_str(&crate::tr!(
                " · 延遲 {} 次",
                " · late {}",
                sh.late.load(Ordering::Relaxed)
            ));
        }
        s
    }

    /// The song being heard and a counter that changes with it (cheap to poll).
    pub fn stream_info(&self) -> (u64, Option<StreamInfo>) {
        let g = self.shared.info_gen.load(Ordering::Relaxed);
        (g, self.shared.info.lock().ok().and_then(|i| i.clone()))
    }

    /// Track length in seconds (0 when unknown).
    pub fn duration(&self) -> f64 {
        self.shared.duration_ms.load(Ordering::Relaxed) as f64 / 1000.0
    }
}

/* ---------------- output ---------------- */

fn device_names() -> Vec<String> {
    cpal::default_host()
        .output_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

/// Owns the audio output: a shared-mode cpal stream, or (Windows) a WASAPI exclusive stream
/// that is re-opened at each song's own sample rate. Re-opens when the device changes or
/// goes away (e.g. USB DAC unplugged), falling back to the system default / shared mode.
struct Output {
    shared: Arc<Shared>,
    want: Option<String>,
    exclusive: bool,
    failed: Arc<AtomicBool>,
    stream: Option<cpal::Stream>,
    #[cfg(windows)]
    excl: Option<crate::wasapi_out::Exclusive>,
    /// Song rate the exclusive stream was opened for (the device may have picked another).
    excl_for: u32,
}

/// Tells Windows this is an audio thread ("Pro Audio" scheduling class), so it keeps
/// getting CPU time on a busy system — fewer dropouts, especially in exclusive mode.
fn boost_audio_thread() {
    #[cfg(windows)]
    {
        #[link(name = "avrt")]
        extern "system" {
            fn AvSetMmThreadCharacteristicsW(
                task: *const u16,
                index: *mut u32,
            ) -> *mut std::ffi::c_void;
            fn AvSetMmThreadPriority(handle: *mut std::ffi::c_void, priority: i32) -> i32;
        }
        let name: Vec<u16> = "Pro Audio"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut index = 0u32;
        unsafe {
            let h = AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut index);
            if !h.is_null() {
                // AVRT_PRIORITY_CRITICAL: the same class foobar2000 / WASAPI samples use
                AvSetMmThreadPriority(h, 2);
            }
        }
    }
}

impl Output {
    fn run(
        shared: Arc<Shared>,
        want: Option<String>,
        exclusive: bool,
        rx: Receiver<OutCmd>,
        ready: Sender<bool>,
    ) {
        boost_audio_thread();
        let mut o = Output {
            shared,
            want,
            exclusive,
            failed: Arc::new(AtomicBool::new(false)),
            stream: None,
            #[cfg(windows)]
            excl: None,
            excl_for: 0,
        };
        let ok = o.open().is_ok();
        let _ = ready.send(ok || o.stream.is_some());
        loop {
            #[cfg(windows)]
            if o.excl.is_some() {
                // exclusive: keep feeding the device, look at commands in between
                match rx.try_recv() {
                    Ok(cmd) => o.command(cmd),
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
                o.pump_exclusive();
                continue;
            }
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(cmd) => o.command(cmd),
                Err(RecvTimeoutError::Timeout) => {
                    if o.failed.swap(false, Ordering::Relaxed) {
                        o.close();
                        thread::sleep(Duration::from_millis(300));
                        let _ = o.open();
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn close(&mut self) {
        self.stream = None;
        #[cfg(windows)]
        {
            self.excl = None;
        }
    }

    /// Opens the output for the current settings. Err = exclusive mode failed (shared mode
    /// is used instead) or nothing could be opened at all.
    fn open(&mut self) -> Result<String, String> {
        self.close();
        self.failed.store(false, Ordering::Relaxed);
        let mut excl_err = None;
        #[cfg(windows)]
        if self.exclusive {
            let want_rate = self.shared.want_rate.load(Ordering::Relaxed);
            self.excl_for = want_rate;
            let rate = if want_rate == 0 { 44_100 } else { want_rate };
            let bits = self.shared.want_bits.load(Ordering::Relaxed);
            let level = (self.shared.buffer_level.load(Ordering::Relaxed) as usize).min(2);
            let res = crate::wasapi_out::Exclusive::open(
                self.want.as_deref(),
                rate,
                bits,
                BUFFER_LEVELS[level].1,
            );
            match res {
                Ok(x) => {
                    self.publish(x.describe());
                    self.shared.out_rate.store(x.rate, Ordering::Relaxed);
                    self.shared.exclusive.store(true, Ordering::Relaxed);
                    self.shared.excl_ack.store(want_rate, Ordering::Relaxed);
                    let d = x.describe();
                    self.excl = Some(x);
                    return Ok(d);
                }
                Err(e) => {
                    eprintln!("output: exclusive failed: {e}");
                    excl_err = Some(crate::tr!("無法使用獨佔模式：{}", "Exclusive mode unavailable: {}", e));
                    self.shared.excl_ack.store(want_rate, Ordering::Relaxed);
                }
            }
        }
        self.shared.exclusive.store(false, Ordering::Relaxed);
        let exact = self
            .want
            .as_deref()
            .and_then(|n| open_output(self.shared.clone(), Some(n), self.failed.clone()));
        self.stream = exact.or_else(|| open_output(self.shared.clone(), None, self.failed.clone()));
        self.publish(if self.stream.is_some() {
            format!(
                "{} kHz",
                self.shared.out_rate.load(Ordering::Relaxed) as f64 / 1000.0
            )
        } else {
            String::new()
        });
        match (excl_err, self.stream.is_some()) {
            (Some(e), _) => Err(e),
            (None, true) => Ok(crate::tr!("共享", "Shared")),
            (None, false) => Err(crate::tr!("找不到音訊輸出裝置", "No audio output device found")),
        }
    }

    fn command(&mut self, cmd: OutCmd) {
        match cmd {
            OutCmd::Use(name, reply) => {
                self.want = name.clone();
                let _ = self.open();
                // did we get the device that was asked for?
                let ok = match &name {
                    None => true,
                    Some(n) => device_names().iter().any(|d| d == n) && self.is_open(),
                };
                let _ = reply.send(ok);
            }
            OutCmd::Exclusive(on, reply) => {
                self.exclusive = on;
                let _ = reply.send(self.open());
            }
            OutCmd::Rebuffer =>
            {
                #[cfg(windows)]
                if self.excl.is_some() {
                    let _ = self.open();
                }
            }
        }
    }

    fn is_open(&self) -> bool {
        #[cfg(windows)]
        if self.excl.is_some() {
            return true;
        }
        self.stream.is_some()
    }

    fn publish(&self, desc: String) {
        if let Ok(mut d) = self.shared.desc.lock() {
            *d = desc;
        }
    }

    #[cfg(windows)]
    fn pump_exclusive(&mut self) {
        // a song with another sample rate / bit depth: re-open the device to match it
        let want = self.shared.want_rate.load(Ordering::Relaxed);
        if want != 0 && want != self.excl_for {
            if self.open().is_err() {
                // the device can't do it: open() fell back to shared mode
                self.exclusive_lost();
            }
            return;
        }
        let shared = self.shared.clone();
        let res = match self.excl.as_mut() {
            Some(x) => {
                x.unity = f32::from_bits(shared.volume.load(Ordering::Relaxed)) >= 1.0
                    && !shared.rg_active.load(Ordering::Relaxed);
                let before = x.late;
                let r = x.pump(|buf| fill(&shared, buf, 2, |v| v));
                if x.late != before {
                    shared.late.fetch_add(x.late - before, Ordering::Relaxed);
                }
                r
            }
            None => return,
        };
        if let Err(e) = res {
            eprintln!("exclusive output error: {e}");
            // device unplugged / taken: try again, else shared mode
            thread::sleep(Duration::from_millis(300));
            if self.open().is_err() {
                self.exclusive_lost();
            }
        }
    }

    #[cfg(windows)]
    fn exclusive_lost(&mut self) {
        // keep the user's choice; the next device change or toggle tries again
        self.shared.exclusive.store(false, Ordering::Relaxed);
    }
}

/// Opens `name` (or the system default) and starts pulling audio from the shared queue.
fn open_output(
    shared: Arc<Shared>,
    name: Option<&str>,
    failed: Arc<AtomicBool>,
) -> Option<cpal::Stream> {
    let host = cpal::default_host();
    let device = match name {
        Some(n) => host
            .output_devices()
            .ok()?
            .find(|d| d.name().ok().as_deref() == Some(n))?,
        None => host.default_output_device()?,
    };
    let config = device.default_output_config().ok()?;
    let channels = config.channels() as usize;
    shared
        .out_rate
        .store(config.sample_rate().0, Ordering::Relaxed);
    let stream_config: cpal::StreamConfig = config.clone().into();
    // device lost / format changed: the output thread notices and re-opens
    let err_fn = move |e: cpal::StreamError| {
        eprintln!("audio output error: {e}");
        failed.store(true, Ordering::Relaxed);
    };

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let s = shared.clone();
            device.build_output_stream(
                &stream_config,
                move |data: &mut [f32], _| fill(&s, data, channels, |v| v),
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let s = shared.clone();
            device.build_output_stream(
                &stream_config,
                move |data: &mut [i16], _| {
                    fill(&s, data, channels, |v| (v * i16::MAX as f32) as i16)
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let s = shared.clone();
            device.build_output_stream(
                &stream_config,
                move |data: &mut [u16], _| {
                    fill(&s, data, channels, |v| {
                        ((v * 0.5 + 0.5) * u16::MAX as f32) as u16
                    })
                },
                err_fn,
                None,
            )
        }
        _ => return None,
    }
    .ok()?;
    stream.play().ok()?;
    Some(stream)
}

fn fill<T: Copy>(shared: &Shared, out: &mut [T], channels: usize, conv: impl Fn(f32) -> T) {
    let silence = conv(0.0);
    if shared.paused.load(Ordering::Relaxed) {
        out.iter_mut().for_each(|s| *s = silence);
        return;
    }
    let vol = f32::from_bits(shared.volume.load(Ordering::Relaxed));
    let vol = vol * vol; // perceptual-ish curve
    let mut q = match shared.queue.lock() {
        Ok(q) => q,
        Err(_) => return,
    };
    let mut frames = 0u64;
    for frame in out.chunks_mut(channels) {
        if q.len() < 2 {
            frame.iter_mut().for_each(|s| *s = silence);
            continue;
        }
        let l = q.pop_front().unwrap_or(0.0) * vol;
        let r = q.pop_front().unwrap_or(0.0) * vol;
        frames += 1;
        for (c, s) in frame.iter_mut().enumerate() {
            *s = match c {
                0 => conv(l),
                1 => conv(r),
                _ => silence,
            };
        }
    }
    shared.played.fetch_add(frames, Ordering::Relaxed);
    let wanted = (out.len() / channels.max(1)) as u64;
    if frames < wanted
        && shared.active.load(Ordering::Relaxed)
        && !shared.decoding_done.load(Ordering::Relaxed)
    {
        shared.underruns.fetch_add(1, Ordering::Relaxed);
    }
}

/* ---------------- decoder ---------------- */

/// Where the samples of one song come from.
enum Source {
    Sym {
        format: Box<dyn FormatReader>,
        decoder: Box<dyn Decoder>,
        track_id: u32,
    },
    Ff(crate::ffdec::FfSource),
}

struct Track {
    src: Source,
    resampler: Resampler,
    src_rate: u32,
    out_rate: u32,
    bits: u32,
    duration: f64,
    gains: Gains,
    info: StreamInfo,
}

fn publish_info(shared: &Shared, info: Option<StreamInfo>) {
    if let Ok(mut i) = shared.info.lock() {
        *i = info;
    }
    shared.info_gen.fetch_add(1, Ordering::Relaxed);
}

impl Track {
    /// Next block of stereo samples at the source rate; None = the song is over.
    fn next_block(&mut self) -> Option<Vec<f32>> {
        match &mut self.src {
            Source::Ff(f) => f.next(),
            Source::Sym {
                format,
                decoder,
                track_id,
            } => loop {
                let packet = match format.next_packet() {
                    Ok(p) => p,
                    Err(SymError::IoError(e))
                        if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                    {
                        return None
                    }
                    Err(SymError::ResetRequired) => {
                        decoder.reset();
                        continue;
                    }
                    Err(e) => {
                        eprintln!("read error: {e}");
                        return None;
                    }
                };
                if packet.track_id() != *track_id {
                    continue;
                }
                let decoded = match decoder.decode(&packet) {
                    Ok(d) => d,
                    Err(SymError::DecodeError(_)) => continue, // skip a corrupt packet
                    Err(e) => {
                        eprintln!("decode error: {e}");
                        return None;
                    }
                };
                let spec = *decoded.spec();
                let channels = spec.channels.count().max(1);
                let mut sb = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                sb.copy_interleaved_ref(decoded);
                return Some(to_stereo(sb.samples(), channels));
            },
        }
    }

    fn seek(&mut self, secs: f64) -> bool {
        let ok = match &mut self.src {
            Source::Ff(f) => f.seek(secs),
            Source::Sym {
                format,
                decoder,
                track_id,
            } => {
                let to = Time::new(secs.trunc() as u64, secs.fract());
                let ok = format
                    .seek(
                        SeekMode::Accurate,
                        SeekTo::Time {
                            time: to,
                            track_id: Some(*track_id),
                        },
                    )
                    .is_ok();
                if ok {
                    decoder.reset();
                }
                ok
            }
        };
        if ok {
            self.resampler.reset();
        }
        ok
    }

    fn set_out_rate(&mut self, rate: u32) {
        if rate != self.out_rate {
            self.resampler = Resampler::new(self.src_rate, rate);
            self.out_rate = rate;
        }
    }
}

use crate::ffdec::Gains;

fn read_gains(g: &mut Gains, tags: &[symphonia::core::meta::Tag]) {
    use symphonia::core::meta::StandardTagKey as K;
    for t in tags {
        let value = t.value.to_string();
        let key = match t.std_key {
            Some(K::ReplayGainTrackGain) => "REPLAYGAIN_TRACK_GAIN",
            Some(K::ReplayGainTrackPeak) => "REPLAYGAIN_TRACK_PEAK",
            Some(K::ReplayGainAlbumGain) => "REPLAYGAIN_ALBUM_GAIN",
            Some(K::ReplayGainAlbumPeak) => "REPLAYGAIN_ALBUM_PEAK",
            // ID3 user text frames come as "TXXX:replaygain_track_gain"
            _ => t.key.rsplit(':').next().unwrap_or(&t.key),
        };
        g.read(key, &value);
    }
}

fn open_symphonia(
    src: Box<dyn MediaSource>,
    ext: Option<&str>,
) -> Result<(Source, u32, u32, f64, Gains, StreamInfo), String> {
    let mss = MediaSourceStream::new(src, Default::default());
    let mut hint = Hint::new();
    if let Some(e) = ext {
        hint.with_extension(e.trim_start_matches('.'));
    }
    let mut probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|e| crate::tr!("無法辨識音訊格式：{}", "Unrecognised audio format: {}", e))?;
    let mut gains = Gains::default();
    if let Some(m) = probed.metadata.get() {
        if let Some(rev) = m.current() {
            read_gains(&mut gains, rev.tags());
        }
    }
    let mut format = probed.format;
    if let Some(rev) = format.metadata().current() {
        read_gains(&mut gains, rev.tags());
    }
    let (track_id, params) = {
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| crate::tr!("找不到音軌", "No audio track found"))?;
        (track.id, track.codec_params.clone())
    };
    let decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| crate::tr!("不支援的編碼：{}", "Unsupported codec: {}", e))?;
    let src_rate = params.sample_rate.unwrap_or(44_100);
    let duration = match (params.n_frames, params.time_base) {
        (Some(n), Some(tb)) => {
            let t = tb.calc_time(n);
            t.seconds as f64 + t.frac
        }
        (Some(n), None) => n as f64 / src_rate as f64,
        _ => 0.0,
    };
    let bits = params.bits_per_sample.unwrap_or(16);
    let codec = symphonia::default::get_codecs()
        .get_codec(params.codec)
        .map(|d| d.short_name.to_string())
        .unwrap_or_default();
    let lossy = matches!(codec.as_str(), "mp1" | "mp2" | "mp3" | "aac" | "vorbis" | "opus");
    let info = StreamInfo {
        lossy,
        bits: if lossy { None } else { params.bits_per_sample },
        rate: src_rate,
        channels: params.channels.map(|c| c.count() as u32).unwrap_or(2),
        kbps: None,
        codec,
    };
    Ok((
        Source::Sym {
            format,
            decoder,
            track_id,
        },
        src_rate,
        bits,
        duration,
        gains,
        info,
    ))
}

/// Opens a song: Symphonia first, FFmpeg when Symphonia can't read the format.
fn open_item(item: Item, out_rate: u32) -> Result<Track, String> {
    let Item {
        open,
        ext,
        alt,
        duration_hint,
    } = item;
    let bytes = open()?;
    let (src, src_rate, bits, duration, gains, info) = match open_symphonia(bytes, ext.as_deref()) {
        Ok(x) => x,
        Err(e) => match alt {
            Some(input) => {
                let f = crate::ffdec::FfSource::open(input)?;
                let i = &f.info;
                let info = StreamInfo {
                    codec: i.codec.clone(),
                    lossy: i.lossy,
                    bits: if i.lossy { None } else { i.src_bits },
                    rate: i.rate,
                    channels: i.channels.max(1),
                    kbps: i.kbps,
                };
                let (rate, bits, dur, gains) = (f.rate, i.bits, i.duration, i.gains);
                (Source::Ff(f), rate, bits, dur, gains, info)
            }
            None => return Err(e),
        },
    };
    Ok(Track {
        src,
        resampler: Resampler::new(src_rate, out_rate),
        src_rate,
        out_rate,
        bits,
        duration: if duration > 0.0 { duration } else { duration_hint },
        gains,
        info,
    })
}

/// The song to continue with.
enum NextSong {
    Waiting(Item),
    /// Already opened (taken back after a seek during the handover).
    Ready(Track),
}

struct Next {
    token: u64,
    song: NextSong,
}

/// The next song is being decoded behind the current one; playback reaches it after `at`
/// output frames (counted like `Shared::played`).
struct Handover {
    token: u64,
    at: u64,
    /// The song still playing out (restored if the user seeks in it).
    prev: Track,
    /// Exclusive mode at another sample rate: let the old song finish, re-open the device,
    /// then start the new one (a short pause is unavoidable there).
    reopen: bool,
}

#[derive(Default)]
struct State {
    cur: Option<Track>,
    next: Option<Next>,
    hand: Option<Handover>,
    ended_sent: bool,
}

fn decoder_thread(rx: Receiver<Cmd>, shared: Arc<Shared>, on_event: impl Fn(PlayerEvent)) {
    let mut st = State::default();
    loop {
        // Wait for a command when idle; otherwise just peek between blocks.
        let cmd = if st.cur.is_none() {
            match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            }
        } else {
            match rx.try_recv() {
                Ok(c) => Some(c),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        };
        if let Some(cmd) = cmd {
            handle(&shared, cmd, &mut st, &on_event);
            continue;
        }

        // Has playback reached the song that was handed over?
        let hand = st
            .hand
            .as_ref()
            .map(|h| (shared.played.load(Ordering::Relaxed) >= h.at, h.reopen));
        if let Some((reached, reopen)) = hand {
            if reached {
                finish_handover(&shared, &mut st, &on_event);
                continue;
            }
            if reopen {
                // the old song is playing out before the device switches rate
                thread::sleep(Duration::from_millis(10));
                continue;
            }
        }

        // Current song fully decoded: hand over to the next one, or wait for the end.
        if shared.decoding_done.load(Ordering::Relaxed) {
            if st.hand.is_none() && !st.ended_sent && st.next.is_some() && st.cur.is_some() {
                start_handover(&shared, &mut st);
                continue;
            }
            let empty = shared.queue.lock().map(|q| q.len() < 2).unwrap_or(true);
            if empty && !st.ended_sent {
                st.ended_sent = true;
                on_event(PlayerEvent::Ended);
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(c) => handle(&shared, c, &mut st, &on_event),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            continue;
        }

        let Some(t) = st.cur.as_mut() else { continue };

        // Keep the buffer topped up.
        let out_rate = shared.out_rate.load(Ordering::Relaxed);
        let level = (shared.buffer_level.load(Ordering::Relaxed) as usize).min(2);
        let limit = (out_rate as f32 * BUFFER_LEVELS[level].0) as usize * 2;
        if shared.queue.lock().map(|q| q.len()).unwrap_or(0) >= limit {
            thread::sleep(Duration::from_millis(10));
            continue;
        }

        let Some(mut block) = t.next_block() else {
            let tail = t.resampler.flush();
            if let Ok(mut q) = shared.queue.lock() {
                q.extend(tail);
            }
            shared.decoding_done.store(true, Ordering::Relaxed);
            continue;
        };
        let gain = t.gains.factor(shared.rg_mode.load(Ordering::Relaxed));
        shared.rg_active.store(gain != 1.0, Ordering::Relaxed);
        if gain != 1.0 {
            for s in block.iter_mut() {
                *s *= gain;
            }
        }
        // the output device may have changed (other sample rate)
        t.set_out_rate(out_rate);
        let out = t.resampler.process(&block);
        if let Ok(mut q) = shared.queue.lock() {
            q.extend(out);
        }
    }
}

/// Opens the next song and starts decoding it right behind the current one.
fn start_handover(shared: &Shared, st: &mut State) {
    let Some(Next { token, song }) = st.next.take() else {
        return;
    };
    let out_rate = shared.out_rate.load(Ordering::Relaxed);
    let mut nt = match song {
        NextSong::Ready(t) => t,
        NextSong::Waiting(item) => match open_item(item, out_rate) {
            Ok(t) => t,
            Err(e) => {
                // let the current song end normally; the app then tries it the usual way
                eprintln!("preload failed: {e}");
                return;
            }
        },
    };
    let queued = shared.queue.lock().map(|q| q.len() / 2).unwrap_or(0) as u64;
    let at = shared.played.load(Ordering::Relaxed) + queued;
    let reopen = shared.exclusive.load(Ordering::Relaxed)
        && nt.src_rate != shared.want_rate.load(Ordering::Relaxed);
    if !reopen {
        nt.set_out_rate(out_rate);
        shared.decoding_done.store(false, Ordering::Relaxed);
    }
    let Some(prev) = st.cur.replace(nt) else {
        return;
    };
    st.hand = Some(Handover {
        token,
        at,
        prev,
        reopen,
    });
}

/// Playback has reached the handed-over song: it becomes the current one.
fn finish_handover(shared: &Shared, st: &mut State, on_event: &impl Fn(PlayerEvent)) {
    let Some(h) = st.hand.take() else { return };
    drop(h.prev);
    let Some(t) = st.cur.as_mut() else { return };
    if h.reopen {
        clear(shared, 0);
        shared.want_bits.store(t.bits, Ordering::Relaxed);
        shared.want_rate.store(t.src_rate, Ordering::Relaxed);
        wait_exclusive(shared, t.src_rate);
        t.set_out_rate(shared.out_rate.load(Ordering::Relaxed));
        shared.decoding_done.store(false, Ordering::Relaxed);
    } else {
        // keep counting from where the new song starts
        let played = shared.played.load(Ordering::Relaxed);
        shared
            .played
            .fetch_sub(played.min(h.at), Ordering::Relaxed);
        shared.base_ms.store(0, Ordering::Relaxed);
        shared.want_bits.store(t.bits, Ordering::Relaxed);
        shared.want_rate.store(t.src_rate, Ordering::Relaxed);
    }
    shared
        .duration_ms
        .store((t.duration * 1000.0) as u64, Ordering::Relaxed);
    publish_info(shared, Some(t.info.clone()));
    st.ended_sent = false;
    on_event(PlayerEvent::Advanced(h.token));
}

/// Exclusive mode: give the output a moment to re-open the device at `rate`.
fn wait_exclusive(shared: &Shared, rate: u32) {
    if !shared.exclusive.load(Ordering::Relaxed) {
        return;
    }
    let until = std::time::Instant::now() + Duration::from_millis(1500);
    while shared.excl_ack.load(Ordering::Relaxed) != rate
        && shared.exclusive.load(Ordering::Relaxed)
        && std::time::Instant::now() < until
    {
        thread::sleep(Duration::from_millis(10));
    }
}

fn handle(shared: &Arc<Shared>, cmd: Cmd, st: &mut State, on_event: &impl Fn(PlayerEvent)) {
    match cmd {
        Cmd::Open { item, start_paused } => {
            shared.paused.store(true, Ordering::Relaxed);
            clear(shared, 0);
            st.cur = None;
            st.next = None;
            st.hand = None;
            let out_rate = shared.out_rate.load(Ordering::Relaxed);
            match open_item(item, out_rate) {
                Ok(mut t) => {
                    shared.want_bits.store(t.bits, Ordering::Relaxed);
                    shared.want_rate.store(t.src_rate, Ordering::Relaxed);
                    // let the exclusive output switch to this song's rate first
                    wait_exclusive(shared, t.src_rate);
                    t.set_out_rate(shared.out_rate.load(Ordering::Relaxed));
                    shared
                        .duration_ms
                        .store((t.duration * 1000.0) as u64, Ordering::Relaxed);
                    shared.active.store(true, Ordering::Relaxed);
                    shared.decoding_done.store(false, Ordering::Relaxed);
                    shared.paused.store(start_paused, Ordering::Relaxed);
                    st.ended_sent = false;
                    publish_info(shared, Some(t.info.clone()));
                    st.cur = Some(t);
                }
                Err(e) => {
                    shared.active.store(false, Ordering::Relaxed);
                    publish_info(shared, None);
                    on_event(PlayerEvent::Error(e));
                }
            }
        }
        Cmd::Next { token, item } => {
            // once the handover has begun, that song stays the next one
            if st.hand.is_none() {
                st.next = Some(Next {
                    token,
                    song: NextSong::Waiting(item),
                });
            }
        }
        Cmd::Seek(secs) => {
            if let Some(h) = st.hand.take() {
                // still in the old song: take it back, keep the new one ready for later
                if let Some(mut upcoming) = st.cur.replace(h.prev) {
                    if upcoming.seek(0.0) {
                        st.next = Some(Next {
                            token: h.token,
                            song: NextSong::Ready(upcoming),
                        });
                    }
                }
            }
            if let Some(t) = st.cur.as_mut() {
                if t.seek(secs) {
                    clear(shared, (secs * 1000.0) as u64);
                    shared.decoding_done.store(false, Ordering::Relaxed);
                    st.ended_sent = false;
                }
            }
        }
        Cmd::Stop => {
            shared.paused.store(true, Ordering::Relaxed);
            shared.active.store(false, Ordering::Relaxed);
            clear(shared, 0);
            shared.duration_ms.store(0, Ordering::Relaxed);
            publish_info(shared, None);
            st.cur = None;
            st.next = None;
            st.hand = None;
        }
    }
}

fn clear(shared: &Shared, base_ms: u64) {
    if let Ok(mut q) = shared.queue.lock() {
        q.clear();
    }
    shared.played.store(0, Ordering::Relaxed);
    shared.base_ms.store(base_ms, Ordering::Relaxed);
}

/// Interleaved samples of any channel count → stereo. 5.1 / 7.1 use the usual downmix
/// (centre and surrounds at -3 dB, LFE left out), scaled so it can't clip.
fn to_stereo(samples: &[f32], channels: usize) -> Vec<f32> {
    const H: f32 = std::f32::consts::FRAC_1_SQRT_2;
    match channels {
        1 => samples.iter().flat_map(|&s| [s, s]).collect(),
        2 => samples.to_vec(),
        // FL FR FC LFE (BL BR) (SL SR)
        6 | 8 => samples
            .chunks_exact(channels)
            .flat_map(|f| {
                let (mut l, mut r) = (f[0] + H * f[2] + H * f[4], f[1] + H * f[2] + H * f[5]);
                if channels == 8 {
                    l += H * f[6];
                    r += H * f[7];
                }
                let norm = if channels == 8 { 1.0 + 3.0 * H } else { 1.0 + 2.0 * H };
                l /= norm;
                r /= norm;
                [l, r]
            })
            .collect(),
        n => samples
            .chunks_exact(n)
            .flat_map(|f| {
                // other layouts: front pair plus an even share of the rest
                let rest: f32 = f[2..].iter().sum::<f32>() * 0.5 / (n - 2) as f32;
                [
                    (f[0] + rest).clamp(-1.0, 1.0),
                    (f[1] + rest).clamp(-1.0, 1.0),
                ]
            })
            .collect(),
    }
}
