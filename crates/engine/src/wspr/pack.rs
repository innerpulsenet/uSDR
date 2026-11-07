//! The 50-bit WSPR message: what the bits mean, both ways.
//!
//! A message is a 28-bit call field and a 22-bit field that carries a
//! 15-bit locator and 7 bits of power. The power field says which of three
//! layouts the message uses. A power of 0, 3 or 7 dBm in the last digit is
//! the plain form, type 1: call, 4-character grid, power. Any other last
//! digit is type 2: the call field holds the base call and the locator field
//! a prefix or suffix, and the power is shifted by one or two to say which
//! half of the prefix space it came from. A negative power value is type 3:
//! the call field holds a 6-character locator, rotated so it fits a call's
//! shape, and the locator field a 15-bit hash of the sender's call, which
//! only means something if that call was heard in full (type 1 or 2) before.

use std::collections::HashMap;

/// Call fields at or above this do not unpack to a call.
const MAX_CALL: u32 = 262_177_560;
/// Locator fields at or above this are not a grid square.
const MAX_GRID: u32 = 32_400;

/// What a message says, field by field.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    /// "type1", "type2" or "type3".
    pub kind: &'static str,
    /// The sender: a plain call, a compound one with its prefix or suffix,
    /// or for type 3 the hashed call in angle brackets ("<...>" when the
    /// hash names no call heard yet).
    pub call: String,
    /// Four characters for type 1, six for type 3, none for type 2.
    pub grid: Option<String>,
    pub power_dbm: i8,
    /// The message as WSJT-X prints it.
    pub text: String,
}

/// Calls heard in full, by the 15-bit hash a type 3 message sends them as.
///
/// Kept across slots: a station with a 6-character locator alternates a
/// type 1 (or type 2) message with a type 3 one, so the hash is usually
/// learned a slot or two before it is needed. Oldest forgotten first; the
/// hash space is only 32 768 wide, so a long memory of a busy band also
/// means more calls standing behind a hash that is not theirs.
#[derive(Default, Debug, Clone)]
pub struct CallHashes {
    calls: HashMap<u16, (String, u64)>,
    clock: u64,
}

impl CallHashes {
    const CAP: usize = 1_000;

    /// The hash WSJT-X computes of a call (`nhash` with seed 146), 15 bits.
    pub fn hash15(call: &str) -> u16 {
        (lookup3(call.as_bytes(), 146) & 0x7fff) as u16
    }

    pub fn save(&mut self, call: &str) {
        if call.len() < 3 || call.starts_with('<') {
            return;
        }
        self.clock += 1;
        self.calls.insert(Self::hash15(call), (call.to_string(), self.clock));
        if self.calls.len() > Self::CAP {
            // Forget the oldest tenth in one go rather than one per insert.
            let mut ages: Vec<u64> = self.calls.values().map(|(_, t)| *t).collect();
            ages.sort_unstable();
            let cut = ages[Self::CAP / 10];
            self.calls.retain(|_, (_, t)| *t > cut);
        }
    }

    pub fn lookup(&self, hash: u16) -> Option<&str> {
        self.calls.get(&hash).map(|(c, _)| c.as_str())
    }

