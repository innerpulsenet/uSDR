//! The MT63 receiver proper: the quadrature split, the time and frequency
//! synchronizer, the carrier demodulator and the deinterleaver and Walsh
//! decoder. A port of Pawel Jalocha's code as carried by fldigi
//! (`mt63base.cxx`, `dsp.cxx`), with fldigi's generated Blackman filter
//! rather than the fixed filter tables.
//!
//! Three small corrections to the original, none changing what it does for
//! a signal where it works: the first bin of the data scan and the frequency
//! refinement index are kept in range (the original reads past its arrays
//! when a 500 Hz signal sits at 1500 Hz), and the sliding carrier sum adds
//! the imaginary part of the entering term (the original subtracts it, a
//! slip: it adds it on the way in everywhere else).

use super::tables::{LONG_INTERLEAVE, SHORT_INTERLEAVE, SYMBOL_SHAPE};
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::f64::consts::PI;
use std::sync::Arc;

pub(super) const WINDOW: usize = 512;
const WINDOW_MASK: i64 = (WINDOW - 1) as i64;
pub(super) const SEPAR: usize = 200;
pub(super) const CARR_SEPAR: usize = 4;
pub(super) const CARRIERS: usize = 64;
const SYMBOL_DIV: usize = 4;
const SYNC_STEP: usize = SEPAR / SYMBOL_DIV;
/// Carriers searched either side, by the synchronizer and the decoder.
pub(super) const MARGIN: usize = 8;
const SCAN_LEN: usize = (CARRIERS + 2 * MARGIN) * CARR_SEPAR;
const FIT_LEN: usize = 2 * MARGIN * CARR_SEPAR;
/// Data carriers demodulated: the sent ones and the margin either side.
pub(super) const DATA_SCAN: usize = CARRIERS + 2 * MARGIN;
/// Decoder offsets tried, `-MARGIN..=MARGIN`.
pub(super) const DECODE_SCAN: usize = 2 * MARGIN + 1;

/// Anything between the Walsh peak and the rest of the transform above this
/// is a clean signal; the cap only stops one clean symbol dominating a
/// low-pass after it ends.
const RAW_SNR_CAP: f32 = 30.0;
/// Carriers next to the signal not counted as noise: the skirts of its
/// keying spread into them.
const GUARD: usize = 3;
/// A carrier's power in an FFT bin against a steady tone's (a symbol is
/// partly overlapped by its neighbors): set from the SNR reading test.
const SNR_FACTOR: f32 = 1.0;

/// The filter length and decimation for a bandwidth (500, 1000 or 2000).
pub(super) fn bandwidth_setup(bw: u32) -> (usize, usize) {
    match bw {
        500 => (128, 8),
        1000 => (64, 4),
        _ => (64, 2),
    }
}

fn low_pass2(inp: f32, mid: &mut f32, out: &mut f32, w: [f32; 3]) {
    let sum = *mid + *out;
    let diff = *mid - *out;
    *mid += w[1] * inp - w[0] * sum;
    *out += w[2] * diff;
}

fn low_pass2c(inp: Complex32, mid: &mut Complex32, out: &mut Complex32, w: [f32; 3]) {
    low_pass2(inp.re, &mut mid.re, &mut out.re, w);
    low_pass2(inp.im, &mut mid.im, &mut out.im, w);
}

fn lp_coeff(len: f32) -> [f32; 3] {
    [1.0 / len, 2.0 / len, 5.0 / len]
}

/// `dspWalshTrans`.
pub(super) fn walsh(d: &mut [f32]) {
    let len = d.len();
    let mut step = 1;
    while step < len {
        let mut ptr = 0;
        while ptr < len {
            for p in ptr..ptr + step {
                let (a, b) = (d[p], d[p + step]);
                d[p] = a + b;
                d[p + step] = b - a;
            }
            ptr += 2 * step;
        }
        step *= 2;
    }
}

