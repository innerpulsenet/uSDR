//! P25 Phase 1 C4FM: demodulation, frame synchronisation, and the NID.
//!
//! A trunked system's control channel is the map of everything else on it, and
//! it is reachable without any of the parts that make P25 voice hard. Control
//! channels use **C4FM Phase 1 signalling even on Phase II systems**, so there
//! is no TDMA here, and signalling carries no speech, so there is no vocoder —
//! the patent-encumbered wall that stops most from-scratch P25 work sits
//! entirely on the voice side.
//!
//! C4FM is 4-level FSK at 4800 baud with deviations of ±600 and ±1800 Hz. An FM
//! discriminator recovers the instantaneous frequency directly, so the chain is
//! discriminator → matched filter → correlate for the 48-bit frame sync → slice
//! the 64-bit NID that follows it.
//!
//! One measurement shapes the whole design: this host's dongles are within
//! ±3 ppm, so the sample clock drifts about **0.012 symbols per second**. Over a
//! frame that is invisible. A full timing-recovery loop would be solving a
//! problem the hardware does not have, so symbol phase is instead taken from
//! each frame's own sync correlation, which is both simpler and exact.

pub mod conventional;
pub mod data;
pub mod phase2;
mod rs64;
pub mod tsbk;

pub use data::{PduHeader, PduMessage, decode_pdu, decode_pdu_header};
pub use tsbk::{ChannelPlan, Convention, Tsbk, TsbkEvent, opcode_name};

use num_complex::Complex32;
use scannerd_dsp::OnePole;
use std::f32::consts::{PI, TAU};

pub const SYMBOL_RATE: f64 = 4800.0;
/// Samples per symbol. Ten divides 2.4 MS/s exactly (÷50 → 48 kHz).
pub const SPS: usize = 10;
pub const CHANNEL_RATE: f64 = SYMBOL_RATE * SPS as f64;

/// Sample rate for a dongle that holds one 12.5 kHz P25 channel — the
/// control channel, or a followed voice grant.
///
/// The air is one 12.5 kHz allocation, offset 100 kHz from the LO so the
/// DC spike misses it. 960 kHz is the slowest exact multiple of
/// [`CHANNEL_RATE`] that keeps the analog IF honest: at 240 kS/s the
/// 412 kHz filter the offset demands cannot fit the ±120 kHz Nyquist
/// window, so the R820T's narrowest 300 kHz filter left 120–150 kHz of
/// spectrum to alias straight onto the channel — and the RTL2832U's
/// low-rate decimation barely rejects aliases at all. Here Nyquist is
/// ±480 kHz, the 600 kHz analog filter sits entirely inside it, and the
/// ADC still sees 2.5× less noise bandwidth than the old 2.4 MS/s.
pub const CONTROL_RATE: f64 = 960_000.0;
/// Same rate as [`CONTROL_RATE`]: voice following is also one 12.5 kHz
/// channel at a time.
pub const VOICE_RATE: f64 = CONTROL_RATE;

/// LO offset that keeps the dongle DC spike out of a 12.5 kHz channel.
/// 100 kHz clears DC (analog uses 60 kHz) and still sits well inside a
/// 600 kHz analog IF; 250 kHz sat past the R820T's 300 kHz 3 dB point.
pub const TUNE_OFFSET_HZ: f64 = 100_000.0;

/// Channel filter for C4FM. The allocation is 12.5 kHz; a tighter IF
/// needs a proven AFC and we do not have one on the live E4000 yet
/// (residual wanders ~1.5 kHz and the outer symbols are only ±1800 Hz).
pub const C4FM_BANDWIDTH_HZ: f32 = 12_500.0;

/// Outer and inner symbol deviations, in Hz.
pub const DEV_OUTER_HZ: f32 = 1800.0;
pub const DEV_INNER_HZ: f32 = 600.0;

/// The 48-bit frame synchronisation pattern (TIA-102.BAAA).
pub const FRAME_SYNC: u64 = 0x5575_F5FF_77FF;
/// Frame sync length in symbols.
pub const SYNC_SYMBOLS: usize = 24;
/// The Network Identifier is 64 bits — 32 dibits — immediately after the sync.
pub const NID_SYMBOLS: usize = 32;

/// Normalised correlation a sync must reach to be believed.
///
/// The sync uses only the outer levels, which makes its correlation peak sharp;
/// 0.7 rejects noise while tolerating a couple of corrupted symbols.
pub const SYNC_THRESHOLD: f32 = 0.70;

/// Data-unit types that can follow a sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Duid {
    /// Header, before voice.
    Header,
    /// Terminator without link control.
    Terminator,
    /// Logical link data unit 1 — voice.
    Ldu1,
    /// Trunking signalling — what a control channel is made of.
    Tsdu,
    /// Logical link data unit 2 — voice.
    Ldu2,
    /// Packet data.
    Pdu,
    /// Terminator with link control.
    TerminatorLc,
    Unknown(u8),
}

impl Duid {
    /// How many symbols this kind of frame occupies on the air, at most.
    ///
    /// P25 frames are not all the same length, so knowing the DUID tells you
    /// where the next one can begin. Figures follow op25's table, which is in
    /// bits; a symbol carries a dibit, hence the halving. A TSDU's 720 bits are
    /// its longest form — sync, NID and three signalling blocks — and a
    /// terminator is only 144.
    pub fn max_symbols(&self) -> usize {
        match self {
            Duid::Header => 792 / 2,
            Duid::Terminator => 144 / 2,
            Duid::Ldu1 | Duid::Ldu2 => 1728 / 2,
            Duid::Tsdu => 720 / 2,
            Duid::Pdu => 962 / 2,
            Duid::TerminatorLc => 432 / 2,
            Duid::Unknown(_) => SYNC_SYMBOLS + NID_SYMBOLS,
        }
    }

    pub fn from_bits(v: u8) -> Self {
        match v {
            0x0 => Duid::Header,
            0x3 => Duid::Terminator,
            0x5 => Duid::Ldu1,
            0x7 => Duid::Tsdu,
            0xA => Duid::Ldu2,
            0xC => Duid::Pdu,
            0xF => Duid::TerminatorLc,
            other => Duid::Unknown(other),
        }
    }

    /// Whether this is a value the standard defines. A run of undefined DUIDs
    /// means the sync is landing on noise.
    pub fn is_known(&self) -> bool {
        !matches!(self, Duid::Unknown(_))
    }

