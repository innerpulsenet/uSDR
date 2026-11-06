//! NATO and US military HF serial-tone modems, identified and decoded from
//! upper-sideband audio: STANAG 4285, MIL-STD-188-110A serial tone, the
//! STANAG 5066 data link frames they carry, and MIL-STD-188-141 2G ALE.
//!
//! **Signals.** STANAG 4285 and 188-110A are both 2400 Bd 8-PSK on an
//! 1800 Hz carrier, scrambled so that every user rate from 75 to 4800 bps
//! looks the same on the air. What separates them is their known symbols.
//! 4285 repeats a 256-symbol frame (106.7 ms) that opens with an 80-symbol
//! BPSK sync, and never says its rate: the receiver works it out (the
//! `s4285` module). 188-110A sends a preamble of 200 ms segments that
//! spells out the rate and interleaver, then data frames with a probe of
//! known symbols every 48 or 40 (`m110a`). 2G ALE is a different animal:
//! 8-FSK at 125 baud, words of three characters under a Golay code, sent
//! three times over (`ale`).
//!
//! **Method.** The audio is mixed down from 1800 Hz, filtered and
//! decimated, then resampled to two samples a symbol. Each receiver hunts
//! for its own known symbols there; once one locks, a fractionally spaced
//! decision-feedback equalizer adapted by recursive least squares takes
//! out the multipath (`modem`), and its soft decisions go through the
//! deinterleaver and Viterbi decoder of the mode (`fec`). The decoded bits
//! are searched for STANAG 5066 D_PDUs (`s5066`), reported only when their
//! header CRC checks. Most traffic is encrypted; a payload that looks like
//! ciphertext is reported by size only.
//!
//! Beside the receivers, the signal's correlation with itself one frame
//! period back, against the same a little off the period, measures how
//! much of it repeats the way each waveform's known symbols do. That names
//! a 188-110A transmission joined after its preamble, which is identified
//! but not decoded (its mode is only in the preamble).
//!
//! **Not here.** STANAG 4529 (the narrowband 1200 Bd relative of 4285) is
//! not a parameter change of 4285: its rates and codes differ, and its sync
//! and scrambler could not be checked against the standard, so it is
//! neither named nor decoded. Nor are the 188-110B appendix C / STANAG 4539
//! high-rate modes, 39-tone parallel modems, or 188-110A secure voice.
//! Inside a D_PDU only the header is interpreted; C_PDUs are not
//! reassembled.

mod ale;
mod fec;
mod m110a;
mod modem;
mod s4285;
mod s5066;
#[cfg(test)]
mod sim;
#[cfg(test)]
mod tests;

pub use s5066::{Dpdu, Payload};

use modem::{C32, CARRIER_HZ, FrontEnd, Resampler, Stream};
use num_complex::Complex32;
use std::collections::VecDeque;

/// The receivers' sample rate: two samples a 2400 Bd symbol.
const HALF_RATE: f32 = 2.0 * s4285::BAUD;
const NAME_4285: &str = "STANAG 4285";
const NAME_110A: &str = "MIL-STD-188-110A";
const NAME_ALE: &str = "MIL-STD-188-141 2G ALE";
/// Periodicity contrast that names 188-110A data without its preamble.
const LATE_110A: f32 = 0.12;
/// A waveform not seen for this long is forgotten, so seeing it again is
/// news.
const FORGET_S: f32 = 5.0;

