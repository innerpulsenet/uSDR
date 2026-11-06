//! HFDL, the HF Data Link of ARINC 635 (ICAO Doc 9741): aircraft and their
//! ground stations trading ACARS and link control messages on HF, heard
//! here as upper sideband audio with the carrier 1440 Hz above the dial
//! (the assigned frequency is the dial).
//!
//! The signal is PSK at 1800 symbols a second with a root raised cosine
//! shape (roll-off 0.31), in bursts that fill one or two of the 13 slots of
//! a 32 second frame. A ground station opens every frame with a squitter.
//! Each burst is a prekey (a quarter second of plain carrier), a 531
//! symbol preamble of 2-PSK, and then data: frames of 30 unknown symbols
//! and a known 15 symbol probe. The preamble is the 127 symbol A sequence
//! twice, M1 (one of eight shifts of another 127 symbol sequence, which
//! says the data rate and whether the 1.8 or 4.2 second interleaver is
//! used), M2 (the first 15 symbols of the same shift) and nine copies of
//! the probe sequence T. Data is the user bits coded at rate 1/2 (each
//! chip sent twice at 300 bps), interleaved, mapped onto 2-PSK (300 and
//! 600 bps), 4-PSK (1200) or 8-PSK (1800) with Gray coding, and turned by
//! 180 degrees wherever a 120 symbol scrambling pattern has a 1.
//!
//! The receiver:
//!
//! - **Front end.** The audio is mixed down by 1440 Hz, cut to about 8 kHz
//!   and matched filtered. Nothing here assumes the sample rate is a round
//!   number: every later stage reads that stream at times of its own
//!   choosing through a cubic interpolator.
//! - **Finding a burst.** Twice a symbol the last 254 symbol times are
//!   correlated with A twice over, in 12 symbol pieces whose powers are
//!   added rather than their values, so a carrier up to about 70 Hz off
//!   still lines up. The correlation is normalized by the signal's power,
//!   so the level does not matter. The prekey's plain carrier matches those
//!   pieces too, by their imbalance of ones and zeros, but it matches a run
//!   of ones better, which A does not, and that rules it out. Every peak
//!   (the best point within eight symbols, which takes the strongest of
//!   paths a few milliseconds apart) is then checked against what follows:
//!   M1 and M2 correlated in two coherent halves, frequency corrected,
//!   against all eight shifts. A burst's own shift explains much of the
//!   power and the others almost none, so this both confirms the burst
//!   (and throws out the half-height peak a copy of A gives one sequence
//!   early) and reads its setting. The phase step between neighboring
//!   pieces over A, A, M1 and M2 gives the carrier frequency.
//! - **Equalizing.** Once the whole burst is in, it is read out half a
//!   symbol apart from the peak, corrected in frequency (refined by a fit
//!   to the phase of the whole known preamble) and scaled to unit power.
//!   An equalizer of 21 taps half a symbol apart, adapted by recursive
//!   least squares, trains on the preamble, then on each probe, and between
//!   probes, at a fifth of the weight, on its own decisions. That handles
//!   two HF paths a few milliseconds apart fading independently, follows
//!   the slow timing drift of a sample clock 100 ppm off, and with a
//!   carrier loop on its output takes out what frequency error is left.
//!   4-PSK and 8-PSK bursts go first through its decision feedback form
//!   (10 past decisions fed back), 2-PSK first through the linear form,
//!   which does better near the decoding threshold; a burst that fails its
//!   frame check is run through the other (the `demod` module says why).
//! - **SNR.** The channel's impulse response is fitted to the known
//!   preamble by least squares; the taps' power over the residual's is the
//!   symbol energy over the noise density, given in 3 kHz.
//! - **Decoding.** Each data symbol is unscrambled and turned into one
//!   log-likelihood ratio per chip, scaled by the error the equalizer made
//!   on the probes either side, so chips in a fade count for less. The
//!   chips are deinterleaved, repeated chips added, and the most likely
//!   bits found by a soft decision Viterbi decoder.
//! - **PDUs.** The bytes are a squitter or an MPDU holding LPDUs (logons,
//!   data), whose data is HF network PDUs (performance and frequency
//!   reports with the aircraft's position, system tables) or ACARS. Each
//!   level has its own frame check sequence; nothing is reported from a
//!   PDU that fails it. See [`pdu`] for what is parsed.

