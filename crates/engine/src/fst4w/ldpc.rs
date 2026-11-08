//! The (240, 74) LDPC code of FST4W and its CRC-24: encoding, belief
//! propagation, and an ordered-statistics decoder for what propagation
//! leaves. Follows `decode240_74.f90` and `fastosd240_74.f90` by Joe Taylor
//! K1JT and Steven Franke K9AN.

use super::tables::{GENERATOR, MN, NM, NRW};
use std::sync::OnceLock;

pub const N: usize = 240;
pub const K: usize = 74;
pub const M: usize = N - K;
/// Message bits before the CRC.
pub const PAYLOAD_BITS: usize = 50;
/// The information size the ordered-statistics decoder works at: the 50
/// message bits and the first 16 CRC bits. The last 8 CRC bits are folded
/// into the code (they follow from the message), which adds distance; the
/// first 16 stay free and are what catches a wrong codeword.
const KEFF: usize = 66;
const CRC_POLY: [u8; 25] = [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 1, 0, 1, 1, 0, 1, 1];
const MAX_ITERS: usize = 30;
/// Saved propagation states the ordered-statistics decoder is run on.
const OSD_PASSES: usize = 2;
/// Reliable bits swapped for parity bits to make the second basis.
const NSWAP: usize = 20;
/// Parity bits an order-3 pattern may miss in the 32-bit window it is
/// prefiltered on (bits `KEFF..KEFF + 32` of the reordered code), as in the
/// source.
const MAX_WINDOW_ERRORS: u32 = 11;

/// The CRC remainder over `bits` (message and CRC field): zero when a
/// received message and CRC agree.
pub fn crc24(bits: &[u8]) -> u32 {
    let mut r = [0u8; 25];
    r.copy_from_slice(&bits[..25]);
    for i in 0..=bits.len() - 25 {
        r[24] = bits[i + 24];
        let top = r[0];
        for j in 0..25 {
            r[j] = (r[j] + top * CRC_POLY[j]) % 2;
        }
        r.rotate_left(1);
    }
    r[..24].iter().fold(0, |a, &b| a << 1 | b as u32)
}

/// The 74 message bits: the 50 payload bits (first bit of the message
/// first, taken from the top of the low 50 bits of `payload`) and their CRC.
pub fn with_crc(payload: u64) -> [u8; K] {
    let mut m = [0u8; K];
    for (i, b) in m[..PAYLOAD_BITS].iter_mut().enumerate() {
        *b = (payload >> (PAYLOAD_BITS - 1 - i) & 1) as u8;
    }
    let crc = crc24(&m);
    for (i, b) in m[PAYLOAD_BITS..].iter_mut().enumerate() {
        *b = (crc >> (23 - i) & 1) as u8;
    }
    m
}

/// The payload from 74 decoded bits, if their CRC checks.
pub fn payload(m: &[u8]) -> Option<u64> {
    (crc24(&m[..K]) == 0).then(|| m[..PAYLOAD_BITS].iter().fold(0u64, |a, &b| a << 1 | b as u64))
}

/// The codeword of 74 message bits: the message, then 166 parity bits.
pub fn encode(m: &[u8; K]) -> [u8; N] {
    let v = m.iter().enumerate().fold(0u128, |a, (i, &b)| a | (b as u128) << i);
    let mut c = [0u8; N];
    c[..K].copy_from_slice(m);
    for (i, g) in GENERATOR.iter().enumerate() {
        c[K + i] = ((g & v).count_ones() & 1) as u8;
    }
    c
}

/// Parity checks a word fails.
pub fn syndrome_weight(c: &[u8; N]) -> usize {
    (0..M).filter(|&j| NM[j][..NRW[j] as usize].iter().map(|&b| c[b as usize - 1] as usize).sum::<usize>() % 2 == 1).count()
}

type Bits = [u64; 4];

#[inline]
fn set(b: &mut Bits, i: usize) {
    b[i >> 6] |= 1 << (i & 63);
}

#[inline]
fn get(b: &Bits, i: usize) -> bool {
    b[i >> 6] >> (i & 63) & 1 == 1
}

#[inline]
fn xor(a: &mut Bits, b: &Bits) {
    for i in 0..4 {
        a[i] ^= b[i];
    }
}

