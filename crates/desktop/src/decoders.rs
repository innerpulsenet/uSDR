//! The decoders the server runs, grouped by what they carry, so the decode
//! log reads a family at a time and a new decoder is one entry in
//! `DECODERS`.
//!
//! A decode is placed by its protocol name as the server sends it ("P25
//! Phase 1", "Motorola SmartNet / SmartZone", "FT8"), matched on the words
//! a decoder lists, the same way its color is chosen.

use usdr_proto::Mode;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Family {
    /// Digital voice, trunking control and the signaling riding on voice.
    Voice,
    Paging,
    Packet,
    /// What a broadcaster sends beside the program: RDS, SAME alerts.
    Broadcast,
    /// HF weak-signal and keyboard modes, decoded from sideband audio.
    HfData,
}

impl Family {
    pub const ALL: [Family; 5] = [Family::Voice, Family::Paging, Family::Packet, Family::Broadcast, Family::HfData];

    pub fn label(self) -> &'static str {
        match self {
            Family::Voice => "VOICE",
            Family::Paging => "PAGING",
            Family::Packet => "PACKET",
            Family::Broadcast => "BROADCAST",
            Family::HfData => "HF DATA",
        }
    }

    /// What the family's log holds, said while it holds nothing.
    pub fn empty_text(self) -> &'static str {
        match self {
            Family::Voice => "Digital voice calls, trunking grants and signaling (P25, DMR, NXDN, SmartNet and the rest) appear here. Pick P25, DMR or NXDN, or AUTO.",
            Family::Paging => "POCSAG and FLEX pages appear here. PAGER walks the paging channels around the tuned frequency; FLEX and PACKET decode the tuned channel.",
            Family::Packet => "AX.25 packets, APRS positions and messages among them, appear here. PACKET decodes the tuned channel.",
            Family::Broadcast => "A broadcast station's RDS (its name, RadioText, clock) and NOAA SAME alerts appear here. Tune WFM to a station for RDS.",
            Family::HfData => "HF data appears here: FT8 and FT4 at the end of each slot, RTTY, PSK31 and NAVTEX text as it is copied, and radiofax pictures in their viewer. Pick a mode and tune a dial frequency; BANDS lists them.",
        }
    }

    /// The family a mode's decodes belong to, when it has just the one.
    pub fn for_mode(m: Mode) -> Option<Family> {
        match m {
            Mode::P25 | Mode::Dmr | Mode::Nxdn => Some(Family::Voice),
            Mode::Pager | Mode::Flex => Some(Family::Paging),
            Mode::Wfm => Some(Family::Broadcast),
            Mode::Ft8 | Mode::Ft4 | Mode::Wefax | Mode::Rtty | Mode::Navtex | Mode::Psk31 => Some(Family::HfData),
            _ => None,
        }
    }
}

/// One decoder as the log knows it: the chip that filters to it, its
/// family, and the words in the protocol names it sends.
pub struct Decoder {
    pub chip: &'static str,
    pub family: Family,
    pub matches: &'static [&'static str],
}

pub const DECODERS: &[Decoder] = &[
    Decoder { chip: "P25", family: Family::Voice, matches: &["P25"] },
    Decoder { chip: "DMR", family: Family::Voice, matches: &["DMR"] },
    Decoder { chip: "NXDN", family: Family::Voice, matches: &["NXDN"] },
    Decoder { chip: "TRUNK", family: Family::Voice, matches: &["SMARTNET", "SMARTZONE", "EDACS", "LTR", "PASSPORT"] },
    Decoder { chip: "SIGNAL", family: Family::Voice, matches: &["MDC", "CTCSS", "DCS"] },
    Decoder { chip: "POCSAG", family: Family::Paging, matches: &["POCSAG"] },
    Decoder { chip: "FLEX", family: Family::Paging, matches: &["FLEX"] },
    Decoder { chip: "APRS", family: Family::Packet, matches: &["APRS", "AX.25"] },
    Decoder { chip: "RDS", family: Family::Broadcast, matches: &["RDS"] },
    Decoder { chip: "SAME", family: Family::Broadcast, matches: &["SAME"] },
    Decoder { chip: "FT8", family: Family::HfData, matches: &["FT8"] },
    Decoder { chip: "FT4", family: Family::HfData, matches: &["FT4"] },
    Decoder { chip: "PSK31", family: Family::HfData, matches: &["PSK"] },
    Decoder { chip: "RTTY", family: Family::HfData, matches: &["RTTY"] },
    Decoder { chip: "NAVTEX", family: Family::HfData, matches: &["NAVTEX"] },
    Decoder { chip: "WEFAX", family: Family::HfData, matches: &["WEFAX"] },
];

