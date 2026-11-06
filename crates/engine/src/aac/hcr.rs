//! Huffman codeword reordering (ISO/IEC 14496-3 4.6.5.3), the
//! error-resilient spectral data layout DRM uses.
//!
//! The spectral codewords of a channel (each one Huffman word with its sign
//! bits and escapes) are first put in priority order: by codebook class
//! (codebook 11 and its virtual codebooks first, the small books last), then
//! by band, by four-line unit and by window group. The reordered data is cut
//! into segments, one per codeword from the start of that order for as long
//! as a segment fits, each as wide as its codeword can be (capped by the
//! longest codeword the frame announces); those priority codewords sit at the
//! start of their segments, so a bit error can only spread inside one. The
//! codewords left over fill the segments' remaining space in sets of one per
//! segment: in each set every codeword tries the segments in turn, starting
//! at its own, taking what space it finds and continuing in the next segment
//! if it does not fit. Odd sets fill segments from their far end backwards,
//! even sets from the near end forwards.
//!
//! A codeword split across segments is complete exactly when its last bits
//! arrive, because a proper prefix of a codeword never decodes (the Huffman
//! codes are prefix-free and the sign and escape bits that follow are counted
//! by it), which is what lets the decoder retrace the encoder's placement.

use super::bits::BitReader;
use super::ics::{is_spectral, Codebooks, IcsInfo, ESC_HCB, MAX_SFB};
use super::tables::BandTables;
use super::AacError;

/// Longest spectral codeword per codebook (0 to 31): Huffman word, signs and
/// escapes at the codebook's largest value. Checked against the codebooks
/// and the VCB11 limits in the tests.
pub(super) const MAX_CW_LEN: [u8; 32] = [
    0, 11, 9, 20, 16, 13, 11, 14, 12, 17, 14, 49, 0, 0, 0, 0, 14, 17, 21, 21, 25, 25, 29, 29, 29, 29, 33,
    33, 33, 37, 37, 41,
];

/// Codebook classes in priority order. Each small-book class covers a book
/// and its partner (9 and 10, 7 and 8, ...).
const PRIORITY: [u8; 22] = [11, 31, 30, 29, 28, 27, 26, 25, 24, 23, 22, 21, 20, 19, 18, 17, 16, 9, 7, 5, 3, 1];

fn in_class(class: u8, cb: u8) -> bool {
    if class < ESC_HCB { cb == class || cb == class + 1 } else { cb == class }
}

/// Lines per codeword: quads for books 1 to 4, pairs otherwise.
pub(super) fn tuple_len(cb: u8) -> usize {
    if cb < 5 { 4 } else { 2 }
}

/// The channel's codewords in HCR order: (codebook, index of the first line
/// in the deinterleaved spectrum).
pub(super) fn codeword_order(info: &IcsInfo, band_type: &[[u8; MAX_SFB]; 8], bands: &BandTables) -> Vec<(u8, usize)> {
    let short = info.short();
    let offsets = bands.offsets(short);
    let win_len = usize::from(*offsets.last().unwrap());
    let mut group_start = [0usize; 8];
    for g in 1..info.groups {
        group_start[g] = group_start[g - 1] + usize::from(info.group_len[g - 1]);
    }
    let mut out = Vec::new();
    for &class in &PRIORITY {
        for b in 0..info.max_sfb {
            let (lo, hi) = (usize::from(offsets[b]), usize::from(offsets[b + 1]));
            let width = hi - lo;
            for unit in 0..width / 4 {
                for g in 0..info.groups {
                    let cb = band_type[g][b];
                    if !is_spectral(cb) || !in_class(class, cb) {
                        continue;
                    }
                    let inc = tuple_len(cb);
                    let glen = usize::from(info.group_len[g]);
                    // Four lines per window of the group, counted along the
                    // group's interleaved (window by window) order.
                    let per_unit = 4 * glen / inc;
                    for cw in 0..per_unit {
                        let q = inc * (cw + unit * per_unit);
                        let w = q / width;
                        out.push((cb, (group_start[g] + w) * win_len + lo + q % width));
                    }
                }
            }
        }
    }
    out
}

