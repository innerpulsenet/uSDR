//! The receiver bar: everything about the receiver being driven, in one
//! band across the top — which receiver, its readout and tuning, its mode,
//! its filter, its audio and its meter. It follows the active receiver, so
//! switching to B turns the same controls onto B.

use crate::app::App;
use crate::net::Get;
use crate::theme::{self, alpha};
use crate::vfo::VfoAct;
use crate::widgets::{self, Fader, Tone};
use egui::{RichText, Sense, Stroke, Ui, vec2};
use serde_json::json;
use usdr_proto::Mode;

const B_MODES: [Mode; 5] = [Mode::Nfm, Mode::Am, Mode::Usb, Mode::Lsb, Mode::Wfm];

fn small(t: impl Into<String>) -> RichText {
    RichText::new(t).font(theme::mono(10.5)).color(theme::text_3())
}

/// A bay: a labelled section of the bar, divided from the next by a hairline.
fn bay<R>(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    bay_with(ui, |ui| theme::engraved(ui, label, theme::mono(10.5), theme::text_3(), 1.0, egui::Sense::hover()), add)
}

/// A bay whose top line is more than its label.
fn bay_with<R, H>(ui: &mut Ui, head: impl FnOnce(&mut Ui) -> H, add: impl FnOnce(&mut Ui) -> R) -> R {
    let r = ui
        .vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            head(ui);
            add(ui)
        })
        .inner;
    let rect = ui.min_rect();
    let x = ui.cursor().left() + 6.0;
    ui.painter().line_segment([egui::pos2(x, rect.top() + 4.0), egui::pos2(x, rect.bottom() - 4.0)], Stroke::new(1.0, theme::line()));
    ui.add_space(12.0);
    r
}

/// Room the mode chips and the audio take, for sizing what is left.
const MODES_W: f32 = 290.0;
const AUDIO_W: f32 = 232.0;
const GAP: f32 = 22.0;

/// Where each one-band fit is first tried, widest first: estimates of the
/// bar each needs, which `receiver_bar` raises to what it measures.
const ONE_BAND: [f32; 3] = [1840.0, 1700.0, 1554.0];

pub fn receiver_bar(app: &mut App, ui: &mut Ui) {
    let w = ui.available_width();
    let edge = ui.max_rect().right();
    let b_active = app.active_b && app.status.rx2.is_some();
    // A violet wash says, before anything is read, that B is being driven;
    // it fades in and out, so the switch is seen before it is read.
    let wash = ui.ctx().animate_bool_with_time(egui::Id::new("rx-b-wash"), b_active, 0.2);
    if wash > 0.01 {
        let r = ui.max_rect();
        ui.painter().rect_filled(r, egui::CornerRadius::ZERO, alpha(theme::VIOLET, 0.06 * wash));
    }
    ui.spacing_mut().item_spacing.x = 10.0;
    // The narrowest bar each one-band fit has been seen to need. A fit whose
    // row ran past the edge is not tried again below the width it took, so
    // the signal bay at the end of the row is never cut off: the readings
    // and labels that set the bays' widths change with the mode, B and the
    // station, so measuring beats any fixed threshold.
    let need_id = egui::Id::new("rx-bar-need");
    let mut need: [f32; 3] = ui.data(|d| d.get_temp(need_id)).unwrap_or(ONE_BAND);
    let fit = need.iter().position(|&n| w >= n);
    // Four fits, widest first. Each row ends in a section that takes what
    // is left, so no row trails off into empty space.
    let row = match fit {
        Some(0) => Some(
            ui.horizontal_top(|ui| {
                switch(app, ui);
                readout(app, ui);
                tuning(app, ui);
                modes(app, ui);
                filter(app, ui, 310.0);
                audio(app, ui, 220.0);
                meter(app, ui, 300.0, 560.0);
            })
            .response
            .rect,
        ),
        // The filter and the meter narrow.
        Some(1) => Some(
            ui.horizontal_top(|ui| {
                switch(app, ui);
                readout(app, ui);
                tuning(app, ui);
                modes(app, ui);
                filter(app, ui, 220.0);
                audio(app, ui, 200.0);
                meter(app, ui, 0.0, 560.0);
            })
            .response
            .rect,
        ),
        // The default 1600 px window: still one band, with a smaller
        // readout, a narrow filter and audio, and the meter at the width
        // its readings need. Two rows here would cost the waterfall 110 px.
        Some(_) => Some(
            ui.horizontal_top(|ui| {
                switch(app, ui);
                readout_sized(app, ui, 32.0);
                tuning(app, ui);
                modes(app, ui);
                filter(app, ui, 140.0);
                audio(app, ui, 160.0);
                meter(app, ui, 0.0, 560.0);
            })
            .response
            .rect,
        ),
        None => {
            ui.horizontal_top(|ui| {
                switch(app, ui);
                readout(app, ui);
                tuning(app, ui);
                // A long bar is a finer one: on its own row the meter may run on.
                meter(app, ui, 260.0, 900.0);
            });
            ui.add_space(4.0);
            // A fader wider than this is a slab of empty track.
            let filter_w = (w - MODES_W - AUDIO_W - 3.0 * GAP).clamp(230.0, 380.0);
            ui.horizontal_top(|ui| {
                modes(app, ui);
                filter(app, ui, filter_w);
                audio(app, ui, 220.0);
            });
            None
        }
    };
    if let (Some(i), Some(r)) = (fit, row) {
        let over = r.right() - edge;
        if over > 0.5 {
            need[i] = need[i].max((w + over + 2.0).ceil());
            ui.data_mut(|d| d.insert_temp(need_id, need));
            // Lay the bar out again before anything is shown.
            ui.ctx().request_discard("the receiver bar ran past its edge");
        }
    }
}

