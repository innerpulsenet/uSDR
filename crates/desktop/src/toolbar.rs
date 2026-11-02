//! The display toolbar: what the panadapter and waterfall show, set where
//! they are shown. The frequent switches sit inline; the rest open as
//! popovers from the bar.

use crate::app::App;
use crate::spectrum::Style;
use crate::theme;
use crate::widgets::{self, Fader};
use egui::{RichText, Ui, vec2};
use serde_json::json;

fn small(t: impl Into<String>) -> RichText {
    RichText::new(t).font(theme::mono(10.5)).color(theme::text_3())
}

fn divider(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(vec2(1.0, 18.0), egui::Sense::hover());
    ui.painter().rect_filled(r, egui::CornerRadius::ZERO, theme::line_2());
}

pub fn toolbar(app: &mut App, ui: &mut Ui) {
    // Under about 900 px the range, style and waterfall menus fold into
    // one, so the bar never runs wider than the display it sits over.
    let narrow = ui.available_width() < 900.0;
    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        // Span and zoom.
        let s = app.status.clone();
        let span = (s.max_freq_hz - s.min_freq_hz).max(s.rate_hz);
        widgets::menu_chip(ui, &format!("SPAN {}", crate::tuning::fmt_short_hz(span)), theme::CYAN, |ui| {
            ui.set_min_width(300.0);
            span_zoom(app, ui);
        });
        if widgets::chip_key(ui, "−", false, theme::CYAN).on_hover_text("Zoom out (−)").clicked() {
            app.set_zoom(app.zoom_ui / 2.0);
        }
        ui.label(RichText::new(format!("{:.1}×", app.zoom_ui.max(1.0))).font(theme::mono(10.5)).color(theme::text_2()));
        if widgets::chip_key(ui, "+", false, theme::CYAN).on_hover_text("Zoom in (+)").clicked() {
            app.set_zoom(app.zoom_ui * 2.0);
        }
        if widgets::chip_key(ui, "CENTER", false, theme::CYAN).on_hover_text("Put the span back around the receiver (C)").clicked() {
            let hz = app.inspect_hz();
            app.center_span(hz);
        }
        divider(ui);
        // Averaging, on the server, so every client sees the same.
        ui.label(small("AVG"));
        let avg = match s.avg.as_deref() {
            Some("slow") => 1,
            Some("deep") => 2,
            _ => 0,
        };
        for (i, (label, mode)) in [("OFF", "off"), ("0.5 s", "slow"), ("2 s", "deep")].into_iter().enumerate() {
            if widgets::chip_key(ui, label, avg == i, theme::CYAN).clicked() && avg != i {
                app.post("/api/sdr/avg", json!({ "mode": mode }));
                app.status.avg = Some(mode.into());
            }
        }
        divider(ui);
        let v = &mut app.prefs.view;
        if widgets::chip_key(ui, "PEAK", v.peak_hold, theme::CYAN).clicked() {
            v.peak_hold = !v.peak_hold;
        }
        if widgets::chip_key(ui, "FILL", v.fill, theme::AMBER).clicked() {
            v.fill = !v.fill;
        }
        if widgets::chip_key(ui, "SPOTS", v.show_peaks, theme::AMBER).on_hover_text("Peak markers and decode labels").clicked() {
            v.show_peaks = !v.show_peaks;
        }
        if widgets::chip_key(ui, "BANDS", v.band_plan, theme::OK).on_hover_text("US allocations along the axis; hover for the name").clicked() {
            v.band_plan = !v.band_plan;
        }
        if narrow {
            widgets::menu_chip(ui, "DISPLAY", theme::AMBER, |ui| {
                ui.set_min_width(280.0);
                ui.label(small("RANGE"));
                range(app, ui);
                ui.separator();
                ui.label(small("STYLE"));
                style(app, ui);
                ui.separator();
                ui.label(small("WATERFALL"));
                waterfall(app, ui);
            });
        } else {
            widgets::menu_chip(ui, if v.auto_range { "RANGE AUTO" } else { "RANGE" }, theme::AMBER, |ui| {
                ui.set_min_width(260.0);
                range(app, ui);
            });
            divider(ui);
            let label = format!("STYLE {}", app.prefs.view.style.label().to_uppercase());
            widgets::menu_chip(ui, &label, theme::AMBER, |ui| style(app, ui));
            widgets::menu_chip(ui, "WATERFALL", theme::AMBER, |ui| {
                ui.set_min_width(280.0);
                waterfall(app, ui);
            });
        }
        // HOLD lives on the waterfall itself, beside the history it holds.
        // Wide windows carry the memories here rather than on a row of their own.
        if app.prefs.show_memories && app.memories_on_toolbar {
            ui.add_space(8.0);
            divider(ui);
            crate::bar::memory_chips(app, ui);
        }
    });
}

fn style(app: &mut App, ui: &mut Ui) {
    let v = &mut app.prefs.view;
    for st in Style::ALL {
        if ui.selectable_label(v.style == st, st.label()).clicked() {
            v.style = st;
            v.palette = st.look().palette;
        }
    }
}

