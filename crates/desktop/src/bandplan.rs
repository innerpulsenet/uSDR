//! Allocations worth knowing at a glance, drawn as a strip along the
//! bottom of the panadapter.
//!
//! United States allocations, from the FCC table and the ARRL band plan:
//! the amateur bands, broadcast, aviation, marine, weather, satellites,
//! personal radio, public safety and paging. It labels what a frequency is
//! allocated to, not what is on it; the decoders say that.

use egui::Color32;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Amateur,
    Broadcast,
    Aviation,
    Marine,
    Weather,
    Personal,
    PublicSafety,
    Data,
}

impl Kind {
    pub fn color(self) -> Color32 {
        let rgb = |h: u32| Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8);
        // The brand's categorical chart order, in its ink set.
        match self {
            Kind::Broadcast => rgb(0x52e3ff),
            Kind::PublicSafety => rgb(0xff7a59),
            Kind::Marine => rgb(0xb38cff),
            Kind::Weather => rgb(0xffb347),
            Kind::Aviation => rgb(0x628dea),
            Kind::Amateur => rgb(0x7dffb0),
            Kind::Personal => rgb(0xf27ad8),
            Kind::Data => rgb(0xb0a99f),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Amateur => "Amateur",
            Kind::Broadcast => "Broadcast",
            Kind::Aviation => "Aviation",
            Kind::Marine => "Marine",
            Kind::Weather => "Weather and satellites",
            Kind::Personal => "Personal radio",
            Kind::PublicSafety => "Public safety and land mobile",
            Kind::Data => "Paging and data",
        }
    }
}

pub struct Band {
    pub lo_hz: f64,
    pub hi_hz: f64,
    pub name: &'static str,
    pub kind: Kind,
}

const fn b(lo_mhz: f64, hi_mhz: f64, name: &'static str, kind: Kind) -> Band {
    Band { lo_hz: lo_mhz * 1e6, hi_hz: hi_mhz * 1e6, name, kind }
}

pub const BANDS: &[Band] = &[
    b(0.535, 1.705, "AM broadcast", Kind::Broadcast),
    b(1.8, 2.0, "160 m", Kind::Amateur),
    b(3.5, 4.0, "80 m", Kind::Amateur),
    b(5.9, 6.2, "49 m broadcast", Kind::Broadcast),
    b(7.0, 7.3, "40 m", Kind::Amateur),
    b(9.4, 9.9, "31 m broadcast", Kind::Broadcast),
    b(10.1, 10.15, "30 m", Kind::Amateur),
    b(11.6, 12.1, "25 m broadcast", Kind::Broadcast),
    b(14.0, 14.35, "20 m", Kind::Amateur),
    b(15.1, 15.8, "19 m broadcast", Kind::Broadcast),
    b(18.068, 18.168, "17 m", Kind::Amateur),
    b(21.0, 21.45, "15 m", Kind::Amateur),
    b(24.89, 24.99, "12 m", Kind::Amateur),
    b(26.965, 27.405, "CB", Kind::Personal),
    b(28.0, 29.7, "10 m", Kind::Amateur),
    b(50.0, 54.0, "6 m", Kind::Amateur),
    b(88.0, 108.0, "FM broadcast", Kind::Broadcast),
    b(108.0, 118.0, "VOR / ILS", Kind::Aviation),
    b(118.0, 137.0, "Airband", Kind::Aviation),
    b(137.0, 138.0, "Weather satellites", Kind::Weather),
    b(144.0, 148.0, "2 m", Kind::Amateur),
    b(156.0, 162.025, "Marine VHF", Kind::Marine),
    b(162.4, 162.55, "NOAA Weather Radio", Kind::Weather),
    b(222.0, 225.0, "1.25 m", Kind::Amateur),
    b(420.0, 450.0, "70 cm", Kind::Amateur),
    b(462.5625, 462.725, "FRS / GMRS", Kind::Personal),
    b(467.5625, 467.725, "FRS / GMRS", Kind::Personal),
    b(769.0, 775.0, "700 MHz public safety", Kind::PublicSafety),
    b(851.0, 869.0, "800 MHz trunked", Kind::PublicSafety),
    b(902.0, 928.0, "33 cm", Kind::Amateur),
    b(929.0, 932.0, "Paging", Kind::Data),
    b(977.0, 979.0, "UAT (978)", Kind::Aviation),
    b(1089.0, 1091.0, "ADS-B (1090)", Kind::Aviation),
    b(1240.0, 1300.0, "23 cm", Kind::Amateur),
];

