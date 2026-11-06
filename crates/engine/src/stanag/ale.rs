//! MIL-STD-188-141 second-generation ALE: 8-FSK at 125 baud, tones 750 to
//! 2500 Hz in 250 Hz steps, three bits a tone in a Gray order.
//!
//! A word is 24 bits: a 3-bit preamble (TO, THIS IS, THIS WAS, DATA, REP,
//! CMD, THRU, FROM) and three 7-bit characters. Each half is coded by the
//! extended (24,12) Golay code, the second half's check bits inverted; the
//! two codewords are interleaved bit by bit, a stuff bit added, and the 49
//! bits sent three times over: 147 bits, 49 tones, 392 ms.
//!
//! The receiver measures the eight tones over a sliding symbol-long window
//! (each tone mixed to zero and summed, which is one DFT bin), finds the
//! sampling phase where one tone stands out most, and keeps the last 147
//! bits' soft values. A word boundary is wherever the three copies agree
//! almost everywhere and both Golay words decode with few errors, which
//! noise practically never does; from there words come every 49 tones.
//! The tones' phase drift across the window measures the mistuning, and
//! the mixers follow it.

use super::StanagEvent;
use super::modem::{C32, CARRIER_HZ};
use std::collections::VecDeque;
use std::f32::consts::PI;
use std::sync::OnceLock;

pub(super) const CENTER_HZ: f32 = 1625.0;
const BAUD: f32 = 125.0;
/// Tribit for each tone, lowest tone first.
const GRAY: [u8; 8] = [0, 1, 3, 2, 6, 7, 5, 4];
const WORD_BITS: usize = 147;
const WORD_SYMS: usize = 49;
const PHASES: usize = 16;
/// Copies out of agreement in a word, of 48 bits, at most, to take the word
/// boundary; random bits disagree in 36 on average.
const SYNC_DISAGREE: usize = 16;
/// And Golay corrections in each half, at most.
const SYNC_GOLAY: u8 = 2;
/// Mean tone purity (strongest tone's share of the eight) over a word's
/// tones, at least, for a word to count: noise averages 0.34, and a dead
/// input, which decodes as all zeros, has none.
const WORD_PURITY: f32 = 0.5;
/// Seconds after the last word that ALE still counts as present.
const ACTIVE_S: f32 = 3.0;

const PREAMBLES: [&str; 8] = ["DATA", "THRU", "TO", "TWAS", "FROM", "TIS", "CMD", "REP"];

/// The Golay generator x^11 + x^9 + x^7 + x^6 + x^5 + x + 1.
const GOLAY_G: u32 = 0xae3;

/// Check bits for 12 data bits: the remainder of d(x) x^11 by the
/// generator, then an overall parity bit making the codeword even.
fn golay_parity(d: u32) -> u32 {
    let mut r = d << 11;
    for i in (11..23).rev() {
        if (r >> i) & 1 == 1 {
            r ^= GOLAY_G << (i - 11);
        }
    }
    let rem = r & 0x7ff;
    let p = (d.count_ones() + rem.count_ones()) & 1;
    rem << 1 | p
}

#[cfg(test)]
fn golay_encode(d: u32) -> u32 {
    (d & 0xfff) << 12 | golay_parity(d & 0xfff)
}

/// For each syndrome, the lightest error pattern (data in the high 12
/// bits) and its weight, for patterns of up to three errors.
fn golay_table() -> &'static Vec<Option<(u32, u8)>> {
    static T: OnceLock<Vec<Option<(u32, u8)>>> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = vec![None; 4096];
        let syn = |e: u32| ((e & 0xfff) ^ golay_parity(e >> 12)) as usize;
        t[0] = Some((0, 0));
        for a in 0..24 {
            let e = 1u32 << a;
            t[syn(e)].get_or_insert((e, 1));
        }
        for a in 0..24 {
            for b in a + 1..24 {
                let e = 1u32 << a | 1 << b;
                t[syn(e)].get_or_insert((e, 2));
            }
        }
        for a in 0..24 {
            for b in a + 1..24 {
                for c in b + 1..24 {
                    let e = 1u32 << a | 1 << b | 1 << c;
                    t[syn(e)].get_or_insert((e, 3));
                }
            }
        }
        t
    })
}

