//! What a frame's 72 bits say. JS8Call calls this layer its varicode.
//!
//! A frame with the data flag is 72 bits of compressed text. Otherwise the
//! leading bits say what it is: a one, text from before JS8Call 2.2 (then a
//! one for compressed, a zero for Huffman); else three bits of frame type,
//! 000 heartbeat, 001 compound callsign, 010 compound directed, 011
//! directed.

use super::jsc;

/// The 64 characters JS8Call writes a frame's bits with, six bits each.
const ALPHABET72: &[u8; 64] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz-+";
/// Callsigns and grids.
const ALPHANUMERIC: &[u8; 39] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ /@";
/// Base-41 digits of a message checksum.
const ALPHABET41: &[u8; 41] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ+-./?";

/// 28-bit callsigns above this are groups and placeholders.
const NBASECALL: u32 = 37 * 36 * 10 * 27 * 27 * 27;
/// 15-bit grids at or above this are not grids; from 32 410 they are
/// commands.
const NBASEGRID: u16 = 180 * 180;
const NUSERGRID: u16 = NBASEGRID + 10;
const NMAXGRID: u16 = (1 << 15) - 1;

/// The callsign that stands for one sent in a compound frame of its own.
pub const PLACEHOLDER: &str = "<....>";

/// Values above [`NBASECALL`], in order from one.
const GROUPS: [&str; 54] = [
    PLACEHOLDER,
    "@ALLCALL",
    "@JS8NET",
    "@DX/NA",
    "@DX/SA",
    "@DX/EU",
    "@DX/AS",
    "@DX/AF",
    "@DX/OC",
    "@DX/AN",
    "@REGION/1",
    "@REGION/2",
    "@REGION/3",
    "@GROUP/0",
    "@GROUP/1",
    "@GROUP/2",
    "@GROUP/3",
    "@GROUP/4",
    "@GROUP/5",
    "@GROUP/6",
    "@GROUP/7",
    "@GROUP/8",
    "@GROUP/9",
    "@COMMAND",
    "@CONTROL",
    "@NET",
    "@NTS",
    "@RESERVE/0",
    "@RESERVE/1",
    "@RESERVE/2",
    "@RESERVE/3",
    "@RESERVE/4",
    "@APRSIS",
    "@RAGCHEW",
    "@JS8",
    "@EMCOMM",
    "@ARES",
    "@MARS",
    "@AMRRON",
    "@RACES",
    "@RAYNET",
    "@RADAR",
    "@SKYWARN",
    "@CQ",
    "@HB",
    "@QSO",
    "@QSOPARTY",
    "@CONTEST",
    "@FIELDDAY",
    "@SOTA",
    "@IOTA",
    "@POTA",
    "@QRP",
    "@QRO",
];

/// The 32 directed commands, by their five-bit code, as JS8Call prints them
/// (with the space that separates them from the callsign before).
pub const COMMANDS: [&str; 32] = [
    " SNR?",
    " DIT DIT",
    " NACK",
    " HEARING?",
    " GRID?",
    ">",
    " STATUS?",
    " STATUS",
    " HEARING",
    " MSG",
    " MSG TO:",
    " QUERY",
    " QUERY MSGS",
    " QUERY CALL",
    " ACK",
    " GRID",
    " INFO?",
    " INFO",
    " FB",
    " HW CPY?",
    " SK",
    " RR",
    " QSL?",
    " QSL",
    " CMD",
    " SNR",
    " NO",
    " YES",
    " 73",
    " HEARTBEAT SNR",
    " AGN?",
    " ",
];
pub const CMD_SNR: u8 = 25;
pub const CMD_HEARTBEAT_SNR: u8 = 29;

/// What follows a directed command and whether a checksum closes it: 16
/// bits as three base-41 characters for the commands that carry a message
/// on, none for the rest.
pub fn checksummed(cmd: u8) -> bool {
    matches!(cmd, 5 | 9 | 10 | 11 | 12 | 13 | 24)
}

