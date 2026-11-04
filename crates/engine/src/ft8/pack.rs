//! The 77-bit message: what the bits mean, both ways.
//!
//! The first three bits from the end (`i3`), and for `i3 = 0` the three
//! before them (`n3`), say how the rest is laid out. Standard messages —
//! two calls and a grid or report — are by far the most common; calls that
//! do not fit the standard 28-bit form travel whole in a 58-bit field
//! beside the other call's hash, and hashes are resolved from the calls
//! heard so far.

use super::Message;
use std::collections::HashMap;

/// Values below this in a 28-bit call field are tokens: DE, QRZ, CQ and
/// the directed CQs.
const NTOKENS: u32 = 2_063_592;
/// Then come 22-bit hashes of calls sent elsewhere in full.
const MAX22: u32 = 4_194_304;
/// Grid values above this are reports and acknowledgements.
const MAXGRID4: u32 = 32_400;

const FULL: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ+-./?";
const ALNUM_SPACE_SLASH: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/";
const ALNUM_SPACE: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const ALNUM: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const NUMERIC: &[u8] = b"0123456789";
const LETTERS_SPACE: &[u8] = b" ABCDEFGHIJKLMNOPQRSTUVWXYZ";

fn index(table: &[u8], c: u8) -> Option<u32> {
    table.iter().position(|&t| t == c).map(|i| i as u32)
}

/// The 77 payload bits as an integer, first bit highest.
#[derive(Clone, Copy)]
struct Bits(u128);

impl Bits {
    fn from_payload(p: &[u8; 10]) -> Bits {
        let mut v: u128 = 0;
        for b in p {
            v = (v << 8) | *b as u128;
        }
        Bits(v >> 3)
    }
    fn get(self, start: u32, len: u32) -> u64 {
        ((self.0 >> (77 - start - len)) & ((1u128 << len) - 1)) as u64
    }
}

fn to_payload(v: u128) -> [u8; 10] {
    let v = v << 3;
    let mut p = [0u8; 10];
    for (i, b) in p.iter_mut().enumerate() {
        *b = (v >> (8 * (9 - i))) as u8;
    }
    p
}

/// Calls heard in full, by the hashes other messages may send them as.
///
/// A station with a call that does not fit the standard form is sent whole
/// once and by hash afterwards; this is how the hash is turned back into
/// the call. Kept across slots, oldest forgotten first.
#[derive(Default, Debug, Clone)]
pub struct CallHashes {
    calls: HashMap<u32, (String, u64)>,
    clock: u64,
}

impl CallHashes {
    const CAP: usize = 2_000;

    /// The 22-bit hash WSJT-X computes of a call (`ihashcall`).
    pub fn hash22(call: &str) -> Option<u32> {
        let call = call.trim_matches(|c| c == '<' || c == '>');
        if call.len() > 11 {
            return None;
        }
        let mut n: u64 = 0;
        for i in 0..11 {
            let c = call.as_bytes().get(i).copied().unwrap_or(b' ');
            n = n * 38 + index(ALNUM_SPACE_SLASH, c)? as u64;
        }
        Some(((47_055_833_459u64.wrapping_mul(n)) >> 42) as u32 & 0x3f_ffff)
    }

    pub fn save(&mut self, call: &str) {
        if call.len() < 3 || call.starts_with('<') {
            return;
        }
        let Some(h) = Self::hash22(call) else { return };
        self.clock += 1;
        self.calls.insert(h, (call.to_string(), self.clock));
        if self.calls.len() > Self::CAP {
            // Forget the oldest tenth in one go rather than one per insert.
            let mut ages: Vec<u64> = self.calls.values().map(|v| v.1).collect();
            ages.sort_unstable();
            let cut = ages[Self::CAP / 10];
            self.calls.retain(|_, v| v.1 > cut);
        }
    }

    /// The call whose hash, cut to `bits` (22, 12 or 10), is `h`.
    pub fn lookup(&self, h: u32, bits: u32) -> Option<&str> {
        if bits == 22 {
            return self.calls.get(&h).map(|v| v.0.as_str());
        }
        let shift = 22 - bits;
        self.calls.iter().filter(|(k, _)| *k >> shift == h).max_by_key(|(_, v)| v.1).map(|(_, v)| v.0.as_str())
    }

