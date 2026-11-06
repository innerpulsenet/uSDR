//! DRM channel coding (ES 201 980 clause 7): the CRCs of annex D, energy
//! dispersal, the bit and cell interleavers' permutation, the punctured
//! rate 1/4 convolutional code and its Viterbi decoder, the QAM mappings and
//! the multilevel coding that ties them together.
//!
//! Multilevel coding gives each bit position of a QAM point its own code.
//! The mapping partitions the points of each axis: the first bit picks one
//! of two interleaved sets whose points sit next to each other's (the most
//! error-prone decision), the last only a sign within sets already far
//! apart, so the first gets the strongest code and the last the weakest
//! (with 64-QAM at protection level 0, rates 1/4, 1/2 and 3/4). The receiver
//! decodes level by level: soft bits for level 0 from the equalized cells
//! with nothing else known, a Viterbi decode, the result encoded again to
//! give the transmitted level-0 bits exactly, and those used to narrow the
//! constellation for level 1, and so on. Each soft bit is a max-log
//! likelihood ratio weighted by the cell's channel power, so cells in a
//! fade count for little.

use super::tables::{self, Rate};
use num_complex::Complex32;

// ---------------------------------------------------------------------------
// CRC (annex D): MSB first, register preset to ones, result inverted.

fn crc_bits(bits: impl Iterator<Item = u8>, degree: u32, poly_low: u32) -> u32 {
    let mask = (1u32 << degree) - 1;
    let mut reg = mask;
    for b in bits {
        let fb = ((reg >> (degree - 1)) & 1) ^ (b as u32 & 1);
        reg = (reg << 1) & mask;
        if fb == 1 {
            reg ^= poly_low;
        }
    }
    !reg & mask
}

/// G8(x) = x^8 + x^4 + x^3 + x^2 + 1, over whole bytes.
pub fn crc8(bytes: &[u8]) -> u8 {
    crc_bits(bytes_to_bits(bytes), 8, 0x1D) as u8
}

/// G8 over the first `nbits` bits of `bytes` (MSB first). The AAC CRC of
/// DRM covers a part of each frame that ends at a bit, not a byte; this is
/// for the AAC decoder, which knows where.
pub fn crc8_bits(bytes: &[u8], nbits: usize) -> u8 {
    crc_bits(bytes_to_bits(bytes).take(nbits), 8, 0x1D) as u8
}

/// G16(x) = x^16 + x^12 + x^5 + 1, over whole bytes.
pub fn crc16(bytes: &[u8]) -> u16 {
    crc_bits(bytes_to_bits(bytes), 16, 0x1021) as u16
}

fn bytes_to_bits(bytes: &[u8]) -> impl Iterator<Item = u8> + '_ {
    bytes.iter().flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1))
}

/// Pack bits (one per byte, MSB first) into bytes; a short last byte is
/// padded with zeros.
pub fn pack(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |a, (i, &b)| a | ((b & 1) << (7 - i)))).collect()
}

pub fn unpack(bytes: &[u8]) -> Vec<u8> {
    bytes_to_bits(bytes).collect()
}

/// A big-endian bit reader over unpacked bits.
pub struct Bits<'a> {
    bits: &'a [u8],
    pub pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(bits: &'a [u8]) -> Self {
        Bits { bits, pos: 0 }
    }

    pub fn left(&self) -> usize {
        self.bits.len().saturating_sub(self.pos)
    }

    /// Up to 32 bits; past the end reads zeros (callers check `left`).
    pub fn take(&mut self, n: usize) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bits.get(self.pos).copied().unwrap_or(0) as u32;
            self.pos += 1;
        }
        v
    }

    pub fn skip(&mut self, n: usize) {
        self.pos += n;
    }
}

// ---------------------------------------------------------------------------
// Energy dispersal (clause 7.2.2): the PRBS of X^9 + X^5 + 1 from all ones.

pub fn prbs(n: usize) -> Vec<u8> {
    let mut reg: u32 = 0x1FF;
    (0..n)
        .map(|_| {
            let b = ((reg >> 4) ^ (reg >> 8)) & 1;
            reg = ((reg << 1) | b) & 0x1FF;
            b as u8
        })
        .collect()
}

