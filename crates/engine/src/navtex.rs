//! NAVTEX: maritime safety broadcasts on 518 and 490 kHz, sent in SITOR
//! mode B (ITU-R M.476 / M.625 forward error correction) at 100 baud with
//! 170 Hz shift, in the message format of ITU-R M.540.
//!
//! The tone pair is found and demodulated by the RTTY decoder's
//! [`FskReceiver`]. From there:
//!
//! - **Bits.** There are no start or stop bits; the bit clock runs
//!   continuously, pulled toward the zero crossings of the soft decision,
//!   and only by crossings that follow a firm bit, so it holds through a
//!   fade instead of wandering after noise.
//! - **Characters.** Each of the 35 valid CCIR 476 characters has exactly
//!   four B (upper tone) and three Y bits, and the first bit sent is the
//!   code's least significant. Random bits pass that test a little over a
//!   quarter of the time, so the one of the seven bit offsets where words
//!   pass far more often than at the others is the character alignment. A
//!   word with three B and four Y passes in the inverted sense, which is
//!   how a station heard with the tones swapped is recognized. A fade
//!   empties every offset's score alike, so the alignment is kept until
//!   another clearly takes over or seconds pass with none.
//! - **Time diversity.** Every character is sent twice: first in a DX slot,
//!   then again four characters later (five slots, 35 bits, 350 ms) in an
//!   RX slot. Which slots are DX comes from the phasing signals (phasing 2
//!   is sent in DX slots, phasing 1 in RX slots) and from RX words
//!   repeating the word five slots back. The soft bits of both copies, as
//!   log-likelihood ratios, are summed and the most likely of the 35 valid
//!   words taken (see [`combine`]); one that no word stands out for prints
//!   as `*`.
//! - **Impulse noise.** On 518 kHz at night lightning crashes come several
//!   a second, 20 to 40 dB over the signal. The receiver blanks them, and
//!   a bit's log-likelihood ratio shrinks with the share of it blanked, so
//!   a crash leaves its characters to their other copies instead of
//!   outvoting them; the fading correction's levels and the bit clock are
//!   held through it.
//! - **Messages.** `ZCZC B1B2B3B4` opens one (station, subject, serial) and
//!   `NNNN` closes it.

use crate::rtty::{Atc, FskReceiver, FskSample, IDLE_FLUSH_S, Lines, SYMS, Shift};

/// NAVTEX is 100 baud, 170 Hz shift everywhere.
const BAUD: f32 = 100.0;
const SHIFT_HZ: f32 = 170.0;

/// Phasing signal 1 (idle signal alpha), BBBBYYY: sent in RX slots while
/// phasing, and in DX slots three times to end an emission.
pub(crate) const PHASING1: u8 = 0x0f;
/// Phasing signal 2 (signal repetition in ARQ), YBBYYBB: sent in DX slots
/// while phasing.
pub(crate) const PHASING2: u8 = 0x66;
/// Idle signal beta, BBYYBBY.
pub(crate) const IDLE_BETA: u8 = 0x33;

/// Words averaged for each alignment's validity score, and the scores to
/// take and to lose an alignment. Random bits score about 0.27.
pub(crate) const SYNC_LEN: f32 = 12.0;
pub(crate) const SYNC_TAKE: f32 = 0.8;
/// Or this much, when it leads every other alignment by `SYNC_LEAD`.
pub(crate) const SYNC_TAKE_WEAK: f32 = 0.55;
pub(crate) const SYNC_LEAD: f32 = 0.3;
pub(crate) const SYNC_LOSE: f32 = 0.45;
/// Words an alignment must have been scored on before it can be taken.
pub(crate) const SYNC_MIN_WORDS: u32 = 12;
/// Bit clock loop gains while hunting and while in sync.
pub(crate) const CLOCK_GAIN_HUNT: f32 = 0.08;
pub(crate) const CLOCK_GAIN_SYNC: f32 = 0.03;
/// A message with no character for this long has been lost.
const MESSAGE_TIMEOUT_S: f32 = 20.0;
/// Bits (about three seconds) of a failing alignment before it is given up
/// with nothing better in sight.
pub(crate) const SYNC_LOSE_BITS: u32 = 300;
/// Soft bits are capped here, so one very clean copy cannot drown a
/// confident disagreement from the other.
pub(crate) const LLR_MAX: f32 = 30.0;
/// The best word must beat the next by this much (twice the difference of
/// their log-likelihoods) to be read.
pub(crate) const ML_MARGIN: f32 = 2.0;
/// Text held through a fade, and for how long.
pub(crate) const FADE_HOLD_S: f32 = 8.0;
pub(crate) const HELD_MAX: usize = 200;

