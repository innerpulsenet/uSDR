//! The short block codes that protect DMR's signalling fields.
//!
//! Generator and parity-check matrices are transcribed from dsd-fme's
//! `fec.c` (itself following ETSI TS 102 361-1 annex B); decoders here are
//! syndrome-table implementations written for this crate.
//!
//! All three codes are systematic: the data bits lead the codeword, so a
//! receiver that only wants the payload can skip decoding entirely — the
//! FEC buys error correction and, just as valuable, a validity check that
//! keeps noise from impersonating signalling.

/// Hamming(7,4): the CACH's 7-bit TACT field. Corrects one error.
#[allow(dead_code)]
const HAMMING_7_4_G: [[u8; 7]; 4] = [
    [1, 0, 0, 0, 1, 0, 1],
    [0, 1, 0, 0, 1, 1, 1],
    [0, 0, 1, 0, 1, 1, 0],
    [0, 0, 0, 1, 0, 1, 1],
];
const HAMMING_7_4_H: [[u8; 7]; 3] = [
    [1, 1, 1, 0, 1, 0, 0],
    [0, 1, 1, 1, 0, 1, 0],
    [1, 1, 0, 1, 0, 0, 1],
];

/// QR(16,7,6): the EMB field in voice bursts B–F. Corrects two errors.
#[allow(dead_code)]
const QR_16_7_6_G: [[u8; 16]; 7] = [
    [1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 1, 1, 1],
    [0, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 1, 1, 1, 0],
    [0, 0, 1, 0, 0, 0, 0, 1, 1, 0, 1, 1, 0, 1, 1, 1],
    [0, 0, 0, 1, 0, 0, 0, 1, 1, 1, 1, 0, 0, 0, 1, 0],
    [0, 0, 0, 0, 1, 0, 0, 1, 1, 1, 0, 0, 1, 0, 0, 1],
    [0, 0, 0, 0, 0, 1, 0, 0, 1, 1, 1, 0, 0, 1, 0, 1],
    [0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 1, 1, 0, 0, 1, 1],
];
const QR_16_7_6_H: [[u8; 16]; 9] = [
    [0, 1, 1, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 1, 1, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
    [1, 0, 0, 1, 1, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0],
    [0, 0, 1, 1, 0, 1, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0],
    [0, 1, 1, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0],
    [1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0],
    [1, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0],
    [1, 1, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0],
    [1, 0, 1, 0, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1],
];

/// Hamming(16,11,4), used by each row of embedded Full Link Control.
const HAMMING_16_11_4_H: [[u8; 16]; 5] = [
    [1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0, 0],
    [0, 1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0],
    [0, 0, 1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0],
    [1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0, 1, 0],
    [1, 0, 1, 0, 0, 1, 1, 0, 1, 1, 1, 0, 0, 0, 0, 1],
];

/// Golay(20,8): the Slot Type PDU. Corrects three errors.
#[allow(dead_code)]
const GOLAY_20_8_G: [[u8; 20]; 8] = [
    [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 0, 1, 1, 0, 1, 0],
    [0, 1, 0, 0, 0, 0, 0, 0, 1, 1, 0, 1, 1, 0, 0, 1, 1, 0, 0, 1],
    [0, 0, 1, 0, 0, 0, 0, 0, 0, 1, 1, 0, 1, 1, 0, 0, 1, 1, 0, 1],
    [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, 1, 0, 1, 1, 0, 0, 1, 1, 1],
    [0, 0, 0, 0, 1, 0, 0, 0, 1, 1, 0, 1, 1, 1, 0, 0, 0, 1, 1, 0],
    [0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 1, 0, 1, 0, 0, 1, 0, 1, 1, 1],
    [0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 0, 1, 0, 0, 1, 1, 1, 1, 1, 0],
    [0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 1, 1, 1, 0, 1, 0, 1, 1],
];
const GOLAY_20_8_H: [[u8; 20]; 12] = [
    [0, 1, 0, 0, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 0, 1, 1, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 0, 1, 1, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 1, 0, 1, 1, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
    [1, 0, 1, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 1, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0],
    [1, 1, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0],
    [1, 1, 1, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0],
    [0, 0, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0],
    [1, 0, 0, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0],
    [0, 1, 1, 1, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
];

/// Multiply a data row by a generator matrix, over GF(2).
#[allow(dead_code)]
fn encode<const K: usize, const N: usize>(data: &[u8; K], g: &[[u8; N]; K]) -> [u8; N] {
    let mut out = [0u8; N];
    for (i, &bit) in data.iter().enumerate() {
        if bit != 0 {
            for (j, &gij) in g[i].iter().enumerate() {
                out[j] ^= gij;
            }
        }
    }
    out
}

/// Syndrome of a received word against a parity-check matrix.
fn syndrome<const S: usize, const N: usize>(word: &[u8; N], h: &[[u8; N]; S]) -> u32 {
    let mut syn = 0u32;
    for (row, hrow) in h.iter().enumerate() {
        let mut parity = 0u8;
        for (k, &h) in hrow.iter().enumerate() {
            parity ^= word[k] & h;
        }
        syn |= u32::from(parity) << (S - 1 - row);
    }
    syn
}

/// The error pattern (as a position list) registered for each syndrome:
/// every zero, one- and two-bit pattern, so the decode below corrects up to
/// the code's designed power by table lookup.
fn syndrome_table<const S: usize, const N: usize>(h: &[[u8; N]; S]) -> Vec<u64> {
    let mut table = vec![u64::MAX; 1 << S];
    let mut register = |mask: u64| {
        let mut word = [0u8; N];
        for k in 0..N {
            if mask >> k & 1 != 0 {
                word[k] = 1;
            }
        }
        table[syndrome(&word, h) as usize] = mask;
    };
    register(0);
    for i in 0..N {
        register(1u64 << i);
        for j in i + 1..N {
            register((1u64 << i) | (1u64 << j));
        }
    }
    table
}

/// Decode in place; returns the number of corrected errors, or `None` when
/// the word is beyond the table's reach.
fn decode_with_table<const S: usize, const N: usize>(
    word: &mut [u8; N],
    h: &[[u8; N]; S],
    table: &[u64],
) -> Option<u32> {
    let mask = table[syndrome(word, h) as usize];
    if mask == u64::MAX {
        return None;
    }
    for (k, bit) in word.iter_mut().enumerate() {
        if mask >> k & 1 != 0 {
            *bit ^= 1;
        }
    }
    Some(mask.count_ones())
}

/// CACH TACT: correct a 7-bit word in place. `true` if it now reads clean.
pub fn hamming74_decode(word: &mut [u8; 7]) -> bool {
    // One-bit code: the three-bit syndrome names the bad position directly.
    // Syndrome (H-column, row 0 as MSB) → bit position. Syn 0 is clean.
    const CORR: [i8; 8] = [-1, 6, 5, 3, 4, 0, 2, 1];
    let syn = syndrome(word, &HAMMING_7_4_H) as usize;
    if syn == 0 {
        return true;
    }
    let pos = CORR[syn];
    if pos < 0 {
        return false;
    }
    word[pos as usize] ^= 1;
    true
}

#[allow(dead_code)]
pub fn hamming74_encode(data: &[u8; 4]) -> [u8; 7] {
    encode(data, &HAMMING_7_4_G)
}

/// EMB: correct a 16-bit word in place. `true` if within two errors.
pub fn qr1676_decode(word: &mut [u8; 16]) -> bool {
    use std::sync::OnceLock;
    static TABLE: OnceLock<Vec<u64>> = OnceLock::new();
    let table = TABLE.get_or_init(|| syndrome_table(&QR_16_7_6_H));
    decode_with_table(word, &QR_16_7_6_H, table).is_some()
}

/// Correct one error in an embedded-LC row, returning the correction count.
pub fn hamming16114_decode(word: &mut [u8; 16]) -> Option<u32> {
    let syn = syndrome(word, &HAMMING_16_11_4_H);
    if syn == 0 {
        return Some(0);
    }
    for position in 0..16 {
        let mut probe = [0u8; 16];
        probe[position] = 1;
        if syndrome(&probe, &HAMMING_16_11_4_H) == syn {
            word[position] ^= 1;
            return Some(1);
        }
    }
    None
}

#[allow(dead_code)]
pub fn qr1676_encode(data: &[u8; 7]) -> [u8; 16] {
    encode(data, &QR_16_7_6_G)
}

/// Slot Type: correct a 20-bit word in place. `true` if within three errors.
pub fn golay208_decode(word: &mut [u8; 20]) -> bool {
    use std::sync::OnceLock;
    static TABLE: OnceLock<Vec<u64>> = OnceLock::new();
    static TABLE3: OnceLock<Vec<u64>> = OnceLock::new();
    let table = TABLE.get_or_init(|| syndrome_table(&GOLAY_20_8_H));
    // Golay(20,8) has distance 8, so three errors remain correctable; the
    // base table covers two. Register the three-bit patterns lazily too.
    let table3 = TABLE3.get_or_init(|| {
        let mut t = table.clone();
        let register = |mask: u64, t: &mut Vec<u64>| {
            let mut word = [0u8; 20];
            for k in 0..20 {
                if mask >> k & 1 != 0 {
                    word[k] = 1;
                }
            }
            t[syndrome(&word, &GOLAY_20_8_H) as usize] = mask;
        };
        for i in 0..20 {
            for j in i + 1..20 {
                for k in j + 1..20 {
                    register((1u64 << i) | (1u64 << j) | (1u64 << k), &mut t);
                }
            }
        }
        t
    });
    decode_with_table(word, &GOLAY_20_8_H, table3).is_some()
}

#[allow(dead_code)]
pub fn golay208_encode(data: &[u8; 8]) -> [u8; 20] {
    encode(data, &GOLAY_20_8_G)
}

/// The decoded EMB payload: colour code, privacy indicator, LCSS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Emb {
    pub color_code: u8,
    pub pi: bool,
    pub lcss: u8,
}

/// Decode a 16-bit EMB field; `None` when damaged beyond correction.
pub fn emb_decode(bits: [u8; 16]) -> Option<Emb> {
    let mut word = bits;
    if !qr1676_decode(&mut word) {
        return None;
    }
    Some(Emb {
        color_code: (word[0] << 3) | (word[1] << 2) | (word[2] << 1) | word[3],
        pi: word[4] != 0,
        lcss: (word[5] << 1) | word[6],
    })
}

#[allow(dead_code)]
pub fn emb_encode(cc: u8, pi: bool, lcss: u8) -> [u8; 16] {
    qr1676_encode(&[
        (cc >> 3) & 1,
        (cc >> 2) & 1,
        (cc >> 1) & 1,
        cc & 1,
        u8::from(pi),
        (lcss >> 1) & 1,
        lcss & 1,
    ])
}

/// The decoded Slot Type payload: colour code and data type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotType {
    pub color_code: u8,
    /// ETSI TS 102 361-1 table 10.24: 0 PI, 1 voice LC header, 2 terminator
    /// with LC, 3 CSBK, 6 data header, 9 idle, ...
    pub data_type: u8,
}

/// Decode the 20 Slot Type bits, split five dibits either side of a data
/// burst's centre field. `None` when damaged beyond correction.
pub fn slot_type_decode(bits: [u8; 20]) -> Option<SlotType> {
    let mut word = bits;
    if !golay208_decode(&mut word) {
        return None;
    }
    let mut value = 0u8;
    for &b in &word[..8] {
        value = (value << 1) | b;
    }
    Some(SlotType {
        color_code: value >> 4,
        data_type: value & 0x0f,
    })
}

#[allow(dead_code)]
pub fn slot_type_encode(cc: u8, data_type: u8) -> [u8; 20] {
    let value = ((cc & 0x0f) << 4) | (data_type & 0x0f);
    let mut data = [0u8; 8];
    for i in 0..8 {
        data[i] = (value >> (7 - i)) & 1;
    }
    golay208_encode(&data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every codeword must sit in the parity-check matrix's null space; this
    /// is what catches a mis-transcribed matrix row.
    #[test]
    fn generator_and_parity_check_matrices_agree() {
        for data in 0..16u8 {
            let d = [(data >> 3) & 1, (data >> 2) & 1, (data >> 1) & 1, data & 1];
            assert_eq!(syndrome(&hamming74_encode(&d), &HAMMING_7_4_H), 0);
        }
        for data in 0..128u8 {
            let mut d = [0u8; 7];
            for i in 0..7 {
                d[i] = (data >> (6 - i)) & 1;
            }
            assert_eq!(syndrome(&qr1676_encode(&d), &QR_16_7_6_H), 0);
        }
        for data in 0..=255u8 {
            let mut d = [0u8; 8];
            for i in 0..8 {
                d[i] = (data >> (7 - i)) & 1;
            }
            assert_eq!(syndrome(&golay208_encode(&d), &GOLAY_20_8_H), 0);
        }
    }

    #[test]
    fn hamming74_corrects_one_bit() {
        for data in 0..16u8 {
            let d = [(data >> 3) & 1, (data >> 2) & 1, (data >> 1) & 1, data & 1];
            for flip in 0..7 {
                let mut w = hamming74_encode(&d);
                w[flip] ^= 1;
                assert!(hamming74_decode(&mut w));
                assert_eq!(&w[..4], &d[..]);
            }
        }
    }

    #[test]
    fn qr1676_corrects_two_bits() {
        for data in 0..128u8 {
            let mut d = [0u8; 7];
            for i in 0..7 {
                d[i] = (data >> (6 - i)) & 1;
            }
            let clean = qr1676_encode(&d);
            for a in 0..16 {
                for b in a..16 {
                    let mut w = clean;
                    w[a] ^= 1;
                    w[b] ^= 1;
                    assert!(qr1676_decode(&mut w), "data {data} flips {a},{b}");
                    assert_eq!(&w[..7], &d[..]);
                }
            }
        }
    }

    #[test]
    fn golay208_corrects_three_bits() {
        for data in [0u8, 0x11, 0x5a, 0xff] {
            let mut d = [0u8; 8];
            for i in 0..8 {
                d[i] = (data >> (7 - i)) & 1;
            }
            let clean = golay208_encode(&d);
            for a in 0..20 {
                for b in a + 1..20 {
                    for c in b + 1..20 {
                        let mut w = clean;
                        w[a] ^= 1;
                        w[b] ^= 1;
                        w[c] ^= 1;
                        assert!(golay208_decode(&mut w), "data {data:#x} flips {a},{b},{c}");
                        assert_eq!(&w[..8], &d[..]);
                    }
                }
            }
        }
    }

    #[test]
    fn emb_round_trips_colour_code() {
        for cc in 0..16u8 {
            let emb = emb_decode(emb_encode(cc, false, 0b01)).unwrap();
            assert_eq!(emb.color_code, cc);
            assert_eq!(emb.lcss, 0b01);
        }
    }

    #[test]
    fn slot_type_round_trips() {
        for cc in 0..16u8 {
            for dt in [0u8, 1, 2, 3, 6, 9] {
                let st = slot_type_decode(slot_type_encode(cc, dt)).unwrap();
                assert_eq!(st.color_code, cc);
                assert_eq!(st.data_type, dt);
            }
        }
    }

    #[test]
    fn garbage_is_rejected() {
        // A random word is very unlikely to land within two errors of a QR
        // codeword (136 of 65536 words are decodable).
        let mut w = [1u8, 1, 0, 1, 0, 1, 1, 1, 0, 0, 1, 0, 1, 0, 0, 1];
        assert!(!qr1676_decode(&mut w));
    }
}
