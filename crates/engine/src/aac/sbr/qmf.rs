//! The SBR QMF banks (ISO/IEC 14496-3 4.6.18.4): a 32-band complex
//! analysis of the core output and a 64-band complex synthesis at twice its
//! rate, both from the standard's 640-tap prototype.
//!
//! Both run as direct matrix products against precomputed modulation
//! tables. The synthesis only touches the bands in use (below kx + M), which
//! is most of the saving a fast transform would give at these sizes.

use super::tables::QMF_WINDOW;
use num_complex::Complex32;
use std::f64::consts::PI;
use std::sync::OnceLock;

struct Modulation {
    /// Analysis: 2 exp(i pi (k + 1/2)(2n - 1/2) / 64), [k][n], k < 32, n < 64.
    ana_re: Vec<f32>,
    ana_im: Vec<f32>,
    /// Synthesis: exp(i pi (k + 1/2)(2n - 255) / 128) / 64, [n][k], n < 128.
    syn_re: Vec<f32>,
    syn_im: Vec<f32>,
}

fn modulation() -> &'static Modulation {
    static M: OnceLock<Modulation> = OnceLock::new();
    M.get_or_init(|| {
        let mut m = Modulation {
            ana_re: vec![0.0; 32 * 64],
            ana_im: vec![0.0; 32 * 64],
            syn_re: vec![0.0; 128 * 64],
            syn_im: vec![0.0; 128 * 64],
        };
        for k in 0..32 {
            for n in 0..64 {
                let a = PI / 64.0 * (k as f64 + 0.5) * (2.0 * n as f64 - 0.5);
                m.ana_re[k * 64 + n] = (2.0 * a.cos()) as f32;
                m.ana_im[k * 64 + n] = (2.0 * a.sin()) as f32;
            }
        }
        for n in 0..128 {
            for k in 0..64 {
                let a = PI / 128.0 * (k as f64 + 0.5) * (2.0 * n as f64 - 255.0);
                m.syn_re[n * 64 + k] = (a.cos() / 64.0) as f32;
                m.syn_im[n * 64 + k] = (a.sin() / 64.0) as f32;
            }
        }
        m
    })
}

/// 32-band analysis, one channel.
pub(super) struct Analysis {
    /// The last 288 input samples, oldest first, then room for a frame.
    buf: Vec<f32>,
}

impl Analysis {
    pub fn new(frame_len: usize) -> Self {
        Self { buf: vec![0.0; 288 + frame_len] }
    }

    /// Analyzes `input` (a multiple of 32 samples) into one row of 32
    /// subband samples per 32 input samples.
    pub fn run(&mut self, input: &[f32], out: &mut [[Complex32; 32]]) {
        let m = modulation();
        let n_in = input.len();
        self.buf[288..288 + n_in].copy_from_slice(input);
        let mut u = [0f32; 64];
        for (l, row) in out.iter_mut().enumerate().take(n_in / 32) {
            // Newest sample last in `seg`; the standard indexes newest first.
            let seg = &self.buf[l * 32..l * 32 + 320];
            for (n, un) in u.iter_mut().enumerate() {
                let mut s = 0.0;
                for j in 0..5 {
                    let i = n + 64 * j;
                    s += seg[319 - i] * QMF_WINDOW[2 * i];
                }
                *un = s;
            }
            for (k, x) in row.iter_mut().enumerate() {
                let re = &m.ana_re[k * 64..k * 64 + 64];
                let im = &m.ana_im[k * 64..k * 64 + 64];
                let (mut a, mut b) = (0.0, 0.0);
                for n in 0..64 {
                    a += u[n] * re[n];
                    b += u[n] * im[n];
                }
                *x = Complex32::new(a, b);
            }
        }
        self.buf.copy_within(n_in..n_in + 288, 0);
    }
}

/// 64-band synthesis, one channel.
pub(super) struct Synthesis {
    v: Vec<f32>,
}

impl Synthesis {
    pub fn new() -> Self {
        Self { v: vec![0.0; 1280] }
    }