/// The generator at `KEFF` as bit rows: 50 message bits with the last 8 CRC
/// bits they imply, and 16 free CRC bits.
fn osd_generator() -> &'static Vec<[u8; N]> {
    static G: OnceLock<Vec<[u8; N]>> = OnceLock::new();
    G.get_or_init(|| {
        (0..KEFF)
            .map(|i| {
                let mut m = [0u8; K];
                m[i] = 1;
                if i < PAYLOAD_BITS {
                    let crc = crc24(&m);
                    for (j, b) in m[PAYLOAD_BITS..].iter_mut().enumerate() {
                        *b = (crc >> (23 - j) & 1) as u8;
                    }
                    m[PAYLOAD_BITS..KEFF].fill(0);
                }
                encode(&m)
            })
            .collect()
    })
}

/// A word that passed the CRC.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decoded {
    pub message: [u8; K],
    pub codeword: [u8; N],
    /// Bits of the codeword that disagree with the sign of the input.
    pub hard_errors: usize,
    /// True when belief propagation finished alone.
    pub bp: bool,
}

/// Belief propagation and then the ordered-statistics decoder. `llr` is
/// positive for a one. Only a word whose full 24-bit CRC checks is returned.
pub fn decode(llr: &[f32; N]) -> Option<Decoded> {
    let mut save = [[0f32; N]; OSD_PASSES];
    if let Some(d) = bp(llr, &mut save) {
        return Some(d);
    }
    save.iter().find_map(|z| osd(z, llr))
}

fn finish(cw: [u8; N], llr: &[f32; N], bp: bool) -> Option<Decoded> {
    let message: [u8; K] = cw[..K].try_into().expect("74 of 240");
    if crc24(&message) != 0 || cw.iter().all(|&b| b == 0) {
        return None;
    }
    let hard_errors = (0..N).filter(|&i| (cw[i] == 1) != (llr[i] >= 0.0)).count();
    Some(Decoded { message, codeword: cw, hard_errors, bp })
}

fn bp(llr: &[f32; N], save: &mut [[f32; N]; OSD_PASSES]) -> Option<Decoded> {
    let mut tov = [[0f32; 3]; N];
    let mut toc = [[0f32; 5]; M];
    for j in 0..M {
        for i in 0..NRW[j] as usize {
            toc[j][i] = llr[NM[j][i] as usize - 1];
        }
    }
    let mut zsum = [0f32; N];
    let (mut ncnt, mut nclast) = (0usize, 0usize);
    let mut zn = [0f32; N];
    let mut cw = [0u8; N];
    for iter in 0..=MAX_ITERS {
        for i in 0..N {
            zn[i] = llr[i] + tov[i].iter().sum::<f32>();
            zsum[i] += zn[i];
            cw[i] = (zn[i] > 0.0) as u8;
        }
        if iter > 0 && iter <= OSD_PASSES {
            save[iter - 1] = zsum;
        }
        let ncheck = syndrome_weight(&cw);
        if ncheck == 0 {
            if let Some(d) = finish(cw, llr, true) {
                return Some(d);
            }
        }
        if iter > 0 {
            if ncheck < nclast {
                ncnt = 0;
            } else {
                ncnt += 1;
            }
            if ncnt >= 5 && iter >= 10 && ncheck > 15 {
                break;
            }
        }
        nclast = ncheck;
        for j in 0..M {
            for i in 0..NRW[j] as usize {
                let ibj = NM[j][i] as usize - 1;
                let mut v = zn[ibj];
                for kk in 0..3 {
                    if MN[ibj][kk] as usize == j + 1 {
                        v -= tov[ibj][kk];
                    }
                }
                toc[j][i] = v;
            }
        }
        let mut tanhtoc = [[0f32; 5]; M];
        for j in 0..M {
            for i in 0..NRW[j] as usize {
                tanhtoc[j][i] = (-toc[j][i] / 2.0).tanh();
            }
        }
        for bit in 0..N {
            for kk in 0..3 {
                let chk = MN[bit][kk] as usize - 1;
                let mut t = 1f32;
                for i in 0..NRW[chk] as usize {
                    if NM[chk][i] as usize != bit + 1 {
                        t *= tanhtoc[chk][i];
                    }
                }
                tov[bit][kk] = 2.0 * (-t).clamp(-0.999_999, 0.999_999).atanh();
            }
        }
    }
    None
}

