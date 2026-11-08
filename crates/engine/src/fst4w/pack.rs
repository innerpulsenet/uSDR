//! The 50-bit FST4W message, which is the WSJT-X 77-bit i3=0, n3=6 family
//! cut to its first 50 bits. It looks like WSPR's but packs differently.
//!
//! - Type 1, last two bits `00`: 28-bit call, 15-bit grid4, 5-bit power.
//! - Type 2, last bit `1`: 28-bit base call, 16-bit prefix or suffix, 5-bit
//!   power.
//! - Type 3, last three bits `010`: 22-bit hash of the call, 25-bit grid6.
//!
//! Power is stored as `round(0.3 * dBm)` and read back as
//! `round(stored * 10 / 3)`; a type 3 message carries none.

use std::collections::HashMap;

const NTOKENS: u32 = 2_063_592;
const MAX22: u32 = 4_194_304;
const NZZZ: u32 = 46_656;
const MAX_GRID4: u32 = 32_400;
const C1: &[u8; 37] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const A2: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const C4: &[u8; 27] = b" ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// What a message says.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    /// "type1", "type2" or "type3".
    pub kind: &'static str,
    /// The sender; for type 3 the hashed call in angle brackets ("<...>"
    /// when the hash names no call heard yet).
    pub call: String,
    pub grid: Option<String>,
    /// `None` for type 3, which sends no power.
    pub power_dbm: Option<i8>,
    /// As WSJT-X prints it: "K1ABC FN42 37".
    pub text: String,
}

/// Calls heard in full, by the 22-bit hash a type 3 message sends.
#[derive(Default, Debug, Clone)]
pub struct CallHashes {
    calls: HashMap<u32, (String, u64)>,
    clock: u64,
}

impl CallHashes {
    const CAP: usize = 1_000;

    /// WSJT-X's `ihashcall(call, 22)`.
    pub fn hash22(call: &str) -> u32 {
        const C: &[u8; 38] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/";
        let bytes = call.as_bytes();
        let mut n: u64 = 0;
        for i in 0..11 {
            let c = bytes.get(i).copied().unwrap_or(b' ');
            let j = C.iter().position(|&x| x == c).unwrap_or(0) as u64;
            n = n.wrapping_mul(38).wrapping_add(j);
        }
        (47_055_833_459u64.wrapping_mul(n) >> 42) as u32
    }

    pub fn save(&mut self, call: &str) {
        if call.len() < 3 || call.starts_with('<') {
            return;
        }
        self.clock += 1;
        self.calls.insert(Self::hash22(call), (call.to_string(), self.clock));
        if self.calls.len() > Self::CAP {
            let mut ages: Vec<u64> = self.calls.values().map(|(_, t)| *t).collect();
            ages.sort_unstable();
            let cut = ages[Self::CAP / 10];
            self.calls.retain(|_, (_, t)| *t > cut);
        }
    }

    pub fn lookup(&self, hash: u32) -> Option<&str> {
        self.calls.get(&hash).map(|(c, _)| c.as_str())
    }

