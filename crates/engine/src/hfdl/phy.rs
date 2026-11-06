//! The HFDL signal in space, ARINC 635 section 4.2.3 and Attachment 2-2:
//! the known sequences, the eight data rate and interleaver settings, the
//! interleaver, the chip to phase mapping and the scrambling pattern.

/// 1800 symbols a second, ±10 ppm at the transmitter.
pub const SYMBOL_RATE: f64 = 1800.0;
/// The carrier sits this far above the assigned (dial) frequency.
pub const CARRIER_HZ: f64 = 1440.0;
/// Root raised cosine roll-off.
pub const ROLLOFF: f32 = 0.31;

/// The prekey: 448 symbols of constant 180 degree phase, a plain tone that
/// lets the transmitter reach power and the receiver's AGC settle. Only the
/// test transmitter needs it: the receiver finds a burst by its A sequences.
#[cfg(test)]
pub const PREKEY_LEN: usize = 448;
pub const A_LEN: usize = 127;
pub const M1_LEN: usize = 127;
pub const M2_LEN: usize = 15;
pub const T_LEN: usize = 15;
pub const T_REPEATS: usize = 9;
/// A, A, M1, M2 and nine T: 531 symbols.
pub const PREAMBLE_LEN: usize = 2 * A_LEN + M1_LEN + M2_LEN + T_REPEATS * T_LEN;
/// Where M1 starts, counted from the first symbol of the first A.
pub const M1_START: usize = 2 * A_LEN;
/// Data frames: 30 unknown symbols then a 15 symbol probe (the T sequence).
pub const DATA_LEN: usize = 30;
pub const FRAME_LEN: usize = DATA_LEN + T_LEN;
/// The scrambling pattern repeats every 120 data symbols.
pub const SCRAMBLE_LEN: usize = 120;
/// Interleaver rows.
pub const ROWS: usize = 40;

/// The A sequence, left-most bit first (Attachment 2-2).
const A_BITS: &str = "010 1101 1101 1110 0011 1010 0010 1011 1000 0001 1110 1100 1100 0100 1001 1100 \
                      1111 1001 0000 0100 0110 1010 1001 1011 0100 1010 0001 0110 0001 1001 0111 1111";
/// The sequence whose cyclic shifts make M1 (and whose first 15 symbols, at
/// the same shift, make M2).
const M_BITS: &str = "011 1011 0111 1010 0010 1100 1011 1110 0010 0000 0110 0110 1100 0111 0011 1010 \
                      1110 0001 0011 0000 0101 0101 1010 0100 1010 0111 1001 0001 1010 1000 0111 1111";
/// The probe and training sequence T.
const T_BITS: &str = "000 100 110 101 111";
/// The 120 bit scrambling pattern, most significant bit of the left-most
/// digit first (section 4.2.3.2.2.4).
const SCRAMBLE_HEX: &str = "131BC4250F8C15EFCD6AEC996E2368";

/// One of the eight settings M1 announces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub rate_bps: u16,
    /// The 4.2 second interleaver, which fills two slots; otherwise the 1.8
    /// second one in a single slot.
    pub long: bool,
    /// Coded chips per data symbol: 1 (2-PSK), 2 (4-PSK) or 3 (8-PSK).
    pub bits_per_symbol: usize,
    /// 2 at 300 bps, where every chip of the rate 1/2 code is sent twice.
    pub repeat: usize,
    /// The M1 shift that announces this setting.
    pub m1_shift: usize,
}

pub const MODES: [Mode; 8] = [
    Mode {
        rate_bps: 300,
        long: false,
        bits_per_symbol: 1,
        repeat: 2,
        m1_shift: 72,
    },
    Mode {
        rate_bps: 600,
        long: false,
        bits_per_symbol: 1,
        repeat: 1,
        m1_shift: 82,
    },
    Mode {
        rate_bps: 1200,
        long: false,
        bits_per_symbol: 2,
        repeat: 1,
        m1_shift: 113,
    },
    Mode {
        rate_bps: 1800,
        long: false,
        bits_per_symbol: 3,
        repeat: 1,
        m1_shift: 123,
    },
    Mode {
        rate_bps: 300,
        long: true,
        bits_per_symbol: 1,
        repeat: 2,
        m1_shift: 61,
    },
    Mode {
        rate_bps: 600,
        long: true,
        bits_per_symbol: 1,
        repeat: 1,
        m1_shift: 103,
    },
    Mode {
        rate_bps: 1200,
        long: true,
        bits_per_symbol: 2,
        repeat: 1,
        m1_shift: 93,
    },
    Mode {
        rate_bps: 1800,
        long: true,
        bits_per_symbol: 3,
        repeat: 1,
        m1_shift: 9,
    },
];

impl Mode {
    /// Data frames: 72 (1.8 s of data) or 168 (4.2 s).
    pub fn frames(&self) -> usize {
        if self.long { 168 } else { 72 }
    }
    pub fn data_symbols(&self) -> usize {
        self.frames() * DATA_LEN
    }
    /// Symbols from the first A to the last probe.
    pub fn burst_symbols(&self) -> usize {
        PREAMBLE_LEN + self.frames() * FRAME_LEN
    }
    pub fn chips(&self) -> usize {
        self.data_symbols() * self.bits_per_symbol
    }
    pub fn columns(&self) -> usize {
        self.chips() / ROWS
    }
    /// User plus flush bits into the encoder.
    #[cfg(test)]
    pub fn info_bits(&self) -> usize {
        self.chips() / (2 * self.repeat)
    }
    pub fn column_step(&self) -> usize {
        if self.long { 23 } else { 17 }
    }
}

