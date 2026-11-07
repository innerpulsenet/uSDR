//! Where to tune, for the mode the receiver is in, in one place: the band
//! keys of a mode heard on many bands (FT8 on 20 m, RTTY on 40 m), or the
//! station list of a mode whose stations keep their own frequencies
//! (radiofax, NAVTEX, HFDL), with the stations on the air now beside it.
//! The same lists feed the HF mode menu's quick-tune row, so a mode and
//! where to hear it are picked in one click.

use crate::app::App;
use crate::decoders;
use crate::stations_data as data;
use crate::directory::{self, Directory};
use crate::theme;
use crate::widgets;
use egui::{RichText, Ui};
use usdr_proto::Mode;

/// One place to tune: a short band name, the dial (Hz), and a longer
/// line for its tooltip.
#[derive(Clone, Debug, Default)]
pub struct Dial {
    pub band: String,
    pub hz: f64,
    pub note: &'static str,
}

/// A mode's dials, where it is heard on many bands.
pub fn dials(mode: Mode) -> Vec<Dial> {
    let d = |band: &str, hz: f64, note: &'static str| Dial { band: band.to_string(), hz, note };
    match mode {
        Mode::Ft8 => decoders::HF_DIALS.iter().map(|(b, ft8, _)| d(b, *ft8, "")).collect(),
        Mode::Ft4 => decoders::HF_DIALS.iter().map(|(b, _, ft4)| d(b, *ft4, "")).collect(),
        Mode::Psk31 => decoders::PSK31_DIALS.iter().map(|(b, hz)| d(b, *hz, "")).collect(),
        Mode::Rtty => decoders::RTTY_DIALS.iter().map(|(b, hz)| d(b, *hz, if b.starts_with("DWD") { "Deutscher Wetterdienst weather RTTY, 50 baud, 450 Hz shift" } else { "" })).collect(),
        Mode::Cw => data::CW_DIALS.iter().map(|(b, hz, n)| d(b, *hz, n)).collect(),
        Mode::Olivia => data::OLIVIA_DIALS.iter().map(|(b, hz, n)| d(b, *hz, n)).collect(),
        Mode::Hell => data::HELL_DIALS.iter().map(|(b, hz, n)| d(b, *hz, n)).collect(),
        Mode::Wspr => data::WSPR_DIALS.iter().map(|(b, hz, n)| d(b, *hz, n)).collect(),
        Mode::Js8 => data::JS8_DIALS.iter().map(|(b, hz, n)| d(b, *hz, n)).collect(),
        Mode::Sstv => data::SSTV_DIALS.iter().filter(|(_, _, sb)| *sb == "USB").map(|(b, hz, _)| d(b, *hz, "SSTV calling frequency (upper sideband)")).collect(),
        Mode::Pactor => decoders::PACTOR_DIALS.iter().map(|(b, hz)| d(b, *hz, "US automatic digital station segment (47 CFR 97.221)")).collect(),
        _ => Vec::new(),
    }
}

/// "20 m" → "20", for a key that has to be small: the row says BAND.
fn short(band: &str) -> String {
    band.trim_end_matches(" m").to_string()
}

/// Tune a dial in a mode: the mode first, then the presets a dial implies
/// (DWD's RTTY is 50 baud and 450 Hz), then the dial.
pub fn tune_dial(app: &mut App, mode: Mode, dial: &Dial) {
    if app.status.mode != mode {
        app.set_mode(mode);
    }
    if mode == Mode::Rtty {
        let dwd = dial.band.starts_with("DWD");
        app.post("/api/sdr/digital", serde_json::json!({ "rtty_baud": if dwd { 50.0 } else { 45.45 }, "rtty_shift_hz": if dwd { 450.0 } else { 170.0 } }));
    }
    app.tune(dial.hz, false);
}

/// A dial as the operator reads it: MHz to the kilohertz.
fn mhz(hz: f64) -> String {
    crate::tuning::fmt_mhz(hz, 4)
}

