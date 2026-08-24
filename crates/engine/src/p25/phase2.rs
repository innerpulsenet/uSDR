//! P25 Phase 2 (TDMA) reception: the second half of M4.
//!
//! Phase 1 (C4FM, in the parent module) carries the control channel even on a
//! Phase 2 system, so everything here is about the **voice** channels a grant
//! points at. The air is H-DQPSK at 6000 symbols/s, but an FM discriminator
//! recovers it: the phase change across a symbol is proportional to the average
//! frequency over it, so the same instantaneous-frequency front end the Phase 1
//! chain uses slices into the same Gray-coded dibits — only the levels differ
//! (±750 and ±2250 Hz here) and there is no status-symbol insertion.
//!
//! Structure of the signal, all established from op25/dsd (algorithms borrowed,
//! not code):
//!
//! - A **superframe** is 12 bursts; a burst is 180 dibits. The first 20 dibits
//!   are a 40-bit ISCH; the rest is payload.
//! - The ISCH is either the fixed **S-ISCH** sync word or one of 128 **I-ISCH**
//!   codewords that number the slot within the superframe. Tracking those keeps
//!   the two logical timeslots (channels 0 and 1) apart.
//! - Payload is **scrambled** by a system-specific LFSR keyed on WACN, system
//!   ID and NAC — all three read from the Phase 1 control channel.
//! - Descrambled, a burst is voice (4V/2V AMBE codewords) or signalling
//!   (SACCH/FACCH/LCCH), the latter protected by Reed–Solomon and a CRC and
//!   carrying the MAC PDUs that name the talkgroup and source.
//!
//! What M4(b) proves without any vocoder: a locked superframe whose MAC_SIGNAL
//! carries the system's NAC, and MAC PDUs whose group address matches the grant
//! that sent us here. Decoding the AMBE is M4(c).

mod acch;
mod duid;
mod framer;
mod isch;
mod scramble;
pub(crate) mod voice;

pub use acch::MacPdu;
pub use duid::BurstType;
pub use framer::{Burst, Phase2Framer};
pub use isch::{Isch, IschInfo};
pub use scramble::Scrambler;

use num_complex::Complex32;
use scannerd_dsp::OnePole;
use std::f32::consts::TAU;

/// Symbol rate on a Phase 2 channel: 6000 symbols/s.
pub const SYMBOL_RATE: f64 = 6000.0;

/// Samples per symbol at the 48 kHz channel rate the DecodeChain targets.
pub const SPS: usize = 8;

/// Channel rate that yields exactly [`SPS`] samples per symbol.
pub const CHANNEL_RATE: f64 = SYMBOL_RATE * SPS as f64;

/// Channel filter for Phase 2 H-DQPSK. Matches the 12.5 kHz allocation so a
/// residual carrier offset cannot clip the outer ±2250 Hz symbols.
pub const PHASE2_BANDWIDTH_HZ: f32 = 12_500.0;

/// Outer and inner deviations. H-DQPSK phase steps ±3π/4 and ±π/4 per symbol
/// become these average frequencies at 6000 sym/s (±0.75 and ±0.25 of half the
/// symbol rate).
pub const DEV_OUTER_HZ: f32 = 2250.0;
pub const DEV_INNER_HZ: f32 = 750.0;

/// Dibits in one burst, and the ISCH that leads it.
pub const BURST_DIBITS: usize = 180;
pub const ISCH_DIBITS: usize = 20;
pub const PAYLOAD_DIBITS: usize = BURST_DIBITS - ISCH_DIBITS;

/// Bursts in a superframe, and dibits across the whole thing — the period of
/// the scrambling sequence.
pub const SUPERFRAME_BURSTS: usize = 12;
pub const SUPERFRAME_DIBITS: usize = SUPERFRAME_BURSTS * BURST_DIBITS;

/// The dibit a Phase 2 frequency sample represents. Same Gray map as C4FM:
/// `01`→+outer, `00`→+inner, `10`→−inner, `11`→−outer.
pub fn slice_dibit(hz: f32) -> u8 {
    slice_at(hz, DEV_OUTER_HZ)
}