/// Width of the RX bay's two rows: the same with or without B, so starting
/// or stopping B moves nothing else on the bar.
const RX_W: f32 = 96.0;

fn switch(app: &mut App, ui: &mut Ui) {
    bay(ui, "RX", |ui| {
        let has_b = app.status.rx2.is_some();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            let half = (RX_W - 3.0) / 2.0;
            if widgets::chip_key_w(ui, "A", !app.active_b || !has_b, theme::AMBER, half).on_hover_text("Drive receiver A (B toggles)").clicked() {
                app.active_b = false;
            }
            if has_b {
                if widgets::chip_key_w(ui, "B", app.active_b, theme::VIOLET, half).on_hover_text("Drive receiver B (B toggles)").clicked() {
                    app.active_b = true;
                }
            } else {
                let can = app.rx2_available();
                let r = ui
                    .add_enabled_ui(can, |ui| widgets::chip_key_w(ui, "+B", false, theme::VIOLET, half))
                    .inner
                    .on_hover_text("Start receiver B beside A (or middle-click the display)")
                    .on_disabled_hover_text("This server has no second receiver");
                if r.clicked() {
                    start_b(app);
                }
            }
        });
        // B's own keys, always laid out and dimmed while B is off.
        ui.add_enabled_ui(has_b, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                if widgets::chip_key_w(ui, "SWAP", false, theme::VIOLET, 41.0).on_hover_text("Trade frequencies between A and B").clicked()
                    && let Some(b) = app.status.rx2.clone()
                {
                    let a = app.inspect_hz();
                    app.active_b = false;
                    app.tune(b.freq_hz, false);
                    app.set_rx2(a, b.mode);
                }
                // One ear each, for following two channels at once.
                let split = has_b && app.prefs.pan <= -0.99 && app.prefs.pan_b >= 0.99;
                if widgets::chip_key_w(ui, "L|R", split, theme::VIOLET, 31.0).on_hover_text(if split { "Both receivers back to the center" } else { "A in the left ear, B in the right" }).clicked() {
                    let (a, b) = if split { (0.0, 0.0) } else { (-1.0, 1.0) };
                    app.prefs.pan = a;
                    app.prefs.pan_b = b;
                    app.player.set_pan(a);
                    app.player_b.set_pan(b);
                    app.sync_fold();
                }
                if widgets::chip_key_w(ui, "✕", false, theme::VIOLET, RX_W - 41.0 - 31.0 - 6.0).on_hover_text("Stop receiver B").clicked() {
                    app.rx2_off();
                }
            });
        });
    });
}

fn start_b(app: &mut App) {
    let a = app.inspect_hz();
    let span = (app.status.max_freq_hz - app.status.min_freq_hz).max(0.0);
    let hz = if a + span / 8.0 < app.status.max_freq_hz { a + span / 8.0 } else { a - span / 8.0 };
    let step = crate::tuning::mode_raster_hz(app.status.mode, hz);
    app.place_b(crate::tuning::snap(hz, step));
    app.active_b = true;
}

fn readout(app: &mut App, ui: &mut Ui) {
    readout_sized(app, ui, 40.0);
}

fn readout_sized(app: &mut App, ui: &mut Ui, digits_px: f32) {
    let b = app.status.rx2.clone().filter(|_| app.active_b);
    let hz = app.active_hz();
    let mut acts = Vec::new();
    let rm = app.reduce_motion();
    bay(ui, if b.is_some() { "RECEIVER B" } else { "RECEIVER A" }, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let (mode, bw) = match &b {
                Some(b) => (b.mode, b.bandwidth_hz),
                None => (app.status.mode, app.status.bandwidth_hz),
            };
            ui.label(RichText::new(mode.label()).font(theme::mono_bold(10.0)).color(if b.is_some() { theme::VIOLET } else { theme::AMBER }));
            ui.label(small(crate::tuning::fmt_hz(bw)));
            let agc = match (app.status.gain_db, app.status.tuner_agc) {
                (None, true) => if app.status.is_sdrplay() { "IF AGC".to_string() } else { "TUNER AGC".into() },
                (None, false) => "AGC".into(),
                (Some(g), _) => format!("MAN {g:.0} dB"),
            };
            ui.label(small(agc));
        });
        app.vfo.tint = Some(if b.is_some() { theme::VIOLET } else { theme::AMBER });
        app.vfo.ui(ui, hz, digits_px, rm, &mut acts);
    });
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