/// The strip for the receiver's mode, on the dock's key row.
pub fn strip(app: &mut App, ui: &mut Ui) {
    let mode = app.status.mode;
    if !mode.is_hf_data() {
        return;
    }
    let tally = widgets::protocol_color(mode.label());
    let rx = app.inspect_hz();
    let list = dials(mode);
    if !list.is_empty() {
        rule(ui);
        ui.label(RichText::new("BAND").font(theme::mono(9.5)).color(theme::text_3()));
        let mut pick = None;
        for dial in &list {
            let on = (rx - dial.hz).abs() < 50.0;
            let label = if dial.band.starts_with("DWD") { dial.band.clone() } else { short(&dial.band) };
            let mut tip = format!("{} {} · dial {} MHz", mode.label(), dial.band, mhz(dial.hz));
            if !dial.note.is_empty() {
                tip += &format!("\n{}", dial.note);
            }
            let w = if label.len() <= 3 { 30.0 } else { 0.0 };
            if widgets::chip_key_w(ui, &label, on, tally, w).on_hover_text(tip).clicked() {
                pick = Some(dial.clone());
            }
        }
        if let Some(d) = pick {
            tune_dial(app, mode, &d);
        }
    }
    if let Some(dir) = crate::stations::for_mode(mode) {
        rule(ui);
        stations(app, ui, dir, tally);
    }
}

fn rule(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(9.0, 18.0), egui::Sense::hover());
    ui.painter().line_segment([r.center_top(), r.center_bottom()], egui::Stroke::new(1.0, theme::line()));
}

/// A station list's menu, named for the station the receiver is on, and
/// the stations on the air now, a key each.
fn stations(app: &mut App, ui: &mut Ui, dir: &'static Directory, tally: egui::Color32) {
    let tuned = dir.tuned(app).map(|(st, khz)| format!("{} {}", st.call, freq_label(dir, khz)));
    let label = tuned.clone().unwrap_or_else(|| "STATIONS".into());
    widgets::menu_chip_lit(ui, &label, &["STATIONS"], tally, tuned.is_some(), |ui| {
        ui.set_width(470.0);
        directory::list(app, ui, dir, tally, true);
    });
    // On the air now: a station sending something its schedule names, on a
    // frequency it uses at this hour.
    let (day, now) = directory::utc_now();
    let mut live: Vec<(&directory::Station, f64, &'static str)> = dir
        .stations
        .iter()
        .filter_map(|st| {
            let (on, _) = directory::now_and_next(st.schedule, day, now);
            let on = on?;
            Some((st, directory::best_khz(st, now)?, on.slot.title))
        })
        .collect();
    live.truncate(3);
    let mut pick = None;
    for (st, khz, title) in live {
        let here = (app.inspect_hz() - dir.dial_hz(khz)).abs() < 50.0;
        let text = format!("● {} {}", st.call, if khz.fract() == 0.0 { format!("{khz:.0}") } else { format!("{khz:.1}") });
        if widgets::chip_key(ui, &text, here, theme::OK).on_hover_text(format!("On the air now: {}\n{} · {}", title, st.name, freq_label(dir, khz))).clicked() {
            pick = Some(khz);
        }
    }
    if let Some(k) = pick {
        directory::tune(app, dir, k);
    }
    if tuned.is_none() && dir.stations.iter().all(|s| s.schedule.is_empty()) {
        ui.label(RichText::new("pick a station").font(theme::mono(10.0)).color(theme::text_3()));
    }
}

/// A published frequency and, where the dial differs, the dial:
/// "8682.0 kHz (8680.1 USB)".
pub fn freq_label(dir: &Directory, khz: f64) -> String {
    let f = |k: f64| if k.fract() == 0.0 { format!("{k:.0}") } else { format!("{k:.1}") };
    if dir.dial_offset_hz == 0.0 { format!("{} kHz", f(khz)) } else { format!("{} kHz ({} USB)", f(khz), f(dir.dial_hz(khz) / 1e3)) }
}
