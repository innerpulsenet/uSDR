//! Ordered-statistics decoding of the (174, 91) code, for words belief
//! propagation could not settle.
//!
//! The 91 most reliable bits that are independent in the code fix a
//! codeword on their own. The decoder builds that word from their hard
//! decisions, then tries it again with every one and every two of those
//! bits flipped, and keeps the word nearest the soft bits. Where
//! propagation wanders between words on a faded transmission, this lands on
//! one. The method is the one WSJT-X runs after its own propagation fails
//! (Franke and Taylor, after Fossorier and Lin).
//!
//! It always returns a codeword, so the CRC and the caller's own screen
//! are what tell a transmission from noise.

use super::ldpc::{K, N};
use super::tables::GENERATOR;
use std::sync::OnceLock;

type Row = [u64; 3];

fn bit(r: &Row, j: usize) -> bool {
    r[j / 64] >> (j % 64) & 1 == 1
}

/// The systematic generator: row `i` is message bit `i` and the parity
/// bits it takes part in.
fn generator() -> &'static [Row; K] {
    static G: OnceLock<[Row; K]> = OnceLock::new();
    G.get_or_init(|| {
        let mut rows = [[0u64; 3]; K];
        for (i, row) in rows.iter_mut().enumerate() {
            row[i / 64] |= 1 << (i % 64);
            for (p, g) in GENERATOR.iter().enumerate() {
                if g[i / 8] >> (7 - i % 8) & 1 == 1 {
                    let j = K + p;
                    row[j / 64] |= 1 << (j % 64);
                }
            }
        }
        rows
    })
}

/// The nearest codeword found, the number of bits where it disagrees with
/// the signs of `llr` among those `counted`, and its distance: the summed
/// size of the soft bits it contradicts. `llr` is positive for a one.
pub fn decode(llr: &[f32; N], counted: &[bool; N]) -> ([u8; N], usize, f32) {
    // Most reliable first.
    let mut perm: [usize; N] = std::array::from_fn(|i| i);
    perm.sort_unstable_by(|&a, &b| llr[b].abs().total_cmp(&llr[a].abs()));
    // The generator with its columns in that order, reduced so the first
    // independent 91 columns carry the identity.
    let mut rows = [[0u64; 3]; K];
    for (row, g) in rows.iter_mut().zip(generator()) {
        for (j, &p) in perm.iter().enumerate() {
            if bit(g, p) {
                row[j / 64] |= 1 << (j % 64);
            }
        }
    }
    let mut pivots = [0usize; K];
    let mut rank = 0;
    for j in 0..N {
        if rank == K {
            break;
        }
        let Some(r) = (rank..K).find(|&r| bit(&rows[r], j)) else { continue };
        rows.swap(rank, r);
        let pivot = rows[rank];
        for (i, row) in rows.iter_mut().enumerate() {
            if i != rank && bit(row, j) {
                for w in 0..3 {
                    row[w] ^= pivot[w];
                }
            }
        }
        pivots[rank] = j;
        rank += 1;
    }
    let mut hard: Row = [0; 3];
    let mut mag = [0f32; N];
    for (j, &p) in perm.iter().enumerate() {
        if llr[p] > 0.0 {
            hard[j / 64] |= 1 << (j % 64);
        }
        mag[j] = llr[p].abs();
    }
    let mut c0: Row = [0; 3];
    for (row, &j) in rows.iter().zip(&pivots) {
        if bit(&hard, j) {
            for w in 0..3 {
                c0[w] ^= row[w];
            }
        }
    }
    let dist = |c: &Row| -> f32 {
        let mut d = 0f32;
        for w in 0..3 {
            let mut e = c[w] ^ hard[w];
            while e != 0 {
                d += mag[w * 64 + e.trailing_zeros() as usize];
                e &= e - 1;
            }
        }
        d
    };
    let mut best = (c0, dist(&c0));
    for a in 0..K {
        let mut c1 = c0;
        for w in 0..3 {
            c1[w] ^= rows[a][w];
        }
        let d = dist(&c1);
        if d < best.1 {
            best = (c1, d);
        }
        for row in &rows[a + 1..] {
            let c2 = [c1[0] ^ row[0], c1[1] ^ row[1], c1[2] ^ row[2]];
            let d = dist(&c2);
            if d < best.1 {
                best = (c2, d);
            }
        }
    }
    let mut cw = [0u8; N];
    let mut errors = 0;
    for (j, &p) in perm.iter().enumerate() {
        cw[p] = bit(&best.0, j) as u8;
        if counted[p] && (cw[p] == 1) != (llr[p] > 0.0) {
            errors += 1;
        }
    }
    (cw, errors, best.1)
}

#[cfg(test)]
mod tests {
    use super::super::ldpc;
    use super::*;

    fn word(seed: u8) -> [u8; N] {
        let mut p = [seed; 10];
        p[3] ^= 0x5a;
        p[9] = 0x48;
        ldpc::encode(&ldpc::add_crc(&p))
    }

    #[test]
    fn the_generator_makes_the_encoders_codewords() {
        let cw = word(0x31);
        let mut sum: Row = [0; 3];
        for (i, g) in generator().iter().enumerate() {
            if cw[i] == 1 {
                for w in 0..3 {
                    sum[w] ^= g[w];
                }
            }
        }
        for (j, &b) in cw.iter().enumerate() {
            assert_eq!(bit(&sum, j), b == 1, "bit {j}");
        }
    }

    #[test]
    fn a_word_too_damaged_for_propagation_is_recovered() {
        let cw = word(0xc7);
        let mut llr = [0f32; N];
        let mut x = 0x2545_f491_u32;
        let mut wrong = 0;
        for (i, b) in cw.iter().enumerate() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let size = 0.5 + (x >> 8) as f32 / (1u32 << 24) as f32 * 4.0;
            llr[i] = if *b == 1 { size } else { -size };
            // Every fifth bit wrong, but unsure of itself.
            if i % 5 == 2 {
                llr[i] = -llr[i].signum() * 0.4;
                wrong += 1;
            }
        }
        assert!(ldpc::bp_decode(&llr, 50).1 > 0, "propagation alone should fail on this one");
        let (got, errors, _) = decode(&llr, &[true; N]);
        assert_eq!(got, cw);
        assert_eq!(errors, wrong);
    }
}
