//! Channel estimation from the gain references, and equalization.
//!
//! The gain references sit on a diagonal grid: carrier k0 + x (s mod y) +
//! x y p in symbol s, so each carrier of the form k0 + x n carries one
//! every y symbols, and every symbol has one every x y carriers. The
//! estimate is built in two steps, the way most DRM receivers do it:
//!
//! 1. In time: each pilot-bearing carrier's estimate (received / known)
//!    is interpolated linearly between its pilots, y symbols apart. That
//!    needs the next pilot, so a symbol is equalized y - 1 symbols after it
//!    arrives. Linear interpolation follows fading up to a few hertz of
//!    Doppler (mode B's pilots are 80 ms apart on each carrier).
//! 2. In frequency: every carrier from the ten nearest pilot carriers (x
//!    apart), with Wiener weights for a delay profile spread evenly over a
//!    window, so the band edges, where the standard boosts the pilots, are
//!    handled like the rest. The window is as wide as the echoes are: the
//!    power delay profile is measured all along (an inverse FFT of the
//!    pilot carriers' estimates, averaged), and the weights picked for the
//!    narrowest window that holds it, up to the whole guard interval. A
//!    narrow window averages more pilots together, and on a channel
//!    without echoes is worth a decibel and a half of SNR at 64-QAM. The
//!    noise the weights allow for comes from the same profile's floor.
//!    The mean delay is measured from the phase step between pilot
//!    carriers (averaged over a few symbols) and turned out of the
//!    estimate before interpolating, so the window sits on the echoes; it
//!    is also what the timing loop steers on.
//!
//! The scatter of each symbol's pilots about the smoothed estimate gives
//! the noise on a pilot, hence the SNR the receiver reports.
//!
//! Each cell comes out as the equalized value and the channel power |H|^2,
//! which weighs its soft bits.

use super::cellmap::CellMap;
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::collections::VecDeque;
use std::f32::consts::PI;
use std::sync::Arc;

/// Pilot carriers each carrier is interpolated from.
const NEAREST: usize = 10;
/// Window widths the weights are designed for, as shares of the guard
/// interval.
const WINDOWS: [f64; 8] = [1.0, 0.71, 0.5, 0.35, 0.25, 0.18, 0.125, 0.09];
/// Noise to signal ratios they are designed for.
const NSRS: [f64; 4] = [0.003, 0.01, 0.03, 0.1];
/// Symbols of profile averaged before the window may narrow, and symbols a
/// narrower window must be called for before it is taken.
const PDP_WARMUP: u32 = 30;
const NARROW_HOLD: u32 = 20;
/// Share of the profile's energy the window must hold.
const PDP_SHARE: f32 = 0.995;
/// Smoothing of the mean delay, per symbol.
const DELAY_ALPHA: f32 = 0.2;

pub struct Equalized {
    /// Frame symbol index of this symbol.
    pub s: usize,
    pub z: Vec<Complex32>,
    pub w: Vec<f32>,
}

/// Wiener weights for one window and noise level: for each carrier, the
/// pilot carriers it is made of and their weights.
type Weights = Vec<Vec<(u16, f32)>>;

