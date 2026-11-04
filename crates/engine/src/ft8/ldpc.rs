//! The forward error correction both modes share: a 14-bit CRC over the
//! 77-bit message, then a (174, 91) LDPC code over message and CRC.

use super::tables::{GENERATOR, MN, NM};

pub const N: usize = 174;
pub const K: usize = 91;
pub const M: usize = 83;

/// CRC-14, polynomial 0x2757 without its leading one.
const CRC_POLY: u16 = 0x2757;

/// The CRC of the first `bits` bits of `msg`, MSB first.
pub fn crc14(msg: &[u8], bits: usize) -> u16 {
    let top = 1u16 << 13;
    let mut rem: u16 = 0;
    for i in 0..bits {
        if i % 8 == 0 {
            rem ^= (msg[i / 8] as u16) << 6;
        }
        rem = if rem & top != 0 { (rem << 1) ^ CRC_POLY } else { rem << 1 };
    }
    rem & 0x3fff
}

/// The 91 bits the code protects: the 77-bit payload and its CRC. The CRC
/// is taken over the payload zero-extended to 82 bits, as the spec says.
pub fn add_crc(payload: &[u8; 10]) -> [u8; 12] {
    let mut a91 = [0u8; 12];
    a91[..10].copy_from_slice(payload);
    a91[9] &= 0xf8;
    let crc = crc14(&a91, 96 - 14);
    a91[9] |= (crc >> 11) as u8;
    a91[10] = (crc >> 3) as u8;
    a91[11] = (crc << 5) as u8;
    a91
}

/// Whether a decoded 91 bits carry a CRC that matches their payload.
pub fn crc_ok(a91: &[u8; 12]) -> bool {
    let sent = ((a91[9] as u16 & 0x07) << 11) | ((a91[10] as u16) << 3) | (a91[11] as u16 >> 5);
    let mut m = *a91;
    m[9] &= 0xf8;
    m[10] = 0;
    m[11] = 0;
    crc14(&m, 96 - 14) == sent
}

/// The 174-bit codeword, one bit per byte: the 91 message bits followed by
/// 83 parity bits.
pub fn encode(a91: &[u8; 12]) -> [u8; N] {
    let mut cw = [0u8; N];
    for (i, b) in cw.iter_mut().take(K).enumerate() {
        *b = (a91[i / 8] >> (7 - i % 8)) & 1;
    }
    for (row, g) in GENERATOR.iter().enumerate() {
        let parity = a91.iter().zip(g).fold(0u32, |p, (m, g)| p ^ (m & g).count_ones()) & 1;
        cw[K + row] = parity as u8;
    }
    cw
}

/// How many parity checks a hard-decided codeword fails.
pub fn check(cw: &[u8; N]) -> usize {
    NM.iter()
        .filter(|row| row.iter().filter(|&&n| n != 0).fold(0u8, |x, &n| x ^ cw[n as usize - 1]) != 0)
        .count()
}

