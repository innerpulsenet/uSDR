//! Motorola Type II / SmartNet / SmartZone 3600-baud control channel.
//!
//! The outbound control stream is binary MSK.  After FM discrimination it is
//! a two-level NRZ waveform containing a 24-bit sync word followed by two
//! BCH(64,16,11) codewords.  Those codewords recover the 16-bit address and
//! 16-bit command that make up an outbound signalling word (OSW).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const BAUD: f64 = 3600.0;
const SYNC: u32 = 0xA4_D7_AA;
const SYNC_BITS: usize = 24;
const CODEWORD_BITS: usize = 128;
const FRAME_BITS: usize = SYNC_BITS + CODEWORD_BITS;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Osw {
    pub address: u16,
    pub command: u16,
    pub opcode: u16,
    pub opcode_name: String,
    pub lcn: u8,
    pub corrected_bits: usize,
    pub sync_errors: usize,
    pub inverted: bool,
    pub raw: String,
    pub fields: BTreeMap<String, String>,
}

impl Osw {
    pub fn is_known(&self) -> bool {
        opcode_name(self.opcode) != "unknown OSW"
    }
}

/// Stateful discriminator-to-OSW decoder. A small rolling window plus a bank
/// of fractional clock phases is intentional: it acquires a control channel
/// without depending on SDR input block boundaries and remains tolerant of
/// the non-integral 13.333 samples/symbol produced at 48 kHz.
pub struct SmartNetDecoder {
    fs: f64,
    samples: Vec<f32>,
    /// Reused median-partition scratch, so the periodic decode does not copy
    /// and fully sort the whole window.
    scratch: Vec<f32>,
    since_decode: usize,
}

impl SmartNetDecoder {
    pub fn new(fs: f64) -> Self {
        Self {
            fs,
            samples: Vec::with_capacity((fs * 0.18) as usize),
            scratch: Vec::new(),
            since_decode: 0,
        }
    }

    pub fn reset(&mut self) {
        self.samples.clear();
        self.since_decode = 0;
    }

    /// Consume FM discriminator samples in any linear unit (normally hertz).
    pub fn process(&mut self, discriminator: &[f32]) -> Vec<Osw> {
        if discriminator.is_empty() {
            return Vec::new();
        }
        self.samples.extend_from_slice(discriminator);
        self.since_decode = self.since_decode.saturating_add(discriminator.len());

        let frame_samples = (self.fs / BAUD * FRAME_BITS as f64).ceil() as usize;
        let history = (self.fs * 0.18).round() as usize;
        if self.samples.len() > history {
            let excess = self.samples.len() - history;
            self.samples.drain(..excess);
        }
        // Do not rescan the rolling window on every tiny device block.
        if self.samples.len() < frame_samples || self.since_decode < (self.fs * 0.025) as usize {
            return Vec::new();
        }
        self.since_decode = 0;
        let mut scratch = std::mem::take(&mut self.scratch);
        let osws = decode_window(&self.samples, self.fs, &mut scratch);
        self.scratch = scratch;
        osws
    }
}

