//! Morse (CW) for the whole sideband at once, the way a skimmer reads a
//! band: every keyed carrier in the passband is found, given a channel of
//! its own, and copied side by side, each at its own speed and fist.
//!
//! The audio is cut to about 8 kHz once, after a blanker has zeroed the
//! few samples around any impulse far above the recent signal level (a
//! static crash would otherwise read as a dit on every channel at once).
//! That one stream feeds the detector and every channel. The detector
//! keeps an averaged spectrum in 8 Hz bins and looks for narrow peaks over
//! a noise floor taken piecewise across the band; a keyed carrier leaves a
//! line at its pitch with its keying sidebands either side, and a peak far
//! weaker than a neighbor within 150 Hz is taken for that neighbor's
//! sidebands. A new channel starts on the last three seconds of audio, so
//! it has the opening of the transmission that got it noticed.
//!
//! Each channel mixes its tone to 0 Hz, filters it to about 500 samples a
//! second, and integrates over a window about four fifths of a dit long:
//! the matched filter for a keyed carrier, re-sized to the channel's speed
//! between words. The magnitude out of it is the key envelope. A mark is
//! seen when the envelope rises part of the way from the noise (a low
//! percentile of the envelope on stretches well clear of any key-down) to
//! the mark level (the settled middle of recent marks), with a floor a
//! little above the noise. Once a mark ends, both its edges are re-timed
//! where the envelope crossed halfway between the noise and that mark's
//! own level, found in the envelope's recent history: so how long a mark
//! was does not depend on how far the threshold lagged a fade, which on
//! HF can take 10 dB out of a signal within a word. The tone's drift is
//! followed from how the filtered carrier turns during marks.
//!
//! Timing is learned, not assumed: the key-down times of the last forty
//! marks are split into two clusters (dits and dahs) and the key-up times
//! into two more (gaps within a character and between characters), all on
//! a log scale, so a heavy or light fist, a 2.5:1 or 4:1 dah, and any
//! speed from about 8 to 50 WPM read alike. A word is decoded once a gap
//! longer than a character space by a margin has closed it, with the
//! timing as it stands then, the word's own elements included. Until a
//! channel has seen enough to tell dits from dahs its words are held,
//! never guessed.
//!
//! A very strong station is heard beside its pitch too: its keying
//! sidebands within a few tens of hertz, and the clicks of its edges much
//! further out. Before a channel learns from a word or reads it, the word
//! is checked against each stronger neighbor alone: a word whose marks
//! start and end with the neighbor's, or sit on its edges, both ways round,
//! is the neighbor and is dropped, as is a run of marks on its edges
//! inside a word that is the channel's own.
//!
//! A channel prints nothing until it has shown that it hears Morse. On a
//! real band most of what crosses a threshold is not: carriers, data
//! tones, splatter and noise all key on and off, and read as Morse they
//! give 60 to 100 WPM runs of E, T and I. So a channel holds its words
//! until its keying has two kinds of mark near 1:3 that stay put, gaps
//! that cluster the same way, a speed between 6 and 55 WPM, two seconds
//! of it, and a reading with characters of several elements, few of them
//! E or T; then it prints what it held and carries on. Once proven, short
//! words ("R", "TU", "5NN") print like any other. Thirty seconds without
//! a word and it has to show it again.
//!
//! What is printed is only what was read: a group of elements that is not
//! in the Morse table prints as `*`, a word mostly made of those is not
//! printed at all, nor is one whose marks stood too little above the
//! noise. Text comes out a word at a time per channel.

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use scannerd_dsp::{DecimFir, Nco};
use std::collections::VecDeque;
use std::f32::consts::PI;
use std::sync::Arc;

/// Something the decoder has to say.
#[derive(Debug, Clone, PartialEq)]
pub enum CwEvent {
    /// One word from one channel. Consecutive words from a channel are
    /// read with a space between them.
    Text {
        channel: u32,
        /// The tone's pitch in the audio, Hz, as tracked when the word
        /// ended.
        audio_hz: f32,
        /// Character speed, from the channel's timing.
        wpm: f32,
        /// Key-down signal to noise in 500 Hz, dB.
        snr_db: f32,
        text: String,
    },
}

impl CwEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            CwEvent::Text { .. } => "text",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            CwEvent::Text { channel, audio_hz, wpm, text, .. } => {
                format!("ch {channel} {audio_hz:.0} Hz {wpm:.0} wpm: {text}")
            }
        }
    }

    pub fn fields(&self) -> Vec<(&'static str, String)> {
        match self {
            CwEvent::Text { channel, audio_hz, wpm, snr_db, text } => vec![
                ("channel", channel.to_string()),
                ("df", format!("{audio_hz:.1}")),
                ("wpm", format!("{wpm:.0}")),
                ("snr", format!("{snr_db:.0}")),
                ("text", text.clone()),
            ],
        }
    }
}

/// One tracked signal.
#[derive(Debug, Clone, PartialEq)]
pub struct CwChannel {
    pub id: u32,
    pub audio_hz: f32,
    /// Key-down signal to noise in 500 Hz, dB: measured on the channel's
    /// marks once it has some, from the averaged spectrum before that.
    pub snr_db: f32,
    /// Character speed, once the channel has learned its timing.
    pub wpm: Option<f32>,
    /// Printing: a word came out of this channel in the last few seconds.
    pub active: bool,
    /// The channel the UI asked to favor.
    pub focused: bool,
    /// Characters printed since the channel started.
    pub chars: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CwStatus {
    /// Some channel is printing.
    pub locked: bool,
    /// The focused channel's pitch, or else the strongest printing one's.
    pub center_hz: Option<f32>,
    pub snr_db: Option<f32>,
    pub wpm: Option<f32>,
    /// Channels tracked, and of those, printing.
    pub channels: usize,
    pub active: usize,
    /// Characters printed since the decoder was made.
    pub chars: u64,
}

// ---------------------------------------------------------------------------
// Rates and the front end.

/// The shared stream's rate is the input rate divided by a whole number,
/// the largest that keeps it at or above this. Room for a band up to 3 kHz
/// and the filter's transition above it.
const FRONT_MIN_RATE: f32 = 7_000.0;
/// The top of the audio the shared filter keeps clean of aliases.
const FRONT_PASS_HZ: f32 = 3_150.0;
/// Each channel's rate, near enough: 2 ms steps, a twelfth of a dit at
/// 50 WPM, with the crossing times interpolated between them.
const CH_RATE: f32 = 500.0;
/// The channel filter is flat to about 60 Hz either side of the tone,
/// enough for the keying sidebands at 50 WPM, and 90 dB down past 140 Hz.
const CH_CUTOFF_HZ: f32 = 100.0;
const CH_TRANSITION_HZ: f32 = 80.0;

/// Blanker: an input sample this many times the recent signal power (an
/// upper percentile of it, about the peak of the strongest carrier) is an
/// impulse, and it and its neighbors are zeroed...
const BLANK_K2: f32 = 16.0;
/// ...this long before and after. The delay line holds the before.
const BLANK_PRE_S: f32 = 0.5e-3;
const BLANK_POST_S: f32 = 1.0e-3;
/// A burst over the threshold for longer than this is a signal, not an
/// impulse, and the level jumps to it.
const BLANK_MAX_S: f32 = 4e-3;
/// Over-threshold samples closer than this belong to one burst: a carrier's
/// power dips to zero twice a cycle.
const BLANK_JOIN_S: f32 = 1.5e-3;
/// The level rises quickly and falls slowly, so it holds near the peak of
/// a keyed carrier through the gaps between its words.
const BLANK_UP_S: f32 = 0.1;
const BLANK_DOWN_S: f32 = 5.0;

// ---------------------------------------------------------------------------
// The detector.

/// Averaged spectrum: an exponential average of Hann-windowed frames over
/// this many seconds, long enough to flatten the noise, short enough to
/// see a new station within a second.
const SPEC_TAU_S: f32 = 1.5;
/// Frames advance by a quarter of their length.
const SPEC_OVERLAP: usize = 4;
/// The detector runs once every this many frames (about 130 ms).
const DETECT_EVERY: u32 = 4;
/// Frames averaged before the first detection.
const MIN_FRAMES: u32 = 16;
/// The noise floor is measured over pieces of the band this wide, so a
/// sideband filter's slope does not read as signal.
const FLOOR_SEG_HZ: f32 = 400.0;
/// A peak's excess over the floor (power over noise, less one, smoothed
/// across three bins) must reach this to be a candidate: about -9 dB in
/// 500 Hz for a carrier keyed half the time, below what copies, and six
/// times the spread of the noise's average.
const DETECT_SCORE: f32 = 0.9;
/// Keying sidebands: a peak within [`SIDELOBE_HZ`] of a stronger one, and
/// weaker than it by more than [`SIDELOBE_DB`] plus [`SIDELOBE_DB_PER_HZ`]
/// for each hertz between them, is taken for that one's sidebands. Random
/// keying at a dit of T spreads as sinc² of fT, about 15 dB down 20 Hz out
/// at 40 WPM and 30 dB down 100 Hz out at 25, and the 5 ms edges a clean
/// transmitter shapes its keying with take it further down beyond that. A
/// channel started on a sideband all the same finds out from its keying
/// (see [`GHOST_SHARE`]).
const SIDELOBE_HZ: f32 = 150.0;
const SIDELOBE_DB: f32 = 4.0;
const SIDELOBE_DB_PER_HZ: f32 = 0.2;
/// Two peaks closer than this are one signal.
const MIN_SEP_HZ: f32 = 20.0;
/// A candidate this close to a channel is that channel's signal.
const MATCH_HZ: f32 = 12.0;
/// Two channels this close are on one signal; the one that printed less
/// goes.
const DUP_HZ: f32 = 9.0;
/// The carrier loop's reach either side of where the detector put the
/// signal.
const AFC_RANGE_HZ: f32 = 15.0;

/// Audio kept so a new channel can start from before it was detected.
const REPLAY_S: f32 = 3.0;
/// Nothing printed for this long and not seen by the detector for
/// [`RETIRE_UNSEEN_S`]: the channel is dropped.
const RETIRE_IDLE_S: f32 = 20.0;
const RETIRE_UNSEEN_S: f32 = 8.0;
/// Nothing printed for this long: dropped even if something is still there.
const RETIRE_HARD_S: f32 = 90.0;
/// A channel this idle may give its place to a stronger newcomer.
const EVICT_IDLE_S: f32 = 10.0;
/// A channel counts as printing for this long after a word.
const ACTIVE_S: f32 = 6.0;

// ---------------------------------------------------------------------------
// The channel.

/// Samples of the decimated stream kept for the matched filter; a power
/// of two above the longest window.
const ZRING: usize = 128;
const ZMASK: usize = ZRING - 1;
/// Magnitudes kept for re-timing a mark's edges: a second, longer than a
/// dah at 8 WPM.
const PRING: usize = 512;
const PMASK: usize = PRING - 1;
/// The matched filter is this share of a dit long: long enough for most
/// of the SNR a dit-long filter gives, short enough that a gap down to
/// about 0.4 dit still opens below the threshold.
const MF_FRACTION: f32 = 0.8;
/// Its length before the speed is known (a dit at 48 WPM, so nothing is
/// smeared), and the bounds on it.
const MF_START_S: f32 = 0.020;
const MF_MIN_S: f32 = 0.016;
const MF_MAX_S: f32 = 0.120;
/// Re-sized only when the speed calls for a length this different.
const MF_RESIZE: f32 = 0.15;
/// Outputs gathered before the first threshold: their 25th percentile
/// gives the noise, their 97th the mark level.
const INIT_S: f32 = 1.5;
/// A mark is seen when the magnitude rises this share of the way from the
/// noise to the mark level, once the level is twice [`THR_WEAK`] times
/// the noise (halfway below [`THR_WEAK`], where noise rather than fading
/// decides what is missed). Below half, so a mark weaker than the last
/// few (a fade setting in) is still seen; its length does not depend on
/// this, since both its edges are then re-timed against its own level.
const THR_FRACTION: f32 = 0.4;
const THR_WEAK: f32 = 5.0;
/// Threshold hysteresis either side.
const HYST: f32 = 0.08;
/// The threshold never drops below this many times the noise's mean
/// magnitude: a Rayleigh variable exceeds it a few percent of the time,
/// and a fading signal is better lost than noise printed.
const THR_FLOOR: f32 = 2.0;
/// Noise: the 25th percentile of the power on key-up samples well clear
/// of any mark, tracked over about this long. A percentile, not a mean,
/// because a signal faded under the threshold reads as key-up too, and
/// must not raise the noise and so the threshold after it.
const NOISE_TAU_S: f32 = 2.0;
/// The 25th percentile of an exponential variable, as a share of its mean.
const P25_OF_MEAN: f32 = 0.2877;
/// Mark level: a stronger mark pulls it this far, a weaker one this far.
const LEVEL_UP: f32 = 0.6;
const LEVEL_DOWN: f32 = 0.35;
/// After a key-up longer than a character space the mark level decays
/// with this time constant, so the threshold finds a signal that came
/// back weaker after a fade.
const LEVEL_DECAY_S: f32 = 2.0;
/// Key-up long enough to be between words at any speed: decoding starts
/// after the first one, so a channel that starts mid-word does not print
/// the word's tail as other characters.
const SYNC_GAP_S: f32 = 0.2;
/// A mark shorter than this share of a dit is a glitch: noise across the
/// threshold, or a click. Fists vary, but a dit is seldom under 0.8 of the
/// fist's own.
const GLITCH_MARK: f32 = 0.5;
/// A gap shorter than this share of the element gap (or of a dit, if
/// shorter) is a fade's dip in a mark. A heavy fist's element gaps run
/// down to half a dit.
const GLITCH: f32 = 0.35;
const GLITCH_ABS_S: f32 = 0.008;
/// A mark under this share of the median amplitude SNR of its word and
/// under this amplitude SNR is noise that crossed the threshold beside a
/// weak signal's marks; under this lower amplitude SNR, beside anything.
/// The share is generous because a fast fade can take a word's last marks
/// 8 dB under its first.
const BLIP_SHARE: f32 = 0.6;
const BLIP_R: f32 = 2.6;
const BLIP_R_ANY: f32 = 2.0;
/// Durations remembered for the timing clusters.
const TIMING_MEMORY: usize = 40;
/// ... and kept across a pause this long, when the other station of a QSO
/// may take over at another speed.
const TIMING_PAUSE_S: f32 = 2.0;
const TIMING_KEEP: usize = 12;
/// Two clusters of durations this far apart (ratio) are two kinds of
/// element; closer, one.
const CLUSTER_SEP: f32 = 1.8;
/// Plausible dits: 80 to 5 WPM, wide of the 8 to 50 the decoder is built
/// for. Timing outside it is noise and is not learned.
const DIT_MIN_S: f32 = 0.015;
const DIT_MAX_S: f32 = 0.24;
/// Words held while the timing is unknown or the channel has yet to show
/// it hears Morse: at most this many, from at most this far back.
const HELD_WORDS: usize = 24;
const HELD_S: f32 = 20.0;
/// Speeds a channel may print at. The decoder is built for 8 to 50; a
/// little either side is allowed for a fist's own reckoning, and beyond
/// that what reads as Morse is noise or a data mode's tones.
const WPM_MIN: f32 = 6.0;
const WPM_MAX: f32 = 55.0;
/// What a channel must show before it prints (see [`Channel::prove`]):
/// dahs this many dits long, element gaps this many dits, character gaps
/// this many element gaps...
const PROVE_DAH_RATIO: (f32, f32) = (2.0, 4.5);
const PROVE_EGAP_RATIO: (f32, f32) = (0.33, 2.5);
const PROVE_CGAP_RATIO: f32 = 1.8;
/// ...at least this many of each kind of mark among the recent ones, and
/// this share of them within this factor of their kind's typical length
/// (a fist's ±15% and the noise on the edges stay well inside it)...
const PROVE_EACH: usize = 3;
const PROVE_TIGHT: f32 = 0.85;
const PROVE_SPREAD: f32 = 1.45;
/// ...kept up over this many seconds of held words, reading as at least
/// this many characters, no more than this share of them E or T (plain
/// text and calls run about a fifth; noise read as Morse, most) or not in
/// the table, and this many elements a character on average (about three
/// in text and calls, nearer one in noise).
const PROVE_S: f32 = 2.0;
const PROVE_CHARS: u64 = 5;
const PROVE_SINGLE: f32 = 0.45;
const PROVE_BAD: f32 = 0.15;
const PROVE_ELEMS: f32 = 2.2;
/// A channel that has printed nothing for this long proves itself again.
const PROVEN_IDLE_S: f32 = 30.0;
/// A word with more elements than this is closed anyway.
const WORD_MAX_ELEMS: usize = 80;
/// A word is printed only if its marks averaged this amplitude SNR (signal
/// over noise RMS, after the matched filter): about -3 dB in 500 Hz at
/// 20 WPM. Noise crossing the threshold floor reads about 2.3.
const WORD_MIN_R: f32 = 3.3;
/// The carrier loop: corrections from marks at least this strong, at this
/// share of the measured error.
const AFC_MIN_R: f32 = 4.0;
const AFC_GAIN: f32 = 0.3;

/// Each channel's marks of about the last 8 seconds, re-timed, in
/// absolute channel-rate samples, for telling a station from a stronger
/// neighbor's leakage.
const RECENT_SPAN: u64 = 4000;
/// Re-timed edges of one keying seen through two channels agree to this
/// many channel samples.
const LEAK_TOL: u64 = 4;
/// A word is a neighbor this much stronger (amplitude) and this close,
/// heard through the channel filter, when this share of its marks match
/// the neighbor's keying and this share of the neighbor's keying over the
/// word is matched by its marks: either its marks start and end with the
/// neighbor's (the marks leaking through), or they sit on the neighbor's
/// edges (the clicks of a hard-keyed or very strong signal). Asking both
/// ways is what keeps a real station, whose marks often overlap a busy
/// neighbor's by chance, from matching. The neighbor's keying is compared
/// alone, never several neighbors' together.
const GHOST_SHARE: f32 = 0.8;
const GHOST_RATIO: f32 = 4.0;
const GHOST_HZ: f32 = 400.0;
/// Closer than this, inside the main lobe of a fast station's keying
/// spectrum, a neighbor leaks through at much more of its level, and only
/// this much stronger is enough.
const GHOST_NEAR_HZ: f32 = 60.0;
const GHOST_NEAR_RATIO: f32 = 1.5;
/// A mark this much weaker than the median of its word, on an edge of a
/// much stronger neighbor, is that neighbor's key click; so is every mark
/// of a run this long of marks on its edges.
const CLICK_SHARE: f32 = 0.5;
const CLICK_RUN: usize = 3;
/// ...and so is one on its edge shorter than this share of the channel's
/// dit, or before the timing is known, this many matched-filter lengths.
const CLICK_DIT: f32 = 0.6;
const CLICK_LEN: f32 = 1.3;
/// Where a channel that only ever heard a neighbor was, no channel is
/// started again for this long.
const GHOST_BLOCK_S: f32 = 30.0;

// ---------------------------------------------------------------------------
// The Morse table.

/// ITU Morse with the common prosigns. A prosign that doubles as
/// punctuation (AR is +, BT is =, KN is an open parenthesis, AS is &)
/// prints as the prosign: on the air that is what it means.
#[rustfmt::skip]
const MORSE: &[(&str, &str)] = &[
    (".-", "A"), ("-...", "B"), ("-.-.", "C"), ("-..", "D"), (".", "E"), ("..-.", "F"), ("--.", "G"),
    ("....", "H"), ("..", "I"), (".---", "J"), ("-.-", "K"), (".-..", "L"), ("--", "M"), ("-.", "N"),
    ("---", "O"), (".--.", "P"), ("--.-", "Q"), (".-.", "R"), ("...", "S"), ("-", "T"), ("..-", "U"),
    ("...-", "V"), (".--", "W"), ("-..-", "X"), ("-.--", "Y"), ("--..", "Z"),
    ("-----", "0"), (".----", "1"), ("..---", "2"), ("...--", "3"), ("....-", "4"), (".....", "5"),
    ("-....", "6"), ("--...", "7"), ("---..", "8"), ("----.", "9"),
    (".-.-.-", "."), ("--..--", ","), ("..--..", "?"), (".----.", "'"), ("-.-.--", "!"), ("-..-.", "/"),
    ("-.--.-", ")"), ("---...", ":"), ("-.-.-.", ";"), ("-....-", "-"), ("..--.-", "_"), (".-..-.", "\""),
    ("...-..-", "$"), (".--.-.", "@"),
    (".-.-.", "<AR>"), ("...-.-", "<SK>"), ("-...-", "<BT>"), ("-.--.", "<KN>"), (".-...", "<AS>"),
    ("...-.", "<SN>"), ("-.-.-", "<KA>"), ("........", "<HH>"), ("...---...", "<SOS>"),
];

/// The longest group in the table.
const MAX_ELEMS: usize = 9;

/// A group as a number: a leading 1, then a bit per element, 1 for a dah.
fn code_of(pattern: &str) -> usize {
    pattern.bytes().fold(1usize, |c, b| (c << 1) | usize::from(b == b'-'))
}

/// Text for every group of up to [`MAX_ELEMS`] elements, empty where the
/// group is not Morse.
fn morse_table() -> Vec<&'static str> {
    let mut t = vec![""; 2 << MAX_ELEMS];
    for &(p, s) in MORSE {
        t[code_of(p)] = s;
    }
    t
}

