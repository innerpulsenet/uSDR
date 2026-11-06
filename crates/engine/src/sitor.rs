//! SITOR (ITU-R M.476 / M.625): the maritime direct-printing telegraph, 100
//! baud FSK with 170 Hz shift in the 7-unit CCIR 476 code, whose 35 valid
//! words each have four B (upper tone) and three Y bits. It is sent two
//! ways, and this decoder copies both as a third party:
//!
//! - **Mode B (FEC)** is one station broadcasting a continuous stream in
//!   which every character goes out twice, in a DX slot and again four
//!   characters later in an RX slot. It is NAVTEX's mode, and the same
//!   machinery is used: the RTTY decoder's [`FskReceiver`] finds and
//!   demodulates the pair, a free-running bit clock is pulled toward the
//!   soft decision's zero crossings, the character alignment is the one of
//!   seven bit offsets where words pass the 4B/3Y test far more often than
//!   chance, and the two copies' soft bits are combined by
//!   [`crate::navtex`]'s maximum-likelihood rule. Unlike NAVTEX there is no
//!   ZCZC/NNNN framing: text comes out line by line as it is copied. A
//!   selective B emission (one addressed to particular stations) sends its
//!   phasing normally and then the call and all its traffic in inverted
//!   ratio, three B and four Y. The polarity of each word is followed, so
//!   the inversion is seen, the address is read from the call signal (the
//!   station's identification signals, then an idle signal beta, six
//!   times over) and the traffic decodes as well.
//! - **Mode A (ARQ)** is two stations taking turns on one frequency in a
//!   450 ms cycle. The information sending station (ISS) sends three
//!   characters in a 210 ms block; the information receiving station (IRS)
//!   answers each block with one 70 ms control signal: CS1 or CS2 to ask
//!   for the next block, the same one again to ask for a repeat, CS3 to
//!   ask to send. As a third party both may be heard, often at very
//!   different strengths and on slightly different tones.
//!
//! Mode A is copied a cycle at a time from the last few seconds of the
//! demodulator's output. Every position in the 450 ms cycle, at the
//! demodulator's sample spacing, is scored by whether a 210 ms burst of
//! signal starts there and its three words pass the 4B/3Y test, and the
//! same for a single 70 ms word; the scores are averaged over a few cycles,
//! so the block and the control signal are found by where valid words keep
//! turning up rather than by which is louder (the IRS can be the stronger
//! station). "Signal" is judged against the noise in the cycle's quiet
//! part, so the score of a weak station's block is as clear as a strong
//! one's. Each burst then gets its own tone levels and log-likelihood
//! ratios, so a control signal 15 dB down on the block is read on its own
//! terms.
//!
//! Above the bursts sits an observer of the M.625 protocol: call blocks
//! give the called station's selective call (four or seven identification
//! signals, `X1 RQ X2`, `X3 X4 RQ` or `RQ X3 X4` and `X5 X6 X7`), the
//! automatic identification blocks give the caller's own seven-signal
//! identity (checked against the check-sum signals the called station
//! returns), information blocks are held until the next block shows
//! whether the IRS asked for a repeat (the same control signal twice), in
//! which case the copies are combined and the block printed once, `β α β`
//! marks a change of direction, and `α α α` the end of the link.
//!
//! Mode A or B is told by timing: bursts on a 450 ms cycle with quiet gaps
//! between, or a continuous stream with the DX/RX interleave. Either can
//! be forced with [`SitorDecoder::set_mode`].
//!
//! A character that fails the 4B/3Y check, in every copy there is, prints
//! as `*`.

use crate::navtex::{
    CCIR_INDEX, CLOCK_GAIN_HUNT, CLOCK_GAIN_SYNC, FADE_HOLD_S, HELD_MAX, IDLE_BETA, LLR_MAX, ML_MARGIN, PHASING1, PHASING2,
    SYNC_LEAD, SYNC_LEN, SYNC_LOSE, SYNC_LOSE_BITS, SYNC_MIN_WORDS, SYNC_TAKE, SYNC_TAKE_WEAK, VALID, Word, combine, valid,
};
use crate::rtty::{Atc, FskReceiver, FskSample, IDLE_FLUSH_S, Lines, SYMS, Shift};

const BAUD: f32 = 100.0;
const SHIFT_HZ: f32 = 170.0;
const TINY: f32 = 1e-30;

/// The service signals (M.625 Table 2). Idle signal alpha is also mode
/// B's phasing signal 1, and signal repetition (RQ) its phasing signal 2.
const ALPHA: u8 = PHASING1;
const BETA: u8 = IDLE_BETA;
const RQ: u8 = PHASING2;

/// Bits in transmission order, B as 1.
const fn pattern(p: &[u8; 7]) -> u8 {
    let mut v = 0u8;
    let mut i = 0;
    while i < 7 {
        if p[i] == b'B' {
            v |= 1 << i;
        }
        i += 1;
    }
    v
}

/// The IRS's control signals. CS1 to CS3 share their words with L, the
/// "no information" combination and N; CS4 and CS5 (M.625 only) with G
/// and H. None of them is an identification signal, so a check-sum
/// signal is never mistaken for one.
const CS1: u8 = pattern(b"BYBYYBB");
const CS2: u8 = pattern(b"YBYBYBB");
const CS3: u8 = pattern(b"BYYBBYB");
const CS4: u8 = pattern(b"BYBYBBY");
const CS5: u8 = pattern(b"BYYBYBB");

fn control_name(code: u8) -> &'static str {
    match code {
        CS1 => "CS1",
        CS2 => "CS2",
        CS3 => "CS3",
        CS4 => "CS4",
        CS5 => "CS5",
        RQ => "RQ",
        _ => "other",
    }
}

/// The identification signals (M.625 Table 3a), at their equivalent
/// numbers: V is 0, A is 19. The check-sum signals use the same mapping.
const ID_LETTERS: [char; 20] =
    ['V', 'X', 'Q', 'K', 'M', 'P', 'C', 'Y', 'F', 'S', 'T', 'B', 'U', 'E', 'O', 'I', 'R', 'Z', 'D', 'A'];

fn id_number(c: char) -> Option<usize> {
    ID_LETTERS.iter().position(|&x| x == c)
}

/// The letter-case meaning of a word, when it is a letter.
fn letter_of(code: u8) -> Option<char> {
    let row = CCIR_INDEX[code as usize & 0x7f];
    (row != 255).then(|| SYMS[row as usize].letter).filter(char::is_ascii_uppercase)
}

/// The identification signal a word is, if any.
fn id_letter(code: u8) -> Option<char> {
    letter_of(code).filter(|&c| id_number(c).is_some())
}

/// The three check-sum signals of a seven-signal identity (M.625 § 2.5).
fn checksums(id: &[char]) -> Option<[char; 3]> {
    let n: Vec<usize> = id.iter().map(|&c| id_number(c)).collect::<Option<_>>()?;
    if n.len() != 7 {
        return None;
    }
    let ck = |a: usize, b: usize, c: usize| ID_LETTERS[(n[a] + n[b] + n[c]) % 20];
    Some([ck(0, 1, 2), ck(2, 3, 4), ck(4, 5, 6)])
}

/// The most likely of `set` given soft bits, when it stands out by
/// `margin` (twice the difference of log-likelihoods, as in NAVTEX).
fn ml_among(soft: &[f32; 7], set: &[u8], margin: f32) -> Option<u8> {
    let (mut best, mut best_m, mut second_m) = (0u8, f32::MIN, f32::MIN);
    for &c in set {
        let m: f32 = soft.iter().enumerate().map(|(k, s)| if c >> k & 1 == 1 { *s } else { -*s }).sum();
        if m > best_m {
            second_m = best_m;
            best_m = m;
            best = c;
        } else if m > second_m {
            second_m = m;
        }
    }
    (best_m - second_m >= margin).then_some(best)
}

/// A word with its bits inverted, as a 3B/4Y word is read in 4B/3Y terms.
fn invert(w: &Word) -> Word {
    let mut soft = w.soft;
    soft.iter_mut().for_each(|s| *s = -*s);
    let code = !w.code & 0x7f;
    Word { code, valid: valid(code), soft }
}

fn normalize(w: &Word, pol: usize) -> Word {
    if pol == 1 { invert(w) } else { Word { valid: valid(w.code), ..*w } }
}

// ---------------------------------------------------------------------------
// Public types.

/// Which SITOR mode to copy, or which was found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SitorMode {
    /// Decide from the timing.
    #[default]
    Auto,
    /// ARQ: blocks and control signals on a 450 ms cycle.
    A,
    /// FEC: a continuous stream with time diversity.
    B,
}

impl SitorMode {
    pub fn label(self) -> &'static str {
        match self {
            SitorMode::Auto => "auto",
            SitorMode::A => "A",
            SitorMode::B => "B",
        }
    }
}

/// One of the two stations of a mode A link. Which one called is known
/// when the call was heard; otherwise they are numbered in the order they
/// were first heard sending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArqSide {
    /// The calling station (the master, whose clock times the link).
    Caller,
    /// The called station.
    Called,
    /// The station heard sending when the decoder joined a link whose
    /// call it did not hear.
    First,
    /// The other one.
    Second,
}