fn tuning(app: &mut App, ui: &mut Ui) {
    let hz = app.active_hz();
    bay(ui, "STEP", |ui| {
        let auto = crate::tuning::mode_raster_hz(app.status.mode, hz);
        let label = match app.prefs.step_hz {
            None => format!("AUTO {}", crate::tuning::fmt_short_hz(auto)),
            Some(s) => crate::tuning::fmt_short_hz(s),
        };
        let color = if app.prefs.step_hz.is_some() { theme::AMBER } else { theme::text() };
        let top = ui.horizontal(|ui| {
            widgets::selector(ui, "step", &label, color, 118.0, |ui| {
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
            if widgets::chip_key(ui, "SPAN LOCK", lock, theme::CYAN)
                .on_hover_text("Keep the span where it is: tuning moves only the receiver (L)")
                .clicked()
            {
                app.prefs.span_lock = !lock;
            }
        });
        // The jog keys either side of the strip, « ‹ [strip] › », as wide
        // as the row above, so the dial is there at every window width.
        let chev = ui.painter().layout_no_wrap("«".into(), theme::mono_bold(10.5), egui::Color32::PLACEHOLDER).size().x + 14.0;
        let strip_w = (top.response.rect.width() - 4.0 * chev - 4.0 * 3.0).max(48.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            let jog = |ui: &mut Ui, app: &mut App, lbl: &str, n: f64| {
                if widgets::chip_key(ui, lbl, false, theme::AMBER).on_hover_text(format!("{n:+} step · Shift for a tenth")).clicked() {
                    let fine = ui.input(|i| i.modifiers.shift);
                    let st = if fine { (app.step_hz() / 10.0).round().max(100.0) } else { app.step_hz() };
                    let target = app.active_hz() + n * st;
                    app.tune(target, true);
                }
            };
            jog(ui, app, "«", -10.0);
            jog(ui, app, "‹", -1.0);
            if let Some(d) = crate::vfo::tune_strip(ui, hz, app.step_hz(), strip_w, 22.0) {
                let target = app.active_hz() + d;
                app.tune(target, true);
            }
            jog(ui, app, "›", 1.0);
            jog(ui, app, "»", 10.0);
        });
    });
}

fn mode_tally(m: Mode) -> egui::Color32 {
    match m {
        Mode::Nfm | Mode::Am | Mode::Usb | Mode::Lsb => theme::AMBER,
        Mode::Wfm | Mode::Pager | Mode::Flex | Mode::Ft8 | Mode::Ft4 => theme::CYAN,
        Mode::P25 | Mode::Dmr | Mode::Nxdn | Mode::Packet => theme::OK,
        _ => theme::VIOLET,
    }
}

/// The listening modes, with AUTO and SCAN: receiver A's first row.
const LISTEN: [Mode; 7] = [Mode::Nfm, Mode::Am, Mode::Usb, Mode::Lsb, Mode::Wfm, Mode::Auto, Mode::Scan];

/// The MODE bay's width: the widest of what it shows for A and for B, so
/// switching receivers does not move the bays after it.
fn modes_width(ui: &Ui) -> f32 {
    let chip = |l: &str| ui.painter().layout_no_wrap(l.to_string(), theme::mono_bold(10.5), egui::Color32::PLACEHOLDER).size().x + 14.0;
    let row = |ms: &mut dyn Iterator<Item = Mode>| {
        let ws: Vec<f32> = ms.map(|m| chip(m.label())).collect();
        ws.iter().sum::<f32>() + 3.0 * ws.len().saturating_sub(1) as f32
    };
    let note = ui.painter().layout_no_wrap(B_NOTE.into(), theme::mono(10.5), egui::Color32::PLACEHOLDER).size().x;
    row(&mut LISTEN.into_iter())
        .max(row(&mut Mode::ALL.into_iter().filter(|m| !LISTEN.contains(m))))
        .max(row(&mut B_MODES.into_iter()))
        .max(note)
}

const B_NOTE: &str = "receiver B listens; the decoders run on A";

fn modes(app: &mut App, ui: &mut Ui) {
    let b = app.status.rx2.clone().filter(|_| app.active_b);
    let w = modes_width(ui);
    bay(ui, "MODE", |ui| {
        ui.set_min_width(w);
        modes_inner(app, ui, b)
    });
}

fn modes_inner(app: &mut App, ui: &mut Ui, b: Option<usdr_proto::SubRx>) {
    match b {
        Some(b) => {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                for m in B_MODES {
                    if widgets::chip_key(ui, m.label(), b.mode == m, theme::VIOLET).clicked() && b.mode != m {
                        app.set_rx2(b.freq_hz, m);
                    }
                }
            });
            ui.label(small(B_NOTE));
        }
        None => {
            // Listening modes and the two that pick for you on the first
            // row, the decoders on the second: those the server offers, so
            // an older one is never asked for a mode it does not have.
            let offered = |m: &Mode| app.modes.is_empty() || app.modes.iter().any(|c| c.mode == *m);
            let decode: Vec<Mode> = Mode::ALL.iter().copied().filter(|m| !LISTEN.contains(m) && offered(m)).collect();
            for row in [&LISTEN[..], &decode[..]] {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    for m in row {
                        let on = app.status.mode == *m;
                        let desc = app.modes.iter().find(|c| c.mode == *m).map(|c| c.description.clone()).unwrap_or_default();
                        let r = widgets::chip_key(ui, m.label(), on, mode_tally(*m));
                        let r = if desc.is_empty() { r } else { r.on_hover_text(desc) };
                        if r.clicked() && !on {
                            app.set_mode(*m);
                        }
                    }
                });
            }
        }
    }
}