fn select_mean_real(data: &[f32], thres: f32) -> (f32, f32) {
    let n = data.len() as f32;
    let sum: f32 = data.iter().sum();
    let mut err = data.iter().map(|v| v * v).sum::<f32>() / n;
    let mut lev = sum / n;
    // Absolute value: with every entry equal, rounding can leave the
    // variance a hair under zero, and then nothing would be included.
    err = (err - lev * lev).abs();
    for _ in 0..4 {
        let t = thres * thres * err;
        let mut s = 0.0;
        let mut e = 0.0;
        let mut inc = 0.0;
        for &v in data {
            let d = (v - lev) * (v - lev);
            if d <= t {
                s += v;
                e += d;
                inc += 1.0;
            }
        }
        if inc == 0.0 {
            return (lev, err.sqrt());
        }
        let dl = s / inc - lev;
        e /= inc;
        err = (e - dl * dl).abs();
        lev += dl;
    }
    (lev, err.sqrt())
}

fn select_mean_cmpx(data: &[Complex32], thres: f32) -> Complex32 {
    let n = data.len() as f32;
    let mut sum = Complex32::new(0.0, 0.0);
    let mut err = 0.0;
    for &v in data {
        sum += v;
        err += v.norm_sqr();
    }
    let mut lev = sum / n;
    err = (err / n - lev.norm_sqr()).abs();
    for _ in 0..4 {
        let t = 0.5 * thres * thres * err;
        let mut s = Complex32::new(0.0, 0.0);
        let mut e = 0.0;
        let mut inc = 0.0;
        for &v in data {
            let d = (v - lev).norm_sqr();
            if d <= t {
                s += v;
                e += d;
                inc += 1.0;
            }
        }
        if inc == 0.0 {
            return lev;
        }
        let dl = s / inc - lev;
        e /= inc;
        err = (e - dl.norm_sqr()).abs();
        lev += dl;
    }
    lev
}

fn phase(z: Complex32) -> f32 {
    z.im.atan2(z.re)
}

/// One symbol's worth of what the decoder decided.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Decoded {
    pub code: u8,
    /// Walsh peak over the RMS of the rest, this symbol (capped).
    pub raw: f32,
    /// The smoothed `raw` of the offset chosen.
    pub smooth: f32,
    /// Carriers off from where the receiver was set.
    pub offset: i32,
}

/// `MT63decoder`: deinterleaver and Walsh decoder at every offset.
pub(super) struct Decoder {
    /// Interleave delay of each carrier, in carriers.
    patt: [usize; CARRIERS],
    pipe: Vec<f32>,
    size: usize,
    ptr: usize,
    walsh: [f32; CARRIERS],
    w: [f32; 3],
    mid: [f32; DECODE_SCAN],
    out: [f32; DECODE_SCAN],
    /// `(code, raw)` per offset, `DecodeLen` symbols deep.
    decode: Vec<(u8, f32)>,
    decode_ptr: usize,
}

impl Decoder {
    pub(super) fn new(long: bool, integ: usize) -> Self {
        let (interleave, pattern) = if long {
            (64, &LONG_INTERLEAVE)
        } else {
            (32, &SHORT_INTERLEAVE)
        };
        let mut patt = [0usize; CARRIERS];
        let mut p = 0;
        for i in 0..CARRIERS {
            patt[i] = p * DATA_SCAN;
            p += pattern[i];
            if p >= interleave {
                p -= interleave;
            }
        }
        let size = (interleave + 1) * DATA_SCAN;
        Self {
            patt,
            pipe: vec![0.0; size],
            size,
            ptr: 0,
            walsh: [0.0; CARRIERS],
            w: lp_coeff(integ as f32),
            mid: [0.0; DECODE_SCAN],
            out: [0.0; DECODE_SCAN],
            decode: vec![(0, 0.0); (integ / 2) * DECODE_SCAN],
            decode_ptr: 0,
        }
    }