impl ArqSide {
    pub fn label(self) -> &'static str {
        match self {
            ArqSide::Caller => "caller",
            ArqSide::Called => "called",
            ArqSide::First => "first",
            ArqSide::Second => "second",
        }
    }

    fn of(index: u8, roles_known: bool) -> Self {
        match (roles_known, index) {
            (true, 0) => ArqSide::Caller,
            (true, _) => ArqSide::Called,
            (false, 0) => ArqSide::First,
            (false, _) => ArqSide::Second,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SitorEvent {
    /// A line of copy.
    Text {
        mode: SitorMode,
        /// Mode A: which station sent it.
        from: Option<ArqSide>,
        /// Mode A: that station's identity, when the link setup gave it.
        station: Option<String>,
        /// Mode B: sent in inverted ratio to selected stations.
        selective: bool,
        text: String,
    },
    /// Mode A: a call signal, with the called station's selective call.
    Call { selcall: String },
    /// Mode A: the called station answered the call.
    Answer {
        /// CS1/CS2 (four-signal call), CS4 (seven-signal call, automatic
        /// identification follows) or CS5 (rephasing).
        control: &'static str,
        rephase: bool,
    },
    /// Mode A: the caller identified itself (seven-signal procedure).
    Identity {
        station: String,
        /// Whether the check-sum signals the called station returned match
        /// the selective call heard; None when either was not heard whole.
        checksum_ok: Option<bool>,
    },
    /// Mode A: the direction of the traffic changed.
    Changeover {
        now_sending: ArqSide,
        /// "irs" when the IRS asked with CS3, "iss" when the ISS offered
        /// with "+?"; None when not heard.
        requested_by: Option<&'static str>,
        /// The change-over block was not copied; the change was inferred
        /// from the blocks moving to the other station's place in the cycle.
        inferred: bool,
    },
    /// Mode A: the link ended.
    LinkEnd {
        /// "endOfCommunication" (α α α), or "lost" (the blocks stopped).
        reason: &'static str,
    },
    /// Mode B: phasing heard, an emission is starting.
    Phasing,
    /// Mode B: the address of a selective emission.
    SelectiveCall { selcall: String },
    /// Mode B: the end-of-emission idle signals alpha.
    EndOfEmission,
}

impl SitorEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            SitorEvent::Text { .. } => "text",
            SitorEvent::Call { .. } => "call",
            SitorEvent::Answer { .. } => "answer",
            SitorEvent::Identity { .. } => "identity",
            SitorEvent::Changeover { .. } => "changeover",
            SitorEvent::LinkEnd { .. } => "linkEnd",
            SitorEvent::Phasing => "phasing",
            SitorEvent::SelectiveCall { .. } => "selectiveCall",
            SitorEvent::EndOfEmission => "endOfEmission",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            SitorEvent::Text { mode: SitorMode::A, from, station, text, .. } => {
                let who = from.map_or("?", ArqSide::label);
                match station {
                    Some(s) => format!("{who} {s}: {text}"),
                    None => format!("{who}: {text}"),
                }
            }
            SitorEvent::Text { selective: true, text, .. } => format!("selective: {text}"),
            SitorEvent::Text { text, .. } => text.clone(),
            SitorEvent::Call { selcall } => format!("Call to {selcall} ({}-signal)", selcall.chars().count()),
            SitorEvent::Answer { control, rephase } => {
                format!("Called station answers {control}{}", if *rephase { " (rephasing)" } else { "" })
            }
            SitorEvent::Identity { station, checksum_ok } => match checksum_ok {
                Some(true) => format!("Caller is {station}, check-sums match"),
                Some(false) => format!("Caller is {station}, check-sums do not match"),
                None => format!("Caller is {station}"),
            },
            SitorEvent::Changeover { now_sending, inferred, .. } => {
                format!("Over: {} now sending{}", now_sending.label(), if *inferred { " (inferred)" } else { "" })
            }
            SitorEvent::LinkEnd { reason } => match *reason {
                "lost" => "Link lost".into(),
                _ => "End of communication".into(),
            },
            SitorEvent::Phasing => "Phasing".into(),
            SitorEvent::SelectiveCall { selcall } => format!("Selective call to {selcall}"),
            SitorEvent::EndOfEmission => "End of emission".into(),
        }
    }

    pub fn fields(&self) -> Vec<(&'static str, String)> {
        let mut f = vec![("kind", self.kind().to_string())];
        match self {
            SitorEvent::Text { mode, from, station, selective, text } => {
                f.push(("mode", mode.label().into()));
                if let Some(s) = from {
                    f.push(("from", s.label().into()));
                }
                if let Some(s) = station {
                    f.push(("station", s.clone()));
                }
                if *mode == SitorMode::B {
                    f.push(("selective", selective.to_string()));
                }
                f.push(("text", text.clone()));
            }
            SitorEvent::Call { selcall } => {
                f.push(("mode", "A".into()));
                f.push(("selcall", selcall.clone()));
                f.push(("signals", selcall.chars().count().to_string()));
            }
            SitorEvent::Answer { control, rephase } => {
                f.push(("mode", "A".into()));
                f.push(("control", (*control).into()));
                f.push(("rephase", rephase.to_string()));
            }
            SitorEvent::Identity { station, checksum_ok } => {
                f.push(("mode", "A".into()));
                f.push(("station", station.clone()));
                if let Some(ok) = checksum_ok {
                    f.push(("checksumOk", ok.to_string()));
                }
            }
            SitorEvent::Changeover { now_sending, requested_by, inferred } => {
                f.push(("mode", "A".into()));
                f.push(("nowSending", now_sending.label().into()));
                if let Some(r) = requested_by {
                    f.push(("requestedBy", (*r).into()));
                }
                f.push(("inferred", inferred.to_string()));
            }
            SitorEvent::LinkEnd { reason } => {
                f.push(("mode", "A".into()));
                f.push(("reason", (*reason).into()));
            }
            SitorEvent::Phasing | SitorEvent::EndOfEmission => f.push(("mode", "B".into())),
            SitorEvent::SelectiveCall { selcall } => {
                f.push(("mode", "B".into()));
                f.push(("selcall", selcall.clone()));
            }
        }
        f
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SitorStatus {
    /// Locked on a tone pair that is still there.
    pub locked: bool,
    /// The mode being copied: forced, or detected (Auto until it is).
    pub mode: SitorMode,
    pub forced: bool,
    pub center_hz: Option<f32>,
    pub snr_db: Option<f32>,
    /// Mode B: character alignment found. Mode A: the cycle is locked.
    pub synced: bool,
    /// Mode A: the ISS's blocks are found in the 450 ms cycle.
    pub cycle_locked: bool,
    /// Mode A: the IRS's control signals are found too.
    pub irs_heard: bool,
    /// Mode A: where the ISS's block starts in the cycle, in ms after the
    /// cycle's arbitrary origin; it moves with a change of direction.
    pub cycle_phase_ms: Option<f32>,
    /// Share of recent characters that could not be read.
    pub error_rate: f32,
    /// Mode A: the called station's selective call; mode B: the address of
    /// a selective emission.
    pub selcall: Option<String>,
    /// Mode A: the caller's identity, from automatic identification.
    pub identity: Option<String>,
    /// Mode A: "standby", "calling", "identifying", "traffic" or "idle";
    /// mode B: "standby", "phasing", "traffic" or "ended".
    pub link: &'static str,
    /// Mode A: which station is sending.
    pub sending: Option<ArqSide>,
    /// Mode B: the emission is selective (inverted ratio).
    pub selective: bool,
    /// The station is heard with its tones swapped.
    pub reverse: bool,
}

// ---------------------------------------------------------------------------
// The demodulator's recent output.

/// The last few seconds of the demodulator's tone amplitudes, addressed by
/// absolute sample number.
struct Hist {
    hi: Vec<f32>,
    lo: Vec<f32>,
    mask: usize,
    /// Samples written.
    n: u64,
}

/// Enough for the cycle being scored, the one before it and a few cycles
/// of hindsight (8192 samples is 4.8 s at the demodulator's ~1.7 kHz).
const HIST_LEN: usize = 8192;

impl Hist {
    fn new() -> Self {
        Self { hi: vec![0.0; HIST_LEN], lo: vec![0.0; HIST_LEN], mask: HIST_LEN - 1, n: 0 }
    }

    fn push(&mut self, hi: f32, lo: f32) {
        let i = self.n as usize & self.mask;
        self.hi[i] = hi;
        self.lo[i] = lo;
        self.n += 1;
    }

    /// Whether `t..t+len` is still held.
    fn holds(&self, t: f64, len: f64) -> bool {
        t >= 0.0 && t + len + 2.0 < self.n as f64 && (t as u64) + (HIST_LEN as u64) > self.n + 2
    }

    /// The two tones at a fractional sample time.
    #[inline]
    fn at(&self, t: f64) -> (f32, f32) {
        let i = t.floor();
        let a = (t - i) as f32;
        let i = i as usize;
        let (j, k) = (i & self.mask, (i + 1) & self.mask);
        (self.hi[j] + a * (self.hi[k] - self.hi[j]), self.lo[j] + a * (self.lo[k] - self.lo[j]))
    }
}

// ---------------------------------------------------------------------------
// Mode A: the cycle.

/// The ARQ cycle.
const CYCLE_S: f64 = 0.45;
const BLOCK_BITS: usize = 21;
const CS_BITS: usize = 7;
/// Energy profile bins over the cycle (5 ms each), and how many of the
/// quietest are the noise floor: a cycle has at least 170 ms of silence.
const FOLD_BINS: usize = 90;
const FLOOR_BINS: usize = 20;
/// A bit sample is signal when its energy is this many times the floor's
/// mean; noise alone passes about 2% of the time.
const PRESENT_K: f32 = 3.0;
/// Bits of a block, and of a control signal, that must be signal.
const BLOCK_PRESENT: usize = 18;
const CS_PRESENT: usize = 6;
/// Weight of each cycle in the position scores.
const ARQ_EMA: f32 = 0.4;
/// Scores to take and to drop a position.
const ARQ_TAKE: f32 = 0.6;
const ARQ_LOSE: f32 = 0.15;
const CS_TAKE: f32 = 0.5;
const CS_LOSE: f32 = 0.15;
/// One clean cycle's worth: where a lone valid word is looked for.
const CS_TRY: f32 = 0.3;
/// A lock follows the best score this many samples either side.
const ARQ_TRACK: usize = 4;
/// Loudest bins over the floor for the cycle to count as bursts.
const BURST_A: f32 = 4.0;
/// Cycles without a block before a link is given up (about 10 s).
const LOST_CYCLES: u64 = 22;
/// Cycles looked back over when a new position is taken.
const RETRO: u64 = 3;

/// A burst read at one position: bit decisions (bit j for the j-th bit
/// sent, set for B), how many bit samples were signal, and how open the
/// eye was, over the whole burst and over its first seven bits; and
/// whether the bit before it was signal too.
struct Probe {
    bits: u32,
    present: usize,
    present7: usize,
    eye: f32,
    eye7: f32,
    before: bool,
    /// The longest run of bits in the burst that were not signal.
    max_gap: usize,
}

impl Probe {
    /// Signal through the whole 210 ms, but for a few bits lost in noise
    /// or a fade: not a control signal and the gap after it.
    fn block_like(&self) -> bool {
        self.present >= BLOCK_PRESENT && self.max_gap <= 2
    }

    /// How much a control signal is at this position: a valid word, all
    /// of it signal, starting where signal starts. A word shifted by a bit
    /// is often valid too (CS4 one bit late reads as the letter shift),
    /// but it takes in a bit of the gap and starts inside the burst.
    fn control_score(&self, pol: usize) -> f32 {
        let ones = (self.bits & 0x7f).count_ones();
        let ok = if pol == 0 { ones == 4 } else { ones == 3 };
        if self.present7 < CS_PRESENT || !ok {
            return 0.0;
        }
        let edge = if self.before { 0.8 } else { 1.0 };
        (0.7 + 0.3 * self.eye7) * (self.present7 as f32 / CS_BITS as f32) * edge
    }
}

fn probe(h: &Hist, t0: f64, spb: f64, thr: f32) -> Probe {
    let (hb, lb) = if t0 >= spb { h.at(t0 - spb) } else { (0.0, 0.0) };
    let before = hb * hb + lb * lb > thr;
    let mut p = Probe { bits: 0, present: 0, present7: 0, max_gap: 0, eye: 0.0, eye7: 0.0, before };
    let mut run = 0;
    for j in 0..BLOCK_BITS {
        let (hi, lo) = h.at(t0 + j as f64 * spb);
        let on = hi * hi + lo * lo > thr;
        let eye = (hi - lo).abs() / (hi + lo + TINY);
        p.bits |= u32::from(hi > lo) << j;
        p.present += usize::from(on);
        run = if on { 0 } else { run + 1 };
        p.max_gap = p.max_gap.max(run);
        p.eye += eye;
        if j < CS_BITS {
            p.present7 += usize::from(on);
            p.eye7 += eye;
        }
    }
    p.eye /= BLOCK_BITS as f32;
    p.eye7 /= CS_BITS as f32;
    p
}

/// A burst's words with soft bits: its own tone levels, the noise from the
/// cycle's quiet part, as log-likelihood ratios like NAVTEX's.
fn read_words<const N: usize>(h: &Hist, t0: f64, spb: f64, noise: f32, pol: usize) -> [Word; N] {
    let mut hl = [(0.0f32, 0.0f32); 21];
    let nb = 7 * N;
    for (j, v) in hl.iter_mut().enumerate().take(nb) {
        *v = h.at(t0 + j as f64 * spb);
    }
    let level = |hi: bool| {
        let (mut s, mut n) = (0.0f32, 0u32);
        for &(a, b) in &hl[..nb] {
            let (on, off) = if hi { (a, b) } else { (b, a) };
            if on > off {
                s += on;
                n += 1;
            }
        }
        if n > 0 { s / n as f32 } else { 0.0 }
    };
    let (ah, al) = (level(true), level(false));
    let den = (0.5 * noise).max(TINY);
    let mut out = [Word::default(); N];
    for (w, word) in out.iter_mut().enumerate() {
        let mut code = 0u8;
        for k in 0..7 {
            let (hi, lo) = hl[7 * w + k];
            let llr = ((ah * (hi - 0.5 * ah) - al * (lo - 0.5 * al)) / den).clamp(-LLR_MAX, LLR_MAX);
            word.soft[k] = llr;
            code |= u8::from(hi > lo) << k;
        }
        word.code = code;
        *word = normalize(word, pol);
    }
    out
}

fn circ_dist(a: usize, b: usize, n: usize) -> usize {
    let d = a.abs_diff(b) % n;
    d.min(n - d)
}

/// Mode A: finds the block and the control signal in the cycle and reads
/// them a cycle at a time.
struct Arq {
    spb: f64,
    cyc: f64,
    nc: usize,
    /// The cycle grid: cycle k starts at sample `g0 + k * cyc`.
    g0: f64,
    k: u64,
    blk: [Vec<f32>; 2],
    cs: Vec<f32>,
    fold: Vec<f32>,
    fold_n: u32,
    floor: f32,
    ratio: f32,
    /// Block position and polarity.
    lock: Option<(usize, usize)>,
    cs_lock: Option<usize>,
    /// The best place for it so far, before it is taken.
    cs_cand: Option<usize>,
    last_block_t: f64,
    last_block_k: u64,
    proto: Proto,
}

impl Arq {
    fn new(fs2: f32) -> Self {
        let spb = fs2 as f64 / BAUD as f64;
        let cyc = CYCLE_S * fs2 as f64;
        let nc = cyc.ceil() as usize;
        Self {
            spb,
            cyc,
            nc,
            g0: 0.0,
            k: 0,
            blk: [vec![0.0; nc], vec![0.0; nc]],
            cs: vec![0.0; nc],
            fold: vec![0.0; FOLD_BINS],
            fold_n: 0,
            floor: 0.0,
            ratio: 0.0,
            lock: None,
            cs_lock: None,
            cs_cand: None,
            last_block_t: f64::MIN,
            last_block_k: 0,
            proto: Proto::new(),
        }
    }

    /// A new signal from sample `at` on.
    fn restart(&mut self, at: u64) {
        self.g0 = at as f64;
        self.k = 0;
        self.blk.iter_mut().for_each(|v| v.fill(0.0));
        self.cs.fill(0.0);
        self.fold.fill(0.0);
        self.fold_n = 0;
        self.floor = 0.0;
        self.ratio = 0.0;
        self.lock = None;
        self.cs_lock = None;
        self.cs_cand = None;
        self.last_block_t = f64::MIN;
        self.last_block_k = 0;
        self.proto.rebase();
    }

    fn locked(&self) -> bool {
        self.lock.is_some()
    }

    fn bursty(&self) -> bool {
        self.fold_n >= 2 && self.ratio > BURST_A
    }

    /// Every cycle the history now covers.
    fn run(&mut self, h: &Hist) {
        loop {
            let base = self.g0 + self.k as f64 * self.cyc;
            if !h.holds(base, self.nc as f64 + BLOCK_BITS as f64 * self.spb + 2.0) {
                if base + 2.0 * self.cyc < h.n as f64 - HIST_LEN as f64 {
                    // Fell behind the history (cannot happen with whole
                    // pushes, but a huge one must not wedge the loop).
                    self.k += 1;
                    continue;
                }
                return;
            }
            self.cycle(h, base);
            self.k += 1;
        }
    }

    fn cycle(&mut self, h: &Hist, base: f64) {
        self.fold_energy(h, base);
        let thr = PRESENT_K * self.floor;
        let pol = self.lock.map_or(0, |l| l.1);
        for c in 0..self.nc {
            let p = probe(h, base + c as f64, self.spb, thr);
            let (m0, m1) = if p.block_like() {
                let e = 0.7 + 0.3 * p.eye;
                let (mut n4, mut n3) = (0, 0);
                for w in 0..3 {
                    match (p.bits >> (7 * w) & 0x7f).count_ones() {
                        4 => n4 += 1,
                        3 => n3 += 1,
                        _ => {}
                    }
                }
                (e * n4 as f32 / 3.0, e * n3 as f32 / 3.0)
            } else {
                (0.0, 0.0)
            };
            self.blk[0][c] += ARQ_EMA * (m0 - self.blk[0][c]);
            self.blk[1][c] += ARQ_EMA * (m1 - self.blk[1][c]);
            let m = p.control_score(pol);
            self.cs[c] += ARQ_EMA * (m - self.cs[c]);
        }
        let (fresh, far) = self.update_lock();
        self.update_cs_lock();
        if far {
            self.proto.moved(self.k);
        }
        let Some((cb, pol)) = self.lock else {
            self.proto.no_block(self.k);
            return;
        };
        // Blocks are read a cycle late, so the control signal after each is
        // in hand; a new position is first tried on the cycles before it.
        let first = if fresh { self.k.saturating_sub(1 + RETRO) } else { self.k.saturating_sub(1) };
        for j in first..self.k {
            let t = self.g0 + j as f64 * self.cyc + cb as f64;
            if t <= self.last_block_t + 0.5 * self.cyc || !h.holds(t, BLOCK_BITS as f64 * self.spb) {
                continue;
            }
            let p = probe(h, t, self.spb, thr);
            if !p.block_like() {
                continue;
            }
            let words: [Word; 3] = read_words(h, t, self.spb, 0.5 * self.floor, pol);
            // Until the control signal's place is learned, the likeliest
            // place is tried, and only a valid word there is taken: the
            // first answer to a call is often the only one of its kind.
            let at = self.cs_lock.map(|c| (c, false)).or(self.cs_cand.map(|c| (c, true)));
            let ctrl = at.and_then(|(cs, provisional)| {
                let dq = (cs as f64 - cb as f64).rem_euclid(self.cyc);
                let tc = t + dq;
                if !h.holds(tc, CS_BITS as f64 * self.spb) {
                    return None;
                }
                let p = probe(h, tc, self.spb, thr);
                if p.present7 < CS_PRESENT {
                    return None;
                }
                let [w]: [Word; 1] = read_words(h, tc, self.spb, 0.5 * self.floor, pol);
                if provisional && !w.valid {
                    return None;
                }
                Some(self.proto.read_control(&w))
            });
            self.last_block_t = t;
            self.last_block_k = j;
            self.proto.cycle(j, words, ctrl);
        }
        if self.k > self.last_block_k + 1 {
            self.proto.no_block(self.k);
        }
        if self.proto.take_forget() {
            // A change of direction: both stations' bursts are about to
            // move, so what was learned of the old places is let go. The
            // old ISS sends only control signals from now on, and a block
            // read where its blocks were would straddle one and the gap.
            // The new place is found within a couple of cycles and the
            // cycles before it are read then.
            self.blk.iter_mut().for_each(|v| v.iter_mut().for_each(|s| *s *= 0.3));
            self.cs.iter_mut().for_each(|s| *s *= 0.3);
            self.lock = None;
            self.cs_lock = None;
            self.cs_cand = None;
        }
    }

    /// The cycle's energy profile, the noise floor, and how bursty it is.
    fn fold_energy(&mut self, h: &Hist, base: f64) {
        let mut bins = [0.0f32; FOLD_BINS];
        let per = self.cyc / FOLD_BINS as f64;
        for (b, v) in bins.iter_mut().enumerate() {
            let t0 = (base + b as f64 * per).ceil() as u64;
            let t1 = (base + (b + 1) as f64 * per).ceil() as u64;
            let mut s = 0.0;
            for t in t0..t1 {
                let (hi, lo) = h.at(t as f64);
                s += hi * hi + lo * lo;
            }
            *v = s / (t1 - t0).max(1) as f32;
        }
        self.fold_n += 1;
        let a = (1.0 / self.fold_n as f32).max(0.3);
        for (f, b) in self.fold.iter_mut().zip(bins) {
            *f += a * (b - *f);
        }
        let mut sorted = self.fold.clone();
        sorted.sort_by(f32::total_cmp);
        let floor = sorted[..FLOOR_BINS].iter().sum::<f32>() / FLOOR_BINS as f32;
        let top = sorted[FOLD_BINS - FLOOR_BINS..].iter().sum::<f32>() / FLOOR_BINS as f32;
        self.floor = floor.max(TINY);
        self.ratio = top / self.floor;
    }

    /// Follows the block's position. Returns whether a position was newly
    /// taken, and whether it is far from the one it replaces.
    fn update_lock(&mut self) -> (bool, bool) {
        let mut best = (0usize, 0usize, f32::MIN);
        for (p, row) in self.blk.iter().enumerate() {
            for (c, &s) in row.iter().enumerate() {
                if s > best.2 {
                    best = (c, p, s);
                }
            }
        }
        match self.lock {
            None => {
                if best.2 >= ARQ_TAKE {
                    self.lock = Some((best.0, best.1));
                    return (true, false);
                }
                (false, false)
            }
            Some((c, p)) => {
                let row = &self.blk[p];
                let mut local = (c, row[c]);
                for d in 1..=ARQ_TRACK {
                    for cc in [(c + d) % self.nc, (c + self.nc - d) % self.nc] {
                        if row[cc] > local.1 {
                            local = (cc, row[cc]);
                        }
                    }
                }
                let far = circ_dist(best.0, local.0, self.nc) > 2 * ARQ_TRACK || best.1 != p;
                if far && best.2 >= ARQ_TAKE && best.2 > local.1 + 0.2 {
                    self.lock = Some((best.0, best.1));
                    (true, true)
                } else if local.1 < ARQ_LOSE {
                    self.lock = None;
                    self.cs_lock = None;
                    (false, false)
                } else {
                    self.lock = Some((local.0, p));
                    (false, false)
                }
            }
        }
    }

    /// Follows the control signal's position, outside the block.
    fn update_cs_lock(&mut self) {
        let Some((cb, _)) = self.lock else {
            self.cs_lock = None;
            self.cs_cand = None;
            return;
        };
        let lo = (BLOCK_BITS as f64 * self.spb) as usize + 2;
        let hi = self.nc.saturating_sub((CS_BITS as f64 * self.spb) as usize + 2);
        let allowed = |c: usize| {
            let rel = (c + self.nc - cb) % self.nc;
            rel >= lo && rel <= hi
        };
        let mut best = (0usize, f32::MIN);
        for (c, &s) in self.cs.iter().enumerate() {
            if allowed(c) && s > best.1 {
                best = (c, s);
            }
        }
        self.cs_cand = (best.1 >= CS_TRY).then_some(best.0);
        self.cs_lock = match self.cs_lock {
            Some(c) if allowed(c) => {
                let mut local = (c, self.cs[c]);
                for d in 1..=ARQ_TRACK {
                    for cc in [(c + d) % self.nc, (c + self.nc - d) % self.nc] {
                        if allowed(cc) && self.cs[cc] > local.1 {
                            local = (cc, self.cs[cc]);
                        }
                    }
                }
                if best.1 >= CS_TAKE && best.1 > local.1 + 0.2 {
                    Some(best.0)
                } else if local.1 < CS_LOSE {
                    None
                } else {
                    Some(local.0)
                }
            }
            _ => (best.1 >= CS_TAKE).then_some(best.0),
        };
    }

    fn cycle_phase_ms(&self) -> Option<f32> {
        self.lock.map(|(c, _)| (c as f64 / self.cyc * CYCLE_S * 1e3) as f32)
    }
}

// ---------------------------------------------------------------------------
// Mode A: the protocol, as a third party sees it.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    Standby,
    Calling,
    Identifying,
    Traffic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    None,
    Call1,
    Call2Four,
    Call2Seven,
    Call3,
    Id1,
    Id2,
    Id3,
    Rq,
    Changeover,
    End,
    Info,
}