/// Belief propagation (sum-product) over the code's sparse graph.
///
/// `llr` holds log(P(1)/P(0)) per codeword bit, positive meaning a one.
/// Returns the best hard decision found and how many checks it fails;
/// zero means a valid codeword. Stops early on success, and gives up on an
/// all-zero word, which the spec rules out and noise converges to.
pub fn bp_decode(llr: &[f32; N], max_iters: usize) -> ([u8; N], usize) {
    // Messages from each bit to its three checks, and from each check to
    // its six or seven bits.
    let mut tov = [[0f32; 3]; N];
    let mut toc = [[0f32; 7]; M];
    let mut plain = [0u8; N];
    let mut best = (plain, M);
    let (mut last, mut stalled) = (M, 0);
    for iter in 0..max_iters {
        let mut ones = 0;
        for n in 0..N {
            plain[n] = u8::from(llr[n] + tov[n][0] + tov[n][1] + tov[n][2] > 0.0);
            ones += plain[n] as usize;
        }
        if ones == 0 {
            break;
        }
        let errors = check(&plain);
        if errors < best.1 {
            best = (plain, errors);
            if errors == 0 {
                break;
            }
        }
        // Far from a codeword and no longer getting closer: noise, most
        // likely. WSJT-X gives up on the same terms.
        stalled = if errors >= last { stalled + 1 } else { 0 };
        last = errors;
        if stalled >= 5 && iter >= 10 && errors > 15 {
            break;
        }
        for (m, row) in NM.iter().enumerate() {
            for (ni, &n1) in row.iter().enumerate() {
                if n1 == 0 {
                    continue;
                }
                let n = n1 as usize - 1;
                let mut t = llr[n];
                for (k, &mm) in MN[n].iter().enumerate() {
                    if mm as usize - 1 != m {
                        t += tov[n][k];
                    }
                }
                toc[m][ni] = fast_tanh(-t / 2.0);
            }
        }
        for n in 0..N {
            for (k, &m1) in MN[n].iter().enumerate() {
                let m = m1 as usize - 1;
                let mut t = 1.0f32;
                for (ni, &n1) in NM[m].iter().enumerate() {
                    if n1 != 0 && n1 as usize - 1 != n {
                        t *= toc[m][ni];
                    }
                }
                tov[n][k] = -2.0 * fast_atanh(t);
            }
        }
    }
    best
}

/// Padé approximations: the decoder spends most of its time here, and the
/// messages need only a few digits.
fn fast_tanh(x: f32) -> f32 {
    if x < -4.97 {
        return -1.0;
    }
    if x > 4.97 {
        return 1.0;
    }
    let x2 = x * x;
    x * (945.0 + x2 * (105.0 + x2)) / (945.0 + x2 * (420.0 + x2 * 15.0))
}

fn fast_atanh(x: f32) -> f32 {
    let x2 = x * x;
    x * (945.0 + x2 * (-735.0 + x2 * 64.0)) / (945.0 + x2 * (-1050.0 + x2 * 225.0))
}

/// The 91 message bits of a codeword, packed MSB first.
pub fn pack91(cw: &[u8; N]) -> [u8; 12] {
    let mut a91 = [0u8; 12];
    for i in 0..K {
        a91[i / 8] |= cw[i] << (7 - i % 8);
    }
    a91
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_column_of_the_parity_matrix_has_weight_three() {
        let mut weight = [0; N];
        for row in NM {
            for n in row.into_iter().filter(|&n| n != 0) {
                weight[n as usize - 1] += 1;
            }
        }
        assert!(weight.iter().all(|&w| w == 3));
    }

    #[test]
    fn encoded_words_pass_every_check_and_carry_their_crc() {
        for seed in 0u32..20 {
            let mut p = [0u8; 10];
            for (i, b) in p.iter_mut().enumerate() {
                *b = (seed.wrapping_mul(2_654_435_761).rotate_left(i as u32 * 3) >> 7) as u8;
            }
            p[9] &= 0xf8;
            let a91 = add_crc(&p);
            assert!(crc_ok(&a91));
            let cw = encode(&a91);
            assert_eq!(check(&cw), 0, "seed {seed}");
            assert_eq!(pack91(&cw), a91);
            // One flipped payload bit breaks the CRC.
            let mut bad = a91;
            bad[3] ^= 0x10;
            assert!(!crc_ok(&bad));
        }
    }

    #[test]
    fn belief_propagation_repairs_flipped_bits() {
        let mut p = [0x5au8; 10];
        p[9] = 0x48;
        let cw = encode(&add_crc(&p));
        let mut llr = [0f32; N];
        for (i, b) in cw.iter().enumerate() {
            llr[i] = if *b == 1 { 3.0 } else { -3.0 };
        }
        // Wrong and fairly sure about a dozen bits.
        for i in (5..N).step_by(14) {
            llr[i] = -llr[i] * 0.4;
        }
        let (plain, errors) = bp_decode(&llr, 30);
        assert_eq!(errors, 0);
        assert_eq!(plain, cw);
    }
}