    pub fn len(&self) -> usize {
        self.calls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    fn bracketed(&self, h: u32, bits: u32) -> String {
        self.lookup(h, bits).map_or_else(|| "<...>".to_string(), |c| format!("<{c}>"))
    }
}

// ---------------------------------------------------------------- unpacking

/// The message a payload carries, and its text as WSJT-X prints it. `None`
/// for a payload no valid message packs to.
pub fn unpack(p: &[u8; 10], hashes: &mut CallHashes) -> Option<(String, Message)> {
    let b = Bits::from_payload(p);
    let i3 = b.get(74, 3) as u8;
    let n3 = b.get(71, 3) as u8;
    match (i3, n3) {
        (1 | 2, _) => unpack_standard(b, i3, hashes),
        (4, _) => unpack_nonstandard(b, hashes),
        (0, 0) => {
            let mut v = (b.0 >> 6) & ((1u128 << 71) - 1);
            let mut c = [b' '; 13];
            for slot in c.iter_mut().rev() {
                *slot = FULL[(v % 42) as usize];
                v /= 42;
            }
            let text = String::from_utf8_lossy(&c).trim().to_string();
            Some((text, Message { kind: "free text", ..Default::default() }))
        }
        (0, 1) => unpack_dxpedition(b, hashes),
        (0, 5) => {
            let v = (b.0 >> 6) & ((1u128 << 71) - 1);
            Some((format!("{v:018X}"), Message { kind: "telemetry", ..Default::default() }))
        }
        // Laid out by the spec but not unpacked here (EU VHF contest, Field
        // Day, RTTY Roundup and the like): their exchanges index tables of
        // sections and states. Reported as what they are rather than guessed.
        (0, 2) => Some(("<EU VHF contest message>".into(), Message { kind: "eu vhf", ..Default::default() })),
        (0, 3 | 4) => Some(("<Field Day message>".into(), Message { kind: "field day", ..Default::default() })),
        (3, _) => Some(("<RTTY Roundup message>".into(), Message { kind: "rtty contest", ..Default::default() })),
        (5, _) => Some(("<WWROF contest message>".into(), Message { kind: "wwrof contest", ..Default::default() })),
        _ => None,
    }
}

/// A 28-bit call field: a token, a hashed call, or a standard call. `ip`
/// is the suffix flag, /R for `i3 = 1` and /P for `i3 = 2`.
fn unpack28(n28: u32, ip: bool, i3: u8, hashes: &mut CallHashes) -> Option<String> {
    if n28 < NTOKENS {
        return match n28 {
            0 => Some("DE".into()),
            1 => Some("QRZ".into()),
            2 => Some("CQ".into()),
            3..=1002 => Some(format!("CQ {:03}", n28 - 3)),
            1003..=532_443 => {
                let mut n = n28 - 1003;
                let mut s = [b' '; 4];
                for c in s.iter_mut().rev() {
                    *c = LETTERS_SPACE[(n % 27) as usize];
                    n /= 27;
                }
                Some(format!("CQ {}", String::from_utf8_lossy(&s).trim()))
            }
            _ => None,
        };
    }
    let n = n28 - NTOKENS;
    if n < MAX22 {
        return Some(hashes.bracketed(n, 22));
    }
    let mut n = n - MAX22;
    let mut c = [b' '; 6];
    c[5] = LETTERS_SPACE[(n % 27) as usize];
    n /= 27;
    c[4] = LETTERS_SPACE[(n % 27) as usize];
    n /= 27;
    c[3] = LETTERS_SPACE[(n % 27) as usize];
    n /= 27;
    c[2] = NUMERIC[(n % 10) as usize];
    n /= 10;
    c[1] = ALNUM[(n % 36) as usize];
    n /= 36;
    c[0] = *ALNUM_SPACE.get(n as usize)?;
    let raw = String::from_utf8_lossy(&c).to_string();
    // Two prefixes that do not fit the form travel in one that does.
    let mut call = if raw.starts_with("3D0") && c[3] != b' ' {
        format!("3DA0{}", raw[3..].trim())
    } else if c[0] == b'Q' && c[1].is_ascii_alphabetic() {
        format!("3X{}", raw[1..].trim())
    } else {
        raw.trim().to_string()
    };
    if call.len() < 3 {
        return None;
    }
    if ip {
        match i3 {
            1 => call.push_str("/R"),
            2 => call.push_str("/P"),
            _ => return None,
        }
    }
    hashes.save(&call);
    Some(call)
}

/// The grid-or-report field: a grid (with "R " when acknowledging), a
/// signed report (with "R" when acknowledging), RRR, RR73, 73, or nothing.
fn unpack_grid(igrid: u32, ir: bool) -> (Option<String>, Option<String>) {
    if igrid <= MAXGRID4 {
        let mut n = igrid;
        let d2 = n % 10;
        n /= 10;
        let d1 = n % 10;
        n /= 10;
        let l2 = (b'A' + (n % 18) as u8) as char;
        n /= 18;
        let l1 = (b'A' + (n % 18) as u8) as char;
        let grid = format!("{l1}{l2}{d1}{d2}");
        let extra = if ir { format!("R {grid}") } else { grid.clone() };
        return (Some(extra), Some(grid));
    }
    let extra = match igrid - MAXGRID4 {
        1 => return (None, None),
        2 => "RRR".to_string(),
        3 => "RR73".to_string(),
        4 => "73".to_string(),
        irpt => {
            let dd = irpt as i32 - 35;
            format!("{}{dd:+03}", if ir { "R" } else { "" })
        }
    };
    (Some(extra), None)
}

fn finish(kind: &'static str, to: Option<String>, de: Option<String>, extra: Option<String>, grid: Option<String>) -> (String, Message) {
    let text = [&to, &de, &extra].iter().filter_map(|f| f.as_deref()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
    let is_cq = to.as_deref().is_some_and(|t| t == "CQ" || t.starts_with("CQ "));
    (text, Message { kind, to, de, extra, grid, is_cq })
}

fn unpack_standard(b: Bits, i3: u8, hashes: &mut CallHashes) -> Option<(String, Message)> {
    let n29a = b.get(0, 29) as u32;
    let n29b = b.get(29, 29) as u32;
    let ir = b.get(58, 1) == 1;
    let igrid = b.get(59, 15) as u32;
    let to = unpack28(n29a >> 1, n29a & 1 == 1, i3, hashes)?;
    let de = unpack28(n29b >> 1, n29b & 1 == 1, i3, hashes)?;
    let (extra, grid) = unpack_grid(igrid, ir);
    Some(finish("standard", Some(to), Some(de), extra, grid))
}

fn unpack58(n58: u64) -> String {
    let mut n = n58;
    let mut c = [b' '; 11];
    for slot in c.iter_mut().rev() {
        *slot = ALNUM_SPACE_SLASH[(n % 38) as usize];
        n /= 38;
    }
    String::from_utf8_lossy(&c).trim().to_string()
}

fn unpack_nonstandard(b: Bits, hashes: &mut CallHashes) -> Option<(String, Message)> {
    let n12 = b.get(0, 12) as u32;
    let n58 = b.get(12, 58);
    let iflip = b.get(70, 1) == 1;
    let nrpt = b.get(71, 2);
    let icq = b.get(73, 1) == 1;
    let whole = unpack58(n58);
    if whole.len() < 3 {
        return None;
    }
    hashes.save(&whole);
    let hashed = hashes.bracketed(n12, 12);
    let (c1, c2) = if iflip { (whole, hashed) } else { (hashed, whole) };
    if icq {
        return Some(finish("nonstandard", Some("CQ".into()), Some(c2), None, None));
    }
    let extra = match nrpt {
        1 => Some("RRR".to_string()),
        2 => Some("RR73".to_string()),
        3 => Some("73".to_string()),
        _ => None,
    };
    Some(finish("nonstandard", Some(c1), Some(c2), extra, None))
}

/// "K1ABC RR73; W9XYZ <KH1/KH7Z> -12": the DX station finishes with one
/// caller and gives the next a report, in one message.
fn unpack_dxpedition(b: Bits, hashes: &mut CallHashes) -> Option<(String, Message)> {
    let done = unpack28(b.get(0, 28) as u32, false, 0, hashes)?;
    let next = unpack28(b.get(28, 28) as u32, false, 0, hashes)?;
    let dx = hashes.bracketed(b.get(56, 10) as u32, 10);
    let rpt = 2 * b.get(66, 5) as i32 - 30;
    let text = format!("{done} RR73; {next} {dx} {rpt:+03}");
    Some((text, Message { kind: "dxpedition", to: Some(next), de: Some(dx), extra: Some(format!("{rpt:+03}")), grid: None, is_cq: false }))
}

// ------------------------------------------------------------------ packing

/// The payload for a message typed as an operator would, trying a standard
/// message, then one with a nonstandard call, then free text. `None` when
/// nothing fits.
pub fn pack(text: &str, hashes: &mut CallHashes) -> Option<[u8; 10]> {
    let text = text.trim().to_ascii_uppercase();
    let words: Vec<&str> = text.split_whitespace().collect();
    // "CQ DX K1ABC FN42": a directed CQ is one field.
    let (to, rest) = match words.as_slice() {
        ["CQ", m, rest @ ..] if !rest.is_empty() && cq_modifier(m).is_some() => (format!("CQ {m}"), rest.to_vec()),
        [first, rest @ ..] => (first.to_string(), rest.to_vec()),
        [] => return None,
    };
    if rest.len() <= 2 && !rest.is_empty() {
        let de = rest[0];
        let extra = rest.get(1).copied().unwrap_or("");
        if let Some(p) = pack_standard(&to, de, extra, hashes) {
            return Some(p);
        }
        if let Some(p) = pack_nonstandard(&to, de, extra, hashes) {
            return Some(p);
        }
    }
    // "K1ABC W9XYZ R FN42" is three words after the first.
    if rest.len() == 3
        && rest[1] == "R"
        && let Some(p) = pack_standard(&to, rest[0], &format!("R {}", rest[2]), hashes)
    {
        return Some(p);
    }
    pack_free(&text)
}

/// "CQ nnn" or "CQ a[bcd]": the value its token carries above the base.
fn cq_modifier(m: &str) -> Option<u32> {
    let b = m.as_bytes();
    if b.len() == 3 && b.iter().all(u8::is_ascii_digit) {
        return m.parse().ok();
    }
    if (1..=4).contains(&b.len()) && b.iter().all(u8::is_ascii_uppercase) {
        return Some(1000 + b.iter().fold(0, |n, c| n * 27 + (c - b'A' + 1) as u32));
    }
    None
}

/// A standard call as its 28-bit number (without the token and hash
/// offsets), or `None` if it is not one.
fn pack_basecall(call: &str) -> Option<u32> {
    let b = call.as_bytes();
    let len = b.len();
    if len < 3 {
        return None;
    }
    let mut c6 = [b' '; 6];
    if call.starts_with("3DA0") && len > 4 && len <= 7 {
        c6[..3].copy_from_slice(b"3D0");
        c6[3..3 + len - 4].copy_from_slice(&b[4..]);
    } else if call.starts_with("3X") && b[2].is_ascii_alphabetic() && len <= 7 {
        c6[0] = b'Q';
        c6[1..len - 1].copy_from_slice(&b[2..]);
    } else if b[2].is_ascii_digit() && len <= 6 {
        c6[..len].copy_from_slice(b);
    } else if b[1].is_ascii_digit() && len <= 5 {
        c6[1..len + 1].copy_from_slice(b);
    } else {
        return None;
    }
    let i0 = index(ALNUM_SPACE, c6[0])?;
    let i1 = index(ALNUM, c6[1])?;
    let i2 = index(NUMERIC, c6[2])?;
    let i3 = index(LETTERS_SPACE, c6[3])?;
    let i4 = index(LETTERS_SPACE, c6[4])?;
    let i5 = index(LETTERS_SPACE, c6[5])?;
    Some(((((i0 * 36 + i1) * 10 + i2) * 27 + i3) * 27 + i4) * 27 + i5)
}

/// A call field: the 28-bit value and the suffix flag.
fn pack28(call: &str, hashes: &mut CallHashes) -> Option<(u32, bool)> {
    match call {
        "DE" => return Some((0, false)),
        "QRZ" => return Some((1, false)),
        "CQ" => return Some((2, false)),
        _ => {}
    }
    if let Some(m) = call.strip_prefix("CQ ") {
        return cq_modifier(m).map(|v| (3 + v, false));
    }
    let (base, ip) = match call.strip_suffix("/P").or_else(|| call.strip_suffix("/R")) {
        Some(b) => (b, true),
        None => (call, false),
    };
    if let Some(n) = pack_basecall(base) {
        hashes.save(call);
        return Some((NTOKENS + MAX22 + n, ip));
    }
    let bare = call.trim_matches(|c| c == '<' || c == '>');
    if (3..=11).contains(&bare.len()) {
        let h = CallHashes::hash22(bare)?;
        hashes.save(bare);
        return Some((NTOKENS + h, false));
    }
    None
}

/// The grid field and its acknowledgement bit.
fn pack_grid(extra: &str) -> Option<(u32, bool)> {
    let e = extra.as_bytes();
    match extra {
        "" => return Some((MAXGRID4 + 1, false)),
        "RRR" => return Some((MAXGRID4 + 2, false)),
        "RR73" => return Some((MAXGRID4 + 3, false)),
        "73" => return Some((MAXGRID4 + 4, false)),
        _ => {}
    }
    let (body, ir) = match extra.strip_prefix("R ") {
        Some(g) => (g.as_bytes(), true),
        None => (e, false),
    };
    if body.len() == 4 && (b'A'..=b'R').contains(&body[0]) && (b'A'..=b'R').contains(&body[1]) && body[2].is_ascii_digit() && body[3].is_ascii_digit() {
        let g = (((body[0] - b'A') as u32 * 18 + (body[1] - b'A') as u32) * 10 + (body[2] - b'0') as u32) * 10 + (body[3] - b'0') as u32;
        return Some((g, ir));
    }
    if ir {
        return None;
    }
    let (rpt, ir) = match extra.strip_prefix('R') {
        Some(r) => (r, true),
        None => (extra, false),
    };
    let dd: i32 = rpt.parse().ok()?;
    if !(rpt.starts_with('+') || rpt.starts_with('-')) || !(-30..=50).contains(&dd) {
        return None;
    }
    Some((MAXGRID4 + (35 + dd) as u32, ir))
}

fn pack_standard(to: &str, de: &str, extra: &str, hashes: &mut CallHashes) -> Option<[u8; 10]> {
    // A slashed call other than /P or /R needs the nonstandard layout.
    // A bracketed call travels as its hash, so its slash does not matter.
    let plain_suffix = |c: &str| c.starts_with('<') || !c.contains('/') || c.ends_with("/P") || c.ends_with("/R");
    if !plain_suffix(to) || !plain_suffix(de) {
        return None;
    }
    let p_suffix = to.ends_with("/P") || de.ends_with("/P");
    if p_suffix && (to.ends_with("/R") || de.ends_with("/R")) {
        return None;
    }
    // Hashing the calling station would hide who is calling.
    pack_basecall(de.strip_suffix("/P").or_else(|| de.strip_suffix("/R")).unwrap_or(de))?;
    let (n28a, ipa) = pack28(to, hashes)?;
    let (n28b, ipb) = pack28(de, hashes)?;
    let (igrid, ir) = pack_grid(extra)?;
    let i3: u128 = if p_suffix { 2 } else { 1 };
    // 28+1, 28+1, 1, 15, 3.
    let mut v: u128 = n28a as u128;
    v = (v << 1) | ipa as u128;
    v = (v << 28) | n28b as u128;
    v = (v << 1) | ipb as u128;
    v = (v << 1) | ir as u128;
    v = (v << 15) | igrid as u128;
    Some(to_payload((v << 3) | i3))
}

fn pack_nonstandard(to: &str, de: &str, extra: &str, hashes: &mut CallHashes) -> Option<[u8; 10]> {
    let icq = to == "CQ";
    if !icq && to.len() < 3 {
        return None;
    }
    if de.len() < 3 {
        return None;
    }
    // The bracketed call is the one sent by hash.
    let iflip = !icq && de.starts_with('<');
    let (hashed, whole) = if iflip { (de, to) } else { (to, de) };
    let n12 = if icq { 0 } else { CallHashes::hash22(hashed)? >> 10 };
    let whole = whole.trim_matches(|c| c == '<' || c == '>');
    if whole.len() > 11 {
        return None;
    }
    let mut n58: u64 = 0;
    for c in whole.bytes() {
        n58 = n58 * 38 + index(ALNUM_SPACE_SLASH, c)? as u64;
    }
    hashes.save(whole);
    let nrpt = match extra {
        "RRR" => 1,
        "RR73" => 2,
        "73" => 3,
        _ => 0,
    };
    let mut v: u128 = n12 as u128;
    v = (v << 58) | n58 as u128;
    v = (v << 1) | iflip as u128;
    v = (v << 2) | if icq { 0 } else { nrpt };
    v = (v << 1) | icq as u128;
    v = (v << 3) | 4;
    Some(to_payload(v))
}

fn pack_free(text: &str) -> Option<[u8; 10]> {
    if text.len() > 13 {
        return None;
    }
    let mut v: u128 = 0;
    for i in 0..13 {
        let c = text.as_bytes().get(i).copied().unwrap_or(b' ');
        v = v * 42 + index(FULL, c)? as u128;
    }
    // i3.n3 = 0.0.
    Some(to_payload(v << 6))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(text: &str) -> (String, Message) {
        let mut h = CallHashes::default();
        let p = pack(text, &mut h).unwrap_or_else(|| panic!("{text} does not pack"));
        unpack(&p, &mut h).unwrap_or_else(|| panic!("{text} does not unpack"))
    }

    #[test]
    fn standard_messages_survive_a_round_trip() {
        for t in [
            "CQ K1ABC FN42",
            "K1ABC W9XYZ -11",
            "W9XYZ K1ABC R-09",
            "K1ABC W9XYZ RR73",
            "K1ABC W9XYZ 73",
            "K1ABC W9XYZ RRR",
            "K1ABC W9XYZ R FN42",
            "K1ABC W9XYZ +05",
            "CQ DX K1ABC FN42",
            "CQ 042 W9XYZ EN37",
            "QRZ K1ABC FN42",
            "PA3XYZ/P GM4ABC/P JO22",
            "W9XYZ/R K1ABC",
            "3DA0XYZ K1ABC",
        ] {
            assert_eq!(round_trip(t).0, t);
        }
    }

    #[test]
    fn a_cq_names_its_caller_and_grid() {
        let (_, m) = round_trip("CQ K1ABC FN42");
        assert!(m.is_cq);
        assert_eq!(m.kind, "standard");
        assert_eq!(m.to.as_deref(), Some("CQ"));
        assert_eq!(m.de.as_deref(), Some("K1ABC"));
        assert_eq!(m.grid.as_deref(), Some("FN42"));
        let (_, r) = round_trip("K1ABC W9XYZ R-09");
        assert!(!r.is_cq);
        assert_eq!(r.extra.as_deref(), Some("R-09"));
        assert_eq!(r.grid, None);
    }

    #[test]
    fn free_text_is_kept_as_typed() {
        let (t, m) = round_trip("TNX BOB 73 GL");
        assert_eq!(t, "TNX BOB 73 GL");
        assert_eq!(m.kind, "free text");
    }

    #[test]
    fn a_nonstandard_call_is_sent_whole_and_its_partner_by_hash() {
        let mut h = CallHashes::default();
        // The hashed call was heard earlier, so it can be named.
        h.save("K1ABC");
        let p = pack("<K1ABC> PJ4/W9XYZ RR73", &mut h).expect("packs");
        let mut fresh = CallHashes::default();
        fresh.save("K1ABC");
        let (t, m) = unpack(&p, &mut fresh).expect("unpacks");
        assert_eq!(t, "<K1ABC> PJ4/W9XYZ RR73");
        assert_eq!(m.kind, "nonstandard");
        // Unheard, the hash stays a hash.
        let (t, _) = unpack(&p, &mut CallHashes::default()).unwrap();
        assert_eq!(t, "<...> PJ4/W9XYZ RR73");
        let p = pack("CQ PJ4/W9XYZ", &mut h).expect("packs");
        let (t, m) = unpack(&p, &mut CallHashes::default()).unwrap();
        assert_eq!(t, "CQ PJ4/W9XYZ");
        assert!(m.is_cq);
    }

    #[test]
    fn a_hashed_call_resolves_once_heard_in_full() {
        let mut h = CallHashes::default();
        let p = pack("K1ABC <PJ4/W9XYZ> -10", &mut h);
        // A slashed call cannot be standard; this one goes as a 22-bit hash.
        assert!(p.is_none() || unpack(&p.unwrap(), &mut CallHashes::default()).is_some());
        assert_eq!(CallHashes::hash22("K1ABC"), CallHashes::hash22("<K1ABC>"));
    }
}
