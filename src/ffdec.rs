//! Fallback decoder through FFmpeg, for formats Symphonia can't read: Opus, APE, WavPack,
//! DSD (DSF/DFF), WMA, Dolby (AC-3 / E-AC-3 in .m4a), DTS, TrueHD, TTA, Musepack…
//! FFmpeg runs as a child process and pipes 32-bit float stereo; nothing is bundled:
//! `ffmpeg.exe` is used from next to SimplePlayer.exe or from PATH.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::OnceLock;

/// Where the decoder reads from.
#[derive(Clone, Debug)]
pub enum Input {
    Path(PathBuf),
    Url(String),
}

impl Input {
    fn arg(&self) -> String {
        match self {
            Input::Path(p) => p.to_string_lossy().into_owned(),
            Input::Url(u) => u.clone(),
        }
    }
}

/// ReplayGain values found in the tags (dB / linear peak).
#[derive(Clone, Copy, Debug, Default)]
pub struct Gains {
    pub track_gain: Option<f32>,
    pub track_peak: Option<f32>,
    pub album_gain: Option<f32>,
    pub album_peak: Option<f32>,
}

impl Gains {
    /// Reads one tag (key matched case-insensitively). Returns true if it was a gain tag.
    pub fn read(&mut self, key: &str, value: &str) -> bool {
        let num = || {
            value
                .trim()
                .split_whitespace()
                .next()
                .and_then(|v| v.trim_end_matches("dB").parse::<f32>().ok())
                .filter(|v| v.is_finite())
        };
        match key.trim().to_ascii_uppercase().as_str() {
            "REPLAYGAIN_TRACK_GAIN" => self.track_gain = num(),
            "REPLAYGAIN_TRACK_PEAK" => self.track_peak = num(),
            "REPLAYGAIN_ALBUM_GAIN" => self.album_gain = num(),
            "REPLAYGAIN_ALBUM_PEAK" => self.album_peak = num(),
            // Opus: Q7.8 dB relative to -23 LUFS; ReplayGain uses -18, hence +5 dB
            "R128_TRACK_GAIN" if self.track_gain.is_none() => {
                self.track_gain = num().map(|v| v / 256.0 + 5.0)
            }
            "R128_ALBUM_GAIN" if self.album_gain.is_none() => {
                self.album_gain = num().map(|v| v / 256.0 + 5.0)
            }
            _ => return false,
        }
        true
    }

    /// Linear factor for a mode (0 off, 1 track, 2 album; each falls back to the other),
    /// limited so the loudest sample stays below full scale when the peak is known.
    pub fn factor(&self, mode: u32) -> f32 {
        let (gain, peak) = match mode {
            1 => (
                self.track_gain.or(self.album_gain),
                self.track_peak.or(self.album_peak),
            ),
            2 => (
                self.album_gain.or(self.track_gain),
                self.album_peak.or(self.track_peak),
            ),
            _ => return 1.0,
        };
        let Some(db) = gain else { return 1.0 };
        let mut f = 10f32.powf(db / 20.0);
        if let Some(p) = peak.filter(|p| *p > 0.0) {
            f = f.min(1.0 / p);
        }
        f
    }
}

/// The FFmpeg executable, if there is one (looked up once).
pub fn ffmpeg() -> Option<&'static PathBuf> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            let beside = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" })))
                .filter(|p| p.is_file());
            let cand = beside.unwrap_or_else(|| PathBuf::from("ffmpeg"));
            command(&cand)
                .arg("-version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .ok()
                .filter(|s| s.success())
                .map(|_| cand)
        })
        .as_ref()
}

