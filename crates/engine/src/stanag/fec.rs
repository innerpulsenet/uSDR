//! The error control both serial-tone modems share: the constraint-length
//! 7, rate 1/2 convolutional code (generators 133 and 171 octal, the T1
//! output sent first), a soft-decision Viterbi decoder for it that also
//! says how hard it had to work, and the two interleavers: the
//! convolutional one of STANAG 4285 and the block one of
//! MIL-STD-188-110A.
//!
//! The decoder's measure of success is the one an operator would use: the
//! decoded bits are encoded again and compared with the hard decisions the
//! demodulator made. On the right mode that disagreement is the raw channel
//! error rate, a few percent; on a wrong guess at the rate or interleaver
//! the decoder is fed bits that obey no code, and a third or more of them
//! disagree with whatever path it settles on.

/// T1 taps: the input and the bits delayed 2, 3, 5 and 6 (x^6+x^4+x^3+x+1
/// in MIL-STD-188-110A's figure 4, the input at x^6).
const G1: u8 = 0b101_1011;
/// T2 taps: the input and the bits delayed 1, 2, 3 and 6.
const G2: u8 = 0b111_1001;

fn parity(x: u8) -> u8 {
    (x.count_ones() & 1) as u8
}

/// The two coded bits for an input bit, from the register holding it in
/// bit 6 and the six before it below, newest highest.
fn code_bits(reg: u8) -> (u8, u8) {
    (parity(reg & G1), parity(reg & G2))
}

#[derive(Clone, Copy, Default)]
pub(super) struct Encoder {
    /// The last six inputs, newest in bit 5.
    state: u8,
}

impl Encoder {
    pub(super) fn push(&mut self, bit: u8) -> (u8, u8) {
        let reg = (bit & 1) << 6 | self.state;
        self.state = reg >> 1;
        code_bits(reg)
    }
}

/// Bits the decoder looks back before it settles one: about nine
/// constraint lengths, where the survivors have long since merged.
const DEPTH: usize = 64;
/// Bits settled at a time.
const CHUNK: usize = 32;
/// The re-encoding error rate is a running mean over about this many bits.
const BER_SPAN: f32 = 1000.0;

/// For each state after a step and each choice of the bit that left the
/// register: the coded pair (T1 in bit 1, T2 in bit 0) of that branch.
const fn branch_table() -> [[u8; 2]; 64] {
    let mut t = [[0u8; 2]; 64];
    let mut ns = 0;
    while ns < 64 {
        let input = (ns >> 5) as u8;
        let mut b = 0;
        while b < 2 {
            let prev = ((ns << 1) & 0x3f) as u8 | b as u8;
            let reg = input << 6 | prev;
            let c1 = ((reg & G1).count_ones() & 1) as u8;
            let c2 = ((reg & G2).count_ones() & 1) as u8;
            t[ns][b] = c1 << 1 | c2;
            b += 1;
        }
        ns += 1;
    }
    t
}

const BRANCH: [[u8; 2]; 64] = branch_table();

/// Soft-decision Viterbi decoder for the 133/171 code, run as a stream.
/// Soft values are log-likelihood ratios, positive for a 0; zero is an
/// erasure (a punctured or missing bit).
pub(super) struct Viterbi {
    metric: [f32; 64],
    /// One word per step: bit `s` says which predecessor state `s` kept.
    dec: Vec<u64>,
    /// The soft pair each step was given, for the re-encoding check.
    soft: Vec<(f32, f32)>,
    steps: u64,
    /// Steps already settled and handed out.
    settled: u64,
    reenc: Encoder,
    ber: f32,
    /// Bits compared, saturating: the error rate means nothing until the
    /// running mean has filled.
    compared: u32,
    /// Running share of ones among the decoded bits.
    ones: f32,
    out: Vec<u8>,
}

const RING: usize = DEPTH + 2 * CHUNK;

impl Viterbi {
    pub(super) fn new() -> Self {
        let mut metric = [-1e4f32; 64];
        metric[0] = 0.0;
        Self {
            metric,
            dec: vec![0; RING],
            soft: vec![(0.0, 0.0); RING],
            steps: 0,
            settled: 0,
            reenc: Encoder::default(),
            ber: 0.5,
            compared: 0,
            ones: 0.5,
            out: Vec::with_capacity(CHUNK),
        }
    }

