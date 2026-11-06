//! The numbers of ETSI ES 201 980 (V4.2.1) that the DRM30 receiver is built
//! on, transcribed from the standard's tables: the OFDM parameters (table
//! 47), the carrier ranges and unused carriers (tables 49, 50), the
//! frequency, time and gain references (tables 51 to 60), the FAC cell
//! positions (tables 62 to 65), the puncturing patterns (tables 27, 28) and
//! the code rate combinations (tables 30, 32, 36, 37, 39).
//!
//! Every count derived from them (the MSC, SDC and FAC cells of each mode
//! and occupancy, the bits per multiplex frame and SDC block) is checked in
//! the tests against the standard's own tables 21, 25, 41 to 44 and annex
//! J, which a mistake in any position or phase table would upset.

/// The receiver works at 48 kHz internally, four times the standard's
/// elementary rate of 12 kHz, so every OFDM length is a whole number of
/// samples and the widest occupancy (20 kHz, reaching 14.6 kHz above the
/// reference frequency) fits.
pub const FS: f64 = 48_000.0;

/// Robustness mode. Mode E (DRM+, VHF) is not part of DRM30 and is left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    A,
    B,
    C,
    D,
}

pub const MODES: [Mode; 4] = [Mode::A, Mode::B, Mode::C, Mode::D];

impl Mode {
    pub fn index(self) -> usize {
        match self {
            Mode::A => 0,
            Mode::B => 1,
            Mode::C => 2,
            Mode::D => 3,
        }
    }

    pub fn letter(self) -> char {
        (b'A' + self.index() as u8) as char
    }

    /// Useful part Tu in samples at [`FS`] (24, 21.33, 14.66, 9.33 ms).
    pub fn tu(self) -> usize {
        [1152, 1024, 704, 448][self.index()]
    }

    /// Guard interval Tg in samples at [`FS`] (Tg/Tu = 1/9, 1/4, 4/11, 11/14).
    pub fn tg(self) -> usize {
        [128, 256, 256, 352][self.index()]
    }

    pub fn ts(self) -> usize {
        self.tu() + self.tg()
    }

    /// OFDM symbols in a 400 ms transmission frame.
    pub fn symbols_per_frame(self) -> usize {
        [15, 15, 20, 24][self.index()]
    }

    /// Carrier spacing 1/Tu in Hz.
    pub fn spacing_hz(self) -> f64 {
        FS / self.tu() as f64
    }

    /// The gain reference grid (table 60): carriers k = k0 + x (s mod y) +
    /// x y p carry gain references in symbol s.
    pub fn grid(self) -> (i32, i32, i32) {
        // (x, y, k0)
        [(4, 5, 2), (2, 3, 1), (2, 2, 1), (1, 3, 1)][self.index()]
    }

    /// Symbols at the start of the super frame that carry the SDC.
    pub fn sdc_symbols(self) -> usize {
        match self {
            Mode::A | Mode::B => 2,
            Mode::C | Mode::D => 3,
        }
    }

    /// Carriers around DC that carry nothing (table 50).
    pub fn unused(self, k: i32) -> bool {
        match self {
            Mode::A => (-1..=1).contains(&k),
            _ => k == 0,
        }
    }

    /// Spectrum occupancies defined for this mode.
    pub fn occupancies(self) -> &'static [u8] {
        match self {
            Mode::A | Mode::B => &[0, 1, 2, 3, 4, 5],
            Mode::C | Mode::D => &[3, 5],
        }
    }
}

/// Table 49: (Kmin, Kmax) for a mode and spectrum occupancy, `None` where
/// the standard leaves the combination undefined.
pub fn k_range(mode: Mode, so: u8) -> Option<(i32, i32)> {
    const KMIN: [[i32; 4]; 6] = [
        [2, 1, 0, 0],
        [2, 1, 0, 0],
        [-102, -91, 0, 0],
        [-114, -103, -69, -44],
        [-98, -87, 0, 0],
        [-110, -99, -67, -43],
    ];
    const KMAX: [[i32; 4]; 6] = [
        [102, 91, 0, 0],
        [114, 103, 0, 0],
        [102, 91, 0, 0],
        [114, 103, 69, 44],
        [314, 279, 0, 0],
        [350, 311, 213, 135],
    ];
    let so = so as usize;
    if so > 5 {
        return None;
    }
    let (lo, hi) = (KMIN[so][mode.index()], KMAX[so][mode.index()]);
    if lo == 0 && hi == 0 { None } else { Some((lo, hi)) }
}