pub fn disperse(bits: &mut [u8]) {
    let p = prbs(bits.len());
    for (b, p) in bits.iter_mut().zip(p) {
        *b ^= p;
    }
}

// ---------------------------------------------------------------------------
// The interleavers' permutation (clauses 7.3.3 and 7.6): y_i = v_Pi(i).

pub fn permutation(xin: usize, t0: usize) -> Vec<u32> {
    if xin == 0 {
        return Vec::new();
    }
    // s = 2^ceil(log2 xin), as the standard writes it. (Dream takes the
    // next power of two above xin when xin is one itself; the two differ
    // only for block sizes that are exact powers of two.)
    let s = xin.next_power_of_two();
    let q = s / 4 - 1;
    let mut p = vec![0u32; xin];
    let mut prev = 0usize;
    for item in p.iter_mut().skip(1) {
        let mut v = (t0 * prev + q) % s;
        while v >= xin {
            v = (t0 * v + q) % s;
        }
        *item = v as u32;
        prev = v;
    }
    p
}

// ---------------------------------------------------------------------------
// The convolutional code (clause 7.3.2): constraint length 7, generators
// 133, 171, 145, 133 (octal). Bit j of the 7-bit register is a_{i-j}.

const GEN: [u32; 4] = [0b110_1101, 0b100_1111, 0b101_0011, 0b110_1101];

fn out_word(reg: u32) -> u8 {
    let mut w = 0u8;
    for (j, g) in GEN.iter().enumerate() {
        w |= (((reg & g).count_ones() & 1) as u8) << j;
    }
    w
}

/// How the tail of a code word is punctured.
#[derive(Clone, Copy, Debug)]
pub enum Tail {
    /// Table 28, by index r_p (MSC and SDC).
    Pattern(usize),
    /// The FAC carries on with the code rate's own pattern.
    Continue,
}

/// The puncturing mask of every encoder step: `m1` bits at rate `a`, `m2`
/// bits at rate `b`, then the six tail steps.
pub fn schedule(m1: usize, a: Rate, m2: usize, b: Rate, tail: Tail) -> Vec<u8> {
    let mut s = Vec::with_capacity(m1 + m2 + 6);
    for i in 0..m1 {
        s.push(a.cols[i % a.rx]);
    }
    for i in 0..m2 {
        s.push(b.cols[i % b.rx]);
    }
    for i in 0..6 {
        s.push(match tail {
            Tail::Pattern(rp) => tables::TAIL[rp][i],
            Tail::Continue => b.cols[(m2 + i) % b.rx],
        });
    }
    s
}

/// Encode `bits` (one per byte) with the puncturing `sched`, which has six
/// more steps than there are bits (the tail).
pub fn encode(bits: &[u8], sched: &[u8]) -> Vec<u8> {
    debug_assert_eq!(bits.len() + 6, sched.len());
    let mut out = Vec::with_capacity(sched.iter().map(|m| m.count_ones() as usize).sum());
    let mut reg = 0u32;
    for (i, &mask) in sched.iter().enumerate() {
        let a = bits.get(i).copied().unwrap_or(0) as u32 & 1;
        reg = ((reg << 1) | a) & 0x7F;
        let w = out_word(reg);
        for j in 0..4 {
            if mask & (1 << j) != 0 {
                out.push((w >> j) & 1);
            }
        }
    }
    out
}

/// Soft decision Viterbi decoding of the punctured code. Soft values are
/// positive for a 0, negative for a 1, zero where nothing is known.
pub struct Viterbi {
    words: [u8; 128],
    decisions: Vec<u64>,
}

impl Default for Viterbi {
    fn default() -> Self {
        Self::new()
    }
}

impl Viterbi {
    pub fn new() -> Self {
        let mut words = [0u8; 128];
        for (r, w) in words.iter_mut().enumerate() {
            *w = out_word(r as u32);
        }
        Viterbi { words, decisions: Vec::new() }
    }