    /// Synthesizes 64 output samples per row of `x`, using bands below
    /// `bands` (the rest are zero).
    pub fn run(&mut self, x: &[[Complex32; 64]], bands: usize, out: &mut [f32]) {
        let m = modulation();
        let bands = bands.min(64);
        for (l, row) in x.iter().enumerate() {
            self.v.copy_within(0..1152, 128);
            for n in 0..128 {
                let re = &m.syn_re[n * 64..n * 64 + bands];
                let im = &m.syn_im[n * 64..n * 64 + bands];
                let mut s = 0.0;
                for k in 0..bands {
                    s += row[k].re * re[k] - row[k].im * im[k];
                }
                self.v[n] = s;
            }
            let o = &mut out[l * 64..l * 64 + 64];
            for (j, oj) in o.iter_mut().enumerate() {
                let mut s = 0.0;
                for i in 0..5 {
                    s += self.v[256 * i + j] * QMF_WINDOW[128 * i + j]
                        + self.v[256 * i + 192 + j] * QMF_WINDOW[128 * i + 64 + j];
                }
                *oj = s;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Analysis then synthesis with the upper 32 bands empty is a 2x
    /// interpolator. Tones across the core band come out at the analysis
    /// plus synthesis delay, at unit gain to within the prototype's ripple,
    /// with aliasing well down. This checks the prototype window and both
    /// modulations against each other.
    #[test]
    fn analysis_then_synthesis_reconstructs_at_twice_the_rate() {
        let frame = 1024;
        let frames = 8;
        let core_rate = 24000.0;
        let rate = 2.0 * core_rate;
        // Analysis 320 taps and synthesis 640, minus one 64-sample block.
        let delay = 578.0;
        for f in [211.3, 1234.5, 3000.5, 7100.75, 9876.0, 11000.1] {
            let input: Vec<f32> = (0..frame * frames)
                .map(|i| (10000.0 * (2.0 * PI * f * i as f64 / core_rate).sin()) as f32)
                .collect();
            let mut ana = Analysis::new(frame);
            let mut syn = Synthesis::new();
            let mut out = vec![0f32; 2 * frame * frames];
            let mut sub = vec![[Complex32::default(); 32]; frame / 32];
            let mut x = vec![[Complex32::default(); 64]; frame / 32];
            for fr in 0..frames {
                ana.run(&input[fr * frame..(fr + 1) * frame], &mut sub);
                for (row, s) in x.iter_mut().zip(&sub) {
                    row[..32].copy_from_slice(s);
                }
                syn.run(&x, 32, &mut out[2 * fr * frame..2 * (fr + 1) * frame]);
            }
            // Least-squares fit of a sine and cosine at the expected delay:
            // the sine term is the gain, the cosine term any phase error,
            // the residual the aliasing.
            let (mut ss, mut sc, mut cc, mut so, mut co) = (0.0, 0.0, 0.0, 0.0, 0.0);
            let range = 4000..14000;
            for i in range.clone() {
                let a = 2.0 * PI * f * (i as f64 - delay) / rate;
                let (sn, cs) = (a.sin(), a.cos());
                ss += sn * sn;
                sc += sn * cs;
                cc += cs * cs;
                so += sn * f64::from(out[i]);
                co += cs * f64::from(out[i]);
            }
            let det = ss * cc - sc * sc;
            let gain = (so * cc - co * sc) / det / 10000.0;
            let quad = (co * ss - so * sc) / det / 10000.0;
            let mut resid = 0.0;
            for i in range.clone() {
                let a = 2.0 * PI * f * (i as f64 - delay) / rate;
                resid += (f64::from(out[i]) - 10000.0 * (gain * a.sin() + quad * a.cos())).powi(2);
            }
            let sig = range.len() as f64 * (10000.0 * gain).powi(2) / 2.0;
            let alias_db = 10.0 * (resid / sig).log10();
            let gain_db = 20.0 * gain.log10();
            assert!(gain_db.abs() < 0.05, "{f} Hz: gain {gain_db:+.4} dB");
            assert!(quad.abs() < 1e-3, "{f} Hz: phase term {quad}");
            assert!(alias_db < -60.0, "{f} Hz: aliasing {alias_db:.1} dB");
        }
    }

    /// Copying a subband up, as the HF generator does, shifts a tone by
    /// whole subbands: 1000 Hz in subband 2 moved to subband 12 comes out at
    /// 1000 + 10 * 375 = 4750 Hz, not mirrored inside the band (4625 Hz).
    #[test]
    fn copying_a_subband_up_shifts_the_tone() {
        let frame = 1024;
        let frames = 6;
        let input: Vec<f32> =
            (0..frame * frames).map(|i| (10000.0 * (2.0 * PI * 1000.0 * i as f64 / 24000.0).sin()) as f32).collect();
        let mut ana = Analysis::new(frame);
        let mut syn = Synthesis::new();
        let mut out = vec![0f32; 2 * frame * frames];
        let mut sub = vec![[Complex32::default(); 32]; frame / 32];
        let mut x = vec![[Complex32::default(); 64]; frame / 32];
        for fr in 0..frames {
            ana.run(&input[fr * frame..(fr + 1) * frame], &mut sub);
            for (row, s) in x.iter_mut().zip(&sub) {
                row.fill(Complex32::default());
                row[12] = s[2];
            }
            syn.run(&x, 64, &mut out[2 * fr * frame..2 * (fr + 1) * frame]);
        }
        let seg = &out[4096..4096 + 4096];
        let power = |f: f64| -> f64 {
            let (mut re, mut im) = (0.0, 0.0);
            for (i, &v) in seg.iter().enumerate() {
                let a = 2.0 * PI * f * i as f64 / 48000.0;
                re += f64::from(v) * a.cos();
                im += f64::from(v) * a.sin();
            }
            re * re + im * im
        };
        let (shifted, mirrored) = (power(4750.0), power(4625.0));
        assert!(shifted > 100.0 * mirrored, "shifted {shifted:.3e} mirrored {mirrored:.3e}");
    }
}