    pub fn len(&self) -> usize {
        self.calls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

/// Bob Jenkins' lookup3 `hashlittle`, which WSJT-X uses for call hashes.
pub(super) fn lookup3(key: &[u8], initval: u32) -> u32 {
    fn mix(a: &mut u32, b: &mut u32, c: &mut u32) {
        *a = a.wrapping_sub(*c);
        *a ^= c.rotate_left(4);
        *c = c.wrapping_add(*b);
        *b = b.wrapping_sub(*a);
        *b ^= a.rotate_left(6);
        *a = a.wrapping_add(*c);
        *c = c.wrapping_sub(*b);
        *c ^= b.rotate_left(8);
        *b = b.wrapping_add(*a);
        *a = a.wrapping_sub(*c);
        *a ^= c.rotate_left(16);
        *c = c.wrapping_add(*b);
        *b = b.wrapping_sub(*a);
        *b ^= a.rotate_left(19);
        *a = a.wrapping_add(*c);
        *c = c.wrapping_sub(*b);
        *c ^= b.rotate_left(4);
        *b = b.wrapping_add(*a);
    }
    fn fin(a: &mut u32, b: &mut u32, c: &mut u32) {
        *c ^= *b;
        *c = c.wrapping_sub(b.rotate_left(14));
        *a ^= *c;
        *a = a.wrapping_sub(c.rotate_left(11));
        *b ^= *a;
        *b = b.wrapping_sub(a.rotate_left(25));
        *c ^= *b;
        *c = c.wrapping_sub(b.rotate_left(16));
        *a ^= *c;
        *a = a.wrapping_sub(c.rotate_left(4));
        *b ^= *a;
        *b = b.wrapping_sub(a.rotate_left(14));
        *c ^= *b;
        *c = c.wrapping_sub(b.rotate_left(24));
    }
    let word = |k: &[u8]| -> u32 { k.iter().enumerate().fold(0u32, |acc, (i, &b)| acc.wrapping_add((b as u32) << (8 * i))) };
    let init = 0xdead_beef_u32.wrapping_add(key.len() as u32).wrapping_add(initval);
    let (mut a, mut b, mut c) = (init, init, init);
    let mut k = key;
    while k.len() > 12 {
        a = a.wrapping_add(word(&k[0..4]));
        b = b.wrapping_add(word(&k[4..8]));
        c = c.wrapping_add(word(&k[8..12]));
        mix(&mut a, &mut b, &mut c);
        k = &k[12..];
    }
    if k.is_empty() {
        return c;
    }
    a = a.wrapping_add(word(&k[..k.len().min(4)]));
    if k.len() > 4 {
        b = b.wrapping_add(word(&k[4..k.len().min(8)]));
    }
    if k.len() > 8 {
        c = c.wrapping_add(word(&k[8..]));
    }
    fin(&mut a, &mut b, &mut c);
    c
}

/// Character codes of the call field: digits, letters, then space.
fn code37(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some((c - b'0') as u32),
        b'A'..=b'Z' => Some((c - b'A') as u32 + 10),
        b' ' => Some(36),
        _ => None,
    }
}

fn char37(v: u32) -> char {
    match v {
        0..=9 => (b'0' + v as u8) as char,
        10..=35 => (b'A' + (v - 10) as u8) as char,
        _ => ' ',
    }
}

/// A call of up to six characters whose digit falls third (with a space
/// put in front when it falls second), as the 28-bit field.
fn pack_call(call: &str) -> Option<u32> {
    let b = call.as_bytes();
    if b.is_empty() || b.len() > 6 {
        return None;
    }
    let mut c = [b' '; 6];
    if b.len() > 2 && b[2].is_ascii_digit() {
        c[..b.len()].copy_from_slice(b);
    } else if b.len() > 1 && b[1].is_ascii_digit() && b.len() < 6 {
        c[1..=b.len()].copy_from_slice(b);
    } else {
        return None;
    }
    let mut n = code37(c[0])?;
    let c1 = code37(c[1])?;
    if c1 > 35 || !c[2].is_ascii_digit() {
        return None;
    }
    n = n * 36 + c1;
    n = n * 10 + (c[2] - b'0') as u32;
    for &ch in &c[3..] {
        let v = code37(ch)?;
        if v < 10 {
            return None;
        }
        n = n * 27 + (v - 10);
    }
    Some(n)
}

fn unpack_call(mut n: u32) -> Option<String> {
    if n >= MAX_CALL {
        return None;
    }
    let mut c = [' '; 6];
    for i in (3..6).rev() {
        c[i] = char37(n % 27 + 10);
        n /= 27;
    }
    c[2] = char37(n % 10);
    n /= 10;
    c[1] = char37(n % 36);
    n /= 36;
    c[0] = char37(n);
    let s: String = c.iter().collect();
    let s = s.trim();
    // Spaces may only pad the ends.
    if s.is_empty() || s.contains(' ') {
        return None;
    }
    Some(s.to_string())
}

fn pack_grid4(grid: &str) -> Option<u32> {
    let g = grid.as_bytes();
    if g.len() != 4 || !(b'A'..=b'R').contains(&g[0]) || !(b'A'..=b'R').contains(&g[1]) || !g[2].is_ascii_digit() || !g[3].is_ascii_digit() {
        return None;
    }
    let lon = 10 * (g[0] - b'A') as u32 + (g[2] - b'0') as u32;
    let lat = 10 * (g[1] - b'A') as u32 + (g[3] - b'0') as u32;
    Some((179 - lon) * 180 + lat)
}

fn unpack_grid4(n: u32) -> Option<String> {
    if n >= MAX_GRID {
        return None;
    }
    let lon = 179 - n / 180;
    let lat = n % 180;
    let g = [b'A' + (lon / 10) as u8, b'A' + (lat / 10) as u8, b'0' + (lon % 10) as u8, b'0' + (lat % 10) as u8];
    Some(String::from_utf8_lossy(&g).into_owned())
}

/// Powers a WSPR message can carry: 0 to 60 dBm, ending in 0, 3 or 7.
fn power_ok(dbm: i32) -> bool {
    (0..=60).contains(&dbm) && matches!(dbm % 10, 0 | 3 | 7)
}

fn grid6_ok(g: &str) -> bool {
    let b = g.as_bytes();
    b.len() == 6
        && (b'A'..=b'R').contains(&b[0])
        && (b'A'..=b'R').contains(&b[1])
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && (b'A'..=b'X').contains(&b[4])
        && (b'A'..=b'X').contains(&b[5])
}

/// The prefix or suffix of a compound call, and the base call, as the
/// 15-bit field plus the one bit that rides on the power.
fn pack_affix(call: &str) -> Option<(u32, u32, u32)> {
    let (left, right) = call.split_once('/')?;
    if right.contains('/') {
        return None;
    }
    if (1..=2).contains(&right.len()) {
        let base = pack_call(left)?;
        let r = right.as_bytes();
        let m = if r.len() == 1 {
            60_000 + code37(r[0]).filter(|&v| v < 36)?
        } else {
            if !r[0].is_ascii_digit() || !r[1].is_ascii_digit() {
                return None;
            }
            60_000 + 26 + 10 * (r[0] - b'0') as u32 + (r[1] - b'0') as u32
        };
        return Some((base, m & 0x7fff, m >> 15));
    }
    if !(1..=3).contains(&left.len()) {
        return None;
    }
    let base = pack_call(right)?;
    let mut m = 0u32;
    for _ in left.len()..3 {
        m = 37 * m + 36;
    }
    for &ch in left.as_bytes() {
        m = 37 * m + code37(ch).filter(|&v| v < 36)?;
    }
    Some((base, m & 0x7fff, m >> 15))
}

fn unpack_affix(n: u32, base: &str) -> Option<String> {
    if n < 60_000 {
        let mut m = n;
        let mut p = [' '; 3];
        for c in p.iter_mut().rev() {
            *c = char37(m % 37);
            m /= 37;
        }
        if m != 0 {
            return None;
        }
        let pfx: String = p.iter().collect();
        let pfx = pfx.trim_start();
        if pfx.is_empty() || pfx.contains(' ') {
            return None;
        }
        return Some(format!("{pfx}/{base}"));
    }
    let nc = n - 60_000;
    match nc {
        0..=35 => Some(format!("{base}/{}", char37(nc))),
        36..=125 => Some(format!("{base}/{:02}", nc - 26)),
        _ => None,
    }
}

/// The 50 message bits, first bit highest, from `text` as WSJT-X prints a
/// WSPR message: "K1ABC FN42 37", "PJ4/K1ABC 37" or "<K1ABC> FN42AX 37".
/// Type 3 hashes the call between the brackets.
pub fn pack(text: &str) -> Option<u64> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let (n1, field, ntype) = match parts.as_slice() {
        [call, grid, pwr] if call.starts_with('<') && call.ends_with('>') => {
            let dbm: i32 = pwr.parse().ok()?;
            let call = &call[1..call.len() - 1];
            if !power_ok(dbm) || !grid6_ok(grid) || call.is_empty() {
                return None;
            }
            // The locator turned one place left reads as a call: "FN42AX" is
            // sent as "N42AXF".
            let rotated = format!("{}{}", &grid[1..], &grid[..1]);
            (pack_call(&rotated)?, CallHashes::hash15(call) as u32, -(dbm + 1))
        }
        [call, grid, pwr] => {
            let dbm: i32 = pwr.parse().ok()?;
            if !power_ok(dbm) {
                return None;
            }
            (pack_call(call)?, pack_grid4(grid)?, dbm)
        }
        [call, pwr] if call.contains('/') => {
            let dbm: i32 = pwr.parse().ok()?;
            if !power_ok(dbm) {
                return None;
            }
            let (base, m, add) = pack_affix(call)?;
            (base, m, dbm + 1 + add as i32)
        }
        _ => return None,
    };
    let n2 = field * 128 + (ntype + 64) as u32;
    Some(((n1 as u64) << 22) | n2 as u64)
}

