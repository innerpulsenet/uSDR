//! DMR AMBE+2 voice-codeword extraction and FEC.
//!
//! Each voice burst carries three 72-bit "3600x2450" frames — the same inner
//! code P25 Phase 2 uses (Golay(24,12) + Golay(23,12) + uncoded C2/C3) under
//! a DMR-specific 4×24 interleave. After correction the 49 information bits
//! are packed MSB-first into the seven-byte buffer `rmbe` expects.

use crate::p25::phase2::voice::{golay23_decode, golay23_encode};

/// One deinterleaved, error-corrected AMBE+2 frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoiceFrame {
    pub data: [u8; 7],
    /// Corrected bits across the protected C0/C1 portions.
    pub errors: u8,
    /// Extended Golay C0 detects four-bit patterns that cannot be corrected.
    pub valid: bool,
}

/// DMR AMBE interleave schedule, transcribed from dsd's `dmr_const.h`.
/// Each of the 36 dibits writes its MSB into `ambe_fr[rW][rX]` and its LSB
/// into `ambe_fr[rY][rZ]`.
const RW: [usize; 36] = [
    0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 2, 0, 2, 0, 2, 0, 2, 0, 2,
    0, 2, 0, 2,
];
const RX: [usize; 36] = [
    23, 10, 22, 9, 21, 8, 20, 7, 19, 6, 18, 5, 17, 4, 16, 3, 15, 2, 14, 1, 13, 0, 12, 10, 11, 9,
    10, 8, 9, 7, 8, 6, 7, 5, 6, 4,
];
const RY: [usize; 36] = [
    0, 2, 0, 2, 0, 2, 0, 2, 0, 3, 0, 3, 1, 3, 1, 3, 1, 3, 1, 3, 1, 3, 1, 3, 1, 3, 1, 3, 1, 3, 1, 3,
    1, 3, 1, 3,
];
const RZ: [usize; 36] = [
    5, 3, 4, 2, 3, 1, 2, 0, 1, 13, 0, 12, 22, 11, 21, 10, 20, 9, 19, 8, 18, 7, 17, 6, 16, 5, 15, 4,
    14, 3, 13, 2, 12, 1, 11, 0,
];

/// The 108 payload dibits of a voice burst: 54 before the centre field and
/// 54 after. Each 36-dibit block is one AMBE frame.
pub fn extract(payload: &[u8; 108]) -> [VoiceFrame; 3] {
    [
        decode_codeword(&payload[0..36]),
        decode_codeword(&payload[36..72]),
        decode_codeword(&payload[72..108]),
    ]
}

/// Inverse of [`extract`]: pack three frames into a voice-burst payload.
#[cfg(test)]
pub fn assemble(frames: &[VoiceFrame; 3]) -> [u8; 108] {
    let mut out = [0u8; 108];
    for (i, frame) in frames.iter().enumerate() {
        let dibits = encode_codeword(frame);
        out[i * 36..(i + 1) * 36].copy_from_slice(&dibits);
    }
    out
}

fn deinterleave(dibits: &[u8]) -> [[u8; 24]; 4] {
    let mut fr = [[0u8; 24]; 4];
    for i in 0..36 {
        let d = dibits[i];
        fr[RW[i]][RX[i]] = (d >> 1) & 1;
        fr[RY[i]][RZ[i]] = d & 1;
    }
    fr
}

#[cfg(test)]
fn interleave(fr: &[[u8; 24]; 4]) -> [u8; 36] {
    let mut dibits = [0u8; 36];
    for i in 0..36 {
        dibits[i] = (fr[RW[i]][RX[i]] << 1) | fr[RY[i]][RZ[i]];
    }
    dibits
}

pub(crate) fn decode_codeword(dibits: &[u8]) -> VoiceFrame {
    let fr = deinterleave(dibits);

    let mut c0 = 0u32;
    for j in 0..24 {
        c0 |= u32::from(fr[0][j]) << j;
    }
    let (u0, c0_errors) = golay23_decode(c0 >> 1);
    let expected_parity = golay23_encode(u0).count_ones() & 1;
    let c0_errors = c0_errors + u32::from((c0 & 1) != expected_parity);

    let mut c1 = 0u32;
    for j in 0..23 {
        c1 |= u32::from(fr[1][j]) << j;
    }
    let (u1, c1_errors) = golay23_decode(c1 ^ modulation(u0));

    let mut u2 = 0u32;
    for j in 0..11 {
        u2 |= u32::from(fr[2][j]) << j;
    }
    let mut u3 = 0u32;
    for j in 0..14 {
        u3 |= u32::from(fr[3][j]) << j;
    }

    VoiceFrame {
        data: pack49(u0, u1, u2, u3),
        errors: (c0_errors + c1_errors) as u8,
        valid: c0_errors <= 3,
    }
}