/// The CQ a heartbeat frame with the alternate bit set calls.
const CQS: [&str; 8] = ["CQ CQ CQ", "CQ DX", "CQ QRP", "CQ CONTEST", "CQ FIELD", "CQ FD", "CQ CQ", "CQ"];

/// What one frame carries.
#[derive(Clone, Debug, PartialEq)]
pub enum FrameContent {
    /// A heartbeat ("@HB HEARTBEAT") or, with `cq` set, a CQ to @ALLCALL,
    /// from a callsign of up to eleven characters.
    Heartbeat { from: String, grid: Option<String>, cq: Option<&'static str> },
    /// A callsign the 28-bit packing cannot hold, sent ahead of the
    /// directed frame that refers to it as `<....>`, with a grid if known.
    Compound { call: String, grid: Option<String> },
    /// A command to a compound callsign: `to` is this frame's call, the
    /// sender came in the compound frame before.
    CompoundDirected { to: String, cmd: u8, num: Option<i32> },
    /// A command from one station to another or to a group.
    Directed { from: String, to: String, cmd: u8, num: Option<i32> },
    /// Text. `compressed` when it came through the dictionary rather than
    /// the Huffman table.
    Data { text: String, compressed: bool },
}

impl FrameContent {
    /// The frame type JS8Call names it by.
    pub fn frame_type(&self) -> &'static str {
        match self {
            FrameContent::Heartbeat { .. } => "heartbeat",
            FrameContent::Compound { .. } => "compound",
            FrameContent::CompoundDirected { .. } => "compoundDirected",
            FrameContent::Directed { .. } => "directed",
            FrameContent::Data { .. } => "data",
        }
    }

    /// The frame as JS8Call shows it in its band activity.
    pub fn text(&self) -> String {
        match self {
            FrameContent::Heartbeat { from, grid, cq } => {
                let g = grid.as_deref().map(|g| format!(" {g}")).unwrap_or_default();
                match cq {
                    Some(cq) => format!("{from}: @ALLCALL {cq}{g}"),
                    None => format!("{from}: @HB HEARTBEAT{g}"),
                }
            }
            FrameContent::Compound { call, .. } => format!("{call}: "),
            FrameContent::CompoundDirected { to, cmd, num } => format!("{to}{}", command_text(*cmd, *num)),
            FrameContent::Directed { from, to, cmd, num } => format!("{from}: {to}{}", command_text(*cmd, *num)),
            FrameContent::Data { text, .. } => text.clone(),
        }
    }
}

/// A command and its number as JS8Call prints them: " SNR -12", " ACK".
pub fn command_text(cmd: u8, num: Option<i32>) -> String {
    let c = COMMANDS[cmd as usize & 31];
    match num {
        Some(n) if is_snr(cmd) => format!("{c} {}", format_snr(n)),
        Some(n) => format!("{c} {n}"),
        None => c.to_string(),
    }
}

/// Whether a callsign as unpacked could be one: a group or the
/// placeholder, or slash-separated parts of which one is a base call (one
/// to three characters with a letter among them, a digit, one to four
/// letters) and the rest short prefixes or suffixes.
///
/// The packing accepts any eleven characters, and JS8Call acknowledges
/// heartbeats automatically, so a station whose software falsely decoded a
/// heartbeat sends a valid, checked frame addressed to a call that never
/// existed. Such frames are reported as decoded, flagged by this.
pub fn plausible_call(call: &str) -> bool {
    if call.starts_with('@') || call == PLACEHOLDER {
        return GROUPS.contains(&call) || (call.len() > 1 && call.len() <= 11);
    }
    let base = |p: &str| {
        let b = p.as_bytes();
        let Some(d) = b.iter().rposition(u8::is_ascii_digit) else { return false };
        (1..=3).contains(&d)
            && (1..=4).contains(&(b.len() - d - 1))
            && b[d + 1..].iter().all(u8::is_ascii_uppercase)
            && b[..d].iter().all(u8::is_ascii_alphanumeric)
            && b[..d].iter().any(u8::is_ascii_uppercase)
    };
    let parts: Vec<&str> = call.split('/').collect();
    parts.len() <= 3
        && parts.iter().filter(|p| base(p)).count() == 1
        && parts.iter().all(|p| (1..=7).contains(&p.len()) && p.bytes().all(|c| c.is_ascii_alphanumeric()))
}

