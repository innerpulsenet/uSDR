//! MFSK's error correction: the NASA K=7 rate 1/2 convolutional code and
//! the diagonal interleaver, laid out as fldigi lays them out.
//!
//! The encoder shifts each data bit into a 7-bit register (newest bit
//! lowest) and sends two parities, of the register masked with 0x6D and
//! then with 0x4F. Coded bits are gathered four to a symbol, first bit
//! highest, and each symbol's bits pass through `depth` stages of a 4 by 4
//! diagonal delay: bit i of a symbol leaves i x depth symbols later, so a
//! fade that wipes out a few symbols costs each code word only a bit or
//! two. The receiver's stages delay bit i by (3 - i) x depth, which puts
//! every bit back in its symbol 3 x depth symbols late.

pub(crate) const K: usize = 7;
pub(crate) const POLY1: u32 = 0x6d;
pub(crate) const POLY2: u32 = 0x4f;
const STATES: usize = 1 << (K - 1);

fn parity(x: u32) -> u32 {
    x.count_ones() & 1
}

/// The two coded bits for a register value (newest bit lowest).
fn code_pair(reg: u32) -> (u32, u32) {
    (parity(reg & POLY1), parity(reg & POLY2))
}

/// The transmitter's encoder (and the receiver's, to check its own
/// decisions against what arrived).
#[derive(Clone, Default)]
pub(crate) struct Encoder {
    reg: u32,
}

impl Encoder {
    /// One data bit in, its two coded bits out, in the order sent.
    pub(crate) fn push(&mut self, bit: bool) -> (bool, bool) {
        self.reg = ((self.reg << 1) | bit as u32) & ((1 << K) - 1);
        let (a, b) = code_pair(self.reg);
        (a == 1, b == 1)
    }
}

/// Soft-decision Viterbi decoding by register exchange: each state keeps
/// the last 64 bits of its best path, and a bit is let out a fixed number
/// of steps after it arrived, from the best state's path.
pub(crate) struct Viterbi {
    metric: [f32; STATES],
    next: [f32; STATES],
    path: [u64; STATES],
    next_path: [u64; STATES],
    /// Steps taken.
    steps: u64,
    delay: u32,
    /// Expected coded bits for each state and input bit, as signs (+1 for
    /// a 0 bit).
    expect: [[(f32, f32); 2]; STATES],
}

impl Viterbi {
    pub(crate) fn new(delay: u32) -> Self {
        let mut expect = [[(0.0, 0.0); 2]; STATES];
        for (s, e) in expect.iter_mut().enumerate() {
            for b in 0..2u32 {
                let reg = ((s as u32) << 1) | b;
                let (c0, c1) = code_pair(reg);
                e[b as usize] = (1.0 - 2.0 * c0 as f32, 1.0 - 2.0 * c1 as f32);
            }
        }
        Self {
            metric: [0.0; STATES],
            next: [0.0; STATES],
            path: [0; STATES],
            next_path: [0; STATES],
            steps: 0,
            delay: delay.min(63),
            expect,
        }
    }

    /// Start over, from no knowledge of the state.
    pub(crate) fn reset(&mut self) {
        self.metric = [0.0; STATES];
        self.path = [0; STATES];
        self.steps = 0;
    }

    /// One pair of log-likelihood ratios (positive for a 0 bit); the data
    /// bit `delay` steps back, once there is one.
    pub(crate) fn push(&mut self, l0: f32, l1: f32) -> Option<bool> {
        // State s is the register's six older bits; a step shifts the new
        // bit in at the bottom and drops the top one.
        for ns in 0..STATES {
            let b = ns & 1;
            let hi = ns >> 1;
            let p0 = hi;
            let p1 = hi | (STATES >> 1);
            let (e0a, e0b) = self.expect[p0][b];
            let (e1a, e1b) = self.expect[p1][b];
            let m0 = self.metric[p0] + 0.5 * (e0a * l0 + e0b * l1);
            let m1 = self.metric[p1] + 0.5 * (e1a * l0 + e1b * l1);
            if m0 >= m1 {
                self.next[ns] = m0;
                self.next_path[ns] = (self.path[p0] << 1) | b as u64;
            } else {
                self.next[ns] = m1;
                self.next_path[ns] = (self.path[p1] << 1) | b as u64;
            }
        }
        std::mem::swap(&mut self.metric, &mut self.next);
        std::mem::swap(&mut self.path, &mut self.next_path);
        // Keep the metrics near zero.
        let best = self
            .metric
            .iter()
            .cloned()
            .fold(f32::NEG_INFINITY, f32::max);
        let mut at = 0;
        for (s, m) in self.metric.iter_mut().enumerate() {
            if *m == best {
                at = s;
            }
            *m -= best;
        }
        self.steps += 1;
        (self.steps > self.delay as u64).then(|| (self.path[at] >> self.delay) & 1 == 1)
    }
}