/// An information block held until the next shows whether it is repeated.
struct Held {
    /// Summed soft bits of every copy.
    sum: [[f32; 7]; 3],
    /// Each position's distinct valid codes seen (at most two kept).
    seen: [[Option<u8>; 2]; 3],
    copies: u32,
    /// The control signal that asked for it.
    req: Option<u8>,
    /// Hard codes of the first copy, for content comparison.
    first: [Option<u8>; 3],
}

impl Held {
    fn new(w: &[Word; 3], req: Option<u8>) -> Self {
        let mut h = Self { sum: [[0.0; 7]; 3], seen: [[None; 2]; 3], copies: 0, req, first: [None; 3] };
        for (i, x) in w.iter().enumerate() {
            h.first[i] = x.valid.then_some(x.code);
        }
        h.add(w);
        h
    }

    fn add(&mut self, w: &[Word; 3]) {
        self.copies += 1;
        for (i, x) in w.iter().enumerate() {
            for k in 0..7 {
                self.sum[i][k] += x.soft[k];
            }
            if x.valid {
                let s = &mut self.seen[i];
                if s[0].is_none() {
                    s[0] = Some(x.code);
                } else if s[0] != Some(x.code) && s[1].is_none() {
                    s[1] = Some(x.code);
                }
            }
        }
    }

    /// Each position's character: the one valid code its copies gave, or
    /// with two or more copies and no single valid code, the most likely
    /// word given all of them; None for one that cannot be read.
    fn codes(&self) -> [Option<u8>; 3] {
        let mut out = [None; 3];
        for (i, o) in out.iter_mut().enumerate() {
            *o = match self.seen[i] {
                [Some(c), None] => Some(c),
                _ if self.copies >= 2 => {
                    let mut s = self.sum[i];
                    s.iter_mut().for_each(|v| *v = v.clamp(-2.0 * LLR_MAX, 2.0 * LLR_MAX));
                    ml_among(&s, &VALID, ML_MARGIN)
                }
                _ => None,
            };
        }
        out
    }
}

struct Proto {
    link: Link,
    roles_known: bool,
    /// The sending station: 0 the caller (or the first heard), 1 the other.
    iss: u8,
    last_kind: Kind,
    last_kind_k: u64,
    k: u64,
    call1: Option<[char; 2]>,
    call2_seven: Option<[char; 2]>,
    selcall: Option<String>,
    call_reported: Option<String>,
    answered: bool,
    rephase: bool,
    id: [Option<[char; 3]>; 3],
    id_stage: usize,
    identity: Option<String>,
    identity_reported: bool,
    ck: [Option<char>; 3],
    held: Option<Held>,
    /// The control signal after the last block: the request for the next.
    req: Option<u8>,
    /// A cycle passed with no block read since the held one.
    gap: bool,
    idle: bool,
    shift: Shift,
    lines: Lines,
    line_from: u8,
    last_char_k: u64,
    tail: [char; 2],
    co_by: Option<&'static str>,
    last_co_k: Option<u64>,
    forget: bool,
    err_rate: f32,
    events: Vec<SitorEvent>,
}