fn command(exe: &PathBuf) -> Command {
    let mut c = Command::new(exe);
    c.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// What `ffmpeg -i` says about the first audio stream.
#[derive(Debug, Default)]
pub struct Info {
    pub codec: String,
    pub rate: u32,
    /// bit depth to ask the output for (lossy and float sources get a sensible value)
    pub bits: u32,
    /// the source's own bit depth when it has one ("s32 (24 bit)" → 24)
    pub src_bits: Option<u32>,
    pub channels: u32,
    pub kbps: Option<u32>,
    pub lossy: bool,
    pub duration: f64,
    pub gains: Gains,
}

/// Parses the stderr of `ffmpeg -hide_banner -i <input>`.
pub fn parse_info(text: &str) -> Option<Info> {
    let mut info = Info::default();
    for line in text.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("Duration:") {
            let t = rest.trim().split(',').next().unwrap_or("").trim();
            let parts: Vec<f64> = t.split(':').filter_map(|p| p.parse().ok()).collect();
            if parts.len() == 3 {
                info.duration = parts[0] * 3600.0 + parts[1] * 60.0 + parts[2];
            }
        } else if l.starts_with("Stream #") && l.contains("Audio:") && info.rate == 0 {
            let a = &l[l.find("Audio:").unwrap() + 6..];
            info.codec = a
                .trim()
                .split(|c: char| c == ' ' || c == ',')
                .next()
                .unwrap_or("")
                .to_string();
            let fields: Vec<&str> = a.split(',').map(|f| f.trim()).collect();
            for f in &fields {
                if let Some(hz) = f.strip_suffix(" Hz") {
                    info.rate = hz.trim().parse().unwrap_or(0);
                }
            }
            let lossy = [
                "aac", "mp3", "mp2", "opus", "vorbis", "ac3", "eac3", "dts", "wmav1", "wmav2",
                "wmapro", "atrac3", "atrac3p", "cook", "amr_nb", "amr_wb", "mpc7", "mpc8",
            ];
            info.lossy = lossy.contains(&info.codec.as_str());
            for f in &fields {
                let f = f.trim();
                info.channels = match f {
                    "mono" => 1,
                    "stereo" => 2,
                    "2.1" => 3,
                    "quad" | "4.0" => 4,
                    "5.0" | "5.0(side)" => 5,
                    _ if f.starts_with("5.1") => 6,
                    _ if f.starts_with("6.1") => 7,
                    _ if f.starts_with("7.1") => 8,
                    _ => match f.strip_suffix(" channels").map(|n| n.trim().parse()) {
                        Some(Ok(n)) => n,
                        _ => {
                            if let Some(i) = f.find(" kb/s") {
                                info.kbps = f[..i].trim().parse().ok();
                            }
                            continue;
                        }
                    },
                };
            }
            info.src_bits = if let Some(i) = a.find(" bit)") {
                a[..i].rsplit('(').next().and_then(|n| n.trim().parse().ok())
            } else if fields.iter().any(|f| f.starts_with("s16")) {
                Some(16)
            } else if fields.iter().any(|f| f.starts_with("s32")) {
                Some(32)
            } else if fields.iter().any(|f| f.starts_with("u8")) {
                Some(8)
            } else {
                None
            };
            info.bits = if info.lossy {
                16
            } else if a.contains("(24 bit)") || a.contains("s24") {
                24
            } else if fields.iter().any(|f| f.starts_with("s16") || f.starts_with("u8")) {
                16
            } else {
                24 // s32 / flt / dbl / DSD: high resolution
            };
        } else if let Some((k, v)) = l.split_once(':') {
            info.gains.read(k, v);
        }
    }
    (info.rate > 0).then_some(info)
}

/// Rate FFmpeg should output: the source rate, but DSD and other very high rates are
/// brought down to 176.4 / 192 kHz (same family) so every output path can play them.
pub fn output_rate(src: u32) -> u32 {
    if src <= 192_000 {
        src
    } else if src % 44_100 == 0 {
        176_400
    } else {
        192_000
    }
}

pub struct FfSource {
    input: Input,
    pub info: Info,
    pub rate: u32,
    child: Option<Child>,
    out: Option<ChildStdout>,
    buf: Vec<u8>,
}

impl FfSource {
    pub fn open(input: Input) -> Result<FfSource, String> {
        let exe = ffmpeg().ok_or_else(|| {
            crate::tr!(
                "這個格式需要 FFmpeg（把 ffmpeg.exe 放在 SimplePlayer.exe 旁邊，或安裝後加入 PATH）",
                "This format needs FFmpeg (put ffmpeg.exe next to SimplePlayer.exe, or install it on PATH)"
            )
        })?;
        let probe = command(exe)
            .args(["-hide_banner", "-nostdin", "-i"])
            .arg(input.arg())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&probe.stderr);
        let info = parse_info(&text).ok_or_else(|| {
            let last = text.lines().last().unwrap_or("").trim().to_string();
            crate::tr!("FFmpeg 無法讀取：{}", "FFmpeg can't read it: {}", last)
        })?;
        let rate = output_rate(info.rate);
        let mut s = FfSource {
            input,
            info,
            rate,
            child: None,
            out: None,
            buf: Vec::new(),
        };
        s.spawn(0.0)?;
        Ok(s)
    }

    fn spawn(&mut self, at: f64) -> Result<(), String> {
        self.kill();
        let exe = ffmpeg().ok_or("ffmpeg")?;
        let mut c = command(exe);
        c.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        if at > 0.0 {
            c.arg("-ss").arg(format!("{at:.3}"));
        }
        c.arg("-i").arg(self.input.arg());
        c.args(["-map", "0:a:0", "-vn", "-sn", "-dn", "-ac", "2"]);
        c.arg("-ar").arg(self.rate.to_string());
        c.args(["-c:a", "pcm_f32le", "-f", "f32le", "pipe:1"]);
        c.stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = c.spawn().map_err(|e| e.to_string())?;
        self.out = child.stdout.take();
        self.child = Some(child);
        self.buf.clear();
        Ok(())
    }

    fn kill(&mut self) {
        self.out = None;
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Next block of interleaved stereo samples; None at the end.
    pub fn next(&mut self) -> Option<Vec<f32>> {
        let out = self.out.as_mut()?;
        let mut chunk = [0u8; 32 * 1024];
        loop {
            match out.read(&mut chunk) {
                Ok(0) => {
                    self.kill();
                    return None;
                }
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    let whole = self.buf.len() / 8 * 8; // whole stereo frames
                    if whole == 0 {
                        continue;
                    }
                    let v: Vec<f32> = self.buf[..whole]
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                        .collect();
                    self.buf.drain(..whole);
                    return Some(v);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.kill();
                    return None;
                }
            }
        }
    }

    pub fn seek(&mut self, secs: f64) -> bool {
        self.spawn(secs.max(0.0)).is_ok()
    }
}

impl Drop for FfSource {
    fn drop(&mut self) {
        self.kill();
    }
}
