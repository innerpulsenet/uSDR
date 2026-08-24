//! Logic Trunked Radio (LTR Standard) sub-audible signalling.
//!
//! A Standard LTR word is 40 bits at 300 bit/s: nine sync bits, 24 data
//! bits (area, channel-in-use, home repeater, group and free channel), and
//! a seven-bit check sequence.  The decoder deliberately requires a valid
//! CRC before calling a word LTR.  Repeating sync-shaped words with a bad
//! CRC are returned as diagnostics; these are useful when investigating
//! proprietary LTR-family protocols such as Passport without mislabelling
//! them as decoded Standard LTR traffic.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const BAUD: f64 = 300.0;
const WORD_BITS: usize = 40;
const SYNC: u16 = 0b1_0101_1000;
const SYNC_INV: u16 = 0b0_1010_0111;
const CRC_CONTRIBUTIONS: [u8; 24] = [
    0x38, 0x1c, 0x0e, 0x46, 0x23, 0x51, 0x68, 0x75, 0x7a, 0x3d, 0x1f, 0x4f, 0x26, 0x52, 0x29, 0x15,
    0x0b, 0x45, 0x62, 0x31, 0x19, 0x0d, 0x07, 0x43,
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LtrWord {
    pub area: u8,
    pub channel: u8,
    pub home: u8,
    pub group: u8,
    pub free: u8,
    pub inbound: bool,
    pub crc_ok: bool,
    pub raw: String,
}

pub struct LtrDecoder {
    fs: f64,
    samples: Vec<f32>,
    since_decode: usize,
}

impl LtrDecoder {
    pub fn new(fs: f64) -> Self {
        Self {
            fs,
            samples: Vec::with_capacity(fs as usize),
            since_decode: 0,
        }
    }

    pub fn reset(&mut self) {
        self.samples.clear();
        self.since_decode = 0;
    }

    /// Consume the already low-passed sub-audible output of `NbfmDemod`.
    pub fn process(&mut self, subaudible: &[f32]) -> Vec<LtrWord> {
        self.samples.extend_from_slice(subaudible);
        self.since_decode += subaudible.len();
        // A non-overlapping window prevents the same accidental sync collision
        // from being counted repeatedly on successive calls. 450 ms contains
        // more than three complete LTR words.
        let window = (self.fs * 0.45) as usize;
        if self.since_decode < window {
            return Vec::new();
        }
        self.since_decode = 0;
        let words = decode_window(&self.samples, self.fs);
        self.samples.clear();
        words
    }
}

fn decode_window(samples: &[f32], fs: f64) -> Vec<LtrWord> {
    let sps = fs / BAUD;
    if sps < 4.0 || samples.len() < (sps * WORD_BITS as f64) as usize {
        return Vec::new();
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f32::total_cmp);
    let threshold = sorted[sorted.len() / 2];
    let mut best_valid = Vec::new();
    let mut best_diagnostics = Vec::new();
    for phase in 0..12 {
        let mut bits = Vec::with_capacity((samples.len() as f64 / sps) as usize + 1);
        let mut t = phase as f64 * sps / 12.0;
        while t < samples.len() as f64 {
            bits.push(samples[t as usize] > threshold);
            t += sps;
        }
        let mut valid = Vec::new();
        let mut diagnostics = Vec::new();
        for start in 0..=bits.len().saturating_sub(WORD_BITS) {
            let sync = pack(&bits[start..start + 9]);
            if sync != SYNC && sync != SYNC_INV {
                continue;
            }
            let inverted = sync == SYNC_INV;
            let word: Vec<bool> = bits[start..start + WORD_BITS]
                .iter()
                .map(|&b| if inverted { !b } else { b })
                .collect();
            let channel = pack(&word[10..15]) as u8;
            let home = pack(&word[15..20]) as u8;
            let group = pack(&word[20..28]) as u8;
            let free = pack(&word[28..33]) as u8;
            if !((1..=20).contains(&channel) || channel == 31)
                || !(1..=20).contains(&home)
                || !((1..=20).contains(&free) || free == 31)
            {
                continue;
            }
            let crc = pack(&word[33..40]) as u8;
            let calc = crc7(&word[9..33]);
            let inbound = crc == (calc ^ 0x7f);
            let crc_ok = crc == calc || inbound;
            let raw_value = word.iter().fold(0u64, |v, &b| (v << 1) | u64::from(b));
            let decoded = LtrWord {
                area: u8::from(word[9]),
                channel,
                home,
                group,
                free,
                inbound,
                crc_ok,
                raw: format!("{raw_value:010X}"),
            };
            if crc_ok {
                valid.push(decoded);
            } else {
                diagnostics.push(decoded);
            }
        }
        if (valid.len(), diagnostics.len()) > (best_valid.len(), best_diagnostics.len()) {
            best_valid = valid;
            best_diagnostics = diagnostics;
        }
    }
    // LTR status is continuous. Requiring the same word at least twice turns
    // the short CRC-7 into a much stronger discriminator against unrelated
    // digital traffic. Proprietary-family diagnostics require three repeats.
    let repeated = |words: Vec<LtrWord>, minimum: usize| {
        let mut counts = HashMap::<String, (usize, LtrWord)>::new();
        for word in words {
            let entry = counts.entry(word.raw.clone()).or_insert((0, word));
            entry.0 += 1;
        }
        counts
            .into_values()
            .filter_map(|(n, word)| (n >= minimum).then_some(word))
            .collect::<Vec<_>>()
    };
    let valid = repeated(best_valid, 2);
    if valid.is_empty() {
        let mut diagnostics = repeated(best_diagnostics, 3);
        diagnostics.truncate(3);
        diagnostics
    } else {
        valid
    }
}

fn crc7(bits: &[bool]) -> u8 {
    bits.iter().zip(CRC_CONTRIBUTIONS).fold(
        0u8,
        |crc, (&bit, contribution)| {
            if bit { crc ^ contribution } else { crc }
        },
    ) & 0x7f
}

fn pack(bits: &[bool]) -> u16 {
    bits.iter().fold(0u16, |v, &b| (v << 1) | u16::from(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_qualified_word_decodes() {
        let payload: [bool; 24] = std::array::from_fn(|i| {
            let value: u32 = (1 << 23) | (7 << 18) | (4 << 13) | (42 << 5) | 9;
            value & (1 << (23 - i)) != 0
        });
        let mut bits = Vec::new();
        for i in (0..9).rev() {
            bits.push(SYNC & (1 << i) != 0);
        }
        for _ in 0..4 {
            bits.extend(payload);
            let crc = crc7(&payload);
            for i in (0..7).rev() {
                bits.push(crc & (1 << i) != 0);
            }
            if bits.len() < 4 * WORD_BITS {
                for i in (0..9).rev() {
                    bits.push(SYNC & (1 << i) != 0);
                }
            }
        }
        let sps = 160usize;
        let samples: Vec<f32> = bits
            .iter()
            .flat_map(|&b| std::iter::repeat_n(if b { 1.0 } else { -1.0 }, sps))
            .collect();
        let words = decode_window(&samples, 48_000.0);
        assert!(words.iter().any(|w| w.crc_ok
            && w.channel == 7
            && w.home == 4
            && w.group == 42
            && w.free == 9));
    }
}
