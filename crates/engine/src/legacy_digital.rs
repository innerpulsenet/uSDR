//! Frame-sync qualified detection for digital voice families that do not yet
//! have complete payload/vocoder implementations in scannerd.
//!
//! This is deliberately more than a baud-rate guess and deliberately less
//! than a full decoder. Long sync words can qualify one frame directly. The
//! short YSF and X2-TDMA words must recur at their specified frame cadence
//! so random 2/4-FSK cannot claim protocol ownership.
//!
//! M17 was removed: its 8-symbol sync words fired as phantom decodes on
//! 4-level paging traffic (the sign slicer maps FLEX's four levels onto two,
//! and an 8-bit pattern recurs at the required cadence far too easily), and
//! no M17 signal was ever present to decode.

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyFrame {
    pub protocol: &'static str,
    pub kind: &'static str,
    pub baud: u32,
    pub modulation: &'static str,
    pub inverted: bool,
    pub sync_errors: usize,
    pub cadence_hits: usize,
}

#[derive(Clone, Copy)]
struct SyncSpec {
    protocol: &'static str,
    kind: &'static str,
    baud: u32,
    modulation: &'static str,
    pattern: &'static str,
    max_errors: usize,
    cadence: Option<(usize, usize)>,
}

const SPECS: &[SyncSpec] = &[
    SyncSpec {
        protocol: "D-STAR",
        kind: "voice sync",
        baud: 4800,
        modulation: "GMSK",
        pattern: "313131313133131113313111",
        max_errors: 0,
        cadence: None,
    },
    SyncSpec {
        protocol: "D-STAR",
        kind: "header sync",
        baud: 4800,
        modulation: "GMSK",
        pattern: "131313131333133113131111",
        max_errors: 0,
        cadence: None,
    },
    SyncSpec {
        protocol: "ProVoice",
        kind: "standard voice sync",
        baud: 9600,
        modulation: "GFSK",
        pattern: "13131333111311311133113311331133",
        max_errors: 1,
        cadence: None,
    },
    SyncSpec {
        protocol: "ProVoice",
        kind: "extended-address voice sync",
        baud: 9600,
        modulation: "GFSK",
        pattern: "31131311331331111133131311311133",
        max_errors: 1,
        cadence: None,
    },
    SyncSpec {
        protocol: "EDACS / ESK",
        kind: "control sync",
        baud: 9600,
        modulation: "GFSK",
        pattern: "313131313131313131313111333133133131313131313131",
        max_errors: 2,
        cadence: None,
    },
    SyncSpec {
        protocol: "X2-TDMA",
        kind: "base-station voice",
        baud: 4800,
        modulation: "4-FSK TDMA",
        pattern: "113131333331313331113311",
        max_errors: 0,
        cadence: Some((144, 2)),
    },
    SyncSpec {
        protocol: "X2-TDMA",
        kind: "base-station data",
        baud: 4800,
        modulation: "4-FSK TDMA",
        pattern: "331313111113131113331133",
        max_errors: 0,
        cadence: Some((144, 2)),
    },
    SyncSpec {
        protocol: "X2-TDMA",
        kind: "mobile data",
        baud: 4800,
        modulation: "4-FSK TDMA",
        pattern: "313113333111111133333313",
        max_errors: 0,
        cadence: Some((144, 2)),
    },
    SyncSpec {
        protocol: "X2-TDMA",
        kind: "mobile voice",
        baud: 4800,
        modulation: "4-FSK TDMA",
        pattern: "131331111333333311111131",
        max_errors: 0,
        cadence: Some((144, 2)),
    },
    SyncSpec {
        protocol: "YSF System Fusion",
        kind: "frame sync",
        baud: 4800,
        modulation: "C4FM",
        pattern: "31111311313113131131",
        max_errors: 0,
        cadence: Some((480, 2)),
    },
    SyncSpec {
        protocol: "dPMR",
        kind: "frame sync 1",
        baud: 2400,
        modulation: "4-FSK",
        pattern: "111333331133131131111313",
        max_errors: 0,
        cadence: None,
    },
    SyncSpec {
        protocol: "dPMR",
        kind: "frame sync 4",
        baud: 2400,
        modulation: "4-FSK",
        pattern: "333111113311313313333131",
        max_errors: 0,
        cadence: None,
    },
];

