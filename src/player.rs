//! Audio engine: Symphonia decodes on a worker thread, a small ring buffer feeds the
//! cpal output callback. Sample-rate conversion is linear (good enough for v0.1;
//! a proper resampler / WASAPI exclusive mode can come later).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

/// Seconds of decoded audio kept ahead of the output.
const BUFFER_SECONDS: f32 = 0.6;

pub enum PlayerEvent {
    /// The current track played to the end.
    Ended,
    /// The track could not be opened or decoded.
    Error(String),
}

pub type Opener = Box<dyn FnOnce() -> Result<Box<dyn MediaSource>, String> + Send>;

enum Cmd {
    Load {
        src: Box<dyn MediaSource>,
        ext: Option<String>,
        start_paused: bool,
    },
    Open {
        open: Opener,
        ext: Option<String>,
        start_paused: bool,
    },
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
}

pub struct Player {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
}

impl Player {
    pub fn new(on_event: impl Fn(PlayerEvent) + Send + 'static) -> Player {
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
        });

        // The output stream lives on its own thread: WASAPI wants a COM apartment that
        // differs from the UI thread's, and cpal::Stream cannot move between threads.
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        let out_shared = shared.clone();
        thread::Builder::new()
            .name("audio-out".into())
            .spawn(move || {
                let stream = open_output(out_shared);
                let ok = stream.is_some();
                let _ = ready_tx.send(ok);
                if ok {
                    // Keep the stream alive for the lifetime of the app.
                    loop {
                        thread::park();
                    }
                }
            })
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

        Player { tx, shared }
    }

    /// Opens the source on the decoder thread (e.g. an HTTP stream), then plays it.
    pub fn open(&self, open: Opener, ext: Option<String>, start_paused: bool) {
        let _ = self.tx.send(Cmd::Open {
            open,
            ext,
            start_paused,
        });
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

    /// Track length in seconds (0 when unknown).
    pub fn duration(&self) -> f64 {
        self.shared.duration_ms.load(Ordering::Relaxed) as f64 / 1000.0
    }
}

/* ---------------- output ---------------- */

fn open_output(shared: Arc<Shared>) -> Option<cpal::Stream> {
    let host = cpal::default_host();
    let device = host.default_output_device()?;
    let config = device.default_output_config().ok()?;
    let channels = config.channels() as usize;
    shared
        .out_rate
        .store(config.sample_rate().0, Ordering::Relaxed);
    let stream_config: cpal::StreamConfig = config.clone().into();
    let err_fn = |e| eprintln!("audio output error: {e}");

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
}

/* ---------------- decoder ---------------- */

struct Track {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    resampler: Resampler,
}

fn open_track(
    src: Box<dyn MediaSource>,
    ext: Option<&str>,
    out_rate: u32,
) -> Result<(Track, f64), String> {
    let mss = MediaSourceStream::new(src, Default::default());
    let mut hint = Hint::new();
    if let Some(e) = ext {
        hint.with_extension(e.trim_start_matches('.'));
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("無法辨識音訊格式：{e}"))?;
    let format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| "找不到音軌".to_string())?;
    let track_id = track.id;
    let params = track.codec_params.clone();
    let decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| format!("不支援的編碼：{e}"))?;
    let src_rate = params.sample_rate.unwrap_or(44_100);
    let duration = match (params.n_frames, params.time_base) {
        (Some(n), Some(tb)) => {
            let t = tb.calc_time(n);
            t.seconds as f64 + t.frac
        }
        (Some(n), None) => n as f64 / src_rate as f64,
        _ => 0.0,
    };
    Ok((
        Track {
            format,
            decoder,
            track_id,
            resampler: Resampler::new(src_rate, out_rate),
        },
        duration,
    ))
}

fn decoder_thread(rx: Receiver<Cmd>, shared: Arc<Shared>, on_event: impl Fn(PlayerEvent)) {
    let mut cur: Option<Track> = None;
    let mut ended_sent = false;

    loop {
        // Wait for a command when idle; otherwise just peek between packets.
        let cmd = if cur.is_none() {
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
            handle(&shared, cmd, &mut cur, &on_event, &mut ended_sent);
            continue;
        }

        // Finished decoding: wait for the output to drain, then report the end.
        if shared.decoding_done.load(Ordering::Relaxed) {
            let empty = shared.queue.lock().map(|q| q.len() < 2).unwrap_or(true);
            if empty && !ended_sent {
                ended_sent = true;
                on_event(PlayerEvent::Ended);
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(c) => handle(&shared, c, &mut cur, &on_event, &mut ended_sent),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            continue;
        }

        let Some(t) = cur.as_mut() else { continue };

        // Keep the buffer topped up.
        let out_rate = shared.out_rate.load(Ordering::Relaxed) as f32;
        let limit = (out_rate * BUFFER_SECONDS) as usize * 2;
        if shared.queue.lock().map(|q| q.len()).unwrap_or(0) >= limit {
            thread::sleep(Duration::from_millis(10));
            continue;
        }

        let packet = match t.format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                shared.decoding_done.store(true, Ordering::Relaxed);
                continue;
            }
            Err(SymError::ResetRequired) => {
                t.decoder.reset();
                continue;
            }
            Err(e) => {
                shared.decoding_done.store(true, Ordering::Relaxed);
                eprintln!("read error: {e}");
                continue;
            }
        };
        if packet.track_id() != t.track_id {
            continue;
        }
        let decoded = match t.decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) => continue, // skip a corrupt packet
            Err(e) => {
                eprintln!("decode error: {e}");
                shared.decoding_done.store(true, Ordering::Relaxed);
                continue;
            }
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        let mut sb = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        sb.copy_interleaved_ref(decoded);
        let stereo = to_stereo(sb.samples(), channels);
        let out = t.resampler.process(&stereo);
        if let Ok(mut q) = shared.queue.lock() {
            q.extend(out);
        }
    }
}

