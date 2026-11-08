//! The receiver bar: one LCD across the window reading the receiver being
//! driven, zone by zone, and under each zone the keys that change what it
//! reads. It follows the active receiver, so switching to B turns the same
//! display and keys onto B, in B's color.

use crate::app::App;
use crate::net::Get;
use crate::theme::{self, alpha};
use crate::vfo::VfoAct;
use crate::widgets::{self, Fader};
use egui::{Rect, RichText, Sense, Stroke, Ui, vec2};
use serde_json::json;
use usdr_proto::{FilterShape, Mode};

const B_MODES: [Mode; 5] = [Mode::Nfm, Mode::Am, Mode::Usb, Mode::Lsb, Mode::Wfm];

fn small(t: impl Into<String>) -> RichText {
    RichText::new(t).font(theme::mono(10.5)).color(theme::text_3())
}

/// The key row under the LCD, and the gap between them.
const KEYS_H: f32 = 24.0;
const KEYS_GAP: f32 = 6.0;

/// How wide each zone of the LCD is, left to right.
#[derive(Clone, Copy, Debug)]
struct Zones {
    rx: f32,
    freq: f32,
    signal: f32,
    audio: f32,
    mode: f32,
    filter: f32,
    step: f32,
}

/// The digits' height for a bar `w` wide.
fn digits_h(w: f32) -> f32 {
    if w >= 1500.0 {
        72.0
    } else if w >= 1240.0 {
        56.0
    } else {
        46.0
    }
}

/// The zones of a bar `w` wide. The frequency zone is sized by its digits,
/// room left for a GHz digit; the others run from what they need at the
/// smallest window to what they use at the default one, and the signal
/// zone takes the rest. Past the default window the extra goes to the
/// zones that read finer for it.
fn zones(w: f32, digits: f32) -> Zones {
    let t = ((w - 1076.0) / 500.0).clamp(0.0, 1.0);
    let span = |a: f32, b: f32| a + (b - a) * t;
    let extra = (w - 1576.0).max(0.0);
    let freq = 4.46 * digits + 24.0;
    let (rx, audio, mode, filter, step) = (span(58.0, 92.0), span(112.0, 176.0) + 0.15 * extra, span(176.0, 236.0), span(118.0, 196.0) + 0.25 * extra, span(112.0, 192.0));
    Zones { rx, freq, signal: w - rx - freq - audio - mode - filter - step, audio, mode, filter, step }
}

/// A caption printed on the LCD.
fn caption(t: impl Into<String>, look: &crate::lcd::Look) -> RichText {
    RichText::new(t).font(theme::mono(9.5)).color(look.caption).extra_letter_spacing(1.0)
}

pub fn receiver_bar(app: &mut App, ui: &mut Ui) {
    let w = ui.available_width();
    let digits = digits_h(w);
    let band_h = if w >= 1500.0 { 106.0 } else { 94.0 };
    let z = zones(w, digits);
    let (area, _) = ui.allocate_exact_size(vec2(w, band_h + KEYS_GAP + KEYS_H), Sense::hover());
    let band = Rect::from_min_size(area.min, vec2(w, band_h));
    let keys_top = band.bottom() + KEYS_GAP;
    let look = app.lcd_look(app.active_b && app.status.rx2.is_some());
    crate::vfo::lcd_back_look(ui.painter(), band, &look);
    // The zones' edges, the first the LCD's own.
    let mut edges = [band.left(); 8];
    for (i, width) in [z.rx, z.freq, z.signal, z.audio, z.mode, z.filter, z.step].into_iter().enumerate() {
        edges[i + 1] = edges[i] + width;
    }
    for x in &edges[1..7] {
        ui.painter().line_segment([egui::pos2(*x, band.top() + 8.0), egui::pos2(*x, band.bottom() - 8.0)], Stroke::new(1.0, look.rule));
    }
    let lcd_rect = |i: usize| Rect::from_min_max(egui::pos2(edges[i] + if i == 0 { 10.0 } else { 9.0 }, band.top() + 6.0), egui::pos2(edges[i + 1] - 8.0, band.bottom() - 5.0));
    let keys_rect = |i: usize, j: usize| Rect::from_min_max(egui::pos2(edges[i] + if i == 0 { 10.0 } else { 9.0 }, keys_top), egui::pos2(edges[j + 1] - 8.0, keys_top + KEYS_H));
    let lcd_ui = |ui: &mut Ui, name: &str, i: usize| ui.new_child(egui::UiBuilder::new().id_salt(("band", name)).max_rect(lcd_rect(i)).layout(egui::Layout::top_down(egui::Align::Min)));
    let row = egui::Layout::left_to_right(egui::Align::Center);
    let keys_ui = |ui: &mut Ui, name: &str, i: usize, j: usize| ui.new_child(egui::UiBuilder::new().id_salt(("band-keys", name)).max_rect(keys_rect(i, j)).layout(row));

    let (mut lcd, mut keys) = (lcd_ui(ui, "rx", 0), keys_ui(ui, "rx", 0, 0));
    rx(app, &mut lcd, &mut keys, look, band_h);
    let mut lcd = lcd_ui(ui, "freq", 1);
    frequency(app, &mut lcd, digits, look);
    let mut lcd = lcd_ui(ui, "signal", 2);
    signal(app, &mut lcd, look);
    // The memories are frequencies: their keys sit under the frequency and
    // the signal, with B's own keys ahead of them while B runs.
    let mut keys = keys_ui(ui, "memories", 1, 2);
    keys.spacing_mut().item_spacing.x = 4.0;
    b_keys(app, &mut keys);
    if app.prefs.show_memories {
        let room = keys.available_width();
        memory_chips(app, &mut keys, room);
    }
    let (mut lcd, mut keys) = (lcd_ui(ui, "audio", 3), keys_ui(ui, "audio", 3, 3));
    audio(app, &mut lcd, &mut keys, look);
    let (mut lcd, mut keys) = (lcd_ui(ui, "mode", 4), keys_ui(ui, "mode", 4, 4));
    modes(app, &mut lcd, &mut keys, look, band_h);
    let (mut lcd, mut keys) = (lcd_ui(ui, "filter", 5), keys_ui(ui, "filter", 5, 5));
    filter(app, &mut lcd, &mut keys, look);
    let (mut lcd, mut keys) = (lcd_ui(ui, "step", 6), keys_ui(ui, "step", 6, 6));
    tuning(app, &mut lcd, &mut keys, look, band_h);
    crate::vfo::lcd_glass(ui.painter(), band);
}

/// Which receiver the bar is driving, large, and the keys that choose it.
fn rx(app: &mut App, lcd: &mut Ui, keys: &mut Ui, look: crate::lcd::Look, band_h: f32) {
    let has_b = app.status.rx2.is_some();
    let is_b = app.active_b && has_b;
    let lit = look.digits;
    lcd.spacing_mut().item_spacing.y = 2.0;
    lcd.label(caption("RX", &look));
    lcd.label(RichText::new(if is_b { "B" } else { "A" }).font(theme::mono_bold(if band_h > 100.0 { 42.0 } else { 34.0 })).color(lit));
    let other = match (has_b, is_b) {
        (false, _) => "B OFF",
        (true, true) => "A ON",
        (true, false) => "B ON",
    };
    lcd.label(RichText::new(other).font(theme::mono(10.0)).color(alpha(lit, if has_b { 0.8 } else { 0.32 })));
    keys.spacing_mut().item_spacing.x = 3.0;
    let half = (keys.available_width() - 3.0) / 2.0;
    if widgets::chip_key_w(keys, "A", !is_b, theme::AMBER, half).on_hover_text("Drive receiver A (B toggles)").clicked() {
        app.active_b = false;
    }
    if has_b {
        if widgets::chip_key_w(keys, "B", is_b, theme::VIOLET, half).on_hover_text("Drive receiver B (B toggles)").clicked() {
            app.active_b = true;
        }
    } else {
        let can = app.rx2_available();
        let r = keys
            .add_enabled_ui(can, |ui| widgets::chip_key_w(ui, "+B", false, theme::VIOLET, half))
            .inner
            .on_hover_text("Start receiver B beside A (or middle-click the display)")
            .on_disabled_hover_text("This server has no second receiver");
        if r.clicked() {
            start_b(app);
        }
    }
}

/// B's own keys, only while it runs.
fn b_keys(app: &mut App, ui: &mut Ui) {
    if app.status.rx2.is_none() {
        return;
    }
    if widgets::chip_key(ui, "SWAP", false, theme::VIOLET).on_hover_text("Trade frequencies between A and B").clicked()
        && let Some(b) = app.status.rx2.clone()
    {
        let a = app.inspect_hz();
        app.active_b = false;
        app.tune(b.freq_hz, false);
        app.set_rx2(a, b.mode);
    }
    // One ear each, for following two channels at once.
    let split = app.prefs.pan <= -0.99 && app.prefs.pan_b >= 0.99;
    if widgets::chip_key(ui, "L|R", split, theme::VIOLET).on_hover_text(if split { "Both receivers back to the center" } else { "A in the left ear, B in the right" }).clicked() {
        let (a, b) = if split { (0.0, 0.0) } else { (-1.0, 1.0) };
        app.prefs.pan = a;
        app.prefs.pan_b = b;
        app.player.set_pan(a);
        app.player_b.set_pan(b);
        app.sync_fold();
    }
    if widgets::chip_key(ui, "✕", false, theme::VIOLET).on_hover_text("Stop receiver B").clicked() {
        app.rx2_off();
    }
    ui.add_space(6.0);
}

fn start_b(app: &mut App) {
    let a = app.inspect_hz();
    let span = (app.status.max_freq_hz - app.status.min_freq_hz).max(0.0);
    let hz = if a + span / 8.0 < app.status.max_freq_hz { a + span / 8.0 } else { a - span / 8.0 };
    let step = crate::tuning::mode_raster_hz(app.status.mode, hz);
    app.place_b(crate::tuning::snap(hz, step));
    app.active_b = true;
}