/// Slice against a fitted outer deviation, so a transmitter that is not
/// exactly at ±2250 Hz (or a residual carrier offset the front end has not
/// fully removed) does not walk the inner symbols across the midpoint.
pub fn slice_at(hz: f32, outer: f32) -> u8 {
    let mid = outer * (DEV_OUTER_HZ + DEV_INNER_HZ) / (2.0 * DEV_OUTER_HZ);
    if hz > mid {
        0b01
    } else if hz > 0.0 {
        0b00
    } else if hz > -mid {
        0b10
    } else {
        0b11
    }
}

/// Least-squares fit of `samples ≈ amp · ideal + offset`.
///
/// Same estimator the Phase 1 sync uses: the 20 known S-ISCH symbols (or a
/// decoded I-ISCH) recover both the deviation the slicer should use and the
/// leftover carrier the slow DC tracker missed.
pub fn fit_symbols(samples: &[f32], ideal: &[f32]) -> (f32, f32) {
    let n = samples.len().min(ideal.len()) as f32;
    if n < 2.0 {
        return (DEV_OUTER_HZ, 0.0);
    }
    let (mut sx, mut sy, mut sxy, mut sxx) = (0.0, 0.0, 0.0, 0.0);
    for (y, x) in samples.iter().zip(ideal) {
        sx += x;
        sy += y;
        sxy += x * y;
        sxx += x * x;
    }
    let den = n * sxx - sx * sx;
    if den.abs() < 1e-6 {
        return (DEV_OUTER_HZ, 0.0);
    }
    let slope = (n * sxy - sx * sy) / den;
    let offset = (sy - slope * sx) / n;
    ((slope * DEV_OUTER_HZ).abs().max(100.0), offset)
}

/// Ideal frequency for a dibit, the inverse of [`slice_dibit`].
pub fn dibit_level(dibit: u8) -> f32 {
    match dibit & 0b11 {
        0b01 => DEV_OUTER_HZ,
        0b00 => DEV_INNER_HZ,
        0b10 => -DEV_INNER_HZ,
        _ => -DEV_OUTER_HZ,
    }
}

/// A Phase 2 receiver: front end and framer wired together, consuming complex
/// baseband and producing tracked bursts.
///
/// This is the Phase 2 counterpart to the Phase 1 `C4fmFrontEnd` + `FrameDetector`
/// pairing, and the piece `follow` and the server drive. Descrambling and MAC
/// parsing layer on top of the bursts it yields.
pub struct Phase2Receiver {
    front: Phase2FrontEnd,
    framer: Phase2Framer,
    hz: Vec<f32>,
    /// Running stats of the frequency samples, for diagnosing the demod: a
    /// non-zero mean is a carrier offset, and the spread reveals the true
    /// deviation the fixed slicer is being asked to cut.
    diag_n: u64,
    diag_sum: f64,
    diag_sumsq: f64,
    diag_absmax: f32,
    /// Decision-directed signal-to-error ratio for the most recent block.
    last_quality_db: f32,
}

/// System-specific layer above framing: identifies, descrambles and decodes a
/// burst once WACN/SYSID/NAC have been learned from the control channel.
pub struct Phase2Decoder {
    scrambler: Scrambler,
    vocoder: rmbe::Decoder,
    encrypted: Option<bool>,
    algorithm_id: Option<u8>,
    key_id: Option<u16>,
    message_indicator: Option<[u8; 9]>,
    decryptor: Option<crate::crypto::VoiceDecryptor>,
    /// Four 4V bursts carry ESS-B, followed by one 2V burst carrying ESS-A.
    /// Keeping this state permits encryption detection after joining mid-call.
    ess_b: [u8; 16],
    voice_burst_id: i8,
    /// op25/mbelib's smoothed protected-bit error estimate. Unprotected AMBE
    /// bits become unreliable when this remains high even if Golay can still
    /// produce a nearest codeword.
    voice_error_rate: f32,
}

