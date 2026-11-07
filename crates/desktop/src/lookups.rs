//! What the raw identifiers in decodes stand for: a ship's MMSI to its
//! flag and kind (and a coast station to its name), an aircraft's ICAO
//! address and registration to the state it is registered in, a flight
//! number to its airline, an ACARS label to what the message is, DRM's
//! country and language codes to names. The tables are published ones
//! (see `crates/desktop/data/lookups-notes.md`); what a table does not
//! hold is left as the decoder gave it, never guessed.

use crate::lookups_data as t;

/// The flag state of a Maritime Identification Digits code.
pub fn mid_country(mid: u16) -> Option<&'static str> {
    t::MID.iter().find(|(m, _)| *m == mid).map(|(_, c)| *c)
}

/// An MMSI's kind and where it is from, as ITU-R M.585 lays MMSIs out:
/// "ship · Liberia", "coast station · Spain", "EPIRB-AIS".
pub fn mmsi_kind(mmsi: &str) -> Option<(&'static str, Option<u16>)> {
    let d: String = mmsi.chars().filter(|c| c.is_ascii_digit()).collect();
    if d.len() != 9 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| d[r].parse::<u16>().ok();
    Some(match &d[..] {
        x if x.starts_with("970") => ("AIS-SART", None),
        x if x.starts_with("972") => ("man overboard device", None),
        x if x.starts_with("974") => ("EPIRB-AIS", None),
        x if x.starts_with("111") => ("SAR aircraft", n(3..6)),
        x if x.starts_with("00") => ("coast station", n(2..5)),
        x if x.starts_with('0') => ("group", n(1..4)),
        x if x.starts_with("98") => ("craft with a parent ship", n(2..5)),
        x if x.starts_with("99") => ("aid to navigation", n(2..5)),
        _ => ("ship", n(0..3)),
    })
}

/// A coast station's name, where the published list has it.
pub fn coast_name(mmsi: &str) -> Option<&'static str> {
    t::COAST.iter().find(|(m, _)| *m == mmsi).map(|(_, n)| *n)
}

/// An MMSI in words: "coast station · Spain", or with its name when the
/// list has it.
pub fn mmsi_line(mmsi: &str) -> String {
    if let Some(n) = coast_name(mmsi) {
        return n.to_string();
    }
    match mmsi_kind(mmsi) {
        Some((kind, mid)) => match mid.and_then(mid_country) {
            Some(c) => format!("{kind} · {c}"),
            None => kind.to_string(),
        },
        None => String::new(),
    }
}

/// ITU's MARS ship station search, to look an MMSI up in: it has no link
/// straight to a ship, so the page opens for the number to be entered.
pub fn mmsi_link(_mmsi: &str) -> Option<String> {
    (!t::MARS_SEARCH.is_empty()).then(|| t::MARS_SEARCH.to_string())
}

/// A public tracker's page for an aircraft by its ICAO address.
pub fn aircraft_link(hex: &str) -> Option<String> {
    (!t::AIRCRAFT_URL.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit())).then(|| t::AIRCRAFT_URL.replace("{hex}", &hex.to_ascii_lowercase()))
}

/// The state an ICAO 24-bit address block is allocated to.
pub fn icao_state(hex: &str) -> Option<&'static str> {
    let a = u32::from_str_radix(hex.trim(), 16).ok()?;
    t::ICAO24.iter().find(|(lo, hi, _)| (*lo..=*hi).contains(&a)).map(|(_, _, s)| *s)
}

/// The state of an aircraft registration, by its nationality mark: the
/// longest prefix that matches ("9V-SKA" Singapore, "N123AB" the US).
pub fn registration_state(reg: &str) -> Option<&'static str> {
    let r = reg.trim().trim_start_matches('.').to_ascii_uppercase();
    t::REG_PREFIX.iter().filter(|(p, _)| r.starts_with(p.trim_end_matches('-'))).max_by_key(|(p, _)| p.trim_end_matches('-').len()).map(|(_, s)| *s)
}

/// The airline of a flight number by its ICAO designator ("UAL901" United).
pub fn airline(flight: &str) -> Option<(&'static str, &'static str)> {
    let f = flight.trim().to_ascii_uppercase();
    if f.len() < 4 || !f[..3].chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    t::AIRLINES.iter().find(|(d, _, _)| *d == &f[..3]).map(|(_, n, c)| (*n, *c))
}

/// What an ACARS label means.
pub fn acars_label(label: &str) -> Option<&'static str> {
    t::ACARS_LABELS.iter().find(|(l, _)| *l == label).map(|(_, m)| *m)
}

pub fn country(code: &str) -> Option<&'static str> {
    let c = code.trim().to_ascii_uppercase();
    t::ISO3166.iter().find(|(k, _)| *k == c).map(|(_, n)| *n)
}

pub fn language(code: &str) -> Option<&'static str> {
    let c = code.trim().to_ascii_lowercase();
    t::ISO639.iter().find(|(k, _)| *k == c).map(|(_, n)| *n)
}

/// An HFDL decode's aircraft in words: flight and airline, registration
/// and its state, the address's state.
pub fn describe_hfdl(ev: &usdr_proto::DecodeEvent) -> String {
    let f = |k: &str| ev.fields.get(k).map(String::as_str).filter(|v| !v.is_empty());
    let mut parts = Vec::new();
    if let Some(fl) = f("flight") {
        parts.push(match airline(fl) {
            Some((name, _)) => format!("{fl} ({name})"),
            None => fl.to_string(),
        });
    }
    if let Some(reg) = f("reg") {
        let reg = reg.trim_start_matches('.');
        parts.push(match registration_state(reg) {
            Some(s) => format!("{reg}, {s}"),
            None => reg.to_string(),
        });
    }
    if let Some(icao) = f("icao") {
        parts.push(match icao_state(icao) {
            Some(s) => format!("ICAO {icao} ({s})"),
            None => format!("ICAO {icao}"),
        });
    }
    if let Some(l) = f("label") {
        if let Some(m) = acars_label(l) {
            parts.push(format!("label {l}: {m}"));
        }
    }
    if parts.is_empty() { ev.summary.clone() } else { parts.join(" · ") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_translate() {
        assert_eq!(mid_country(366), Some("United States of America"));
        assert!(mid_country(224).is_some_and(|c| c.contains("Spain")));
        assert!(registration_state("ET-ATH").is_some_and(|s| s.contains("Ethiopia")));
        assert!(registration_state("CS-TJE").is_some_and(|s| s.contains("Portugal")), "{:?}", registration_state("CS-TJE"));
        assert!(airline("UAL901").is_some());
        assert!(icao_state("A12345").is_some_and(|s| s.contains("United States")));
        assert_eq!(coast_name("003669995").is_some(), true);
        assert!(language("eng").is_some_and(|l| l.contains("English")));
    }

    #[test]
    fn mmsi_kinds_follow_m585() {
        assert_eq!(mmsi_kind("366123456").map(|k| k.0), Some("ship"));
        assert_eq!(mmsi_kind("366123456").and_then(|k| k.1), Some(366));
        assert_eq!(mmsi_kind("002241022").map(|k| k.0), Some("coast station"));
        assert_eq!(mmsi_kind("002241022").and_then(|k| k.1), Some(224));
        assert_eq!(mmsi_kind("036600001").map(|k| k.0), Some("group"));
        assert_eq!(mmsi_kind("970123456").map(|k| k.0), Some("AIS-SART"));
        assert_eq!(mmsi_kind("12345"), None);
    }
}
