//! Spectral band replication, HE-AAC v1 (ISO/IEC 14496-3 4.6.18), in the
//! high-quality (complex QMF) form.
//!
//! The AAC core codes only the low band, at half the output rate. Here its
//! output is split into 32 complex QMF subbands; the encoder's side
//! information (an envelope of the high band on a time/frequency grid, a
//! noise floor, inverse-filtering modes, sinusoid flags) says how to rebuild
//! the subbands above the crossover from copies of the low ones; and a
//! 64-band synthesis puts the full band back together at twice the core
//! rate.
//!
//! Before the first SBR header arrives (and after a header the standard
//! does not allow) the synthesis still runs, with every core subband passed
//! and nothing above: the output is the core upsampled, at the same delay
//! the SBR path has, so the start of SBR never jumps.

pub(super) mod freq;
mod hf;
mod parse;
mod qmf;
pub(super) mod tables;

use super::bits::BitReader;
use super::ps::Ps;
use super::AacError;
use freq::FreqTables;
use hf::{HfState, ROWS, T_HFADJ, T_HFGEN};
use num_complex::Complex32;
use parse::{DeltaRef, FrameData};

/// The SBR header fields; the defaults are those the standard gives when
/// the optional parts are absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Header {
    pub amp_res: u8,
    pub start_freq: u8,
    pub stop_freq: u8,
    pub xover_band: u8,
    pub freq_scale: u8,
    pub alter_scale: u8,
    pub noise_bands: u8,
    pub limiter_bands: u8,
    pub limiter_gains: u8,
    pub interpol_freq: bool,
    pub smoothing_mode: bool,
}

impl Default for Header {
    fn default() -> Self {
        Self {
            amp_res: 1,
            start_freq: 0,
            stop_freq: 0,
            xover_band: 0,
            freq_scale: 2,
            alter_scale: 1,
            noise_bands: 2,
            limiter_bands: 2,
            limiter_gains: 2,
            interpol_freq: true,
            smoothing_mode: true,
        }
    }
}

impl Header {
    /// Fields whose change rebuilds the frequency tables and resets SBR.
    fn tables_key(&self) -> (u8, u8, u8, u8, u8, u8) {
        (self.start_freq, self.stop_freq, self.xover_band, self.freq_scale, self.alter_scale, self.noise_bands)
    }
}

type Row32 = [Complex32; 32];
type Row64 = [Complex32; 64];

struct Channel {
    ana: qmf::Analysis,
    syn: qmf::Synthesis,
    /// The last T_HFGEN analysis rows of the previous frame.
    tail: [Row32; T_HFGEN],
    x_low: Vec<[Complex32; ROWS]>,
    x_high: Vec<[Complex32; ROWS]>,
    /// Adjusted high band of this frame and the previous one, by QMF row
    /// (rows past the frame end spill into the next).
    y: Vec<Row64>,
    y_prev: Vec<Row64>,
    hf: HfState,
    kx_prev: usize,
    m_prev: usize,
    /// Where the previous frame's last envelope ended, in QMF rows.
    end_prev: usize,
}

impl Channel {
    fn new(frame_len: usize) -> Self {
        let slots = frame_len / 32;
        Self {
            ana: qmf::Analysis::new(frame_len),
            syn: qmf::Synthesis::new(),
            tail: [[Complex32::default(); 32]; T_HFGEN],
            x_low: vec![[Complex32::default(); ROWS]; 32],
            x_high: vec![[Complex32::default(); ROWS]; 64],
            y: vec![[Complex32::default(); 64]; slots + 6],
            y_prev: vec![[Complex32::default(); 64]; slots + 6],
            hf: HfState::default(),
            kx_prev: 32,
            m_prev: 0,
            end_prev: 0,
        }
    }
}

