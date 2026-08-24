//! MDC-1200 in-band signalling: the PTT-ID burst analog squelch tags a
//! transmission with.
//!
//! 1200-baud XOR-precoded MSK on 1200/1800 Hz, sent at the start (and
//! sometimes the end) of a voice call. A burst is an alternating preamble, a
//! 40-bit sync word, and one 112-bit interleaved codeword — two for the
//! "double" opcodes. Polarity on the air is arbitrary, so the sync matcher
//! accepts the inverted word too and flips every bit that follows.
//!
//! In this modulation the sampled waveform polarity is the decoded data state:
//! 1200 Hz completes one cycle per bit and preserves it, while 1800 Hz completes
//! one-and-a-half cycles and flips it. A phase bank samples that state at every
//! possible bit alignment; each lane runs its own sync/CRC state machine and the
//! first lane to land a valid CRC wins.

use crate::biquad::Cascade;

#[cfg(test)]
const MARK: f32 = 1200.0;
#[cfg(test)]
const SPACE: f32 = 1800.0;
const BAUD: f32 = 1200.0;
/// One lane per sample across a baud at 16 kHz (≈13.3 samples/bit).
const PHASES: usize = 16;

/// The 40-bit sync word, transmitted MSB first: 0x07 then 0x092A446F.
const SYNC: u64 = 0x07_09_2A_44_6F;
const SYNC_BITS: u32 = 40;
const SYNC_MASK: u64 = (1 << SYNC_BITS) - 1;
/// Bit errors tolerated on the sync word.
const SYNC_TOLERANCE: u32 = 3;

/// Bits in one codeword: 14 bytes, interleaved.
const WORD_BITS: usize = 112;

/// A decoded MDC-1200 packet.
#[derive(Clone, Debug, PartialEq)]
pub struct MdcPacket {
    pub op: u8,
    pub arg: u8,
    pub unit_id: u16,
    /// Second word of a double packet (opcodes 0x35 and 0x55).
    pub extra: Option<[u8; 4]>,
}

impl MdcPacket {
    /// e.g. "MDC 1A2B PTT-ID". Unit IDs are conventionally four hex digits.
    pub fn label(&self) -> String {
        let op = match self.op {
            0x01 => "PTT-ID".to_string(),
            0x35 => "MSG".into(),
            0x55 => "STS".into(),
            other => format!("OP {other:02X}"),
        };
        format!("MDC {:04X} {op}", self.unit_id)
    }
}

/// One slicer phase and the burst it is currently chasing.
struct Lane {
    slicer: PolaritySlicer,
    /// Sliding sync window: the last 40 bits seen, newest in bit 0.
    window: u64,
    window_bits: u32,
    invert: bool,
    collecting: bool,
    word: [bool; WORD_BITS],
    word_len: usize,
    /// First word of a double packet, waiting on the second.
    pending: Option<[u8; 4]>,
}

impl Lane {
    fn new(fs: f64, delay: usize) -> Self {
        Self {
            slicer: PolaritySlicer::new(fs, delay),
            window: 0,
            window_bits: 0,
            invert: false,
            collecting: false,
            word: [false; WORD_BITS],
            word_len: 0,
            pending: None,
        }
    }

    /// Back to hunting; the slicer keeps its bit clock.
    fn abort(&mut self) {
        self.collecting = false;
        self.word_len = 0;
        self.pending = None;
    }

    fn reset(&mut self) {
        self.slicer.reset();
        self.window = 0;
        self.window_bits = 0;
        self.invert = false;
        self.abort();
    }