impl Proto {
    fn new() -> Self {
        Self {
            link: Link::Standby,
            roles_known: false,
            iss: 0,
            last_kind: Kind::None,
            last_kind_k: 0,
            k: 0,
            call1: None,
            call2_seven: None,
            selcall: None,
            call_reported: None,
            answered: false,
            rephase: false,
            id: [None; 3],
            id_stage: 0,
            identity: None,
            identity_reported: false,
            ck: [None; 3],
            held: None,
            req: None,
            gap: false,
            idle: false,
            shift: Shift::default(),
            lines: Lines::default(),
            line_from: 0,
            last_char_k: 0,
            tail: [' '; 2],
            co_by: None,
            last_co_k: None,
            forget: false,
            err_rate: 0.0,
            events: Vec::new(),
        }
    }

    fn side(&self, i: u8) -> ArqSide {
        ArqSide::of(i, self.roles_known)
    }

    fn station(&self, i: u8) -> Option<String> {
        if !self.roles_known {
            return None;
        }
        if i == 0 { self.identity.clone() } else { self.selcall.clone() }
    }

    fn take_forget(&mut self) -> bool {
        std::mem::take(&mut self.forget)
    }

    /// A control signal word: what the protocol lets the IRS send now,
    /// read even when the 4B/3Y check fails if one clearly stands out.
    fn read_control(&self, w: &Word) -> Option<u8> {
        if w.valid {
            return Some(w.code);
        }
        match self.link {
            Link::Traffic => ml_among(&w.soft, &[CS1, CS2, CS3, RQ], 2.0 * ML_MARGIN),
            Link::Calling => ml_among(&w.soft, &[CS1, CS2, CS3, CS4, CS5], 2.0 * ML_MARGIN),
            _ => None,
        }
    }

    /// A cycle with no block read.
    fn no_block(&mut self, k: u64) {
        self.k = self.k.max(k);
        if self.held.is_some() {
            self.gap = true;
        }
        if !self.lines.is_empty() && (k.saturating_sub(self.last_char_k)) as f64 * CYCLE_S > IDLE_FLUSH_S as f64 {
            self.flush_line();
        }
        if self.link != Link::Standby && k > self.last_kind_k + LOST_CYCLES {
            self.release();
            self.flush_line();
            self.events.push(SitorEvent::LinkEnd { reason: "lost" });
            self.standby();
        }
    }

    fn standby(&mut self) {
        self.link = Link::Standby;
        self.held = None;
        self.req = None;
        self.gap = false;
        self.idle = false;
        self.shift = Shift::default();
        self.co_by = None;
        self.last_kind = Kind::None;
    }

    /// The block position jumped: in traffic, without a change-over block
    /// heard, the other station has most likely taken over.
    fn moved(&mut self, k: u64) {
        let recent = self.last_co_k.is_some_and(|c| k < c + 8);
        if self.link == Link::Traffic && !recent {
            self.changeover(k, true);
        }
    }

    fn flush(&mut self) {
        self.release();
        self.flush_line();
    }

    /// The cycle count starts over with a new lock on the tones: what was
    /// pending is let go, and the link's timers count from the new start.
    /// The link itself is kept, so a station back after a long fade is the
    /// same link, and given up as lost if it does not come back.
    fn rebase(&mut self) {
        self.flush();
        self.k = 0;
        self.last_kind_k = 0;
        self.last_char_k = 0;
        self.last_co_k = None;
        self.req = None;
    }

    fn cycle(&mut self, k: u64, w: [Word; 3], ctrl: Option<Option<u8>>) {
        self.k = k;
        let v = w.map(|x| x.valid.then_some(x.code));
        let kind = self.classify(&v);
        let dup = kind == self.last_kind && k <= self.last_kind_k + 2;
        match kind {
            Kind::Rq => {
                if self.link == Link::Identifying {
                    self.identity_done();
                }
            }
            Kind::Changeover => {
                if !dup {
                    self.changeover(k, false);
                }
            }
            Kind::End => {
                if !dup && self.link != Link::Standby {
                    self.release();
                    self.flush_line();
                    self.events.push(SitorEvent::LinkEnd { reason: "endOfCommunication" });
                }
                self.standby();
            }
            Kind::Call1 => {
                let [Some(a), _, Some(b)] = v else { unreachable!() };
                if self.link != Link::Calling || !self.roles_known {
                    self.new_call();
                }
                self.call1 = id_letter(a).zip(id_letter(b)).map(|(a, b)| [a, b]);
            }
            Kind::Call2Four => {
                let [Some(c), Some(d), _] = v else { unreachable!() };
                if let (Some([a, b]), Some(c), Some(d)) = (self.call1, id_letter(c), id_letter(d)) {
                    self.report_call([a, b, c, d].iter().collect());
                }
            }
            Kind::Call2Seven => {
                let [_, Some(c), Some(d)] = v else { unreachable!() };
                self.call2_seven = id_letter(c).zip(id_letter(d)).map(|(c, d)| [c, d]);
            }
            Kind::Call3 => {
                let x: Vec<char> = v.iter().filter_map(|c| c.and_then(id_letter)).collect();
                if let (Some([a, b]), Some([c, d]), [e, f, g]) = (self.call1, self.call2_seven, x.as_slice()) {
                    self.report_call([a, b, c, d, *e, *f, *g].iter().collect());
                }
            }
            Kind::Id1 | Kind::Id2 | Kind::Id3 => {
                if self.link == Link::Calling {
                    self.answered = true;
                }
                self.link = Link::Identifying;
                self.iss = 0;
                let letters: Vec<char> = v.iter().filter_map(|c| c.and_then(id_letter)).collect();
                let i = match kind {
                    Kind::Id1 => 0,
                    Kind::Id2 => 1,
                    _ => 2,
                };
                let mut part = [' '; 3];
                for (p, c) in part.iter_mut().zip(&letters) {
                    *p = *c;
                }
                self.id[i] = Some(part);
                self.id_stage = i;
            }
            Kind::Info | Kind::None => self.info(w, &v),
        }
        if kind != Kind::None {
            self.last_kind = kind;
            self.last_kind_k = k;
        }
        match ctrl {
            Some(c) => self.control(c),
            // No answer heard: what the IRS asked for next is unknown.
            None => self.req = None,
        }
        self.gap = false;
    }

    fn classify(&self, v: &[Option<u8>; 3]) -> Kind {
        // A service block with one character lost is still the service
        // block: information blocks carry none of these signals.
        let fits = |p: [u8; 3]| {
            let hits = v.iter().zip(p).filter(|(c, q)| **c == Some(*q)).count();
            let miss = v.iter().zip(p).filter(|(c, q)| c.is_some_and(|c| c != *q)).count();
            hits >= 2 && miss == 0
        };
        if fits([RQ, RQ, RQ]) {
            return Kind::Rq;
        }
        if fits([BETA, ALPHA, BETA]) {
            return Kind::Changeover;
        }
        if fits([ALPHA, ALPHA, ALPHA]) {
            return Kind::End;
        }
        let id = |c: Option<u8>| c.and_then(id_letter).is_some();
        let is = |c: Option<u8>, s: u8| c == Some(s);
        match *v {
            [a, r, b] if is(r, RQ) && id(a) && id(b) => return Kind::Call1,
            [a, b, r] if is(r, RQ) && id(a) && id(b) => return Kind::Call2Four,
            [r, a, b] if is(r, RQ) && id(a) && id(b) => return Kind::Call2Seven,
            [a, x, b] if is(x, ALPHA) && id(a) && id(b) => return Kind::Id1,
            [x, a, b] if is(x, ALPHA) && id(a) && id(b) => return Kind::Id2,
            _ => {}
        }
        let all_id = v.iter().all(|&c| id(c));
        let recent = |k: Kind| self.last_kind == k && self.k <= self.last_kind_k + 2;
        if all_id && self.link == Link::Calling && recent(Kind::Call2Seven) {
            return Kind::Call3;
        }
        if all_id && self.link == Link::Identifying && recent(Kind::Id2) {
            return Kind::Id3;
        }
        Kind::Info
    }

    fn new_call(&mut self) {
        self.flush();
        self.standby();
        self.link = Link::Calling;
        self.roles_known = true;
        self.iss = 0;
        self.call1 = None;
        self.call2_seven = None;
        self.answered = false;
        self.rephase = false;
        self.id = [None; 3];
        self.identity = None;
        self.identity_reported = false;
        self.ck = [None; 3];
    }

    fn report_call(&mut self, s: String) {
        self.selcall = Some(s.clone());
        if self.call_reported.as_ref() != Some(&s) {
            self.call_reported = Some(s.clone());
            self.events.push(SitorEvent::Call { selcall: s });
        }
    }

    fn identity_done(&mut self) {
        if self.identity_reported {
            return;
        }
        // Identification blocks: Y1 α Y2, α Y3 Y4, Y5 Y6 Y7.
        let (Some(a), Some(b), Some(c)) = (self.id[0], self.id[1], self.id[2]) else { return };
        let id: Vec<char> = [a[0], a[1], b[0], b[1], c[0], c[1], c[2]].into_iter().collect();
        if id.contains(&' ') {
            return;
        }
        let station: String = id.iter().collect();
        let called: Option<Vec<char>> = self.selcall.as_ref().map(|s| s.chars().collect());
        let checksum_ok = match (called.as_deref().and_then(checksums), self.ck) {
            (Some(want), [Some(x), Some(y), Some(z)]) => Some(want == [x, y, z]),
            _ => None,
        };
        self.identity = Some(station.clone());
        self.identity_reported = true;
        self.events.push(SitorEvent::Identity { station, checksum_ok });
    }

    fn control(&mut self, c: Option<u8>) {
        self.req = c;
        let Some(c) = c else { return };
        match self.link {
            Link::Calling => {
                let (rephase, next) = match c {
                    CS1 | CS2 => (false, Link::Traffic),
                    CS4 => (false, Link::Identifying),
                    CS5 => (true, Link::Identifying),
                    _ => return,
                };
                if !self.answered {
                    self.answered = true;
                    self.rephase = rephase;
                    self.events.push(SitorEvent::Answer { control: control_name(c), rephase });
                }
                self.link = next;
                self.iss = 0;
            }
            Link::Identifying => {
                if let Some(l) = id_letter(c) {
                    self.ck[self.id_stage] = Some(l);
                } else if matches!(c, CS1 | CS3) {
                    self.identity_done();
                    self.link = Link::Traffic;
                    if c == CS3 {
                        self.co_by = Some("irs");
                    }
                }
            }
            Link::Traffic | Link::Standby => {
                if c == CS3 {
                    self.co_by = Some("irs");
                }
            }
        }
    }

    fn changeover(&mut self, k: u64, inferred: bool) {
        self.release();
        self.flush_line();
        if self.link == Link::Identifying {
            self.identity_done();
        }
        if self.link != Link::Standby {
            self.link = Link::Traffic;
        }
        self.iss ^= 1;
        // The ISS's "+?" comes first; the IRS's CS3 is then its answer.
        let by = if self.tail == ['+', '?'] { Some("iss") } else { self.co_by };
        self.co_by = None;
        self.tail = [' '; 2];
        self.events.push(SitorEvent::Changeover { now_sending: self.side(self.iss), requested_by: by, inferred });
        self.last_co_k = Some(k);
        self.req = None;
        self.shift = Shift::default();
        self.forget = !inferred;
    }

    fn info(&mut self, w: [Word; 3], v: &[Option<u8>; 3]) {
        match self.link {
            // Nothing but call blocks comes before an answer; a block that
            // is not one means the answer was missed.
            Link::Calling | Link::Identifying => {
                if self.link == Link::Identifying {
                    self.identity_done();
                }
                self.link = Link::Traffic;
            }
            Link::Standby => self.link = Link::Traffic,
            Link::Traffic => {}
        }
        let repeat = match (&self.held, self.req) {
            (None, _) => false,
            (Some(h), Some(r)) if matches!(r, CS1 | CS2) && matches!(h.req, Some(CS1 | CS2)) && !self.gap => h.req == Some(r),
            (Some(h), _) => {
                // The request was not heard: the same block again, as far
                // as both copies can be read, is taken for a repeat.
                let both: Vec<(u8, u8)> = h.first.iter().zip(v).filter_map(|(a, b)| a.zip(*b)).collect();
                both.len() >= 2 && both.iter().all(|(a, b)| a == b)
            }
        };
        if repeat {
            if let Some(h) = &mut self.held {
                h.add(&w);
            }
        } else {
            self.release();
            self.held = Some(Held::new(&w, self.req));
        }
        self.idle = v.iter().all(|c| *c == Some(BETA));
    }

