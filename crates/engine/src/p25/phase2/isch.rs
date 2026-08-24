//! The Inter-Slot Signalling CHannel: 40-bit words leading every burst.
//!
//! Two kinds share the space. **S-ISCH** is a single fixed sync word marking a
//! synchronisation burst. **I-ISCH** is a (40,9,16) block code: 128 valid
//! codewords, each carrying a 9-bit number that says which slot of the
//! superframe this burst is, whether the channel is busy, and which logical
//! timeslot (0 or 1) it belongs to. The code corrects up to 7 bit errors, so a
//! received word is matched by smallest Hamming distance rather than exact
//! equality — which is also what lets the ISCH double as a soft frame sync.

/// The fixed S-ISCH sync codeword.
pub const S_ISCH: u64 = 0x575d_57f7_ff;

/// Bit mask for a 40-bit ISCH word.
const MASK40: u64 = (1 << 40) - 1;

/// Largest Hamming distance the (40,9,16) code can correct.
const MAX_CORRECT: u32 = 7;

/// The 128 I-ISCH codewords, indexed by their 9-bit information value 0..=127.
/// Vendored as data from op25's table (see the module docs); the encoder that
/// produced them is not needed at the receiver.
static I_ISCH: [u64; 128] = [
    0x184229d461,
    0x18761451f6,
    0x181ae27e2f,
    0x182edffbb8,
    0x18df8a7510,
    0x18ebb7f087,
    0x188741df5e,
    0x18b37c5ac9,
    0x1146a44f13,
    0x117299ca84,
    0x111e6fe55d,
    0x112a5260ca,
    0x11db07ee62,
    0x11ef3a6bf5,
    0x1183cc442c,
    0x11b7f1c1bb,
    0x1a4a2e239e,
    0x1a7e13a609,
    0x1a12e589d0,
    0x1a26d80c47,
    0x1ad78d82ef,
    0x1ae3b00778,
    0x1a8f4628a1,
    0x1abb7bad36,
    0x134ea3b8ec,
    0x137a9e3d7b,
    0x13166812a2,
    0x1322559735,
    0x13d300199d,
    0x13e73d9c0a,
    0x138bcbb3d3,
    0x13bff63644,
    0x1442f705ef,
    0x1476ca8078,
    0x141a3cafa1,
    0x142e012a36,
    0x14df54a49e,
    0x14eb692109,
    0x14879f0ed0,
    0x14b3a28b47,
    0x1d467a9e9d,
    0x1d72471b0a,
    0x1d1eb134d3,
    0x1d2a8cb144,
    0x1ddbd93fec,
    0x1defe4ba7b,
    0x1d831295a2,
    0x1db72f1035,
    0x164af0f210,
    0x167ecd7787,
    0x16123b585e,
    0x162606ddc9,
    0x16d7535361,
    0x16e36ed6f6,
    0x168f98f92f,
    0x16bba57cb8,
    0x1f4e7d6962,
    0x1f7a40ecf5,
    0x1f16b6c32c,
    0x1f228b46bb,
    0x1fd3dec813,
    0x1fe7e34d84,
    0x1f8b15625d,
    0x1fbf28e7ca,
    0x084d62c339,
    0x08795f46ae,
    0x0815a96977,
    0x082194ece0,
    0x08d0c16248,
    0x08e4fce7df,
    0x08880ac806,
    0x08bc374d91,
    0x0149ef584b,
    0x017dd2dddc,
    0x011124f205,
    0x0125197792,
    0x01d44cf93a,
    0x01e0717cad,
    0x018c875374,
    0x01b8bad6e3,
    0x0a456534c6,
    0x0a7158b151,
    0x0a1dae9e88,
    0x0a29931b1f,
    0x0ad8c695b7,
    0x0aecfb1020,
    0x0a800d3ff9,
    0x0ab430ba6e,
    0x0341e8afb4,
    0x0375d52a23,
    0x03192305fa,
    0x032d1e806d,
    0x03dc4b0ec5,
    0x03e8768b52,
    0x038480a48b,
    0x03b0bd211c,
    0x044dbc12b7,
    0x0479819720,
    0x041577b8f9,
    0x04214a3d6e,
    0x04d01fb3c6,
    0x04e4223651,
    0x0488d41988,
    0x04bce99c1f,
    0x0d493189c5,
    0x0d7d0c0c52,
    0x0d11fa238b,
    0x0d25c7a61c,
    0x0dd49228b4,
    0x0de0afad23,
    0x0d8c5982fa,
    0x0db864076d,
    0x0645bbe548,
    0x06718660df,
    0x061d704f06,
    0x06294dca91,
    0x06d8184439,
    0x06ec25c1ae,
    0x0680d3ee77,
    0x06b4ee6be0,
    0x0f41367e3a,
    0x0f750bfbad,
    0x0f19fdd474,
    0x0f2dc051e3,
    0x0fdc95df4b,
    0x0fe8a85adc,
    0x0f845e7505,
    0x0fb063f092,
];