/// The frequency zone: the annunciators and the digits.
fn frequency(app: &mut App, ui: &mut Ui, digits_px: f32, look: crate::lcd::Look) {
    let b = app.status.rx2.clone().filter(|_| app.active_b);
    let hz = app.active_hz();
    let mut acts = Vec::new();
    let rm = app.reduce_motion();
    let (mode, bw) = match &b {
        Some(b) => (b.mode, b.bandwidth_hz),
        None => (app.status.mode, app.status.bandwidth_hz),
    };
    let gain = match (app.status.gain_db, app.status.tuner_agc) {
        (None, true) => if app.status.is_sdrplay() { "IF AGC".to_string() } else { "TUNER AGC".into() },
        (None, false) => "AGC".into(),
        (Some(g), _) => format!("MAN {g:.0} dB"),
    };
    let is_b = b.is_some();
    let rec = if is_b { app.player_b.reading().recording.is_some() } else { app.player.reading().recording.is_some() };
    app.vfo.tint = Some(look.accent);
    app.vfo.look = Some(look);
    app.vfo.bare = true;
    app.vfo.annun = crate::vfo::Annunciators {
        rx: if is_b { "B" } else { "A" },
        mode: mode.label().to_string(),
        bw: crate::tuning::fmt_hz(bw),
        gain,
        // Only what is worth seeing beside the frequency: a recording, and
        // that the next memory key stores.
        flags: if app.memory_armed { vec![("PICK A SLOT", true, look.accent), ("● REC", rec, theme::EMBER)] } else { vec![("● REC", rec, theme::EMBER)] },
    };
    app.vfo.well_h = ui.available_height();
    app.vfo.ui(ui, hz, digits_px, rm, &mut acts);
    for a in acts {
        match a {
            VfoAct::Step(d) => {
                let hz = app.active_hz() + d;
                app.tune(hz, true);
            }
            VfoAct::Set(hz) => app.tune(hz, false),
        }
    }
}

/// The step: what the digits and the wheel move by, read on the LCD, chosen
/// by a touch on it, and the jog keys under it.
fn tuning(app: &mut App, lcd: &mut Ui, keys: &mut Ui, look: crate::lcd::Look, band_h: f32) {
    let hz = app.active_hz();
    let lit = look.value;
    let auto = crate::tuning::mode_raster_hz(app.status.mode, hz);
    let (word, value) = match app.prefs.step_hz {
        None => ("AUTO", crate::tuning::fmt_short_hz(auto)),
        Some(s) => ("SET", crate::tuning::fmt_short_hz(s)),
    };
    let width = lcd.available_width();
    lcd.spacing_mut().item_spacing.y = 2.0;
    lcd.horizontal(|ui| {
        ui.label(caption("STEP", &look));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(word).font(theme::mono(10.0)).color(lit));
        });
    });
    let font = theme::mono_bold(if band_h > 100.0 { 34.0 } else { 26.0 });
    let (rect, resp) = lcd.allocate_exact_size(vec2(width, font.size + 8.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, format!("{word} {value}")));
    let p = lcd.painter();
    let shown = p.text(rect.left_center(), egui::Align2::LEFT_CENTER, &value, font, lit);
    let unit = p.text(egui::pos2(shown.right() + 5.0, rect.center().y + 6.0), egui::Align2::LEFT_CENTER, "Hz", theme::mono(10.0), alpha(lit, 0.66));
    p.text(egui::pos2(unit.right() + 5.0, unit.center().y), egui::Align2::LEFT_CENTER, "▾", theme::mono(11.0), alpha(lit, if resp.hovered() { 1.0 } else { 0.66 }));
    if resp.hovered() {
        lcd.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    egui::Popup::menu(&resp).id(egui::Id::new("step-menu")).width(width.max(180.0)).show(|ui| {
        if ui.selectable_label(app.prefs.step_hz.is_none(), format!("Auto · mode raster ({})", crate::tuning::fmt_short_hz(auto))).clicked() {
            app.prefs.step_hz = None;
            ui.close();
        }
        for s in crate::tuning::STEPS_HZ {
            if ui.selectable_label(app.prefs.step_hz == Some(s), crate::tuning::fmt_hz(s)).clicked() {
                app.prefs.step_hz = Some(s);
                ui.close();
            }
        }
    });
    let lock = app.prefs.span_lock;
    // A touch on the tag sets it too, for a window too narrow for its key.
    let tag = widgets::lcd_tag_in(lcd, "SPAN LOCK", &look, lock).on_hover_text("Keep the span where it is: tuning moves only the receiver (L)");
    if tag.hovered() {
        lcd.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if tag.clicked() {
        app.prefs.span_lock = !lock;
    }
    // The jog keys either side of the strip, « ‹ [strip] › », then the lock,
    // the least needed giving way first in a narrow window.
    keys.spacing_mut().item_spacing.x = 3.0;
    let chev = keys.painter().layout_no_wrap("«".into(), widgets::key_font(), egui::Color32::PLACEHOLDER).size().x + 10.0;
    let lock_w = keys.painter().layout_no_wrap("LOCK".into(), widgets::key_font(), egui::Color32::PLACEHOLDER).size().x + 12.0;
    let room = keys.available_width();
    let has_lock = room >= 2.0 * chev + lock_w + 3.0 * 3.0 + 28.0;
    let has_tens = room >= 4.0 * chev + lock_w + 5.0 * 3.0 + 28.0;
    let jogs = if has_tens { 4.0 } else { 2.0 };
    let strip_w = room - jogs * (chev + 3.0) - if has_lock { lock_w + 3.0 } else { 0.0 };
    let jog = |ui: &mut Ui, app: &mut App, lbl: &str, n: f64| {
        if widgets::chip_key_w(ui, lbl, false, theme::AMBER, chev).on_hover_text(format!("{n:+} step · Shift for a tenth")).clicked() {
            // On the raster, as a click and the wheel land: a receiver left
            // between two points is back on one at the first press.
            let st = crate::tuning::jog_step(app.step_hz(), ui.input(|i| i.modifiers.shift));
            let target = crate::tuning::step_from(app.active_hz(), n, st);
            app.tune(target, true);
        }
    };
    if has_tens {
        jog(keys, app, "«", -10.0);
    }
    jog(keys, app, "‹", -1.0);
    if let Some(d) = crate::vfo::tune_strip(keys, hz, app.step_hz(), strip_w, 22.0) {
        let st = crate::tuning::jog_step(app.step_hz(), keys.input(|i| i.modifiers.shift));
        let target = crate::tuning::step_from(app.active_hz(), (d / st).round(), st);
        app.tune(target, true);
    }
    jog(keys, app, "›", 1.0);
    if has_tens {
        jog(keys, app, "»", 10.0);
    }
    if has_lock
        && widgets::chip_key_named(keys, "LOCK", "SPAN LOCK", lock, theme::AMBER, lock_w)
            .on_hover_text("Keep the span where it is: tuning moves only the receiver (L)")
            .clicked()
    {
        app.prefs.span_lock = !lock;
    }
}

/// The modes a person listens to; every other mode is a decoder, behind
/// the DATA key.
const VOICE: [Mode; 5] = [Mode::Nfm, Mode::Am, Mode::Usb, Mode::Lsb, Mode::Wfm];

fn is_data(m: Mode) -> bool {
    !VOICE.contains(&m)
}

/// The decoders as the DATA key's menu lays them out: by who is on the
/// air, a card of tiles each, two cards to a row.
const DATA_DOMAINS: &[(&str, &[Mode])] = &[
    ("VHF · UHF DIGITAL VOICE", &[Mode::P25, Mode::Dmr, Mode::Nxdn]),
    ("PAGING & PACKET", &[Mode::Packet, Mode::Pager, Mode::Flex]),
    ("AMATEUR · WEAK SIGNAL & KEYBOARD", &[Mode::Ft8, Mode::Ft4, Mode::Wspr, Mode::Fst4w, Mode::Js8, Mode::Psk31, Mode::Rtty, Mode::Cw, Mode::Olivia, Mode::Contestia, Mode::Thor, Mode::Dominoex, Mode::Mt63, Mode::Mfsk, Mode::Hell, Mode::Sstv]),
    ("MARITIME & WEATHER", &[Mode::Ais, Mode::Dsc, Mode::Navtex, Mode::Sitor, Mode::Wefax]),
    ("AVIATION, MILITARY & DATA LINKS", &[Mode::Hfdl, Mode::Selcal, Mode::Ale, Mode::Stanag, Mode::Pactor]),
    ("TIME & BROADCAST", &[Mode::Wwv, Mode::Drm]),
    ("SEARCH", &[Mode::Auto, Mode::Scan]),
];

/// A mode's tile line under its name: what it is on the air, in a few
/// words, as the decoder takes it.
fn hf_spec(m: Mode) -> &'static str {
    match m {
        Mode::Ft8 => "3 kHz · 15 s",
        Mode::Ft4 => "3 kHz · 7.5 s",
        Mode::Psk31 => "every signal",
        Mode::Rtty => "45–75 bd",
        Mode::Wefax => "120 LPM charts",
        Mode::Navtex => "518 · 490 kHz",
        Mode::Sitor => "100 bd ARQ/FEC",
        Mode::Dsc => "100 bd GMDSS",
        Mode::Hfdl => "PSK · ACARS",
        Mode::Stanag => "4285 · 110A",
        Mode::Ale => "8-FSK · 125 bd",
        Mode::Cw => "Morse · skimmer",
        Mode::Js8 => "FT8-style chat",
        Mode::Wspr => "2 min · beacons",
        Mode::Fst4w => "2–30 min · beacons",
        Mode::Olivia => "MFSK · deep fades",
        Mode::Contestia => "MFSK · faster",
        Mode::Dominoex => "IFK · Micro–22",
        Mode::Thor => "IFK+FEC · Micro–22",
        Mode::Mt63 => "64 tones · nets",
        Mode::Selcal => "tone pairs · voice",
        Mode::Mfsk => "text · pictures",
        Mode::Hell => "on-off · by eye",
        Mode::Sstv => "color pictures",
        Mode::Pactor => "PACTOR-I",
        Mode::Wwv => "100 Hz code",
        Mode::Drm => "OFDM · AAC",
        Mode::Ais => "VHF ships · 2 ch",
        Mode::P25 => "Phase 1 · C4FM",
        Mode::Dmr => "Tier II · TDMA",
        Mode::Nxdn => "4-FSK · 6.25k",
        Mode::Packet => "AX.25 · APRS",
        Mode::Pager => "POCSAG",
        Mode::Flex => "FLEX paging",
        Mode::Auto => "every decoder",
        Mode::Scan => "voice scanner",
        _ => "",
    }
}

/// One key on the mode board: a mode, or the key that lists the decoders.
#[derive(Clone, Copy, PartialEq)]
enum Key {
    Mode(Mode),
    Data,
}

/// The mode board for receiver A: one row of tall keys, the five modes a
/// person listens to and DATA, which opens every decoder. Only the modes
/// the server offers, so an older one is never asked for one it lacks.
fn board_a(app: &App) -> Vec<Vec<Vec<Key>>> {
    let offered = |m: Mode| app.modes.is_empty() || app.modes.iter().any(|c| c.mode == m);
    let voice: Vec<Key> = VOICE.iter().copied().filter(|m| offered(*m)).map(Key::Mode).collect();
    let data: Vec<Key> = if Mode::ALL.iter().any(|m| is_data(*m) && offered(*m)) { vec![Key::Data] } else { vec![] };
    vec![vec![voice, data].into_iter().filter(|t| !t.is_empty()).collect()]
}

/// Receiver B listens only: one track.
fn board_b() -> Vec<Vec<Vec<Key>>> {
    vec![vec![B_MODES.iter().copied().map(Key::Mode).collect()]]
}

