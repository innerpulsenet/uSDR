//! SACCH/FACCH error correction and MAC PDU decoding.

use super::duid::BurstType;
use super::{PAYLOAD_DIBITS, Scrambler};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MacPdu {
    Signal {
        nac: u16,
        bytes: Vec<u8>,
    },
    PushToTalk {
        source: u32,
        talkgroup: u16,
        algorithm: u8,
        key_id: u16,
        message_indicator: [u8; 9],
    },
    EndPushToTalk {
        source: u32,
        talkgroup: u16,
    },
    Idle,
    Active,
    Hangtime,
    Other {
        opcode: u8,
        bytes: Vec<u8>,
    },
}

/// Decode one burst's signalling. Voice and unknown DUIDs return `None`.
pub fn decode(
    payload: &[u8; PAYLOAD_DIBITS],
    kind: BurstType,
    superframe_slot: u8,
    scrambler: Option<&Scrambler>,
) -> Option<MacPdu> {
    let fast = matches!(kind, BurstType::Facch | BurstType::FacchScrambled);
    let lcch = kind == BurstType::Lcch;
    if !fast && !lcch && !matches!(kind, BurstType::Sacch | BurstType::SacchScrambled) {
        return None;
    }

    let mut p = *payload;
    if kind.scrambled() {
        let s = scrambler?;
        for (i, dibit) in p.iter_mut().enumerate() {
            // Our payload starts at raw burst offset 20.  The Phase 2 mask is
            // aligned to the 170-dibit window beginning at raw offset 10.
            *dibit = s.apply(superframe_slot as usize * 180 + 10 + i, *dibit);
        }
    }

    let ranges: &[(usize, usize)] = if fast {
        &[(1, 37), (38, 69), (90, 122), (123, 159)]
    } else {
        &[(1, 37), (38, 122), (123, 159)]
    };
    let mut bits = Vec::new();
    for &(start, end) in ranges {
        for &d in &p[start..end] {
            bits.push((d >> 1) & 1);
            bits.push(d & 1);
        }
    }

    let (start_hb, erasures): (usize, &[usize]) = if fast {
        (
            9,
            &[
                0, 1, 2, 3, 4, 5, 6, 7, 8, 54, 55, 56, 57, 58, 59, 60, 61, 62,
            ],
        )
    } else {
        (5, &[0, 1, 2, 3, 4, 57, 58, 59, 60, 61, 62])
    };
    let mut symbols = [0u8; 63];
    for (i, chunk) in bits.chunks_exact(6).enumerate() {
        symbols[start_hb + i] = chunk.iter().fold(0, |v, &b| (v << 1) | b);
    }
    if !recover_erasures(&mut symbols, erasures) {
        return None;
    }

    let useful_bits = if fast { 156 } else { 180 };
    let mut corrected = Vec::with_capacity(useful_bits);
    for &symbol in &symbols[start_hb..] {
        for bit in (0..6).rev() {
            corrected.push((symbol >> bit) & 1);
            if corrected.len() == useful_bits {
                break;
            }
        }
        if corrected.len() == useful_bits {
            break;
        }
    }

    let data_bits = if fast {
        144
    } else if lcch {
        180
    } else {
        168
    };
    let crc_ok = if lcch {
        crc16(&corrected[..data_bits]) == 0
    } else {
        received(&corrected[data_bits..data_bits + 12]) == crc12(&corrected[..data_bits])
    };
    if !crc_ok {
        return None;
    }
    let byte_count = if lcch { 23 } else { data_bits / 8 };
    let mut bytes = vec![0u8; byte_count];
    for (i, byte) in bytes.iter_mut().enumerate() {
        for bit in 0..8 {
            let at = i * 8 + bit;
            *byte <<= 1;
            if at < corrected.len() {
                *byte |= corrected[at];
            }
        }
    }
    parse_mac(&bytes)
}

