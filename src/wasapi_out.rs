//! WASAPI exclusive-mode output (Windows). The device is opened at the song's own sample
//! rate and bit depth, so the samples reach the DAC unchanged (bit-perfect at 100 % volume),
//! bypassing the Windows mixer. While it runs, other apps cannot play through that device.

use std::time::{Duration, Instant};

use wasapi::{Direction, SampleType, ShareMode, WaveFormat};

/// How samples are written for the format the device accepted.
#[derive(Clone, Copy, Debug)]
enum Kind {
    I16,
    I24Packed,
    I24In32,
    I32,
    F32,
}

impl Kind {
    fn bytes(self) -> usize {
        match self {
            Kind::I16 => 2,
            Kind::I24Packed => 3,
            Kind::I24In32 | Kind::I32 | Kind::F32 => 4,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Kind::I16 => "16-bit",
            Kind::I24Packed | Kind::I24In32 => "24-bit",
            Kind::I32 => "32-bit",
            Kind::F32 => "32-bit float",
        }
    }
}

pub struct Exclusive {
    client: wasapi::AudioClient,
    render: wasapi::AudioRenderClient,
    event: wasapi::Handle,
    kind: Kind,
    pub rate: u32,
    /// What the song asked for (shown when the device could not do it).
    asked_rate: u32,
    asked_bits: u32,
    /// Why the song's own format was refused (first error), for the status line.
    refusal: Option<String>,
    channels: usize,
    bytes: Vec<u8>,
    samples: Vec<f32>,
    /* timing diagnostics: a wake-up later than the device buffer = an audible gap that the
    decoder's underrun counter cannot see (it always had data) */
    period: Duration,
    last_wake: Option<Instant>,
    /// Late wake-ups since the stream was opened.
    pub late: u64,
    /// The player sets this before each pump: volume at 100 % (samples pass unchanged).
    pub unity: bool,
    /// The device can't take more than 16 bits in exclusive mode (dither is only used then).
    pub device16: bool,
    /// xorshift state for the dither noise
    rng: u32,
}

fn find_device(name: Option<&str>) -> Result<wasapi::Device, String> {
    if let Some(name) = name {
        let coll = wasapi::DeviceCollection::new(&Direction::Render).map_err(|e| e.to_string())?;
        let n = coll.get_nbr_devices().map_err(|e| e.to_string())?;
        for i in 0..n {
            if let Ok(d) = coll.get_device_at_index(i) {
                if d.get_friendlyname().ok().as_deref() == Some(name) {
                    return Ok(d);
                }
            }
        }
    }
    wasapi::get_default_device(&Direction::Render).map_err(|e| e.to_string())
}

/// Asks the driver (without opening anything) whether it takes 24/32-bit at `rate`
/// in exclusive mode, trying the usual WAVEFORMATEX / channel-mask variants.
fn higher_bits_ok(name: Option<&str>, rate: u32) -> bool {
    let Ok(device) = find_device(name) else {
        return false;
    };
    let Ok(client) = device.get_iaudioclient() else {
        return false;
    };
    [Kind::I24In32, Kind::I24Packed, Kind::I32, Kind::F32]
        .iter()
        .any(|&k| {
            client
                .is_supported_exclusive_with_quirks(&format(k, rate))
                .is_ok()
        })
}

fn format(kind: Kind, rate: u32) -> WaveFormat {
    let (store, valid, st) = match kind {
        Kind::I16 => (16, 16, SampleType::Int),
        Kind::I24Packed => (24, 24, SampleType::Int),
        Kind::I24In32 => (32, 24, SampleType::Int),
        Kind::I32 => (32, 32, SampleType::Int),
        Kind::F32 => (32, 32, SampleType::Float),
    };
    WaveFormat::new(store, valid, &st, rate as usize, 2, None)
}

