//! Passport trunking outbound signalling decoder.
//!
//! Passport uses a 68-bit, 300 bit/s sub-audible word. The first nine bits
//! are the LTR-family sync pattern; the final eight bits protect the 51-bit
//! payload with CRC-7 plus parity. Only checksum-valid words are exposed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const BAUD: f64 = 300.0;
const WORD_BITS: usize = 68;
const SYNC: u16 = 0b1_0101_1000;
const CRC_CONTRIBUTIONS: [u8; 51] = [
    0x6e, 0xbf, 0xd6, 0xe3, 0xf8, 0x7c, 0x3e, 0x97, 0xc2, 0xe9, 0x75, 0x3b, 0x94, 0x4a, 0xad, 0x57,
    0xa2, 0xd9, 0x6d, 0x37, 0x92, 0xc1, 0x61, 0x31, 0x19, 0x0d, 0x07, 0x8a, 0xcd, 0x67, 0xba, 0xd5,
    0x6b, 0xbc, 0x5e, 0xa7, 0xda, 0xe5, 0x73, 0xb0, 0x58, 0x2c, 0x16, 0x83, 0xc8, 0x64, 0x32, 0x91,
    0x49, 0x25, 0x13,
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PassportWord {
    pub color_code: u8,
    pub channel: u16,
    pub site: u8,
    pub group: u16,
    pub radio_id: u32,
    pub message_type: u8,
    pub message_name: String,
    pub free: u16,
    pub raw: String,
}

pub struct PassportDecoder {
    fs: f64,
    samples: Vec<f32>,
    since_decode: usize,
}

impl PassportDecoder {
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

    /// Consume the low-passed sub-audible output of `NbfmDemod`.
    pub fn process(&mut self, subaudible: &[f32]) -> Vec<PassportWord> {
        self.samples.extend_from_slice(subaudible);
        self.since_decode += subaudible.len();
        // 550 ms spans more than two complete Passport words.
        if self.since_decode < (self.fs * 0.55) as usize {
            return Vec::new();
        }
        self.since_decode = 0;
        let words = decode_window(&self.samples, self.fs);
        self.samples.clear();
        words
    }
}

fn decode_window(samples: &[f32], fs: f64) -> Vec<PassportWord> {
    let sps = fs / BAUD;
    if sps < 4.0 || samples.len() < (sps * WORD_BITS as f64) as usize {
        return Vec::new();
    }

    let mut sorted = samples.to_vec();
    sorted.sort_by(f32::total_cmp);
    let threshold = sorted[sorted.len() / 2];
    let mut decoded = HashMap::<String, PassportWord>::new();

    for phase in 0..12 {
        let mut bits = Vec::with_capacity((samples.len() as f64 / sps) as usize + 1);
        let mut t = phase as f64 * sps / 12.0;
        while t < samples.len() as f64 {
            bits.push(samples[t as usize] > threshold);
            t += sps;
        }

        for start in 0..=bits.len().saturating_sub(WORD_BITS) {
            let word = &bits[start..start + WORD_BITS];
            if pack_u16(&word[..9]) != SYNC
                || checksum(&word[9..60]) != pack_u16(&word[60..68]) as u8
            {
                continue;
            }
            let raw_value = word.iter().fold(0u128, |v, &b| (v << 1) | u128::from(b));
            let channel = pack_u16(&word[11..22]);
            let free = pack_u16(&word[49..60]);
            let message_type = pack_u16(&word[45..49]) as u8;
            let message_name = message_name(message_type, channel, free).to_owned();
            let raw = format!("{raw_value:017X}");
            decoded.entry(raw.clone()).or_insert(PassportWord {
                color_code: pack_u16(&word[9..11]) as u8,
                channel,
                site: pack_u16(&word[22..29]) as u8,
                group: pack_u16(&word[29..45]),
                radio_id: pack_u32(&word[22..45]),
                message_type,
                message_name,
                free,
                raw,
            });
        }
    }

    decoded.into_values().collect()
}

fn message_name(message_type: u8, channel: u16, free: u16) -> &'static str {
    match message_type {
        0 | 2 => "group call start",
        1 if free == 2042 => "talkgroup assignment",
        1 if channel < 1792 => "call start",
        1 if channel == 1792 || channel == 1793 => "system idle",
        1 if channel == 2047 => "call end",
        5 => "call page",
        6 => "radio ID",
        9 => "data call start",
        11 => "radio registration",
        _ => "unknown",
    }
}

fn checksum(bits: &[bool]) -> u8 {
    bits.iter().zip(CRC_CONTRIBUTIONS).fold(
        0u8,
        |sum, (&bit, contribution)| {
            if bit { sum ^ contribution } else { sum }
        },
    )
}

fn pack_u16(bits: &[bool]) -> u16 {
    bits.iter().fold(0u16, |v, &b| (v << 1) | u16::from(b))
}

fn pack_u32(bits: &[bool]) -> u32 {
    bits.iter().fold(0u32, |v, &b| (v << 1) | u32::from(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_qualified_group_call_decodes() {
        let mut word = vec![false; WORD_BITS];
        set(&mut word, 0, 9, SYNC as u32);
        set(&mut word, 9, 11, 2);
        set(&mut word, 11, 22, 417);
        set(&mut word, 22, 29, 23);
        set(&mut word, 29, 45, 12_345);
        set(&mut word, 45, 49, 0);
        set(&mut word, 49, 60, 42);
        let crc = checksum(&word[9..60]);
        set(&mut word, 60, 68, crc as u32);

        let bits: Vec<bool> = std::iter::repeat_n(word, 3).flatten().collect();
        let samples: Vec<f32> = bits
            .iter()
            .flat_map(|&bit| std::iter::repeat_n(if bit { 1.0 } else { -1.0 }, 160))
            .collect();
        let words = decode_window(&samples, 48_000.0);
        assert!(words.iter().any(|decoded| {
            decoded.color_code == 2
                && decoded.channel == 417
                && decoded.site == 23
                && decoded.group == 12_345
                && decoded.free == 42
                && decoded.message_name == "group call start"
        }));
    }

    fn set(bits: &mut [bool], start: usize, end: usize, value: u32) {
        for (offset, bit) in bits[start..end].iter_mut().enumerate() {
            *bit = value & (1 << (end - start - 1 - offset)) != 0;
        }
    }
}
