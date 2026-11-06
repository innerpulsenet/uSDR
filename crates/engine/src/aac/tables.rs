//! Rate-dependent tables for the AAC core: sampling-frequency indices, the
//! scalefactor band offsets, and the TNS band limits.
//!
//! The 1024/128 band tables are those of ISO/IEC 14496-3 4.5.4. The 960/120
//! tables (DRM, DAB+) are not printed there as separate tables: each is the
//! 1024/128 table cut at the first offset at or above 960 (120), with that
//! offset replaced by the frame end. That rule reproduces the 960/120 tables
//! other decoders carry (including 46 long bands at 64 kHz) and is checked
//! in the tests: every table ends exactly at the frame length and rises
//! strictly.

pub(super) const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// The sampling-frequency index of a rate, if it is one AAC defines.
pub(super) fn rate_index(rate: u32) -> Option<usize> {
    SAMPLE_RATES.iter().position(|&r| r == rate)
}

const LONG_96: &[u16] = &[
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 96, 108, 120, 132,
    144, 156, 172, 188, 212, 240, 276, 320, 384, 448, 512, 576, 640, 704, 768, 832, 896, 960, 1024,
];
const LONG_64: &[u16] = &[
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 100, 112, 124, 140,
    156, 172, 192, 216, 240, 268, 304, 344, 384, 424, 464, 504, 544, 584, 624, 664, 704, 744, 784,
    824, 864, 904, 944, 984, 1024,
];
const LONG_48: &[u16] = &[
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 48, 56, 64, 72, 80, 88, 96, 108, 120, 132, 144, 160,
    176, 196, 216, 240, 264, 292, 320, 352, 384, 416, 448, 480, 512, 544, 576, 608, 640, 672, 704,
    736, 768, 800, 832, 864, 896, 928, 1024,
];
const LONG_32: &[u16] = &[
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 48, 56, 64, 72, 80, 88, 96, 108, 120, 132, 144, 160,
    176, 196, 216, 240, 264, 292, 320, 352, 384, 416, 448, 480, 512, 544, 576, 608, 640, 672, 704,
    736, 768, 800, 832, 864, 896, 928, 960, 992, 1024,
];
const LONG_24: &[u16] = &[
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 52, 60, 68, 76, 84, 92, 100, 108, 116, 124, 136,
    148, 160, 172, 188, 204, 220, 240, 260, 284, 308, 336, 364, 396, 432, 468, 508, 552, 600, 652,
    704, 768, 832, 896, 960, 1024,
];
const LONG_16: &[u16] = &[
    0, 8, 16, 24, 32, 40, 48, 56, 64, 72, 80, 88, 100, 112, 124, 136, 148, 160, 172, 184, 196, 212,
    228, 244, 260, 280, 300, 320, 344, 368, 396, 424, 456, 492, 532, 572, 616, 664, 716, 772, 832,
    896, 960, 1024,
];
const LONG_8: &[u16] = &[
    0, 12, 24, 36, 48, 60, 72, 84, 96, 108, 120, 132, 144, 156, 172, 188, 204, 220, 236, 252, 268,
    288, 308, 328, 348, 372, 396, 420, 448, 476, 508, 544, 580, 620, 664, 712, 764, 820, 880, 944,
    1024,
];
const SHORT_96: &[u16] = &[0, 4, 8, 12, 16, 20, 24, 32, 40, 48, 64, 92, 128];
const SHORT_48: &[u16] = &[0, 4, 8, 12, 16, 20, 28, 36, 44, 56, 68, 80, 96, 112, 128];
const SHORT_24: &[u16] = &[0, 4, 8, 12, 16, 20, 24, 28, 36, 44, 52, 64, 76, 92, 108, 128];
const SHORT_16: &[u16] = &[0, 4, 8, 12, 16, 20, 24, 28, 32, 40, 48, 60, 72, 88, 108, 128];
const SHORT_8: &[u16] = &[0, 4, 8, 12, 16, 20, 24, 28, 36, 44, 52, 60, 72, 88, 108, 128];

/// Per sampling-frequency index: the long and short band tables.
const BANDS: [(&[u16], &[u16]); 13] = [
    (LONG_96, SHORT_96),
    (LONG_96, SHORT_96),
    (LONG_64, SHORT_96),
    (LONG_48, SHORT_48),
    (LONG_48, SHORT_48),
    (LONG_32, SHORT_48),
    (LONG_24, SHORT_24),
    (LONG_24, SHORT_24),
    (LONG_16, SHORT_16),
    (LONG_16, SHORT_16),
    (LONG_16, SHORT_16),
    (LONG_8, SHORT_8),
    (LONG_8, SHORT_8),
];

/// TNS may act only on bands below these (AAC-LC, long and short windows).
const TNS_MAX_BANDS_LONG: [u8; 13] = [31, 31, 34, 40, 42, 51, 46, 46, 42, 42, 42, 39, 39];
const TNS_MAX_BANDS_SHORT: [u8; 13] = [9, 9, 10, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14];

/// The band layout for one sampling rate and frame length.
pub(super) struct BandTables {
    /// Long-window band offsets, `num_swb + 1` entries ending at the frame
    /// length.
    pub long: Vec<u16>,
    /// Short-window band offsets, ending at the short window length.
    pub short: Vec<u16>,
    pub tns_max_long: usize,
    pub tns_max_short: usize,
}

/// Cuts a 1024/128 table down to a shorter frame.
fn truncate(table: &[u16], len: u16) -> Vec<u16> {
    let n = table.iter().position(|&o| o >= len).expect("table reaches the frame end");
    let mut t = table[..n].to_vec();
    t.push(len);
    t
}

impl BandTables {
    pub fn new(rate_index: usize, frame_len: usize) -> Self {
        let (long, short) = BANDS[rate_index];
        let (long, short) = if frame_len == 960 {
            (truncate(long, 960), truncate(short, 120))
        } else {
            (long.to_vec(), short.to_vec())
        };
        Self {
            long,
            short,
            tns_max_long: usize::from(TNS_MAX_BANDS_LONG[rate_index]),
            tns_max_short: usize::from(TNS_MAX_BANDS_SHORT[rate_index]),
        }
    }

    pub fn num_swb(&self, short: bool) -> usize {
        if short { self.short.len() - 1 } else { self.long.len() - 1 }
    }

    pub fn offsets(&self, short: bool) -> &[u16] {
        if short { &self.short } else { &self.long }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_tables_cover_the_frame_for_both_lengths() {
        // Band counts the 960/120 tables have in DRM/DAB+ decoders.
        let num_960 = [40, 40, 46, 49, 49, 49, 46, 46, 42, 42, 42, 40, 40];
        let num_120 = [12, 12, 12, 14, 14, 14, 15, 15, 15, 15, 15, 15, 15];
        for idx in 0..13 {
            for (frame, short) in [(1024usize, 128usize), (960, 120)] {
                let t = BandTables::new(idx, frame);
                assert_eq!(*t.long.last().unwrap() as usize, frame);
                assert_eq!(*t.short.last().unwrap() as usize, short);
                assert!(t.long.windows(2).all(|w| w[0] < w[1]));
                assert!(t.short.windows(2).all(|w| w[0] < w[1]));
                // Spectral codewords hold 2 or 4 lines and must not cross a band.
                assert!(t.long.iter().chain(&t.short).all(|&o| o % 4 == 0));
                if frame == 960 {
                    assert_eq!(t.num_swb(false), num_960[idx], "rate index {idx}");
                    assert_eq!(t.num_swb(true), num_120[idx], "rate index {idx}");
                }
            }
        }
    }
}