    pub fn label(&self) -> String {
        match self {
            Duid::Header => "HDU".into(),
            Duid::Terminator => "TDU".into(),
            Duid::Ldu1 => "LDU1".into(),
            Duid::Tsdu => "TSDU".into(),
            Duid::Ldu2 => "LDU2".into(),
            Duid::Pdu => "PDU".into(),
            Duid::TerminatorLc => "TDULC".into(),
            Duid::Unknown(v) => format!("?{v:X}"),
        }
    }
}

/// GF(2^6) primitive polynomial x^6 + x + 1, as used by the NID's BCH code.
const GF_POLY: u16 = 0x43;

/// Exponential and logarithm tables for GF(64).
fn gf_tables() -> &'static ([u8; 64], [u8; 64]) {
    static TABLES: std::sync::OnceLock<([u8; 64], [u8; 64])> = std::sync::OnceLock::new();
    TABLES.get_or_init(|| {
        let mut exp = [0u8; 64];
        let mut log = [0u8; 64];
        let mut x: u16 = 1;
        for i in 0..63 {
            exp[i] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x40 != 0 {
                x ^= GF_POLY;
            }
        }
        exp[63] = exp[0];
        (exp, log)
    })
}

fn gf_inv(a: u8) -> u8 {
    let (exp, log) = gf_tables();
    assert!(a != 0, "zero has no inverse");
    exp[((63 - u16::from(log[a as usize])) % 63) as usize]
}

fn gf_div(a: u8, b: u8) -> u8 {
    if a == 0 { 0 } else { gf_mul(a, gf_inv(b)) }
}

/// Carry-less multiply of two binary polynomials.
fn poly_mul2(a: u64, b: u64) -> u64 {
    let mut out = 0u64;
    for i in 0..64 {
        if (b >> i) & 1 == 1 {
            out ^= a << i;
        }
    }
    out
}

/// Remainder of `a` modulo `m`, both binary polynomials.
fn poly_mod2(mut a: u64, m: u64) -> u64 {
    let deg_m = 63 - m.leading_zeros() as i32;
    loop {
        let deg_a = 63 - a.leading_zeros() as i32;
        if a == 0 || deg_a < deg_m {
            return a;
        }
        a ^= m << (deg_a - deg_m);
    }
}

/// Minimal polynomial over GF(2) of α^j, bit-packed with bit i = degree i.
fn minimal_poly(j: usize) -> u64 {
    let mut roots = Vec::new();
    let mut e = j % 63;
    while !roots.contains(&e) {
        roots.push(e);
        e = (e * 2) % 63;
    }
    let (exp, _) = gf_tables();
    // Multiply out (x + α^e) over GF(2^6); the product is guaranteed binary.
    let mut poly: Vec<u8> = vec![1];
    for &r in &roots {
        let a = exp[r];
        let mut next = vec![0u8; poly.len() + 1];
        for (i, &c) in poly.iter().enumerate() {
            next[i + 1] ^= c;
            next[i] ^= gf_mul(c, a);
        }
        poly = next;
    }
    let mut bits = 0u64;
    for (i, &c) in poly.iter().enumerate() {
        debug_assert!(c <= 1, "minimal polynomial escaped GF(2)");
        if c == 1 {
            bits |= 1 << i;
        }
    }
    bits
}

/// Generator polynomial of the NID's BCH(63,16,23) code.
pub fn bch_generator() -> u64 {
    static G: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        let mut g = 1u64;
        let mut covered = std::collections::BTreeSet::new();
        for j in 1..=22usize {
            if covered.contains(&j) {
                continue;
            }
            let mut e = j;
            while covered.insert(e) {
                e = (e * 2) % 63;
            }
            g = poly_mul2(g, minimal_poly(j));
        }
        g
    })
}

/// Systematically encode 16 information bits into a 63-bit codeword.
///
/// Only the tests need this, but they need it badly: without an encoder there
/// is no way to check that the corrector recovers a known word.
pub fn bch_encode(msg: u16) -> u64 {
    let shifted = (u64::from(msg) & 0xFFFF) << 47;
    shifted | poly_mod2(shifted, bch_generator())
}

fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let (exp, log) = gf_tables();
    exp[((u16::from(log[a as usize]) + u16::from(log[b as usize])) % 63) as usize]
}

/// Whether the 63-bit NID codeword satisfies its BCH(63,16,23) parity checks.
///
/// The NID carries the NAC and DUID under a heavy error-correcting code, and
/// this is why: on a live signal a third of frames arrive with a bit error, so
/// the runner-up "NACs" are simply single-bit corruptions of the real one.
/// Checking the syndrome turns a stream that is mostly right into one that is
/// entirely right, by discarding the rest.
///
/// Only detection, not correction — a Berlekamp-Massey decoder would recover
/// those frames rather than drop them, and is worth adding if the frame rate
/// ever matters more than simplicity.
pub fn nid_bch_ok(codeword: u64) -> bool {
    syndrome_zero(codeword, false)
}

/// The same check with the bit-to-degree mapping reversed.
///
/// Which end of the transmitted word is the highest-degree coefficient cannot
/// be settled from first principles without the standard in hand, and getting
/// it wrong rejects every frame rather than failing loudly. A live capture
/// settled it: over 120 frames of the CLMRN control channel the primary
/// convention passed one frame — carrying the dominant NAC, 3A0 — and the
/// reversed one passed none. Kept so the question stays answerable if the
/// demodulator changes enough to reopen it.
pub fn nid_bch_ok_reversed(codeword: u64) -> bool {
    syndrome_zero(codeword, true)
}

fn syndrome_zero(codeword: u64, reversed: bool) -> bool {
    syndromes(codeword, reversed).iter().all(|&s| s == 0)
}

/// S_j = r(α^j) for j = 1..=22.
fn syndromes(codeword: u64, reversed: bool) -> [u8; 22] {
    let (exp, _) = gf_tables();
    let mut out = [0u8; 22];
    for (idx, slot) in out.iter_mut().enumerate() {
        let j = (idx + 1) as u16;
        let mut acc = 0u8;
        for i in 0..63u16 {
            let bit = if reversed { 62 - i } else { i };
            if (codeword >> bit) & 1 == 1 {
                acc ^= exp[((i * j) % 63) as usize];
            }
        }
        *slot = acc;
    }
    out
}