    fn push_bit(&mut self, raw: bool) -> Option<MdcPacket> {
        if !self.collecting {
            self.window = (self.window << 1 | u64::from(raw)) & SYNC_MASK;
            self.window_bits += 1;
            if self.window_bits >= SYNC_BITS {
                let errors = (self.window ^ SYNC).count_ones();
                if errors <= SYNC_TOLERANCE || errors >= SYNC_BITS - SYNC_TOLERANCE {
                    // ≥37 errors means the inverted sync matched: the air
                    // polarity is opposite the slicer's, so flip everything.
                    self.invert = errors >= SYNC_BITS - SYNC_TOLERANCE;
                    self.collecting = true;
                    self.word_len = 0;
                }
            }
            return None;
        }

        self.word[self.word_len] = raw ^ self.invert;
        self.word_len += 1;
        if self.word_len < WORD_BITS {
            return None;
        }
        self.word_len = 0;

        let mut data = unpack(&self.word);
        gofix(&mut data);
        let received = u16::from(data[5]) << 8 | u16::from(data[4]);
        if crc(&data[..4]) != received {
            // A double packet whose second word failed still has a valid first.
            let pkt = self.pending.take().map(|first| packet(first, None));
            self.collecting = false;
            return pkt;
        }
        let word: [u8; 4] = data[..4].try_into().expect("four bytes");
        match self.pending.take() {
            Some(first) => {
                self.collecting = false;
                Some(packet(first, Some(word)))
            }
            None if matches!(word[0], 0x35 | 0x55) => {
                // Double packet: a second 112-bit word follows directly.
                self.pending = Some(word);
                None
            }
            None => {
                self.collecting = false;
                Some(packet(word, None))
            }
        }
    }
}

/// Fractional 1200-baud clock for one sample phase.
///
/// This intentionally samples waveform polarity instead of deciding which
/// tone has more energy. Treating the 1200/1800 Hz choice as a data bit can
/// round-trip a matching synthetic generator, but it is not how MDC's
/// XOR-precoded MSK is decoded over the air.
struct PolaritySlicer {
    spb: f32,
    bits: usize,
    samples: usize,
    delay: usize,
    delay0: usize,
}

impl PolaritySlicer {
    fn new(fs: f64, delay: usize) -> Self {
        Self {
            spb: (fs as f32 / BAUD).max(4.0),
            bits: 0,
            samples: 0,
            delay,
            delay0: delay,
        }
    }

    fn reset(&mut self) {
        self.bits = 0;
        self.samples = 0;
        self.delay = self.delay0;
    }

    fn push(&mut self, x: f32) -> Option<bool> {
        if self.delay > 0 {
            self.delay -= 1;
            return None;
        }
        self.samples += 1;
        let due = ((self.bits as f32 + 1.0) * self.spb).round() as usize;
        if self.samples < due {
            return None;
        }
        self.bits += 1;
        if self.bits >= 1200 {
            self.bits -= 1200;
            self.samples = self
                .samples
                .saturating_sub((1200.0 * self.spb).round() as usize);
        }
        Some(x >= 0.0)
    }
}

fn packet(word: [u8; 4], extra: Option<[u8; 4]>) -> MdcPacket {
    MdcPacket {
        op: word[0],
        arg: word[1],
        unit_id: u16::from(word[2]) << 8 | u16::from(word[3]),
        extra,
    }
}

pub struct MdcDecoder {
    lanes: Vec<Lane>,
    highpass: Cascade,
}

impl MdcDecoder {
    pub fn new(fs: f64) -> Self {
        let n = PHASES.min((fs / f64::from(BAUD)).round().max(1.0) as usize);
        Self {
            lanes: (0..n).map(|d| Lane::new(fs, d)).collect(),
            // Receiver discriminator audio can carry a large mistuning offset
            // and CTCSS/DCS. Neither belongs in the polarity decision.
            highpass: Cascade::highpass(300.0, fs as f32, 2),
        }
    }

    pub fn reset(&mut self) {
        self.highpass.reset();
        for lane in &mut self.lanes {
            lane.reset();
        }
    }

    /// Discriminator audio. Returns the latest completed packet, if one
    /// finished inside this block.
    pub fn push(&mut self, audio: &[f32]) -> Option<MdcPacket> {
        let mut out = None;
        for &x in audio {
            let x = self.highpass.process(x);
            let mut decoded = false;
            for lane in &mut self.lanes {
                let Some(bit) = lane.slicer.push(x) else {
                    continue;
                };
                if let Some(p) = lane.push_bit(bit) {
                    out = Some(p);
                    decoded = true;
                }
            }
            if decoded {
                // The burst is over; the other lanes must not report it again.
                for lane in &mut self.lanes {
                    lane.abort();
                }
            }
        }
        out
    }
}