mod demod;
mod fec;
pub mod pdu;
mod phy;
#[cfg(test)]
mod synth;
#[cfg(test)]
mod tests;

pub use pdu::{
    Acars, BurstInfo, Direction, GsStatus, HfdlEvent, HfdlPdu, Link, Logon, LogonKind, OtherPdu,
    Position, PropagatingFreqs, Squitter,
};

use demod::{LEAD_SYM, TAIL_SYM};
use num_complex::{Complex32, Complex64};
use phy::{A_LEN, CARRIER_HZ, M1_LEN, M2_LEN, MODES, SYMBOL_RATE, Tables};
use scannerd_dsp::DecimFir;
use std::collections::VecDeque;

/// The front end's rate is the input rate divided by the largest whole
/// number that keeps it at or above this: four samples a symbol, and room
/// for the decimating filter's transition above the 1.2 kHz half-band.
const FRONT_MIN_RATE: f32 = 7_200.0;
/// The decimating lowpass's cutoff.
const FRONT_CUTOFF_HZ: f32 = 2_000.0;
/// The matched filter's reach either side, symbols.
const MF_SPAN_SYM: f64 = 6.0;
/// Correlation pieces, symbols.
const SEG: usize = 12;
/// Normalized correlation a peak must reach to be checked. Noise alone
/// scores 1/SEG (0.083) with a spread of about 0.02; a clean burst 1.
const DETECT_RHO: f32 = 0.25;
/// A peak must top the correlation this many detection points (eight
/// symbols) either side. That takes the strongest of paths a few
/// milliseconds apart, and the 254 symbol window ignores the rest.
const PEAK_HALF: usize = 16;
/// M1 and M2, frequency corrected and correlated in two halves, must
/// explain at least this share of the power for the best of the eight
/// shifts, and this many times more than the next best. Noise or a wrong
/// shift explains about 1/71; so does the prekey's plain tone, which the A
/// correlation in pieces does not tell from a burst.
const M1_RHO: f32 = 0.1;
const M1_LEAD: f32 = 3.0;
/// Symbols of front end output kept behind the newest detection point:
/// enough to read a burst out after its A peak has been held and checked.
const KEEP_SYM: usize = 2 * A_LEN + M1_LEN + M2_LEN + PEAK_HALF + 64;

/// What the decoder is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HfdlStatus {
    /// A burst's preamble has been found and its data is coming in.
    pub locked: bool,
    /// Where the last burst's carrier was, Hz above the dial.
    pub center_hz: Option<f32>,
    /// The last burst's signal to noise in 3 kHz, dB.
    pub snr_db: Option<f32>,
    /// The last burst's carrier offset from 1440 Hz.
    pub freq_error_hz: Option<f32>,
    /// The last burst's data rate.
    pub rate_bps: Option<u16>,
    /// The last burst used the 4.2 second (double slot) interleaver.
    pub long_interleaver: Option<bool>,
    /// Bursts whose preamble was found and setting read.
    pub bursts: u64,
    /// Bursts whose SPDU or MPDU header passed its check.
    pub decoded: u64,
    /// Bursts that did not.
    pub crc_failed: u64,
    /// LPDUs that failed their own check inside a good MPDU.
    pub lpdu_failed: u64,
}

struct Pending {
    /// Front end sample time of the first A symbol.
    t_a: f64,
    /// Carrier offset, Hz.
    freq_hz: f64,
    mode: usize,
    /// Front end sample needed before the burst can be read out.
    end_t: f64,
}

/// An HFDL receiver for one channel's upper sideband audio.
pub struct HfdlDecoder {
    fs2: f64,
    /// Front end samples a symbol.
    sps: f64,
    osc: Complex64,
    osc_step: Complex64,
    osc_count: u32,
    lp: DecimFir,
    mixed: Vec<Complex32>,
    lp_out: Vec<Complex32>,
    mf_taps: Vec<f32>,
    /// Matched filter history, written twice so the newest taps' worth is
    /// always one contiguous slice.
    mf_hist: Vec<Complex32>,
    mf_pos: usize,
    /// Matched filter output, `mf[0]` being front end sample `mf_base`.
    mf: Vec<Complex32>,
    mf_base: u64,
    /// Detection points, half a symbol apart: `det[0]` is point `det_base`,
    /// at front end time `point * sps / 2`.
    det: Vec<Complex32>,
    det_base: u64,
    /// The last `2 * PEAK_HALF + 1` correlation values, for finding peaks.
    rho_hist: VecDeque<(u64, f32)>,
    /// Peaks waiting for their M1 and M2 to come in.
    peaks: Vec<u64>,
    suppress_until: u64,
    pending: Option<Pending>,
    tables: Tables,
    /// A twice.
    aa: Vec<f32>,
    /// As many ones, the template of a plain carrier.
    ones: Vec<f32>,
    events: Vec<HfdlEvent>,
    status: HfdlStatus,
}