/// Nominal channel bandwidth of a spectrum occupancy, Hz (table 48).
pub fn bandwidth_hz(so: u8) -> f64 {
    [4_500.0, 5_000.0, 9_000.0, 10_000.0, 18_000.0, 20_000.0][so.min(5) as usize]
}

/// Table 59: the gain references boosted to a power of 4 at the band edges.
pub fn boosted(mode: Mode, so: u8) -> [i32; 4] {
    const A: [[i32; 4]; 6] = [
        [2, 6, 98, 102],
        [2, 6, 110, 114],
        [-102, -98, 98, 102],
        [-114, -110, 110, 114],
        [-98, -94, 310, 314],
        [-110, -106, 346, 350],
    ];
    const B: [[i32; 4]; 6] = [
        [1, 3, 89, 91],
        [1, 3, 101, 103],
        [-91, -89, 89, 91],
        [-103, -101, 101, 103],
        [-87, -85, 277, 279],
        [-99, -97, 309, 311],
    ];
    let so = so.min(5) as usize;
    match mode {
        Mode::A => A[so],
        Mode::B => B[so],
        Mode::C => {
            if so == 5 {
                [-67, -65, 211, 213]
            } else {
                [-69, -67, 67, 69]
            }
        }
        Mode::D => {
            if so == 5 {
                [-43, -42, 134, 135]
            } else {
                [-44, -43, 43, 44]
            }
        }
    }
}

/// Tables 51 and 52: the three frequency references (carrier, phase in
/// 1024ths of a cycle). They are in every symbol; in mode D the first two
/// turn over (+512) on odd symbols.
pub fn freq_pilots(mode: Mode) -> [(i32, u16); 3] {
    match mode {
        Mode::A => [(18, 205), (54, 836), (72, 215)],
        Mode::B => [(16, 331), (48, 651), (64, 555)],
        Mode::C => [(11, 214), (33, 392), (44, 242)],
        Mode::D => [(7, 788), (21, 1014), (28, 332)],
    }
}

/// Tables 53 to 56: the time references of the first symbol of each frame
/// (carrier, phase in 1024ths).
pub fn time_pilots(mode: Mode) -> &'static [(i32, u16)] {
    const A: [(i32, u16); 21] = [
        (17, 973),
        (18, 205),
        (19, 717),
        (21, 264),
        (28, 357),
        (29, 357),
        (32, 952),
        (33, 440),
        (39, 856),
        (40, 88),
        (41, 88),
        (53, 68),
        (54, 836),
        (55, 836),
        (56, 836),
        (60, 1008),
        (61, 1008),
        (63, 752),
        (71, 215),
        (72, 215),
        (73, 727),
    ];
    const B: [(i32, u16); 19] = [
        (14, 304),
        (16, 331),
        (18, 108),
        (20, 620),
        (24, 192),
        (26, 704),
        (32, 44),
        (36, 432),
        (42, 588),
        (44, 844),
        (48, 651),
        (49, 651),
        (50, 651),
        (54, 460),
        (56, 460),
        (62, 944),
        (64, 555),
        (66, 940),
        (68, 428),
    ];
    const C: [(i32, u16); 19] = [
        (8, 722),
        (10, 466),
        (11, 214),
        (12, 214),
        (14, 479),
        (16, 516),
        (18, 260),
        (22, 577),
        (24, 662),
        (28, 3),
        (30, 771),
        (32, 392),
        (33, 392),
        (36, 37),
        (38, 37),
        (42, 474),
        (44, 242),
        (45, 242),
        (46, 754),
    ];
    const D: [(i32, u16); 21] = [
        (5, 636),
        (6, 124),
        (7, 788),
        (8, 788),
        (9, 200),
        (11, 688),
        (12, 152),
        (14, 920),
        (15, 920),
        (17, 644),
        (18, 388),
        (20, 652),
        (21, 1014),
        (23, 176),
        (24, 176),
        (26, 752),
        (27, 496),
        (28, 332),
        (29, 432),
        (30, 964),
        (32, 452),
    ];
    match mode {
        Mode::A => &A,
        Mode::B => &B,
        Mode::C => &C,
        Mode::D => &D,
    }
}