    pub fn len(&self) -> usize {
        self.calls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

/// A standard call packed as WSJT-X's `pack28` does, with the digit of the
/// call area in the third of six places.
fn pack_call(call: &str) -> Option<u32> {
    let b = call.as_bytes();
    let area = (1..b.len()).rev().find(|&i| b[i].is_ascii_digit())? + 1;
    if !(2..=3).contains(&area) || b[..area - 1].iter().all(|c| c.is_ascii_digit()) || b.len() - area > 3 {
        return None;
    }
    let mut s = [b' '; 6];
    let at = 3 - area;
    s[at..at + b.len()].copy_from_slice(b);
    let i1 = C1.iter().position(|&x| x == s[0])? as u32;
    let i2 = A2.iter().position(|&x| x == s[1])? as u32;
    if !s[2].is_ascii_digit() {
        return None;
    }
    let i3 = (s[2] - b'0') as u32;
    let i4 = C4.iter().position(|&x| x == s[3])? as u32;
    let i5 = C4.iter().position(|&x| x == s[4])? as u32;
    let i6 = C4.iter().position(|&x| x == s[5])? as u32;
    let n = 36 * 10 * 27 * 27 * 27 * i1 + 10 * 27 * 27 * 27 * i2 + 27 * 27 * 27 * i3 + 27 * 27 * i4 + 27 * i5 + i6;
    Some(n + NTOKENS + MAX22)
}

fn unpack_call(n28: u32) -> Option<String> {
    let mut n = n28.checked_sub(NTOKENS + MAX22)?;
    let i1 = n / (36 * 10 * 27 * 27 * 27);
    n -= 36 * 10 * 27 * 27 * 27 * i1;
    let i2 = n / (10 * 27 * 27 * 27);
    n -= 10 * 27 * 27 * 27 * i2;
    let i3 = n / (27 * 27 * 27);
    n -= 27 * 27 * 27 * i3;
    let i4 = n / (27 * 27);
    n -= 27 * 27 * i4;
    let i5 = n / 27;
    let i6 = n - 27 * i5;
    if i1 > 36 {
        return None;
    }
    let raw = [C1[i1 as usize], A2[i2 as usize], b'0' + i3 as u8, C4[i4 as usize], C4[i5 as usize], C4[i6 as usize]];
    let call = std::str::from_utf8(&raw).ok()?.trim().to_string();
    // A call has no inner blanks and is at least three characters long.
    (call.len() >= 3 && !call.contains(' ') && call.bytes().any(|c| c.is_ascii_alphabetic())).then_some(call)
}

fn power(stored: u32) -> Option<i8> {
    let dbm = (stored as f32 * 10.0 / 3.0).round() as i32;
    (0..=60).contains(&dbm).then_some(dbm as i8)
}

fn store_power(dbm: i32) -> u32 {
    (0.3 * dbm.clamp(0, 60) as f32).round() as u32
}

fn grid4_of(n: u32) -> Option<String> {
    if n >= MAX_GRID4 {
        return None;
    }
    let (j1, n) = (n / 1800, n % 1800);
    let (j2, n) = (n / 100, n % 100);
    let g = [b'A' + j1 as u8, b'A' + j2 as u8, b'0' + (n / 10) as u8, b'0' + (n % 10) as u8];
    String::from_utf8(g.to_vec()).ok()
}

fn grid6_of(n: u32) -> Option<String> {
    let (j1, n) = (n / (18 * 10 * 10 * 625), n % (18 * 10 * 10 * 625));
    let (j2, n) = (n / (10 * 10 * 625), n % (10 * 10 * 625));
    let (j3, n) = (n / (10 * 625), n % (10 * 625));
    let (j4, n) = (n / 625, n % 625);
    let (j5, j6) = (n / 25, n % 25);
    if j1 > 17 || j2 > 17 {
        return None;
    }
    let mut g = vec![b'A' + j1 as u8, b'A' + j2 as u8, b'0' + j3 as u8, b'0' + j4 as u8];
    if j5 != 24 || j6 != 24 {
        if j5 > 23 || j6 > 23 {
            return None;
        }
        g.push(b'A' + j5 as u8);
        g.push(b'A' + j6 as u8);
    }
    String::from_utf8(g).ok()
}

fn is_grid4(s: &[u8]) -> bool {
    s.len() == 4 && (b'A'..=b'R').contains(&s[0]) && (b'A'..=b'R').contains(&s[1]) && s[2].is_ascii_digit() && s[3].is_ascii_digit()
}

fn is_grid6(s: &[u8]) -> bool {
    (s.len() == 4 || s.len() == 6) && is_grid4(&s[..4]) && (s.len() == 4 || ((b'A'..=b'X').contains(&s[4]) && (b'A'..=b'X').contains(&s[5])))
}

/// Pack a message ("K1ABC FN42 37", "PJ4/K1ABC 37", "<K1ABC> FN42AX") into
/// its 50 bits, the first in the top of the low 50 bits of the value.
pub fn pack(text: &str) -> Option<u64> {
    let words: Vec<&str> = text.split_whitespace().collect();
    match words.as_slice() {
        [call, grid, dbm] => {
            if !(3..=6).contains(&call.len()) || !is_grid4(grid.as_bytes()) {
                return None;
            }
            let n28 = pack_call(call)? as u64;
            let g = grid.as_bytes();
            let igrid = (g[0] - b'A') as u64 * 1800 + (g[1] - b'A') as u64 * 100 + (g[2] - b'0') as u64 * 10 + (g[3] - b'0') as u64;
            let p = store_power(dbm.parse().ok()?) as u64;
            Some(n28 << 22 | igrid << 7 | p << 2)
        }
        [call, grid] if call.starts_with('<') && call.ends_with('>') && call.len() >= 5 && is_grid6(grid.as_bytes()) => {
            let h = CallHashes::hash22(&call[1..call.len() - 1]) as u64;
            let g = grid.as_bytes();
            let mut n = (g[0] - b'A') as u64 * (18 * 10 * 10 * 625)
                + (g[1] - b'A') as u64 * (10 * 10 * 625)
                + (g[2] - b'0') as u64 * (10 * 625)
                + (g[3] - b'0') as u64 * 625;
            n += if g.len() == 6 { (g[4] - b'A') as u64 * 25 + (g[5] - b'A') as u64 } else { 24 * 25 + 24 };
            Some(h << 28 | n << 3 | 2)
        }
        [call, dbm] => {
            let slash = call.find('/')?;
            if !(5..=10).contains(&call.len()) || slash < 1 || slash + 1 == call.len() {
                return None;
            }
            let (head, tail) = (&call[..slash], &call[slash + 1..]);
            let digit = |c: u8| A2.iter().position(|&x| x == c).unwrap_or(0) as u32;
            let (base, npfx) = if head.len() <= 3 {
                (tail, head.bytes().fold(0, |a, c| 36 * a + digit(c)))
            } else {
                let t = tail.as_bytes();
                let n = match t.len() {
                    1 => digit(t[0]),
                    2 => 36 * digit(t[0]) + digit(t[1]),
                    3 if t[2].is_ascii_digit() => 360 * digit(t[0]) + 10 * digit(t[1]) + digit(t[2]),
                    _ => return None,
                };
                (head, n + NZZZ)
            };
            let n28 = pack_call(base)? as u64;
            let p = store_power(dbm.parse().ok()?) as u64;
            Some(n28 << 22 | (npfx as u64) << 6 | p << 1 | 1)
        }
        _ => None,
    }
}

/// What the 50 bits say. Calls heard in full are added to `hashes`, and a
/// type 3 hash is looked up there. `None` for a field no station could send.
pub fn unpack(bits: u64, hashes: &mut CallHashes) -> Option<Message> {
    let n28 = (bits >> 22 & 0x0fff_ffff) as u32;
    let (j48, j49, j50) = ((bits >> 2 & 1) as u8, (bits >> 1 & 1) as u8, (bits & 1) as u8);
    if j50 == 1 {
        let npfx = (bits >> 6 & 0xffff) as u32;
        let p = power((bits >> 1 & 0x1f) as u32)?;
        let base = unpack_call(n28)?;
        let call = if npfx < NZZZ {
            let mut pfx = Vec::new();
            let mut n = npfx;
            for _ in 0..3 {
                pfx.insert(0, A2[(n % 36) as usize]);
                n /= 36;
                if n == 0 {
                    break;
                }
            }
            format!("{}/{}", String::from_utf8(pfx).ok()?, base)
        } else {
            let n = npfx - NZZZ;
            let sfx: Vec<u8> = if n <= 35 {
                vec![A2[n as usize]]
            } else if n <= 1295 {
                vec![A2[(n / 36) as usize], A2[(n % 36) as usize]]
            } else if n <= 12959 {
                vec![A2[(n / 360) as usize], A2[(n / 10 % 36) as usize], A2[(n % 10) as usize]]
            } else {
                return None;
            };
            format!("{}/{}", base, String::from_utf8(sfx).ok()?)
        };
        hashes.save(&call);
        return Some(Message { kind: "type2", text: format!("{call} {p}"), call, grid: None, power_dbm: Some(p) });
    }
    if j49 == 0 {
        let p = power((bits >> 2 & 0x1f) as u32)?;
        let grid = grid4_of((bits >> 7 & 0x7fff) as u32)?;
        let call = unpack_call(n28)?;
        hashes.save(&call);
        return Some(Message { kind: "type1", text: format!("{call} {grid} {p}"), call, grid: Some(grid), power_dbm: Some(p) });
    }
    if j48 == 0 {
        let n22 = (bits >> 28 & 0x3f_ffff) as u32;
        let grid = grid6_of((bits >> 3 & 0x1ff_ffff) as u32)?;
        let call = hashes.lookup(n22).map_or_else(|| "<...>".to_string(), |c| format!("<{c}>"));
        return Some(Message { kind: "type3", text: format!("{call} {grid}"), call, grid: Some(grid), power_dbm: None });
    }
    None
}