impl Decoder {
    pub fn takes(&self, protocol_upper: &str) -> bool {
        self.matches.iter().any(|m| protocol_upper.contains(m))
    }
}

/// The decoder a protocol name belongs to, if the log knows it.
pub fn decoder_of(protocol: &str) -> Option<&'static Decoder> {
    let u = protocol.to_ascii_uppercase();
    DECODERS.iter().find(|d| d.takes(&u))
}

pub fn family_of(protocol: &str) -> Option<Family> {
    decoder_of(protocol).map(|d| d.family)
}

pub fn in_family(f: Family) -> impl Iterator<Item = &'static Decoder> {
    DECODERS.iter().filter(move |d| d.family == f)
}

/// The FT8 and FT4 dial frequencies (the suppressed carrier, USB), as the
/// amateur community uses them, by band.
pub const HF_DIALS: &[(&str, f64, f64)] = &[
    ("160 m", 1_840_000.0, 1_840_000.0),
    ("80 m", 3_573_000.0, 3_575_000.0),
    ("60 m", 5_357_000.0, 5_357_000.0),
    ("40 m", 7_074_000.0, 7_047_500.0),
    ("30 m", 10_136_000.0, 10_140_000.0),
    ("20 m", 14_074_000.0, 14_080_000.0),
    ("17 m", 18_100_000.0, 18_104_000.0),
    ("15 m", 21_074_000.0, 21_140_000.0),
    ("12 m", 24_915_000.0, 24_919_000.0),
    ("10 m", 28_074_000.0, 28_180_000.0),
    ("6 m", 50_313_000.0, 50_318_000.0),
    ("2 m", 144_174_000.0, 144_170_000.0),
];

/// Where the text modes are found, as (where, dial Hz): the dial is the
/// suppressed carrier, the signals a few hundred hertz to 3 kHz above it.
pub const PSK31_DIALS: &[(&str, f64)] = &[
    ("80 m", 3_580_000.0),
    ("40 m", 7_070_000.0),
    ("30 m", 10_142_000.0),
    ("20 m", 14_070_000.0),
    // Below FT8's 18.100, which took the old PSK31 spot.
    ("17 m", 18_097_000.0),
    ("15 m", 21_070_000.0),
    ("10 m", 28_120_000.0),
];

pub const RTTY_DIALS: &[(&str, f64)] = &[
    ("80 m", 3_580_000.0),
    ("40 m", 7_040_000.0),
    ("20 m", 14_080_000.0),
    ("15 m", 21_080_000.0),
    // Deutscher Wetterdienst's weather broadcasts, 50 baud and 450 Hz.
    ("DWD 10.1", 10_099_300.0),
    ("DWD 7.6", 7_644_500.0),
];

pub const NAVTEX_DIALS: &[(&str, f64)] = &[
    // The international channel and the national one, and the HF one;
    // the dial goes 1 kHz below so the tones sit at 1 kHz.
    ("518 kHz", 517_000.0),
    ("490 kHz", 489_000.0),
    ("4209.5 kHz", 4_208_500.0),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_s_protocol_names_land_in_their_families() {
        for (p, f) in [
            ("P25 Phase 1", Family::Voice),
            ("DMR Tier II", Family::Voice),
            ("Motorola SmartNet / SmartZone", Family::Voice),
            ("POCSAG Pager", Family::Paging),
            ("FLEX", Family::Paging),
            ("APRS", Family::Packet),
            ("RDS", Family::Broadcast),
            ("NOAA SAME Weather", Family::Broadcast),
            ("FT8", Family::HfData),
            ("FT4", Family::HfData),
            ("RTTY", Family::HfData),
            ("PSK31", Family::HfData),
            ("NAVTEX", Family::HfData),
            ("WEFAX", Family::HfData),
        ] {
            assert_eq!(family_of(p), Some(f), "{p}");
        }
        assert_eq!(family_of("Analog FM"), None);
    }

    #[test]
    fn every_family_has_a_decoder() {
        for f in Family::ALL {
            assert!(in_family(f).next().is_some(), "{f:?}");
        }
    }
}
