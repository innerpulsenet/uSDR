//! The applet column: meter, mode, audio, gain, front end, display, scope,
//! recorder and radio, each a collapsible card.

use crate::app::App;
use crate::net::{Fetch, Get};
use crate::theme::{self, alpha};
use crate::widgets::{self, Fader, Tone};
use egui::{RichText, Ui, vec2};
use serde_json::{Value, json};
use usdr_proto::Mode;

fn small(t: impl Into<String>) -> RichText {
    RichText::new(t).font(theme::mono(10.5)).color(theme::text_3())
}

pub fn side(app: &mut App, ui: &mut Ui) {
    ui.spacing_mut().item_spacing.y = 8.0;
    let rm = app.reduce_motion();
    widgets::applet(ui, "meter", "Signal", |_| {}, |ui| {
        let mark = app.prefs.view.sql_mark;
        app.meter.ui(ui, rm, mark);
        // The squelch mark: a level to watch the channel against. The
        // server has no squelch; this opens and closes nothing.
        let (lo, hi) = app.pan.range();
        let on = mark.is_some();
        let mut v = mark.unwrap_or(app.pan.floor_est + 10.0) as f64;
        let (ch, _) = Fader {
            value: &mut v,
            min: lo.floor() as f64,
            max: hi.ceil() as f64,
            step: 1.0,
            fine: 0.5,
            label: "SQL MARK",
            text: |v| if on { format!("{v:.0} dBFS") } else { "off".into() },
            color: theme::ERR,
            enabled: on,
            ticks: vec![],
        }
        .show(ui);
        if ch && on {
            app.prefs.view.sql_mark = Some(v as f32);
        }
        ui.horizontal(|ui| {
            if widgets::key(ui, "MARK", on, theme::ERR).on_hover_text("A level line on the spectrum and the meter; drag it on the spectrum too. Visual only: the server has no squelch.").clicked() {
                app.prefs.view.sql_mark = if on { None } else { Some(app.pan.floor_est + 10.0) };
            }
            if let Some(m) = app.prefs.view.sql_mark {
                let open = app.meter.rf.is_some_and(|r| r >= m);
                widgets::chip(ui, if open { Tone::Ok } else { Tone::Idle }, if open { "OVER MARK" } else { "UNDER MARK" });
            }
        });
        let floor = app.meter.floor.map_or("—".into(), |f| format!("{f:.0} dBFS"));
        let snr = app.meter.snr.map_or("—".into(), |s| format!("{s:.1} dB"));
        ui.label(small(format!("SNR {snr} · floor {floor}")));
        let (c, word) = app.shown_class();
        if let Some(c) = c.filter(|_| word != "IDLE") {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&c.protocol).font(theme::sans_semibold(13.0)).color(widgets::protocol_color(&c.protocol)));
                ui.label(small(format!("{} · {:.0}%", c.modulation, c.confidence * 100.0)));
            });
            ui.label(small(format!(
                "dev peak {:.1}k · rms {:.1}k · DC {:+.0} Hz",
                c.peak_dev_hz / 1000.0,
                c.rms_dev_hz / 1000.0,
                c.center_offset_hz
            )));
            if let Some(d) = &c.details {
                ui.label(RichText::new(d).font(theme::sans(11.5)).color(theme::text_2()));
            }
        }
        if let Some(sweep) = &app.status.pager_sweep {
            ui.label(small(format!("sweeping POCSAG/FLEX · {sweep}")));
        }
        if let Some(d) = &app.status.flex_diag {
            let g = |k: &str| d.get(k).and_then(Value::as_u64).unwrap_or(0);
            ui.label(small(format!(
                "FLEX syncs {} (6.4k {} · 3.2k {} · 1.6k {}) · BCH ok {} fix {} err {}",
                g("syncs1600") + g("syncs3200") + g("syncs6400"),
                g("syncs6400"),
                g("syncs3200"),
                g("syncs1600"),
                g("bchOk"),
                g("bchFixed"),
                g("bchErr")
            )));
        }
    });

    widgets::applet(ui, "mode", "Mode", |_| {}, |ui| mode_grid(app, ui));
    widgets::applet(ui, "filter", "Filter", |_| {}, |ui| filter(app, ui));
    widgets::applet(ui, "memories", "Memories", |_| {}, |ui| memories(app, ui));
    widgets::applet(ui, "audio", "Audio", |_| {}, |ui| audio(app, ui));
    widgets::applet(ui, "rxb", "Receiver B", |_| {}, |ui| receiver_b(app, ui));
    widgets::applet(ui, "gain", "RF gain", |_| {}, |ui| gain(app, ui));
    widgets::applet(ui, "frontend", "Front end", |_| {}, |ui| front_end(app, ui));
    widgets::applet(ui, "display", "Span · zoom · display", |_| {}, |ui| display(app, ui));
    if app.prefs.show_scope {
        widgets::applet(ui, "scope", "Scope", |_| {}, |ui| {
            let mode = app.status.mode;
            let kind = app.frame.as_ref().map(|f| f.scope_kind).unwrap_or_default();
            let (title, tag) = app.scope.title(mode, kind);
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).font(theme::sans_semibold(12.0)).color(theme::text()));
                ui.label(small(format!("· {tag} · click to switch")));
            });
            let pass = passband_slice(app);
            let (_, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 0.0), egui::Sense::hover());
            let _ = resp;
            let h = 150.0;
            let rect = egui::Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), h));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
            app.scope.ui(&mut child, app.frame.as_deref(), mode, pass.as_ref().map(|(v, r)| (v.as_slice(), *r)), app.prefs.view.palette);
            ui.advance_cursor_after_rect(rect);
        });
    }
    widgets::applet(ui, "recorder", "Recorder · last 20 s", |_| {}, |ui| recorder(app, ui));
    widgets::applet(ui, "radio", "Radio", |_| {}, |ui| radio(app, ui));
    if app.status.mode == Mode::P25 {
        widgets::applet(ui, "p25", "P25 channel key", |_| {}, |ui| p25(app, ui));
    }
}

