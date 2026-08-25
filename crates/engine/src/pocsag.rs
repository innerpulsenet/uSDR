//! POCSAG pager decoder (512/1200/2400 baud) after NBFM.
//!
//! 2-level FSK NRZ directly on the carrier: the discriminator sign IS the
//! bit. Air polarity is ambiguous, so the sync word and its inverse are both
//! hunted and the winning match locks polarity for the batch. A batch is one
//! sync codeword plus 8 frames × 2 codewords; idle codewords pad the frames.

#[cfg(test)]
use crate::afsk::bit_len;
use crate::timing::{GardnerTed, TimingLoop};

use serde::Serialize;

const SYNC: u32 = 0x7CD2_15D8;
const IDLE: u32 = 0x7A89_C197;
/// BCH(31,21) generator x^10+x^9+x^8+x^6+x^5+x^3+1.
const GEN: u32 = 0b111_0110_1001;
const WORDS_PER_BATCH: usize = 16;
/// Staggered bit-clock phase count.
///
/// Six phases put the worst static timing error at half a phase — well inside
/// what the Gardner TED in each lane tracks (it corrects the residual every
/// bit). Sixteen bought finer alignment the tracking loop could not use, and
/// each lane pays a DC update, a half-integration and a clock tick on every
/// sample of every block: at 48 kHz that was 48 lanes × 48k = 2.3 M pushes/s
/// for this decoder alone.
const PHASES: usize = 6;