const KEY_FONT: f32 = 10.5;
const KEY_PAD: f32 = 12.0;
const KEY_H: f32 = 22.0;
const TRACK_GAP: f32 = 6.0;
const ROW_GAP: f32 = 4.0;

fn key_label(k: Key, _current: Mode) -> String {
    match k {
        Key::Mode(m) => m.label().to_string(),
        Key::Data => "DATA ▾".into(),
    }
}

/// A key's own width: its label and padding.
fn key_width(ui: &Ui, k: Key) -> f32 {
    let w = |t: String| ui.painter().layout_no_wrap(t, theme::mono_bold(KEY_FONT), egui::Color32::PLACEHOLDER).size().x + KEY_PAD;
    match k {
        Key::Mode(m) => w(m.label().to_string()),
        Key::Data => w("DATA ▾".into()),
    }
}

fn row_width(ui: &Ui, row: &[Vec<Key>]) -> f32 {
    row.iter().map(|t| t.iter().map(|k| key_width(ui, *k)).sum::<f32>()).sum::<f32>() + TRACK_GAP * row.len().saturating_sub(1) as f32
}

const B_NOTE: &str = "B listens; decoders on A";

/// The mode's name on the LCD at the most it can be in `room`, so the
/// longest decoder's fits the narrowest zone.
fn mode_size(room: f32, label: &str, most: f32) -> f32 {
    // (The mono face's advance is six tenths of its size.)
    (room / (label.chars().count() as f32 * 0.6)).min(most)
}

fn modes(app: &mut App, lcd: &mut Ui, keys: &mut Ui, look: crate::lcd::Look, band_h: f32) {
    let b = app.status.rx2.clone().filter(|_| app.active_b);
    let lit = look.value;
    let cur = b.as_ref().map_or(app.status.mode, |b| b.mode);
    let data = is_data(cur);
    let width = lcd.available_width();
    lcd.spacing_mut().item_spacing.y = 1.0;
    lcd.label(caption("MODE", &look));
    let tag_w = if data { 44.0 } else { 0.0 };
    let label = cur.label();
    let size = mode_size(width - tag_w - 2.0, label, if band_h > 100.0 { 34.0 } else { 26.0 });
    let line_h = if band_h > 100.0 { 44.0 } else { 34.0 };
    lcd.allocate_ui_with_layout(vec2(width, line_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.label(RichText::new(label).font(theme::mono_bold(size)).color(lit));
        if data {
            ui.add_space(4.0);
            widgets::lcd_tag_in(ui, "DATA", &look, true);
        }
    });
    if b.is_some() {
        lcd.add(egui::Label::new(RichText::new(B_NOTE).font(theme::mono(9.5)).color(alpha(lit, 0.66))).truncate());
    } else if width >= 170.0 {
        // Printed on the glass, the family lit; the keys under it are what
        // a screen reader finds.
        let (row, _) = lcd.allocate_exact_size(vec2(width, 13.0), Sense::hover());
        let p = lcd.painter();
        let mut x = row.left();
        for (name, on) in VOICE.iter().map(|m| (m.label(), *m == cur)).chain([("DATA", data)]) {
            x = p.text(egui::pos2(x, row.center().y), egui::Align2::LEFT_CENTER, name, theme::mono(9.5), alpha(lit, if on { 1.0 } else { 0.3 })).right() + 8.0;
        }
    }
    let board = keys.available_width();
    keys.spacing_mut().item_spacing.x = 0.0;
    match b {
        Some(b) => {
            if let Some(Key::Mode(m)) = mode_board(app, keys, "b", &board_b(), b.mode, board, theme::VIOLET)
                && b.mode != m
            {
                app.set_rx2(b.freq_hz, m);
            }
        }
        None => {
            let picked = mode_board(app, keys, "a", &board_a(app), cur, board, egui::Color32::TRANSPARENT);
            if let Some((m, dial)) = take_quick_tune(keys) {
                crate::tunestrip::tune_dial(app, m, &dial);
                return;
            }
            if let Some(Key::Mode(m)) = picked
                && cur != m
            {
                app.set_mode(m);
            }
        }
    }
}

/// Draw a mode board `width` wide and return the key clicked (a mode,
/// including one picked from the HF key's menu). Each family is one block
/// of joined keys; every row is stretched to the same width, so the blocks
/// end flush. One lit key glides to the choice, so the change is seen as a
/// move and not a blink.
fn mode_board(app: &App, ui: &mut Ui, id: &str, rows: &[Vec<Vec<Key>>], current: Mode, width: f32, tint: egui::Color32) -> Option<Key> {
    let key_h = KEY_H;
    let id = egui::Id::new(("mode-board", id));
    let font = theme::mono_bold(KEY_FONT);
    let h = rows.len() as f32 * key_h + ROW_GAP * rows.len().saturating_sub(1) as f32;
    let (area, _) = ui.allocate_exact_size(vec2(width, h), Sense::hover());
    let p = ui.painter().clone();
    // Lay out every key: the stretch is shared evenly by a row's keys.
    let mut cells: Vec<(Key, egui::Rect, egui::CornerRadius)> = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        let keys = row.iter().map(Vec::len).sum::<usize>().max(1) as f32;
        // Shared evenly by a row's keys: a narrow zone takes it from their
        // padding, never from the keys beside it.
        let extra = ((width - row_width(ui, row)) / keys).max(-KEY_PAD + 2.0);
        let top = area.top() + r as f32 * (key_h + ROW_GAP);
        let mut x = area.left();
        for track in row {
            let tw = track.iter().map(|k| key_width(ui, *k) + extra).sum::<f32>();
            let tr = egui::Rect::from_min_size(egui::pos2(x, top), vec2(tw, key_h));
            let mut kx = tr.left();
            for (i, k) in track.iter().enumerate() {
                let kw = key_width(ui, *k) + extra;
                cells.push((*k, egui::Rect::from_min_size(egui::pos2(kx, tr.top()), vec2(kw, key_h)), widgets::joined_corners(i, track.len())));
                kx += kw;
            }
            x += tw + TRACK_GAP;
        }
    }
    let lit_key = if is_data(current) && cells.iter().any(|(k, _, _)| *k == Key::Data) { Key::Data } else { Key::Mode(current) };
    // Every key up first, so the lit one can slide across them.
    let want = if tint == egui::Color32::TRANSPARENT { theme::AMBER } else { theme::on_color(tint) };
    let mut states = Vec::with_capacity(cells.len());
    for (k, cell, corners) in &cells {
        let label = key_label(*k, current);
        let resp = ui.interact(*cell, id.with(label.as_str()), Sense::click());
        let on = *k == lit_key;
        let shown = match k {
            Key::Mode(m) => m.label(),
            Key::Data => "DATA",
        };
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, on, shown));
        let hover = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() && !on, 0.12);
        let state = theme::KeyState { hover, on: 0.0, press: 0.0, color: want, focus: resp.has_focus() };
        theme::key_body(&p, cell.shrink2(vec2(0.5, 0.0)), *corners, state);
        states.push((resp, state));
    }
    if let Some((_, c, corners)) = cells.iter().find(|(k, _, _)| *k == lit_key) {
        let reduce = ui.data(|d| d.get_temp::<bool>(egui::Id::new("usdr-reduce-motion")).unwrap_or(false));
        let t = if reduce { 0.0 } else { 0.18 };
        let anim = |name: &str, v: f32| ui.ctx().animate_value_with_time(id.with(name), v, t);
        let rect = egui::Rect::from_min_max(egui::pos2(anim("l", c.left()), anim("t", c.top())), egui::pos2(anim("r", c.right()), anim("b", c.bottom())));
        let col = egui::Color32::from_rgb(anim("cr", want.r() as f32) as u8, anim("cg", want.g() as f32) as u8, anim("cb", want.b() as f32) as u8);
        theme::key_body(&p, rect.shrink2(vec2(0.5, 0.0)), *corners, theme::KeyState { hover: 0.0, on: 1.0, press: 0.0, color: col, focus: false });
    }
    let mut out = None;
    for ((k, cell, _), (resp, state)) in cells.iter().zip(states) {
        let label = key_label(*k, current);
        let on = *k == lit_key;
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let c = theme::key_text(theme::KeyState { on: if on { 1.0 } else { 0.0 }, ..state }, true);
        p.text(cell.center() + vec2(0.0, if on { 0.6 } else { 0.0 }), egui::Align2::CENTER_CENTER, &label, font.clone(), c);
        let desc = |m: Mode| app.modes.iter().find(|c| c.mode == m).map(|c| c.description.clone()).filter(|d| !d.is_empty());
        match k {
            Key::Mode(m) => {
                let resp = match desc(*m) {
                    Some(d) => resp.on_hover_text(d),
                    None => resp,
                };
                if resp.clicked() {
                    out = Some(*k);
                }
            }
            Key::Data => {
                if let Some(m) = egui::Popup::menu(&resp).show(|ui| data_matrix(app, ui, current)).and_then(|r| r.inner) {
                    out = Some(Key::Mode(m));
                }
            }
        }
    }
    out
}