    /// Decode; returns the information bits (the tail dropped).
    pub fn decode(&mut self, soft: &[f32], sched: &[u8]) -> Vec<u8> {
        let steps = sched.len();
        self.decisions.clear();
        self.decisions.resize(steps, 0);
        let mut pm = [f32::NEG_INFINITY; 64];
        pm[0] = 0.0;
        let mut next = [0f32; 64];
        let mut pos = 0;
        for (t, &mask) in sched.iter().enumerate() {
            let mut lam = [0f32; 4];
            for (j, l) in lam.iter_mut().enumerate() {
                if mask & (1 << j) != 0 {
                    *l = soft.get(pos).copied().unwrap_or(0.0);
                    pos += 1;
                }
            }
            // Branch metric for each of the 16 output words.
            let mut bm = [0f32; 16];
            for (w, m) in bm.iter_mut().enumerate() {
                let mut v = 0.0;
                for (j, l) in lam.iter().enumerate() {
                    v += if w & (1 << j) != 0 { -l } else { *l };
                }
                *m = v;
            }
            let mut dec = 0u64;
            let mut best = f32::NEG_INFINITY;
            for (ns, nx) in next.iter_mut().enumerate() {
                let a = ns & 1;
                let p0 = ns >> 1;
                let p1 = p0 | 32;
                let m0 = pm[p0] + bm[self.words[(p0 << 1) | a] as usize];
                let m1 = pm[p1] + bm[self.words[(p1 << 1) | a] as usize];
                if m1 > m0 {
                    *nx = m1;
                    dec |= 1 << ns;
                } else {
                    *nx = m0;
                }
                best = best.max(*nx);
            }
            self.decisions[t] = dec;
            for (p, n) in pm.iter_mut().zip(next.iter()) {
                *p = n - best;
            }
        }
        // The tail brings the encoder back to state 0.
        let mut state = 0usize;
        let mut bits = vec![0u8; steps];
        for t in (0..steps).rev() {
            bits[t] = (state & 1) as u8;
            let d = ((self.decisions[t] >> state) & 1) as usize;
            state = (state >> 1) | (d << 5);
        }
        bits.truncate(steps - 6);
        bits
    }
}

// ---------------------------------------------------------------------------
// QAM (clause 7.4). Each axis is a PAM whose point is picked by one bit of
// each level, level 0 the most significant bit of the index.

const PAM2: [f32; 2] = [1.0, -1.0];
const PAM4: [f32; 4] = [3.0, -1.0, 1.0, -3.0];
const PAM8: [f32; 8] = [7.0, -1.0, 3.0, -5.0, 5.0, -3.0, 1.0, -7.0];

/// The axis amplitudes (normalized) for a constellation of `levels` levels.
pub fn pam(levels: usize) -> (&'static [f32], f32) {
    match levels {
        1 => (&PAM2, std::f32::consts::FRAC_1_SQRT_2),
        2 => (&PAM4, 1.0 / 10f32.sqrt()),
        _ => (&PAM8, 1.0 / 42f32.sqrt()),
    }
}

/// The nearest constellation point to `z`.
pub fn slice(z: Complex32, levels: usize) -> Complex32 {
    let (pts, a) = pam(levels);
    let near = |v: f32| {
        pts.iter().map(|p| p * a).fold(f32::NAN, |b, p| if b.is_nan() || (v - p).abs() < (v - b).abs() { p } else { b })
    };
    Complex32::new(near(z.re), near(z.im))
}

// ---------------------------------------------------------------------------
// Multilevel coding (clause 7.3.1).

