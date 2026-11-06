//! Error control: the frame check sequences and the rate 1/2, constraint
//! length 7 convolutional code (generators 133 and 171 octal).

/// CRC-16 with the reflected CCITT polynomial, one table for both checks.
const fn crc_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u16;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0x8408
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC: [u16; 256] = crc_table();

fn crc16(data: &[u8], init: u16) -> u16 {
    data.iter().fold(init, |c, &b| {
        (c >> 8) ^ CRC[((c ^ b as u16) & 0xff) as usize]
    })
}

/// The HFDL frame check sequence over `data`: the HDLC (X.25) CRC, sent
/// least significant byte first.
pub fn fcs(data: &[u8]) -> u16 {
    crc16(data, 0xffff) ^ 0xffff
}

/// Whether the two bytes after the first `len` bytes of `buf` are their
/// frame check sequence.
pub fn fcs_ok(buf: &[u8], len: usize) -> bool {
    buf.len() >= len + 2 && fcs(&buf[..len]) == u16::from_le_bytes([buf[len], buf[len + 1]])
}

/// The ACARS block check over everything from the mode character to the
/// block check itself: zero when it is intact.
pub fn acars_residue(data: &[u8]) -> u16 {
    crc16(data, 0)
}

/// The generators with the newest bit in the least significant place:
/// 133 and 171 octal, bit-reversed. The 133 chip goes first.
const POLY_A: u32 = 0x6d;
const POLY_B: u32 = 0x4f;

fn parity(x: u32) -> u8 {
    (x.count_ones() & 1) as u8
}

#[cfg(test)]
pub fn conv_encode(bits: &[u8]) -> Vec<u8> {
    let mut sr = 0u32;
    let mut out = Vec::with_capacity(bits.len() * 2);
    for &b in bits {
        sr = ((sr << 1) | b as u32) & 0x7f;
        out.push(parity(sr & POLY_A));
        out.push(parity(sr & POLY_B));
    }
    out
}

/// Soft decision Viterbi decoder. `llr` holds two values per bit, positive
/// for a 0 chip; the result is the most likely input bits. The encoder
/// starts from the all-zero state; the end is left open, since the flush
/// bits only follow the user data and the frame check sequence already
/// says whether what matters came through.
pub fn viterbi(llr: &[f32]) -> Vec<u8> {
    let nbits = llr.len() / 2;
    // The two chips each 7-bit register (six bits of state and the new bit)
    // produces.
    let mut chips = [(0u8, 0u8); 128];
    for (reg, c) in chips.iter_mut().enumerate() {
        *c = (parity(reg as u32 & POLY_A), parity(reg as u32 & POLY_B));
    }
    let mut metric = [f32::NEG_INFINITY; 64];
    metric[0] = 0.0;
    let mut next = [0f32; 64];
    let mut decisions: Vec<u64> = Vec::with_capacity(nbits);
    for t in 0..nbits {
        let (l0, l1) = (llr[2 * t], llr[2 * t + 1]);
        let mut word = 0u64;
        for ns in 0..64usize {
            // Predecessors: the new state drops the old state's top bit,
            // which is the register's bit 6.
            let s0 = ns >> 1;
            let s1 = s0 | 32;
            let (a0, b0) = chips[ns];
            let (a1, b1) = chips[ns | 64];
            let bm0 = if a0 == 0 { l0 } else { -l0 } + if b0 == 0 { l1 } else { -l1 };
            let bm1 = if a1 == 0 { l0 } else { -l0 } + if b1 == 0 { l1 } else { -l1 };
            let m0 = metric[s0] + bm0;
            let m1 = metric[s1] + bm1;
            if m1 > m0 {
                next[ns] = m1;
                word |= 1 << ns;
            } else {
                next[ns] = m0;
            }
        }
        decisions.push(word);
        // Keep the metrics near zero.
        let top = next.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        for (m, n) in metric.iter_mut().zip(next.iter()) {
            *m = n - top;
        }
    }
    let mut state = metric
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let mut bits = vec![0u8; nbits];
    for t in (0..nbits).rev() {
        bits[t] = (state & 1) as u8;
        let high = (decisions[t] >> state) & 1;
        state = (state >> 1) | ((high as usize) << 5);
    }
    bits
}

/// Bits into bytes, the first bit in each byte's least significant place
/// (fields go least significant bit first, section 5.2.2).
pub fn pack_lsb_first(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| {
            c.iter()
                .enumerate()
                .fold(0u8, |acc, (i, &b)| acc | ((b & 1) << i))
        })
        .collect()
}

#[cfg(test)]
pub fn unpack_lsb_first(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .flat_map(|&b| (0..8).map(move |i| (b >> i) & 1))
        .collect()
}