impl FrameContent {
    /// The callsigns this frame names that cannot be real ones.
    pub fn implausible_calls(&self) -> Vec<String> {
        let calls: Vec<&String> = match self {
            FrameContent::Heartbeat { from, .. } => vec![from],
            FrameContent::Compound { call, .. } => vec![call],
            FrameContent::CompoundDirected { to, .. } => vec![to],
            FrameContent::Directed { from, to, .. } => vec![from, to],
            FrameContent::Data { .. } => vec![],
        };
        calls.into_iter().filter(|c| !plausible_call(c)).cloned().collect()
    }
}

pub fn is_snr(cmd: u8) -> bool {
    cmd == CMD_SNR || cmd == CMD_HEARTBEAT_SNR
}

/// "+05", "-12", as JS8Call writes a report.
pub fn format_snr(n: i32) -> String {
    if n >= 0 { format!("+{n:02}") } else { format!("-{:02}", -n) }
}

fn bit(p: &[u8; 9], i: usize) -> u64 {
    ((p[i / 8] >> (7 - i % 8)) & 1) as u64
}

/// `n` bits from bit `at`, most significant first.
fn bits(p: &[u8; 9], at: usize, n: usize) -> u64 {
    (at..at + n).fold(0, |v, i| (v << 1) | bit(p, i))
}

/// The 72 bits written as JS8Call writes a frame.
pub fn raw_text(p: &[u8; 9]) -> String {
    (0..12).map(|i| ALPHABET72[bits(p, 6 * i, 6) as usize] as char).collect()
}

/// A 28-bit callsign: a group or placeholder above the base range, else
/// six characters packed as in JT65, with JS8Call's two exceptions undone.
fn unpack_callsign(v: u32, portable: bool) -> String {
    if v > NBASECALL {
        return GROUPS.get((v - NBASECALL - 1) as usize).map_or_else(|| format!("<{v}>"), |g| g.to_string());
    }
    let mut v = v;
    let mut w = [b' '; 6];
    for i in (3..6).rev() {
        w[i] = ALPHANUMERIC[(v % 27 + 10) as usize];
        v /= 27;
    }
    w[2] = ALPHANUMERIC[(v % 10) as usize];
    v /= 10;
    w[1] = ALPHANUMERIC[(v % 36) as usize];
    v /= 36;
    w[0] = ALPHANUMERIC[(v as usize).min(38)];
    let mut call = String::from_utf8_lossy(&w).to_string();
    if let Some(rest) = call.strip_prefix("3D0") {
        call = format!("3DA0{rest}");
    }
    let b = call.as_bytes();
    if b[0] == b'Q' && b[1].is_ascii_uppercase() {
        call = format!("3X{}", &call[1..]);
    }
    let call = call.trim().to_string();
    if portable { format!("{call}/P") } else { call }
}

/// A callsign of up to eleven characters, with a slash allowed after the
/// third and the seventh, in 50 bits.
fn unpack_alphanumeric50(v: u64) -> String {
    let mut v = v;
    let mut w = [b' '; 11];
    for i in (0..11).rev() {
        match i {
            3 | 7 => {
                w[i] = if v % 2 == 1 { b'/' } else { b' ' };
                v /= 2;
            }
            0 => w[0] = ALPHANUMERIC[(v % 39) as usize],
            _ => {
                w[i] = ALPHANUMERIC[(v % 38) as usize];
                v /= 38;
            }
        }
    }
    w.iter().filter(|&&c| c != b' ').map(|&c| c as char).collect()
}