/// The gain reference phase (clause 8.4.4.3.1):
/// (4 Z256[n,m] + p W1024[n,m] + p^2 (1 + s) Q1024) mod 1024 with
/// n = s mod y, m = floor(s / y), p = (k - k0 - n x) / (x y).
pub fn gain_phase(mode: Mode, s: usize, k: i32) -> u16 {
    const WA: [[i32; 3]; 5] = [[228, 341, 455], [455, 569, 683], [683, 796, 910], [910, 0, 114], [114, 228, 341]];
    const ZA: [[i32; 3]; 5] = [[0, 81, 248], [18, 106, 106], [122, 116, 31], [129, 129, 39], [33, 32, 111]];
    const WB: [[i32; 5]; 3] = [[512, 0, 512, 0, 512], [0, 512, 0, 512, 0], [512, 0, 512, 0, 512]];
    const ZB: [[i32; 5]; 3] = [[0, 57, 164, 64, 12], [168, 255, 161, 106, 118], [25, 232, 132, 233, 38]];
    const WC: [[i32; 10]; 2] =
        [[465, 372, 279, 186, 93, 0, 931, 838, 745, 652], [931, 838, 745, 652, 559, 465, 372, 279, 186, 93]];
    const ZC: [[i32; 10]; 2] =
        [[0, 76, 29, 76, 9, 190, 161, 248, 33, 108], [179, 178, 83, 253, 127, 105, 101, 198, 250, 145]];
    const WD: [[i32; 8]; 3] = [
        [366, 439, 512, 585, 658, 731, 805, 878],
        [731, 805, 878, 951, 0, 73, 146, 219],
        [73, 146, 219, 293, 366, 439, 512, 585],
    ];
    const ZD: [[i32; 8]; 3] = [
        [0, 240, 17, 60, 220, 38, 151, 101],
        [110, 7, 78, 82, 175, 150, 106, 25],
        [165, 7, 252, 124, 253, 177, 197, 142],
    ];
    let (x, y, k0) = mode.grid();
    let n = s as i32 % y;
    let m = (s as i32 / y) as usize;
    let p = (k - k0 - n * x).div_euclid(x * y);
    let n = n as usize;
    let (w, z, q) = match mode {
        Mode::A => (WA[n][m], ZA[n][m], 36),
        Mode::B => (WB[n][m], ZB[n][m], 12),
        Mode::C => (WC[n][m], ZC[n][m], 12),
        Mode::D => (WD[n][m], ZD[n][m], 14),
    };
    (4 * z + p * w + p * p * (1 + s as i32) * q).rem_euclid(1024) as u16
}

/// Tables 62 to 65: the FAC cells of each frame, as (symbol, carriers).
/// Cells are filled in this order: symbol by symbol, carriers ascending.
pub fn fac_cells(mode: Mode) -> &'static [(u8, &'static [i32])] {
    const A: [(u8, &[i32]); 12] = [
        (2, &[26, 46, 66, 86]),
        (3, &[10, 30, 50, 70, 90]),
        (4, &[14, 22, 34, 62, 74, 94]),
        (5, &[26, 38, 58, 66, 78]),
        (6, &[22, 30, 42, 62, 70, 82]),
        (7, &[26, 34, 46, 66, 74, 86]),
        (8, &[10, 30, 38, 50, 58, 70, 78, 90]),
        (9, &[14, 22, 34, 42, 62, 74, 82, 94]),
        (10, &[26, 38, 46, 66, 86]),
        (11, &[10, 30, 50, 70, 90]),
        (12, &[14, 34, 74, 94]),
        (13, &[38, 58, 78]),
    ];
    const B: [(u8, &[i32]); 12] = [
        (2, &[13, 25, 43, 55, 67]),
        (3, &[15, 27, 45, 57, 69]),
        (4, &[17, 29, 47, 59, 71]),
        (5, &[19, 31, 49, 61, 73]),
        (6, &[9, 21, 33, 51, 63, 75]),
        (7, &[11, 23, 35, 53, 65, 77]),
        (8, &[13, 25, 37, 55, 67, 79]),
        (9, &[15, 27, 39, 57, 69, 81]),
        (10, &[17, 29, 41, 59, 71, 83]),
        (11, &[19, 31, 43, 61, 73]),
        (12, &[21, 33, 45, 63, 75]),
        (13, &[23, 35, 47, 65, 77]),
    ];
    const C: [(u8, &[i32]); 16] = [
        (3, &[9, 21, 45, 57]),
        (4, &[23, 35, 47]),
        (5, &[13, 25, 37, 49]),
        (6, &[15, 27, 39, 51]),
        (7, &[5, 17, 29, 41, 53]),
        (8, &[7, 19, 31, 43, 55]),
        (9, &[9, 21, 45, 57]),
        (10, &[23, 35, 47]),
        (11, &[13, 25, 37, 49]),
        (12, &[15, 27, 39, 51]),
        (13, &[5, 17, 29, 41, 53]),
        (14, &[7, 19, 31, 43, 55]),
        (15, &[9, 21, 45, 57]),
        (16, &[23, 35, 47]),
        (17, &[13, 25, 37, 49]),
        (18, &[15, 27, 39, 51]),
    ];
    const D: [(u8, &[i32]); 20] = [
        (3, &[9, 18, 27]),
        (4, &[10, 19]),
        (5, &[11, 20, 29]),
        (6, &[12, 30]),
        (7, &[13, 22, 31]),
        (8, &[5, 14, 23, 32]),
        (9, &[6, 15, 24, 33]),
        (10, &[16, 25, 34]),
        (11, &[8, 17, 26, 35]),
        (12, &[9, 18, 27, 36]),
        (13, &[10, 19, 37]),
        (14, &[11, 20, 29]),
        (15, &[12, 30]),
        (16, &[13, 22, 31]),
        (17, &[5, 14, 23, 32]),
        (18, &[6, 15, 24, 33]),
        (19, &[16, 25, 34]),
        (20, &[8, 17, 26, 35]),
        (21, &[9, 18, 27, 36]),
        (22, &[10, 19, 37]),
    ];
    match mode {
        Mode::A => &A,
        Mode::B => &B,
        Mode::C => &C,
        Mode::D => &D,
    }
}