pub struct NavtexDecoder {
    fs: f32,
    rx: FskReceiver,
    samples: Vec<FskSample>,
    atc: Atc,
    spb: f32,
    half_len: f32,
    /// Output samples until the next bit is sampled.
    ph: f32,
    /// The last bit's soft decision, weighed down by its blanking.
    last_v: f32,
    /// Bits in a row the alignment has scored poorly.
    low_words: u32,
    /// Characters held while copy is too poor to print, and since when.
    held: Vec<char>,
    held_since: Option<u64>,
    prev: (f32, f32, f32),
    /// The previous sample's unblanked share and whether an impulse was
    /// near it.
    prev_w: (f32, bool),
    have_prev: bool,
    /// The last seven bits, the newest in bit 6.
    reg: u8,
    /// Which of them were mostly blanked.
    erased: u8,
    nbits: u64,
    soft: [f32; 7],
    tones: [(f32, f32); 7],
    score: [[f32; 2]; 7],
    scored: [[u32; 2]; 7],
    /// Bit offset and polarity (1 = inverted) of the character alignment.
    sync: Option<(usize, usize)>,
    slot: u64,
    words: [Word; 8],
    dx_ev: [f32; 2],
    dx_par: Option<usize>,
    dx_alpha_run: u32,
    /// After an end of emission, nothing prints until phasing resumes.
    quiet: bool,
    shift: Shift,
    /// Characters out of the last combined word.
    chars_buf: Vec<char>,
    err_fast: f32,
    err_rate: f32,
    printing: bool,
    parser: Parser,
    lines: Lines,
    line_in_msg: bool,
    events: Vec<NavtexEvent>,
    t_in: u64,
    last_char: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Word {
    pub(crate) code: u8,
    pub(crate) valid: bool,
    /// Soft bits as log-likelihood ratios, first sent first, positive
    /// for B.
    pub(crate) soft: [f32; 7],
    /// A bit of it was mostly blanked as impulse noise: its hard code is
    /// a guess there, and it is not taken as valid however it reads.
    pub(crate) erased: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NavtexEvent {
    /// A message from ZCZC to NNNN (or cut off by signal loss after a timeout, flagged).
    Message(NavtexMessage),
    /// Text heard outside a message header, or the running text of a message in progress, line by line,
    /// so the operator sees it arrive (same chunking rules as RTTY).
    Text { text: String, in_message: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub struct NavtexMessage {
    /// B1: the transmitting station, A to Z; '?' if the header was unreadable.
    pub station: char,
    /// B2: the subject indicator, A to Z; '?' if unreadable.
    pub subject: char,
    pub subject_name: &'static str,
    /// B3B4; 0 if unreadable.
    pub serial: u8,
    /// The body, between the header line and NNNN.
    pub text: String,
    /// NNNN was received; false when the message was cut off.
    pub complete: bool,
    /// Share of the body's characters that could not be recovered.
    pub error_rate: f32,
    pub center_hz: f32,
    pub snr_db: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NavtexStatus {
    /// Locked on a tone pair that is still there.
    pub locked: bool,
    /// Character alignment found.
    pub synced: bool,
    pub center_hz: f32,
    pub snr_db: f32,
    pub in_message: bool,
    /// Share of recent characters neither copy of which could be read.
    pub error_rate: f32,
    /// The station is heard with the tones swapped.
    pub reverse: bool,
}

/// The subject indicator's meaning (ITU-R M.540, IMO NAVTEX Manual).
pub fn subject_name(c: char) -> &'static str {
    match c {
        'A' => "Navigational warnings",
        'B' => "Meteorological warnings",
        'C' => "Ice reports",
        'D' => "Search and rescue information, piracy warnings",
        'E' => "Meteorological forecasts",
        'F' => "Pilot service messages",
        'G' => "AIS messages",
        'H' => "LORAN messages",
        'I' => "Not used",
        'J' => "SATNAV messages",
        'K' => "Other electronic navaid messages",
        'L' => "Navigational warnings, additional",
        'V' | 'W' | 'X' | 'Y' => "Special services",
        'Z' => "No messages on hand",
        'M'..='U' => "Reserved",
        _ => "Unknown",
    }
}

const fn ccir_index() -> [u8; 128] {
    let mut t = [255u8; 128];
    let mut i = 0;
    while i < SYMS.len() {
        t[SYMS[i].ccir as usize] = i as u8;
        i += 1;
    }
    t
}

/// Row of [`SYMS`] for each CCIR 476 code; 255 for the service signals
/// and invalid words.
pub(crate) const CCIR_INDEX: [u8; 128] = ccir_index();

pub(crate) fn valid(code: u8) -> bool {
    code.count_ones() == 4
}

/// What a message parser is in the middle of.
enum State {
    Idle,
    Header(String),
    Body(Msg),
}

struct Msg {
    station: char,
    subject: char,
    serial: u8,
    text: String,
    chars: u32,
    errors: u32,
}

/// Finds `ZCZC B1B2B3B4` … `NNNN` in the character stream.
struct Parser {
    state: State,
    /// The last four characters, newlines included, so a marker cannot
    /// span lines.
    tail: [char; 4],
}

/// Four characters match a marker with at most one of them wrong or
/// unreadable. Neither ZCZC nor NNNN comes close to anything in plain
/// text, and requiring all four lost whole messages to one bad character.
fn marker(tail: &[char; 4], pat: &str) -> bool {
    !tail.contains(&'\n') && tail.iter().zip(pat.chars()).filter(|(a, b)| **a != *b).count() <= 1
}

impl Parser {
    fn new() -> Self {
        Self { state: State::Idle, tail: [' '; 4] }
    }

    fn in_message(&self) -> bool {
        !matches!(self.state, State::Idle)
    }

    /// One character ('\n' for a line end). A finished message (complete
    /// or not) when this character ends one.
    fn push(&mut self, c: char) -> Option<(Msg, bool)> {
        self.tail = [self.tail[1], self.tail[2], self.tail[3], c];
        let mut done = None;
        if marker(&self.tail, "ZCZC") {
            if let State::Body(mut m) = std::mem::replace(&mut self.state, State::Header(String::new())) {
                // A new header before NNNN: the last message was cut off.
                let keep = m.text.trim_end_matches(['Z', 'C', '*']).len();
                m.text.truncate(keep);
                done = Some((m, false));
            }
            self.state = State::Header(String::new());
            return done;
        }
        match &mut self.state {
            State::Idle => {}
            State::Header(h) => {
                let seen = h.chars().filter(|c| !c.is_whitespace()).count();
                if c == '\n' || (c == ' ' && seen >= 4) || h.len() > 12 {
                    let m = Self::header(h);
                    self.state = State::Body(m);
                } else {
                    h.push(c);
                }
            }
            State::Body(m) => {
                m.text.push(c);
                if !c.is_whitespace() {
                    m.chars += 1;
                    if c == '*' {
                        m.errors += 1;
                    }
                }
                if marker(&self.tail, "NNNN") {
                    let keep = m.text.len() - 4;
                    m.text.truncate(keep);
                    m.chars = m.chars.saturating_sub(4);
                    if let State::Body(m) = std::mem::replace(&mut self.state, State::Idle) {
                        done = Some((m, true));
                    }
                }
            }
        }
        done
    }

    fn header(h: &str) -> Msg {
        let b: Vec<char> = h.chars().filter(|c| !c.is_whitespace()).collect();
        let letter = |i: usize| b.get(i).copied().filter(char::is_ascii_uppercase).unwrap_or('?');
        let digit = |i: usize| b.get(i).and_then(|c| c.to_digit(10));
        let serial = match (digit(2), digit(3)) {
            (Some(a), Some(b)) => (10 * a + b) as u8,
            _ => 0,
        };
        Msg { station: letter(0), subject: letter(1), serial, text: String::new(), chars: 0, errors: 0 }
    }

    /// Give up on a message in progress.
    fn cut(&mut self) -> Option<Msg> {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Body(m) => Some(m),
            State::Header(h) => Some(Self::header(&h)),
            State::Idle => None,
        }
    }
}

impl NavtexDecoder {
    pub fn new(fs: f32) -> Self {
        let rx = FskReceiver::new(fs, BAUD, SHIFT_HZ, None);
        let spb = rx.samples_per_bit();
        let half_len = 0.5 * rx.filter_len() as f32;
        Self {
            fs,
            rx,
            samples: Vec::new(),
            atc: Atc::new(spb),
            spb,
            half_len,
            ph: spb,
            last_v: 0.0,
            low_words: 0,
            held: Vec::with_capacity(HELD_MAX),
            held_since: None,
            prev: (0.0, 0.0, 0.0),
            prev_w: (1.0, false),
            have_prev: false,
            reg: 0,
            erased: 0,
            nbits: 0,
            soft: [0.0; 7],
            tones: [(0.0, 0.0); 7],
            score: [[0.0; 2]; 7],
            scored: [[0; 2]; 7],
            sync: None,
            slot: 0,
            words: [Word::default(); 8],
            dx_ev: [0.0; 2],
            dx_par: None,
            dx_alpha_run: 0,
            quiet: false,
            shift: Shift::default(),
            chars_buf: Vec::with_capacity(16),
            err_fast: 0.5,
            err_rate: 0.0,
            printing: false,
            parser: Parser::new(),
            lines: Lines::default(),
            line_in_msg: false,
            events: Vec::new(),
            t_in: 0,
            last_char: 0,
        }
    }

    pub fn push(&mut self, audio: &[f32]) {
        let mut samples = std::mem::take(&mut self.samples);
        self.rx.push(audio, &mut samples);
        for s in &samples {
            if s.fresh {
                self.restart();
            }
            self.sample(s);
        }
        self.samples = samples;
        self.t_in += audio.len() as u64;
        self.rx.set_tracking(self.sync.is_some());
        let idle = (self.t_in - self.last_char) as f32 / self.fs;
        if idle > IDLE_FLUSH_S {
            let mut out = std::mem::take(&mut self.chars_buf);
            out.clear();
            self.shift.flush(false, false, &mut out);
            for &c in &out {
                self.take_char(c);
            }
            self.chars_buf = out;
        }
        if !self.lines.is_empty() && idle > IDLE_FLUSH_S {
            self.flush_line();
        }
        if self.parser.in_message()
            && idle > MESSAGE_TIMEOUT_S
            && let Some(m) = self.parser.cut()
        {
            self.flush_line();
            self.finish(m, false);
        }
    }

    pub fn take(&mut self) -> Vec<NavtexEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn status(&self) -> NavtexStatus {
        NavtexStatus {
            locked: self.rx.locked(),
            synced: self.sync.is_some(),
            center_hz: self.rx.center_hz(),
            snr_db: self.rx.snr_db(),
            in_message: self.parser.in_message(),
            error_rate: self.err_rate,
            reverse: self.sync.is_some_and(|(_, p)| p == 1),
        }
    }

    /// A new lock: the bit clock and the alignment start over.
    fn restart(&mut self) {
        self.atc.reset();
        self.ph = self.spb;
        self.have_prev = false;
        self.reg = 0;
        self.erased = 0;
        self.nbits = 0;
        self.score = [[0.0; 2]; 7];
        self.scored = [[0; 2]; 7];
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
        self.quiet = false;
        self.shift = Shift::default();
        self.err_fast = 0.5;
        self.printing = false;
    }

    /// One demodulator sample: the bit clock.
    fn sample(&mut self, s: &FskSample) {
        let cur = (self.atc.step_or_hold(s.hi, s.lo, s.impulse), s.hi, s.lo);
        let cur_w = (1.0 - s.blank, s.impulse);
        if !self.have_prev {
            self.prev = cur;
            self.prev_w = cur_w;
            self.have_prev = true;
            return;
        }
        let prev = std::mem::replace(&mut self.prev, cur);
        let prev_w = std::mem::replace(&mut self.prev_w, cur_w);
        self.ph -= 1.0;
        // A crossing an impulse made, or the edge of a blanked stretch, says
        // nothing about where the bits are.
        if (prev.0 > 0.0) != (cur.0 > 0.0) && !prev_w.1 && !cur_w.1 {
            // The matched filter crosses zero half a filter after an edge,
            // and holds a whole bit half a filter after that: bit samples
            // belong half a filter after each crossing.
            let back = 1.0 - prev.0 / (prev.0 - cur.0);
            let mut e = self.ph + back - self.half_len;
            e -= self.spb * (e / self.spb).round();
            // A crossing after a firm bit is a transition; one after a
            // doubtful bit, in a fade, is as likely noise, and pulling the
            // clock with it would walk it off the bits the fade is hiding.
            let w = self.last_v.abs().min(1.0);
            let g = if self.sync.is_some() { CLOCK_GAIN_SYNC } else { CLOCK_GAIN_HUNT };
            self.ph -= g * w * e;
        }
        while self.ph <= 0.0 {
            let a = 1.0 + self.ph;
            let v = prev.0 + (cur.0 - prev.0) * a;
            let h = prev.1 + (cur.1 - prev.1) * a;
            let l = prev.2 + (cur.2 - prev.2) * a;
            self.ph += self.spb;
            self.bit(v, h, l, prev_w.0.min(cur_w.0));
        }
    }

    /// One bit, `w` of it left after blanking: alignment scoring, and a
    /// word when one is complete.
    fn bit(&mut self, v: f32, hi: f32, lo: f32, w: f32) {
        let i = (self.nbits % 7) as usize;
        self.reg = (self.reg >> 1) | (u8::from(v > 0.0) << 6);
        self.erased = (self.erased >> 1) | (u8::from(w < 0.5) << 6);
        self.last_v = v * w;
        // As a log-likelihood ratio, so a copy heard in a fade counts for
        // less than one heard clearly when the two are combined. A bit
        // partly blanked keeps only that part of its energy against the
        // whole of the noise, so its ratio shrinks with it, and a bit lost
        // to an impulse is left to its other copy.
        self.soft[i] = (v * self.atc.llr_scale() * w).clamp(-LLR_MAX, LLR_MAX);
        self.tones[i] = (hi, lo);
        self.nbits += 1;
        if self.nbits < 7 {
            return;
        }
        let off = (self.nbits % 7) as usize;
        let ones = self.reg.count_ones();
        // Words with a blanked bit are scored too: the bit mostly keeps
        // its sign, and leaving them out slowed the alignment's finding
        // more than their errors hurt it.
        for (pol, ok) in [(0, ones == 4), (1, ones == 3)] {
            let n = &mut self.scored[off][pol];
            let a = (1.0 / (*n + 1) as f32).max(1.0 / SYNC_LEN);
            *n += 1;
            let sc = &mut self.score[off][pol];
            *sc += a * (f32::from(u8::from(ok)) - *sc);
        }
        self.update_sync();
        let Some((o, pol)) = self.sync else { return };
        if off != o {
            return;
        }
        let sign = if pol == 1 { -1.0 } else { 1.0 };
        let mut w = Word { code: if pol == 1 { !self.reg & 0x7f } else { self.reg }, erased: self.erased != 0, ..Word::default() };
        w.valid = valid(w.code) && !w.erased;
        // The oldest of the seven bits is the first sent, the code's LSB.
        for k in 0..7 {
            let j = (self.nbits as usize + k) % 7;
            w.soft[k] = sign * self.soft[j];
        }
        self.word(w);
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
        // Weak signals pass the 4B/3Y test well short of every time, but
        // still far more often than the other thirteen alignments do.
        let n = self.scored[best.0][best.1];
        let clear = best.2 >= SYNC_TAKE || (n >= 2 * SYNC_MIN_WORDS && best.2 >= SYNC_TAKE_WEAK && best.2 >= second + SYNC_LEAD);
        match self.sync {
            None if clear => {
                self.lose_sync();
                self.sync = Some((best.0, best.1));
            }
            Some((o, p)) => {
                let cur = self.score[o][p];
                // A fade empties every alignment's score alike; the
                // alignment is only given up when another has clearly
                // taken over, or after seconds with none.
                self.low_words = if cur < SYNC_LOSE { self.low_words + 1 } else { 0 };
                if (best.0, best.1) != (o, p) && clear && best.2 > cur + 0.25 {
                    self.lose_sync();
                    self.sync = Some((best.0, best.1));
                } else if self.low_words >= SYNC_LOSE_BITS {
                    self.lose_sync();
                }
            }
            None => {}
        }
    }

    /// One character slot.
    fn word(&mut self, w: Word) {
        let k = self.slot;
        self.slot += 1;
        self.words[(k % 8) as usize] = w;
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
                let o = self.words[((k - 5) % 8) as usize];
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
            if w.valid && w.code == PHASING2 {
                self.quiet = false;
            }
            if w.valid && w.code == PHASING1 {
                self.dx_alpha_run += 1;
                if self.dx_alpha_run >= 3 {
                    self.end_of_emission();
                }
            } else {
                self.dx_alpha_run = 0;
            }
            return;
        }
        if k < 5 {
            return;
        }
        let d = self.words[((k - 5) % 8) as usize];
        let code = combine(&d, &w);
        self.character(code);
    }

    fn end_of_emission(&mut self) {
        self.quiet = true;
        self.shift = Shift::default();
        self.flush_line();
    }

    /// A combined character, or None for one neither copy gave.
    fn character(&mut self, code: Option<u8>) {
        let bad = code.is_none();
        self.err_fast += 0.25 * (f32::from(u8::from(bad)) - self.err_fast);
        self.err_rate += 0.02 * (f32::from(u8::from(bad)) - self.err_rate);
        // A run of unreadable characters means a fade or the end of the
        // signal. What comes meanwhile is held; if good copy resumes
        // within a few seconds it was a fade and the held text is let
        // go, otherwise the signal has gone and it was noise.
        // The alignment's validity rate tells signal from noise better
        // than the unreadable rate does: maximum-likelihood decoding reads
        // something from noise too, but noise passes the 4B/3Y test only a
        // quarter of the time.
        let score = self.sync.map_or(0.0, |(o, p)| self.score[o][p]);
        if self.printing && (self.err_fast > 0.85 || score < 0.45) {
            self.printing = false;
            self.held_since = Some(self.t_in);
        } else if !self.printing && self.err_fast < 0.6 && score >= 0.6 {
            self.printing = true;
            self.held_since = None;
            for c in std::mem::take(&mut self.held) {
                self.show_char(c);
            }
        }
        if self.quiet {
            return;
        }
        let mut out = std::mem::take(&mut self.chars_buf);
        out.clear();
        match code {
            None => {
                self.shift.flush(false, false, &mut out);
                out.push('*');
            }
            Some(PHASING1 | PHASING2 | IDLE_BETA) => {}
            Some(code) => {
                let row = CCIR_INDEX[code as usize];
                if row != 255 {
                    self.shift.feed(row as usize, false, false, &mut out);
                }
            }
        }
        for &c in &out {
            self.take_char(c);
        }
        self.chars_buf = out;
    }

    /// One decoded character, into the message being assembled and the
    /// running text.
    fn take_char(&mut self, c: char) {
        let c = if c == '\r' { '\n' } else { c };
        // CR LF arrives as two line ends; one is enough.
        if c == '\n' && self.parser.tail[3] == '\n' {
            return;
        }
        // Messages are assembled from everything heard in sync, fades
        // included: a message is better with a garbled line than lost for
        // want of its NNNN. Only the running text waits out a fade.
        self.last_char = self.t_in;
        let was = self.parser.in_message();
        let done = self.parser.push(c);
        self.line_in_msg |= was || self.parser.in_message();
        if self.printing {
            self.show_char(c);
        } else if let Some(t) = self.held_since {
            if (self.t_in - t) as f32 > FADE_HOLD_S * self.fs || self.held.len() >= HELD_MAX {
                self.held.clear();
                self.held_since = None;
            } else {
                self.held.push(c);
            }
        }
        if let Some((m, complete)) = done {
            for c in std::mem::take(&mut self.held) {
                self.show_char(c);
            }
            self.flush_line();
            self.finish(m, complete);
        }
    }

    /// One character of running text.
    fn show_char(&mut self, c: char) {
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
        let in_message = self.line_in_msg || self.parser.in_message();
        self.line_in_msg = false;
        self.events.push(NavtexEvent::Text { text, in_message });
    }

    fn finish(&mut self, m: Msg, complete: bool) {
        let text = m
            .text
            .lines()
            .map(str::trim_end)
            .skip_while(|l| l.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string();
        self.events.push(NavtexEvent::Message(NavtexMessage {
            station: m.station,
            subject: m.subject,
            subject_name: subject_name(m.subject),
            serial: m.serial,
            text,
            complete,
            error_rate: m.errors as f32 / m.chars.max(1) as f32,
            center_hz: self.rx.center_hz(),
            snr_db: self.rx.snr_db(),
        }));
    }
}

/// The 35 words with four B and three Y bits.
const fn valid_codes() -> [u8; 35] {
    let mut t = [0u8; 35];
    let (mut c, mut n) = (0u8, 0);
    while c < 128 {
        if c.count_ones() == 4 {
            t[n] = c;
            n += 1;
        }
        c += 1;
    }
    t
}

pub(crate) const VALID: [u8; 35] = valid_codes();

/// One character from its DX and RX copies: the most likely of the 35
/// valid words given both copies' soft bits.
///
/// Each copy's bits are log-likelihood ratios, so their sum is the
/// evidence of both together, and the valid word that agrees best with it
/// is the maximum-likelihood choice. This takes in every case the older
/// rules handle (a valid copy, a copy whose weakest bit is the stronger,
/// two copies with a weak bit each) and the ones they cannot: two invalid
/// copies whose sum is invalid too, and a clean copy outvoted by a
/// confident one. A character none stands out for, by less than
/// [`ML_MARGIN`], is unreadable.
pub(crate) fn combine(dx: &Word, rx: &Word) -> Option<u8> {
    if dx.valid && rx.valid && dx.code == rx.code {
        return Some(dx.code);
    }
    // While phasing, DX slots carry phasing 2 and RX slots phasing 1: the
    // two are not copies of each other, and neither prints.
    let phasing = |w: &Word| w.valid && (w.code == PHASING1 || w.code == PHASING2);
    if phasing(dx) || phasing(rx) {
        return Some(PHASING1);
    }
    let mut sum = [0.0f32; 7];
    for (k, s) in sum.iter_mut().enumerate() {
        *s = dx.soft[k] + rx.soft[k];
    }
    let (mut best, mut best_m, mut second_m) = (0u8, f32::MIN, f32::MIN);
    for &c in &VALID {
        let m: f32 = sum.iter().enumerate().map(|(k, s)| if c >> k & 1 == 1 { *s } else { -*s }).sum();
        if m > best_m {
            second_m = best_m;
            best_m = m;
            best = c;
        } else if m > second_m {
            second_m = m;
        }
    }
    (best_m - second_m >= ML_MARGIN).then_some(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtty::testsig::*;
    use crate::rtty::{FIGS, LTRS};

    const FS: f32 = 48_000.0;

    /// CCIR 476 codes for `text` with shifts.
    fn encode(text: &str) -> Vec<u8> {
        let letter = |c: char| SYMS.iter().find(|s| s.letter == c);
        let figure = |c: char| SYMS.iter().find(|s| s.figure == c);
        let ltrs = letter(LTRS).unwrap().ccir;
        let figs_code = letter(FIGS).unwrap().ccir;
        let mut out = vec![ltrs];
        let mut figs = false;
        for c in text.chars() {
            if matches!(c, ' ' | '\r' | '\n') {
                out.push(letter(c).unwrap().ccir);
            } else if let Some(s) = letter(c) {
                if figs {
                    out.push(ltrs);
                    figs = false;
                }
                out.push(s.ccir);
            } else if let Some(s) = figure(c) {
                if !figs {
                    out.push(figs_code);
                    figs = true;
                }
                out.push(s.ccir);
            } else {
                panic!("no CCIR 476 code for {c:?}");
            }
        }
        out
    }

    /// The emitted character slots: phasing, then each character in a DX
    /// slot and again five slots later in an RX slot, then the end of
    /// emission.
    fn slots(text: &str, phasing_pairs: usize) -> Vec<u8> {
        let mut dx: Vec<u8> = vec![PHASING2; phasing_pairs];
        let first = dx.len();
        dx.extend(encode(text));
        let last = dx.len();
        dx.extend([PHASING1; 3]);
        let mut out = Vec::new();
        for i in 0..dx.len() {
            out.push(dx[i]);
            // RX slot 2i + 1 repeats DX slot 2(i - 2).
            let rx = if i >= 2 && (first..last).contains(&(i - 2)) { dx[i - 2] } else { PHASING1 };
            out.push(rx);
        }
        out
    }

    fn levels(slots: &[u8], invert: bool) -> Vec<bool> {
        slots.iter().flat_map(|&c| (0..7).map(move |b| (c >> b & 1 == 1) != invert)).collect()
    }

    const MESSAGE: &str = "ZCZC FA12\r\nGALE WARNING 042\r\nDOVER STRAIT. WIND SW 8 TO 9 AT 1200 UTC.\r\nPOSITION 51-30N 001-20E (APPROX)\r\nNNNN\r\n";
    const BODY: &str = "GALE WARNING 042\nDOVER STRAIT. WIND SW 8 TO 9 AT 1200 UTC.\nPOSITION 51-30N 001-20E (APPROX)";

    fn signal(center: f32, snr_db: f32, invert: bool, seed: u64) -> (Vec<f32>, f32) {
        let amp = 0.2;
        let lv = levels(&slots(MESSAGE, 40), invert);
        let mut x = vec![0.0; FS as usize];
        x.extend(fsk(&lv, BAUD, FS, center, SHIFT_HZ, amp));
        x.extend(vec![0.0; (3.0 * FS) as usize]);
        let mut rng = Rng::new(seed);
        add_noise(&mut x, amp, snr_db, FS, &mut rng);
        (x, amp)
    }

    /// The events, and the status when the first message came out.
    fn run(x: &[f32]) -> (Vec<NavtexEvent>, NavtexStatus) {
        let mut d = NavtexDecoder::new(FS);
        let mut got = Vec::new();
        let mut at_message = None;
        chunks(x, |c| {
            d.push(c);
            let ev = d.take();
            if at_message.is_none() && ev.iter().any(|e| matches!(e, NavtexEvent::Message(_))) {
                at_message = Some(d.status());
            }
            got.extend(ev);
        });
        (got, at_message.unwrap_or_else(|| d.status()))
    }

    fn messages(ev: &[NavtexEvent]) -> Vec<&NavtexMessage> {
        ev.iter()
            .filter_map(|e| match e {
                NavtexEvent::Message(m) => Some(m),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn navtex_code_table_is_four_of_seven() {
        let mut seen = std::collections::HashSet::new();
        for s in &SYMS {
            assert!(valid(s.ccir), "{:?}", s.letter);
            assert!(seen.insert(s.ccir));
        }
        for c in [PHASING1, PHASING2, IDLE_BETA] {
            assert!(valid(c) && seen.insert(c));
        }
        assert_eq!(seen.len(), 35);
    }

    #[test]
    fn navtex_message_round_trips() {
        let (x, _) = signal(1000.0, 10.0, false, 1);
        let (ev, st) = run(&x);
        let m = messages(&ev);
        assert_eq!(m.len(), 1, "{ev:#?}");
        let m = m[0];
        assert_eq!((m.station, m.subject, m.serial), ('F', 'A', 12));
        assert_eq!(m.subject_name, "Navigational warnings");
        assert_eq!(m.text, BODY);
        assert!(m.complete);
        assert_eq!(m.error_rate, 0.0);
        assert!((m.center_hz - 1000.0).abs() < 3.0, "center {}", m.center_hz);
        assert!(!st.reverse);
        // The text arrives line by line as well.
        let lines: Vec<(&str, bool)> = ev
            .iter()
            .filter_map(|e| match e {
                NavtexEvent::Text { text, in_message } => Some((text.as_str(), *in_message)),
                _ => None,
            })
            .collect();
        let mut want = vec![("ZCZC FA12", true)];
        want.extend(BODY.lines().map(|l| (l, true)));
        want.push(("NNNN", true));
        assert_eq!(lines, want);
    }

    #[test]
    fn navtex_finds_the_pair_and_the_polarity() {
        for (center, invert) in [(1700.0, false), (640.0, true)] {
            let (x, _) = signal(center, 0.0, invert, 2);
            let (ev, st) = run(&x);
            let m = messages(&ev);
            assert_eq!(m.len(), 1, "{ev:#?}");
            assert_eq!((m[0].station, m[0].subject, m[0].serial), ('F', 'A', 12));
            assert_eq!(m[0].text, BODY);
            assert!((st.center_hz - center).abs() < 3.0, "center {}", st.center_hz);
            assert_eq!(st.reverse, invert);
        }
    }

    #[test]
    fn navtex_rx_copies_cover_a_noise_burst() {
        let (mut x, amp) = signal(1000.0, 10.0, false, 3);
        // 200 ms of noise 20 dB over the signal, a few seconds into the
        // body: it destroys both copies of the characters inside it, but
        // each one's other copy lies 350 ms away, outside it.
        let mut rng = Rng::new(33);
        let start = ((1.0 + 40.0 * 0.14 + 4.0) * FS) as usize;
        let sigma = noise_sigma(amp, -20.0, FS);
        for v in &mut x[start..start + (0.2 * FS) as usize] {
            *v += sigma * rng.gauss();
        }
        let (ev, _) = run(&x);
        let m = messages(&ev);
        assert_eq!(m.len(), 1, "{ev:#?}");
        assert_eq!(m[0].text, BODY);
        assert!(m[0].complete);
    }

    /// A message through `ch` at `snr`; the body's errors against the
    /// sent body (all of it when no message came out).
    fn faded(ch: Channel, snr: f32, seed: u64) -> (usize, bool) {
        let amp = 0.2;
        let lv = levels(&slots(MESSAGE, 40), false);
        let mut rng = Rng::new(seed);
        let mut x = vec![0.0; FS as usize];
        x.extend(fsk_through(&lv, BAUD, FS, 1000.0, SHIFT_HZ, amp, ch, &mut rng));
        x.extend(vec![0.0; (3.0 * FS) as usize]);
        add_noise(&mut x, amp, snr, FS, &mut rng);
        let (ev, _) = run(&x);
        match messages(&ev).first() {
            Some(m) => (levenshtein(&m.text, BODY), true),
            None => (BODY.len(), false),
        }
    }

    /// Body errors of a message at `snr` with impulse noise at `rate`
    /// bursts a second, over `seeds` seeds: errors, characters, messages.
    fn impulse_errors(snr: f32, rate: f32, seeds: u64) -> (usize, usize, usize) {
        let (mut errs, mut found) = (0, 0);
        for seed in 1..=seeds {
            let (mut x, amp) = signal(1000.0, snr, false, 700 + seed);
            if rate > 0.0 {
                add_impulses(&mut x, amp, FS, rate, &mut Rng::new(950 + seed));
            }
            let (ev, _) = run(&x);
            match messages(&ev).first() {
                Some(m) => {
                    errs += levenshtein(&m.text, BODY);
                    found += 1;
                }
                None => errs += BODY.len(),
            }
        }
        (errs, seeds as usize * BODY.len(), found)
    }

    /// `USDR_IMPULSE_TABLE=1 cargo test --release -p scannerd-engine
    /// navtex_impulse_table -- --nocapture`.
    #[test]
    fn navtex_impulse_table() {
        if std::env::var("USDR_IMPULSE_TABLE").is_err() {
            return;
        }
        let rates = [0.0f32, 1.0, 3.0, 5.0];
        println!("NAVTEX: body CER % (messages of 6) with impulse noise, by bursts a second");
        println!("{:>10} {}", "", rates.iter().map(|r| format!("{r:>11.0}")).collect::<String>());
        for snr in [0.0f32, -5.0] {
            let row: String = rates
                .iter()
                .map(|&r| {
                    let (e, n, f) = impulse_errors(snr, r, 6);
                    format!("{:>7.1} ({f})", 100.0 * e as f32 / n as f32)
                })
                .collect();
            println!("{:>7.0} dB {row}", snr);
        }
    }

    #[test]
    fn navtex_copy_through_impulse_noise() {
        // Three crashes a second, 20 to 35 dB over the signal, at -5 dB:
        // with nothing done about them one message of six came out and
        // the body error rate was 85% (`navtex_impulse_table`). Blanked
        // and erased, each character's other copy decides.
        let (e, n, found) = impulse_errors(-5.0, 3.0, 4);
        let cer = e as f32 / n as f32;
        println!("NAVTEX at -5 dB, 3 impulses a second: {found}/4 messages, body CER {:.2}%", 100.0 * cer);
        assert_eq!(found, 4);
        assert!(cer <= 0.02, "CER {cer}");
    }

    #[test]
    fn navtex_impulses_alone_yield_nothing() {
        let mut rng = Rng::new(24);
        let sigma = noise_sigma(0.2, -5.0, FS);
        let mut x: Vec<f32> = (0..(40.0 * FS) as usize).map(|_| sigma * rng.gauss()).collect();
        add_impulses(&mut x, 0.2, FS, 5.0, &mut rng);
        let (ev, _) = run(&x);
        assert!(ev.is_empty(), "{ev:#?}");
    }

    #[test]
    fn navtex_cer_table() {
        if std::env::var("USDR_CER_TABLE").is_err() {
            return;
        }
        let seeds: u64 = std::env::var("USDR_CER_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
        let snrs = [5.0f32, 0.0, -3.0, -5.0, -7.0, -9.0];
        println!("NAVTEX: body CER % (messages found of {seeds}) by SNR (2500 Hz)");
        println!("{:>18} {}", "", snrs.iter().map(|s| format!("{s:>11.0}")).collect::<String>());
        for ch in [Channel::Awgn, Channel::MODERATE, Channel::POOR, Channel::Selective(0.5)] {
            let mut row = String::new();
            for &snr in &snrs {
                let (mut errs, mut found) = (0, 0);
                for seed in 1..=seeds {
                    let (e, f) = faded(ch, snr, 200 + seed);
                    errs += e;
                    found += usize::from(f);
                }
                row += &format!("{:>7.1} ({found})", 100.0 * errs as f32 / (seeds as usize * BODY.len()) as f32);
            }
            println!("{:>18} {row}", ch.name());
        }
    }

    #[test]
    fn navtex_character_error_rate() {
        for (snr, max_err) in [(-5.0f32, 0.0f32), (-7.0, 0.06)] {
            let mut errs = 0;
            let mut total = 0;
            let mut found = 0;
            for seed in 1..=3 {
                let (x, _) = signal(1000.0, snr, false, 10 + seed);
                let (ev, _) = run(&x);
                if let Some(m) = messages(&ev).first() {
                    found += 1;
                    errs += levenshtein(&m.text, BODY);
                } else {
                    errs += BODY.len();
                }
                total += BODY.len();
            }
            let cer = errs as f32 / total as f32;
            println!("NAVTEX at {snr:+.0} dB SNR (2500 Hz): {found}/3 messages, body CER {:.2}%", 100.0 * cer);
            assert!(cer <= max_err, "CER {cer} at {snr} dB");
        }
    }

    #[test]
    fn navtex_holds_copy_through_fading() {
        for ch in [Channel::POOR, Channel::Selective(0.5)] {
            let (mut errs, mut found) = (0, 0);
            for seed in 1..=4 {
                let (e, f) = faded(ch, 0.0, 300 + seed);
                errs += e;
                found += usize::from(f);
            }
            let cer = errs as f32 / (4 * BODY.len()) as f32;
            println!("NAVTEX {} at 0 dB: {found}/4 messages, body CER {:.1}%", ch.name(), 100.0 * cer);
            assert_eq!(found, 4, "{}", ch.name());
            assert!(cer <= 0.03, "{}: CER {cer}", ch.name());
        }
    }

    #[test]
    fn navtex_noise_alone_yields_no_message() {
        let mut texts = 0;
        for (seed, amp) in [(21u64, 1e-4f32), (22, 0.05), (23, 0.8)] {
            let mut rng = Rng::new(seed);
            let x: Vec<f32> = (0..(60.0 * FS) as usize).map(|_| amp * rng.gauss()).collect();
            let (ev, _) = run(&x);
            assert!(messages(&ev).is_empty(), "{ev:#?}");
            texts += ev.len();
        }
        assert_eq!(texts, 0);
    }

    #[test]
    fn navtex_cut_off_message_is_flagged() {
        let amp = 0.2;
        let text = "ZCZC SE05\r\nWEATHER FORECAST\r\nSEA AREA 7: WIND NW 5.\r\n";
        let lv = levels(&slots(text, 40), false);
        let mut x = vec![0.0; FS as usize];
        x.extend(fsk(&lv, BAUD, FS, 1000.0, SHIFT_HZ, amp));
        // The signal fades out without NNNN.
        x.extend(vec![0.0; (25.0 * FS) as usize]);
        let mut rng = Rng::new(5);
        add_noise(&mut x, amp, 10.0, FS, &mut rng);
        let (ev, _) = run(&x);
        let m = messages(&ev);
        assert_eq!(m.len(), 1, "{ev:#?}");
        assert_eq!((m[0].station, m[0].subject, m[0].serial), ('S', 'E', 5));
        assert_eq!(m[0].subject_name, "Meteorological forecasts");
        assert!(!m[0].complete);
        assert!(m[0].text.starts_with("WEATHER FORECAST\nSEA AREA 7: WIND NW 5."), "{:?}", m[0].text);
    }

    #[test]
    fn navtex_realtime_factor() {
        let (x, _) = signal(1000.0, -5.0, false, 9);
        let mut d = NavtexDecoder::new(FS);
        let t = std::time::Instant::now();
        chunks(&x, |c| d.push(c));
        let el = t.elapsed().as_secs_f64();
        let audio = x.len() as f64 / FS as f64;
        println!("NAVTEX: {audio:.1} s of audio in {:.1} ms, {:.3}% of one core", el * 1e3, 100.0 * el / audio);
        assert!(el / audio < 0.05);
    }
}
