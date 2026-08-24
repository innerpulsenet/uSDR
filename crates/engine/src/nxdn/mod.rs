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

pub use channel::{NxdnChannelReceiver, NxdnSpec};
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
    prev: Complex32,
    hz_per_rad: f32,
    dc: OnePole,
    raw: Vec<f32>,
    consumed: u64,
    next_abs: u64,
    last_offset_hz: f32,
    candidate: Option<(u64, Frame)>,
    locked: bool,
    expected_abs: u64,
}

impl NxdnReceiver {
    pub fn new(rate: Rate, fs: f64) -> Self {
        Self {
            rate,
            prev: Complex32::new(0.0, 0.0),
            hz_per_rad: (fs / TAU as f64) as f32,
            dc: OnePole::new((fs * 0.05) as f32),
            raw: Vec::new(),
            consumed: 0,
            next_abs: 0,
            last_offset_hz: 0.0,
            candidate: None,
            locked: false,
            expected_abs: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new(self.rate, CHANNEL_RATE);
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
            self.raw.push(hz - self.dc.process(hz));
        }
        let sps = self.rate.samples_per_symbol();
        let needed = FRAME_SYMBOLS * sps;
        let mut out = Vec::new();
        let mut start = 0usize;
        while start + needed <= self.raw.len() {
            if self.consumed + (start as u64) < self.next_abs {
                start += 1;
                continue;
            }
            let sync: Vec<f32> = (0..FSW_SYMBOLS)
                .map(|n| self.raw[start + n * sps])
                .collect();
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
                let probe: Vec<f32> = (0..FSW_SYMBOLS)
                    .map(|n| self.raw[start + phase + n * sps])
                    .collect();
                let (c, a, o) = fit_sync(&probe);
                if c > best.0 {
                    best = (c, a, o, start + phase);
                }
            }
            let (correlation, scale, offset, aligned) = best;
            if aligned + needed > self.raw.len() {
                break;
            }
            let hard_errors = sync_hard_errors(&self.raw, aligned, sps, scale, offset);
            if correlation < 0.90 || hard_errors > 2 {
                start += 1;
                continue;
            }
            let mut dibits: Vec<u8> = (FSW_SYMBOLS..FRAME_SYMBOLS)
                .map(|n| slice((self.raw[aligned + n * sps] - offset) / scale))
                .collect();
            pn95(&mut dibits, 0xe4);
            if let Some(frame) = parse_frame(self.rate, dibits, correlation, scale.abs(), offset) {
                let abs = self.consumed + aligned as u64;
                self.last_offset_hz = offset;
                self.next_abs = self.consumed + (aligned + needed - sps) as u64;
                let tolerance = (sps * 3) as u64;
                if self.locked && abs.abs_diff(self.expected_abs) <= tolerance {
                    self.expected_abs = abs + needed as u64;
                    out.push(frame);
                } else if let Some((candidate_abs, candidate)) = self.candidate.take() {
                    if abs.abs_diff(candidate_abs + needed as u64) <= tolerance {
                        self.locked = true;
                        self.expected_abs = abs + needed as u64;
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
                start = aligned + needed - sps;
            } else {
                start += 1;
            }
        }
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

fn sync_hard_errors(samples: &[f32], start: usize, sps: usize, scale: f32, offset: f32) -> u32 {
    let received = (0..FSW_SYMBOLS).fold(0u32, |value, n| {
        (value << 2) | u32::from(slice((samples[start + n * sps] - offset) / scale))
    });
    (received ^ 0xC_DF_59).count_ones()
}

fn slice(normalized: f32) -> u8 {
    if normalized > 0.66 {
        0b01
    } else if normalized > 0.0 {
        0b00
    } else if normalized > -0.66 {
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

    #[test]
    fn receiver_requires_and_acquires_consecutive_valid_frames() {
        let lich = 0x57u8;
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
        let iq = dibits
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
            .collect::<Vec<_>>();

        let mut receiver = NxdnReceiver::new(Rate::Nxdn96, CHANNEL_RATE);
        let mut frames = Vec::new();
        for block in iq.chunks(384) {
            frames.extend(receiver.process(block));
        }
        assert!(frames.len() >= 2, "decoded {} frames", frames.len());
        assert!(frames.iter().all(|frame| frame.lich == lich));
    }
}