    /// Prints the held block. The shift is applied directly, without the
    /// RTTY decoder's guard against misread figure shifts: here every
    /// shift passed the 4B/3Y check, and the guard would print the "+?"
    /// that offers a change of direction as letters.
    fn release(&mut self) {
        let Some(h) = self.held.take() else { return };
        for code in h.codes() {
            match code {
                None => {
                    self.err_rate += 0.05 * (1.0 - self.err_rate);
                    self.show_char('*');
                }
                Some(ALPHA | BETA | RQ) => {}
                Some(c) => {
                    self.err_rate -= 0.05 * self.err_rate;
                    let row = CCIR_INDEX[c as usize];
                    if row != 255
                        && let Some(ch) = self.shift.apply(&SYMS[row as usize], false, false)
                    {
                        self.show_char(ch);
                    }
                }
            }
        }
    }

    fn show_char(&mut self, c: char) {
        if self.line_from != self.iss {
            self.flush_line();
            self.line_from = self.iss;
        }
        self.last_char_k = self.k;
        if !c.is_whitespace() {
            self.tail = [self.tail[1], c];
        }
        if let Some(text) = self.lines.push(c) {
            self.emit_line(text);
        }
    }

    fn flush_line(&mut self) {
        if let Some(text) = self.lines.take() {
            self.emit_line(text);
        }
    }

    fn emit_line(&mut self, text: String) {
        let from = self.line_from;
        self.events.push(SitorEvent::Text {
            mode: SitorMode::A,
            from: Some(self.side(from)),
            station: self.station(from),
            selective: false,
            text,
        });
    }

    fn link_label(&self) -> &'static str {
        match self.link {
            Link::Standby => "standby",
            Link::Calling => "calling",
            Link::Identifying => "identifying",
            Link::Traffic if self.idle => "idle",
            Link::Traffic => "traffic",
        }
    }
}

// ---------------------------------------------------------------------------
// Mode B.

/// Consecutive alpha signals in DX slots that end an emission (M.625
/// § 4.6.7.2), and phasing signals 2 in DX slots that start one.
const END_ALPHAS: u32 = 2;
const PHASING_RUN: u32 = 3;
/// Characters of a selective call phase before giving up on its end.
const CALL_MAX: u32 = 150;

/// Mode B: NAVTEX's bit clock, alignment and time-diversity combining,
/// with the polarity of each word followed for selective emissions.
struct Fec {
    atc: Atc,
    spb: f32,
    half_len: f32,
    ph: f32,
    last_v: f32,
    prev: f32,
    have_prev: bool,
    reg: u8,
    nbits: u64,
    soft: [f32; 7],
    score: [[f32; 2]; 7],
    scored: [[u32; 2]; 7],
    sync: Option<usize>,
    low_words: u32,
    /// Polarity of the words now (1 = 3B/4Y), characters in a row read in
    /// the other, and the polarity the phasing signals came in.
    pol: usize,
    alt_run: u32,
    base_pol: usize,
    phased: bool,
    phasing_run: u32,
    slot: u64,
    /// Words as heard, not normalized.
    words: [Word; 8],
    dx_ev: [f32; 2],
    dx_par: Option<usize>,
    dx_alpha_run: u32,
    quiet: bool,
    selective: bool,
    call_phase: bool,
    /// A character has been read since the phasing.
    traffic: bool,
    call_chars: u32,
    group: String,
    last_group: Option<String>,
    selcall: Option<String>,
    shift: Shift,
    err_fast: f32,
    err_rate: f32,
    printing: bool,
    held: Vec<char>,
    held_since: Option<u64>,
    last_nl: bool,
    lines: Lines,
    line_selective: bool,
    events: Vec<SitorEvent>,
    t: u64,
    last_char: u64,
}

impl Fec {
    fn new(spb: f32, filter_len: usize) -> Self {
        Self {
            atc: Atc::new(spb),
            spb,
            half_len: 0.5 * filter_len as f32,
            ph: spb,
            last_v: 0.0,
            prev: 0.0,
            have_prev: false,
            reg: 0,
            nbits: 0,
            soft: [0.0; 7],
            score: [[0.0; 2]; 7],
            scored: [[0; 2]; 7],
            sync: None,
            low_words: 0,
            pol: 0,
            alt_run: 0,
            base_pol: 0,
            phased: false,
            phasing_run: 0,
            slot: 0,
            words: [Word::default(); 8],
            dx_ev: [0.0; 2],
            dx_par: None,
            dx_alpha_run: 0,
            quiet: false,
            selective: false,
            call_phase: false,
            traffic: false,
            call_chars: 0,
            group: String::new(),
            last_group: None,
            selcall: None,
            shift: Shift::default(),
            err_fast: 0.5,
            err_rate: 0.0,
            printing: false,
            held: Vec::with_capacity(HELD_MAX),
            held_since: None,
            last_nl: false,
            lines: Lines::default(),
            line_selective: false,
            events: Vec::new(),
            t: 0,
            last_char: 0,
        }
    }

    fn restart(&mut self) {
        self.atc.reset();
        self.ph = self.spb;
        self.have_prev = false;
        self.reg = 0;
        self.nbits = 0;
        self.score = [[0.0; 2]; 7];
        self.scored = [[0; 2]; 7];
        self.flush_line();
        self.lose_sync();
    }

    fn lose_sync(&mut self) {
        self.sync = None;
        self.low_words = 0;
        self.held.clear();
        self.held_since = None;
        self.slot = 0;
        self.dx_ev = [0.0; 2];
        self.dx_par = None;
        self.dx_alpha_run = 0;
        self.phasing_run = 0;
        self.quiet = false;
        self.selective = false;
        self.call_phase = false;
        self.phased = false;
        self.alt_run = 0;
        self.shift = Shift::default();
        self.err_fast = 0.5;
        self.printing = false;
    }

    fn synced(&self) -> bool {
        self.sync.is_some()
    }

    /// One demodulator sample: the bit clock (as NAVTEX's).
    fn sample(&mut self, hi: f32, lo: f32) {
        self.t += 1;
        let cur = self.atc.step(hi, lo);
        if !self.have_prev {
            self.prev = cur;
            self.have_prev = true;
            return;
        }
        let prev = self.prev;
        self.prev = cur;
        self.ph -= 1.0;
        if (prev > 0.0) != (cur > 0.0) {
            let back = 1.0 - prev / (prev - cur);
            let mut e = self.ph + back - self.half_len;
            e -= self.spb * (e / self.spb).round();
            let w = self.last_v.abs().min(1.0);
            let g = if self.sync.is_some() { CLOCK_GAIN_SYNC } else { CLOCK_GAIN_HUNT };
            self.ph -= g * w * e;
        }
        while self.ph <= 0.0 {
            let a = 1.0 + self.ph;
            let v = prev + (cur - prev) * a;
            self.ph += self.spb;
            self.bit(v);
        }
    }

    fn bit(&mut self, v: f32) {
        let i = (self.nbits % 7) as usize;
        self.reg = (self.reg >> 1) | (u8::from(v > 0.0) << 6);
        self.last_v = v;
        self.soft[i] = (v * self.atc.llr_scale()).clamp(-LLR_MAX, LLR_MAX);
        self.nbits += 1;
        if self.nbits < 7 {
            return;
        }
        let off = (self.nbits % 7) as usize;
        let ones = self.reg.count_ones();
        for (pol, ok) in [(0, ones == 4), (1, ones == 3)] {
            let n = &mut self.scored[off][pol];
            let a = (1.0 / (*n + 1) as f32).max(1.0 / SYNC_LEN);
            *n += 1;
            let sc = &mut self.score[off][pol];
            *sc += a * (f32::from(u8::from(ok)) - *sc);
        }
        self.update_sync();
        let Some(o) = self.sync else { return };
        if off != o {
            return;
        }
        let mut w = Word { code: self.reg, valid: false, soft: [0.0; 7] };
        for k in 0..7 {
            w.soft[k] = self.soft[(self.nbits as usize + k) % 7];
        }
        self.word(w);
    }

    /// The alignment's score under either polarity: a selective emission
    /// changes polarity at the same alignment.
    fn offset_score(&self, o: usize) -> f32 {
        self.score[o][0].max(self.score[o][1])
    }

    fn update_sync(&mut self) {
        let mut best = (0usize, 0usize, f32::MIN);
        let mut second = f32::MIN;
        for (o, row) in self.score.iter().enumerate() {
            for (p, &s) in row.iter().enumerate() {
                if self.scored[o][p] < SYNC_MIN_WORDS {
                    continue;
                }
                if s > best.2 {
                    second = best.2;
                    best = (o, p, s);
                } else if s > second {
                    second = s;
                }
            }
        }
        let n = self.scored[best.0][best.1];
        let clear = best.2 >= SYNC_TAKE || (n >= 2 * SYNC_MIN_WORDS && best.2 >= SYNC_TAKE_WEAK && best.2 >= second + SYNC_LEAD);
        match self.sync {
            None if clear => {
                self.lose_sync();
                self.sync = Some(best.0);
                self.pol = best.1;
            }
            Some(o) => {
                let cur = self.offset_score(o);
                self.low_words = if cur < SYNC_LOSE { self.low_words + 1 } else { 0 };
                if best.0 != o && clear && best.2 > cur + 0.25 {
                    self.flush_line();
                    self.lose_sync();
                    self.sync = Some(best.0);
                    self.pol = best.1;
                } else if self.low_words >= SYNC_LOSE_BITS {
                    self.flush_line();
                    self.lose_sync();
                }
            }
            None => {}
        }
    }

    fn word(&mut self, raw: Word) {
        let w = normalize(&raw, self.pol);
        let k = self.slot;
        self.slot += 1;
        self.words[(k % 8) as usize] = raw;
        let par = (k % 2) as usize;
        for e in &mut self.dx_ev {
            *e *= 0.95;
        }
        if w.valid {
            match w.code {
                PHASING2 => self.dx_ev[par] += 1.0,
                PHASING1 => self.dx_ev[1 - par] += 1.0,
                _ => {}
            }
            if k >= 5 {
                let o = normalize(&self.words[((k - 5) % 8) as usize], self.pol);
                if o.valid && o.code == w.code && w.code != PHASING1 && w.code != PHASING2 {
                    self.dx_ev[1 - par] += 1.0;
                }
            }
        }
        let lead = if self.dx_ev[0] >= self.dx_ev[1] { 0 } else { 1 };
        match self.dx_par {
            None if self.dx_ev[lead] >= 3.0 && self.dx_ev[lead] > 2.0 * self.dx_ev[1 - lead] => self.dx_par = Some(lead),
            Some(cur) if self.dx_ev[1 - cur] > self.dx_ev[cur] + 4.0 => self.dx_par = Some(1 - cur),
            _ => {}
        }
        let Some(dx) = self.dx_par else { return };
        if par == dx {
            self.dx_slot(&w);
            return;
        }
        if k < 5 {
            return;
        }
        // Both copies are read in the polarity of the later one: a
        // character and its repeat were sent in the same ratio.
        let raw_d = self.words[((k - 5) % 8) as usize];
        let d = normalize(&raw_d, self.pol);
        // A selective emission turns to inverted ratio after its phasing.
        // A pair neither copy of which is valid in the present polarity,
        // but which is valid and the same in the other, is that character;
        // two in a row and the polarity has turned. Single words are no
        // guide: noise in a fade passes as either ratio equally often.
        let (d2, w2) = (normalize(&raw_d, 1 - self.pol), normalize(&raw, 1 - self.pol));
        let code = if !d.valid && !w.valid && d2.valid && w2.valid && d2.code == w2.code {
            self.alt_run += 1;
            // Between phasing and the traffic is where a selective call
            // starts, and its first letter must not print: one pair is
            // enough there.
            let opening = self.phased && !self.traffic && self.pol == self.base_pol;
            if self.alt_run >= 2 || opening {
                self.alt_run = 0;
                self.pol = 1 - self.pol;
                self.follow_selective();
            }
            Some(w2.code)
        } else {
            self.alt_run = 0;
            combine(&d, &w)
        };
        self.character(code);
    }