fn filter(app: &mut App, ui: &mut Ui, width_px: f32) {
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
    let range = if app.server.as_ref().is_some_and(|i| i.has("filter")) {
        app.modes.iter().find(|c| c.mode == mode).and_then(|c| c.filter)
    } else {
        None
    };
    let custom = if b.is_some() { range.is_some() && (width - app.modes.iter().find(|c| c.mode == mode).map_or(0.0, |c| c.bandwidth_hz)).abs() > 1.0 } else { app.status.filter_custom };
    let label = format!("FILTER  {} … {}", k(lo), k(hi));
    bay(ui, &label, |ui| {
        ui.set_min_width(width_px);
        ui.set_max_width(width_px);
        let Some(range) = range else {
            // One short line, so it never wraps in the narrow bay; the
            // whole of it is a hover away.
            let (note, tip) = if app.server.as_ref().is_some_and(|i| i.has("filter")) {
                ("set by the decoder", format!("{}'s decoder sets its own channel filter", mode.label()))
            } else {
                ("fixed on this server", "This server predates the adjustable filter".to_string())
            };
            ui.label(small(note)).on_hover_text(tip);
            return;
        };
        let mut presets: Vec<f32> = match mode {
            Mode::Usb | Mode::Lsb => vec![1_800.0, 2_100.0, 2_400.0, 2_700.0, 3_000.0, 3_600.0],
            Mode::Am => vec![4_000.0, 6_000.0, 8_000.0, 10_000.0, 12_000.0, 16_000.0],
            Mode::Nfm => vec![6_000.0, 8_000.0, 11_000.0, 12_500.0, 16_000.0, 25_000.0],
            Mode::Wfm => vec![100_000.0, 150_000.0, 180_000.0, 200_000.0, 230_000.0],
            _ => vec![],
        };
        // The mode's own width is always a preset, and choosing it is the
        // reset: no separate key to find once the filter has wandered.
        let home = app.modes.iter().find(|c| c.mode == mode).map(|c| c.bandwidth_hz).filter(|w| *w > 0.0);
        if let Some(h) = home
            && !presets.iter().any(|p| (p - h).abs() < 1.0)
        {
            presets.push(h);
            presets.sort_by(f32::total_cmp);
        }
        presets.retain(|p| *p >= range.min_width_hz - 1.0 && *p <= range.max_width_hz + 1.0);
        if presets.is_empty() {
            presets.push(width);
        }
        let around = |w: f32| -> (f32, f32) {
            match mode {
                Mode::Usb => (lo, (lo + w).min(range.max_edge_hz)),
                Mode::Lsb => ((hi - w).max(-range.max_edge_hz), hi),
                _ => (-w / 2.0, w / 2.0),
            }
        };
        let mut set: Option<(f32, f32)> = None;
        let mut reset = false;
        let is_home = |p: f32| home.is_some_and(|h| (p - h).abs() < 1.0);
        let label_of = |p: f32| format!("{}k", format!("{:.1}", p / 1000.0).trim_end_matches(".0"));
        // Arrows either side always reach the next preset narrower and
        // wider, so a narrow bay never strands the width at one end.
        let narrower = presets.iter().rev().copied().find(|p| *p < width - 1.0);
        let wider = presets.iter().copied().find(|p| *p > width + 1.0);
        // Between them, as many presets as fit, grown outward from the one
        // nearest the width in use so its neighbors on both sides show.
        const GAP_X: f32 = 3.0;
        let chip_w = |p: f32| ui.painter().layout_no_wrap(label_of(p), theme::mono_bold(10.5), egui::Color32::PLACEHOLDER).size().x + 14.0 + GAP_X;
        let widths: Vec<f32> = presets.iter().map(|p| chip_w(*p)).collect();
        let room = width_px - 2.0 * (20.0 + GAP_X);
        let near = presets.iter().enumerate().min_by(|a, b| (a.1 - width).abs().total_cmp(&(b.1 - width).abs())).map_or(0, |(i, _)| i);
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
                && let Some(p) = narrower
            {
                set = Some(around(p));
            }
            for p in &presets[first..=last] {
                let on = (width - p).abs() < 1.0;
                let mut r = widgets::chip_key(ui, &label_of(*p), on, theme::AMBER);
                if is_home(*p) {
                    // A green pip marks the mode's own filter.
                    let c = r.rect.right_top() + vec2(-4.0, 4.0);
                    ui.painter().circle_filled(c, 2.0, if custom { theme::OK } else { alpha(theme::OK, 0.6) });
                    r = r.on_hover_text(if custom { "The mode's own filter — click to reset" } else { "The mode's own filter (in use)" });
                }
                if r.clicked() {
                    if is_home(*p) {
                        reset = true;
                    } else {
                        set = Some(around(*p));
                    }
                }
            }
            if widgets::step_chip(ui, true, wider.is_some()).on_hover_text("Wider (wheel up over the presets)").clicked()
                && let Some(p) = wider
            {
                set = Some(around(p));
            }
        });
        if row.response.contains_pointer() {
            let notches = widgets::wheel(ui);
            if notches != 0.0 {
                widgets::eat_scroll(ui);
                if let Some(p) = if notches > 0.0 { wider } else { narrower } {
                    set = Some(around(p));
                }
            }
        }
        // Stepping onto the mode's own width is the reset too, so it lands
        // with the mode's own edges (sideband filters sit off the carrier).
        if let Some((l, h)) = set
            && is_home(h - l)
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
        let mut wv = width as f64;
        let (ch, _) = Fader {
            value: &mut wv,
            min: range.min_width_hz as f64,
            max: range.max_width_hz as f64,
            step: if range.max_width_hz > 50_000.0 { 5_000.0 } else { 100.0 },
            fine: if range.max_width_hz > 50_000.0 { 1_000.0 } else { 50.0 },
            label: "WIDTH",
            text: |v| k(v as f32),
            color: theme::AMBER,
            enabled: true,
            ticks: vec![],
        }
        .show_compact(ui);
        if ch {
            set = Some(around(wv as f32));
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
    });
}