/// A four-character Maidenhead square from 15 bits: two-degree columns of
/// 180 one-degree rows.
pub fn unpack_grid(v: u16) -> Option<String> {
    if v >= NBASEGRID {
        return None;
    }
    let (q, r) = ((v / 180) as u32, (v % 180) as u32);
    // Five-minute steps of longitude west from 180°, 2.5-minute steps of
    // latitude north from the pole, as JS8Call's deg2grid counts them.
    let nlong = 12 * (358 - 2 * q);
    let nlat = 24 * r;
    let g = [b'A' + (nlong / 240) as u8, b'A' + (nlat / 240) as u8, b'0' + (nlong % 240 / 24) as u8, b'0' + (nlat % 240 / 24) as u8];
    g.iter().all(|c| c.is_ascii_alphanumeric()).then(|| String::from_utf8_lossy(&g).to_string())
}

/// The [3 type][50 call][16 extra][3 more] layout heartbeat and compound
/// frames share.
fn compound_fields(p: &[u8; 9]) -> (u8, String, u16, u8) {
    let ty = bits(p, 0, 3) as u8;
    let call = unpack_alphanumeric50(bits(p, 3, 50));
    let extra = ((bits(p, 53, 11) << 5) | bits(p, 64, 5)) as u16;
    (ty, call, extra, bits(p, 69, 3) as u8)
}

/// Unpack a frame. Every 72-bit pattern means something, so this cannot
/// fail; whether it was sent is what the CRC said.
pub fn unpack(p: &[u8; 9], flags: u8) -> FrameContent {
    if flags & 4 != 0 {
        // Fast data: all 72 bits, padded with a zero and then ones.
        let all: Vec<bool> = (0..72).map(|i| bit(p, i) == 1).collect();
        let end = all.iter().rposition(|b| !b).unwrap_or(72);
        return FrameContent::Data { text: jsc::decompress(&all[..end]), compressed: true };
    }
    if bit(p, 0) == 1 {
        // The pre-2.2 data frame: a one, then one for compressed, then the
        // bits, padded the same way.
        let rest: Vec<bool> = (1..72).map(|i| bit(p, i) == 1).collect();
        let compressed = rest[0];
        let end = rest.iter().rposition(|b| !b).unwrap_or(rest.len());
        let body = if end >= 1 { &rest[1..end] } else { &rest[1..] };
        let text = if compressed { jsc::decompress(body) } else { huffman(body) };
        return FrameContent::Data { text, compressed };
    }
    match bits(p, 0, 3) {
        3 => {
            let extra = bits(p, 64, 8) as u8;
            let from = unpack_callsign(bits(p, 3, 28) as u32, extra & 0x80 != 0);
            let to = unpack_callsign(bits(p, 31, 28) as u32, extra & 0x40 != 0);
            let cmd = bits(p, 59, 5) as u8;
            let n = (extra & 0x3f) as i32;
            FrameContent::Directed { from, to, cmd, num: (n != 0).then_some(n - 31) }
        }
        _ => {
            let (ty, call, extra, more) = compound_fields(p);
            match ty {
                0 => {
                    let cq = (extra & 0x8000 != 0).then(|| CQS[more as usize]);
                    FrameContent::Heartbeat { from: call, grid: unpack_grid(extra & 0x7fff), cq }
                }
                _ => {
                    if (NUSERGRID..NMAXGRID).contains(&extra) {
                        // A command where a grid would be; the number only
                        // survives for an SNR.
                        let v = (extra - NUSERGRID) as u8;
                        let (cmd, num) = if v & 0x80 != 0 {
                            (if v & 0x40 != 0 { CMD_HEARTBEAT_SNR } else { CMD_SNR }, Some((v & 0x3f) as i32 - 31))
                        } else {
                            (v & 0x1f, None)
                        };
                        if ty == 2 {
                            return FrameContent::CompoundDirected { to: call, cmd, num };
                        }
                        // A plain compound frame does not carry commands;
                        // JS8Call would print the command after the call.
                        return FrameContent::Compound { call, grid: None };
                    }
                    let grid = if extra <= NBASEGRID { unpack_grid(extra) } else { None };
                    if ty == 2 {
                        // Directed without a command: what JS8Call shows
                        // as the call alone.
                        return FrameContent::CompoundDirected { to: call, cmd: 31, num: None };
                    }
                    FrameContent::Compound { call, grid }
                }
            }
        }
    }
}