impl Exclusive {
    /// Opens `name` (None = default device) exclusively at `rate`, preferring the song's own
    /// bit depth. Falls back to other common rates when the device can't do `rate`.
    pub fn open(
        name: Option<&str>,
        rate: u32,
        bits: u32,
        period_ms: u32,
    ) -> Result<Exclusive, String> {
        let _ = wasapi::initialize_mta();
        let kinds: Vec<Kind> = match bits {
            0..=16 => vec![
                Kind::I16,
                Kind::I24In32,
                Kind::I24Packed,
                Kind::I32,
                Kind::F32,
            ],
            17..=24 => vec![
                Kind::I24In32,
                Kind::I24Packed,
                Kind::I32,
                Kind::I16,
                Kind::F32,
            ],
            _ => vec![
                Kind::I32,
                Kind::I24In32,
                Kind::I24Packed,
                Kind::F32,
                Kind::I16,
            ],
        };
        let mut rates = vec![rate];
        for r in [48_000, 44_100, 96_000, 88_200, 192_000] {
            if !rates.contains(&r) {
                rates.push(r);
            }
        }
        let mut last_err = crate::tr!("不支援的格式", "Unsupported format");
        let mut refusal: Option<String> = None;
        for &r in &rates {
            for &k in &kinds {
                // ask before opening: once we hold the device exclusively, the driver
                // may refuse every query with "device in use"
                let device16 = matches!(k, Kind::I16) && !higher_bits_ok(name, r);
                match Self::try_open(name, k, r, period_ms) {
                    Ok(mut x) => {
                        x.asked_rate = rate;
                        x.asked_bits = bits;
                        x.refusal = refusal;
                        if matches!(k, Kind::I16) {
                            x.device16 = device16;
                        }
                        return Ok(x);
                    }
                    Err(e) => {
                        if refusal.is_none() {
                            refusal = Some(e.clone());
                        }
                        last_err = e;
                    }
                }
            }
        }
        Err(last_err)
    }

    fn try_open(
        name: Option<&str>,
        kind: Kind,
        rate: u32,
        period_ms: u32,
    ) -> Result<Exclusive, String> {
        // A roomy device buffer (settings → 緩衝大小, instead of the default ~10 ms) so a busy PC
        // doesn't run it dry (= crackles / stutter). Some drivers want other sizes: then default.
        let fmt = format(kind, rate);
        let mut last = String::new();
        let mut opened = None;
        for attempt in 0..2 {
            let device = find_device(name)?;
            let mut client = device.get_iaudioclient().map_err(|e| e.to_string())?;
            let (default_period, min_period) = client.get_periods().map_err(|e| e.to_string())?;
            let period = if attempt == 0 {
                // the chosen buffer length, rounded to whole frames (alignment 8 frames)
                let frames = ((rate as i64 * period_ms as i64 / 1000) / 8 * 8).max(8);
                (frames * 10_000_000 / rate as i64).max(min_period)
            } else {
                default_period
            };
            match client.initialize_client(
                &fmt,
                period,
                &Direction::Render,
                &ShareMode::Exclusive,
                false,
            ) {
                Ok(()) => {
                    opened = Some(client);
                    break;
                }
                Err(e) => last = e.to_string(),
            }
        }
        let client = opened.ok_or(last)?;
        let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
        let render = client.get_audiorenderclient().map_err(|e| e.to_string())?;
        let mut x = Exclusive {
            client,
            render,
            event,
            kind,
            rate,
            asked_rate: rate,
            asked_bits: 0,
            refusal: None,
            channels: 2,
            bytes: Vec::new(),
            samples: Vec::new(),
            period: Duration::from_millis(10),
            last_wake: None,
            late: 0,
            unity: true,
            device16: false,
            rng: 0x9E37_79B9,
        };
        // start with one buffer of silence, as WASAPI expects in event mode
        let frames = x
            .client
            .get_available_space_in_frames()
            .map_err(|e| e.to_string())? as usize;
        x.period = Duration::from_secs_f64(frames.max(1) as f64 / rate as f64);
        x.bytes.clear();
        x.bytes.resize(frames * x.channels * kind.bytes(), 0);
        x.render
            .write_to_device(frames, &x.bytes, None)
            .map_err(|e| e.to_string())?;
        x.client.start_stream().map_err(|e| e.to_string())?;
        Ok(x)
    }