fn handle(
    shared: &Arc<Shared>,
    cmd: Cmd,
    cur: &mut Option<Track>,
    on_event: &impl Fn(PlayerEvent),
    ended_sent: &mut bool,
) {
    let cmd = match cmd {
        Cmd::Open {
            open,
            ext,
            start_paused,
        } => {
            shared.paused.store(true, Ordering::Relaxed);
            clear(shared, 0);
            *cur = None;
            match open() {
                Ok(src) => Cmd::Load {
                    src,
                    ext,
                    start_paused,
                },
                Err(e) => {
                    shared.active.store(false, Ordering::Relaxed);
                    on_event(PlayerEvent::Error(e));
                    return;
                }
            }
        }
        other => other,
    };
    match cmd {
        Cmd::Open { .. } => {}
        Cmd::Load {
            src,
            ext,
            start_paused,
        } => {
            shared.paused.store(true, Ordering::Relaxed);
            clear(shared, 0);
            *cur = None;
            let out_rate = shared.out_rate.load(Ordering::Relaxed);
            match open_track(src, ext.as_deref(), out_rate) {
                Ok((t, dur)) => {
                    shared
                        .duration_ms
                        .store((dur * 1000.0) as u64, Ordering::Relaxed);
                    shared.active.store(true, Ordering::Relaxed);
                    shared.decoding_done.store(false, Ordering::Relaxed);
                    shared.paused.store(start_paused, Ordering::Relaxed);
                    *ended_sent = false;
                    *cur = Some(t);
                }
                Err(e) => {
                    shared.active.store(false, Ordering::Relaxed);
                    on_event(PlayerEvent::Error(e));
                }
            }
        }
        Cmd::Seek(secs) => {
            if let Some(t) = cur.as_mut() {
                let to = Time::new(secs.trunc() as u64, secs.fract());
                if t.format
                    .seek(
                        SeekMode::Accurate,
                        SeekTo::Time {
                            time: to,
                            track_id: Some(t.track_id),
                        },
                    )
                    .is_ok()
                {
                    t.decoder.reset();
                    t.resampler.reset();
                    clear(shared, (secs * 1000.0) as u64);
                    shared.decoding_done.store(false, Ordering::Relaxed);
                    *ended_sent = false;
                }
            }
        }
        Cmd::Stop => {
            shared.paused.store(true, Ordering::Relaxed);
            shared.active.store(false, Ordering::Relaxed);
            clear(shared, 0);
            shared.duration_ms.store(0, Ordering::Relaxed);
            *cur = None;
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

fn to_stereo(samples: &[f32], channels: usize) -> Vec<f32> {
    match channels {
        1 => samples.iter().flat_map(|&s| [s, s]).collect(),
        2 => samples.to_vec(),
        n => samples
            .chunks(n)
            .flat_map(|f| {
                // Simple downmix: front left/right plus half of the rest.
                let rest: f32 = f[2..].iter().sum::<f32>() * 0.5 / (n - 2) as f32;
                [
                    (f[0] + rest).clamp(-1.0, 1.0),
                    (f[1] + rest).clamp(-1.0, 1.0),
                ]
            })
            .collect(),
    }
}

/// Linear-interpolation resampler for interleaved stereo.
struct Resampler {
    step: f64, // source frames per output frame
    pos: f64,  // position in the (prev + current) source stream
    prev: [f32; 2],
}

impl Resampler {
    fn new(src: u32, dst: u32) -> Self {
        Resampler {
            step: src as f64 / dst.max(1) as f64,
            pos: 0.0,
            prev: [0.0, 0.0],
        }
    }

    fn reset(&mut self) {
        self.pos = 0.0;
        self.prev = [0.0, 0.0];
    }

    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if (self.step - 1.0).abs() < 1e-9 {
            return input.to_vec();
        }
        let frames = input.len() / 2;
        if frames == 0 {
            return Vec::new();
        }
        let prev = self.prev;
        let frame = |i: isize| -> [f32; 2] {
            if i < 0 {
                prev
            } else {
                let i = i as usize;
                [input[i * 2], input[i * 2 + 1]]
            }
        };
        let mut out = Vec::with_capacity((frames as f64 / self.step) as usize * 2 + 4);
        // pos is measured from the previous block's last frame (index -1).
        let mut pos = self.pos;
        while pos < frames as f64 {
            let i = pos.floor() as isize - 1;
            let t = (pos - pos.floor()) as f32;
            let a = frame(i);
            let b = frame(i + 1);
            out.push(a[0] + (b[0] - a[0]) * t);
            out.push(a[1] + (b[1] - a[1]) * t);
            pos += self.step;
        }
        self.pos = pos;
        self.pos -= frames as f64;
        self.prev = [input[(frames - 1) * 2], input[(frames - 1) * 2 + 1]];
        out
    }
}
