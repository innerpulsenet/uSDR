//! HF generation and adjustment (ISO/IEC 14496-3 4.6.18.6, 4.6.18.7).
//!
//! The generator copies low-band QMF subbands up into the SBR range along
//! the patches, each through a second-order inverse (whitening) filter whose
//! strength the encoder chooses per noise band (the chirp factor). The
//! adjuster then measures the copied band's energy per envelope and subband,
//! computes gains toward the transmitted envelope with the limiter and its
//! compensating boost, smooths them over time, and adds the noise floor and
//! any sinusoids.
//!
//! Time indexing follows the standard: SBR slot `l` of the current frame is
//! QMF row `l + T_HFADJ` of the low and high buffers, whose first `T_HFGEN`
//! rows are the end of the previous frame; the output rows of frame `n` are
//! thereby delayed by `T_HFGEN - T_HFADJ` = 6 QMF slots against its core.

use super::freq::FreqTables;
use super::parse::ChannelData;
use super::tables::NOISE_TABLE;
use super::Header;
use num_complex::Complex32;

pub(super) const T_HFGEN: usize = 8;
pub(super) const T_HFADJ: usize = 2;
/// Low/high buffer rows: up to 32 QMF slots plus the T_HFGEN overlap.
pub(super) const ROWS: usize = 40;

/// Limiter gain caps for bs_limiter_gains (-3, 0, +3 dB, off).
const LIM_GAIN: [f32; 4] = [0.707_95, 1.0, 1.412_54, 1e10];
const SMOOTH: [f32; 5] = [0.333_333_33, 0.301_502_83, 0.218_169_5, 0.115_163_83, 0.031_830_5];
const EPS: f32 = 1e-12;

/// Per-channel state the adjuster keeps across frames.
#[derive(Clone)]
pub(super) struct HfState {
    pub invf_prev: [u8; 5],
    pub bw: [f32; 5],
    /// Sinusoid flags of the previous frame's last envelope, by subband.
    pub s_index_prev: [bool; 48],
    /// Gains and noise levels of the last four QMF slots, newest last.
    pub g_hist: [[f32; 48]; 4],
    pub q_hist: [[f32; 48]; 4],
    pub noise_index: usize,
    pub sine_index: usize,
    /// The previous frame's transient envelope and envelope count.
    pub l_a_prev: i32,
    pub num_env_prev: usize,
}

impl Default for HfState {
    fn default() -> Self {
        Self {
            invf_prev: [0; 5],
            bw: [0.0; 5],
            s_index_prev: [false; 48],
            g_hist: [[0.0; 48]; 4],
            q_hist: [[0.0; 48]; 4],
            noise_index: 0,
            sine_index: 0,
            l_a_prev: -1,
            num_env_prev: 0,
        }
    }
}

/// Second-order complex linear prediction of each low subband over the
/// frame (4.6.18.6.2): alpha0, alpha1 per subband below k0.
pub(super) fn inverse_filter(x_low: &[[Complex32; ROWS]], k0: usize, slots: usize) -> Vec<(Complex32, Complex32)> {
    let mut out = vec![(Complex32::default(), Complex32::default()); k0];
    for (k, a) in out.iter_mut().enumerate() {
        let x = &x_low[k];
        // phi(i, j) = sum_n x[n - i + 2] conj(x[n - j + 2]), n < slots + 6.
        let phi = |i: usize, j: usize| -> Complex32 {
            let mut s = Complex32::default();
            for n in 0..slots + 6 {
                s += x[n + 2 - i] * x[n + 2 - j].conj();
            }
            s
        };
        let (p01, p02, p11, p12, p22) = (phi(0, 1), phi(0, 2), phi(1, 1).re, phi(1, 2), phi(2, 2).re);
        let d = p22 * p11 - p12.norm_sqr() / (1.0 + 1e-6);
        let a1 = if d != 0.0 { (p01 * p12 - p02 * p11) / d } else { Complex32::default() };
        let a0 = if p11 != 0.0 { -(p01 + a1 * p12.conj()) / p11 } else { Complex32::default() };
        *a = if a0.norm_sqr() >= 16.0 || a1.norm_sqr() >= 16.0 {
            (Complex32::default(), Complex32::default())
        } else {
            (a0, a1)
        };
    }
    out
}

