//! Sample-rate conversion for stereo f32 audio: a windowed-sinc (Kaiser) polyphase filter,
//! the same family of converter good players use. Passband to about 20 kHz for 44.1/48 kHz
//! audio with ~90 dB image/alias rejection; when downsampling (e.g. 96 → 48 kHz) the cutoff
//! follows the lower rate so nothing folds back. Exact rational stepping: no drift.

pub struct Resampler {
    passthrough: bool,
    /// source frames per output frame = step_int + step_num / den
    step_int: usize,
    step_num: u64,
    den: u64,
    /// half the filter length in source frames (taps = 2 * half)
    half: usize,
    /// (PHASES + 1) rows of 2 * half coefficients
    table: Vec<f32>,
    /// interleaved stereo history; frame `base` is the current output position
    buf: Vec<f32>,
    base: usize,
    frac: u64,
}

const PHASES: usize = 256;
const ZERO_CROSSINGS: f64 = 48.0;
const BETA: f64 = 9.0;
const CUTOFF: f64 = 0.95;

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// Modified Bessel function of the first kind, order 0 (for the Kaiser window).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..64 {
        term *= q / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

impl Resampler {
    pub fn new(src: u32, dst: u32) -> Resampler {
        let (src, dst) = (src.max(1) as u64, dst.max(1) as u64);
        let g = gcd(src, dst);
        let (s, d) = (src / g, dst / g);
        let passthrough = s == d;
        // normalised cutoff (1.0 = source Nyquist), lowered when downsampling
        let r = CUTOFF * (d as f64 / s as f64).min(1.0);
        let half = if passthrough {
            0
        } else {
            (ZERO_CROSSINGS / r).ceil() as usize
        };
        let taps = 2 * half;
        let mut table = vec![0f32; (PHASES + 1) * taps];
        if !passthrough {
            let i0b = bessel_i0(BETA);
            for p in 0..=PHASES {
                let f = p as f64 / PHASES as f64;
                let row = &mut table[p * taps..(p + 1) * taps];
                let mut sum = 0.0;
                let mut vals = vec![0f64; taps];
                for (j, v) in vals.iter_mut().enumerate() {
                    // tap j sits at source frame base - half + 1 + j; distance from the output time
                    let t = (j as f64 - half as f64 + 1.0) - f;
                    let x = t / half as f64;
                    if x.abs() >= 1.0 {
                        continue;
                    }
                    let a = std::f64::consts::PI * r * t;
                    let sinc = if a.abs() < 1e-12 { 1.0 } else { a.sin() / a };
                    let w = bessel_i0(BETA * (1.0 - x * x).sqrt()) / i0b;
                    *v = r * sinc * w;
                    sum += *v;
                }
                // unity gain at DC for every phase
                for (o, v) in row.iter_mut().zip(vals) {
                    *o = (v / sum) as f32;
                }
            }
        }
        let mut buf = Vec::new();
        // history before the first sample is silence
        buf.resize(half.saturating_sub(1) * 2, 0.0);
        Resampler {
            passthrough,
            step_int: (s / d) as usize,
            step_num: s % d,
            den: d,
            half,
            table,
            buf,
            base: half.saturating_sub(1),
            frac: 0,
        }
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.buf.resize(self.half.saturating_sub(1) * 2, 0.0);
        self.base = self.half.saturating_sub(1);
        self.frac = 0;
    }

    /// Converts a block of interleaved stereo frames. Output lags input by `half` frames
    /// (< 1 ms); state carries over between blocks.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.passthrough {
            return input.to_vec();
        }
        self.buf.extend_from_slice(input);
        let frames = self.buf.len() / 2;
        let half = self.half;
        let taps = 2 * half;
        let est = ((input.len() / 2) as u64 * self.den / (self.step_int as u64 * self.den + self.step_num).max(1)) as usize;
        let mut out = Vec::with_capacity(est * 2 + 8);
        // need frames base - half + 1 ..= base + half
        while self.base + half < frames {
            let fp = self.frac as f64 / self.den as f64 * PHASES as f64;
            let p = (fp as usize).min(PHASES - 1);
            let w = (fp - p as f64) as f32;
            let r0 = &self.table[p * taps..(p + 1) * taps];
            let r1 = &self.table[(p + 1) * taps..(p + 2) * taps];
            let start = (self.base + 1 - half) * 2;
            let src = &self.buf[start..start + taps * 2];
            let (mut l, mut r) = (0f32, 0f32);
            for j in 0..taps {
                let c = r0[j] + (r1[j] - r0[j]) * w;
                l += src[j * 2] * c;
                r += src[j * 2 + 1] * c;
            }
            out.push(l);
            out.push(r);
            self.base += self.step_int;
            self.frac += self.step_num;
            if self.frac >= self.den {
                self.frac -= self.den;
                self.base += 1;
            }
        }
        // drop history that no future output needs
        let keep_from = (self.base + 1).saturating_sub(half).min(frames);
        if keep_from > 0 {
            self.buf.drain(..keep_from * 2);
            self.base -= keep_from;
        }
        out
    }
}