/// A decoded POCSAG message addressed to one capcode.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PocsagMessage {
    pub capcode: u32,
    /// Function bits 0–3 from the address codeword.
    pub function: u8,
    /// Alphanumeric or BCD-numeric text; empty when no message words followed.
    pub text: String,
    /// Decoder-selected presentation, while both interpretations remain
    /// available below because POCSAG carries no explicit alpha/numeric bit.
    pub format: PocsagFormat,
    pub alpha_text: String,
    pub numeric_text: String,
    /// Enriched, structured human-readable parsed text.
    pub parsed: Option<String>,
    /// 512, 1200, or 2400 baud.
    pub baud: u32,
    /// True when the message was cut short by an undecodable word, so the
    /// text is only the leading fragment of the page.
    #[serde(default)]
    pub partial: bool,
    /// BCH bits corrected across the address and message words.
    pub corrected_bits: u32,
    /// Native 20-bit message payload words, retained losslessly.
    pub raw_words: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PocsagFormat {
    ToneOnly,
    Alphanumeric,
    Numeric,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PocsagDiagnostics {
    pub bits_512: u64,
    pub bits_1200: u64,
    pub bits_2400: u64,
    pub syncs_512: u64,
    pub syncs_1200: u64,
    pub syncs_2400: u64,
    pub bch_ok: u64,
    pub bch_fixed: u64,
    pub bch_err: u64,
    pub addr_words: u64,
    pub msg_words: u64,
    pub idle_words: u64,
    pub last_sync_baud: Option<u32>,
    pub events: Vec<String>,
}

fn add_event(diag: &mut PocsagDiagnostics, ev: String) {
    if diag.events.len() >= 40 {
        diag.events.remove(0);
    }
    diag.events.push(ev);
}

/// Sign-of-sum slicer over one bit period, staggered by `delay` and tracking
/// its own bit clock.
///
/// The stagger covers an unknown starting phase; the clock covers everything
/// after that. Without it a lane holds alignment only while the accumulated
/// error stays inside ~1/32 of a bit, which across a 544-bit batch is a 57
/// ppm budget that no transmitter and no receive chain actually meets. The
/// two halves of the integration are kept apart so a Gardner detector can
/// read the timing error out of them at no extra cost.
struct BitSlicer {
    clock: TimingLoop,
    ted: GardnerTed,
    delay: usize,
    delay0: usize,
    first: f32,
    second: f32,
    n_first: u32,
    n_second: u32,
    dc: f32,
    /// Reliability of the most recent decision, 0 (coin flip) .. 1 (solid).
    last_soft: f32,
}

impl BitSlicer {
    fn new(fs: f64, baud: u32, delay: usize) -> Self {
        let spb = (fs / f64::from(baud)).max(4.0);
        Self {
            clock: TimingLoop::new(spb),
            ted: GardnerTed::new(),
            delay,
            delay0: delay,
            first: 0.0,
            second: 0.0,
            n_first: 0,
            n_second: 0,
            dc: 0.0,
            last_soft: 1.0,
        }
    }

    /// Reliability of the most recent sliced bit (see `last_soft`).
    fn soft(&self) -> f32 {
        self.last_soft
    }

    fn reset(&mut self) {
        self.clock.reset();
        self.ted.reset();
        self.delay = self.delay0;
        self.first = 0.0;
        self.second = 0.0;
        self.n_first = 0;
        self.n_second = 0;
        self.dc = 0.0;
    }

    /// `true` = positive discriminator deviation over the bit period.
    fn push(&mut self, x: f32, hunting: bool) -> Option<bool> {
        if self.delay > 0 {
            self.delay -= 1;
            return None;
        }
        // Adaptive DC tracking. Fast while hunting; during a locked batch a
        // much slower trace still follows real drift (residual tuner error,
        // discriminator bias) without letting data transitions bias the
        // slice point — the old hunt-only freeze let a mid-batch drift walk
        // the decision threshold exactly when SNR margin was thinnest.
        if hunting {
            self.dc += 0.0005 * (x - self.dc);
        }
        let centred = x - self.dc;
        // Which half of the symbol this sample belongs to, from the clock's
        // own fractional position rather than a sample count, so the split
        // stays honest as the period is pulled. The decision still comes from
        // the sign of the whole integration, exactly as before; the halves
        // exist only so the detector has something to measure.
        if self.clock.at_second_half() {
            self.second += centred;
            self.n_second += 1;
        } else {
            self.first += centred;
            self.n_first += 1;
        }
        if !self.clock.tick() {
            return None;
        }
        let total = self.first + self.second;
        let bit = total >= 0.0;
        // Soft reliability: |total| relative to this lane's recent symbol
        // scale. Near zero means the sample sat on the slice point.
        let scale = (self.first.abs() + self.second.abs()).max(1e-6);
        let soft = 1.0 - (total.abs() / (scale * 1.5)).min(1.0);
        let h1 = self.first / self.n_first.max(1) as f32;
        let h2 = self.second / self.n_second.max(1) as f32;
        // Steering is safe during the sync hunt as well as inside a batch,
        // because the loop is first-order: noise jitters the sampling phase
        // but cannot accumulate into a rate error. Tracking through the hunt
        // is what lets a sync word be found at all when the sender's clock
        // is not ours.
        if let Some(err) = self.ted.push(h1, h2) {
            self.clock.correct(err);
        }
        self.first = 0.0;
        self.second = 0.0;
        self.n_first = 0;
        self.n_second = 0;
        self.last_soft = soft;
        Some(bit)
    }
}

/// Message under construction: address word plus the message words that
/// follow it (each carrying 20 data bits, MSB first).
struct Msg {
    addr18: u32,
    function: u8,
    frame: u8,
    data: Vec<u32>,
    corrected_bits: u32,
}

struct Lane {
    baud: u32,
    phase: usize,
    slicer: BitSlicer,
    shift: u32,
    /// Word index 0..16 within the batch; `usize::MAX` while hunting.
    word: usize,
    inverted: bool,
    word_bits: u32,
    bits_in: u8,
    cur: Option<Msg>,
}

pub struct PocsagDecoder {
    lanes: Vec<Lane>,
    pub diag: PocsagDiagnostics,
}

impl PocsagDecoder {
    /// Create a decoder for `baud` (512, 1200, 2400), or auto (all three) if 0 or unsupported.
    pub fn new(fs: f64, baud: u32) -> Self {
        match baud {
            512 | 1200 | 2400 => {
                let spb = fs as f32 / baud as f32;
                let n = PHASES.min(spb.floor().max(1.0) as usize);
                let lanes = (0..n)
                    .map(|d| {
                        let delay = ((d as f32 / n as f32) * spb).round() as usize;
                        Lane {
                            baud,
                            phase: d,
                            slicer: BitSlicer::new(fs, baud, delay),
                            shift: 0,
                            word: usize::MAX,
                            inverted: false,
                            word_bits: 0,
                            bits_in: 0,
                            cur: None,
                        }
                    })
                    .collect();
                Self {
                    lanes,
                    diag: PocsagDiagnostics::default(),
                }
            }
            _ => Self::auto(fs),
        }
    }

    /// Auto-decoding POCSAG decoder that runs 512, 1200, and 2400 baud lanes concurrently.
    pub fn auto(fs: f64) -> Self {
        let mut lanes = Vec::new();
        for &baud in &[512, 1200, 2400] {
            let spb = fs as f32 / baud as f32;
            let n = PHASES.min(spb.floor().max(1.0) as usize);
            for d in 0..n {
                let delay = ((d as f32 / n as f32) * spb).round() as usize;
                lanes.push(Lane {
                    baud,
                    phase: d,
                    slicer: BitSlicer::new(fs, baud, delay),
                    shift: 0,
                    word: usize::MAX,
                    inverted: false,
                    word_bits: 0,
                    bits_in: 0,
                    cur: None,
                });
            }
        }
        Self {
            lanes,
            diag: PocsagDiagnostics::default(),
        }
    }

    pub fn diagnostics(&self) -> &PocsagDiagnostics {
        &self.diag
    }

    /// A decoder that owns no lanes and decodes nothing.
    ///
    /// Used when the caller runs its own POCSAG bank over the same
    /// discriminator; sync observations are fed back through
    /// [`PocsagDecoder::note_sync`] instead.
    pub fn idle() -> Self {
        Self {
            lanes: Vec::new(),
            diag: PocsagDiagnostics::default(),
        }
    }

    /// Record a POCSAG sync seen by an externally-owned decoder, so a caller
    /// running its own bank can still drive protocol matching here.
    pub fn note_sync(&mut self, baud: u32) {
        match baud {
            512 => self.diag.syncs_512 += 1,
            1200 => self.diag.syncs_1200 += 1,
            _ => self.diag.syncs_2400 += 1,
        }
        self.diag.last_sync_baud = Some(baud);
    }

    pub fn reset(&mut self) {
        for lane in &mut self.lanes {
            lane.slicer.reset();
            lane.shift = 0;
            lane.word = usize::MAX;
            lane.inverted = false;
            lane.word_bits = 0;
            lane.bits_in = 0;
            lane.cur = None;
        }
    }

    /// Discriminator output in Hz. Returns completed messages.
    pub fn process(&mut self, disc_hz: &[f32]) -> Vec<PocsagMessage> {
        let mut out = Vec::new();
        for &x in disc_hz {
            // First pass: every lane slices this sample. Record which lanes
            // produced a bit so pairs can be cross-examined.
            let mut sliced: Vec<(usize, bool)> = Vec::new();
            for (li, lane) in self.lanes.iter_mut().enumerate() {
                let hunting = lane.word == usize::MAX;
                if let Some(bit) = lane.slicer.push(x, hunting) {
                    sliced.push((li, bit));
                }
            }
            // Cross-lane vote: two lanes whose clocks are within 15% of a
            // symbol are sampling the SAME bit. When they disagree, the
            // less-reliable one adopts the more-reliable one's answer. This
            // is worth about a dB at low SNR — the weak lane's independent
            // error becomes an agreement rather than a coin flip.
            for a in 0..sliced.len() {
                for b in (a + 1)..sliced.len() {
                    let (lane_a, bit_a) = sliced[a];
                    let (lane_b, bit_b) = sliced[b];
                    if self.lanes[lane_a].baud != self.lanes[lane_b].baud {
                        continue;
                    }
                    let phase_a = self.lanes[lane_a].slicer.clock.phase_fraction();
                    let phase_b = self.lanes[lane_b].slicer.clock.phase_fraction();
                    if (phase_a - phase_b).abs() > 0.15 || bit_a == bit_b {
                        continue;
                    }
                    // Disagreement at matched phase: the more reliable
                    // slicer wins; the weaker adopts its answer.
                    if self.lanes[lane_a].slicer.soft()
                        > self.lanes[lane_b].slicer.soft()
                    {
                        sliced[b].1 = bit_a;
                    } else {
                        sliced[a].1 = bit_b;
                    }
                }
            }
            for (li, bit) in sliced {
                let lane = &mut self.lanes[li];
                if let Some(p) = lane.push_bit(bit, &mut self.diag) {
                    if let Some(pos) = out.iter().position(|q: &PocsagMessage| {
                        q.capcode == p.capcode
                            && q.baud == p.baud
                            && (q.text == p.text
                                || q.text.starts_with(&p.text)
                                || p.text.starts_with(&q.text))
                    }) {
                        // Prefer a longer decode, then a complete or lower-FEC
                        // decode when staggered timing lanes recovered the same
                        // payload at equal length.
                        if p.text.len() > out[pos].text.len()
                            || (p.text.len() == out[pos].text.len()
                                && ((out[pos].partial && !p.partial)
                                    || (out[pos].partial == p.partial
                                        && p.corrected_bits < out[pos].corrected_bits)))
                        {
                            out[pos] = p;
                        }
                    } else {
                        out.push(p);
                    }
                }
            }
        }
        out
    }
}

impl Lane {
    fn push_bit(&mut self, bit: bool, diag: &mut PocsagDiagnostics) -> Option<PocsagMessage> {
        match self.baud {
            512 => diag.bits_512 += 1,
            1200 => diag.bits_1200 += 1,
            2400 => diag.bits_2400 += 1,
            _ => {}
        }
        if self.word == usize::MAX {
            self.shift = (self.shift << 1) | u32::from(bit);
            let err_norm = (self.shift ^ SYNC).count_ones();
            let err_inv = (self.shift ^ !SYNC).count_ones();
            if err_norm <= 2 || err_inv <= 2 {
                self.inverted = err_inv < err_norm;
                self.word = 0;
                self.word_bits = 0;
                self.bits_in = 0;
                // Sync acquired. The hunt tracker has followed the whole
                // preamble (tau ~ 2000 samples at 48 kS/s), so its DC is
                // settled and honest; it now freezes for the batch.
                let errs = err_norm.min(err_inv);
                match self.baud {
                    512 => diag.syncs_512 += 1,
                    1200 => diag.syncs_1200 += 1,
                    2400 => diag.syncs_2400 += 1,
                    _ => {}
                }
                diag.last_sync_baud = Some(self.baud);
                add_event(
                    diag,
                    format!(
                        "SYNC found ({} baud, {}, phase {}, {} bit errs)",
                        self.baud,
                        if self.inverted { "inverted" } else { "normal" },
                        self.phase,
                        errs
                    ),
                );
            }
            return None;
        }

        let bit = bit != self.inverted;
        self.word_bits = (self.word_bits << 1) | u32::from(bit);
        self.bits_in += 1;
        if self.bits_in < 32 {
            return None;
        }

        let w = self.word_bits;
        self.word_bits = 0;
        self.bits_in = 0;

        if self.word == WORDS_PER_BATCH {
            // We just finished reading the 32-bit SYNC word between batches!
            let err_sync = (w ^ SYNC).count_ones();
            if err_sync <= 3 {
                // SYNC confirmed at batch boundary: advance to word 0 of the new batch
                self.word = 0;
                match self.baud {
                    512 => diag.syncs_512 += 1,
                    1200 => diag.syncs_1200 += 1,
                    2400 => diag.syncs_2400 += 1,
                    _ => {}
                }
                diag.last_sync_baud = Some(self.baud);
                add_event(
                    diag,
                    format!(
                        "SYNC locked next batch ({} baud, phase {}, {} bit errs)",
                        self.baud, self.phase, err_sync
                    ),
                );
                return None;
            } else {
                // Expected SYNC missed / transmission finished. Flush in-progress message.
                let msg = self.take_msg(diag, true);
                self.word = usize::MAX;
                self.shift = if self.inverted { !w } else { w };
                return msg;
            }
        }

        let idx = self.word;
        self.word += 1;
        self.push_word(w, idx, diag)
    }

    fn push_word(
        &mut self,
        w: u32,
        idx: usize,
        diag: &mut PocsagDiagnostics,
    ) -> Option<PocsagMessage> {
        let (w, errs) = match decode_word(w) {
            Some(res) => res,
            None => {
                // Unrecoverable word: the page is cut off mid-stream, so what
                // we hold is only the leading fragment.
                diag.bch_err += 1;
                return self.take_msg(diag, true);
            }
        };
        if errs == 0 {
            diag.bch_ok += 1;
        } else {
            diag.bch_fixed += 1;
        }
        if w == IDLE {
            diag.idle_words += 1;
            return self.take_msg(diag, false);
        }
        if w & 0x8000_0000 == 0 {
            // Address word: 18 address bits (30..13), 2 function bits (12..11).
            let prev = self.take_msg(diag, false);
            diag.addr_words += 1;
            let addr18 = (w >> 13) & 0x3_FFFF;
            let function = ((w >> 11) & 3) as u8;
            let frame = (idx / 2) as u8;
            let capcode = (addr18 << 3) | u32::from(frame);
            add_event(
                diag,
                format!(
                    "ADDR word: capcode={capcode} fn={function} ({} baud)",
                    self.baud
                ),
            );
            self.cur = Some(Msg {
                addr18,
                function,
                frame,
                data: Vec::new(),
                corrected_bits: errs as u32,
            });
            prev
        } else if let Some(cur) = &mut self.cur {
            diag.msg_words += 1;
            cur.data.push((w >> 11) & 0xF_FFFF);
            cur.corrected_bits += errs as u32;
            None
        } else {
            None
        }
    }

    fn take_msg(&mut self, diag: &mut PocsagDiagnostics, partial: bool) -> Option<PocsagMessage> {
        let m = self.cur.take()?;
        let decoded = decode_text(&m.data);
        let text = decoded.text;
        let capcode = (m.addr18 << 3) | u32::from(m.frame);
        let parsed = crate::pager_parser::parse_pager_text(&text, Some(m.function));
        add_event(
            diag,
            format!(
                "PAGE [{} baud]: capcode={capcode} fn={} '{text}'{}",
                self.baud,
                m.function,
                if partial { " (partial)" } else { "" }
            ),
        );
        Some(PocsagMessage {
            capcode,
            function: m.function,
            text,
            format: decoded.format,
            alpha_text: decoded.alpha,
            numeric_text: decoded.numeric,
            parsed: Some(parsed),
            baud: self.baud,
            partial,
            corrected_bits: m.corrected_bits,
            raw_words: m.data,
        })
    }
}

/// Remainder of the 31-bit BCH word (bit 30 = x^30) mod the generator.
fn syndrome(mut v: u32) -> u32 {
    for i in (10..31).rev() {
        if v >> i & 1 == 1 {
            v ^= GEN << (i - 10);
        }
    }
    v & 0x3FF
}

const VALID: u32 = 1 << 31;

/// syndrome → error mask over the 31-bit BCH word, for 0/1/2-bit errors.
fn syndrome_table() -> &'static [u32; 1024] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 1024]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 1024];
        t[0] = VALID;
        for i in 0..31 {
            t[syndrome(1 << i) as usize] = (1 << i) | VALID;
            for j in (i + 1)..31 {
                t[syndrome((1 << i) | (1 << j)) as usize] = (1 << i) | (1 << j) | VALID;
            }
        }
        t
    })
}