/// The 12 data bits and the corrections made, if correctable.
fn golay_decode(code: u32) -> Option<(u32, u8)> {
    let s = ((code & 0xfff) ^ golay_parity(code >> 12)) as usize;
    let (e, w) = golay_table()[s]?;
    Some(((code ^ e) >> 12, w))
}

fn ascii38(c: u8) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'@' || c == b'?'
}

fn ascii64(c: u8) -> bool {
    (0x20..0x60).contains(&c)
}

#[derive(Clone, Copy, Debug)]
struct Word {
    preamble: u8,
    chars: [u8; 3],
    errors: u8,
}

impl Word {
    fn text(&self) -> String {
        self.chars.iter().map(|&c| c as char).collect()
    }

    fn plausible(&self) -> bool {
        match self.preamble {
            6 => self.chars.iter().all(|&c| c < 0x80),
            0 | 7 => self.chars.iter().all(|&c| ascii64(c)),
            _ => self.chars.iter().all(|&c| ascii38(c)),
        }
    }
}

/// A call being put together from its words.
#[derive(Default)]
struct Call {
    called: Option<String>,
    caller: Option<String>,
    conclusion: Option<&'static str>,
    /// Which address the next DATA or REP word extends.
    extending: Option<bool>,
    /// The first word of each address, to tell a repeat from a new one.
    called_first: Option<String>,
    caller_first: Option<String>,
}

pub(super) struct Ale {
    fs: f32,
    ns: usize,
    half: usize,
    offset: f32,
    lo: [C32; 8],
    step: [C32; 8],
    ring: Vec<[C32; 8]>,
    at: usize,
    a: [C32; 8],
    b: [C32; 8],
    t: u64,
    /// Symbol phase: fraction of a symbol at the current sample.
    frac: f64,
    period: f64,
    quality: [f32; PHASES],
    phase: usize,
    bits: VecDeque<f32>,
    /// Each tone's purity and energy over the last word.
    pure: VecDeque<(f32, f32)>,
    /// Mean tone energy of the words taken, for telling a word from a
    /// dead input that happens to decode.
    level: f32,
    syms: u64,
    last_word_sym: u64,
    sync_at: Option<u64>,
    misses: u32,
    last_word: Option<(u8, [u8; 3])>,
    call: Call,
    purity: f32,
    snr: f32,
    freq_acc: (f32, f32),
    last_word_t: Option<u64>,
    events: Vec<StanagEvent>,
}

impl Ale {
    pub(super) fn new(fs: f32) -> Self {
        let ns = (fs / BAUD).round() as usize;
        let mut s = Self {
            fs,
            ns,
            half: ns / 2,
            offset: 0.0,
            lo: [C32::new(1.0, 0.0); 8],
            step: [C32::new(1.0, 0.0); 8],
            ring: vec![[C32::default(); 8]; ns],
            at: 0,
            a: [C32::default(); 8],
            b: [C32::default(); 8],
            t: 0,
            frac: 0.0,
            period: f64::from(fs / BAUD),
            quality: [0.0; PHASES],
            phase: 0,
            bits: VecDeque::with_capacity(WORD_BITS + 3),
            pure: VecDeque::with_capacity(WORD_SYMS + 1),
            level: 0.0,
            syms: 0,
            last_word_sym: 0,
            sync_at: None,
            misses: 0,
            last_word: None,
            call: Call::default(),
            purity: 0.0,
            snr: 0.0,
            freq_acc: (0.0, 0.0),
            last_word_t: None,
            events: Vec::new(),
        };
        s.tune();
        s
    }

    fn tune(&mut self) {
        for k in 0..8 {
            let f = 750.0 + 250.0 * k as f32 - CARRIER_HZ + self.offset;
            self.step[k] = C32::from_polar(1.0, -2.0 * PI * f / self.fs);
        }
    }

    pub(super) fn active(&self) -> bool {
        self.last_word_t.is_some_and(|t| ((self.t - t) as f32) < ACTIVE_S * self.fs)
    }

