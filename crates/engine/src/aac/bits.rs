//! An MSB-first bit reader over one frame.
//!
//! Reading past the end yields zeros and marks the reader overrun rather than
//! failing each call: the parsers check `overrun()` at element boundaries and
//! inside every loop whose length the stream controls, which keeps the hot
//! paths free of error plumbing while still refusing a truncated frame.

pub(super) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, end: data.len() * 8 }
    }

    /// A reader over `len` bits starting at bit `start` of `data`.
    pub fn sub(data: &'a [u8], start: usize, len: usize) -> Self {
        Self { data, pos: start, end: (start + len).min(data.len() * 8) }
    }

    /// Another reader over `len` bits from bit `start` of the same data.
    pub fn fork(&self, start: usize, len: usize) -> BitReader<'a> {
        Self::sub(self.data, start, len)
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn bits_left(&self) -> isize {
        self.end as isize - self.pos as isize
    }

    pub fn overrun(&self) -> bool {
        self.pos > self.end
    }

    /// The next `n` bits (n <= 32) without consuming them, zero-padded past
    /// the end.
    #[inline]
    pub fn peek(&self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let byte = self.pos >> 3;
        let mut v: u64 = 0;
        if byte + 8 <= self.data.len() {
            v = u64::from_be_bytes(self.data[byte..byte + 8].try_into().unwrap());
        } else {
            for i in 0..8 {
                v = (v << 8) | u64::from(*self.data.get(byte + i).unwrap_or(&0));
            }
        }
        // Bits past `end` (a sub-reader's limit) read as zero too.
        let v = (v << (self.pos & 7)) >> (64 - n);
        let avail = self.end.saturating_sub(self.pos);
        if (avail as u32) < n {
            if avail == 0 {
                return 0;
            }
            return (v as u32) & !((1u32 << (n - avail as u32)) - 1);
        }
        v as u32
    }

    #[inline]
    pub fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    #[inline]
    pub fn read(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.pos += n as usize;
        v
    }

    #[inline]
    pub fn bit(&mut self) -> bool {
        self.read(1) != 0
    }

    pub fn byte_align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first_and_pads() {
        let d = [0b1010_0001u8, 0xFF];
        let mut br = BitReader::new(&d);
        assert_eq!(br.read(1), 1);
        assert_eq!(br.read(3), 0b010);
        assert_eq!(br.read(8), 0b0001_1111);
        assert_eq!(br.read(8), 0b1111_0000);
        assert!(br.overrun());
    }

    #[test]
    fn sub_reader_stops_at_its_limit() {
        let d = [0xFFu8, 0xFF];
        let mut br = BitReader::sub(&d, 4, 6);
        assert_eq!(br.read(8), 0b1111_1100);
        assert!(br.overrun());
    }
}