/// What an ISCH word turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isch {
    /// A synchronisation burst (S-ISCH), within the correctable distance.
    Sync,
    /// An information burst (I-ISCH), carrying its decoded 9-bit value.
    Info(IschInfo),
    /// Neither codeword was within the code's correction radius.
    Unknown,
}

/// The fields an I-ISCH information value carries.
///
/// Layout, MSB first within the 9 bits: `chan(2) | loc(2) | fr(1) | cnt(2)` —
/// wait, op25 unpacks it low-to-high as cnt, fr, loc, chan. We keep the same
/// order it decodes them in so the slot check matches the standard exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IschInfo {
    /// The raw 9-bit information value, 0..=127 (top bits always land in range).
    pub value: u16,
    /// Superframe slot counter, 0..=3 within a channel's frame.
    pub cnt: u8,
    /// Frame flag.
    pub fr: u8,
    /// Location within the superframe.
    pub loc: u8,
    /// Logical timeslot: channel 0 or 1.
    pub chan: u8,
}

/// The I-ISCH codeword that encodes a given information value, or `None` if the
/// value is out of range.
pub fn codeword_for(value: u16) -> Option<u64> {
    I_ISCH.get(value as usize).copied()
}

impl IschInfo {
    pub fn from_value(value: u16) -> Self {
        let mut rc = value;
        let cnt = (rc & 3) as u8;
        rc >>= 2;
        let fr = (rc & 1) as u8;
        rc >>= 1;
        let loc = (rc & 3) as u8;
        rc >>= 2;
        let chan = (rc & 3) as u8;
        Self {
            value,
            cnt,
            fr,
            loc,
            chan,
        }
    }

    /// op25's `checkval`: `loc*4 + chan`, the quantity the superframe-position
    /// table is expressed in.
    pub fn checkval(&self) -> i32 {
        i32::from(self.loc) * 4 + i32::from(self.chan)
    }
}

/// Decode a 40-bit ISCH word, correcting up to 7 bit errors.
pub fn decode(cw: u64) -> Isch {
    let cw = cw & MASK40;
    let sync_dist = (cw ^ S_ISCH).count_ones();
    if sync_dist == 0 {
        return Isch::Sync;
    }
    let mut best: Option<(u32, u16)> = if sync_dist <= MAX_CORRECT {
        Some((sync_dist, u16::MAX))
    } else {
        None
    };
    for (value, &code) in I_ISCH.iter().enumerate() {
        let d = (cw ^ code).count_ones();
        if d == 0 {
            return Isch::Info(IschInfo::from_value(value as u16));
        }
        if d <= MAX_CORRECT && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, value as u16));
        }
    }
    match best {
        Some((_, u16::MAX)) => Isch::Sync,
        Some((_, value)) => Isch::Info(IschInfo::from_value(value)),
        None => Isch::Unknown,
    }
}

/// Decode from 20 dibits (as they arrive on air), MSB dibit first.
pub fn decode_dibits(dibits: &[u8]) -> Isch {
    debug_assert!(dibits.len() >= super::ISCH_DIBITS);
    let mut cw = 0u64;
    for &d in &dibits[..super::ISCH_DIBITS] {
        cw = (cw << 2) | u64::from(d & 3);
    }
    decode(cw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sync_word_decodes_as_sync() {
        assert_eq!(decode(S_ISCH), Isch::Sync);
    }

    #[test]
    fn every_iisch_codeword_decodes_to_its_own_index() {
        for (value, &code) in I_ISCH.iter().enumerate() {
            match decode(code) {
                Isch::Info(info) => assert_eq!(info.value, value as u16, "codeword {value}"),
                other => panic!("codeword {value} decoded as {other:?}"),
            }
        }
    }

    #[test]
    fn up_to_seven_errors_are_corrected() {
        let code = I_ISCH[42];
        // Flip 7 bits; still the nearest codeword.
        let mut corrupt = code;
        for b in 0..7 {
            corrupt ^= 1 << (b * 5);
        }
        match decode(corrupt) {
            Isch::Info(info) => assert_eq!(info.value, 42),
            other => panic!("decoded as {other:?}"),
        }
    }

    #[test]
    fn a_word_far_from_every_codeword_is_unknown() {
        // All-ones is >7 from every valid word.
        assert_eq!(decode(MASK40), Isch::Unknown);
    }

    #[test]
    fn dibit_decoding_matches_bit_decoding() {
        let code = I_ISCH[10];
        let mut dibits = [0u8; super::super::ISCH_DIBITS];
        for (i, d) in dibits.iter_mut().enumerate() {
            *d = ((code >> (38 - 2 * i)) & 3) as u8;
        }
        assert_eq!(decode_dibits(&dibits), decode(code));
    }

    #[test]
    fn info_fields_unpack_in_op25_order() {
        // MSB→LSB the 9 bits are chan(2) loc(2) fr(1) cnt(2):
        // chan=2, loc=2, fr=1, cnt=3 → 0b10_10_1_11 = 87.
        let info = IschInfo::from_value(0b10_10_1_11);
        assert_eq!((info.chan, info.loc, info.fr, info.cnt), (2, 2, 1, 3));
        assert_eq!(info.checkval(), 2 * 4 + 2);
    }
}
