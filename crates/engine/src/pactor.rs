//! PACTOR-I, listened to from outside the link.
//!
//! PACTOR-I (DL6MAA and DF4KV, 1990) is 200 Hz shift FSK at 100 or 200
//! baud in a fixed 1.25 s cycle: the information sending station (ISS)
//! sends a 0.96 s packet, the receiving station (IRS) answers with a 12-bit
//! control signal (CS1 to CS4, always at 100 baud), and the rest of the
//! cycle covers propagation and switching. A packet is a header byte (55 or
//! AA hex, inverted for every packet that carries new information), 8 data
//! bytes at 100 baud or 20 at 200, a status byte (packet counter, data
//! format, break-in and QRT bits) and a CRC-16 over everything after the
//! header, as X.25 computes it. Every byte is sent least significant bit
//! first, and each station inverts its shift with every transmission, so
//! neither mark nor space means anything and every packet is tried both
//! ways up.
//!
//! The decoder works in four layers:
//!
//! 1. **Tones.** The audio is mixed down around 1500 Hz and cut to about
//!    3.2 kHz. An averaged spectrum is searched for two tones 200 Hz apart
//!    with centers anywhere from 600 to 2400 Hz; the pair must be found in
//!    three evaluations in a row. Once it is, each tone is mixed to DC and
//!    summed over one bit at both speeds, and the two tone levels become a
//!    log-likelihood ratio per sample through the RTTY decoder's selective
//!    fading correction (`Atc`). The last few seconds of baseband are kept
//!    and run through again at lock, so what was sent while the pair was
//!    being found is not lost.
//! 2. **Packets.** Every sample is tried as the start of a packet: a header
//!    that alternates (at either speed), CS3 at the head of a break-in
//!    packet, or a header followed by eight callsign characters (a connect
//!    frame). A candidate is read at the bit centers and kept only if its
//!    CRC checks, either way up. Neighboring samples pass for the same
//!    packet; the one with the widest eye is kept.
//! 3. **Cycle.** A packet only counts once its timing has been confirmed: a
//!    second packet a whole number of cycles later (1.25 s, 1.4 s for long
//!    path, 0.96 s for unproto), or the same packet again. From then on the
//!    ISS's next packet is expected at a known instant. When one is missed,
//!    its soft bits are taken anyway at that instant and summed with the
//!    copies before it that were shaped like it (memory ARQ, as the
//!    protocol intends): repeats of a packet are summed until the sum passes
//!    the CRC. Nothing that has not passed the CRC is ever printed.
//! 4. **Link.** The status byte's counter marks repeats, which print once.
//!    A break-in packet, or packets arriving at a different phase of the
//!    cycle, mean the stations have swapped roles. A 200 baud packet
//!    rejected with CS4 is sent again at 100 baud under the same counter, so
//!    what the listener already printed from it is not printed twice. The
//!    connect frame names the called station; the caller's own call is
//!    taken from the first line it sends, which the protocol fills with its
//!    level and callsign. A packet with the QRT bit ends the link and names
//!    the station it is addressed to.
//!
//! Data formats: 8-bit ASCII, the fixed PACTOR Huffman code, and the same
//! code with upper and lower case swapped (format 2, which later SCS
//! firmware sends for text that is mostly capitals; the 1990 description
//! leaves the value unassigned). Format 3 is not printed. Control signals
//! are not decoded: the speed is read from the packets themselves.
//!
//! PACTOR-II, -III and -IV are proprietary and unpublished. They are not
//! decoded, and the decoder does not claim to recognize them.

use crate::rtty::{Atc, Lines};
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::collections::VecDeque;
use std::f32::consts::{PI, TAU};
use std::sync::Arc;

/// Keeps divisions by a silent channel finite.
const TINY: f32 = 1e-30;
/// The rate the decoder works at, roughly: an integer fraction of the input.
const WORK_RATE: f32 = 3_200.0;
/// The audio is mixed down around this frequency.
const MIX_HZ: f32 = 1_500.0;
/// Half the baseband kept: tones from 500 to 2500 Hz and their sidebands.
const PASS_HZ: f32 = 1_250.0;
/// Where the pair's center may be.
const CENTER_LO_HZ: f32 = 600.0;
const CENTER_HI_HZ: f32 = 2_400.0;
/// The tones sit this far either side of the center.
const HALF_SHIFT_HZ: f32 = 100.0;
/// Baseband kept, as a power of two, in work samples (about five seconds).
const RING: usize = 1 << 14;
const MASK: usize = RING - 1;
/// Baseband run through the demodulator again at lock.
const HISTORY_S: f64 = 3.0;
/// Soft bits are capped here, so one very clean copy cannot drown a
/// confident disagreement from the others in a memory-ARQ sum.
const LLR_MAX: f32 = 30.0;

// The protocol.

/// Packet length, whatever the speed.
const PACKET_S: f64 = 0.96;
/// Cycle lengths: ARQ, long-path ARQ, and unproto (packets back to back).
const CYCLE_S: f64 = 1.25;
const LONGPATH_CYCLE_S: f64 = 1.40;
const UNPROTO_CYCLE_S: f64 = 0.96;
/// The break-in control signal, which heads a changeover packet. Bits are
/// sent least significant first.
const CS3: u32 = 0x34b;
/// The idle character, sent where there is nothing to say.
const IDLE: u8 = 0x1e;
/// Status byte bits.
const ST_COUNTER: u8 = 0x03;
const ST_QRT: u8 = 0x80;

// Timing.

/// How far a packet may sit from where the cycle puts it: the timing a
/// candidate is read at, multipath and fading pull it a millisecond or two.
const PHASE_TOL_S: f64 = 0.005;
/// Clock error allowed between the stations' cycle and this receiver's
/// sample clock, per cycle: the stations are within 15 ppm, a sound card
/// within 100 ppm or so.
const DRIFT: f64 = 300e-6;
/// The furthest a packet is still matched to an old cycle, however many
/// cycles have passed. A changeover moves the cycle by at least 0.12 s.
const PHASE_TOL_MAX_S: f64 = 0.04;
/// An unconfirmed packet waits this long for a second one to vouch for it.
const PROVISIONAL_S: f64 = 10.0;
/// The most cycles between two packets that confirm each other.
const CONFIRM_CYCLES: i64 = 8;
/// Missed packets in a row before the cycle is no longer predicted.
const LOCK_LOSS_SLOTS: u32 = 12;
/// Nothing heard on a link for this long ends it.
const LINK_IDLE_S: f64 = 60.0;
/// After a QRT, repeats of it are absorbed for this long.
const QRT_HOLD_S: f64 = 10.0;
/// The caller's first line is waited for this long after a connect.
const IDENT_WAIT_S: f64 = 15.0;
/// Text pending this long without a new character is let go.
const LINE_FLUSH_S: f64 = 3.0;
/// The same while the cycle is followed and packets are expected.
const LINE_HOLD_S: f64 = 15.0;
/// Two copies are summed when their soft bits correlate at least this
/// well. Copies of one packet at a bit error rate of 15% correlate around
/// 0.5; different packets near 0 ± 0.1.
const MARQ_CORR: f32 = 0.25;

// Finding the pair.

/// Averaging weight of each new spectrum: about a second at 80 ms a frame.
const SPEC_ALPHA: f32 = 0.1;
/// Frames averaged before a pair may be claimed.
const MIN_FRAMES: u32 = 8;
/// Mean excess of the weaker tone window over the noise floor, in units of
/// the floor, to acquire and to hold a pair.
const ACQUIRE_RHO: f32 = 0.7;
const HOLD_RHO: f32 = 0.25;
/// Evaluations in a row that must find the same pair before locking.
const CONFIRM: u32 = 3;
/// A pair absent this long is given up and the search resumes.
const HOLD_S: f32 = 10.0;
/// Half the width of the window each tone is measured in. Both speeds have
/// a whole-number modulation index, so most of each tone's power is in a
/// line at the tone itself.
const TONE_HALF_HZ: f32 = 25.0;
/// The weaker tone may be this far below the stronger, in power.
const MAX_TILT: f32 = 0.03;
/// Bandwidth the reported SNR is referred to, as WSJT does.
const SNR_REF_HZ: f32 = 2_500.0;

// ---------------------------------------------------------------------------
// Public types.

/// Which station of a link something came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PactorSide {
    /// The station that sent the connect frames.
    Caller,
    /// The station they named.
    Called,
    /// On a link whose connect was not heard, the station first heard
    /// sending, and the other one.
    First,
    Second,
    /// An unproto (FEC) broadcast.
    Unproto,
}