// ---------------------------------------------------------------------------
// Timing.

/// A fist, learned: typical key-down and key-up times, seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Timing {
    dit: f32,
    dah: f32,
    /// The gap between elements of a character.
    egap: f32,
    /// The gap between characters.
    cgap: f32,
}

impl Timing {
    /// The Morse unit. A fist's weighting lengthens the marks by what it
    /// takes from the gaps, so a dit and an element gap together are two
    /// units whatever the weighting, and however long the dahs.
    fn unit(&self) -> f32 {
        0.5 * (self.dit + self.egap)
    }

    fn wpm(&self) -> f32 {
        1.2 / self.unit()
    }

    /// Dit or dah: the geometric middle of the two clusters.
    fn mark_cut(&self) -> f32 {
        (self.dit * self.dah).sqrt()
    }

    /// Element gap or character gap, the same way.
    fn char_cut(&self) -> f32 {
        (self.egap * self.cgap).sqrt()
    }

    /// Character gap or word gap: a word gap is seven units to the
    /// character gap's three, and slow senders stretch both.
    fn word_cut(&self) -> f32 {
        (5.0 * self.unit()).max(1.65 * self.cgap)
    }
}

/// Two clusters of `v` (log durations) by k-means, from `init`: their
/// centers and sizes.
fn two_means(v: &[f32], init: (f32, f32)) -> (f32, f32, usize, usize) {
    let (mut lo, mut hi) = if init.0 <= init.1 { init } else { (init.1, init.0) };
    let mut n = (0, 0);
    for _ in 0..12 {
        let cut = 0.5 * (lo + hi);
        let (mut s0, mut n0, mut s1, mut n1) = (0.0f32, 0usize, 0.0f32, 0usize);
        for &x in v {
            if x < cut {
                s0 += x;
                n0 += 1;
            } else {
                s1 += x;
                n1 += 1;
            }
        }
        let nlo = if n0 > 0 { s0 / n0 as f32 } else { lo };
        let nhi = if n1 > 0 { s1 / n1 as f32 } else { hi };
        n = (n0, n1);
        if nlo == lo && nhi == hi {
            break;
        }
        (lo, hi) = (nlo, nhi);
    }
    (lo, hi, n.0, n.1)
}

fn log_mean(v: &[f32]) -> f32 {
    v.iter().sum::<f32>() / v.len().max(1) as f32
}

fn extent(v: &[f32]) -> (f32, f32) {
    v.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), &x| (a.min(x), b.max(x)))
}

/// The timing that best explains the recent marks and gaps (seconds),
/// starting from what was known before. `None` while there is no telling
/// dits from dahs: a word of single-element characters, or of characters
/// whose gaps are as long as their marks, could be either.
fn estimate(prev: Option<Timing>, marks: &VecDeque<f32>, gaps: &VecDeque<f32>) -> Option<Timing> {
    if marks.is_empty() {
        return prev;
    }
    let lm: Vec<f32> = marks.iter().map(|v| v.max(1e-3).ln()).collect();
    let lg: Vec<f32> = gaps.iter().map(|v| v.max(1e-3).ln()).collect();
    let sep = CLUSTER_SEP.ln();
    let ln3 = 3f32.ln();
    let init = prev.map_or_else(|| extent(&lm), |t| (t.dit.ln(), t.dah.ln()));
    let (lo, hi, nlo, nhi) = two_means(&lm, init);
    let (dit, dah) = if nlo > 0 && nhi > 0 && hi - lo >= sep {
        (lo.exp(), hi.exp())
    } else {
        // One kind of mark only. What it is follows from what came before,
        // or else from the gaps beside it.
        let m = log_mean(&lm);
        match prev {
            Some(t) => {
                if (m - t.dit.ln()).abs() <= (m - t.dah.ln()).abs() {
                    (m.exp(), m.exp() * t.dah / t.dit)
                } else {
                    (m.exp() * t.dit / t.dah, m.exp())
                }
            }
            None => {
                if lg.is_empty() {
                    return None;
                }
                let (glo, ghi, ng0, ng1) = two_means(&lg, extent(&lg));
                let egap = if ng0 > 0 && ng1 > 0 && ghi - glo >= sep {
                    glo
                } else {
                    let s = log_mean(&lg);
                    if m > s + 0.5 * ln3 {
                        // Marks longer than every gap: dahs and element
                        // gaps (an O, an M, a 0).
                        s
                    } else if m < s - 0.5 * ln3 {
                        // Gaps longer than every mark: dits between
                        // characters (E E E).
                        m
                    } else {
                        return None;
                    }
                };
                if m < egap + 0.5 * ln3 { (m.exp(), 3.0 * m.exp()) } else { (m.exp() / 3.0, m.exp()) }
            }
        }
    };
    if !(DIT_MIN_S..=DIT_MAX_S).contains(&dit) {
        return prev;
    }
    // The gaps: within a character or between characters, split first
    // where a 1:3 fist at the marks' speed would put the line and then
    // where their own clusters do. Anything as long as a word gap (two
    // words read as one before the timing was known) is left out.
    let um = 0.5 * (dit + dah / 3.0);
    let top = (6.0 * um).ln();
    let mut cut = (3f32.sqrt() * um).ln();
    let (mut egap, mut cgap) = prev.map_or((dit, 3.0 * um), |t| (t.egap, t.cgap));
    for _ in 0..4 {
        let (mut se, mut ne, mut sc, mut nc) = (0.0f32, 0u32, 0.0f32, 0u32);
        for &g in &lg {
            if g < cut {
                se += g;
                ne += 1;
            } else if g < top {
                sc += g;
                nc += 1;
            }
        }
        if ne > 0 {
            egap = (se / ne as f32).exp();
        }
        if nc > 0 {
            cgap = (sc / nc as f32).exp();
        }
        cgap = cgap.max(1.5 * egap);
        cut = 0.5 * (egap.ln() + cgap.ln());
    }
    Some(Timing { dit, dah, egap, cgap })
}

// ---------------------------------------------------------------------------
// Signal processing helpers.

/// Blackman-Harris windowed-sinc lowpass with unity gain at DC.
fn lowpass(cutoff_hz: f32, fs: f32, ntaps: usize) -> Vec<f32> {
    let fc = cutoff_hz / fs;
    let mid = (ntaps / 2) as f32;
    let mut taps: Vec<f32> = (0..ntaps)
        .map(|i| {
            let x = i as f32 - mid;
            let sinc = if x == 0.0 { 2.0 * fc } else { (2.0 * PI * fc * x).sin() / (PI * x) };
            let xw = 2.0 * PI * i as f32 / (ntaps - 1) as f32;
            let w = 0.358_75 - 0.488_29 * xw.cos() + 0.141_28 * (2.0 * xw).cos() - 0.011_68 * (3.0 * xw).cos();
            sinc * w
        })
        .collect();
    let sum: f32 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t /= sum);
    taps
}

/// Inner product in eight independent lanes, so the compiler can keep
/// them in vector registers.
#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let (ca, cb) = (a.chunks_exact(8), b.chunks_exact(8));
    let (ra, rb) = (ca.remainder(), cb.remainder());
    for (x, h) in ca.zip(cb) {
        for l in 0..8 {
            acc[l] += x[l] * h[l];
        }
    }
    let mut s = acc.iter().sum::<f32>();
    for (x, h) in ra.iter().zip(rb) {
        s += x * h;
    }
    s
}

/// Zeroes impulses at the input rate, before any filter spreads them.
///
/// The reference is the recent signal power, risen to quickly and fallen
/// from slowly, so it sits near the peak of the strongest carrier and a
/// keyed signal never trips it; an impulse has to stand well above
/// everything there is. A burst over the threshold that lasts is a new,
/// stronger signal, and the reference jumps to it.
struct Blanker {
    delay: Vec<f32>,
    pos: usize,
    reference: f32,
    warm: u32,
    n: u64,
    /// Output samples in `lo..hi` are zeroed.
    lo: u64,
    hi: u64,
    burst_start: u64,
    last_over: u64,
    post: u64,
    max_burst: u64,
    join: u64,
    a_up: f32,
    a_down: f32,
    a_fast: f32,
    /// Samples zeroed, for the tests.
    blanked: u64,
}

impl Blanker {
    fn new(fs: f32) -> Self {
        let pre = ((BLANK_PRE_S * fs).ceil() as usize).max(1);
        Self {
            delay: vec![0.0; pre],
            pos: 0,
            reference: 0.0,
            warm: 0,
            n: 0,
            lo: 0,
            hi: 0,
            burst_start: 0,
            last_over: 0,
            post: (BLANK_POST_S * fs).ceil() as u64,
            max_burst: (BLANK_MAX_S * fs) as u64,
            join: (BLANK_JOIN_S * fs) as u64,
            a_up: 1.0 / (BLANK_UP_S * fs),
            a_down: 1.0 / (BLANK_DOWN_S * fs),
            a_fast: 1.0 / (0.1 * BLANK_MAX_S * fs),
            blanked: 0,
        }
    }