pub(super) struct Sbr {
    rate: u32,
    /// QMF slots per frame: 32 (1024) or 30 (960).
    slots: usize,
    header: Option<Header>,
    tables: Option<FreqTables>,
    /// Tables changed since the last frame was processed.
    reset: bool,
    data: Option<FrameData>,
    refs: [DeltaRef; 2],
    ch: Vec<Channel>,
    ps: Option<Box<Ps>>,
    /// The synthesis for the channel parametric stereo makes.
    ch_right: Option<qmf::Synthesis>,
    w: Vec<Row32>,
    x: Vec<Row64>,
    x_right: Vec<Row64>,
}

impl Sbr {
    /// `rate` is the SBR output rate; `channels` the core channels.
    pub fn new(rate: u32, frame_len: usize, channels: usize, ps: bool) -> Self {
        let slots = frame_len / 32;
        Self {
            rate,
            slots,
            header: None,
            tables: None,
            reset: false,
            data: None,
            refs: [DeltaRef::default(); 2],
            ch: (0..channels).map(|_| Channel::new(frame_len)).collect(),
            ps: ps.then(|| Box::new(Ps::new(slots))),
            ch_right: ps.then(qmf::Synthesis::new),
            w: vec![[Complex32::default(); 32]; slots],
            x: vec![[Complex32::default(); 64]; slots],
            x_right: vec![[Complex32::default(); 64]; slots],
        }
    }

    /// sbr_extension_data(), after the extension type. A payload that
    /// fails to parse is dropped and the last good data stays in force.
    pub fn parse(&mut self, br: &mut BitReader, cpe: bool, crc: bool) -> Result<(), AacError> {
        if crc {
            br.skip(10);
        }
        if br.bit() {
            let h = parse::parse_header(br);
            if br.overrun() {
                return Err(AacError::Truncated);
            }
            let rebuild = self.header.is_none_or(|old| old.tables_key() != h.tables_key());
            if rebuild {
                match FreqTables::new(self.rate, &h) {
                    Ok(t) => {
                        self.tables = Some(t);
                        self.header = Some(h);
                        self.reset = true;
                        // Data coded against the old tables cannot be reused.
                        self.data = None;
                    }
                    Err(e) => {
                        self.tables = None;
                        self.header = None;
                        self.data = None;
                        return Err(e);
                    }
                }
            } else if self.header != Some(h) {
                if let Some(t) = self.tables.as_mut() {
                    t.make_limiter(h.limiter_bands);
                }
                self.header = Some(h);
            }
        }
        let (Some(h), Some(t)) = (self.header.as_ref(), self.tables.as_ref()) else {
            return Err(AacError::Invalid("SBR data before any SBR header"));
        };
        let fd = parse::parse_data(br, h, t, cpe, &self.refs, self.slots / 2)?;
        for (r, d) in self.refs.iter_mut().zip(&fd.ch) {
            *r = parse::delta_ref(d);
        }
        if let (Some(ps), Some((start, bits))) = (self.ps.as_mut(), fd.ps) {
            let mut ps_br = br.fork(start, bits);
            ps.parse(&mut ps_br);
        }
        self.data = Some(fd);
        Ok(())
    }

    /// DRM's SBR payload: `parse` (no extension type, no CRC field), then
    /// `check` with the reader's position; if that fails (DRM's SBR CRC),
    /// everything the payload changed is undone.
    pub fn parse_checked(
        &mut self,
        br: &mut BitReader,
        cpe: bool,
        check: impl FnOnce(usize) -> bool,
    ) -> Result<(), AacError> {
        let saved = (self.header, self.tables.clone(), self.reset, self.data.clone(), self.refs);
        let result = self.parse(br, cpe, false);
        if result.is_err() || !check(br.position()) {
            (self.header, self.tables, self.reset, self.data, self.refs) = saved;
            return result.and(Err(AacError::Crc("SBR")));
        }
        Ok(())
    }

    /// A lost frame: the last data repeats (the caller scales it).
    pub fn conceal(&mut self) {
        if let Some(ps) = self.ps.as_mut() {
            ps.conceal();
        }
    }