/// The DATA key's menu: every decoder in domain cards, a tile each with
/// what the mode is, and under them the bands or stations of the mode the
/// pointer is on, so a mode and where to hear it are one click. A tile
/// returns its mode; a band or station key is left for the caller to tune
/// (see [`take_quick_tune`]).
fn data_matrix(app: &App, ui: &mut Ui, current: Mode) -> Option<Mode> {
    let offered = |m: Mode| app.modes.is_empty() || app.modes.iter().any(|c| c.mode == m);
    let desc = |m: Mode| app.modes.iter().find(|c| c.mode == m).map(|c| c.description.clone()).filter(|d| !d.is_empty());
    let hover_id = egui::Id::new("hf-matrix-hover");
    let mut focus: Mode = ui.data(|d| d.get_temp(hover_id)).unwrap_or(if is_data(current) { current } else { Mode::Ft8 });
    let mut out = None;
    const TILE: f32 = 104.0;
    ui.set_width(2.0 * (4.0 * TILE + 3.0 * 4.0) + 3.0 * 10.0);
    ui.label(RichText::new("DATA · EVERY DECODER").font(theme::mono_bold(11.0)).color(theme::text_2()));
    ui.add_space(4.0);
    // Cards with nothing the server offers are left out, and the rest
    // paired up again so no row holds a gap.
    let domains: Vec<&(&str, &[Mode])> = DATA_DOMAINS.iter().filter(|(_, ms)| ms.iter().any(|m| offered(*m))).collect();
    for pair in domains.chunks(2) {
        ui.horizontal_top(|ui| {
            for &&(title, modes) in pair {
                egui::Frame::new().fill(theme::surface_2()).stroke(Stroke::new(1.0, theme::line())).corner_radius(egui::CornerRadius::same(4)).inner_margin(egui::Margin::same(6)).show(ui, |ui| ui.vertical(|ui| {
                    ui.set_width(4.0 * TILE + 3.0 * 4.0);
                    ui.label(RichText::new(title).font(theme::mono(9.5)).color(theme::text_3()).extra_letter_spacing(0.6));
                    ui.add_space(3.0);
                    // Four tiles to a row; a domain with more takes rows.
                    ui.horizontal_wrapped(|ui| {
                        ui.set_max_width(4.0 * TILE + 3.0 * 4.0 + 1.0);
                        ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
                        for &m in modes.iter().filter(|m| offered(**m)) {
                            let r = hf_tile(ui, m, current == m, TILE);
                            if r.hovered() {
                                focus = m;
                            }
                            let r = match desc(m) {
                                Some(d) => r.on_hover_text(d),
                                None => r,
                            };
                            if r.clicked() {
                                out = Some(m);
                                ui.close();
                            }
                        }
                    });
                }));
            }
        });
        ui.add_space(6.0);
    }
    ui.data_mut(|d| d.insert_temp(hover_id, focus));
    // Where to hear the mode under the pointer.
    egui::Frame::new().fill(alpha(theme::ink(), 0.6)).stroke(Stroke::new(1.0, theme::line())).corner_radius(egui::CornerRadius::same(4)).inner_margin(egui::Margin::same(6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let dials = crate::tunestrip::dials(focus);
        let dir = crate::stations::for_mode(focus);
        let what = if !dials.is_empty() { "BANDS" } else if dir.is_some() { "STATIONS" } else { "" };
        ui.label(RichText::new(format!("{} {what} · a key sets the mode and tunes", focus.label())).font(theme::mono(9.5)).color(theme::text_3()).extra_letter_spacing(0.4));
        ui.add_space(2.0);
        let tally = crate::widgets::protocol_color(focus.label());
        let mut pick: Option<crate::tunestrip::Dial> = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for dial in &dials {
                let on = app.status.mode == focus && (app.inspect_hz() - dial.hz).abs() < 50.0;
                let text = format!("{} {}", dial.band.trim_end_matches(" m"), crate::tuning::fmt_mhz(dial.hz, 3));
                if widgets::chip_key(ui, &text, on, tally).clicked() {
                    pick = Some(dial.clone());
                }
            }
            if let Some(dir) = dir {
                // The stations on the air now, then the rest, a frequency
                // each that they use at this hour.
                let (day, now) = crate::directory::utc_now();
                let mut list: Vec<(&crate::directory::Station, bool)> = dir.stations.iter().map(|st| (st, crate::directory::now_and_next(st.schedule, day, now).0.is_some())).collect();
                list.sort_by_key(|(_, on)| !*on);
                for (st, on_air) in list.into_iter().take(10) {
                    let Some(khz) = crate::directory::best_khz(st, now) else { continue };
                    let hz = dir.dial_hz(khz);
                    let on = app.status.mode == focus && (app.inspect_hz() - hz).abs() < 50.0;
                    let text = format!("{}{} {}", if on_air { "● " } else { "" }, st.call, if khz.fract() == 0.0 { format!("{khz:.0}") } else { format!("{khz:.1}") });
                    if widgets::chip_key(ui, &text, on, tally).on_hover_text(format!("{} · {}", st.name, crate::tunestrip::freq_label(dir, khz))).clicked() {
                        pick = Some(crate::tunestrip::Dial { band: st.call.to_string(), hz, note: "" });
                    }
                }
            }
            if dials.is_empty() && dir.is_none() {
                ui.label(RichText::new("Tune where you hear it: this mode has no fixed bands or stations.").font(theme::sans(11.0)).color(theme::text_3()));
            }
        });
        if let Some(d) = pick {
            ui.data_mut(|m| m.insert_temp(egui::Id::new("hf-quick-tune"), (focus, d)));
            ui.close();
        }
    });
    out
}

/// A band or station picked in the HF menu's quick-tune row, to tune now.
pub fn take_quick_tune(ui: &Ui) -> Option<(Mode, crate::tunestrip::Dial)> {
    ui.data_mut(|d| d.remove_temp::<(Mode, crate::tunestrip::Dial)>(egui::Id::new("hf-quick-tune")))
}

/// One mode in the HF menu: its name, and what it is under it.
fn hf_tile(ui: &mut Ui, m: Mode, on: bool, w: f32) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(w, 36.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, on, m.label()));
    let (press, c) = widgets::paint_key(ui, &resp, r, egui::CornerRadius::same(3), on, theme::AMBER);
    let p = ui.painter();
    p.text(r.left_top() + vec2(8.0, 5.0 + press), egui::Align2::LEFT_TOP, m.label(), theme::mono_bold(11.5), c);
    p.text(r.left_bottom() + vec2(8.0, -5.0), egui::Align2::LEFT_BOTTOM, hf_spec(m), theme::mono(9.5), theme::text_3());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// A filter's response drawn small: flat top, and skirts as steep as the
/// shape's, with the elliptic shapes' ripple, so the three tell apart at a
/// glance.
pub fn shape_glyph(p: &egui::Painter, r: egui::Rect, shape: FilterShape, color: egui::Color32) {
    let (flat, skirt, ripple) = match shape {
        FilterShape::Sharp => (0.33, 0.025, 0.10),
        FilterShape::Normal => (0.24, 0.17, 0.035),
        FilterShape::Soft => (0.12, 0.33, 0.0),
    };
    let n = 48;
    let pts: Vec<egui::Pos2> = (0..=n)
        .map(|i| {
            let x = i as f32 / n as f32;
            let d = (x - 0.5).abs() - flat;
            let y = if d <= 0.0 {
                1.0 - ripple * (0.5 - 0.5 * (std::f32::consts::PI * 3.0 * (x - 0.5) / flat).cos())
            } else if d < skirt {
                0.5 * (1.0 + (std::f32::consts::PI * d / skirt).cos())
            } else {
                0.0
            };
            egui::pos2(r.left() + x * r.width(), r.bottom() - y * r.height())
        })
        .collect();
    let mut mesh = egui::Mesh::default();
    for (i, q) in pts.iter().enumerate() {
        mesh.colored_vertex(*q, alpha(color, 0.28));
        mesh.colored_vertex(egui::pos2(q.x, r.bottom()), alpha(color, 0.04));
        if i > 0 {
            let k = (2 * i) as u32;
            mesh.add_triangle(k - 2, k - 1, k);
            mesh.add_triangle(k - 1, k + 1, k);
        }
    }
    p.add(egui::Shape::mesh(mesh));
    p.add(egui::Shape::line(pts, Stroke::new(1.25, color)));
}

/// The header key that shows the filter's shape and opens the choice of
/// the three, each drawn and with what it trades.
fn shape_key(ui: &mut Ui, shape: FilterShape, height: f32) -> Option<FilterShape> {
    let font = theme::mono_bold(10.0);
    let label = format!("{} ▾", shape.label());
    let tw = ui.painter().layout_no_wrap(label.clone(), font.clone(), egui::Color32::PLACEHOLDER).size().x;
    let (rect, resp) = ui.allocate_exact_size(vec2(18.0 + 5.0 + tw + 8.0, height), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, format!("Filter shape {}", shape.label())));
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    let (press, c) = widgets::paint_key(ui, &resp, rect, egui::CornerRadius::same(3), open, theme::AMBER);
    let p = ui.painter();
    let g = egui::Rect::from_center_size(egui::pos2(rect.left() + 12.0, rect.center().y + press), vec2(16.0, (height - 5.0).max(6.0)));
    shape_glyph(p, g, shape, theme::AMBER);
    p.text(egui::pos2(g.right() + 5.0, rect.center().y + press), egui::Align2::LEFT_CENTER, &label, font, c);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let resp = resp.on_hover_text(shape.describe());
    let mut out = None;
    egui::Popup::menu(&resp).width(320.0).show(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for s in FilterShape::ALL {
            let (row, r) = ui.allocate_exact_size(vec2(320.0, 40.0), Sense::click());
            r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, s == shape, s.label()));
            let hv = ui.ctx().animate_bool_with_time(r.id.with("hv"), r.hovered(), 0.1);
            let on = s == shape;
            let p = ui.painter();
            if on || hv > 0.0 {
                p.rect(row, egui::CornerRadius::same(3), alpha(theme::AMBER, if on { 0.16 } else { 0.07 * hv }), Stroke::new(1.0, alpha(theme::AMBER, if on { 0.6 } else { 0.0 })), egui::StrokeKind::Inside);
            }
            let g = egui::Rect::from_min_size(row.left_top() + vec2(8.0, 9.0), vec2(40.0, 22.0));
            shape_glyph(p, g, s, if on { theme::AMBER } else { theme::lerp(theme::text_3(), theme::AMBER, 0.5 + 0.5 * hv) });
            let trade = s.describe().split_once(": ").map_or("", |(_, t)| t);
            p.text(egui::pos2(g.right() + 10.0, row.top() + 7.0), egui::Align2::LEFT_TOP, s.label(), theme::mono_bold(11.0), if on { theme::text() } else { theme::text_2() });
            let skirt = format!("{:.0} Hz skirt", s.skirt_hz());
            p.text(egui::pos2(row.right() - 8.0, row.top() + 7.0), egui::Align2::RIGHT_TOP, skirt, theme::mono(10.0), theme::text_3());
            let short = trade.split(". ").last().unwrap_or(trade);
            p.text(egui::pos2(g.right() + 10.0, row.top() + 22.0), egui::Align2::LEFT_TOP, short, theme::sans(11.0), theme::text_3());
            if r.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if r.clicked() {
                if !on {
                    out = Some(s);
                }
                ui.close();
            }
        }
    });
    out
}

/// A filter preset: a width that keeps the low edge where it is (or, for
/// symmetric filters, centers on the carrier), or named edges of its own.
#[derive(Clone)]
struct Preset {
    label: String,
    width: f32,
    edges: Option<(f32, f32)>,
    tip: Option<&'static str>,
}