/// Deinterleave 112 received bits into 14 bytes, LSB first per byte.
fn unpack(rx: &[bool; WORD_BITS]) -> [u8; 14] {
    let mut lbits = [false; WORD_BITS];
    for i in 0..16 {
        for j in 0..7 {
            lbits[i * 7 + j] = rx[j * 16 + i];
        }
    }
    let mut data = [0u8; 14];
    for (i, byte) in data.iter_mut().enumerate() {
        for b in 0..8 {
            if lbits[8 * i + b] {
                *byte |= 1 << b;
            }
        }
    }
    data
}

/// Reflected CCITT over the four payload bytes, as the air interface defines
/// it: each input byte bit-reversed, the 16-bit result bit-reversed and
/// complemented.
fn crc(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        let c = b.reverse_bits();
        let mut j = 0x80u8;
        while j != 0 {
            let mut bit = crc & 0x8000;
            crc <<= 1;
            if c & j != 0 {
                bit ^= 0x8000;
            }
            if bit != 0 {
                crc ^= 0x1021;
            }
            j >>= 1;
        }
    }
    crc.reverse_bits() ^ 0xFFFF
}

/// Single-bit correction from the eight FEC parity bytes. The syndrome taps
/// (0xA6) name the error pattern; three or more set bits pin the fault to one
/// data bit, which is flipped before the CRC gets its say.
fn gofix(data: &mut [u8; 14]) {
    let mut csr = [0u8; 7];
    let mut syn = 0u8;
    for i in 0..7 {
        for j in 0..8 {
            for k in (1..7).rev() {
                csr[k] = csr[k - 1];
            }
            csr[0] = (data[i] >> j) & 1;
            let b = csr[0] + csr[2] + csr[5] + csr[6];
            syn <<= 1;
            if (b & 1) ^ ((data[i + 7] >> j) & 1) != 0 {
                syn |= 1;
            }
            if (syn & 0xA6).count_ones() >= 3 {
                syn ^= 0xA6;
                let mut fixi = i as i32;
                let mut fixj = j - 7;
                if fixj < 0 {
                    fixi -= 1;
                    fixj += 8;
                }
                if fixi >= 0 {
                    data[fixi as usize] ^= 1 << fixj;
                }
            }
        }
    }
}

/// Test encoder: 14 bytes (payload, CRC, zero byte, 7 FEC bytes) → the 112
/// transmitted bits, interleaved the inverse of [`unpack`].
#[cfg(test)]
fn encode_word(payload: [u8; 4]) -> [bool; WORD_BITS] {
    let mut data = [0u8; 14];
    data[..4].copy_from_slice(&payload);
    let c = crc(&payload);
    data[4] = c as u8;
    data[5] = (c >> 8) as u8;
    // FEC parity over the bit stream of bytes 0..7 (LSB first): the decoder
    // expects parity[p] = d[p] ^ d[p-2] ^ d[p-5] ^ d[p-6].
    let d = |data: &[u8; 14], p: i32| -> u8 {
        if p < 0 {
            0
        } else {
            (data[(p / 8) as usize] >> (p % 8)) & 1
        }
    };
    for i in 0..7 {
        let mut parity = 0u8;
        for j in 0..8 {
            let p = (i * 8 + j) as i32;
            let bit = d(&data, p) ^ d(&data, p - 2) ^ d(&data, p - 5) ^ d(&data, p - 6);
            parity |= bit << j;
        }
        data[7 + i] = parity;
    }
    let mut lbits = [false; WORD_BITS];
    for (i, &byte) in data.iter().enumerate() {
        for b in 0..8 {
            lbits[8 * i + b] = (byte >> b) & 1 == 1;
        }
    }
    let mut tx = [false; WORD_BITS];
    for i in 0..16 {
        for j in 0..7 {
            tx[j * 16 + i] = lbits[i * 7 + j];
        }
    }
    tx
}

