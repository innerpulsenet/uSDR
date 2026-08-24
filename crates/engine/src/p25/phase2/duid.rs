//! Phase 2 burst data-unit identification.

/// The useful Phase 2 burst formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BurstType {
    Voice4,
    SacchScrambled,
    Voice2,
    FacchScrambled,
    Sacch,
    Lcch,
    Facch,
    Unknown,
}

impl BurstType {
    pub fn scrambled(self) -> bool {
        matches!(
            self,
            Self::Voice4 | Self::SacchScrambled | Self::Voice2 | Self::FacchScrambled
        )
    }
}

/// Decode the (8,4,4) DUID codeword.  Positions are relative to the 170-dibit
/// burst window beginning ten dibits into the ISCH; the `payload` supplied by
/// our framer begins ten dibits later, hence indices 0,37,122,159 here.
pub fn decode(payload: &[u8; super::PAYLOAD_DIBITS]) -> BurstType {
    let received = (payload[0] << 6) | (payload[37] << 4) | (payload[122] << 2) | payload[159];
    let mut best = (u32::MAX, 0u8);
    for value in 0..16u8 {
        let code = encode(value);
        let distance = (received ^ code).count_ones();
        if distance < best.0 {
            best = (distance, value);
        }
    }
    // Minimum distance four: one error is unambiguously correctable.
    if best.0 > 1 {
        return BurstType::Unknown;
    }
    match best.1 {
        0 => BurstType::Voice4,
        3 => BurstType::SacchScrambled,
        6 => BurstType::Voice2,
        9 => BurstType::FacchScrambled,
        12 => BurstType::Sacch,
        13 => BurstType::Lcch,
        15 => BurstType::Facch,
        _ => BurstType::Unknown,
    }
}

fn encode(value: u8) -> u8 {
    let a = (value >> 3) & 1;
    let b = (value >> 2) & 1;
    let c = (value >> 1) & 1;
    let d = value & 1;
    (a << 7)
        | (b << 6)
        | (c << 5)
        | (d << 4)
        | ((a ^ b ^ c) << 3)
        | ((a ^ c ^ d) << 2)
        | ((b ^ c ^ d) << 1)
        | (a ^ b ^ d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(value: u8) -> [u8; super::super::PAYLOAD_DIBITS] {
        let code = encode(value);
        let mut p = [0; super::super::PAYLOAD_DIBITS];
        for (i, at) in [0, 37, 122, 159].into_iter().enumerate() {
            p[at] = (code >> (6 - i * 2)) & 3;
        }
        p
    }

    #[test]
    fn known_burst_types_decode() {
        assert_eq!(decode(&payload(0)), BurstType::Voice4);
        assert_eq!(decode(&payload(3)), BurstType::SacchScrambled);
        assert_eq!(decode(&payload(6)), BurstType::Voice2);
        assert_eq!(decode(&payload(9)), BurstType::FacchScrambled);
        assert_eq!(decode(&payload(12)), BurstType::Sacch);
        assert_eq!(decode(&payload(13)), BurstType::Lcch);
        assert_eq!(decode(&payload(15)), BurstType::Facch);
    }

    #[test]
    fn one_bad_duid_bit_is_corrected() {
        let mut p = payload(6);
        p[37] ^= 1;
        assert_eq!(decode(&p), BurstType::Voice2);
    }
}