/// The presets a mode offers. Extended SSB is a voice preset only: CW has
/// no mode here, and the data modes' presets widen the decoder's search.
fn presets_for(mode: Mode) -> Vec<Preset> {
    let widths = |ws: &[f32]| ws.iter().map(|w| Preset { label: preset_label(*w), width: *w, edges: None, tip: None }).collect::<Vec<_>>();
    match mode {
        Mode::Usb | Mode::Lsb => {
            let sign = if mode == Mode::Lsb { -1.0 } else { 1.0 };
            let named = |label: &str, lo: f32, hi: f32, tip: &'static str| {
                let (a, b) = (sign * lo, sign * hi);
                Preset { label: label.into(), width: hi - lo, edges: Some((a.min(b), a.max(b))), tip: Some(tip) }
            };
            let mut v = widths(&[1_800.0, 2_100.0, 2_400.0, 2_700.0, 3_000.0, 3_600.0]);
            v.push(named("ESSB", 50.0, 4_000.0, "Extended SSB: 50–4000 Hz, for wide-audio voice stations"));
            v.push(named("HI-FI", 50.0, 4_500.0, "Hi-fi SSB: 50–4500 Hz, the widest ESSB stations"));
            v
        }
        // The decoder searches what the filter passes: wider finds the
        // stations above 3 kHz.
        Mode::Ft8 | Mode::Ft4 => widths(&[2_000.0, 3_000.0, 4_000.0, 4_900.0]),
        Mode::Am => widths(&[4_000.0, 6_000.0, 8_000.0, 10_000.0, 12_000.0, 16_000.0]),
        Mode::Nfm => widths(&[6_000.0, 8_000.0, 11_000.0, 12_500.0, 16_000.0, 25_000.0]),
        Mode::Wfm => widths(&[100_000.0, 150_000.0, 180_000.0, 200_000.0, 230_000.0]),
        _ => vec![],
    }
}

fn preset_label(w: f32) -> String {
    format!("{}k", format!("{:.1}", w / 1000.0).trim_end_matches(".0"))
}

fn filter(app: &mut App, lcd: &mut Ui, keys: &mut Ui, look: crate::lcd::Look) {
    let width_px = keys.available_width();
    let lit = look.value;
    // Receiver B's filter is its mode's, set the same way with its mode named.
    let b = app.status.rx2.clone().filter(|_| app.active_b);
    let (mode, lo, hi) = match &b {
        Some(b) => {
            let c = b.passband_offset_hz as f32;
            let w = b.bandwidth_hz as f32;
            (b.mode, c - w / 2.0, c + w / 2.0)
        }
        None if app.status.filter_high_hz > app.status.filter_low_hz => (app.status.mode, app.status.filter_low_hz as f32, app.status.filter_high_hz as f32),
        None => {
            let (c, w) = (app.status.passband_offset_hz as f32, app.status.bandwidth_hz as f32);
            (app.status.mode, c - w / 2.0, c + w / 2.0)
        }
    };
    let width = (hi - lo).max(0.0);
    let k = |v: f32| format!("{}k", format!("{:.2}", v / 1000.0).trim_end_matches('0').trim_end_matches('.'));
    let server = app.server.clone();
    let has = |f: &str| server.as_ref().is_some_and(|i| i.has(f));
    let range = if has("filter") { app.modes.iter().find(|c| c.mode == mode).and_then(|c| c.filter) } else { None };
    let shape = match &b {
        Some(b) => b.filter_shape,
        None => app.status.filter_shape,
    };
    let shaped = range.is_some_and(|r| r.shaped) && has("filter-shape");
    let custom = if b.is_some() { range.is_some() && (width - app.modes.iter().find(|c| c.mode == mode).map_or(0.0, |c| c.bandwidth_hz)).abs() > 1.0 } else { app.status.filter_custom };
    // The name, and the shape: a touch on the LCD, as what it chooses among
    // is read there.
    lcd.spacing_mut().item_spacing.y = 2.0;
    let mut new_shape = None;
    let named = !shaped || lcd.available_width() >= 130.0;
    lcd.horizontal(|ui| {
        if named {
            ui.label(caption("FILTER", &look));
        }
        if shaped {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                new_shape = shape_key(ui, shape, 16.0);
            });
        }
    });
    if let Some(s) = new_shape {
        app.set_filter_shape(s);
    }
    let Some(range) = range else {
        // One short line, so it never wraps in the narrow zone; the whole
        // of it is a hover away.
        let (note, tip) = if has("filter") {
            ("set by the decoder", format!("{}'s decoder sets its own channel filter", mode.label()))
        } else {
            ("fixed on this server", "This server predates the adjustable filter".to_string())
        };
        lcd.label(RichText::new(note).font(theme::mono(10.0)).color(alpha(lit, 0.66))).on_hover_text(tip);
        return;
    };
    let ui = keys;
        let mut presets = presets_for(mode);
        // The mode's own width is always a preset, and choosing it is the
        // reset: no separate key to find once the filter has wandered.
        let home = app.modes.iter().find(|c| c.mode == mode).map(|c| c.bandwidth_hz).filter(|w| *w > 0.0);
        if let Some(h) = home
            && !presets.iter().any(|p| p.edges.is_none() && (p.width - h).abs() < 1.0)
        {
            presets.push(Preset { label: preset_label(h), width: h, edges: None, tip: None });
        }
        presets.sort_by(|a, b| a.width.total_cmp(&b.width));
        presets.retain(|p| p.width >= range.min_width_hz - 1.0 && p.width <= range.max_width_hz + 1.0 && p.edges.is_none_or(|(l, h)| l.abs().max(h.abs()) <= range.max_edge_hz + 1.0));
        if presets.is_empty() {
            presets.push(Preset { label: preset_label(width), width, edges: None, tip: None });
        }
        let around = |w: f32| -> (f32, f32) {
            match mode {
                Mode::Usb | Mode::Ft8 | Mode::Ft4 => (lo, (lo + w).min(range.max_edge_hz)),
                Mode::Lsb => ((hi - w).max(-range.max_edge_hz), hi),
                _ => (-w / 2.0, w / 2.0),
            }
        };
        let edges_of = |p: &Preset| p.edges.unwrap_or_else(|| around(p.width));
        let is_on = |p: &Preset| match p.edges {
            Some((l, h)) => (l - lo).abs() < 1.0 && (h - hi).abs() < 1.0,
            None => (width - p.width).abs() < 1.0 && !presets.iter().any(|q| q.edges.is_some_and(|(l, h)| (l - lo).abs() < 1.0 && (h - hi).abs() < 1.0)),
        };
        let mut set: Option<(f32, f32)> = None;
        let mut reset = false;
        let is_home = |p: &Preset| p.edges.is_none() && home.is_some_and(|h| (p.width - h).abs() < 1.0);
        // Arrows either side always reach the next preset narrower and
        // wider, so a narrow bay never strands the width at one end.
        let narrower = presets.iter().rev().find(|p| p.width < width - 1.0).cloned();
        let wider = presets.iter().find(|p| p.width > width + 1.0).cloned();
        // Between them, as many presets as fit, grown outward from the one
        // nearest the width in use so its neighbors on both sides show.
        const GAP_X: f32 = 3.0;
        let chip_w = |p: &Preset| ui.painter().layout_no_wrap(p.label.clone(), theme::mono_bold(10.5), egui::Color32::PLACEHOLDER).size().x + 14.0 + GAP_X;
        let widths: Vec<f32> = presets.iter().map(chip_w).collect();
        let room = width_px - 2.0 * (20.0 + GAP_X);
        let near = presets.iter().position(is_on).unwrap_or_else(|| presets.iter().enumerate().min_by(|a, b| (a.1.width - width).abs().total_cmp(&(b.1.width - width).abs())).map_or(0, |(i, _)| i));
        let (mut first, mut last, mut used) = (near, near, widths[near]);
        loop {
            let right = (last + 1 < presets.len()).then(|| widths[last + 1]).filter(|w| used + w <= room);
            if let Some(w) = right {
                last += 1;
                used += w;
            }
            let left = (first > 0).then(|| widths[first - 1]).filter(|w| used + w <= room);
            if let Some(w) = left {
                first -= 1;
                used += w;
            }
            if right.is_none() && left.is_none() {
                break;
            }
        }
        let row = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP_X;
            if widgets::step_chip(ui, false, narrower.is_some()).on_hover_text("Narrower (wheel down over the presets)").clicked()
                && let Some(p) = &narrower
            {
                set = Some(edges_of(p));
            }
            for p in &presets[first..=last] {
                let mut r = widgets::chip_key(ui, &p.label, is_on(p), theme::AMBER);
                if is_home(p) {
                    // A green pip marks the mode's own filter.
                    let c = r.rect.right_top() + vec2(-4.0, 4.0);
                    ui.painter().circle_filled(c, 2.0, if custom { theme::OK } else { alpha(theme::OK, 0.6) });
                    r = r.on_hover_text(if custom { "The mode's own filter — click to reset" } else { "The mode's own filter (in use)" });
                } else if let Some(t) = p.tip {
                    r = r.on_hover_text(t);
                }
                if r.clicked() {
                    if is_home(p) {
                        reset = true;
                    } else {
                        set = Some(edges_of(p));
                    }
                }
            }
            if widgets::step_chip(ui, true, wider.is_some()).on_hover_text("Wider (wheel up over the presets)").clicked()
                && let Some(p) = &wider
            {
                set = Some(edges_of(p));
            }
        });
        if row.response.contains_pointer() {
            let notches = widgets::wheel(ui);
            if notches != 0.0 {
                widgets::eat_scroll(ui);
                if let Some(p) = if notches > 0.0 { &wider } else { &narrower } {
                    set = Some(edges_of(p));
                }
            }
        }
        // Stepping onto the mode's own width is the reset too, so it lands
        // with the mode's own edges (sideband filters sit off the carrier).
        if let Some((l, h)) = set
            && presets.iter().any(|p| is_home(p) && (p.width - (h - l)).abs() < 1.0 && p.edges.is_none())
            && !presets.iter().any(|p| p.edges.is_some_and(|(pl, ph)| (pl - l).abs() < 1.0 && (ph - h).abs() < 1.0))
        {
            set = None;
            reset = true;
        }
        if reset && custom {
            if b.is_some() {
                app.post("/api/sdr/filter", json!({ "reset": true, "mode": mode.wire() }));
            } else {
                app.reset_filter();
            }
        }
        // The passband itself, drawn to its shape and dragged by its edges,
        // its edges and width read at the glass's end.
        let edges = if range.symmetric {
            format!("±{}", k(hi.abs().max(lo.abs())))
        } else {
            let (l, h) = (lo.abs().min(hi.abs()), lo.abs().max(hi.abs()));
            format!("{}–{}", k(l).trim_end_matches('k'), k(h))
        };
        let plot_h = lcd.available_height();
        if let Some(edges) = crate::passband::show(lcd, plot_h, lo, hi, &range, shaped.then_some(shape), &look, (&edges, &k(width)), &around) {
            set = Some(edges);
        }
        if let Some((l, h)) = set {
            match &b {
                Some(_) => {
                    app.post("/api/sdr/filter", json!({ "low_hz": l.round(), "high_hz": h.round(), "mode": mode.wire() }));
                    if let Some(rx) = app.status.rx2.as_mut() {
                        rx.bandwidth_hz = (h - l) as f64;
                        rx.passband_offset_hz = ((l + h) / 2.0) as f64;
                    }
                }
                None => app.set_filter(l.round() as f64, h.round() as f64),
            }
        }
}

