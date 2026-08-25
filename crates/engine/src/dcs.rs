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

    let words: Vec<(u16, u32)> = code_table()
        .iter()
        .copied()
        .collect();
    let mut best: Option<(usize, u32, Detection)> = None;

    // Quarter-sample phase steps are cheap at this rate and tolerate the
    // fractional 7.4405 samples/bit without a separate clock-recovery loop.
    let phase_steps = (SAMPLES_PER_BIT * 4.0).ceil() as usize;
    for phase_step in 0..phase_steps {
        let phase = phase_step as f64 / 4.0;
        let mut bits = Vec::new();
        let mut at = phase;
        while (at.round() as usize) < samples.len() {
            bits.push(samples[at.round() as usize] >= mean);
            at += SAMPLES_PER_BIT;
        }
        if bits.len() < 46 {
            continue;
        }

        for &(code, target) in &words {
            // Normal and inverted DCS names are cyclic/complement aliases (for
            // example 532-N is the same waveform as 343-I). Configuration in
            // scannerd uses the conventional normal form, so report the normal
            // code instead of returning an arbitrary equivalent polarity.
            let inverted = false;
            let mut matches = 0usize;
            let mut errors = 0u32;
            for start in 0..=bits.len() - 23 {
                let mut received = 0u32;
                for &bit in &bits[start..start + 23] {
                    received = (received << 1) | u32::from(bit);
                }
                let distance = (received ^ target).count_ones();
                if distance <= MAX_ERRORS {
                    matches += 1;
                    errors += distance;
                }
            }
            if matches < 2 {
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

/// code → transmitted word, computed once. The encode is a per-code Golay
/// pass that used to rerun on every analyse window (~30×/s while active).
fn code_table() -> &'static [(u16, u32)] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<Vec<(u16, u32)>> = OnceLock::new();
    TABLE.get_or_init(|| CODES.iter().map(|&c| (c, transmitted_word(c))).collect())
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
}