    /// A selective emission starts with its call signal: the address is
    /// collected until the traffic's CR or LF.
    fn follow_selective(&mut self) {
        let sel = self.phased && self.pol != self.base_pol;
        if sel != self.selective {
            self.selective = sel;
            self.call_phase = sel && !self.traffic;
            self.call_chars = 0;
            self.group.clear();
            self.last_group = None;
        }
    }

    fn dx_slot(&mut self, w: &Word) {
        if w.valid && w.code == PHASING2 {
            self.phasing_run += 1;
            if self.phasing_run >= PHASING_RUN && (self.quiet || !self.phased) {
                self.quiet = false;
                self.phased = true;
                self.base_pol = self.pol;
                self.selective = false;
                self.call_phase = false;
                self.traffic = false;
                self.selcall = None;
                self.flush_line();
                self.events.push(SitorEvent::Phasing);
            }
        } else {
            self.phasing_run = 0;
        }
        if w.valid && w.code == ALPHA {
            self.dx_alpha_run += 1;
            if self.dx_alpha_run >= END_ALPHAS && !self.quiet {
                self.end_of_emission();
            }
        } else {
            self.dx_alpha_run = 0;
        }
    }

    fn end_of_emission(&mut self) {
        self.release_held();
        self.flush_line();
        self.quiet = true;
        self.phased = false;
        self.selective = false;
        self.call_phase = false;
        self.shift = Shift::default();
        self.events.push(SitorEvent::EndOfEmission);
    }

    fn character(&mut self, code: Option<u8>) {
        let bad = code.is_none();
        self.err_fast += 0.25 * (f32::from(u8::from(bad)) - self.err_fast);
        self.err_rate += 0.02 * (f32::from(u8::from(bad)) - self.err_rate);
        // As NAVTEX: hold text while copy is poor, and let it go if good
        // copy resumes within seconds.
        let score = self.sync.map_or(0.0, |o| self.offset_score(o));
        if self.printing && (self.err_fast > 0.85 || score < 0.45) {
            self.printing = false;
            self.held_since = Some(self.t);
        } else if !self.printing && self.err_fast < 0.6 && score >= 0.6 {
            self.printing = true;
            self.held_since = None;
            self.release_held();
        }
        if self.quiet {
            return;
        }
        let row = code.map(|c| CCIR_INDEX[c as usize]).filter(|&r| r != 255);
        if self.call_phase {
            self.call_chars += 1;
            let letter = row.map(|r| SYMS[r as usize].letter);
            match (code, letter) {
                (Some(BETA), _) => {
                    self.end_group();
                    return;
                }
                (_, Some('\r' | '\n')) => self.call_phase = false,
                (_, Some(c)) if id_number(c).is_some() => {
                    self.group.push(c);
                    return;
                }
                (Some(PHASING1 | PHASING2), _) => return,
                _ => {
                    self.group.push('?');
                    if self.call_chars > CALL_MAX {
                        self.call_phase = false;
                    } else {
                        return;
                    }
                }
            }
        }
        // Shifts are applied directly, as in mode A: RTTY's guard against
        // a misread figure shift would print "(5 NM), " as "(5 NMLN ".
        match code {
            None => self.take_char('*'),
            Some(PHASING1 | PHASING2 | IDLE_BETA) => {}
            Some(_) => {
                if let Some(r) = row
                    && let Some(c) = self.shift.apply(&SYMS[r as usize], false, false)
                {
                    self.take_char(c);
                }
            }
        }
    }

    /// One group of the selective call signal ended by an idle signal beta.
    fn end_group(&mut self) {
        let g = std::mem::take(&mut self.group);
        let n = g.chars().count();
        if !(n == 4 || n == 7) || g.contains('?') {
            self.last_group = None;
            return;
        }
        // Two groups alike, so a group caught partway is never taken for a
        // four-signal call.
        if self.last_group.as_ref() == Some(&g) && self.selcall.as_ref() != Some(&g) {
            self.selcall = Some(g.clone());
            self.events.push(SitorEvent::SelectiveCall { selcall: g.clone() });
        }
        self.last_group = Some(g);
    }

    fn take_char(&mut self, c: char) {
        let c = if c == '\r' { '\n' } else { c };
        if c == '\n' && self.last_nl {
            return;
        }
        // Between phasing and the first character read, an unreadable one
        // is the turn from phasing to what follows, not lost text.
        if c == '*' && self.phased && !self.traffic {
            return;
        }
        self.traffic |= c != '*';
        self.last_nl = c == '\n';
        self.last_char = self.t;
        if self.printing {
            self.show_char(c);
        } else if let Some(t) = self.held_since {
            if (self.t - t) as f32 > FADE_HOLD_S * BAUD * self.spb || self.held.len() >= HELD_MAX {
                self.held.clear();
                self.held_since = None;
            } else {
                self.held.push(c);
            }
        }
    }

    fn release_held(&mut self) {
        for c in std::mem::take(&mut self.held) {
            self.show_char(c);
        }
    }

    fn show_char(&mut self, c: char) {
        if self.selective != self.line_selective {
            self.flush_line();
            self.line_selective = self.selective;
        }
        if let Some(text) = self.lines.push(c) {
            self.emit_line(text);
        }
    }

    fn flush_line(&mut self) {
        if let Some(text) = self.lines.take() {
            self.emit_line(text);
        }
    }

    fn emit_line(&mut self, text: String) {
        self.events.push(SitorEvent::Text {
            mode: SitorMode::B,
            from: None,
            station: None,
            selective: self.line_selective,
            text,
        });
    }

    /// Lets go of text after a pause.
    fn idle(&mut self) {
        let idle = (self.t - self.last_char) as f32 / (BAUD * self.spb);
        if idle > IDLE_FLUSH_S && !self.lines.is_empty() {
            self.flush_line();
        }
    }

    fn link_label(&self) -> &'static str {
        match (self.sync, self.quiet, self.phased) {
            (None, _, _) => "standby",
            (_, true, _) => "ended",
            (_, _, true) if self.call_phase => "phasing",
            _ => "traffic",
        }
    }
}

// ---------------------------------------------------------------------------
// The decoder.

/// Events kept while the mode is undecided.
const PENDING_MAX: usize = 400;

pub struct SitorDecoder {
    rx: FskReceiver,
    samples: Vec<FskSample>,
    hist: Hist,
    fec: Fec,
    arq: Arq,
    forced: SitorMode,
    detected: SitorMode,
    pending_a: Vec<SitorEvent>,
    pending_b: Vec<SitorEvent>,
    events: Vec<SitorEvent>,
}

impl SitorDecoder {
    pub fn new(fs: f32) -> Self {
        let rx = FskReceiver::new(fs, BAUD, SHIFT_HZ, None);
        let fs2 = rx.rate();
        let fec = Fec::new(rx.samples_per_bit(), rx.filter_len());
        Self {
            rx,
            samples: Vec::new(),
            hist: Hist::new(),
            fec,
            arq: Arq::new(fs2),
            forced: SitorMode::Auto,
            detected: SitorMode::Auto,
            pending_a: Vec::new(),
            pending_b: Vec::new(),
            events: Vec::new(),
        }
    }

    /// Copy only mode A or only mode B, or decide (Auto).
    pub fn set_mode(&mut self, m: SitorMode) {
        self.forced = m;
        self.pending_a.clear();
        self.pending_b.clear();
    }

    pub fn push(&mut self, audio: &[f32]) {
        let mut samples = std::mem::take(&mut self.samples);
        self.rx.push(audio, &mut samples);
        for s in &samples {
            if s.fresh {
                self.arq.run(&self.hist);
                self.route();
                self.fec.restart();
                self.arq.restart(self.hist.n);
            }
            self.hist.push(s.hi, s.lo);
            self.fec.sample(s.hi, s.lo);
        }
        self.samples = samples;
        self.arq.run(&self.hist);
        self.fec.idle();
        self.decide();
        self.route();
        self.rx.set_tracking(self.fec.synced() || self.arq.locked());
    }

    pub fn take(&mut self) -> Vec<SitorEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn status(&self) -> SitorStatus {
        let mode = self.mode();
        let locked = self.rx.locked();
        let a = mode == SitorMode::A;
        let p = &self.arq.proto;
        SitorStatus {
            locked,
            mode,
            forced: self.forced != SitorMode::Auto,
            center_hz: locked.then(|| self.rx.center_hz()),
            snr_db: (locked && self.rx.snr_db().is_finite()).then(|| self.rx.snr_db()),
            synced: if a { self.arq.locked() } else { self.fec.synced() },
            cycle_locked: self.arq.locked() && mode != SitorMode::B,
            irs_heard: self.arq.cs_lock.is_some() && mode != SitorMode::B,
            cycle_phase_ms: if mode != SitorMode::B { self.arq.cycle_phase_ms() } else { None },
            error_rate: if a { p.err_rate } else { self.fec.err_rate },
            selcall: if a { p.selcall.clone() } else { self.fec.selcall.clone() },
            identity: if a { p.identity.clone() } else { None },
            link: match mode {
                SitorMode::A => p.link_label(),
                SitorMode::B => self.fec.link_label(),
                SitorMode::Auto => "standby",
            },
            sending: (a && p.link != Link::Standby).then(|| p.side(p.iss)),
            selective: !a && self.fec.selective,
            reverse: if a {
                self.arq.lock.is_some_and(|l| l.1 == 1)
            } else {
                (if self.fec.phased { self.fec.base_pol } else { self.fec.pol }) == 1
            },
        }
    }

    fn mode(&self) -> SitorMode {
        if self.forced != SitorMode::Auto { self.forced } else { self.detected }
    }

    /// Mode A when the cycle is locked and the signal comes in bursts with
    /// quiet between; mode B when the continuous stream is aligned. Neither
    /// aligns on the other's signal (an ARQ block's alignment moves three
    /// bits each cycle; an FEC stream has no quiet in it), so the two never
    /// both hold.
    fn decide(&mut self) {
        if self.arq.locked() && self.arq.bursty() {
            self.detected = SitorMode::A;
        } else if self.fec.synced() && !self.arq.bursty() {
            self.detected = SitorMode::B;
        } else if !self.rx.locked() && !self.arq.locked() && !self.fec.synced() {
            self.detected = SitorMode::Auto;
        }
    }