/// The record key for the receiver being driven: on the recorder's card,
/// and in the audio menu when the column is hidden.
pub fn rec_key(app: &mut App, ui: &mut Ui) {
    let is_b = app.active_b && app.status.rx2.is_some();
    let rec = if is_b { app.player_b.reading().recording.is_some() } else { app.player.reading().recording.is_some() };
    if widgets::chip_key(ui, if rec { "● REC" } else { "REC" }, rec, theme::EMBER).on_hover_text("Record what this receiver plays to a WAV (R)").clicked() {
        let ctx = ui.ctx().clone();
        if is_b {
            app.toggle_recording_b(&ctx);
        } else {
            app.toggle_local_recording(&ctx);
        }
    }
}

/// The audio zone: muted or the gain, and the level; the mute key, the gain
/// fader and the rest of it under them.
fn audio(app: &mut App, lcd: &mut Ui, keys: &mut Ui, look: crate::lcd::Look) {
    let is_b = app.active_b && app.status.rx2.is_some();
    let lit = look.value;
    let (vol, muted) = if is_b { (app.prefs.volume_b, app.prefs.muted_b) } else { (app.prefs.volume, app.prefs.muted) };
    let player = if is_b { app.player_b.clone() } else { app.player.clone() };
    lcd.spacing_mut().item_spacing.y = 4.0;
    lcd.label(caption(if is_b { "AUDIO B" } else { "AUDIO A" }, &look));
    lcd.allocate_ui_with_layout(vec2(lcd.available_width(), 22.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        if muted {
            widgets::lcd_tag_in(ui, "MUTED", &look, true);
        } else {
            ui.label(RichText::new(format!("{:.0}", vol * 100.0)).font(theme::mono_bold(20.0)).color(lit));
            ui.label(caption("AF", &look));
        }
    });
    let r = player.reading();
    let lw = lcd.available_width();
    level_meter(lcd, lw, r.rms, r.peak, look);
    keys.spacing_mut().item_spacing.x = 5.0;
    // A narrow zone keeps the fader long: the LCD says it is muted.
    let word = if muted { "MUTED" } else { "MUTE" };
    let shown = if keys.available_width() >= 130.0 { word } else { "M" };
    if widgets::chip_key_named(keys, shown, word, muted, theme::AMBER, 0.0).on_hover_text("M").clicked() {
        if is_b {
            app.prefs.muted_b = !muted;
            app.player_b.set_muted(app.prefs.muted_b);
        } else {
            app.prefs.muted = !muted;
            app.player.set_muted(app.prefs.muted);
        }
    }
    // 100 is unity, at the middle mark; past it is up to +12 dB of boost
    // for weak signals, with the player's limiter keeping it clean.
    let more_w = keys.painter().layout_no_wrap("···".into(), widgets::key_font(), egui::Color32::PLACEHOLDER).size().x + 12.0;
    let fader_w = (keys.available_width() - more_w - 5.0).max(20.0);
    let mut v = vol as f64 * 100.0;
    let (ch, _) = keys
        .allocate_ui_with_layout(vec2(fader_w, 22.0), egui::Layout::top_down(egui::Align::Min), |ui| {
            Fader {
                value: &mut v,
                min: 0.0,
                max: 200.0,
                step: 2.0,
                fine: 1.0,
                label: "AF GAIN",
                text: |v| format!("{v:.0}"),
                color: if is_b { theme::VIOLET } else { theme::AMBER },
                enabled: !muted,
                ticks: vec![],
            }
            .show_bare(ui)
        })
        .inner;
    if ch {
        if is_b {
            app.prefs.volume_b = (v / 100.0) as f32;
            app.player_b.set_volume(app.prefs.volume_b);
        } else {
            app.prefs.volume = (v / 100.0) as f32;
            app.player.set_volume(app.prefs.volume);
        }
    }
    widgets::menu_glyph(keys, "···", more_w, |ui| {
        ui.set_min_width(220.0);
        if !app.prefs.show_side {
            rec_key(app, ui);
        }
        let mut pan = if is_b { app.prefs.pan_b } else { app.prefs.pan } as f64;
        let (ch, _) = Fader {
            value: &mut pan,
            min: -1.0,
            max: 1.0,
            step: 0.1,
            fine: 0.02,
            label: "BALANCE",
            text: |p| if p.abs() < 0.01 { "center".into() } else if p < 0.0 { format!("L {:.0}", -p * 100.0) } else { format!("R {:.0}", p * 100.0) },
            color: theme::text_2(),
            enabled: true,
            ticks: vec![],
        }
        .show(ui);
        if ch {
            if is_b {
                app.prefs.pan_b = pan as f32;
                app.player_b.set_pan(app.prefs.pan_b);
            } else {
                app.prefs.pan = pan as f32;
                app.player.set_pan(app.prefs.pan);
            }
            app.sync_fold();
        }
        let r = player.reading();
        ui.label(small(if r.queue_ms > 0.0 {
            format!("{} Hz in · queue {:.0} ms · clock {:+.0} ppm", r.in_rate.round(), r.queue_ms, r.drift_ppm)
        } else {
            "no audio arriving".into()
        }));
        if let Some(p) = app.player.clip_progress()
            && widgets::action(ui, &format!("STOP PLAYBACK ({:.0}%) · BACK TO LIVE", p * 100.0)).clicked()
        {
            app.player.stop_clip();
        }
    });
}

/// What the receiver is playing, −60 to 0 dBFS, on the LCD: RMS as lit
/// segments (red from −3), the peak as a pip, ticks at −40, −20 and −6, and
/// a clip lamp that holds a second.
fn level_meter(ui: &mut Ui, w: f32, rms: f32, peak: f32, look: crate::lcd::Look) {
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 22.0), Sense::hover());
    let p = ui.painter_at(rect.expand(3.0));
    let db = |v: f32| 20.0 * v.max(1e-6).log10();
    let lamp_w = 10.0;
    let bar = egui::Rect::from_min_max(egui::pos2(rect.left() + 5.0, rect.top() + 6.0), egui::pos2(rect.right() - 5.0 - lamp_w, rect.top() + 13.0));
    let x_of = |d: f32| bar.left() + bar.width() * ((d + 60.0) / 60.0).clamp(0.0, 1.0);
    let lit_to = x_of(db(rms));
    let mut x = bar.left();
    while x < bar.right() - 0.5 {
        let d = (x - bar.left()) / bar.width() * 60.0 - 60.0;
        let c = if d >= -3.0 { theme::ERR } else { look.meter[0] };
        let seg = egui::Rect::from_min_max(egui::pos2(x, bar.top()), egui::pos2((x + 2.0).min(bar.right()), bar.bottom()));
        if x < lit_to {
            p.rect_filled(seg.expand2(vec2(1.0, 2.0)), egui::CornerRadius::same(1), alpha(c, 0.18));
            p.rect_filled(seg, egui::CornerRadius::ZERO, c);
            p.rect_filled(egui::Rect::from_min_size(seg.min, vec2(seg.width(), 2.0)), egui::CornerRadius::ZERO, alpha(theme::lerp(c, egui::Color32::WHITE, 0.45), 0.9));
        } else {
            p.rect_filled(seg, egui::CornerRadius::ZERO, alpha(c, 0.12));
        }
        x += 3.0;
    }
    if peak > 1e-4 {
        let px = x_of(db(peak));
        p.line_segment([egui::pos2(px, bar.top() - 1.0), egui::pos2(px, bar.bottom() + 1.0)], Stroke::new(1.5, look.value));
    }
    for d in [-40.0, -20.0, -6.0] {
        let tx = x_of(d);
        p.line_segment([egui::pos2(tx, bar.bottom() + 2.0), egui::pos2(tx, bar.bottom() + 5.0)], Stroke::new(1.0, look.caption));
    }
    let id = resp.id.with("clip");
    let now = ui.input(|i| i.time);
    if peak >= 0.99 {
        ui.data_mut(|d| d.insert_temp(id, now));
    }
    let clipped_at: f64 = ui.data(|d| d.get_temp(id).unwrap_or(f64::NEG_INFINITY));
    let on = now - clipped_at < 1.0;
    let lamp = egui::pos2(rect.right() - 4.0 - lamp_w / 2.0, bar.center().y);
    if on {
        crate::anim::more(ui.ctx());
    }
    theme::lamp(&p, lamp, 2.6, theme::ERR, if on { 1.0 } else { 0.0 }, theme::LampShape::Round);
    resp.on_hover_text(if rms > 1e-5 {
        format!("Output: {:.0} dBFS RMS, peak {:.0} dBFS · ticks at −40, −20, −6 · the lamp lights on a clip", db(rms), db(peak))
    } else {
        "Output level: nothing playing".into()
    });
}

/// The signal zone: the reading, the meter, what is heard and the SNR.
fn signal(app: &mut App, ui: &mut Ui, look: crate::lcd::Look) {
    let is_b = app.active_b && app.status.rx2.is_some();
    let rm = app.reduce_motion();
    let mark = app.prefs.view.sql_mark;
    let s = &app.status;
    let err = s.freq_error_hz.filter(|_| !is_b);
    let label = if is_b { "SIGNAL B" } else { "SIGNAL" };
    let now = ui.input(|i| i.time);
    if err.is_some_and(|e| e.abs() > 2000.0) {
        app.err_warn_until = now + 5.0;
    }
    let err_warn = now < app.err_warn_until;
    let ((s_txt, _), (db_txt, db_col)) = if is_b { app.meter_b.readout() } else { app.meter.readout() };
    let mut calibrate = false;
    let lit = look.value;
    let dim = look.caption;
    let reading = theme::mono_bold(if ui.available_width() >= 320.0 { 22.0 } else { 16.0 });
    ui.spacing_mut().item_spacing.y = 3.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(caption(label, &look));
        fixed(ui, &s_txt, reading.clone(), lit, "S9+60");
        fixed(ui, &db_txt, theme::mono(11.0), if db_col == theme::ERR { theme::ERR } else { lit }, "-100.0 dBFS");
        if let Some(e) = err {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let t = if e.abs() >= 1_000.0 { format!("CAL {:+.1}k", e / 1000.0) } else { format!("CAL {e:+.0}") };
                let r = widgets::lcd_tag_in(ui, &t, &look, err_warn)
                    .on_hover_text("The carrier is this far from where the receiver put it: click to calibrate the ppm from it");
                if r.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                calibrate = r.clicked();
            });
        }
    });
    if is_b {
        app.meter_b.ui_bar_bare(ui, rm, mark, &look);
        let snr = app.meter_b.snr.map_or(format!("{:>4} dB", "—"), |s| format!("{s:>4.1} dB"));
        ui.add(egui::Label::new(RichText::new(format!("SNR {snr} · measured from the spectrum")).font(theme::mono(10.5)).color(dim)).truncate());
    } else {
        app.meter.ui_bar_bare(ui, rm, mark, &look);
        status_row(app, ui, rm, lit, dim, look);
    }
    if calibrate && let Some(l) = &app.link {
        l.post_get("/api/sdr/calibrate", json!({}), Get::Calibrate);
    }
}

