//! The applet column: gain, front end, the recorder, the P25 key in P25,
//! the radio and the memories — one column, each a collapsible card, so the
//! recorder is in view while its 20 seconds are still worth saving.

use crate::app::App;
use crate::net::{Fetch, Get};
use crate::theme::{self, alpha};
use crate::widgets::{self, Fader, Tone};
use egui::{Align2, RichText, Ui, vec2};
use serde_json::{Value, json};
use usdr_proto::Mode;

fn small(t: impl Into<String>) -> RichText {
    RichText::new(t).font(theme::mono(crate::theme::ty::LABEL)).color(theme::text_3())
}

pub fn side(app: &mut App, ui: &mut Ui) {
    ui.spacing_mut().item_spacing.y = 8.0;
    let out = egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        // On broadcast FM the station is what the column is for; it leads.
        if app.status.mode == Mode::Wfm {
            let head = |ui: &mut Ui| {
                let (t, c) = match app.rds() {
                    Some(r) if r.locked => ("LOCKED", widgets::protocol_color("RDS")),
                    Some(_) => ("LOST", theme::text_3()),
                    None => ("SEARCHING", theme::text_3()),
                };
                ui.label(RichText::new(t).font(theme::mono(crate::theme::ty::LABEL)).color(c));
            };
            widgets::applet(ui, "rds", "Station · RDS", head, |ui| station(app, ui));
        }
        widgets::applet(ui, "gain", "Gain", |_| {}, |ui| gain(app, ui));
        let on = front_end_on(&app.status);
        let head = |ui: &mut Ui| {
            let (t, c) = if on == 0 { ("ALL OFF".to_string(), theme::text_3()) } else { (format!("{on} ON"), theme::AMBER) };
            ui.label(RichText::new(t).font(theme::mono(crate::theme::ty::LABEL)).color(c));
        };
        widgets::applet(ui, "frontend", "Front end", head, |ui| front_end(app, ui));
        if app.status.processing.is_some() {
            widgets::applet(ui, "processing", "Noise and AGC", |_| {}, |ui| processing(app, ui));
        }
        widgets::applet(ui, "recorder", "Recorder · last 20 s", |_| {}, |ui| recorder(app, ui));
        if app.status.mode == Mode::P25 {
            widgets::applet(ui, "p25", "P25 channel key", |_| {}, |ui| p25(app, ui));
        }

        widgets::applet(ui, "radio", "Radio", |_| {}, |ui| radio(app, ui));
        // Recall is the memory row's; the bank's storing, renaming and
        // clearing open here when wanted.
        widgets::applet_folded(ui, "memory-bank", "Memory bank", |_| {}, |ui| memories(app, ui));
    });
    // More cards than the column shows: its edge fades into the panel
    // where they continue, so a card cut in half reads as "scroll on".
    let (shown, whole, off) = (out.inner_rect, out.content_size.y, out.state.offset.y);
    let fade = |top: bool| {
        let h = 32.0;
        let edge = if top { egui::Rect::from_min_size(shown.min, vec2(shown.width(), h)) } else { egui::Rect::from_min_max(egui::pos2(shown.left(), shown.bottom() - h), shown.max) };
        let (clear, solid) = (alpha(theme::plate(), 0.0), theme::plate());
        let (a, b) = if top { (solid, clear) } else { (clear, solid) };
        let mut mesh = egui::Mesh::default();
        mesh.colored_vertex(edge.left_top(), a);
        mesh.colored_vertex(edge.right_top(), a);
        mesh.colored_vertex(edge.right_bottom(), b);
        mesh.colored_vertex(edge.left_bottom(), b);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        ui.painter().add(egui::Shape::mesh(mesh));
    };
    if off > 1.0 {
        fade(true);
    }
    if whole - off > shown.height() + 1.0 {
        fade(false);
    }
}

/// What a broadcast station is sending over RDS, in rows of fixed height:
/// the card is the same size from "searching" to a full RadioText, so the
/// cards under it never move while it fills in. Everything shown passed
/// its checkwords on the server; a row with nothing confirmed for it says
/// so in its own place.
/// The station's RDS on its own display, as a car radio shows it.
fn station(app: &App, ui: &mut Ui) {
    widgets::glass(ui, egui::Id::new("rds-glass"), widgets::protocol_color("RDS"), |ui| station_body(app, ui));
}