impl HfdlDecoder {
    /// A receiver for audio at `fs` samples a second (any rate from about
    /// 7.2 kHz up; it need not be a round number).
    pub fn new(fs: f32) -> Self {
        let decim = ((fs / FRONT_MIN_RATE).floor() as usize).max(1);
        let fs2 = fs as f64 / decim as f64;
        let sps = fs2 / SYMBOL_RATE;
        let half = (MF_SPAN_SYM * sps).ceil() as i64;
        let mut mf_taps: Vec<f32> = (-half..=half)
            .map(|i| phy::rrc((i as f64 / sps) as f32))
            .collect();
        let norm = mf_taps.iter().map(|t| t * t).sum::<f32>().sqrt();
        mf_taps.iter_mut().for_each(|t| *t /= norm);
        let n = mf_taps.len();
        let tables = Tables::new();
        let aa: Vec<f32> = tables.a.iter().chain(tables.a.iter()).copied().collect();
        Self {
            fs2,
            sps,
            osc: Complex64::new(1.0, 0.0),
            osc_step: Complex64::from_polar(
                1.0,
                -2.0 * std::f64::consts::PI * CARRIER_HZ / fs as f64,
            ),
            osc_count: 0,
            lp: DecimFir::new(FRONT_CUTOFF_HZ, fs, decim, 8 * decim + 1),
            mixed: Vec::new(),
            lp_out: Vec::new(),
            mf_taps,
            mf_hist: vec![Complex32::new(0.0, 0.0); 2 * n],
            mf_pos: 0,
            mf: Vec::new(),
            mf_base: 0,
            det: Vec::new(),
            // The first point the interpolator can reach.
            det_base: 4,
            rho_hist: VecDeque::new(),
            peaks: Vec::new(),
            suppress_until: 0,
            pending: None,
            tables,
            ones: vec![1.0; aa.len()],
            aa,
            events: Vec::new(),
            status: HfdlStatus::default(),
        }
    }

    /// Feed audio, the dial at 0 Hz. A burst is decoded once all of it
    /// has arrived, up to 4.5 s after it began.
    pub fn push(&mut self, audio: &[f32]) {
        // Mix the carrier to 0 Hz.
        self.mixed.clear();
        for &s in audio {
            self.mixed.push(Complex32::new(
                (s as f64 * self.osc.re) as f32,
                (s as f64 * self.osc.im) as f32,
            ));
            self.osc *= self.osc_step;
            self.osc_count += 1;
            if self.osc_count == 4096 {
                self.osc /= self.osc.norm();
                self.osc_count = 0;
            }
        }
        let mixed = std::mem::take(&mut self.mixed);
        self.lp.process(&mixed, &mut self.lp_out);
        self.mixed = mixed;
        let n = self.mf_taps.len();
        for i in 0..self.lp_out.len() {
            let s = self.lp_out[i];
            self.mf_hist[self.mf_pos] = s;
            self.mf_hist[self.mf_pos + n] = s;
            self.mf_pos = (self.mf_pos + 1) % n;
            let win = &self.mf_hist[self.mf_pos..self.mf_pos + n];
            let mut acc = Complex32::new(0.0, 0.0);
            for (x, t) in win.iter().zip(self.mf_taps.iter()) {
                acc += x * *t;
            }
            self.mf.push(acc);
        }
        self.run_detector();
        self.trim();
    }

    /// The PDUs decoded since the last call, oldest first.
    pub fn take(&mut self) -> Vec<HfdlEvent> {
        std::mem::take(&mut self.events)
    }

    /// What the receiver is doing, and how its last burst went.
    pub fn status(&self) -> HfdlStatus {
        HfdlStatus {
            locked: self.pending.is_some(),
            ..self.status
        }
    }