/// Chirp factors per noise band from this frame's and the last frame's
/// inverse-filtering modes, smoothed against the last values.
pub(super) fn chirp(st: &mut HfState, invf: &[u8; 5], n_q: usize) {
    const BW: [f32; 4] = [0.0, 0.75, 0.9, 0.98];
    for i in 0..n_q {
        let mut bw = if invf[i] + st.invf_prev[i] == 1 { 0.6 } else { BW[usize::from(invf[i])] };
        bw = if bw < st.bw[i] { 0.75 * bw + 0.25 * st.bw[i] } else { 0.906_25 * bw + 0.093_75 * st.bw[i] };
        st.bw[i] = if bw < 0.015_625 { 0.0 } else { bw };
    }
    st.invf_prev = *invf;
}

/// Fills `x_high` rows `2 t_env[0] + 2 .. 2 t_env[L] + 2` of the SBR bands.
pub(super) fn generate(
    x_low: &[[Complex32; ROWS]],
    x_high: &mut [[Complex32; ROWS]],
    alpha: &[(Complex32, Complex32)],
    bw: &[f32; 5],
    t: &FreqTables,
    d: &ChannelData,
) {
    let (lo, hi) = (2 * d.t_env[0], 2 * d.t_env[d.num_env]);
    let mut k = t.kx;
    for &(start, count) in &t.patches {
        for x in 0..count {
            let p = start + x;
            // The noise band this target subband falls in.
            let g = t.noise[1..].iter().position(|&f| k < f).unwrap_or(t.n_q() - 1);
            let b = bw[g];
            let (a0, a1) = alpha[p];
            let (c0, c1) = (a0 * b, a1 * (b * b));
            let src = &x_low[p];
            let dst = &mut x_high[k];
            for i in lo..hi {
                let r = i + T_HFADJ;
                dst[r] = src[r] + c0 * src[r - 1] + c1 * src[r - 2];
            }
            k += 1;
        }
    }
    for row in x_high.iter_mut().take(t.kx + t.m).skip(k) {
        row.fill(Complex32::default());
    }
}

/// Dequantized envelope and noise floor of one channel, [env][band] and
/// [noise env][band] (4.6.18.3.5).
pub(super) struct Dequant {
    pub env: [[f32; 48]; 5],
    pub noise: [[f32; 5]; 2],
}

fn exp2(x: f32) -> f32 {
    x.exp2()
}

/// Dequantizes one channel, or a coupled pair (level and balance).
pub(super) fn dequantize(data: &[ChannelData], coupling: bool, t: &FreqTables) -> Vec<Dequant> {
    let blank = || Dequant { env: [[0.0; 48]; 5], noise: [[0.0; 5]; 2] };
    let n_q = t.n_q();
    if coupling && data.len() == 2 {
        let (a, b) = (&data[0], &data[1]);
        let mut out = [blank(), blank()];
        let step = if a.amp_res == 1 { 1.0 } else { 0.5 };
        let pan = if a.amp_res == 1 { 12.0 } else { 24.0 };
        for l in 0..a.num_env {
            let n = t.table(a.freq_res[l]).len() - 1;
            for k in 0..n {
                let level = exp2(a.env[l][k] as f32 * step + 7.0);
                let ratio = exp2((pan - b.env[l][k] as f32) * step);
                let left = level / (1.0 + ratio);
                out[0].env[l][k] = left;
                out[1].env[l][k] = left * ratio;
            }
        }
        for l in 0..a.num_noise {
            for k in 0..n_q {
                let level = exp2(6.0 - a.noise[l][k] as f32 + 1.0);
                let ratio = exp2(12.0 - b.noise[l][k] as f32);
                let left = level / (1.0 + ratio);
                out[0].noise[l][k] = left;
                out[1].noise[l][k] = left * ratio;
            }
        }
        return out.into();
    }
    data.iter()
        .map(|d| {
            let mut q = blank();
            let step = if d.amp_res == 1 { 1.0 } else { 0.5 };
            for l in 0..d.num_env {
                let n = t.table(d.freq_res[l]).len() - 1;
                for k in 0..n {
                    q.env[l][k] = exp2(d.env[l][k] as f32 * step + 6.0);
                }
            }
            for l in 0..d.num_noise {
                for k in 0..n_q {
                    q.noise[l][k] = exp2(6.0 - d.noise[l][k] as f32);
                }
            }
            q
        })
        .collect()
}