fn station_body(app: &App, ui: &mut Ui) {
    let r = app.rds();
    let w = ui.available_width();
    let dim = r.is_none_or(|r| !r.locked);
    let fade = |c: egui::Color32| if dim { theme::text_3() } else { c };
    let rds_c = widgets::protocol_color("RDS");
    ui.spacing_mut().item_spacing.y = 2.0;
    let row = |ui: &mut Ui, h: f32| ui.allocate_exact_size(vec2(w, h), egui::Sense::hover());
    let fit = |ui: &Ui, t: &str, font: egui::FontId, room: f32| widgets::ellipsize(ui.painter(), t, font, room.max(0.0));

    // Who: the name, and the program type as a tag at the right.
    let (rect, resp) = row(ui, 26.0);
    let p = ui.painter().clone();
    let mut room = w;
    if let Some(pty) = r.and_then(|r| r.pty_name.clone().filter(|_| r.pty != Some(0))) {
        let g = p.layout_no_wrap(pty.to_uppercase(), theme::mono(crate::theme::ty::MICRO), fade(rds_c));
        let tag = egui::Rect::from_min_size(egui::pos2(rect.right() - g.size().x - 12.0, rect.center().y - 9.0), g.size() + vec2(12.0, 0.0)).with_max_y(rect.center().y + 9.0);
        p.rect(tag, egui::CornerRadius::same(9), alpha(fade(rds_c), 0.10), egui::Stroke::new(1.0, alpha(fade(rds_c), 0.5)), egui::StrokeKind::Inside);
        p.galley(tag.center() - g.size() / 2.0, g, fade(rds_c));
        room -= tag.width() + 10.0;
    }
    let (name, known) = match r {
        Some(r) => (crate::bar::rds_station(r), (r.ps_steady && !r.ps_dynamic) || r.pi_call.is_some()),
        None => ("Searching…".to_string(), false),
    };
    let font = theme::mono_bold(18.0);
    p.text(egui::pos2(rect.left(), rect.center().y), Align2::LEFT_CENTER, fit(ui, &name, font.clone(), room), font, if known { fade(theme::text()) } else { theme::text_3() });
    if r.is_some() {
        resp.on_hover_text("The station's PS once it has held steady, else the call letters its PI code maps to by the RBDS rules");
    }

    // The station's code and flags at the left; the PS at the right while
    // it is not the name above (a scrolling station's word of the moment).
    let (rect, resp) = row(ui, 16.0);
    let p = ui.painter().clone();
    let id = match r {
        Some(r) => {
            let mut id = format!("PI {}", r.pi);
            if let Some(c) = &r.pi_call {
                id.push_str(&format!(" · {c}"));
            }
            if r.tp {
                id.push_str(" · TP");
            }
            if r.ta {
                id.push_str(" · TA");
            }
            id
        }
        None => "no station code yet".into(),
    };
    let ps_word = r.and_then(|r| r.ps.clone().filter(|_| r.ps_dynamic || !r.ps_steady).map(|ps| (ps, r.ps_dynamic)));
    let right_w = match &ps_word {
        Some((ps, _)) => p.layout_no_wrap(format!("PS {ps}"), theme::mono(crate::theme::ty::LABEL), theme::text_2()).size().x + 8.0,
        None => 0.0,
    };
    p.text(egui::pos2(rect.left(), rect.center().y), Align2::LEFT_CENTER, fit(ui, &id, theme::mono(crate::theme::ty::LABEL), w - right_w - 8.0), theme::mono(crate::theme::ty::LABEL), theme::text_3());
    if let Some((ps, scrolling)) = &ps_word {
        p.text(egui::pos2(rect.right(), rect.center().y), Align2::RIGHT_CENTER, format!("PS {ps}"), theme::mono(crate::theme::ty::LABEL), fade(if *scrolling { theme::text_2() } else { theme::text_3() }));
    }
    resp.on_hover_text(match &ps_word {
        Some((_, true)) => "Call letters come from the PI code by the RBDS mapping. The PS at the right scrolls a message a word at a time.",
        Some((_, false)) => "Call letters come from the PI code by the RBDS mapping. The PS at the right is taken for the name once it has held for ten seconds.",
        None => "Call letters come from the PI code by the RBDS mapping; a station that changed calls can keep its old PI.",
    });

    // What is on: two lines, always. Tagged title and artist each on their
    // own; else the RadioText across both, a text still arriving with its
    // gaps; and a hairline under it that fills as a new text comes in.
    ui.add_space(4.0);
    let (rect, _) = row(ui, 38.0);
    let p = ui.painter().clone();
    let (line1, line2) = (egui::Rect::from_min_size(rect.min, vec2(w, 20.0)), egui::Rect::from_min_size(rect.min + vec2(0.0, 20.0), vec2(w, 18.0)));
    let partial = r.and_then(|r| r.rt_partial.clone().filter(|p| !p.is_empty()).map(|p| (p, r.rt_progress)));
    match (r, &partial) {
        (Some(r), None) if r.title.is_some() || r.artist.is_some() => {
            let t = r.title.clone().unwrap_or_default();
            let a = r.artist.clone().unwrap_or_default();
            p.text(line1.left_center(), Align2::LEFT_CENTER, fit(ui, &t, theme::sans_semibold(14.0), w), theme::sans_semibold(14.0), fade(theme::text()));
            p.text(line2.left_center(), Align2::LEFT_CENTER, fit(ui, &a, theme::sans(crate::theme::ty::LEDE), w), theme::sans(crate::theme::ty::LEDE), fade(theme::text_2()));
        }
        (Some(r), _) if partial.is_some() || r.rt.is_some() => {
            let text = partial.as_ref().map(|p| p.0.clone()).or_else(|| r.rt.clone()).unwrap_or_default();
            let mut job = crate::bar::gapped(&text, theme::sans(crate::theme::ty::LEDE), fade(theme::text()));
            job.wrap = egui::text::TextWrapping { max_width: w, max_rows: 2, break_anywhere: false, overflow_character: Some('…') };
            let g = p.layout_job(job);
            let y = rect.top() + (rect.height() - g.size().y).max(0.0) / 2.0;
            p.galley(egui::pos2(rect.left(), y), g, theme::text());
        }
        _ => {
            p.text(line1.left_center(), Align2::LEFT_CENTER, "no RadioText yet", theme::sans(crate::theme::ty::LEDE), theme::text_3());
        }
    }
    let (bar, _) = row(ui, 3.0);
    if let Some((_, progress)) = &partial {
        let p = ui.painter();
        p.rect_filled(bar, egui::CornerRadius::same(crate::theme::R_CELL), alpha(rds_c, 0.15));
        p.rect_filled(egui::Rect::from_min_size(bar.min, vec2(bar.width() * progress.clamp(0.0, 1.0), 3.0)), egui::CornerRadius::same(crate::theme::R_CELL), alpha(rds_c, 0.8));
    }

    // How it is being received: a meter of the blocks read (out of every 50),
    // the carrier's source, and the station's clock.
    ui.add_space(4.0);
    let (rect, resp) = row(ui, 16.0);
    let p = ui.painter().clone();
    let segs = 10;
    let meter = egui::Rect::from_min_size(egui::pos2(rect.left(), rect.center().y - 4.0), vec2(48.0, 8.0));
    let good = r.map_or(0.0, |r| if r.locked { 1.0 - r.bler } else { 0.0 });
    let c = if good >= 0.9 { theme::OK } else if good >= 0.7 { theme::WARN } else { theme::ERR };
    for k in 0..segs {
        let x = meter.left() + k as f32 * (meter.width() / segs as f32);
        let seg = egui::Rect::from_min_size(egui::pos2(x, meter.top()), vec2(meter.width() / segs as f32 - 1.5, meter.height()));
        let lit = (k as f32 + 0.5) / segs as f32 <= good;
        p.rect_filled(seg, egui::CornerRadius::same(crate::theme::R_CELL), if lit { c } else { alpha(theme::text_3(), 0.25) });
    }
    let status = match r {
        Some(r) if r.locked => {
            let mut s = format!("{:.0}% read · {}", good * 100.0, if r.pilot { "pilot" } else { "no pilot" });
            if let Some(clk) = crate::bar::rds_clock(r) {
                s.push_str(&format!(" · clock {}", clk.split(' ').next().unwrap_or("")));
            }
            s
        }
        Some(_) => "lock lost · last heard shown".into(),
        None => "listening for RDS".into(),
    };
    let x0 = meter.right() + 8.0;
    p.text(egui::pos2(x0, rect.center().y), Align2::LEFT_CENTER, fit(ui, &status, theme::mono(crate::theme::ty::MICRO), rect.right() - x0), theme::mono(crate::theme::ty::MICRO), theme::text_3());
    if let Some(r) = r {
        let at = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(r.updated_ms);
        let mut tip = format!(
            "{:.0}% of the last 50 blocks read · carrier {}",
            good * 100.0,
            if r.pilot { "from the 19 kHz pilot" } else { "free-running (no pilot)" }
        );
        if let Some(clk) = crate::bar::rds_clock(r) {
            tip.push_str(&format!("\nstation clock {clk}, as the station sends it (group 4A)"));
        }
        if r.updated_ms > 0 {
            tip.push_str(&format!("\nlast changed {} UTC", crate::app::local_hms(at)));
        }
        resp.on_hover_text(tip);
    }
}