/// A full burst: alternating preamble, sync, then each word.
#[cfg(test)]
pub(crate) fn burst(words: &[[u8; 4]], inverted: bool) -> Vec<f32> {
    let mut bits = Vec::new();
    for k in 0..32 {
        bits.push(k % 2 == 0);
    }
    for k in (0..SYNC_BITS).rev() {
        bits.push((SYNC >> k) & 1 == 1);
    }
    for &word in words {
        bits.extend_from_slice(&encode_word(word));
    }
    // XOR precoding: an unchanged data state selects 1200 Hz (one cycle per
    // bit); a transition selects 1800 Hz (one-and-a-half cycles per bit).
    let mut previous = false;
    let marks: Vec<bool> = bits
        .into_iter()
        .map(|bit| {
            let mark = bit == previous;
            previous = bit;
            mark
        })
        .collect();
    let mut audio = crate::afsk::marks_to_audio(&marks, 16_000.0, BAUD, MARK, SPACE, 0.8);
    if inverted {
        for x in &mut audio {
            *x = -*x;
        }
    }
    // Let the final sampled symbol clear the receive filter and clock.
    audio.extend(std::iter::repeat_n(0.0, 64));
    audio
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ptt_id_burst_round_trips() {
        let audio = burst(&[[0x01, 0x00, 0x1A, 0x2B]], false);
        let mut dec = MdcDecoder::new(16_000.0);
        let pkt = dec.push(&audio).expect("the burst should decode");
        assert_eq!(pkt.op, 0x01);
        assert_eq!(pkt.unit_id, 0x1A2B);
        assert_eq!(pkt.extra, None);
        assert_eq!(pkt.label(), "MDC 1A2B PTT-ID");
    }

    #[test]
    fn an_inverted_burst_decodes_through_the_invert_branch() {
        let audio = burst(&[[0x01, 0x00, 0x1A, 0x2B]], true);
        let mut dec = MdcDecoder::new(16_000.0);
        let pkt = dec.push(&audio).expect("inverted polarity should decode");
        assert_eq!(pkt.unit_id, 0x1A2B);
    }

    #[test]
    fn a_double_packet_exposes_its_second_word() {
        let audio = burst(&[[0x55, 0x00, 0x1A, 0x2B], [0x00, 0x00, 0x0B, 0x0E]], false);
        let mut dec = MdcDecoder::new(16_000.0);
        let pkt = dec.push(&audio).expect("the double burst should decode");
        assert_eq!(pkt.op, 0x55);
        assert_eq!(pkt.extra, Some([0x00, 0x00, 0x0B, 0x0E]));
        assert_eq!(pkt.label(), "MDC 1A2B STS");
    }

    #[test]
    fn noise_produces_no_packet() {
        let mut s = 0x1234_5678u32;
        let audio: Vec<f32> = (0..16_000)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f32 / 8388608.0 - 1.0
            })
            .collect();
        let mut dec = MdcDecoder::new(16_000.0);
        assert_eq!(dec.push(&audio), None);
    }

    /// One flipped bit in the payload must not cost the packet: the FEC fixes
    /// it before the CRC runs.
    #[test]
    fn a_single_bit_error_is_corrected() {
        let mut bits = Vec::new();
        for k in 0..32 {
            bits.push(k % 2 == 0);
        }
        for k in (0..SYNC_BITS).rev() {
            bits.push((SYNC >> k) & 1 == 1);
        }
        let mut word = encode_word([0x01, 0x00, 0x1A, 0x2B]);
        // A payload bit: transmitted position 20 lands inside the first byte.
        word[20] = !word[20];
        bits.extend_from_slice(&word);
        let mut previous = false;
        let marks: Vec<bool> = bits
            .into_iter()
            .map(|bit| {
                let mark = bit == previous;
                previous = bit;
                mark
            })
            .collect();
        let audio = crate::afsk::marks_to_audio(&marks, 16_000.0, BAUD, MARK, SPACE, 0.8);
        let mut dec = MdcDecoder::new(16_000.0);
        let pkt = dec.push(&audio).expect("one bad bit should be corrected");
        assert_eq!(pkt.unit_id, 0x1A2B);
    }
}