    pub(super) fn process(&mut self, data: &[f32; DATA_SCAN]) -> Decoded {
        self.pipe[self.ptr..self.ptr + DATA_SCAN].copy_from_slice(data);
        for s in 0..DECODE_SCAN {
            for i in 0..CARRIERS {
                let mut k = self.ptr as isize - DATA_SCAN as isize - self.patt[i] as isize;
                if k < 0 {
                    k += self.size as isize;
                }
                let mut k = k as usize;
                if (s & 1) != 0 && (i & 1) != 0 {
                    k += DATA_SCAN;
                    if k >= self.size {
                        k -= self.size;
                    }
                }
                self.walsh[i] = self.pipe[k + s + i];
            }
            walsh(&mut self.walsh);
            let (mut min, mut max, mut min_at, mut max_at) = (f32::MAX, f32::MIN, 0, 0);
            for (j, &v) in self.walsh.iter().enumerate() {
                if v < min {
                    min = v;
                    min_at = j;
                }
                if v > max {
                    max = v;
                    max_at = j;
                }
            }
            let (code, sig) = if max.abs() > min.abs() {
                self.walsh[max_at] = 0.0;
                ((max_at + CARRIERS) as u8, max.abs())
            } else {
                self.walsh[min_at] = 0.0;
                (min_at as u8, min.abs())
            };
            let rms = (self.walsh.iter().map(|v| v * v).sum::<f32>() / CARRIERS as f32).sqrt();
            let raw = if rms > 0.0 {
                (sig / rms).min(RAW_SNR_CAP)
            } else {
                0.0
            };
            low_pass2(raw, &mut self.mid[s], &mut self.out[s], self.w);
            self.decode[self.decode_ptr + s] = (code, raw);
        }
        self.ptr += DATA_SCAN;
        if self.ptr >= self.size {
            self.ptr = 0;
        }
        self.decode_ptr += DECODE_SCAN;
        if self.decode_ptr >= self.decode.len() {
            self.decode_ptr = 0;
        }
        let (mut best, mut at) = (f32::MIN, 0);
        for (s, &v) in self.out.iter().enumerate() {
            if v > best {
                best = v;
                at = s;
            }
        }
        let (code, raw) = self.decode[self.decode_ptr + at];
        Decoded {
            code,
            raw,
            smooth: best,
            offset: at as i32 - MARGIN as i32,
        }
    }
}

/// `MT63rx` up to the decoder: everything shared by the two interleaves.
pub(super) struct Front {
    bin_hz: f32,
    /// Noise bandwidth of one FFT bin as windowed, in Hz.
    pub(super) bin_noise_hz: f32,
    rate: usize,
    taps_i: Vec<f32>,
    taps_q: Vec<f32>,
    tap: Vec<f32>,
    /// Decimated I/Q, starting at absolute sample `line_base`.
    line: Vec<Complex32>,
    line_base: i64,
    sync_ptr: i64,
    data_ptr: i64,
    delay: i64,
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
    buf: Vec<Complex32>,
    buf2: Vec<Complex32>,
    twiddle: Vec<Complex32>,
    scan_first: i64,
    // Synchronizer.
    sync_pipe: [Vec<Complex32>; SYMBOL_DIV],
    ph_corr: Vec<Complex32>,
    correl_mid: [Vec<Complex32>; SYMBOL_DIV],
    correl_out: [Vec<Complex32>; SYMBOL_DIV],
    power_mid: Vec<f32>,
    power_out: Vec<f32>,
    correl_norm: [Vec<Complex32>; SYMBOL_DIV],
    correl_aver: [Vec<Complex32>; SYMBOL_DIV],
    symb_fit: Vec<Complex32>,
    symb_fit_pos: i32,
    w: [f32; 3],
    wp: [f32; 3],
    sync_ptr_i: usize,
    symb_ptr: usize,
    symb_pipe: Vec<Complex32>,
    freq_pipe: Vec<f32>,
    track_ptr: usize,
    aver_symb: Complex32,
    aver_freq: f32,
    pub(super) locked: bool,
    pub(super) conf: f32,
    pub(super) freq_ofs: f32,
    pub(super) freq_dev: f32,
    symb_shift: f32,
    hold_thres: f32,
    lock_thres: f32,
    // Data.
    data_first: i64,
    ref_slice: Vec<Complex32>,
    pwr_mid: Vec<f32>,
    pwr_out: Vec<f32>,
    dw: [f32; 3],
    data_pipe: Vec<[Complex32; DATA_SCAN]>,
    data_pipe_ptr: usize,
    data_vect: [Complex32; DATA_SCAN],
    phases: [f32; DATA_SCAN],
    /// Absolute symbols demodulated, for the gate's clock.
    pub(super) symbols: u64,
}

