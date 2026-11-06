//! STANAG 5066 D_PDUs found in a decoded bit stream.
//!
//! Every D_PDU opens with the 16-bit Maury-Styles sync 0x90EB, then a
//! header: the D_PDU type and engineering orderwire (EOW) in two bytes, the
//! end-of-transmission count (EOT, in half seconds), a byte holding the
//! address field's size (3 bits) and the header's size (5 bits), the
//! address field (destination then source, each half of it in nibbles),
//! the type's own fields, and a CRC-16 over all of that. Types that carry
//! user data follow with the segment and a CRC-32 over it.
//!
//! A D_PDU is reported only when its header CRC checks: with the sync that
//! is 32 bits a random stream would have to get right, and the sizes and
//! type must make sense besides. The stream is searched at every bit
//! offset and in both bit orders, since modems disagree on whether a byte
//! leaves least or most significant bit first. Header size is read as the
//! bytes from the type through the type's own fields, the address not
//! counted, which is how implementations build it; the CRC is accepted in
//! either byte order.

/// The sync as sent least significant bit first: 0x90 then 0xEB.
const SYNC_BYTES: [u8; 2] = [0x90, 0xEB];
/// Longest D_PDU searched for: a 31-byte header, a 7-byte address, the
/// header CRC, a 1023-byte segment and its CRC.
const MAX_BYTES: usize = 4 + 31 + 7 + 2 + 1023 + 4;

#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    /// No user data in this type.
    None,
    /// The segment arrived and its CRC-32 checks.
    Ok(Vec<u8>),
    /// The segment arrived but its CRC-32 failed: only its size is kept.
    BadCrc(usize),
    /// The stream ended before the segment did.
    Cut(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Dpdu {
    pub dtype: u8,
    pub eow: u16,
    /// End of transmission, in half seconds; 0 when not given.
    pub eot: u8,
    pub address_bytes: u8,
    pub header_bytes: u8,
    pub destination: u32,
    pub source: u32,
    /// The type's own header fields, between the address and the CRC.
    pub fields: Vec<u8>,
    /// The segmented C_PDU size, for types that carry data.
    pub segment_bytes: Option<u16>,
    pub payload: Payload,
    /// The bit order the stream used: true for most significant first.
    pub msb_first: bool,
}

impl Dpdu {
    pub fn type_name(&self) -> &'static str {
        type_name(self.dtype)
    }

    /// The transmit sequence number, for the ARQ data types.
    pub fn tx_seq(&self) -> Option<u8> {
        matches!(self.dtype, 0 | 2 | 4).then(|| self.fields.get(2).copied()).flatten()
    }

    /// The C_PDU id, for the non-ARQ data types.
    pub fn c_pdu_id(&self) -> Option<u16> {
        if !matches!(self.dtype, 7 | 8) || self.fields.len() < 3 {
            return None;
        }
        Some(u16::from(self.fields[0] >> 4) << 8 | u16::from(self.fields[2]))
    }
}

pub fn type_name(t: u8) -> &'static str {
    match t {
        0 => "DATA-ONLY",
        1 => "ACK-ONLY",
        2 => "DATA-ACK",
        3 => "RESET/WIN-RESYNC",
        4 => "EXPEDITED-DATA-ONLY",
        5 => "EXPEDITED-ACK-ONLY",
        6 => "MANAGEMENT",
        7 => "NON-ARQ DATA",
        8 => "EXPEDITED-NON-ARQ DATA",
        15 => "WARNING",
        _ => "RESERVED",
    }
}

/// Bytes of the type's own header fields: exact, or at least.
fn type_fields(t: u8) -> Option<(usize, bool)> {
    match t {
        0 | 3 | 4 => Some((3, true)),
        1 | 5 => Some((1, false)),
        2 => Some((4, false)),
        6 => Some((2, false)),
        7 | 8 => Some((9, true)),
        15 => Some((1, true)),
        _ => None,
    }
}

fn carries_data(t: u8) -> bool {
    matches!(t, 0 | 2 | 4 | 7 | 8)
}

/// CRC-16 of STANAG 5066 annex C: polynomial 0x9299 in the reflected form,
/// zero start, bits taken least significant first.
pub(super) fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in bytes {
        for i in 0..8 {
            let bit = (crc ^ u16::from(b >> i)) & 1;
            crc >>= 1;
            if bit != 0 {
                crc ^= 0x9299;
            }
        }
    }
    crc
}

/// CRC-32 over the data segment, the same way with 0xF3A4E550.
pub(super) fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in bytes {
        for i in 0..8 {
            let bit = (crc ^ u32::from(b >> i)) & 1;
            crc >>= 1;
            if bit != 0 {
                crc ^= 0xF3A4_E550;
            }
        }
    }
    crc
}