#[cfg(test)]
fn encode_codeword(frame: &VoiceFrame) -> [u8; 36] {
    let (u0, u1, u2, u3) = unpack49(frame.data);
    let mut fr = [[0u8; 24]; 4];

    let c0 = (golay23_encode(u0) << 1) | (golay23_encode(u0).count_ones() & 1);
    for j in 0..24 {
        fr[0][j] = ((c0 >> j) & 1) as u8;
    }

    let c1 = golay23_encode(u1) ^ modulation(u0);
    for j in 0..23 {
        fr[1][j] = ((c1 >> j) & 1) as u8;
    }

    for j in 0..11 {
        fr[2][j] = ((u2 >> j) & 1) as u8;
    }
    for j in 0..14 {
        fr[3][j] = ((u3 >> j) & 1) as u8;
    }

    interleave(&fr)
}

fn pack49(u0: u32, u1: u32, u2: u32, u3: u32) -> [u8; 7] {
    [
        (u0 >> 4) as u8,
        (((u0 & 0x0f) << 4) | (u1 >> 8)) as u8,
        u1 as u8,
        (u2 >> 3) as u8,
        (((u2 & 7) << 5) | (u3 >> 9)) as u8,
        (u3 >> 1) as u8,
        ((u3 & 1) << 7) as u8,
    ]
}

#[cfg(test)]
fn unpack49(data: [u8; 7]) -> (u32, u32, u32, u32) {
    let u0 = (u32::from(data[0]) << 4) | u32::from(data[1] >> 4);
    let u1 = (u32::from(data[1] & 0x0f) << 8) | u32::from(data[2]);
    let u2 = (u32::from(data[3]) << 3) | u32::from(data[4] >> 5);
    let u3 = (u32::from(data[4] & 0x1f) << 9) | (u32::from(data[5]) << 1) | u32::from(data[6] >> 7);
    (u0, u1, u2, u3)
}

/// mbelib / P25 Phase 2 C1 modulator: a 23-bit mask seeded from C0's data.
fn modulation(seed: u32) -> u32 {
    let mut state = 16 * seed;
    let mut out = 0u32;
    for _ in 0..23 {
        state = (173 * state + 13_849) % 65_536;
        out = (out << 1) | ((state / 32_768) & 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_packed_frame_survives_the_interleave() {
        let original = VoiceFrame {
            data: [0xa8, 0xb8, 0x08, 0xed, 0x3d, 0x8e, 0x80],
            errors: 0,
            valid: true,
        };
        let dibits = encode_codeword(&original);
        let got = decode_codeword(&dibits);
        assert_eq!(got.data, original.data);
        assert!(got.valid);
        assert_eq!(got.errors, 0);
    }

    #[test]
    fn three_frames_round_trip_through_a_burst_payload() {
        let frames = [
            VoiceFrame {
                data: [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x80],
                errors: 0,
                valid: true,
            },
            VoiceFrame {
                data: [0xa8, 0xb8, 0x08, 0xed, 0x3d, 0x8e, 0x80],
                errors: 0,
                valid: true,
            },
            VoiceFrame {
                data: [0x00, 0xff, 0x0f, 0xf0, 0xaa, 0x55, 0x80],
                errors: 0,
                valid: true,
            },
        ];
        assert_eq!(
            extract(&assemble(&frames)).map(|f| f.data),
            frames.map(|f| f.data)
        );
    }

    #[test]
    fn one_flipped_dibit_is_corrected() {
        let original = VoiceFrame {
            data: [0xa8, 0xb8, 0x08, 0xed, 0x3d, 0x8e, 0x80],
            errors: 0,
            valid: true,
        };
        let mut dibits = encode_codeword(&original);
        dibits[4] ^= 0b11;
        let got = decode_codeword(&dibits);
        assert_eq!(got.data, original.data);
        assert!(got.valid);
        assert!(got.errors > 0);
    }

    #[test]
    fn pack49_is_the_inverse_of_unpack49() {
        let data = [0xa8, 0xb8, 0x08, 0xed, 0x3d, 0x8e, 0x80];
        let (u0, u1, u2, u3) = unpack49(data);
        assert_eq!(pack49(u0, u1, u2, u3), data);
    }
}