/// A row `h` high the whole width, laid out left to right, whatever `add`
/// puts in it (or nothing), so a message that comes and goes moves nothing.
fn fixed_line(ui: &mut Ui, h: f32, add: impl FnOnce(&mut Ui)) {
    let w = ui.available_width();
    ui.allocate_ui_with_layout(vec2(w, h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_size(vec2(w, h));
        ui.set_max_height(h);
        add(ui);
    });
}

/// The channel's level over its noise floor across the 20 s the server's
/// buffer holds, so a burst worth saving can be seen before it is saved.
/// Without a buffer (`live` false) the same strip says so, at the same size.
fn activity(app: &App, ui: &mut Ui, live: bool) {
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, 30.0), egui::Sense::hover());
    let p = ui.painter();
    // On the recorder's glass: a faint floor line, no well of its own.
    p.line_segment([egui::pos2(r.left(), r.bottom() - 1.5), egui::pos2(r.right(), r.bottom() - 1.5)], egui::Stroke::new(1.0, alpha(theme::text_3(), 0.25)));
    let now = ui.input(|i| i.time);
    let x_at = |t: f64| r.right() - ((now - t) / 20.0).clamp(0.0, 1.0) as f32 * r.width();
    // 0–40 dB over the floor, bottom to top.
    let y_at = |snr: f32| r.bottom() - 2.0 - (snr / 40.0).clamp(0.0, 1.0) * (r.height() - 4.0);
    let pts: Vec<egui::Pos2> = if live { app.activity.iter().map(|&(t, ch, nz)| egui::pos2(x_at(t), y_at(ch - nz))).collect() } else { Vec::new() };
    if pts.len() > 1 {
        crate::spectrum::fill_under(p, &pts, r.bottom(), theme::AMBER, 0.5);
        p.add(egui::Shape::line(pts, egui::Stroke::new(1.0, theme::AMBER)));
    } else {
        let note = if live { "nothing measured on this channel yet".to_string() } else { format!("{} keeps no buffer", app.status.mode.label()) };
        p.text(r.center(), egui::Align2::CENTER_CENTER, note, theme::mono(crate::theme::ty::LABEL), theme::text_3());
    }
    ui.horizontal(|ui| {
        ui.label(small("−20 s"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(small("now"));
            ui.add_space(ui.available_width() / 2.0 - 60.0);
            ui.label(small("level over the floor"));
        });
    });
}

/// The main display's trace across the passband, for the scope.
pub fn passband_slice(app: &App) -> Option<(Vec<f32>, (f32, f32))> {
    let f = app.frame.as_ref()?;
    let n = f.pwr.len();
    if n < 4 {
        return None;
    }
    let bw = if app.status.bandwidth_hz > 0.0 { app.status.bandwidth_hz } else { 12_500.0 };
    let lo = f.center_hz - f.rate_hz / 2.0;
    let a = (((f.inspect_hz - bw / 2.0 - lo) / f.rate_hz) * (n - 1) as f64).round().clamp(0.0, (n - 1) as f64) as usize;
    let b = (((f.inspect_hz + bw / 2.0 - lo) / f.rate_hz) * (n - 1) as f64).round().clamp(0.0, (n - 1) as f64) as usize;
    if b <= a + 2 {
        return None;
    }
    Some((f.pwr[a..=b].to_vec(), app.pan.range()))
}

