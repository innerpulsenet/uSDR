//! The channel code: a constraint-length 32, rate 1/2 convolutional code
//! over the 50 message bits and 31 zeros that flush the register, an
//! interleaver that spreads a fade across the codeword, and a Fano
//! sequential decoder to undo the code.
//!
//! A code this long cannot be decoded by Viterbi (2^31 states); a
//! sequential decoder walks the code tree instead, following the branch
//! that agrees best with what was received and backing up when the running
//! metric falls under a threshold that it lowers only as far as it has to.
//! At good signal it goes straight down the tree; near the threshold it
//! searches, and on noise it runs out of cycles, which is what keeps it
//! from inventing messages.

/// The two generator polynomials (the tap on the newest bit is set in
/// both, so the two outputs for a 1 are the complement of those for a 0).
const POLY1: u32 = 0xf2d0_5351;
const POLY2: u32 = 0xe461_3c47;
/// Message bits, then the zeros that bring the register back to rest.
pub const DATA_BITS: usize = 50;
pub const TAIL_BITS: usize = 31;
pub const NBITS: usize = DATA_BITS + TAIL_BITS;
pub const NSYM: usize = 2 * NBITS;

fn parity(x: u32) -> u8 {
    (x.count_ones() & 1) as u8
}

/// The two code bits for a register whose newest bit is its lowest.
fn outputs(reg: u32) -> (u8, u8) {
    (parity(reg & POLY1), parity(reg & POLY2))
}

/// The 162 code bits of a 50-bit message (first bit highest), in code order.
pub fn encode(bits: u64) -> [u8; NSYM] {
    let mut out = [0u8; NSYM];
    let mut reg = 0u32;
    for k in 0..NBITS {
        let b = if k < DATA_BITS { ((bits >> (DATA_BITS - 1 - k)) & 1) as u32 } else { 0 };
        reg = (reg << 1) | b;
        let (a, c) = outputs(reg);
        out[2 * k] = a;
        out[2 * k + 1] = c;
    }
    out
}

/// Where each code bit is sent: walking 0..256 with the bits of the
/// counter reversed, code bit `p` goes to the `p`-th reversed value that
/// falls inside the 162 channel symbols.
pub fn interleave_map() -> [usize; NSYM] {
    let mut map = [0usize; NSYM];
    let mut p = 0;
    for i in 0..=255u8 {
        let j = i.reverse_bits() as usize;
        if j < NSYM {
            map[p] = j;
            p += 1;
        }
    }
    map
}

/// The sequential decoder's knobs, in the metric's own units (log2 of a
/// likelihood ratio, scaled by [`SCALE`]).
pub const SCALE: f32 = 16.0;

#[derive(Clone, Copy, Debug)]
pub struct FanoParams {
    /// How far the threshold moves at a time.
    pub delta: i32,
    /// Cycles allowed per decoded bit before giving up.
    pub cycles_per_bit: usize,
}

/// Decode from per-code-bit metrics: `metrics[i] = (m0, m1)` is what code
/// bit `i` (code order) adds to a path that says it was 0 or 1. The 50
/// message bits, first highest, or `None` when the cycles run out.
pub fn fano(metrics: &[(i32, i32); NSYM], params: FanoParams) -> Option<u64> {
    // What each node adds for each of the four output pairs.
    let mut pair = [[0i32; 4]; NBITS];
    for (k, p) in pair.iter_mut().enumerate() {
        let (a, b) = (metrics[2 * k], metrics[2 * k + 1]);
        *p = [a.0 + b.0, a.0 + b.1, a.1 + b.0, a.1 + b.1];
    }
    let tail = NBITS - TAIL_BITS;
    // Per node: the register before its bit, the bit of its best branch,
    // the two branch metrics best first, which branch is being tried, and
    // the path metric on arrival.
    let mut reg = [0u32; NBITS + 1];
    let mut best = [0u32; NBITS];
    let mut tm = [[0i32; 2]; NBITS];
    let mut tried = [0u8; NBITS];
    let mut gamma = [0i32; NBITS + 1];
    let sort = |k: usize, reg: &[u32], best: &mut [u32], tm: &mut [[i32; 2]]| {
        let (a, c) = outputs(reg[k] << 1);
        let s0 = ((a << 1) | c) as usize;
        let m0 = pair[k][s0];
        if k >= tail {
            best[k] = 0;
            tm[k] = [m0, i32::MIN / 4];
        } else {
            let m1 = pair[k][s0 ^ 3];
            if m0 >= m1 {
                best[k] = 0;
                tm[k] = [m0, m1];
            } else {
                best[k] = 1;
                tm[k] = [m1, m0];
            }
        }
    };
    sort(0, &reg, &mut best, &mut tm);
    let delta = params.delta;
    let mut t = 0i32;
    let mut k = 0usize;
    let limit = params.cycles_per_bit * NBITS;
    for _ in 0..limit {
        let ngamma = gamma[k] + tm[k][tried[k] as usize];
        if ngamma >= t {
            // First visit to this node: tighten the threshold as far as the
            // path allows.
            if gamma[k] < t + delta {
                while ngamma >= t + delta {
                    t += delta;
                }
            }
            let bit = best[k] ^ tried[k] as u32;
            reg[k + 1] = (reg[k] << 1) | bit;
            gamma[k + 1] = ngamma;
            k += 1;
            if k == NBITS {
                let mut bits = 0u64;
                for r in &reg[1..=DATA_BITS] {
                    bits = (bits << 1) | (r & 1) as u64;
                }
                return Some(bits);
            }
            sort(k, &reg, &mut best, &mut tm);
            tried[k] = 0;
            continue;
        }
        // Can't go forward: look back.
        loop {
            if k == 0 || gamma[k - 1] < t {
                // Can't back up either: lower the threshold and go forward
                // again on the best branch.
                t -= delta;
                tried[k] = 0;
                break;
            }
            k -= 1;
            if k < tail && tried[k] == 0 {
                tried[k] = 1;
                break;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleave_is_a_permutation() {
        let map = interleave_map();
        let mut seen = [false; NSYM];
        for &j in &map {
            assert!(!seen[j]);
            seen[j] = true;
        }
        assert_eq!(map[0], 0);
        assert_eq!(map[1], 128);
        assert_eq!(map[2], 64);
    }

    #[test]
    fn clean_codeword_decodes_at_once() {
        let bits = 0x2_d3c4_b5a6_9788u64 & ((1 << 50) - 1);
        let code = encode(bits);
        let metrics: [(i32, i32); NSYM] = std::array::from_fn(|i| if code[i] == 1 { (-100, 8) } else { (8, -100) });
        // One cycle per bit: straight down the tree.
        let d = fano(&metrics, FanoParams { delta: 32, cycles_per_bit: 1 }).expect("decodes");
        assert_eq!(d, bits);
    }
}
