//! The four 48-bit DMR burst sync words (ETSI TS 102 361-1 §9.1.1).
//!
//! Voice sync opens a voice superframe (burst A); data sync heads every
//! burst that carries a Slot Type PDU. BS-sourced patterns come from
//! repeaters, MS-sourced from radios (simplex or uplink).

/// Dibits in a sync word: 48 bits.
pub const SYNC_DIBITS: usize = 24;

/// What a matched sync says about the burst.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncKind {
    BsVoice,
    BsData,
    MsVoice,
    MsData,
}

impl SyncKind {
    /// Whether the burst belongs to a voice superframe.
    pub fn is_voice(self) -> bool {
        matches!(self, SyncKind::BsVoice | SyncKind::MsVoice)
    }

    /// Who transmitted the burst.
    pub fn source(self) -> super::SyncSource {
        match self {
            SyncKind::BsVoice | SyncKind::BsData => super::SyncSource::Bs,
            _ => super::SyncSource::Ms,
        }
    }
}

/// The sync words, as the 48 bits transmitted MSB first.
pub const PATTERNS: [(u64, SyncKind); 4] = [
    (0x755F_D7DF_75F7, SyncKind::BsVoice),
    (0xDFF5_7D75_DF5D, SyncKind::BsData),
    (0x7F7D_5DD5_7DFD, SyncKind::MsVoice),
    (0xD5D7_F77F_D757, SyncKind::MsData),
];

/// The dibit sequence of a sync word, first-transmitted first.
pub fn dibits(word: u64) -> [u8; SYNC_DIBITS] {
    let mut out = [0u8; SYNC_DIBITS];
    for (k, slot) in out.iter_mut().enumerate() {
        *slot = ((word >> (46 - 2 * k)) & 0b11) as u8;
    }
    out
}

/// The sync word packed two bits per dibit, matching [`classify`]'s packed
/// observation. First dibit in the high bits.
fn pack_pattern(word: u64) -> u64 {
    let mut out = 0u64;
    for d in dibits(word) {
        out = (out << 2) | u64::from(d);
    }
    out
}

/// The sync words in [`classify`]'s packed form, computed once. `acquire`
/// tests all four words at every sample position of its sweep, twice for
/// polarity — repacking them on each call was pure overhead.
fn packed_patterns() -> &'static [(u64, SyncKind); 4] {
    static PACKED: std::sync::OnceLock<[(u64, SyncKind); 4]> = std::sync::OnceLock::new();
    PACKED.get_or_init(|| {
        std::array::from_fn(|i| (pack_pattern(PATTERNS[i].0), PATTERNS[i].1))
    })
}

/// Ideal symbol levels for a sync word, for correlation and fitting.
#[allow(dead_code)]
pub fn levels(word: u64) -> [f32; SYNC_DIBITS] {
    let mut out = [0.0; SYNC_DIBITS];
    for (k, slot) in out.iter_mut().enumerate() {
        *slot = super::dibit_level(dibits(word)[k]);
    }
    out
}

/// Pack a dibit observation into the word [`classify_packed`] compares
/// against, first dibit in the high bits. Pack once per position and test
/// both polarities against the result.
pub fn pack_dibits(centre: &[u8; SYNC_DIBITS]) -> u64 {
    let mut got = 0u64;
    for &d in centre.iter() {
        got = (got << 2) | u64::from(d);
    }
    got
}

/// Which sync, if any a packed observation (see [`pack_dibits`]) carries, and
/// with how many dibit errors. Exact matches preferred; the caller sets the
/// error budget.
pub fn classify_packed(got: u64, inverted: bool) -> Option<(SyncKind, u8)> {
    let mut best: Option<(SyncKind, u8)> = None;
    for &(want, kind) in packed_patterns() {
        let diff = got ^ want;
        // Inversion flips the high bit of each dibit: a repeating 0b10 mask,
        // clipped to the 48 bits the packed word actually occupies.
        let inv_mask = 0xAAAA_AAAA_AAAAu64;
        let diff = if inverted { diff ^ inv_mask } else { diff };
        // Count mismatching DIBITS, not bits: fold each pair's two diff bits
        // into its low bit, then popcount the low-bit mask.
        let swar = (diff | (diff >> 1)) & 0x5555_5555_5555_5555u64;
        let errors = swar.count_ones() as u8;
        if best.map_or(true, |(_, e)| errors < e) {
            best = Some((kind, errors));
        }
    }
    best
}

/// Which sync, if any, a burst's 24 centre dibits carry, and with how many
/// dibit errors. Exact matches preferred; the caller sets the error budget.
pub fn classify(centre: &[u8; SYNC_DIBITS], inverted: bool) -> Option<(SyncKind, u8)> {
    classify_packed(pack_dibits(centre), inverted)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dsd matches syncs on the sign of the deviation only: '1' is a positive
    /// symbol, '3' a negative one. Its published patterns (refs/dsd,
    /// include/dsd.h) therefore pin the MSB of every sync dibit — a
    /// transcription check for the full 48-bit words above that does not
    /// depend on this crate's own conventions.
    #[test]
    fn sync_words_match_dsd_sign_patterns() {
        let dsd = [
            ("131111333113313313113313", SyncKind::BsVoice),
            ("313333111331131131331131", SyncKind::BsData),
            ("133313311131311113313331", SyncKind::MsVoice),
            ("311131133313133331131113", SyncKind::MsData),
        ];
        for (signs, kind) in dsd {
            let (word, _) = PATTERNS.iter().find(|&&(_, k)| k == kind).unwrap();
            let got: String = dibits(*word)
                .iter()
                .map(|d| if d & 0b10 != 0 { '3' } else { '1' })
                .collect();
            assert_eq!(got, signs, "{kind:?}");
        }
    }

    /// The four words are mutually distant, so a couple of RF errors cannot
    /// flip one into another.
    #[test]
    fn sync_words_are_well_separated() {
        for (i, &(a, _)) in PATTERNS.iter().enumerate() {
            for &(b, _) in &PATTERNS[i + 1..] {
                let da = dibits(a);
                let db = dibits(b);
                let dist = da.iter().zip(&db).filter(|(x, y)| x != y).count();
                assert!(dist >= 8, "distance {dist}");
            }
        }
    }

    #[test]
    fn classify_recovers_each_pattern_through_noise() {
        for &(word, kind) in &PATTERNS {
            let mut centre = dibits(word);
            centre[5] ^= 0b11;
            centre[17] ^= 0b01;
            assert_eq!(classify(&centre, false), Some((kind, 2)));
            let inverted: Vec<u8> = dibits(word).iter().map(|d| d ^ 0b10).collect();
            assert_eq!(
                classify(&inverted.try_into().unwrap(), true),
                Some((kind, 0))
            );
        }
    }

    /// `dibits` extracts bits 47..0 in transmit order and the repack puts
    /// them back in the same order, so packing is the identity on any 48-bit
    /// word — the cached packed forms equal `PATTERNS` exactly.
    #[test]
    fn packed_patterns_are_the_words_themselves() {
        for &(word, _) in &PATTERNS {
            assert_eq!(pack_pattern(word), word);
        }
    }
}
