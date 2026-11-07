//! NXDN 6.25/12.5 kHz framing (NXDN48 and NXDN96).
//!
//! Both rates carry the same 192-symbol frame.  The narrow form runs it at
//! 2400 symbols/s and the wide form at 4800 symbols/s.  A valid frame requires
//! the 20-bit FSW, PN95 descrambling, a divided LICH with valid parity, and a
//! defined functional-channel combination; that stack is deliberately much
//! harder for generic 4-FSK noise to imitate than a baud-rate guess.

pub mod channel;
pub mod control;

use crate::dmr::voice::{self, VoiceFrame};
use num_complex::Complex32;
use scannerd_dsp::OnePole;
use std::f32::consts::TAU;

pub use channel::{NxdnChannelReceiver, NxdnGrant, NxdnSpec};
pub use control::{ControlMessage, decode_control};

pub const CHANNEL_RATE: f64 = 48_000.0;
pub const NXDN_BANDWIDTH_HZ: f32 = 12_500.0;
pub const FRAME_SYMBOLS: usize = 192;
pub const FSW_SYMBOLS: usize = 10;
/// NXDN frame sync 0xCDF59 as ideal C4FM levels. Keeping the inner/outer
/// magnitudes is essential: sign-only correlation also matches ordinary
/// binary FSK and Bell 202 surprisingly often.
const FSW_LEVELS: [f32; FSW_SYMBOLS] = [-3.0, 1.0, -3.0, 3.0, -3.0, -3.0, 3.0, 3.0, -1.0, 3.0];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rate {
    Nxdn48,
    Nxdn96,
}

impl Rate {
    pub const fn symbol_rate(self) -> f64 {
        match self {
            Self::Nxdn48 => 2400.0,
            Self::Nxdn96 => 4800.0,
        }
    }