    /// Runs the core channels in `core` through SBR (and PS) into `out`,
    /// one plane per output channel, each twice the core length. `level`
    /// scales the regenerated band along with the faded core.
    pub fn process(&mut self, core: &[Vec<f32>], out: &mut [Vec<f32>], level: f32) {
        let slots = self.slots;
        let active = self.tables.is_some() && self.data.is_some();
        let deq = match (&self.tables, &self.data) {
            (Some(t), Some(d)) => hf::dequantize(&d.ch, d.coupling, t),
            _ => Vec::new(),
        };
        let (kx, m) = match (&self.tables, active) {
            (Some(t), true) => (t.kx, t.m),
            _ => (32, 0),
        };
        let reset = std::mem::take(&mut self.reset);
        for c in 0..core.len() {
            let ch = &mut self.ch[c];
            ch.ana.run(&core[c], &mut self.w);

            // Low band rows: the previous frame's tail, then this frame.
            for k in 0..32 {
                let row = &mut ch.x_low[k];
                for i in 0..T_HFGEN {
                    row[i] = if k < ch.kx_prev { ch.tail[i][k] } else { Complex32::default() };
                }
                for i in 0..slots {
                    row[i + T_HFGEN] = if k < kx { self.w[i][k] } else { Complex32::default() };
                }
            }

            let mut end = 0;
            for r in ch.y.iter_mut() {
                r.fill(Complex32::default());
            }
            if let (true, Some(t), Some(h), Some(fd)) = (active, &self.tables, &self.header, &self.data) {
                let d = &fd.ch[c.min(fd.ch.len() - 1)];
                let q = &deq[c.min(deq.len() - 1)];
                let alpha = hf::inverse_filter(&ch.x_low, t.k0, slots);
                hf::chirp(&mut ch.hf, &d.invf, t.n_q());
                hf::generate(&ch.x_low, &mut ch.x_high, &alpha, &ch.hf.bw, t, d);
                hf::adjust(&mut ch.hf, &ch.x_high, &mut ch.y, d, q, t, h, reset, level);
                end = 2 * d.t_env[d.num_env];
            }

            // Assemble the 64 bands: rows still covered by the last frame's
            // envelopes take its high band and crossover.
            let spill = ch.end_prev.saturating_sub(slots);
            for i in 0..slots {
                let row = &mut self.x[i];
                row.fill(Complex32::default());
                let (kx_i, m_i, high) =
                    if i < spill { (ch.kx_prev, ch.m_prev, &ch.y_prev[i + slots]) } else { (kx, m, &ch.y[i]) };
                for k in 0..kx_i {
                    row[k] = ch.x_low[k][i + T_HFADJ];
                }
                row[kx_i..kx_i + m_i].copy_from_slice(&high[kx_i..kx_i + m_i]);
            }
            let bands = (kx + m).max(ch.kx_prev + ch.m_prev);

            for i in 0..T_HFGEN {
                ch.tail[i] = self.w[slots - T_HFGEN + i];
            }
            ch.kx_prev = kx;
            ch.m_prev = m;
            ch.end_prev = end;
            std::mem::swap(&mut ch.y, &mut ch.y_prev);

            if let (0, Some(ps)) = (c, self.ps.as_mut()) {
                ps.process(&mut self.x, &mut self.x_right, bands);
                ch.syn.run(&self.x, bands, &mut out[0]);
                if let Some(right) = self.ch_right.as_mut() {
                    right.run(&self.x_right, bands, &mut out[1]);
                }
            } else {
                ch.syn.run(&self.x, bands, &mut out[c]);
            }
        }
        if core.len() == 1 && out.len() == 2 && self.ps.is_none() {
            let (l, r) = out.split_at_mut(1);
            r[0].copy_from_slice(&l[0]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::BitWriter;
    use super::tables::data as t;
    use super::*;

    /// One mono payload: a header (limiter off, no smoothing), one FIXFIX
    /// envelope at high resolution with a flat level, the noise floor at its
    /// lowest, no inverse filtering, no sinusoids.
    fn payload(level: u32, n_high: usize, n_q: usize) -> BitWriter {
        let mut w = BitWriter::new();
        w.put(1, 1); // header
        w.put(1, 1); // amp_res (overridden to 1.5 dB by a one-envelope FIXFIX)
        w.put(5, 4); // start
        w.put(9, 4); // stop
        w.put(0, 3); // xover
        w.put(0, 2);
        w.put(1, 1);
        w.put(1, 1);
        w.put(2, 2); // freq_scale
        w.put(1, 1); // alter_scale
        w.put(2, 2); // noise_bands
        w.put(2, 2); // limiter_bands
        w.put(3, 2); // limiter_gains: off
        w.put(1, 1); // interpol_freq
        w.put(1, 1); // smoothing_mode: off
        w.put(0, 1); // bs_data_extra
        w.put(0, 2); // FIXFIX
        w.put(0, 2); // one envelope
        w.put(1, 1); // high resolution
        w.put(0, 1); // envelope in frequency
        w.put(0, 1); // noise in frequency
        for _ in 0..n_q {
            w.put(0, 2); // inverse filtering off
        }
        w.put(level, 7);
        let (c, l) = t::F_ENV_1_5DB[60];
        for _ in 1..n_high {
            w.put(c, u32::from(l));
        }
        w.put(30, 5);
        let (c, l) = t::F_ENV_3_0DB[31];
        for _ in 1..n_q {
            w.put(c, u32::from(l));
        }
        w.put(0, 1); // no sinusoids
        w.put(0, 1); // no extended data
        w
    }

    /// Mean power per output subband over frames 6.., white noise core.
    fn hf_power(frame_len: usize) -> (Vec<f64>, FreqTables) {
        let t = FreqTables::new(48000, &Header { start_freq: 5, stop_freq: 9, ..Header::default() }).unwrap();
        let mut sbr = Sbr::new(48000, frame_len, 1, false);
        let w = payload(40, t.n_high(), t.n_q());
        let mut seed = 0x1234_5678u32;
        let mut out = vec![vec![0.0f32; 2 * frame_len]];
        let mut acc = vec![0.0f64; 64];
        let mut ana = qmf::Analysis::new(2 * frame_len);
        let mut rows = vec![[Complex32::default(); 32]; 2 * frame_len / 32];
        for f in 0..14 {
            sbr.parse(&mut BitReader::new(&w.bytes), false, false).unwrap();
            let core: Vec<f32> = (0..frame_len)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed as i32 as f32) / 2.0e5
                })
                .collect();
            sbr.process(&[core], &mut out, 1.0);
            if f >= 6 {
                // A 32-band analysis of the 48 kHz output gives 750 Hz bands:
                // pairs of SBR subbands.
                ana.run(&out[0], &mut rows);
                for r in &rows {
                    for (k, v) in r.iter().enumerate() {
                        acc[2 * k] += f64::from(v.norm_sqr()) / rows.len() as f64;
                    }
                }
            }
        }
        (acc, t)
    }

    /// The high band meets the same envelope at both frame lengths: with the
    /// limiter off and a white core, its power per band matches between 960
    /// and 1024 to within a fraction of a dB, which checks the 15-slot time
    /// grid, the 30-row buffers and the energy normalization of the shorter
    /// frame.
    #[test]
    fn high_band_power_is_the_same_at_960_and_1024() {
        let (long, t) = hf_power(1024);
        let (short, _) = hf_power(960);
        let mut checked = 0;
        for k in (t.kx / 2 + 1)..(t.k2 / 2 - 1) {
            let (a, b) = (long[2 * k], short[2 * k]);
            let d = 10.0 * (b / a).log10();
            assert!(d.abs() < 0.5, "750 Hz band {k}: 960 is {d:+.2} dB from 1024");
            checked += 1;
        }
        assert!(checked >= 8);
        // And there is a regenerated band to compare: far above what lies
        // past k2, where nothing is generated.
        let above = short[2 * (t.k2 / 2 + 2)].max(1e-30);
        assert!(short[2 * (t.kx / 2 + 2)] > 1e3 * above, "no high band");
    }
}