    pub(super) fn offset_hz(&self) -> f32 {
        self.offset
    }

    pub(super) fn snr_db(&self) -> Option<f32> {
        self.active().then(|| 10.0 * self.snr.max(1e-3).log10())
    }

    /// How cleanly one tone at a time stands out: 0 for noise, 1 for a
    /// clean 8-FSK signal.
    pub(super) fn score(&self) -> f32 {
        ((self.purity - 0.4) / 0.5).clamp(0.0, 1.0)
    }

    pub(super) fn take(&mut self) -> Vec<StanagEvent> {
        std::mem::take(&mut self.events)
    }

    pub(super) fn push(&mut self, x: &[C32]) {
        for &v in x {
            self.sample(v);
        }
    }

    fn sample(&mut self, v: C32) {
        let mut m = [C32::default(); 8];
        for ((m, lo), step) in m.iter_mut().zip(&mut self.lo).zip(&self.step) {
            *m = v * *lo;
            *lo *= *step;
        }
        let ns = self.ns;
        let mid = self.ring[(self.at + ns - self.half) % ns];
        let old = self.ring[self.at];
        for k in 0..8 {
            self.b[k] += m[k] - mid[k];
            self.a[k] += mid[k] - old[k];
        }
        self.ring[self.at] = m;
        self.at = (self.at + 1) % ns;
        self.t += 1;
        if self.t.is_multiple_of(4096) {
            self.renormalize();
        }
        let mut p = [0f32; 8];
        for (p, (a, b)) in p.iter_mut().zip(self.a.iter().zip(&self.b)) {
            *p = (a + b).norm_sqr();
        }
        let sum: f32 = p.iter().sum();
        let max = p.iter().fold(0f32, |m, &v| m.max(v));
        let q = if sum > 0.0 { max / sum } else { 0.0 };
        let before = self.frac;
        self.frac += 1.0 / self.period;
        if self.frac >= 1.0 {
            self.frac -= 1.0;
        }
        let bin = ((self.frac * PHASES as f64) as usize).min(PHASES - 1);
        self.quality[bin] += (q - self.quality[bin]) / 200.0;
        // Sample in the middle of the best phase bin.
        let target = (self.phase as f64 + 0.5) / PHASES as f64;
        let crossed = if before <= self.frac { before < target && self.frac >= target } else { before < target || self.frac >= target };
        if crossed {
            self.symbol(&p, sum, q);
            let (best, &bq) = self.quality.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
            if bq > 1.1 * self.quality[self.phase] {
                self.phase = best;
            }
        }
    }

    /// Rebuild the window sums from the ring, so rounding cannot build up.
    fn renormalize(&mut self) {
        let ns = self.ns;
        self.a = [C32::default(); 8];
        self.b = [C32::default(); 8];
        for i in 0..ns {
            let r = self.ring[(self.at + i) % ns];
            let sums = if i < ns - self.half { &mut self.a } else { &mut self.b };
            for (s, v) in sums.iter_mut().zip(r) {
                *s += v;
            }
        }
        for l in &mut self.lo {
            *l /= l.norm();
        }
    }

