//! The synthesis filterbank: IMDCT, the sine and KBD windows, the four
//! window sequences and overlap-add (ISO/IEC 14496-3 4.6.11).
//!
//! The IMDCT runs as a DCT-IV through an N/4-point complex FFT, then unfolds
//! by the transform's symmetries; `frame_len` sets N, so the 960/120 frames
//! of DRM and DAB+ use the same code with 480- and 60-point FFTs. Scaling is
//! the standard's 2/N, which leaves the output in 16-bit sample units.

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::f64::consts::PI;
use std::sync::Arc;

pub(super) const ONLY_LONG: u8 = 0;
pub(super) const LONG_START: u8 = 1;
pub(super) const EIGHT_SHORT: u8 = 2;
pub(super) const LONG_STOP: u8 = 3;

/// An inverse MDCT producing `2 * m` samples from `m` coefficients.
pub(super) struct Imdct {
    m: usize,
    fft: Arc<dyn Fft<f32>>,
    /// exp(-i pi (k + 1/8) / m) for k < m/2, used before and after the FFT.
    twiddle: Vec<Complex32>,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    dct: Vec<f32>,
}

impl Imdct {
    pub fn new(m: usize, planner: &mut FftPlanner<f32>) -> Self {
        let fft = planner.plan_fft_forward(m / 2);
        let twiddle = (0..m / 2)
            .map(|k| {
                let a = -PI * (k as f64 + 0.125) / m as f64;
                Complex32::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        Self { m, fft, twiddle, buf: vec![Complex32::default(); m / 2], scratch, dct: vec![0.0; m] }
    }

    /// DCT-IV of `x` into `self.dct`, unscaled.
    fn dct4(&mut self, x: &[f32]) {
        let m = self.m;
        for k in 0..m / 2 {
            let z = Complex32::new(x[2 * k], x[m - 1 - 2 * k]);
            self.buf[k] = z * self.twiddle[k];
        }
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        for j in 0..m / 2 {
            let y = self.buf[j] * self.twiddle[j];
            self.dct[2 * j] = y.re;
            self.dct[m - 1 - 2 * j] = -y.im;
        }
    }

    /// `out[n] = 2/N sum_k x[k] cos(2 pi / N (n + n0)(k + 1/2))`, N = 2m,
    /// n0 = (m + 1) / 2.
    pub fn run(&mut self, x: &[f32], out: &mut [f32]) {
        let m = self.m;
        self.dct4(&x[..m]);
        let scale = 1.0 / m as f32;
        let h = m / 2;
        for n in 0..h {
            out[n] = self.dct[h + n] * scale;
        }
        for n in h..3 * h {
            out[n] = -self.dct[3 * h - 1 - n] * scale;
        }
        for n in 3 * h..2 * m {
            out[n] = -self.dct[n - 3 * h] * scale;
        }
    }
}

/// The rising half of a window of length `2 * m`.
fn sine_window(m: usize) -> Vec<f32> {
    (0..m).map(|n| (PI / (2 * m) as f64 * (n as f64 + 0.5)).sin() as f32).collect()
}

/// Modified Bessel function of the first kind, order zero.
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..100 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// The rising half of a Kaiser-Bessel-derived window of length `2 * m`.
fn kbd_window(m: usize, alpha: f64) -> Vec<f32> {
    let n4 = m as f64 / 2.0;
    let kernel: Vec<f64> = (0..=m)
        .map(|n| {
            let r = (n as f64 - n4) / n4;
            bessel_i0(PI * alpha * (1.0 - r * r).max(0.0).sqrt())
        })
        .collect();
    let total: f64 = kernel.iter().sum();
    let mut acc = 0.0;
    (0..m)
        .map(|n| {
            acc += kernel[n];
            (acc / total).sqrt() as f32
        })
        .collect()
}

/// Per-channel filterbank state: the overlap half and the previous window
/// shape (the left half of each window takes the previous frame's shape).
pub(super) struct Overlap {
    pub samples: Vec<f32>,
    pub prev_shape: u8,
}

impl Overlap {
    pub fn new(frame_len: usize) -> Self {
        Self { samples: vec![0.0; frame_len], prev_shape: 0 }
    }
}

pub(super) struct FilterBank {
    frame_len: usize,
    short_len: usize,
    long: Imdct,
    short: Imdct,
    /// Rising halves, indexed by window shape (0 sine, 1 KBD).
    long_win: [Vec<f32>; 2],
    short_win: [Vec<f32>; 2],
    buf: Vec<f32>,
    frame: Vec<f32>,
}

impl FilterBank {
    pub fn new(frame_len: usize) -> Self {
        let short_len = frame_len / 8;
        let mut planner = FftPlanner::new();
        Self {
            frame_len,
            short_len,
            long: Imdct::new(frame_len, &mut planner),
            short: Imdct::new(short_len, &mut planner),
            long_win: [sine_window(frame_len), kbd_window(frame_len, 4.0)],
            short_win: [sine_window(short_len), kbd_window(short_len, 6.0)],
            buf: vec![0.0; 2 * frame_len],
            frame: vec![0.0; 2 * frame_len],
        }
    }

    /// Inverse transform of one channel's spectrum, windowed and overlapped
    /// with the previous frame; writes `frame_len` samples to `out`.
    pub fn synthesize(
        &mut self,
        spec: &[f32],
        seq: u8,
        shape: u8,
        ov: &mut Overlap,
        out: &mut [f32],
    ) {
        let (n, s) = (self.frame_len, self.short_len);
        let prev = usize::from(ov.prev_shape);
        let cur = usize::from(shape);
        // Where the short windows sit inside the long frame: (N - N/8) / 2.
        let flat = (n - s) / 2;
        let frame = &mut self.frame;
        if seq == EIGHT_SHORT {
            frame.fill(0.0);
            for w in 0..8 {
                self.short.run(&spec[w * s..(w + 1) * s], &mut self.buf[..2 * s]);
                let left = if w == 0 { &self.short_win[prev] } else { &self.short_win[cur] };
                let right = &self.short_win[cur];
                let base = flat + w * s;
                for i in 0..s {
                    frame[base + i] += self.buf[i] * left[i];
                    frame[base + s + i] += self.buf[s + i] * right[s - 1 - i];
                }
            }
        } else {
            self.long.run(&spec[..n], frame);
            // Left half.
            if seq == LONG_STOP {
                let left = &self.short_win[prev];
                frame[..flat].fill(0.0);
                for i in 0..s {
                    frame[flat + i] *= left[i];
                }
            } else {
                let left = &self.long_win[prev];
                for i in 0..n {
                    frame[i] *= left[i];
                }
            }
            // Right half.
            if seq == LONG_START {
                let right = &self.short_win[cur];
                for i in 0..s {
                    frame[n + flat + i] *= right[s - 1 - i];
                }
                frame[n + flat + s..].fill(0.0);
            } else {
                let right = &self.long_win[cur];
                for i in 0..n {
                    frame[n + i] *= right[n - 1 - i];
                }
            }
        }
        for i in 0..n {
            out[i] = ov.samples[i] + frame[i];
        }
        ov.samples.copy_from_slice(&frame[n..]);
        ov.prev_shape = shape;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct_imdct(x: &[f32]) -> Vec<f64> {
        let m = x.len();
        let n = 2 * m;
        let n0 = (m as f64 + 1.0) / 2.0;
        (0..n)
            .map(|i| {
                let s: f64 = x
                    .iter()
                    .enumerate()
                    .map(|(k, &v)| {
                        f64::from(v)
                            * (2.0 * PI / n as f64 * (i as f64 + n0) * (k as f64 + 0.5)).cos()
                    })
                    .sum();
                2.0 / n as f64 * s
            })
            .collect()
    }

    #[test]
    fn fast_imdct_matches_the_definition() {
        let mut planner = FftPlanner::new();
        for m in [1024, 960, 128, 120] {
            let x: Vec<f32> = (0..m).map(|k| ((k * 7919) % 211) as f32 - 105.0).collect();
            let mut imdct = Imdct::new(m, &mut planner);
            let mut out = vec![0.0; 2 * m];
            imdct.run(&x, &mut out);
            let want = direct_imdct(&x);
            let err = out.iter().zip(&want).map(|(a, b)| (f64::from(*a) - b).abs()).fold(0.0, f64::max);
            let peak = want.iter().map(|v| v.abs()).fold(0.0, f64::max);
            assert!(err < peak * 1e-5, "m={m}: error {err} of peak {peak}");
        }
    }

    #[test]
    fn windows_meet_princen_bradley() {
        for m in [1024, 960, 128, 120] {
            for w in [sine_window(m), kbd_window(m, if m > 200 { 4.0 } else { 6.0 })] {
                for i in 0..m {
                    let p = w[i] * w[i] + w[m - 1 - i] * w[m - 1 - i];
                    assert!((p - 1.0).abs() < 1e-5, "m={m} i={i} {p}");
                }
            }
        }
    }

    /// Forward MDCT with the standard's sign and phase, for reconstruction.
    fn mdct(x: &[f64]) -> Vec<f32> {
        let n = x.len();
        let m = n / 2;
        let n0 = (m as f64 + 1.0) / 2.0;
        (0..m)
            .map(|k| {
                x.iter()
                    .enumerate()
                    .map(|(i, &v)| {
                        v * (2.0 * PI / n as f64 * (i as f64 + n0) * (k as f64 + 0.5)).cos()
                    })
                    .sum::<f64>() as f32
                    * 2.0
            })
            .collect()
    }

    /// Analysis matching what `synthesize` undoes, for every sequence.
    fn analyze(signal: &[f64], start: usize, seq: u8, shape: u8, prev: u8, fb: &FilterBank) -> Vec<f32> {
        let (n, s) = (fb.frame_len, fb.short_len);
        let flat = (n - s) / 2;
        let lw = |sh: u8| -> Vec<f64> { fb.long_win[usize::from(sh)].iter().map(|&v| f64::from(v)).collect() };
        let sw = |sh: u8| -> Vec<f64> { fb.short_win[usize::from(sh)].iter().map(|&v| f64::from(v)).collect() };
        if seq == EIGHT_SHORT {
            let mut spec = vec![0.0f32; n];
            for w in 0..8 {
                let base = start + flat + w * s;
                let left = if w == 0 { sw(prev) } else { sw(shape) };
                let right = sw(shape);
                let seg: Vec<f64> = (0..2 * s)
                    .map(|i| signal[base + i] * if i < s { left[i] } else { right[2 * s - 1 - i] })
                    .collect();
                spec[w * s..(w + 1) * s].copy_from_slice(&mdct(&seg));
            }
            return spec;
        }
        let mut win = vec![0.0f64; 2 * n];
        if seq == LONG_STOP {
            let l = sw(prev);
            win[flat..flat + s].copy_from_slice(&l[..s]);
            for v in &mut win[flat + s..n] {
                *v = 1.0;
            }
        } else {
            win[..n].copy_from_slice(&lw(prev));
        }
        if seq == LONG_START {
            let r = sw(shape);
            for v in &mut win[n..n + flat] {
                *v = 1.0;
            }
            for i in 0..s {
                win[n + flat + i] = r[s - 1 - i];
            }
        } else {
            let r = lw(shape);
            for i in 0..n {
                win[n + i] = r[n - 1 - i];
            }
        }
        let seg: Vec<f64> = (0..2 * n).map(|i| signal[start + i] * win[i]).collect();
        mdct(&seg)
    }

    #[test]
    fn every_sequence_reconstructs_perfectly_at_both_frame_lengths() {
        for frame_len in [1024usize, 960] {
            let mut fb = FilterBank::new(frame_len);
            let seqs = [ONLY_LONG, LONG_START, EIGHT_SHORT, EIGHT_SHORT, LONG_STOP, ONLY_LONG, LONG_START, EIGHT_SHORT, LONG_STOP];
            let shapes = [0u8, 1, 1, 0, 1, 0, 1, 1, 0];
            let total = (seqs.len() + 2) * frame_len;
            let signal: Vec<f64> = (0..total)
                .map(|i| {
                    let t = i as f64;
                    8000.0 * (t * 0.031).sin() + 3000.0 * (t * 0.47).cos() + if i % 700 == 0 { 9000.0 } else { 0.0 }
                })
                .collect();
            let mut ov = Overlap::new(frame_len);
            let mut out = vec![0.0f32; frame_len];
            let mut prev = 0u8;
            let mut worst = 0.0f64;
            for (f, (&seq, &shape)) in seqs.iter().zip(&shapes).enumerate() {
                let spec = analyze(&signal, f * frame_len, seq, shape, prev, &fb);
                fb.synthesize(&spec, seq, shape, &mut ov, &mut out);
                prev = shape;
                if f > 0 {
                    // Frame f's first half completes samples f*N .. (f+1)*N.
                    for i in 0..frame_len {
                        let want = signal[f * frame_len + i];
                        worst = worst.max((f64::from(out[i]) - want).abs());
                    }
                }
            }
            assert!(worst < 0.05, "frame {frame_len}: worst error {worst}");
        }
    }
}