/// The coding of one block (a multiplex frame, an SDC block or an FAC
/// block): its levels, cells, bits per level and part, puncturing and
/// interleaving.
#[derive(Clone)]
pub struct Mlc {
    pub levels: usize,
    /// Cells of the higher (n1) and lower (n2) protected parts.
    pub n1: usize,
    pub n2: usize,
    /// Information bits of each level in part A and part B: M_p,1, M_p,2.
    pub m: Vec<[usize; 2]>,
    sched: Vec<Vec<u8>>,
    /// Bit interleaver permutations of each level, parts A and B.
    perm: Vec<Option<(Vec<u32>, Vec<u32>)>>,
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

impl Mlc {
    fn build(
        levels: usize,
        n1: usize,
        n2: usize,
        ra: &[Rate],
        rb: &[Rate],
        tail_continue: bool,
        m2_fixed: Option<usize>,
    ) -> Option<Mlc> {
        if 2 * n2 < 12 + 1 {
            return None;
        }
        let mut m = Vec::new();
        let mut sched = Vec::new();
        let mut perm = Vec::new();
        for p in 0..levels {
            let a = ra[p];
            let b = rb[p];
            let m1 = 2 * n1 * a.rx / a.ry;
            let (m2, tail) = match m2_fixed {
                Some(v) => (v, Tail::Continue),
                None => {
                    let x = 2 * n2 - 12;
                    let m2 = b.rx * (x / b.ry);
                    let rp = x - b.ry * (x / b.ry);
                    (m2, if tail_continue { Tail::Continue } else { Tail::Pattern(rp) })
                }
            };
            let s = schedule(m1, a, m2, b, tail);
            let coded: usize = s.iter().map(|w| w.count_ones() as usize).sum();
            if coded != 2 * (n1 + n2) {
                return None;
            }
            m.push([m1, m2]);
            sched.push(s);
            perm.push(tables::interleaver_t0(levels, p).map(|t0| (permutation(2 * n1, t0), permutation(2 * n2, t0))));
        }
        Some(Mlc { levels, n1, n2, m, sched, perm })
    }

    /// The FAC: 65 cells of 4-QAM at rate 3/5, 72 bits.
    pub fn fac() -> Mlc {
        Self::build(1, 0, tables::FAC_CELLS, &[tables::FAC_RATE], &[tables::FAC_RATE], true, Some(tables::FAC_BITS))
            .expect("FAC coding")
    }

    /// The SDC on `n_sdc` cells, 16-QAM or 4-QAM.
    pub fn sdc(n_sdc: usize, qam16: bool) -> Option<Mlc> {
        if qam16 {
            Self::build(2, 0, n_sdc, &tables::SDC16_RATES, &tables::SDC16_RATES, false, None)
        } else {
            Self::build(1, 0, n_sdc, &[tables::SDC4_RATE], &[tables::SDC4_RATE], false, None)
        }
    }

    /// The MSC: `n_mux` cells of 16-QAM (`levels` 2) or 64-QAM (3), with
    /// protection levels `pl_a`, `pl_b` and `len_a` bytes in part A.
    pub fn msc(n_mux: usize, levels: usize, pl_a: u8, pl_b: u8, len_a: usize) -> Option<Mlc> {
        let (ra, rb, lcm): (Vec<Rate>, Vec<Rate>, usize) = if levels == 2 {
            let (a, lcm) = tables::msc16_rates(pl_a);
            (a.to_vec(), tables::msc16_rates(pl_b).0.to_vec(), lcm)
        } else {
            let (a, lcm) = tables::msc64_rates(pl_a);
            (a.to_vec(), tables::msc64_rates(pl_b).0.to_vec(), lcm)
        };
        // N1 = ceil(8 X / (2 RYlcm sum R_p)) RYlcm, in whole numbers.
        let n1 = if len_a == 0 {
            0
        } else {
            let den = ra.iter().fold(1, |d, r| d / gcd(d, r.ry) * r.ry);
            let num: usize = ra.iter().map(|r| r.rx * den / r.ry).sum();
            let top = 8 * len_a * den;
            let bot = 2 * lcm * num;
            top.div_ceil(bot) * lcm
        };
        if n1 + 20 > n_mux {
            return None;
        }
        Self::build(levels, n1, n_mux - n1, &ra, &rb, false, None)
    }

    pub fn cells(&self) -> usize {
        self.n1 + self.n2
    }

    /// Bits of the higher protected part, L1.
    pub fn l1(&self) -> usize {
        self.m.iter().map(|m| m[0]).sum()
    }

    /// Bits of the lower protected part, L2.
    pub fn l2(&self) -> usize {
        self.m.iter().map(|m| m[1]).sum()
    }