fn crc16_matches(c: u16, b: &[u8]) -> bool {
    let be = u16::from(b[0]) << 8 | u16::from(b[1]);
    c == be || c == be.swap_bytes()
}

fn crc32_matches(c: u32, b: &[u8]) -> bool {
    let be = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    c == be || c == be.swap_bytes()
}

/// What a candidate needs next.
enum Parse {
    /// Not enough bytes yet.
    More,
    /// Not a D_PDU.
    Reject,
    /// A header whose CRC checks; the payload, if any, still to come.
    Header(Dpdu, usize),
}

/// Read a header from the bytes after the sync.
fn parse_header(b: &[u8], msb_first: bool) -> Parse {
    if b.len() < 4 {
        return Parse::More;
    }
    let dtype = b[0] >> 4;
    let Some((min_fields, exact)) = type_fields(dtype) else {
        return Parse::Reject;
    };
    let addr = (b[3] >> 5) as usize;
    let hdr = (b[3] & 0x1f) as usize;
    if addr == 0 || hdr < 4 + min_fields || (exact && hdr != 4 + min_fields) {
        return Parse::Reject;
    }
    let end = hdr + addr;
    if b.len() < end + 2 {
        return Parse::More;
    }
    if !crc16_matches(crc16(&b[..end]), &b[end..end + 2]) {
        return Parse::Reject;
    }
    // The address field: `addr` bytes, `addr` nibbles of destination then
    // as many of source.
    let nib = |i: usize| {
        let byte = b[4 + i / 2];
        if i.is_multiple_of(2) { byte >> 4 } else { byte & 0x0f }
    };
    let mut destination = 0u32;
    let mut source = 0u32;
    for i in 0..addr {
        destination = destination << 4 | u32::from(nib(i));
        source = source << 4 | u32::from(nib(addr + i));
    }
    let fields = b[4 + addr..end].to_vec();
    let segment_bytes =
        carries_data(dtype).then(|| u16::from(fields[0] & 0x03) << 8 | u16::from(fields[1]));
    let d = Dpdu {
        dtype,
        eow: u16::from(b[0] & 0x0f) << 8 | u16::from(b[1]),
        eot: b[2],
        address_bytes: addr as u8,
        header_bytes: hdr as u8,
        destination,
        source,
        fields,
        segment_bytes,
        payload: Payload::None,
        msb_first,
    };
    Parse::Header(d, end + 2)
}

/// A sync seen, and what is known so far.
struct Candidate {
    /// Bit index, in the searcher's history, of the first bit after sync.
    start: u64,
    msb_first: bool,
    header: Option<(Dpdu, usize)>,
}

/// Finds D_PDUs in a stream of hard bits.
pub(super) struct DpduSearch {
    /// Recent bits, oldest first; `base` is the index of the first.
    bits: Vec<u8>,
    base: u64,
    /// The last 16 bits, newest in bit 0.
    reg: u16,
    seen: u64,
    pending: Vec<Candidate>,
    found: Vec<Dpdu>,
}

/// The 16 sync bits as they arrive, newest in bit 0, in each order.
const fn sync_patterns() -> [u16; 2] {
    let mut lsb = 0u16;
    let mut msb = 0u16;
    let mut k = 0;
    while k < 2 {
        let byte = SYNC_BYTES[k];
        let mut i = 0;
        while i < 8 {
            lsb = lsb << 1 | ((byte >> i) & 1) as u16;
            msb = msb << 1 | ((byte >> (7 - i)) & 1) as u16;
            i += 1;
        }
        k += 1;
    }
    [lsb, msb]
}

const SYNC_SEQ: [u16; 2] = sync_patterns();

impl DpduSearch {
    pub(super) fn new() -> Self {
        Self { bits: Vec::new(), base: 0, reg: 0, seen: 0, pending: Vec::new(), found: Vec::new() }
    }

    pub(super) fn push_bits(&mut self, bits: &[u8]) {
        for &b in bits {
            self.push(b & 1);
        }
        self.trim();
    }

    fn push(&mut self, bit: u8) {
        self.bits.push(bit);
        self.reg = self.reg << 1 | u16::from(bit);
        self.seen += 1;
        if self.seen >= 16 {
            for (order, &pat) in SYNC_SEQ.iter().enumerate() {
                if self.reg == pat && self.pending.len() < 64 {
                    self.pending.push(Candidate { start: self.seen, msb_first: order == 1, header: None });
                }
            }
        }
        if self.seen.is_multiple_of(8) {
            self.advance(false);
        }
    }