/// The display's last line: what is heard (the decoder's copy, the
/// classifier's call, a station's RDS), the stereo pilot, the notches in,
/// and the SNR, as annunciators and lit text.
fn status_row(app: &App, ui: &mut Ui, rm: bool, lit: egui::Color32, dim: egui::Color32, look: crate::lcd::Look) {
    let s = &app.status;
    let snr = app.meter.snr.map_or(format!("{:>4} dB", "—"), |d| format!("{d:>4.1} dB"));
    let small_lit = |t: String| RichText::new(t).font(theme::mono(10.5)).color(lit);
    let mut notches = Vec::new();
    if s.rf_notch == Some(true) {
        notches.push("AM/FM".to_string());
    }
    if s.dab_notch == Some(true) {
        notches.push("DAB".to_string());
    }
    if !s.spurs_hz.is_empty() {
        notches.push(format!("SPUR {}", s.spurs_hz.len()));
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        // A decoder owns the channel: the bay reports its copy.
        let (word, on, color, txt, tip, col) = if s.mode.is_hf_data() || s.mode == Mode::Ais {
            let (busy, line) = hf_decoder_line(app);
            (if busy { "DECODING" } else { "LISTENING" }, busy, look.tag, line.clone(), Some(line), if busy { lit } else { dim })
        } else {
            let (c, word) = app.shown_class();
            let short = |p: &str| p.split(" (").next().unwrap_or(p).to_string();
            let rds = app.rds().and_then(|r| rds_line(r).map(|t| (t, r)));
            let (txt, tip, col) = match (&c, rds) {
                (_, Some((t, r))) => (t, Some(rds_tip(r)), if r.locked { lit } else { dim }),
                (Some(c), None) if word != "IDLE" => (short(&c.protocol), Some(format!("{} · {}", c.protocol, c.modulation)), lit),
                _ => (crate::app::mode_idle_text(s.mode).to_string(), None, dim),
            };
            let color = if word == "HELD" { theme::WARN } else { look.tag };
            (word, word != "IDLE", color, txt, tip, col)
        };
        if color == look.tag {
            widgets::lcd_tag_in(ui, word, &look, on);
        } else {
            widgets::lcd_tag(ui, word, color, on);
        }
        if s.mode == Mode::Wfm
            && let Some(st) = app.player.reading().stereo
        {
            let mp = s.stereo.as_ref().and_then(|x| x.multipath_pct.map(|p| (p, x.equalizer)));
            stereo_badge(ui, &st, mp, rm, lit);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(small_lit(format!("SNR {snr}")));
            if !notches.is_empty() {
                widgets::lcd_tag_in(ui, &format!("NOTCH {}", notches.join(" ")), &look, true);
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                let r = ui.add(egui::Label::new(RichText::new(txt).font(theme::sans(12.0)).color(col)).truncate());
                if let Some(t) = tip {
                    r.on_hover_text(t);
                }
            });
        });
    });
}

/// The SIGNAL bay's line for an HF data decoder, from its own decodes:
/// whether it has copied anything lately, and what. Slotted modes count the
/// last slot and give the time to the next; the rest count the last minute.
fn hf_decoder_line(app: &App) -> (bool, String) {
    let mode = app.status.mode;
    let name = mode.label();
    let now = std::time::SystemTime::now();
    let ago = |d: &crate::app::DecodeRow| now.duration_since(d.at).map_or(0.0, |x| x.as_secs_f64());
    let mine = |d: &&crate::app::DecodeRow| d.ev.protocol == name || (d.family == Some(crate::decoders::Family::HfData) && (d.freq_hz - app.status.freq_hz).abs() < 3_000.0);
    let slot_s = match mode {
        Mode::Ft8 => Some(15.0),
        Mode::Ft4 => Some(7.5),
        Mode::Wspr | Mode::Fst4w => Some(120.0),
        _ => None,
    };
    if let Some(slot) = slot_s {
        let unix = now.duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let next = slot - unix % slot;
        // A slot is decoded after it ends, so its decodes land in the slot
        // after: the last slot's copy is what arrived within one slot length.
        let n = app.decodes.iter().filter(mine).take_while(|d| ago(d) < slot * 1.5).count();
        // Short enough for the bay: the mode key already names the mode.
        let line = if n > 0 { format!("{n} heard · next {next:.0} s") } else { format!("next slot in {next:.0} s") };
        return (n > 0, line);
    }
    let recent: Vec<&crate::app::DecodeRow> = app.decodes.iter().filter(mine).take_while(|d| ago(d) < 60.0).collect();
    match recent.first() {
        Some(last) => {
            let what: String = last.ev.summary.chars().take(40).collect();
            (true, format!("{} this minute · {what}", recent.len()))
        }
        None => (false, crate::app::mode_idle_text(mode).to_string()),
    }
}

/// The stereo decoder's reading, as the audio now playing carries it: two
/// rings that sit on top of each other for mono and part as the blend opens
/// to full separation, lit while a pilot is locked. Shown only when the
/// server measured it.
fn stereo_badge(ui: &mut Ui, st: &usdr_proto::StereoReading, mp: Option<(f32, bool)>, reduce_motion: bool, on: egui::Color32) {
    let id = ui.id().with("stereo-badge");
    // The rings' parting follows the blend eased on the motion clock, with
    // a dead zone: the reading wanders a percent or two frame to frame,
    // which egui's own easing would chase with a frame every frame.
    let blend = {
        let key = id.with("blend");
        let target = (st.blend * 20.0).round() / 20.0;
        let cur: f32 = ui.ctx().data(|d| d.get_temp(key)).unwrap_or(target);
        let next = if reduce_motion || (cur - target).abs() < 0.03 {
            target
        } else {
            let dt = ui.input(|i| i.stable_dt).min(0.1);
            // A reading in steps of a twentieth: a real swing moves at the
            // motion rate, a step or two rides the frames already coming.
            if (cur - target).abs() > 0.15 {
                crate::anim::more(ui.ctx());
            } else {
                crate::anim::drift(ui.ctx());
            }
            cur + (target - cur) * (1.0 - (-dt * 14.0).exp())
        };
        ui.ctx().data_mut(|d| d.insert_temp(key, next));
        next
    };
    let lit = ui.ctx().animate_bool_with_time(id.with("lit"), st.pilot, if reduce_motion { 0.0 } else { 0.2 });
    let col = theme::lerp(alpha(on, 0.45), on, lit);
    let word = if !st.pilot {
        "MONO".to_string()
    } else if st.blend < 0.995 {
        format!("STEREO {:>3.0}%", st.blend * 100.0)
    } else {
        "STEREO".to_string()
    };
    // Laid out at its widest, so the row does not shift as the blend moves.
    let wide = ui.painter().layout_no_wrap("STEREO 100%".into(), theme::mono(10.5), col).size().x;
    let g = ui.painter().layout_no_wrap(word, theme::mono(10.5), col);
    let (rect, resp) = ui.allocate_exact_size(vec2(22.0 + wide, 18.0), Sense::hover());
    let p = ui.painter();
    let (cy, r) = (rect.center().y, 4.5);
    let cx = rect.left() + 9.0;
    let sep = 3.0 * blend;
    for (k, dx) in [(0, -sep), (1, sep)] {
        let c = if k == 0 { col } else { alpha(col, 0.55 + 0.45 * lit) };
        p.circle_stroke(egui::pos2(cx + dx, cy), r, Stroke::new(1.3, c));
    }
    p.galley(egui::pos2(rect.left() + 20.0, cy - g.size().y / 2.0), g, col);
    // The multipath reading rides in the status, not the audio frames.
    let mp_line = match mp {
        Some((p, true)) => format!("\nMultipath ripples the signal {p:.0}%; the equalizer is undoing it"),
        Some((p, false)) if p >= 3.0 => format!("\nMultipath ripples the signal {p:.0}%"),
        Some((p, false)) => format!("\nMultipath {p:.1}%: a clean channel"),
        None => String::new(),
    };
    resp.on_hover_text(if st.pilot {
        format!(
            "19 kHz pilot locked{}\nSeparation in use: {:.0}%, blended toward mono as the S/N falls\nStereo S/N {}{mp_line}",
            st.pilot_dev_hz.map(|h| format!(" at {:.2} kHz deviation", h / 1000.0)).unwrap_or_default(),
            st.blend * 100.0,
            st.snr_db.map_or("not measured".into(), |d| format!("{d:.0} dB")),
        )
    } else if st.blend > 0.0 {
        format!("Pilot lost: back to mono ({:.0}% separation left)", st.blend * 100.0)
    } else {
        "No stereo pilot locked: mono".into()
    });
}

/// A RadioText character not yet received, as the server marks it.
pub const RT_GAP: char = '\u{2591}';

/// Who the station is: its PS once it has held steady (the server's ten
/// seconds without a change, never for one that scrolls), else the call
/// letters its PI maps to, else the PI. The PI is confirmed long before any
/// text, so this is the first thing to show, and a scrolling station's
/// first word is never taken for its name.
pub fn rds_station(r: &usdr_proto::Rds) -> String {
    match (&r.ps, &r.pi_call) {
        (Some(ps), _) if r.ps_steady && !r.ps_dynamic => ps.clone(),
        (_, Some(call)) => call.clone(),
        _ => format!("PI {}", r.pi),
    }
}

/// What is on, and how much of it is in while it is still arriving: a
/// RadioText being received (gaps as [`RT_GAP`]) before anything older,
/// since it is what the station is sending now; then the tagged title and
/// artist, the RadioText, or a scrolling PS's word, which is that station's
/// way of saying what is playing.
pub fn rds_now(r: &usdr_proto::Rds) -> Option<(String, Option<f32>)> {
    if let Some(p) = r.rt_partial.as_ref().filter(|p| !p.is_empty()) {
        return Some((p.clone(), Some(r.rt_progress)));
    }
    match (&r.title, &r.artist, &r.rt) {
        (Some(t), Some(a), _) => Some((format!("{t} — {a}"), None)),
        (Some(t), None, _) => Some((t.clone(), None)),
        (None, _, Some(rt)) => Some((rt.clone(), None)),
        _ if r.ps_dynamic => r.ps.clone().map(|p| (p, None)),
        _ => None,
    }
}

/// `text` laid out with what was received in `color` and each [`RT_GAP`] a
/// faint underscore, one per missing character, as a radio's display shows
/// a text still coming in.
pub fn gapped(text: &str, font: egui::FontId, color: egui::Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let (mut run, mut gap) = (String::new(), false);
    let flush = |job: &mut egui::text::LayoutJob, run: &mut String, gap: bool| {
        if !run.is_empty() {
            let c = if gap { theme::alpha(theme::text_3(), 0.7) } else { color };
            job.append(run, 0.0, egui::TextFormat { font_id: font.clone(), color: c, ..Default::default() });
            run.clear();
        }
    };
    for ch in text.chars() {
        let is_gap = ch == RT_GAP;
        if is_gap != gap {
            flush(&mut job, &mut run, gap);
            gap = is_gap;
        }
        run.push(if is_gap { '_' } else { ch });
    }
    flush(&mut job, &mut run, gap);
    job
}