/// Envelope adjustment: writes the SBR bands of `y` rows
/// `2 t_env[0] .. 2 t_env[L]` from `x_high`. `level` scales the target
/// energies (1 normally, the fade during concealment).
#[allow(clippy::too_many_arguments)]
pub(super) fn adjust(
    st: &mut HfState,
    x_high: &[[Complex32; ROWS]],
    y: &mut [[Complex32; 64]],
    d: &ChannelData,
    q: &Dequant,
    t: &FreqTables,
    h: &Header,
    reset: bool,
    level: f32,
) {
    let (kx, m_count) = (t.kx, t.m);
    let l_a = d.l_a;
    // A transient at the very end of the last frame marks this frame's
    // first envelope too.
    let l_a_prev = if st.l_a_prev >= 0 && st.l_a_prev as usize == st.num_env_prev { 0 } else { -1 };
    let energy_scale = level * level;

    let mut gains = [[0f32; 48]; 5];
    let mut q_m = [[0f32; 48]; 5];
    let mut s_m = [[0f32; 48]; 5];
    let mut s_index_last = [false; 48];

    for l in 0..d.num_env {
        let table = t.table(d.freq_res[l]);
        let noise_env = usize::from(d.num_noise > 1 && d.t_env[l] >= d.t_noise[1]);

        // Map envelope, noise floor and sinusoids onto subbands.
        let mut e_orig = [0f32; 48];
        let mut q_orig = [0f32; 48];
        for i in 0..table.len() - 1 {
            for m in table[i]..table[i + 1] {
                e_orig[m - kx] = q.env[l][i] * energy_scale;
            }
        }
        for i in 0..t.n_q() {
            for m in t.noise[i]..t.noise[i + 1] {
                q_orig[m - kx] = q.noise[noise_env][i];
            }
        }
        let mut s_index = [false; 48];
        if d.add_harmonic_flag {
            for i in 0..t.n_high() {
                let mid = (t.high[i] + t.high[i + 1]) / 2 - kx;
                s_index[mid] = d.add_harmonic[i] && (l as i32 >= l_a || st.s_index_prev[mid]);
            }
        }
        let mut s_mapped = [false; 48];
        for i in 0..table.len() - 1 {
            let (a, b) = (table[i] - kx, table[i + 1] - kx);
            let any = s_index[a..b].iter().any(|&s| s);
            s_mapped[a..b].fill(any);
        }

        // Energy of the generated high band over this envelope.
        let (lo, hi) = (2 * d.t_env[l] + T_HFADJ, 2 * d.t_env[l + 1] + T_HFADJ);
        let mut e_curr = [0f32; 48];
        if h.interpol_freq {
            let inv = 1.0 / (hi - lo) as f32;
            for m in 0..m_count {
                let row = &x_high[m + kx];
                e_curr[m] = row[lo..hi].iter().map(|v| v.norm_sqr()).sum::<f32>() * inv;
            }
        } else {
            for i in 0..table.len() - 1 {
                let mut sum = 0.0;
                for k in table[i]..table[i + 1] {
                    sum += x_high[k][lo..hi].iter().map(|v| v.norm_sqr()).sum::<f32>();
                }
                let avg = sum / ((hi - lo) * (table[i + 1] - table[i])) as f32;
                e_curr[table[i] - kx..table[i + 1] - kx].fill(avg);
            }
        }

        // Gains, then the limiter and its boost, per limiter band.
        let delta = !(l as i32 == l_a || l as i32 == l_a_prev);
        let (g, qm, sm) = (&mut gains[l], &mut q_m[l], &mut s_m[l]);
        for band in t.limiter.windows(2) {
            let (a, b) = (band[0] - kx, band[1] - kx);
            for m in a..b {
                let tmp = e_orig[m] / (1.0 + q_orig[m]);
                qm[m] = (tmp * q_orig[m]).sqrt();
                sm[m] = if s_index[m] { tmp.sqrt() } else { 0.0 };
                g[m] = if s_mapped[m] {
                    (e_orig[m] * q_orig[m] / ((1.0 + e_curr[m]) * (1.0 + q_orig[m]))).sqrt()
                } else {
                    let qd = if delta { q_orig[m] } else { 0.0 };
                    (e_orig[m] / ((1.0 + e_curr[m]) * (1.0 + qd))).sqrt()
                } + f32::MIN_POSITIVE;
            }
            let sum_orig: f32 = e_orig[a..b].iter().sum();
            let sum_curr: f32 = e_curr[a..b].iter().sum();
            let g_max = (LIM_GAIN[usize::from(h.limiter_gains)] * ((EPS + sum_orig) / (EPS + sum_curr)).sqrt()).min(1e5);
            for m in a..b {
                let q_max = qm[m] * g_max / g[m];
                qm[m] = qm[m].min(q_max);
                g[m] = g[m].min(g_max);
            }
            let mut sum_adj = 0.0;
            for m in a..b {
                sum_adj += e_curr[m] * g[m] * g[m] + sm[m] * sm[m];
                if delta && sm[m] == 0.0 {
                    sum_adj += qm[m] * qm[m];
                }
            }
            let boost = ((EPS + sum_orig) / (EPS + sum_adj)).sqrt().min(1.584_893_2);
            for m in a..b {
                g[m] *= boost;
                qm[m] *= boost;
                sm[m] *= boost;
            }
        }
        if l == d.num_env - 1 {
            s_index_last = s_index;
        }
    }

    // Assembly over the QMF slots, with gain smoothing unless disabled.
    let smooth = !h.smoothing_mode;
    if reset {
        // New tables: no history to smooth against, and the noise sequence
        // starts over.
        st.g_hist = [gains[0]; 4];
        st.q_hist = [q_m[0]; 4];
        st.noise_index = 0;
    }
    let kx_odd = kx & 1 == 1;
    for l in 0..d.num_env {
        let transient = l as i32 == l_a || l as i32 == l_a_prev;
        for i in 2 * d.t_env[l]..2 * d.t_env[l + 1] {
            let (g_cur, q_cur) = (&gains[l], &q_m[l]);
            let mut g_filt = [0f32; 48];
            let mut q_filt = [0f32; 48];
            if smooth && !transient {
                for m in 0..m_count {
                    let mut gs = g_cur[m] * SMOOTH[0];
                    let mut qs = q_cur[m] * SMOOTH[0];
                    for j in 1..5 {
                        gs += st.g_hist[4 - j][m] * SMOOTH[j];
                        qs += st.q_hist[4 - j][m] * SMOOTH[j];
                    }
                    g_filt[m] = gs;
                    q_filt[m] = qs;
                }
            } else {
                g_filt[..m_count].copy_from_slice(&g_cur[..m_count]);
                q_filt[..m_count].copy_from_slice(&q_cur[..m_count]);
            }
            st.g_hist.rotate_left(1);
            st.g_hist[3] = *g_cur;
            st.q_hist.rotate_left(1);
            st.q_hist[3] = *q_cur;

            let row = &mut y[i];
            let src = i + T_HFADJ;
            let sine = st.sine_index;
            // The sinusoid's phase for this slot, alternating in sign per
            // subband in the imaginary part.
            let (sre, sim) = [(1.0f32, 0.0f32), (0.0, 1.0), (-1.0, 0.0), (0.0, -1.0)][sine];
            for m in 0..m_count {
                let k = m + kx;
                let mut v = x_high[k][src] * g_filt[m];
                let sign = if (m & 1 == 1) != kx_odd { -1.0 } else { 1.0 };
                if s_m[l][m] != 0.0 {
                    v += Complex32::new(s_m[l][m] * sre, s_m[l][m] * sim * sign);
                } else if !transient {
                    let n = NOISE_TABLE[(st.noise_index + m + 1) & 511];
                    v += Complex32::new(q_filt[m] * n[0], q_filt[m] * n[1]);
                }
                row[k] = v;
            }
            st.noise_index = (st.noise_index + m_count) & 511;
            st.sine_index = (st.sine_index + 1) & 3;
        }
    }

    st.s_index_prev = s_index_last;
    st.l_a_prev = l_a;
    st.num_env_prev = d.num_env;
}