/// Correct up to 11 bit errors in a 63-bit NID codeword.
///
/// This is the difference between discarding a frame and reading it. On the
/// live control channel roughly one frame in a hundred arrived error-free, so
/// detection alone threw away almost everything the code was designed to
/// rescue — the runner-up "NACs" were single-bit corruptions of the real one.
///
/// Berlekamp-Massey finds the error locator, a Chien search finds its roots,
/// and the result is verified rather than trusted: a corrector handed more
/// errors than it can fix will happily produce a valid-looking wrong answer, so
/// the syndromes must vanish afterwards or the frame is refused.
pub fn bch_correct(codeword: u64) -> Option<(u64, usize)> {
    let syn = syndromes(codeword, false);
    if syn.iter().all(|&s| s == 0) {
        return Some((codeword, 0));
    }

    // Berlekamp-Massey over GF(2^6).
    let mut lambda = vec![1u8];
    let mut b = vec![1u8];
    let mut l = 0usize;
    let mut m = 1usize;
    let mut bb = 1u8;
    for r in 0..syn.len() {
        let mut d = syn[r];
        for i in 1..=l {
            if i < lambda.len() && r >= i {
                d ^= gf_mul(lambda[i], syn[r - i]);
            }
        }
        if d == 0 {
            m += 1;
        } else {
            let coef = gf_div(d, bb);
            let prev = lambda.clone();
            if lambda.len() < b.len() + m {
                lambda.resize(b.len() + m, 0);
            }
            for (i, &bi) in b.iter().enumerate() {
                lambda[i + m] ^= gf_mul(coef, bi);
            }
            if 2 * l <= r {
                l = r + 1 - l;
                b = prev;
                bb = d;
                m = 1;
            } else {
                m += 1;
            }
        }
    }
    if l > 11 {
        return None;
    }

    // Chien search: position i is in error when Λ(α^-i) = 0.
    let (exp, _) = gf_tables();
    let mut fixed = codeword;
    let mut found = 0usize;
    for i in 0..63usize {
        let mut acc = 0u8;
        for (k, &c) in lambda.iter().enumerate() {
            if c != 0 {
                acc ^= gf_mul(c, exp[k * (63 - i) % 63]);
            }
        }
        if acc == 0 {
            fixed ^= 1 << i;
            found += 1;
        }
    }
    if found != l || !syndrome_zero(fixed, false) {
        return None;
    }
    Some((fixed, found))
}

/// One recovered frame header.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    /// Network Access Code — the system's identity. Constant for a given site.
    pub nac: u16,
    pub duid: Duid,
    /// Normalised sync correlation, in `0.0..=1.0`.
    pub correlation: f32,
    /// Outer-level amplitude the sync was fitted at, in Hz. Should be near 1800.
    pub deviation_hz: f32,
    /// Whether the NID's BCH parity checks hold, after correction if any.
    pub bch_ok: bool,
    /// Bit errors the BCH decoder repaired. Zero means the frame arrived clean.
    pub corrected_bits: usize,
    /// Carrier offset fitted from this frame's own sync, in Hz.
    pub offset_hz: f32,
    /// Signalling blocks, when this frame is a TSDU and the payload was present.
    pub tsbks: Vec<Tsbk>,
    /// Data dibits following sync and NID, with status symbols removed.
    ///
    /// This is populated when the complete data unit is present. Voice LDUs
    /// use it for their nine 144-bit IMBE codewords and embedded LC/ESS.
    pub payload: Vec<u8>,
}

/// The ideal frequency of each dibit.
pub fn dibit_level(dibit: u8) -> f32 {
    match dibit & 0b11 {
        0b01 => DEV_OUTER_HZ,
        0b00 => DEV_INNER_HZ,
        0b10 => -DEV_INNER_HZ,
        _ => -DEV_OUTER_HZ,
    }
}

/// The dibit a frequency sample represents, sliced against `outer`.
pub fn slice_dibit(hz: f32, outer: f32) -> u8 {
    // Thresholds sit midway between the nominal levels, scaled by whatever
    // deviation the sync was actually fitted at.
    let mid = outer * (DEV_OUTER_HZ + DEV_INNER_HZ) / (2.0 * DEV_OUTER_HZ);
    if hz > mid {
        0b01
    } else if hz > 0.0 {
        0b00
    } else if hz > -mid {
        0b10
    } else {
        0b11
    }
}

/// The frame sync as 24 ideal symbol levels.
pub fn sync_levels() -> [f32; SYNC_SYMBOLS] {
    let mut out = [0.0; SYNC_SYMBOLS];
    for (i, slot) in out.iter_mut().enumerate() {
        let shift = 46 - 2 * i;
        *slot = dibit_level(((FRAME_SYNC >> shift) & 0b11) as u8);
    }
    out
}

/// Root-raised-cosine taps, `span` symbols long at `sps` samples per symbol.
pub fn rrc_taps(sps: usize, span: usize, beta: f32) -> Vec<f32> {
    let n = sps * span + 1;
    let mut taps = Vec::with_capacity(n);
    for i in 0..n {
        let t = (i as f32 - (n as f32 - 1.0) / 2.0) / sps as f32;
        let v = if t.abs() < 1e-6 {
            1.0 - beta + 4.0 * beta / PI
        } else if (t.abs() - 1.0 / (4.0 * beta)).abs() < 1e-4 {
            // The removable singularity at t = ±1/(4β).
            let a = PI / (4.0 * beta);
            beta / 2f32.sqrt() * ((1.0 + 2.0 / PI) * a.sin() + (1.0 - 2.0 / PI) * a.cos())
        } else {
            let num =
                (PI * t * (1.0 - beta)).sin() + 4.0 * beta * t * (PI * t * (1.0 + beta)).cos();
            let den = PI * t * (1.0 - (4.0 * beta * t).powi(2));
            num / den
        };
        taps.push(v);
    }
    let norm: f32 = taps.iter().map(|v| v * v).sum::<f32>().sqrt();
    for t in &mut taps {
        *t /= norm;
    }
    taps
}

/// Which receive filter to match the transmitted pulse with.
///
/// The choice is not cosmetic. A matched filter only cancels inter-symbol
/// interference when it matches the pulse actually transmitted, and measurement
/// against the live CLMRN control channel settled which one that is: an RRC
/// receive filter fitted the sync at 4824 Hz where the standard deviation is
/// 1800 — a ratio of 2.68, essentially the RRC's own DC gain. A filter seeing
/// DC gain on a symbol is a filter whose input is flat across that symbol, so
/// the transmitted pulse is near-rectangular and the boxcar is what matches it.
/// With RRC the decoder caught 2.8 frames/s and never held a NAC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchedFilter {
    /// Integrate-and-dump over one symbol. Unit DC gain, so a fitted deviation
    /// can be compared against the standard's 1800 Hz directly.
    Boxcar,
    /// Root-raised-cosine, for a transmitter that shapes with one too.
    Rrc,
}

