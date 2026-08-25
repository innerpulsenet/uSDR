//! Motorola FLEX pager decoding after NBFM discrimination.
//!
//! The synchronisation word and FIW are always sent as 1600-symbol/s 2-FSK.
//! Sync-1's A-code selects the DATA-section rate and level count: 1600/2,
//! 1600/4, 3200/2, or 3200/4.  The DATA section is then de-interleaved into
//! phases A-D before BCH correction and BIW/address/vector/message parsing.
//!
//! The framing and field layout are based on Motorola/Freescale FLEX decoder
//! documentation and independently checked against multimon-ng and xng.  FLEX
//! transmits its BCH words LSB first; keeping that native orientation here is
//! important because it is the opposite of POCSAG's presentation.

use crate::timing::{GardnerTed, TimingLoop};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const SYNC_MARKER: u32 = 0xA6C6_AAAA;
const A_1600_2: u16 = 0x870C;
const A_1600_4: u16 = 0xB068;
const A_3200_2: u16 = 0x7B18;
const A_3200_4: u16 = 0xDEA0;
const A_3200_4_ALT: u16 = 0x4C7C;
const WORDS_PER_PHASE: usize = 88;
const DATA_MS: usize = 1760;
const SYNC2_MS: usize = 25;
const MAX_SYNC_ERRORS: u32 = 3;
const FRAGMENT_TTL: Duration = Duration::from_secs(12);
const GROUP_MIN: u64 = 2_029_568;
const GROUP_MAX: u64 = 2_029_583;
const MAX_CAPCODE: u64 = 4_297_068_542;
const NUMERIC: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', ' ', 'U', ' ', '-', ']', '[',
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FlexFormat {
    Secure,
    ShortInstruction,
    ToneOnly,
    StandardNumeric,
    SpecialNumeric,
    Alphanumeric,
    Binary,
    NumberedNumeric,
}

impl FlexFormat {
    fn from_vector(word: u32) -> Self {
        match (word >> 4) & 7 {
            0 => Self::Secure,
            1 => Self::ShortInstruction,
            2 => Self::ToneOnly,
            3 => Self::StandardNumeric,
            4 => Self::SpecialNumeric,
            5 => Self::Alphanumeric,
            6 => Self::Binary,
            _ => Self::NumberedNumeric,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FlexFragment {
    Complete,
    First,
    Middle,
    Continuation,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlexMessage {
    pub capcode: u64,
    pub cycle: u8,
    pub frame: u8,
    pub phase: char,
    pub format: FlexFormat,
    pub text: String,
    pub parsed: Option<String>,
    /// Information bit rate (1600, 3200, or 6400).
    pub baud: u32,
    pub symbol_rate: u32,
    pub levels: u8,
    pub long_address: bool,
    pub address_type: String,
    pub priority: bool,
    pub fragment: FlexFragment,
    pub fragment_number: Option<u8>,
    pub message_number: Option<u8>,
    pub retrieval: Option<bool>,
    pub maildrop: Option<bool>,
    pub payload_checksum_ok: Option<bool>,
    pub secure_subtype: Option<String>,
    pub complete: bool,
    pub reassembled: bool,
    pub fec_corrected: u32,
    pub fec_uncorrectable: u32,
    /// Recipients accumulated by a short-instruction group assignment.
    pub group_recipients: Vec<u64>,
    /// Corrected 21-bit message words, retained for secure/binary inspection.
    pub raw_words: Vec<u32>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FlexDiagnostics {
    pub syncs_1600: u64,
    pub syncs_3200: u64,
    pub syncs_6400: u64,
    pub frames_decoded: u64,
    pub phases_decoded: u64,
    pub bch_ok: u64,
    pub bch_fixed: u64,
    pub bch_err: u64,
    pub addr_words: u64,
    pub msg_words: u64,
    pub fragments_started: u64,
    pub fragments_completed: u64,
    pub last_frame_no: Option<u8>,
    pub last_cycle_no: Option<u8>,
    pub last_symbol_rate: Option<u32>,
    pub last_levels: Option<u8>,
    pub events: Vec<String>,
}

fn add_event(diag: &mut FlexDiagnostics, event: String) {
    if diag.events.len() >= 80 {
        diag.events.remove(0);
    }
    diag.events.push(event);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mode {
    symbol_rate: u32,
    levels: u8,
}

impl Mode {
    fn baud(self) -> u32 {
        self.symbol_rate * u32::from(self.levels) / 2
    }

    fn from_a_code(code: u16) -> Option<Self> {
        [
            (A_1600_2, 1600, 2),
            (A_1600_4, 1600, 4),
            (A_3200_2, 3200, 2),
            (A_3200_4, 3200, 4),
            (A_3200_4_ALT, 3200, 4),
        ]
        .into_iter()
        .filter_map(|(known, symbol_rate, levels)| {
            let errors = (known ^ code).count_ones();
            (errors <= MAX_SYNC_ERRORS).then_some((
                errors,
                Self {
                    symbol_rate,
                    levels,
                },
            ))
        })
        .min_by_key(|(errors, _)| *errors)
        .map(|(_, mode)| mode)
    }
}

/// BCH(31,21)+parity in FLEX-native orientation: information in bits 0..20,
/// check bits 21..30, parity at bit 31.
const BCH_POLY: u32 = 0x769;

fn reflect31(value: u32) -> u32 {
    let mut out = 0;
    for bit in 0..31 {
        if value & (1 << bit) != 0 {
            out |= 1 << (30 - bit);
        }
    }
    out
}

fn syndrome(word: u32) -> u32 {
    let mut value = reflect31(word & 0x7FFF_FFFF);
    for bit in (10..31).rev() {
        if value & (1 << bit) != 0 {
            value ^= BCH_POLY << (bit - 10);
        }
    }
    value & 0x3FF
}

fn parity_bit(word31: u32) -> u32 {
    (word31 & 0x7FFF_FFFF).count_ones() & 1
}

fn valid_word(word: u32) -> bool {
    syndrome(word) == 0 && word.count_ones() & 1 == 0
}

/// Correct every pattern of up to two errors, including the parity bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlexBchError;

pub fn bch_correct(raw: u32) -> Result<(u32, usize), FlexBchError> {
    if valid_word(raw) {
        return Ok((raw, 0));
    }
    let base = raw & 0x7FFF_FFFF;
    if syndrome(base) == 0 {
        let fixed = base | (parity_bit(base) << 31);
        return Ok((fixed, (fixed ^ raw).count_ones() as usize));
    }
    for first in 0..31 {
        let candidate = base ^ (1 << first);
        if syndrome(candidate) == 0 {
            let fixed = candidate | (parity_bit(candidate) << 31);
            let errors = (fixed ^ raw).count_ones();
            if errors <= 2 {
                return Ok((fixed, errors as usize));
            }
        }
    }
    for first in 0..31 {
        for second in (first + 1)..31 {
            let candidate = base ^ (1 << first) ^ (1 << second);
            if syndrome(candidate) == 0 {
                let fixed = candidate | (parity_bit(candidate) << 31);
                let errors = (fixed ^ raw).count_ones();
                if errors <= 2 {
                    return Ok((fixed, errors as usize));
                }
            }
        }
    }
    Err(FlexBchError)
}

#[cfg(test)]
fn encode_word(data: u32) -> u32 {
    let data = data & 0x1F_FFFF;
    let mut reflected = 0;
    for bit in 0..21 {
        if data & (1 << bit) != 0 {
            reflected |= 1 << (30 - bit);
        }
    }
    let mut remainder = reflected;
    for bit in (10..31).rev() {
        if remainder & (1 << bit) != 0 {
            remainder ^= BCH_POLY << (bit - 10);
        }
    }
    let word31 = reflect31(reflected | (remainder & 0x3FF));
    word31 | (parity_bit(word31) << 31)
}

/// Integrate-and-dump four-level slicer with a tracked symbol clock.
///
/// The stagger covers an unknown starting phase; the loop covers the rate
/// error that accumulates after it. FLEX runs 1600, 3200 and 6400 sym/s in
/// one stream, so the clock is re-targeted when the sync word says which,
/// and the halves of the integration feed a Gardner detector that works the
/// same on four levels as on two.
struct SymbolClock {
    fs: f64,
    clock: TimingLoop,
    ted: GardnerTed,
    delay: usize,
    initial_delay: usize,
    sum: f32,
    count: usize,
    first: f32,
    second: f32,
    n_first: u32,
    n_second: u32,
    dc: f32,
    outer: f32,
}

impl SymbolClock {
    fn new(fs: f64, delay: usize) -> Self {
        Self {
            fs,
            clock: TimingLoop::new(fs / 1600.0),
            ted: GardnerTed::new(),
            delay,
            initial_delay: delay,
            sum: 0.0,
            count: 0,
            first: 0.0,
            second: 0.0,
            n_first: 0,
            n_second: 0,
            dc: 0.0,
            outer: 1.0,
        }
    }

    fn reset(&mut self) {
        self.clock = TimingLoop::new(self.fs / 1600.0);
        self.ted.reset();
        self.delay = self.initial_delay;
        self.clear_integration();
        self.dc = 0.0;
        self.outer = 1.0;
    }

    fn clear_integration(&mut self) {
        self.sum = 0.0;
        self.count = 0;
        self.first = 0.0;
        self.second = 0.0;
        self.n_first = 0;
        self.n_second = 0;
    }

    fn set_rate(&mut self, rate: u32) {
        self.clock.set_period(self.fs / f64::from(rate));
        self.ted.reset();
        self.clear_integration();
    }

    fn push(&mut self, sample: f32, hunting: bool) -> Option<u8> {
        if self.delay > 0 {
            self.delay -= 1;
            return None;
        }
        if hunting {
            self.dc += 0.0005 * (sample - self.dc);
        }
        let centered = sample - self.dc;
        self.sum += centered;
        self.count += 1;
        if self.clock.at_second_half() {
            self.second += centered;
            self.n_second += 1;
        } else {
            self.first += centered;
            self.n_first += 1;
        }
        if !self.clock.tick() {
            return None;
        }
        let average = self.sum / self.count.max(1) as f32;
        // Steered through the sync hunt as well as inside a frame: the loop
        // is first-order, so noise moves the sampling phase but cannot
        // accumulate into a rate error the next burst would inherit.
        if let Some(err) = self.ted.push(
            self.first / self.n_first.max(1) as f32,
            self.second / self.n_second.max(1) as f32,
        ) {
            self.clock.correct(err);
        }
        self.clear_integration();
        if hunting {
            let magnitude = average.abs();
            if magnitude > self.outer * 0.5 {
                self.outer += 0.03 * (magnitude - self.outer);
            } else if magnitude < self.outer * 0.25 {
                // Decay the reference back down when the band goes quiet.
                // Without this one noisy peak during a previous hunt ratchets
                // `outer` up forever, and the 2/3 threshold then swallows the
                // outer levels of every later weak frame.
                self.outer *= 0.995;
            }
        }
        let threshold = (self.outer * 0.667).max(1.0);
        Some(if average >= threshold {
            3
        } else if average >= 0.0 {
            2
        } else if average > -threshold {
            1
        } else {
            0
        })
    }
}

enum LaneState {
    Hunting {
        shift: u64,
        symbols: usize,
    },
    Fiw {
        mode: Mode,
        inverted: bool,
        skip: usize,
        read: usize,
        word: u32,
    },
    Sync2 {
        mode: Mode,
        inverted: bool,
        fiw: u32,
        fiw_errors: u32,
        remaining: usize,
    },
    Data {
        mode: Mode,
        inverted: bool,
        fiw: u32,
        fiw_errors: u32,
        symbols: Vec<u8>,
        needed: usize,
    },
}

struct Lane {
    clock: SymbolClock,
    state: LaneState,
}

struct RawFrame {
    mode: Mode,
    fiw: u32,
    fiw_errors: u32,
    symbols: Vec<u8>,
}

impl Lane {
    fn new(fs: f64, delay: usize) -> Self {
        Self {
            clock: SymbolClock::new(fs, delay),
            state: LaneState::Hunting {
                shift: 0,
                symbols: 0,
            },
        }
    }

    fn reset(&mut self) {
        self.clock.reset();
        self.state = LaneState::Hunting {
            shift: 0,
            symbols: 0,
        };
    }

    fn hunt(&mut self) {
        self.clock.set_rate(1600);
        self.state = LaneState::Hunting {
            shift: 0,
            symbols: 0,
        };
    }

    fn push(&mut self, sample: f32, diag: &mut FlexDiagnostics) -> Option<RawFrame> {
        let hunting = matches!(self.state, LaneState::Hunting { .. });
        let symbol = self.clock.push(sample, hunting)?;
        match &mut self.state {
            LaneState::Hunting { shift, symbols } => {
                *shift = (*shift << 1) | u64::from(symbol < 2);
                *symbols = (*symbols + 1).min(64);
                if *symbols == 64
                    && let Some((mode, inverted)) = sync_mode(*shift)
                {
                    let baud = mode.baud();
                    match baud {
                        1600 => diag.syncs_1600 += 1,
                        3200 => diag.syncs_3200 += 1,
                        6400 => diag.syncs_6400 += 1,
                        _ => {}
                    }
                    diag.last_symbol_rate = Some(mode.symbol_rate);
                    diag.last_levels = Some(mode.levels);
                    add_event(
                        diag,
                        format!(
                            "SYNC1 locked: {} bps, {} sym/s, {}-FSK{}",
                            baud,
                            mode.symbol_rate,
                            mode.levels,
                            if inverted { ", inverted" } else { "" }
                        ),
                    );
                    self.state = LaneState::Fiw {
                        mode,
                        inverted,
                        skip: 16,
                        read: 0,
                        word: 0,
                    };
                }
            }
            LaneState::Fiw {
                mode,
                inverted,
                skip,
                read,
                word,
            } => {
                if *skip > 0 {
                    *skip -= 1;
                    return None;
                }
                let rectified = if *inverted { 3 - symbol } else { symbol };
                *word = (*word >> 1) | ((rectified > 1) as u32) << 31;
                *read += 1;
                if *read == 32 {
                    let mode_copy = *mode;
                    let inverted_copy = *inverted;
                    match bch_correct(*word) {
                        Ok((fiw, errors)) if fiw_checksum_ok(fiw) => {
                            let cycle = ((fiw >> 4) & 0xF) as u8;
                            let frame = ((fiw >> 8) & 0x7F) as u8;
                            diag.last_cycle_no = Some(cycle);
                            diag.last_frame_no = Some(frame);
                            account_bch(diag, errors);
                            add_event(
                                diag,
                                format!("FIW cycle={cycle} frame={frame} corrected={errors}"),
                            );
                            self.clock.set_rate(mode_copy.symbol_rate);
                            self.state = LaneState::Sync2 {
                                mode: mode_copy,
                                inverted: inverted_copy,
                                fiw,
                                fiw_errors: errors as u32,
                                remaining: mode_copy.symbol_rate as usize * SYNC2_MS / 1000,
                            };
                        }
                        _ => {
                            diag.bch_err += 1;
                            add_event(diag, "FIW rejected (BCH/checksum)".into());
                            self.hunt();
                        }
                    }
                }
            }
            LaneState::Sync2 {
                mode,
                inverted,
                fiw,
                fiw_errors,
                remaining,
            } => {
                *remaining = remaining.saturating_sub(1);
                if *remaining == 0 {
                    let mode_copy = *mode;
                    self.state = LaneState::Data {
                        mode: mode_copy,
                        inverted: *inverted,
                        fiw: *fiw,
                        fiw_errors: *fiw_errors,
                        symbols: Vec::with_capacity(
                            mode_copy.symbol_rate as usize * DATA_MS / 1000,
                        ),
                        needed: mode_copy.symbol_rate as usize * DATA_MS / 1000,
                    };
                }
            }
            LaneState::Data {
                mode,
                inverted,
                fiw,
                fiw_errors,
                symbols,
                needed,
            } => {
                symbols.push(if *inverted { 3 - symbol } else { symbol });
                if symbols.len() == *needed {
                    let frame = RawFrame {
                        mode: *mode,
                        fiw: *fiw,
                        fiw_errors: *fiw_errors,
                        symbols: std::mem::take(symbols),
                    };
                    self.hunt();
                    return Some(frame);
                }
            }
        }
        None
    }
}

fn sync_mode(raw: u64) -> Option<(Mode, bool)> {
    let mut best: Option<(u32, Mode, bool)> = None;
    for inverted in [false, true] {
        let word = if inverted { !raw } else { raw };
        let marker = ((word >> 16) & 0xFFFF_FFFF) as u32;
        let high = (word >> 48) as u16;
        let low = !(word as u16);
        let errors = (marker ^ SYNC_MARKER).count_ones() + (high ^ low).count_ones();
        if (marker ^ SYNC_MARKER).count_ones() <= MAX_SYNC_ERRORS
            && (high ^ low).count_ones() <= MAX_SYNC_ERRORS
            && let Some(mode) = Mode::from_a_code(high)
            && best.is_none_or(|(old, _, _)| errors < old)
        {
            best = Some((errors, mode, inverted));
        }
    }
    best.map(|(_, mode, inverted)| (mode, inverted))
}

fn fiw_checksum_ok(word: u32) -> bool {
    let data = word & 0x1F_FFFF;
    let sum = (data & 0xF)
        + ((data >> 4) & 0xF)
        + ((data >> 8) & 0xF)
        + ((data >> 12) & 0xF)
        + ((data >> 16) & 0xF)
        + ((data >> 20) & 1);
    sum & 0xF == 0xF
}

fn account_bch(diag: &mut FlexDiagnostics, errors: usize) {
    if errors == 0 {
        diag.bch_ok += 1;
    } else {
        diag.bch_fixed += 1;
    }
}

fn phase_index(counter: u32) -> usize {
    (((counter >> 5) & 0xFFF8) | (counter & 7)) as usize
}

fn deinterleave(symbols: &[u8], mode: Mode) -> Vec<(char, [u32; WORDS_PER_PHASE])> {
    let mut phases = [[0u32; WORDS_PER_PHASE]; 4];
    let mut counter = 0u32;
    let mut toggle = false;
    for &symbol in symbols {
        let bit_a = u32::from(symbol > 1);
        let bit_b = u32::from(symbol == 1 || symbol == 2);
        let idx = phase_index(counter);
        let pair = if mode.symbol_rate == 3200 && toggle {
            2
        } else {
            0
        };
        phases[pair][idx] = (phases[pair][idx] >> 1) | (bit_a << 31);
        if mode.levels == 4 {
            phases[pair + 1][idx] = (phases[pair + 1][idx] >> 1) | (bit_b << 31);
        }
        if mode.symbol_rate == 3200 {
            toggle = !toggle;
            if !toggle {
                counter += 1;
            }
        } else {
            counter += 1;
        }
    }
    match (mode.symbol_rate, mode.levels) {
        (1600, 2) => vec![('A', phases[0])],
        (1600, 4) => vec![('A', phases[0]), ('B', phases[1])],
        (3200, 2) => vec![('A', phases[0]), ('C', phases[2])],
        (3200, 4) => vec![
            ('A', phases[0]),
            ('B', phases[1]),
            ('C', phases[2]),
            ('D', phases[3]),
        ],
        _ => Vec::new(),
    }
}

#[derive(Clone)]
struct CorrectedPhase {
    words: [Option<u32>; WORDS_PER_PHASE],
    errors: [u8; WORDS_PER_PHASE],
}

fn correct_phase(raw: &[u32; WORDS_PER_PHASE], diag: &mut FlexDiagnostics) -> CorrectedPhase {
    let mut words = [None; WORDS_PER_PHASE];
    let mut errors = [0; WORDS_PER_PHASE];
    for (index, &word) in raw.iter().enumerate() {
        match bch_correct(word) {
            Ok((fixed, count)) => {
                account_bch(diag, count);
                words[index] = Some(fixed & 0x1F_FFFF);
                errors[index] = count as u8;
            }
            Err(_) => diag.bch_err += 1,
        }
    }
    CorrectedPhase { words, errors }
}

#[derive(Clone)]
struct PendingFragment {
    text: String,
    updated: Instant,
}

#[derive(Clone, Default)]
struct GroupAssignment {
    cycle: u8,
    frame: u8,
    recipients: Vec<u64>,
}

pub struct FlexDecoder {
    lanes: Vec<Lane>,
    diag: FlexDiagnostics,
    recent_hashes: VecDeque<(u64, Instant)>,
    /// Frames already recovered, so converged lanes are not counted twice.
    recent_frames: VecDeque<(u64, Instant)>,
    fragments: HashMap<(u64, Option<u8>), PendingFragment>,
    groups: HashMap<u8, GroupAssignment>,
}

impl FlexDecoder {
    pub const DEFAULT_BAUD: u32 = 1600;

    pub fn new(fs: f64) -> Self {
        // Six static phases, like POCSAG: the per-lane tracking loop absorbs
        // the residual, and every extra lane costs a full slicer pass on every
        // sample. At 48 kHz that is 16 lanes × 48k = 0.77 M pushes/s saved.
        let phases = ((fs / 1600.0).floor() as usize).clamp(1, 6);
        let lanes = (0..phases).map(|delay| Lane::new(fs, delay)).collect();
        Self {
            lanes,
            diag: FlexDiagnostics::default(),
            recent_hashes: VecDeque::new(),
            recent_frames: VecDeque::new(),
            fragments: HashMap::new(),
            groups: HashMap::new(),
        }
    }

    pub fn reset(&mut self) {
        for lane in &mut self.lanes {
            lane.reset();
        }
        self.recent_hashes.clear();
        self.recent_frames.clear();
        self.fragments.clear();
        self.groups.clear();
    }

    pub fn diagnostics(&self) -> &FlexDiagnostics {
        &self.diag
    }

    /// A decoder that owns no lanes and decodes nothing.
    ///
    /// Used when the caller runs its own FLEX bank over the same
    /// discriminator; sync observations are fed back through
    /// [`FlexDecoder::note_sync`] instead.
    pub fn idle() -> Self {
        Self {
            lanes: Vec::new(),
            diag: FlexDiagnostics::default(),
            recent_hashes: VecDeque::new(),
            recent_frames: VecDeque::new(),
            fragments: HashMap::new(),
            groups: HashMap::new(),
        }
    }

    /// Record a FLEX sync seen by an externally-owned decoder, so a caller
    /// running its own bank can still drive protocol matching here.
    pub fn note_sync(&mut self, baud: u32) {
        match baud {
            3200 => self.diag.syncs_3200 += 1,
            6400 => self.diag.syncs_6400 += 1,
            _ => self.diag.syncs_1600 += 1,
        }
    }

    pub fn process(&mut self, discriminator_hz: &[f32]) -> Vec<FlexMessage> {
        let now = Instant::now();
        self.recent_hashes
            .retain(|(_, seen)| now.duration_since(*seen) < Duration::from_secs(3));
        self.recent_frames
            .retain(|(_, seen)| now.duration_since(*seen) < Duration::from_secs(3));
        self.fragments
            .retain(|_, fragment| now.duration_since(fragment.updated) < FRAGMENT_TTL);
        let mut frames = Vec::new();
        for &sample in discriminator_hz {
            for lane in &mut self.lanes {
                if let Some(frame) = lane.push(sample, &mut self.diag) {
                    frames.push(frame);
                }
            }
        }

        let mut output = Vec::new();
        for frame in frames {
            // Now that each lane tracks its own clock, neighbouring lanes
            // converge on the same sampling instant and recover the *same*
            // air frame. That is the loop working, but it must not be counted
            // or FEC-decoded several times: it would multiply the frame and
            // phase diagnostics by however many lanes happened to lock, and
            // do the Reed-Solomon work once per lane for one transmission.
            let frame_hash = frame_hash(&frame);
            if self
                .recent_frames
                .iter()
                .any(|(known, _)| *known == frame_hash)
            {
                continue;
            }
            self.recent_frames.push_back((frame_hash, now));
            self.diag.frames_decoded += 1;
            for (phase_name, raw) in deinterleave(&frame.symbols, frame.mode) {
                self.diag.phases_decoded += 1;
                let corrected = correct_phase(&raw, &mut self.diag);
                let mut messages = self.decode_phase(
                    frame.fiw,
                    frame.fiw_errors,
                    frame.mode,
                    phase_name,
                    &corrected,
                );
                for mut message in messages.drain(..) {
                    self.reassemble(&mut message, now);
                    let hash = message_hash(&message);
                    if !self.recent_hashes.iter().any(|(known, _)| *known == hash) {
                        self.recent_hashes.push_back((hash, now));
                        output.push(message);
                    }
                }
            }
        }
        output
    }

    fn reassemble(&mut self, message: &mut FlexMessage, now: Instant) {
        let key = (message.capcode, message.message_number);
        match message.fragment {
            FlexFragment::Complete => {}
            FlexFragment::First => {
                self.diag.fragments_started += 1;
                self.fragments.insert(
                    key,
                    PendingFragment {
                        text: message.text.clone(),
                        updated: now,
                    },
                );
            }
            FlexFragment::Middle => {
                if let Some(fragment) = self.fragments.get_mut(&key) {
                    fragment.text.push_str(&message.text);
                    fragment.updated = now;
                    message.text = fragment.text.clone();
                } else {
                    self.diag.fragments_started += 1;
                    self.fragments.insert(
                        key,
                        PendingFragment {
                            text: message.text.clone(),
                            updated: now,
                        },
                    );
                }
            }
            FlexFragment::Continuation => {
                if let Some(mut pending) = self.fragments.remove(&key) {
                    pending.text.push_str(&message.text);
                    message.text = pending.text;
                    message.parsed =
                        Some(crate::pager_parser::parse_pager_text(&message.text, None));
                    message.complete = true;
                    message.reassembled = true;
                    self.diag.fragments_completed += 1;
                }
            }
        }
    }

    fn decode_phase(
        &mut self,
        fiw: u32,
        fiw_errors: u32,
        mode: Mode,
        phase_name: char,
        phase: &CorrectedPhase,
    ) -> Vec<FlexMessage> {
        if !fiw_checksum_ok(fiw) {
            return Vec::new();
        }
        let Some(biw) = phase.words[0] else {
            return Vec::new();
        };
        if biw == 0 || biw == 0x1F_FFFF {
            return Vec::new();
        }
        let address_start = (((biw >> 8) & 3) + 1) as usize;
        let vector_start = ((biw >> 10) & 0x3F) as usize;
        if address_start == 0 || address_start > vector_start || vector_start >= WORDS_PER_PHASE {
            return Vec::new();
        }
        let cycle = ((fiw >> 4) & 0xF) as u8;
        let frame = ((fiw >> 8) & 0x7F) as u8;
        let priority_words = ((biw >> 4) & 0x0F) as usize;
        let mut messages = Vec::new();
        let mut address_index = address_start;
        while address_index < vector_start {
            let Some(aw1) = phase.words[address_index] else {
                address_index += 1;
                continue;
            };
            if aw1 == 0 || aw1 == 0x1F_FFFF {
                address_index += 1;
                continue;
            }
            let vector_index = vector_start + address_index - address_start;
            let Some(viw) = phase.words.get(vector_index).copied().flatten() else {
                break;
            };
            let Some(address) = decode_address(phase, address_index, vector_start) else {
                address_index += 1;
                continue;
            };
            let capcode = address.capcode;
            let long_address = address.long;
            let consumed = address.consumed;
            self.diag.addr_words += consumed as u64;
            let format = FlexFormat::from_vector(viw);
            if format == FlexFormat::ShortInstruction {
                let instruction_type = ((viw >> 7) & 7) as u8;
                let assigned_frame = ((viw >> 10) & 0x7F) as u8;
                let group = ((viw >> 17) & 0x0F) as u8;
                let assigned_cycle = if assigned_frame > frame {
                    cycle
                } else {
                    (cycle + 1) & 0x0F
                };
                let (text, group_recipients) = if instruction_type == 0 {
                    let assignment = self.groups.entry(group).or_default();
                    if assignment.frame != assigned_frame || assignment.cycle != assigned_cycle {
                        assignment.recipients.clear();
                    }
                    assignment.frame = assigned_frame;
                    assignment.cycle = assigned_cycle;
                    if !assignment.recipients.contains(&capcode) {
                        assignment.recipients.push(capcode);
                    }
                    (
                        format!(
                            "[Temporary group {group}: cycle {assigned_cycle}, frame {assigned_frame}]"
                        ),
                        assignment.recipients.clone(),
                    )
                } else if instruction_type == 1 {
                    let flags = [
                        "traffic split SSID",
                        "traffic split NID",
                        "channel setup change",
                        "new NID frequency",
                        "new SSID frequency",
                    ];
                    let events = (viw >> 9) & 0x7ff;
                    let active = flags
                        .iter()
                        .enumerate()
                        .filter(|(bit, _)| events & (1 << bit) != 0)
                        .map(|(_, name)| *name)
                        .collect::<Vec<_>>();
                    (
                        format!(
                            "[System event: {}]",
                            if active.is_empty() {
                                "none".into()
                            } else {
                                active.join(", ")
                            }
                        ),
                        Vec::new(),
                    )
                } else {
                    (
                        format!(
                            "[Reserved instruction type {instruction_type}: 0x{:03X}]",
                            (viw >> 9) & 0x7ff
                        ),
                        Vec::new(),
                    )
                };
                messages.push(FlexMessage {
                    capcode,
                    cycle,
                    frame,
                    phase: phase_name,
                    format,
                    text,
                    parsed: None,
                    baud: mode.baud(),
                    symbol_rate: mode.symbol_rate,
                    levels: mode.levels,
                    long_address,
                    address_type: address.kind.into(),
                    priority: address_index - address_start < priority_words,
                    fragment: FlexFragment::Complete,
                    fragment_number: None,
                    message_number: None,
                    retrieval: None,
                    maildrop: None,
                    payload_checksum_ok: None,
                    secure_subtype: None,
                    complete: true,
                    reassembled: false,
                    fec_corrected: fiw_errors
                        + (address_index..address_index + consumed)
                            .map(|index| u32::from(phase.errors[index]))
                            .sum::<u32>()
                        + u32::from(phase.errors[vector_index]),
                    fec_uncorrectable: 0,
                    group_recipients,
                    raw_words: vec![viw],
                });
                address_index += consumed;
                continue;
            }

            let mut decoded = decode_vector(viw, vector_index, long_address, phase);
            let group_recipients = if (GROUP_MIN..=GROUP_MAX).contains(&capcode) {
                let group = (capcode - GROUP_MIN) as u8;
                self.groups
                    .remove(&group)
                    .filter(|assignment| assignment.cycle == cycle && assignment.frame == frame)
                    .map(|assignment| assignment.recipients)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let address_corrected: u32 = (address_index..address_index + consumed)
                .map(|index| u32::from(phase.errors[index]))
                .sum();
            let corrected = fiw_errors
                + address_corrected
                + u32::from(phase.errors[vector_index])
                + decoded.fec_corrected;
            self.diag.msg_words += decoded.raw_words.len() as u64;
            let parsed = (!decoded.text.is_empty())
                .then(|| crate::pager_parser::parse_pager_text(&decoded.text, None));
            messages.push(FlexMessage {
                capcode,
                cycle,
                frame,
                phase: phase_name,
                format,
                text: std::mem::take(&mut decoded.text),
                parsed,
                baud: mode.baud(),
                symbol_rate: mode.symbol_rate,
                levels: mode.levels,
                long_address,
                address_type: address.kind.into(),
                priority: address_index - address_start < priority_words,
                fragment: decoded.fragment,
                fragment_number: decoded.fragment_number,
                message_number: decoded.message_number,
                retrieval: decoded.retrieval,
                maildrop: decoded.maildrop,
                payload_checksum_ok: decoded.payload_checksum_ok,
                secure_subtype: decoded.secure_subtype,
                complete: decoded.complete,
                reassembled: false,
                fec_corrected: corrected,
                fec_uncorrectable: decoded.fec_uncorrectable,
                group_recipients,
                raw_words: decoded.raw_words,
            });
            address_index += consumed;
        }
        messages
    }
}

struct VectorDecode {
    text: String,
    fragment: FlexFragment,
    fragment_number: Option<u8>,
    message_number: Option<u8>,
    retrieval: Option<bool>,
    maildrop: Option<bool>,
    payload_checksum_ok: Option<bool>,
    secure_subtype: Option<String>,
    complete: bool,
    fec_corrected: u32,
    fec_uncorrectable: u32,
    raw_words: Vec<u32>,
}

struct DecodedAddress {
    capcode: u64,
    kind: &'static str,
    long: bool,
    consumed: usize,
}

fn decode_address(
    phase: &CorrectedPhase,
    index: usize,
    vector_start: usize,
) -> Option<DecodedAddress> {
    let first = phase.words.get(index).copied().flatten()?;
    let short_kind = match first {
        0x008001..=0x1E0000 => Some("Short"),
        0x1F0001..=0x1F27FF => Some("Reserved Short"),
        0x1F2800..=0x1F67FF => Some("Information Service"),
        0x1F6800..=0x1F77FF => Some("Network"),
        0x1F7800..=0x1F780F => Some("Temporary Group"),
        0x1F7810..=0x1F781F => Some("Operator Message"),
        0x1F7820..=0x1F7FFE => Some("Reserved Short"),
        _ => None,
    };
    if let Some(kind) = short_kind {
        return Some(DecodedAddress {
            capcode: u64::from(first - 0x8000),
            kind,
            long: false,
            consumed: 1,
        });
    }

    let long_first = (1..=0x8000).contains(&first) || (0x1F7FFF..=0x1FFFFE).contains(&first);
    if !long_first || index + 1 >= vector_start {
        return None;
    }
    let second = phase.words.get(index + 1).copied().flatten()?;
    let (capcode, kind) = if (1..=0x8000).contains(&first)
        && (0x1F7FFF..=0x1FFFFE).contains(&second)
    {
        (
            u64::from(first) + u64::from(0x1F_FFFF - second) * 32_768 + 2_068_480,
            "Long 1-2",
        )
    } else if (1..=0x8000).contains(&first) && (0x1E0001..=0x1F0000).contains(&second) {
        let set = if second <= 0x1E8000 {
            "Long 1-3"
        } else {
            "Long 1-4"
        };
        (
            u64::from(first) + u64::from(second - 1_933_312) * 32_768 + 2_068_480,
            set,
        )
    } else if (0x1F7FFF..=0x1FFFFE).contains(&first) && (0x1E0001..=0x1F0000).contains(&second) {
        let set = if second <= 0x1E8000 {
            "Long 2-3"
        } else {
            "Long 2-4"
        };
        (
            u64::from(first - 2_064_383) + u64::from(second - 1_867_776) * 32_768 + 2_068_479,
            set,
        )
    } else {
        return None;
    };
    (capcode <= MAX_CAPCODE).then_some(DecodedAddress {
        capcode,
        kind,
        long: true,
        consumed: 2,
    })
}

fn message_window(viw: u32) -> Option<(usize, usize)> {
    let start = ((viw >> 7) & 0x7F) as usize;
    let len = ((viw >> 14) & 0x7F) as usize;
    (start > 0 && len > 0 && start < WORDS_PER_PHASE)
        .then_some((start, len.min(WORDS_PER_PHASE - start)))
}

fn collect_words(phase: &CorrectedPhase, start: usize, len: usize) -> (Vec<u32>, u32, u32) {
    let mut words = Vec::new();
    let mut corrected = 0;
    let mut bad = 0;
    for index in start..start.saturating_add(len).min(WORDS_PER_PHASE) {
        match phase.words[index] {
            Some(word) => {
                words.push(word);
                corrected += u32::from(phase.errors[index]);
            }
            None => {
                bad += 1;
                break;
            }
        }
    }
    (words, corrected, bad)
}

fn decode_vector(
    viw: u32,
    vector_index: usize,
    long: bool,
    phase: &CorrectedPhase,
) -> VectorDecode {
    let format = FlexFormat::from_vector(viw);
    let default = || VectorDecode {
        text: String::new(),
        fragment: FlexFragment::Complete,
        fragment_number: None,
        message_number: None,
        retrieval: None,
        maildrop: None,
        payload_checksum_ok: None,
        secure_subtype: None,
        complete: true,
        fec_corrected: 0,
        fec_uncorrectable: 0,
        raw_words: Vec::new(),
    };
    match format {
        FlexFormat::ToneOnly => {
            let subtype = (viw >> 7) & 3;
            let mut text = if subtype == 0 {
                let mut value = String::new();
                for shift in [9, 13, 17] {
                    value.push(NUMERIC[((viw >> shift) & 0xF) as usize]);
                }
                if long && let Some(extra) = phase.words.get(vector_index + 1).copied().flatten() {
                    for shift in [0, 4, 8, 12, 16] {
                        value.push(NUMERIC[((extra >> shift) & 0xF) as usize]);
                    }
                }
                format!("[Short Numeric] {}", value.trim_end())
            } else {
                "[Tone Only]".to_string()
            };
            if text == "[Short Numeric]" {
                text = "[Tone Only]".into();
            }
            VectorDecode {
                text,
                raw_words: vec![viw],
                ..default()
            }
        }
        FlexFormat::StandardNumeric | FlexFormat::SpecialNumeric | FlexFormat::NumberedNumeric => {
            let start = ((viw >> 7) & 0x7f) as usize;
            if start == 0 || start >= WORDS_PER_PHASE {
                return default();
            }
            // Numeric vectors use a three-bit count encoded as n-1; bits
            // 17..20 are checksum, not an extension of the word count.
            let len = (((viw >> 14) & 7) + 1) as usize;
            let (mut words, corrected, bad) = if long {
                let mut words = Vec::new();
                let mut corrected = 0;
                let mut bad = 0;
                match phase.words.get(vector_index + 1).copied().flatten() {
                    Some(word) => {
                        words.push(word);
                        corrected += u32::from(phase.errors[vector_index + 1]);
                    }
                    None => bad += 1,
                }
                let (tail, tail_corrected, tail_bad) =
                    collect_words(phase, start, len.saturating_sub(1));
                words.extend(tail);
                corrected += tail_corrected;
                bad += tail_bad;
                (words, corrected, bad)
            } else {
                collect_words(phase, start, len)
            };
            words.truncate(len);
            let text = decode_numeric_stream(&words, format == FlexFormat::NumberedNumeric);
            let header = words.first().copied();
            let checksum_ok = numeric_checksum_ok(viw, &words);
            VectorDecode {
                text,
                fragment: header
                    .map(fragment_from_alpha_header)
                    .unwrap_or(FlexFragment::Complete),
                fragment_number: header.map(|word| ((word >> 11) & 3) as u8),
                message_number: header.map(|word| ((word >> 13) & 0x3f) as u8),
                retrieval: header.map(|word| word >> 19 & 1 != 0),
                maildrop: header.map(|word| word >> 20 & 1 != 0),
                payload_checksum_ok: Some(checksum_ok),
                complete: bad == 0 && checksum_ok && header.is_some_and(|word| word >> 10 & 1 == 0),
                fec_corrected: corrected,
                fec_uncorrectable: bad,
                raw_words: words,
                ..default()
            }
        }
        FlexFormat::Alphanumeric | FlexFormat::Secure => {
            let Some((start, len)) = message_window(viw) else {
                return default();
            };
            let (mut words, mut corrected, mut bad) =
                collect_words(phase, start, if long { len.saturating_sub(1) } else { len });
            let header = if long {
                match phase.words.get(vector_index + 1).copied().flatten() {
                    Some(header) => {
                        corrected += u32::from(phase.errors[vector_index + 1]);
                        header
                    }
                    None => {
                        bad += 1;
                        0
                    }
                }
            } else {
                words.first().copied().unwrap_or_else(|| {
                    bad += 1;
                    0
                })
            };
            if !long && !words.is_empty() {
                words.remove(0);
            }
            let fragment_number = ((header >> 11) & 3) as u8;
            let fragment = fragment_from_alpha_header(header);
            let secure_type = (header >> 19) & 3;
            let decode_as_alpha = format == FlexFormat::Alphanumeric || secure_type == 0;
            let (text, signature_ok) = if decode_as_alpha {
                decode_alpha_words_checked(&words, fragment_number == 3)
            } else {
                (
                    format!(
                        "[Secure payload] {}",
                        words
                            .iter()
                            .map(|word| format!("{word:06X}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ),
                    None,
                )
            };
            let checksum_ok = alpha_checksum_ok(header, &words);
            let secure_subtype = (format == FlexFormat::Secure).then(|| {
                match (header >> 19) & 3 {
                    0 => "Alpha",
                    1 => "Vendor-defined",
                    2 => "Binary",
                    _ => "Reserved",
                }
                .into()
            });
            VectorDecode {
                text,
                fragment,
                fragment_number: Some(fragment_number),
                message_number: Some(((header >> 13) & 0x3f) as u8),
                retrieval: Some(header >> 19 & 1 != 0),
                maildrop: Some(header >> 20 & 1 != 0),
                payload_checksum_ok: Some(checksum_ok && signature_ok.unwrap_or(true)),
                secure_subtype,
                complete: fragment == FlexFragment::Complete
                    && bad == 0
                    && checksum_ok
                    && signature_ok.unwrap_or(true),
                fec_corrected: corrected,
                fec_uncorrectable: bad,
                raw_words: words,
            }
        }
        FlexFormat::Binary => {
            let Some((start, len)) = message_window(viw) else {
                return default();
            };
            let (mut words, mut corrected, mut bad) =
                collect_words(phase, start, if long { len.saturating_sub(1) } else { len });
            let header = if long {
                phase.words.get(vector_index + 1).copied().flatten()
            } else {
                phase.words.get(start).copied().flatten()
            };
            if !long && !words.is_empty() {
                words.remove(0);
            }
            if header.is_none() {
                bad += 1;
            } else {
                corrected += u32::from(phase.errors[if long { vector_index + 1 } else { start }]);
            }
            let hex = words
                .iter()
                .map(|word| format!("{word:06X}"))
                .collect::<Vec<_>>()
                .join(" ");
            let label = "Binary";
            let fragment = header
                .map(fragment_from_binary_header)
                .unwrap_or(FlexFragment::Complete);
            VectorDecode {
                text: format!("[{label} payload] {hex}"),
                fragment,
                fragment_number: header.map(|word| ((word >> 13) & 3) as u8),
                message_number: header.map(|word| ((word >> 15) & 0x3f) as u8),
                complete: bad == 0 && fragment == FlexFragment::Complete,
                fec_corrected: corrected,
                fec_uncorrectable: bad,
                raw_words: words,
                ..default()
            }
        }
        FlexFormat::ShortInstruction => default(),
    }
}

fn fragment_from_alpha_header(header: u32) -> FlexFragment {
    let number = ((header >> 11) & 3) as u8;
    let continued = header >> 10 & 1 != 0;
    match (number == 3, continued) {
        (true, false) => FlexFragment::Complete,
        (true, true) => FlexFragment::First,
        (false, true) => FlexFragment::Middle,
        (false, false) => FlexFragment::Continuation,
    }
}

fn fragment_from_binary_header(header: u32) -> FlexFragment {
    let number = ((header >> 13) & 3) as u8;
    let continued = header >> 12 & 1 != 0;
    match (number == 3, continued) {
        (true, false) => FlexFragment::Complete,
        (true, true) => FlexFragment::First,
        (false, true) => FlexFragment::Middle,
        (false, false) => FlexFragment::Continuation,
    }
}

fn alpha_checksum_ok(header: u32, words: &[u32]) -> bool {
    let mut sum = word_group_sum(header & !0x3ff);
    for &word in words {
        sum += word_group_sum(word);
    }
    header & 0x3ff == (!sum & 0x3ff)
}

fn numeric_checksum_ok(vector: u32, words: &[u32]) -> bool {
    let Some((&first, rest)) = words.split_first() else {
        return false;
    };
    let mut sum = word_group_sum(first & !3);
    for &word in rest {
        sum += word_group_sum(word);
    }
    let folded = sum & 0xff;
    let expected = (!((folded & 0x3f) + (folded >> 6))) & 0x3f;
    let received = ((first & 3) << 4) | ((vector >> 17) & 0x0f);
    received == expected
}

fn word_group_sum(word: u32) -> u32 {
    (word & 0xff) + ((word >> 8) & 0xff) + ((word >> 16) & 0x1f)
}

fn decode_alpha_words_checked(words: &[u32], skip_signature: bool) -> (String, Option<bool>) {
    let mut bytes = Vec::new();
    let mut signature = None;
    let mut character_sum = 0u32;
    for (word_index, &word) in words.iter().enumerate() {
        for char_index in 0..3 {
            if word_index == 0 && char_index == 0 && skip_signature {
                signature = Some((word & 0x7f) as u8);
                continue;
            }
            let byte = ((word >> (char_index * 7)) & 0x7F) as u8;
            if byte == 0x03 {
                continue;
            }
            character_sum += u32::from(byte);
            if byte == b'\n' || byte == b'\r' || byte == b'\t' || (0x20..=0x7E).contains(&byte) {
                bytes.push(byte);
            }
        }
    }
    (
        String::from_utf8_lossy(&bytes).trim_end().to_string(),
        signature.map(|received| received == ((!character_sum) & 0x7f) as u8),
    )
}

#[cfg(test)]
fn decode_alpha_words(words: &[u32], skip_signature: bool) -> String {
    decode_alpha_words_checked(words, skip_signature).0
}

fn decode_numeric_stream(words: &[u32], numbered: bool) -> String {
    let Some((&first, rest)) = words.split_first() else {
        return String::new();
    };
    let mut stream = Vec::with_capacity(words.len() * 21);
    for bit in 0..21 {
        stream.push(((first >> bit) & 1) as u8);
    }
    for &word in rest {
        for bit in 0..21 {
            stream.push(((word >> bit) & 1) as u8);
        }
    }
    let skip = if numbered { 10 } else { 2 };
    let mut output = String::new();
    for nibble in stream
        .into_iter()
        .skip(skip)
        .collect::<Vec<_>>()
        .chunks_exact(4)
    {
        let value = nibble
            .iter()
            .enumerate()
            .fold(0u8, |acc, (bit, &set)| acc | (set << bit));
        if value != 0xC {
            output.push(NUMERIC[value as usize]);
        }
    }
    output.trim_end().to_string()
}

/// Identity of one recovered air frame, so several lanes reporting it are
/// recognised as one transmission.
fn frame_hash(frame: &RawFrame) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut state = std::collections::hash_map::DefaultHasher::new();
    frame.fiw.hash(&mut state);
    frame.symbols.hash(&mut state);
    state.finish()
}

fn message_hash(message: &FlexMessage) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut state = std::collections::hash_map::DefaultHasher::new();
    message.capcode.hash(&mut state);
    message.cycle.hash(&mut state);
    message.frame.hash(&mut state);
    message.phase.hash(&mut state);
    message.format.hash(&mut state);
    message.text.hash(&mut state);
    state.finish()
}

impl std::hash::Hash for FlexFormat {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (*self as u8).hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fiw(cycle: u8, frame: u8) -> u32 {
        let body = (u32::from(cycle) << 4) | (u32::from(frame) << 8);
        let partial = ((body >> 4) & 0xF)
            + ((body >> 8) & 0xF)
            + ((body >> 12) & 0xF)
            + ((body >> 16) & 0xF)
            + ((body >> 20) & 1);
        encode_word(body | (0xFu32.wrapping_sub(partial) & 0xF))
    }

    fn alpha_words(text: &str, fragment: u8, continued: bool) -> Vec<u32> {
        let signature = (!text.bytes().map(u32::from).sum::<u32>() & 0x7f) as u8;
        let mut bytes = vec![signature];
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(0x03);
        let mut content = Vec::new();
        for chunk in bytes.chunks(3) {
            let mut word = 0;
            for (index, &byte) in chunk.iter().enumerate() {
                word |= u32::from(byte & 0x7F) << (index * 7);
            }
            content.push(word);
        }
        let mut header = (u32::from(fragment) << 11) | (u32::from(continued) << 10);
        let sum = word_group_sum(header) + content.iter().copied().map(word_group_sum).sum::<u32>();
        header |= !sum & 0x3ff;
        let mut out = vec![header];
        out.extend(content);
        out
    }

    fn discriminator_frame(
        mode_code: u16,
        page_phase: usize,
        cycle: u8,
        frame: u8,
        capcode: u32,
        text: &str,
    ) -> Vec<f32> {
        discriminator_frame_at(16_000, mode_code, page_phase, cycle, frame, capcode, text)
    }

    /// Same frame rendered at an arbitrary audio rate, so tests can ask what
    /// samples per symbol the fast FLEX modes actually need.
    fn discriminator_frame_at(
        fs: usize,
        mode_code: u16,
        page_phase: usize,
        cycle: u8,
        frame: u8,
        capcode: u32,
        text: &str,
    ) -> Vec<f32> {
        let mode = Mode::from_a_code(mode_code).unwrap();
        let sync =
            (u64::from(mode_code) << 48) | (u64::from(SYNC_MARKER) << 16) | u64::from(!mode_code);
        let mut symbols = Vec::new();
        for bit in (0..64).rev() {
            // Sync detection intentionally uses the sign convention opposite
            // to FIW/data rectification.
            symbols.push(if sync >> bit & 1 != 0 { 0 } else { 3 });
        }
        symbols.extend((0..16).map(|i| if i & 1 == 0 { 0 } else { 3 }));
        let frame_word = fiw(cycle, frame);
        for bit in 0..32 {
            symbols.push(if frame_word >> bit & 1 != 0 { 3 } else { 0 });
        }

        let body = alpha_words(text, 3, false);
        let mut phases = [[encode_word(0); WORDS_PER_PHASE]; 4];
        let phase = &mut phases[page_phase];
        phase[0] = encode_word(2 << 10); // address offset 1, vector offset 2
        phase[1] = encode_word(0x8000 + capcode);
        phase[2] = encode_word((5 << 4) | (3 << 7) | ((body.len() as u32) << 14));
        for (index, word) in body.into_iter().enumerate() {
            phase[3 + index] = encode_word(word);
        }
        let mut data_symbols = Vec::new();
        for counter in 0..(WORDS_PER_PHASE * 32) as u32 {
            let index = phase_index(counter);
            let word_bit = (counter >> 3) & 31;
            let symbol_for = |a_phase: usize, b_phase: usize| {
                let a = ((phases[a_phase][index] >> word_bit) & 1) as u8;
                let b = ((phases[b_phase][index] >> word_bit) & 1) as u8;
                if mode.levels == 2 {
                    if a != 0 { 3 } else { 0 }
                } else {
                    match (a, b) {
                        (0, 0) => 0,
                        (0, 1) => 1,
                        (1, 1) => 2,
                        _ => 3,
                    }
                }
            };
            data_symbols.push(symbol_for(0, 1));
            if mode.symbol_rate == 3200 {
                data_symbols.push(symbol_for(2, 3));
            }
        }

        let levels = [-4_500.0, -1_500.0, 1_500.0, 4_500.0];
        let mut samples: Vec<f32> = symbols
            .into_iter()
            .flat_map(|symbol| std::iter::repeat_n(levels[symbol], fs / 1600))
            .collect();
        let samples_per_data_symbol = fs / mode.symbol_rate as usize;
        let sync2_symbols = mode.symbol_rate as usize * SYNC2_MS / 1000;
        samples.extend(std::iter::repeat_n(
            levels[0],
            sync2_symbols * samples_per_data_symbol,
        ));
        samples.extend(
            data_symbols
                .into_iter()
                .flat_map(|symbol| std::iter::repeat_n(levels[symbol], samples_per_data_symbol)),
        );
        samples
    }

    #[test]
    fn bch_corrects_every_single_and_a_double_error() {
        let word = encode_word(0x12_3456);
        assert!(valid_word(word));
        for bit in 0..32 {
            assert_eq!(bch_correct(word ^ (1 << bit)).unwrap().0, word);
        }
        assert_eq!(bch_correct(word ^ (1 << 4) ^ (1 << 27)).unwrap().0, word);
    }

    #[test]
    fn all_air_modes_are_recognized() {
        for (code, symbol_rate, levels, baud) in [
            (A_1600_2, 1600, 2, 1600),
            (A_1600_4, 1600, 4, 3200),
            (A_3200_2, 3200, 2, 3200),
            (A_3200_4, 3200, 4, 6400),
            (A_3200_4_ALT, 3200, 4, 6400),
        ] {
            let raw = (u64::from(code) << 48) | (u64::from(SYNC_MARKER) << 16) | u64::from(!code);
            let (mode, inverted) = sync_mode(raw).unwrap();
            assert_eq!(
                (mode.symbol_rate, mode.levels, mode.baud()),
                (symbol_rate, levels, baud)
            );
            assert!(!inverted);
            assert_eq!(sync_mode(!raw).unwrap(), (mode, true));
        }
    }

    #[test]
    fn deinterleave_routes_a_c_for_3200_two_level() {
        let mode = Mode {
            symbol_rate: 3200,
            levels: 2,
        };
        let mut symbols = vec![0u8; 5632];
        symbols[0] = 3;
        symbols[1] = 3;
        let phases = deinterleave(&symbols, mode);
        assert_eq!(phases.iter().map(|p| p.0).collect::<Vec<_>>(), ['A', 'C']);
        assert_ne!(phases[0].1[0], 0);
        assert_ne!(phases[1].1[0], 0);
    }

    #[test]
    fn alpha_header_and_signature_do_not_leak_into_text() {
        let words = alpha_words("DISPATCH TEST", 3, false);
        assert_eq!(decode_alpha_words(&words[1..], true), "DISPATCH TEST");
    }

    #[test]
    fn payload_checksum_failure_is_visible_without_losing_text() {
        let mut encoded = alpha_words("CHECK ME", 3, false);
        encoded[0] ^= 1;
        let mut phase = CorrectedPhase {
            words: [Some(0); WORDS_PER_PHASE],
            errors: [0; WORDS_PER_PHASE],
        };
        for (index, word) in encoded.iter().copied().enumerate() {
            phase.words[10 + index] = Some(word);
        }
        let viw = (5 << 4) | (10 << 7) | ((encoded.len() as u32) << 14);
        let decoded = decode_vector(viw, 2, false, &phase);
        assert_eq!(decoded.text, "CHECK ME");
        assert_eq!(decoded.payload_checksum_ok, Some(false));
        assert!(!decoded.complete);
    }

    #[test]
    fn one_word_numeric_vector_is_not_dropped_when_n_is_zero() {
        let mut phase = CorrectedPhase {
            words: [Some(0); WORDS_PER_PHASE],
            errors: [0; WORDS_PER_PHASE],
        };
        phase.words[10] = Some(0x12345);
        let viw = (3 << 4) | (10 << 7);
        let decoded = decode_vector(viw, 2, false, &phase);
        assert_eq!(decoded.raw_words, vec![0x12345]);
    }

    #[test]
    fn real_biw_vector_layout_decodes_a_page() {
        let mode = Mode {
            symbol_rate: 1600,
            levels: 2,
        };
        let mut phase = CorrectedPhase {
            words: [Some(0); WORDS_PER_PHASE],
            errors: [0; WORDS_PER_PHASE],
        };
        let body = alpha_words("FULL FLEX MESSAGE", 3, false);
        let aoff = 1usize;
        let voff = 2usize;
        let start = 3usize;
        phase.words[0] = Some((aoff as u32 - 1) << 8 | (voff as u32) << 10);
        phase.words[1] = Some(0x8000 + 123_456);
        phase.words[2] = Some((5 << 4) | ((start as u32) << 7) | ((body.len() as u32) << 14));
        for (index, word) in body.into_iter().enumerate() {
            phase.words[start + index] = Some(word);
        }
        let mut decoder = FlexDecoder::new(16_000.0);
        let pages = decoder.decode_phase(fiw(2, 77), 0, mode, 'A', &phase);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].capcode, 123_456);
        assert_eq!(pages[0].text, "FULL FLEX MESSAGE");
        assert_eq!(pages[0].format, FlexFormat::Alphanumeric);
        assert_eq!(pages[0].address_type, "Short");
        assert_eq!(pages[0].payload_checksum_ok, Some(true));
    }

    #[test]
    fn every_long_address_set_and_special_short_range_is_classified() {
        let mut phase = CorrectedPhase {
            words: [Some(0); WORDS_PER_PHASE],
            errors: [0; WORDS_PER_PHASE],
        };
        for (first, second, kind) in [
            (1, 0x1F_FFFE, "Long 1-2"),
            (1, 0x1E_0001, "Long 1-3"),
            (1, 0x1E_8001, "Long 1-4"),
            (0x1F_7FFF, 0x1E_0001, "Long 2-3"),
        ] {
            phase.words[1] = Some(first);
            phase.words[2] = Some(second);
            let decoded = decode_address(&phase, 1, 3).expect(kind);
            assert_eq!(decoded.kind, kind);
            assert!(decoded.long);
            assert_eq!(decoded.consumed, 2);
            assert!((1..=MAX_CAPCODE).contains(&decoded.capcode));
        }
        // The nominal 2-4 set begins one above FLEX's maximum capcode and is
        // therefore correctly rejected by the final range gate.
        phase.words[1] = Some(0x1F_7FFF);
        phase.words[2] = Some(0x1E_8001);
        assert!(decode_address(&phase, 1, 3).is_none());
        for (word, kind) in [
            (0x8001, "Short"),
            (0x1F2800, "Information Service"),
            (0x1F6800, "Network"),
            (0x1F7800, "Temporary Group"),
            (0x1F7810, "Operator Message"),
        ] {
            phase.words[1] = Some(word);
            let decoded = decode_address(&phase, 1, 3).expect(kind);
            assert_eq!(decoded.kind, kind);
            assert!(!decoded.long);
        }
    }

    #[test]
    fn complete_discriminator_frame_decodes_end_to_end() {
        let audio = discriminator_frame(A_1600_2, 0, 6, 91, 654_321, "END TO END FLEX");
        let mut decoder = FlexDecoder::new(16_000.0);
        let pages = decoder.process(&audio);
        let page = pages
            .iter()
            .find(|page| page.capcode == 654_321)
            .expect("page decoded from discriminator samples");
        assert_eq!(page.text, "END TO END FLEX");
        assert_eq!((page.cycle, page.frame, page.phase), (6, 91, 'A'));
        assert_eq!((page.baud, page.symbol_rate, page.levels), (1600, 1600, 2));
        assert!(page.complete);
        assert_eq!(decoder.diagnostics().frames_decoded, 1);
    }

    /// FLEX runs 1600-6400 sym/s in one stream, so the faster the mode the
    /// less absolute timing error a frame survives. A fixed-phase bank alone
    /// held only while the accumulated error stayed inside a fraction of a
    /// symbol — about 57 ppm across a batch — which is tighter than any real
    /// transmitter clock. The loop widens that by roughly seven times at
    /// 16 kHz, which is where it needs to be once the chain itself stops
    /// lying about its rate.
    ///
    /// The ceiling is samples per symbol rather than the loop, which is why
    /// this runs at the rate the pager channel actually uses: 6400 sym/s at
    /// 16 kHz is 2.5 samples a symbol and the four-level modes give out
    /// first, while at 32 kHz every mode holds across the range.
    #[test]
    fn a_clock_offset_still_decodes_in_every_air_mode() {
        for (mode, phase) in [(A_1600_2, 0), (A_1600_4, 1), (A_3200_2, 2), (A_3200_4, 3)] {
            for ppm in [-200i32, -100, 100, 200] {
                let audio =
                    discriminator_frame_at(32_000, mode, phase, 3, 27, 456_789, "CLOCK PULL");
                // The samples were rendered at 16 kHz; the decoder is built
                // for a slightly different clock, which is the same thing as
                // a transmitter whose baud rate is off by that much.
                let mut decoder = FlexDecoder::new(32_000.0 / (1.0 + f64::from(ppm) * 1e-6));
                let pages = decoder.process(&audio);
                assert!(
                    pages
                        .iter()
                        .any(|page| page.capcode == 456_789 && page.text == "CLOCK PULL"),
                    "mode {mode:?} phase {phase} at {ppm} ppm decoded nothing"
                );
                assert_eq!(
                    decoder.diagnostics().frames_decoded,
                    1,
                    "mode {mode:?} at {ppm} ppm counted converged lanes as separate frames"
                );
            }
        }
    }

    /// Four samples a symbol is where the timing loop starts to work well.
    /// At 16 kHz the 3200 sym/s four-level mode has five and 6400 has 2.5,
    /// which is why the pager channel does not run at the nominal audio rate.
    #[test]
    fn the_fast_four_level_mode_wants_more_samples_per_symbol() {
        let tolerant = |fs: usize| {
            [-800i32, 800].iter().all(|&ppm| {
                let audio = discriminator_frame_at(fs, A_3200_4, 3, 3, 27, 456_789, "CLOCK PULL");
                let mut decoder = FlexDecoder::new(fs as f64 / (1.0 + f64::from(ppm) * 1e-6));
                decoder
                    .process(&audio)
                    .iter()
                    .any(|page| page.capcode == 456_789)
            })
        };
        assert!(!tolerant(16_000), "16 kHz was expected to be the tight one");
        assert!(
            tolerant(32_000),
            "32 kHz should carry this mode comfortably"
        );
    }

    #[test]
    fn every_flex_air_mode_and_phase_decodes_end_to_end() {
        for (mode, phase, phase_name) in [
            (A_1600_2, 0, 'A'),
            (A_1600_4, 1, 'B'),
            (A_3200_2, 2, 'C'),
            (A_3200_4, 3, 'D'),
        ] {
            let audio = discriminator_frame(mode, phase, 3, 27, 456_789, "ALL MODES");
            let mut decoder = FlexDecoder::new(16_000.0);
            let pages = decoder.process(&audio);
            let page = pages
                .iter()
                .find(|page| page.capcode == 456_789)
                .unwrap_or_else(|| panic!("mode {mode:04X} phase {phase_name}: {pages:?}"));
            assert_eq!(page.text, "ALL MODES");
            assert_eq!(page.phase, phase_name);
        }
    }

    #[test]
    fn fragments_are_reassembled_without_losing_characters() {
        let mut decoder = FlexDecoder::new(16_000.0);
        let now = Instant::now();
        let base = |text: &str, fragment, complete| FlexMessage {
            capcode: 42,
            cycle: 1,
            frame: 1,
            phase: 'A',
            format: FlexFormat::Alphanumeric,
            text: text.into(),
            parsed: None,
            baud: 3200,
            symbol_rate: 1600,
            levels: 4,
            long_address: false,
            address_type: "Short".into(),
            priority: false,
            fragment,
            fragment_number: Some(0),
            message_number: Some(1),
            retrieval: None,
            maildrop: None,
            payload_checksum_ok: None,
            secure_subtype: None,
            complete,
            reassembled: false,
            fec_corrected: 0,
            fec_uncorrectable: 0,
            group_recipients: Vec::new(),
            raw_words: Vec::new(),
        };
        let mut first = base("STRUCTURE FIRE ", FlexFragment::First, false);
        decoder.reassemble(&mut first, now);
        let mut last = base("AT ELM ST", FlexFragment::Continuation, false);
        decoder.reassemble(&mut last, now);
        assert_eq!(last.text, "STRUCTURE FIRE AT ELM ST");
        assert!(last.complete && last.reassembled);
    }

    #[test]
    fn three_part_fragments_accumulate_middle_content() {
        let mut decoder = FlexDecoder::new(16_000.0);
        let now = Instant::now();
        let make = |text: &str, fragment| FlexMessage {
            capcode: 9001,
            cycle: 1,
            frame: 1,
            phase: 'A',
            format: FlexFormat::Alphanumeric,
            text: text.into(),
            parsed: None,
            baud: 1600,
            symbol_rate: 1600,
            levels: 2,
            long_address: false,
            address_type: "Short".into(),
            priority: false,
            fragment,
            fragment_number: Some(1),
            message_number: Some(23),
            retrieval: Some(true),
            maildrop: Some(false),
            payload_checksum_ok: Some(true),
            secure_subtype: None,
            complete: false,
            reassembled: false,
            fec_corrected: 0,
            fec_uncorrectable: 0,
            group_recipients: Vec::new(),
            raw_words: Vec::new(),
        };
        let mut first = make("ALPHA ", FlexFragment::First);
        decoder.reassemble(&mut first, now);
        let mut middle = make("BRAVO ", FlexFragment::Middle);
        decoder.reassemble(&mut middle, now);
        assert_eq!(middle.text, "ALPHA BRAVO ");
        assert!(!middle.complete);
        let mut final_part = make("CHARLIE", FlexFragment::Continuation);
        decoder.reassemble(&mut final_part, now);
        assert_eq!(final_part.text, "ALPHA BRAVO CHARLIE");
        assert!(final_part.complete && final_part.reassembled);
    }
}
