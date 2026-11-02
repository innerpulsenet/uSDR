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
    RichText::new(t).font(theme::mono(10.0)).color(theme::text_3())
}

/// A bay: a labelled section of the bar, divided from the next by a hairline.
fn bay<R>(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let r = ui
        .vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.label(RichText::new(label).font(theme::mono(9.5)).color(theme::text_3()).extra_letter_spacing(1.0));
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

pub fn receiver_bar(app: &mut App, ui: &mut Ui) {
    let w = ui.available_width();
    let b_active = app.active_b && app.status.rx2.is_some();
    // A violet wash says, before anything is read, that B is being driven.
    if b_active {
        let r = ui.max_rect();
        ui.painter().rect_filled(r, egui::CornerRadius::ZERO, alpha(theme::VIOLET, 0.05));
    }
    ui.spacing_mut().item_spacing.x = 10.0;
    // Three fits, widest first. Each row ends in a section that takes what
    // is left, so no row trails off into empty space.
    if w >= 1840.0 {
        ui.horizontal_top(|ui| {
            switch(app, ui);
            readout(app, ui);
            tuning(app, ui, true);
            modes(app, ui);
            filter(app, ui, 310.0);
            audio(app, ui);
            meter(app, ui, 300.0, 560.0);
        });
    } else if w >= 1640.0 {
        // The tune strip folds away (the jog keys stay); filter and meter narrow.
        ui.horizontal_top(|ui| {
            switch(app, ui);
            readout(app, ui);
            tuning(app, ui, false);
            modes(app, ui);
            filter(app, ui, 250.0);
            audio(app, ui);
            meter(app, ui, 230.0, 560.0);
        });
    } else {
        ui.horizontal_top(|ui| {
            switch(app, ui);
            readout(app, ui);
            tuning(app, ui, false);
            meter(app, ui, 260.0, 760.0);
        });
        ui.add_space(4.0);
        let filter_w = (w - MODES_W - AUDIO_W - 3.0 * GAP).clamp(230.0, 900.0);
        ui.horizontal_top(|ui| {
            modes(app, ui);
            filter(app, ui, filter_w);
            audio(app, ui);
        });
    }
}

fn switch(app: &mut App, ui: &mut Ui) {
    bay(ui, "RX", |ui| {
        let has_b = app.status.rx2.is_some();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            if widgets::chip_key(ui, "A", !app.active_b || !has_b, theme::AMBER).on_hover_text("Drive receiver A (B toggles)").clicked() {
                app.active_b = false;
            }
            if has_b {
                if widgets::chip_key(ui, "B", app.active_b, theme::VIOLET).on_hover_text("Drive receiver B (B toggles)").clicked() {
                    app.active_b = true;
                }
            } else if app.rx2_available()
                && widgets::chip_key(ui, "+B", false, theme::VIOLET)
                    .on_hover_text("Start receiver B beside A (or middle-click the display)")
                    .clicked()
            {
                start_b(app);
            }
        });
        if has_b {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                if widgets::chip_key(ui, "SWAP", false, theme::VIOLET).on_hover_text("Trade frequencies between A and B").clicked()
                    && let Some(b) = app.status.rx2.clone()
                {
                    let a = app.inspect_hz();
                    app.active_b = false;
                    app.tune(b.freq_hz, false);
                    app.set_rx2(a, b.mode);
                }
                if widgets::chip_key(ui, "B OFF", false, theme::VIOLET).clicked() {
                    app.rx2_off();
                }
            });
        }
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
    let b = app.status.rx2.clone().filter(|_| app.active_b);
    let hz = app.active_hz();
    let mut acts = Vec::new();
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
        app.vfo.ui(ui, hz, 40.0, &mut acts);
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