    pub fn bits(&self) -> usize {
        self.l1() + self.l2()
    }

    /// The level-p information bits within the block's bit order (clause
    /// 7.3.1.1: part A of every level, then part B of every level).
    fn spans(&self, p: usize) -> [(usize, usize); 2] {
        let a: usize = self.m[..p].iter().map(|m| m[0]).sum();
        let b: usize = self.l1() + self.m[..p].iter().map(|m| m[1]).sum::<usize>();
        [(a, self.m[p][0]), (b, self.m[p][1])]
    }

    fn interleave(&self, p: usize, v: &[u8]) -> Vec<u8> {
        match &self.perm[p] {
            None => v.to_vec(),
            Some((pa, pb)) => {
                let k = 2 * self.n1;
                let mut y = Vec::with_capacity(v.len());
                y.extend(pa.iter().map(|&i| v[i as usize]));
                y.extend(pb.iter().map(|&i| v[k + i as usize]));
                y
            }
        }
    }

    fn deinterleave(&self, p: usize, y: &[f32]) -> Vec<f32> {
        match &self.perm[p] {
            None => y.to_vec(),
            Some((pa, pb)) => {
                let k = 2 * self.n1;
                let mut v = vec![0f32; y.len()];
                for (i, &j) in pa.iter().enumerate() {
                    v[j as usize] = y[i];
                }
                for (i, &j) in pb.iter().enumerate() {
                    v[k + j as usize] = y[k + i];
                }
                v
            }
        }
    }

    #[cfg(test)]
    fn level_bits(&self, p: usize, bits: &[u8]) -> Vec<u8> {
        let mut x = Vec::with_capacity(self.m[p][0] + self.m[p][1]);
        for (start, len) in self.spans(p) {
            x.extend_from_slice(&bits[start..start + len]);
        }
        x
    }

    /// Code, interleave and map `bits` (already dispersed) onto cells
    /// (the tests' transmitter).
    #[cfg(test)]
    pub fn encode(&self, bits: &[u8]) -> Vec<Complex32> {
        let n = self.cells();
        let (pts, a) = pam(self.levels);
        let mut idx = vec![0usize; 2 * n];
        for p in 0..self.levels {
            let v = encode(&self.level_bits(p, bits), &self.sched[p]);
            let y = self.interleave(p, &v);
            for (i, b) in y.iter().enumerate() {
                idx[i] = (idx[i] << 1) | *b as usize;
            }
        }
        (0..n).map(|i| Complex32::new(pts[idx[2 * i]] * a, pts[idx[2 * i + 1]] * a)).collect()
    }