#[derive(Clone, Debug)]
pub struct DecodedBurst {
    pub kind: BurstType,
    pub mac: Option<MacPdu>,
    pub voice: Vec<[u8; 7]>,
    /// 8 kHz signed PCM, 160 samples per AMBE frame. Frames rejected by the
    /// quality guard are represented by silence so call timing stays exact.
    pub audio: Vec<i16>,
    pub corrected_bits: u32,
    pub rejected_voice: usize,
    pub encrypted: Option<bool>,
    /// Ciphertext was successfully transformed with a matching loaded key.
    pub decrypted: bool,
    pub algorithm_id: Option<u8>,
    pub key_id: Option<u16>,
    pub message_indicator: Option<[u8; 9]>,
    /// RS symbols repaired while recovering Phase 2 ESS on this burst.
    pub ess_corrected: Option<u8>,
}

impl Phase2Decoder {
    pub fn new(wacn: u32, sysid: u16, nac: u16) -> Self {
        Self {
            scrambler: Scrambler::new(wacn, sysid, nac),
            vocoder: rmbe::Decoder::new(),
            encrypted: None,
            algorithm_id: None,
            key_id: None,
            message_indicator: None,
            decryptor: None,
            ess_b: [0; 16],
            voice_burst_id: -1,
            voice_error_rate: 0.0,
        }
    }

    pub fn decode(&mut self, burst: &Burst) -> DecodedBurst {
        let kind = duid::decode(&burst.payload);
        let mac = acch::decode(
            &burst.payload,
            kind,
            burst.superframe_slot,
            Some(&self.scrambler),
        );
        match &mac {
            Some(MacPdu::PushToTalk {
                algorithm,
                key_id,
                message_indicator,
                ..
            }) => {
                let algorithm = normalize_algorithm(*algorithm);
                self.algorithm_id = Some(algorithm);
                self.key_id = Some(*key_id);
                self.message_indicator = Some(*message_indicator);
                self.encrypted = Some(algorithm != 0x80);
                self.decryptor = crate::crypto::VoiceDecryptor::for_call(
                    crate::crypto::Protocol::P25,
                    algorithm,
                    *key_id,
                    message_indicator,
                );
                self.voice_burst_id = -1;
            }
            Some(MacPdu::EndPushToTalk { .. }) => {
                self.encrypted = None;
                self.algorithm_id = None;
                self.key_id = None;
                self.message_indicator = None;
                self.decryptor = None;
                self.voice_burst_id = -1;
            }
            _ => {}
        }
        let ess_corrected = self.update_ess(burst, kind);
        let extracted =
            voice::extract(&burst.payload, kind, burst.superframe_slot, &self.scrambler);
        let voice: Vec<[u8; 7]> = extracted.iter().map(|frame| frame.data).collect();
        let mut audio = Vec::new();
        let mut corrected_bits = 0u32;
        let mut rejected_voice = 0usize;
        if self.encrypted != Some(true) || self.decryptor.is_some() {
            for frame in &extracted {
                corrected_bits += u32::from(frame.errors);
                // This is the error-rate estimator and threshold used by op25.
                self.voice_error_rate =
                    0.95 * self.voice_error_rate + 0.001_064 * f32::from(frame.errors);
                let reliable = frame.valid && frame.errors <= 4 && self.voice_error_rate <= 0.096;
                if reliable {
                    let mut data = frame.data;
                    if let Some(decryptor) = &mut self.decryptor {
                        decryptor.apply_ambe49(&mut data);
                    }
                    if let Ok(samples) = self.vocoder.decode(&data) {
                        audio.extend(samples);
                    }
                } else {
                    // Preserve real time without feeding corrupt prediction
                    // parameters into the stateful AMBE synthesizer.
                    audio.extend([0i16; rmbe::SAMPLES_PER_FRAME]);
                    rejected_voice += 1;
                }
            }
        } else {
            for frame in &extracted {
                corrected_bits += u32::from(frame.errors);
                self.voice_error_rate =
                    0.95 * self.voice_error_rate + 0.001_064 * f32::from(frame.errors);
            }
        }
        // A length-valid rmbe frame is infallible; keep timing intact even if a
        // future implementation adds content validation.
        if (self.encrypted != Some(true) || self.decryptor.is_some())
            && audio.len() < voice.len() * rmbe::SAMPLES_PER_FRAME
        {
            while audio.len() < voice.len() * rmbe::SAMPLES_PER_FRAME {
                audio.push(0);
            }
        }
        DecodedBurst {
            kind,
            mac,
            voice,
            audio,
            corrected_bits,
            rejected_voice,
            encrypted: self.encrypted,
            decrypted: self.encrypted == Some(true) && self.decryptor.is_some(),
            algorithm_id: self.algorithm_id,
            key_id: self.key_id,
            message_indicator: self.message_indicator,
            ess_corrected,
        }
    }