fn audio(app: &mut App, ui: &mut Ui, width_px: f32) {
    let is_b = app.active_b && app.status.rx2.is_some();
    bay(ui, if is_b { "AUDIO B" } else { "AUDIO A" }, |ui| {
        ui.set_min_width(width_px - 20.0);
        ui.set_max_width(width_px);
        let (vol, muted) = if is_b { (app.prefs.volume_b, app.prefs.muted_b) } else { (app.prefs.volume, app.prefs.muted) };
        let mut v = vol as f64 * 100.0;
        // 100 is unity, at the middle mark; past it is up to +12 dB of boost
        // for weak signals, with the player's limiter keeping it clean.
        let (ch, _) = Fader {
            value: &mut v,
            min: 0.0,
            max: 200.0,
            step: 2.0,
            fine: 1.0,
            label: "AF GAIN",
            text: |v| if muted { "muted".into() } else { format!("{v:.0}") },
            color: if is_b { theme::VIOLET } else { theme::AMBER },
            enabled: !muted,
            ticks: vec![(0.5, String::new())],
        }
        .show_compact(ui);
        if ch {
            if is_b {
                app.prefs.volume_b = (v / 100.0) as f32;
                app.player_b.set_volume(app.prefs.volume_b);
            } else {
                app.prefs.volume = (v / 100.0) as f32;
                app.player.set_volume(app.prefs.volume);
            }
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            if widgets::chip_key(ui, if muted { "MUTED" } else { "MUTE" }, muted, theme::WARN).on_hover_text("M").clicked() {
                if is_b {
                    app.prefs.muted_b = !muted;
                    app.player_b.set_muted(app.prefs.muted_b);
                } else {
                    app.prefs.muted = !muted;
                    app.player.set_muted(app.prefs.muted);
                }
            }
            let player = if is_b { app.player_b.clone() } else { app.player.clone() };
            let rec = player.reading().recording.is_some();
            if widgets::chip_key(ui, if rec { "● REC" } else { "REC" }, rec, theme::EMBER).on_hover_text("Record what this receiver plays to a WAV (R)").clicked() {
                let ctx = ui.ctx().clone();
                if is_b {
                    app.toggle_recording_b(&ctx);
                } else {
                    app.toggle_local_recording(&ctx);
                }
            }
            // Output level, beside the keys.
            let r = player.reading();
            let lw = (ui.available_width() - 34.0).clamp(40.0, 140.0);
            level_meter(ui, lw, r.rms, r.peak);
            widgets::menu_chip(ui, "···", theme::text_2(), |ui| {
                ui.set_min_width(220.0);
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
        });
    });
}

/// What the receiver is playing, −60 to 0 dBFS, in a well at chip height:
/// RMS as lit segments (green, amber from −12, red from −3), the peak as a
/// pip, ticks at −40, −20 and −6, and a clip lamp that holds a second.
fn level_meter(ui: &mut Ui, w: f32, rms: f32, peak: f32) {
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 22.0), Sense::hover());
    let p = ui.painter_at(rect.expand(1.0));
    theme::well(&p, rect, None);
    let db = |v: f32| 20.0 * v.max(1e-6).log10();
    let lamp_w = 10.0;
    let bar = egui::Rect::from_min_max(egui::pos2(rect.left() + 5.0, rect.top() + 6.0), egui::pos2(rect.right() - 5.0 - lamp_w, rect.top() + 13.0));
    let x_of = |d: f32| bar.left() + bar.width() * ((d + 60.0) / 60.0).clamp(0.0, 1.0);
    let lit_to = x_of(db(rms));
    let mut x = bar.left();
    while x < bar.right() - 0.5 {
        let d = (x - bar.left()) / bar.width() * 60.0 - 60.0;
        let c = if d >= -3.0 { theme::ERR } else if d >= -12.0 { theme::AMBER } else { theme::OK };
        let seg = egui::Rect::from_min_max(egui::pos2(x, bar.top()), egui::pos2((x + 2.0).min(bar.right()), bar.bottom()));
        p.rect_filled(seg, egui::CornerRadius::ZERO, if x < lit_to { c } else { alpha(c, 0.10) });
        x += 3.0;
    }
    if peak > 1e-4 {
        let px = x_of(db(peak));
        p.line_segment([egui::pos2(px, bar.top() - 1.0), egui::pos2(px, bar.bottom() + 1.0)], Stroke::new(1.5, theme::text()));
    }
    for d in [-40.0, -20.0, -6.0] {
        let tx = x_of(d);
        p.line_segment([egui::pos2(tx, bar.bottom() + 2.0), egui::pos2(tx, bar.bottom() + 5.0)], Stroke::new(1.0, theme::text_3()));
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

fn meter(app: &mut App, ui: &mut Ui, min_w: f32, max_w: f32) {
    let is_b = app.active_b && app.status.rx2.is_some();
    let rm = app.reduce_motion();
    let mark = app.prefs.view.sql_mark;
    let s = &app.status;
    let err = s.freq_error_hz.filter(|_| !is_b);
    // Never narrower than its header, so the CAL key cannot land on the
    // reading; past that the last bay takes whatever width the row has left.
    let text_w = |t: &str, f: egui::FontId| ui.painter().layout_no_wrap(t.into(), f, egui::Color32::PLACEHOLDER).size().x;
    let label = if is_b { "SIGNAL B" } else { "SIGNAL" };
    // (The label is set with a point of tracking a letter.)
    let head_w = text_w(label, theme::mono(10.5))
        + label.len() as f32
        + text_w("S9+60", theme::mono_bold(15.0))
        + text_w("-100.0 dBFS", theme::mono(11.0))
        + 2.0 * 8.0
        + err.map_or(0.0, |_| 8.0 + text_w("CAL -99.9k", theme::mono_bold(10.0)) + 12.0)
        + 8.0;
    // (Less the gap and hairline space the bay leaves after itself, so the
    // row ends at the bar's edge.)
    let w = (ui.available_width() - 22.0).clamp(min_w.max(head_w), max_w.max(head_w));
    let now = ui.input(|i| i.time);
    if err.is_some_and(|e| e.abs() > 2000.0) {
        app.err_warn_until = now + 5.0;
    }
    let err_warn = now < app.err_warn_until;
    let ((s_txt, s_col), (db_txt, db_col)) = if is_b { app.meter_b.readout() } else { app.meter.readout() };
    let mut calibrate = false;
    // The header is the bay's label, with the reading beside it, so the
    // bar below can have the bay's whole width.
    let head = |ui: &mut Ui| {
        // The bay's width is set here, before the CAL key is pushed to its
        // right edge.
        ui.set_min_width(w);
        ui.set_max_width(w);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            theme::engraved(ui, if is_b { "SIGNAL B" } else { "SIGNAL" }, theme::mono(10.5), theme::text_3(), 1.0, egui::Sense::hover());
            // Fixed widths in the mono face, so nothing after them moves
            // while the numbers change.
            fixed(ui, &s_txt, theme::mono_bold(15.0), s_col, "S9+60");
            fixed(ui, &db_txt, theme::mono(11.0), db_col, "-100.0 dBFS");
            if let Some(e) = err {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let t = if e.abs() >= 1_000.0 { format!("CAL {:+.1}k", e / 1000.0) } else { format!("CAL {e:+.0}") };
                    let r = widgets::mini_key(ui, &t, theme::AMBER, err_warn)
                        .on_hover_text("The carrier is this far from where the receiver put it: click to calibrate the ppm from it");
                    calibrate = r.clicked();
                });
            }
        });
    };
    bay_with(ui, head, |ui| {
        ui.spacing_mut().item_spacing.y = 3.0;
        if is_b {
            app.meter_b.ui_bar(ui, rm, mark);
            let snr = app.meter_b.snr.map_or(format!("{:>4} dB", "—"), |s| format!("{s:>4.1} dB"));
            ui.label(small(format!("SNR {snr} · measured from the spectrum")));
            return;
        }
        app.meter.ui_bar(ui, rm, mark);
        let s = &app.status;
        let snr = app.meter.snr.map_or(format!("{:>4} dB", "—"), |d| format!("{d:>4.1} dB"));
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
            let (c, word) = app.shown_class();
            let tone = match word {
                "ACTIVE" => Tone::Ok,
                "HELD" => Tone::Warn,
                _ => Tone::Idle,
            };
            widgets::chip(ui, tone, word);
            if s.mode == Mode::Wfm
                && let Some(st) = app.player.reading().stereo
            {
                stereo_badge(ui, &st, rm);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(small(format!("SNR {snr}")));
                if !notches.is_empty() {
                    ui.label(RichText::new(format!("notch {}", notches.join(" "))).font(theme::mono(10.0)).color(theme::VIOLET));
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    // The protocol alone, without its qualifier, so it
                    // fits; the whole of it and the modulation are a hover
                    // away.
                    let short = |p: &str| p.split(" (").next().unwrap_or(p).to_string();
                    let rds = app.rds().and_then(|r| rds_line(r).map(|t| (t, r)));
                    let (txt, tip, col) = match (&c, rds) {
                        // A broadcast station's name and what it says is on.
                        (_, Some((t, r))) => (t, Some(rds_tip(r)), if r.locked { widgets::protocol_color("RDS") } else { theme::text_3() }),
                        (Some(c), None) if word != "IDLE" => (short(&c.protocol), Some(format!("{} · {}", c.protocol, c.modulation)), widgets::protocol_color(&c.protocol)),
                        _ => (crate::app::mode_idle_text(app.status.mode).to_string(), None, theme::text_3()),
                    };
                    let r = ui.add(egui::Label::new(RichText::new(txt).font(theme::sans(12.0)).color(col)).truncate());
                    if let Some(t) = tip {
                        r.on_hover_text(t);
                    }
                });
            });
        });
    });
    if calibrate && let Some(l) = &app.link {
        l.post_get("/api/sdr/calibrate", json!({}), Get::Calibrate);
    }
}

