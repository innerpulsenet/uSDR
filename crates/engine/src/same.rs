//! NOAA SAME headers (ZCZC-…-NNNN) after NBFM.
//!
//! 520.83 baud AFSK: mark 2083.3 Hz = 1, space 1562.5 Hz = 0, 8N1 LSB first.

use crate::afsk::ToneSlicer;

const MARK: f32 = 2083.3;
const SPACE: f32 = 1562.5;
const BAUD: f32 = 520.83;

pub struct SameDecoder {
    slicer: ToneSlicer,
    idle_ones: u8,
    in_byte: bool,
    bits: u8,
    byte: u8,
    window: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SameHeader {
    pub raw: String,
}

impl SameDecoder {
    pub fn new(fs: f64) -> Self {
        Self {
            slicer: ToneSlicer::new(fs, MARK, SPACE, BAUD),
            idle_ones: 0,
            in_byte: false,
            bits: 0,
            byte: 0,
            window: String::new(),
        }
    }

    pub fn reset(&mut self) {
        self.slicer.reset();
        self.idle_ones = 0;
        self.in_byte = false;
        self.bits = 0;
        self.byte = 0;
        self.window.clear();
    }

    pub fn process(&mut self, audio: &[f32]) -> Vec<SameHeader> {
        let mut out = Vec::new();
        for &x in audio {
            let Some(mark) = self.slicer.push(x) else {
                continue;
            };
            if let Some(h) = self.push_bit(mark) {
                out.push(h);
            }
        }
        out
    }

    fn push_bit(&mut self, mark: bool) -> Option<SameHeader> {
        if !self.in_byte {
            if mark {
                self.idle_ones = self.idle_ones.saturating_add(1);
                return None;
            }
            // start bit (space)
            self.in_byte = true;
            self.bits = 0;
            self.byte = 0;
            self.idle_ones = 0;
            return None;
        }
        if self.bits < 8 {
            if mark {
                self.byte |= 1 << self.bits;
            }
            self.bits += 1;
            return None;
        }
        // stop bit
        self.in_byte = false;
        if !mark {
            return None;
        }
        let b = self.byte;
        if (0x20..=0x7E).contains(&b) {
            self.window.push(b as char);
            if self.window.len() > 180 {
                self.window.drain(..self.window.len() - 140);
            }
            return self.take_header();
        }
        None
    }

    fn take_header(&mut self) -> Option<SameHeader> {
        let i = self.window.find("ZCZC-")?;
        let rest = &self.window[i..];
        let end = rest.find("NNNN")?;
        if end < 16 {
            return None;
        }
        let raw = rest[..=end + 3].to_string();
        self.window.clear();
        Some(SameHeader { raw })
    }
}

#[cfg(test)]
fn bytes_to_uart_bits(data: &[u8]) -> Vec<bool> {
    let mut bits = Vec::new();
    for _ in 0..16 {
        bits.push(true); // idle mark
    }
    for &b in data {
        bits.push(false); // start
        for i in 0..8 {
            bits.push((b >> i) & 1 == 1);
        }
        bits.push(true); // stop
    }
    for _ in 0..8 {
        bits.push(true);
    }
    bits
}

#[cfg(test)]
fn bits_to_audio(bits: &[bool], fs: f32) -> Vec<f32> {
    crate::afsk::marks_to_audio(bits, fs, BAUD, MARK, SPACE, 2500.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_header_from_ascii_window() {
        let mut d = SameDecoder::new(16_000.0);
        d.window = "xxxxZCZC-WXR-TOR-090001+0030-1231200-KOKX-NNNN".into();
        let h = d.take_header().unwrap();
        assert!(h.raw.starts_with("ZCZC-WXR-TOR"));
        assert!(h.raw.ends_with("NNNN"));
    }

    #[test]
    fn demodulates_a_synthesized_header() {
        let msg = b"ZCZC-WXR-TOR-090001+0030-1231200-KOKX-NNNN";
        let mut framed = vec![0xABu8; 16];
        framed.extend_from_slice(msg);
        let audio = bits_to_audio(&bytes_to_uart_bits(&framed), 16_000.0);
        let mut d = SameDecoder::new(16_000.0);
        let hits = d.process(&audio);
        assert!(
            hits.iter()
                .any(|h| h.raw.contains("ZCZC-WXR-TOR") && h.raw.ends_with("NNNN")),
            "got {hits:?}"
        );
    }
}