    fn byte_at(&self, at: u64, msb_first: bool) -> Option<u8> {
        let i = at.checked_sub(self.base)? as usize;
        let s = self.bits.get(i..i + 8)?;
        let mut v = 0u8;
        for (k, &b) in s.iter().enumerate() {
            v |= b << if msb_first { 7 - k } else { k };
        }
        Some(v)
    }

    fn bytes(&self, start: u64, n: usize, msb_first: bool) -> Vec<u8> {
        let mut v = Vec::with_capacity(n);
        for k in 0..n {
            match self.byte_at(start + 8 * k as u64, msb_first) {
                Some(b) => v.push(b),
                None => break,
            }
        }
        v
    }

    /// Settle what can be settled; at the end of a stream (`end`), report
    /// headers whose payloads were cut off.
    fn advance(&mut self, end: bool) {
        let mut keep = Vec::with_capacity(self.pending.len());
        for mut c in std::mem::take(&mut self.pending) {
            let avail = ((self.seen - c.start) / 8) as usize;
            if c.header.is_none() {
                let b = self.bytes(c.start, avail.min(4 + 31 + 7 + 2), c.msb_first);
                match parse_header(&b, c.msb_first) {
                    Parse::More if !end => {
                        keep.push(c);
                        continue;
                    }
                    Parse::More | Parse::Reject => continue,
                    Parse::Header(d, len) => c.header = Some((d, len)),
                }
            }
            let Some((mut d, hlen)) = c.header.take() else { continue };
            let Some(seg) = d.segment_bytes.filter(|&s| s > 0).map(usize::from) else {
                self.found.push(d);
                continue;
            };
            if avail >= hlen + seg + 4 {
                let b = self.bytes(c.start + 8 * hlen as u64, seg + 4, c.msb_first);
                d.payload = if crc32_matches(crc32(&b[..seg]), &b[seg..]) {
                    Payload::Ok(b[..seg].to_vec())
                } else {
                    Payload::BadCrc(seg)
                };
                self.found.push(d);
            } else if end {
                d.payload = Payload::Cut(seg);
                self.found.push(d);
            } else {
                c.header = Some((d, hlen));
                keep.push(c);
            }
        }
        self.pending = keep;
    }

    fn trim(&mut self) {
        let oldest = self.pending.iter().map(|c| c.start).min().unwrap_or(self.seen);
        let keep_from = oldest.min(self.seen.saturating_sub(16)).max(self.seen.saturating_sub(8 * MAX_BYTES as u64));
        // Candidates older than the longest D_PDU are given up.
        self.pending.retain(|c| c.start >= keep_from);
        if keep_from > self.base + 4096 {
            let drop = (keep_from - self.base) as usize;
            self.bits.drain(..drop);
            self.base = keep_from;
        }
    }

    /// The stream has stopped: report what is pending as it stands.
    pub(super) fn flush(&mut self) {
        self.advance(true);
        self.pending.clear();
    }

    pub(super) fn take(&mut self) -> Vec<Dpdu> {
        std::mem::take(&mut self.found)
    }
}

/// A payload is shown only if it does not look like ciphertext: mostly
/// printable, or too short or too repetitive for random bytes. Its size is
/// all that is said otherwise.
pub(super) fn looks_encrypted(p: &[u8]) -> bool {
    if p.len() < 16 {
        return false;
    }
    let printable = p.iter().filter(|&&c| (0x20..0x7f).contains(&c) || c == b'\n' || c == b'\r').count();
    if printable as f32 >= 0.85 * p.len() as f32 {
        return false;
    }
    let mut hist = [0u32; 256];
    for &c in p {
        hist[c as usize] += 1;
    }
    let n = p.len() as f32;
    let h: f32 = hist.iter().filter(|&&k| k > 0).map(|&k| -(k as f32 / n) * (k as f32 / n).log2()).sum();
    // A random string of n bytes has entropy near log2(n) for small n and
    // 8 bits for large; ciphertext reaches nearly that.
    let max = (n.log2()).min(8.0);
    h > 0.8 * max
}

