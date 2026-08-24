//! The Phase 2 payload scrambler.
//!
//! Every voice channel's payload dibits are XORed with a keystream unique to
//! the system, so a receiver cannot read a burst without knowing the network's
//! WACN, system ID and NAC — all three of which are broadcast in the clear on
//! the Phase 1 control channel (NET_STS_BCST for WACN/SYSID, the NID for NAC).
//!
//! The keystream is a 44-bit linear-feedback shift register. The seed
//! `wacn<<24 | sysid<<12 | nac` is first multiplied by a fixed GF(2) matrix,
//! then the register is clocked, emitting its top bit each step; bits are
//! paired into dibits. This is a direct port of op25's `lfsr.py`, and is checked
//! bit-for-bit against that implementation's output in the tests.
//!
//! The matrix multiply reduces to a sparse convolution: the reference matrix
//! `M` has `M[i][j] = 1` exactly when `j - i ∈ {0, 4, 9, 15, 20, 34}`, so the
//! transformed bit `j` is the XOR of seed bits `j - t` for each tap `t`.

use super::SUPERFRAME_DIBITS;

/// Taps of the seed-conditioning matrix, as offsets `j - i`.
const SEED_TAPS: [usize; 6] = [0, 4, 9, 15, 20, 34];

/// One superframe of scrambling dibits (each 0..=3), the sequence's full period.
pub struct Scrambler {
    mask: Vec<u8>,
}

impl Scrambler {
    /// Build the keystream for a system. `nac` is 12 bits, `sysid` 12, `wacn`
    /// 20; higher bits are ignored.
    pub fn new(wacn: u32, sysid: u16, nac: u16) -> Self {
        let seed = (u64::from(wacn) << 24) | (u64::from(sysid) << 12) | u64::from(nac & 0xFFF);
        let mut reg = condition_seed(seed);
        // Two output bits per dibit, one superframe long.
        let mut mask = Vec::with_capacity(SUPERFRAME_DIBITS);
        for _ in 0..SUPERFRAME_DIBITS {
            let hi = top_bit(reg);
            reg = clock(reg);
            let lo = top_bit(reg);
            reg = clock(reg);
            mask.push((hi << 1) | lo);
        }
        Self { mask }
    }

    /// Descramble one payload dibit at absolute superframe position `pos`
    /// (`slot * BURST_DIBITS + offset_within_burst`). XOR is its own inverse, so
    /// this both scrambles and descrambles.
    pub fn apply(&self, pos: usize, dibit: u8) -> u8 {
        dibit ^ self.mask[pos % SUPERFRAME_DIBITS]
    }

    /// The raw keystream dibit at a position, for testing and diagnostics.
    pub fn dibit(&self, pos: usize) -> u8 {
        self.mask[pos % SUPERFRAME_DIBITS]
    }
}

/// Multiply the 44-bit seed by the conditioning matrix (see module docs).
fn condition_seed(seed: u64) -> u64 {
    // Seed bits, MSB first: bit position `i` (0 = MSB) holds seed bit 43-i.
    let seed_bit = |i: usize| ((seed >> (43 - i)) & 1) as u8;
    let mut out = 0u64;
    for j in 0..44 {
        let mut b = 0u8;
        for &t in &SEED_TAPS {
            if j >= t {
                b ^= seed_bit(j - t);
            }
        }
        out = (out << 1) | u64::from(b & 1);
    }
    out
}

/// The register's output bit: bit 43 (MSB of the 44-bit value).
fn top_bit(reg: u64) -> u8 {
    ((reg >> 43) & 1) as u8
}

/// One LFSR clock. The register is six independent sub-shifts whose feedback
/// bits cross-couple through the first stage's carry, exactly as op25 clocks it.
fn clock(reg: u64) -> u64 {
    let (mut s1, mut s2, mut s3, mut s4, mut s5, mut s6) = disasm(reg);
    let cy1 = (s1 >> 3) & 1;
    let cy2 = (s2 >> 4) & 1;
    let cy3 = (s3 >> 5) & 1;
    let cy4 = (s4 >> 4) & 1;
    let cy5 = (s5 >> 13) & 1;
    let cy6 = (s6 >> 9) & 1;
    s1 = ((s1 << 1) & 0xF) | (cy1 ^ cy2);
    s2 = ((s2 << 1) & 0x1F) | (cy1 ^ cy3);
    s3 = ((s3 << 1) & 0x3F) | (cy1 ^ cy4);
    s4 = ((s4 << 1) & 0x1F) | (cy1 ^ cy5);
    s5 = ((s5 << 1) & 0x3FFF) | (cy1 ^ cy6);
    s6 = ((s6 << 1) & 0x3FF) | cy1;
    asm(s1, s2, s3, s4, s5, s6)
}

/// Split the register into its six sub-registers (widths 4,5,6,5,14,10).
fn disasm(r: u64) -> (u64, u64, u64, u64, u64, u64) {
    (
        (r >> 40) & 0xF,
        (r >> 35) & 0x1F,
        (r >> 29) & 0x3F,
        (r >> 24) & 0x1F,
        (r >> 10) & 0x3FFF,
        r & 0x3FF,
    )
}

fn asm(s1: u64, s2: u64, s3: u64, s4: u64, s5: u64, s6: u64) -> u64 {
    (s1 << 40) | (s2 << 35) | (s3 << 29) | (s4 << 24) | (s5 << 10) | s6
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference sequence op25's `lfsr.py` prints for its self-test
    /// parameters (nac 0x293, sysid 0x18, wacn 0x1) — the first 100 dibits,
    /// captured directly from that program. If this drifts, the descrambler is
    /// wrong and no voice burst will ever decode.
    const OP25_REF: &str = "0000000001000120022103120231033113221330302120333223200211202310320222132032103201012033033320222001";

    #[test]
    fn matches_op25_reference_sequence() {
        let s = Scrambler::new(0x1, 0x18, 0x293);
        let got: String = (0..100).map(|i| (b'0' + s.dibit(i)) as char).collect();
        assert_eq!(got, OP25_REF);
    }

    #[test]
    fn apply_is_its_own_inverse() {
        let s = Scrambler::new(0xBEE07, 0x2AB, 0x3A0);
        for pos in [0usize, 1, 179, 180, 2159] {
            for d in 0..4u8 {
                assert_eq!(s.apply(pos, s.apply(pos, d)), d);
            }
        }
    }

    #[test]
    fn the_mask_spans_a_whole_superframe() {
        let s = Scrambler::new(0x1, 0x18, 0x293);
        // Position wraps modulo the superframe.
        assert_eq!(s.dibit(0), s.dibit(SUPERFRAME_DIBITS));
    }
}