fn memories(app: &mut App, ui: &mut Ui) {
    let armed = app.memory_armed;
    if widgets::key(ui, "STORE", armed, theme::AMBER).on_hover_text("Then pick a slot; Shift-click a slot also stores").clicked() {
        app.memory_armed = !armed;
    }
    let list = app.memories();
    let inspect = app.inspect_hz();
    let mut store_at: Option<usize> = None;
    let mut delete: Option<usize> = None;
    let w = (ui.available_width() - 3.0 * 4.0) / 4.0;
    egui::Grid::new("mem").spacing(vec2(4.0, 4.0)).show(ui, |ui| {
        let slots = list.len().max(8).div_ceil(4) * 4;
        for i in 0..slots {
            let m = list.get(i);
            let label = m.map_or("—".to_string(), |m| m.name.clone());
            let lit = m.is_some_and(|m| {
                m.mode == app.status.mode && (m.freq_hz - inspect).abs() <= (crate::tuning::mode_raster_hz(m.mode, m.freq_hz) / 2.0).max(1000.0)
            });
            let r = widgets::key_sized(ui, &label, lit, theme::AMBER, vec2(w, 26.0));
            let r = match m {
                Some(m) => r.on_hover_text(format!("{} MHz · {} · Ctrl+{}", crate::tuning::fmt_mhz(m.freq_hz, 5), m.mode.label(), i + 1)),
                None => r.on_hover_text("Empty: STORE, then this slot"),
            };
            if r.clicked() {
                if app.memory_armed || ui.input(|i| i.modifiers.shift) {
                    store_at = Some(i);
                } else if let Some(m) = m {
                    let (hz, mode) = (m.freq_hz, m.mode);
                    app.tune(hz, true);
                    if mode != app.status.mode {
                        app.set_mode(mode);
                    }
                }
            }
            r.context_menu(|ui| {
                if crate::widgets::menu_item(ui, "Store the current channel here").clicked() {
                    store_at = Some(i);
                    ui.close();
                }
                if m.is_some() && crate::widgets::menu_item(ui, "Rename…").clicked() {
                    app.renaming = Some((i, m.map(|m| m.name.clone()).unwrap_or_default()));
                    ui.close();
                }
                if m.is_some() && crate::widgets::menu_item(ui, "Clear").clicked() {
                    delete = Some(i);
                    ui.close();
                }
            });
            if i % 4 == 3 {
                ui.end_row();
            }
        }
    });
    ui.label(small(if app.memory_armed {
        format!("pick a slot for {} {}", crate::tuning::fmt_mhz(inspect, 4), app.status.mode.label())
    } else if app.memories.is_some() {
        "shared with every client of this server".into()
    } else {
        "defaults · storing one shares the bank with every client".into()
    }));
    // Renaming a slot, in place under the bank.
    let mut rename_done: Option<(usize, String)> = None;
    if let Some((i, name)) = &mut app.renaming {
        let i = *i;
        ui.horizontal(|ui| {
            ui.label(small(format!("slot {}", i + 1)));
            let r = ui.add(egui::TextEdit::singleline(name).char_limit(24).desired_width(140.0).font(theme::mono(crate::theme::ty::VALUE)));
            r.request_focus();
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) || widgets::action(ui, "OK").clicked() {
                rename_done = Some((i, name.trim().to_string()));
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                rename_done = Some((usize::MAX, String::new()));
            }
        });
    }
    if let Some((i, name)) = rename_done {
        app.renaming = None;
        let mut l = list.clone();
        if let Some(m) = l.get_mut(i).filter(|_| !name.is_empty()) {
            m.name = name;
            app.save_memories(l);
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
    if let Some(i) = delete {
        let mut l = list;
        if i < l.len() {
            l.remove(i);
            app.save_memories(l);
        }
    }
}

fn gain(app: &mut App, ui: &mut Ui) {
    let s = app.status.clone();
    let auto = s.gain_db.is_none();
    let (lo, hi) = if s.gain_max_db > s.gain_min_db { (s.gain_min_db.floor(), s.gain_max_db.ceil()) } else { (0.0, 50.0) };
    let shown = if auto { s.gain_now_db.unwrap_or(app.gain_ui) } else { app.gain_ui };
    let mut v = shown;
    let ticks = (0..=4).map(|i| (i as f32 / 4.0, format!("{:.0}", lo + (hi - lo) * i as f64 / 4.0))).collect();
    let (ch, _) = Fader {
        value: &mut v,
        min: lo,
        max: hi,
        step: 1.0,
        fine: 1.0,
        label: "RF GAIN",
        text: |v| if auto { format!("auto · {shown:.0} dB") } else { format!("{v:.0} dB") },
        color: theme::AMBER,
        enabled: !auto,
        reset: None,
        ticks,
    }
    .show(ui);
    if ch {
        app.gain_ui = v;
        app.post("/api/sdr/gain", json!({ "gain_db": v }));
    }
    if s.is_sdrplay() {
        widgets::glass(ui, egui::Id::new("gain-glass"), theme::AMBER, |ui| gain_staging(ui, &s));
    }
    if let Some(sp) = s.agc_setpoint_dbfs {
        let mut v = sp;
        let active = s.tuner_agc && auto;
        let (ch, _) = Fader {
            value: &mut v,
            min: -60.0,
            max: 0.0,
            step: 1.0,
            fine: 1.0,
            label: "IF AGC TARGET",
            text: |v| format!("{v:.0} dBFS"),
            color: theme::text_2(),
            enabled: active,
            reset: None, ticks: vec![],
        }
        .show(ui);
        if ch {
            app.post("/api/sdr/frontend", json!({ "agc_setpoint": v }));
        }
    }
    ui.horizontal_wrapped(|ui| {
        if widgets::key(ui, "AGC", auto, theme::OK).on_hover_text("Automatic gain (clip-guarded loop or the tuner's own AGC)").clicked() {
            let body = if auto { json!({ "gain_db": app.gain_ui }) } else { json!({ "gain_db": null }) };
            app.post("/api/sdr/gain", body);
        }
        let agc_name = if s.is_sdrplay() { "IF AGC" } else { "TUNER AGC" };
        if widgets::key(ui, agc_name, s.tuner_agc, theme::CYAN).on_hover_text("With AGC: hand gain to the radio's own loop").clicked() {
            app.post("/api/sdr/frontend", json!({ "tuner_agc": !s.tuner_agc }));
        }
        if widgets::key(ui, "CLIP GUARD", s.clip_guard, theme::AMBER).on_hover_text("Let the guard back gain off when the ADC clips").clicked() {
            app.post("/api/sdr/frontend", json!({ "clip_guard": !s.clip_guard }));
        }
    });
}

/// An RSP's gain as the two stages it is, each as the reduction it applies:
/// the LNA's states for this band, every segment as wide as its decibels
/// (so the 19 dB jump to the deepest state at VHF reads as one), and the IF
/// stage on its 20–59 dB range, with the window the LNA keeps it in while
/// it follows the IF AGC. Then the API's own overload report. A value the
/// radio did not report is drawn as unknown, never as the last one set.
fn gain_staging(ui: &mut Ui, s: &usdr_proto::Status) {
    let steps = &s.lna_steps_db;
    let state = s.rfgr_db.map(|v| v.round().max(0.0) as usize);
    let lna_db = state.and_then(|i| steps.get(i).copied());
    let (if_min, if_max) = (20.0, 59.0);
    let label_w = 30.0;
    let value_w = 58.0;
    let row = |ui: &mut Ui, label: &str, value: String, value_c: egui::Color32, tip: &str, draw: &dyn Fn(&egui::Painter, egui::Rect)| {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), egui::Sense::hover());
        let p = ui.painter();
        p.text(egui::pos2(rect.left(), rect.center().y), egui::Align2::LEFT_CENTER, label, theme::mono(crate::theme::ty::LABEL), theme::text_3());
        p.text(egui::pos2(rect.right(), rect.center().y), egui::Align2::RIGHT_CENTER, value, theme::mono_bold(crate::theme::ty::SMALL), value_c);
        let track = egui::Rect::from_min_max(egui::pos2(rect.left() + label_w, rect.center().y - 5.0), egui::pos2(rect.right() - value_w, rect.center().y + 5.0));
        // On the glass, a faint channel for the cells to light in.
        p.rect_filled(track, egui::CornerRadius::same(crate::theme::R_CAP), alpha(theme::ink(), 0.5));
        draw(p, track.shrink2(vec2(3.0, 2.5)));
        resp.on_hover_text(tip);
    };
    // The LNA: a segment per state past the first, lit to the state in use.
    let lna_value = match (state, lna_db) {
        (_, Some(db)) if db < 0.5 => "0 dB".into(),
        (_, Some(db)) => format!("−{db:.0} dB"),
        (Some(i), None) => format!("state {i}"),
        _ => "—".into(),
    };
    let lna_tip = if s.lna_auto {
        "LNA gain reduction, in the steps this band has. It follows the IF AGC: deeper when the IF runs out of reduction (or the radio reports overload), shallower once the IF has room again."
    } else {
        "LNA gain reduction, in the steps this band has. Set by RF GAIN: the first 20 dB of reduction comes off the IF, then the LNA takes over."
    };
    row(ui, "LNA", lna_value, if lna_db.is_some_and(|d| d > 0.0) { theme::AMBER } else { theme::text_2() }, lna_tip, &|p, t| {
        let Some(&top) = steps.last().filter(|t| **t > 0.0) else { return };
        let x = |db: f64| t.left() + t.width() * (db / top) as f32;
        for w in steps.windows(2) {
            let seg = egui::Rect::from_min_max(egui::pos2(x(w[0]) + 0.5, t.top()), egui::pos2(x(w[1]) - 0.5, t.bottom()));
            let lit = lna_db.is_some_and(|d| d >= w[1] - 0.01);
            p.rect_filled(seg, egui::CornerRadius::same(crate::theme::R_CELL), if lit { alpha(theme::AMBER, 0.85) } else { alpha(theme::AMBER, 0.10) });
        }
    });
    // The IF stage: reduction lit from the left, the take-over window
    // shaded while the LNA follows, the end of the range in red.
    let ifgr = s.ifgr_db;
    let pinned = ifgr.is_some_and(|v| v >= if_max - 2.0);
    let if_value = ifgr.map_or("—".into(), |v| format!("−{v:.0} dB"));
    let if_tip = if ifgr.is_none() {
        "IF gain reduction. The radio is not reporting where its IF AGC has it (the driver needs the live-gain patch), so it is not shown."
    } else if s.lna_auto {
        "IF gain reduction, under the RSP's IF AGC. The shaded window is where the LNA keeps it; at the right-hand end the IF has nothing left and the front end is overdriven."
    } else {
        "IF gain reduction, 20 to 59 dB."
    };
    row(ui, "IF", if_value, if pinned { theme::ERR } else { theme::AMBER }, if_tip, &|p, t| {
        let x = |db: f64| t.left() + t.width() * ((db - if_min) / (if_max - if_min)).clamp(0.0, 1.0) as f32;
        if s.lna_auto {
            let band = egui::Rect::from_min_max(egui::pos2(x(30.0), t.top() - 1.0), egui::pos2(x(50.0), t.bottom() + 1.0));
            p.rect_filled(band, egui::CornerRadius::same(crate::theme::R_CELL), alpha(theme::AMBER, 0.14));
        }
        if let Some(v) = ifgr {
            let c = if pinned { theme::ERR } else { theme::AMBER };
            p.rect_filled(egui::Rect::from_min_max(t.min, egui::pos2(x(v), t.bottom())), egui::CornerRadius::same(crate::theme::R_CELL), alpha(c, 0.55));
            p.line_segment([egui::pos2(x(v), t.top() - 2.0), egui::pos2(x(v), t.bottom() + 2.0)], egui::Stroke::new(2.0, c));
        }
    });
    // What it is doing, and the radio's own overload report.
    let note = if s.lna_auto { "LNA follows the IF AGC" } else { "LNA and IF set by RF GAIN" };
    let past = match (s.overload, s.overload_count) {
        (Some(false), Some(n)) if n > 0 => Some(format!("{n} overload{} since open", if n == 1 { "" } else { "s" })),
        _ => None,
    };
    ui.horizontal(|ui| {
        // The count goes beside the note only where both fit with room
        // between them; otherwise it is the note's hover.
        let width = |t: &str| ui.painter().layout_no_wrap(t.to_owned(), theme::mono(crate::theme::ty::LABEL), theme::text_3()).size().x;
        let fits = past.as_ref().is_some_and(|p| width(note) + width(p) + 16.0 <= ui.available_width());
        let r = ui.label(small(note));
        if let Some(p) = past.as_ref().filter(|_| !fits) {
            r.on_hover_text(p.as_str());
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if s.overload == Some(true) {
                widgets::lcd_tag(ui, "OVERLOAD", theme::ERR, true).on_hover_text("The radio reports its front end overloaded: strong signals are being mixed into false ones across the band.");
            } else if let Some(p) = past.filter(|_| fits) {
                ui.label(small(p));
            }
        });
    });
}