fn decode_window(samples: &[f32], fs: f64, scratch: &mut Vec<f32>) -> Vec<Osw> {
    let sps = fs / BAUD;
    if sps < 2.0 || samples.len() < (sps * FRAME_BITS as f64).ceil() as usize {
        return Vec::new();
    }

    // Remove receiver mistuning. Median is robust when the bit balance in the
    // short decode window is uneven. Only that one order statistic is read,
    // so a partition replaces the copy-and-full-sort this used to pay.
    scratch.clear();
    scratch.extend_from_slice(samples);
    let mid = scratch.len() / 2;
    scratch.select_nth_unstable_by(mid, f32::total_cmp);
    let threshold = scratch[mid];
    let mut decoded = BTreeMap::<String, Osw>::new();

    // A phase bank acquires symbol timing. Integrating the middle 60% of each
    // symbol approximates the MSK matched filter without smearing transitions.
    for phase in 0..20 {
        let mut bits = Vec::with_capacity((samples.len() as f64 / sps) as usize);
        let mut center = (phase as f64 / 20.0 + 0.5) * sps;
        while center + 0.3 * sps < samples.len() as f64 {
            let start = (center - 0.3 * sps).max(0.0).floor() as usize;
            let end = (center + 0.3 * sps).ceil() as usize;
            let mean = samples[start..end.min(samples.len())].iter().sum::<f32>()
                / (end.min(samples.len()) - start).max(1) as f32;
            bits.push(mean > threshold);
            center += sps;
        }

        for start in 0..=bits.len().saturating_sub(FRAME_BITS) {
            let (sync_errors, inverted) = sync_distance(&bits[start..start + SYNC_BITS]);
            if sync_errors > 2 {
                continue;
            }
            let post = &bits[start + SYNC_BITS..start + FRAME_BITS];
            let Some((address, e1)) = decode_half(&post[..64], inverted) else {
                continue;
            };
            let Some((command, e2)) = decode_half(&post[64..], inverted) else {
                continue;
            };
            let corrected_bits = e1 + e2;
            let opcode = command >> 4;
            // Unknown opcodes are retained only with particularly strong RF
            // evidence. This preserves vendor diagnostics without letting two
            // heavily corrected random words claim protocol ownership.
            if opcode_name(opcode) == "unknown OSW" && (sync_errors != 0 || corrected_bits > 2) {
                continue;
            }
            let osw = parse_osw(address, command, corrected_bits, sync_errors, inverted);
            decoded.entry(osw.raw.clone()).or_insert(osw);
        }
    }
    decoded.into_values().collect()
}

fn sync_distance(bits: &[bool]) -> (usize, bool) {
    let errors = (0..SYNC_BITS)
        .filter(|&i| bits[i] != (SYNC & (1 << (SYNC_BITS - 1 - i)) != 0))
        .count();
    if errors <= SYNC_BITS - errors {
        (errors, false)
    } else {
        (SYNC_BITS - errors, true)
    }
}

fn decode_half(bits: &[bool], inverted: bool) -> Option<(u16, usize)> {
    let mut wire = 0u64;
    for &bit in bits {
        wire = (wire << 1) | u64::from(if inverted { !bit } else { bit });
    }
    let received_parity = (wire & 1) as u32;
    let cw = wire >> 1;
    let (fixed, mut errors) = crate::p25::bch_correct(cw)?;
    let parity = fixed.count_ones() & 1;
    errors += usize::from(parity != received_parity);
    (errors <= 11).then_some(((fixed >> 47) as u16, errors))
}

fn parse_osw(
    address: u16,
    command: u16,
    corrected_bits: usize,
    sync_errors: usize,
    inverted: bool,
) -> Osw {
    let opcode = command >> 4;
    let lcn = (command & 0x0f) as u8;
    let name = opcode_name(opcode);
    let mut fields = BTreeMap::new();
    fields.insert("address".into(), address.to_string());
    fields.insert("opcode".into(), format!("0x{opcode:03X}"));
    fields.insert("lcn".into(), lcn.to_string());
    match opcode {
        0x308 | 0x309 => {
            fields.insert("talkgroup".into(), address.to_string());
            fields.insert("callType".into(), "group voice".into());
        }
        0x30b => {
            fields.insert("targetRadio".into(), address.to_string());
            fields.insert("callType".into(), "private voice".into());
        }
        0x31b => {
            fields.insert("adjacentSite".into(), address.to_string());
            fields.insert("controlLcn".into(), lcn.to_string());
        }
        0x080 => {
            fields.insert("systemId".into(), format!("0x{address:04X}"));
            fields.insert("systemClass".into(), lcn.to_string());
        }
        0x310 => {
            fields.insert("target".into(), address.to_string());
            fields.insert("callType".into(), "data".into());
        }
        0x320 => {
            fields.insert("radioId".into(), address.to_string());
        }
        0x140 => {
            fields.insert("encryptedAddress".into(), address.to_string());
            fields.insert("encrypted".into(), "true".into());
        }
        0x300 => {
            fields.insert("emergencyAddress".into(), address.to_string());
            fields.insert("emergency".into(), "true".into());
        }
        _ => {}
    }
    Osw {
        address,
        command,
        opcode,
        opcode_name: name.into(),
        lcn,
        corrected_bits,
        sync_errors,
        inverted,
        raw: format!("{address:04X}{command:04X}"),
        fields,
    }
}