    /// Accumulate and decode the Phase 2 Encryption Sync Sequence carried in
    /// voice bursts. This is the late-entry path: MAC_PTT may have happened
    /// before tuning to the traffic channel, but ESS repeats every five voice
    /// bursts and supplies ALGID, KID, and the 72-bit message indicator.
    fn update_ess(&mut self, burst: &Burst, kind: BurstType) -> Option<u8> {
        self.voice_burst_id = match kind {
            BurstType::Voice4 => (self.voice_burst_id + 1).rem_euclid(5),
            BurstType::Voice2 => 4,
            _ => return None,
        };

        let mut payload = burst.payload;
        for (i, dibit) in payload.iter_mut().enumerate() {
            *dibit = self
                .scrambler
                .apply(burst.superframe_slot as usize * 180 + 10 + i, *dibit);
        }
        const ESS_START: usize = 74;
        if self.voice_burst_id < 4 {
            let base = self.voice_burst_id as usize * 4;
            for i in 0..4 {
                let at = ESS_START + i * 3;
                self.ess_b[base + i] = hexbits(&payload[at..at + 3]);
            }
            return None;
        }

        let mut word = [0u8; 44];
        word[..16].copy_from_slice(&self.ess_b);
        let mut at = ESS_START;
        for i in 0..28 {
            word[16 + i] = hexbits(&payload[at..at + 3]);
            at += if i == 15 { 4 } else { 3 };
        }
        let corrected = super::rs64::correct(&mut word, 16)?;
        self.ess_b.copy_from_slice(&word[..16]);

        let algorithm = normalize_algorithm((word[0] << 2) | (word[1] >> 4));
        let key_id =
            (u16::from(word[1] & 0x0f) << 12) | (u16::from(word[2]) << 6) | u16::from(word[3]);
        let mut mi = [0u8; 9];
        for group in 0..3 {
            let j = group * 4 + 4;
            mi[group * 3] = (word[j] << 2) | (word[j + 1] >> 4);
            mi[group * 3 + 1] = ((word[j + 1] & 0x0f) << 4) | (word[j + 2] >> 2);
            mi[group * 3 + 2] = ((word[j + 2] & 0x03) << 6) | word[j + 3];
        }
        self.algorithm_id = Some(algorithm);
        self.key_id = Some(key_id);
        self.message_indicator = Some(mi);
        self.encrypted = Some(algorithm != 0x80);
        self.decryptor = crate::crypto::VoiceDecryptor::for_call(
            crate::crypto::Protocol::P25,
            algorithm,
            key_id,
            &mi,
        );
        Some(corrected)
    }
}

fn hexbits(dibits: &[u8]) -> u8 {
    (dibits[0] << 4) | (dibits[1] << 2) | dibits[2]
}

fn normalize_algorithm(algorithm: u8) -> u8 {
    // Mature Phase 2 receivers treat zero here as a known spurious clear value.
    if algorithm == 0 { 0x80 } else { algorithm }
}

/// Logical traffic channel carried by a superframe position. This is not a
/// simple alternating pattern in the final two positions.
pub fn logical_channel(superframe_slot: u8) -> u8 {
    const WHICH: [u8; 12] = [0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 1, 0];
    WHICH[usize::from(superframe_slot) % WHICH.len()]
}

impl Phase2Receiver {
    pub fn new(fs: f64) -> Self {
        Self {
            front: Phase2FrontEnd::new(fs),
            framer: Phase2Framer::new(fs),
            hz: Vec::new(),
            diag_n: 0,
            diag_sum: 0.0,
            diag_sumsq: 0.0,
            diag_absmax: 0.0,
            last_quality_db: 0.0,
        }
    }