    fn route(&mut self) {
        let a = std::mem::take(&mut self.arq.proto.events);
        let b = std::mem::take(&mut self.fec.events);
        match self.mode() {
            SitorMode::A => {
                self.events.append(&mut self.pending_a);
                self.events.extend(a);
                self.pending_b.clear();
            }
            SitorMode::B => {
                self.events.append(&mut self.pending_b);
                self.events.extend(b);
                self.pending_a.clear();
            }
            SitorMode::Auto => {
                self.pending_a.extend(a);
                self.pending_b.extend(b);
                for p in [&mut self.pending_a, &mut self.pending_b] {
                    if p.len() > PENDING_MAX {
                        let n = p.len() - PENDING_MAX;
                        p.drain(..n);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtty::testsig::*;
    use crate::rtty::{FIGS, LTRS};
    use num_complex::Complex32;
    use std::f64::consts::TAU;

    /// The live rate, deliberately not a round number.
    const FS: f32 = 47_904.0;

    fn letter(c: char) -> u8 {
        SYMS.iter().find(|s| s.letter == c).unwrap().ccir
    }

    /// CCIR 476 codes for `text`, with shifts, starting in letters.
    fn encode(text: &str) -> Vec<u8> {
        let fig = |c: char| SYMS.iter().find(|s| s.figure == c);
        let mut out = vec![letter(LTRS)];
        let mut figs = false;
        for c in text.chars() {
            if matches!(c, ' ' | '\r' | '\n') {
                out.push(letter(c));
            } else if c.is_ascii_uppercase() {
                if figs {
                    out.push(letter(LTRS));
                    figs = false;
                }
                out.push(letter(c));
            } else if let Some(s) = fig(c) {
                if !figs {
                    out.push(letter(FIGS));
                    figs = true;
                }
                out.push(s.ccir);
            } else {
                panic!("no CCIR 476 code for {c:?}");
            }
        }
        out
    }

    /// Words from one station, sent from `start` (transmitter seconds).
    struct Burst {
        start: f64,
        codes: Vec<u8>,
        station: usize,
    }

    /// One station's level and tone offset.
    #[derive(Clone, Copy)]
    struct Station {
        amp: f32,
        df: f32,
    }

    /// The bursts as received: continuous-phase FSK per burst, each
    /// station through its own `ch`, sampled by a clock `ppm` off, with the
    /// tones swapped when `rev` (the wrong sideband).
    #[allow(clippy::too_many_arguments)]
    fn render(
        bursts: &[Burst],
        stations: &[Station],
        dur: f64,
        center: f32,
        ppm: f64,
        ch: Channel,
        rev: bool,
        rng: &mut Rng,
    ) -> Vec<f32> {
        let fs_true = FS as f64 * (1.0 + ppm * 1e-6);
        let n = (dur * fs_true) as usize;
        let mut z = vec![vec![Complex32::new(0.0, 0.0); n]; stations.len()];
        for b in bursts {
            let st = stations[b.station];
            let nb = 7 * b.codes.len();
            let i0 = (b.start * fs_true).ceil() as usize;
            let i1 = (((b.start + nb as f64 / BAUD as f64) * fs_true).ceil() as usize).min(n);
            let mut ph = rng.uniform() * TAU;
            for (i, v) in z[b.station].iter_mut().enumerate().take(i1).skip(i0) {
                let bit = (((i as f64 / fs_true - b.start) * BAUD as f64) as usize).min(nb - 1);
                let one = (b.codes[bit / 7] >> (bit % 7) & 1 == 1) != rev;
                let f = (center + st.df) as f64 + if one { 85.0 } else { -85.0 };
                *v += st.amp * Complex32::new(ph.cos() as f32, ph.sin() as f32);
                ph = (ph + TAU * f / fs_true) % TAU;
            }
        }
        let mut out = vec![0.0f32; n];
        for s in &z {
            match ch {
                Channel::Watterson(delay_ms, spread) => {
                    let g1 = rayleigh(n, FS, spread, rng);
                    let g2 = rayleigh(n, FS, spread, rng);
                    let d = (delay_ms * 1e-3 * FS) as usize;
                    for i in 0..n {
                        let late = if i >= d { s[i - d] * g2[i] } else { Complex32::new(0.0, 0.0) };
                        out[i] += std::f32::consts::FRAC_1_SQRT_2 * (s[i] * g1[i] + late).re;
                    }
                }
                _ => out.iter_mut().zip(s).for_each(|(o, v)| *o += v.re),
            }
        }
        out
    }

    fn run(x: &[f32], mode: SitorMode) -> (Vec<SitorEvent>, Vec<SitorStatus>) {
        let mut d = SitorDecoder::new(FS);
        d.set_mode(mode);
        let mut ev = Vec::new();
        let mut st = Vec::new();
        let mut n = 0usize;
        chunks(x, |c| {
            d.push(c);
            ev.extend(d.take());
            n += c.len();
            if n >= FS as usize {
                n = 0;
                st.push(d.status());
            }
        });
        st.push(d.status());
        (ev, st)
    }

    fn text_of(ev: &[SitorEvent], want: Option<ArqSide>) -> String {
        ev.iter()
            .filter_map(|e| match e {
                SitorEvent::Text { from, text, .. } if *from == want => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // -----------------------------------------------------------------------
    // Mode A.

    fn blocks(text: &str) -> Vec<Vec<u8>> {
        let mut c = encode(text);
        while !c.len().is_multiple_of(3) {
            c.push(BETA);
        }
        c.chunks(3).map(<[u8]>::to_vec).collect()
    }

    /// The control signal asking for information block `i` (counting from
    /// 0): blocks alternate "block 1" and "block 2".
    fn cs_for(i: usize) -> u8 {
        if i.is_multiple_of(2) { CS1 } else { CS2 }
    }

    /// One cycle: what the master and the slave send.
    type Cycle = (Option<Vec<u8>>, Option<Vec<u8>>);

    /// The master transmits at the start of each 450 ms cycle; the slave a
    /// fixed time after the end of what it heard (M.625 Fig. 1).
    fn link_bursts(cycles: &[Cycle], t0: f64, delay: f64) -> Vec<Burst> {
        let mut out = Vec::new();
        for (n, (m, s)) in cycles.iter().enumerate() {
            let at = t0 + n as f64 * CYCLE_S;
            let mlen = m.as_ref().map_or(0.07, |v| 0.07 * v.len() as f64);
            if let Some(v) = m {
                out.push(Burst { start: at, codes: v.clone(), station: 0 });
            }
            if let Some(v) = s {
                out.push(Burst { start: at + mlen + delay, codes: v.clone(), station: 1 });
            }
        }
        out
    }

    const CALLER_TEXT: &str = "CQ DE QRVEDSK\r\nWX 1200 UTC: WIND NW 15 KT, SEA 4\r\nVIS GOOD (5 NM)\r\n+?";
    const CALLER_LINES: &str = "CQ DE QRVEDSK\nWX 1200 UTC: WIND NW 15 KT, SEA 4\nVIS GOOD (5 NM)\n+?";
    const CALLED_TEXT: &str = "QSL TNX 73\r\n";
    const CALLED_LINES: &str = "QSL TNX 73";

    /// The block repeated after the IRS asks for it again, and the block
    /// after which the ISS misses the control signal and sends RQ RQ RQ.
    const REPEATED: usize = 3;
    const RQ_AFTER: usize = 6;

    /// A whole link: the call (the seven-signal call of M.625 Fig. 3,
    /// KQVZRSE called by QRVEDSK, with automatic identification; or a
    /// four-signal call), the caller's traffic with one block repeated
    /// (its first copy hit) and one RQ RQ RQ exchange, a change of
    /// direction offered with "+?", the called station's traffic and the
    /// end of communication.
    fn link_script(four: bool) -> Vec<Cycle> {
        let l = |s: &str| s.chars().map(letter).collect::<Vec<u8>>();
        let mut c: Vec<Cycle> = Vec::new();
        let calls: Vec<Vec<u8>> = if four {
            vec![vec![letter('K'), RQ, letter('Q')], vec![letter('V'), letter('Z'), RQ]]
        } else {
            vec![vec![letter('K'), RQ, letter('Q')], vec![RQ, letter('V'), letter('Z')], l("RSE")]
        };
        for rep in 0..3 {
            for (j, b) in calls.iter().enumerate() {
                let last = rep == 2 && j == calls.len() - 1;
                let answer = if four { CS1 } else { CS4 };
                c.push((Some(b.clone()), last.then(|| vec![answer])));
            }
        }
        if !four {
            let ids = [vec![letter('Q'), ALPHA, letter('R')], vec![ALPHA, letter('V'), letter('E')], l("DSK")];
            for (b, ck) in ids.iter().zip(['P', 'E', 'D']) {
                c.push((Some(b.clone()), Some(vec![letter(ck)])));
            }
            c.push((Some(vec![RQ; 3]), Some(vec![CS1])));
        }
        let mb = blocks(CALLER_TEXT);
        assert!(mb.len() > RQ_AFTER + 2);
        for (i, b) in mb.iter().enumerate() {
            if i == REPEATED {
                // The IRS asks for this block again; the copy it got (and
                // the one heard here) has a bit hit.
                let mut hit = b.clone();
                hit[1] ^= 0x01;
                c.push((Some(hit), Some(vec![cs_for(i)])));
            }
            if i == mb.len() - 1 {
                c.push((Some(b.clone()), Some(vec![CS3])));
            } else if i == RQ_AFTER {
                c.push((Some(b.clone()), Some(vec![cs_for(i + 1)])));
                c.push((Some(vec![RQ; 3]), Some(vec![cs_for(i + 1)])));
            } else {
                c.push((Some(b.clone()), Some(vec![cs_for(i + 1)])));
            }
        }
        c.push((Some(vec![BETA, ALPHA, BETA]), None));
        c.push((None, Some(vec![RQ; 3])));
        let sb = blocks(CALLED_TEXT);
        for (i, b) in sb.iter().enumerate() {
            c.push((Some(vec![cs_for(i)]), Some(b.clone())));
        }
        c.push((Some(vec![cs_for(sb.len())]), Some(vec![ALPHA; 3])));
        c.push((Some(vec![cs_for(sb.len() + 1)]), None));
        c
    }

    struct LinkRun {
        snr: f32,
        irs_db: f32,
        center: f32,
        irs_df: f32,
        ppm: f64,
        ch: Channel,
        four: bool,
        reverse: bool,
        seed: u64,
    }

    fn link_audio(r: &LinkRun) -> Vec<f32> {
        let amp = 0.2;
        let cycles = link_script(r.four);
        let t0 = 1.3;
        let bursts = link_bursts(&cycles, t0, 0.0437);
        let stations = [Station { amp, df: 0.0 }, Station { amp: amp * 10f32.powf(r.irs_db / 20.0), df: r.irs_df }];
        let mut rng = Rng::new(r.seed);
        let dur = t0 + cycles.len() as f64 * CYCLE_S + 3.0;
        let mut x = render(&bursts, &stations, dur, r.center, r.ppm, r.ch, r.reverse, &mut rng);
        add_noise(&mut x, amp, r.snr, FS, &mut rng);
        x
    }

    #[test]
    fn sitor_checksums_follow_m625() {
        let id: Vec<char> = "PEARDBY".chars().collect();
        assert_eq!(checksums(&id), Some(['Z', 'E', 'R']));
        let id: Vec<char> = "KQVZRSE".chars().collect();
        assert_eq!(checksums(&id), Some(['P', 'E', 'D']));
        // The control signals are valid words, and none is an
        // identification signal.
        for c in [CS1, CS2, CS3, CS4, CS5] {
            assert!(valid(c));
            assert!(id_letter(c).is_none());
        }
        assert_eq!((letter('L'), letter('N'), letter('G'), letter('H')), (CS1, CS3, CS4, CS5));
    }

    fn check_link(r: &LinkRun) {
        let x = link_audio(r);
        let (ev, st) = run(&x, SitorMode::Auto);
        let calls: Vec<&String> = ev
            .iter()
            .filter_map(|e| match e {
                SitorEvent::Call { selcall } => Some(selcall),
                _ => None,
            })
            .collect();
        let want_call = if r.four { "KQVZ" } else { "KQVZRSE" };
        assert_eq!(calls, vec![want_call], "{ev:#?}");
        let answer = if r.four { "CS1" } else { "CS4" };
        assert!(ev.contains(&SitorEvent::Answer { control: answer, rephase: false }), "{ev:#?}");
        if !r.four {
            assert!(ev.contains(&SitorEvent::Identity { station: "QRVEDSK".into(), checksum_ok: Some(true) }), "{ev:#?}");
        }
        assert_eq!(text_of(&ev, Some(ArqSide::Caller)), CALLER_LINES, "{ev:#?}");
        assert_eq!(text_of(&ev, Some(ArqSide::Called)), CALLED_LINES, "{ev:#?}");
        let co: Vec<&SitorEvent> = ev.iter().filter(|e| e.kind() == "changeover").collect();
        assert_eq!(
            co,
            vec![&SitorEvent::Changeover { now_sending: ArqSide::Called, requested_by: Some("iss"), inferred: false }],
            "{ev:#?}"
        );
        assert!(ev.contains(&SitorEvent::LinkEnd { reason: "endOfCommunication" }), "{ev:#?}");
        assert!(!ev.iter().any(|e| e.kind() == "linkEnd" && *e != SitorEvent::LinkEnd { reason: "endOfCommunication" }));
        let mid = &st[st.len() / 2];
        assert_eq!(mid.mode, SitorMode::A, "{mid:#?}");
        assert!(mid.cycle_locked && mid.locked, "{mid:#?}");
        assert!(st.iter().any(|s| s.irs_heard), "{st:#?}");
        assert!(st.iter().all(|s| s.mode != SitorMode::B));
        // The called station's lines carry its selective call.
        let called_station = ev.iter().find_map(|e| match e {
            SitorEvent::Text { from: Some(ArqSide::Called), station, .. } => Some(station.clone()),
            _ => None,
        });
        assert_eq!(called_station, Some(Some(want_call.to_string())));
    }

    #[test]
    fn sitor_a_link_seven_signal_weak_irs() {
        // The IRS 15 dB under the ISS: it is at -3 dB in 2500 Hz, and
        // after the change-over it is the ISS.
        for (center, ppm, seed) in [(1750.0, 100.0, 1), (1650.0, -100.0, 2)] {
            check_link(&LinkRun {
                snr: 12.0,
                irs_db: -15.0,
                center,
                irs_df: 9.0,
                ppm,
                ch: Channel::Awgn,
                four: false,
                reverse: false,
                seed,
            });
        }
    }

    #[test]
    fn sitor_a_link_four_signal_reversed() {
        // Heard on the other sideband: every word in 3B/4Y.
        let r = LinkRun {
            snr: 6.0,
            irs_db: -6.0,
            center: 1700.0,
            irs_df: -7.0,
            ppm: 60.0,
            ch: Channel::Awgn,
            four: true,
            reverse: true,
            seed: 3,
        };
        check_link(&r);
        let (_, st) = run(&link_audio(&r), SitorMode::A);
        assert!(st[st.len() / 2].reverse);
    }

    /// Through fading, a third party loses what the IRS had repeated for
    /// itself only when its own copy was bad too; here the IRS never asks,
    /// so what fades out here is lost and the copy is judged by its error
    /// rate. The link's structure must still come through.
    #[test]
    fn sitor_a_link_through_fading() {
        let (mut errs, mut total) = (0usize, 0usize);
        for seed in 1..=4u64 {
            let r = LinkRun {
                snr: 15.0,
                irs_db: -6.0,
                center: 1650.0 + 25.0 * seed as f32,
                irs_df: -7.0,
                ppm: if seed.is_multiple_of(2) { 100.0 } else { -100.0 },
                ch: Channel::MODERATE,
                four: seed.is_multiple_of(2),
                reverse: false,
                seed: 40 + seed,
            };
            let (ev, st) = run(&link_audio(&r), SitorMode::Auto);
            let want_call = if r.four { "KQVZ" } else { "KQVZRSE" };
            assert!(ev.contains(&SitorEvent::Call { selcall: want_call.into() }), "{ev:#?}");
            assert!(ev.iter().any(|e| matches!(e, SitorEvent::Changeover { now_sending: ArqSide::Called, .. })), "{ev:#?}");
            assert!(st.iter().all(|s| s.mode != SitorMode::B));
            errs += levenshtein(&text_of(&ev, Some(ArqSide::Caller)), CALLER_LINES);
            errs += levenshtein(&text_of(&ev, Some(ArqSide::Called)), CALLED_LINES);
            total += CALLER_LINES.len() + CALLED_LINES.len();
        }
        let cer = errs as f32 / total as f32;
        println!("SITOR A, CCIR moderate, ISS +15 dB / IRS +9 dB: CER {:.1}%", 100.0 * cer);
        assert!(cer <= 0.08, "CER {cer}");
    }

    #[test]
    fn sitor_a_forced_b_copies_nothing() {
        let x = link_audio(&LinkRun {
            snr: 12.0,
            irs_db: -15.0,
            center: 1700.0,
            irs_df: 0.0,
            ppm: 0.0,
            ch: Channel::Awgn,
            four: false,
            reverse: false,
            seed: 4,
        });
        let (ev, st) = run(&x, SitorMode::B);
        assert!(ev.is_empty(), "{ev:#?}");
        assert!(st.iter().all(|s| s.mode == SitorMode::B && s.forced));
    }

    // -----------------------------------------------------------------------
    // Mode B.

    const B_TEXT: &str =
        "SECURITE SECURITE\r\nWX 1200 UTC: WIND NW 15 KT, SEA 4.\r\nVIS GOOD (5 NM), SWELL 2 M\r\nNEXT REPORT 1800 UTC\r\n";
    const B_LINES: &str =
        "SECURITE SECURITE\nWX 1200 UTC: WIND NW 15 KT, SEA 4.\nVIS GOOD (5 NM), SWELL 2 M\nNEXT REPORT 1800 UTC";

    /// The DX/RX slots: phasing (RQ in DX, alpha in RX), the content in DX
    /// slots and again two DX slots (four characters) later in RX slots,
    /// then the end of emission.
    fn fec_slots(content: &[u8], pairs: usize, end: u8) -> Vec<u8> {
        let mut dx = vec![RQ; pairs];
        let first = dx.len();
        dx.extend_from_slice(content);
        let last = dx.len();
        dx.extend([end; 6]);
        let mut out = Vec::new();
        for i in 0..dx.len() {
            out.push(dx[i]);
            let rx = match i.checked_sub(2) {
                Some(j) if (first..last).contains(&j) => dx[j],
                Some(j) if j >= last => end,
                _ => ALPHA,
            };
            out.push(rx);
        }
        out
    }

    fn fec_audio(slots: Vec<u8>, snr: f32, center: f32, ppm: f64, ch: Channel, rev: bool, seed: u64) -> Vec<f32> {
        let amp = 0.2;
        let mut rng = Rng::new(seed);
        let dur = 1.0 + slots.len() as f64 * 0.07 + 3.0;
        let bursts = [Burst { start: 1.0, codes: slots, station: 0 }];
        let mut x = render(&bursts, &[Station { amp, df: 0.0 }], dur, center, ppm, ch, rev, &mut rng);
        add_noise(&mut x, amp, snr, FS, &mut rng);
        x
    }

    #[test]
    fn sitor_b_collective_round_trips() {
        for (center, ppm, rev, seed) in [(1750.0, -100.0, false, 11), (1650.0, 100.0, true, 14)] {
            let x = fec_audio(fec_slots(&encode(B_TEXT), 20, ALPHA), 10.0, center, ppm, Channel::Awgn, rev, seed);
            let (ev, st) = run(&x, SitorMode::Auto);
            assert_eq!(text_of(&ev, None), B_LINES, "{ev:#?}");
            assert!(ev.iter().all(|e| !matches!(e, SitorEvent::Text { selective: true, .. })));
            assert_eq!(ev.first(), Some(&SitorEvent::Phasing), "{ev:#?}");
            assert_eq!(ev.iter().filter(|e| **e == SitorEvent::EndOfEmission).count(), 1, "{ev:#?}");
            let mid = &st[st.len() / 2];
            assert_eq!(mid.mode, SitorMode::B, "{mid:#?}");
            assert!(mid.synced && !mid.cycle_locked && !mid.selective && mid.reverse == rev, "{mid:#?}");
            assert!((mid.center_hz.unwrap() - center).abs() < 3.0, "{mid:#?}");
            assert!(st.iter().all(|s| s.mode != SitorMode::A));
        }
    }

    /// Phasing in normal ratio, then six call groups (the identity and an
    /// idle beta), CR LF and the traffic, all inverted (M.625 § 4.5).
    fn selective_slots(id: &str, text: &str) -> Vec<u8> {
        let mut content = Vec::new();
        for _ in 0..6 {
            content.extend(id.chars().map(letter));
            content.push(BETA);
        }
        content.extend(encode(&format!("\r\n{text}")));
        let inv: Vec<u8> = content.iter().map(|c| !c & 0x7f).collect();
        let slots = fec_slots(&inv, 20, !ALPHA & 0x7f);
        // The phasing stays in normal ratio.
        let n = 2 * 20;
        slots.iter().enumerate().map(|(i, &c)| if i < n { [RQ, ALPHA][i % 2] } else { c }).collect()
    }

    #[test]
    fn sitor_b_selective_call_and_inverted_traffic() {
        for (id, center, ppm, seed) in [("KQVZ", 1650.0, 100.0, 12), ("PEARDBY", 1700.0, -50.0, 13)] {
            let x = fec_audio(selective_slots(id, B_TEXT), 6.0, center, ppm, Channel::Awgn, false, seed);
            let (ev, st) = run(&x, SitorMode::Auto);
            let calls: Vec<&SitorEvent> = ev.iter().filter(|e| e.kind() == "selectiveCall").collect();
            assert_eq!(calls, vec![&SitorEvent::SelectiveCall { selcall: id.into() }], "{ev:#?}");
            assert_eq!(text_of(&ev, None), B_LINES, "{ev:#?}");
            assert!(ev.iter().all(|e| !matches!(e, SitorEvent::Text { selective: false, .. })), "{ev:#?}");
            let mid = &st[st.len() / 2];
            assert_eq!(mid.mode, SitorMode::B);
            assert!(mid.selective && !mid.reverse, "{mid:#?}");
            assert_eq!(mid.selcall.as_deref(), Some(id));
        }
    }

    fn b_cer(ch: Channel, snr: f32, seed: u64) -> f32 {
        let center = 1650.0 + 50.0 * (seed % 3) as f32;
        let ppm = if seed.is_multiple_of(2) { 100.0 } else { -100.0 };
        let x = fec_audio(fec_slots(&encode(B_TEXT), 20, ALPHA), snr, center, ppm, ch, false, seed);
        let (ev, _) = run(&x, SitorMode::Auto);
        if std::env::var("USDR_SITOR_SHOW").is_ok() {
            println!("seed {seed} snr {snr}: {:?}", text_of(&ev, None));
        }
        levenshtein(&text_of(&ev, None), B_LINES) as f32 / B_LINES.len() as f32
    }

    #[test]
    fn sitor_b_through_fading() {
        for (ch, snr, max) in [(Channel::Awgn, -5.0f32, 0.01f32), (Channel::MODERATE, 0.0, 0.03), (Channel::POOR, 0.0, 0.05)] {
            let mut total = 0.0;
            for seed in 1..=3 {
                total += b_cer(ch, snr, 100 + seed);
            }
            let cer = total / 3.0;
            println!("SITOR B {} at {snr:+.0} dB: CER {:.2}%", ch.name(), 100.0 * cer);
            assert!(cer <= max, "{}: CER {cer}", ch.name());
        }
    }

    /// Character error rates by SNR, for reference:
    /// `USDR_CER_TABLE=1 cargo test --release -p scannerd-engine sitor_cer_table -- --nocapture`.
    #[test]
    fn sitor_cer_table() {
        if std::env::var("USDR_CER_TABLE").is_err() {
            return;
        }
        let snrs = [6.0f32, 3.0, 0.0, -3.0, -5.0, -7.0, -9.0];
        println!("SITOR B: CER % by SNR (2500 Hz), 3 seeds");
        println!("{:>14} {}", "", snrs.iter().map(|s| format!("{s:>7.0}")).collect::<String>());
        for ch in [Channel::Awgn, Channel::MODERATE, Channel::POOR] {
            let row: String = snrs
                .iter()
                .map(|&snr| {
                    let cer = (1..=3).map(|s| b_cer(ch, snr, 500 + s)).sum::<f32>() / 3.0;
                    format!("{:>7.1}", 100.0 * cer)
                })
                .collect();
            println!("{:>14} {row}", ch.name());
        }
        println!("SITOR A: CER % by ISS SNR (2500 Hz), IRS 15 dB weaker, 2 seeds");
        println!("{:>14} {}", "", snrs.iter().map(|s| format!("{s:>7.0}")).collect::<String>());
        for ch in [Channel::Awgn, Channel::MODERATE] {
            let row: String = snrs
                .iter()
                .map(|&snr| {
                    let (mut e, mut n) = (0, 0);
                    for seed in 1..=2u64 {
                        let r = LinkRun {
                            snr: snr + 15.0,
                            irs_db: -15.0,
                            center: 1700.0,
                            irs_df: 5.0,
                            ppm: 50.0,
                            ch,
                            four: false,
                            reverse: false,
                            seed: 600 + seed,
                        };
                        let (ev, _) = run(&link_audio(&r), SitorMode::Auto);
                        e += levenshtein(&text_of(&ev, Some(ArqSide::Caller)), CALLER_LINES);
                        e += levenshtein(&text_of(&ev, Some(ArqSide::Called)), CALLED_LINES);
                        n += CALLER_LINES.len() + CALLED_LINES.len();
                    }
                    format!("{:>7.1}", 100.0 * e as f32 / n as f32)
                })
                .collect();
            println!("{:>14} {row}  (IRS SNR = column)", ch.name());
        }
    }

    #[test]
    fn sitor_noise_alone_yields_nothing() {
        for (seed, amp) in [(21u64, 1e-4f32), (22, 0.05), (23, 0.8)] {
            let mut rng = Rng::new(seed);
            let x: Vec<f32> = (0..(40.0 * FS) as usize).map(|_| amp * rng.gauss()).collect();
            let (ev, st) = run(&x, SitorMode::Auto);
            assert!(ev.is_empty(), "{ev:#?}");
            assert!(st.iter().all(|s| s.mode == SitorMode::Auto));
        }
    }

    #[test]
    fn sitor_realtime_factor() {
        let x = link_audio(&LinkRun {
            snr: 10.0,
            irs_db: -10.0,
            center: 1700.0,
            irs_df: 0.0,
            ppm: 0.0,
            ch: Channel::Awgn,
            four: false,
            reverse: false,
            seed: 9,
        });
        let mut d = SitorDecoder::new(FS);
        let t = std::time::Instant::now();
        chunks(&x, |c| d.push(c));
        let el = t.elapsed().as_secs_f64();
        let audio = x.len() as f64 / FS as f64;
        println!("SITOR: {audio:.1} s of audio in {:.1} ms, {:.3}% of one core", el * 1e3, 100.0 * el / audio);
        assert!(el / audio < 0.05);
    }
}