/// The Huffman code of the first data frames. Codes are prefix-free, so
/// reading on until none matches is the whole decoder.
const HUFFMAN: [(char, &str); 44] = [
    (' ', "01"),
    ('E', "100"),
    ('T', "1101"),
    ('A', "0011"),
    ('O', "11111"),
    ('I', "11100"),
    ('N', "10111"),
    ('S', "10100"),
    ('H', "00011"),
    ('R', "00000"),
    ('D', "111011"),
    ('L', "110011"),
    ('C', "110001"),
    ('U', "101101"),
    ('M', "101011"),
    ('W', "001011"),
    ('F', "001001"),
    ('G', "000101"),
    ('Y', "000011"),
    ('P', "1111011"),
    ('B', "1111001"),
    ('.', "1110100"),
    ('V', "1100101"),
    ('K', "1100100"),
    ('-', "1100001"),
    ('+', "1100000"),
    ('?', "1011001"),
    ('!', "1011000"),
    ('"', "1010101"),
    ('X', "1010100"),
    ('0', "0010101"),
    ('J', "0010100"),
    ('1', "0010001"),
    ('Q', "0010000"),
    ('2', "0001001"),
    ('Z', "0001000"),
    ('3', "0000101"),
    ('5', "0000100"),
    ('4', "11110101"),
    ('9', "11110100"),
    ('8', "11110001"),
    ('6', "11110000"),
    ('7', "11101011"),
    ('/', "11101010"),
];

fn huffman(bits: &[bool]) -> String {
    let mut out = String::new();
    let mut i = 0;
    'next: while i < bits.len() {
        for (c, code) in HUFFMAN {
            let n = code.len();
            if i + n <= bits.len() && code.bytes().zip(&bits[i..i + n]).all(|(b, &v)| (b == b'1') == v) {
                out.push(c);
                i += n;
                continue 'next;
            }
        }
        break;
    }
    out
}

/// CRC-16/KERMIT of the text's bytes, as three base-41 characters: the
/// checksum JS8Call appends to a relayed or stored message.
pub fn checksum16(text: &str) -> String {
    let mut crc: u16 = 0;
    for b in text.bytes() {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x8408 } else { crc >> 1 };
        }
    }
    let c = crc as usize;
    [c / 1681, c / 41 % 41, c % 41].iter().map(|&i| ALPHABET41[i] as char).collect()
}

/// For the test encoder: the inverse of [`unpack_callsign`] for plain
/// callsigns and groups.
#[cfg(test)]
pub fn pack_callsign(call: &str) -> Option<(u32, bool)> {
    if let Some(i) = GROUPS.iter().position(|g| *g == call) {
        return Some((NBASECALL + 1 + i as u32, false));
    }
    let (call, portable) = match call.strip_suffix("/P") {
        Some(c) => (c, true),
        None => (call, false),
    };
    // Swaziland's and Guinea's prefixes, folded the way JS8Call folds them.
    let call = match call.strip_prefix("3DA0") {
        Some(rest) => format!("3D0{rest}"),
        None if call.starts_with("3X") && call.as_bytes().get(2).is_some_and(u8::is_ascii_uppercase) => format!("Q{}", &call[2..]),
        None => call.to_string(),
    };
    let b = call.as_bytes();
    // The digit goes third: pad a leading space when it is second.
    let padded = if b.len() >= 2 && b[1].is_ascii_digit() { format!(" {call}") } else { call.to_string() };
    let w: Vec<u8> = format!("{padded:<6}").bytes().collect();
    if w.len() != 6 || !w[2].is_ascii_digit() {
        return None;
    }
    let idx = |c: u8| ALPHANUMERIC.iter().position(|&a| a == c).map(|v| v as u32);
    let mut v = idx(w[0])?;
    v = 36 * v + idx(w[1])?;
    v = 10 * v + idx(w[2])?;
    for &c in &w[3..] {
        v = 27 * v + idx(c)? - 10;
    }
    Some((v, portable))
}