impl PactorSide {
    pub fn as_str(&self) -> &'static str {
        match self {
            PactorSide::Caller => "caller",
            PactorSide::Called => "called",
            PactorSide::First => "first",
            PactorSide::Second => "second",
            PactorSide::Unproto => "unproto",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PactorMode {
    /// A connected link, packets acknowledged.
    Arq,
    /// Packets repeated without acknowledgement, to anyone listening.
    Unproto,
}

impl PactorMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PactorMode::Arq => "arq",
            PactorMode::Unproto => "unproto",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PactorEvent {
    /// Text from one station, a line at a time (or what there is after a
    /// pause, a changeover or the end of the link).
    Text { side: PactorSide, call: Option<String>, text: String },
    /// A connect frame: the called station, and the caller once its first
    /// line has named it.
    Connect { caller: Option<String>, called: String, longpath: bool },
    /// The packets changed speed.
    SpeedChange { side: PactorSide, call: Option<String>, baud: u16, previous: u16 },
    /// The stations swapped roles; `side` now sends.
    Changeover { side: PactorSide, call: Option<String> },
    /// A QRT packet from `side`, addressed to `named`.
    End { side: PactorSide, call: Option<String>, named: Option<String> },
}

impl PactorEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            PactorEvent::Text { .. } => "text",
            PactorEvent::Connect { .. } => "connect",
            PactorEvent::SpeedChange { .. } => "speedChange",
            PactorEvent::Changeover { .. } => "changeover",
            PactorEvent::End { .. } => "end",
        }
    }

    pub fn summary(&self) -> String {
        fn who(side: &PactorSide, call: &Option<String>) -> String {
            call.clone().unwrap_or_else(|| side.as_str().to_string())
        }
        match self {
            PactorEvent::Text { side, call, text } => format!("{}: {}", who(side, call), text),
            PactorEvent::Connect { caller, called, longpath } => format!(
                "{} calls {}{}",
                caller.as_deref().unwrap_or("?"),
                called,
                if *longpath { " (long path)" } else { "" }
            ),
            PactorEvent::SpeedChange { side, call, baud, previous } => {
                format!("{}: {} baud (was {})", who(side, call), baud, previous)
            }
            PactorEvent::Changeover { side, call } => format!("changeover: {} sending", who(side, call)),
            PactorEvent::End { side, call, named } => match named {
                Some(n) => format!("QRT from {} to {}", who(side, call), n),
                None => format!("QRT from {}", who(side, call)),
            },
        }
    }

    pub fn fields(&self) -> Vec<(&'static str, String)> {
        let opt = |v: &Option<String>| v.clone().unwrap_or_default();
        match self {
            PactorEvent::Text { side, call, text } => {
                vec![("side", side.as_str().into()), ("call", opt(call)), ("text", text.clone())]
            }
            PactorEvent::Connect { caller, called, longpath } => {
                vec![("caller", opt(caller)), ("called", called.clone()), ("longpath", longpath.to_string())]
            }
            PactorEvent::SpeedChange { side, call, baud, previous } => vec![
                ("side", side.as_str().into()),
                ("call", opt(call)),
                ("baud", baud.to_string()),
                ("previousBaud", previous.to_string()),
            ],
            PactorEvent::Changeover { side, call } => vec![("side", side.as_str().into()), ("call", opt(call))],
            PactorEvent::End { side, call, named } => {
                vec![("side", side.as_str().into()), ("call", opt(call)), ("named", opt(named))]
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PactorStatus {
    /// Locked on a tone pair that is still there.
    pub locked: bool,
    pub center_hz: Option<f32>,
    pub snr_db: Option<f32>,
    /// Speed of the last packet read.
    pub baud: Option<u16>,
    /// The cycle is known and the next packet expected at a known instant.
    pub cycle_locked: bool,
    pub mode: Option<PactorMode>,
    pub longpath: bool,
    /// Who is sending packets now.
    pub sending: Option<PactorSide>,
    pub caller: Option<String>,
    pub called: Option<String>,
    /// Packets that passed the CRC, repeats included.
    pub packets_ok: u64,
    /// Of those, the ones that only passed as a memory-ARQ sum.
    pub packets_memory_arq: u64,
    /// Expected packets that could not be read.
    pub packets_failed: u64,
    /// Repeats of a packet already printed.
    pub repeats: u64,
}

// ---------------------------------------------------------------------------
// The code tables.

/// The PACTOR Huffman code from the 1990 protocol description's annex:
/// each ASCII code with its bits in the order sent. A complete prefix code
/// (the Kraft sum is exactly one), so every bit string decodes.
const HUFFMAN: [(u8, &str); 128] = [
    (32, "10"), (101, "011"), (110, "0101"), (105, "1101"), (114, "1110"), (116, "00000"),
    (115, "00100"), (100, "00111"), (97, "01000"), (117, "11111"), (108, "000010"),
    (104, "000100"), (103, "000111"), (109, "001011"), (13, "001100"), (10, "001101"),
    (111, "010010"), (99, "010011"), (98, "0000110"), (102, "0000111"), (119, "0001100"),
    (68, "0001101"), (107, "0010101"), (122, "1100010"), (46, "1100100"), (44, "1100101"),
    (83, "1111011"), (65, "00101001"), (69, "11000000"), (112, "11000010"), (118, "11000011"),
    (48, "11000111"), (70, "11001100"), (66, "11001111"), (67, "11110001"), (73, "11110010"),
    (84, "11110100"), (79, "000101000"), (80, "000101100"), (49, "001010000"), (82, "110000010"),
    (40, "110011011"), (41, "110011100"), (76, "110011101"), (78, "111100000"), (90, "111100110"),
    (77, "111101010"), (57, "0001010010"), (87, "0001010100"), (53, "0001010101"),
    (121, "0001010110"), (50, "0001011010"), (51, "0001011011"), (52, "0001011100"),
    (54, "0001011101"), (55, "0001011110"), (56, "0001011111"), (72, "0010100010"),
    (74, "1100000110"), (85, "1100000111"), (86, "1100011000"), (28, "1100011001"),
    (120, "1100011010"), (75, "1100110100"), (63, "1100110101"), (61, "1111000010"),
    (113, "1111010110"), (81, "1111010111"), (106, "00010100110"), (71, "00010100111"),
    (45, "00010101111"), (58, "00101000111"), (33, "11110011101"), (47, "11110011110"),
    (42, "001010001100"), (34, "110001101100"), (37, "110001101101"), (39, "110001101110"),
    (95, "111100001100"), (38, "111100111001"), (43, "111100111110"), (62, "111100111111"),
    (64, "0001010111000"), (36, "0001010111001"), (60, "0001010111010"), (88, "0001010111011"),
    (35, "0010100011011"), (89, "00101000110101"), (59, "11110000110100"), (92, "11110000110101"),
    (91, "001010001101000"), (93, "001010001101001"), (127, "110001101111000"),
    (126, "110001101111001"), (125, "110001101111010"), (124, "110001101111011"),
    (123, "110001101111100"), (96, "110001101111101"), (94, "110001101111110"),
    (31, "110001101111111"), (29, "111100001101100"), (27, "111100001101101"),
    (25, "111100001101110"), (24, "111100001101111"), (23, "111100001110000"),
    (22, "111100001110001"), (21, "111100001110010"), (20, "111100001110011"),
    (19, "111100001110100"), (18, "111100001110101"), (17, "111100001110110"),
    (16, "111100001110111"), (30, "111100001111000"), (15, "111100001111001"),
    (14, "111100001111010"), (12, "111100001111011"), (11, "111100001111100"),
    (9, "111100001111101"), (8, "111100001111110"), (7, "111100001111111"),
    (6, "111100111000000"), (5, "111100111000001"), (4, "111100111000010"),
    (3, "111100111000011"), (2, "111100111000100"), (1, "111100111000101"),
    (0, "111100111000110"), (26, "111100111000111"),
];

/// A child that is a leaf carries this flag and the symbol.
const LEAF: u16 = 0x8000;

/// The Huffman code as a binary tree: node 0 is the root, and a child of
/// 0 is never set because the root is no one's child.
struct Huffman {
    nodes: Vec<[u16; 2]>,
}

impl Huffman {
    fn new() -> Self {
        let mut nodes = vec![[0u16; 2]];
        for (sym, code) in HUFFMAN {
            let mut at = 0usize;
            let bits = code.as_bytes();
            for (i, &c) in bits.iter().enumerate() {
                let b = usize::from(c == b'1');
                if i + 1 == bits.len() {
                    nodes[at][b] = LEAF | sym as u16;
                } else {
                    if nodes[at][b] == 0 {
                        nodes.push([0, 0]);
                        nodes[at][b] = (nodes.len() - 1) as u16;
                    }
                    at = nodes[at][b] as usize;
                }
            }
        }
        Self { nodes }
    }

    /// The symbols in a data field, first bit sent first. A code cut off
    /// by the end of the field is the start of a character sent whole in
    /// the next packet (or idle fill), and is dropped.
    fn decode(&self, field: &[u8], out: &mut Vec<u8>) {
        let mut at = 0usize;
        for i in 0..field.len() * 8 {
            let b = (field[i / 8] >> (i % 8)) & 1;
            let nx = self.nodes[at][b as usize];
            if nx & LEAF != 0 {
                out.push((nx & 0x7f) as u8);
                at = 0;
            } else {
                at = nx as usize;
            }
        }
    }
}

/// CRC-16 as X.25 computes it: polynomial x^16 + x^12 + x^5 + 1, reflected,
/// preset to all ones and complemented, sent low byte first.
const fn crc_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u16;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0x8408 } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC_TABLE: [u16; 256] = crc_table();

/// The CRC of `data`, each byte xored with `mask` first (0xff reads a
/// packet the other way up).
fn crc16(data: &[u8], mask: u8) -> u16 {
    let mut c: u16 = 0xffff;
    for &b in data {
        c = (c >> 8) ^ CRC_TABLE[((c ^ u16::from(b ^ mask)) & 0xff) as usize];
    }
    !c
}

/// Whether a body (everything after the header, the CRC last) checks.
fn crc_ok(body: &[u8], mask: u8) -> bool {
    let n = body.len();
    let want = u16::from_le_bytes([body[n - 2] ^ mask, body[n - 1] ^ mask]);
    crc16(&body[..n - 2], mask) == want
}

/// Errors against the alternating header (55 or AA, either way up).
fn header_errors(h: u8) -> u32 {
    let e = (h ^ 0x55).count_ones();
    e.min(8 - e)
}

/// A callsign as connect frames and QRT packets carry it: up to eight
/// letters, digits or strokes, padded with 0F hex.
fn callsign(bytes: &[u8]) -> Option<String> {
    let len = bytes.iter().position(|&b| b == 0x0f).unwrap_or(bytes.len());
    if bytes[len..].iter().any(|&b| b != 0x0f) {
        return None;
    }
    let s: String = bytes[..len].iter().map(|&b| b as char).collect();
    valid_call(&s).then_some(s)
}

fn valid_call(s: &str) -> bool {
    (3..=10).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'/')
        && s.bytes().any(|b| b.is_ascii_digit())
        && s.bytes().any(|b| b.is_ascii_uppercase())
}

/// The caller's call from its first line: the protocol has it send its
/// level digit and its call, as in `1DF4KV`.
fn ident(line: &str) -> Option<String> {
    let t = line.trim();
    let mut c = t.chars();
    if c.next().is_some_and(|d| d.is_ascii_digit()) {
        let rest = c.as_str().trim();
        if valid_call(rest) {
            return Some(rest.to_string());
        }
    }
    valid_call(t).then(|| t.to_string())
}

/// A byte as printed: line ends, printable ASCII and Latin-1. Other control
/// codes are left out rather than shown as something they are not.
fn printable(b: u8) -> Option<char> {
    match b {
        b'\r' | b'\n' => Some('\n'),
        b'\t' => Some(' '),
        0x20..=0x7e | 0xa0..=0xff => Some(b as char),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Small DSP pieces.

/// Blackman-windowed sinc lowpass with unity gain at DC, its transition
/// from `pass` to `stop` Hz.
fn lowpass(fs: f32, pass: f32, stop: f32, max_taps: usize) -> Vec<f32> {
    let n = ((5.5 * fs / (stop - pass).max(1.0)).ceil() as usize).clamp(15, max_taps) | 1;
    let cutoff = 0.5 * (pass + stop) / fs;
    let mid = (n / 2) as f32;
    let m = (n - 1) as f32;
    let mut taps: Vec<f32> = (0..n)
        .map(|i| {
            let x = i as f32 - mid;
            let sinc = if x == 0.0 { 2.0 * cutoff } else { (TAU * cutoff * x).sin() / (PI * x) };
            let w = 0.42 - 0.5 * (TAU * i as f32 / m).cos() + 0.08 * (2.0 * TAU * i as f32 / m).cos();
            sinc * w
        })
        .collect();
    let sum: f32 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t /= sum);
    taps
}

/// A complex oscillator stepping by repeated multiplication, renormalized
/// every 1024 samples.
#[derive(Clone, Copy)]
struct Osc {
    cur: Complex32,
    step: Complex32,
    n: u32,
}

impl Osc {
    /// Mixes down by `hz`.
    fn new(hz: f64, fs: f64) -> Self {
        let mut o = Self { cur: Complex32::new(1.0, 0.0), step: Complex32::new(1.0, 0.0), n: 0 };
        o.set(hz, fs);
        o
    }

    /// A new frequency, keeping the phase.
    fn set(&mut self, hz: f64, fs: f64) {
        let w = -std::f64::consts::TAU * hz / fs;
        self.step = Complex32::new(w.cos() as f32, w.sin() as f32);
    }

    #[inline]
    fn next(&mut self) -> Complex32 {
        let v = self.cur;
        self.cur *= self.step;
        self.n += 1;
        if self.n >= 1024 {
            self.n = 0;
            self.cur *= 1.5 - 0.5 * self.cur.norm_sqr();
        }
        v
    }
}

/// Mixes the audio down around [`MIX_HZ`] and decimates it to about
/// [`WORK_RATE`], evaluating only the outputs it keeps. The history is
/// written twice so the filter's window is always one contiguous slice.
struct Front {
    osc: Osc,
    taps: Vec<f32>,
    buf: Vec<Complex32>,
    pos: usize,
    decim: usize,
    phase: usize,
    rate: f32,
}

impl Front {
    fn new(fs: f32) -> Self {
        let decim = ((fs / WORK_RATE).round() as usize).max(1);
        let rate = fs / decim as f32;
        let pass = PASS_HZ.min(0.4 * rate);
        // Stop where the fold around the output rate would land on the
        // passband.
        let stop = (rate - pass).max(pass + 0.05 * rate);
        let taps = lowpass(fs, pass, stop, 1023);
        let n = taps.len();
        Self {
            osc: Osc::new(MIX_HZ as f64, fs as f64),
            taps,
            buf: vec![Complex32::new(0.0, 0.0); 2 * n],
            pos: 0,
            decim,
            phase: 0,
            rate,
        }
    }

    #[inline]
    fn push(&mut self, x: f32) -> Option<Complex32> {
        let n = self.taps.len();
        let v = self.osc.next() * x;
        self.buf[self.pos] = v;
        self.buf[self.pos + n] = v;
        self.pos += 1;
        if self.pos == n {
            self.pos = 0;
        }
        self.phase += 1;
        if self.phase < self.decim {
            return None;
        }
        self.phase = 0;
        let win = &self.buf[self.pos..self.pos + n];
        let (mut re, mut im) = ([0.0f32; 4], [0.0f32; 4]);
        let mut tc = self.taps.chunks_exact(4);
        let mut wc = win.chunks_exact(4);
        for (t, w) in (&mut tc).zip(&mut wc) {
            for k in 0..4 {
                re[k] += t[k] * w[k].re;
                im[k] += t[k] * w[k].im;
            }
        }
        let mut s = Complex32::new(re.iter().sum(), im.iter().sum());
        for (t, w) in tc.remainder().iter().zip(wc.remainder()) {
            s += w * *t;
        }
        Some(s)
    }
}

// ---------------------------------------------------------------------------
// Finding the pair.

#[derive(Clone, Copy, Debug, Default)]
struct Pair {
    rho_hi: f32,
    rho_lo: f32,
    rho_mid: f32,
}

impl Pair {
    /// The excess of the weaker tone, or zero when this is not shaped like
    /// a pair: a keyed pair has less in the middle than at either tone, and
    /// a lone carrier's skirt is far weaker than the carrier.
    fn score(&self) -> f32 {
        let weak = self.rho_hi.min(self.rho_lo);
        let strong = self.rho_hi.max(self.rho_lo);
        if weak < MAX_TILT * strong || self.rho_mid > 0.5 * weak {
            return 0.0;
        }
        weak
    }
}

/// The averaged spectrum of the baseband and the pair search over it.
struct Finder {
    n: usize,
    hop: usize,
    bin_hz: f32,
    fft: Arc<dyn Fft<f32>>,
    win: Vec<f32>,
    ring: Vec<Complex32>,
    pos: usize,
    filled: usize,
    since: usize,
    work: Vec<Complex32>,
    scratch: Vec<Complex32>,
    /// Averaged power, lowest frequency first: index `j` is
    /// `MIX_HZ + (j - n/2) * bin_hz`.
    avg: Vec<f32>,
    prefix: Vec<f64>,
    sorted: Vec<f32>,
    frames: u32,
    floor: f32,
    lo: usize,
    hi: usize,
    half_win: f32,
    half_shift: f32,
    half_mid: f32,
}

impl Finder {
    fn new(rate: f32) -> Self {
        let n = ((rate * 0.16) as usize).next_power_of_two().max(128);
        let bin_hz = rate / n as f32;
        let fft = FftPlanner::new().plan_fft_forward(n);
        let scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let win = (0..n).map(|i| 0.5 - 0.5 * (TAU * i as f32 / n as f32).cos()).collect();
        let bin = |hz: f32| (hz - MIX_HZ) / bin_hz + (n / 2) as f32;
        let lo = (bin(CENTER_LO_HZ - 100.0).ceil().max(1.0)) as usize;
        let hi = (bin(CENTER_HI_HZ + 100.0).floor() as usize).min(n - 2);
        Self {
            n,
            hop: n / 2,
            bin_hz,
            fft,
            win,
            ring: vec![Complex32::new(0.0, 0.0); n],
            pos: 0,
            filled: 0,
            since: 0,
            work: vec![Complex32::new(0.0, 0.0); n],
            scratch,
            avg: vec![0.0; n],
            prefix: vec![0.0; n + 1],
            sorted: Vec::with_capacity(n),
            frames: 0,
            floor: 0.0,
            lo,
            hi: hi.max(lo + 8),
            half_win: (TONE_HALF_HZ / bin_hz).max(1.5),
            half_shift: HALF_SHIFT_HZ / bin_hz,
            half_mid: ((HALF_SHIFT_HZ - TONE_HALF_HZ - 15.0) / bin_hz).max(1.0),
        }
    }

    fn bin(&self, hz: f32) -> f32 {
        (hz - MIX_HZ) / self.bin_hz + (self.n / 2) as f32
    }

    /// One sample in; true when a new averaged spectrum is ready.
    fn push(&mut self, x: Complex32) -> bool {
        self.ring[self.pos] = x;
        self.pos = (self.pos + 1) % self.n;
        self.filled = (self.filled + 1).min(self.n);
        self.since += 1;
        if self.since < self.hop || self.filled < self.n {
            return false;
        }
        self.since = 0;
        self.frame();
        true
    }

    fn frame(&mut self) {
        let n = self.n;
        for (i, w) in self.work.iter_mut().enumerate() {
            *w = self.ring[(self.pos + i) % n] * self.win[i];
        }
        self.fft.process_with_scratch(&mut self.work, &mut self.scratch);
        self.frames += 1;
        let a = (1.0 / self.frames as f32).max(SPEC_ALPHA);
        for j in 0..n {
            let p = self.work[(j + n / 2) % n].norm_sqr();
            self.avg[j] += a * (p - self.avg[j]);
        }
        let mut run = 0.0f64;
        for (k, &p) in self.avg.iter().enumerate() {
            run += p as f64;
            self.prefix[k + 1] = run;
        }
        // Signals fill a small share of the passband, so its median is
        // the noise.
        self.sorted.clear();
        self.sorted.extend_from_slice(&self.avg[self.lo..=self.hi]);
        let mid = self.sorted.len() / 2;
        let (_, m, _) = self.sorted.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
        self.floor = *m;
    }

    fn window(&self, c: f32, half: f32) -> (usize, usize) {
        let last = self.n - 1;
        let a = ((c - half).round().max(0.0) as usize).min(last);
        let b = ((c + half).round().max(0.0) as usize).min(last);
        (a, b.max(a))
    }

    /// Mean excess over the floor in units of the floor, and the excess
    /// power.
    fn excess(&self, (a, b): (usize, usize)) -> (f32, f32) {
        let n = (b - a + 1) as f64;
        let floor = self.floor as f64;
        let e = self.prefix[b + 1] - self.prefix[a] - n * floor;
        ((e / (n * floor)) as f32, e as f32)
    }

    fn pair_at(&self, center_hz: f32) -> Pair {
        if self.floor <= 0.0 || !self.floor.is_finite() {
            return Pair::default();
        }
        let c = self.bin(center_hz);
        let (rho_hi, _) = self.excess(self.window(c + self.half_shift, self.half_win));
        let (rho_lo, _) = self.excess(self.window(c - self.half_shift, self.half_win));
        let (rho_mid, _) = self.excess(self.window(c, self.half_mid));
        Pair { rho_hi, rho_lo, rho_mid }
    }

    /// Noise-subtracted centroid of a window, in bins.
    fn centroid(&self, (a, b): (usize, usize)) -> Option<f32> {
        let (mut sw, mut swk) = (0.0f32, 0.0f32);
        for k in a..=b {
            let p = (self.avg[k] - self.floor).max(0.0);
            sw += p;
            swk += p * k as f32;
        }
        (sw > 0.0).then(|| swk / sw)
    }

    /// The pair's center measured from the two tone windows around `hz`.
    fn refine(&self, hz: f32) -> Option<f32> {
        let c = self.bin(hz);
        let h = self.centroid(self.window(c + self.half_shift, self.half_win))?;
        let l = self.centroid(self.window(c - self.half_shift, self.half_win))?;
        Some(MIX_HZ + (0.5 * (h + l) - (self.n / 2) as f32) * self.bin_hz)
    }

    /// The strongest pair: its center in Hz and its score.
    fn best(&self) -> Option<(f32, f32)> {
        if self.floor <= 0.0 || !self.floor.is_finite() {
            return None;
        }
        let first = self.bin(CENTER_LO_HZ).ceil() as usize;
        let last = self.bin(CENTER_HI_HZ).floor() as usize;
        let mut best: Option<(f32, f32)> = None;
        for cb in first..=last {
            let hz = MIX_HZ + (cb as f32 - (self.n / 2) as f32) * self.bin_hz;
            let s = self.pair_at(hz).score();
            if best.is_none_or(|(_, b)| s > b) {
                best = Some((hz, s));
            }
        }
        let (hz, score) = best?;
        Some((self.refine(hz).unwrap_or(hz), score))
    }

    /// SNR in 2500 Hz: everything above the floor within 250 Hz of the
    /// center against the floor's density.
    fn snr_db(&self, center_hz: f32) -> f32 {
        let c = self.bin(center_hz);
        let (_, p) = self.excess(self.window(c, 250.0 / self.bin_hz));
        let noise = self.floor * SNR_REF_HZ / self.bin_hz;
        10.0 * (p.max(TINY) / noise.max(TINY)).log10()
    }
}

// ---------------------------------------------------------------------------
// The demodulator.

/// Mixes the pair's center to 0 Hz and each tone to DC, sums each over one
/// bit at both speeds, and turns each pair of tone levels into a
/// log-likelihood ratio, positive for the upper tone.
struct Demod {
    rate: f64,
    osc: Osc,
    up: Osc,
    down: Osc,
    len: [usize; 2],
    ring_h: Vec<Complex32>,
    ring_l: Vec<Complex32>,
    pos: usize,
    sum_h: [Complex32; 2],
    sum_l: [Complex32; 2],
    since_resum: u32,
    atc: [Atc; 2],
    spb: [f32; 2],
}

impl Demod {
    fn new(rate: f64, len: [usize; 2]) -> Self {
        let spb = [(rate / 100.0) as f32, (rate / 200.0) as f32];
        let z = Complex32::new(0.0, 0.0);
        Self {
            rate,
            osc: Osc::new(0.0, rate),
            up: Osc::new(HALF_SHIFT_HZ as f64, rate),
            down: Osc::new(-HALF_SHIFT_HZ as f64, rate),
            len,
            ring_h: vec![z; len[0]],
            ring_l: vec![z; len[0]],
            pos: 0,
            sum_h: [z; 2],
            sum_l: [z; 2],
            since_resum: 0,
            atc: [Atc::new(spb[0]), Atc::new(spb[1])],
            spb,
        }
    }

    fn set_center(&mut self, hz: f32) {
        self.osc.set((hz - MIX_HZ) as f64, self.rate);
    }

    fn reset(&mut self, hz: f32) {
        self.set_center(hz);
        let z = Complex32::new(0.0, 0.0);
        self.ring_h.fill(z);
        self.ring_l.fill(z);
        self.pos = 0;
        self.sum_h = [z; 2];
        self.sum_l = [z; 2];
        self.atc = [Atc::new(self.spb[0]), Atc::new(self.spb[1])];
    }

    /// One baseband sample; the soft decision at 100 and at 200 baud.
    #[inline]
    fn push(&mut self, x: Complex32) -> [f32; 2] {
        let y = x * self.osc.next();
        let h = y * self.up.next();
        let l = y * self.down.next();
        let n = self.len[0];
        // The 200 baud sum drops the sample it saw `len[1]` ago, still in
        // the 100 baud ring.
        let back = (self.pos + n - self.len[1]) % n;
        self.sum_h[1] += h - self.ring_h[back];
        self.sum_l[1] += l - self.ring_l[back];
        self.sum_h[0] += h - self.ring_h[self.pos];
        self.sum_l[0] += l - self.ring_l[self.pos];
        self.ring_h[self.pos] = h;
        self.ring_l[self.pos] = l;
        self.pos = (self.pos + 1) % n;
        self.since_resum += 1;
        if self.since_resum >= 4096 {
            // Rounding in the add-and-subtract cannot build up.
            self.since_resum = 0;
            self.sum_h[0] = self.ring_h.iter().sum();
            self.sum_l[0] = self.ring_l.iter().sum();
            let (mut sh, mut sl) = (Complex32::new(0.0, 0.0), Complex32::new(0.0, 0.0));
            for i in 0..self.len[1] {
                let k = (self.pos + n - 1 - i) % n;
                sh += self.ring_h[k];
                sl += self.ring_l[k];
            }
            self.sum_h[1] = sh;
            self.sum_l[1] = sl;
        }
        let mut out = [0.0f32; 2];
        for (b, o) in out.iter_mut().enumerate() {
            let inv = 1.0 / self.len[b] as f32;
            let (ah, al) = (self.sum_h[b].norm() * inv, self.sum_l[b].norm() * inv);
            let v = self.atc[b].step(ah, al);
            *o = (v * self.atc[b].llr_scale()).clamp(-LLR_MAX, LLR_MAX);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Packets.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A data packet with the 55/AA header.
    Data,
    /// A changeover packet headed by CS3.
    Bk,
    /// A connect frame (no CRC).
    Connect,
}

/// A packet as read off the air.
#[derive(Clone, Debug)]
struct Raw {
    /// Start of its first bit, in work samples.
    t0: f64,
    kind: Kind,
    /// 0 for 100 baud, 1 for 200.
    baud: usize,
    /// Everything after the header (or CS3): data, status, CRC, the right
    /// way up.
    body: Vec<u8>,
    /// Bit errors in the header or CS3 as received.
    hdr_err: u32,
    /// Sum of the soft bits' magnitudes: the widest eye wins among
    /// neighboring samples.
    metric: f32,
    /// Copies summed to pass the CRC.
    copies: u32,
    /// Connect frames: the called station, the long-path flag, and whether
    /// the 200 baud repeat of the call matched.
    call: String,
    longpath: bool,
    confirmed: bool,
}

impl Raw {
    fn status(&self) -> u8 {
        self.body[self.body.len() - 3]
    }

    fn data(&self) -> &[u8] {
        &self.body[..self.body.len() - 3]
    }

    /// Candidates checked by a CRC outrank a connect frame.
    fn rank(&self) -> u8 {
        u8::from(self.kind != Kind::Connect)
    }
}

fn baud_of(b: usize) -> u16 {
    [100, 200][b]
}

fn to_bytes(soft: &[f32], out: &mut Vec<u8>) {
    out.clear();
    out.resize(soft.len().div_ceil(8), 0);
    for (i, &v) in soft.iter().enumerate() {
        if v > 0.0 {
            out[i / 8] |= 1 << (i % 8);
        }
    }
}

/// Memory-ARQ sum of the copies of one packet at one speed.
#[derive(Default)]
struct Marq {
    sum: Vec<f32>,
    n: u32,
}

impl Marq {
    fn clear(&mut self) {
        self.sum.clear();
        self.n = 0;
    }

    /// Add a copy, turned to agree with the sum; a copy that does not
    /// correlate with the sum is another packet and starts a new one.
    fn add(&mut self, v: &[f32]) {
        if self.n > 0 && self.sum.len() == v.len() {
            let (mut sv, mut ss, mut vv) = (0.0f32, 0.0f32, 0.0f32);
            for (s, x) in self.sum.iter().zip(v) {
                sv += s * x;
                ss += s * s;
                vv += x * x;
            }
            let corr = sv / (ss * vv).sqrt().max(TINY);
            if corr.abs() >= MARQ_CORR {
                let sign = corr.signum();
                self.sum.iter_mut().zip(v).for_each(|(s, x)| *s += sign * x);
                self.n += 1;
                return;
            }
        }
        self.sum.clear();
        self.sum.extend_from_slice(v);
        self.n = 1;
    }
}

// ---------------------------------------------------------------------------
// The cycle and the link.

/// Weight kept by the timing fit's old points at each new one: the fit
/// spans the last thirty packets or so, long enough to average out the
/// timing of single packets, short enough to follow a drifting clock.
const FIT_KEEP: f64 = 0.97;

/// A least-squares line through the starts of the packets read since the
/// sender last changed, against their cycle numbers. One packet's timing
/// is good to a few tenths of a millisecond; a line through many gives the
/// cycle closely enough to place a packet a dozen cycles on, which memory
/// ARQ needs when every copy is too weak to time itself.
#[derive(Clone, Debug, Default)]
struct Fit {
    origin: f64,
    /// Cycle number of the last point.
    k: f64,
    w: f64,
    sk: f64,
    st: f64,
    skk: f64,
    skt: f64,
}

impl Fit {
    fn new(t: f64) -> Self {
        let mut f = Self { origin: t, ..Self::default() };
        f.add(0.0, t);
        f
    }

    fn add(&mut self, k: f64, t: f64) {
        let x = t - self.origin;
        for s in [&mut self.w, &mut self.sk, &mut self.st, &mut self.skk, &mut self.skt] {
            *s *= FIT_KEEP;
        }
        self.k = k;
        self.w += 1.0;
        self.sk += k;
        self.st += x;
        self.skk += k * k;
        self.skt += k * x;
    }

    /// The slope (the period) and where the line puts the last point.
    fn line(&self) -> Option<(f64, f64)> {
        let den = self.w * self.skk - self.sk * self.sk;
        if self.w < 2.5 || den <= 1e-9 {
            return None;
        }
        let slope = (self.w * self.skt - self.sk * self.st) / den;
        let icpt = (self.st - slope * self.sk) / self.w;
        Some((slope, self.origin + icpt + slope * self.k))
    }
}

/// The ISS's packet timing.
#[derive(Clone, Debug)]
struct Stream {
    mode: PactorMode,
    /// Cycle length in work samples, as measured.
    period: f64,
    nominal: f64,
    /// Start of the last packet read, as the fit puts it.
    phase: f64,
    /// The next packet expected and not yet looked for.
    next_slot: f64,
    misses: u32,
    locked: bool,
    rate: f64,
    fit: Fit,
}

impl Stream {
    fn new(mode: PactorMode, nominal_s: f64, period: f64, phase: f64, rate: f64) -> Self {
        Self {
            mode,
            period,
            nominal: nominal_s * rate,
            phase,
            next_slot: phase + period,
            misses: 0,
            locked: true,
            rate,
            fit: Fit::new(phase),
        }
    }

    /// A new sender: its packets start at `t`, the period stays.
    fn restart(&mut self, t: f64) {
        self.phase = t;
        self.next_slot = t + self.period;
        self.misses = 0;
        self.locked = true;
        self.fit = Fit::new(t);
    }

    fn tol(&self, k: f64) -> f64 {
        (PHASE_TOL_S * self.rate + k.abs() * self.period * DRIFT).min(PHASE_TOL_MAX_S * self.rate)
    }

    /// Cycles from the last packet to one starting at `t`, when it falls on
    /// this cycle.
    fn fits(&self, t: f64) -> Option<i64> {
        let k = ((t - self.phase) / self.period).round();
        let resid = t - self.phase - k * self.period;
        (resid.abs() <= self.tol(k)).then_some(k as i64)
    }

    /// Whether `t` falls where the IRS's control signal goes, which is
    /// where its changeover packet starts.
    fn in_cs_window(&self, t: f64) -> bool {
        let x = (t - self.phase).rem_euclid(self.period) / self.rate;
        x >= PACKET_S - 0.06 || x <= 0.05
    }

    /// A packet read `k` cycles after the last.
    fn update(&mut self, t: f64, k: i64) {
        let prev = self.phase;
        self.phase = t;
        if k >= 1 {
            let lim = 4.0 * DRIFT * self.nominal;
            self.fit.add(self.fit.k + k as f64, t);
            match self.fit.line() {
                Some((p, at)) if (p - self.nominal).abs() <= lim => {
                    self.period = p;
                    self.phase = at;
                }
                _ => {
                    let p = (t - prev) / k as f64;
                    self.period = (self.period + 0.3 * (p - self.period)).clamp(self.nominal - lim, self.nominal + lim);
                }
            }
        }
        self.next_slot = self.phase + self.period;
        self.misses = 0;
        self.locked = true;
    }
}

/// A period two packets `dt` apart fit: its nominal length in seconds and
/// the number of cycles.
fn fit_period(dt: f64, rate: f64) -> Option<(f64, i64)> {
    let mut best: Option<(f64, i64, f64)> = None;
    for p_s in [CYCLE_S, LONGPATH_CYCLE_S, UNPROTO_CYCLE_S] {
        let p = p_s * rate;
        let k = (dt / p).round();
        if !(1.0..=CONFIRM_CYCLES as f64).contains(&k) {
            continue;
        }
        let tol = PHASE_TOL_S * rate + k * p * DRIFT;
        let r = (dt - k * p).abs() / tol;
        if r <= 1.0 && best.is_none_or(|(_, _, b)| r < b) {
            best = Some((p_s, k as i64, r));
        }
    }
    best.map(|(p, k, _)| (p, k))
}

/// One station's side of the link.
#[derive(Default)]
struct Side {
    call: Option<String>,
    lines: Lines,
    last_char_t: f64,
    last_counter: Option<u8>,
    last_body: Vec<u8>,
    last_baud: Option<usize>,
    /// What the last packet printed, for a 200 baud packet sent again at
    /// 100 baud.
    last_chars: Vec<u8>,
    /// Characters already printed that the packets now arriving repeat.
    skip: VecDeque<u8>,
}

impl Side {
    fn forget_packets(&mut self) {
        self.last_counter = None;
        self.last_body.clear();
        self.last_baud = None;
        self.last_chars.clear();
        self.skip.clear();
    }
}

struct Pending {
    called: String,
    longpath: bool,
    deadline: f64,
}

#[derive(Default)]
struct Link {
    active: bool,
    stream: Option<Stream>,
    sides: [Side; 2],
    roles: [Option<PactorSide>; 2],
    sender: usize,
    baud: Option<usize>,
    longpath: bool,
    ended: Option<f64>,
    last_t: f64,
    /// A connect waiting for the caller to name itself.
    pending: Option<Pending>,
    /// Data packets read since the link began.
    data_seen: bool,
}

impl Link {
    fn role(&self, s: usize) -> PactorSide {
        self.roles[s].unwrap_or(PactorSide::First)
    }

    fn mode(&self) -> Option<PactorMode> {
        self.stream.as_ref().map(|s| s.mode)
    }

    fn called(&self) -> Option<&str> {
        (self.roles[1] == Some(PactorSide::Called)).then(|| self.sides[1].call.as_deref()).flatten()
    }
}

// ---------------------------------------------------------------------------
// The decoder.

/// Which offset table a read uses.
#[derive(Clone, Copy)]
enum Table {
    Bits100,
    Bits200,
    Call200,
}

pub struct PactorDecoder {
    rate: f64,
    front: Front,
    finder: Finder,
    demod: Demod,
    spb: [f64; 2],
    bb: Vec<Complex32>,
    soft: [Vec<f32>; 2],
    /// Work samples produced so far.
    n: u64,
    // Pair lock.
    running: bool,
    present: bool,
    absent: u32,
    hold_frames: u32,
    cand: Option<f32>,
    cand_n: u32,
    center: f32,
    snr_db: f32,
    /// Soft decisions before this belong to another lock or none.
    valid_from: u64,
    // Packet search.
    /// Next sample to try as a packet start.
    scan: u64,
    /// Samples a packet needs after its start before it can be read.
    span: u64,
    /// Where each bit is sampled, after the packet's start: whole samples
    /// and the fraction to interpolate.
    offs100: Vec<(u64, f32)>,
    offs200: Vec<(u64, f32)>,
    offs_call: Vec<(u64, f32)>,
    /// How late after an expected packet's start it is looked for, once
    /// the search would have found it.
    slot_margin: f64,
    best: Option<Raw>,
    busy_until: f64,
    marq: [Marq; 2],
    prov: Vec<Raw>,
    prov_calls: Vec<(String, f64)>,
    link: Link,
    last_calls: (Option<String>, Option<String>),
    huff: Huffman,
    events: Vec<PactorEvent>,
    packets_ok: u64,
    packets_marq: u64,
    packets_failed: u64,
    repeats: u64,
    tmp: Vec<f32>,
    bytes: Vec<u8>,
}

impl PactorDecoder {
    pub fn new(fs: f32) -> Self {
        let front = Front::new(fs);
        let rate = front.rate as f64;
        let finder = Finder::new(front.rate);
        let spb = [rate / 100.0, rate / 200.0];
        let len = [spb[0].round().max(2.0) as usize, spb[1].round().max(2.0) as usize];
        let split = |x: f64| {
            let i = x.floor();
            (i as u64, (x - i) as f32)
        };
        let offs = |b: usize, nb: usize| -> Vec<(u64, f32)> {
            (0..nb).map(|k| split(k as f64 * spb[b] + (len[b] - 1) as f64)).collect()
        };
        let offs_call = (0..48).map(|j| split(72.0 * spb[0] + j as f64 * spb[1] + (len[1] - 1) as f64)).collect();
        let hold_frames = (HOLD_S * front.rate / finder.hop as f32) as u32;
        let tol1 = PHASE_TOL_S * rate + CYCLE_S * rate * DRIFT;
        Self {
            rate,
            demod: Demod::new(rate, len),
            front,
            finder,
            spb,
            bb: vec![Complex32::new(0.0, 0.0); RING],
            soft: [vec![0.0; RING], vec![0.0; RING]],
            n: 0,
            running: false,
            present: false,
            absent: 0,
            hold_frames,
            cand: None,
            cand_n: 0,
            center: MIX_HZ,
            snr_db: f32::NEG_INFINITY,
            valid_from: u64::MAX,
            scan: 0,
            span: (PACKET_S * rate + 8.0).ceil() as u64,
            offs100: offs(0, 96),
            offs200: offs(1, 192),
            offs_call,
            slot_margin: 2.0 * spb[0] + tol1 + 4.0,
            best: None,
            busy_until: f64::NEG_INFINITY,
            marq: [Marq::default(), Marq::default()],
            prov: Vec::new(),
            prov_calls: Vec::new(),
            link: Link::default(),
            last_calls: (None, None),
            huff: Huffman::new(),
            events: Vec::new(),
            packets_ok: 0,
            packets_marq: 0,
            packets_failed: 0,
            repeats: 0,
            tmp: Vec::with_capacity(192),
            bytes: Vec::with_capacity(24),
        }
    }

    pub fn push(&mut self, audio: &[f32]) {
        for &x in audio {
            let Some(y) = self.front.push(x) else { continue };
            let i = self.n;
            self.bb[i as usize & MASK] = y;
            self.n += 1;
            if self.finder.push(y) && self.evaluate() {
                self.relock();
                continue;
            }
            if self.running {
                let s = self.demod.push(y);
                self.soft[0][i as usize & MASK] = s[0];
                self.soft[1][i as usize & MASK] = s[1];
            }
            // Keep the search close behind, so the ring is never overrun.
            if self.running && self.n - self.scan.min(self.n) > (RING / 4) as u64 + self.span {
                self.run_scan();
            }
        }
        self.run_scan();
    }

    pub fn take(&mut self) -> Vec<PactorEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn status(&self) -> PactorStatus {
        let l = &self.link;
        let (caller, called) = if l.active {
            let call = |r: PactorSide| (0..2).find(|&s| l.roles[s] == Some(r)).and_then(|s| l.sides[s].call.clone());
            let a = call(PactorSide::Caller).or_else(|| call(PactorSide::First));
            let b = call(PactorSide::Called).or_else(|| call(PactorSide::Second));
            (a, b)
        } else {
            self.last_calls.clone()
        };
        PactorStatus {
            locked: self.running && self.present,
            center_hz: self.running.then_some(self.center),
            snr_db: (self.running && self.present).then_some(self.snr_db),
            baud: l.baud.map(baud_of),
            cycle_locked: l.stream.as_ref().is_some_and(|s| s.locked),
            mode: l.mode(),
            longpath: l.longpath,
            sending: l.active.then(|| l.role(l.sender)),
            caller,
            called,
            packets_ok: self.packets_ok,
            packets_memory_arq: self.packets_marq,
            packets_failed: self.packets_failed,
            repeats: self.repeats,
        }
    }

    // -----------------------------------------------------------------------
    // Pair lock.

    /// Look at a new spectrum. True when it completes a lock.
    fn evaluate(&mut self) -> bool {
        if self.finder.frames < MIN_FRAMES {
            return false;
        }
        if self.running {
            let p = self.finder.pair_at(self.center);
            self.present = p.score() > HOLD_RHO;
            if self.present {
                self.absent = 0;
                self.snr_db = self.finder.snr_db(self.center);
                // Follow drift slowly: the spectrum mixes both stations,
                // which can sit a few hertz apart.
                if let Some(c) = self.finder.refine(self.center) {
                    let d = c - self.center;
                    if d.abs() < TONE_HALF_HZ {
                        self.center += 0.2 * d;
                        self.demod.set_center(self.center);
                    }
                }
            } else {
                self.absent += 1;
                if self.absent > self.hold_frames {
                    self.running = false;
                    self.cand = None;
                    self.cand_n = 0;
                }
            }
            return false;
        }
        match self.finder.best() {
            Some((c, score)) if score > ACQUIRE_RHO => {
                self.cand_n = match self.cand {
                    Some(prev) if (prev - c).abs() <= 15.0 => self.cand_n + 1,
                    _ => 1,
                };
                self.cand = Some(c);
                if self.cand_n >= CONFIRM {
                    self.center = c;
                    self.running = true;
                    self.present = true;
                    self.absent = 0;
                    self.snr_db = self.finder.snr_db(c);
                    return true;
                }
            }
            _ => {
                self.cand = None;
                self.cand_n = 0;
            }
        }
        false
    }

    /// A new lock: run the kept baseband through the demodulator.
    fn history(&self) -> u64 {
        ((HISTORY_S * self.rate) as u64).min((RING as u64).saturating_sub(self.span + 64))
    }

    fn relock(&mut self) {
        self.demod.reset(self.center);
        let start = self.n.saturating_sub(self.history());
        for i in start..self.n {
            let s = self.demod.push(self.bb[i as usize & MASK]);
            self.soft[0][i as usize & MASK] = s[0];
            self.soft[1][i as usize & MASK] = s[1];
        }
        // The matched filters fill before anything they give is used.
        self.valid_from = start + self.spb[0] as u64 + 2;
        self.scan = self.scan.max(self.valid_from);
    }

    // -----------------------------------------------------------------------
    // Reading bits.

    fn table(&self, t: Table) -> &[(u64, f32)] {
        match t {
            Table::Bits100 => &self.offs100,
            Table::Bits200 => &self.offs200,
            Table::Call200 => &self.offs_call,
        }
    }

    #[inline]
    fn soft_at(&self, b: usize, t0: u64, off: (u64, f32)) -> f32 {
        let i = (t0 + off.0) as usize;
        let a = self.soft[b][i & MASK];
        let c = self.soft[b][(i + 1) & MASK];
        a + off.1 * (c - a)
    }

    /// `n` bits from bit `k0`, packed first bit lowest.
    fn hard(&self, t: Table, b: usize, t0: u64, k0: usize, n: usize) -> u32 {
        let offs = self.table(t);
        let mut v = 0u32;
        for k in 0..n {
            if self.soft_at(b, t0, offs[k0 + k]) > 0.0 {
                v |= 1 << k;
            }
        }
        v
    }

    fn gather(&self, t: Table, b: usize, t0: u64, out: &mut Vec<f32>) {
        out.clear();
        out.extend(self.table(t).iter().map(|&o| self.soft_at(b, t0, o)));
    }

    /// Read a packet's soft bits: CRC-checked either way up.
    fn decode(&self, soft: &[f32], kind: Kind, b: usize, t0: f64, hdr_err: u32, bytes: &mut Vec<u8>) -> Option<Raw> {
        to_bytes(soft, bytes);
        let start = match (kind, b) {
            (Kind::Bk, 0) => 2,
            (Kind::Bk, _) => 3,
            _ => 1,
        };
        let hdr_err = if kind == Kind::Data { header_errors(bytes[0]) } else { hdr_err };
        let body = &bytes[start..];
        let mask = [0x00u8, 0xff].into_iter().find(|&m| crc_ok(body, m))?;
        Some(Raw {
            t0,
            kind,
            baud: b,
            body: body.iter().map(|&x| x ^ mask).collect(),
            hdr_err,
            metric: soft.iter().map(|v| v.abs()).sum(),
            copies: 1,
            call: String::new(),
            longpath: false,
            confirmed: false,
        })
    }

    /// A connect frame starting at `t0`: the header and the called call at
    /// 100 baud, then its first six characters again at 200.
    fn read_connect(&mut self, t0: u64) -> Option<Raw> {
        let mut soft = std::mem::take(&mut self.tmp);
        self.gather(Table::Bits100, 0, t0, &mut soft);
        soft.truncate(72);
        let mut bytes = std::mem::take(&mut self.bytes);
        to_bytes(&soft, &mut bytes);
        let head = bytes[0];
        let mask = if (head ^ 0x55).count_ones() <= 4 { 0x00 } else { 0xff };
        let mut call: Vec<u8> = bytes[1..9].iter().map(|&x| x ^ mask).collect();
        let metric: f32 = soft.iter().map(|v| v.abs()).sum();
        self.tmp = soft;
        self.bytes = bytes;
        // On the long path the first character is sent inverted.
        let mut longpath = false;
        let mut name = callsign(&call);
        if name.is_none() {
            call[0] = !call[0];
            name = callsign(&call);
            longpath = true;
        }
        let name = name?;
        let r200 = self.hard(Table::Call200, 1, t0, 0, 32) as u64 | (self.hard(Table::Call200, 1, t0, 32, 16) as u64) << 32;
        let mut want = 0u64;
        for (i, &c) in call[..6].iter().enumerate() {
            want |= (c as u64) << (8 * i);
        }
        let mut want_lp = want;
        if longpath {
            want_lp ^= 0xff;
        }
        let m48 = if mask == 0xff { (1u64 << 48) - 1 } else { 0 };
        let errs = ((r200 ^ m48) ^ want).count_ones().min(((r200 ^ m48) ^ want_lp).count_ones());
        Some(Raw {
            t0: t0 as f64,
            kind: Kind::Connect,
            baud: 0,
            body: Vec::new(),
            hdr_err: header_errors(head),
            metric,
            copies: 1,
            call: name,
            longpath,
            confirmed: errs <= 6,
        })
    }

    // -----------------------------------------------------------------------
    // The search.

    fn run_scan(&mut self) {
        if !self.running {
            // Stay as far behind as the replay at lock reaches, so what it
            // demodulates is searched.
            self.scan = self.scan.max(self.n.saturating_sub(self.history()));
            if let Some(b) = self.best.take() {
                self.on_raw(b);
            }
            self.housekeeping();
            return;
        }
        while self.scan + self.span <= self.n {
            let t0 = self.scan;
            let tf = t0 as f64;
            if let Some(b) = &self.best
                && tf > b.t0 + self.spb[b.baud] + 1.0
            {
                let b = self.best.take().expect("checked");
                self.on_raw(b);
            }
            self.slots_due(tf);
            if t0 >= self.valid_from && tf >= self.busy_until {
                self.detect(t0);
            }
            self.scan += 1;
        }
        self.housekeeping();
    }

    /// Try one sample as the start of each kind of packet.
    fn detect(&mut self, t0: u64) {
        let m100 = header_errors(self.hard(Table::Bits100, 0, t0, 0, 8) as u8);
        let m200 = header_errors(self.hard(Table::Bits200, 1, t0, 0, 8) as u8);
        let cs = self.hard(Table::Bits100, 0, t0, 0, 12);
        let e3 = (cs ^ CS3).count_ones();
        let m3 = e3.min(12 - e3);
        let mut soft = std::mem::take(&mut self.tmp);
        let mut bytes = std::mem::take(&mut self.bytes);
        let mut found: Vec<Raw> = Vec::new();
        if m100 <= 1 {
            self.gather(Table::Bits100, 0, t0, &mut soft);
            if let Some(r) = self.decode(&soft, Kind::Data, 0, t0 as f64, m100, &mut bytes) {
                found.push(r);
            }
        }
        if m200 <= 1 {
            self.gather(Table::Bits200, 1, t0, &mut soft);
            if let Some(r) = self.decode(&soft, Kind::Data, 1, t0 as f64, m200, &mut bytes) {
                found.push(r);
            }
        }
        if m3 <= 1 {
            self.gather(Table::Bits100, 0, t0, &mut soft);
            if let Some(r) = self.decode(&soft, Kind::Bk, 0, t0 as f64, m3, &mut bytes) {
                found.push(r);
            }
            self.gather(Table::Bits200, 1, t0, &mut soft);
            if let Some(r) = self.decode(&soft, Kind::Bk, 1, t0 as f64, m3, &mut bytes) {
                found.push(r);
            }
        }
        self.tmp = soft;
        self.bytes = bytes;
        if m100 <= 1
            && found.is_empty()
            && let Some(r) = self.read_connect(t0)
        {
            found.push(r);
        }
        for r in found {
            self.offer(r);
        }
    }

    /// A candidate: neighboring samples pass for the same packet, and the
    /// one with the widest eye is kept.
    fn offer(&mut self, r: Raw) {
        if let Some(b) = &self.best {
            let near = r.t0 - b.t0 <= self.spb[b.baud].max(self.spb[r.baud]);
            if near {
                if (r.rank(), r.metric) > (b.rank(), b.metric) {
                    self.best = Some(r);
                }
                return;
            }
            let b = self.best.take().expect("checked");
            self.on_raw(b);
        }
        self.best = Some(r);
    }

    fn on_raw(&mut self, r: Raw) {
        match r.kind {
            Kind::Connect => self.on_connect(r),
            _ => self.on_packet(r),
        }
    }

    // -----------------------------------------------------------------------
    // Expected packets and memory ARQ.

    fn slots_due(&mut self, now: f64) {
        loop {
            let Some(st) = self.link.stream.as_mut() else { return };
            if !st.locked || now < st.next_slot + self.slot_margin {
                return;
            }
            let slot = st.next_slot;
            st.next_slot += st.period;
            self.check_slot(slot);
        }
    }

    /// An expected packet the search did not find: read it where the cycle
    /// puts it, alone and summed with the copies before it.
    ///
    /// The cycle is known to a fraction of a millisecond, the stations'
    /// clocks being crystals: closer than the widest eye of one weak copy
    /// would place it, which wanders by a sixth of a bit at the SNRs where
    /// memory ARQ is needed.
    fn check_slot(&mut self, slot: f64) {
        let cur = self.link.baud.unwrap_or(0);
        let at = slot.round() as u64;
        let mut got = None;
        let mut soft = std::mem::take(&mut self.tmp);
        let mut bytes = std::mem::take(&mut self.bytes);
        'speeds: for b in [cur, 1 - cur] {
            let table = if b == 0 { Table::Bits100 } else { Table::Bits200 };
            for d in [0i64, -1, 1, -2, 2] {
                let t0 = (at as i64 + d).max(0) as u64;
                if t0 < self.valid_from {
                    continue;
                }
                self.gather(table, b, t0, &mut soft);
                if let Some(r) = self.decode(&soft, Kind::Data, b, t0 as f64, 0, &mut bytes)
                    && r.hdr_err <= 1
                {
                    got = Some(r);
                    break 'speeds;
                }
            }
            if at < self.valid_from {
                continue;
            }
            let t0 = at;
            self.gather(table, b, t0, &mut soft);
            self.marq[b].add(&soft);
            if self.marq[b].n >= 2
                && let Some(mut r) = self.decode(&self.marq[b].sum, Kind::Data, b, t0 as f64, 0, &mut bytes)
                && r.hdr_err <= 1
            {
                r.copies = self.marq[b].n;
                got = Some(r);
                break;
            }
        }
        self.tmp = soft;
        self.bytes = bytes;
        match got {
            Some(r) => {
                if let Some(b) = self.best.take() {
                    self.on_raw(b);
                }
                self.on_packet(r);
            }
            None => {
                // A slot after the sender has gone quiet is not a failure.
                if self.link.data_seen && self.link.ended.is_none() && self.present {
                    self.packets_failed += 1;
                }
                if let Some(st) = self.link.stream.as_mut() {
                    st.misses += 1;
                    if st.misses >= LOCK_LOSS_SLOTS {
                        st.locked = false;
                        self.marq.iter_mut().for_each(Marq::clear);
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // The link.

    fn on_packet(&mut self, r: Raw) {
        let t = r.t0;
        match r.kind {
            Kind::Data => {
                if let Some(k) = self.link.stream.as_ref().and_then(|s| s.fits(t)) {
                    if k <= 0 {
                        // The packet last read, found again.
                        return;
                    }
                    if let Some(st) = self.link.stream.as_mut() {
                        st.update(t, k);
                    }
                    self.marq.iter_mut().for_each(Marq::clear);
                    self.deliver(r);
                    return;
                }
            }
            Kind::Bk => {
                let in_window = self
                    .link
                    .stream
                    .as_ref()
                    .is_some_and(|s| s.mode == PactorMode::Arq && s.in_cs_window(t));
                if in_window && r.status() & ST_COUNTER == 0 {
                    self.changeover(t);
                    self.deliver(r);
                    return;
                }
            }
            Kind::Connect => {}
        }
        self.provisional(r);
    }

    /// A packet off any known cycle waits for a second to confirm it.
    fn provisional(&mut self, r: Raw) {
        // Only clean headers start a stream: a header with an error lets
        // noise through to the CRC four times as often.
        if r.hdr_err > 0 {
            return;
        }
        let t = r.t0;
        let mut found = None;
        for (i, p) in self.prov.iter().enumerate().rev() {
            let dt = t - p.t0;
            if dt < 0.5 * self.rate {
                continue;
            }
            if let Some(fit) = fit_period(dt, self.rate) {
                found = Some((i, Some(fit)));
                break;
            }
            if p.body == r.body && dt < 3.0 * self.rate {
                found = Some((i, None));
                break;
            }
        }
        match found {
            Some((i, fit)) => {
                let p = self.prov.remove(i);
                self.prov.clear();
                self.establish(p, r, fit);
            }
            None => self.prov.push(r),
        }
    }

    /// Two packets that vouch for each other start a stream.
    fn establish(&mut self, p: Raw, q: Raw, fit: Option<(f64, i64)>) {
        let dt = q.t0 - p.t0;
        let (mode, nominal, k) = match fit {
            Some((ps, k)) if ps == UNPROTO_CYCLE_S => (PactorMode::Unproto, ps, k),
            Some((ps, k)) => (PactorMode::Arq, ps, k),
            // The same packet twice at a spacing that fits no cycle:
            // unproto with a repetition rate of the sender's choosing.
            None => (PactorMode::Unproto, dt / self.rate, 1),
        };
        let continuing = mode == PactorMode::Arq
            && self.link.active
            && self.link.ended.is_none()
            && self.link.mode() == Some(PactorMode::Arq);
        if continuing {
            let old = self.link.stream.clone().expect("an ARQ link has a stream");
            if old.fits(p.t0).is_none() {
                // The other station has taken over, its break-in missed.
                self.changeover(p.t0);
            }
            let mut st = Stream::new(mode, nominal, old.period, p.t0, self.rate);
            st.nominal = old.nominal;
            self.link.stream = Some(st);
        } else {
            self.new_link(mode);
            self.link.longpath = nominal == LONGPATH_CYCLE_S;
            self.link.stream = Some(Stream::new(mode, nominal, dt / k as f64, p.t0, self.rate));
        }
        self.marq.iter_mut().for_each(Marq::clear);
        self.deliver(p);
        if let Some(st) = self.link.stream.as_mut() {
            st.update(q.t0, k);
        }
        self.deliver(q);
    }

    fn on_connect(&mut self, r: Raw) {
        let t = r.t0;
        if !r.confirmed {
            // Without the 200 baud repeat to vouch for it, the same call a
            // whole number of cycles later does.
            let seen = self
                .prov_calls
                .iter()
                .any(|(c, t1)| *c == r.call && fit_period(t - t1, self.rate).is_some_and(|(p, _)| p != UNPROTO_CYCLE_S));
            if !seen {
                self.prov_calls.push((r.call, t));
                return;
            }
        }
        self.busy_until = self.busy_until.max(t + 0.95 * PACKET_S * self.rate);
        if self.link.active
            && self.link.ended.is_none()
            && self.link.called() == Some(r.call.as_str())
            && let Some(st) = self.link.stream.as_mut()
            && let Some(k) = st.fits(t)
        {
            // The caller calling again: the link heard so far.
            if k >= 1 {
                st.update(t, k);
            }
            return;
        }
        self.reset_link();
        let l = &mut self.link;
        l.active = true;
        l.roles = [Some(PactorSide::Caller), Some(PactorSide::Called)];
        l.sides[1].call = Some(r.call.clone());
        l.sender = 0;
        l.longpath = r.longpath;
        l.last_t = t;
        let nominal = if r.longpath { LONGPATH_CYCLE_S } else { CYCLE_S };
        l.stream = Some(Stream::new(PactorMode::Arq, nominal, nominal * self.rate, t, self.rate));
        l.pending = Some(Pending { called: r.call, longpath: r.longpath, deadline: t + IDENT_WAIT_S * self.rate });
        self.prov.clear();
        self.marq.iter_mut().for_each(Marq::clear);
    }

    fn new_link(&mut self, mode: PactorMode) {
        self.reset_link();
        let l = &mut self.link;
        l.active = true;
        l.roles = match mode {
            PactorMode::Arq => [Some(PactorSide::First), Some(PactorSide::Second)],
            PactorMode::Unproto => [Some(PactorSide::Unproto), Some(PactorSide::Unproto)],
        };
        l.sender = 0;
    }

    /// Let go of the link: its text, the connect it is waiting on.
    fn reset_link(&mut self) {
        if self.link.active {
            self.resolve_connect();
            self.flush_side(0);
            self.flush_side(1);
            let s = self.status();
            self.last_calls = (s.caller, s.called);
        }
        self.link = Link::default();
    }

    fn changeover(&mut self, t: f64) {
        self.resolve_connect();
        let old = self.link.sender;
        self.flush_side(old);
        let new = 1 - old;
        self.link.sender = new;
        self.link.sides[new].forget_packets();
        if let Some(st) = self.link.stream.as_mut() {
            st.restart(t);
        }
        self.marq.iter_mut().for_each(Marq::clear);
        let side = self.link.role(new);
        let call = self.link.sides[new].call.clone();
        self.events.push(PactorEvent::Changeover { side, call });
    }

    /// A packet that passed the CRC and belongs to the link.
    fn deliver(&mut self, r: Raw) {
        self.packets_ok += 1;
        if r.copies > 1 {
            self.packets_marq += 1;
        }
        self.link.last_t = r.t0;
        self.busy_until = self.busy_until.max(r.t0 + 0.95 * PACKET_S * self.rate);
        if self.link.ended.is_some() {
            // The QRT packet again, until the other side's last CS gets
            // through.
            return;
        }
        let s = self.link.sender;
        let status = r.status();
        let counter = status & ST_COUNTER;
        if status & ST_QRT != 0 {
            self.end(&r);
            return;
        }
        let side = &self.link.sides[s];
        if side.last_counter == Some(counter) && side.last_body == r.body {
            self.repeats += 1;
            return;
        }
        if let Some(prev) = self.link.baud
            && prev != r.baud
        {
            let (side, call) = (self.link.role(s), self.link.sides[s].call.clone());
            self.events.push(PactorEvent::SpeedChange { side, call, baud: baud_of(r.baud), previous: baud_of(prev) });
        }
        self.link.baud = Some(r.baud);
        self.link.data_seen = true;
        let chars = self.field_text(r.data(), (status >> 2) & 3);
        let side = &mut self.link.sides[s];
        if r.baud == 0 && side.last_baud == Some(1) && side.last_counter == Some(counter) {
            // A 200 baud packet sent again at 100: what it carried is
            // coming again, a few characters a packet.
            side.skip = side.last_chars.iter().copied().collect();
        }
        side.last_counter = Some(counter);
        side.last_body = r.body.clone();
        side.last_baud = Some(r.baud);
        side.last_chars = chars.clone();
        let mut out = Vec::with_capacity(chars.len());
        for c in chars {
            if let Some(&d) = side.skip.front() {
                if d == c {
                    side.skip.pop_front();
                    continue;
                }
                side.skip.clear();
            }
            out.push(c);
        }
        if side.last_chars.is_empty() {
            // An idle packet: the sender has paused, so what it sent so
            // far is all there is for now.
            self.flush_side(s);
        }
        for c in out {
            self.put_char(s, c, r.t0);
        }
    }

    /// The characters in a data field.
    fn field_text(&self, data: &[u8], format: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity(32);
        match format {
            0 => v.extend(data.iter().copied()),
            1 | 2 => self.huff.decode(data, &mut v),
            _ => {}
        }
        v.retain(|&b| b != IDLE);
        if format == 2 {
            v.iter_mut().for_each(|b| {
                if b.is_ascii_alphabetic() {
                    *b ^= 0x20;
                }
            });
        }
        v
    }

    /// A QRT packet: the call it carries, sent backward, is the station it
    /// is addressed to.
    fn end(&mut self, r: &Raw) {
        let d = r.data();
        let rev: Vec<u8> = if r.baud == 0 {
            d[..7].to_vec()
        } else {
            // At 200 baud each bit of the call is sent twice, so a
            // 100 baud receiver searching for it finds it too.
            (0..7)
                .map(|j| {
                    let mut v = 0u8;
                    for m in 0..8 {
                        let i = 8 + 16 * j + 2 * m;
                        v |= ((d[i / 8] >> (i % 8)) & 1) << m;
                    }
                    v
                })
                .collect()
        };
        let call: Vec<u8> = rev.into_iter().rev().collect();
        let named = callsign(&call);
        let s = self.link.sender;
        let o = 1 - s;
        if let Some(n) = &named
            && self.link.sides[o].call.is_none()
            && self.link.roles[o] != Some(PactorSide::Unproto)
        {
            self.link.sides[o].call = Some(n.clone());
        }
        self.resolve_connect();
        self.flush_side(s);
        self.flush_side(o);
        let (side, call) = (self.link.role(s), self.link.sides[s].call.clone());
        self.events.push(PactorEvent::End { side, call, named });
        self.link.ended = Some(r.t0);
        if let Some(st) = self.link.stream.as_mut() {
            st.locked = false;
        }
    }

    fn put_char(&mut self, s: usize, c: u8, t: f64) {
        let Some(ch) = printable(c) else { return };
        let side = &mut self.link.sides[s];
        side.last_char_t = t;
        if let Some(line) = side.lines.push(ch) {
            self.emit_text(s, line);
        }
    }

    fn flush_side(&mut self, s: usize) {
        if let Some(line) = self.link.sides[s].lines.take() {
            self.emit_text(s, line);
        }
    }

    fn emit_text(&mut self, s: usize, text: String) {
        if self.link.pending.is_some() {
            if s == 0
                && self.link.sides[0].call.is_none()
                && let Some(call) = ident(&text)
            {
                self.link.sides[0].call = Some(call);
            }
            self.resolve_connect();
        }
        let (side, call) = (self.link.role(s), self.link.sides[s].call.clone());
        self.events.push(PactorEvent::Text { side, call, text });
    }

    /// Report the connect, with the caller's call if it has been heard.
    fn resolve_connect(&mut self) {
        if let Some(p) = self.link.pending.take() {
            let caller = self.link.sides[0].call.clone();
            self.events.push(PactorEvent::Connect { caller, called: p.called, longpath: p.longpath });
        }
    }

    fn housekeeping(&mut self) {
        let now = self.scan as f64;
        let r = self.rate;
        if let Some(p) = &self.link.pending
            && now > p.deadline
        {
            self.resolve_connect();
        }
        // While the cycle is followed, a pause is more likely repeats on
        // their way to a memory-ARQ sum than the sender stopping, and an
        // idle packet would say so.
        let wait = if self.link.stream.as_ref().is_some_and(|s| s.locked) { LINE_HOLD_S } else { LINE_FLUSH_S };
        for s in 0..2 {
            let side = &self.link.sides[s];
            if !side.lines.is_empty() && now - side.last_char_t > wait * r {
                self.flush_side(s);
            }
        }
        self.prov.retain(|p| now - p.t0 < PROVISIONAL_S * r);
        self.prov_calls.retain(|(_, t)| now - t < PROVISIONAL_S * r);
        if self.link.active {
            let ended = self.link.ended.is_some_and(|t| now - t > QRT_HOLD_S * r);
            if ended || now - self.link.last_t > LINK_IDLE_S * r {
                self.reset_link();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: a transmitter written from the protocol description.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtty::testsig::{Rng, chunks, noise_sigma, rayleigh};
    use std::f32::consts::FRAC_1_SQRT_2;
    use std::f64::consts::TAU as TAU64;

    const FS: f32 = 47_904.0;
    const CALLER: &str = "K1ABC";
    const CALLED: &str = "DL2XYZ";
    const CS1: u32 = 0x4d5;
    const CS2: u32 = 0xab2;
    const CS4: u32 = 0xd2c;
    /// From the end of a packet to the start of the answer: propagation
    /// and switching.
    const GAP: f64 = 0.03;

    fn bits_of(bytes: &[u8]) -> Vec<bool> {
        bytes.iter().flat_map(|&b| (0..8).map(move |i| (b >> i) & 1 == 1)).collect()
    }

    fn pack(bits: &[bool]) -> Vec<u8> {
        let mut v = vec![0u8; bits.len().div_ceil(8)];
        for (i, &b) in bits.iter().enumerate() {
            if b {
                v[i / 8] |= 1 << (i % 8);
            }
        }
        v
    }

    fn doubled(bits: &[bool]) -> Vec<bool> {
        bits.iter().flat_map(|&b| [b, b]).collect()
    }

    fn code(c: u8) -> &'static str {
        HUFFMAN.iter().find(|e| e.0 == c).map(|e| e.1).expect("every ASCII code is in the table")
    }

    fn swap_case(c: u8) -> u8 {
        if c.is_ascii_alphabetic() { c ^ 0x20 } else { c }
    }

    /// Huffman codes packed while they fit, then idle codes to the end,
    /// the last one cut off.
    fn huff_fill(text: &[u8], field: usize, swapped: bool) -> (Vec<u8>, usize) {
        let nbits = field * 8;
        let mut bits = Vec::with_capacity(nbits);
        let mut used = 0;
        for &c in text {
            let k = code(if swapped { swap_case(c) } else { c });
            if bits.len() + k.len() > nbits {
                break;
            }
            bits.extend(k.bytes().map(|b| b == b'1'));
            used += 1;
        }
        while bits.len() < nbits {
            for b in code(IDLE).bytes() {
                if bits.len() == nbits {
                    break;
                }
                bits.push(b == b'1');
            }
        }
        (pack(&bits), used)
    }

    /// A data field from the text: whichever of plain bytes, Huffman and
    /// case-swapped Huffman carries the most characters.
    fn fill(text: &[u8], field: usize) -> (Vec<u8>, u8, usize) {
        let (h, nh) = huff_fill(text, field, false);
        let (s, ns) = huff_fill(text, field, true);
        let na = text.len().min(field);
        if nh > na && nh >= ns {
            (h, 1, nh)
        } else if ns > na && ns > nh {
            (s, 2, ns)
        } else {
            let mut a = text[..na].to_vec();
            a.resize(field, IDLE);
            (a, 0, na)
        }
    }

    fn with_crc(mut b: Vec<u8>) -> Vec<u8> {
        let c = crc16(&b, 0);
        b.extend(c.to_le_bytes());
        b
    }

    fn call8(call: &str) -> Vec<u8> {
        let mut v = call.as_bytes().to_vec();
        v.resize(8, 0x0f);
        v
    }

    fn data_bits(header: u8, field: &[u8], status: u8) -> Vec<bool> {
        let mut body = field.to_vec();
        body.push(status);
        let mut bytes = vec![header];
        bytes.extend(with_crc(body));
        bits_of(&bytes)
    }

    fn cs_bits(cs: u32) -> Vec<bool> {
        (0..12).map(|k| (cs >> k) & 1 == 1).collect()
    }

    /// A changeover packet: CS3 at 100 baud where the header goes, then the
    /// shorter field.
    fn bk_bits(baud: f64, field: &[u8], status: u8) -> Vec<bool> {
        let mut body = field.to_vec();
        body.push(status);
        let body = bits_of(&with_crc(body));
        let mut b = if baud == 100.0 {
            let mut b = cs_bits(CS3);
            b.extend([false; 4]);
            b
        } else {
            doubled(&cs_bits(CS3))
        };
        b.extend(body);
        b
    }

    /// Header and call at 100 baud, then the call's first six characters
    /// at 200.
    fn connect_segs(call: &str) -> Vec<(f64, Vec<bool>)> {
        let c = call8(call);
        let mut a = vec![0x55u8];
        a.extend(&c);
        vec![(100.0, bits_of(&a)), (200.0, bits_of(&c[..6]))]
    }

    /// A QRT packet addressed to `to`, its call sent backward.
    fn qrt_bits(baud: f64, header: u8, counter: u8, to: &str) -> Vec<bool> {
        let c = call8(to);
        let rev: Vec<u8> = c[..7].iter().rev().copied().collect();
        let status = ST_QRT | counter;
        if baud == 100.0 {
            let mut f = rev;
            f.push(header);
            data_bits(header, &f, status)
        } else {
            let mut fb = vec![false; 8];
            fb.extend(doubled(&bits_of(&rev)));
            fb.extend(doubled(&bits_of(&[header])));
            fb.extend([false; 24]);
            data_bits(header, &pack(&fb), status)
        }
    }

    struct Burst {
        t: f64,
        segs: Vec<(f64, Vec<bool>)>,
        station: usize,
        gain: f32,
    }

    /// Everything sent, each station inverting its shift with every
    /// transmission.
    #[derive(Default)]
    struct Air {
        bursts: Vec<Burst>,
        pol: [bool; 2],
    }

    impl Air {
        fn send(&mut self, station: usize, t: f64, segs: Vec<(f64, Vec<bool>)>, gain: f32) {
            let inv = self.pol[station];
            self.pol[station] = !inv;
            let segs = segs.into_iter().map(|(b, bits)| (b, bits.into_iter().map(|x| x ^ inv).collect())).collect();
            self.bursts.push(Burst { t, segs, station, gain });
        }

        fn end(&self) -> f64 {
            self.bursts
                .iter()
                .map(|b| b.t + b.segs.iter().map(|(baud, bits)| bits.len() as f64 / baud).sum::<f64>())
                .fold(0.0, f64::max)
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Chan {
        Awgn,
        /// Two Rayleigh paths per station: delay in ms, spread in Hz.
        Fading(f32, f32),
    }

    #[derive(Clone, Copy, Debug)]
    struct Scene {
        centers: [f32; 2],
        amps: [f32; 2],
        /// The receiver's sample clock error.
        ppm: f64,
        chan: Chan,
        /// Against a sine of amplitude one, in 2500 Hz.
        snr_db: f32,
        seed: u64,
    }

    impl Scene {
        fn plain(center: f32, snr_db: f32) -> Self {
            Self { centers: [center, center + 9.0], amps: [1.0, 0.8], ppm: 0.0, chan: Chan::Awgn, snr_db, seed: 1 }
        }
    }

    /// The air as sideband audio at the receiver.
    fn render(air: &Air, sc: &Scene, tail_s: f64) -> Vec<f32> {
        let fs_true = FS as f64 * (1.0 + sc.ppm * 1e-6);
        let n = ((air.end() + tail_s) * fs_true) as usize;
        let mut rng = Rng::new(sc.seed);
        let mut out = vec![0.0f32; n];
        for s in 0..2 {
            let mut z = vec![Complex32::new(0.0, 0.0); n];
            for b in air.bursts.iter().filter(|b| b.station == s) {
                let mut ph = 0.0f64;
                let mut t_seg = b.t;
                for (baud, bits) in &b.segs {
                    let dur = bits.len() as f64 / baud;
                    let m0 = (t_seg * fs_true).ceil() as usize;
                    let m1 = (((t_seg + dur) * fs_true).ceil() as usize).min(n);
                    for (m, zm) in z.iter_mut().enumerate().take(m1).skip(m0) {
                        let tau = m as f64 / fs_true - t_seg;
                        let k = ((tau * baud) as usize).min(bits.len() - 1);
                        let f = sc.centers[s] as f64 + if bits[k] { 100.0 } else { -100.0 };
                        *zm = Complex32::new(ph.cos() as f32, ph.sin() as f32) * b.gain;
                        ph = (ph + TAU64 * f / fs_true) % TAU64;
                    }
                    t_seg += dur;
                }
            }
            let a = sc.amps[s];
            match sc.chan {
                Chan::Awgn => out.iter_mut().zip(&z).for_each(|(o, v)| *o += a * v.re),
                Chan::Fading(delay_ms, spread) => {
                    let g1 = rayleigh(n, fs_true as f32, spread, &mut rng);
                    let g2 = rayleigh(n, fs_true as f32, spread, &mut rng);
                    let d = (delay_ms as f64 * 1e-3 * fs_true) as usize;
                    let k = a * FRAC_1_SQRT_2;
                    for m in 0..n {
                        let late = if m >= d { z[m - d] * g2[m] } else { Complex32::new(0.0, 0.0) };
                        out[m] += k * (z[m] * g1[m] + late).re;
                    }
                }
            }
        }
        let sigma = noise_sigma(1.0, sc.snr_db, FS);
        out.iter_mut().for_each(|v| *v += sigma * rng.gauss());
        out
    }

    #[derive(Clone, Copy)]
    enum Answer {
        Ack,
        SpeedUp,
    }

    /// A scripted ARQ link. The IRS's answers are written into the script:
    /// what it would have heard is not simulated.
    struct Arq {
        air: Air,
        /// Start of the current cycle's packet.
        t: f64,
        iss: usize,
        baud: f64,
        counter: u8,
        header: u8,
        text: [VecDeque<u8>; 2],
        /// What each side's IRS received, in order: what a listener should
        /// print.
        sent: [Vec<u8>; 2],
        cs: u32,
        /// Copies of each packet `drain` sends, and the level of the
        /// changeover packet.
        copies: usize,
        bk_gain: f32,
    }

    impl Arq {
        fn new() -> Self {
            Self {
                air: Air::default(),
                t: 1.0,
                iss: 0,
                baud: 100.0,
                counter: 0,
                header: 0x55,
                text: [VecDeque::new(), VecDeque::new()],
                sent: [Vec::new(), Vec::new()],
                cs: CS1,
                copies: 1,
                bk_gain: 1.0,
            }
        }

        fn say(&mut self, side: usize, text: &str) {
            self.text[side].extend(text.bytes());
        }

        fn irs(&self) -> usize {
            1 - self.iss
        }

        /// The IRS answers the packet just sent, and the cycle moves on.
        fn answer(&mut self, cs: u32) {
            let t = self.t + PACKET_S + GAP;
            let irs = self.irs();
            self.air.send(irs, t, vec![(100.0, cs_bits(cs))], 1.0);
            self.t += CYCLE_S;
        }

        fn next_ack(&mut self) -> u32 {
            self.cs = if self.cs == CS1 { CS2 } else { CS1 };
            self.cs
        }

        /// Connect frames until the called station answers (with CS4: the
        /// 200 baud part was not good enough, so the link starts at 100).
        fn connect(&mut self, cycles: usize) {
            for i in 0..cycles {
                self.air.send(0, self.t, connect_segs(CALLED), 1.0);
                if i + 1 == cycles {
                    self.answer(CS4);
                } else {
                    self.t += CYCLE_S;
                }
            }
            self.header = 0x55;
            self.counter = 0;
        }

        /// The next packet from the ISS's text, and the characters in it.
        fn next_packet(&mut self, new: bool) -> (Vec<bool>, Vec<u8>) {
            let field = if self.baud == 100.0 { 8 } else { 20 };
            let text: Vec<u8> = self.text[self.iss].iter().copied().collect();
            let (f, mode, used) = fill(&text, field);
            if new {
                self.counter = (self.counter + 1) & 3;
                self.header = !self.header;
            }
            let status = self.counter | (mode << 2);
            let chars: Vec<u8> = self.text[self.iss].drain(..used).collect();
            (data_bits(self.header, &f, status), chars)
        }

        /// A packet sent `copies` times (the IRS asking for repeats by
        /// repeating its CS) at `gain`, then answered.
        fn packet(&mut self, copies: usize, gain: f32, answer: Answer) {
            let (bits, chars) = self.next_packet(true);
            for i in 0..copies {
                self.air.send(self.iss, self.t, vec![(self.baud, bits.clone())], gain);
                if i + 1 < copies {
                    let cs = self.cs;
                    self.answer(cs);
                }
            }
            self.sent[self.iss].extend(chars);
            match answer {
                Answer::Ack => {
                    let cs = self.next_ack();
                    self.answer(cs);
                }
                Answer::SpeedUp => {
                    self.answer(CS4);
                    self.baud = 200.0;
                }
            }
        }

        /// Packets until the ISS has nothing left to say, the last one
        /// answered with `last`.
        fn drain(&mut self, last: Answer) {
            while !self.text[self.iss].is_empty() {
                let field = if self.baud == 100.0 { 8 } else { 20 };
                let v: Vec<u8> = self.text[self.iss].iter().copied().collect();
                let (_, _, used) = fill(&v, field);
                let answer = if used == v.len() { last } else { Answer::Ack };
                self.packet(self.copies, 1.0, answer);
            }
        }

        /// A 200 baud packet the IRS rejects with CS4: what it carried goes
        /// again at 100 baud, the first packet under the same counter, the
        /// header reset to 55.
        fn rejected(&mut self) {
            assert_eq!(self.baud, 200.0);
            let (bits, chars) = self.next_packet(true);
            self.air.send(self.iss, self.t, vec![(200.0, bits)], 1.0);
            self.answer(CS4);
            self.baud = 100.0;
            for c in chars.into_iter().rev() {
                self.text[self.iss].push_front(c);
            }
            self.header = 0x55;
            let (bits, chars) = self.next_packet(false);
            self.air.send(self.iss, self.t, vec![(100.0, bits)], 1.0);
            self.sent[self.iss].extend(chars);
            let cs = self.next_ack();
            self.answer(cs);
        }

        /// The IRS breaks in: its changeover packet goes where its CS would,
        /// and the cycle follows it.
        fn break_in(&mut self) {
            let new = self.irs();
            let t = self.t + PACKET_S + GAP;
            let field = if self.baud == 100.0 { 7 } else { 18 };
            let text: Vec<u8> = self.text[new].iter().copied().collect();
            let (f, mode, used) = fill(&text, field);
            let chars: Vec<u8> = self.text[new].drain(..used).collect();
            self.air.send(new, t, vec![(self.baud, bk_bits(self.baud, &f, mode << 2))], self.bk_gain);
            self.sent[new].extend(chars);
            self.iss = new;
            self.counter = 0;
            // The packet after a changeover packet has header 55.
            self.header = 0xaa;
            self.t = t;
            self.cs = CS2;
            let cs = self.next_ack();
            self.answer(cs);
        }

        fn qrt(&mut self, copies: usize) {
            let to = if self.iss == 0 { CALLED } else { CALLER };
            self.counter = (self.counter + 1) & 3;
            self.header = !self.header;
            let bits = qrt_bits(self.baud, self.header, self.counter, to);
            for i in 0..copies {
                self.air.send(self.iss, self.t, vec![(self.baud, bits.clone())], 1.0);
                let cs = if i + 1 == copies { self.next_ack() } else { self.cs };
                self.answer(cs);
            }
        }
    }

    const CALLER_TEXT: [&str; 6] = [
        "1K1ABC\rhello om, this is a test of the pactor link at 100 baud.\r",
        "QSL UR 599 IN MASS ES TNX FER THE CALL\r",
        "now at two hundred baud the packets carry twenty bytes each.\r[[{{~~}}]] <<>> ##\r",
        "this line goes at 200 and then again at 100.\r",
        "memory arq got this.\r",
        "back to normal copy, over.\r",
    ];
    const CALLED_TEXT: &str = "dl2xyz here, thanks for the test.\r73 and qrt.\r";

    /// Connect at 100 baud, text, a speed-up, case-swapped Huffman and
    /// plain bytes at 200, a packet rejected at 200 and sent again at 100,
    /// a packet that only memory ARQ recovers (`weak` copies at `gain`),
    /// a changeover, and a QRT.
    fn full_link(weak: usize, gain: f32) -> Arq {
        full_link_with(weak, gain, 1, 1.0)
    }

    /// The same with every packet sent `copies` times and the changeover
    /// packet at `bk_gain`.
    fn full_link_with(weak: usize, gain: f32, copies: usize, bk_gain: f32) -> Arq {
        let mut l = Arq::new();
        l.copies = copies;
        l.bk_gain = bk_gain;
        l.connect(3);
        l.say(0, CALLER_TEXT[0]);
        l.drain(Answer::SpeedUp);
        assert_eq!(l.baud, 200.0);
        l.say(0, CALLER_TEXT[1]);
        l.drain(Answer::Ack);
        l.say(0, CALLER_TEXT[2]);
        l.drain(Answer::Ack);
        l.say(0, CALLER_TEXT[3]);
        l.rejected();
        l.drain(Answer::Ack);
        l.say(0, CALLER_TEXT[4]);
        l.packet(weak, gain, Answer::Ack);
        l.drain(Answer::Ack);
        l.say(0, CALLER_TEXT[5]);
        l.drain(Answer::Ack);
        l.say(1, CALLED_TEXT);
        l.break_in();
        l.drain(Answer::Ack);
        l.qrt(2);
        l
    }

    fn lines(text: &[u8]) -> Vec<String> {
        String::from_utf8_lossy(text)
            .split(['\r', '\n'])
            .map(|s| s.trim_end().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    fn run(audio: &[f32]) -> (Vec<PactorEvent>, PactorStatus) {
        let mut d = PactorDecoder::new(FS);
        let mut ev = Vec::new();
        chunks(audio, |c| {
            d.push(c);
            ev.extend(d.take());
        });
        (ev, d.status())
    }

    fn texts(ev: &[PactorEvent], want: PactorSide) -> Vec<String> {
        ev.iter()
            .filter_map(|e| match e {
                PactorEvent::Text { side, text, .. } if *side == want => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn show(ev: &[PactorEvent]) {
        for e in ev {
            eprintln!("  {:12} {}", e.kind(), e.summary());
        }
    }

    /// `a` appears in `b` in order: nothing in between invented.
    fn subsequence(a: &str, b: &str) -> bool {
        let mut it = b.chars();
        a.chars().all(|c| it.any(|d| d == c))
    }

    // -- Tables.

    #[test]
    fn pactor_crc_is_x25() {
        assert_eq!(crc16(b"123456789", 0), 0x906e);
        let body = with_crc(vec![1, 2, 3, 0x55]);
        assert!(crc_ok(&body, 0));
        let inv: Vec<u8> = body.iter().map(|b| !b).collect();
        assert!(crc_ok(&inv, 0xff));
        assert!(!crc_ok(&inv, 0));
    }

    #[test]
    fn pactor_huffman_table_is_a_complete_prefix_code() {
        let mut seen = [false; 128];
        let mut kraft = 0.0f64;
        for (sym, c) in HUFFMAN {
            assert!(!seen[sym as usize], "{sym} twice");
            seen[sym as usize] = true;
            assert!((2..=15).contains(&c.len()));
            kraft += 0.5f64.powi(c.len() as i32);
            for (s2, c2) in HUFFMAN {
                assert!(s2 == sym || !c2.starts_with(c), "{c} is a prefix of {c2}");
            }
        }
        assert!(seen.iter().all(|&s| s));
        assert!((kraft - 1.0).abs() < 1e-12);
        let h = Huffman::new();
        let text = b"Hello World, 73 de K1ABC: [pactor] ~ {test}\r\n";
        let (f, used) = huff_fill(text, 40, false);
        let mut out = Vec::new();
        h.decode(&f, &mut out);
        out.retain(|&b| b != IDLE);
        assert_eq!(&out[..], &text[..used]);
    }

    // -- Links.

    fn check_full_link(l: &Arq, ev: &[PactorEvent], st: &PactorStatus) {
        show(ev);
        eprintln!("  {st:?}");
        assert_eq!(texts(ev, PactorSide::Caller), lines(&l.sent[0]));
        assert_eq!(texts(ev, PactorSide::Called), lines(&l.sent[1]));
        let connect = ev.iter().position(|e| matches!(e, PactorEvent::Connect { .. })).expect("connect");
        assert_eq!(
            ev[connect],
            PactorEvent::Connect { caller: Some(CALLER.into()), called: CALLED.into(), longpath: false }
        );
        let speeds: Vec<(u16, u16)> = ev
            .iter()
            .filter_map(|e| match e {
                PactorEvent::SpeedChange { baud, previous, .. } => Some((*previous, *baud)),
                _ => None,
            })
            .collect();
        assert_eq!(speeds, vec![(100, 200), (200, 100)]);
        let changes: Vec<&PactorEvent> = ev.iter().filter(|e| matches!(e, PactorEvent::Changeover { .. })).collect();
        assert_eq!(changes, vec![&PactorEvent::Changeover { side: PactorSide::Called, call: Some(CALLED.into()) }]);
        let end = ev.last().expect("events");
        assert_eq!(
            *end,
            PactorEvent::End { side: PactorSide::Called, call: Some(CALLED.into()), named: Some(CALLER.into()) }
        );
        assert!(st.packets_memory_arq >= 1, "the weak packet should need memory ARQ");
        assert_eq!(st.caller.as_deref(), Some(CALLER));
        assert_eq!(st.called.as_deref(), Some(CALLED));
    }

    #[test]
    fn pactor_full_link_offset_and_clock_error() {
        let l = full_link(6, 0.19);
        let mut sc = Scene::plain(1550.0, 6.0);
        sc.ppm = 100.0;
        let audio = render(&l.air, &sc, 3.0);
        let (ev, st) = run(&audio);
        check_full_link(&l, &ev, &st);
    }

    #[test]
    fn pactor_full_link_low_and_slow_clock() {
        let l = full_link(6, 0.19);
        let mut sc = Scene::plain(1450.0, 6.0);
        sc.ppm = -100.0;
        sc.seed = 7;
        let audio = render(&l.air, &sc, 3.0);
        let (ev, st) = run(&audio);
        check_full_link(&l, &ev, &st);
    }

    #[test]
    fn pactor_weak_packet_fails_alone() {
        // The same link with the weak packet sent once: it cannot be read,
        // so its text is missing and nothing wrong is printed in its place.
        let l = full_link(1, 0.19);
        let audio = render(&l.air, &Scene::plain(1500.0, 6.0), 3.0);
        let (ev, st) = run(&audio);
        show(&ev);
        let got = texts(&ev, PactorSide::Caller).concat();
        let want = lines(&l.sent[0]).concat();
        assert!(subsequence(&got, &want), "{got:?}");
        assert!(got.len() < want.len());
        assert_eq!(st.packets_memory_arq, 0);
        assert!(st.packets_failed >= 1);
    }

    /// What each side printed is what it sent, in order, with at least
    /// `share` of it there.
    fn check_copy(l: &Arq, ev: &[PactorEvent], sides: [PactorSide; 2], share: f32) {
        for (side, sent) in [(sides[0], &l.sent[0]), (sides[1], &l.sent[1])] {
            let got = texts(ev, side).concat();
            let want = lines(sent).concat();
            assert!(subsequence(&got, &want), "{side:?} printed what was not sent: {got:?}");
            assert!(got.len() as f32 >= share * want.len() as f32, "{side:?}: {} of {}", got.len(), want.len());
        }
    }

    #[test]
    fn pactor_full_link_through_fading() {
        // CCIR moderate on both paths, every packet sent twice (as an IRS
        // in the same fading would ask). The scripted IRS does not hear the
        // fades, so the listener may miss text the IRS got; what it prints
        // must still be what was sent.
        for (seed, ch) in [(3, Chan::Fading(1.0, 0.5)), (4, Chan::Fading(2.0, 1.0))] {
            let l = full_link_with(6, 0.3, 2, 1.0);
            let mut sc = Scene::plain(1460.0, 15.0);
            sc.chan = ch;
            sc.ppm = -100.0;
            sc.seed = seed;
            let audio = render(&l.air, &sc, 3.0);
            let (ev, st) = run(&audio);
            eprintln!("{ch:?}");
            show(&ev);
            eprintln!("  {st:?}");
            check_copy(&l, &ev, [PactorSide::Caller, PactorSide::Called], 0.8);
            assert!(ev.iter().any(|e| matches!(e, PactorEvent::Connect { called, .. } if called == CALLED)));
            assert!(ev.iter().any(|e| matches!(e, PactorEvent::SpeedChange { baud: 200, .. })));
            assert!(ev.iter().any(|e| matches!(e, PactorEvent::Changeover { side: PactorSide::Called, .. })));
            assert!(ev.iter().any(|e| matches!(e, PactorEvent::End { side: PactorSide::Called, .. })));
        }
    }

    #[test]
    fn pactor_changeover_without_its_packet() {
        // The changeover packet is lost; the called station's packets
        // arriving on a different phase of the cycle still show who sends.
        let l = full_link_with(6, 0.19, 1, 0.05);
        let audio = render(&l.air, &Scene::plain(1500.0, 6.0), 3.0);
        let (ev, st) = run(&audio);
        show(&ev);
        eprintln!("  {st:?}");
        assert_eq!(texts(&ev, PactorSide::Caller), lines(&l.sent[0]));
        check_copy(&l, &ev, [PactorSide::Caller, PactorSide::Called], 0.7);
        let changes: Vec<&PactorEvent> = ev.iter().filter(|e| matches!(e, PactorEvent::Changeover { .. })).collect();
        assert_eq!(changes, vec![&PactorEvent::Changeover { side: PactorSide::Called, call: Some(CALLED.into()) }]);
        assert!(matches!(ev.last(), Some(PactorEvent::End { side: PactorSide::Called, .. })));
    }

    #[test]
    fn pactor_joins_a_link_in_progress() {
        // Tuned in after the connect: the stations have no names until the
        // QRT packet names the one it is addressed to.
        let l = full_link(6, 0.19);
        let audio = render(&l.air, &Scene::plain(1500.0, 6.0), 3.0);
        let (ev, st) = run(&audio[(20.0 * FS) as usize..]);
        show(&ev);
        eprintln!("  {st:?}");
        assert!(!ev.iter().any(|e| matches!(e, PactorEvent::Connect { .. })));
        // Most of the caller's text went by before the receiver was tuned.
        check_copy(&l, &ev, [PactorSide::First, PactorSide::Second], 0.2);
        let tail = lines(&l.sent[0]).concat();
        assert!(tail.ends_with(&texts(&ev, PactorSide::First).concat()));
        assert_eq!(texts(&ev, PactorSide::Second), lines(&l.sent[1]));
        assert!(ev.iter().any(|e| matches!(e, PactorEvent::Changeover { side: PactorSide::Second, call: None })));
        assert_eq!(
            ev.last(),
            Some(&PactorEvent::End { side: PactorSide::Second, call: None, named: Some(CALLER.into()) })
        );
        assert_eq!(st.caller.as_deref(), Some(CALLER));
    }

    /// Unproto: every packet sent `copies` times back to back, no answers.
    fn unproto(parts: &[(f64, &str)], copies: usize) -> (Air, Vec<u8>) {
        let mut air = Air::default();
        let mut t = 1.0;
        let (mut counter, mut header) = (0u8, 0x55u8);
        let mut sent = Vec::new();
        for &(baud, text) in parts {
            let mut q: VecDeque<u8> = text.bytes().collect();
            while !q.is_empty() {
                let field = if baud == 100.0 { 8 } else { 20 };
                let v: Vec<u8> = q.iter().copied().collect();
                let (f, mode, used) = fill(&v, field);
                sent.extend(q.drain(..used));
                counter = (counter + 1) & 3;
                header = !header;
                let bits = data_bits(header, &f, counter | (mode << 2));
                for _ in 0..copies {
                    air.send(0, t, vec![(baud, bits.clone())], 1.0);
                    t += UNPROTO_CYCLE_S;
                }
            }
        }
        (air, sent)
    }

    const UNPROTO_TEXT: [(f64, &str); 2] = [
        (100.0, "cq cq cq de k1abc k1abc unproto test\r"),
        (200.0, "this part of the bulletin goes at 200 baud, twice each.\rk1abc sk\r"),
    ];

    #[test]
    fn pactor_unproto() {
        let (air, sent) = unproto(&UNPROTO_TEXT, 2);
        let mut sc = Scene::plain(1800.0, 3.0);
        sc.ppm = 60.0;
        let audio = render(&air, &sc, 3.0);
        let (ev, st) = run(&audio);
        show(&ev);
        eprintln!("  {st:?}");
        assert_eq!(texts(&ev, PactorSide::Unproto), lines(&sent));
        assert!(ev.iter().all(|e| matches!(e, PactorEvent::Text { .. } | PactorEvent::SpeedChange { .. })));
        assert_eq!(st.mode, Some(PactorMode::Unproto));
        assert!(st.repeats >= 1);
    }

    #[test]
    fn pactor_first_packet_is_not_lost() {
        // Each packet sent once: the first is over before the pair has been
        // found, and is read from the baseband kept for the lock.
        let (air, sent) = unproto(&UNPROTO_TEXT, 1);
        let audio = render(&air, &Scene::plain(1500.0, 6.0), 3.0);
        let (ev, _) = run(&audio);
        show(&ev);
        assert_eq!(texts(&ev, PactorSide::Unproto), lines(&sent));
    }

    #[test]
    fn pactor_unproto_through_fading() {
        let (air, sent) = unproto(&UNPROTO_TEXT, 3);
        let mut sc = Scene::plain(900.0, 10.0);
        sc.chan = Chan::Fading(2.0, 1.0);
        sc.seed = 11;
        let audio = render(&air, &sc, 3.0);
        let (ev, st) = run(&audio);
        show(&ev);
        eprintln!("  {st:?}");
        let got = texts(&ev, PactorSide::Unproto).concat();
        let want = lines(&sent).concat();
        assert!(subsequence(&got, &want), "{got:?}");
        assert!(got.len() as f32 >= 0.7 * want.len() as f32, "{} of {}", got.len(), want.len());
    }

    // -- Nothing from nothing.

    #[test]
    fn pactor_noise_alone_prints_nothing() {
        let mut rng = Rng::new(5);
        let audio: Vec<f32> = (0..(60.0 * FS) as usize).map(|_| 0.3 * rng.gauss()).collect();
        let (ev, st) = run(&audio);
        assert!(ev.is_empty(), "{ev:?}");
        assert_eq!(st.packets_ok, 0);
    }

    /// PACTOR-shaped bursts on the ARQ cycle, headers right and every CRC
    /// wrong, with control signals between, twenty cycles at a speed.
    fn bad_crcs(cycles: usize, seed: u64) -> Air {
        let mut air = Air::default();
        let mut rng = Rng::new(seed);
        let mut header = 0x55u8;
        for k in 0..cycles {
            let t = 1.0 + k as f64 * CYCLE_S;
            let baud = if (k / 20) % 2 == 0 { 100.0 } else { 200.0 };
            let n = if baud == 100.0 { 11 } else { 23 };
            let mut bytes = vec![header];
            bytes.extend((0..n).map(|_| (rng.uniform() * 256.0) as u8));
            if crc_ok(&bytes[1..], 0) || crc_ok(&bytes[1..], 0xff) {
                bytes[n] ^= 1;
            }
            header = !header;
            air.send(0, t, vec![(baud, bits_of(&bytes))], 1.0);
            air.send(1, t + PACKET_S + GAP, vec![(100.0, cs_bits(CS1))], 1.0);
        }
        air
    }

    #[test]
    fn pactor_packets_with_bad_crcs_print_nothing() {
        let audio = render(&bad_crcs(48, 9), &Scene::plain(1500.0, 10.0), 2.0);
        let (ev, st) = run(&audio);
        assert!(ev.is_empty(), "{ev:?}");
        assert_eq!(st.packets_ok, 0);
        assert!(st.center_hz.is_some(), "the pair should have been found");
    }

    /// The long version, run by hand (`cargo test --release -p
    /// scannerd-engine pactor_soak -- --ignored`, about 15 s): the full
    /// link over ten noise seeds, offsets and both clock errors, and twenty
    /// minutes of bursts with bad CRCs.
    #[test]
    #[ignore]
    fn pactor_soak() {
        let l = full_link(6, 0.19);
        let mut bad = 0;
        for seed in 1..=10 {
            let mut sc = Scene::plain(1500.0 + 7.0 * seed as f32, 6.0);
            sc.seed = 100 + seed;
            sc.ppm = if seed % 2 == 0 { 100.0 } else { -100.0 };
            let audio = render(&l.air, &sc, 3.0);
            let (ev, st) = run(&audio);
            let ok = texts(&ev, PactorSide::Caller) == lines(&l.sent[0])
                && texts(&ev, PactorSide::Called) == lines(&l.sent[1]);
            eprintln!("seed {seed}: copy {ok}, memory ARQ {}, failed {}", st.packets_memory_arq, st.packets_failed);
            if !ok {
                bad += 1;
                show(&ev);
            }
        }
        assert_eq!(bad, 0);
        let audio = render(&bad_crcs(960, 99), &Scene::plain(1500.0, 3.0), 2.0);
        let (ev, st) = run(&audio);
        assert!(ev.is_empty(), "{ev:?}");
        assert_eq!(st.packets_ok, 0);
    }

    #[test]
    fn pactor_reports_snr() {
        // Unproto at 200 baud back to back: the signal is on all the time.
        let text: String = "the quick brown fox jumps over the lazy dog. ".repeat(12);
        for snr in [0.0f32, 10.0] {
            let (air, _) = unproto(&[(200.0, &text)], 1);
            let audio = render(&air, &Scene::plain(1500.0, snr), 0.0);
            let mut d = PactorDecoder::new(FS);
            d.push(&audio);
            let st = d.status();
            eprintln!("  {snr} dB: {:?}", st.snr_db);
            let got = st.snr_db.expect("locked");
            assert!((got - snr).abs() < 2.0, "{snr} dB read as {got}");
        }
    }

    #[test]
    fn pactor_realtime_factor() {
        let l = full_link(6, 0.19);
        let audio = render(&l.air, &Scene::plain(1500.0, 6.0), 3.0);
        let mut d = PactorDecoder::new(FS);
        let t = std::time::Instant::now();
        chunks(&audio, |c| d.push(c));
        let el = t.elapsed().as_secs_f64();
        let secs = audio.len() as f64 / FS as f64;
        let rtf = el / secs;
        eprintln!("pactor: {secs:.1} s of audio in {el:.3} s, real-time factor {rtf:.4}");
        if !cfg!(debug_assertions) {
            assert!(rtf < 0.05, "real-time factor {rtf}");
        }
    }
}