/// BCH-correct up to 2 bit errors, then verify the even parity bit. Returns (word, corrected_bit_count).
fn decode_word(w: u32) -> Option<(u32, usize)> {
    let entry = syndrome_table()[syndrome(w >> 1) as usize];
    if entry & VALID == 0 {
        return None;
    }
    let mask = entry & !VALID;
    let mut full = ((w >> 1) ^ mask) << 1 | (w & 1);
    let mut errs = mask.count_ones() as usize;
    if full.count_ones() & 1 == 1 {
        // Parity fails after correction: with at most one corrected bit the
        // parity bit itself is the second error; otherwise it is 3+ errors.
        if mask.count_ones() <= 1 {
            full ^= 1;
            errs += 1;
        } else {
            return None;
        }
    }
    Some((full, errs))
}

/// Message words → text. Alphanumeric 7-bit ASCII (groups LSB-first) when the
/// decode is mostly printable, otherwise numeric BCD (4 bits/char, LSB-first).
struct DecodedText {
    text: String,
    alpha: String,
    numeric: String,
    format: PocsagFormat,
}

fn decode_text(chunks: &[u32]) -> DecodedText {
    let mut bits = Vec::with_capacity(20 * chunks.len());
    for &c in chunks {
        for i in (0..20).rev() {
            bits.push(c >> i & 1 == 1);
        }
    }
    if bits.is_empty() {
        return DecodedText {
            text: String::new(),
            alpha: String::new(),
            numeric: String::new(),
            format: PocsagFormat::ToneOnly,
        };
    }
    let alpha: Vec<char> = bits
        .chunks_exact(7)
        .map(|g| {
            g.iter()
                .enumerate()
                .fold(0u8, |v, (j, &b)| v | u8::from(b) << j) as char
        })
        .collect();
    let printable = alpha.iter().filter(|c| (' '..='~').contains(c)).count();
    let wordy = alpha
        .iter()
        .filter(|c| c.is_ascii_alphanumeric() || **c == ' ')
        .count();
    // "Mostly printable" alone is fooled by short numeric pages: BCD digits
    // regrouped into 7 bits often land on punctuation. Require over two
    // thirds letters/digits/spaces before calling it alphanumeric.
    let alpha_text = alpha
        .iter()
        .collect::<String>()
        .trim_end_matches(|c: char| {
            c == '\0' || c == '\x03' || c == '\x04' || !(' '..='~').contains(&c)
        })
        .trim_end()
        .to_string();
    const SET: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '*', 'U', ' ', '-', ')', '(',
    ];
    let numeric_text = bits
        .chunks_exact(4)
        .map(|g| {
            SET[g
                .iter()
                .enumerate()
                .fold(0usize, |v, (j, &b)| v | usize::from(b) << j)]
        })
        .collect::<String>()
        .trim_end()
        .to_string();
    let alpha_selected =
        !alpha.is_empty() && printable * 5 >= alpha.len() * 4 && wordy * 3 > alpha.len() * 2;
    DecodedText {
        text: if alpha_selected {
            alpha_text.clone()
        } else {
            numeric_text.clone()
        },
        alpha: alpha_text,
        numeric: numeric_text,
        format: if alpha_selected {
            PocsagFormat::Alphanumeric
        } else {
            PocsagFormat::Numeric
        },
    }
}