/// Discriminator plus matched filter: complex baseband in, frequency out.
pub struct C4fmFrontEnd {
    prev: Complex32,
    hz_per_rad: f32,
    taps: Vec<f32>,
    hist: Vec<f32>,
    /// Slow carrier-offset tracker. Over many symbols the data is balanced, so
    /// any residual mean is the tuning error, not the message.
    dc: OnePole,
    /// The matched filter's gain at DC, which is *not* one.
    ///
    /// The taps are normalised to unit energy, so their sum — the DC gain — is
    /// about 2.9. Reporting the tracker's raw value as a frequency offset
    /// therefore overstated it by that factor.
    dc_gain: f32,
}

impl C4fmFrontEnd {
    pub fn new(fs: f64) -> Self {
        Self::with_filter(fs, MatchedFilter::Boxcar)
    }

    pub fn with_filter(fs: f64, filter: MatchedFilter) -> Self {
        let taps = match filter {
            MatchedFilter::Boxcar => vec![1.0 / SPS as f32; SPS],
            MatchedFilter::Rrc => rrc_taps(SPS, 8, 0.2),
        };
        Self {
            prev: Complex32::new(0.0, 0.0),
            hz_per_rad: (fs / TAU as f64) as f32,
            dc_gain: taps.iter().sum::<f32>().abs().max(1e-6),
            taps,
            hist: Vec::new(),
            // ~40 ms: far longer than a symbol, far shorter than a drift.
            dc: OnePole::new((fs * 0.04) as f32),
        }
    }

    /// Instantaneous frequency in Hz, matched-filtered and carrier-corrected.
    pub fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        out.clear();
        let ntaps = self.taps.len();
        for &x in iq {
            let d = x * self.prev.conj();
            self.prev = x;
            let hz = if d.norm_sqr() > 0.0 {
                d.arg() * self.hz_per_rad
            } else {
                0.0
            };
            self.hist.push(hz);
            if self.hist.len() >= ntaps {
                let base = self.hist.len() - ntaps;
                let mut acc = 0.0;
                for (k, &t) in self.taps.iter().enumerate() {
                    acc += self.hist[base + k] * t;
                }
                out.push(acc - self.dc.process(acc));
            }
        }
        // Keep only what the next call needs to stay continuous.
        if self.hist.len() > ntaps {
            self.hist.drain(..self.hist.len() - (ntaps - 1));
        }
    }

    /// Tracked carrier offset in Hz — the same diagnostic the FM receiver has.
    ///
    /// Divided by the matched filter's DC gain so the answer is in the units of
    /// the input, not of the filtered signal.
    pub fn offset_hz(&self) -> f32 {
        self.dc.value() / self.dc_gain
    }
}

/// Finds frame syncs in a stream of frequency samples and reads the NID.
pub struct FrameDetector {
    /// Which interleave and CRC convention to decode signalling blocks with.
    pub convention: Convention,
    buf: Vec<f32>,
    levels: [f32; SYNC_SYMBOLS],
    /// Samples consumed, so a caller can reason about timing.
    pub consumed: u64,
    /// Absolute sample index before which syncs have already been reported.
    ///
    /// Each push retains a tail of the buffer so a frame straddling two calls
    /// is not lost, and the scan restarts from the beginning of it — which
    /// re-finds every frame in that tail. Without this the same frame was
    /// emitted about seven times, inflating the rate from 30/s to 224/s.
    next_abs: u64,
    /// Absolute sample index up to which correlation has already been tried.
    ///
    /// Each push keeps a tail of the buffer so a frame straddling two calls is
    /// not lost, but the scan used to restart at the head of that tail and
    /// re-correlate every position in it. With the payload buffer that is
    /// ~8,700 samples re-examined against ~1,000 new ones — the same answer
    /// computed eight times. A sample's correlation cannot change once its
    /// window has arrived, so a position evaluated is a position finished.
    scanned_abs: u64,
    /// Wait for complete HDU/LDU/TDULC units and retain their payload.
    capture_payload: bool,
    /// Classifiers need MPDU headers but should report voice NIDs immediately.
    capture_only_pdu: bool,
}

/// Samples spanned by that, with room for the status symbols interleaved.
const NEEDED: usize = (864 + 8) * SPS;
/// Maximum on-air span of a 127-block MPDU, including sync/NID and status
/// symbols. Packet-payload detectors retain this much while a long unit is
/// still arriving instead of dropping its sync from the rolling buffer.
const MAX_PDU_NEEDED: usize = (13_200 + 8) * SPS;

/// The shortest distance to the next possible sync: past this frame's header.
const HEADER_SKIP: usize = (SYNC_SYMBOLS + NID_SYMBOLS + 2) * SPS;

/// Sliced sync symbols allowed to disagree with the pattern.
const MAX_SYNC_ERRORS: usize = 2;

/// P25 inserts a status symbol every 36th symbol of the frame, counting the
/// frame sync itself. Symbol 35 (zero-based) therefore carries no data, and
/// reading straight through it shifts every dibit after it by one — which
/// corrupts the back half of the NID while leaving the front half, and so the
/// leading digits of the NAC, looking perfectly stable.
const STATUS_SYMBOL_EVERY: usize = 36;

/// Symbol index within the frame, skipping status symbols.
fn data_symbol_index(nth: usize) -> usize {
    // Walk rather than compute, so the rule stays obvious.
    let mut idx = 0;
    let mut seen = 0;
    loop {
        if (idx + 1) % STATUS_SYMBOL_EVERY == 0 {
            idx += 1;
            continue;
        }
        if seen == nth {
            return idx;
        }
        seen += 1;
        idx += 1;
    }
}

impl Default for FrameDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDetector {
    pub fn new() -> Self {
        Self {
            convention: Convention::default(),
            buf: Vec::new(),
            levels: sync_levels(),
            consumed: 0,
            next_abs: 0,
            scanned_abs: 0,
            capture_payload: false,
            capture_only_pdu: false,
        }
    }

    /// A detector for a voice channel, where complete data units (rather than
    /// low-latency header notifications) are required.
    pub fn with_payload() -> Self {
        let mut detector = Self::new();
        detector.capture_payload = true;
        detector
    }

    /// Retain a complete packet-data unit while keeping low-latency header
    /// notifications for voice and terminator units.
    pub fn with_packet_payload() -> Self {
        let mut detector = Self::new();
        detector.capture_payload = true;
        detector.capture_only_pdu = true;
        detector
    }

    /// Feed samples; returns any frames whose sync and NID are fully present.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Frame> {
        self.buf.extend_from_slice(samples);
        let mut found = Vec::new();
        let mut i = 0usize;

