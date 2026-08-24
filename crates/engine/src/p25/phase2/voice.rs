//! Phase 2 AMBE+2 voice-codeword extraction and FEC.

use super::{BurstType, PAYLOAD_DIBITS, Scrambler};

pub(crate) struct VoiceFrame {
    pub data: [u8; 7],
    /// Corrected bits across the protected C0/C1 portions.
    pub errors: u8,
    /// Extended Golay C0 detects four-bit patterns that cannot be corrected.
    pub valid: bool,
}

/// Extract and error-correct the 49 information bits from each voice codeword
/// in a 4V or 2V burst. The returned seven bytes are packed MSB first in the
/// wire format accepted by AMBE+2 decoders; the low seven bits of byte six are
/// padding.
pub fn extract(
    payload: &[u8; PAYLOAD_DIBITS],
    kind: BurstType,
    superframe_slot: u8,
    scrambler: &Scrambler,
) -> Vec<VoiceFrame> {
    let starts: &[usize] = match kind {
        BurstType::Voice4 => &[1, 38, 86, 123],
        BurstType::Voice2 => &[1, 38],
        _ => return Vec::new(),
    };
    let mut p = *payload;
    for (i, dibit) in p.iter_mut().enumerate() {
        *dibit = scrambler.apply(superframe_slot as usize * 180 + 10 + i, *dibit);
    }
    starts
        .iter()
        // A transmitted all-zero 36-dibit block is a null codeword, not AMBE.
        .filter(|&&start| payload[start..start + 36].iter().any(|&dibit| dibit != 0))
        .map(|&start| decode_codeword(&p[start..start + 36]))
        .collect()
}

fn decode_codeword(dibits: &[u8]) -> VoiceFrame {
    let mut vf = [0u8; 72];
    for (i, &d) in dibits.iter().enumerate() {
        vf[i * 2] = (d >> 1) & 1;
        vf[i * 2 + 1] = d & 1;
    }

    // These are the inverse of the P25 Phase 2 voice interleave. Each array
    // gives the received bit index for ascending numeric bit positions.
    const C0: [usize; 24] = [
        21, 17, 13, 9, 5, 1, 68, 64, 60, 56, 52, 48, 44, 40, 36, 32, 28, 24, 20, 16, 12, 8, 4, 0,
    ];
    const C1: [usize; 23] = [
        42, 38, 34, 30, 26, 22, 18, 14, 10, 6, 2, 69, 65, 61, 57, 53, 49, 45, 41, 37, 33, 29, 25,
    ];
    const C2: [usize; 11] = [15, 11, 7, 3, 70, 66, 62, 58, 54, 50, 46];
    const C3: [usize; 14] = [71, 67, 63, 59, 55, 51, 47, 43, 39, 35, 31, 27, 23, 19];

    let c0 = gather(&vf, &C0);
    let c1 = gather(&vf, &C1);
    let (u0, c0_errors) = golay23_decode(c0 >> 1);
    let expected_parity = golay23_encode(u0).count_ones() & 1;
    let parity_error = u32::from((c0 & 1) != expected_parity);
    let c0_errors = c0_errors + parity_error;
    let (u1, c1_errors) = golay23_decode(c1 ^ modulation(u0));
    let u2 = gather(&vf, &C2);
    let u3 = gather(&vf, &C3);

    VoiceFrame {
        data: [
            (u0 >> 4) as u8,
            (((u0 & 0x0f) << 4) | (u1 >> 8)) as u8,
            u1 as u8,
            (u2 >> 3) as u8,
            (((u2 & 7) << 5) | (u3 >> 9)) as u8,
            (u3 >> 1) as u8,
            ((u3 & 1) << 7) as u8,
        ],
        errors: (c0_errors + c1_errors) as u8,
        valid: c0_errors <= 3,
    }
}

fn gather<const N: usize>(bits: &[u8; 72], indexes: &[usize; N]) -> u32 {
    indexes
        .iter()
        .enumerate()
        .fold(0u32, |v, (bit, &at)| v | (u32::from(bits[at]) << bit))
}

fn modulation(seed: u32) -> u32 {
    let mut state = 16 * seed;
    let mut out = 0u32;
    for _ in 0..23 {
        state = (173 * state + 13_849) % 65_536;
        out = (out << 1) | ((state / 32_768) & 1);
    }
    out
}

pub(crate) fn golay_syndrome_table() -> &'static [u32; 2048] {
    static TABLE: std::sync::OnceLock<[u32; 2048]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 2048];
        for i in 0..23 {
            let e1 = 1u32 << i;
            let syn1 = (e1 & 0x07ff) ^ (golay23_encode(e1 >> 11) & 0x07ff);
            table[syn1 as usize] = e1;
            for j in (i + 1)..23 {
                let e2 = e1 | (1u32 << j);
                let syn2 = (e2 & 0x07ff) ^ (golay23_encode(e2 >> 11) & 0x07ff);
                table[syn2 as usize] = e2;
                for k in (j + 1)..23 {
                    let e3 = e2 | (1u32 << k);
                    let syn3 = (e3 & 0x07ff) ^ (golay23_encode(e3 >> 11) & 0x07ff);
                    table[syn3 as usize] = e3;
                }
            }
        }
        table
    })
}

pub(crate) fn golay23_decode(received: u32) -> (u32, u32) {
    let rec_data = (received >> 11) & 0x0fff;
    let rec_parity = received & 0x07ff;
    let calc_parity = golay23_encode(rec_data) & 0x07ff;
    let syndrome = (rec_parity ^ calc_parity) as usize;
    let table = golay_syndrome_table();
    let err_pattern = table[syndrome];
    let corrected = received ^ err_pattern;
    (corrected >> 11, err_pattern.count_ones())
}

pub(crate) fn golay23_encode(data: u32) -> u32 {
    let mut word = (data & 0x0fff) << 11;
    let mut divisor = 0x0c75u32 << 11;
    for bit in (11..=22).rev() {
        if word & (1 << bit) != 0 {
            word ^= divisor;
        }
        divisor >>= 1;
    }
    ((data & 0x0fff) << 11) | (word & 0x07ff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golay_corrects_three_bits() {
        let code = golay23_encode(0x0a53);
        assert_eq!(golay23_decode(code ^ 0x0000_0085), (0x0a53, 3));
    }

    #[test]
    fn extended_golay_detects_four_bad_c0_bits() {
        let data = 0x0a53;
        let code = golay23_encode(data);
        let parity = code.count_ones() & 1;
        let damaged = ((code << 1) | parity) ^ 0x0000_00aa;
        let (decoded, distance) = golay23_decode(damaged >> 1);
        let extended =
            distance + u32::from((damaged & 1) != (golay23_encode(decoded).count_ones() & 1));
        assert_eq!(extended, 4);
    }

    #[test]
    fn modulation_is_23_bits() {
        assert!(modulation(0x0fff) < (1 << 23));
    }

    #[test]
    fn voice_fec_matches_op25_reference() {
        // Generated independently by p25p2_vf::encode_vcw/process_vcw for a
        // fixed parameter vector, then frozen here as an interoperability
        // vector. This checks interleave direction, Golay orientation and the
        // final 49-bit packing convention together.
        let dibits: Vec<u8> = "211222112301110023133022222311132221"
            .bytes()
            .map(|b| b - b'0')
            .collect();
        assert_eq!(
            decode_codeword(&dibits).data,
            [0xa8, 0xb8, 0x08, 0xed, 0x3d, 0x8e, 0x80]
        );
        assert!(decode_codeword(&dibits).valid);
        assert_eq!(decode_codeword(&dibits).errors, 0);
    }
}