/// Build a 32-bit codeword: 20 data bits (30..11) + BCH + even parity.
#[cfg(test)]
fn codeword(data20: u32, message: bool) -> u32 {
    let bch_data = (data20 << 11 | u32::from(message) << 31) >> 1;
    let bch = bch_data | syndrome(bch_data);
    bch << 1 | (bch.count_ones() & 1)
}

/// 7-bit ASCII string → message words, groups LSB-first as on the air.
#[cfg(test)]
fn alpha_words(s: &str) -> Vec<u32> {
    let mut bits = Vec::new();
    for c in s.bytes() {
        for j in 0..7 {
            bits.push(c >> j & 1 == 1);
        }
    }
    bits.chunks(20)
        .map(|g| {
            g.iter()
                .enumerate()
                .fold(0u32, |v, (j, &b)| v | u32::from(b) << (19 - j))
        })
        .collect()
}

/// Preamble + sync + batches holding `addr18`/function in `frame`, followed
/// by the message words; everything else idle. Rendered as a ±4.5 kHz square.
#[cfg(test)]
fn burst_audio(
    fs: f32,
    baud: u32,
    addr18: u32,
    function: u8,
    frame: usize,
    msg: &[u32],
) -> Vec<f32> {
    let mut all_words = Vec::new();
    let addr_word = codeword(addr18 << 2 | u32::from(function), false);

    // Batch 1
    let mut batch1 = vec![IDLE; WORDS_PER_BATCH];
    batch1[frame * 2] = addr_word;
    let mut msg_idx = 0;
    for i in (frame * 2 + 1)..WORDS_PER_BATCH {
        if msg_idx < msg.len() {
            batch1[i] = codeword(msg[msg_idx], true);
            msg_idx += 1;
        }
    }
    all_words.push(SYNC);
    all_words.extend(batch1);

    // Subsequent batches if message words remain
    while msg_idx < msg.len() {
        let mut next_batch = vec![IDLE; WORDS_PER_BATCH];
        for i in 0..WORDS_PER_BATCH {
            if msg_idx < msg.len() {
                next_batch[i] = codeword(msg[msg_idx], true);
                msg_idx += 1;
            }
        }
        all_words.push(SYNC);
        all_words.extend(next_batch);
    }

    let mut bits = Vec::new();
    for i in 0..600 {
        bits.push(i % 2 == 0);
    }
    for w in all_words {
        for i in (0..32).rev() {
            bits.push(w >> i & 1 == 1);
        }
    }
    let spb = fs / baud as f32;
    let mut out = Vec::new();
    for (k, &b) in bits.iter().enumerate() {
        let hz = if b { 4500.0 } else { -4500.0 };
        out.extend(std::iter::repeat_n(hz, bit_len(k, spb)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f32 = 16_000.0;

    #[test]
    fn bch_accepts_the_standard_words_and_corrects_two_bits() {
        assert_eq!(decode_word(SYNC), Some((SYNC, 0)));
        assert_eq!(decode_word(IDLE), Some((IDLE, 0)));
        assert_eq!(decode_word(SYNC ^ 0b101), Some((SYNC, 2)));
        assert_eq!(decode_word(IDLE ^ (1 << 5) ^ (1 << 20)), Some((IDLE, 2)));
        // The parity bit itself can be one of the two errors.
        assert_eq!(decode_word(SYNC ^ 1 ^ (1 << 12)), Some((SYNC, 2)));
        // Three bit errors must not come back as the original word.
        let mut rejected = false;
        for w in 0..200u32 {
            let w = w.wrapping_mul(0x9E37_79B1) ^ 0xA5A5_5A5A;
            if decode_word(w).is_none() {
                rejected = true;
                break;
            }
        }
        assert!(rejected, "no word rejected in 200 tries");
        assert_ne!(decode_word(SYNC ^ 0b101 ^ (1 << 22)), Some((SYNC, 0)));
    }

    #[test]
    fn round_trip_at_each_baud() {
        for baud in [512, 1200, 2400] {
            let msg = alpha_words("HELLO N1TEST");
            let audio = burst_audio(FS, baud, 0x12345, 3, 2, &msg);
            let mut dec = PocsagDecoder::new(f64::from(FS), baud);
            let got = dec.process(&audio);
            let want = (0x12345 << 3) | 2;
            assert!(
                got.iter()
                    .any(|m| m.capcode == want && m.text == "HELLO N1TEST"),
                "baud {baud}: got {got:?}"
            );
            // One burst, one message despite 8 lanes.
            assert_eq!(got.iter().filter(|m| m.capcode == want).count(), 1);
        }
    }

    /// The clock the transmitter used is never exactly the clock we assumed.
    /// A fixed-phase lane bank tolerates about 1/32 of a bit of accumulated
    /// error across a batch — 57 ppm over 544 bits — so before the timing
    /// loop existed this range decoded nothing, or worse, decoded the first
    /// words and then fed the BCH corrector debris it happily accepted as
    /// fresh addresses.
    #[test]
    fn a_clock_offset_the_lane_bank_alone_could_not_survive_still_decodes() {
        let want = (0x12345u32 << 3) | 2;
        for baud in [512u32, 1200, 2400] {
            for ppm in [-3_000i32, -1_600, -500, 0, 500, 1_600, 3_000] {
                let msg = alpha_words("HELLO N1TEST");
                // Generate at a rate the decoder was *not* told about: the
                // 1600 ppm case is exactly what the DATA hopper saw when a
                // 2.083334 MS/s chain was described to it as 16 kHz.
                let air_fs = FS * (1.0 + ppm as f32 * 1e-6);
                let audio = burst_audio(air_fs, baud, 0x12345, 3, 2, &msg);
                let mut dec = PocsagDecoder::new(f64::from(FS), baud);
                let got = dec.process(&audio);
                assert!(
                    got.iter()
                        .any(|m| m.capcode == want && m.text == "HELLO N1TEST"),
                    "baud {baud} at {ppm} ppm: got {got:?}"
                );
                assert_eq!(
                    got.iter().filter(|m| m.capcode == want).count(),
                    1,
                    "baud {baud} at {ppm} ppm produced duplicates: {got:?}"
                );
                // And no invented pages: a slipping clock used to manufacture
                // capcodes out of misaligned words.
                assert_eq!(
                    got.len(),
                    1,
                    "baud {baud} at {ppm} ppm invented traffic: {got:?}"
                );
            }
        }
    }

    #[test]
    fn inverted_polarity_still_decodes() {
        let msg = alpha_words("FLIP ME");
        let audio: Vec<f32> = burst_audio(FS, 1200, 777, 3, 5, &msg)
            .into_iter()
            .map(|x| -x)
            .collect();
        let mut dec = PocsagDecoder::new(f64::from(FS), 1200);
        let got = dec.process(&audio);
        assert!(
            got.iter()
                .any(|m| m.capcode == (777 << 3) | 5 && m.text == "FLIP ME"),
            "got {got:?}"
        );
    }

    #[test]
    fn numeric_function_decodes_bcd() {
        // "123-456" in BCD, LSB-first per char, space-padded to whole words.
        let digits = [1u32, 2, 3, 13, 4, 5, 6, 12, 12, 12];
        let mut bits = Vec::new();
        for d in digits {
            for j in 0..4 {
                bits.push(d >> j & 1 == 1);
            }
        }
        let words: Vec<u32> = bits
            .chunks(20)
            .map(|g| {
                g.iter()
                    .enumerate()
                    .fold(0u32, |v, (j, &b)| v | u32::from(b) << (19 - j))
            })
            .collect();
        let audio = burst_audio(FS, 1200, 42, 0, 0, &words);
        let mut dec = PocsagDecoder::new(f64::from(FS), 1200);
        let got = dec.process(&audio);
        assert!(
            got.iter()
                .any(|m| m.capcode == 42 << 3 && m.text == "123-456"),
            "got {got:?}"
        );
    }

    #[test]
    fn message_split_across_process_calls_decodes() {
        let msg = alpha_words("CHUNKED DELIVERY");
        let audio = burst_audio(FS, 1200, 1000, 3, 2, &msg);
        let mut dec = PocsagDecoder::new(f64::from(FS), 1200);
        let mut got = Vec::new();
        for chunk in audio.chunks(1000) {
            got.extend(dec.process(chunk));
        }
        assert!(
            got.iter()
                .any(|m| m.capcode == (1000 << 3) | 2 && m.text == "CHUNKED DELIVERY"),
            "got {got:?}"
        );
    }

    #[test]
    fn an_address_only_page_has_empty_text() {
        let audio = burst_audio(FS, 2400, 55, 1, 3, &[]);
        let mut dec = PocsagDecoder::new(f64::from(FS), 2400);
        let got = dec.process(&audio);
        assert!(
            got.iter()
                .any(|m| m.capcode == (55 << 3) | 3 && m.function == 1 && m.text.is_empty()),
            "got {got:?}"
        );
    }

    #[test]
    fn noise_yields_nothing() {
        let mut x = 0x1234_5678u32;
        let noise: Vec<f32> = (0..32_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x as f32 / u32::MAX as f32 - 0.5) * 12_000.0
            })
            .collect();
        let mut dec = PocsagDecoder::new(f64::from(FS), 1200);
        assert!(dec.process(&noise).is_empty());
    }

    #[test]
    fn auto_decode_mixed_bauds() {
        let mut dec = PocsagDecoder::auto(f64::from(FS));
        let m512 = alpha_words("512 PAGE");
        let a512 = burst_audio(FS, 512, 100, 3, 1, &m512);
        let m1200 = alpha_words("1200 PAGE");
        let a1200 = burst_audio(FS, 1200, 200, 3, 2, &m1200);
        let m2400 = alpha_words("2400 PAGE");
        let a2400 = burst_audio(FS, 2400, 300, 3, 3, &m2400);

        let mut all_audio = Vec::new();
        all_audio.extend_from_slice(&a512);
        all_audio.extend(vec![0.0f32; 1000]);
        all_audio.extend_from_slice(&a1200);
        all_audio.extend(vec![0.0f32; 1000]);
        all_audio.extend_from_slice(&a2400);

        let got = dec.process(&all_audio);
        assert_eq!(got.len(), 3, "got {got:?}");
        assert_eq!(got[0].baud, 512);
        assert_eq!(got[0].text, "512 PAGE");
        assert_eq!(got[1].baud, 1200);
        assert_eq!(got[1].text, "1200 PAGE");
        assert_eq!(got[2].baud, 2400);
        assert_eq!(got[2].text, "2400 PAGE");
    }

    #[test]
    fn long_message_spanning_multiple_batches_decodes_fully() {
        let text = "DISPATCH: STRUCTURE FIRE AT 144 ELM ST - UNITS E1, E3, T2 - CROSS OF MAPLE & PINE - SMOKE REPORTED ON 2ND FLOOR - CODE BLUE RESPOND STAT";
        let msg = alpha_words(text);
        assert!(
            msg.len() > WORDS_PER_BATCH,
            "test message must span multiple batches"
        );

        let audio = burst_audio(FS, 1200, 0x34321, 3, 1, &msg);
        let mut dec = PocsagDecoder::new(f64::from(FS), 1200);
        let got = dec.process(&audio);
        let want_cap = (0x34321 << 3) | 1;
        assert!(
            got.iter().any(|m| m.capcode == want_cap && m.text == text),
            "multi-batch message failed to decode fully. Got: {got:?}"
        );
        let matched = got.iter().find(|m| m.capcode == want_cap).unwrap();
        assert_eq!(matched.text, text);
        assert!(!matched.partial);
    }

    #[test]
    fn a_lost_batch_sync_emits_the_visible_prefix_as_partial() {
        let text = "THIS PAGE CONTINUES INTO ANOTHER BATCH AND MUST BE MARKED PARTIAL";
        let words = alpha_words(text);
        assert!(words.len() > 15);
        let mut audio = burst_audio(FS, 1200, 0x23456, 3, 0, &words);

        // Corrupt four bits of the inter-batch sync. Three are tolerated; four
        // must terminate the page without silently presenting it as complete.
        let spb = FS / 1200.0;
        let sync_bit = 600 + 32 + WORDS_PER_BATCH * 32;
        for bit in sync_bit..sync_bit + 4 {
            let start: usize = (0..bit).map(|k| bit_len(k, spb)).sum();
            let end = start + bit_len(bit, spb);
            for sample in &mut audio[start..end] {
                *sample = -*sample;
            }
        }

        let mut decoder = PocsagDecoder::new(f64::from(FS), 1200);
        let got = decoder.process(&audio);
        let page = got
            .iter()
            .find(|page| page.capcode == (0x23456 << 3))
            .expect("leading page fragment");
        assert!(page.partial);
        assert!(text.starts_with(&page.text));
        assert_eq!(page.raw_words.len(), 15);
    }
}