fn parse_mac(bytes: &[u8]) -> Option<MacPdu> {
    let &first = bytes.first()?;
    let opcode = first >> 5;
    Some(match opcode {
        0 if bytes.len() >= 21 => MacPdu::Signal {
            nac: (u16::from(bytes[19]) << 4) | u16::from(bytes[20] >> 4),
            bytes: bytes.to_vec(),
        },
        1 if bytes.len() >= 18 => {
            let mut message_indicator = [0u8; 9];
            message_indicator.copy_from_slice(&bytes[1..10]);
            MacPdu::PushToTalk {
                source: (u32::from(bytes[13]) << 16)
                    | (u32::from(bytes[14]) << 8)
                    | u32::from(bytes[15]),
                talkgroup: (u16::from(bytes[16]) << 8) | u16::from(bytes[17]),
                algorithm: bytes[10],
                key_id: (u16::from(bytes[11]) << 8) | u16::from(bytes[12]),
                message_indicator,
            }
        }
        2 if bytes.len() >= 18 => MacPdu::EndPushToTalk {
            source: (u32::from(bytes[13]) << 16)
                | (u32::from(bytes[14]) << 8)
                | u32::from(bytes[15]),
            talkgroup: (u16::from(bytes[16]) << 8) | u16::from(bytes[17]),
        },
        3 => MacPdu::Idle,
        4 => MacPdu::Active,
        6 => MacPdu::Hangtime,
        _ => MacPdu::Other {
            opcode,
            bytes: bytes.to_vec(),
        },
    })
}

fn received(bits: &[u8]) -> u16 {
    bits.iter().fold(0, |v, &b| (v << 1) | u16::from(b))
}

fn crc12(bits: &[u8]) -> u16 {
    let mut work = vec![0u8; bits.len() + 12];
    work[..bits.len()].copy_from_slice(bits);
    const POLY: [u8; 13] = [1, 1, 0, 0, 0, 1, 0, 0, 1, 0, 1, 1, 1];
    for i in 0..bits.len() {
        if work[i] != 0 {
            for (j, &p) in POLY.iter().enumerate() {
                work[i + j] ^= p;
            }
        }
    }
    received(&work[bits.len()..]) ^ 0x0fff
}

fn crc16(bits: &[u8]) -> u16 {
    // P25's CCITT CRC, evaluated MSB first. A complete LCCH codeword has a
    // zero remainder, so no separate received/computed comparison is needed.
    let mut crc = 0u16;
    for &bit in bits {
        let feedback = ((crc >> 15) as u8) ^ bit;
        crc <<= 1;
        if feedback != 0 {
            crc ^= 0x1021;
        }
    }
    crc
}

/// Correct the punctured RS(63,35) codeword, including unknown symbol errors.
/// The final MAC CRC remains an independent acceptance gate.
fn recover_erasures(word: &mut [u8; 63], erasures: &[usize]) -> bool {
    super::super::rs64::correct_with_erasures(word, 35, erasures).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OP25_CODEWORD: [u8; 63] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f, 0x20, 0x21, 0x22, 0x02, 0x1d, 0x2b, 0x30, 0x15, 0x0d, 0x0a, 0x0d, 0x15, 0x15,
        0x04, 0x2c, 0x3f, 0x0e, 0x0f, 0x0f, 0x03, 0x21, 0x14, 0x05, 0x19, 0x02, 0x30, 0x29, 0x28,
        0x24, 0x2f, 0x04,
    ];

    #[test]
    fn erasure_solver_matches_op25_rs_63_35() {
        let erasures = [0usize, 1, 2, 3, 4, 57, 58, 59, 60, 61, 62];
        let mut damaged = OP25_CODEWORD;
        for &at in &erasures {
            damaged[at] = 0;
        }
        assert!(recover_erasures(&mut damaged, &erasures));
        assert_eq!(damaged, OP25_CODEWORD);
    }

    #[test]
    fn acch_repairs_unknown_errors_in_addition_to_punctures() {
        let erasures = [0usize, 1, 2, 3, 4, 57, 58, 59, 60, 61, 62];
        let mut damaged = OP25_CODEWORD;
        for &at in &erasures {
            damaged[at] = 0;
        }
        damaged[10] ^= 0x11;
        damaged[31] ^= 0x22;
        damaged[49] ^= 0x07;
        assert!(recover_erasures(&mut damaged, &erasures));
        assert_eq!(damaged, OP25_CODEWORD);
    }

    #[test]
    fn crc12_matches_the_reference_polynomial() {
        let bits = [1, 0, 1, 1, 0, 0, 1, 0];
        assert_eq!(crc12(&bits), 0x0c77);
    }
}