/// FAC cells per frame and bits per FAC block (clause 7.2.1.2).
pub const FAC_CELLS: usize = 65;
pub const FAC_BITS: usize = 72;

/// A code rate RX/RY of the punctured mother code: its puncturing pattern
/// is RX columns, one per input bit, each a mask of the mother code's
/// outputs b0..b3 sent for that bit (bit j set: b_j is sent, in order b0,
/// b1, b2, b3). Table 27.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rate {
    pub rx: usize,
    pub ry: usize,
    pub cols: &'static [u8],
}

impl Rate {
    #[cfg(test)]
    pub fn value(&self) -> f64 {
        self.rx as f64 / self.ry as f64
    }
}

pub const R1_4: Rate = Rate { rx: 1, ry: 4, cols: &[0b1111] };
// Rates 3/10, 4/11, 4/7 and 8/11 complete table 27 but serve only
// robustness mode E and the old hierarchical modes; the tests run them.
#[cfg(test)]
pub const R3_10: Rate = Rate { rx: 3, ry: 10, cols: &[0b1111, 0b0111, 0b0111] };
pub const R1_3: Rate = Rate { rx: 1, ry: 3, cols: &[0b0111] };
#[cfg(test)]
pub const R4_11: Rate = Rate { rx: 4, ry: 11, cols: &[0b0111, 0b0111, 0b0111, 0b0011] };
pub const R1_2: Rate = Rate { rx: 1, ry: 2, cols: &[0b0011] };
#[cfg(test)]
pub const R4_7: Rate = Rate { rx: 4, ry: 7, cols: &[0b0011, 0b0101, 0b0011, 0b0001] };
pub const R3_5: Rate = Rate { rx: 3, ry: 5, cols: &[0b0011, 0b0001, 0b0011] };
pub const R2_3: Rate = Rate { rx: 2, ry: 3, cols: &[0b0011, 0b0001] };
#[cfg(test)]
pub const R8_11: Rate = Rate { rx: 8, ry: 11, cols: &[0b0011, 0b0001, 0b0001, 0b0011, 0b0001, 0b0001, 0b0011, 0b0001] };
pub const R3_4: Rate = Rate { rx: 3, ry: 4, cols: &[0b0011, 0b0001, 0b0001] };
pub const R4_5: Rate = Rate { rx: 4, ry: 5, cols: &[0b0011, 0b0001, 0b0001, 0b0001] };
pub const R7_8: Rate = Rate { rx: 7, ry: 8, cols: &[0b0011, 0b0001, 0b0001, 0b0001, 0b0001, 0b0001, 0b0001] };
pub const R8_9: Rate = Rate { rx: 8, ry: 9, cols: &[0b0011, 0b0001, 0b0001, 0b0001, 0b0001, 0b0001, 0b0001, 0b0001] };