pub struct LegacyDigitalDetector {
    fs: f64,
    samples: Vec<f32>,
    /// Reused bit-packed scratch for the sliced sign windows.
    words: Vec<u64>,
    since_decode: usize,
}

impl LegacyDigitalDetector {
    pub fn new(fs: f64) -> Self {
        Self {
            fs,
            samples: Vec::with_capacity((fs * 0.7) as usize),
            words: Vec::new(),
            since_decode: 0,
        }
    }

    pub fn reset(&mut self) {
        self.samples.clear();
        self.since_decode = 0;
    }

    /// `carrier` says whether there is anything above the noise floor worth
    /// searching. Sweeping three baud rates across eight phases of a 0.65 s
    /// history is the most expensive thing in the classifier, and on an idle
    /// channel every one of those passes is looking for sync patterns in
    /// noise. History is still collected while quiet, so a signal appearing
    /// mid-window is scanned with its full context on the next pass.
    pub fn process(&mut self, discriminator_hz: &[f32], carrier: bool) -> Vec<LegacyFrame> {
        if discriminator_hz.is_empty() {
            return Vec::new();
        }
        self.samples.extend_from_slice(discriminator_hz);
        self.since_decode = self.since_decode.saturating_add(discriminator_hz.len());
        let history = (self.fs * 0.65).round() as usize;
        if self.samples.len() > history {
            let excess = self.samples.len() - history;
            self.samples.drain(..excess);
        }
        // The cadence-qualified families need a long history, but rescanning
        // that history at USB block cadence would waste a core and make the
        // web UI stutter. Ten inspections per second is still far faster than
        // the shortest qualifying two-frame interval.
        if self.since_decode < (self.fs * 0.10) as usize {
            return Vec::new();
        }
        if !carrier {
            // Keep the cadence clock honest: the window was not examined.
            self.since_decode = 0;
            return Vec::new();
        }
        self.since_decode = 0;
        let mut words = std::mem::take(&mut self.words);
        let frames = decode_window(&self.samples, self.fs, &mut words);
        self.words = words;
        frames
    }
}

fn decode_window(samples: &[f32], fs: f64, words: &mut Vec<u64>) -> Vec<LegacyFrame> {
    let mut out = BTreeMap::<(&'static str, &'static str), LegacyFrame>::new();
    let dc = samples.iter().sum::<f32>() / samples.len().max(1) as f32;
    for baud in [2400u32, 4800, 9600] {
        let sps = fs / f64::from(baud);
        if sps < 2.0 {
            continue;
        }
        for phase in 0..8 {
            let means = slice_means(samples, sps, phase as f64 / 8.0);
            let signs: Vec<bool> = means.iter().map(|&m| m >= dc).collect();
            // Pack the sliced window once per phase: each alignment test is
            // then a shift-and-XOR over pre-packed words instead of a
            // per-position bit loop.
            pack_signs(&signs, words);
            for (spec, packed) in SPECS.iter().zip(packed_specs()) {
                if spec.baud != baud {
                    continue;
                }
                if let Some((inverted, errors, hits)) =
                    find_spec(words, &means, dc, spec, packed)
                {
                    out.entry((spec.protocol, spec.kind))
                        .or_insert(LegacyFrame {
                            protocol: spec.protocol,
                            kind: spec.kind,
                            baud,
                            modulation: spec.modulation,
                            inverted,
                            sync_errors: errors,
                            cadence_hits: hits,
                        });
                }
            }
        }
    }
    out.into_values().collect()
}

/// Mean discriminator value over the middle half of each symbol period.
/// The sign against the window's DC is the sliced bit; the magnitude is what
/// [`eye_open`] judges a candidate sync word by.
fn slice_means(samples: &[f32], sps: f64, phase: f64) -> Vec<f32> {
    let mut out = Vec::with_capacity((samples.len() as f64 / sps) as usize);
    let mut center = (phase + 0.5) * sps;
    while center + 0.25 * sps < samples.len() as f64 {
        let start = (center - 0.25 * sps).max(0.0).floor() as usize;
        let end = (center + 0.25 * sps).ceil() as usize;
        let end = end.min(samples.len());
        let mean = samples[start..end].iter().sum::<f32>() / (end - start).max(1) as f32;
        out.push(mean);
        center += sps;
    }
    out
}

