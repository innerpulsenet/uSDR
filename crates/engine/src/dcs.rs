//! Digital Coded Squelch (DCS/DPL) detection.
//!
//! ETSI TS 103 236 defines a continuously repeated 23-bit cyclic Golay word
//! at 134.4 bit/s.  The low nine information bits are the familiar three-digit
//! octal code, followed by the fixed bits `100`; the word is sent LSB first.
//! There is no separate sync pattern, so detection tries each symbol phase and
//! confirms the same valid word on at least two successive repetitions.

const ANALYSIS_RATE: f64 = 1_000.0;
const BAUD: f64 = 134.4;
const SAMPLES_PER_BIT: f64 = ANALYSIS_RATE / BAUD;
const WINDOW: usize = 400;
const MAX_ERRORS: u32 = 3;

/// The 105 DCS assignments commonly supported by current radios. Values are
/// written in octal because that is how DCS codes are named and configured.
const CODES: [u16; 105] = [
    0o023, 0o025, 0o026, 0o031, 0o032, 0o036, 0o043, 0o047, 0o051, 0o053, 0o054, 0o065, 0o071,
    0o072, 0o073, 0o074, 0o114, 0o115, 0o116, 0o122, 0o125, 0o131, 0o132, 0o134, 0o143, 0o145,
    0o152, 0o155, 0o156, 0o162, 0o165, 0o172, 0o174, 0o205, 0o212, 0o223, 0o225, 0o226, 0o243,
    0o244, 0o245, 0o246, 0o251, 0o252, 0o255, 0o261, 0o263, 0o265, 0o266, 0o271, 0o274, 0o306,
    0o311, 0o315, 0o325, 0o331, 0o332, 0o343, 0o346, 0o351, 0o356, 0o364, 0o365, 0o371, 0o411,
    0o412, 0o413, 0o423, 0o431, 0o432, 0o445, 0o446, 0o452, 0o454, 0o455, 0o462, 0o464, 0o465,
    0o466, 0o503, 0o506, 0o516, 0o523, 0o526, 0o532, 0o546, 0o565, 0o606, 0o612, 0o624, 0o627,
    0o631, 0o632, 0o645, 0o654, 0o662, 0o664, 0o703, 0o712, 0o723, 0o731, 0o732, 0o734, 0o743,
    0o754,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Detection {
    pub code: u16,
    pub inverted: bool,
    pub errors: u32,
}

pub struct DcsDetector {
    decim: usize,
    counter: usize,
    buf: Vec<f32>,
}

pub fn is_valid_code(code: u16) -> bool {
    CODES.contains(&code)
}

impl DcsDetector {
    pub fn new(fs_in: f64) -> Self {
        Self {
            decim: ((fs_in / ANALYSIS_RATE).round() as usize).max(1),
            counter: 0,
            buf: Vec::with_capacity(WINDOW),
        }
    }

    /// Feed unfiltered/sub-audible discriminator audio. A verdict is returned
    /// after 400 ms; `Some(None)` means a complete window contained no DCS.
    pub fn push(&mut self, subaudible: &[f32]) -> Option<Option<Detection>> {
        for &sample in subaudible {
            if self.counter == 0 {
                self.buf.push(sample);
            }
            self.counter = (self.counter + 1) % self.decim;
        }
        if self.buf.len() < WINDOW {
            return None;
        }
        let verdict = analyse(&self.buf[..WINDOW]);
        self.buf.drain(..WINDOW);
        Some(verdict)
    }

    pub fn reset(&mut self) {
        self.counter = 0;
        self.buf.clear();
    }
}

pub fn analyse(samples: &[f32]) -> Option<Detection> {
    if samples.len() < WINDOW {
        return None;
    }
    let mean = samples.iter().sum::<f32>() / samples.len() as f32;
    let rms = (samples
        .iter()
        .map(|sample| (sample - mean) * (sample - mean))
        .sum::<f32>()
        / samples.len() as f32)
        .sqrt();
    if rms < 0.005 {
        return None;
    }

    let mut best: Option<(usize, u32, Detection)> = None;

    // One packed bit stream, reused across phases: bit `i` of the phase's
    // slicer output lives in bit `i % 64` of `packed[i / 64]`, so every
    // candidate start extracts its 23-bit window with a shift-or instead of
    // re-packing 23 bits one at a time (which dominated this search at
    // ~30 phases x 105 codes x ~30 starts per window).
    let mut packed: Vec<u64> = Vec::new();

    // Quarter-sample phase steps are cheap at this rate and tolerate the
    // fractional 7.4405 samples/bit without a separate clock-recovery loop.
    let phase_steps = (SAMPLES_PER_BIT * 4.0).ceil() as usize;
    for phase_step in 0..phase_steps {
        let phase = phase_step as f64 / 4.0;
        packed.clear();
        let mut word = 0u64;
        let mut used = 0u32;
        let mut n_bits = 0usize;
        let mut at = phase;
        while (at.round() as usize) < samples.len() {
            if samples[at.round() as usize] >= mean {
                word |= 1u64 << used;
            }
            n_bits += 1;
            used += 1;
            if used == 64 {
                packed.push(word);
                word = 0;
                used = 0;
            }
            at += SAMPLES_PER_BIT;
        }
        if used > 0 {
            packed.push(word);
        }
        if n_bits < 46 {
            // Two full repetitions are needed for the confirmation below.
            continue;
        }

        for &(code, target) in code_table() {
            // Normal and inverted DCS names are cyclic/complement aliases (for
            // example 532-N is the same waveform as 343-I). Configuration in
            // scannerd uses the conventional normal form, so report the normal
            // code instead of returning an arbitrary equivalent polarity.
            let inverted = false;
            // The 23 bits from `start`, first bit in bit 0 — the layout the
            // reversed targets in `code_table` compare against.
            let distance_at = |start: usize| -> u32 {
                let wi = start / 64;
                let off = start % 64;
                let mut window = packed[wi] >> off;
                if off > 0 {
                    window |= packed.get(wi + 1).copied().unwrap_or(0) << (64 - off);
                }
                ((window & 0x7F_FFFF) ^ u64::from(target)).count_ones()
            };
            // DCS is the word repeated back to back, so a real one matches at
            // `start` AND `start + 23` in the same phase, with the error
            // budget shared across both repetitions. Counting any two matches
            // anywhere in the window let noise qualify almost every window:
            // 30 phases × 105 codes × 31 starts at three errors each is
            // thousands of chances at a 2.4e-4 event.
            let mut matches = 0usize;
            let mut errors = 0u32;
            for start in 0..=n_bits - 46 {
                let distance = distance_at(start) + distance_at(start + 23);
                if distance <= MAX_ERRORS {
                    matches += 1;
                    errors += distance;
                }
            }
            if matches == 0 {
                continue;
            }
            let detection = Detection {
                code,
                inverted,
                errors,
            };
            if best.as_ref().is_none_or(|(best_matches, best_errors, _)| {
                matches > *best_matches || (matches == *best_matches && errors < *best_errors)
            }) {
                best = Some((matches, errors, detection));
            }
        }
    }
    best.map(|(_, _, detection)| detection)
}

/// Canonical over-the-air rolling word. The Golay encoder is systematic with
/// the information in its high 12 bits; reversing the information before
/// encoding accounts for DCS transmitting bit 1 first.
pub(crate) fn transmitted_word(code: u16) -> u32 {
    let data = u32::from(code) | (0b100 << 9);
    golay23_encode(reverse(data, 12))
}

/// code → comparison word, computed once. The encode is a per-code Golay
/// pass that used to rerun on every analyse window (~30×/s while active).
///
/// The stored word is the transmitted word *reversed into 23 bits*: bit 0 is
/// the first bit on air, matching the LSB-first windows the packed bit
/// stream yields, so a candidate start compares with one XOR + popcount.
/// The Hamming distance is unchanged — both sides are permuted identically.
fn code_table() -> &'static [(u16, u32)] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<Vec<(u16, u32)>> = OnceLock::new();
    TABLE.get_or_init(|| {
        CODES
            .iter()
            .map(|&c| (c, reverse(transmitted_word(c), 23)))
            .collect()
    })
}