    /// e.g. "44.1 kHz · 16-bit", plus what the song wanted when the device refused it.
    pub fn describe(&self) -> String {
        let khz = |r: u32| (r as f64 / 1000.0).to_string();
        let mut s = format!("{} kHz · {}", khz(self.rate), self.kind.label());
        let bits_now: u32 = match self.kind {
            Kind::I16 => 16,
            Kind::I24Packed | Kind::I24In32 => 24,
            _ => 32,
        };
        let bits_ok = self.asked_bits == 0 || bits_now >= self.asked_bits;
        if self.device16 && self.asked_bits > 16 {
            s.push_str(" + dither");
        }
        if self.rate != self.asked_rate || !bits_ok {
            s.push_str(&crate::tr!(
                "（歌曲是 {} kHz · {}-bit，裝置不接受",
                " (song is {} kHz · {}-bit, the device refused it",
                khz(self.asked_rate),
                self.asked_bits
            ));
            if let Some(e) = &self.refusal {
                let e: String = e.chars().take(80).collect();
                s.push_str(&crate::tr!("：{}", ": {}", e));
            }
            s.push_str(&crate::tr!("）", ")"));
        }
        s
    }

    /// Waits for the device to want data, then fills it. `take` fills interleaved stereo
    /// f32 samples (silence when paused).
    pub fn pump(&mut self, take: impl FnOnce(&mut [f32])) -> Result<(), String> {
        if self.event.wait_for_event(200).is_err() {
            // no request from the device for 200 ms: it is stalled (or we were)
            self.late += 1;
            self.last_wake = None;
            return Ok(());
        }
        let now = Instant::now();
        if let Some(prev) = self.last_wake {
            let gap = now - prev;
            // the device double-buffers: later than ~1.5 periods = it played silence
            if gap > self.period * 3 / 2 {
                self.late += 1;
            }
        }
        self.last_wake = Some(now);
        let frames = self
            .client
            .get_available_space_in_frames()
            .map_err(|e| e.to_string())? as usize;
        if frames == 0 {
            return Ok(());
        }
        self.samples.clear();
        self.samples.resize(frames * self.channels, 0.0);
        take(&mut self.samples);
        let b = self.kind.bytes();
        // TPDF dither only on a device that can't do more than 16 bits, when the samples hold
        // more detail than that: a 24-bit song, or any song below 100 % volume.
        // A 16-bit song at 100 % stays bit-perfect.
        let dither = self.device16 && (self.asked_bits > 16 || !self.unity);
        self.bytes.clear();
        self.bytes.reserve(self.samples.len() * b);
        let mut rng = self.rng;
        for &v in &self.samples {
            let v = v.clamp(-1.0, 1.0);
            match self.kind {
                Kind::I16 => {
                    let mut x = v * 32768.0;
                    if dither && v != 0.0 {
                        // digital silence stays silent; two uniform [0,1) draws: their difference is triangular ±1 LSB
                        x += noise(&mut rng) - noise(&mut rng);
                    }
                    let s = x.round().clamp(-32768.0, 32767.0) as i16;
                    self.bytes.extend_from_slice(&s.to_le_bytes());
                }
                Kind::I24Packed => {
                    let s = (v as f64 * 8_388_608.0)
                        .round()
                        .clamp(-8_388_608.0, 8_388_607.0) as i32;
                    self.bytes.extend_from_slice(&s.to_le_bytes()[..3]);
                }
                Kind::I24In32 => {
                    let s = (v as f64 * 8_388_608.0)
                        .round()
                        .clamp(-8_388_608.0, 8_388_607.0) as i32;
                    self.bytes.extend_from_slice(&(s << 8).to_le_bytes());
                }
                Kind::I32 => {
                    let s = (v as f64 * 2_147_483_648.0)
                        .round()
                        .clamp(-2_147_483_648.0, 2_147_483_647.0)
                        as i32;
                    self.bytes.extend_from_slice(&s.to_le_bytes());
                }
                Kind::F32 => self.bytes.extend_from_slice(&v.to_le_bytes()),
            }
        }
        self.rng = rng;
        self.render
            .write_to_device(frames, &self.bytes, None)
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl Drop for Exclusive {
    fn drop(&mut self) {
        let _ = self.client.stop_stream();
    }
}

/// Uniform random number in [0, 1) (xorshift32; plenty for dither noise).
fn noise(r: &mut u32) -> f32 {
    *r ^= *r << 13;
    *r ^= *r >> 17;
    *r ^= *r << 5;
    (*r >> 8) as f32 / (1u32 << 24) as f32
}