#[derive(Clone, Debug, PartialEq)]
pub enum StanagEvent {
    /// A waveform has been identified (reported when it changes).
    Waveform {
        waveform: &'static str,
        modulation: Option<&'static str>,
        center_hz: f32,
        snr_db: Option<f32>,
    },
    /// The data rate and interleaver are known.
    Mode {
        waveform: &'static str,
        data_rate_bps: u32,
        modulation: &'static str,
        coded: bool,
        interleave: &'static str,
        /// "preamble" when the signal said so, "decoded" when found by
        /// trying every mode.
        source: &'static str,
    },
    /// A STANAG 5066 D_PDU whose header CRC checks.
    Dpdu { waveform: &'static str, data_rate_bps: Option<u32>, dpdu: Dpdu },
    /// The 188-110A end-of-message pattern.
    EndOfMessage { waveform: &'static str },
    /// One 2G ALE word.
    AleWord { preamble: &'static str, text: String, golay_errors: u8 },
    /// A 2G ALE call or sounding, from its first word to its conclusion.
    AleCall { called: Option<String>, caller: Option<String>, conclusion: &'static str },
}

impl StanagEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Waveform { .. } => "waveform",
            Self::Mode { .. } => "mode",
            Self::Dpdu { .. } => "dpdu",
            Self::EndOfMessage { .. } => "eom",
            Self::AleWord { .. } => "ale_word",
            Self::AleCall { .. } => "ale_call",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Self::Waveform { waveform, modulation, center_hz, snr_db } => {
                let mut s = waveform.to_string();
                if let Some(m) = modulation {
                    s += &format!(", {m}");
                }
                s += &format!(", {center_hz:.0} Hz");
                if let Some(snr) = snr_db {
                    s += &format!(", {snr:+.0} dB");
                }
                s
            }
            Self::Mode { waveform, data_rate_bps, modulation, coded, interleave, source } => {
                let c = if *coded { "coded" } else { "uncoded" };
                format!("{waveform} {data_rate_bps} bps {modulation} {c}, {interleave} interleave ({source})")
            }
            Self::Dpdu { dpdu: d, .. } => {
                let mut s = format!(
                    "5066 {} {} -> {}",
                    d.type_name(),
                    address(d.source),
                    address(d.destination)
                );
                if let Some(q) = d.tx_seq() {
                    s += &format!(", seq {q}");
                }
                if let Some(id) = d.c_pdu_id() {
                    s += &format!(", C_PDU {id}");
                }
                s += &match &d.payload {
                    Payload::None => String::new(),
                    Payload::Ok(p) if s5066::looks_encrypted(p) => {
                        format!(", {} bytes, encrypted or compressed (not decodable)", p.len())
                    }
                    Payload::Ok(p) => format!(", {} bytes: {}", p.len(), printable(p)),
                    Payload::BadCrc(n) => format!(", {n} bytes, payload CRC failed"),
                    Payload::Cut(n) => format!(", {n} bytes, payload cut off"),
                };
                s
            }
            Self::EndOfMessage { waveform } => format!("{waveform} end of message"),
            Self::AleWord { preamble, text, .. } => format!("ALE {preamble} {text}"),
            Self::AleCall { called, caller, conclusion } => {
                let mut s = String::from("ALE ");
                match (called, caller) {
                    (Some(to), Some(from)) => s += &format!("call {to} from {from}"),
                    (None, Some(from)) => s += &format!("sounding from {from}"),
                    (Some(to), None) => s += &format!("call {to}"),
                    (None, None) => s += "call",
                }
                s += &format!(" ({conclusion})");
                s
            }
        }
    }

    pub fn fields(&self) -> Vec<(&'static str, String)> {
        let mut v = vec![("kind", self.kind().to_string())];
        match self {
            Self::Waveform { waveform, modulation, center_hz, snr_db } => {
                v.push(("waveform", waveform.to_string()));
                if let Some(m) = modulation {
                    v.push(("modulation", m.to_string()));
                }
                v.push(("centerHz", format!("{center_hz:.1}")));
                if let Some(s) = snr_db {
                    v.push(("snrDb", format!("{s:.1}")));
                }
            }
            Self::Mode { waveform, data_rate_bps, modulation, coded, interleave, source } => {
                v.push(("waveform", waveform.to_string()));
                v.push(("dataRateBps", data_rate_bps.to_string()));
                v.push(("modulation", modulation.to_string()));
                v.push(("coded", coded.to_string()));
                v.push(("interleave", interleave.to_string()));
                v.push(("modeSource", source.to_string()));
            }
            Self::Dpdu { waveform, data_rate_bps, dpdu: d } => {
                v.push(("waveform", waveform.to_string()));
                if let Some(r) = data_rate_bps {
                    v.push(("dataRateBps", r.to_string()));
                }
                v.push(("dpduType", d.dtype.to_string()));
                v.push(("dpduTypeName", d.type_name().to_string()));
                v.push(("source", address(d.source)));
                v.push(("destination", address(d.destination)));
                v.push(("addressBytes", d.address_bytes.to_string()));
                v.push(("headerBytes", d.header_bytes.to_string()));
                v.push(("eow", format!("0x{:03x}", d.eow)));
                v.push(("eotHalfSeconds", d.eot.to_string()));
                if let Some(q) = d.tx_seq() {
                    v.push(("txSeq", q.to_string()));
                }
                if let Some(id) = d.c_pdu_id() {
                    v.push(("cPduId", id.to_string()));
                }
                if let Some(n) = d.segment_bytes {
                    v.push(("segmentBytes", n.to_string()));
                }
                v.push(("bitOrder", if d.msb_first { "msb-first" } else { "lsb-first" }.to_string()));
                match &d.payload {
                    Payload::None => {}
                    Payload::Ok(p) if s5066::looks_encrypted(p) => {
                        v.push(("payload", "encrypted or compressed, not decodable".to_string()));
                        v.push(("payloadBytes", p.len().to_string()));
                    }
                    Payload::Ok(p) => {
                        v.push(("payload", "ok".to_string()));
                        v.push(("payloadBytes", p.len().to_string()));
                        v.push(("payloadHex", p.iter().map(|b| format!("{b:02x}")).collect()));
                        v.push(("payloadText", printable(p)));
                    }
                    Payload::BadCrc(n) => {
                        v.push(("payload", "crc failed".to_string()));
                        v.push(("payloadBytes", n.to_string()));
                    }
                    Payload::Cut(n) => {
                        v.push(("payload", "cut off".to_string()));
                        v.push(("payloadBytes", n.to_string()));
                    }
                }
            }
            Self::EndOfMessage { waveform } => v.push(("waveform", waveform.to_string())),
            Self::AleWord { preamble, text, golay_errors } => {
                v.push(("preamble", preamble.to_string()));
                v.push(("text", text.clone()));
                v.push(("golayErrors", golay_errors.to_string()));
            }
            Self::AleCall { called, caller, conclusion } => {
                if let Some(c) = called {
                    v.push(("called", c.clone()));
                }
                if let Some(c) = caller {
                    v.push(("caller", c.clone()));
                }
                v.push(("conclusion", conclusion.to_string()));
            }
        }
        v
    }
}

/// A 5066 address as dotted bytes of its value, as operators write them.
fn address(a: u32) -> String {
    format!("{}.{}.{}.{}", (a >> 24) & 0xff, (a >> 16) & 0xff, (a >> 8) & 0xff, a & 0xff)
}

fn printable(p: &[u8]) -> String {
    p.iter().map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '.' }).collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct StanagStatus {
    /// A receiver is locked to a signal (or ALE words are arriving).
    pub locked: bool,
    /// The signal's center in the audio.
    pub center_hz: Option<f32>,
    /// Signal to noise in 3 kHz, from the equalizer's error on known
    /// symbols.
    pub snr_db: Option<f32>,
    pub waveform: Option<&'static str>,
    pub modulation: Option<&'static str>,
    pub data_rate_bps: Option<u32>,
    pub interleave: Option<&'static str>,
    pub coded: Option<bool>,
    /// Carrier offset from the nominal 1800 Hz.
    pub freq_error_hz: Option<f32>,
    /// How much the signal looks like each waveform, 0 to 1.
    pub scores: Vec<(&'static str, f32)>,
}

/// The signal's correlation with itself one frame period back, at the
/// periods that separate the waveforms. Known symbols repeat, data does
/// not, so the magnitude is about the share of known symbols: half for
/// STANAG 4285 at 256 symbols, half for 188-110A at 160 symbols in its
/// 20 + 20 frames, a third at 480 in its 32 + 16. A steady tone correlates
/// with itself at every lag, so what counts is the contrast: the
/// correlation at the period less the larger of those 29 samples either
/// side, where no waveform here repeats.
struct Cyclic {
    hist: Vec<C32>,
    at: usize,
    filled: bool,
    /// For each period: the correlation at it, 29 before and 29 after.
    r: [[C32; 3]; 3],
    p: f32,
}

/// Periods in samples at two a 2400 Bd symbol: 4285's frame, and 110A's
/// probe pattern in its slower and faster frames.
const LAGS: [usize; 3] = [512, 320, 960];
const SIDE: usize = 29;
const HIST: usize = 1024;
const CYCLIC_ALPHA: f32 = 1.0 / 9600.0;

impl Cyclic {
    fn new() -> Self {
        Self { hist: vec![C32::default(); HIST], at: 0, filled: false, r: [[C32::default(); 3]; 3], p: 0.0 }
    }

    fn push(&mut self, x: &[C32]) {
        for &v in x {
            if self.filled {
                for (k, &lag) in LAGS.iter().enumerate() {
                    for (j, l) in [lag, lag - SIDE, lag + SIDE].into_iter().enumerate() {
                        let old = self.hist[(self.at + HIST - l) % HIST];
                        self.r[k][j] += (v * old.conj() - self.r[k][j]) * CYCLIC_ALPHA;
                    }
                }
                self.p += (v.norm_sqr() - self.p) * CYCLIC_ALPHA;
            }
            self.hist[self.at] = v;
            self.at = (self.at + 1) % HIST;
            self.filled |= self.at == 0;
        }
    }

    /// Correlation at period `k` over its sides, 0 to about 0.5.
    fn contrast(&self, k: usize) -> f32 {
        if self.p <= 0.0 {
            return 0.0;
        }
        let [at, a, b] = self.r[k].map(|c| c.norm() / self.p);
        (at - a.max(b)).max(0.0)
    }
}

/// Decodes the serial-tone modems and 2G ALE from sideband audio. Feed it
/// audio with [`push`](Self::push), collect [`StanagEvent`]s with
/// [`take`](Self::take); [`status`](Self::status) and
/// [`constellation`](Self::constellation) are for a live display.
pub struct StanagDecoder {
    fs: f32,
    front: FrontEnd,
    rs: Resampler,
    base: Vec<C32>,
    half: Vec<C32>,
    stream: Stream,
    rx4285: s4285::Rx4285,
    rx110: m110a::Rx110,
    ale: ale::Ale,
    cyclic: Cyclic,
    events: Vec<StanagEvent>,
    /// The waveform last reported, and when it was last seen.
    reported: Option<&'static str>,
    seen_at: u64,
    recent: VecDeque<(u64, Dpdu)>,
    t: u64,
}

impl StanagDecoder {
    /// For audio at `fs`, any rate from 9 kHz up; it need not be a round
    /// number.
    pub fn new(fs: f32) -> Self {
        let front = FrontEnd::new(fs);
        let fs1 = front.rate();
        Self {
            fs,
            rs: Resampler::new(fs1, HALF_RATE),
            ale: ale::Ale::new(fs1),
            front,
            base: Vec::new(),
            half: Vec::new(),
            stream: Stream::new(),
            rx4285: s4285::Rx4285::new(),
            rx110: m110a::Rx110::new(),
            cyclic: Cyclic::new(),
            events: Vec::new(),
            reported: None,
            seen_at: 0,
            recent: VecDeque::new(),
            t: 0,
        }
    }

    /// Upper-sideband audio, any level, in blocks of any size.
    pub fn push(&mut self, audio: &[f32]) {
        for chunk in audio.chunks(4096) {
            self.base.clear();
            self.front.process(chunk, &mut self.base);
            self.ale.push(&self.base);
            self.half.clear();
            self.rs.process(&self.base, &mut self.half);
            self.cyclic.push(&self.half);
            self.stream.extend(&self.half);
            self.rx4285.run(&self.stream);
            self.rx110.run(&self.stream);
            self.t += chunk.len() as u64;
            self.collect();
            let keep = self.rx4285.needs_from().min(self.rx110.needs_from()).min(self.stream.end().saturating_sub(4096));
            self.stream.trim(keep);
            self.identify();
        }
    }

    /// Events since the last call, oldest first.
    pub fn take(&mut self) -> Vec<StanagEvent> {
        std::mem::take(&mut self.events)
    }

    /// The last few hundred equalized symbols, descrambled, for a
    /// constellation display.
    pub fn constellation(&self) -> Vec<Complex32> {
        if self.rx110.active() { self.rx110.shown() } else { self.rx4285.shown() }
    }

    /// What is being received now.
    pub fn status(&self) -> StanagStatus {
        let scores = self.scores();
        let waveform = self.current();
        let mut st = StanagStatus {
            locked: false,
            center_hz: None,
            snr_db: None,
            waveform,
            modulation: None,
            data_rate_bps: None,
            interleave: None,
            coded: None,
            freq_error_hz: None,
            scores,
        };
        if self.rx110.active() {
            st.locked = true;
            st.freq_error_hz = self.rx110.freq_error_hz();
            st.snr_db = self.rx110.snr_db();
            if let Some(m) = self.rx110.mode() {
                st.modulation = Some(m.modulation());
                st.data_rate_bps = Some(m.bps);
                st.interleave = Some(m.interleave_label());
                st.coded = Some(m.coded());
            }
        } else if self.rx4285.locked() {
            st.locked = true;
            st.freq_error_hz = self.rx4285.freq_error_hz();
            st.snr_db = self.rx4285.snr_db();
            st.modulation = self.rx4285.order().map(s4285::modulation);
            if let Some(m) = self.rx4285.mode() {
                st.data_rate_bps = Some(m.bps);
                st.interleave = Some(m.interleave.label());
                st.coded = Some(m.coded);
            }
        } else if self.ale.active() {
            st.locked = true;
            st.modulation = Some("8-FSK");
            st.freq_error_hz = Some(self.ale.offset_hz());
            st.center_hz = Some(ale::CENTER_HZ + self.ale.offset_hz());
            st.snr_db = self.ale.snr_db();
        }
        if st.center_hz.is_none() && st.locked {
            st.center_hz = Some(CARRIER_HZ + st.freq_error_hz.unwrap_or(0.0));
        }
        st
    }

    fn scores(&self) -> Vec<(&'static str, f32)> {
        let c = |k: usize| (self.cyclic.contrast(k) / 0.3).min(1.0);
        let s4285 = self.rx4285.score().max(c(0)).min(1.0);
        let s110 = self.rx110.score().max(c(1)).max(c(2)).min(1.0);
        vec![(NAME_4285, s4285), (NAME_110A, s110), (NAME_ALE, self.ale.score())]
    }

    /// What the signal is now, if anything.
    fn current(&self) -> Option<&'static str> {
        if self.rx110.active() {
            return Some(NAME_110A);
        }
        if self.rx4285.locked() {
            return Some(NAME_4285);
        }
        if self.ale.active() {
            return Some(NAME_ALE);
        }
        // 188-110A data joined late: its probes repeat, 4285's sync does not.
        let c = |k: usize| self.cyclic.contrast(k);
        if (c(1) > LATE_110A || c(2) > LATE_110A) && c(0) < 0.05 {
            return Some(NAME_110A);
        }
        None
    }

    fn identify(&mut self) {
        let now = self.current();
        match now {
            Some(w) => {
                self.seen_at = self.t;
                if self.reported != Some(w) {
                    self.reported = Some(w);
                    let st = self.status();
                    self.events.push(StanagEvent::Waveform {
                        waveform: w,
                        modulation: st.modulation,
                        center_hz: st.center_hz.unwrap_or(CARRIER_HZ),
                        snr_db: st.snr_db,
                    });
                }
            }
            None => {
                if (self.t - self.seen_at) as f32 > FORGET_S * self.fs {
                    self.reported = None;
                }
            }
        }
    }

    fn collect(&mut self) {
        for n in self.rx4285.take_notes() {
            match n {
                s4285::Note::Mode(m) => self.events.push(StanagEvent::Mode {
                    waveform: NAME_4285,
                    data_rate_bps: m.bps,
                    modulation: m.modulation(),
                    coded: m.coded,
                    interleave: m.interleave.label(),
                    source: "decoded",
                }),
                s4285::Note::Dpdu(m, d) => self.dpdu(NAME_4285, Some(m.bps), d),
                s4285::Note::Order(_) => {
                    // The modulation is news for the waveform report.
                    self.reported = None;
                }
                s4285::Note::Locked | s4285::Note::Lost => {}
            }
        }
        for n in self.rx110.take_notes() {
            match n {
                m110a::Note::Mode(m) => self.events.push(StanagEvent::Mode {
                    waveform: NAME_110A,
                    data_rate_bps: m.bps,
                    modulation: m.modulation(),
                    coded: m.coded(),
                    interleave: m.interleave_label(),
                    source: "preamble",
                }),
                m110a::Note::Dpdu(m, d) => self.dpdu(NAME_110A, Some(m.bps), d),
                m110a::Note::Eom => self.events.push(StanagEvent::EndOfMessage { waveform: NAME_110A }),
            }
        }
        for e in self.ale.take() {
            self.events.push(e);
        }
    }

    fn dpdu(&mut self, waveform: &'static str, rate: Option<u32>, d: Dpdu) {
        let window = self.fs as u64;
        while self.recent.front().is_some_and(|(t, _)| t + window < self.t) {
            self.recent.pop_front();
        }
        if self.recent.iter().any(|(_, r)| *r == d) {
            return;
        }
        self.recent.push_back((self.t, d.clone()));
        self.events.push(StanagEvent::Dpdu { waveform, data_rate_bps: rate, dpdu: d });
    }
}
