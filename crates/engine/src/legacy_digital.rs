//! Frame-sync qualified detection for digital voice families that do not yet
//! have complete payload/vocoder implementations in scannerd.
//!
//! This is deliberately more than a baud-rate guess and deliberately less
//! than a full decoder. Long sync words can qualify one frame directly. The
//! short M17, YSF, and X2-TDMA words must recur at their specified frame
//! cadence so random 2/4-FSK cannot claim protocol ownership.

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
        protocol: "M17",
        kind: "link setup frame",
        baud: 4800,
        modulation: "4-FSK",
        pattern: "11113313",
        max_errors: 0,
        cadence: Some((192, 3)),
    },
    SyncSpec {
        protocol: "M17",
        kind: "stream frame",
        baud: 4800,
        modulation: "4-FSK",
        pattern: "33331131",
        max_errors: 0,
        cadence: Some((192, 3)),
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
    since_decode: usize,
}

impl LegacyDigitalDetector {
    pub fn new(fs: f64) -> Self {
        Self {
            fs,
            samples: Vec::with_capacity((fs * 0.7) as usize),
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
        decode_window(&self.samples, self.fs)
    }
}

fn decode_window(samples: &[f32], fs: f64) -> Vec<LegacyFrame> {
    let mut out = BTreeMap::<(&'static str, &'static str), LegacyFrame>::new();
    let dc = samples.iter().sum::<f32>() / samples.len().max(1) as f32;
    for baud in [2400u32, 4800, 9600] {
        let sps = fs / f64::from(baud);
        if sps < 2.0 {
            continue;
        }
        for phase in 0..8 {
            let signs = slice_signs(samples, sps, phase as f64 / 8.0, dc);
            for spec in SPECS.iter().filter(|spec| spec.baud == baud) {
                if let Some((inverted, errors, hits)) = find_spec(&signs, spec) {
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

fn slice_signs(samples: &[f32], sps: f64, phase: f64, dc: f32) -> Vec<bool> {
    let mut out = Vec::with_capacity((samples.len() as f64 / sps) as usize);
    let mut center = (phase + 0.5) * sps;
    while center + 0.25 * sps < samples.len() as f64 {
        let start = (center - 0.25 * sps).max(0.0).floor() as usize;
        let end = (center + 0.25 * sps).ceil() as usize;
        let end = end.min(samples.len());
        let mean = samples[start..end].iter().sum::<f32>() / (end - start).max(1) as f32;
        out.push(mean >= dc);
        center += sps;
    }
    out
}

/// Longest sync pattern in `SPECS`, in bits — bounds the packed-pattern array.
const MAX_PATTERN_BITS: usize = 48;

fn find_spec(signs: &[bool], spec: &SyncSpec) -> Option<(bool, usize, usize)> {
    let expected: Vec<bool> = spec.pattern.bytes().map(|b| b == b'1').collect();
    if signs.len() < expected.len() {
        return None;
    }
    // Pack the pattern once; each alignment is then an XOR + popcount over a
    // few words instead of a per-bit zip. The sweep over every start position
    // was the dominant cost of the classifier's 10 Hz window scan.
    let mut packed = [0u64; (MAX_PATTERN_BITS + 63) / 64];
    for (i, &bit) in expected.iter().enumerate() {
        if bit {
            packed[i / 64] |= 1 << (i % 64);
        }
    }
    let n_words = expected.len().div_ceil(64);
    let mut matches = Vec::<(usize, bool, usize)>::new();
    'scan: for start in 0..=signs.len() - expected.len() {
        let mut errors = 0usize;
        // One word at a time, but never past the window: the tail of the
        // last word belongs to whatever follows the candidate alignment.
        for w in 0..n_words {
            let base = start + w * 64;
            let width = (expected.len() - w * 64).min(64);
            let mut chunk = 0u64;
            for b in 0..width {
                if signs[base + b] {
                    chunk |= 1 << b;
                }
            }
            let mut want = packed[w];
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
            if errors > expected.len() && errors > 2 * spec.max_errors + expected.len() / 2 {
                continue 'scan;
            }
        }
        let (errors, inverted) = if errors <= expected.len() - errors {
            (errors, false)
        } else {
            (expected.len() - errors, true)
        };
        if errors <= spec.max_errors {
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
        let got = decode_window(&waveform(pattern, 9600, 1, 80), 48_000.0);
        assert!(got.iter().any(|frame| frame.protocol == "EDACS / ESK"));
    }

    #[test]
    fn one_short_m17_sync_is_not_enough() {
        let one = waveform("33331131", 4800, 1, 192);
        assert!(
            !decode_window(&one, 48_000.0)
                .iter()
                .any(|frame| frame.protocol == "M17")
        );
    }

    #[test]
    fn three_cadenced_m17_syncs_qualify_streaming() {
        let got = decode_window(&waveform("33331131", 4800, 3, 192), 48_000.0);
        let frame = got
            .iter()
            .find(|frame| frame.protocol == "M17")
            .expect("cadenced M17 frames");
        assert_eq!(frame.cadence_hits, 3);
    }

    #[test]
    fn inverted_dstar_sync_is_reported_as_inverted() {
        let pattern = "313131313133131113313111";
        let mut iq = waveform(pattern, 4800, 1, 40);
        for sample in &mut iq {
            *sample = -*sample;
        }
        let got = decode_window(&iq, 48_000.0);
        assert!(
            got.iter()
                .any(|frame| frame.protocol == "D-STAR" && frame.inverted)
        );
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
            let got = decode_window(&waveform(pattern, baud, repeat, cadence), 48_000.0);
            assert!(
                got.iter().any(|frame| frame.protocol == protocol),
                "{protocol} did not qualify: {got:?}"
            );
        }
    }
}