/// Front-end switches on, for the card's header.
fn front_end_on(s: &usdr_proto::Status) -> usize {
    [s.rf_notch, s.dab_notch, s.iq_correction, s.bias_t].iter().filter(|v| **v == Some(true)).count() + usize::from(s.lo_offset) + usize::from(!s.spurs_hz.is_empty())
}

#[derive(Clone, Copy)]
enum FrontKey {
    RfNotch,
    DabNotch,
    LoOffset,
    IqCorr,
    BiasT,
    Spur,
}

fn front_end(app: &mut App, ui: &mut Ui) {
    let s = app.status.clone();
    let spur_on = !s.spurs_hz.is_empty();
    let checking = app.spur.is_some();
    let lo = if s.lo_offset { format!("LO {:+}", crate::tuning::fmt_short_hz(s.lo_offset_hz)) } else { "LO OFFSET".into() };
    let spur = if checking { "CHECKING…".to_string() } else if spur_on { format!("SPUR NOTCH {}", s.spurs_hz.len()) } else { "SPUR NOTCH".into() };
    // The keys this radio has, two to a row at one width, so the card's
    // edges line up whatever the labels.
    let mut keys: Vec<(String, bool, egui::Color32, &str, FrontKey)> = Vec::new();
    if let Some(on) = s.rf_notch {
        keys.push(("AM/FM NOTCH".into(), on, theme::AMBER, "The tuner's broadcast-band notch", FrontKey::RfNotch));
    }
    if let Some(on) = s.dab_notch {
        keys.push(("DAB NOTCH".into(), on, theme::AMBER, "The tuner's DAB-band notch", FrontKey::DabNotch));
    }
    keys.push((lo, s.lo_offset, theme::CYAN, "Park the tuner off-center so its DC spur stays out of the channel", FrontKey::LoOffset));
    if let Some(on) = s.iq_correction {
        keys.push(("IQ CORR".into(), on, theme::CYAN, "Correct I/Q imbalance, so the image of a strong signal does not show", FrontKey::IqCorr));
    }
    if let Some(on) = s.bias_t {
        keys.push(("BIAS-T".into(), on, theme::WARN, "DC on the antenna feed, for an inline preamp; saved per radio", FrontKey::BiasT));
    }
    keys.push((spur, spur_on || checking, theme::AMBER, "Find the receiver's own spurs by moving the span an eighth and back, then notch them. Click again to clear.", FrontKey::Spur));
    let mut hit = None;
    let col_w = ((ui.available_width() - 6.0) / 2.0).floor();
    for pair in keys.chunks(2) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for (label, on, tally, tip, k) in pair {
                if widgets::key_sized(ui, label, *on, *tally, vec2(col_w, 26.0)).on_hover_text(*tip).clicked() {
                    hit = Some(*k);
                }
            }
        });
    }
    match hit {
        Some(FrontKey::RfNotch) => app.post("/api/sdr/frontend", json!({ "rf_notch": s.rf_notch != Some(true) })),
        Some(FrontKey::DabNotch) => app.post("/api/sdr/frontend", json!({ "dab_notch": s.dab_notch != Some(true) })),
        Some(FrontKey::LoOffset) => app.post("/api/sdr/frontend", json!({ "lo_offset": !s.lo_offset })),
        Some(FrontKey::IqCorr) => app.post("/api/sdr/frontend", json!({ "iq_correction": s.iq_correction != Some(true) })),
        Some(FrontKey::BiasT) => app.post("/api/sdr/frontend", json!({ "bias_t": s.bias_t != Some(true) })),
        Some(FrontKey::Spur) if !checking => {
            let now = ui.input(|i| i.time);
            app.start_spur_check(now);
        }
        _ => {}
    }
    if !s.if_bandwidths_hz.is_empty() {
        let cur = s.if_bandwidth_hz.unwrap_or(0.0);
        let label = if s.if_bandwidth_manual { crate::tuning::fmt_hz(cur) } else { format!("AUTO · {}", crate::tuning::fmt_hz(cur)) };
        ui.horizontal(|ui| {
            ui.label(small("IF FILTER"));
            let w = (ui.painter().layout_no_wrap(label.clone(), theme::mono_bold(crate::theme::ty::BODY), egui::Color32::PLACEHOLDER).size().x + 32.0).max(90.0);
            widgets::lcd_selector(ui, "iffilter", "", &label, theme::AMBER, w, |ui| {
                if ui.selectable_label(!s.if_bandwidth_manual, "Auto · follow the span").clicked() {
                    app.post("/api/sdr/frontend", json!({ "if_bandwidth": 0 }));
                    ui.close();
                }
                for w in s.if_bandwidths_hz.iter().filter(|w| **w > 0.0 && **w <= s.rate_hz * 1.001) {
                    if ui.selectable_label(s.if_bandwidth_manual && (*w - cur).abs() < 1.0, crate::tuning::fmt_hz(*w)).clicked() {
                        app.post("/api/sdr/frontend", json!({ "if_bandwidth": w }));
                        ui.close();
                    }
                }
            });
        });
    }
}