    /// One coded pair, T1 then T2. Settled bits collect in [`Self::take`].
    pub(super) fn push(&mut self, s1: f32, s2: f32) {
        // Branch metrics for the four coded pairs: the correlation of the
        // pair, as signs, with the soft values.
        let bm = [s1 + s2, s1 - s2, -s1 + s2, -s1 - s2];
        let mut next = [0f32; 64];
        let mut word = 0u64;
        let mut best = f32::MIN;
        for (ns, slot) in next.iter_mut().enumerate() {
            let p0 = (ns << 1) & 0x3f;
            let a = self.metric[p0] + bm[BRANCH[ns][0] as usize];
            let b = self.metric[p0 | 1] + bm[BRANCH[ns][1] as usize];
            let v = if b > a {
                word |= 1 << ns;
                b
            } else {
                a
            };
            *slot = v;
            best = best.max(v);
        }
        // Keep the metrics near zero; only their differences matter.
        for m in &mut next {
            *m -= best;
        }
        self.metric = next;
        let at = (self.steps % RING as u64) as usize;
        self.dec[at] = word;
        self.soft[at] = (s1, s2);
        self.steps += 1;
        if self.steps - self.settled >= (DEPTH + CHUNK) as u64 {
            self.traceback();
        }
    }

    fn traceback(&mut self) {
        let mut state = (0..64).max_by(|&a, &b| self.metric[a].total_cmp(&self.metric[b])).unwrap_or(0);
        let newest = self.steps - 1;
        let first = self.settled;
        let last = first + CHUNK as u64; // exclusive
        let mut bits = [0u8; CHUNK];
        let mut t = newest;
        loop {
            if t < last {
                bits[(t - first) as usize] = (state >> 5) as u8;
            }
            if t == first {
                break;
            }
            let w = self.dec[(t % RING as u64) as usize];
            state = ((state << 1) & 0x3f) | ((w >> state) & 1) as usize;
            t -= 1;
        }
        for (i, &b) in bits.iter().enumerate() {
            let (s1, s2) = self.soft[((first + i as u64) % RING as u64) as usize];
            let (c1, c2) = self.reenc.push(b);
            for (s, c) in [(s1, c1), (s2, c2)] {
                if s != 0.0 {
                    let wrong = (s < 0.0) != (c == 1);
                    self.ber += (f32::from(wrong) - self.ber) / BER_SPAN;
                    self.compared = self.compared.saturating_add(1);
                }
            }
            self.ones += (f32::from(b) - self.ones) / 200.0;
            self.out.push(b);
        }
        self.settled = last;
    }

    /// Bits settled since the last call, oldest first.
    pub(super) fn take(&mut self, into: &mut Vec<u8>) {
        into.append(&mut self.out);
    }

    pub(super) fn ones(&self) -> f32 {
        self.ones
    }

    /// Share of the coded bits that disagree with the decoded path, once
    /// enough have been seen to say.
    pub(super) fn ber(&self) -> Option<f32> {
        (self.compared as f32 >= BER_SPAN).then_some(self.ber)
    }
}

/// STANAG 4285's convolutional interleaver: 32 rows, row `r` delayed by
/// `r * inc` sweeps of 32 bits, with the input bits spread over the rows
/// by stepping nine at a time. `inc` is set by the rate and the short or
/// long setting.
pub(super) struct ConvDeinterleaver {
    inc: usize,
    /// Received sweeps, a ring of `31 * inc + 1`.
    ring: Vec<[f32; 32]>,
    sweeps: u64,
    fill: usize,
    cur: [f32; 32],
}

/// The row each input position goes to.
fn row_of(j: usize) -> usize {
    (9 * j) % 32
}

impl ConvDeinterleaver {
    pub(super) fn new(inc: usize) -> Self {
        Self { inc, ring: vec![[0.0; 32]; 31 * inc + 1], sweeps: 0, fill: 0, cur: [0.0; 32] }
    }