    /// Feed one block of complex baseband; returns any completed bursts.
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<Burst> {
        self.front.process(iq, &mut self.hz);
        let mut signal = 0.0f64;
        let mut error = 0.0f64;
        for &h in &self.hz {
            self.diag_n += 1;
            self.diag_sum += f64::from(h);
            self.diag_sumsq += f64::from(h) * f64::from(h);
            self.diag_absmax = self.diag_absmax.max(h.abs());
            let ideal = f64::from(dibit_level(slice_dibit(h)));
            signal += ideal * ideal;
            let residual = f64::from(h) - ideal;
            error += residual * residual;
        }
        self.last_quality_db = if error > 0.0 {
            (10.0 * (signal / error).max(f64::MIN_POSITIVE).log10()) as f32
        } else {
            0.0
        };
        self.framer.push(&self.hz)
    }

    /// Whether the framer holds superframe lock.
    pub fn locked(&self) -> bool {
        self.framer.locked()
    }

    /// Total bursts seen, and how many had a decodable ISCH.
    pub fn stats(&self) -> (u64, u64) {
        (self.framer.bursts, self.framer.isch_ok)
    }

    /// Smallest Hamming distance to the S-ISCH sync the acquisition scan has
    /// seen. Near 0 means sync is close; stuck high means the demod is wrong.
    pub fn acq_best(&self) -> u32 {
        self.framer.acq_best
    }

    /// Decision-directed dibit SNR for the latest input block, in dB. Unlike
    /// raw tuner power this measures the part that determines decode quality.
    pub fn quality_db(&self) -> f32 {
        self.last_quality_db
    }

    /// Residual carrier the front-end DC tracker currently holds, in Hz.
    pub fn offset_hz(&self) -> f32 {
        self.front.offset_hz()
    }

    /// Frequency-sample diagnostics: `(mean_hz, rms_hz, abs_max_hz)`.
    ///
    /// On a live voice burst the mean is the residual carrier offset and the RMS
    /// reflects the deviation; comparing them against the ±750/±2250 Hz the
    /// slicer assumes shows whether an offset or a scale error is defeating it.
    pub fn diag(&self) -> (f32, f32, f32) {
        if self.diag_n == 0 {
            return (0.0, 0.0, 0.0);
        }
        let n = self.diag_n as f64;
        let mean = self.diag_sum / n;
        let var = (self.diag_sumsq / n - mean * mean).max(0.0);
        (mean as f32, var.sqrt() as f32, self.diag_absmax)
    }
}

/// FM discriminator plus a one-symbol boxcar, producing one frequency estimate
/// per input sample. This mirrors the Phase 1 front end (the transmitted pulse
/// is near-rectangular, so a boxcar is the matched filter) but omits the
/// carrier tracker: bursts are short and gated, and the per-symbol decision
/// here is made against fixed thresholds.
pub struct Phase2FrontEnd {
    prev: Complex32,
    hz_per_rad: f32,
    buf: [f32; SPS],
    head: usize,
    count: usize,
    sum: f32,
    /// Slow residual-carrier tracker.  The H-DQPSK dibits are balanced over
    /// time, while the dongle's tuning error is a DC term.  Live CLMRN voice
    /// carriers measured roughly +2 kHz here, enough to move both positive
    /// levels across the fixed slicer's thresholds without this correction.
    dc: OnePole,
}

impl Phase2FrontEnd {
    pub fn new(fs: f64) -> Self {
        Self {
            prev: Complex32::new(0.0, 0.0),
            hz_per_rad: (fs / TAU as f64) as f32,
            buf: [0.0; SPS],
            head: 0,
            count: 0,
            sum: 0.0,
            dc: OnePole::new((fs * 0.04) as f32),
        }
    }