fn processing(app: &mut App, ui: &mut Ui) {
    let Some(p) = app.status.processing.clone() else { return };
    let mode = app.status.mode;
    let voice = matches!(mode, Mode::Nfm | Mode::Am | Mode::Usb | Mode::Lsb);
    let row = |ui: &mut Ui, label: &str, hint: &str, add: &mut dyn FnMut(&mut Ui)| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            let r = ui.label(small(label)).on_hover_text(hint);
            ui.add_space((78.0 - r.rect.width()).max(0.0));
            add(ui);
        });
    };
    let mut post = None;
    // Keys for processing the mode's audio never passes through are dimmed,
    // not hidden: the setting holds for when a voice mode comes back.
    let agc = matches!(mode, Mode::Am | Mode::Usb | Mode::Lsb);
    row(ui, "NOISE RED.", "Spectral noise reduction on voice: NFM, AM, USB and LSB. Higher cuts more hiss and adds about 16 ms of delay.", &mut |ui| {
        crate::widgets::enabled_or(ui, voice, "Noise reduction works on voice: NFM, AM, USB and LSB", |ui| {
            let vals = ["off", "low", "medium", "high"];
            if let Some(i) = widgets::segmented(ui, "nr", &["OFF", "LOW", "MED", "HIGH"], vals.iter().position(|v| *v == p.nr), theme::CYAN) {
                post = Some(json!({ "nr": vals[i] }));
            }
        });
    });
    row(ui, "BLANKER", "Wideband impulse blanker, for ignition and power-line clicks. It also clips the start of strong packet bursts, so leave it off for APRS.", &mut |ui| {
        if let Some(i) = widgets::segmented(ui, "blanker", &["OFF", "LOW", "MED", "HIGH"], Some(p.blanker.min(3) as usize), theme::AMBER) {
            post = Some(json!({ "blanker": i }));
        }
    });
    row(ui, "VOICE AGC", "AM and sideband AGC speed: fast follows flutter, slow rides through pauses and fades.", &mut |ui| {
        crate::widgets::enabled_or(ui, agc, "Voice AGC applies to AM, USB and LSB", |ui| {
            let vals = ["fast", "medium", "slow"];
            if let Some(i) = widgets::segmented(ui, "agc", &["FAST", "MED", "SLOW"], vals.iter().position(|v| *v == p.agc), theme::AMBER) {
                post = Some(json!({ "agc": vals[i] }));
            }
        });
    });
    if ui
        .add_enabled_ui(voice, |ui| widgets::key(ui, "AUTO NOTCH", p.auto_notch, theme::CYAN))
        .inner
        .on_hover_text("Find and null steady tones (heterodynes, carriers) in voice audio")
        .on_disabled_hover_text("Auto notch works on voice: NFM, AM, USB and LSB")
        .clicked()
    {
        post = Some(json!({ "auto_notch": !p.auto_notch }));
    }
    if let Some(body) = post {
        app.post("/api/sdr/processing", body);
    }
    let note = if agc {
        "applies to this mode's audio"
    } else if voice {
        "NFM: noise reduction and notch apply; AGC is AM and sideband only"
    } else if mode == Mode::Wfm {
        "WFM: broadcast audio is left as it is; only the blanker acts"
    } else {
        "noise reduction, notch and AGC act on NFM, AM, USB and LSB"
    };
    // Two lines kept for it, whichever mode's note it is.
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), egui::Sense::hover());
    let mut job = egui::text::LayoutJob::simple(note.to_string(), theme::mono(crate::theme::ty::LABEL), theme::text_3(), w);
    job.wrap.max_rows = 2;
    job.wrap.overflow_character = Some('…');
    let g = ui.painter().layout_job(job);
    ui.painter().galley(rect.min, g, theme::text_3());
    resp.on_hover_text(note);
}