    /// Decode equalized `cells` with channel weights `csi` (|H|^2 or any
    /// proportional weight). Returns the bits (still dispersed) and the
    /// share of the code bits whose hard decision disagreed with the
    /// decoded word, a measure of the channel's raw bit error rate.
    pub fn decode(&self, cells: &[Complex32], csi: &[f32], vit: &mut Viterbi) -> (Vec<u8>, f32) {
        let n = self.cells();
        let levels = self.levels;
        let (pts, a) = pam(levels);
        let axis: Vec<f32> = cells.iter().take(n).flat_map(|z| [z.re / a, z.im / a]).collect();
        let w: Vec<f32> = csi.iter().take(n).flat_map(|&w| [w, w]).collect();
        let mut known = vec![0usize; 2 * n];
        let mut out = vec![0u8; self.bits()];
        let mut errors = 0usize;
        let mut total = 0usize;
        for p in 0..levels {
            let shift = levels - 1 - p;
            let soft: Vec<f32> = axis
                .iter()
                .zip(w.iter())
                .zip(known.iter())
                .map(|((&r, &w), &k)| {
                    let mut d0 = f32::INFINITY;
                    let mut d1 = f32::INFINITY;
                    for (idx, &pt) in pts.iter().enumerate() {
                        if idx >> (shift + 1) != k {
                            continue;
                        }
                        let d = (r - pt) * (r - pt);
                        if (idx >> shift) & 1 == 0 {
                            d0 = d0.min(d);
                        } else {
                            d1 = d1.min(d);
                        }
                    }
                    w * (d1 - d0)
                })
                .collect();
            let v_soft = self.deinterleave(p, &soft);
            let x = vit.decode(&v_soft, &self.sched[p]);
            let v = encode(&x, &self.sched[p]);
            for (s, b) in v_soft.iter().zip(v.iter()) {
                if *s != 0.0 {
                    total += 1;
                    if (*s < 0.0) != (*b == 1) {
                        errors += 1;
                    }
                }
            }
            let y = self.interleave(p, &v);
            for (k, b) in known.iter_mut().zip(y.iter()) {
                *k = (*k << 1) | *b as usize;
            }
            let mut off = 0;
            for (start, len) in self.spans(p) {
                out[start..start + len].copy_from_slice(&x[off..off + len]);
                off += len;
            }
        }
        (out, if total > 0 { errors as f32 / total as f32 } else { 0.0 })
    }
}

/// The MSC cell interleaver (clause 7.6) seen from the receiver: cells of
/// each received multiplex frame are put back in place; with long
/// interleaving a cell i of frame n belongs to frame n - (i mod 5), so a
/// frame is complete four frames after its own.
pub struct CellDeinterleaver {
    perm: Vec<u32>,
    depth: usize,
    /// Ring of partially filled frames: values and weights.
    mem: Vec<(Vec<Complex32>, Vec<f32>)>,
    head: usize,
    fed: usize,
}

impl CellDeinterleaver {
    pub fn new(n_mux: usize, long: bool) -> Self {
        let depth = if long { 5 } else { 1 };
        CellDeinterleaver {
            perm: permutation(n_mux, 5),
            depth,
            mem: (0..depth).map(|_| (vec![Complex32::new(0.0, 0.0); n_mux], vec![0.0; n_mux])).collect(),
            head: 0,
            fed: 0,
        }
    }

    pub fn long(&self) -> bool {
        self.depth > 1
    }

    pub fn n_mux(&self) -> usize {
        self.perm.len()
    }

    /// Feed one received multiplex frame; returns the frame now complete,
    /// once enough have been fed for one to be.
    pub fn push(&mut self, cells: &[Complex32], csi: &[f32]) -> Option<(Vec<Complex32>, Vec<f32>)> {
        let d = self.depth;
        // This frame's slot last held the frame d before it, already given
        // out; clear it (weight 0: nothing known yet).
        let cur = (self.head + 1) % d;
        for w in self.mem[cur].1.iter_mut() {
            *w = 0.0;
        }
        for (i, (&z, &w)) in cells.iter().zip(csi.iter()).enumerate() {
            let slot = (cur + d - (i % d)) % d;
            let j = self.perm[i] as usize;
            self.mem[slot].0[j] = z;
            self.mem[slot].1[j] = w;
        }
        self.head = cur;
        self.fed += 1;
        // The frame d - 1 before this one has had its last cells now.
        if self.fed >= d { Some(self.mem[(cur + 1) % d].clone()) } else { None }
    }
}

/// The transmitter's cell interleaver, for the tests.
#[cfg(test)]
pub struct CellInterleaver {
    perm: Vec<u32>,
    depth: usize,
    hist: std::collections::VecDeque<Vec<Complex32>>,
}

#[cfg(test)]
impl CellInterleaver {
    /// `fill` stands in for the frames sent before the first: a
    /// transmitter that has been on the air for a while.
    pub fn new(n_mux: usize, long: bool, fill: impl Fn(usize) -> Complex32) -> Self {
        let depth = if long { 5 } else { 1 };
        let hist = (0..depth).map(|_| (0..n_mux).map(&fill).collect()).collect();
        CellInterleaver { perm: permutation(n_mux, 5), depth, hist }
    }

    pub fn push(&mut self, frame: Vec<Complex32>) -> Vec<Complex32> {
        self.hist.push_front(frame);
        self.hist.truncate(self.depth);
        (0..self.perm.len()).map(|i| self.hist[i % self.depth][self.perm[i] as usize]).collect()
    }
}