    pub const fn samples_per_symbol(self) -> usize {
        match self {
            Self::Nxdn48 => 20,
            Self::Nxdn96 => 10,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Nxdn48 => "NXDN48",
            Self::Nxdn96 => "NXDN96",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum System {
    ConventionalOrTypeC,
    TypeD,
}

impl System {
    pub const fn label(self) -> &'static str {
        match self {
            Self::ConventionalOrTypeC => "NXDN Conventional / Type-C",
            Self::TypeD => "NXDN Type-D / IDAS",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub rate: Rate,
    /// Seven-bit LICH after divided-bit reconstruction and parity removal.
    pub lich: u8,
    pub system: System,
    pub rf_channel: u8,
    pub functional_channel: u8,
    pub option: u8,
    pub outbound: bool,
    pub voice_blocks: Vec<VoiceFrame>,
    /// Full descrambled post-FSW dibit payload (LICH starts at zero).
    pub payload: Vec<u8>,
    pub correlation: f32,
    pub deviation_hz: f32,
    pub offset_hz: f32,
}

impl Frame {
    pub fn is_control(&self) -> bool {
        self.rf_channel == 0 || matches!(self.lich, 0x60..=0x6f)
    }

    pub fn has_voice(&self) -> bool {
        !self.voice_blocks.is_empty()
    }

    pub fn kind(&self) -> &'static str {
        if self.has_voice() {
            "Voice"
        } else if self.is_control() {
            "Control"
        } else {
            "Data"
        }
    }
}

pub struct NxdnReceiver {
    rate: Rate,
    /// Input sample rate the receiver was built for. The analog chain
    /// delivers its true rate — ppm error and all — not the nominal channel
    /// rate, so `reset` must rebuild with this rather than `CHANNEL_RATE`.
    fs: f64,
    prev: Complex32,
    hz_per_rad: f32,
    dc: OnePole,
    /// Samples per symbol, `fs / symbol_rate` — not necessarily whole.
    ///
    /// The 2.048 MS/s span decimates to 47 627.9 Hz, 9.92 samples per
    /// NXDN96 symbol and 19.85 per NXDN48 one. A whole-sample stride slipped
    /// 1.5 symbols across a 192-symbol frame, so the LICH near the sync read
    /// and the voice blocks at the far end did not. Symbols are read at
    /// `start + n · spb` with linear interpolation, as the DMR framer does.
    spb: f64,
    /// One-symbol integrate-and-dump ahead of the slicer (running sum over
    /// `box_buf`). The discriminator's noise rises with frequency across
    /// the whole channel filter, and the symbol occupies only the bottom
    /// 1.2–2.4 kHz of it: point-sampling one discriminator sample a symbol
    /// sliced all of that noise, while a boxcar the length of the symbol
    /// is the matched filter for its near-rectangular frequency pulse — the
    /// same front end DMR and P25 run.
    box_buf: Vec<f32>,
    box_head: usize,
    box_sum: f32,
    raw: Vec<f32>,
    consumed: u64,
    next_abs: u64,
    /// Absolute sample index up to which the sync scan has already judged
    /// every position.
    ///
    /// The retained buffer overlaps between blocks, so without this cursor
    /// each block re-correlated the whole retained tail against the few
    /// hundred samples that were new. A position's fit cannot change once its
    /// window has arrived, so a rejected position is final — a found frame
    /// steps `next_abs` past itself, which this is maxed with.
    scanned_abs: u64,
    last_offset_hz: f32,
    candidate: Option<(u64, Frame)>,
    locked: bool,
    expected_abs: u64,
}

impl NxdnReceiver {
    pub fn new(rate: Rate, fs: f64) -> Self {
        let spb = fs / rate.symbol_rate();
        let boxcar = spb.round().max(1.0) as usize;
        Self {
            rate,
            fs,
            prev: Complex32::new(0.0, 0.0),
            hz_per_rad: (fs / TAU as f64) as f32,
            dc: OnePole::new((fs * 0.05) as f32),
            spb,
            box_buf: vec![0.0; boxcar],
            box_head: 0,
            box_sum: 0.0,
            raw: Vec::new(),
            consumed: 0,
            next_abs: 0,
            scanned_abs: 0,
            last_offset_hz: 0.0,
            candidate: None,
            locked: false,
            expected_abs: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new(self.rate, self.fs);
    }

    pub fn offset_hz(&self) -> f32 {
        self.last_offset_hz
    }

    pub fn process(&mut self, iq: &[Complex32]) -> Vec<Frame> {
        for &x in iq {
            let d = x * self.prev.conj();
            self.prev = x;
            let hz = if d.norm_sqr() > 0.0 {
                d.arg() * self.hz_per_rad
            } else {
                0.0
            };
            self.integrate(hz);
        }
        self.scan()
    }

    /// Boxcar one discriminator sample in, then strip the carrier offset.
    fn integrate(&mut self, hz: f32) {
        self.box_sum += hz - self.box_buf[self.box_head];
        self.box_buf[self.box_head] = hz;
        self.box_head = (self.box_head + 1) % self.box_buf.len();
        let acc = self.box_sum / self.box_buf.len() as f32;
        self.raw.push(acc - self.dc.process(acc));
    }

    /// Run the receiver over pre-discriminated Hz, for callers that
    /// discriminate a shared filtered buffer once for several consumers. The
    /// carrier tracker still runs here, on the discriminated samples.
    pub fn process_hz(&mut self, hz: &[f32]) -> Vec<Frame> {
        for &h in hz {
            self.integrate(h);
        }
        self.scan()
    }

    /// Correlate the FSW across the retained buffer; the shared tail of both
    /// input paths.
    fn scan(&mut self) -> Vec<Frame> {
        let spb = self.spb;
        let sps = spb.round().max(1.0) as usize;
        // Samples a frame spans, plus the one the interpolator reads past
        // its last symbol; and the frame-to-frame stride, rounded down so a
        // step never lands past the next sync (the alignment probe only
        // looks forward).
        let needed = (FRAME_SYMBOLS as f64 * spb).ceil() as usize + 1;
        let period = (FRAME_SYMBOLS as f64 * spb).floor() as usize;
        let mut out = Vec::new();
        let mut start = 0usize;
        // Resume past positions earlier blocks already judged. Computed once:
        // inside the loop `next_abs` only ever moves to a position the scan
        // has already reached or stepped over.
        let scan_from = self.next_abs.max(self.scanned_abs);
        while start + needed <= self.raw.len() {
            if self.consumed + (start as u64) < scan_from {
                start += 1;
                continue;
            }
            let mut sync = [0.0f32; FSW_SYMBOLS];
            for (n, v) in sync.iter_mut().enumerate() {
                *v = symbol_at(&self.raw, start, n, spb);
            }
            let (corr, scale, offset) = fit_sync(&sync);
            if corr < 0.82 || scale.abs() < 250.0 {
                start += 1;
                continue;
            }
            let mut best = (corr, scale, offset, start);
            for phase in 1..sps {
                if start + phase + needed > self.raw.len() {
                    break;
                }
                let mut probe = [0.0f32; FSW_SYMBOLS];
                for (n, v) in probe.iter_mut().enumerate() {
                    *v = symbol_at(&self.raw, start + phase, n, spb);
                }
                let (c, a, o) = fit_sync(&probe);
                if c > best.0 {
                    best = (c, a, o, start + phase);
                }
            }
            let (correlation, scale, offset, aligned) = best;
            if aligned + needed > self.raw.len() {
                break;
            }
            let hard_errors = sync_hard_errors(&self.raw, aligned, spb, scale, offset);
            if correlation < 0.90 || hard_errors > 2 {
                start += 1;
                continue;
            }
            let mut dibits: Vec<u8> = (FSW_SYMBOLS..FRAME_SYMBOLS)
                .map(|n| slice((symbol_at(&self.raw, aligned, n, spb) - offset) / scale))
                .collect();
            pn95(&mut dibits, 0xe4);
            if let Some(frame) = parse_frame(self.rate, dibits, correlation, scale.abs(), offset) {
                let abs = self.consumed + aligned as u64;
                self.last_offset_hz = offset;
                self.next_abs = self.consumed + (aligned + period - sps) as u64;
                let tolerance = (sps * 3) as u64;
                if self.locked && abs.abs_diff(self.expected_abs) <= tolerance {
                    self.expected_abs = abs + period as u64;
                    out.push(frame);
                } else if let Some((candidate_abs, candidate)) = self.candidate.take() {
                    if abs.abs_diff(candidate_abs + period as u64) <= tolerance {
                        self.locked = true;
                        self.expected_abs = abs + period as u64;
                        out.push(candidate);
                        out.push(frame);
                    } else {
                        self.locked = false;
                        self.candidate = Some((abs, frame));
                    }
                } else {
                    self.locked = false;
                    self.candidate = Some((abs, frame));
                }
                start = aligned + period - sps;
            } else {
                start += 1;
            }
        }
        // Everything below `start` has been judged — or deliberately stepped
        // over by a found frame, which `next_abs` covers as well. `start`
        // itself is the first position whose window has not fully arrived.
        self.scanned_abs = self.scanned_abs.max(self.consumed + start as u64);
        let keep = needed + sps;
        if self.raw.len() > keep {
            let drop = self.raw.len() - keep;
            self.raw.drain(..drop);
            self.consumed += drop as u64;
        }
        out
    }
}

fn fit_sync(samples: &[f32]) -> (f32, f32, f32) {
    let n = FSW_SYMBOLS as f32;
    let mean = samples.iter().sum::<f32>() / n;
    let ideal_mean = FSW_LEVELS.iter().sum::<f32>() / n;
    let mut cov = 0.0;
    let mut xx = 0.0;
    let mut yy = 0.0;
    for (&sample, &ideal) in samples.iter().zip(&FSW_LEVELS) {
        cov += (sample - mean) * (ideal - ideal_mean);
        xx += (ideal - ideal_mean).powi(2);
        yy += (sample - mean).powi(2);
    }
    let scale = cov / xx.max(1e-6);
    let offset = mean - scale * ideal_mean;
    let corr = cov.abs() / (xx * yy).sqrt().max(1e-6);
    (corr, scale, offset)
}

/// Symbol `n` of a frame starting at `start`, `spb` samples apart,
/// linearly interpolated between the two samples it falls between.
fn symbol_at(samples: &[f32], start: usize, n: usize, spb: f64) -> f32 {
    let pos = n as f64 * spb;
    let i = pos.floor();
    let frac = (pos - i) as f32;
    let at = start + i as usize;
    let a = samples[at];
    match samples.get(at + 1) {
        Some(&b) if frac > 0.0 => a + frac * (b - a),
        _ => a,
    }
}

fn sync_hard_errors(samples: &[f32], start: usize, spb: f64, scale: f32, offset: f32) -> u32 {
    let received = (0..FSW_SYMBOLS).fold(0u32, |value, n| {
        (value << 2) | u32::from(slice((symbol_at(samples, start, n, spb) - offset) / scale))
    });
    (received ^ 0xC_DF_59).count_ones()
}

/// The dibit for a sample normalized by the sync fit, whose levels are
/// ±1 (inner) and ±3 (outer): the decisions sit midway, at 0 and ±2.
fn slice(normalized: f32) -> u8 {
    if normalized > 2.0 {
        0b01
    } else if normalized > 0.0 {
        0b00
    } else if normalized > -2.0 {
        0b10
    } else {
        0b11
    }
}

fn pn95(dibits: &mut [u8], seed: u16) {
    let mut lfsr = seed;
    for dibit in dibits {
        if lfsr & 1 != 0 {
            *dibit ^= 0b10;
        }
        let bit = ((lfsr >> 4) ^ lfsr) & 1;
        lfsr = (lfsr >> 1) | (bit << 8);
    }
}

fn parse_frame(
    rate: Rate,
    payload: Vec<u8>,
    correlation: f32,
    deviation_hz: f32,
    offset_hz: f32,
) -> Option<Frame> {
    if payload.len() != FRAME_SYMBOLS - FSW_SYMBOLS {
        return None;
    }
    let mut lich_full = 0u8;
    let mut off_bits = 0u8;
    for &d in payload.iter().take(8) {
        lich_full = (lich_full << 1) | ((d >> 1) & 1);
        off_bits += d & 1;
    }
    let received = lich_full & 1;
    let parity = ((lich_full >> 7) ^ (lich_full >> 6) ^ (lich_full >> 5) ^ (lich_full >> 4)) & 1;
    if received != parity || off_bits < 6 {
        return None;
    }
    let lich = lich_full >> 1;
    let (system, voice_positions) = lich_layout(lich)?;
    let mut voice_blocks = Vec::new();
    for position in voice_positions {
        let start = 38 + position * 36;
        voice_blocks.push(voice::decode_codeword(payload.get(start..start + 36)?));
    }
    Some(Frame {
        rate,
        lich,
        system,
        rf_channel: (lich >> 5) & 3,
        functional_channel: (lich >> 3) & 3,
        option: (lich >> 1) & 3,
        outbound: lich & 1 != 0,
        voice_blocks,
        payload,
        correlation,
        deviation_hz,
        offset_hz,
    })
}

fn lich_layout(lich: u8) -> Option<(System, Vec<usize>)> {
    let conventional = System::ConventionalOrTypeC;
    let type_d = System::TypeD;
    let value = match lich {
        0x32 | 0x33 | 0x52 | 0x53 => (conventional, vec![2, 3]),
        0x34 | 0x35 | 0x54 | 0x55 => (conventional, vec![0, 1]),
        0x36 | 0x37 | 0x56 | 0x57 => (conventional, vec![0, 1, 2, 3]),
        0x20 | 0x21 | 0x28 | 0x29 | 0x2e | 0x2f | 0x30 | 0x31 | 0x38 | 0x39 | 0x40 | 0x41
        | 0x49 | 0x4e | 0x4f | 0x50 | 0x51 | 0x01 | 0x05 => (conventional, Vec::new()),
        0x72 | 0x73 => (type_d, vec![2, 3]),
        0x75 => (type_d, vec![0, 1]),
        0x76 | 0x77 => (type_d, vec![0, 1, 2, 3]),
        0x60..=0x71 => (type_d, Vec::new()),
        _ => return None,
    };
    Some(value)
}

/// Three back-to-back valid frames at the wide rate, FM-modulated onto a
/// carrier so the whole discrimination/scan stack is exercised. Crate-level
/// test helper: the channel-receiver tests use it too.
#[cfg(test)]
pub(crate) fn tests_modulated_frames(lich: u8) -> Vec<Complex32> {
    use std::f32::consts::TAU;
    let parity = ((lich >> 6) ^ (lich >> 5) ^ (lich >> 4) ^ (lich >> 3)) & 1;
    let lich_full = (lich << 1) | parity;
    let mut payload = vec![0u8; FRAME_SYMBOLS - FSW_SYMBOLS];
    for (i, dibit) in payload.iter_mut().take(8).enumerate() {
        *dibit = (((lich_full >> (7 - i)) & 1) << 1) | 1;
    }
    pn95(&mut payload, 0xe4);

    let fsw = (0..FSW_SYMBOLS)
        .map(|i| ((0xC_DF_59 >> (18 - i * 2)) & 3) as u8)
        .collect::<Vec<_>>();
    let dibits = std::iter::repeat_n(fsw.into_iter().chain(payload).collect::<Vec<_>>(), 3)
        .flatten()
        .collect::<Vec<_>>();
    let mut phase = 0.0f32;
    dibits
        .iter()
        .flat_map(|&dibit| {
            let level = match dibit {
                0 => 1.0,
                1 => 3.0,
                2 => -1.0,
                _ => -3.0,
            };
            std::iter::repeat_n(level, Rate::Nxdn96.samples_per_symbol())
        })
        .map(|level| {
            phase += TAU * level * 600.0 / CHANNEL_RATE as f32;
            Complex32::new(phase.cos(), phase.sin())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    #[test]
    fn pn95_is_its_own_inverse() {
        let original: Vec<u8> = (0..182).map(|n| (n & 3) as u8).collect();
        let mut value = original.clone();
        pn95(&mut value, 0xe4);
        pn95(&mut value, 0xe4);
        assert_eq!(value, original);
    }

    #[test]
    fn known_lich_values_name_voice_and_trunk_families() {
        assert_eq!(lich_layout(0x36).unwrap().1.len(), 4);
        assert_eq!(lich_layout(0x77).unwrap().0, System::TypeD);
        assert!(lich_layout(0x7f).is_none());
    }

    /// Three back-to-back valid frames at the wide rate, FM-modulated onto a
    /// carrier so the whole discrimination/scan stack is exercised. Shared
    /// with the channel-receiver tests (same crate) as `tests_modulated_frames`.
    fn modulated_frames(lich: u8) -> Vec<Complex32> {
        super::tests_modulated_frames(lich)
    }

    #[test]
    fn receiver_requires_and_acquires_consecutive_valid_frames() {
        let lich = 0x57u8;
        let iq = modulated_frames(lich);

        let mut receiver = NxdnReceiver::new(Rate::Nxdn96, CHANNEL_RATE);
        let mut frames = Vec::new();
        for block in iq.chunks(384) {
            frames.extend(receiver.process(block));
        }
        assert!(frames.len() >= 2, "decoded {} frames", frames.len());
        assert!(frames.iter().all(|frame| frame.lich == lich));
    }

    /// The classifier discriminates the shared filtered buffer once and feeds
    /// every consumer from that pass; the pre-discriminated entry point must
    /// decode exactly what the inline-discrimination path decodes.
    #[test]
    fn process_hz_matches_process_exactly() {
        let iq = modulated_frames(0x57);
        let mut direct = NxdnReceiver::new(Rate::Nxdn96, CHANNEL_RATE);
        let mut via_hz = NxdnReceiver::new(Rate::Nxdn96, CHANNEL_RATE);
        // The same delay-line discrimination the receiver performs inline.
        let hz_per_rad = (CHANNEL_RATE / TAU as f64) as f32;
        let mut prev = Complex32::new(0.0, 0.0);
        let mut hz = Vec::new();
        let mut frames_direct = Vec::new();
        let mut frames_hz = Vec::new();
        for block in iq.chunks(384) {
            frames_direct.extend(direct.process(block));
            hz.clear();
            for &x in block {
                let d = x * prev.conj();
                prev = x;
                hz.push(if d.norm_sqr() > 0.0 {
                    d.arg() * hz_per_rad
                } else {
                    0.0
                });
            }
            frames_hz.extend(via_hz.process_hz(&hz));
        }
        assert!(frames_direct.len() >= 2, "baseline decoded nothing");
        assert_eq!(frames_direct, frames_hz);
    }

    /// Deterministic LCG so the noise fixtures are the same on every run.
    fn lcg(seed: &mut u64) -> f32 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*seed >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
    }

    /// Approximately Gaussian, unit variance, via the sum of twelve uniforms.
    fn gauss(seed: &mut u64) -> f32 {
        (0..12).map(|_| lcg(seed)).sum::<f32>() / 2.0
    }

    /// `frames` back-to-back NXDN frames with pseudorandom payload under a
    /// valid voice LICH, FM-modulated at the rate's standard deviation on a
    /// sample clock of `fs` (sample `n` carries the symbol on the air at
    /// `n / fs`). Returns the IQ and each frame's descrambled payload.
    fn on_air(rate: Rate, fs: f64, frames: usize, seed: u64) -> (Vec<Complex32>, Vec<Vec<u8>>) {
        let lich = 0x57u8;
        let parity = ((lich >> 6) ^ (lich >> 5) ^ (lich >> 4) ^ (lich >> 3)) & 1;
        let lich_full = (lich << 1) | parity;
        let fsw: Vec<u8> = (0..FSW_SYMBOLS)
            .map(|i| ((0xC_DF_59 >> (18 - i * 2)) & 3) as u8)
            .collect();
        let mut state = seed;
        let mut dibits = Vec::new();
        let mut sent = Vec::new();
        for _ in 0..frames {
            let mut payload: Vec<u8> = (0..FRAME_SYMBOLS - FSW_SYMBOLS)
                .map(|_| ((lcg(&mut state) + 1.0) * 2.0).min(3.0) as u8)
                .collect();
            for (i, dibit) in payload.iter_mut().take(8).enumerate() {
                *dibit = (((lich_full >> (7 - i)) & 1) << 1) | 1;
            }
            sent.push(payload.clone());
            pn95(&mut payload, 0xe4);
            dibits.extend(&fsw);
            dibits.extend(payload);
        }
        // Outer deviation: 1050 Hz for NXDN48, 2400 Hz for NXDN96.
        let unit = match rate {
            Rate::Nxdn48 => 350.0,
            Rate::Nxdn96 => 800.0,
        };
        let n = (dibits.len() as f64 * fs / rate.symbol_rate()) as usize;
        let mut phase = 0.0f64;
        let iq = (0..n)
            .map(|i| {
                let sym = ((i as f64 * rate.symbol_rate() / fs) as usize).min(dibits.len() - 1);
                let level = match dibits[sym] {
                    0 => 1.0,
                    1 => 3.0,
                    2 => -1.0,
                    _ => -3.0,
                };
                phase += TAU as f64 * level * unit / fs;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();
        (iq, sent)
    }

    /// Frames recovered and payload dibit errors over them, for `frames`
    /// frames at `cnr_db` carrier-to-noise in the 12.5 kHz channel, run
    /// through the same channel filter the live receiver rides.
    fn run(rate: Rate, fs: f64, frames: usize, cnr_db: f32, seed: u64) -> (usize, usize) {
        let (clean, sent) = on_air(rate, fs, frames, seed);
        let sigma = (10f32.powf(-cnr_db / 10.0) * fs as f32 / NXDN_BANDWIDTH_HZ / 2.0).sqrt();
        let mut state = seed ^ 0x9e37_79b9;
        let noisy: Vec<Complex32> = clean
            .iter()
            .map(|&x| x + Complex32::new(gauss(&mut state), gauss(&mut state)) * sigma)
            .collect();
        let mut chain = scannerd_dsp::DecodeChain::new(fs, NXDN_BANDWIDTH_HZ, CHANNEL_RATE);
        let mut rx = NxdnReceiver::new(rate, chain.fs_out());
        let mut baseband = Vec::new();
        let mut got = Vec::new();
        for block in noisy.chunks(960) {
            chain.process(block, &mut baseband);
            got.extend(rx.process(&baseband));
        }
        let errors = got
            .iter()
            .map(|frame| {
                sent.iter()
                    .map(|p| p.iter().zip(&frame.payload).filter(|(a, b)| a != b).count())
                    .min()
                    .unwrap_or(0)
            })
            .sum();
        (got.len(), errors)
    }

    /// At 10 dB CNR in the channel, at the exact 48 kHz rate and at the
    /// 47 627.9 Hz the 2.048 MS/s span delivers, both rates must find
    /// nearly every frame and read its payload at under 3 % dibit errors.
    /// Before the integrator, the fractional stride and the inner-level
    /// threshold, NXDN48 found 6/40 frames at 48 kHz and none at 47.6 kHz,
    /// and NXDN96 read its payload at a 38 % dibit error rate even when it
    /// found the frame.
    #[test]
    fn both_rates_decode_at_10_db_on_every_span_rate() {
        for rate in [Rate::Nxdn48, Rate::Nxdn96] {
            for fs in [CHANNEL_RATE, 2_048_000.0 / 43.0] {
                let (found, errors) = run(rate, fs, 40, 10.0, 7);
                let ber = errors as f32 / (found.max(1) * (FRAME_SYMBOLS - FSW_SYMBOLS)) as f32;
                assert!(found >= 36, "{} fs {fs:.1}: {found}/40 frames", rate.label());
                assert!(ber < 0.03, "{} fs {fs:.1}: dibit error rate {ber:.4}", rate.label());
                let (_, clean) = run(rate, fs, 6, 30.0, 3);
                assert_eq!(clean, 0, "{} fs {fs:.1}: errors at 30 dB", rate.label());
            }
        }
    }

    /// The slicer's decision points sit midway between the fitted levels:
    /// ±2 in units of the inner deviation. At ±0.66 every inner symbol read
    /// as an outer one, so the LICH (all outer) survived while the voice
    /// and control payload behind it came out garbage.
    #[test]
    fn inner_symbols_slice_as_inner() {
        assert_eq!(slice(3.0), 0b01);
        assert_eq!(slice(1.0), 0b00);
        assert_eq!(slice(-1.0), 0b10);
        assert_eq!(slice(-3.0), 0b11);
    }

    /// Measurement sweep, not a pass/fail check: `cargo test -p
    /// scannerd-engine --lib nxdn_sensitivity_sweep -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn nxdn_sensitivity_sweep() {
        for rate in [Rate::Nxdn48, Rate::Nxdn96] {
            for fs in [CHANNEL_RATE, 2_048_000.0 / 43.0] {
                for cnr in [6.0, 8.0, 10.0, 12.0, 14.0, 18.0, 30.0] {
                    let (found, errors) = run(rate, fs, 40, cnr, 7);
                    let ber = errors as f32 / (found.max(1) * (FRAME_SYMBOLS - FSW_SYMBOLS)) as f32;
                    eprintln!(
                        "{} fs {fs:.1} CNR {cnr:>4.1} dB: {found:>2}/40 frames, dibit error rate {ber:.4}",
                        rate.label()
                    );
                }
            }
        }
    }
}