fn recorder(app: &mut App, ui: &mut Ui) {
    let kinds: Vec<String> = app.mode_caps().map(|c| c.capture.clone()).unwrap_or_else(|| vec!["iq".into(), "voice".into(), "discriminator".into()]);
    let every_kind = app.modes.is_empty();
    let has = |k: &str| every_kind || kinds.iter().any(|x| x == k);
    let receiving = app.status.error.is_none() && app.status.rate_hz > 0.0 && !kinds.is_empty();
    // What the ring holds, on its display: filling or not, the channel and
    // its rate, and the last 20 s of level over the floor.
    widgets::glass(ui, egui::Id::new("recorder-glass"), theme::AMBER, |ui| {
        ui.horizontal(|ui| {
            widgets::lcd_tag(ui, if receiving { "FILLING" } else { "OFF" }, theme::AMBER, receiving);
            let head = if !receiving {
                format!("{} keeps no buffer", app.status.mode.label())
            } else {
                format!("{} · {}S/s", crate::tuning::fmt_mhz(app.inspect_hz(), 3), crate::tuning::fmt_short_hz(app.status.inspect_rate_hz))
            };
            ui.label(small(head));
        });
        // Drawn whether or not this mode keeps a buffer, so the card keeps
        // its height when the mode or the radio's state changes it.
        activity(app, ui, receiving);
    });
    let dir = app.prefs.record_dir();
    let busy = app.recorder_busy.clone();
    ui.horizontal_wrapped(|ui| {
        crate::bar::rec_key(app, ui);
        for (kind, label, tip) in [("iq", "SAVE I/Q", "CS16LE + JSON sidecar"), ("voice", "SAVE AUDIO", "voice WAV"), ("discriminator", "SAVE DISC", "discriminator WAV")] {
            crate::widgets::enabled_or(ui, has(kind) && busy.is_none(), "Nothing is buffered for this in the current mode, or a save is still running", |ui| {
                if widgets::action(ui, label).on_hover_text(tip).clicked()
                    && let Some(l) = &app.link
                {
                    let k = if kind == "discriminator" { "disc" } else { kind };
                    l.fetch(&format!("/api/sdr/capture.wav?kind={kind}"), Fetch::SaveCapture { kind: k.into(), stem: dir.join("x") });
                    app.recorder_busy = Some(kind.into());
                }
            });
        }
    });
    ui.horizontal_wrapped(|ui| {
        let playing = app.player.clip_progress().filter(|_| app.playing == Some(crate::app::Playing::Capture));
        if let Some(p) = playing {
            if widgets::action(ui, "■ STOP").on_hover_text("Stop and return to the live audio").clicked() {
                app.player.stop_clip();
                app.playing = None;
            }
            crate::dock::progress_bar(ui, p, 60.0);
        } else {
            crate::widgets::enabled_or(ui, has("voice") && busy.is_none(), "No voice audio is buffered in this mode, or a save is still running", |ui| {
                if widgets::action(ui, "PLAY 20 S").on_hover_text("Hear the last 20 s again, in place of the live audio").clicked()
                    && let Some(l) = &app.link
                {
                    l.fetch("/api/sdr/capture.wav?kind=voice", Fetch::PlayCapture);
                    app.recorder_busy = Some("play".into());
                }
            });
        }
        crate::widgets::enabled_or(ui, has("iq") && busy.is_none(), "No I/Q is buffered yet, or a save is still running", |ui| {
            if widgets::action(ui, "REPLAY I/Q").on_hover_text("Run the last 20 s of I/Q through the decoders again").clicked()
                && let Some(l) = &app.link
            {
                l.fetch("/api/sdr/capture.wav?kind=iq", Fetch::ReplayCapture);
                app.recorder_busy = Some("replay".into());
            }
        });
    });
    // One line for what the recorder is doing or last did, there whether
    // or not it has anything to say, so nothing under it moves.
    fixed_line(ui, 18.0, |ui| {
        if let Some(b) = &busy {
            widgets::spinner(ui, 12.0, theme::AMBER);
            ui.label(small(format!("{b}…")));
        } else if let Some(n) = &app.replay_note {
            ui.add(egui::Label::new(small(n.clone())).truncate()).on_hover_text(n.as_str());
        }
    });
    ui.horizontal_wrapped(|ui| {
        if widgets::action(ui, "LOAD I/Q…").on_hover_text("Decode a .iq (with its .json) or an I/Q WAV on the server").clicked() {
            crate::app::pick(app, "replay");
        }
        if widgets::action(ui, "FOLDER…").on_hover_text("Where saves and recordings go").clicked() {
            crate::app::pick(app, "folder");
        }
    });
    ui.label(small(format!("Saves to {} · or drop a .iq + .json on the window", dir.display())));
}

fn radio(app: &mut App, ui: &mut Ui) {
    let s = app.status.clone();
    ui.horizontal(|ui| {
        ui.label(small("DEVICE"));
        let cur = app.devices.iter().find(|d| d.serial == s.serial).map(device_label).unwrap_or_else(|| format!("{} · {}", s.serial, s.radio_label()));
        // The selector gets what the label and the refresh key leave, in a
        // space of its own: a truncating combo box fills whatever room its
        // row has, and a row wider than the column shifts every card in it.
        let w = (ui.available_width() - 34.0).max(100.0);
        widgets::lcd_selector(ui, "device", "", &cur, theme::AMBER, w, |ui| {
            for d in app.devices.clone() {
                if ui.selectable_label(d.serial == s.serial, device_label(&d)).clicked() {
                    if d.serial != s.serial {
                        app.post("/api/sdr/device", json!({ "serial": d.serial }));
                    }
                    ui.close();
                }
            }
        });
        if crate::app::icon_button(ui, "↻", "Look for radios again").clicked()
            && let Some(l) = &app.link
        {
            l.get("/api/devices", Get::Devices);
        }
    });
    // Sideband has no carrier, so there is nothing to calibrate from.
    let sideband = s.mode.is_sideband();
    ui.horizontal(|ui| {
        ui.label(small("PPM"));
        let mut ppm = s.ppm;
        let r = widgets::numeric(ui, &mut ppm, -200.0..=200.0, 0.05, 2, 76.0);
        if r.changed() {
            app.post("/api/sdr/ppm", json!({ "ppm": ppm }));
            app.status.ppm = ppm;
        }
        let r = ui.add_enabled_ui(!sideband, |ui| widgets::action(ui, "CALIBRATE")).inner;
        let r = if sideband {
            r.on_disabled_hover_text("Sideband has no carrier: calibrate on an AM or FM station with a known carrier")
        } else {
            r.on_hover_text("Measure the crystal error from the carrier you are tuned to")
        };
        if r.clicked() && let Some(l) = &app.link {
            l.post_get("/api/sdr/calibrate", json!({}), Get::Calibrate);
        }
    });
    let err = match s.freq_error_hz {
        Some(e) => format!("carrier error {e:+.0} Hz"),
        None if sideband => "no carrier in sideband".to_string(),
        None => "no carrier measured".to_string(),
    };
    ui.label(small(format!("{err} · tune {}–{}", crate::tuning::fmt_hz(s.tune_min_hz), crate::tuning::fmt_hz(s.tune_max_hz))));
}