fn opcode_name(opcode: u16) -> &'static str {
    match opcode {
        0x308 => "group voice channel grant",
        0x309 => "group voice channel grant update",
        0x30b => "private call grant",
        0x31b => "adjacent site status",
        0x080 => "extended system ID",
        0x310 => "data channel grant",
        0x320 => "affiliation response",
        0x28d | 0x290 => "idle",
        0x140 => "encryption indication",
        0x300 => "emergency indication",
        _ => "unknown OSW",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded_half(value: u16) -> Vec<bool> {
        let cw = crate::p25::bch_encode(value);
        let parity = cw.count_ones() & 1;
        (0..63)
            .rev()
            .map(|i| cw & (1 << i) != 0)
            .chain(std::iter::once(parity != 0))
            .collect()
    }

    fn frame(address: u16, command: u16) -> Vec<bool> {
        let mut bits = (0..SYNC_BITS)
            .rev()
            .map(|i| SYNC & (1 << i) != 0)
            .collect::<Vec<_>>();
        bits.extend(encoded_half(address));
        bits.extend(encoded_half(command));
        bits
    }

    #[test]
    fn bch_corrects_errors_in_both_halves() {
        let address = 0x4321;
        let command = 0x3087;
        let mut bits = frame(address, command);
        bits[SYNC_BITS + 4] = !bits[SYNC_BITS + 4];
        bits[SYNC_BITS + 64 + 33] = !bits[SYNC_BITS + 64 + 33];
        let (a, e1) = decode_half(&bits[SYNC_BITS..SYNC_BITS + 64], false).unwrap();
        let (c, e2) = decode_half(&bits[SYNC_BITS + 64..], false).unwrap();
        assert_eq!((a, c), (address, command));
        assert_eq!(e1 + e2, 2);
    }

    #[test]
    fn discriminator_stream_decodes_group_grant_at_fractional_sps() {
        let bits = frame(12_345, 0x3087);
        let fs = 48_000.0;
        let sps = fs / BAUD;
        let lead = 37usize;
        let count = lead + (bits.len() as f64 * sps).ceil() as usize + 40;
        let samples = (0..count)
            .map(|i| {
                if i < lead {
                    return -2200.0;
                }
                let bit = ((i - lead) as f64 / sps).floor() as usize;
                if bits.get(bit).copied().unwrap_or(false) {
                    2200.0
                } else {
                    -2200.0
                }
            })
            .collect::<Vec<_>>();
        let decoded = decode_window(&samples, fs, &mut Vec::new());
        assert!(decoded.iter().any(|osw| {
            osw.address == 12_345 && osw.lcn == 7 && osw.opcode_name == "group voice channel grant"
        }));
    }

    #[test]
    fn parses_site_and_security_activity() {
        let adjacent = parse_osw(17, 0x31b4, 0, 0, false);
        assert_eq!(adjacent.fields["adjacentSite"], "17");
        assert_eq!(adjacent.fields["controlLcn"], "4");
        assert_eq!(
            parse_osw(99, 0x1400, 0, 0, false).fields["encrypted"],
            "true"
        );
        assert_eq!(
            parse_osw(99, 0x3000, 0, 0, false).fields["emergency"],
            "true"
        );
    }
}