/// The stereo decoder's reading, as the audio now playing carries it: two
/// rings that sit on top of each other for mono and part as the blend opens
/// to full separation, lit while a pilot is locked. Shown only when the
/// server measured it.
fn stereo_badge(ui: &mut Ui, st: &usdr_proto::StereoReading, reduce_motion: bool) {
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
            crate::anim::more(ui.ctx());
            cur + (target - cur) * (1.0 - (-dt * 14.0).exp())
        };
        ui.ctx().data_mut(|d| d.insert_temp(key, next));
        next
    };
    let lit = ui.ctx().animate_bool_with_time(id.with("lit"), st.pilot, if reduce_motion { 0.0 } else { 0.2 });
    let col = theme::lerp(theme::text_3(), theme::CYAN, lit);
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
    resp.on_hover_text(if st.pilot {
        format!(
            "19 kHz pilot locked{}\nSeparation in use: {:.0}%, blended toward mono as the S/N falls\nStereo S/N {}, from the noise beside the pilot",
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
pub fn memory_row(app: &mut App, ui: &mut Ui) {
    ui.horizontal(|ui| memory_chips(app, ui));
}

/// Every memory slot's width, filled or empty, so a slot is always in the
/// same place: storing one never moves the slots after it.
pub const SLOT_W: f32 = 108.0;

/// A memory slot: its number (Ctrl+number recalls it), the frequency and
/// its name in the mode's color, the name cut to the slot; an empty slot is
/// a dashed outline with its number, which brightens while STORE is waiting
/// for a slot.
fn memory_pill(ui: &mut Ui, slot: usize, m: Option<&crate::prefs::Memory>, lit: bool, armed: bool) -> egui::Response {
    let num_f = theme::mono(10.0);
    let font = theme::mono_bold(10.5);
    let p = ui.painter().clone();
    let num = p.layout_no_wrap(slot.to_string(), num_f, egui::Color32::PLACEHOLDER);
    let parts = m.map(|m| {
        let mhz = crate::tuning::fmt_mhz(m.freq_hz, 4);
        let mhz = mhz.trim_end_matches('0').trim_end_matches('.').to_string();
        let f = p.layout_no_wrap(mhz, font.clone(), egui::Color32::PLACEHOLDER);
        let room = SLOT_W - 8.0 - num.size().x - 6.0 - f.size().x - 5.0 - 8.0;
        let name = widgets::ellipsize(&p, &m.name, theme::mono_bold(10.0), room);
        (f, p.layout_no_wrap(name, theme::mono_bold(10.0), egui::Color32::PLACEHOLDER), mode_tally(m.mode))
    });
    let (rect, resp) = ui.allocate_exact_size(vec2(SLOT_W, 22.0), Sense::click());
    let name = m.map_or_else(|| format!("empty slot {slot}"), |m| m.name.clone());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, lit, &name));
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.12);
    let t = ui.ctx().animate_bool_with_time(resp.id.with("on"), lit, 0.15);
    let cr = egui::CornerRadius::same(3);
    let y = rect.center().y;
    match parts {
        Some((f, n, tally)) => {
            let bg = theme::lerp(theme::lerp(theme::surface_3(), theme::surface_4(), hover), alpha(theme::AMBER, 0.20), t);
            p.rect(rect, cr, bg, Stroke::new(1.0, theme::lerp(theme::line(), alpha(theme::AMBER, 0.75), t)), egui::StrokeKind::Inside);
            let mut x = rect.left() + 8.0;
            p.galley(egui::pos2(x, y - num.size().y / 2.0), num.clone(), theme::text_3());
            x += num.size().x + 6.0;
            p.galley(egui::pos2(x, y - f.size().y / 2.0), f.clone(), theme::lerp(theme::text_2(), theme::text(), t.max(hover)));
            x += f.size().x + 5.0;
            p.galley(egui::pos2(x, y - n.size().y / 2.0 + 0.5), n, alpha(tally, 0.9));
        }
        None => {
            let c = if armed { alpha(theme::AMBER, 0.8) } else { theme::lerp(theme::line_2(), theme::text_3(), hover) };
            let r = rect.shrink(0.5);
            let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
            p.extend(egui::Shape::dashed_line(&pts, Stroke::new(1.0, c), 3.0, 2.0));
            p.galley(rect.center() - num.size() / 2.0, num, if armed { theme::AMBER } else { theme::text_3() });
        }
    }
    resp
}