    /// The front end stream at time `t` (samples), by cubic interpolation;
    /// `None` until the samples around it are in.
    fn at(&self, t: f64) -> Option<Complex32> {
        let i = t.floor();
        let mu = (t - i) as f32;
        let i = i as i64 - self.mf_base as i64;
        if i < 1 || i as usize + 2 >= self.mf.len() {
            return None;
        }
        let i = i as usize;
        let (a, b, c, d) = (self.mf[i - 1], self.mf[i], self.mf[i + 1], self.mf[i + 2]);
        let wa = -mu * (mu - 1.0) * (mu - 2.0) / 6.0;
        let wb = (mu + 1.0) * (mu - 1.0) * (mu - 2.0) / 2.0;
        let wc = -(mu + 1.0) * mu * (mu - 2.0) / 2.0;
        let wd = (mu + 1.0) * mu * (mu - 1.0) / 6.0;
        Some(a * wa + b * wb + c * wc + d * wd)
    }

    /// Correlation of `seq` against detection points from `m0`, symbol `j`
    /// at point `m0 + 2j`, in `SEG` symbol pieces: the pieces' values and
    /// the share of the signal's power they explain (0 to 1).
    fn correlate(&self, m0: u64, seq: &[f32]) -> Option<(Vec<Complex32>, f32)> {
        let start = m0.checked_sub(self.det_base)? as usize;
        let end = start + 2 * (seq.len() - 1);
        if end >= self.det.len() {
            return None;
        }
        let used = seq.len() / SEG * SEG;
        let skip = seq.len() - used;
        let mut pieces = Vec::with_capacity(seq.len() / SEG);
        let mut energy = 0f32;
        let mut power = 0f32;
        for (p, chunk) in seq[skip..].chunks_exact(SEG).enumerate() {
            let mut c = Complex32::new(0.0, 0.0);
            for (j, &s) in chunk.iter().enumerate() {
                let d = self.det[start + 2 * (skip + p * SEG + j)];
                c += d * s;
                energy += d.norm_sqr();
            }
            power += c.norm_sqr();
            pieces.push(c);
        }
        let rho = if energy > 0.0 {
            power / (SEG as f32 * energy)
        } else {
            0.0
        };
        Some((pieces, rho))
    }

    fn run_detector(&mut self) {
        let step = self.sps / 2.0;
        let a_end = 2 * (2 * A_LEN as u64 - 1);
        loop {
            let m = self.det_base + self.det.len() as u64;
            let Some(v) = self.at(m as f64 * step) else {
                break;
            };
            self.det.push(v);
            if let Some(p) = &self.pending
                && self.mf_base as f64 + self.mf.len() as f64 > p.end_t + 4.0
            {
                self.finish_burst();
            }
            let mut rho = if m < self.suppress_until || m < a_end {
                0.0
            } else {
                self.correlate(m - a_end, &self.aa)
                    .map(|(_, r)| r)
                    .unwrap_or(0.0)
            };
            // A plain carrier (the prekey) matches A's pieces by their
            // imbalance of ones and zeros, up to half as well as A itself.
            // It matches a run of ones far better, which A does not.
            if rho > DETECT_RHO {
                let tone = self
                    .correlate(m - a_end, &self.ones)
                    .map(|(_, r)| r)
                    .unwrap_or(1.0);
                if tone >= rho {
                    rho = 0.0;
                }
            }
            // A peak is a point above the threshold and no lower than any
            // within `PEAK_HALF` points either side (later ties go to the
            // earlier point).
            self.rho_hist.push_back((m, rho));
            if self.rho_hist.len() > 2 * PEAK_HALF + 1 {
                self.rho_hist.pop_front();
            }
            if self.rho_hist.len() == 2 * PEAK_HALF + 1 {
                let (cm, cr) = self.rho_hist[PEAK_HALF];
                let is_peak = cr > DETECT_RHO
                    && self.rho_hist.iter().enumerate().all(|(i, &(_, r))| {
                        i == PEAK_HALF || if i < PEAK_HALF { r < cr } else { r <= cr }
                    });
                if is_peak {
                    self.peaks.push(cm);
                }
            }
            // Check queued peaks once M1 and M2 are in.
            let need = 2 * (M1_LEN + M2_LEN) as u64 + 8;
            while let Some(&pm) = self.peaks.first() {
                if m < pm + need {
                    break;
                }
                self.peaks.remove(0);
                if pm >= self.suppress_until && self.check_peak(pm) {
                    self.peaks.clear();
                    self.rho_hist.clear();
                    break;
                }
            }
        }
    }