    /// One received soft bit, in the order sent. A whole sweep of
    /// deinterleaved bits comes out every 32, in the order they were
    /// coded, `31 * inc` sweeps late.
    pub(super) fn push(&mut self, v: f32, out: &mut Vec<f32>) {
        self.cur[self.fill] = v;
        self.fill += 1;
        if self.fill < 32 {
            return;
        }
        self.fill = 0;
        let n = self.ring.len() as u64;
        self.ring[(self.sweeps % n) as usize] = self.cur;
        self.sweeps += 1;
        // The sweep just completed is `sweeps - 1`; the oldest input sweep
        // now whole is 31 * inc before it. Before that much has arrived the
        // missing rows read as erasures (the ring starts zeroed).
        let newest = self.sweeps - 1;
        let lag = (31 * self.inc) as u64;
        let s = newest as i64 - lag as i64;
        for j in 0..32 {
            let r = row_of(j);
            let src = s + (r * self.inc) as i64;
            out.push(if src < 0 { 0.0 } else { self.ring[(src as u64 % n) as usize][r] });
        }
    }
}

/// The transmit side of [`ConvDeinterleaver`], for the test transmitters.
#[cfg(test)]
pub(super) struct ConvInterleaver {
    inc: usize,
    /// Input sweeps, a ring of `31 * inc + 1`.
    ring: Vec<[u8; 32]>,
    sweeps: u64,
    fill: usize,
    cur: [u8; 32],
}

#[cfg(test)]
impl ConvInterleaver {
    pub(super) fn new(inc: usize) -> Self {
        Self { inc, ring: vec![[0; 32]; 31 * inc + 1], sweeps: 0, fill: 0, cur: [0; 32] }
    }

    pub(super) fn push(&mut self, bit: u8, out: &mut Vec<u8>) {
        self.cur[self.fill] = bit;
        self.fill += 1;
        if self.fill < 32 {
            return;
        }
        self.fill = 0;
        let n = self.ring.len() as u64;
        self.ring[(self.sweeps % n) as usize] = self.cur;
        let s = self.sweeps;
        self.sweeps += 1;
        // Output row r of sweep s carries the input that went into row r
        // r * inc sweeps earlier.
        let mut inv = [0usize; 32];
        for j in 0..32 {
            inv[row_of(j)] = j;
        }
        for (r, &j) in inv.iter().enumerate() {
            let src = s as i64 - (r * self.inc) as i64;
            out.push(if src < 0 { 0 } else { self.ring[(src as u64 % n) as usize][j] });
        }
    }
}

/// MIL-STD-188-110A's block interleaver: bits are loaded down the columns
/// of a `rows` x `cols` matrix, the row stepping by `load_step` modulo the
/// rows, and fetched along diagonals, the row up one and the column down
/// `fetch_step` each bit. Built as the permutation from load order to
/// fetch order.
pub(super) struct BlockInterleaver {
    /// For each bit in coded (load) order, its position in the sent
    /// (fetch) order.
    perm: Vec<u32>,
}

impl BlockInterleaver {
    pub(super) fn new(rows: usize, cols: usize, load_step: usize, fetch_step: usize) -> Self {
        let mut perm = Vec::with_capacity(rows * cols);
        for i in 0..rows * cols {
            let col = i / rows;
            let row = (load_step * (i % rows)) % rows;
            // Fetch index j = base * rows + row takes column
            // (base - fetch_step * row) mod cols.
            let base = (col + fetch_step * row) % cols;
            perm.push((base * rows + row) as u32);
        }
        Self { perm }
    }

    pub(super) fn len(&self) -> usize {
        self.perm.len()
    }

    /// A block of received soft bits, in the order sent, into coded order.
    pub(super) fn deinterleave(&self, sent: &[f32], out: &mut Vec<f32>) {
        out.extend(self.perm.iter().map(|&j| sent[j as usize]));
    }