    fn process(&mut self, x: &[f32], out: &mut Vec<f32>) {
        out.clear();
        let pre = self.delay.len() as u64;
        for &v in x {
            let n = self.n;
            let p = v * v;
            // The first 20 ms set the reference by a plain mean.
            if self.warm < 1000 {
                self.warm += 1;
                self.reference += (p - self.reference) / self.warm as f32;
            } else if p > BLANK_K2 * self.reference {
                if n - self.last_over > self.join {
                    self.burst_start = n;
                }
                self.last_over = n;
                if n - self.burst_start <= self.max_burst {
                    let lo = n.saturating_sub(pre);
                    if lo <= self.hi {
                        self.hi = n + self.post;
                    } else {
                        (self.lo, self.hi) = (lo, n + self.post);
                    }
                } else {
                    self.reference += (p - self.reference) * self.a_fast;
                }
            } else {
                let a = if p > self.reference {
                    if n - self.burst_start > self.max_burst && n - self.last_over <= self.join {
                        self.a_fast
                    } else {
                        self.a_up
                    }
                } else {
                    self.a_down
                };
                self.reference += (p - self.reference) * a;
            }
            let d = self.delay[self.pos];
            self.delay[self.pos] = v;
            self.pos = (self.pos + 1) % self.delay.len();
            let m = n.wrapping_sub(pre);
            if n >= pre && m >= self.lo && m < self.hi {
                out.push(0.0);
                self.blanked += 1;
            } else {
                out.push(d);
            }
            self.n += 1;
        }
    }
}

/// What every channel shares: rates, the channel filter, the table.
struct Shape {
    fs1: f32,
    decim: usize,
    fs2: f32,
    fir_taps: usize,
    table: Vec<&'static str>,
}

impl Shape {
    fn new(fs1: f32) -> Self {
        let decim = ((fs1 / CH_RATE).round() as usize).max(1);
        let fs2 = fs1 / decim as f32;
        let fir_taps = (((8.0 * fs1 / CH_TRANSITION_HZ).ceil() as usize) | 1).clamp(63, 2047);
        Self { fs1, decim, fs2, fir_taps, table: morse_table() }
    }

    fn samples(&self, s: f32) -> f32 {
        s * self.fs2
    }
}

/// One key-down and the key-up after it, in channel samples.
#[derive(Clone, Copy, Debug)]
struct Elem {
    mark: f32,
    gap: f32,
    /// The mark's amplitude SNR after the matched filter, and its level.
    r: f32,
    level: f32,
    /// Absolute channel-rate times of the key going down and up.
    t0: u64,
    t1: u64,
}

/// A word's worth of elements, closed by a long key-up.
struct Word {
    elems: Vec<Elem>,
}

/// A word read, waiting to be handed on.
struct Decoded {
    text: String,
    chars: u64,
    wpm: f32,
    snr_db: f32,
}

/// A run of elements read as characters.
#[derive(Default)]
struct Reading {
    text: String,
    chars: u64,
    /// Characters not in the table, characters of one element (E and T),
    /// and elements in all.
    bad: u64,
    single: u64,
    elems: u64,
}

impl Reading {
    fn add(&mut self, o: &Reading) {
        self.chars += o.chars;
        self.bad += o.bad;
        self.single += o.single;
        self.elems += o.elems;
    }
}

/// Read `elems` as characters with timing `t`.
fn read(elems: &[Elem], t: Timing, sh: &Shape) -> Reading {
    let cut_m = sh.samples(t.mark_cut());
    let cut_c = sh.samples(t.char_cut());
    let mut r = Reading::default();
    let (mut code, mut n) = (1usize, 0usize);
    for (i, e) in elems.iter().enumerate() {
        if n < MAX_ELEMS {
            code = (code << 1) | usize::from(e.mark > cut_m);
        }
        n += 1;
        if i + 1 == elems.len() || e.gap > cut_c {
            r.chars += 1;
            r.elems += n as u64;
            r.single += u64::from(n == 1);
            let s = if n <= MAX_ELEMS { sh.table[code] } else { "" };
            if s.is_empty() {
                r.text.push('*');
                r.bad += 1;
            } else {
                r.text.push_str(s);
            }
            (code, n) = (1, 0);
        }
    }
    r
}

struct Scratch {
    mixed: Vec<Complex32>,
    dec: Vec<Complex32>,
}

struct Channel {
    id: u32,
    /// Where the detector last put the signal; the carrier loop stays
    /// within [`AFC_RANGE_HZ`] of it.
    home_hz: f32,
    nco_hz: f32,
    /// The carrier loop's correction, applied at the channel rate after
    /// the filter so it takes effect at once.
    fine_hz: f32,
    nco: Nco,
    fir: DecimFir,
    rot: Complex32,
    rot_step: Complex32,
    /// Shared-rate sample number of the next input.
    t_in: u64,

    /// Matched filter: the last [`ZRING`] channel samples and the running
    /// sum of the last `l`, in f64 so it does not drift.
    l: usize,
    zring: [Complex32; ZRING],
    sum: (f64, f64),
    nz: u64,
    /// Outputs gathered before the first threshold, with their absolute
    /// times.
    init: Option<Vec<(Complex32, u64)>>,

    /// Steps of the key detector, and the power at each, by step.
    k: u64,
    pring: [f32; PRING],
    noise_q: f32,
    noise_pow: f32,
    level: f32,
    down: bool,
    /// Step time (fractional) of the last crossing, the step a mark was
    /// first seen at, and the re-timed end of the last mark.
    edge: f64,
    start_k: u64,
    last_end: f64,
    prev_y: Complex32,
    mark_sum: f64,
    mark_n: u32,
    mark_peak: f32,
    afc: Complex32,
    synced: bool,

    word: Vec<Elem>,
    marks: VecDeque<f32>,
    gaps: VecDeque<f32>,
    timing: Option<Timing>,
    /// While the timing is unknown, a first guess from the words so far,
    /// for where the current word ends.
    provisional: Option<Timing>,
    held: VecDeque<Word>,
    /// The channel has shown that it hears Morse (see [`Channel::prove`]),
    /// and when, in absolute channel samples, it last read a word.
    proven: bool,
    last_print: u64,
    /// Words closed, waiting for the decoder to check them against the
    /// neighbors; then words decoded.
    ready: Vec<Word>,
    done: Vec<Decoded>,

    /// Recent marks as absolute channel-rate spans, and the span of time
    /// the channel has listened over.
    recent: VecDeque<(u64, u64)>,
    heard_from: Option<u64>,
    last_abs: u64,

    /// Key-down SNR in 500 Hz (linear), averaged over the last eight or so
    /// marks, and how many marks there have been; the detector's reading.
    snr_lin: f32,
    snr_marks: u32,
    spec_snr_db: f32,
    chars: u64,
    /// Shared-rate sample clock at the last word printed, at the last time
    /// the detector saw the signal, and at the channel's start.
    last_word: Option<u64>,
    last_seen: u64,
    started: u64,
    ghost_words: u32,
}

impl Channel {
    fn new(id: u32, hz: f32, spec_snr_db: f32, t_in: u64, now: u64, sh: &Shape) -> Self {
        let mut nco = Nco::new();
        nco.set_freq(hz as f64, sh.fs1 as f64);
        let zero = Complex32::new(0.0, 0.0);
        Self {
            id,
            home_hz: hz,
            nco_hz: hz,
            fine_hz: 0.0,
            nco,
            fir: DecimFir::new(CH_CUTOFF_HZ, sh.fs1, sh.decim, sh.fir_taps),
            rot: Complex32::new(1.0, 0.0),
            rot_step: Complex32::new(1.0, 0.0),
            t_in,
            l: (sh.samples(MF_START_S).round() as usize).clamp(2, ZRING - 1),
            zring: [zero; ZRING],
            sum: (0.0, 0.0),
            nz: 0,
            init: Some(Vec::with_capacity((INIT_S * sh.fs2) as usize + 1)),
            k: 0,
            pring: [0.0; PRING],
            noise_q: 0.0,
            noise_pow: 0.0,
            level: 0.0,
            down: false,
            edge: 0.0,
            start_k: 0,
            last_end: 0.0,
            prev_y: zero,
            mark_sum: 0.0,
            mark_n: 0,
            mark_peak: 0.0,
            afc: zero,
            synced: false,
            word: Vec::with_capacity(32),
            marks: VecDeque::with_capacity(TIMING_MEMORY + 1),
            gaps: VecDeque::with_capacity(TIMING_MEMORY + 1),
            timing: None,
            held: VecDeque::new(),
            provisional: None,
            proven: false,
            last_print: 0,
            ready: Vec::new(),
            done: Vec::new(),
            recent: VecDeque::new(),
            heard_from: None,
            last_abs: 0,
            snr_lin: 0.0,
            snr_marks: 0,
            spec_snr_db,
            chars: 0,
            last_word: None,
            last_seen: now,
            started: now,
            ghost_words: 0,
        }
    }

    fn hz(&self) -> f32 {
        self.nco_hz + self.fine_hz
    }

    fn set_fine(&mut self, hz: f32, sh: &Shape) {
        self.fine_hz = hz;
        let (s, c) = (-2.0 * PI * hz / sh.fs2).sin_cos();
        self.rot_step = Complex32::new(c, s);
    }

    /// Move to where the detector sees the signal now.
    fn retune(&mut self, hz: f32, sh: &Shape) {
        self.home_hz = hz;
        self.nco_hz = hz;
        self.nco.set_freq(hz as f64, sh.fs1 as f64);
        self.set_fine(0.0, sh);
    }

    /// The channel's speed, once it has shown that it hears Morse; before
    /// that its timing is a guess at whatever crosses the threshold.
    fn read_wpm(&self) -> Option<f32> {
        self.timing.filter(|_| self.proven).map(|t| t.wpm())
    }

    fn snr_db(&self) -> f32 {
        if self.snr_marks >= 3 { 10.0 * self.snr_lin.max(1e-4).log10() } else { self.spec_snr_db }
    }

    fn active(&self, now: u64, sh: &Shape) -> bool {
        self.last_word.is_some_and(|t| now.saturating_sub(t) < (ACTIVE_S * sh.fs1) as u64)
    }

    fn noise_amp(&self) -> f32 {
        (self.noise_pow * PI / 4.0).sqrt()
    }

    fn threshold(&self) -> f32 {
        let na = self.noise_amp();
        // Halfway for a weak signal, where noise decides; lower for a
        // strong one, where a fade does.
        let strong = ((self.level / na.max(1e-30) - THR_WEAK) / THR_WEAK).clamp(0.0, 1.0);
        let f = 0.5 + (THR_FRACTION - 0.5) * strong;
        (na + f * (self.level - na)).max(THR_FLOOR * na)
    }

    fn process(&mut self, xc: &[Complex32], scratch: &mut Scratch, sh: &Shape) {
        self.nco.mix(xc, &mut scratch.mixed);
        self.fir.process(&scratch.mixed, &mut scratch.dec);
        let end = self.t_in + xc.len() as u64;
        let m = scratch.dec.len() as u64;
        for (j, &z) in scratch.dec.iter().enumerate() {
            // Shared-rate time of this output, near enough, as an absolute
            // channel-rate sample number that every channel agrees on.
            let t = end.saturating_sub((m - 1 - j as u64) * sh.decim as u64);
            self.sample(z, t / sh.decim as u64, sh);
        }
        self.t_in = end;
    }

    fn sample(&mut self, z: Complex32, abs: u64, sh: &Shape) {
        let z = z * self.rot;
        self.rot *= self.rot_step;
        if self.nz & 255 == 0 {
            self.rot /= self.rot.norm();
        }
        let i = self.nz as usize & ZMASK;
        let old = self.zring[(self.nz as usize + ZRING - self.l) & ZMASK];
        self.zring[i] = z;
        self.sum.0 += z.re as f64 - old.re as f64;
        self.sum.1 += z.im as f64 - old.im as f64;
        self.nz += 1;
        if self.nz < self.l as u64 {
            return;
        }
        let inv = 1.0 / self.l as f64;
        let y = Complex32::new((self.sum.0 * inv) as f32, (self.sum.1 * inv) as f32);
        if let Some(buf) = &mut self.init {
            buf.push((y, abs));
            if buf.len() as f32 >= INIT_S * sh.fs2 {
                self.start(sh);
            }
            return;
        }
        self.step(y, abs, sh);
    }

    /// Set the first noise and mark levels from the gathered outputs, then
    /// run the key detector over them.
    fn start(&mut self, sh: &Shape) {
        let Some(buf) = self.init.take() else { return };
        let mut p: Vec<f32> = buf.iter().map(|(y, _)| y.norm_sqr()).collect();
        let n = p.len();
        let (_, p25, _) = p.select_nth_unstable_by(n / 4, f32::total_cmp);
        // A carrier keyed most of the time would inflate this; that only
        // makes the threshold cautious until the noise is measured.
        self.noise_q = p25.max(1e-30);
        self.noise_pow = self.noise_q / P25_OF_MEAN;
        let k97 = (n * 97 / 100).min(n - 1);
        let (_, p97, _) = p.select_nth_unstable_by(k97, f32::total_cmp);
        self.level = p97.sqrt().max(self.noise_amp());
        for (y, abs) in buf {
            self.step(y, abs, sh);
        }
    }

    /// Change the matched filter's length, between words.
    fn resize(&mut self, l: usize) {
        let l = l.clamp(2, ZRING - 1);
        if l == self.l || (self.nz as usize) < ZRING {
            return;
        }
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for j in 0..l {
            let z = self.zring[(self.nz as usize + ZRING - 1 - j) & ZMASK];
            re += z.re as f64;
            im += z.im as f64;
        }
        self.sum = (re, im);
        // The noise out of a boxcar goes as one over its length; marks
        // longer than it keep their level.
        self.noise_q *= self.l as f32 / l as f32;
        self.noise_pow = self.noise_q / P25_OF_MEAN;
        self.l = l;
    }

    /// Magnitude at step `j`, from the history.
    fn mag(&self, j: u64) -> f32 {
        self.pring[j as usize & PMASK].sqrt()
    }

    /// When the magnitude rose through `target` around step `from`: back
    /// from it if it was already above (the threshold was higher than half
    /// this mark), on from it if not, never before the last mark ended nor
    /// past step `to`.
    fn rise_time(&self, from: u64, to: u64, target: f32) -> f64 {
        let lo = self.k.saturating_sub(PRING as u64 - 2).max(self.last_end.ceil() as u64 + 1).max(1);
        let mut j = from.max(lo);
        if self.mag(j) >= target {
            while j > lo && self.mag(j - 1) >= target {
                j -= 1;
            }
        } else {
            while j < to && self.mag(j) < target {
                j += 1;
            }
        }
        let (a0, a1) = (self.mag(j - 1), self.mag(j));
        let f = if a1 > a0 { ((target - a0) / (a1 - a0)).clamp(0.0, 1.0) } else { 1.0 };
        (j - 1) as f64 + f as f64
    }

    /// When the magnitude last fell through `target`, back from step `to`
    /// to no earlier than `from`.
    fn fall_time(&self, from: u64, to: u64, target: f32) -> f64 {
        let mut j = to - 1;
        while j > from && self.mag(j) < target {
            j -= 1;
        }
        let (a0, a1) = (self.mag(j), self.mag(j + 1));
        let f = if a0 > a1 { ((a0 - target) / (a0 - a1)).clamp(0.0, 1.0) } else { 0.0 };
        j as f64 + f as f64
    }

