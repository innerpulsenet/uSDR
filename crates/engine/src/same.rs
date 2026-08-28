//! NOAA SAME headers (ZCZC-…-NNNN) after NBFM.
//!
//! 520.83 baud AFSK: mark 2083.3 Hz = 1, space 1562.5 Hz = 0, 8N1 LSB first.
//!
//! SAME transmits every alert message **three times** precisely so that
//! bit errors can be voted out: two byte-identical copies almost never fail
//! identically. Like multimon-ng (MIN_IDENTICAL_MSGS), a header is published
//! only when a second identical copy arrives within the replay window;
//! single malformed copies are held, not emitted.

use crate::afsk::ToneSlicer;

const MARK: f32 = 2083.3;
const SPACE: f32 = 1562.5;
const BAUD: f32 = 520.83;
/// Two identical copies must arrive inside this window. A real SAME burst
/// repeats at roughly 1 s intervals; this tolerates squelch tails and channel
/// changes far beyond that without letting stale candidates accumulate.
const VOTE_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

pub struct SameDecoder {
    slicer: ToneSlicer,
    idle_ones: u8,
    in_byte: bool,
    bits: u8,
    byte: u8,
    window: String,
    /// Complete headers seen once and awaiting a second identical copy:
    /// (raw text, first-seen instant). Bounded by pruning.
    pending: Vec<(String, std::time::Instant)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SameHeader {
    /// The verbatim header text, ZCZC-…-NNNN.
    pub raw: String,
    /// Originator code, e.g. "WXR" (weather service) or "PEP" (EAS).
    pub originator: String,
    /// Three-letter event code, e.g. "TOR", "SVR", "RMT".
    pub event: String,
    /// FIPS county codes as transmitted, e.g. ["090001"].
    pub locations: Vec<String>,
}

impl SameHeader {
    fn parse(raw: &str) -> Option<Self> {
        let body = raw.strip_prefix("ZCZC-")?.strip_suffix("NNNN")?;
        let mut fields = body.split('-');
        let originator = fields.next()?.to_string();
        let event = fields.next()?.to_string();
        if originator.is_empty() || event.len() != 3 {
            return None;
        }
        // What follows the event is hyphen-separated location blocks, each
        // "PSSCCC+DDDD" (one or more counties joined by '+' with a trailing
        // duration). Everything after the block holding '+' is
        // Julian/date/station/NNNN bookkeeping, already stripped here except
        // the duration tail.
        let mut locations = Vec::new();
        'outer: for field in fields {
            // The location block is "CCC000+CCC001+DDDD": counties joined by
            // '+', ending in the bare 4-digit duration. A county code is six
            // digits; anything else ends the list.
            let mut plus = field.split('+');
            plus.next()?; // first county or the duration gap — re-checked below
            for county in std::iter::once(field.split('+').next().unwrap()).chain(plus) {
                if county.len() == 6 && county.chars().all(|c| c.is_ascii_digit()) {
                    locations.push(county.to_string());
                } else {
                    break 'outer;
                }
            }
        }
        Some(Self {
            raw: raw.to_string(),
            originator,
            event,
            locations,
        })
    }

    /// Human-readable summary: "TOR for 090001".
    pub fn summary(&self) -> String {
        if self.locations.is_empty() {
            format!("{} ({})", self.event, self.originator)
        } else {
            format!("{} for {}", self.event, self.locations.join(", "))
        }
    }
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
            pending: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.slicer.reset();
        self.idle_ones = 0;
        self.in_byte = false;
        self.bits = 0;
        self.byte = 0;
        self.window.clear();
        self.pending.clear();
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
        let now = std::time::Instant::now();
        self.pending.retain(|(_, seen)| now.duration_since(*seen) < VOTE_WINDOW);
        // Second identical copy wins — publish the corroborated text.
        if let Some(pos) = self.pending.iter().position(|(seen, _)| *seen == raw) {
            self.pending.swap_remove(pos);
            return SameHeader::parse(&raw);
        }
        // First copy: hold it and wait for corroboration. A noisy reception
        // produces three DIFFERENT texts and nothing is ever published, which
        // beats publishing one garbled alert.
        self.pending.push((raw, now));
        None
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
        assert!(d.take_header().is_none(), "a first copy must be held");
        d.window = "xxxxZCZC-WXR-TOR-090001+0030-1231200-KOKX-NNNN".into();
        let h = d.take_header().unwrap();
        assert!(h.raw.starts_with("ZCZC-WXR-TOR"));
        assert!(h.raw.ends_with("NNNN"));
    }

    #[test]
    fn parses_structured_fields() {
        let h = SameHeader::parse("ZCZC-WXR-TOR-090001+0030-1231200-KOKX-NNNN").unwrap();
        assert_eq!(h.originator, "WXR");
        assert_eq!(h.event, "TOR");
        assert_eq!(h.locations, vec!["090001"]);
        assert_eq!(h.summary(), "TOR for 090001");
        // Several counties ride joined by '+' inside the location block.
        let h = SameHeader::parse("ZCZC-WXR-SVR-036081+036085+0100-1231200-KOKX-NNNN").unwrap();
        assert_eq!(h.locations, vec!["036081", "036085"]);
    }

    /// As broadcast on air: the same header, three times over.
    #[test]
    fn demodulates_a_synthesized_triplet() {
        let msg = b"ZCZC-WXR-TOR-090001+0030-1231200-KOKX-NNNN";
        let mut framed: Vec<u8> = vec![0xAB; 16];
        framed.extend_from_slice(msg);
        for _ in 0..3 {
            framed.extend_from_slice(msg);
        }
        let audio = bits_to_audio(&bytes_to_uart_bits(&framed), 16_000.0);
        let mut d = SameDecoder::new(16_000.0);
        let hits = d.process(&audio);
        assert_eq!(hits.len(), 2, "copies 2 and 3 corroborate; got {hits:?}");
        assert!(
            hits.iter()
                .all(|h| h.raw.contains("ZCZC-WXR-TOR") && h.raw.ends_with("NNNN"))
        );
    }

    /// The point of the vote: noise corrupts ONE copy differently, so the two
    /// clean copies still agree and the damaged ones never see print.
    #[test]
    fn a_singly_corrupted_copy_is_voted_out() {
        let good = b"ZCZC-WXR-TOR-090001+0030-1231200-KOKX-NNNN".to_vec();
        let mut bad = good.clone();
        bad[12] = b'R'; // WXR -> RXR in copy 1

        let mut framed: Vec<u8> = vec![0xAB; 16];
        for m in [&bad, &good, &good] {
            framed.extend_from_slice(m);
        }
        let audio = bits_to_audio(&bytes_to_uart_bits(&framed), 16_000.0);
        let mut d = SameDecoder::new(16_000.0);
        let hits = d.process(&audio);
        // bad → held; good #1 → held (differs from bad); good #2 → agrees
        // with good #1 and publishes ONCE. One alert in, one alert out.
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].raw.contains("ZCZC-WXR-TOR"));

        // All-different copies publish nothing: better silence than garbage.
        let mut worse = good.clone();
        worse[24] = b'9';
        let mut framed: Vec<u8> = vec![0xAB; 16];
        for m in [&bad, &worse, &worse[..24].to_vec()].into_iter() {
            framed.extend_from_slice(m);
        }
        framed.truncate(200); // the third copy is truncated garbage anyway
        let audio = bits_to_audio(&bytes_to_uart_bits(&framed), 16_000.0);
        let mut d = SameDecoder::new(16_000.0);
        assert!(d.process(&audio).is_empty());
    }
}