/// Table 28: the puncturing of the six tail bits, indexed by
/// r_p = (2 N2 - 12) - RY floor((2 N2 - 12) / RY).
pub const TAIL: [[u8; 6]; 12] = [
    [0b0011, 0b0011, 0b0011, 0b0011, 0b0011, 0b0011],
    [0b0111, 0b0011, 0b0011, 0b0011, 0b0011, 0b0011],
    [0b0111, 0b0011, 0b0011, 0b0111, 0b0011, 0b0011],
    [0b0111, 0b0111, 0b0011, 0b0111, 0b0011, 0b0011],
    [0b0111, 0b0111, 0b0011, 0b0111, 0b0111, 0b0011],
    [0b0111, 0b0111, 0b0111, 0b0111, 0b0111, 0b0011],
    [0b0111, 0b0111, 0b0111, 0b0111, 0b0111, 0b0111],
    [0b1111, 0b0111, 0b0111, 0b0111, 0b0111, 0b0111],
    [0b1111, 0b0111, 0b0111, 0b1111, 0b0111, 0b0111],
    [0b1111, 0b1111, 0b0111, 0b1111, 0b0111, 0b0111],
    [0b1111, 0b1111, 0b0111, 0b1111, 0b0111, 0b1111],
    [0b1111, 0b1111, 0b1111, 0b1111, 0b0111, 0b1111],
];

/// Table 30: the 16-QAM MSC protection levels, (R0, R1) and RYlcm.
pub fn msc16_rates(level: u8) -> ([Rate; 2], usize) {
    if level & 1 == 0 { ([R1_3, R2_3], 3) } else { ([R1_2, R3_4], 4) }
}

/// Table 32: the 64-QAM MSC protection levels, (R0, R1, R2) and RYlcm.
pub fn msc64_rates(level: u8) -> ([Rate; 3], usize) {
    match level & 3 {
        0 => ([R1_4, R1_2, R3_4], 4),
        1 => ([R1_3, R2_3, R4_5], 15),
        2 => ([R1_2, R3_4, R7_8], 8),
        _ => ([R2_3, R4_5, R8_9], 45),
    }
}

/// Table 36 (16-QAM SDC) and table 37 (4-QAM SDC).
pub const SDC16_RATES: [Rate; 2] = [R1_3, R2_3];
pub const SDC4_RATE: Rate = R1_2;
/// Table 39: the FAC.
pub const FAC_RATE: Rate = R3_5;

/// Bit interleaver constants t_p of clause 7.3.3 for each level, `None`
/// where a level is not interleaved: 4-QAM level 0: 21; 16-QAM: 13, 21;
/// 64-QAM: none, 13, 21.
pub fn interleaver_t0(levels: usize, level: usize) -> Option<usize> {
    match (levels, level) {
        (1, 0) => Some(21),
        (2, 0) => Some(13),
        (2, 1) => Some(21),
        (3, 1) => Some(13),
        (3, 2) => Some(21),
        _ => None,
    }
}

/// Table 18: the FAC language codes.
pub fn language_name(code: u8) -> &'static str {
    const NAMES: [&str; 16] = [
        "No language specified",
        "Arabic",
        "Bengali",
        "Chinese (Mandarin)",
        "Dutch",
        "English",
        "French",
        "German",
        "Hindi",
        "Japanese",
        "Javanese",
        "Korean",
        "Portuguese",
        "Russian",
        "Spanish",
        "Other language",
    ];
    NAMES[(code & 15) as usize]
}

/// Table 19: program type codes (the standard's "programme", in American
/// spelling).
pub fn program_type_name(code: u8) -> &'static str {
    const NAMES: [&str; 32] = [
        "No program type",
        "News",
        "Current Affairs",
        "Information",
        "Sport",
        "Education",
        "Drama",
        "Culture",
        "Science",
        "Varied",
        "Pop Music",
        "Rock Music",
        "Easy Listening Music",
        "Light Classical",
        "Serious Classical",
        "Other Music",
        "Weather/meteorology",
        "Finance/Business",
        "Children's programs",
        "Social Affairs",
        "Religion",
        "Phone In",
        "Travel",
        "Leisure",
        "Jazz Music",
        "Country Music",
        "National Music",
        "Oldies Music",
        "Folk Music",
        "Documentary",
        "Warning/alarm indicator",
        "Skip indicator",
    ];
    NAMES[(code & 31) as usize]
}