/// The weakest symbol of a candidate sync word relative to the word's mean
/// symbol magnitude. A sync word is a run of full-deviation symbols, so on
/// real FSK every one sits near ±dev and this ratio is high (ISI on the
/// isolated bits of an alternating run costs some, not most, of it). Sliced
/// receiver noise or analog voice can match a 24-bit pattern by chance —
/// often, across eight phases and both polarities of a 0.65 s window — but
/// some of those symbols always sit close to the slicing threshold, and
/// analog voice's envelope varies across the word besides. Below this ratio
/// the bits were a coin toss, not a sync word.
const EYE_OPEN_MIN: f32 = 0.35;

fn eye_open(means: &[f32], dc: f32) -> bool {
    // The word's own two levels, split by the same slicer that matched it,
    // so a DC offset on the discriminator (carrier off-centre, or a window
    // dominated by one polarity) does not skew the judgement.
    let (mut hi_sum, mut hi_n, mut lo_sum, mut lo_n) = (0.0f32, 0usize, 0.0f32, 0usize);
    for &m in means {
        if m >= dc {
            hi_sum += m;
            hi_n += 1;
        } else {
            lo_sum += m;
            lo_n += 1;
        }
    }
    // Every sync word in SPECS has symbols of both signs.
    if hi_n == 0 || lo_n == 0 {
        return false;
    }
    let hi = hi_sum / hi_n as f32;
    let lo = lo_sum / lo_n as f32;
    let mid = (hi + lo) * 0.5;
    let half = (hi - lo) * 0.5;
    if half <= 0.0 {
        return false;
    }
    means.iter().all(|&m| (m - mid).abs() >= EYE_OPEN_MIN * half)
}

/// Longest sync pattern in `SPECS`, in bits — bounds the packed-pattern array.
const MAX_PATTERN_BITS: usize = 48;

/// One spec's pattern pre-packed the way [`find_spec`] compares it: bit `i`
/// of the pattern in bit `i % 64` of word `i / 64`, plus the bit count.
struct PackedPattern {
    words: [u64; (MAX_PATTERN_BITS + 63) / 64],
    bits: usize,
}

/// Every spec's pattern, packed once. `decode_window` runs at 10 Hz over up
/// to eight phases × three bauds; repacking each pattern per call was pure
/// overhead on the sweep.
fn packed_specs() -> &'static [PackedPattern; SPECS.len()] {
    static PACKED: std::sync::OnceLock<[PackedPattern; SPECS.len()]> = std::sync::OnceLock::new();
    PACKED.get_or_init(|| {
        std::array::from_fn(|i| {
            let mut words = [0u64; (MAX_PATTERN_BITS + 63) / 64];
            let mut bits = 0;
            for b in SPECS[i].pattern.bytes() {
                if b == b'1' {
                    words[bits / 64] |= 1 << (bits % 64);
                }
                bits += 1;
            }
            PackedPattern { words, bits }
        })
    })
}

/// Pack a sliced sign window into words with the same bit layout the packed
/// patterns use, first bit in the low bit of word 0.
fn pack_signs(signs: &[bool], words: &mut Vec<u64>) {
    words.clear();
    words.resize(signs.len().div_ceil(64), 0);
    for (i, &bit) in signs.iter().enumerate() {
        if bit {
            words[i / 64] |= 1 << (i % 64);
        }
    }
}

/// Extract `width` observation bits starting at absolute bit `base`,
/// LSB-aligned, from the pre-packed window.
#[inline]
fn extract_bits(words: &[u64], base: usize, width: usize) -> u64 {
    let wi = base / 64;
    let off = base % 64;
    let mut chunk = words[wi] >> off;
    if off > 0 && wi + 1 < words.len() {
        chunk |= words[wi + 1] << (64 - off);
    }
    if width < 64 {
        chunk &= (1u64 << width) - 1;
    }
    chunk
}