impl Front {
    pub(super) fn new(bw: u32, center_hz: f32, integ: usize, data_delay: usize) -> Self {
        let (filter_len, rate) = bandwidth_setup(bw);
        let hbw = 1.5 * bw as f64 / 2.0;
        let low = ((center_hz as f64 - hbw).max(100.0)) * PI / 4000.0;
        let high = ((center_hz as f64 + hbw).min(4000.0)) * PI / 4000.0;
        let (taps_i, taps_q) = quadr_shape(low, high, filter_len);
        let first_carr =
            ((center_hz as f64 - bw as f64 / 2.0) * WINDOW as f64 * rate as f64 / 8000.0 + 0.5)
                .floor() as i64;
        let delay = (data_delay * SEPAR) as i64;
        let hist = delay as usize + WINDOW + SEPAR;
        let fft = FftPlanner::new().plan_fft_forward(WINDOW);
        let scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let twiddle: Vec<Complex32> = (0..WINDOW)
            .map(|i| {
                let ph = 2.0 * PI * i as f64 / WINDOW as f64;
                Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let mut scan_first = first_carr - (MARGIN * CARR_SEPAR) as i64;
        if scan_first < 0 {
            scan_first += WINDOW as i64;
        }
        // The phase a carrier turns between one slice's square and the
        // next's, for each bin scanned.
        let ph_corr: Vec<Complex32> = (0..SCAN_LEN)
            .map(|i| {
                let c = ((scan_first + i as i64) * SEPAR as i64) & WINDOW_MASK;
                let t = twiddle[c as usize];
                Complex32::new(t.re * t.re - t.im * t.im, 2.0 * t.re * t.im)
            })
            .collect();
        let zc = Complex32::new(0.0, 0.0);
        let vz = || vec![zc; SCAN_LEN];
        let hold_thres = 1.5 * (1.0 / (integ * CARRIERS) as f32).sqrt();
        let enbw_bins = WINDOW as f32 * SYMBOL_SHAPE.iter().map(|v| v * v).sum::<f32>()
            / SYMBOL_SHAPE.iter().sum::<f32>().powi(2);
        let bin_hz = 8000.0 / rate as f32 / WINDOW as f32;
        Self {
            bin_hz,
            bin_noise_hz: enbw_bins * bin_hz,
            rate,
            taps_i,
            taps_q,
            tap: Vec::with_capacity(4096),
            line: vec![zc; hist],
            line_base: 0,
            sync_ptr: hist as i64,
            data_ptr: hist as i64 - delay,
            delay,
            fft,
            scratch,
            buf: vec![zc; WINDOW],
            buf2: vec![zc; WINDOW],
            twiddle,
            scan_first,
            sync_pipe: std::array::from_fn(|_| vz()),
            ph_corr,
            correl_mid: std::array::from_fn(|_| vz()),
            correl_out: std::array::from_fn(|_| vz()),
            power_mid: vec![0.0; SCAN_LEN],
            power_out: vec![0.0; SCAN_LEN],
            correl_norm: std::array::from_fn(|_| vz()),
            correl_aver: std::array::from_fn(|_| vec![zc; FIT_LEN]),
            symb_fit: vec![zc; FIT_LEN],
            symb_fit_pos: (MARGIN * CARR_SEPAR) as i32,
            w: lp_coeff(integ as f32),
            wp: lp_coeff((integ * SYMBOL_DIV) as f32),
            sync_ptr_i: 0,
            symb_ptr: 0,
            symb_pipe: vec![zc; integ],
            freq_pipe: vec![0.0; integ],
            track_ptr: 0,
            aver_symb: zc,
            aver_freq: 0.0,
            locked: false,
            conf: 0.0,
            freq_ofs: 0.0,
            freq_dev: 0.0,
            symb_shift: 0.0,
            hold_thres,
            lock_thres: 1.5 * hold_thres,
            data_first: first_carr - (MARGIN * CARR_SEPAR) as i64,
            ref_slice: vec![zc; DATA_SCAN],
            pwr_mid: vec![0.0; DATA_SCAN],
            pwr_out: vec![0.0; DATA_SCAN],
            dw: lp_coeff(integ as f32),
            data_pipe: vec![[zc; DATA_SCAN]; integ / 2],
            data_pipe_ptr: 0,
            data_vect: [zc; DATA_SCAN],
            phases: [0.0; DATA_SCAN],
            symbols: 0,
        }
    }

    /// Hz per carrier-offset of the decoder.
    pub(super) fn carrier_hz(&self) -> f32 {
        CARR_SEPAR as f32 * self.bin_hz
    }

    pub(super) fn bin_hz(&self) -> f32 {
        self.bin_hz
    }

    /// 8 kHz real samples in; each demodulated symbol goes to `sink`.
    pub(super) fn push(&mut self, x: &[f32], mut sink: impl FnMut(&[f32; DATA_SCAN], &Front)) {
        self.tap.extend_from_slice(x);
        let len = self.taps_i.len();
        let mut i = 0;
        while i + len < self.tap.len() {
            let (mut re, mut im) = (0.0f32, 0.0f32);
            for t in 0..len {
                re += self.tap[i + t] * self.taps_i[t];
                im += self.tap[i + t] * self.taps_q[t];
            }
            self.line.push(Complex32::new(re, im));
            i += self.rate;
        }
        self.tap.drain(..i);

        while self.sync_ptr + (WINDOW as i64) < self.line_base + self.line.len() as i64 {
            self.sync_process();
            if self.sync_ptr_i == self.symb_ptr {
                let s1 = self.sync_ptr - self.delay
                    + (self.symb_shift as i64 - self.symb_ptr as i64 * SYNC_STEP as i64);
                let s2 = s1 + (SEPAR / 2) as i64;
                self.data_process(s1, s2);
                sink(&self.phases, self);
                self.symbols += 1;
            }
            self.sync_ptr += SYNC_STEP as i64;
        }
        // Keep what later slices can reach back to.
        let keep = self.sync_ptr - self.delay - SEPAR as i64 - 8;
        let drop = (keep - self.line_base).max(0) as usize;
        if drop > 4096 {
            self.line.drain(..drop);
            self.line_base += drop as i64;
        }
    }

    fn sync_process(&mut self) {
        self.sync_ptr_i = (self.sync_ptr_i + 1) & (SYMBOL_DIV - 1);
        let sp = self.sync_ptr_i;
        let start = (self.sync_ptr - self.line_base) as usize;
        for i in 0..WINDOW {
            self.buf[i] = self.line[start + i] * SYMBOL_SHAPE[i];
        }
        self.fft
            .process_with_scratch(&mut self.buf, &mut self.scratch);

        for i in 0..SCAN_LEN {
            let k = ((self.scan_first + i as i64) & WINDOW_MASK) as usize;
            let z = self.buf[k];
            let p = z.norm_sqr();
            let a = p.sqrt();
            let d = if p > 0.0 {
                Complex32::new((z.re * z.re - z.im * z.im) / a, 2.0 * z.re * z.im / a)
            } else {
                Complex32::new(0.0, 0.0)
            };
            low_pass2(p, &mut self.power_mid[i], &mut self.power_out[i], self.wp);
            let prev = self.sync_pipe[sp][i] * self.ph_corr[i];
            let correl = d * prev.conj();
            low_pass2c(
                correl,
                &mut self.correl_mid[sp][i],
                &mut self.correl_out[sp][i],
                self.w,
            );
            self.sync_pipe[sp][i] = d;
        }
        if sp == self.symb_ptr ^ 2 {
            self.fit();
        }
    }

    /// The carrier-pattern fit, symbol-shift and frequency estimates, and
    /// the lock decision (the `if (SyncPtr==(SymbPtr^2))` part of
    /// `SyncProcess`).
    fn fit(&mut self) {
        for s in 0..SYMBOL_DIV {
            for i in 0..SCAN_LEN {
                self.correl_norm[s][i] = if self.power_out[i] > 0.0 {
                    self.correl_out[s][i] / self.power_out[i]
                } else {
                    Complex32::new(0.0, 0.0)
                };
            }
        }
        for s in 0..SYMBOL_DIV {
            let s2 = (s + SYMBOL_DIV / 2) & (SYMBOL_DIV - 1);
            for k in 0..2 * CARR_SEPAR {
                do_correl_sum(
                    &self.correl_norm[s][k..],
                    &self.correl_norm[s2][k + CARR_SEPAR..],
                    &mut self.correl_aver[s][k..],
                );
            }
        }
        for i in 0..FIT_LEN {
            self.symb_fit[i] = Complex32::new(
                self.correl_aver[0][i].norm() - self.correl_aver[2][i].norm(),
                self.correl_aver[1][i].norm() - self.correl_aver[3][i].norm(),
            );
        }
        let (mut p, mut j) = (f32::MIN, 2i32);
        for (i, z) in self.symb_fit[2..FIT_LEN - 2].iter().enumerate() {
            let v = z.norm_sqr();
            if v > p {
                p = v;
                j = i as i32 + 2;
            }
        }
        let k = (j - self.symb_fit_pos) / CARR_SEPAR as i32;
        if k > 1 {
            j -= (k - 1) * CARR_SEPAR as i32;
        } else if k < -1 {
            j -= (k + 1) * CARR_SEPAR as i32;
        }
        self.symb_fit_pos = j;
        let (mut symb_time, mut freq_ofs);
        if p > 0.0 {
            let f = &self.symb_fit;
            let u = j as usize;
            let i2 = f[u].re + 0.5 * (f[u - 1].re + f[u + 1].re);
            let q2 = f[u].im + 0.5 * (f[u - 1].im + f[u + 1].im);
            symb_time = Complex32::new(i2, q2);
            let mut shift = phase(symb_time) / (2.0 * std::f32::consts::PI) * SYMBOL_DIV as f32;
            if shift < 0.0 {
                shift += SYMBOL_DIV as f32;
            }
            let dot = |z: Complex32| i2 * z.re + q2 * z.im;
            let p_i = dot(f[u]) + 0.7 * dot(f[u - 1]) + 0.7 * dot(f[u + 1]);
            let p_q = 0.7 * dot(f[u + 1]) - 0.7 * dot(f[u - 1]) + 0.5 * dot(f[u + 2])
                - 0.5 * dot(f[u - 2]);
            freq_ofs = j as f32 + p_q.atan2(p_i) / (2.0 * std::f32::consts::PI / 8.0);
            // Refine it.
            let i = ((freq_ofs + 0.5).floor() as i32).clamp(0, FIT_LEN as i32 - 1) as usize;
            let s = (shift.floor() as usize) & (SYMBOL_DIV - 1);
            let s2 = (s + 1) & (SYMBOL_DIV - 1);
            let w0 = s as f32 + 1.0 - shift;
            let w1 = shift - s as f32;
            let a = (0.5 * WINDOW as f32) / SEPAR as f32;
            let z = self.correl_aver[s][i] * w0 + self.correl_aver[s2][i] * w1;
            let f0 = i as f32 + phase(z) / (2.0 * std::f32::consts::PI) * a - freq_ofs;
            freq_ofs += nearest(f0 - a, f0, f0 + a);
        } else {
            symb_time = Complex32::new(0.0, 0.0);
            freq_ofs = 0.0;
        }

        if self.locked {
            if scal(symb_time, self.aver_symb) < 0.0 {
                symb_time = -symb_time;
                freq_ofs -= CARR_SEPAR as f32;
            }
            let a = (2 * CARR_SEPAR) as f32;
            let k = ((freq_ofs - self.aver_freq) / a + 0.5).floor();
            freq_ofs -= k * a;
            let a = (0.5 * WINDOW as f32) / SEPAR as f32;
            let f0 = freq_ofs - self.aver_freq;
            freq_ofs += a_step(f0, a);
        } else {
            if scal(symb_time, self.symb_pipe[self.track_ptr]) < 0.0 {
                symb_time = -symb_time;
                freq_ofs -= CARR_SEPAR as f32;
            }
            let a = (2 * CARR_SEPAR) as f32;
            let k = (freq_ofs / a + 0.5).floor();
            freq_ofs -= k * a;
            let f0 = freq_ofs - self.freq_pipe[self.track_ptr];
            freq_ofs += a_step(f0, a);
        }
        self.track_ptr = (self.track_ptr + 1) % self.symb_pipe.len();
        self.symb_pipe[self.track_ptr] = symb_time;
        self.freq_pipe[self.track_ptr] = freq_ofs;
        self.aver_symb = select_mean_cmpx(&self.symb_pipe, 3.0);
        let (aver_freq, rms) = select_mean_real(&self.freq_pipe, 2.5);
        self.aver_freq = aver_freq;
        self.freq_dev = rms;

        let conf = self.aver_symb.norm();
        self.conf = conf;
        self.freq_ofs = self.aver_freq;
        if conf > 0.0 {
            let ph = phase(self.aver_symb) / (2.0 * std::f32::consts::PI);
            let mut shift = ph * SEPAR as f32;
            if shift < 0.0 {
                shift += SEPAR as f32;
            }
            let mut ptr = (ph * SYMBOL_DIV as f32).floor() as i32;
            if ptr < 0 {
                ptr += SYMBOL_DIV as i32;
            }
            self.symb_ptr = ptr as usize & (SYMBOL_DIV - 1);
            self.symb_shift = shift;
        }
        if self.locked {
            if conf < self.hold_thres || self.freq_dev > 0.250 {
                self.locked = false;
            }
        } else if conf > self.lock_thres && self.freq_dev < 0.125 {
            self.locked = true;
        }
    }

    fn data_process(&mut self, s1: i64, s2: i64) {
        let time_dist = s1 - self.data_ptr;
        self.data_ptr = s1;
        let a = (s1 - self.line_base) as usize;
        let b = (s2 - self.line_base) as usize;
        let step = -2.0 * PI * self.freq_ofs as f64 / WINDOW as f64;
        let (mut re, mut im) = (1.0f64, 0.0f64);
        let (fc, fs) = (step.cos(), step.sin());
        for i in 0..WINDOW {
            let rot = Complex32::new(re as f32, im as f32);
            self.buf[i] = self.line[a + i] * rot * SYMBOL_SHAPE[i];
            self.buf2[i] = self.line[b + i] * rot * SYMBOL_SHAPE[i];
            let (r2, i2) = (re * fc - im * fs, re * fs + im * fc);
            re = r2;
            im = i2;
        }
        self.fft
            .process_with_scratch(&mut self.buf, &mut self.scratch);
        self.fft
            .process_with_scratch(&mut self.buf2, &mut self.scratch);

        let incr = ((time_dist * CARR_SEPAR as i64) & WINDOW_MASK) as usize;
        let mut p = ((time_dist * self.data_first) & WINDOW_MASK) as usize;
        let mut c = (self.data_first & WINDOW_MASK) as usize;
        let mut i = 0;
        while i < DATA_SCAN {
            for even in [true, false] {
                let z = if even { self.buf[c] } else { self.buf2[c] };
                let refz = self.ref_slice[i] * self.twiddle[p];
                self.data_vect[i] = z * refz.conj();
                low_pass2(
                    z.norm_sqr(),
                    &mut self.pwr_mid[i],
                    &mut self.pwr_out[i],
                    self.dw,
                );
                self.ref_slice[i] = z;
                c = (c + CARR_SEPAR) & (WINDOW_MASK as usize);
                p = (p + incr) & (WINDOW_MASK as usize);
                i += 1;
            }
        }
        let ph = -(time_dist as f64) * 2.0 * PI * self.freq_ofs as f64 / WINDOW as f64;
        let rot = Complex32::new(ph.cos() as f32, ph.sin() as f32);
        for i in 0..DATA_SCAN {
            let f = self.data_vect[i] * rot;
            self.data_vect[i] = self.data_pipe[self.data_pipe_ptr][i];
            self.data_pipe[self.data_pipe_ptr][i] = f;
        }
        self.data_pipe_ptr = (self.data_pipe_ptr + 1) % self.data_pipe.len();
        for i in 0..DATA_SCAN {
            self.phases[i] = if self.pwr_out[i] > 0.0 {
                (self.data_vect[i].re / self.pwr_out[i]).clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
    }

    /// Signal to noise in 2500 Hz, in dB, from the demodulator's mean power
    /// per carrier: the carriers `offset` puts on the signal against those
    /// outside it (the scan's margins, less one carrier of guard). None when
    /// the search has gone so far that no margin is left to measure.
    pub(super) fn snr_db(&self, offset: i32) -> Option<f32> {
        let lo = (MARGIN as i32 + offset) as usize;
        let hi = lo + CARRIERS;
        let mut noise: Vec<f32> = self
            .pwr_out
            .iter()
            .enumerate()
            .filter(|(i, _)| *i + GUARD < lo || *i >= hi + GUARD)
            .map(|(_, p)| *p)
            .collect();
        if noise.len() < 3 {
            return None;
        }
        noise.sort_unstable_by(f32::total_cmp);
        let n = noise[noise.len() / 2].max(1e-20);
        let total: f32 = self.pwr_out[lo..hi].iter().map(|p| (p - n).max(0.0)).sum();
        Some(
            (10.0 * (SNR_FACTOR * total * self.bin_noise_hz / (2500.0 * n)).log10())
                .clamp(-30.0, 60.0),
        )
    }
}

fn scal(a: Complex32, b: Complex32) -> f32 {
    a.re * b.re + a.im * b.im
}

/// The step, of -a, 0 or +a, that brings `f0` nearest zero.
fn a_step(f0: f32, a: f32) -> f32 {
    let (fl, fu) = (f0 - a, f0 + a);
    if fl.abs() < f0.abs() {
        if fu.abs() < fl.abs() { a } else { -a }
    } else if fu.abs() < f0.abs() {
        a
    } else {
        0.0
    }
}

fn nearest(fl: f32, f0: f32, fu: f32) -> f32 {
    if fl.abs() < f0.abs() {
        if fu.abs() < fl.abs() { fu } else { fl }
    } else if fu.abs() < f0.abs() {
        fu
    } else {
        f0
    }
}

fn do_correl_sum(c1: &[Complex32], c2: &[Complex32], aver: &mut [Complex32]) {
    let s = 2 * CARR_SEPAR;
    let d = CARRIERS * CARR_SEPAR;
    let mut sx = Complex32::new(0.0, 0.0);
    let mut i = 0;
    while i < d {
        sx += c1[i];
        sx += c2[i];
        i += s;
    }
    aver[0] = sx / CARRIERS as f32;
    let mut i = 0;
    while i < FIT_LEN - s {
        sx -= c1[i];
        sx -= c2[i];
        sx += c1[i + d];
        sx += c2[i + d];
        i += s;
        aver[i] = sx / CARRIERS as f32;
    }
}

/// `dspWinFirI` and `WinFirQ` with the 3-term Blackman window.
fn quadr_shape(low: f64, high: f64, len: usize) -> (Vec<f32>, Vec<f32>) {
    let blackman3 = |ph: f64| {
        0.35875 + 0.48829 * ph.cos() + 0.14128 * (2.0 * ph).cos() + 0.01168 * (3.0 * ph).cos()
    };
    let mut ti = vec![0.0f32; len];
    let mut tq = vec![0.0f32; len];
    for i in 0..len {
        let time = i as f64 + 1.0 - len as f64 / 2.0;
        let win = blackman3(2.0 * PI * time / len as f64);
        let (si, sq) = if time == 0.0 {
            (high - low, 0.0)
        } else {
            (
                ((high * time).sin() - (low * time).sin()) / time,
                (-(high * time).cos() + (low * time).cos()) / time,
            )
        };
        ti[i] = (si * win / PI) as f32;
        tq[i] = (-sq * win / PI) as f32;
    }
    (ti, tq)
}

/// The filter and FFT shared with the tests' transmitter, as fldigi uses
/// them for both directions.
#[cfg(test)]
pub(super) fn tx_setup(bw: u32, center_hz: f32) -> (Vec<f32>, Vec<f32>, usize) {
    let (filter_len, rate) = bandwidth_setup(bw);
    let hbw = 1.5 * bw as f64 / 2.0;
    let low = ((center_hz as f64 - hbw).max(100.0)) * PI / 4000.0;
    let high = ((center_hz as f64 + hbw).min(4000.0)) * PI / 4000.0;
    let (ti, tq) = quadr_shape(low, high, filter_len);
    (ti, tq, rate)
}