fn device_label(d: &usdr_proto::Device) -> String {
    let mut s = format!("{} · {}", d.serial, d.tuner);
    if let Some(h) = &d.host {
        s.push_str(&format!(" @ {h}"));
    }
    if d.role != "free" && !d.role.is_empty() {
        s.push_str(&format!(" ({})", d.role));
    }
    s
}

// ---------- P25 keys ----------

#[derive(Default)]
pub struct P25Form {
    pub freq: u64,
    pub requested: u64,
    pub entries: Vec<(f64, u8, u16)>,
    pub configured: bool,
    pub algorithm: u8,
    pub key_id: String,
    pub key: String,
    pub message: Option<(Tone, String)>,
}

impl P25Form {
    pub fn load(&mut self, v: Value) {
        self.configured = v.get("configured").and_then(Value::as_bool).unwrap_or(false);
        self.entries = v
            .get("entries")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|e| {
                        (
                            e.get("frequency_hz").and_then(Value::as_f64).unwrap_or(0.0),
                            e.get("algorithm").and_then(Value::as_u64).unwrap_or(0) as u8,
                            e.get("key_id").and_then(Value::as_u64).unwrap_or(0) as u16,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(m) = v.get("message").and_then(Value::as_str) {
            self.message = Some((Tone::Ok, m.into()));
        }
    }
}

const ALGS: [(u8, &str, usize); 4] = [(170, "Motorola ADP / RC4", 10), (129, "DES-OFB", 16), (132, "AES-256-OFB", 64), (137, "AES-128-OFB", 32)];

fn alg_name(a: u8) -> String {
    match a {
        170 => "ADP".into(),
        129 => "DES-OFB".into(),
        132 => "AES-256".into(),
        137 => "AES-128".into(),
        x => format!("ALG 0x{x:02X}"),
    }
}

fn p25(app: &mut App, ui: &mut Ui) {
    let freq = app.inspect_hz().round() as u64;
    if app.p25.requested != freq
        && let Some(l) = &app.link
    {
        app.p25.requested = freq;
        app.p25.freq = freq;
        l.get(&format!("/api/sdr/p25/key?frequency_hz={freq}"), Get::P25Key);
    }
    let f = &mut app.p25;
    if f.algorithm == 0 {
        f.algorithm = 132;
    }
    ui.label(small(format!(
        "{} MHz · {}",
        crate::tuning::fmt_mhz(freq as f64, 6),
        if f.configured { "a key is configured" } else { "no key for this channel" }
    )));
    let alg_label = ALGS.iter().find(|a| a.0 == f.algorithm).map(|a| a.1).unwrap_or("?");
    widgets::selector(ui, "p25alg", alg_label, theme::text(), 0.0, |ui| {
        for (v, name, _) in ALGS {
            if ui.selectable_value(&mut f.algorithm, v, name).clicked() {
                ui.close();
            }
        }
    });
    let need = ALGS.iter().find(|a| a.0 == f.algorithm).map(|a| a.2).unwrap_or(64);
    ui.horizontal(|ui| {
        ui.label(small("KEY ID"));
        ui.add(egui::TextEdit::singleline(&mut f.key_id).desired_width(70.0).hint_text("hex").font(theme::mono(crate::theme::ty::VALUE)));
    });
    ui.add(egui::TextEdit::singleline(&mut f.key).password(true).hint_text(format!("{need} hex digits")).font(theme::mono(crate::theme::ty::VALUE)).desired_width(f32::INFINITY));
    let mut post: Option<Value> = None;
    ui.horizontal(|ui| {
        if widgets::action(ui, "SAVE KEY").clicked() {
            let kid = f.key_id.trim().trim_start_matches("0x").trim_start_matches("0X");
            let key: String = f.key.trim().chars().filter(|c| !c.is_whitespace()).collect();
            if kid.is_empty() || kid.len() > 4 || u16::from_str_radix(kid, 16).is_err() {
                f.message = Some((Tone::Err, "Key ID is 1–4 hex digits".into()));
            } else if key.len() != need || !key.chars().all(|c| c.is_ascii_hexdigit()) {
                f.message = Some((Tone::Err, format!("This algorithm takes {need} hex digits")));
            } else {
                post = Some(json!({
                    "frequency_hz": freq,
                    "action": "save",
                    "entry": { "algorithm": f.algorithm, "key_id": u16::from_str_radix(kid, 16).unwrap_or(0), "key": key }
                }));
            }
            // The key leaves the form the moment it is used.
            f.key.clear();
        }
        if f.configured && widgets::action(ui, "REMOVE").clicked() {
            post = Some(json!({ "frequency_hz": freq, "action": "remove" }));
        }
    });
    // The last key action's result, in a line kept for it.
    fixed_line(ui, 22.0, |ui| {
        if let Some((tone, m)) = &f.message {
            widgets::chip(ui, *tone, m);
        }
    });
    for (hz, alg, kid) in &f.entries {
        let cur = (*hz - freq as f64).abs() < 1.0;
        ui.label(RichText::new(format!("{} MHz · {} · KID 0x{kid:04X}", crate::tuning::fmt_mhz(*hz, 6), alg_name(*alg))).font(theme::mono(crate::theme::ty::LABEL)).color(if cur { theme::AMBER } else { theme::text_3() }));
    }
    if let (Some(body), Some(l)) = (post, &app.link) {
        l.post_get("/api/sdr/p25/key", body, Get::P25Key);
        app.p25.message = Some((Tone::Idle, "sent".into()));
    }
}