fn reverse(mut value: u32, bits: usize) -> u32 {
    let mut reversed = 0;
    for _ in 0..bits {
        reversed = (reversed << 1) | (value & 1);
        value >>= 1;
    }
    reversed
}

fn golay23_encode(data: u32) -> u32 {
    let mut reg = data << 11;
    const POLY: u32 = 0x0ae3;
    for bit in (11..=22).rev() {
        if reg & (1 << bit) != 0 {
            reg ^= POLY << (bit - 11);
        }
    }
    (data << 11) | (reg & 0x07ff)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waveform(code: u16, inverted: bool, fs: f64, seconds: f64) -> Vec<f32> {
        let word = transmitted_word(code);
        (0..(fs * seconds) as usize)
            .map(|sample| {
                let bit = ((sample as f64 * BAUD / fs) as usize) % 23;
                let one = ((word >> (22 - bit)) & 1) != 0;
                if one ^ inverted { 0.2 } else { -0.2 }
            })
            .collect()
    }

    #[test]
    fn generated_words_match_published_dcs_vectors() {
        assert_eq!(transmitted_word(0o023), 6_557_239);
        assert_eq!(transmitted_word(0o071), 5_115_123);
        assert_eq!(transmitted_word(0o532), 2_969_144);
    }

    #[test]
    fn normal_and_inverted_waveforms_decode_to_their_normal_alias() {
        let normal = analyse(&waveform(0o071, false, ANALYSIS_RATE, 0.7)).unwrap();
        assert_eq!((normal.code, normal.inverted), (0o071, false));
        let inverted = analyse(&waveform(0o532, true, ANALYSIS_RATE, 0.7)).unwrap();
        assert_eq!((inverted.code, inverted.inverted), (0o343, false));
    }

    #[test]
    fn streaming_detector_decimates_receiver_audio() {
        let mut detector = DcsDetector::new(16_000.0);
        let input = waveform(0o532, false, 16_000.0, 0.7);
        let result = detector
            .push(&input)
            .expect("window complete")
            .expect("DCS");
        assert_eq!((result.code, result.inverted), (0o532, false));
    }

    #[test]
    fn silence_is_not_a_code() {
        assert_eq!(analyse(&vec![0.0; WINDOW]), None);
    }

    fn lcg(seed: &mut u64) -> f32 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*seed >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
    }

    /// The sub-audible band of a channel with no squelch code on it is
    /// receiver noise. Two matches of a 23-bit word within three errors,
    /// anywhere in a window, over 30 phases and 105 codes, was satisfied by
    /// noise in almost every window: a NOAA weather broadcast carried a
    /// steady "DCS Code: D074" it does not transmit.
    #[test]
    fn noise_is_not_a_code() {
        let mut seed = 0xdc5_u64;
        let mut hits = 0;
        for _ in 0..200 {
            let samples: Vec<f32> = (0..WINDOW)
                .map(|_| (0..12).map(|_| lcg(&mut seed)).sum::<f32>() / 2.0 * 0.2)
                .collect();
            if analyse(&samples).is_some() {
                hits += 1;
            }
        }
        assert_eq!(hits, 0, "{hits} of 200 noise windows produced a DCS code");
    }

    /// Mains hum in the sub-audible band slices to a periodic bit stream,
    /// which is not a DCS word at any phase.
    #[test]
    fn hum_is_not_a_code() {
        for hum_hz in [50.0f32, 60.0, 100.0, 120.0, 180.0] {
            let samples: Vec<f32> = (0..WINDOW)
                .map(|i| (std::f32::consts::TAU * hum_hz * i as f32 / ANALYSIS_RATE as f32).sin() * 0.2)
                .collect();
            assert_eq!(analyse(&samples), None, "{hum_hz} Hz hum produced a DCS code");
        }
    }

    /// A real word survives the successive-repetition requirement at the
    /// bit-error rate a usable channel has.
    #[test]
    fn word_with_scattered_bit_errors_still_decodes() {
        let mut samples = waveform(0o532, false, ANALYSIS_RATE, 0.7);
        // Flip one bit's worth of samples in each of the two repetitions the
        // detector compares.
        for &at in &[40usize, 250] {
            for x in &mut samples[at..at + 7] {
                *x = -*x;
            }
        }
        let got = analyse(&samples).expect("DCS with two bit errors");
        assert_eq!(got.code, 0o532);
    }
}