    /// One output of the matched filter.
    fn step(&mut self, y: Complex32, abs: u64, sh: &Shape) {
        self.k += 1;
        let k = self.k;
        let p = y.norm_sqr();
        let a = p.sqrt();
        self.pring[k as usize & PMASK] = p;
        let l = self.l as f32;
        let h = (self.l / 2).max(1);
        let since = (k as f64 - self.edge) as f32;
        let na = self.noise_amp();
        if self.down {
            // The middle of a mark, where the filter has settled: its
            // power, and how the carrier turns.
            if since >= 0.5 * l {
                self.mark_sum += p as f64;
                self.mark_n += 1;
                self.afc += y * self.prev_y.conj();
            }
            self.mark_peak = self.mark_peak.max(p);
            // The mark ends when it falls through the midpoint between the
            // noise and its own level (by its peak until it has settled):
            // half its level when strong, and higher when weak, which is
            // where noise on a mark and noise in a gap are told apart best.
            let own = if self.mark_n > 0 {
                (self.mark_sum / self.mark_n as f64).sqrt() as f32
            } else {
                self.mark_peak.sqrt()
            };
            if a < 0.5 * (own + na) * (1.0 - HYST) {
                // The last half window of samples summed lie on the falling
                // edge, past the end of the key-down.
                for j in 0..h as u64 {
                    let s = k - j;
                    if s as f64 >= self.edge + 0.5 * l as f64 && self.mark_n > 0 {
                        self.mark_sum -= self.pring[s as usize & PMASK] as f64;
                        self.mark_n -= 1;
                    }
                }
                let pow = if self.mark_n > 0 { (self.mark_sum / self.mark_n as f64) as f32 } else { self.mark_peak };
                // Both edges at that midpoint, so the mark's length does not
                // depend on how far the threshold lagged a fade.
                let target = 0.5 * (pow.sqrt() + na);
                let t1 = self.fall_time(self.start_k, k, target);
                let t0 = self.rise_time(self.start_k, k, target).min(t1);
                let gap = (t0 - self.last_end) as f32;
                self.down = false;
                self.edge = k as f64;
                self.last_end = t1;
                let back = |t: f64| abs.saturating_sub(h as u64 + (k as f64 - t).max(0.0) as u64);
                let (a0, a1) = (back(t0), back(t1));
                self.recent.push_back((a0, a1));
                while self.recent.front().is_some_and(|m| m.1 + RECENT_SPAN < a1) {
                    self.recent.pop_front();
                }
                self.end_mark(gap, (t1 - t0) as f32, pow, a0, a1, sh);
            }
        } else {
            // Noise from the sample half a window back, if the key has
            // been up since well before it: clear of the last mark's tail
            // and of the next one's rise.
            if since >= 2.0 * l {
                let q = self.pring[(k - h as u64) as usize & PMASK];
                let eta = 4.0 / (NOISE_TAU_S * sh.fs2);
                self.noise_q *= if q < self.noise_q { 1.0 - 0.75 * eta } else { 1.0 + 0.25 * eta };
                self.noise_pow = self.noise_q / P25_OF_MEAN;
            }
            if a >= self.threshold() * (1.0 + HYST) {
                self.down = true;
                self.edge = k as f64;
                self.start_k = k;
                self.mark_sum = 0.0;
                self.mark_n = 0;
                self.mark_peak = p;
            } else {
                let decay_after = self.timing.map_or(SYNC_GAP_S, |t| t.char_cut());
                if since > sh.samples(decay_after) {
                    self.level = na + (self.level - na) * (1.0 - 1.0 / (LEVEL_DECAY_S * sh.fs2));
                }
                if !self.synced && since > sh.samples(SYNC_GAP_S) {
                    self.synced = true;
                }
                if !self.word.is_empty() && since > self.word_cut(sh) {
                    self.end_word();
                }
                if since > sh.samples(TIMING_PAUSE_S) && self.marks.len() > TIMING_KEEP {
                    // A pause: perhaps the other station now. Keep only a
                    // little of this fist, so a new one is learned quickly.
                    let n = self.marks.len() - TIMING_KEEP;
                    self.marks.drain(..n);
                    let n = self.gaps.len().saturating_sub(TIMING_KEEP);
                    self.gaps.drain(..n);
                }
            }
        }
        let at = abs.saturating_sub(h as u64);
        if self.heard_from.is_none() {
            self.heard_from = Some(at);
        }
        self.last_abs = at;
        self.prev_y = y;
    }

    /// The key-up that closes a word, in samples.
    fn word_cut(&self, sh: &Shape) -> f32 {
        match self.timing.or(self.provisional) {
            Some(t) => sh.samples(t.word_cut()),
            None => {
                // No guess yet: seven of the shortest real mark so far,
                // which is a dit unless the word has none, and no less than
                // a word gap at 35 WPM. Two words read as one are split
                // again once the timing is known; a word cut in two could
                // not be mended. A word of noise alone closes soon.
                let m = self
                    .word
                    .iter()
                    .filter(|e| e.r >= BLIP_R && e.mark >= sh.samples(DIT_MIN_S))
                    .map(|e| e.mark)
                    .fold(f32::INFINITY, f32::min);
                if m.is_finite() { (7.0 * m).clamp(sh.samples(0.24), sh.samples(1.5)) } else { sh.samples(0.15) }
            }
        }
    }

    /// A mark has ended: `gap` since the last one, `dur` long, `pow` its
    /// settled power, `t0..t1` its absolute channel-rate span.
    fn end_mark(&mut self, gap: f32, dur: f32, pow: f32, t0: u64, t1: u64, sh: &Shape) {
        let l = self.l as f32;
        let r = ((pow - self.noise_pow).max(0.0) / self.noise_pow.max(1e-30)).sqrt();
        let m = pow.sqrt();
        // A blip shorter than the filter is noise or an impulse: it has no
        // settled middle, and its peak would only drag the level about.
        if dur >= 0.6 * l {
            let w = if m > self.level { LEVEL_UP } else { LEVEL_DOWN };
            self.level += (m - self.level) * w;
            if r >= 2.0 {
                let enbw = sh.fs2 / l;
                let snr = r * r * enbw / 500.0;
                self.snr_marks += 1;
                let a = 1.0 / self.snr_marks.min(8) as f32;
                self.snr_lin += (snr - self.snr_lin) * a;
            }
            if r >= AFC_MIN_R && self.afc.norm_sqr() > 0.0 {
                let err = self.afc.arg() * sh.fs2 / (2.0 * PI);
                let fine = (self.fine_hz + AFC_GAIN * err)
                    .clamp(self.home_hz - self.nco_hz - AFC_RANGE_HZ, self.home_hz - self.nco_hz + AFC_RANGE_HZ);
                self.set_fine(fine, sh);
            }
        }
        self.afc = Complex32::new(0.0, 0.0);
        if !self.synced {
            return;
        }
        if let Some(e) = self.word.last_mut() {
            e.gap = gap;
        }
        self.word.push(Elem { mark: dur, gap: 0.0, r, level: m, t0, t1 });
        if self.timing.is_none() && self.word.len() >= 6 {
            // A first guess at the timing from what has been heard, so a
            // long first word is not left open until a pause.
            let mut marks = self.marks.clone();
            let mut gaps = self.gaps.clone();
            marks.extend(self.word.iter().map(|e| e.mark / sh.fs2));
            gaps.extend(self.word[..self.word.len() - 1].iter().map(|e| e.gap / sh.fs2));
            self.provisional = estimate(None, &marks, &gaps);
        }
        if self.word.len() >= WORD_MAX_ELEMS {
            self.end_word();
        }
    }

    /// Close the word: the decoder checks it against the neighbors, then
    /// hands it back to [`Channel::decode_word`].
    fn end_word(&mut self) {
        self.provisional = None;
        let elems = std::mem::take(&mut self.word);
        if !elems.is_empty() {
            self.ready.push(Word { elems });
        }
    }

    /// Learn from a word and read it, once the decoder has taken out what
    /// was a neighbor's.
    fn decode_word(&mut self, raw: Vec<Elem>, sh: &Shape) {
        if raw.is_empty() {
            return;
        }
        // Glitches out: a mark far weaker than the word's others is noise
        // that crossed the threshold before or between them, a mark too
        // short to be a dit is too, and a gap too short to be one is a dip
        // in a mark.
        let mut rs: Vec<f32> = raw.iter().map(|e| e.r).collect();
        let mid = rs.len() / 2;
        let median = *rs.select_nth_unstable_by(mid, f32::total_cmp).1;
        let blip = |r: f32| r < BLIP_R_ANY || (r < BLIP_R && r < BLIP_SHARE * median);
        let (min_mark, min_gap) = match self.timing {
            Some(t) => (GLITCH_MARK * t.dit, GLITCH * t.egap.min(t.dit)),
            None => (0.0, 0.0),
        };
        let (min_mark, min_gap) = (sh.samples(min_mark.max(GLITCH_ABS_S)), sh.samples(min_gap.max(GLITCH_ABS_S)));
        let mut elems: Vec<Elem> = Vec::with_capacity(raw.len());
        for e in raw {
            if e.mark < min_mark || blip(e.r) {
                if let Some(last) = elems.last_mut() {
                    last.gap += e.mark + e.gap;
                }
                continue;
            }
            if let Some(last) = elems.last_mut()
                && last.gap < min_gap
            {
                last.mark += last.gap + e.mark;
                last.gap = e.gap;
                last.r = last.r.max(e.r);
                last.level = last.level.max(e.level);
                last.t1 = e.t1;
                continue;
            }
            elems.push(e);
        }
        if elems.is_empty() {
            return;
        }
        // Too weak to trust: neither printed nor learned from, so noise on
        // an idle channel does not teach it a speed.
        let r = elems.iter().map(|e| e.r).sum::<f32>() / elems.len() as f32;
        if r < WORD_MIN_R {
            return;
        }
        for e in &elems {
            self.marks.push_back(e.mark / sh.fs2);
        }
        for e in &elems[..elems.len() - 1] {
            self.gaps.push_back(e.gap / sh.fs2);
        }
        while self.marks.len() > TIMING_MEMORY {
            self.marks.pop_front();
        }
        while self.gaps.len() > TIMING_MEMORY {
            self.gaps.pop_front();
        }
        self.timing = estimate(self.timing, &self.marks, &self.gaps);
        let (t0, t1) = (elems[0].t0, elems[elems.len() - 1].t1);
        if self.proven && t0.saturating_sub(self.last_print) as f32 > sh.samples(PROVEN_IDLE_S) {
            // Quiet long enough that whatever keys here next must show
            // again that it is Morse.
            self.proven = false;
        }
        let Some(t) = self.timing.filter(|t| (WPM_MIN..=WPM_MAX).contains(&t.wpm())) else {
            // No timing, or none a Morse sender could have: nothing is
            // read, and what is held waits for better.
            self.hold(Word { elems }, sh);
            return;
        };
        // The matched filter follows the speed whether or not the channel
        // has proven itself: a filter left short lets in more of the noise
        // and of a loud neighbor's clicks than the proof can see past.
        let target = sh.samples((MF_FRACTION * t.dit).clamp(MF_MIN_S, MF_MAX_S)).round() as usize;
        if (target as f32 - self.l as f32).abs() > MF_RESIZE * self.l as f32 {
            self.resize(target);
        }
        if !self.proven {
            self.hold(Word { elems }, sh);
            if !self.prove(t, sh) {
                return;
            }
            self.proven = true;
            while let Some(w) = self.held.pop_front() {
                self.spell(&w.elems, t, sh);
            }
        } else {
            while let Some(w) = self.held.pop_front() {
                self.spell(&w.elems, t, sh);
            }
            self.spell(&elems, t, sh);
        }
        self.last_print = t1;
    }

    /// Keep a word until the channel can read it, or has shown that what it
    /// hears is Morse; the oldest go once there are too many or they are
    /// too old.
    fn hold(&mut self, w: Word, sh: &Shape) {
        let t1 = w.elems[w.elems.len() - 1].t1;
        self.held.push_back(w);
        while self.held.len() > HELD_WORDS
            || self.held.front().is_some_and(|h| t1.saturating_sub(h.elems[0].t0) as f32 > sh.samples(HELD_S))
        {
            self.held.pop_front();
        }
    }

    /// Whether what this channel has heard is Morse: keying with two kinds
    /// of mark near 1:3 that stay where they are, gaps that cluster the
    /// same way, kept up for some seconds, and a reading that looks like
    /// text, not the run of E, T and I that noise or a digital mode's tones
    /// make when read as Morse. Noise crossing a threshold gives marks of
    /// one length, the filter's; a data mode's tones give marks in whole
    /// symbols; neither gives dits and dahs at a steady 1:3 with
    /// characters of several elements between them.
    fn prove(&self, t: Timing, sh: &Shape) -> bool {
        let ratio = t.dah / t.dit;
        if !(PROVE_DAH_RATIO.0..=PROVE_DAH_RATIO.1).contains(&ratio)
            || !(PROVE_EGAP_RATIO.0..=PROVE_EGAP_RATIO.1).contains(&(t.egap / t.dit))
            || t.cgap < PROVE_CGAP_RATIO * t.egap
        {
            return false;
        }
        // Both kinds of mark present, and the marks close to one or the
        // other.
        let (ld, lh, cut) = (t.dit.ln(), t.dah.ln(), t.mark_cut());
        let near = PROVE_SPREAD.ln();
        let (mut dits, mut dahs, mut tight) = (0, 0, 0);
        for &m in &self.marks {
            if m < cut {
                dits += 1;
            } else {
                dahs += 1;
            }
            if (m.ln() - ld).abs() < near || (m.ln() - lh).abs() < near {
                tight += 1;
            }
        }
        if dits < PROVE_EACH || dahs < PROVE_EACH || (tight as f32) < PROVE_TIGHT * self.marks.len() as f32 {
            return false;
        }
        // Kept up for a while...
        let (Some(first), Some(last)) = (self.held.front(), self.held.back()) else { return false };
        let span = last.elems[last.elems.len() - 1].t1.saturating_sub(first.elems[0].t0);
        if (span as f32) < sh.samples(PROVE_S) {
            return false;
        }
        // ...and reading as text.
        let mut r = Reading::default();
        for w in &self.held {
            r.add(&read(&w.elems, t, sh));
        }
        r.chars >= PROVE_CHARS
            && (r.single as f32) <= PROVE_SINGLE * r.chars as f32
            && (r.bad as f32) <= PROVE_BAD * r.chars as f32
            && (r.elems as f32) >= PROVE_ELEMS * r.chars as f32
    }

    /// Read a word with the timing as it stands, as more than one if it
    /// was closed before the timing was known and holds a word gap.
    fn spell(&mut self, elems: &[Elem], t: Timing, sh: &Shape) {
        let cut_w = sh.samples(t.word_cut());
        let mut from = 0;
        for i in 0..elems.len() {
            if i + 1 == elems.len() || elems[i].gap > cut_w {
                self.spell_one(&elems[from..=i], t, sh);
                from = i + 1;
            }
        }
    }

    fn spell_one(&mut self, elems: &[Elem], t: Timing, sh: &Shape) {
        let rd = read(elems, t, sh);
        let r = elems.iter().map(|e| e.r).sum::<f32>() / elems.len() as f32;
        if r < WORD_MIN_R || 2 * rd.bad > rd.chars {
            return;
        }
        self.done.push(Decoded { text: rd.text, chars: rd.chars, wpm: t.wpm(), snr_db: self.snr_db() });
    }

    /// Whether the channel listened over all of `t0..t1`.
    fn heard(&self, t0: u64, t1: u64) -> bool {
        self.heard_from.is_some_and(|f| f <= t0) && self.last_abs >= t1
    }

    /// Close a word left open when the channel goes.
    fn finish(&mut self) {
        if !self.word.is_empty() {
            self.end_word();
        }
    }
}