    fn symbol(&mut self, p: &[f32; 8], sum: f32, q: f32) {
        self.purity += (q - self.purity) / 64.0;
        let (best, &pb) = p.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
        let noise = (sum - pb) / 7.0;
        // Mistuning: the strongest tone's turn between the window halves.
        let turn = self.b[best] * self.a[best].conj();
        let hz = turn.arg() / (2.0 * PI * self.half as f32 / self.fs);
        self.freq_acc.0 += hz * pb;
        self.freq_acc.1 += pb;
        if noise > 0.0 {
            // Tone over noise in one bin, 125 Hz, to signal over noise in 3 kHz.
            let s = ((pb - noise) / noise).max(0.0) * BAUD / 3000.0;
            self.snr += (s - self.snr) / 64.0;
        }
        let norm = sum.max(1e-30);
        for bit in (0..3).rev() {
            let mut one = 0f32;
            let mut zero = 0f32;
            for (k, &g) in GRAY.iter().enumerate() {
                if (g >> bit) & 1 == 1 {
                    one = one.max(p[k]);
                } else {
                    zero = zero.max(p[k]);
                }
            }
            self.bits.push_back((one - zero) / norm);
        }
        while self.bits.len() > WORD_BITS {
            self.bits.pop_front();
        }
        self.pure.push_back((q, sum));
        while self.pure.len() > WORD_SYMS {
            self.pure.pop_front();
        }
        self.syms += 1;
        // A concluded call with nothing after it for two words is over.
        if self.call.conclusion.is_some() && self.syms > self.last_word_sym + 2 * WORD_SYMS as u64 {
            self.end_call();
        }
        if self.bits.len() < WORD_BITS {
            return;
        }
        match self.sync_at {
            Some(s) if (self.syms - s).is_multiple_of(WORD_SYMS as u64) => match self.word(4, 48) {
                Some(w) => {
                    self.misses = 0;
                    self.take_word(w);
                }
                None => {
                    self.misses += 1;
                    if self.misses >= 2 {
                        self.lost();
                    }
                }
            },
            Some(_) => {}
            None => {
                if let Some(w) = self.word(SYNC_GOLAY, SYNC_DISAGREE)
                    && w.plausible()
                {
                    self.sync_at = Some(self.syms);
                    self.misses = 0;
                    self.take_word(w);
                }
            }
        }
    }

    /// Read the last 147 bits as a word.
    fn word(&self, max_golay: u8, max_disagree: usize) -> Option<Word> {
        if self.pure.iter().map(|p| p.0).sum::<f32>() < WORD_PURITY * WORD_SYMS as f32 {
            return None;
        }
        // Twenty dB below the words before it is not a word but silence.
        let energy = self.pure.iter().map(|p| p.1).sum::<f32>() / WORD_SYMS as f32;
        if self.sync_at.is_some() && energy < 0.01 * self.level {
            return None;
        }
        let mut a = 0u32;
        let mut b = 0u32;
        let mut disagree = 0;
        for i in 0..48 {
            let c = [self.bits[i], self.bits[i + WORD_SYMS], self.bits[i + 2 * WORD_SYMS]];
            let s: f32 = c.iter().sum();
            let bit = u32::from(s > 0.0);
            if c.iter().any(|&v| (v > 0.0) != (s > 0.0)) {
                disagree += 1;
            }
            if i % 2 == 0 {
                a = a << 1 | bit;
            } else {
                b = b << 1 | bit;
            }
        }
        if disagree > max_disagree {
            return None;
        }
        let (da, ea) = golay_decode(a)?;
        let (db, eb) = golay_decode(b ^ 0xfff)?;
        if ea > max_golay || eb > max_golay {
            return None;
        }
        let w = da << 12 | db;
        Some(Word {
            preamble: (w >> 21) as u8,
            chars: [((w >> 14) & 0x7f) as u8, ((w >> 7) & 0x7f) as u8, (w & 0x7f) as u8],
            errors: ea + eb,
        })
    }