    /// Complex baseband in, matched-filtered instantaneous frequency out.
    pub fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        out.clear();
        for &x in iq {
            let d = x * self.prev.conj();
            self.prev = x;
            let hz = if d.norm_sqr() > 0.0 {
                d.arg() * self.hz_per_rad
            } else {
                0.0
            };
            self.sum -= self.buf[self.head];
            self.buf[self.head] = hz;
            self.sum += hz;
            self.head = (self.head + 1) % SPS;
            if self.count < SPS {
                self.count += 1;
            }
            if self.count >= SPS {
                let acc = self.sum / SPS as f32;
                out.push(acc - self.dc.process(acc));
            }
        }
    }
    /// Current residual carrier estimate in Hz.
    pub fn offset_hz(&self) -> f32 {
        self.dc.value()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Modulate dibits as flat-held frequency symbols, the same near-rectangular
    /// pulse the live signal uses and the boxcar matches.
    pub(super) fn modulate(dibits: &[u8], fs: f64) -> Vec<Complex32> {
        let mut phase = 0.0f64;
        let mut out = Vec::with_capacity(dibits.len() * SPS);
        for &d in dibits {
            let hz = f64::from(dibit_level(d));
            for _ in 0..SPS {
                phase += TAU as f64 * hz / fs;
                out.push(Complex32::new(phase.cos() as f32, phase.sin() as f32));
            }
        }
        out
    }

    /// Hold each symbol for exactly `1/6000` s. At the analog scanner's
    /// channel rate (2.048 MS/s ÷ 43) that is not a whole number of samples.
    pub(super) fn modulate_timed(dibits: &[u8], fs: f64) -> Vec<Complex32> {
        if dibits.is_empty() || fs <= 0.0 {
            return Vec::new();
        }
        let symbol_s = 1.0 / SYMBOL_RATE;
        let n = ((dibits.len() as f64 * symbol_s * fs).round() as usize).max(1);
        let mut phase = 0.0f64;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / fs;
            let sym = ((t / symbol_s) as usize).min(dibits.len() - 1);
            let hz = f64::from(dibit_level(dibits[sym]));
            phase += TAU as f64 * hz / fs;
            out.push(Complex32::new(phase.cos() as f32, phase.sin() as f32));
        }
        out
    }

    #[test]
    fn the_dibit_map_round_trips_through_the_slicer() {
        for d in 0..4u8 {
            assert_eq!(slice_dibit(dibit_level(d)), d, "dibit {d}");
        }
    }

    #[test]
    fn a_known_sync_fits_scale_and_offset() {
        let mut ideal = [0.0f32; ISCH_DIBITS];
        for (j, slot) in ideal.iter_mut().enumerate() {
            let d = ((super::isch::S_ISCH >> (38 - 2 * j)) & 0b11) as u8;
            *slot = dibit_level(d);
        }
        let samples: Vec<f32> = ideal.iter().map(|l| l * 0.8 + 400.0).collect();
        let (amp, off) = fit_symbols(&samples, &ideal);
        assert!(
            (amp - 0.8 * DEV_OUTER_HZ).abs() < 50.0,
            "fitted amp {amp:.0}, expected {}",
            0.8 * DEV_OUTER_HZ
        );
        assert!((off - 400.0).abs() < 20.0, "fitted offset {off:.0}");
    }

    #[test]
    fn a_scaled_slicer_recovers_inner_symbols_a_fixed_threshold_would_lose() {
        // +900 Hz of residual walks the +750 inner symbol across the
        // unscaled midpoint at 1500 Hz; scaling the decision from a fit
        // has to put it back.
        let hz = DEV_INNER_HZ + 900.0;
        assert_ne!(slice_dibit(hz), 0b00, "fixed slicer should misread this");
        let (amp, off) = (DEV_OUTER_HZ, 900.0);
        assert_eq!(slice_at(hz - off, amp), 0b00);
    }

    #[test]
    fn a_clean_signal_reports_healthy_decision_snr() {
        let dibits: Vec<u8> = (0..2048).map(|i| (i & 3) as u8).collect();
        let iq = modulate(&dibits, CHANNEL_RATE);
        let mut rx = Phase2Receiver::new(CHANNEL_RATE);
        let mid = iq.len() / 2;
        let _ = rx.process(&iq[..mid]);
        let _ = rx.process(&iq[mid..]);
        assert!(
            rx.quality_db() > 8.0,
            "clean symbols should sit well above the S-meter floor, got {}",
            rx.quality_db()
        );
    }

    #[test]
    fn the_front_end_recovers_modulated_dibits() {
        let dibits = [0b01, 0b00, 0b10, 0b11, 0b01, 0b11, 0b00, 0b10];
        let iq = modulate(&dibits, CHANNEL_RATE);
        let mut front = Phase2FrontEnd::new(CHANNEL_RATE);
        let mut hz = Vec::new();
        front.process(&iq, &mut hz);
        // Each held symbol is SPS samples; the discriminator gives one frequency
        // estimate per sample, and the boxcar output at index k*SPS is the mean
        // over exactly the samples of symbol k. The first symbol is swallowed by
        // the differential warm-up (the initial sample has no predecessor).
        let mut got = Vec::new();
        for k in 1..dibits.len() {
            let idx = k * SPS;
            if idx < hz.len() {
                got.push(slice_dibit(hz[idx]));
            }
        }
        assert_eq!(got, dibits[1..].to_vec());
    }

    #[test]
    fn the_front_end_tracks_a_live_sized_carrier_offset() {
        let mut dibits = Vec::new();
        for i in 0..4096 {
            // Balanced deterministic dibits keep their mean exactly zero.
            dibits.push((i & 3) as u8);
        }
        let mut iq = modulate(&dibits, CHANNEL_RATE);
        let mut phase = 0.0f32;
        let step = TAU * 2_000.0 / CHANNEL_RATE as f32;
        for x in &mut iq {
            phase += step;
            *x *= Complex32::new(phase.cos(), phase.sin());
        }
        let mut front = Phase2FrontEnd::new(CHANNEL_RATE);
        let mut hz = Vec::new();
        front.process(&iq, &mut hz);
        assert!((front.offset_hz() - 2_000.0).abs() < 150.0);
        let tail = &hz[hz.len() - 1024..];
        assert!(tail.iter().sum::<f32>().abs() / (tail.len() as f32) < 100.0);
    }

    #[test]
    fn superframe_geometry_is_consistent() {
        assert_eq!(PAYLOAD_DIBITS, 160);
        assert_eq!(SUPERFRAME_DIBITS, 2160);
        assert_eq!(CHANNEL_RATE, 48_000.0);
    }

    #[test]
    fn voice_burst_ess_supports_encrypted_late_entry() {
        let mut information = [0u8; 16];
        information[0] = 0x21; // ALGID 0x84 across symbols 0..1
        information[1] = 0x01;
        information[2] = 0x08;
        information[3] = 0x34; // KID 0x1234 across symbols 1..3
        for (i, symbol) in information[4..].iter_mut().enumerate() {
            *symbol = ((i * 13 + 7) & 0x3f) as u8;
        }
        let mut encoded = super::super::rs64::encode_for_test(&information, 28);
        // Exercise the RS(44,16,29) path rather than merely parsing clean ESS.
        for i in [2usize, 19, 37] {
            encoded[i] ^= 0x15;
        }

        let seed = (0xBEE07, 0x2AB, 0x3A0);
        let scrambler = Scrambler::new(seed.0, seed.1, seed.2);
        let mut decoder = Phase2Decoder::new(seed.0, seed.1, seed.2);
        let mut final_correction = None;
        for burst_no in 0..5 {
            let mut clear = [0u8; PAYLOAD_DIBITS];
            let mut put = |at: usize, value: u8| {
                clear[at] = (value >> 4) & 3;
                clear[at + 1] = (value >> 2) & 3;
                clear[at + 2] = value & 3;
            };
            if burst_no < 4 {
                for i in 0..4 {
                    put(74 + i * 3, encoded[burst_no * 4 + i]);
                }
            } else {
                let mut at = 74;
                for i in 0..28 {
                    put(at, encoded[16 + i]);
                    at += if i == 15 { 4 } else { 3 };
                }
            }
            let mut raw = clear;
            for (i, dibit) in raw.iter_mut().enumerate() {
                *dibit = scrambler.apply(10 + i, *dibit);
            }
            let burst = Burst {
                superframe_slot: 0,
                isch: Isch::Sync,
                payload: raw,
            };
            final_correction = decoder.update_ess(
                &burst,
                if burst_no < 4 {
                    BurstType::Voice4
                } else {
                    BurstType::Voice2
                },
            );
        }
        assert_eq!(final_correction, Some(3));
        assert_eq!(decoder.algorithm_id, Some(0x84));
        assert_eq!(decoder.key_id, Some(0x1234));
        assert_eq!(decoder.encrypted, Some(true));
        assert!(decoder.message_indicator.is_some());
    }
}
