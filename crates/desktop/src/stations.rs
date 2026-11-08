//! The station lists the directory draws (see [`crate::directory`]): which
//! mode each list tunes and how its published frequencies sit against the
//! dial. The lists themselves are generated from sourced research (see
//! `crates/desktop/data/hf-directory-notes.md` and `tools/gen-stations.py`).

use crate::directory::Directory;
use crate::stations_data as data;
use usdr_proto::Mode;

/// The directory a mode's stations are listed in, where it has one.
pub fn for_mode(mode: Mode) -> Option<&'static Directory> {
    match mode {
        Mode::Wefax => Some(&WEFAX),
        Mode::Navtex => Some(&NAVTEX),
        Mode::Sitor => Some(&SITOR),
        Mode::Dsc => Some(&DSC),
        Mode::Hfdl => Some(&HFDL),
        Mode::Ale => Some(&ALE),
        Mode::Selcal => Some(&SELCAL),
        Mode::Mfsk => Some(&RADIOGRAM),
        Mode::Wwv => Some(&WWV),
        Mode::Drm => Some(&DRM),
        _ => None,
    }
}


pub const WEFAX: Directory = Directory {
    mode: Mode::Wefax,
    title: "RADIOFAX",
    dial_offset_hz: -1_900.0,
    source: "Schedules: NWS (tgftp.nws.noaa.gov/fax), DWD, NWS worldwide list for GYA; checked 2026-11-06. The NWS schedules date from 2012–2020 and give no lengths, so a chart shows as begun, not as running.",
    stations: data::WEFAX,
};

pub const NAVTEX: Directory = Directory {
    mode: Mode::Navtex,
    title: "NAVTEX",
    dial_offset_hz: -1_000.0,
    source: "Weather broadcast times from weather.gov (USCG) and Radio Aids to Marine Navigation 2026 (CCG); warnings go at any time. Checked 2026-11-06.",
    stations: data::NAVTEX,
};

pub const SITOR: Directory = Directory {
    mode: Mode::Sitor,
    title: "SITOR-B BROADCASTS",
    dial_offset_hz: -1_700.0,
    source: "High seas broadcasts from weather.gov (USCG) and Radio Aids to Marine Navigation 2026 (CCG); checked 2026-11-06.",
    stations: data::SITOR,
};

pub const DSC: Directory = Directory {
    mode: Mode::Dsc,
    title: "DSC",
    dial_offset_hz: -1_700.0,
    source: "Radio Aids to Marine Navigation 2026 (CCG); checked 2026-11-06. US Coast Guard watch stations could not be confirmed and are left out.",
    stations: data::DSC,
};

pub const HFDL: Directory = Directory {
    mode: Mode::Hfdl,
    title: "HFDL GROUND STATIONS",
    dial_offset_hz: 0.0,
    source: "System table version 52 (December 2023), as dumphfdl ships it; checked 2026-11-06. Each station uses a few of its frequencies at a time and names them in its squitters.",
    stations: data::HFDL,
};

pub const ALE: Directory = Directory {
    mode: Mode::Ale,
    title: "2G ALE NETS",
    dial_offset_hz: 0.0,
    source: "Amateur ALE channels from hflink.com, checked 2026-11-06. No military or government ALE list could be confirmed from an official source, so none is given; HFGCS's published frequencies are its voice channels.",
    stations: data::ALE,
};

pub const SELCAL: Directory = Directory {
    mode: Mode::Selcal,
    title: "OCEANIC HF VOICE",
    dial_offset_hz: 0.0,
    source: "North Atlantic and Caribbean HF families from the RadioReference MWARA page (last edited 2 August 2021; the NAT lists confirmed there November 2020). Community-kept, so assignments may have changed.",
    stations: data::SELCAL,
};

pub const RADIOGRAM: Directory = Directory {
    mode: Mode::Mfsk,
    title: "SHORTWAVE RADIOGRAM",
    dial_offset_hz: 0.0,
    source: "WRMI's schedule sheet effective 1 November 2026, checked 2026-11-06. It gives the slots, not the modes; each show uses several (MFSK32 and MFSK64 among them). Tune the dial to the frequency in USB; the decoder finds the signal.",
    stations: data::RADIOGRAM,
};

pub const WWV: Directory = Directory {
    mode: Mode::Wwv,
    title: "TIME STATIONS",
    dial_offset_hz: 0.0,
    source: "NIST (nist.gov), checked 2026-11-06.",
    stations: data::WWV,
};

pub const DRM: Directory = Directory {
    mode: Mode::Drm,
    title: "DRM BROADCASTS · B26",
    dial_offset_hz: 0.0,
    source: "drm.org schedule for B26 (updated 2026-10-26), checked 2026-11-06. None is aimed at North America; the Atlantic ones are the likeliest to be heard here.",
    stations: data::DRM,
};