/// A channel started on the audio heard so far, so it has the opening of
/// the transmission that got it noticed.
fn replayed(id: u32, hz: f32, snr_db: f32, now: u64, history: &[f32], sc: &mut Scratch, sh: &Shape) -> Channel {
    let mut ch = Channel::new(id, hz, snr_db, now - history.len() as u64, now, sh);
    let xc: Vec<Complex32> = history.iter().map(|&v| Complex32::new(v, 0.0)).collect();
    for piece in xc.chunks(4096) {
        ch.process(piece, sc, sh);
    }
    ch
}

struct Candidate {
    hz: f32,
    score: f32,
    snr_db: f32,
}

/// Multi-channel Morse decoder over one sideband's audio.
pub struct CwDecoder {
    sh: Shape,
    low_hz: f32,
    high_hz: f32,
    max_channels: usize,
    focus: Option<f32>,

    blanker: Blanker,
    blanked: Vec<f32>,
    fe_decim: usize,
    fe_taps: Vec<f32>,
    fe_hist: Vec<f32>,
    x1: Vec<f32>,
    xc: Vec<Complex32>,
    history: Vec<f32>,
    history_len: usize,

    fft: Arc<dyn Fft<f32>>,
    fft_len: usize,
    window: Vec<f32>,
    spec_ring: Vec<f32>,
    spec_pos: usize,
    since_hop: usize,
    fft_buf: Vec<Complex32>,
    fft_scratch: Vec<Complex32>,
    avg: Vec<f32>,
    frames: u32,
    frames_since_detect: u32,
    floor: Vec<f32>,
    excess: Vec<f32>,
    score: Vec<f32>,
    sorted: Vec<f32>,
    cands: Vec<Candidate>,

    channels: Vec<Channel>,
    /// Pitches where a channel only heard a neighbor, and until when.
    blocked: Vec<(f32, u64)>,
    next_id: u32,
    /// Shared-rate samples since the start.
    now: u64,
    scratch: Scratch,
    chars: u64,
    out: Vec<CwEvent>,
}