fn tuning(app: &mut App, ui: &mut Ui, strip: bool) {
    let hz = app.active_hz();
    bay(ui, "STEP", |ui| {
        let auto = crate::tuning::mode_raster_hz(app.status.mode, hz);
        let label = match app.prefs.step_hz {
            None => format!("AUTO {}", crate::tuning::fmt_short_hz(auto)),
            Some(s) => crate::tuning::fmt_short_hz(s),
        };
        let color = if app.prefs.step_hz.is_some() { theme::WARN } else { theme::text() };
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("step")
                .selected_text(RichText::new(label).font(theme::mono(11.0)).color(color))
                .width(104.0)
                .show_ui(ui, |ui| {
                    if ui.selectable_label(app.prefs.step_hz.is_none(), format!("Auto · mode raster ({})", crate::tuning::fmt_short_hz(auto))).clicked() {
                        app.prefs.step_hz = None;
                    }
                    for s in crate::tuning::STEPS_HZ {
                        if ui.selectable_label(app.prefs.step_hz == Some(s), crate::tuning::fmt_hz(s)).clicked() {
                            app.prefs.step_hz = Some(s);
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
        // The jog keys either side of the strip: « ‹ [strip] › ».
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
            if strip && let Some(d) = crate::vfo::tune_strip(ui, hz, app.step_hz(), 150.0, 24.0) {
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
        Mode::Wfm | Mode::Pager | Mode::Flex => theme::CYAN,
        Mode::P25 | Mode::Dmr | Mode::Nxdn | Mode::Packet => theme::OK,
        _ => theme::VIOLET,
    }
}

fn modes(app: &mut App, ui: &mut Ui) {
    let b = app.status.rx2.clone().filter(|_| app.active_b);
    bay(ui, "MODE", |ui| match b {
        Some(b) => {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                for m in B_MODES {
                    if widgets::chip_key(ui, m.label(), b.mode == m, theme::VIOLET).clicked() && b.mode != m {
                        app.set_rx2(b.freq_hz, m);
                    }
                }
            });
            ui.label(small("receiver B listens; the decoders run on A"));
        }
        None => {
            for row in [&Mode::ALL[..7], &Mode::ALL[7..]] {
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
    });
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
            ui.label(small(if app.server.as_ref().is_some_and(|i| i.has("filter")) {
                format!("{} keeps its decoder's filter", mode.label())
            } else {
                "this server cannot set the filter".into()
            }));
            return;
        };
        let presets: &[f32] = match mode {
            Mode::Usb | Mode::Lsb => &[1_800.0, 2_100.0, 2_400.0, 2_700.0, 3_000.0, 3_600.0],
            Mode::Am => &[4_000.0, 6_000.0, 8_000.0, 10_000.0, 12_000.0, 16_000.0],
            Mode::Nfm => &[6_000.0, 8_000.0, 11_000.0, 12_500.0, 16_000.0, 25_000.0],
            Mode::Wfm => &[100_000.0, 150_000.0, 180_000.0, 200_000.0, 230_000.0],
            _ => &[],
        };
        let around = |w: f32| -> (f32, f32) {
            match mode {
                Mode::Usb => (lo, (lo + w).min(range.max_edge_hz)),
                Mode::Lsb => ((hi - w).max(-range.max_edge_hz), hi),
                _ => (-w / 2.0, w / 2.0),
            }
        };
        let mut set: Option<(f32, f32)> = None;
        // As many presets as the section has room for, narrowest first.
        let fit = (((width_px - if custom { 58.0 } else { 0.0 }) / 47.0).floor() as usize).max(2);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            for p in presets.iter().take(fit) {
                let label = format!("{}k", format!("{:.1}", p / 1000.0).trim_end_matches(".0"));
                if widgets::chip_key(ui, &label, (width - p).abs() < 1.0, theme::AMBER).clicked() {
                    set = Some(around(*p));
                }
            }
            if custom && widgets::chip_key(ui, "RESET", false, theme::OK).on_hover_text("Back to the mode's own filter").clicked() {
                if b.is_some() {
                    app.post("/api/sdr/filter", json!({ "reset": true, "mode": mode.wire() }));
                } else {
                    app.reset_filter();
                }
            }
        });
        let mut wv = width as f64;
        let (ch, _) = Fader {
            value: &mut wv,
            min: range.min_width_hz as f64,
            max: range.max_width_hz as f64,
            step: if range.max_width_hz > 50_000.0 { 5_000.0 } else { 100.0 },
            fine: if range.max_width_hz > 50_000.0 { 1_000.0 } else { 50.0 },
            label: "WIDTH",
            text: |v| format!("{:.2}k", v / 1000.0),
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

fn audio(app: &mut App, ui: &mut Ui) {
    let is_b = app.active_b && app.status.rx2.is_some();
    bay(ui, if is_b { "AUDIO B" } else { "AUDIO A" }, |ui| {
        ui.set_min_width(200.0);
        ui.set_max_width(220.0);
        let (vol, muted) = if is_b { (app.prefs.volume_b, app.prefs.muted_b) } else { (app.prefs.volume, app.prefs.muted) };
        let mut v = vol as f64 * 100.0;
        let (ch, _) = Fader {
            value: &mut v,
            min: 0.0,
            max: 100.0,
            step: 2.0,
            fine: 1.0,
            label: "AF GAIN",
            text: |v| if muted { "muted".into() } else { format!("{v:.0}") },
            color: if is_b { theme::VIOLET } else { theme::AMBER },
            enabled: !muted,
            ticks: vec![],
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
            let lw = (ui.available_width() - 30.0).clamp(30.0, 120.0);
            let (rect, _) = ui.allocate_exact_size(vec2(lw, 4.0), Sense::hover());
            ui.painter().rect_filled(rect, egui::CornerRadius::same(1), theme::ink());
            let lvl = |v: f32| ((20.0 * v.max(1e-5).log10() + 60.0) / 60.0).clamp(0.0, 1.0);
            ui.painter().rect_filled(egui::Rect::from_min_size(rect.min, vec2(rect.width() * lvl(r.rms), rect.height())), egui::CornerRadius::same(1), alpha(theme::OK, 0.8));
            let px = rect.left() + rect.width() * lvl(r.peak);
            ui.painter().line_segment([egui::pos2(px, rect.top()), egui::pos2(px, rect.bottom())], Stroke::new(1.5, theme::text()));
            ui.menu_button(RichText::new("···").font(theme::mono_bold(11.0)), |ui| {
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
                }
                let r = player.reading();
                ui.label(small(if r.queue_ms > 0.0 {
                    format!("{} Hz in · queue {:.0} ms · clock {:+.0} ppm", r.in_rate.round(), r.queue_ms, r.drift_ppm)
                } else {
                    "no audio arriving".into()
                }));
                if let Some(p) = app.player.clip_progress()
                    && ui.button(format!("Stop playback ({:.0}%) and return to live", p * 100.0)).clicked()
                {
                    app.player.stop_clip();
                }
            });
        });
    });
}

fn meter(app: &mut App, ui: &mut Ui, min_w: f32, max_w: f32) {
    let is_b = app.active_b && app.status.rx2.is_some();
    let rm = app.reduce_motion();
    let mark = app.prefs.view.sql_mark;
    // The last bay takes whatever width the row has left.
    let w = (ui.available_width() - 20.0).clamp(min_w, max_w);
    bay(ui, if is_b { "SIGNAL B" } else { "SIGNAL" }, |ui| {
        ui.set_min_width(w);
        ui.set_max_width(w);
        if is_b {
            app.meter_b.ui_compact(ui, rm, mark);
            let snr = app.meter_b.snr.map_or("—".into(), |s| format!("{s:.1} dB"));
            ui.label(small(format!("SNR {snr} · measured from the spectrum")));
            return;
        }
        app.meter.ui_compact(ui, rm, mark);
        let s = &app.status;
        let snr = app.meter.snr.map_or("—".into(), |d| format!("{d:.1} dB"));
        let err = s.freq_error_hz.map_or("—".into(), |e| if e.abs() >= 10_000.0 { format!("{:+.1}k", e / 1000.0) } else { format!("{e:+.0} Hz") });
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
            ui.label(small(format!("SNR {snr}")));
            let ec = if s.freq_error_hz.is_some_and(|e| e.abs() > 2000.0) { theme::WARN } else { theme::text_3() };
            let r = ui.add(egui::Label::new(RichText::new(format!("err {err}")).font(theme::mono(10.0)).color(ec)).sense(Sense::click()));
            if r.on_hover_text("Click to calibrate the ppm from this carrier").clicked()
                && let Some(l) = &app.link
            {
                l.post_get("/api/sdr/calibrate", json!({}), Get::Calibrate);
            }
            if !notches.is_empty() {
                ui.label(RichText::new(format!("notch {}", notches.join(" "))).font(theme::mono(10.0)).color(theme::VIOLET));
            }
            let (c, word) = app.shown_class();
            let tone = match word {
                "ACTIVE" => Tone::Ok,
                "HELD" => Tone::Warn,
                _ => Tone::Idle,
            };
            widgets::chip(ui, tone, word);
            let (txt, col) = match &c {
                Some(c) if word != "IDLE" => (format!("{} · {}", c.protocol, c.modulation), widgets::protocol_color(&c.protocol)),
                _ => (crate::app::mode_idle_text(app.status.mode).to_string(), theme::text_3()),
            };
            ui.add(egui::Label::new(RichText::new(txt).font(theme::sans(12.0)).color(col)).truncate());
        });
    });
}

/// One-click memories under the bar, like a radio's band buttons. Storing
/// and renaming live in the Tools tab.
pub fn memory_row(app: &mut App, ui: &mut Ui) {
    ui.horizontal(|ui| memory_chips(app, ui));
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
            let r = widgets::chip_key(ui, m.map_or("—", |m| m.name.as_str()), lit, theme::AMBER);
            let r = match m {
                Some(m) => r.on_hover_text(format!("{} MHz · {} · Ctrl+{}", crate::tuning::fmt_mhz(m.freq_hz, 5), m.mode.label(), i + 1)),
                None => r.on_hover_text("Empty: STORE, then this slot"),
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