pub struct ChanEst {
    ncar: usize,
    x: i32,
    y: usize,
    kmin: i32,
    nu: f32,
    tg: f64,
    pilot_car: Vec<usize>,
    car_pilot: Vec<Option<usize>>,
    hist: Vec<[(i64, Complex32); 2]>,
    weights: Vec<Option<Weights>>,
    ring: VecDeque<(i64, usize, Vec<Complex32>)>,
    /// Power delay profile over the pilot carriers' span (bins of
    /// tu / (x m) samples, circular), averaged.
    pdp: Vec<f32>,
    pdp_fft: Arc<dyn Fft<f32>>,
    pdp_n: u32,
    window: usize,
    nsr_level: usize,
    narrow_for: u32,
    /// Mean delay of the last symbol estimated, samples (positive: the
    /// signal arrives later than the timing assumes).
    pub delay: f32,
    /// The rms delay spread of the averaged delay profile, samples, the
    /// noise floor and the analysis window's own width taken out.
    pub spread: f32,
    /// Noise against signal of the pilot carriers' estimates, from the
    /// profile's floor.
    pub nsr: f32,
    /// Noise against signal on a single pilot: its scatter about the
    /// smoothed estimate. Pilots have twice a data cell's power, so a data
    /// cell's SNR is 1 / (2 pilot_nsr).
    pub pilot_nsr: f32,
    pilot_err: f32,
    pilot_sig: f32,
    /// Variance (bins^2) of a single path's profile: the Hann window's.
    kernel_var: f32,
    /// The same between one carrier's successive pilots, y symbols apart:
    /// 1 for a still channel, lower with Doppler spread.
    pub trho: f32,
    /// The channel's turn from the last symbol estimated to this one, at
    /// the pilot carriers, with the last symbol's estimate turned to this
    /// one's timing (see `shift`): the residual frequency offset's work
    /// and the paths' own fading, none of the timing loop's. (Taking out
    /// each symbol's own mean delay instead would not do: as echoes fade
    /// against each other the mean delay moves by several samples a
    /// symbol, and the two turns' difference across the band can add up
    /// to a negative sum.)
    pub turn: Complex32,
    prev_hp: Vec<Complex32>,
    /// The phase step between pilot carriers x apart, averaged: the mean
    /// delay.
    dacc: Complex32,
    tacc: Complex32,
    tpow: f32,
}

fn hann(n: usize, len: usize) -> f32 {
    0.5 - 0.5 * (2.0 * PI * (n as f32 + 0.5) / len as f32).cos()
}

/// Mean and variance, in bins, of a circular delay profile (bins past the
/// middle are negative delays).
fn profile_var(p: &[f32]) -> (f32, f32) {
    let m = p.len();
    let d = |j: usize| if j < m / 2 { j as f32 } else { j as f32 - m as f32 };
    let total: f32 = p.iter().sum();
    if total <= 0.0 {
        return (0.0, 0.0);
    }
    let mean = p.iter().enumerate().map(|(j, &v)| v * d(j)).sum::<f32>() / total;
    let var = p.iter().enumerate().map(|(j, &v)| v * (d(j) - mean).powi(2)).sum::<f32>() / total;
    (mean, var)
}