    /// A correlation peak at point `pm` (the last symbol of the second A):
    /// accept it if M1 and M2 follow, read the setting from which shift
    /// they are, measure the carrier, and wait for the rest of the burst.
    fn check_peak(&mut self, pm: u64) -> bool {
        let a_end = 2 * (2 * A_LEN as u64 - 1);
        let Some(m0) = pm.checked_sub(a_end) else {
            return false;
        };
        // The carrier, roughly, from the phase step between the A pieces.
        let Some((pieces, _)) = self.correlate(m0, &self.aa) else {
            return false;
        };
        let coarse = Self::piece_freq(&pieces);
        // M1 and M2 correlated in two halves, the carrier taken out, their
        // powers added (fading turns the phase a little over 80 ms): a match
        // explains a good share of the power, a wrong shift or noise about
        // 1/71.
        let w = -2.0 * std::f64::consts::PI * coarse / SYMBOL_RATE;
        let mut scores: Vec<(f32, usize)> = (0..MODES.len())
            .map(|i| {
                let seq = &self.tables.m[i];
                let mut best = 0f32;
                for lag in [pm + 1, pm + 2, pm + 3] {
                    let Some(start) = lag.checked_sub(self.det_base) else {
                        continue;
                    };
                    let start = start as usize;
                    if start + 2 * seq.len() > self.det.len() {
                        continue;
                    }
                    let mut c = [Complex64::new(0.0, 0.0); 2];
                    let mut e = 0f64;
                    let half = seq.len() / 2;
                    for (j, &sv) in seq.iter().enumerate() {
                        let d = self.det[start + 2 * j];
                        let d = Complex64::new(d.re as f64, d.im as f64);
                        c[(j >= half) as usize] +=
                            d * Complex64::from_polar(sv as f64, w * j as f64);
                        e += d.norm_sqr();
                    }
                    if e > 0.0 {
                        let p = c[0].norm_sqr() + c[1].norm_sqr();
                        best = best.max((p / (half as f64 * e)) as f32);
                    }
                }
                (best, i)
            })
            .collect();
        scores.sort_by(|a, b| b.0.total_cmp(&a.0));
        let (best, mode) = scores[0];
        if best < M1_RHO || best < M1_LEAD * scores[1].0 {
            return false;
        }
        // Timing to a fraction of a point from the correlation either side.
        let rho_at =
            |s: &Self, m: u64| s.correlate(m - a_end, &s.aa).map(|(_, r)| r).unwrap_or(0.0);
        let (rl, rc, rr) = (rho_at(self, pm - 1), rho_at(self, pm), rho_at(self, pm + 1));
        let den = rl - 2.0 * rc + rr;
        let frac = if den < 0.0 {
            (0.5 * (rl - rr) / den).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        // The carrier again over A, A, M1 and M2 together.
        let known: Vec<f32> = self
            .aa
            .iter()
            .chain(self.tables.m[mode].iter())
            .copied()
            .collect();
        let freq_hz = match self.correlate(m0, &known) {
            Some((pieces, _)) => Self::piece_freq(&pieces),
            None => coarse,
        };
        let t_a = (m0 as f64 + frac as f64) * self.sps / 2.0;
        let total = MODES[mode].burst_symbols();
        let end_t = t_a + (total + TAIL_SYM) as f64 * self.sps + 3.0;
        self.status.bursts += 1;
        self.suppress_until = m0 + 2 * total as u64;
        self.pending = Some(Pending {
            t_a,
            freq_hz,
            mode,
            end_t,
        });
        true
    }

    /// Frequency from the phase step between neighboring correlation
    /// pieces, Hz; unambiguous to ±75 Hz.
    fn piece_freq(pieces: &[Complex32]) -> f64 {
        let turn: Complex32 = pieces.windows(2).map(|w| w[1] * w[0].conj()).sum();
        turn.arg() as f64 / (2.0 * std::f64::consts::PI * SEG as f64 / SYMBOL_RATE)
    }

    fn finish_burst(&mut self) {
        let Some(p) = self.pending.take() else { return };
        let mode = MODES[p.mode];
        let total = mode.burst_symbols();
        let n = 2 * (LEAD_SYM + total + TAIL_SYM);
        let step = self.sps / 2.0;
        let raw: Vec<Complex32> = (0..n)
            .map(|i| {
                self.at(p.t_a + (i as f64 - 2.0 * LEAD_SYM as f64) * step)
                    .unwrap_or_default()
            })
            .collect();
        let derotate = |freq_hz: f64| -> Vec<Complex32> {
            let w = -2.0 * std::f64::consts::PI * freq_hz / self.fs2;
            raw.iter()
                .enumerate()
                .map(|(i, v)| {
                    let rot =
                        Complex64::from_polar(1.0, w * (i as f64 - 2.0 * LEAD_SYM as f64) * step);
                    Complex32::new(
                        (v.re as f64 * rot.re - v.im as f64 * rot.im) as f32,
                        (v.re as f64 * rot.im + v.im as f64 * rot.re) as f32,
                    )
                })
                .collect()
        };
        let freq_hz = p.freq_hz + demod::fine_freq(&derotate(p.freq_hz), p.mode, &self.tables);
        let mut x = derotate(freq_hz);
        // Unit power over the two A sequences.
        let a = &x[2 * LEAD_SYM..2 * (LEAD_SYM + 2 * A_LEN)];
        let pw = a.iter().map(|v| v.norm_sqr()).sum::<f32>() / a.len() as f32;
        if pw > 0.0 {
            let g = 1.0 / pw.sqrt();
            x.iter_mut().for_each(|v| *v *= g);
        }
        // The linear equalizer first for 2-PSK, which decodes near the
        // threshold where feedback errors would spread; decision feedback
        // first for 4-PSK and 8-PSK, which need it on two paths. The other
        // shape if the first fails its check.
        let first = mode.bits_per_symbol > 1;
        let mut d = demod::demod(&x, p.mode, &self.tables, first);
        let mut parsed = pdu::parse_burst(&d.bytes);
        if parsed.is_none() {
            let d2 = demod::demod(&x, p.mode, &self.tables, !first);
            let p2 = pdu::parse_burst(&d2.bytes);
            if p2.is_some() {
                d = d2;
                parsed = p2;
            }
        }
        let freq_error_hz = (freq_hz + d.residual_hz as f64) as f32;
        let burst = BurstInfo {
            rate_bps: mode.rate_bps,
            long_interleaver: mode.long,
            freq_error_hz,
            center_hz: CARRIER_HZ as f32 + freq_error_hz,
            snr_db: demod::channel_snr(&x, p.mode, &self.tables).unwrap_or(d.snr_db),
        };
        self.status.center_hz = Some(burst.center_hz);
        self.status.snr_db = Some(burst.snr_db);
        self.status.freq_error_hz = Some(freq_error_hz);
        self.status.rate_bps = Some(mode.rate_bps);
        self.status.long_interleaver = Some(mode.long);
        match parsed {
            Some(parsed) => {
                self.status.decoded += 1;
                self.status.lpdu_failed += parsed.lpdu_failed as u64;
                self.events
                    .extend(parsed.pdus.into_iter().map(|pdu| HfdlEvent { burst, pdu }));
            }
            None => self.status.crc_failed += 1,
        }
    }

    fn trim(&mut self) {
        // Front end samples: what a pending burst needs, and enough behind
        // the detector to read out a burst it has yet to confirm.
        let det_t = (self.det_base + self.det.len() as u64) as f64 * self.sps / 2.0;
        let mut keep = det_t - KEEP_SYM as f64 * self.sps;
        if let Some(p) = &self.pending {
            keep = keep.min(p.t_a - (LEAD_SYM + 4) as f64 * self.sps);
        }
        let keep = keep.max(0.0) as u64;
        if keep > self.mf_base + 8192 {
            let k = ((keep - self.mf_base) as usize).min(self.mf.len());
            self.mf.drain(..k);
            self.mf_base += k as u64;
        }
        let keep_pts = 2 * (2 * A_LEN + M1_LEN + M2_LEN + PEAK_HALF + 16);
        if self.det.len() > keep_pts + 8192 {
            let k = self.det.len() - keep_pts;
            self.det.drain(..k);
            self.det_base += k as u64;
        }
    }
}