    #[cfg(test)]
    pub(super) fn interleave(&self, coded: &[u8]) -> Vec<u8> {
        let mut sent = vec![0u8; coded.len()];
        for (i, &j) in self.perm.iter().enumerate() {
            sent[j as usize] = coded[i];
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *seed >> 33
    }

    #[test]
    fn viterbi_corrects_scattered_errors() {
        let mut seed = 7u64;
        let data: Vec<u8> = (0..8000).map(|_| (lcg(&mut seed) & 1) as u8).collect();
        let mut enc = Encoder::default();
        let mut vit = Viterbi::new();
        for (i, &b) in data.iter().enumerate() {
            let (c1, c2) = enc.push(b);
            // One coded bit in twenty flipped.
            let f1 = if i % 10 == 3 { 1 } else { 0 };
            let f2 = if i % 10 == 8 { 1 } else { 0 };
            let s = |c: u8| if c == 0 { 1.0 } else { -1.0 };
            vit.push(s(c1 ^ f1), s(c2 ^ f2));
        }
        let mut out = Vec::new();
        vit.take(&mut out);
        assert!(out.len() > 7800);
        assert_eq!(&out[..], &data[..out.len()]);
        let ber = vit.ber().unwrap();
        assert!((ber - 0.1).abs() < 0.03, "ber {ber}");
    }

    #[test]
    fn viterbi_on_noise_reports_a_high_error_rate() {
        let mut seed = 9u64;
        let mut vit = Viterbi::new();
        for _ in 0..6000 {
            let a = (lcg(&mut seed) % 2000) as f32 / 1000.0 - 1.0;
            let b = (lcg(&mut seed) % 2000) as f32 / 1000.0 - 1.0;
            vit.push(a, b);
        }
        // The best path through random input still agrees with four
        // fifths of it: far from the few percent of a real decode.
        let ber = vit.ber().unwrap();
        assert!(ber > 0.15, "ber {ber}");
    }

    #[test]
    fn punctured_noise_error_rate() {
        // Rate 2/3 by dropping every fourth coded bit, as STANAG 4285 sends
        // 2400 bps: with fewer bits to fit, random input fits better.
        let mut seed = 11u64;
        let mut vit = Viterbi::new();
        for i in 0..20000 {
            let a = (lcg(&mut seed) % 2000) as f32 / 1000.0 - 1.0;
            let b = if i % 2 == 1 { 0.0 } else { (lcg(&mut seed) % 2000) as f32 / 1000.0 - 1.0 };
            vit.push(a, b);
        }
        let ber = vit.ber().unwrap();
        assert!(ber > 0.08, "ber {ber}");
        eprintln!("punctured random-input error rate {ber}");
    }

    #[test]
    fn conv_interleaver_round_trip() {
        for inc in [1, 2, 4, 12] {
            let mut seed = inc as u64;
            let data: Vec<u8> = (0..32 * 31 * inc * 3).map(|_| (lcg(&mut seed) & 1) as u8).collect();
            let mut il = ConvInterleaver::new(inc);
            let mut sent = Vec::new();
            for &b in &data {
                il.push(b, &mut sent);
            }
            let mut de = ConvDeinterleaver::new(inc);
            let mut back = Vec::new();
            for &b in &sent {
                de.push(if b == 0 { 1.0 } else { -1.0 }, &mut back);
            }
            let lag = 32 * 31 * inc;
            for (i, &v) in back.iter().enumerate().skip(lag) {
                assert_eq!(u8::from(v < 0.0), data[i - lag], "inc {inc} at {i}");
            }
        }
    }

    #[test]
    fn block_interleaver_matches_the_standard_example() {
        // 2400 bps long: the second bit fetched comes from row 1, column
        // 559, the third from row 2, column 542. Bit (row r, column c) is
        // loaded at index c * 40 + k where 9k = r mod 40.
        let il = BlockInterleaver::new(40, 576, 9, 17);
        let load_index = |row: usize, col: usize| {
            let k = (0..40).find(|k| (9 * k) % 40 == row).unwrap();
            col * 40 + k
        };
        assert_eq!(il.perm[load_index(0, 0)], 0);
        assert_eq!(il.perm[load_index(1, 559)], 1);
        assert_eq!(il.perm[load_index(2, 542)], 2);
        let mut seen = vec![false; il.len()];
        for &j in &il.perm {
            assert!(!seen[j as usize]);
            seen[j as usize] = true;
        }
    }
}
