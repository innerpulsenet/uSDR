//! The forward error correction: a 12-bit CRC over the 75 message bits,
//! then the (174, 87) LDPC code over message and CRC.
//!
//! The codeword goes out parity first: the first 29 data symbols carry the
//! 87 parity bits and the last 29 the message, three bits a symbol, most
//! significant first, with no Gray mapping.

use super::tables::{GENERATOR, MN, NM};
use crate::ft8::ldpc;

pub use ldpc::N;
/// Message and CRC bits.
pub const K: usize = 87;

/// CRC-12, polynomial 0xc06, over the frame's 11 bytes with the CRC's own
/// bits zero (an augmented CRC, as boost computes it), then XORed with 42:
/// JS8Call's way of keeping its frames from passing an FT8 check.
fn crc12(bytes: &[u8; 11]) -> u16 {
    let mut rem: u16 = 0;
    for i in 0..88 {
        rem = (rem << 1) | ((bytes[i / 8] >> (7 - i % 8)) & 1) as u16;
        if rem & 0x1000 != 0 {
            rem ^= 0x1c06;
        }
    }
    (rem & 0x0fff) ^ 42
}

/// The 87 bits the code protects, in 11 bytes: the 72-bit payload, the
/// three flag bits and the CRC.
fn add_crc(payload: &[u8; 9], flags: u8) -> [u8; 11] {
    let mut b = [0u8; 11];
    b[..9].copy_from_slice(payload);
    b[9] = (flags & 7) << 5;
    let crc = crc12(&b);
    b[9] |= ((crc >> 7) & 0x1f) as u8;
    b[10] = ((crc & 0x7f) << 1) as u8;
    b
}

/// The 174-bit codeword, one bit per byte: 87 parity bits, then the 87
/// message bits.
pub fn encode(payload: &[u8; 9], flags: u8) -> [u8; N] {
    let m = add_crc(payload, flags);
    let mut cw = [0u8; N];
    for (row, g) in GENERATOR.iter().enumerate() {
        // The last bit of `m` is always zero, so the generator's unused
        // 88th column adds nothing.
        cw[row] = (m.iter().zip(g).fold(0u32, |p, (a, b)| p ^ (a & b).count_ones()) & 1) as u8;
    }
    for i in 0..K {
        cw[K + i] = (m[i / 8] >> (7 - i % 8)) & 1;
    }
    cw
}

/// Belief propagation, then the CRC: the payload and flag bits, and the
/// codeword they came from, or `None` if either check fails.
pub fn decode(llr: &[f32; N], iters: usize) -> Option<([u8; 9], u8, [u8; N])> {
    let (cw, errors) = ldpc::bp_decode_with(llr, iters, &MN, &NM);
    if errors > 0 {
        return None;
    }
    let mut m = [0u8; 11];
    for i in 0..K {
        m[i / 8] |= cw[K + i] << (7 - i % 8);
    }
    let sent = (((m[9] & 0x1f) as u16) << 7) | (m[10] >> 1) as u16;
    m[9] &= 0xe0;
    m[10] = 0;
    if crc12(&m) != sent {
        return None;
    }
    let mut payload = [0u8; 9];
    payload.copy_from_slice(&m[..9]);
    Some((payload, m[9] >> 5, cw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_describe_one_code() {
        let mut weight = [0; N];
        for row in NM {
            for n in row.into_iter().filter(|&n| n != 0) {
                weight[n as usize - 1] += 1;
            }
        }
        assert!(weight.iter().all(|&w| w == 3));
        for (n, checks) in MN.iter().enumerate() {
            for &m in checks {
                assert!(NM[m as usize - 1].contains(&(n as u8 + 1)), "bit {n} check {m}");
            }
        }
    }

    #[test]
    fn encoded_words_pass_every_check_and_decode() {
        for seed in 0u32..20 {
            let mut p = [0u8; 9];
            for (i, b) in p.iter_mut().enumerate() {
                *b = (seed.wrapping_mul(2_654_435_761).rotate_left(i as u32 * 3) >> 7) as u8;
            }
            let flags = (seed % 8) as u8;
            let cw = encode(&p, flags);
            assert_eq!(ldpc::check_with(&cw, &NM), 0, "seed {seed}");
            let llr: [f32; N] = std::array::from_fn(|i| if cw[i] == 1 { 4.0 } else { -4.0 });
            let (q, f, _) = decode(&llr, 10).expect("decodes");
            assert_eq!((q, f), (p, flags));
            // A flipped message bit that the code then "repairs" into another
            // codeword is what the CRC is for: flip one in the payload of a
            // re-encoded word and the CRC alone must refuse it.
            let mut m = add_crc(&p, flags);
            m[2] ^= 0x10;
            let sent = (((m[9] & 0x1f) as u16) << 7) | (m[10] >> 1) as u16;
            m[9] &= 0xe0;
            m[10] = 0;
            assert_ne!(crc12(&m), sent);
        }
    }

    #[test]
    fn belief_propagation_repairs_flipped_bits() {
        let p = [0x5a, 0x3c, 0x99, 0x01, 0xfe, 0x42, 0x77, 0x18, 0xc3];
        let cw = encode(&p, 3);
        let mut llr: [f32; N] = std::array::from_fn(|i| if cw[i] == 1 { 3.0 } else { -3.0 });
        for i in (5..N).step_by(14) {
            llr[i] = -llr[i] * 0.4;
        }
        let (q, f, got) = decode(&llr, 30).expect("repaired");
        assert_eq!((q, f), (p, 3));
        assert_eq!(got, cw);
    }
}