/// fldigi's diagonal interleaver, either way round.
pub(crate) struct Interleaver<T: Copy> {
    size: usize,
    depth: usize,
    table: Vec<T>,
    forward: bool,
}

impl<T: Copy> Interleaver<T> {
    /// `forward` interleaves (the transmitter); otherwise it undoes it.
    /// `fill` is what the receiver's stages hold before anything arrives.
    pub(crate) fn new(size: usize, depth: usize, forward: bool, fill: T) -> Self {
        Self {
            size,
            depth,
            table: vec![fill; size * size * depth],
            forward,
        }
    }

    /// One symbol's bits, first bit first, through every stage.
    pub(crate) fn symbols(&mut self, syms: &mut [T]) {
        let n = self.size;
        for k in 0..self.depth {
            let base = k * n * n;
            for (i, s) in syms.iter().enumerate().take(n) {
                let row = base + i * n;
                self.table.copy_within(row + 1..row + n, row);
                self.table[row + n - 1] = *s;
            }
            for (i, s) in syms.iter_mut().enumerate().take(n) {
                let j = if self.forward { n - i - 1 } else { i };
                *s = self.table[base + i * n + j];
            }
        }
    }

    /// Symbols a bit spends in this interleaver and its inverse together.
    pub(crate) fn latency(&self) -> usize {
        (self.size - 1) * self.depth
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaver_pair_is_a_fixed_delay() {
        let (size, depth) = (4, 10);
        let mut tx = Interleaver::new(size, depth, true, 0u32);
        let mut rx = Interleaver::new(size, depth, false, u32::MAX);
        let lat = tx.latency();
        for g in 0..200u32 {
            let mut s: Vec<u32> = (0..size as u32).map(|i| g * 10 + i).collect();
            tx.symbols(&mut s);
            rx.symbols(&mut s);
            if g as usize >= lat {
                let want: Vec<u32> = (0..size as u32)
                    .map(|i| (g - lat as u32) * 10 + i)
                    .collect();
                assert_eq!(s, want);
            }
        }
    }

    #[test]
    fn interleaver_spreads_a_symbol() {
        // Bit i of a symbol leaves i x depth symbols later.
        let mut tx = Interleaver::new(4, 10, true, 0u8);
        let mut seen = Vec::new();
        for g in 0..40 {
            let mut s = if g == 0 { [1u8, 2, 3, 4] } else { [0u8; 4] };
            tx.symbols(&mut s);
            for (i, v) in s.iter().enumerate() {
                if *v != 0 {
                    seen.push((g, i, *v));
                }
            }
        }
        assert_eq!(seen, vec![(0, 0, 1), (10, 1, 2), (20, 2, 3), (30, 3, 4)]);
    }

    #[test]
    fn viterbi_corrects_errors() {
        let mut enc = Encoder::default();
        let mut vit = Viterbi::new(40);
        let mut rng = 12345u64;
        let bits: Vec<bool> = (0..2000)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng & 1 == 1
            })
            .collect();
        let mut out = Vec::new();
        for (i, &b) in bits
            .iter()
            .chain(std::iter::repeat_n(&false, 60))
            .enumerate()
        {
            let (c0, c1) = enc.push(b);
            // Flip one coded bit in 20.
            let f0 = i % 20 == 3;
            let l0 = if c0 != f0 { -1.0 } else { 1.0 };
            let l1 = if c1 { -1.0 } else { 1.0 };
            if let Some(d) = vit.push(l0, l1) {
                out.push(d);
            }
        }
        assert_eq!(&out[..bits.len()], &bits[..]);
    }
}