        // A sync ending at `i` needs SYNC_SYMBOLS symbols before it and the NID
        // after, so only positions with both available are considered.
        let first = (SYNC_SYMBOLS - 1) * SPS;
        // Start where the last call stopped rather than at the head of the
        // retained tail. `evaluated` only advances past positions that were
        // actually decided; one that needed data still to arrive is left to be
        // retried next time.
        let mut evaluated = self.scanned_abs;
        if self.scanned_abs > self.consumed {
            i = (self.scanned_abs - self.consumed) as usize;
        }
        while i + NID_SYMBOLS * SPS < self.buf.len() {
            let abs = self.consumed + i as u64;
            if i < first || abs < self.next_abs || abs < self.scanned_abs {
                evaluated = evaluated.max(abs + 1);
                i += 1;
                continue;
            }
            let start = i - (SYNC_SYMBOLS - 1) * SPS;
            let (corr, amp, offset) = self.correlate(start);
            if corr >= SYNC_THRESHOLD && amp > 100.0 {
                // Take the best alignment in this neighbourhood, not the first
                // that clears the threshold. Sync symbols are all outer levels
                // and survive a sample or two of misalignment, so the first
                // crossing is routinely off-centre — where the NID's *inner*
                // levels, which have half the margin, no longer slice reliably.
                let mut best = (corr, amp, offset, start);
                for probe in 1..SPS {
                    if start + probe + (SYNC_SYMBOLS - 1) * SPS >= self.buf.len() {
                        break;
                    }
                    let (c, a, o) = self.correlate(start + probe);
                    if c > best.0 {
                        best = (c, a, o, start + probe);
                    }
                }
                let (corr, amp, offset, start) = best;
                if self.sync_errors(start, amp, offset) > MAX_SYNC_ERRORS {
                    i += 1;
                    continue;
                }
                let last_needed = start + data_symbol_index(SYNC_SYMBOLS + NID_SYMBOLS - 1) * SPS;
                if last_needed >= self.buf.len() {
                    break;
                }
                let mut bits = 0u64;
                for k in 0..NID_SYMBOLS {
                    let sym = data_symbol_index(SYNC_SYMBOLS + k);
                    let s = self.buf[start + sym * SPS] - offset;
                    bits = (bits << 2) | u64::from(slice_dibit(s, amp));
                }
                // Correct before reading: the NAC and DUID are only meaningful
                // once the codeword they sit in has been repaired.
                let (word, corrected) = match bch_correct(bits >> 1) {
                    Some((w, n)) => (w, Some(n)),
                    None => (bits >> 1, None),
                };
                let info = ((word >> 47) & 0xFFFF) as u16;
                let duid = Duid::from_bits((info & 0xF) as u8);

                // Voice and header units are only useful with their payload.
                // Hold the sync in the retained buffer until the entire unit
                // has arrived. TSDUs retain their existing variable-length
                // handling below because LAST may end them early.
                let full_unit = self.capture_payload
                    && (!self.capture_only_pdu || duid == Duid::Pdu)
                    && matches!(
                        duid,
                        Duid::Header | Duid::Ldu1 | Duid::Ldu2 | Duid::Pdu | Duid::TerminatorLc
                    );
                let mut unit_symbols = duid.max_symbols();
                if full_unit && duid == Duid::Pdu {
                    let header_last =
                        data_symbol_index(SYNC_SYMBOLS + NID_SYMBOLS + tsbk::TSBK_DIBITS - 1);
                    if start + header_last * SPS >= self.buf.len() {
                        break;
                    }
                    let header_payload: Vec<u8> = (0..tsbk::TSBK_DIBITS)
                        .map(|k| {
                            let sym = data_symbol_index(SYNC_SYMBOLS + NID_SYMBOLS + k);
                            slice_dibit(self.buf[start + sym * SPS] - offset, amp)
                        })
                        .collect();
                    if let Some(header) = data::decode_pdu_header(&header_payload) {
                        let data_symbols = SYNC_SYMBOLS
                            + NID_SYMBOLS
                            + (usize::from(header.blocks_to_follow) + 1) * tsbk::TSBK_DIBITS;
                        unit_symbols = data_symbol_index(data_symbols - 1) + 1;
                    }
                }
                if full_unit {
                    let last_air = start + (unit_symbols - 1) * SPS;
                    if last_air >= self.buf.len() {
                        break;
                    }
                }

                // Only a TSDU has signalling blocks, and only when its payload
                // has actually arrived.
                let mut tsbks = Vec::new();
                if duid == Duid::Tsdu && corrected.is_some() {
                    // A TSDU's payload arrives after its header, so emitting the
                    // frame the moment the NID is readable yields a frame with no
                    // blocks. Wait instead: the sync has not been consumed, so
                    // the next push re-finds it with the payload present.
                    let first_end =
                        data_symbol_index(SYNC_SYMBOLS + NID_SYMBOLS + tsbk::TSBK_DIBITS - 1);
                    if start + first_end * SPS >= self.buf.len() {
                        break;
                    }
                    for block in 0..3 {
                        let first = SYNC_SYMBOLS + NID_SYMBOLS + block * tsbk::TSBK_DIBITS;
                        let last = data_symbol_index(first + tsbk::TSBK_DIBITS - 1);
                        if start + last * SPS >= self.buf.len() {
                            break;
                        }
                        let samples: Vec<f32> = (0..tsbk::TSBK_DIBITS)
                            .map(|k| {
                                let sym = data_symbol_index(first + k);
                                self.buf[start + sym * SPS] - offset
                            })
                            .collect();
                        match Tsbk::decode_soft(&samples, amp, self.convention) {
                            Some(t) => {
                                // `last` is only meaningful on a block whose CRC
                                // holds; on a failed one it is a coin toss.
                                let done = t.last && t.crc_ok;
                                tsbks.push(t);
                                if done {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                }
                // Only blocks that passed CRC count as present. A block that
                // failed is not evidence the payload is there at all, and
                // skipping past one that is not steps over the next frame —
                // `Tsbk::decode` returns a block either way, so counting
                // decodes rather than valid decodes silently reintroduced this.
                let block_count = tsbks.iter().filter(|t| t.crc_ok).count();
                let data_count = (0..unit_symbols)
                    .filter(|air| (air + 1) % STATUS_SYMBOL_EVERY != 0)
                    .count();
                let payload = if full_unit {
                    (SYNC_SYMBOLS + NID_SYMBOLS..data_count)
                        .map(|nth| {
                            let sym = data_symbol_index(nth);
                            slice_dibit(self.buf[start + sym * SPS] - offset, amp)
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                found.push(Frame {
                    nac: info >> 4,
                    duid,
                    correlation: corr,
                    deviation_hz: amp,
                    bch_ok: corrected.is_some(),
                    corrected_bits: corrected.unwrap_or(0),
                    offset_hz: offset,
                    tsbks,
                    payload,
                });
                // Skip what this frame actually occupies, so the scan does not
                // spend the payload hunting for syncs that cannot be there.
                //
                // A frame's length depends on its DUID, and for a TSDU also on
                // how many blocks it turned out to carry — skipping a
                // maximum-length TSDU would step over whatever follows a short
                // one, which is a mistake this made once already.
                // Only skip past blocks that actually decoded. Assuming a block
                // is there when it did not decode skips over whatever follows,
                // which on a short frame is the next frame itself.
                let consumed_symbols = if full_unit {
                    unit_symbols
                } else if duid == Duid::Tsdu && block_count > 0 {
                    let blocks = block_count;
                    data_symbol_index(SYNC_SYMBOLS + NID_SYMBOLS + blocks * tsbk::TSBK_DIBITS - 1)
                } else {
                    duid.max_symbols().min(SYNC_SYMBOLS + NID_SYMBOLS)
                };
                let skip = (consumed_symbols * SPS).max(HEADER_SKIP);
                self.next_abs = self.consumed + (start + skip) as u64;
                evaluated = evaluated.max(self.consumed + (i + skip) as u64);
                i += skip;
                continue;
            }
            evaluated = evaluated.max(self.consumed + i as u64 + 1);
            i += 1;
        }
        self.scanned_abs = self.scanned_abs.max(evaluated);

        let keep = if self.capture_only_pdu {
            MAX_PDU_NEEDED
        } else {
            NEEDED + SPS
        };
        if self.buf.len() > keep {
            let drop = self.buf.len() - keep;
            self.buf.drain(..drop);
            self.consumed += drop as u64;
        }
        found
    }

    /// Dibit errors between the sliced sync at `start` and the known pattern.
    ///
    /// Correlation alone is not enough. It is a continuous measure over 24
    /// samples, and scanning every sample position of a long capture gives
    /// noise many chances to clear any threshold — measured, it produced false
    /// frames at better than 0.7. Requiring the *quantised* symbols to match as
    /// well is a combinatorial constraint that noise cannot pass by luck.
    fn sync_errors(&self, start: usize, amp: f32, offset: f32) -> usize {
        let mut errors = 0;
        for k in 0..SYNC_SYMBOLS {
            let got = slice_dibit(self.buf[start + k * SPS] - offset, amp);
            let want = slice_dibit(self.levels[k], DEV_OUTER_HZ);
            if got != want {
                errors += 1;
            }
        }
        errors
    }

    /// Least-squares fit of the sync at `start`, returning correlation,
    /// amplitude, and carrier offset.
    ///
    /// The sync's 24 symbols are known, so fitting `s ≈ a·ideal + d` recovers
    /// both the deviation the transmitter is using and the frequency error at
    /// that instant. Both then scale the NID's slicing, so each frame
    /// calibrates itself.
    ///
    /// Fitting the offset per frame rather than tracking it globally matters
    /// here: the E4000's carrier wandered between −1067 and −1522 Hz across
    /// runs, and the inner symbols are only ±600 Hz, so a stale offset estimate
    /// misreads them outright.
    fn correlate(&self, start: usize) -> (f32, f32, f32) {
        let n = SYNC_SYMBOLS as f32;
        let (mut sx, mut sy, mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for k in 0..SYNC_SYMBOLS {
            let y = self.buf[start + k * SPS];
            let x = self.levels[k];
            sx += x;
            sy += y;
            sxy += x * y;
            sxx += x * x;
            syy += y * y;
        }
        let den = n * sxx - sx * sx;
        if den.abs() < 1e-6 {
            return (0.0, 0.0, 0.0);
        }
        let slope = (n * sxy - sx * sy) / den;
        let offset = (sy - slope * sx) / n;

        // Correlation of the offset-removed samples, so a carrier error cannot
        // masquerade as a poor match.
        let cov = sxy - sx * sy / n;
        let vx = sxx - sx * sx / n;
        let vy = syy - sy * sy / n;
        let corr = if vx > 0.0 && vy > 0.0 {
            cov / (vx * vy).sqrt()
        } else {
            0.0
        };
        (corr, slope * DEV_OUTER_HZ, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Modulate dibits as C4FM at `fs`, with an optional carrier offset.
    ///
    /// Symbols are held flat for their full duration, which is how the live
    /// signal measured — see [`MatchedFilter`]. Paired with the boxcar receive
    /// filter this is matched, and the combined response is a triangle peaking
    /// at the symbol centre with nothing left over at its neighbours.
    fn modulate(dibits: &[u8], fs: f64, offset_hz: f32) -> Vec<Complex32> {
        let mut phase = 0.0f64;
        let mut out = Vec::with_capacity(dibits.len() * SPS);
        for &d in dibits {
            let hz = f64::from(dibit_level(d) + offset_hz);
            for _ in 0..SPS {
                phase += TAU as f64 * hz / fs;
                out.push(Complex32::new(phase.cos() as f32, phase.sin() as f32));
            }
        }
        out
    }

    /// Enough trailing symbols that the detector has the NID plus its lookahead.
    fn padded(mut d: Vec<u8>) -> Vec<u8> {
        d.extend(prng(120, 99));
        d
    }

    /// Deterministic pseudorandom dibits, standing in for the parity and
    /// payload that real frames carry.
    fn prng(n: usize, seed: u32) -> Vec<u8> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                ((s >> 16) & 0b11) as u8
            })
            .collect()
    }

    fn sync_dibits() -> Vec<u8> {
        (0..SYNC_SYMBOLS)
            .map(|i| ((FRAME_SYNC >> (46 - 2 * i)) & 0b11) as u8)
            .collect()
    }

    /// Build one frame: sync, then a NID carrying `nac` and `duid`.
    ///
    /// The NID's remaining 48 bits are pseudorandom rather than zero. In a real
    /// frame they are BCH parity, which is balanced; filling them with zeros
    /// makes every one a +600 Hz symbol and gives the whole signal a large DC
    /// offset that has nothing to do with tuning.
    fn frame(nac: u16, duid: u8, _seed: u32) -> Vec<u8> {
        // A real NID is a BCH codeword plus one overall parity bit, so the test
        // has to build one — otherwise the corrector is fed noise and rightly
        // refuses it.
        let code = bch_encode((nac << 4) | u16::from(duid));
        let nid: u64 = code << 1;
        let mut data = sync_dibits();
        for k in 0..NID_SYMBOLS {
            data.push(((nid >> (62 - 2 * k)) & 0b11) as u8);
        }

        // Interleave the status symbols a real transmitter inserts.
        let mut out = Vec::new();
        let mut taken = 0;
        let mut idx = 0;
        while taken < data.len() {
            if (idx + 1) % STATUS_SYMBOL_EVERY == 0 {
                out.push(0b01); // status symbols are not data; any level will do
            } else {
                out.push(data[taken]);
                taken += 1;
            }
            idx += 1;
        }
        out
    }

    #[test]
    fn the_sync_pattern_is_all_outer_symbols() {
        // A useful property: it makes the correlation peak sharp.
        for v in sync_levels() {
            assert!(
                v.abs() == DEV_OUTER_HZ,
                "sync level {v} is not an outer symbol"
            );
        }
    }

    #[test]
    fn dibits_and_levels_round_trip() {
        for d in 0..4u8 {
            assert_eq!(slice_dibit(dibit_level(d), DEV_OUTER_HZ), d);
        }
    }

    #[test]
    fn slicing_puts_thresholds_between_the_levels() {
        assert_eq!(slice_dibit(1800.0, 1800.0), 0b01);
        assert_eq!(slice_dibit(600.0, 1800.0), 0b00);
        assert_eq!(slice_dibit(-600.0, 1800.0), 0b10);
        assert_eq!(slice_dibit(-1800.0, 1800.0), 0b11);
        // Halfway between inner and outer is the decision point.
        assert_eq!(slice_dibit(1250.0, 1800.0), 0b01);
        assert_eq!(slice_dibit(1150.0, 1800.0), 0b00);
    }

    /// The filter's DC gain is not one, and the offset diagnostic has to divide
    /// it out or it reports roughly three times the real tuning error.
    #[test]
    fn the_rrc_dc_gain_is_above_one() {
        let sum: f32 = rrc_taps(SPS, 8, 0.2).iter().sum();
        assert!(
            sum > 2.0,
            "unit-energy RRC should have a DC gain well above 1, got {sum:.3}"
        );
    }

    /// The boxcar's unit DC gain is what lets a fitted deviation be read
    /// against the standard's 1800 Hz as a sanity check on a live signal.
    #[test]
    fn the_boxcar_fits_the_standard_deviation() {
        let dibits = padded(frame(0x293, 0x7, 5));
        let iq = modulate(&dibits, CHANNEL_RATE, 0.0);
        let mut fe = C4fmFrontEnd::with_filter(CHANNEL_RATE, MatchedFilter::Boxcar);
        let mut hz = Vec::new();
        fe.process(&iq, &mut hz);
        let f = &FrameDetector::new().push(&hz)[0];
        assert!(
            (f.deviation_hz - DEV_OUTER_HZ).abs() < 150.0,
            "fitted {:.0} Hz, expected about {DEV_OUTER_HZ:.0}",
            f.deviation_hz
        );
    }

    #[test]
    fn the_matched_filter_is_unit_energy_and_symmetric() {
        let t = rrc_taps(SPS, 8, 0.2);
        assert_eq!(t.len(), SPS * 8 + 1);
        let energy: f32 = t.iter().map(|v| v * v).sum();
        assert!((energy - 1.0).abs() < 1e-4, "energy {energy}");
        for k in 0..t.len() / 2 {
            assert!(
                (t[k] - t[t.len() - 1 - k]).abs() < 1e-5,
                "asymmetric at {k}"
            );
        }
    }

    /// The end-to-end claim: a synthesised frame comes back with its NAC and
    /// DUID intact.
    #[test]
    fn a_synthetic_frame_yields_its_nac_and_duid() {
        let dibits = padded(frame(0x293, 0x7, 1));
        let iq = modulate(&dibits, CHANNEL_RATE, 0.0);
        let mut fe = C4fmFrontEnd::new(CHANNEL_RATE);
        let mut hz = Vec::new();
        fe.process(&iq, &mut hz);

        let mut det = FrameDetector::new();
        let frames = det.push(&hz);
        assert!(!frames.is_empty(), "no frame detected");
        let f = &frames[0];
        assert_eq!(f.nac, 0x293, "NAC came back as {:03X}", f.nac);
        assert_eq!(f.duid, Duid::Tsdu);
        assert!(f.correlation > SYNC_THRESHOLD);
        assert!(
            f.bch_ok,
            "a clean synthetic frame must pass its parity checks"
        );
        // No payload was synthesised, so no blocks should be claimed.
        assert!(f.tsbks.iter().all(|t| !t.crc_ok));
        assert_eq!(f.corrected_bits, 0);
        assert!(
            (f.deviation_hz - DEV_OUTER_HZ).abs() < 400.0,
            "fitted deviation {:.0} Hz",
            f.deviation_hz
        );
    }

    #[test]
    fn a_linear_cqpsk_frame_yields_the_same_nid() {
        let dibits = padded(frame(0x293, 0x7, 77));
        let sps = SPS;
        let mut iq = Vec::with_capacity((dibits.len() + 1) * sps);
        let mut phase = 0.0f32;
        // A reference symbol fills the differential delay before frame sync.
        for _ in 0..sps {
            iq.push(Complex32::new(phase.cos(), phase.sin()));
        }
        for dibit in dibits {
            let step = match dibit {
                1 => std::f32::consts::FRAC_PI_4 * 3.0,
                0 => std::f32::consts::FRAC_PI_4,
                2 => -std::f32::consts::FRAC_PI_4,
                _ => -std::f32::consts::FRAC_PI_4 * 3.0,
            };
            for _ in 0..sps {
                phase += step / sps as f32;
                iq.push(Complex32::new(phase.cos(), phase.sin()));
            }
        }
        let mut cqpsk =
            crate::cqpsk::CqpskDemodulator::new(CHANNEL_RATE, 4_800.0).with_tracking(0.0, 0.0);
        let mut equivalent = Vec::new();
        cqpsk.process(&iq, &mut equivalent);
        let frames = FrameDetector::new().push(&equivalent);
        let decoded = frames
            .iter()
            .find(|frame| frame.bch_ok)
            .expect("CQPSK frame should pass its NID BCH");
        assert_eq!(decoded.nac, 0x293);
        assert_eq!(decoded.duid, Duid::Tsdu);
    }

    /// A real receiver is never exactly on frequency; the tracker must absorb
    /// the offset the dongles actually have.
    #[test]
    fn a_carrier_offset_is_tracked_out() {
        // Several frames, so the slow DC tracker has time to settle.
        let mut dibits = Vec::new();
        for i in 0..24 {
            dibits.extend(frame(0x293, 0x7, 100 + i));
            // Real control channels run frames back to back with payload
            // between the headers; the DC tracker has to see that too.
            dibits.extend(prng(60, 200 + i));
        }
        let iq = modulate(&padded(dibits), CHANNEL_RATE, 500.0);
        let mut fe = C4fmFrontEnd::new(CHANNEL_RATE);
        let mut hz = Vec::new();
        fe.process(&iq, &mut hz);

        let mut det = FrameDetector::new();
        let frames = det.push(&hz);
        let good = frames.iter().filter(|f| f.nac == 0x293).count();
        assert!(good >= 12, "only {good} of {} frames decoded", frames.len());
        assert!(
            (fe.offset_hz() - 500.0).abs() < 250.0,
            "offset tracked as {:.0} Hz, expected about 500",
            fe.offset_hz()
        );
    }

    /// Noise must not manufacture frames — a false NAC would look exactly like
    /// a real one to everything downstream.
    #[test]
    fn noise_produces_no_frames() {
        let mut s = 12345u32;
        let hz: Vec<f32> = (0..40_000)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                ((s >> 8) as f32 / 8388608.0 - 1.0) * DEV_OUTER_HZ
            })
            .collect();
        let mut det = FrameDetector::new();
        let frames = det.push(&hz);
        let plausible = frames.iter().filter(|f| f.duid.is_known()).count();
        assert!(
            plausible == 0,
            "noise produced {plausible} plausible frames out of {}",
            frames.len()
        );
    }

    /// The status symbol at index 35 must be stepped over, and nothing before
    /// it disturbed.
    /// The field arithmetic has to be right before the syndrome means anything.
    #[test]
    fn the_galois_field_is_well_formed() {
        let (exp, log) = gf_tables();
        // α is primitive: its powers cover every non-zero element exactly once.
        let mut seen = [false; 64];
        for i in 0..63 {
            let v = exp[i] as usize;
            assert!(v != 0 && !seen[v], "α^{i} = {v} repeats or is zero");
            seen[v] = true;
            assert_eq!(log[v] as usize, i);
        }
        assert_eq!(gf_mul(1, 5), 5);
        assert_eq!(gf_mul(0, 9), 0);
    }

    /// All-zero is a valid codeword of any linear code, and a lone set bit is
    /// not — the weakest useful check that the syndrome is actually computed.
    /// The generator must have the degree the code demands, or the encoder is
    /// producing something other than BCH(63,16,23).
    #[test]
    fn control_rate_decimates_onto_the_channel_rate() {
        let decim = (CONTROL_RATE / CHANNEL_RATE).round();
        assert!((CONTROL_RATE / decim - CHANNEL_RATE).abs() < 1e-9);
        assert_eq!(VOICE_RATE, CONTROL_RATE);
        assert!(
            CONTROL_RATE > 2.0 * TUNE_OFFSET_HZ,
            "Nyquist must clear the 100 kHz LO offset"
        );
    }

    #[test]
    fn the_generator_has_degree_47() {
        let g = bch_generator();
        assert_eq!(63 - g.leading_zeros(), 47, "generator is 0x{g:X}");
        assert_eq!(g & 1, 1, "a generator polynomial has a constant term");
    }

    /// Every encoded message is a codeword, and its information bits survive.
    #[test]
    fn encoding_produces_codewords_that_carry_their_message() {
        for msg in [0u16, 0x2937, 0xFFFF, 0x1234, 0xABCD] {
            let c = bch_encode(msg);
            assert!(nid_bch_ok(c), "0x{msg:04X} did not encode to a codeword");
            assert_eq!(((c >> 47) & 0xFFFF) as u16, msg, "message not systematic");
        }
    }

    /// The point of the whole exercise: errors within the code's power are
    /// repaired exactly.
    #[test]
    fn correction_repairs_up_to_eleven_errors() {
        let msg = 0x3A07; // NAC 3A0, DUID 7 — the live system.
        let clean = bch_encode(msg);
        for nerr in 1..=11usize {
            let mut corrupt = clean;
            // Spread the errors so they are not accidentally easy.
            for k in 0..nerr {
                corrupt ^= 1 << ((k * 5 + 3) % 63);
            }
            let (fixed, n) = bch_correct(corrupt)
                .unwrap_or_else(|| panic!("{nerr} errors defeated the corrector"));
            assert_eq!(fixed, clean, "{nerr} errors were mis-corrected");
            assert_eq!(n, nerr);
            assert_eq!(((fixed >> 47) & 0xFFFF) as u16, msg);
        }
    }

    /// And beyond its power it must refuse rather than invent an answer — a
    /// confidently wrong NAC is worse than a dropped frame.
    #[test]
    fn correction_refuses_more_errors_than_it_can_fix() {
        let clean = bch_encode(0x3A07);
        let mut corrupt = clean;
        for k in 0..20 {
            corrupt ^= 1 << ((k * 3 + 1) % 63);
        }
        match bch_correct(corrupt) {
            None => {}
            Some((fixed, _)) => assert_eq!(
                fixed, clean,
                "corrector returned a different codeword instead of refusing"
            ),
        }
    }

    #[test]
    fn the_bch_check_rejects_a_non_codeword() {
        assert!(nid_bch_ok(0));
        assert!(!nid_bch_ok(1));
        assert!(!nid_bch_ok(1 << 62));
        // Both conventions must agree on the degenerate cases.
        assert!(nid_bch_ok_reversed(0));
        assert!(!nid_bch_ok_reversed(1));
    }

    #[test]
    fn status_symbols_are_skipped_when_indexing_data() {
        assert_eq!(data_symbol_index(0), 0);
        assert_eq!(data_symbol_index(34), 34);
        // Index 35 is the status symbol, so the 36th data symbol is at 36.
        assert_eq!(data_symbol_index(35), 36);
        assert_eq!(data_symbol_index(69), 70);
        // And the second status symbol pushes it again.
        assert_eq!(data_symbol_index(70), 72);
    }

    /// Frame lengths differ by kind, which is how the scanner knows where the
    /// next sync can be. A terminator is short; a voice frame is long.
    #[test]
    fn frame_lengths_follow_the_data_unit_kind() {
        assert_eq!(Duid::Terminator.max_symbols(), 72);
        assert_eq!(Duid::Tsdu.max_symbols(), 360);
        assert_eq!(Duid::Ldu1.max_symbols(), 864);
        assert!(Duid::Terminator.max_symbols() < Duid::Tsdu.max_symbols());
        // An unrecognised DUID must not claim a long frame and skip real ones.
        assert_eq!(Duid::Unknown(9).max_symbols(), SYNC_SYMBOLS + NID_SYMBOLS);
    }

    #[test]
    fn duids_map_to_their_standard_names() {
        assert_eq!(Duid::from_bits(0x7), Duid::Tsdu);
        assert_eq!(Duid::from_bits(0x5), Duid::Ldu1);
        assert!(!Duid::from_bits(0x9).is_known());
        assert_eq!(Duid::from_bits(0x7).label(), "TSDU");
    }
}