/// For the test encoder: the 50-bit long callsign.
#[cfg(test)]
pub fn pack_alphanumeric50(call: &str) -> u64 {
    let mut w: Vec<u8> = call.bytes().collect();
    if w.len() > 3 && w[3] != b'/' {
        w.insert(3, b' ');
    }
    if w.len() > 7 && w[7] != b'/' {
        w.insert(7, b' ');
    }
    w.resize(11, b' ');
    let idx = |c: u8| ALPHANUMERIC.iter().position(|&a| a == c).unwrap_or(36) as u64;
    let mut v = idx(w[0]);
    for (i, &c) in w.iter().enumerate().skip(1) {
        v = match i {
            3 | 7 => 2 * v + u64::from(c == b'/'),
            _ => 38 * v + idx(c),
        };
    }
    v
}

/// For the test encoder: a four-character square in 15 bits, as JS8Call's
/// packGrid computes it from the square's center.
#[cfg(test)]
pub fn pack_grid(grid: &str) -> u16 {
    let g = grid.as_bytes();
    let dlong = 180.0 - 20.0 * (g[0] - b'A') as f64 - 2.0 * (g[2] - b'0') as f64 - 62.5 / 60.0;
    let dlat = -90.0 + 10.0 * (g[1] - b'A') as f64 + (g[3] - b'0') as f64 + 31.25 / 60.0;
    let (ilong, ilat) = (dlong as i32, (dlat + 90.0) as i32);
    (((ilong + 180) / 2) * 180 + ilat) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_raw(raw: &str) -> [u8; 9] {
        let mut v: u128 = 0;
        for c in raw.bytes() {
            v = (v << 6) | ALPHABET72.iter().position(|&a| a == c).unwrap() as u128;
        }
        let mut p = [0u8; 9];
        for (i, b) in p.iter_mut().enumerate() {
            *b = (v >> (64 - 8 * i)) as u8;
        }
        p
    }

    #[test]
    fn a_directed_frame_from_js8calls_own_self_test() {
        // JS8Call checks this frame decodes to a message from KN4CRD to K0OG.
        let p = from_raw("SN5-lUuJkby0");
        assert_eq!(raw_text(&p), "SN5-lUuJkby0");
        let c = unpack(&p, 1);
        let text = c.text();
        assert!(text.starts_with("KN4CRD: K0OG"), "{text} ({c:?})");
    }

    #[test]
    fn grids_and_calls_round_trip() {
        for g in ["EM73", "FN42", "JO65", "AA00", "RR99", "IO91", "PM95", "GG66"] {
            assert_eq!(unpack_grid(pack_grid(g)).as_deref(), Some(g), "{g}");
        }
        for c in ["KN4CRD", "K0OG", "W1AW", "G4ABC", "VK2DEF", "3DA0RU", "KB1ABC/P", "@ALLCALL", "@HB"] {
            let (v, p) = pack_callsign(c).unwrap();
            assert_eq!(unpack_callsign(v, p), c);
        }
        for c in ["VE7/KN4CRD", "KN4CRD/P", "@GROUP/4", "PJ4/K1ABC"] {
            assert_eq!(unpack_alphanumeric50(pack_alphanumeric50(c)), c);
        }
    }

    #[test]
    fn huffman_and_checksum() {
        // "HI" = H 00011, I 11100, then the zero-and-ones pad is cut first.
        let bits: Vec<bool> = "0001111100".bytes().map(|b| b == b'1').collect();
        assert_eq!(huffman(&bits), "HI");
        // CRC-16/KERMIT's check value, 0x2189, in base 41.
        let c = 0x2189usize;
        let want: String = [c / 1681, c / 41 % 41, c % 41].iter().map(|&i| ALPHABET41[i] as char).collect();
        assert_eq!(checksum16("123456789"), want);
    }
}