/// The SIGNAL bay's one line of RDS: who, then what is on, the gaps of a
/// text still arriving shown as underscores.
fn rds_line(r: &usdr_proto::Rds) -> Option<String> {
    let who = rds_station(r);
    Some(match rds_now(r) {
        Some((n, _)) => format!("{who} · {}", n.replace(RT_GAP, "_")),
        None => who,
    })
}

/// Everything RDS has confirmed, for a hover.
pub fn rds_tip(r: &usdr_proto::Rds) -> String {
    let mut lines = Vec::new();
    if let Some(ps) = &r.ps {
        lines.push(if r.ps_dynamic { format!("{ps}  (the PS scrolls)") } else { ps.clone() });
    }
    if let Some(rt) = &r.rt {
        lines.push(rt.clone());
    }
    if let Some(t) = &r.title {
        lines.push(format!("title  {t}"));
    }
    if let Some(a) = &r.artist {
        lines.push(format!("artist  {a}"));
    }
    let mut id = format!("PI {}", r.pi);
    if let Some(c) = &r.pi_call {
        id.push_str(&format!(" → {c} by the RBDS call mapping"));
    }
    lines.push(id);
    if let Some(p) = &r.pty_name {
        lines.push(format!("program type  {p}"));
    }
    if let Some(c) = rds_clock(r) {
        lines.push(format!("station clock  {c}"));
    }
    lines.push(if r.locked {
        format!("RDS locked · {:.0}% of blocks unreadable", r.bler * 100.0)
    } else {
        "RDS lock lost · last heard values".to_string()
    });
    lines.join("\n")
}

/// The station's clock in its own local time, with the offset it sent.
pub fn rds_clock(r: &usdr_proto::Rds) -> Option<String> {
    let utc = r.clock_utc.as_deref()?;
    let off = r.clock_offset_min?;
    let hm = utc.split('T').nth(1)?.trim_end_matches('Z');
    let (h, m) = hm.split_once(':')?;
    let mins = (h.parse::<i32>().ok()? * 60 + m.parse::<i32>().ok()? + off).rem_euclid(24 * 60);
    let sign = if off < 0 { '−' } else { '+' };
    Some(format!("{:02}:{:02} (UTC{sign}{}:{:02}) · {}", mins / 60, mins % 60, off.abs() / 60, off.abs() % 60, utc.split('T').next()?))
}

/// `text` in a box as wide as `widest` would be, so what follows holds still.
fn fixed(ui: &mut Ui, text: &str, font: egui::FontId, color: egui::Color32, widest: &str) {
    let w = ui.painter().layout_no_wrap(widest.into(), font.clone(), color).size().x;
    let g = ui.painter().layout_no_wrap(text.into(), font, color);
    let (r, _) = ui.allocate_exact_size(vec2(w, g.size().y), Sense::hover());
    ui.painter().galley(r.left_top(), g, color);
}

/// One-click memories under the bar, like a radio's band buttons. Storing
/// and renaming live in the Tools tab.
/// The most a slot takes, and how many sit in the row before the rest move
/// to a menu: fixed counts, so storing or renaming one moves no other.
pub const SLOT_W: f32 = 108.0;
const SLOTS_SHOWN: usize = 6;

/// A memory slot: its number (Ctrl+number recalls it), the frequency and
/// its name in the mode's color, the name cut to the slot; the free slot
/// after them is a short dashed "+ N" that stores the receiver there.
fn memory_pill(ui: &mut Ui, slot: usize, m: Option<&crate::prefs::Memory>, lit: bool, armed: bool, slot_w: f32) -> egui::Response {
    let num_f = theme::mono(10.0);
    let font = theme::mono_bold(10.5);
    let p = ui.painter().clone();
    let num = p.layout_no_wrap(slot.to_string(), num_f, egui::Color32::PLACEHOLDER);
    let parts = m.map(|m| {
        let mhz = crate::tuning::fmt_mhz(m.freq_hz, 4);
        let mhz = mhz.trim_end_matches('0').trim_end_matches('.').to_string();
        let f = p.layout_no_wrap(mhz, font.clone(), egui::Color32::PLACEHOLDER);
        let room = slot_w - 8.0 - num.size().x - 6.0 - f.size().x - 5.0 - 8.0;
        let name = widgets::ellipsize(&p, &m.name, theme::mono_bold(10.0), room);
        (f, p.layout_no_wrap(name, theme::mono_bold(10.0), egui::Color32::PLACEHOLDER))
    });
    let plus = m.is_none().then(|| p.layout_no_wrap(format!("+ {slot}"), theme::mono(10.0), egui::Color32::PLACEHOLDER));
    let w = plus.as_ref().map_or(slot_w, |g| g.size().x + 20.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 22.0), Sense::click());
    let name = m.map_or_else(|| format!("empty slot {slot}"), |m| m.name.clone());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, lit, &name));
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.12);
    let y = rect.center().y;
    match parts {
        Some((f, n)) => {
            let (press, c) = widgets::paint_key(ui, &resp, rect, egui::CornerRadius::same(3), lit, theme::AMBER);
            let y = y + press;
            let mut x = rect.left() + 8.0;
            p.galley(egui::pos2(x, y - num.size().y / 2.0), num.clone(), theme::text_3());
            x += num.size().x + 6.0;
            p.galley(egui::pos2(x, y - f.size().y / 2.0), f.clone(), c);
            x += f.size().x + 5.0;
            p.galley(egui::pos2(x, y - n.size().y / 2.0 + 0.5), n, theme::text_2());
        }
        None => {
            let c = if armed { alpha(theme::AMBER, 0.8) } else { theme::lerp(theme::line_2(), theme::text_3(), hover) };
            let r = rect.shrink(0.5);
            let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
            p.extend(egui::Shape::dashed_line(&pts, Stroke::new(1.0, c), 3.0, 2.0));
            let g = plus.unwrap_or(num);
            p.galley(rect.center() - g.size() / 2.0, g, if armed || hover > 0.5 { theme::AMBER } else { theme::text_3() });
        }
    }
    resp
}

/// Recall a stored memory: both the row's slots and its menu do this.
fn recall(app: &mut App, m: &crate::prefs::Memory) {
    let (hz, mode) = (m.freq_hz, m.mode);
    app.active_b = false;
    app.tune(hz, true);
    if mode != app.status.mode {
        app.set_mode(mode);
    }
}

/// The memory keys, `room` wide: STORE, the first slots, and the free slot
/// after them or a menu of the rest.
pub fn memory_chips(app: &mut App, ui: &mut Ui, room: f32) {
    let list = app.memories();
    let inspect = app.inspect_hz();
    ui.spacing_mut().item_spacing.x = 4.0;
    let armed = app.memory_armed;
    let store = widgets::chip_key(ui, "STORE", armed, theme::AMBER).on_hover_text("Then pick a slot · Shift-click a slot also stores");
    if store.clicked() {
        app.memory_armed = !armed;
    }
    let more_w = 40.0;
    let slot_w = ((room - store.rect.width() - 4.0 - more_w) / SLOTS_SHOWN as f32 - 4.0).clamp(60.0, SLOT_W);
    let mut store_at = None;
    // The stored slots, then the one free slot after them: memories are
    // kept in order, so that is the only empty slot a store can fill.
    let fits = (((room - store.rect.width() - 4.0 - more_w) / (slot_w + 4.0)).floor() as usize).clamp(1, SLOTS_SHOWN);
    let shown = list.len().min(fits);
    let free = list.len() <= fits;
    for i in 0..shown + free as usize {
        let m = list.get(i);
        let lit = m.is_some_and(|m| m.mode == app.status.mode && (m.freq_hz - inspect).abs() <= (crate::tuning::mode_raster_hz(m.mode, m.freq_hz) / 2.0).max(1000.0));
        let r = memory_pill(ui, i + 1, m, lit, app.memory_armed, slot_w);
        let r = match m {
            Some(m) => r.on_hover_text(format!("{} · {} MHz · {} · Ctrl+{}", m.name, crate::tuning::fmt_mhz(m.freq_hz, 5), m.mode.label(), i + 1)),
            None => r.on_hover_text(format!("Store {:.4} MHz {} in slot {}", inspect / 1e6, app.status.mode.label(), i + 1)),
        };
        if r.clicked() {
            if m.is_none() || app.memory_armed || ui.input(|i| i.modifiers.shift) {
                store_at = Some(i);
            } else if let Some(m) = m {
                recall(app, m);
            }
        }
    }
    if list.len() > fits {
        let hidden = list.len() - fits;
        let picked = widgets::menu_chip(ui, &format!("+{hidden}"), theme::AMBER, |ui| {
            let mut out = None;
            for (i, m) in list.iter().enumerate().skip(fits) {
                if ui.button(format!("{}  {} MHz  {}", i + 1, crate::tuning::fmt_mhz(m.freq_hz, 4), m.name)).clicked() {
                    out = Some(i);
                    ui.close();
                }
            }
            if ui.button(format!("Store {} MHz in slot {}", crate::tuning::fmt_mhz(inspect, 4), list.len() + 1)).clicked() {
                out = Some(list.len());
                ui.close();
            }
            out
        })
        .flatten();
        match picked {
            Some(i) if app.memory_armed || i == list.len() => store_at = Some(i),
            Some(i) => recall(app, &list[i]),
            None => {}
        }
    }
    if let Some(i) = store_at {
        let mut l = list.clone();
        let m = crate::prefs::Memory { name: app.status.mode.label().into(), freq_hz: inspect.round(), mode: app.status.mode };
        if i < l.len() {
            l[i] = m;
        } else {
            l.push(m);
        }
        app.memory_armed = false;
        app.save_memories(l);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_zones_fill_the_bar_with_room_for_their_readings() {
        for w in [1076.0, 1100.0, 1240.0, 1256.0, 1400.0, 1499.0, 1500.0, 1576.0, 1876.0, 2536.0] {
            let z = zones(w, digits_h(w));
            let sum = z.rx + z.freq + z.signal + z.audio + z.mode + z.filter + z.step;
            assert!((sum - w).abs() < 0.01, "{w}: {sum}");
            // A label, S9+60 and -100.0 dBFS, and the CAL tag.
            assert!(z.signal >= 270.0, "{w}: signal {}", z.signal);
        }
    }

    #[test]
    fn every_mode_name_fits_its_zone() {
        for m in Mode::ALL {
            let room = 156.0 - 17.0 - 44.0 - 2.0;
            let label = m.label();
            let width = mode_size(room, label, 26.0) * 0.6 * label.chars().count() as f32;
            assert!(width <= room + 0.01, "{label}: {width}");
        }
    }
}