impl CwDecoder {
    /// A decoder for audio at `fs` Hz, any rate from 8 kHz up.
    pub fn new(fs: f32) -> Self {
        let fe_decim = ((fs / FRONT_MIN_RATE).floor() as usize).max(1);
        let fs1 = fs / fe_decim as f32;
        let fe_taps = if fe_decim > 1 {
            // Cut at half the new rate; the transition runs from the top of
            // the passband to its mirror image, the first thing to fold.
            let width = (fs1 - 2.0 * FRONT_PASS_HZ).max(0.1 * fs1);
            let n = (((8.0 * fs / width).ceil() as usize) | 1).clamp(31, 511);
            lowpass(0.5 * fs1, fs, n)
        } else {
            Vec::new()
        };
        // Bins of about 8 Hz: a keyed carrier's line stands out of the
        // noise, and two signals 25 Hz apart are three bins apart.
        let fft_len = ((fs1 / 8.0) as usize).next_power_of_two().max(256);
        let fft = FftPlanner::new().plan_fft_forward(fft_len);
        let window: Vec<f32> = (0..fft_len).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / fft_len as f32).cos()).collect();
        let sh = Shape::new(fs1);
        let history_len = (REPLAY_S * fs1) as usize;
        let zero = Complex32::new(0.0, 0.0);
        let mut this = Self {
            sh,
            low_hz: 200.0,
            high_hz: 1200.0,
            max_channels: 24,
            focus: None,
            blanker: Blanker::new(fs),
            blanked: Vec::new(),
            fe_decim,
            fe_taps,
            fe_hist: Vec::new(),
            x1: Vec::new(),
            xc: Vec::new(),
            history: Vec::with_capacity(2 * history_len + 8192),
            history_len,
            fft_scratch: vec![zero; fft.get_inplace_scratch_len()],
            fft,
            fft_len,
            window,
            spec_ring: vec![0.0; fft_len],
            spec_pos: 0,
            since_hop: 0,
            fft_buf: vec![zero; fft_len],
            avg: vec![0.0; fft_len / 2],
            frames: 0,
            frames_since_detect: 0,
            floor: Vec::new(),
            excess: Vec::new(),
            score: Vec::new(),
            sorted: Vec::new(),
            cands: Vec::new(),
            channels: Vec::new(),
            blocked: Vec::new(),
            next_id: 1,
            now: 0,
            scratch: Scratch { mixed: Vec::new(), dec: Vec::new() },
            chars: 0,
            out: Vec::new(),
        };
        this.set_band(200.0, 1200.0);
        this
    }

    /// The audio searched for signals, Hz. The top is held below what the
    /// shared filter keeps clean.
    pub fn set_band(&mut self, low_hz: f32, high_hz: f32) {
        let top = FRONT_PASS_HZ.min(0.5 * self.sh.fs1) - 150.0;
        self.low_hz = low_hz.clamp(100.0, top - 100.0);
        self.high_hz = high_hz.clamp(self.low_hz + 100.0, top);
    }

    pub fn set_max_channels(&mut self, n: usize) {
        self.max_channels = n.max(1);
    }

    /// Favor the signal at this pitch: a channel is kept on it whether or
    /// not the detector sees anything there, and it is the one the status
    /// reports. `None` releases it.
    pub fn set_pitch_focus(&mut self, hz: Option<f32>) {
        self.focus = hz.filter(|h| h.is_finite());
        if self.focus.is_some() && self.frames >= MIN_FRAMES {
            self.ensure_focus();
        }
    }

    pub fn push(&mut self, audio: &[f32]) {
        // Pieces of about 50 ms, so the detector and the word timers run on
        // time however large the caller's blocks are.
        let piece = (self.fe_decim * 400).max(1);
        for chunk in audio.chunks(piece) {
            self.push_piece(chunk);
        }
    }

    /// Words decoded since the last call.
    pub fn take(&mut self) -> Vec<CwEvent> {
        std::mem::take(&mut self.out)
    }

    pub fn channels(&self) -> Vec<CwChannel> {
        let f = self.focused();
        self.channels
            .iter()
            .enumerate()
            .map(|(i, c)| CwChannel {
                id: c.id,
                audio_hz: c.hz(),
                snr_db: c.snr_db(),
                wpm: c.read_wpm(),
                active: c.active(self.now, &self.sh),
                focused: f == Some(i),
                chars: c.chars,
            })
            .collect()
    }

    pub fn status(&self) -> CwStatus {
        let active: Vec<&Channel> = self.channels.iter().filter(|c| c.active(self.now, &self.sh)).collect();
        let pick = self
            .focused()
            .map(|i| &self.channels[i])
            .or_else(|| active.iter().max_by(|a, b| a.snr_db().total_cmp(&b.snr_db())).copied());
        CwStatus {
            locked: !active.is_empty(),
            center_hz: pick.map(|c| c.hz()),
            snr_db: pick.map(|c| c.snr_db()),
            wpm: pick.and_then(Channel::read_wpm),
            channels: self.channels.len(),
            active: active.len(),
            chars: self.chars,
        }
    }

    /// The channel nearest the focus pitch, if one is close enough.
    fn focused(&self) -> Option<usize> {
        let f = self.focus?;
        (0..self.channels.len())
            .filter(|&i| (self.channels[i].hz() - f).abs() < MIN_SEP_HZ + AFC_RANGE_HZ)
            .min_by(|&a, &b| (self.channels[a].hz() - f).abs().total_cmp(&(self.channels[b].hz() - f).abs()))
    }

    fn push_piece(&mut self, audio: &[f32]) {
        let mut blanked = std::mem::take(&mut self.blanked);
        self.blanker.process(audio, &mut blanked);
        self.front_end(&blanked);
        self.blanked = blanked;
        if self.x1.is_empty() {
            return;
        }
        self.xc.clear();
        self.xc.extend(self.x1.iter().map(|&v| Complex32::new(v, 0.0)));
        self.history.extend_from_slice(&self.x1);
        if self.history.len() > 2 * self.history_len {
            let cut = self.history.len() - self.history_len;
            self.history.drain(..cut);
        }
        for ch in &mut self.channels {
            ch.process(&self.xc, &mut self.scratch, &self.sh);
        }
        self.now += self.x1.len() as u64;
        self.spectrum();
        if self.frames_since_detect >= DETECT_EVERY && self.frames >= MIN_FRAMES {
            self.frames_since_detect = 0;
            self.detect();
        }
        self.settle();
    }

    fn front_end(&mut self, audio: &[f32]) {
        self.x1.clear();
        if self.fe_decim == 1 {
            self.x1.extend_from_slice(audio);
            return;
        }
        self.fe_hist.extend_from_slice(audio);
        let n = self.fe_taps.len();
        let mut i = 0;
        while i + n <= self.fe_hist.len() {
            self.x1.push(dot(&self.fe_hist[i..i + n], &self.fe_taps));
            i += self.fe_decim;
        }
        self.fe_hist.drain(..i.min(self.fe_hist.len()));
    }

    fn spectrum(&mut self) {
        let hop = self.fft_len / SPEC_OVERLAP;
        let alpha0 = hop as f32 / (SPEC_TAU_S * self.sh.fs1);
        for k in 0..self.x1.len() {
            self.spec_ring[self.spec_pos] = self.x1[k];
            self.spec_pos = (self.spec_pos + 1) % self.fft_len;
            self.since_hop += 1;
            if self.since_hop < hop {
                continue;
            }
            self.since_hop = 0;
            let n = self.fft_len;
            for j in 0..n {
                let v = self.spec_ring[(self.spec_pos + j) % n];
                self.fft_buf[j] = Complex32::new(v * self.window[j], 0.0);
            }
            self.fft.process_with_scratch(&mut self.fft_buf, &mut self.fft_scratch);
            self.frames += 1;
            self.frames_since_detect += 1;
            let alpha = (1.0 / self.frames as f32).max(alpha0);
            for (a, x) in self.avg.iter_mut().zip(&self.fft_buf) {
                *a += (x.norm_sqr() - *a) * alpha;
            }
        }
    }

    /// Take the words the channels have closed, drop what a channel only
    /// heard through its filter from a much stronger neighbor, let the
    /// channel learn from and read the rest, and hand on what it reads.
    fn settle(&mut self) {
        for i in 0..self.channels.len() {
            let ready = std::mem::take(&mut self.channels[i].ready);
            for w in ready {
                match self.unmix(i, w) {
                    Some(elems) => self.channels[i].decode_word(elems, &self.sh),
                    None => self.channels[i].ghost_words += 1,
                }
            }
            let ch = &mut self.channels[i];
            for d in std::mem::take(&mut ch.done) {
                ch.chars += d.chars;
                ch.last_word = Some(self.now);
                ch.ghost_words = 0;
                self.chars += d.chars;
                self.out.push(CwEvent::Text {
                    channel: ch.id,
                    audio_hz: ch.hz(),
                    wpm: d.wpm,
                    snr_db: d.snr_db,
                    text: d.text,
                });
            }
        }
    }

    /// Channel `i`'s word less any neighbor's part in it: `None` if the
    /// whole word keys with a much stronger neighbor (its marks leaking
    /// through, or its edges' clicks), else the word without the weak
    /// marks that sit on a strong neighbor's edges.
    fn unmix(&self, i: usize, w: Word) -> Option<Vec<Elem>> {
        let me = &self.channels[i];
        let n = w.elems.len();
        let level = w.elems.iter().map(|e| e.level).sum::<f32>() / n as f32;
        let (t0, t1) = (w.elems[0].t0, w.elems[n - 1].t1);
        // A click comes out of the matched filter centered on the edge that
        // made it, as a leaked mark comes out with the neighbor's edges.
        let tol = LEAK_TOL;
        let center = |e: &Elem| (e.t0 + e.t1) / 2;
        let close = |a: u64, b: u64, tol: u64| a.abs_diff(b) <= tol;
        let share = |k: usize, of: usize| of > 0 && k as f32 >= GHOST_SHARE * of as f32;
        let neighbors = || {
            self.channels.iter().enumerate().filter(move |&(j, o)| {
                let df = (o.hz() - me.hz()).abs();
                j != i && df < GHOST_HZ
            })
        };
        for (_, o) in neighbors() {
            let df = (o.hz() - me.hz()).abs();
            let ratio = if df < GHOST_NEAR_HZ { GHOST_NEAR_RATIO } else { GHOST_RATIO };
            if o.level < ratio * level || !o.heard(t0.saturating_sub(tol), t1 + tol) {
                continue;
            }
            // Its marks over the word, and its edges.
            let marks: Vec<(u64, u64)> =
                o.recent.iter().filter(|m| m.0 + LEAK_TOL >= t0 && m.1 <= t1 + LEAK_TOL).copied().collect();
            let leaked = |e: &Elem, m: &(u64, u64)| close(e.t0, m.0, LEAK_TOL) && close(e.t1, m.1, LEAK_TOL);
            let fwd = w.elems.iter().filter(|e| marks.iter().any(|m| leaked(e, m))).count();
            let rev = marks.iter().filter(|m| w.elems.iter().any(|e| leaked(e, m))).count();
            let leak = share(fwd, n) && share(rev, marks.len());
            let edges: Vec<u64> =
                o.recent.iter().flat_map(|m| [m.0, m.1]).filter(|&t| t + tol >= t0 && t <= t1 + tol).collect();
            let fwd = w.elems.iter().filter(|e| edges.iter().any(|&t| close(center(e), t, tol))).count();
            let rev = edges.iter().filter(|&&t| w.elems.iter().any(|e| close(center(e), t, tol))).count();
            let clicks = edges.len() >= 2 && share(fwd, n) && share(rev, edges.len());
            if leak || clicks {
                return None;
            }
        }
        // Within a word that is the channel's own, a mark centered on a
        // strong neighbor's edge is its click if it is much weaker than the
        // rest, or one of a run of such marks: a real mark lands there by
        // chance a third of the time next to a busy neighbor, three in a
        // row seldom.
        let on_edge: Vec<bool> = w
            .elems
            .iter()
            .map(|e| {
                neighbors().any(|(_, o)| {
                    (o.hz() - me.hz()).abs() >= MIN_SEP_HZ
                        && o.level >= GHOST_RATIO * e.level
                        && o.recent.iter().any(|m| close(center(e), m.0, LEAK_TOL) || close(center(e), m.1, LEAK_TOL))
                })
            })
            .collect();
        let mut in_run = vec![false; n];
        let mut a = 0;
        while a < n {
            let mut b = a;
            while b < n && on_edge[b] {
                b += 1;
            }
            if b - a >= CLICK_RUN {
                in_run[a..b].iter_mut().for_each(|x| *x = true);
            }
            a = b + 1;
        }
        let mut rs: Vec<f32> = w.elems.iter().map(|e| e.r).collect();
        let median = *rs.select_nth_unstable_by(n / 2, f32::total_cmp).1;
        // A click is about as long as the matched filter; a dit is longer,
        // and a click well under the channel's dit is no dit at all.
        let short = me.timing.map_or(CLICK_LEN * me.l as f32, |t| CLICK_DIT * self.sh.samples(t.dit));
        let mut out: Vec<Elem> = Vec::with_capacity(n);
        for (k, e) in w.elems.into_iter().enumerate() {
            if on_edge[k] && (in_run[k] || e.r < CLICK_SHARE * median || e.mark < short) {
                if let Some(last) = out.last_mut() {
                    last.gap += e.mark + e.gap;
                }
                continue;
            }
            out.push(e);
        }
        Some(out)
    }

    fn new_channel(&mut self, hz: f32, snr_db: f32) {
        let start = self.history.len().saturating_sub(self.history_len);
        let ch = replayed(self.next_id, hz, snr_db, self.now, &self.history[start..], &mut self.scratch, &self.sh);
        self.next_id += 1;
        self.channels.push(ch);
    }

    /// Keep a channel on the focus pitch: on the strongest peak near it,
    /// or right on it.
    fn ensure_focus(&mut self) {
        let Some(f) = self.focus else { return };
        if self.focused().is_some() {
            return;
        }
        let hz = self
            .cands
            .iter()
            .filter(|c| (c.hz - f).abs() < MIN_SEP_HZ + AFC_RANGE_HZ)
            .max_by(|a, b| a.score.total_cmp(&b.score))
            .map_or(f, |c| c.hz);
        if self.channels.iter().any(|c| (c.hz() - hz).abs() < MATCH_HZ) {
            return;
        }
        let snr_db = self.spectral_snr_db(hz);
        self.new_channel(hz, snr_db);
        self.settle();
    }

    /// The average power over a line at `hz` and its keying sidebands, over
    /// the noise in 500 Hz, from the last look at the spectrum. Keyed half
    /// the time, a carrier reads about 3 dB under its key-down SNR here.
    fn spectral_snr_db(&self, hz: f32) -> f32 {
        let bin_hz = self.sh.fs1 / self.fft_len as f32;
        let k = (hz / bin_hz).round() as usize;
        let e: f32 = if k >= 3 && k + 3 < self.excess.len() {
            self.excess[k - 3..=k + 3].iter().map(|v| v.max(0.0)).sum()
        } else {
            0.0
        };
        10.0 * (e * bin_hz / 500.0).max(1e-6).log10()
    }

    /// Find keyed carriers in the averaged spectrum, then match them to the
    /// channels, start channels for new ones, and drop stale channels.
    fn detect(&mut self) {
        self.find_candidates();
        let now = self.now;
        let fs1 = self.sh.fs1;
        let cands = std::mem::take(&mut self.cands);
        for cand in &cands {
            if let Some(ch) = self.channels.iter_mut().find(|c| (c.hz() - cand.hz).abs() < MATCH_HZ) {
                ch.last_seen = now;
                ch.spec_snr_db = cand.snr_db;
                if !ch.active(now, &self.sh) && ch.timing.is_none() && (cand.hz - ch.hz()).abs() > 3.0 {
                    // Not copying yet, and the signal is elsewhere: go to it.
                    ch.retune(cand.hz, &self.sh);
                } else {
                    ch.home_hz = ch.hz();
                }
                continue;
            }
            if self.channels.iter().any(|c| (c.hz() - cand.hz).abs() < MIN_SEP_HZ)
                || self.blocked.iter().any(|b| (b.0 - cand.hz).abs() < MIN_SEP_HZ)
            {
                continue;
            }
            if self.channels.len() >= self.max_channels {
                // Make room only by dropping an idle channel weaker than
                // the newcomer.
                let evict = (EVICT_IDLE_S * fs1) as u64;
                let f = self.focused();
                let victim = self
                    .channels
                    .iter()
                    .enumerate()
                    .filter(|&(i, c)| {
                        Some(i) != f
                            && now.saturating_sub(c.last_word.unwrap_or(c.started)) >= evict
                            && c.snr_db() < cand.snr_db
                    })
                    .min_by(|a, b| a.1.snr_db().total_cmp(&b.1.snr_db()))
                    .map(|(i, _)| i);
                match victim {
                    Some(i) => self.retire(i),
                    None => continue,
                }
            }
            self.new_channel(cand.hz, cand.snr_db);
        }
        self.cands = cands;
        self.blocked.retain(|b| b.1 > now);
        self.ensure_focus();

        // Two channels on one signal: keep the one that has printed more.
        let mut i = 0;
        while i < self.channels.len() {
            let dup = (0..self.channels.len()).any(|j| {
                j != i && (self.channels[i].hz() - self.channels[j].hz()).abs() < DUP_HZ && {
                    let (a, b) = (&self.channels[i], &self.channels[j]);
                    (a.chars, std::cmp::Reverse(a.id)) < (b.chars, std::cmp::Reverse(b.id))
                }
            });
            if dup && self.focused() != Some(i) {
                self.retire(i);
            } else {
                i += 1;
            }
        }

        let idle = (RETIRE_IDLE_S * fs1) as u64;
        let unseen = (RETIRE_UNSEEN_S * fs1) as u64;
        let hard = (RETIRE_HARD_S * fs1) as u64;
        let (lo, hi) = (self.low_hz - MIN_SEP_HZ, self.high_hz + MIN_SEP_HZ);
        let mut i = 0;
        while i < self.channels.len() {
            let c = &self.channels[i];
            let quiet = now.saturating_sub(c.last_word.unwrap_or(c.started));
            let stale = (quiet >= idle && now.saturating_sub(c.last_seen) >= unseen) || quiet >= hard;
            // A channel that has only ever heard a neighbor goes too.
            let ghost = c.ghost_words >= 3 && c.chars == 0;
            if self.focused() != Some(i) && (stale || ghost || c.hz() < lo || c.hz() > hi) {
                if ghost {
                    self.blocked.push((c.hz(), now + (GHOST_BLOCK_S * fs1) as u64));
                }
                self.retire(i);
            } else {
                i += 1;
            }
        }
        while self.channels.len() > self.max_channels {
            let f = self.focused();
            let Some(i) = (0..self.channels.len())
                .filter(|&i| Some(i) != f)
                .min_by_key(|&i| self.channels[i].last_word.unwrap_or(self.channels[i].started))
            else {
                break;
            };
            self.retire(i);
        }
    }

    fn retire(&mut self, i: usize) {
        self.channels[i].finish();
        self.settle();
        self.channels.remove(i);
    }

    fn find_candidates(&mut self) {
        self.cands.clear();
        let bin_hz = self.sh.fs1 / self.fft_len as f32;
        let nb = self.avg.len();
        let klo = ((self.low_hz / bin_hz).ceil() as usize).max(3);
        let khi = ((self.high_hz / bin_hz).floor() as usize).min(nb - 4);
        if khi <= klo {
            return;
        }
        // The floor is taken a little beyond the band, so a signal at its
        // edge is judged against noise either side.
        let margin = (200.0 / bin_hz) as usize;
        let (a, b) = (klo.saturating_sub(margin).max(2), (khi + margin + 1).min(nb));

        // Noise floor per segment: the mean of the bins below twice the
        // 30th percentile. Signals sit above that cut and drop out, and for
        // noise alone the trimmed mean is within a few percent of the mean.
        self.floor.clear();
        self.floor.resize(nb, 0.0);
        let seg = ((FLOOR_SEG_HZ / bin_hz) as usize).max(8);
        let nseg = (b - a).div_ceil(seg).clamp(1, 32);
        let seg = (b - a).div_ceil(nseg);
        let mut centers: [(f32, f32); 32] = [(0.0, 0.0); 32];
        for (s, center) in centers.iter_mut().enumerate().take(nseg) {
            let s0 = (a + s * seg).min(b - 1);
            let s1 = (s0 + seg).min(b);
            self.sorted.clear();
            self.sorted.extend_from_slice(&self.avg[s0..s1]);
            let k = self.sorted.len() * 3 / 10;
            let (_, p30, _) = self.sorted.select_nth_unstable_by(k, f32::total_cmp);
            let cut = 2.0 * *p30;
            let (sum, cnt) =
                self.avg[s0..s1].iter().filter(|&&v| v < cut).fold((0.0f32, 0u32), |(s, n), &v| (s + v, n + 1));
            let f = if cnt > 0 { sum / cnt as f32 } else { cut };
            *center = (0.5 * (s0 + s1) as f32, f.max(1e-30));
        }
        for k in a..b {
            let x = k as f32;
            let j = centers[..nseg].partition_point(|c| c.0 <= x);
            self.floor[k] = if j == 0 {
                centers[0].1
            } else if j >= nseg {
                centers[nseg - 1].1
            } else {
                let (x0, f0) = centers[j - 1];
                let (x1, f1) = centers[j];
                f0 + (f1 - f0) * (x - x0) / (x1 - x0)
            };
        }
        self.excess.clear();
        self.excess.resize(nb, 0.0);
        for k in a..b {
            self.excess[k] = self.avg[k] / self.floor[k] - 1.0;
        }
        self.score.clear();
        self.score.resize(nb, 0.0);
        for k in klo - 1..=khi + 1 {
            self.score[k] = 0.25 * self.excess[k - 1] + 0.5 * self.excess[k] + 0.25 * self.excess[k + 1];
        }
        // Peaks: the highest score within two bins either side.
        let mut peaks: Vec<(usize, f32)> = (klo..=khi)
            .filter(|&k| {
                let s = self.score[k];
                s >= DETECT_SCORE
                    && s > self.score[k - 1]
                    && s >= self.score[k + 1]
                    && s >= self.score[k - 2]
                    && s >= self.score[k + 2]
            })
            .map(|k| (k, self.score[k]))
            .collect();
        peaks.sort_by(|x, y| y.1.total_cmp(&x.1));
        let limit = 2 * self.max_channels + 8;
        for (k, s) in peaks {
            if self.cands.len() >= limit {
                break;
            }
            // Between bins, from a parabola through the logs of the peak
            // and its neighbors, which fits a Hann window's main lobe.
            let (l, m, r) =
                (self.avg[k - 1].max(1e-30).ln(), self.avg[k].max(1e-30).ln(), self.avg[k + 1].max(1e-30).ln());
            let den = l - 2.0 * m + r;
            let off = if den < 0.0 { (0.5 * (l - r) / den).clamp(-0.5, 0.5) } else { 0.0 };
            let hz = (k as f32 + off) * bin_hz;
            let shadowed = self.cands.iter().any(|o| {
                let d = (o.hz - hz).abs();
                d < MIN_SEP_HZ
                    || (d < SIDELOBE_HZ && o.score > s * 10f32.powf(0.1 * (SIDELOBE_DB + SIDELOBE_DB_PER_HZ * d)))
            });
            if shadowed {
                continue;
            }
            let snr_db = self.spectral_snr_db(hz);
            self.cands.push(Candidate { hz, score: s, snr_db });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// The live sideband rate: not a round number.
    const FS: f32 = 47_904.0;

    /// xorshift64*, for noise and fists that are the same on every run.
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
        }
        fn uniform(&mut self) -> f64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
        }
        fn gauss(&mut self) -> f64 {
            let u = self.uniform().max(1e-300);
            let v = self.uniform();
            (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
        }
    }

    /// Text as tokens: a prosign in angle brackets is one.
    fn tokens(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut it = text.chars().peekable();
        while let Some(c) = it.next() {
            if c == '<' {
                let mut t = String::from("<");
                for d in it.by_ref() {
                    t.push(d);
                    if d == '>' {
                        break;
                    }
                }
                out.push(t);
            } else {
                out.push(c.to_string());
            }
        }
        out
    }

    fn pattern(token: &str) -> &'static str {
        MORSE.iter().find(|(_, s)| *s == token).map(|(p, _)| *p).unwrap_or_else(|| panic!("no Morse for {token:?}"))
    }

    /// One station: its text keyed by a human fist.
    #[derive(Clone)]
    struct Tx {
        hz: f64,
        wpm: f64,
        /// Units added to every mark and taken from every gap.
        weight: f64,
        /// A dah's length in units.
        dah: f64,
        /// Each duration is scaled by up to this much either way.
        jitter: f64,
        amp: f64,
        start_s: f64,
        text: String,
        /// Fading: depth in dB either side of the mean, and period.
        qsb: Option<(f64, f64)>,
        drift_hz_per_s: f64,
        seed: u64,
    }

    impl Tx {
        fn new(hz: f64, wpm: f64, amp: f64, text: &str) -> Self {
            Self {
                hz,
                wpm,
                weight: 0.0,
                dah: 3.0,
                jitter: 0.15,
                amp,
                start_s: 0.5,
                text: text.to_string(),
                qsb: None,
                drift_hz_per_s: 0.0,
                seed: hz as u64 * 31 + wpm as u64,
            }
        }

        /// Key-down intervals, seconds from the start of the transmission.
        fn keying(&self) -> Vec<(f64, f64)> {
            let u = 1.2 / self.wpm;
            let mut rng = Rng::new(self.seed);
            let mut j = |d: f64| d * (1.0 + self.jitter * (2.0 * rng.uniform() - 1.0));
            let mut t = 0.0;
            let mut out = Vec::new();
            for (wi, word) in self.text.split(' ').enumerate() {
                if wi > 0 {
                    t += j((7.0 - self.weight) * u);
                }
                for (ci, tok) in tokens(word).iter().enumerate() {
                    if ci > 0 {
                        t += j((3.0 - self.weight) * u);
                    }
                    for (ei, e) in pattern(tok).bytes().enumerate() {
                        if ei > 0 {
                            t += j((1.0 - self.weight) * u);
                        }
                        let len = if e == b'-' { self.dah } else { 1.0 } + self.weight;
                        let d = j(len * u);
                        out.push((t, t + d));
                        t += d;
                    }
                }
            }
            out
        }

        fn duration_s(&self) -> f64 {
            self.keying().last().map_or(0.0, |k| k.1) + 0.01
        }

        /// Add the signal into `out`; returns its key-down power.
        fn render(&self, out: &mut [f32]) -> f64 {
            let fs = FS as f64;
            let edge = 0.005;
            let mut env = vec![0.0f64; ((self.duration_s() + 0.02) * fs) as usize];
            for (on, off) in self.keying() {
                let a = ((on - edge / 2.0) * fs).max(0.0) as usize;
                let b = (((off + edge / 2.0) * fs) as usize).min(env.len());
                for (i, e) in env.iter_mut().enumerate().take(b).skip(a) {
                    let t = i as f64 / fs;
                    let rise = ((t - on) / edge + 0.5).clamp(0.0, 1.0);
                    let fall = ((off - t) / edge + 0.5).clamp(0.0, 1.0);
                    let rc = |x: f64| 0.5 - 0.5 * (std::f64::consts::PI * x).cos();
                    *e = rc(rise) * rc(fall);
                }
            }
            let s0 = (self.start_s * fs) as usize;
            let mut phase = 0.7f64;
            for (i, &e) in env.iter().enumerate() {
                if s0 + i >= out.len() {
                    break;
                }
                let t = i as f64 / fs;
                phase += 2.0 * std::f64::consts::PI * (self.hz + self.drift_hz_per_s * t) / fs;
                if e == 0.0 {
                    continue;
                }
                let g = match self.qsb {
                    None => 1.0,
                    Some((db, period)) => {
                        let x = 2.0 * std::f64::consts::PI * (t + self.start_s) / period;
                        // Two incommensurate swings, so the fades differ.
                        let v = 0.65 * x.sin() + 0.35 * (2.71 * x + 1.0).sin();
                        10f64.powf(db * v / 20.0)
                    }
                };
                out[s0 + i] += (self.amp * g * e * phase.cos()) as f32;
            }
            self.amp * self.amp / 2.0
        }
    }

    /// Noise power per sample (flat to half the rate) that puts a key-down
    /// power `p` at `snr_db` in 500 Hz.
    fn noise_for(p: f64, snr_db: f64) -> f64 {
        p / 10f64.powf(snr_db / 10.0) * (FS as f64 / 2.0) / 500.0
    }

    fn add_noise(out: &mut [f32], power: f64, seed: u64) {
        let mut rng = Rng::new(seed);
        let s = power.sqrt();
        for v in out {
            *v += (rng.gauss() * s) as f32;
        }
    }

    fn audio_for(txs: &[Tx], extra_s: f64) -> (Vec<f32>, Vec<f64>) {
        let dur = txs.iter().map(|t| t.start_s + t.duration_s()).fold(0.0, f64::max) + extra_s;
        let mut out = vec![0.0f32; (dur * FS as f64) as usize];
        let p = txs.iter().map(|t| t.render(&mut out)).collect();
        (out, p)
    }

    struct Run {
        events: Vec<CwEvent>,
        secs: f64,
        audio_s: f64,
    }

    fn run(dec: &mut CwDecoder, audio: &[f32]) -> Run {
        let mut events = Vec::new();
        let t0 = Instant::now();
        for chunk in audio.chunks(958) {
            dec.push(chunk);
            events.extend(dec.take());
        }
        Run { events, secs: t0.elapsed().as_secs_f64(), audio_s: audio.len() as f64 / FS as f64 }
    }

    fn words(ev: &[CwEvent]) -> Vec<(u32, f32, f32, f32, String)> {
        ev.iter()
            .map(|e| match e {
                CwEvent::Text { channel, audio_hz, wpm, snr_db, text } => {
                    (*channel, *audio_hz, *wpm, *snr_db, text.clone())
                }
            })
            .collect()
    }

    /// All text from the channel that printed most near `hz`, joined.
    fn text_near(ev: &[CwEvent], hz: f32) -> (Option<u32>, String) {
        let w = words(ev);
        let mut ids: Vec<u32> = w.iter().filter(|x| (x.1 - hz).abs() < 15.0).map(|x| x.0).collect();
        ids.sort();
        ids.dedup();
        let best = ids.into_iter().max_by_key(|id| w.iter().filter(|x| x.0 == *id).map(|x| x.4.len()).sum::<usize>());
        let s = w.iter().filter(|x| Some(x.0) == best).map(|x| x.4.as_str()).collect::<Vec<_>>().join(" ");
        (best, s)
    }

    fn levenshtein(a: &[String], b: &[String]) -> usize {
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        let mut cur = vec![0; b.len() + 1];
        for i in 1..=a.len() {
            cur[0] = i;
            for j in 1..=b.len() {
                let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
                cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        prev[b.len()]
    }

    /// Character error rate, a prosign one character and spaces counted.
    fn cer(want: &str, got: &str) -> f64 {
        let (a, b) = (tokens(want), tokens(got));
        levenshtein(&a, &b) as f64 / a.len() as f64
    }

    const QSO: &str = "CQ CQ CQ DE W1AW W1AW K K2ABC DE W1AW GM ES TNX FER CALL UR RST 579 579 \
        <BT> NAME IS PAT PAT QTH ALBANY NY <BT> RIG IS 100W TO DIPOLE WX COLD 2C HW? <AR> K2ABC DE W1AW <KN>";

    #[test]
    fn cw_morse_table_is_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for &(p, s) in MORSE {
            assert!(p.len() <= MAX_ELEMS && p.bytes().all(|b| b == b'.' || b == b'-'), "{p}");
            assert!(seen.insert(p), "duplicate {p}");
            assert_eq!(morse_table()[code_of(p)], s);
        }
        for t in tokens(QSO) {
            if t != " " {
                pattern(&t);
            }
        }
    }

    #[test]
    fn cw_timing_learns_uneven_fists() {
        // Marks and gaps from a heavy fist (long dahs, short gaps) and a
        // light one, with jitter, at three speeds.
        for (wpm, weight, dah) in [(12.0, 0.25, 3.8), (25.0, -0.2, 2.6), (40.0, 0.0, 3.0)] {
            let mut tx = Tx::new(700.0, wpm, 0.1, "PARIS CQ TEST 5NN");
            tx.weight = weight;
            tx.dah = dah;
            let k = tx.keying();
            let marks: VecDeque<f32> = k.iter().map(|&(a, b)| (b - a) as f32).collect();
            // The gaps within the words only.
            let gaps: VecDeque<f32> =
                k.windows(2).map(|w| (w[1].0 - w[0].1) as f32).filter(|&g| (g as f64) < 5.0 * 1.2 / wpm).collect();
            let t = estimate(None, &marks, &gaps).expect("timing");
            let u = 1.2 / wpm as f32;
            println!("{wpm} wpm weight {weight} dah {dah}: {t:?}, reads {:.1} wpm", t.wpm());
            assert!((t.dit / u - (1.0 + weight as f32)).abs() < 0.12);
            assert!((t.dah / u - (dah + weight) as f32).abs() < 0.3);
            assert!((t.wpm() - wpm as f32).abs() < 0.1 * wpm as f32);
        }
        // One kind of mark and nothing to compare it with: no guess.
        let marks: VecDeque<f32> = [0.06, 0.062, 0.058].into();
        let gaps: VecDeque<f32> = [0.18, 0.17].into();
        assert!(estimate(None, &marks, &gaps).is_some_and(|t| t.dit < 0.07), "E E E: dits, from the long gaps");
        let gaps: VecDeque<f32> = [0.06, 0.065].into();
        assert!(estimate(None, &marks, &gaps).is_none(), "S or T T T: cannot tell");
    }

    #[test]
    fn cw_single_signal_round_trip() {
        let tx = Tx::new(650.0, 20.0, 0.05, QSO);
        let (mut audio, p) = audio_for(std::slice::from_ref(&tx), 2.0);
        add_noise(&mut audio, noise_for(p[0], 10.0), 1);
        let mut dec = CwDecoder::new(FS);
        let r = run(&mut dec, &audio);
        let (id, got) = text_near(&r.events, 650.0);
        let w = words(&r.events);
        let last = w.iter().rfind(|x| Some(x.0) == id).expect("words");
        println!("single: {id:?} {got:?}\n  last word at {:.1} Hz {:.1} wpm {:.1} dB", last.1, last.2, last.3);
        assert_eq!(got, QSO);
        assert!((last.1 - 650.0).abs() < 2.0);
        assert!((last.2 - 20.0).abs() < 2.0);
        assert!((last.3 - 10.0).abs() < 3.0, "SNR read {}", last.3);
        let st = dec.status();
        println!("{st:?}");
        assert!(st.locked && st.center_hz.is_some_and(|h| (h - 650.0).abs() < 2.0));
    }

    #[test]
    fn cw_several_signals_at_once() {
        // Six stations, each its own speed and fist.
        let spec: [(f64, f64, f64, f64, f64, &str); 6] = [
            (310.0, 12.0, 6.0, 0.2, 3.5, "CQ CQ DE N0CALL N0CALL PSE K"),
            (440.0, 18.0, 15.0, 0.0, 3.0, "UR RST 599 5NN IN OHIO OHIO <BT> OP JOE <KN>"),
            (575.0, 25.0, 3.0, -0.15, 2.7, "TNX FER QSO 73 ES GUD DX <SK> VE3XYZ"),
            (720.0, 32.0, 10.0, 0.1, 3.2, "TEST DE K1ABC K1ABC TEST"),
            (890.0, 40.0, 8.0, 0.0, 3.0, "QRZ? DE DL1XX DL1XX AR 5NN 14"),
            (1080.0, 22.0, 20.0, 0.0, 3.0, "VVV VVV DE W2XYZ BEACON QTH FN20"),
        ];
        let txs: Vec<Tx> = spec
            .iter()
            .enumerate()
            .map(|(i, &(hz, wpm, _, weight, dah, text))| {
                let mut t = Tx::new(hz, wpm, 0.02, text);
                t.weight = weight;
                t.dah = dah;
                t.start_s = 0.4 + 0.9 * i as f64;
                t
            })
            .collect();
        let mut txs = txs;
        // Set each one's level for its SNR against one noise floor.
        let noise = noise_for(0.02 * 0.02 / 2.0, 0.0);
        for (t, s) in txs.iter_mut().zip(&spec) {
            t.amp = (2.0 * noise * 10f64.powf(s.2 / 10.0) * 500.0 / (FS as f64 / 2.0)).sqrt();
        }
        let (mut audio, _) = audio_for(&txs, 2.0);
        add_noise(&mut audio, noise, 2);
        let mut dec = CwDecoder::new(FS);
        let r = run(&mut dec, &audio);
        let mut ids = Vec::new();
        for (t, s) in txs.iter().zip(&spec) {
            let (id, got) = text_near(&r.events, t.hz as f32);
            let w = words(&r.events);
            let wpm = w.iter().rfind(|x| Some(x.0) == id).map_or(0.0, |x| x.2);
            println!(
                "{:>5} Hz {:>2} wpm {:>3} dB: {id:?} reads {wpm:.1} wpm, CER {:.1}% {got:?}",
                t.hz,
                t.wpm,
                s.2,
                100.0 * cer(&t.text, &got)
            );
            assert!(cer(&t.text, &got) < 0.02, "at {} Hz: {got:?}", t.hz);
            assert!((wpm - t.wpm as f32).abs() < 0.15 * t.wpm as f32);
            ids.push(id);
        }
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), spec.len());
        // Nothing else printed anything: no leakage copies, no noise.
        let stray: Vec<_> = words(&r.events).into_iter().filter(|w| !ids.contains(&Some(w.0))).collect();
        println!("stray: {stray:?}");
        assert!(stray.is_empty());
    }

    #[test]
    fn cw_strong_neighbor_is_not_copied_twice() {
        // A loud station (40 dB over the noise in 500 Hz) and one 20 dB
        // weaker 120 Hz above it, sending at once. The loud one's edges
        // click across the band and its sidebands reach the weak one's
        // pitch; neither may come out as a second copy of it.
        let mut a = Tx::new(600.0, 28.0, 0.3, "CQ TEST DE W1AW W1AW TEST");
        a.start_s = 0.5;
        let mut b = Tx::new(720.0, 20.0, 0.03, "K2ABC 5NN 05 K2ABC");
        b.start_s = 1.5;
        let (mut audio, p) = audio_for(&[a.clone(), b.clone()], 2.0);
        add_noise(&mut audio, noise_for(p[0], 40.0), 3);
        let mut dec = CwDecoder::new(FS);
        let r = run(&mut dec, &audio);
        let (ia, ga) = text_near(&r.events, 600.0);
        let (ib, gb) = text_near(&r.events, 720.0);
        println!("loud {ia:?} {ga:?}\nweak {ib:?} {gb:?}\nall {:?}", words(&r.events));
        assert_eq!(ga, a.text);
        // Its first word is learned from while the clicks are still in it;
        // after that it copies clean.
        assert!(gb.ends_with("5NN 05 K2ABC") && cer(&b.text, &gb) < 0.2, "{gb:?}");
        let stray: Vec<_> = words(&r.events).into_iter().filter(|w| Some(w.0) != ia && Some(w.0) != ib).collect();
        assert!(stray.is_empty(), "{stray:?}");
    }

    #[test]
    fn cw_qsb_fading() {
        let text = QSO.to_string() + " " + QSO;
        let mut tx = Tx::new(820.0, 22.0, 0.05, &text);
        tx.qsb = Some((10.0, 7.0));
        let (mut audio, p) = audio_for(std::slice::from_ref(&tx), 2.0);
        add_noise(&mut audio, noise_for(p[0], 14.0), 4);
        let mut dec = CwDecoder::new(FS);
        let r = run(&mut dec, &audio);
        let (_, got) = text_near(&r.events, 820.0);
        let c = cer(&text, &got);
        println!("QSB +-10 dB at 14 dB mean: CER {:.2}% {got:?}", 100.0 * c);
        assert!(c < 0.03);
    }

    /// CER at `snr_db` in 500 Hz on a plain channel.
    fn cer_at(snr_db: f64, wpm: f64, seed: u64) -> f64 {
        let text = [QSO; 3].join(" ");
        let mut tx = Tx::new(700.0, wpm, 0.01, &text);
        tx.seed = seed;
        let (mut audio, p) = audio_for(std::slice::from_ref(&tx), 2.0);
        add_noise(&mut audio, noise_for(p[0], snr_db), seed);
        let mut dec = CwDecoder::new(FS);
        let r = run(&mut dec, &audio);
        let (_, got) = text_near(&r.events, 700.0);
        cer(&text, &got)
    }

    #[test]
    fn cw_awgn_character_error_rate() {
        // The SNR in 500 Hz at which the character error rate falls under
        // 2%, at 20 and 30 WPM with a ±15% fist.
        let snrs = [6.0, 3.0, 1.0, -1.0, -3.0];
        for wpm in [20.0, 30.0] {
            let mut row = Vec::new();
            for &s in &snrs {
                row.push((s, cer_at(s, wpm, 5)));
            }
            let under = row.iter().filter(|r| r.1 < 0.02).map(|r| r.0).fold(f64::INFINITY, f64::min);
            println!(
                "{wpm} wpm: {} -> CER < 2% down to {under} dB in 500 Hz",
                row.iter().map(|(s, c)| format!("{s:+.0} dB {:.1}%", 100.0 * c)).collect::<Vec<_>>().join(", ")
            );
            assert!(row[0].1 < 0.01 && row[1].1 < 0.02, "{row:?}");
        }
    }

    #[test]
    fn cw_impulse_noise() {
        // Static crashes: 1 ms bursts 30 to 100 times the signal's
        // amplitude, about ten a second.
        let crashes = |audio: &mut [f32], amp: f64, seed: u64| {
            let mut rng = Rng::new(seed);
            let fs = FS as f64;
            let mut t = 0.0;
            loop {
                t += -rng.uniform().max(1e-9).ln() / 10.0;
                let s = (t * fs) as usize;
                if s + 64 >= audio.len() {
                    break;
                }
                let a = amp * (30.0 + 70.0 * rng.uniform());
                for i in 0..(0.001 * fs) as usize {
                    let decay = (-(i as f64) / (0.0003 * fs)).exp();
                    audio[s + i] += (a * decay * rng.gauss()) as f32;
                }
            }
        };
        let tx = Tx::new(560.0, 24.0, 0.02, QSO);
        let (mut audio, p) = audio_for(std::slice::from_ref(&tx), 2.0);
        add_noise(&mut audio, noise_for(p[0], 8.0), 6);
        crashes(&mut audio, 0.02, 7);
        let mut dec = CwDecoder::new(FS);
        let r = run(&mut dec, &audio);
        let (_, got) = text_near(&r.events, 560.0);
        let c = cer(QSO, &got);
        println!("crashes: CER {:.2}%, {} samples blanked, {got:?}", 100.0 * c, dec.blanker.blanked);
        assert!(c < 0.02);
        let others: Vec<_> = words(&r.events).into_iter().filter(|w| (w.1 - 560.0).abs() > 15.0).collect();
        assert!(others.is_empty(), "{others:?}");

        // Crashes on noise alone print nothing.
        let mut audio = vec![0.0f32; 40 * FS as usize];
        add_noise(&mut audio, noise_for(p[0], 8.0), 8);
        crashes(&mut audio, 0.02, 9);
        let mut dec = CwDecoder::new(FS);
        let r = run(&mut dec, &audio);
        println!("crashes alone: {:?}", words(&r.events));
        assert!(r.events.is_empty());
    }

    #[test]
    fn cw_noise_alone_prints_nothing() {
        let mut audio = vec![0.0f32; 60 * FS as usize];
        add_noise(&mut audio, 1e-3, 10);
        let mut dec = CwDecoder::new(FS);
        // A channel held on noise the whole time as well.
        dec.set_pitch_focus(Some(777.0));
        let r = run(&mut dec, &audio);
        println!("noise: {:?}, channels {:?}", words(&r.events), dec.channels());
        assert!(r.events.is_empty());
        assert!(dec.channels().iter().any(|c| c.focused && (c.audio_hz - 777.0).abs() < 1.0));
        assert!(!dec.status().locked);
    }

    #[test]
    fn cw_tracks_drift_and_offset() {
        // 30 Hz of drift over the transmission.
        let text = [QSO; 2].join(" ");
        let mut tx = Tx::new(500.0, 25.0, 0.02, &text);
        tx.drift_hz_per_s = 30.0 / tx.duration_s();
        let (mut audio, p) = audio_for(std::slice::from_ref(&tx), 2.0);
        add_noise(&mut audio, noise_for(p[0], 8.0), 11);
        let mut dec = CwDecoder::new(FS);
        let r = run(&mut dec, &audio);
        let w = words(&r.events);
        let mut ids: Vec<u32> = w.iter().map(|x| x.0).collect();
        ids.dedup();
        let got = w.iter().map(|x| x.4.as_str()).collect::<Vec<_>>().join(" ");
        let end_hz = w.last().map_or(0.0, |x| x.1);
        println!("drift: channels {ids:?}, ends at {end_hz:.1} Hz, CER {:.2}%", 100.0 * cer(&text, &got));
        assert!(cer(&text, &got) < 0.02, "{got:?}");
        assert!((end_hz - 530.0).abs() < 4.0);
    }

    #[test]
    fn cw_realtime_factor() {
        // Ten stations across the default band.
        let txs: Vec<Tx> = (0..10)
            .map(|i| {
                let mut t = Tx::new(240.0 + 95.0 * i as f64, 16.0 + 2.5 * i as f64, 0.02, QSO);
                t.start_s = 0.3 * i as f64;
                t
            })
            .collect();
        let (mut audio, p) = audio_for(&txs, 2.0);
        add_noise(&mut audio, noise_for(p[0], 10.0), 12);
        let mut dec = CwDecoder::new(FS);
        let r = run(&mut dec, &audio);
        let mut decoded = 0;
        for t in &txs {
            let (_, got) = text_near(&r.events, t.hz as f32);
            if cer(QSO, &got) < 0.02 {
                decoded += 1;
            } else {
                println!("{} Hz: {got:?}", t.hz);
            }
        }
        let core = 100.0 * r.secs / r.audio_s;
        println!(
            "10 stations: {:.1} s of audio in {:.3} s, {:.0}x realtime ({core:.2}% of a core), {decoded}/10 decoded",
            r.audio_s,
            r.secs,
            r.audio_s / r.secs
        );
        assert_eq!(decoded, 10);
        if !cfg!(debug_assertions) {
            assert!(core < 5.0);
        }
    }

    #[test]
    fn cw_speed_extremes() {
        // The slow end, where a dah is 450 ms, and the fast end.
        for (wpm, text) in [(8.0, "CQ DE W1AW K"), (48.0, "CQ TEST DE W1AW W1AW TEST 5NN 14 TU")] {
            let tx = Tx::new(640.0, wpm, 0.02, text);
            let (mut audio, p) = audio_for(std::slice::from_ref(&tx), 3.0);
            add_noise(&mut audio, noise_for(p[0], 12.0), 13);
            let mut dec = CwDecoder::new(FS);
            let r = run(&mut dec, &audio);
            let (id, got) = text_near(&r.events, 640.0);
            let w = words(&r.events);
            let read = w.iter().rfind(|x| Some(x.0) == id).map_or(0.0, |x| x.2);
            println!("{wpm} wpm: reads {read:.1} wpm, {got:?}");
            assert_eq!(got, text);
            assert!((read - wpm as f32).abs() < 0.1 * wpm as f32);
        }
    }

    #[test]
    fn cw_level_does_not_matter() {
        // The same signal and noise scaled over 120 dB read the same.
        let tx = Tx::new(930.0, 24.0, 1.0, QSO);
        let (mut base, p) = audio_for(std::slice::from_ref(&tx), 2.0);
        add_noise(&mut base, noise_for(p[0], 8.0), 14);
        for scale in [1e-5f32, 1.0, 1e1] {
            let audio: Vec<f32> = base.iter().map(|v| v * scale).collect();
            let mut dec = CwDecoder::new(FS);
            let r = run(&mut dec, &audio);
            let (_, got) = text_near(&r.events, 930.0);
            println!("scale {scale:e}: {got:?}");
            assert_eq!(got, QSO);
        }
    }

    /// The tables for the weak-signal work: CER by SNR in 500 Hz over three
    /// fists and seeds, plain and fading, and noise alone for ten minutes.
    /// Slow; run it by name.
    #[test]
    #[ignore]
    fn cw_weak_signal_table() {
        let snrs = [6.0, 3.0, 1.0, -1.0];
        println!("CER %            {}", snrs.map(|s| format!("{s:>+7.0}")).join(""));
        for wpm in [15.0, 25.0, 35.0] {
            for qsb in [None, Some((10.0, 7.0))] {
                let mut row = String::new();
                for &snr in &snrs {
                    let mut c = 0.0;
                    for seed in 21..24 {
                        let text = [QSO; 2].join(" ");
                        let mut tx = Tx::new(700.0, wpm, 0.01, &text);
                        tx.seed = seed;
                        tx.qsb = qsb;
                        tx.weight = [0.0, 0.2, -0.15][seed as usize % 3];
                        let (mut audio, p) = audio_for(std::slice::from_ref(&tx), 2.0);
                        add_noise(&mut audio, noise_for(p[0], snr), seed);
                        let mut dec = CwDecoder::new(FS);
                        let r = run(&mut dec, &audio);
                        c += cer(&text, &text_near(&r.events, 700.0).1);
                    }
                    row += &format!("{:>7.1}", 100.0 * c / 3.0);
                }
                println!("{wpm} wpm {:<8} {row}", if qsb.is_some() { "QSB" } else { "AWGN" });
            }
        }
        let mut chars = 0;
        for seed in 0..10 {
            let mut audio = vec![0.0f32; 60 * FS as usize];
            add_noise(&mut audio, 1e-2, 100 + seed);
            let mut dec = CwDecoder::new(FS);
            dec.set_pitch_focus(Some(500.0 + 50.0 * seed as f32));
            let r = run(&mut dec, &audio);
            chars += words(&r.events).iter().map(|w| w.4.len()).sum::<usize>();
        }
        println!("noise alone, 10 minutes: {chars} characters");
    }

    /// An I/Q capture (WAV, two int16 channels, the dial at 0 Hz) as
    /// upper-sideband audio: +100 to +3100 Hz kept, the rest zeroed, twice
    /// the real part of what is left.
    fn usb_from_iq_wav(path: &std::path::Path) -> Option<(f32, Vec<f32>)> {
        let b = std::fs::read(path).ok()?;
        if b.len() < 44 || &b[..4] != b"RIFF" || &b[8..12] != b"WAVE" {
            return None;
        }
        let (mut at, mut fs) = (12, 0u32);
        while at + 8 <= b.len() {
            let len = u32::from_le_bytes(b[at + 4..at + 8].try_into().ok()?) as usize;
            let body = &b[at + 8..(at + 8 + len).min(b.len())];
            if &b[at..at + 4] == b"fmt " {
                fs = u32::from_le_bytes(body[4..8].try_into().ok()?);
            } else if &b[at..at + 4] == b"data" {
                let mut x: Vec<Complex32> = body
                    .chunks_exact(4)
                    .map(|c| {
                        let i = i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0;
                        let q = i16::from_le_bytes([c[2], c[3]]) as f32 / 32768.0;
                        Complex32::new(i, q)
                    })
                    .collect();
                let n = x.len();
                let mut planner = FftPlanner::new();
                planner.plan_fft_forward(n).process(&mut x);
                let bin = fs as f32 / n as f32;
                for (k, v) in x.iter_mut().enumerate() {
                    let f = k as f32 * bin;
                    if !(100.0..=3100.0).contains(&f) {
                        *v = Complex32::new(0.0, 0.0);
                    }
                }
                planner.plan_fft_inverse(n).process(&mut x);
                return Some((fs as f32, x.iter().map(|v| 2.0 * v.re / n as f32).collect()));
            }
            at += 8 + len + (len & 1);
        }
        None
    }

    /// G.711 µ-law, so a fixture of sideband audio costs a byte a sample.
    fn ulaw_encode(x: f32) -> u8 {
        let v = (x.clamp(-1.0, 1.0) * 32124.0) as i32;
        let sign = if v < 0 { 0x80 } else { 0 };
        let m = v.unsigned_abs() as i32 + 132;
        let exp = (0..8).rev().find(|&e| m & (0x80 << e) != 0).unwrap_or(0);
        let mant = (m >> (exp + 3)) & 0x0f;
        !(sign | (exp << 4) as u8 | mant as u8)
    }

    fn ulaw_decode(b: u8) -> f32 {
        let b = !b;
        let exp = ((b >> 4) & 7) as i32;
        let m = ((((b & 0x0f) as i32) << 3) + 132) << exp;
        let v = (m - 132) as f32 / 32124.0;
        if b & 0x80 != 0 { -v } else { v }
    }

    /// The I/Q capture at `path` as upper-sideband audio at 8 kHz, its first
    /// `secs` seconds, peak at 0.9, µ-law.
    fn ulaw_fixture(path: &std::path::Path, secs: f32) -> Option<Vec<u8>> {
        let b = std::fs::read(path).ok()?;
        let data = b.windows(4).position(|w| w == b"data")? + 8;
        let fs = 47_904.0f32;
        // A whole number of 5988-sample blocks, which at 8 kHz are 1000:
        // the bins of both transforms then line up exactly.
        let n = ((secs * fs) as usize / 5988) * 5988;
        let m = n / 5988 * 1000;
        let mut x: Vec<Complex32> = b[data..]
            .chunks_exact(4)
            .take(n)
            .map(|c| Complex32::new(i16::from_le_bytes([c[0], c[1]]) as f32, i16::from_le_bytes([c[2], c[3]]) as f32))
            .collect();
        if x.len() < n {
            return None;
        }
        let mut planner = FftPlanner::new();
        planner.plan_fft_forward(n).process(&mut x);
        let bin = fs / n as f32;
        let mut y = vec![Complex32::new(0.0, 0.0); m];
        for (k, v) in x.iter().enumerate().take(m / 2) {
            if (100.0..=3100.0).contains(&(k as f32 * bin)) && k > 0 {
                y[k] = *v;
                y[m - k] = v.conj();
            }
        }
        planner.plan_fft_inverse(m).process(&mut y);
        let peak = y.iter().map(|v| v.re.abs()).fold(0.0, f32::max).max(1e-30);
        Some(y.iter().map(|v| ulaw_encode(0.9 * v.re / peak)).collect())
    }

    /// Writes the fixtures in `tests/data/cw/` from the captures in
    /// `USDR_CW_LIVE`: 40 m at 7029.3 kHz (two CW stations among carriers
    /// and data tones) and 7078 kHz (JS8, no Morse at all).
    #[test]
    #[ignore]
    fn cw_make_live_fixtures() {
        let Ok(dir) = std::env::var("USDR_CW_LIVE") else { return };
        let out = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/cw/");
        std::fs::create_dir_all(out).unwrap();
        for (name, secs) in [("cw-7029-1", 20.0), ("js8-7078-2", 12.0)] {
            let f = ulaw_fixture(&std::path::Path::new(&dir).join(format!("{name}.wav")), secs).unwrap();
            std::fs::write(format!("{out}{name}.ul"), f).unwrap();
        }
    }

    /// Decode a fixture (8 kHz µ-law) over 150 to 3000 Hz, as the server
    /// first ran it on the air: the words, by channel.
    fn decode_fixture(name: &str) -> Vec<(u32, f32, f32, f32, String)> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/cw/").to_owned() + name;
        let audio: Vec<f32> = std::fs::read(path).unwrap().into_iter().map(ulaw_decode).collect();
        let mut dec = CwDecoder::new(8000.0);
        dec.set_band(150.0, 3000.0);
        let mut ev = Vec::new();
        for chunk in audio.chunks(160) {
            dec.push(chunk);
            ev.extend(dec.take());
        }
        dec.push(&vec![0.0; 24_000]);
        ev.extend(dec.take());
        for c in dec.channels() {
            assert!(c.wpm.is_none_or(|w| (WPM_MIN..=WPM_MAX).contains(&w)), "{c:?}");
        }
        words(&ev)
    }

    #[test]
    fn cw_live_band_prints_only_morse() {
        // 40 m at 7029.3 kHz, 23:30 UTC: AD4HB at 670 Hz calling KP3U, and
        // KP3U at 800 Hz, among carriers, data tones and splatter that the
        // first version read as 70 WPM runs of E and T.
        let w = decode_fixture("cw-7029-1.ul");
        for x in &w {
            println!("ch {} {:.0} Hz {:.0} wpm {:.0} dB: {}", x.0, x.1, x.2, x.3, x.4);
        }
        let at =
            |hz: f32| w.iter().filter(|x| (x.1 - hz).abs() < 15.0).map(|x| x.4.as_str()).collect::<Vec<_>>().join(" ");
        assert!(at(670.0).starts_with("AD4HB AD4HB K"), "{:?}", at(670.0));
        assert!(at(800.0).starts_with("KP3U K"), "{:?}", at(800.0));
        let stray: Vec<_> = w.iter().filter(|x| (x.1 - 670.0).abs() >= 15.0 && (x.1 - 800.0).abs() >= 15.0).collect();
        assert!(stray.is_empty(), "{stray:?}");

        // 7078 kHz: JS8 across the passband, nothing that is Morse.
        let w = decode_fixture("js8-7078-2.ul");
        assert!(w.is_empty(), "{w:?}");
    }

    /// Real captures from the air. Run with `USDR_CW_LIVE=<dir> cargo test
    /// --release -p scannerd-engine cw_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn cw_live_recordings() {
        let Ok(dir) = std::env::var("USDR_CW_LIVE") else { return };
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.ends_with(".wav"))
            .collect();
        names.sort();
        for name in names {
            let Some((fs, audio)) = usb_from_iq_wav(&std::path::Path::new(&dir).join(&name)) else { continue };
            for band in [(150.0, 3000.0), (200.0, 1200.0)] {
                let mut dec = CwDecoder::new(fs);
                dec.set_band(band.0, band.1);
                let mut ev = Vec::new();
                for chunk in audio.chunks((fs * 0.02) as usize) {
                    dec.push(chunk);
                    ev.extend(dec.take());
                }
                dec.push(&vec![0.0; (3.0 * fs) as usize]);
                ev.extend(dec.take());
                let w = words(&ev);
                let chars: usize = w.iter().map(|x| tokens(&x.4).len()).sum();
                println!("== {name} band {band:?}: {:.1} s, {} words, {chars} chars", audio.len() as f32 / fs, w.len());
                let mut ids: Vec<u32> = w.iter().map(|x| x.0).collect();
                ids.sort();
                ids.dedup();
                for id in ids {
                    let mine: Vec<_> = w.iter().filter(|x| x.0 == id).collect();
                    let text = mine.iter().map(|x| x.4.as_str()).collect::<Vec<_>>().join(" ");
                    println!("  ch {id:>2} {:7.1} Hz {:4.0} wpm {:5.1} dB: {text}", mine[0].1, mine[0].2, mine[0].3);
                }
                let tracked: Vec<_> = dec
                    .channels()
                    .iter()
                    .map(|c| (c.audio_hz as i32, c.snr_db as i32, c.wpm.map(|w| w as i32)))
                    .collect();
                println!("  tracked at the end: {tracked:?}");
            }
        }
    }
}