/// The main display's trace across the passband, for the scope.
fn passband_slice(app: &App) -> Option<(Vec<f32>, (f32, f32))> {
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

fn mode_grid(app: &mut App, ui: &mut Ui) {
    let groups: [(&str, &[Mode]); 2] = [
        ("DEMOD", &[Mode::Nfm, Mode::Am, Mode::Usb, Mode::Lsb, Mode::Wfm, Mode::P25, Mode::Dmr, Mode::Nxdn]),
        ("DECODE · SCAN", &[Mode::Packet, Mode::Pager, Mode::Flex, Mode::Auto, Mode::Scan]),
    ];
    for (title, modes) in groups {
        ui.label(small(title));
        let w = (ui.available_width() - 3.0 * 4.0) / 4.0;
        egui::Grid::new(title).spacing(vec2(4.0, 4.0)).show(ui, |ui| {
            for (i, m) in modes.iter().enumerate() {
                let on = app.status.mode == *m;
                let tally = match m {
                    Mode::Nfm | Mode::Am | Mode::Usb | Mode::Lsb => theme::AMBER,
                    Mode::Wfm | Mode::Pager | Mode::Flex => theme::CYAN,
                    Mode::P25 | Mode::Dmr | Mode::Nxdn | Mode::Packet => theme::OK,
                    _ => theme::VIOLET,
                };
                let desc = app.modes.iter().find(|c| c.mode == *m).map(|c| c.description.clone()).unwrap_or_default();
                let r = widgets::key_sized(ui, m.label(), on, tally, vec2(w, 26.0));
                let r = if desc.is_empty() { r } else { r.on_hover_text(desc) };
                if r.clicked() && !on {
                    app.set_mode(*m);
                }
                if i % 4 == 3 {
                    ui.end_row();
                }
            }
        });
    }
}

fn filter(app: &mut App, ui: &mut Ui) {
    let lo = app.status.filter_low_hz as f32;
    let hi = app.status.filter_high_hz as f32;
    let width = (hi - lo).max(0.0);
    let k = |v: f32| if v.abs() >= 10_000.0 { format!("{:.0}k", v / 1000.0) } else { format!("{:.2}k", v / 1000.0) };
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{} … {}", k(lo), k(hi))).font(theme::mono_bold(13.0)).color(theme::AMBER));
        ui.label(small(format!("width {}", k(width))));
    });
    let Some(range) = app.filter_range() else {
        let why = if app.server.as_ref().is_some_and(|i| i.has("filter")) {
            format!("{} keeps the filter its decoder needs", app.status.mode.label())
        } else {
            "this server cannot set the filter (it predates API 2)".to_string()
        };
        ui.label(small(why));
        return;
    };
    let mode = app.status.mode;
    // Presets: widths that suit the mode, from the low edge for sideband.
    let presets: &[f32] = match mode {
        Mode::Usb | Mode::Lsb => &[1_800.0, 2_100.0, 2_400.0, 2_700.0, 3_000.0, 3_600.0],
        Mode::Am => &[4_000.0, 6_000.0, 8_000.0, 10_000.0, 12_000.0, 16_000.0],
        Mode::Nfm => &[6_000.0, 8_000.0, 11_000.0, 12_500.0, 16_000.0, 25_000.0],
        Mode::Wfm => &[100_000.0, 150_000.0, 180_000.0, 200_000.0, 230_000.0],
        _ => &[],
    };
    let mut set: Option<(f32, f32)> = None;
    let w = (ui.available_width() - 5.0 * 4.0) / 6.0;
    egui::Grid::new("filter-presets").spacing(vec2(4.0, 4.0)).show(ui, |ui| {
        for (i, p) in presets.iter().enumerate() {
            let on = (width - p).abs() < 1.0;
            let label = format!("{}k", format!("{:.1}", p / 1000.0).trim_end_matches(".0"));
            if widgets::key_sized(ui, &label, on, theme::AMBER, vec2(w, 24.0)).clicked() {
                set = Some(match mode {
                    Mode::Usb => (lo, (lo + p).min(range.max_edge_hz)),
                    Mode::Lsb => ((hi - p).max(-range.max_edge_hz), hi),
                    _ => (-p / 2.0, p / 2.0),
                });
            }
            if i % 6 == 5 {
                ui.end_row();
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
        text: |v| format!("{:.2} kHz", v / 1000.0),
        color: theme::AMBER,
        enabled: true,
        ticks: vec![],
    }
    .show(ui);
    if ch {
        let v = wv as f32;
        set = Some(match mode {
            Mode::Usb => (lo, (lo + v).min(range.max_edge_hz)),
            Mode::Lsb => ((hi - v).max(-range.max_edge_hz), hi),
            _ => (-v / 2.0, v / 2.0),
        });
    }
    if !range.symmetric {
        // Shift moves the passband, keeping its width, along the sideband.
        let center = (lo + hi) / 2.0;
        let (cmin, cmax) = if mode == Mode::Usb {
            (width / 2.0, range.max_edge_hz - width / 2.0)
        } else {
            (-range.max_edge_hz + width / 2.0, -width / 2.0)
        };
        let mut cv = center as f64;
        let (ch, _) = Fader {
            value: &mut cv,
            min: cmin as f64,
            max: cmax.max(cmin) as f64,
            step: 50.0,
            fine: 10.0,
            label: "SHIFT (passband center)",
            text: |v| format!("{:+.0} Hz", v),
            color: theme::CYAN,
            enabled: cmax > cmin,
            ticks: vec![],
        }
        .show(ui);
        if ch {
            let c = cv as f32;
            set = Some((c - width / 2.0, c + width / 2.0));
        }
    }
    ui.horizontal(|ui| {
        let custom = app.status.filter_custom;
        if widgets::key(ui, if custom { "RESET" } else { "MODE DEFAULT" }, !custom, theme::OK).on_hover_text("Back to the mode's own filter").clicked() && custom {
            app.reset_filter();
        }
        ui.label(small(if range.symmetric { "drag a passband edge on the display" } else { "drag an edge on the display · Ctrl-drag inside to shift" }));
    });
    if let Some((l, h)) = set {
        app.set_filter(l.round() as f64, h.round() as f64);
    }
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
                if ui.button("Store the current channel here").clicked() {
                    store_at = Some(i);
                    ui.close();
                }
                if m.is_some() && ui.button("Rename…").clicked() {
                    app.renaming = Some((i, m.map(|m| m.name.clone()).unwrap_or_default()));
                    ui.close();
                }
                if m.is_some() && ui.button("Clear").clicked() {
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
            let r = ui.add(egui::TextEdit::singleline(name).char_limit(24).desired_width(140.0).font(theme::mono(11.5)));
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

fn audio(app: &mut App, ui: &mut Ui) {
    let mut vol = app.prefs.volume as f64 * 100.0;
    let (ch, _) = Fader {
        value: &mut vol,
        min: 0.0,
        max: 100.0,
        step: 2.0,
        fine: 1.0,
        label: "AF GAIN",
        text: |v| if app.prefs.muted { "muted".into() } else { format!("{v:.0}") },
        color: theme::AMBER,
        enabled: !app.prefs.muted,
        ticks: vec![],
    }
    .show(ui);
    if ch {
        app.prefs.volume = (vol / 100.0) as f32;
        app.player.set_volume(app.prefs.volume);
    }
    let mut pan = app.prefs.pan as f64;
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
        app.prefs.pan = pan as f32;
        app.player.set_pan(app.prefs.pan);
    }
    // Output level.
    let r = app.player.reading();
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, egui::CornerRadius::same(2), theme::ink());
    let lvl = |v: f32| ((20.0 * v.max(1e-5).log10() + 60.0) / 60.0).clamp(0.0, 1.0);
    let rms = egui::Rect::from_min_size(rect.min, vec2(rect.width() * lvl(r.rms), rect.height()));
    ui.painter().rect_filled(rms, egui::CornerRadius::same(2), alpha(theme::OK, 0.8));
    let px = rect.left() + rect.width() * lvl(r.peak);
    ui.painter().line_segment([egui::pos2(px, rect.top()), egui::pos2(px, rect.bottom())], egui::Stroke::new(2.0, theme::text()));
    ui.horizontal(|ui| {
        if widgets::key(ui, if app.prefs.muted { "MUTED" } else { "MONITOR" }, !app.prefs.muted, theme::OK).on_hover_text("M").clicked() {
            app.prefs.muted = !app.prefs.muted;
            app.player.set_muted(app.prefs.muted);
        }
        let rec = r.recording.is_some();
        if widgets::key(ui, if rec { "STOP REC" } else { "REC AUDIO" }, rec, theme::EMBER)
            .on_hover_text("Record what you hear to a WAV on this computer (R)")
            .clicked()
        {
            let ctx = ui.ctx().clone();
            app.toggle_local_recording(&ctx);
        }
        if let Some(p) = app.player.clip_progress() {
            if widgets::action(ui, &format!("■ {:.0}%", p * 100.0)).on_hover_text("Stop playback, back to live").clicked() {
                app.player.stop_clip();
            }
            crate::anim::more(ui.ctx());
        }
    });
    if r.queue_ms > 0.0 {
        ui.label(small(format!("{} Hz in · queue {:.0} ms · clock {:+.0} ppm", r.in_rate.round(), r.queue_ms, r.drift_ppm)));
    } else {
        ui.label(small("no audio arriving"));
    }
}

fn receiver_b(app: &mut App, ui: &mut Ui) {
    if !app.rx2_available() {
        ui.label(small("this server has no second receiver (it predates API 2)"));
        return;
    }
    let Some(b) = app.status.rx2.clone() else {
        ui.label(small("A second listener inside the span, with its own audio. Middle-click the display, or:"));
        if widgets::action(ui, "START RX B").clicked() {
            // Beside receiver A, inside the span.
            let a = app.inspect_hz();
            let span = (app.status.max_freq_hz - app.status.min_freq_hz).max(0.0);
            let hz = if a + span / 8.0 < app.status.max_freq_hz { a + span / 8.0 } else { a - span / 8.0 };
            let step = crate::tuning::mode_raster_hz(app.status.mode, hz);
            app.place_b(crate::tuning::snap(hz, step));
            app.active_b = true;
        }
        return;
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(crate::tuning::fmt_mhz(b.freq_hz, 5)).font(theme::mono_bold(15.0)).color(theme::VIOLET));
        ui.label(small(format!("{} · {}", b.mode.label(), crate::tuning::fmt_short_hz(b.bandwidth_hz))));
    });
    if let Some(n) = &b.note {
        widgets::chip(ui, Tone::Warn, n);
    }
    let w = (ui.available_width() - 4.0 * 4.0) / 5.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for m in [Mode::Nfm, Mode::Am, Mode::Usb, Mode::Lsb, Mode::Wfm] {
            if widgets::key_sized(ui, m.label(), b.mode == m, theme::VIOLET, vec2(w, 24.0)).clicked() && b.mode != m {
                app.set_rx2(b.freq_hz, m);
            }
        }
    });
    ui.horizontal_wrapped(|ui| {
        if widgets::key(ui, "ACTIVE", app.active_b, theme::EMBER).on_hover_text("The display, the readout and the keys drive receiver B (B)").clicked() {
            app.active_b = !app.active_b;
        }
        if widgets::action(ui, "SWAP A↔B").on_hover_text("Trade frequencies between the receivers").clicked() {
            let a = app.inspect_hz();
            app.active_b = false;
            app.tune(b.freq_hz, false);
            app.set_rx2(a, b.mode);
        }
        if widgets::action(ui, "OFF").clicked() {
            app.rx2_off();
        }
    });
    let mut vol = app.prefs.volume_b as f64 * 100.0;
    let muted = app.prefs.muted_b;
    let (ch, _) = Fader {
        value: &mut vol,
        min: 0.0,
        max: 100.0,
        step: 2.0,
        fine: 1.0,
        label: "AF GAIN B",
        text: |v| if muted { "muted".into() } else { format!("{v:.0}") },
        color: theme::VIOLET,
        enabled: !muted,
        ticks: vec![],
    }
    .show(ui);
    if ch {
        app.prefs.volume_b = (vol / 100.0) as f32;
        app.player_b.set_volume(app.prefs.volume_b);
    }
    let mut pan = app.prefs.pan_b as f64;
    let (ch, _) = Fader {
        value: &mut pan,
        min: -1.0,
        max: 1.0,
        step: 0.1,
        fine: 0.02,
        label: "BALANCE B",
        text: |p| if p.abs() < 0.01 { "center".into() } else if p < 0.0 { format!("L {:.0}", -p * 100.0) } else { format!("R {:.0}", p * 100.0) },
        color: theme::text_2(),
        enabled: true,
        ticks: vec![],
    }
    .show(ui);
    if ch {
        app.prefs.pan_b = pan as f32;
        app.player_b.set_pan(app.prefs.pan_b);
    }
    ui.horizontal(|ui| {
        if widgets::key(ui, if muted { "MUTED" } else { "MONITOR" }, !muted, theme::OK).clicked() {
            app.prefs.muted_b = !muted;
            app.player_b.set_muted(app.prefs.muted_b);
        }
        let r = app.player_b.reading();
        ui.label(small(if r.queue_ms > 0.0 { format!("{} Hz · {:.0} ms", r.in_rate.round(), r.queue_ms) } else { "no audio yet".into() }));
    });
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
        ticks,
    }
    .show(ui);
    if ch {
        app.gain_ui = v;
        app.post("/api/sdr/gain", json!({ "gain_db": v }));
    }
    if s.rfgr_db.is_some() || s.ifgr_db.is_some() {
        ui.label(small(format!(
            "LNA state {} · IF reduction {} dB",
            s.rfgr_db.map_or("—".into(), |v| format!("{v:.0}")),
            s.ifgr_db.map_or("—".into(), |v| format!("{v:.0}"))
        )));
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
            color: theme::CYAN,
            enabled: active,
            ticks: vec![],
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

fn front_end(app: &mut App, ui: &mut Ui) {
    let s = app.status.clone();
    ui.horizontal_wrapped(|ui| {
        if let Some(on) = s.rf_notch
            && widgets::key(ui, "AM/FM NOTCH", on, theme::VIOLET).clicked()
        {
            app.post("/api/sdr/frontend", json!({ "rf_notch": !on }));
        }
        if let Some(on) = s.dab_notch
            && widgets::key(ui, "DAB NOTCH", on, theme::VIOLET).clicked()
        {
            app.post("/api/sdr/frontend", json!({ "dab_notch": !on }));
        }
        let lo = if s.lo_offset { format!("LO {:+}", crate::tuning::fmt_short_hz(s.lo_offset_hz)) } else { "LO OFFSET".into() };
        if widgets::key(ui, &lo, s.lo_offset, theme::CYAN).on_hover_text("Park the tuner off-center so its DC spur stays out of the channel").clicked() {
            app.post("/api/sdr/frontend", json!({ "lo_offset": !s.lo_offset }));
        }
        if let Some(on) = s.iq_correction
            && widgets::key(ui, "IQ CORR", on, theme::CYAN).clicked()
        {
            app.post("/api/sdr/frontend", json!({ "iq_correction": !on }));
        }
        if let Some(on) = s.bias_t
            && widgets::key(ui, "BIAS-T", on, theme::WARN).on_hover_text("DC on the antenna feed, for an inline preamp; saved per radio").clicked()
        {
            app.post("/api/sdr/frontend", json!({ "bias_t": !on }));
        }
        let spur_on = !s.spurs_hz.is_empty();
        let checking = app.spur.is_some();
        let label = if checking { "CHECKING…".to_string() } else if spur_on { format!("SPUR NOTCH {}", s.spurs_hz.len()) } else { "SPUR NOTCH".into() };
        if widgets::key(ui, &label, spur_on || checking, theme::VIOLET)
            .on_hover_text("Find the receiver's own spurs by moving the span an eighth and back, then notch them. Click again to clear.")
            .clicked()
            && !checking
        {
            let now = ui.input(|i| i.time);
            app.start_spur_check(now);
        }
    });
    if !s.if_bandwidths_hz.is_empty() {
        let cur = s.if_bandwidth_hz.unwrap_or(0.0);
        let label = if s.if_bandwidth_manual { crate::tuning::fmt_hz(cur) } else { format!("AUTO · {}", crate::tuning::fmt_hz(cur)) };
        ui.horizontal(|ui| {
            ui.label(small("IF FILTER"));
            egui::ComboBox::from_id_salt("iffilter").selected_text(RichText::new(label).font(theme::mono(11.0))).show_ui(ui, |ui| {
                if ui.selectable_label(!s.if_bandwidth_manual, "Auto · follow the span").clicked() {
                    app.post("/api/sdr/frontend", json!({ "if_bandwidth": 0 }));
                }
                for w in s.if_bandwidths_hz.iter().filter(|w| **w > 0.0 && **w <= s.rate_hz * 1.001) {
                    if ui.selectable_label(s.if_bandwidth_manual && (*w - cur).abs() < 1.0, crate::tuning::fmt_hz(*w)).clicked() {
                        app.post("/api/sdr/frontend", json!({ "if_bandwidth": w }));
                    }
                }
            });
        });
    }
    let n = [s.rf_notch, s.dab_notch, s.iq_correction, s.bias_t].iter().filter(|v| **v == Some(true)).count() + usize::from(s.lo_offset);
    ui.label(small(format!("{} · {}", s.tuner.trim_start_matches("Rafael Micro "), if n == 0 { "all off".into() } else { format!("{n} on") })));
}

fn display(app: &mut App, ui: &mut Ui) {
    let s = app.status.clone();
    let rates: Vec<f64> = if s.rates_hz.is_empty() { vec![250e3, 1.024e6, 1.536e6, 2.048e6, 2.4e6, 3.2e6] } else { s.rates_hz.clone() };
    let want = app.span_pending.map(|(w, _)| w).unwrap_or(s.rate_hz);
    let cur = rates.iter().enumerate().min_by(|a, b| (a.1 - want).abs().total_cmp(&(b.1 - want).abs())).map(|x| x.0).unwrap_or(0);
    let mut idx = cur as f64;
    let n = rates.len().max(2);
    let ticks = rates
        .iter()
        .enumerate()
        .filter(|(i, _)| *i == 0 || *i == n - 1 || i % (n.div_ceil(5)) == 0)
        .map(|(i, r)| (i as f32 / (n - 1) as f32, crate::tuning::fmt_short_hz(*r)))
        .collect();
    let mut text = format!("{}S/s", crate::tuning::fmt_short_hz(rates[cur]));
    if s.tuner.to_ascii_uppercase().contains("RSP1A") {
        let r = rates[cur];
        let bits = if r <= 6.048e6 { 14 } else if r <= 8.064e6 { 12 } else if r <= 9.216e6 { 10 } else { 8 };
        text = format!("{text} · {bits}-bit");
    }
    let (ch, rel) = Fader { value: &mut idx, min: 0.0, max: (n - 1) as f64, step: 1.0, fine: 1.0, label: "SPAN", text: |_| text.clone(), color: theme::CYAN, enabled: true, ticks }.show(ui);
    if ch {
        let r = rates[idx.round() as usize];
        let now = ui.input(|i| i.time);
        app.span_pending = Some((r, now));
    }
    // The span changes on release (or after the wheel rests), because each
    // change rebuilds every chain on the server.
    if let Some((r, at)) = app.span_pending {
        let now = ui.input(|i| i.time);
        let dragging = ui.input(|i| i.pointer.any_down());
        if (rel || (!dragging && now - at > 0.45)) && (r - s.rate_hz).abs() > 1.0 {
            app.post("/api/sdr/rate", json!({ "rate_hz": r }));
            app.span_pending = Some((r, f64::INFINITY));
        } else if at.is_finite() {
            crate::anim::more(ui.ctx());
        }
    }
    let mut z = app.zoom_ui.max(1.0).log2();
    let ticks = (0..=5).map(|i| (i as f32 / 5.0, format!("{}×", 1 << i))).collect();
    let (ch, _) = Fader { value: &mut z, min: 0.0, max: 5.0, step: 0.25, fine: 0.05, label: "ZOOM", text: |z| format!("{:.1}×", 2f64.powf(z)), color: theme::CYAN, enabled: true, ticks }.show(ui);
    if ch {
        app.set_zoom(2f64.powf(z));
    }
    ui.horizontal(|ui| {
        if widgets::action(ui, "ZOOM −").clicked() {
            app.set_zoom(app.zoom_ui / 2.0);
        }
        if widgets::action(ui, "ZOOM +").clicked() {
            app.set_zoom(app.zoom_ui * 2.0);
        }
        if widgets::action(ui, "1×").clicked() {
            app.set_zoom(1.0);
        }
        if widgets::action(ui, "CENTER").on_hover_text("Put the span back around the receive frequency (C)").clicked() {
            let hz = app.inspect_hz();
            app.center_span(hz);
        }
    });
    ui.label(small("AVERAGING"));
    let avg_idx = match s.avg.as_deref() {
        Some("slow") => 1,
        Some("deep") => 2,
        _ => 0,
    };
    if let Some(i) = widgets::segmented(ui, &["OFF", "0.5 s", "2 s"], avg_idx) {
        let mode = ["off", "slow", "deep"][i];
        app.post("/api/sdr/avg", json!({ "mode": mode }));
        app.status.avg = Some(mode.into());
    }
    let v = &mut app.prefs.view;
    ui.horizontal_wrapped(|ui| {
        if widgets::key(ui, "PEAK", v.peak_hold, theme::CYAN).clicked() {
            v.peak_hold = !v.peak_hold;
        }
        if widgets::key(ui, "FILL", v.fill, theme::AMBER).clicked() {
            v.fill = !v.fill;
        }
        if widgets::key(ui, "RANGE AUTO", v.auto_range, theme::OK).clicked() {
            v.auto_range = !v.auto_range;
        }
        if widgets::key(ui, "WF PEAK", v.wf_peak, theme::VIOLET).on_hover_text("Paint the waterfall from each frame's strongest segment, so bursts show at full level").clicked() {
            v.wf_peak = !v.wf_peak;
        }
    });
    if !v.auto_range {
        let mut fl = v.floor_db as f64;
        let mut ce = v.ceiling_db as f64;
        let (a, _) = Fader { value: &mut fl, min: -150.0, max: -20.0, step: 2.0, fine: 1.0, label: "FLOOR", text: |v| format!("{v:.0} dBFS"), color: theme::text_2(), enabled: true, ticks: vec![] }.show(ui);
        let (b, _) = Fader { value: &mut ce, min: -100.0, max: 10.0, step: 2.0, fine: 1.0, label: "CEILING", text: |v| format!("{v:.0} dBFS"), color: theme::text_2(), enabled: true, ticks: vec![] }.show(ui);
        if a || b {
            v.floor_db = fl as f32;
            v.ceiling_db = (ce as f32).max(v.floor_db + 10.0);
        }
    }
    ui.label(small("WATERFALL"));
    ui.horizontal(|ui| {
        ui.label(small("palette"));
        egui::ComboBox::from_id_salt("palette").selected_text(v.palette.label()).show_ui(ui, |ui| {
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
        if widgets::key(ui, "AUTO BLACK", v.wf_auto_black, theme::OK).on_hover_text("Keep the black level just under the noise floor").clicked() {
            v.wf_auto_black = !v.wf_auto_black;
            if !v.wf_auto_black {
                v.wf_black = app.pan.floor_est;
            } else {
                v.wf_black = -3.0;
            }
        }
    });
    let mut black = v.wf_black as f64;
    let (lo, hi, label) = if v.wf_auto_black { (-20.0, 20.0, "BLACK (from floor)") } else { (-150.0, -20.0, "BLACK LEVEL") };
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

fn recorder(app: &mut App, ui: &mut Ui) {
    let kinds: Vec<String> = app.mode_caps().map(|c| c.capture.clone()).unwrap_or_else(|| vec!["iq".into(), "voice".into(), "discriminator".into()]);
    let has = |k: &str| app.modes.is_empty() || kinds.iter().any(|x| x == k);
    let receiving = app.status.error.is_none() && app.status.rate_hz > 0.0 && !kinds.is_empty();
    ui.horizontal(|ui| {
        widgets::chip(ui, if receiving { Tone::Live } else { Tone::Idle }, if receiving { "FILLING" } else { "OFF" });
        let head = if !receiving {
            format!("off in {}", app.status.mode.label())
        } else {
            format!("{} · {}S/s", crate::tuning::fmt_mhz(app.inspect_hz(), 3), crate::tuning::fmt_short_hz(app.status.inspect_rate_hz))
        };
        ui.label(small(head));
    });
    let dir = app.prefs.record_dir();
    let busy = app.recorder_busy.clone();
    ui.horizontal_wrapped(|ui| {
        for (kind, label, tip) in [("iq", "SAVE I/Q", "CS16LE + JSON sidecar"), ("voice", "SAVE AUDIO", "voice WAV"), ("discriminator", "SAVE DISC", "discriminator WAV")] {
            ui.add_enabled_ui(has(kind) && busy.is_none(), |ui| {
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
        ui.add_enabled_ui(has("voice") && busy.is_none(), |ui| {
            if widgets::action(ui, "PLAY 20 S").on_hover_text("Hear the last 20 s again, in place of the live audio").clicked()
                && let Some(l) = &app.link
            {
                l.fetch("/api/sdr/capture.wav?kind=voice", Fetch::PlayCapture);
                app.recorder_busy = Some("play".into());
            }
        });
        ui.add_enabled_ui(has("iq") && busy.is_none(), |ui| {
            if widgets::action(ui, "REPLAY I/Q").on_hover_text("Run the last 20 s of I/Q through the decoders again").clicked()
                && let Some(l) = &app.link
            {
                l.fetch("/api/sdr/capture.wav?kind=iq", Fetch::ReplayCapture);
                app.recorder_busy = Some("replay".into());
            }
        });
    });
    if let Some(b) = &busy {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(12.0).color(theme::AMBER));
            ui.label(small(format!("{b}…")));
        });
    }
    if let Some(n) = &app.replay_note {
        ui.label(small(n.clone()));
    }
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
        egui::ComboBox::from_id_salt("device").selected_text(RichText::new(cur).font(theme::mono(11.0))).width(ui.available_width() - 30.0).show_ui(ui, |ui| {
            for d in app.devices.clone() {
                if ui.selectable_label(d.serial == s.serial, device_label(&d)).clicked() && d.serial != s.serial {
                    app.post("/api/sdr/device", json!({ "serial": d.serial }));
                }
            }
        });
        if crate::app::icon_button(ui, "↻", "Look for radios again").clicked()
            && let Some(l) = &app.link
        {
            l.get("/api/devices", Get::Devices);
        }
    });
    ui.horizontal(|ui| {
        ui.label(small("PPM"));
        let mut ppm = s.ppm;
        let r = ui.add(egui::DragValue::new(&mut ppm).range(-200.0..=200.0).speed(0.05).fixed_decimals(2));
        if r.changed() {
            app.post("/api/sdr/ppm", json!({ "ppm": ppm }));
            app.status.ppm = ppm;
        }
        if widgets::action(ui, "CALIBRATE").on_hover_text("Measure the crystal error from the carrier you are tuned to").clicked()
            && let Some(l) = &app.link
        {
            l.post_get("/api/sdr/calibrate", json!({}), Get::Calibrate);
        }
    });
    let err = s.freq_error_hz.map_or("no carrier measured".into(), |e| format!("carrier error {e:+.0} Hz"));
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
    egui::ComboBox::from_id_salt("p25alg")
        .selected_text(ALGS.iter().find(|a| a.0 == f.algorithm).map(|a| a.1).unwrap_or("?"))
        .show_ui(ui, |ui| {
            for (v, name, _) in ALGS {
                ui.selectable_value(&mut f.algorithm, v, name);
            }
        });
    let need = ALGS.iter().find(|a| a.0 == f.algorithm).map(|a| a.2).unwrap_or(64);
    ui.horizontal(|ui| {
        ui.label(small("KEY ID"));
        ui.add(egui::TextEdit::singleline(&mut f.key_id).desired_width(70.0).hint_text("hex").font(theme::mono(11.5)));
    });
    ui.add(egui::TextEdit::singleline(&mut f.key).password(true).hint_text(format!("{need} hex digits")).font(theme::mono(11.5)).desired_width(f32::INFINITY));
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
    if let Some((tone, m)) = &f.message {
        widgets::chip(ui, *tone, m);
    }
    for (hz, alg, kid) in &f.entries {
        let cur = (*hz - freq as f64).abs() < 1.0;
        ui.label(RichText::new(format!("{} MHz · {} · KID 0x{kid:04X}", crate::tuning::fmt_mhz(*hz, 6), alg_name(*alg))).font(theme::mono(10.5)).color(if cur { theme::AMBER } else { theme::text_3() }));
    }
    if let (Some(body), Some(l)) = (post, &app.link) {
        l.post_get("/api/sdr/p25/key", body, Get::P25Key);
        app.p25.message = Some((Tone::Idle, "sent".into()));
    }
}
