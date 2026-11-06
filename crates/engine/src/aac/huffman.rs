//! Table-driven Huffman decoding for the AAC and SBR codebooks.
//!
//! The codebooks in the standard are not canonical, so each one is turned
//! into a two-level lookup: the first `ROOT_BITS` of the stream index a root
//! table, and codes longer than that continue into a subtable sized for the
//! longest code under that prefix. Every codebook used here is a complete
//! prefix code (checked when the table is built), so every bit pattern
//! decodes to some symbol and the decoder never has to handle a miss.

use super::bits::BitReader;

const ROOT_BITS: u32 = 9;

/// One lookup entry: a leaf (symbol, code length) or a link to a subtable.
#[derive(Clone, Copy, Default)]
struct Entry {
    /// Leaf: the symbol. Link: the subtable's offset in `entries`.
    value: u16,
    /// Leaf: the code length (or the remaining length, in a subtable).
    /// Link: zero.
    len: u8,
    /// Link: the subtable's index width.
    sub_bits: u8,
}

pub(super) struct Huffman {
    entries: Vec<Entry>,
    root_bits: u32,
}

impl Huffman {
    /// Builds the decoder from `(code, length)` pairs indexed by symbol.
    pub fn new<C: Copy + Into<u32>>(codes: &[(C, u8)]) -> Self {
        let max_len = codes.iter().map(|c| u32::from(c.1)).max().unwrap_or(1);
        let root_bits = ROOT_BITS.min(max_len);
        // Complete prefix code: the Kraft sum is exactly one.
        let kraft: u64 = codes.iter().map(|c| 1u64 << (32 - u32::from(c.1))).sum();
        assert_eq!(kraft, 1u64 << 32, "codebook is not a complete prefix code");

        let mut entries = vec![Entry::default(); 1 << root_bits];
        // Longest code under each root prefix that overflows the root table.
        let mut sub_len = vec![0u32; 1 << root_bits];
        for &(code, len) in codes {
            let (code, len) = (code.into(), u32::from(len));
            if len > root_bits {
                let prefix = (code >> (len - root_bits)) as usize;
                sub_len[prefix] = sub_len[prefix].max(len - root_bits);
            }
        }
        for (prefix, &bits) in sub_len.iter().enumerate() {
            if bits > 0 {
                let offset = entries.len();
                entries.resize(offset + (1 << bits), Entry::default());
                entries[prefix] = Entry { value: offset as u16, len: 0, sub_bits: bits as u8 };
            }
        }
        for (sym, &(code, len)) in codes.iter().enumerate() {
            let (code, len) = (code.into(), u32::from(len));
            if len <= root_bits {
                let shift = root_bits - len;
                let base = (code << shift) as usize;
                for e in &mut entries[base..base + (1 << shift)] {
                    *e = Entry { value: sym as u16, len: len as u8, sub_bits: 0 };
                }
            } else {
                let prefix = (code >> (len - root_bits)) as usize;
                let link = entries[prefix];
                let rest = len - root_bits;
                let shift = u32::from(link.sub_bits) - rest;
                let low = code & ((1 << rest) - 1);
                let base = usize::from(link.value) + ((low << shift) as usize);
                for e in &mut entries[base..base + (1 << shift)] {
                    *e = Entry { value: sym as u16, len: rest as u8, sub_bits: 0 };
                }
            }
        }
        Self { entries, root_bits }
    }

    /// Decodes one symbol.
    #[inline]
    pub fn decode(&self, br: &mut BitReader) -> usize {
        let e = self.entries[br.peek(self.root_bits) as usize];
        if e.len > 0 {
            br.skip(usize::from(e.len));
            return usize::from(e.value);
        }
        br.skip(self.root_bits as usize);
        let s = self.entries[usize::from(e.value) + br.peek(u32::from(e.sub_bits)) as usize];
        br.skip(usize::from(s.len));
        usize::from(s.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes codewords MSB first into bytes.
    fn pack(codes: &[(u32, u8)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut acc: u64 = 0;
        let mut n = 0;
        for &(c, l) in codes {
            acc = (acc << l) | u64::from(c);
            n += u32::from(l);
            while n >= 8 {
                out.push((acc >> (n - 8)) as u8);
                n -= 8;
            }
        }
        if n > 0 {
            out.push((acc << (8 - n)) as u8);
        }
        out
    }

    #[test]
    fn every_codebook_round_trips_every_symbol() {
        use super::super::codebooks::{SCALEFACTOR, SPECTRUM};
        let books: Vec<Vec<(u32, u8)>> = std::iter::once(SCALEFACTOR.to_vec())
            .chain(SPECTRUM.iter().map(|b| b.iter().map(|&(c, l)| (u32::from(c), l)).collect()))
            .collect();
        for book in &books {
            let h = Huffman::new(book);
            let bytes = pack(book);
            let mut br = BitReader::new(&bytes);
            for sym in 0..book.len() {
                assert_eq!(h.decode(&mut br), sym);
            }
        }
    }
}