fn solve(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Vec<f64> {
    let n = b.len();
    for c in 0..n {
        let p = (c..n).max_by(|&i, &j| a[i][c].abs().total_cmp(&a[j][c].abs())).unwrap_or(c);
        a.swap(c, p);
        b.swap(c, p);
        let d = a[c][c];
        if d.abs() < 1e-12 {
            continue;
        }
        let pivot = a[c].clone();
        for r in 0..n {
            if r != c {
                let f = a[r][c] / d;
                if f != 0.0 {
                    for (x, p) in a[r][c..].iter_mut().zip(&pivot[c..]) {
                        *x -= f * p;
                    }
                    b[r] -= f * b[c];
                }
            }
        }
    }
    (0..n).map(|i| if a[i][i].abs() < 1e-12 { 0.0 } else { b[i] / a[i][i] }).collect()
}

impl ChanEst {
    pub fn new(map: &CellMap) -> Self {
        let (x, y, _) = map.mode.grid();
        let ncar = map.ncar;
        let pilot_car: Vec<usize> = (0..ncar).filter(|&i| (0..map.ns).any(|s| map.on_grid(s, i))).collect();
        let mut car_pilot = vec![None; ncar];
        for (j, &i) in pilot_car.iter().enumerate() {
            car_pilot[i] = Some(j);
        }
        // The profile's FFT spans the pilot carriers' grid, gaps (mode D's
        // DC) included.
        let span = (pilot_car.last().unwrap_or(&0) - pilot_car.first().unwrap_or(&0)) / x as usize + 1;
        let m = span.next_power_of_two();
        let np = pilot_car.len();
        let pdp_fft = FftPlanner::new().plan_fft_inverse(m);
        // A single path's profile is the window's transform.
        let mut k: Vec<Complex32> =
            (0..m).map(|n| Complex32::new(if n < span { hann(n, span) } else { 0.0 }, 0.0)).collect();
        pdp_fft.process(&mut k);
        let p: Vec<f32> = k.iter().map(|c| c.norm_sqr()).collect();
        let kernel_var = profile_var(&p).1;
        ChanEst {
            ncar,
            x,
            y: y as usize,
            kmin: map.kmin,
            nu: map.mode.tu() as f32,
            tg: map.mode.tg() as f64,
            pilot_car,
            car_pilot,
            hist: vec![[(i64::MIN, Complex32::new(0.0, 0.0)); 2]; np],
            weights: vec![None; WINDOWS.len() * NSRS.len()],
            ring: VecDeque::new(),
            pdp: vec![0.0; m],
            pdp_fft,
            pdp_n: 0,
            window: 0,
            nsr_level: 1,
            narrow_for: 0,
            delay: 0.0,
            spread: 0.0,
            nsr: 0.0,
            pilot_nsr: 0.0,
            pilot_err: 0.0,
            pilot_sig: 0.0,
            kernel_var,
            trho: 1.0,
            turn: Complex32::new(0.0, 0.0),
            prev_hp: Vec::new(),
            dacc: Complex32::new(0.0, 0.0),
            tacc: Complex32::new(0.0, 0.0),
            tpow: 0.0,
        }
    }

    /// Wiener weights for window `w` (an index into `WINDOWS`) and noise
    /// level `n`: the channel's correlation between carriers dk apart, for
    /// delays spread evenly over a window W samples wide, is sinc(dk W/Tu).
    fn design(&self, w: usize, n: usize) -> Weights {
        let spread = WINDOWS[w] * self.tg / self.nu as f64;
        let nsr = NSRS[n];
        let corr = |dk: f64| {
            let v = std::f64::consts::PI * dk * spread;
            if v.abs() < 1e-9 { 1.0 } else { v.sin() / v }
        };
        let pc = &self.pilot_car;
        (0..self.ncar)
            .map(|i| {
                let mut near: Vec<usize> = (0..pc.len()).collect();
                near.sort_by_key(|&j| (pc[j] as i64 - i as i64).abs());
                near.truncate(NEAREST.min(pc.len()));
                near.sort();
                let a: Vec<Vec<f64>> = near
                    .iter()
                    .map(|&p| {
                        near.iter()
                            .map(|&q| corr(pc[p] as f64 - pc[q] as f64) + if p == q { nsr } else { 0.0 })
                            .collect()
                    })
                    .collect();
                let r: Vec<f64> = near.iter().map(|&p| corr(i as f64 - pc[p] as f64)).collect();
                near.iter().zip(solve(a, r)).map(|(&p, w)| (p as u16, w as f32)).collect()
            })
            .collect()
    }

    /// The timing loop moved the FFT window `delta` samples later than the
    /// symbol period alone would have: what is kept from earlier symbols
    /// is turned to match, as if it had been cut there too, so that time
    /// interpolation never mixes two timings.
    pub fn shift(&mut self, delta: f32) {
        let w = 2.0 * PI * delta / self.nu;
        self.dacc *= Complex32::from_polar(1.0, w * self.x as f32);
        let kmin = self.kmin;
        for (j, &i) in self.pilot_car.iter().enumerate() {
            let r = Complex32::from_polar(1.0, w * (kmin + i as i32) as f32);
            for h in self.hist[j].iter_mut() {
                h.1 *= r;
            }
            if let Some(p) = self.prev_hp.get_mut(j) {
                *p *= r;
            }
        }
        for (_, _, y) in self.ring.iter_mut() {
            for (i, v) in y.iter_mut().enumerate() {
                *v *= Complex32::from_polar(1.0, w * (kmin + i as i32) as f32);
            }
        }
    }

    /// Fold the centered estimate `rot` into the power delay profile and
    /// pick the window and noise level from it.
    fn profile(&mut self, rot: &[Complex32]) {
        let m = self.pdp.len();
        let first = self.pilot_car[0];
        let mut v = vec![Complex32::new(0.0, 0.0); m];
        let span = (self.pilot_car[self.pilot_car.len() - 1] - first) / self.x as usize + 1;
        let mut have = vec![false; span];
        for (&i, &h) in self.pilot_car.iter().zip(rot) {
            let n = (i - first) / self.x as usize;
            v[n] = h;
            have[n] = true;
        }
        // Gaps (mode D's DC carrier) from their neighbors; then a Hann
        // window, so the band's edges do not smear the profile.
        for n in 1..span.saturating_sub(1) {
            if !have[n] {
                v[n] = (v[n - 1] + v[n + 1]) * 0.5;
            }
        }
        for (n, c) in v.iter_mut().take(span).enumerate() {
            *c *= hann(n, span);
        }
        self.pdp_fft.process(&mut v);
        let a = if self.pdp_n < PDP_WARMUP { 1.0 / (self.pdp_n + 1) as f32 } else { 0.03 };
        for (p, c) in self.pdp.iter_mut().zip(v.iter()) {
            *p += a * (c.norm_sqr() - *p);
        }
        self.pdp_n += 1;
        if self.pdp_n < PDP_WARMUP {
            return;
        }
        // The floor is the median bin; what stands above it is the
        // echoes. Find the narrowest window around zero delay holding
        // nearly all of it.
        let mut sorted = self.pdp.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let floor = sorted[m / 2];
        let excess: Vec<f32> = self.pdp.iter().map(|&p| (p - 2.0 * floor).max(0.0)).collect();
        let total: f32 = excess.iter().sum();
        if total <= 0.0 {
            return;
        }
        let mut held = excess[0];
        let mut h = 0;
        while held < PDP_SHARE * total && h < m / 2 {
            h += 1;
            held += excess[h] + excess[(m - h) % m];
        }
        // Bins are tu / (x m) samples; the Hann window's main lobe adds two.
        let bin = self.nu as f64 / (self.x as f64 * m as f64);
        let need = 2.0 * (h as f64 + 2.0) * bin;
        let want = WINDOWS.iter().rposition(|&w| w * self.tg >= need).unwrap_or(0);
        if want < self.window {
            self.window = want;
            self.narrow_for = 0;
        } else if want > self.window {
            self.narrow_for += 1;
            if self.narrow_for >= NARROW_HOLD {
                self.window += 1;
                self.narrow_for = 0;
            }
        } else {
            self.narrow_for = 0;
        }
        self.spread = (profile_var(&excess).1 - self.kernel_var).max(0.0).sqrt() * bin as f32;
        // Noise against signal per pilot carrier: the floor spread over
        // all bins against the echoes' energy.
        let nsr = (floor * m as f32 / total.max(1e-30)) as f64;
        self.nsr = nsr as f32;
        self.nsr_level = NSRS
            .iter()
            .enumerate()
            .min_by(|a, b| (a.1.ln() - nsr.max(1e-4).ln()).abs().total_cmp(&(b.1.ln() - nsr.max(1e-4).ln()).abs()))
            .map_or(1, |p| p.0);
    }

    /// Feed the carriers `y` (compensated FFT output, carrier index order)
    /// of symbol `t` whose frame symbol index is `s`. Returns symbol
    /// t - (y - 1) equalized, once there is one.
    pub fn push(&mut self, t: i64, s: usize, y: Vec<Complex32>, map: &CellMap) -> Option<Equalized> {
        let mut tc = Complex32::new(0.0, 0.0);
        let mut tp = 0.0f32;
        for (i, &yi) in y.iter().enumerate() {
            if map.on_grid(s, i) {
                let p = map.pilot_at(s, i);
                let h = yi / p;
                if let Some(j) = self.car_pilot[i] {
                    let (t1, h1) = self.hist[j][1];
                    if t1 == t - self.y as i64 {
                        tc += h * h1.conj();
                        tp += 0.5 * (h.norm_sqr() + h1.norm_sqr());
                    }
                    self.hist[j] = [self.hist[j][1], (t, h)];
                }
            }
        }
        if tp > 0.0 {
            // Averaged as complex sums, so the figure is that of the
            // average channel, not of its fading moments.
            self.tacc = self.tacc * 0.98 + tc;
            self.tpow = self.tpow * 0.98 + tp;
            self.trho = self.tacc.norm() / self.tpow;
        }
        self.ring.push_back((t, s, y));
        if self.ring.len() < self.y {
            return None;
        }
        let (te, se, ye) = self.ring.pop_front()?;
        // Time interpolation at symbol te.
        let hp: Vec<Complex32> = self
            .hist
            .iter()
            .map(|&[(t0, h0), (t1, h1)]| {
                if t1 == te || t0 == i64::MIN || t1 < te {
                    h1
                } else if t0 == te {
                    h0
                } else {
                    let a = (te - t0) as f32 / (t1 - t0) as f32;
                    h0 + (h1 - h0) * a
                }
            })
            .collect();
        // Mean delay from the phase step between pilot carriers x apart.
        let mut acc = Complex32::new(0.0, 0.0);
        for j in 1..hp.len() {
            if self.pilot_car[j] - self.pilot_car[j - 1] == self.x as usize {
                acc += hp[j] * hp[j - 1].conj();
            }
        }
        // Averaged over a few symbols: with long echoes (mode D's 4 ms is
        // most of a turn between neighboring carriers) one symbol's phase
        // is easily thrown, and a center off by a path's delay would put
        // it outside the interpolation window.
        self.dacc = self.dacc * (1.0 - DELAY_ALPHA) + acc * DELAY_ALPHA;
        let tau = -self.dacc.arg() * self.nu / (2.0 * PI * self.x as f32);
        self.delay = tau;
        let kmin = self.kmin;
        let k_of = move |i: usize| (kmin + i as i32) as f32;
        let rot: Vec<Complex32> = self
            .pilot_car
            .iter()
            .zip(hp.iter())
            .map(|(&i, &h)| h * Complex32::from_polar(1.0, 2.0 * PI * k_of(i) * tau / self.nu))
            .collect();
        self.profile(&rot);
        self.turn = if self.prev_hp.len() == hp.len() {
            hp.iter().zip(self.prev_hp.iter()).map(|(a, b)| a * b.conj()).sum()
        } else {
            Complex32::new(0.0, 0.0)
        };
        let wi = self.window * NSRS.len() + self.nsr_level;
        if self.weights[wi].is_none() {
            self.weights[wi] = Some(self.design(self.window, self.nsr_level));
        }
        let weights = self.weights[wi].as_ref()?;
        let mut z = vec![Complex32::new(0.0, 0.0); self.ncar];
        let mut w = vec![0f32; self.ncar];
        let mut full = vec![Complex32::new(0.0, 0.0); self.ncar];
        for i in 0..self.ncar {
            let mut h = Complex32::new(0.0, 0.0);
            for &(p, wt) in &weights[i] {
                h += rot[p as usize] * wt;
            }
            let h = h * Complex32::from_polar(1.0, -2.0 * PI * k_of(i) * tau / self.nu);
            full[i] = h;
            let p = h.norm_sqr();
            if p > 1e-20 {
                z[i] = ye[i] / h;
                w[i] = p;
            }
        }
        // The pilots of this very symbol against the smoothed estimate:
        // their scatter is the noise on a pilot.
        let (mut err, mut sig) = (0.0f32, 0.0f32);
        for (j, &i) in self.pilot_car.iter().enumerate() {
            let [(t0, h0), (t1, h1)] = self.hist[j];
            let raw = if t1 == te {
                h1
            } else if t0 == te {
                h0
            } else {
                continue;
            };
            // The smoothed estimate keeps some of this pilot's own noise:
            // the scatter is N (1 - 2 w_self + sum w^2), so scale it back.
            let (mut own, mut sq) = (0.0f32, 0.0f32);
            for &(p, wt) in &weights[i] {
                sq += wt * wt;
                if p as usize == j {
                    own = wt;
                }
            }
            let keep = (1.0 - 2.0 * own + sq).max(0.1);
            err += (raw - full[i]).norm_sqr() / keep;
            sig += full[i].norm_sqr();
        }
        if sig > 0.0 {
            self.pilot_err = 0.97 * self.pilot_err + 0.03 * err;
            self.pilot_sig = 0.97 * self.pilot_sig + 0.03 * sig;
            self.pilot_nsr = self.pilot_err / self.pilot_sig;
        }
        self.prev_hp = hp;
        Some(Equalized { s: se, z, w })
    }
}