/// What the 50 bits say; `None` when they do not unpack to a message
/// WSJT-X would print. Type 1 and 2 calls are added to `hashes`, and type 3
/// hashes are looked up in it.
pub fn unpack(bits: u64, hashes: &mut CallHashes) -> Option<Message> {
    let n1 = (bits >> 22) as u32;
    let n2 = (bits & 0x3f_ffff) as u32;
    let field = n2 >> 7;
    let ntype = (n2 & 127) as i32 - 64;
    let call = unpack_call(n1)?;
    if (0..=62).contains(&ntype) {
        let nu = ntype % 10;
        if matches!(nu, 0 | 3 | 7) {
            let grid = unpack_grid4(field)?;
            hashes.save(&call);
            let text = format!("{call} {grid} {ntype}");
            return Some(Message { kind: "type1", call, grid: Some(grid), power_dbm: ntype as i8, text });
        }
        let nadd = if nu > 7 {
            nu - 7
        } else if nu > 3 {
            nu - 3
        } else {
            nu
        };
        let dbm = ntype - nadd;
        if !power_ok(dbm) {
            return None;
        }
        let full = unpack_affix(field + 32_768 * (nadd as u32 - 1), &call)?;
        hashes.save(&full);
        let text = format!("{full} {dbm}");
        return Some(Message { kind: "type2", call: full, grid: None, power_dbm: dbm as i8, text });
    }
    if ntype >= 0 {
        return None;
    }
    let dbm = -(ntype + 1);
    if !power_ok(dbm) {
        return None;
    }
    // The call field is the locator turned one place left; turn it back.
    // A shorter "call" would be a locator padded with spaces, which WSJT-X
    // prints but which says less than it seems to; only whole ones pass.
    if call.len() != 6 {
        return None;
    }
    let grid = format!("{}{}", &call[5..], &call[..5]);
    if !grid6_ok(&grid) {
        return None;
    }
    let name = match hashes.lookup(field as u16) {
        Some(c) => format!("<{c}>"),
        None => "<...>".to_string(),
    };
    let text = format!("{name} {grid} {dbm}");
    Some(Message { kind: "type3", call: name, grid: Some(grid), power_dbm: dbm as i8, text })
}