fn find_spec(
    signs: &[u64],
    means: &[f32],
    dc: f32,
    spec: &SyncSpec,
    packed: &PackedPattern,
) -> Option<(bool, usize, usize)> {
    let expected_len = packed.bits;
    let n_signs = means.len();
    if n_signs < expected_len {
        return None;
    }
    let n_words = expected_len.div_ceil(64);
    let mut matches = Vec::<(usize, bool, usize)>::new();
    'scan: for start in 0..=n_signs - expected_len {
        let mut errors = 0usize;
        // One word at a time, but never past the window: the tail of the last
        // word belongs to whatever follows the candidate alignment.
        for w in 0..n_words {
            let base = start + w * 64;
            let width = (expected_len - w * 64).min(64);
            let chunk = extract_bits(signs, base, width);
            let mut want = packed.words[w];
            // Zero the pattern bits above the window's last word so the
            // XOR counts only the alignment's own bits.
            if w == n_words - 1 && width < 64 {
                want &= (1u64 << width) - 1;
            }
            errors += (chunk ^ want).count_ones() as usize;
            // Early abort only when the alignment cannot pass even with
            // every remaining bit flipping to agree (inverted polarity
            // gives `L - errors`); max_errors*2 covers both polarities'
            // budgets plus the parity flip the caller may still accept.
            if errors > expected_len && errors > 2 * spec.max_errors + expected_len / 2 {
                continue 'scan;
            }
        }
        let (errors, inverted) = if errors <= expected_len - errors {
            (errors, false)
        } else {
            (expected_len - errors, true)
        };
        if errors <= spec.max_errors && eye_open(&means[start..start + expected_len], dc) {
            matches.push((start, inverted, errors));
        }
    }
    let Some((cadence, required)) = spec.cadence else {
        return matches
            .into_iter()
            .min_by_key(|(_, _, errors)| *errors)
            .map(|(_, inverted, errors)| (inverted, errors, 1));
    };
    for &(start, inverted, errors) in &matches {
        let mut hits = 1;
        let mut at = start;
        while hits < required {
            at += cadence;
            if matches
                .iter()
                .any(|&(candidate, inv, _)| candidate.abs_diff(at) <= 1 && inv == inverted)
            {
                hits += 1;
            } else {
                break;
            }
        }
        if hits >= required {
            return Some((inverted, errors, hits));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waveform(pattern: &str, baud: u32, repeat: usize, cadence: usize) -> Vec<f32> {
        let sps = 48_000 / baud as usize;
        let mut signs = vec![false; cadence * repeat];
        for frame in 0..repeat {
            for (i, bit) in pattern.bytes().enumerate() {
                signs[frame * cadence + i] = bit == b'1';
            }
        }
        signs
            .into_iter()
            .flat_map(|positive| vec![if positive { 1800.0 } else { -1800.0 }; sps])
            .collect()
    }

    #[test]
    fn long_edacs_sync_qualifies_a_control_frame() {
        let pattern = "313131313131313131313111333133133131313131313131";
        let got = decode_window(&waveform(pattern, 9600, 1, 80), 48_000.0, &mut Vec::new());
        assert!(got.iter().any(|frame| frame.protocol == "EDACS / ESK"));
    }

    /// The ghost that got M17 removed: a 4-level paging-shaped waveform must
    /// not qualify ANY family, let alone one that is not on the air. The
    /// pattern/cadence pair below is what FLEX traffic happily produced.
    #[test]
    fn four_level_paging_shaped_noise_claims_no_family() {
        let cadence_stream = waveform("33331131", 4800, 3, 192);
        let got = decode_window(&cadence_stream, 48_000.0, &mut Vec::new());
        assert!(
            got.iter().all(|frame| frame.protocol != "M17"),
            "M17 must stay removed: {got:?}"
        );
    }

    #[test]
    fn inverted_dstar_sync_is_reported_as_inverted() {
        let pattern = "313131313133131113313111";
        let mut iq = waveform(pattern, 4800, 1, 40);
        for sample in &mut iq {
            *sample = -*sample;
        }
        let got = decode_window(&iq, 48_000.0, &mut Vec::new());
        assert!(
            got.iter()
                .any(|frame| frame.protocol == "D-STAR" && frame.inverted)
        );
    }

    /// Deterministic LCG so the noise fixtures are the same on every run.
    fn lcg(seed: &mut u64) -> f32 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*seed >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
    }

    /// Approximately Gaussian via the sum of twelve uniforms.
    fn gauss(seed: &mut u64) -> f32 {
        (0..12).map(|_| lcg(seed)).sum::<f32>() / 2.0
    }

    /// Run the detector the way the classifier does — 20 ms blocks with the
    /// carrier gate open — over `secs` of discriminator output, and return
    /// every family it claimed.
    fn run_detector(disc: &[f32], fs: f64) -> Vec<LegacyFrame> {
        let mut det = LegacyDigitalDetector::new(fs);
        let block = (fs * 0.02) as usize;
        let mut got = Vec::new();
        for chunk in disc.chunks(block) {
            got.extend(det.process(chunk, true));
        }
        got
    }

    /// Receiver noise sliced at three bauds across eight phases for a long
    /// time must not claim any family. 24-bit words with zero error budget
    /// and no cadence requirement did — about once every half minute on an
    /// idle channel, published as "D-STAR voice sync" at 0.94 confidence.
    #[test]
    fn discriminator_noise_claims_no_family() {
        let fs = 48_000.0;
        let mut seed = 0x5eed_1234_u64;
        let disc: Vec<f32> = (0..(fs as usize * 120)).map(|_| gauss(&mut seed) * 2_500.0).collect();
        let got = run_detector(&disc, fs);
        assert!(got.is_empty(), "noise qualified a family: {got:?}");
    }

    /// Analog voice — a handful of tones wandering in frequency and level,
    /// as a discriminator sees NBFM speech — must not qualify a family either.
    /// A 2.4 kHz component slices to the 1010… run that opens the D-STAR
    /// sync word, and a NOAA weather broadcast was labelled D-STAR by it.
    #[test]
    fn analog_voice_claims_no_family() {
        let fs = 48_000.0;
        let mut seed = 0x0f0f_1ce5_u64;
        let n = fs as usize * 120;
        let mut disc = Vec::with_capacity(n);
        let mut phases = [0.0f32; 4];
        let mut freqs = [300.0f32, 900.0, 1700.0, 2400.0];
        let mut amps = [1.0f32, 0.7, 0.5, 0.4];
        for i in 0..n {
            if i % 480 == 0 {
                // Every 10 ms the formants drift and the level breathes.
                for k in 0..4 {
                    freqs[k] = (freqs[k] + lcg(&mut seed) * 60.0).clamp(150.0, 3_000.0);
                    amps[k] = (amps[k] + lcg(&mut seed) * 0.15).clamp(0.05, 1.2);
                }
            }
            let mut v = 0.0f32;
            for k in 0..4 {
                phases[k] += std::f32::consts::TAU * freqs[k] / fs as f32;
                v += amps[k] * phases[k].sin();
            }
            // Syllabic envelope plus a little receiver noise.
            let env = 0.55 + 0.45 * (i as f32 / fs as f32 * 3.7).sin();
            disc.push(v * env * 1_500.0 + gauss(&mut seed) * 200.0);
        }
        let got = run_detector(&disc, fs);
        assert!(got.is_empty(), "analog voice qualified a family: {got:?}");
    }

    #[test]
    fn every_requested_basic_family_has_frame_evidence() {
        let cases = [
            ("ProVoice", "13131333111311311133113311331133", 9600, 1, 80),
            ("dPMR", "111333331133131131111313", 2400, 1, 80),
            ("YSF System Fusion", "31111311313113131131", 4800, 2, 480),
            ("X2-TDMA", "113131333331313331113311", 4800, 2, 144),
        ];
        for (protocol, pattern, baud, repeat, cadence) in cases {
            let got = decode_window(&waveform(pattern, baud, repeat, cadence), 48_000.0, &mut Vec::new());
            assert!(
                got.iter().any(|frame| frame.protocol == protocol),
                "{protocol} did not qualify: {got:?}"
            );
        }
    }
}