/// Single frequencies known by what is on them, as (Hz, ± Hz, name).
const SPOTS: &[(f64, f64, &str)] = &[
    (2.5e6, 1_500.0, "WWV time signal"),
    (3.330e6, 1_500.0, "CHU time signal"),
    (5.0e6, 1_500.0, "WWV time signal"),
    (7.850e6, 1_500.0, "CHU time signal"),
    (10.0e6, 1_500.0, "WWV time signal"),
    (14.670e6, 1_500.0, "CHU time signal"),
    (15.0e6, 1_500.0, "WWV time signal"),
    (20.0e6, 1_500.0, "WWV time signal"),
    (25.0e6, 1_500.0, "WWV time signal"),
    (121.5e6, 4_000.0, "Aviation emergency, 121.5"),
    (146.52e6, 4_000.0, "2 m national simplex"),
    (156.8e6, 6_000.0, "Marine channel 16, distress and calling"),
    (156.45e6, 6_000.0, "Marine channel 9, calling"),
    (162.400e6, 6_000.0, "NOAA Weather Radio"),
    (162.425e6, 6_000.0, "NOAA Weather Radio"),
    (162.450e6, 6_000.0, "NOAA Weather Radio"),
    (162.475e6, 6_000.0, "NOAA Weather Radio"),
    (162.500e6, 6_000.0, "NOAA Weather Radio"),
    (162.525e6, 6_000.0, "NOAA Weather Radio"),
    (162.550e6, 6_000.0, "NOAA Weather Radio"),
];

/// What a frequency is for, in a few words, from the spots and then the
/// allocations: what the receiver says when no decoder has named the
/// signal. `present` is whether a signal stands clear of the noise there.
/// The words say what the frequency is allocated to, never what was
/// decoded, so they are worded as a station or a band, not a protocol.
pub fn describe(hz: f64, mode: usdr_proto::Mode, present: bool) -> String {
    use usdr_proto::Mode;
    if let Some((_, _, name)) = SPOTS.iter().find(|(f, w, _)| (hz - f).abs() <= *w) {
        return if present { (*name).to_string() } else { format!("{name} · quiet") };
    }
    let band = BANDS.iter().find(|b| hz >= b.lo_hz && hz < b.hi_hz);
    let what = match band {
        Some(b) if b.name == "FM broadcast" && mode == Mode::Wfm => "FM broadcast station".to_string(),
        Some(b) if b.name == "AM broadcast" => "AM broadcast station".to_string(),
        Some(b) if b.name.ends_with(" broadcast") && b.name.contains(" m ") => format!("Shortwave broadcast, {}", b.name.trim_end_matches(" broadcast")),
        Some(b) if b.kind == Kind::Amateur => format!("{} amateur band", b.name),
        Some(b) => b.name.to_string(),
        None => return if present { "Signal".into() } else { "Noise".into() },
    };
    if present { what } else { format!("{what} · quiet") }
}

/// The allocations that overlap `lo_hz..hi_hz`.
pub fn within(lo_hz: f64, hi_hz: f64) -> impl Iterator<Item = &'static Band> {
    BANDS.iter().filter(move |b| b.hi_hz > lo_hz && b.lo_hz < hi_hz)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_ordered_and_sane() {
        for w in BANDS.windows(2) {
            assert!(w[0].lo_hz <= w[1].lo_hz, "{} before {}", w[0].name, w[1].name);
        }
        assert!(BANDS.iter().all(|b| b.hi_hz > b.lo_hz));
        let fm: Vec<&str> = within(100e6, 101e6).map(|b| b.name).collect();
        assert_eq!(fm, vec!["FM broadcast"]);
    }

    #[test]
    fn frequencies_are_described() {
        use usdr_proto::Mode;
        assert_eq!(describe(100.5e6, Mode::Wfm, true), "FM broadcast station");
        assert_eq!(describe(100.5e6, Mode::Nfm, false), "FM broadcast · quiet");
        assert_eq!(describe(15.31e6, Mode::Am, true), "Shortwave broadcast, 19 m");
        assert_eq!(describe(1.09e6, Mode::Am, true), "AM broadcast station");
        assert_eq!(describe(10.0e6, Mode::Am, true), "WWV time signal");
        assert_eq!(describe(162.55e6, Mode::Nfm, true), "NOAA Weather Radio");
        assert_eq!(describe(121.5e6, Mode::Am, false), "Aviation emergency, 121.5 · quiet");
        assert_eq!(describe(127.85e6, Mode::Am, true), "Airband");
        assert_eq!(describe(14.2e6, Mode::Usb, true), "20 m amateur band");
        assert_eq!(describe(46.0e6, Mode::Nfm, false), "Noise");
    }
}