/// Build the bytes of a D_PDU, sync included, for the test transmitters.
#[cfg(test)]
pub(super) fn build(dtype: u8, eot: u8, dest: u32, src: u32, addr_bytes: u8, fields: &[u8], data: &[u8]) -> Vec<u8> {
    let hdr = 4 + fields.len();
    let mut v = vec![dtype << 4, 0, eot, addr_bytes << 5 | hdr as u8];
    let n = addr_bytes as usize;
    let mut nibbles = Vec::new();
    for i in (0..n).rev() {
        nibbles.push(((dest >> (4 * i)) & 0xf) as u8);
    }
    for i in (0..n).rev() {
        nibbles.push(((src >> (4 * i)) & 0xf) as u8);
    }
    for pair in nibbles.chunks(2) {
        v.push(pair[0] << 4 | pair[1]);
    }
    v.extend_from_slice(fields);
    let c = crc16(&v);
    v.extend_from_slice(&c.to_be_bytes());
    if !data.is_empty() {
        v.extend_from_slice(data);
        v.extend_from_slice(&crc32(data).to_be_bytes());
    }
    let mut out = SYNC_BYTES.to_vec();
    out.extend(v);
    out
}

/// A NON-ARQ DATA D_PDU carrying `data` whole, for the tests.
#[cfg(test)]
pub(super) fn non_arq(dest: u32, src: u32, c_pdu_id: u16, data: &[u8]) -> Vec<u8> {
    let seg = data.len() as u16;
    let size = data.len() as u16;
    let fields = [
        ((c_pdu_id >> 8) as u8) << 4 | (seg >> 8) as u8 & 0x03,
        seg as u8,
        c_pdu_id as u8,
        (size >> 8) as u8,
        size as u8,
        0,
        0,
        0,
        0,
    ];
    build(7, 0, dest, src, 4, &fields, data)
}

#[cfg(test)]
pub(super) fn bytes_to_bits(bytes: &[u8], msb_first: bool) -> Vec<u8> {
    let mut v = Vec::with_capacity(bytes.len() * 8);
    for &b in bytes {
        for i in 0..8 {
            v.push(if msb_first { (b >> (7 - i)) & 1 } else { (b >> i) & 1 });
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_dpdu_at_any_offset_in_either_order() {
        let pdu = non_arq(0x1234, 0x0abc_def0 & 0x0fff_ffff, 0x155, b"HELLO STANAG 5066 WORLD");
        for msb in [false, true] {
            for pad in [0usize, 3, 13] {
                let mut bits = vec![1u8; pad];
                bits.extend(bytes_to_bits(&[0x55, 0x12], msb));
                bits.extend(bytes_to_bits(&pdu, msb));
                bits.extend(vec![0u8; 40]);
                let mut s = DpduSearch::new();
                s.push_bits(&bits);
                let f = s.take();
                assert_eq!(f.len(), 1, "msb {msb} pad {pad}");
                let d = &f[0];
                assert_eq!(d.dtype, 7);
                assert_eq!(d.destination, 0x1234);
                assert_eq!(d.source, 0x0abc_def0 & 0xffff);
                assert_eq!(d.c_pdu_id(), Some(0x155));
                assert_eq!(d.payload, Payload::Ok(b"HELLO STANAG 5066 WORLD".to_vec()));
                assert_eq!(d.msb_first, msb);
            }
        }
    }

    #[test]
    fn a_damaged_header_is_not_reported() {
        let pdu = non_arq(0x1, 0x2, 7, b"abc");
        let mut bits = bytes_to_bits(&pdu, false);
        bits[16 + 20] ^= 1;
        bits.extend(vec![0u8; 64]);
        let mut s = DpduSearch::new();
        s.push_bits(&bits);
        s.flush();
        assert!(s.take().is_empty());
    }

    #[test]
    fn a_damaged_payload_keeps_only_its_size() {
        let pdu = non_arq(0x1, 0x2, 7, b"abcdefgh");
        let mut bits = bytes_to_bits(&pdu, false);
        let n = bits.len();
        bits[n - 40] ^= 1;
        bits.extend(vec![0u8; 64]);
        let mut s = DpduSearch::new();
        s.push_bits(&bits);
        let f = s.take();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].payload, Payload::BadCrc(8));
    }

    #[test]
    fn random_bits_find_nothing() {
        let mut seed = 99u64;
        let mut bits = Vec::new();
        for _ in 0..400_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            bits.push((seed >> 40) as u8 & 1);
        }
        let mut s = DpduSearch::new();
        s.push_bits(&bits);
        s.flush();
        assert!(s.take().is_empty());
    }

    #[test]
    fn ciphertext_is_recognized() {
        let mut seed = 5u64;
        let random: Vec<u8> = (0..200)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (seed >> 33) as u8
            })
            .collect();
        assert!(looks_encrypted(&random));
        assert!(!looks_encrypted(b"THIS IS PLAIN TEXT FROM A SHIP AT SEA"));
        assert!(!looks_encrypted(&[0u8; 64]));
    }
}