/// The memory chips themselves, for a row of their own or the end of the
/// display toolbar.
pub fn memory_chips(app: &mut App, ui: &mut Ui) {
    let list = app.memories();
    let inspect = app.inspect_hz();
    {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(small("MEMORIES"));
        let armed = app.memory_armed;
        if widgets::chip_key(ui, "STORE", armed, theme::AMBER).on_hover_text("Then pick a slot · Shift-click a slot also stores").clicked() {
            app.memory_armed = !armed;
        }
        let mut store_at = None;
        let slots = list.len().max(8);
        for i in 0..slots {
            let m = list.get(i);
            let lit = m.is_some_and(|m| m.mode == app.status.mode && (m.freq_hz - inspect).abs() <= (crate::tuning::mode_raster_hz(m.mode, m.freq_hz) / 2.0).max(1000.0));
            let r = memory_pill(ui, i + 1, m, lit, app.memory_armed);
            let r = match m {
                Some(m) => r.on_hover_text(format!("{} · {} MHz · {} · Ctrl+{}", m.name, crate::tuning::fmt_mhz(m.freq_hz, 5), m.mode.label(), i + 1)),
                None => r.on_hover_text(format!("Slot {} is empty: STORE, then this slot", i + 1)),
            };
            if r.clicked() {
                if app.memory_armed || ui.input(|i| i.modifiers.shift) {
                    store_at = Some(i);
                } else if let Some(m) = m {
                    let (hz, mode) = (m.freq_hz, m.mode);
                    app.active_b = false;
                    app.tune(hz, true);
                    if mode != app.status.mode {
                        app.set_mode(mode);
                    }
                }
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
        if app.memory_armed {
            ui.label(RichText::new(format!("pick a slot for {} {}", crate::tuning::fmt_mhz(inspect, 4), app.status.mode.label())).font(theme::mono(10.0)).color(theme::AMBER));
        }
    }
}