/// Segment widths along the order, as many as fit in `len` bits; the
/// segments run back to back and the last one takes the leftover bits.
pub(super) fn segments(order: &[(u8, usize)], len: usize, longest: usize) -> Vec<(usize, usize)> {
    let mut segs: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0;
    for &(cb, _) in order {
        let w = usize::from(MAX_CW_LEN[usize::from(cb)]).min(longest);
        if pos + w > len {
            break;
        }
        segs.push((pos, pos + w));
        pos += w;
    }
    if let Some(last) = segs.last_mut() {
        last.1 = len;
    }
    segs
}

/// Tries one codeword on `bits`; Some(values, bits used) if they hold all of it.
fn try_decode(books: &Codebooks, cb: u8, bits: &[bool]) -> Option<([i32; 4], usize)> {
    let mut bytes = vec![0u8; bits.len().div_ceil(8) + 1];
    for (i, &b) in bits.iter().enumerate() {
        if b {
            bytes[i / 8] |= 0x80 >> (i % 8);
        }
    }
    let mut br = BitReader::sub(&bytes, 0, bits.len());
    let vals = books.read_tuple(cb, &mut br).ok()?;
    (!br.overrun()).then_some((vals, br.position()))
}

/// Decodes `len` bits of reordered spectral data starting at bit `start` of
/// `data` into `quant` (deinterleaved). Codewords the data never completes
/// (a corrupt frame) stay zero. Returns whether the decode was clean: every
/// codeword complete and every bit used, as an intact frame always is.
pub(super) fn decode(
    data: &[u8],
    start: usize,
    len: usize,
    longest: usize,
    order: &[(u8, usize)],
    quant: &mut [i32],
) -> Result<bool, AacError> {
    if order.is_empty() || len == 0 {
        return Ok(len == 0);
    }
    if longest == 0 || len < longest {
        return Err(AacError::Invalid("HCR lengths inconsistent"));
    }
    let mut br = BitReader::sub(data, start, len);
    let bits: Vec<bool> = (0..len).map(|_| br.bit()).collect();
    if br.overrun() {
        return Err(AacError::Truncated);
    }
    let books = Codebooks::get();
    let mut segs = segments(order, len, longest);
    let n_seg = segs.len();
    if n_seg == 0 {
        return Err(AacError::Invalid("HCR data shorter than one segment"));
    }
    let put = |quant: &mut [i32], (cb, at): (u8, usize), v: [i32; 4]| {
        let n = tuple_len(cb);
        if at + n <= quant.len() {
            quant[at..at + n].copy_from_slice(&v[..n]);
        }
    };
    // Priority codewords, from the start of their segments.
    for (s, seg) in segs.iter_mut().enumerate() {
        let (cb, _) = order[s];
        let (v, used) = try_decode(books, cb, &bits[seg.0..seg.1])
            .ok_or(AacError::Invalid("HCR priority codeword does not decode"))?;
        put(quant, order[s], v);
        seg.0 += used;
    }
    // The rest in sets, alternating direction.
    let rest = &order[n_seg..];
    let mut done = vec![false; rest.len()];
    let mut pending: Vec<Vec<bool>> = vec![Vec::new(); rest.len()];
    for set in 0..rest.len().div_ceil(n_seg) {
        let backward = set % 2 == 0;
        for trial in 0..n_seg {
            for c in 0..n_seg {
                let idx = set * n_seg + c;
                if idx >= rest.len() {
                    break;
                }
                let seg = &mut segs[(trial + c) % n_seg];
                if done[idx] || seg.0 >= seg.1 {
                    continue;
                }
                let mut cand = std::mem::take(&mut pending[idx]);
                let had = cand.len();
                if backward {
                    cand.extend((seg.0..seg.1).rev().map(|i| bits[i]));
                } else {
                    cand.extend(bits[seg.0..seg.1].iter().copied());
                }
                match try_decode(books, rest[idx].0, &cand) {
                    Some((v, used)) => {
                        let took = used - had;
                        if backward {
                            seg.1 -= took;
                        } else {
                            seg.0 += took;
                        }
                        put(quant, rest[idx], v);
                        done[idx] = true;
                    }
                    None => {
                        pending[idx] = cand;
                        seg.0 = seg.1;
                    }
                }
            }
        }
    }
    let left: usize = segs.iter().map(|s| s.1 - s.0).sum();
    Ok(left == 0 && done.iter().all(|&d| d))
}