    fn take_word(&mut self, w: Word) {
        self.last_word_t = Some(self.t);
        self.last_word_sym = self.syms;
        let energy = self.pure.iter().map(|p| p.1).sum::<f32>() / WORD_SYMS as f32;
        self.level = if self.level > 0.0 { 0.7 * self.level + 0.3 * energy } else { energy };
        // Follow the mistuning once a word.
        if self.freq_acc.1 > 0.0 {
            let hz = self.freq_acc.0 / self.freq_acc.1;
            if hz.abs() > 3.0 {
                self.offset = (self.offset + 0.7 * hz).clamp(-100.0, 100.0);
                self.tune();
            }
        }
        self.freq_acc = (0.0, 0.0);
        let key = (w.preamble, w.chars);
        if self.last_word != Some(key) {
            self.last_word = Some(key);
            self.events.push(StanagEvent::AleWord {
                preamble: PREAMBLES[w.preamble as usize],
                text: w.text(),
                golay_errors: w.errors,
            });
        }
        let text = w.text();
        match w.preamble {
            2 => {
                // TO: a call's first words, repeated while the called station scans.
                if self.call.conclusion.is_some() {
                    self.end_call();
                }
                if self.call.called_first.as_deref() != Some(text.as_str()) {
                    self.call.called_first = Some(text.clone());
                    self.call.called = Some(text);
                }
                self.call.extending = Some(true);
            }
            5 | 3 => {
                let c = if w.preamble == 5 { "TIS" } else { "TWAS" };
                if self.call.caller_first.as_deref() != Some(text.as_str()) {
                    if self.call.conclusion.is_some() {
                        self.end_call();
                    }
                    self.call.caller_first = Some(text.clone());
                    self.call.caller = Some(text);
                }
                self.call.conclusion = Some(c);
                self.call.extending = Some(false);
            }
            0 | 7 => {
                let target = match self.call.extending {
                    Some(true) => &mut self.call.called,
                    Some(false) => &mut self.call.caller,
                    None => return,
                };
                if let Some(s) = target
                    && s.len() < 15
                    && !s.ends_with(&text)
                {
                    s.push_str(&text);
                }
            }
            _ => {
                if self.call.conclusion.is_some() {
                    self.end_call();
                }
                self.call.extending = None;
            }
        }
    }

    fn end_call(&mut self) {
        let c = std::mem::take(&mut self.call);
        if let Some(conclusion) = c.conclusion {
            let clean = |s: Option<String>| s.map(|s| s.trim_end_matches('@').to_string());
            self.events.push(StanagEvent::AleCall { called: clean(c.called), caller: clean(c.caller), conclusion });
        }
    }

    fn lost(&mut self) {
        self.sync_at = None;
        self.level = 0.0;
        self.misses = 0;
        self.last_word = None;
        self.end_call();
    }
}

/// ALE words as audio, for the tests: each word's 49 tones, phase
/// continuous at 125 baud.
#[cfg(test)]
pub(super) fn transmit(words: &[(u8, [u8; 3])], fs: f64, offset_hz: f64) -> Vec<f32> {
    let mut tones = Vec::new();
    for &(pre, ch) in words {
        let w = u32::from(pre) << 21 | u32::from(ch[0]) << 14 | u32::from(ch[1]) << 7 | u32::from(ch[2]);
        let a = golay_encode(w >> 12);
        let b = golay_encode(w & 0xfff) ^ 0xfff;
        let mut bits = Vec::with_capacity(49);
        for i in (0..24).rev() {
            bits.push(((a >> i) & 1) as u8);
            bits.push(((b >> i) & 1) as u8);
        }
        bits.push(0);
        let mut all = Vec::new();
        for _ in 0..3 {
            all.extend_from_slice(&bits);
        }
        for t in all.chunks(3) {
            let tri = t[0] << 2 | t[1] << 1 | t[2];
            tones.push(GRAY.iter().position(|&g| g == tri).unwrap());
        }
    }
    let per = fs / f64::from(BAUD);
    let n = (tones.len() as f64 * per) as usize;
    let mut ph = 0.0f64;
    (0..n)
        .map(|i| {
            let k = tones[((i as f64 / per) as usize).min(tones.len() - 1)];
            ph += 2.0 * std::f64::consts::PI * (750.0 + 250.0 * k as f64 + offset_hz) / fs;
            (ph.sin() * 0.5) as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golay_matches_the_published_check_bits() {
        // Check bits of the unit data words, as tabulated for 188-141.
        let rows = [0x5c7, 0xb8d, 0x2de, 0x5bc, 0xb78, 0x337, 0x66d, 0xcd9, 0xc76, 0xd2b, 0xf92, 0xae3];
        for (k, &r) in rows.iter().enumerate() {
            assert_eq!(golay_parity(1 << k), r, "bit {k}");
        }
    }

    #[test]
    fn golay_corrects_three() {
        for d in [0u32, 0x123, 0xfff, 0xa5a] {
            let c = golay_encode(d);
            assert_eq!(golay_decode(c), Some((d, 0)));
            let e = c ^ (1 << 3) ^ (1 << 15) ^ (1 << 22);
            assert_eq!(golay_decode(e), Some((d, 3)));
        }
    }
}