fn span_zoom(app: &mut App, ui: &mut Ui) {
    let s = app.status.clone();
    let rates: Vec<f64> = if s.rates_hz.is_empty() { vec![250e3, 1.024e6, 1.536e6, 2.048e6, 2.4e6, 3.2e6] } else { s.rates_hz.clone() };
    let cur = rates.iter().enumerate().min_by(|a, b| (a.1 - s.rate_hz).abs().total_cmp(&(b.1 - s.rate_hz).abs())).map(|x| x.0).unwrap_or(0);
    ui.label(small("SPAN — the sample rate; each change rebuilds the server's chains"));
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(3.0, 3.0);
        for (i, r) in rates.iter().enumerate() {
            let mut label = crate::tuning::fmt_short_hz(*r);
            if s.tuner.to_ascii_uppercase().contains("RSP1A") && *r > 6.048e6 {
                let bits = if *r <= 8.064e6 { 12 } else if *r <= 9.216e6 { 10 } else { 8 };
                label = format!("{label} · {bits}b");
            }
            if widgets::chip_key(ui, &label, i == cur, theme::CYAN).clicked() && i != cur {
                app.post("/api/sdr/rate", json!({ "rate_hz": r }));
            }
        }
    });
    let mut z = app.zoom_ui.max(1.0).log2();
    let ticks = (0..=5).map(|i| (i as f32 / 5.0, format!("{}×", 1 << i))).collect();
    let (ch, _) = Fader { value: &mut z, min: 0.0, max: 5.0, step: 0.25, fine: 0.05, label: "ZOOM", text: |z| format!("{:.1}×", 2f64.powf(z)), color: theme::CYAN, enabled: true, ticks }.show(ui);
    if ch {
        app.set_zoom(2f64.powf(z));
    }
}

fn range(app: &mut App, ui: &mut Ui) {
    let v = &mut app.prefs.view;
    if widgets::key(ui, "AUTO RANGE", v.auto_range, theme::OK).clicked() {
        v.auto_range = !v.auto_range;
        if !v.auto_range {
            let (lo, hi) = app.pan.range();
            v.floor_db = lo;
            v.ceiling_db = hi;
        }
    }
    let mut fl = v.floor_db as f64;
    let mut ce = v.ceiling_db as f64;
    let (a, _) = Fader { value: &mut fl, min: -150.0, max: -20.0, step: 2.0, fine: 1.0, label: "FLOOR", text: |v| format!("{v:.0} dBFS"), color: theme::text_2(), enabled: !v.auto_range, ticks: vec![] }.show(ui);
    let (b, _) = Fader { value: &mut ce, min: -100.0, max: 10.0, step: 2.0, fine: 1.0, label: "CEILING", text: |v| format!("{v:.0} dBFS"), color: theme::text_2(), enabled: !v.auto_range, ticks: vec![] }.show(ui);
    if a || b {
        v.auto_range = false;
        v.floor_db = fl as f32;
        v.ceiling_db = (ce as f32).max(v.floor_db + 10.0);
    }
    ui.label(small("or wheel and drag the dB scale"));
}

fn waterfall(app: &mut App, ui: &mut Ui) {
    let floor = app.pan.floor_est;
    let v = &mut app.prefs.view;
    ui.horizontal(|ui| {
        ui.label(small("palette"));
        widgets::selector(ui, "palette", v.palette.label(), theme::text(), 130.0, |ui| {
            for m in crate::colormap::Colormap::ALL {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(60.0, 12.0), egui::Sense::hover());
                    for i in 0..30 {
                        let c = m.sample(i as f32 / 29.0);
                        ui.painter().rect_filled(egui::Rect::from_min_size(egui::pos2(r.left() + i as f32 * 2.0, r.top()), vec2(2.0, 12.0)), egui::CornerRadius::ZERO, c);
                    }
                    if ui.selectable_label(v.palette == m, m.label()).clicked() {
                        v.palette = m;
                    }
                });
            }
        });
    });
    ui.horizontal(|ui| {
        if widgets::key(ui, "AUTO BLACK", v.wf_auto_black, theme::OK).on_hover_text("Keep the black level just under the noise floor").clicked() {
            v.wf_auto_black = !v.wf_auto_black;
            v.wf_black = if v.wf_auto_black { -3.0 } else { floor };
        }
        if widgets::key(ui, "WF PEAK", v.wf_peak, theme::VIOLET).on_hover_text("Paint from each frame's strongest segment, so bursts show at full level").clicked() {
            v.wf_peak = !v.wf_peak;
        }
    });
    let mut black = v.wf_black as f64;
    let (lo, hi, label) = if v.wf_auto_black { (-20.0, 20.0, "BLACK (from the floor)") } else { (-150.0, -20.0, "BLACK LEVEL") };
    let (ch, _) = Fader { value: &mut black, min: lo, max: hi, step: 1.0, fine: 0.5, label, text: |v| format!("{v:+.0} dB"), color: theme::text_2(), enabled: true, ticks: vec![] }.show(ui);
    if ch {
        v.wf_black = black as f32;
    }
    let mut range = v.wf_range as f64;
    let (ch, _) = Fader { value: &mut range, min: 10.0, max: 100.0, step: 1.0, fine: 0.5, label: "CONTRAST RANGE", text: |v| format!("{v:.0} dB"), color: theme::text_2(), enabled: true, ticks: vec![] }.show(ui);
    if ch {
        v.wf_range = range as f32;
    }
}