/// One ordered-statistics pass over two reliability orderings of `z`; `llr`
/// is the channel input, for the hard-error count. Exhaustive to order 3,
/// the third order behind a partial-syndrome prefilter.
fn osd(z: &[f32; N], llr: &[f32; N]) -> Option<Decoded> {
    let gen_rows = osd_generator();
    let mut order: Vec<usize> = (0..N).collect();
    order.sort_by(|&a, &b| z[b].abs().total_cmp(&z[a].abs()));
    for basis in 0..2 {
        let mut perm = order.clone();
        if basis == 1 {
            for i in KEFF - NSWAP..KEFF {
                perm.swap(i, i + NSWAP);
            }
        }
        let (rows, indices) = reduce(gen_rows, &perm);
        let mut hd: Bits = [0; 4];
        let mut mag = [0f32; N];
        for (j, &p) in indices.iter().enumerate() {
            if z[p] >= 0.0 {
                set(&mut hd, j);
            }
            mag[j] = z[p].abs();
        }
        let mut c0: Bits = [0; 4];
        for (i, r) in rows.iter().enumerate() {
            if get(&hd, i) {
                xor(&mut c0, r);
            }
        }
        let dist = |c: &Bits| -> f32 {
            let mut d = 0f32;
            for w in 0..4 {
                let mut e = c[w] ^ hd[w];
                while e != 0 {
                    d += mag[w * 64 + e.trailing_zeros() as usize];
                    e &= e - 1;
                }
            }
            d
        };
        let mut best = c0;
        let mut dmin = dist(&c0);
        let mut consider = |c: &Bits| {
            let d = dist(c);
            if d < dmin {
                dmin = d;
                best = *c;
            }
        };
        // Bits KEFF..KEFF + 32 lie within word 1 for KEFF = 66.
        let window_of = |w: u64| ((w ^ hd[1]) >> (KEFF - 64) & 0xffff_ffff).count_ones();
        for a in 0..KEFF {
            let mut c1 = c0;
            xor(&mut c1, &rows[a]);
            consider(&c1);
            for b in a + 1..KEFF {
                let mut c2 = c1;
                xor(&mut c2, &rows[b]);
                consider(&c2);
                for r in &rows[b + 1..KEFF] {
                    if window_of(c2[1] ^ r[1]) <= MAX_WINDOW_ERRORS {
                        let mut c3 = c2;
                        xor(&mut c3, r);
                        consider(&c3);
                    }
                }
            }
        }
        let mut cw = [0u8; N];
        for (j, &p) in indices.iter().enumerate() {
            cw[p] = get(&best, j) as u8;
        }
        if let Some(d) = finish(cw, llr, false) {
            return Some(d);
        }
    }
    None
}

/// Bring the generator to systematic form on the columns named by `perm`,
/// most reliable first. Rows come back with columns in decoding order; the
/// second value says which code bit each decoding-order column is.
fn reduce(rows_in: &[[u8; N]], perm: &[usize]) -> (Vec<Bits>, Vec<usize>) {
    let mut rows: Vec<Bits> = rows_in
        .iter()
        .map(|g| {
            let mut r: Bits = [0; 4];
            for (j, &p) in perm.iter().enumerate() {
                if g[p] == 1 {
                    set(&mut r, j);
                }
            }
            r
        })
        .collect();
    let (mut pivots, mut skipped) = (Vec::with_capacity(KEFF), Vec::new());
    let mut col = 0usize;
    for id in 0..KEFF {
        loop {
            match (id..KEFF).find(|&j| get(&rows[j], col)) {
                Some(j) => {
                    rows.swap(id, j);
                    break;
                }
                None => {
                    skipped.push(col);
                    col += 1;
                }
            }
        }
        pivots.push(col);
        let pivot = rows[id];
        for (j, r) in rows.iter_mut().enumerate() {
            if j != id && get(r, col) {
                xor(r, &pivot);
            }
        }
        col += 1;
    }
    // Final order: pivot columns, then the skipped ones, then the rest.
    let mut order = pivots;
    order.extend(skipped);
    order.extend(col..N);
    let indices: Vec<usize> = order.iter().map(|&c| perm[c]).collect();
    let rows = rows
        .iter()
        .map(|r| {
            let mut out: Bits = [0; 4];
            for (j, &c) in order.iter().enumerate() {
                if get(r, c) {
                    set(&mut out, j);
                }
            }
            out
        })
        .collect();
    (rows, indices)
}