fn parse_bits(s: &str) -> Vec<u8> {
    s.bytes()
        .filter(|b| *b == b'0' || *b == b'1')
        .map(|b| b - b'0')
        .collect()
}

/// A bit as a 2-PSK symbol: 0 is 0 degrees, 1 is 180.
fn bpsk(b: u8) -> f32 {
    if b == 0 { 1.0 } else { -1.0 }
}

pub struct Tables {
    pub a: Vec<f32>,
    /// M1 followed by M2 (142 symbols) for each mode.
    pub m: Vec<Vec<f32>>,
    pub t: Vec<f32>,
    /// 1 where a data symbol is turned by 180 degrees.
    pub scramble: Vec<u8>,
}

impl Tables {
    pub fn new() -> Self {
        let a: Vec<f32> = parse_bits(A_BITS).into_iter().map(bpsk).collect();
        let base = parse_bits(M_BITS);
        debug_assert_eq!(a.len(), A_LEN);
        debug_assert_eq!(base.len(), M1_LEN);
        let m = MODES
            .iter()
            .map(|mode| {
                (0..M1_LEN + M2_LEN)
                    .map(|j| bpsk(base[(mode.m1_shift + j % M1_LEN) % M1_LEN]))
                    .collect()
            })
            .collect();
        let t = parse_bits(T_BITS).into_iter().map(bpsk).collect();
        let scramble = SCRAMBLE_HEX
            .chars()
            .flat_map(|c| {
                let v = c.to_digit(16).unwrap_or(0) as u8;
                (0..4).rev().map(move |k| (v >> k) & 1)
            })
            .collect();
        Self { a, m, t, scramble }
    }

    /// The known symbol at burst position `k` (0 = the first symbol of the
    /// first A), or `None` for a data symbol.
    pub fn known(&self, mode: usize, k: usize) -> Option<f32> {
        if k < M1_START {
            Some(self.a[k % A_LEN])
        } else if k < M1_START + M1_LEN + M2_LEN {
            Some(self.m[mode][k - M1_START])
        } else if k < PREAMBLE_LEN {
            Some(self.t[(k - M1_START - M1_LEN - M2_LEN) % T_LEN])
        } else {
            let r = (k - PREAMBLE_LEN) % FRAME_LEN;
            (r >= DATA_LEN).then(|| self.t[r - DATA_LEN])
        }
    }
}

/// For each chip as sent, the index of the coded chip it carries.
///
/// The transmitter writes coded chips down the columns of a 40 row matrix,
/// each one 9 rows below the last (modulo 40), and reads them out a row
/// further down and 17 (or 23) columns to the left each time, one column
/// to the right at each wrap of the row. So chip `j` sent comes from row
/// `j mod 40`, column `(j / 40 - step·j) mod N`, and that cell was written
/// by coded chip `40·column + (9·row mod 40)`, since 9 is its own inverse
/// modulo 40.
pub fn interleave_map(mode: &Mode) -> Vec<u32> {
    let n = mode.columns() as i64;
    let step = mode.column_step() as i64;
    (0..mode.chips() as i64)
        .map(|j| {
            let row = j % ROWS as i64;
            let col = (j / ROWS as i64 - step * j).rem_euclid(n);
            (ROWS as i64 * col + (9 * row) % ROWS as i64) as u32
        })
        .collect()
}

/// The phase index (in units of 360/M degrees) of a Gray coded label, first
/// chip in the most significant bit: 8-PSK 000 is 0, 001 is 45, 011 is 90,
/// 010 is 135, 110 is 180 and so on (Table 4-7).
#[cfg(test)]
pub fn gray_to_index(label: u8) -> u8 {
    let mut v = label;
    let mut s = label >> 1;
    while s != 0 {
        v ^= s;
        s >>= 1;
    }
    v
}

pub fn index_to_gray(k: u8) -> u8 {
    k ^ (k >> 1)
}

/// Root raised cosine pulse at `t` symbols from its center.
pub fn rrc(t: f32) -> f32 {
    let b = ROLLOFF;
    let pi = std::f32::consts::PI;
    if t.abs() < 1e-6 {
        return 1.0 - b + 4.0 * b / pi;
    }
    let edge = 1.0 / (4.0 * b);
    if (t.abs() - edge).abs() < 1e-5 {
        return b / 2f32.sqrt()
            * ((1.0 + 2.0 / pi) * (pi / (4.0 * b)).sin()
                + (1.0 - 2.0 / pi) * (pi / (4.0 * b)).cos());
    }
    let num = (pi * t * (1.0 - b)).sin() + 4.0 * b * t * (pi * t * (1.0 + b)).cos();
    let den = pi * t * (1.0 - (4.0 * b * t) * (4.0 * b * t));
    num / den
}
