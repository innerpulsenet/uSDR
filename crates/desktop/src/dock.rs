//! The dock under the display: the decode log with its inspector, the
//! classifier log, the calls heard, and the voice scanner.

use crate::app::{App, DockTab, local_hms};
use crate::net::Fetch;
use crate::theme::{self, alpha};
use crate::widgets::{self, Tone};
use egui::{RichText, Ui, vec2};
use serde_json::json;
use usdr_proto::Mode;

fn mono(t: impl Into<String>, c: egui::Color32) -> RichText {
    RichText::new(t).font(theme::mono(11.0)).color(c)
}

pub fn dock(app: &mut App, ui: &mut Ui) {
    // The scope: always in view beside the logs.
    if app.prefs.show_scope {
        let resp = egui::Panel::right("dock-scope")
            .resizable(true)
            .default_size(app.prefs.scope_width)
            .size_range(300.0..=800.0)
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 10, right: 0, top: 0, bottom: 0 }))
            .show(ui, |ui| scope(app, ui));
        app.prefs.scope_width = resp.response.rect.width();
    }
    ui.horizontal(|ui| {
        let tabs = [
            (DockTab::Decodes, format!("Decodes {}", app.decodes.len())),
            (DockTab::Classifier, format!("Classifier {}", app.class_log.len())),
            (DockTab::Calls, format!("Last heard {}", app.calls.len())),
            (DockTab::Scan, "Scan".to_string()),
        ];
        for (t, label) in tabs {
            let on = app.dock_tab == t;
            let r = ui.add(egui::Button::selectable(on, RichText::new(label).font(theme::sans_semibold(12.0)).color(if on { theme::text() } else { theme::text_2() })).frame_when_inactive(false));
            if on {
                let rr = r.rect;
                ui.painter().rect_filled(egui::Rect::from_min_max(egui::pos2(rr.left() + 4.0, rr.bottom()), egui::pos2(rr.right() - 4.0, rr.bottom() + 2.0)), egui::CornerRadius::ZERO, theme::AMBER);
            }
            if r.clicked() {
                app.dock_tab = t;
            }
        }
    });
    ui.add_space(4.0);
    match app.dock_tab {
        DockTab::Decodes => decodes(app, ui),
        DockTab::Classifier => classifier(app, ui),
        DockTab::Calls => calls(app, ui),
        DockTab::Scan => scan(app, ui),
    }
}

fn decodes(app: &mut App, ui: &mut Ui) {
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut app.decode_filter).hint_text("filter: protocol, kind or text").desired_width(220.0));
        for chip in ["ALL", "FLEX", "POCSAG", "APRS", "P25", "DMR", "NXDN"] {
            let on = if chip == "ALL" { app.decode_filter.is_empty() } else { app.decode_filter.eq_ignore_ascii_case(chip) };
            if ui.add(egui::Button::selectable(on, RichText::new(chip).font(theme::mono(10.5)))).clicked() {
                app.decode_filter = if chip == "ALL" { String::new() } else { chip.to_string() };
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Clear").clicked() {
                app.decodes.clear();
                app.selected_decode = None;
            }
            if ui.button("Collapse all").clicked() {
                app.decodes.iter_mut().for_each(|d| d.open = false);
            }
            if ui.button("Expand all").clicked() {
                app.decodes.iter_mut().for_each(|d| d.open = true);
            }
        });
    });
    let filter = app.decode_filter.to_lowercase();
    let visible: Vec<usize> = app
        .decodes
        .iter()
        .enumerate()
        .filter(|(_, d)| filter.is_empty() || format!("{} {} {}", d.ev.protocol, d.ev.kind, d.ev.summary).to_lowercase().contains(&filter))
        .map(|(i, _)| i)
        .collect();
    let total = app.decodes.len();
    let avail = ui.available_size();
    let list_w = if app.selected_decode.is_some() { avail.x * 0.58 } else { avail.x };
    let mut tune_to = None;
    ui.horizontal_top(|ui| {
        ui.allocate_ui(vec2(list_w, avail.y), |ui| {
            ui.label(mono(if visible.len() == total { format!("{total} frames") } else { format!("{} of {total} frames", visible.len()) }, theme::text_3()));
            // Scrolled into the log, new frames arriving on top push it down
            // by their height, so the rows being read stay where they are.
            let row_h = 24.0;
            let mut area = egui::ScrollArea::vertical().id_salt("decodes").auto_shrink([false, false]);
            if app.decode_scroll > 1.0 && app.decodes_added > 0 {
                area = area.vertical_scroll_offset(app.decode_scroll + app.decodes_added as f32 * row_h);
            }
            app.decodes_added = 0;
            let mut toggle: Option<usize> = None;
            let out = area.show(ui, |ui| {
                for &i in &visible {
                    let d = &app.decodes[i];
                    let sel = app.selected_decode == Some(i);
                    let resp = ui
                        .horizontal(|ui| {
                            ui.set_min_height(18.0);
                            if ui.add(egui::Label::new(mono(if d.open { "▾" } else { "▸" }, theme::text_3())).sense(egui::Sense::click())).clicked() {
                                toggle = Some(i);
                            }
                            ui.label(mono(local_hms(d.at), theme::text_3()));
                            if ui.add(egui::Label::new(mono(crate::tuning::fmt_mhz(d.freq_hz, 4), theme::AMBER)).sense(egui::Sense::click())).on_hover_text("Tune here").clicked() {
                                tune_to = Some(d.freq_hz);
                            }
                            let c = if d.ev.valid { widgets::protocol_color(&d.ev.protocol) } else { theme::ERR };
                            ui.label(mono(format!("[{}]", d.ev.protocol), c));
                            ui.label(mono(&d.ev.kind, theme::text_2()));
                            ui.add(egui::Label::new(RichText::new(&d.ev.summary).font(theme::sans(12.0)).color(theme::text())).truncate());
                        })
                        .response;
                    let resp = resp.interact(egui::Sense::click());
                    if sel {
                        ui.painter().rect_filled(resp.rect, egui::CornerRadius::same(3), alpha(theme::AMBER, 0.08));
                    }
                    if resp.clicked() {
                        app.selected_decode = if sel { None } else { Some(i) };
                    }
                    if d.open {
                        ui.indent(("open", i), |ui| {
                            egui::Grid::new(("row", i)).num_columns(2).spacing(vec2(12.0, 1.0)).show(ui, |ui| {
                                ui.label(mono("checksum", theme::text_3()));
                                ui.label(mono(if d.ev.valid { "valid" } else { "rejected" }, if d.ev.valid { theme::OK } else { theme::ERR }));
                                ui.end_row();
                                for (k, v) in &d.ev.fields {
                                    ui.label(mono(k, theme::text_3()));
                                    ui.add(egui::Label::new(mono(v, theme::text())).truncate());
                                    ui.end_row();
                                }
                            });
                        });
                    }
                }
            });
            app.decode_scroll = out.state.offset.y;
            if let Some(i) = toggle {
                app.decodes[i].open = !app.decodes[i].open;
            }
        });
        if let Some(i) = app.selected_decode
            && let Some(d) = app.decodes.get(i)
        {
            ui.separator();
            ui.vertical(|ui| {
                egui::ScrollArea::vertical().id_salt("inspector").auto_shrink([false, false]).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("[{}] {}", d.ev.protocol, d.ev.kind)).font(theme::sans_semibold(13.0)).color(widgets::protocol_color(&d.ev.protocol)));
                        if d.ev.valid {
                            widgets::chip(ui, Tone::Ok, "CRC PASS");
                        } else {
                            widgets::chip(ui, Tone::Err, "CRC FAIL");
                        }
                    });
                    ui.label(mono(format!("{} MHz · {} UTC", crate::tuning::fmt_mhz(d.freq_hz, 5), local_hms(d.at)), theme::text_3()));
                    ui.label(RichText::new(&d.ev.summary).color(theme::text()));
                    ui.add_space(4.0);
                    let payload_keys = ["payloadHex", "payload", "rawWords", "raw"];
                    egui::Grid::new("fields").num_columns(2).spacing(vec2(12.0, 2.0)).show(ui, |ui| {
                        for (k, v) in &d.ev.fields {
                            if payload_keys.contains(&k.as_str()) {
                                continue;
                            }
                            ui.label(mono(k, theme::text_3()));
                            ui.label(mono(v, theme::text()));
                            ui.end_row();
                        }
                    });
                    if let Some((k, v)) = payload_keys.iter().find_map(|k| d.ev.fields.get(*k).filter(|v| v.chars().filter(|c| !c.is_whitespace()).count() >= 4).map(|v| (*k, v))) {
                        ui.add_space(4.0);
                        let dump = if k == "rawWords" { word_dump(v) } else { hex_dump(v) };
                        ui.horizontal(|ui| {
                            ui.label(mono(format!("payload · {k}"), theme::text_3()));
                            if ui.small_button("copy").clicked() {
                                ui.ctx().copy_text(v.clone());
                            }
                        });
                        egui::Frame::new().fill(theme::ink()).corner_radius(egui::CornerRadius::same(3)).inner_margin(egui::Margin::same(6)).show(ui, |ui| {
                            ui.label(RichText::new(dump).font(theme::mono(10.5)).color(theme::text_2()));
                        });
                    }
                });
            });
        }
    });
    if let Some(hz) = tune_to {
        app.tune(hz, false);
    }
}

fn hex_dump(s: &str) -> String {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() < s.chars().filter(|c| !c.is_whitespace()).count() {
        return s.to_string();
    }
    let bytes: Vec<u8> = (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()).collect();
    bytes
        .chunks(16)
        .enumerate()
        .map(|(i, c)| {
            let h: Vec<String> = c.iter().map(|b| format!("{b:02x}")).collect();
            let a: String = c.iter().map(|b| if (0x20..0x7f).contains(b) { *b as char } else { '.' }).collect();
            format!("{:04x}  {:<47}  {a}", i * 16, h.join(" "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn word_dump(s: &str) -> String {
    let words: Vec<&str> = s.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()).collect();
    words.chunks(8).map(|c| c.join(" ")).collect::<Vec<_>>().join("\n")
}

fn scope(app: &mut App, ui: &mut Ui) {
    let mode = app.status.mode;
    let kind = app.frame.as_ref().map(|f| f.scope_kind).unwrap_or_default();
    let (title, tag) = app.scope.title(mode, kind);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).font(theme::sans_semibold(12.0)).color(theme::text()));
        ui.label(RichText::new(format!("{tag} · click to switch")).font(theme::mono(10.0)).color(theme::text_3()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            for hz in [25.0f32, 10.0, 5.0] {
                if widgets::chip_key(ui, &format!("{hz:.0} Hz"), (app.prefs.scope_rate_hz - hz).abs() < 0.1, theme::OK)
                    .on_hover_text("How often the scope updates")
                    .clicked()
                {
                    app.prefs.scope_rate_hz = hz;
                }
            }
        });
    });
    app.scope.rate_hz = app.prefs.scope_rate_hz;
    let pass = crate::panels::passband_slice(app);
    let rect = ui.available_rect_before_wrap();
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    app.scope.ui(&mut child, app.frame.as_deref(), mode, pass.as_ref().map(|(v, r)| (v.as_slice(), *r)), app.prefs.view.palette);
    ui.advance_cursor_after_rect(rect);
}

/// What the classifier hears right now, above its log.
fn signal_details(app: &App, ui: &mut Ui) {
    let (c, word) = app.shown_class();
    if let Some(c) = c.filter(|_| word != "IDLE") {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&c.protocol).font(theme::sans_semibold(13.0)).color(widgets::protocol_color(&c.protocol)));
            ui.label(mono(format!(
                "{} · {:.0}% · SNR {:+.1} dB · RF {:.0} dBFS · dev peak {:.1}k rms {:.1}k · DC {:+.0} Hz",
                c.modulation,
                c.confidence * 100.0,
                c.snr_db,
                c.rf_dbfs,
                c.peak_dev_hz / 1000.0,
                c.rms_dev_hz / 1000.0,
                c.center_offset_hz
            ), theme::text_2()));
            if let Some(d) = &c.details {
                ui.label(RichText::new(d).font(theme::sans(11.5)).color(theme::text_3()));
            }
        });
    }
    if let Some(sweep) = &app.status.pager_sweep {
        ui.label(mono(format!("sweeping POCSAG/FLEX · {sweep}"), theme::text_2()));
    }
    if let Some(d) = &app.status.flex_diag {
        let g = |k: &str| d.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
        ui.label(mono(format!(
            "FLEX syncs {} (6.4k {} · 3.2k {} · 1.6k {}) · BCH ok {} fix {} err {}",
            g("syncs1600") + g("syncs3200") + g("syncs6400"),
            g("syncs6400"),
            g("syncs3200"),
            g("syncs1600"),
            g("bchOk"),
            g("bchFixed"),
            g("bchErr")
        ), theme::text_2()));
    }
}

fn classifier(app: &mut App, ui: &mut Ui) {
    signal_details(app, ui);
    ui.horizontal(|ui| {
        ui.label(mono(format!("{} detections", app.class_log.len()), theme::text_3()));
        if ui.button("Clear").clicked() {
            app.class_log.clear();
        }
    });
    let mut tune_to = None;
    egui::ScrollArea::vertical().id_salt("class").auto_shrink([false, false]).show_rows(ui, 20.0, app.class_log.len(), |ui, range| {
        for row in app.class_log.range(range) {
            ui.horizontal(|ui| {
                ui.label(mono(local_hms(row.at), theme::text_3()));
                if ui.add(egui::Label::new(mono(crate::tuning::fmt_mhz(row.freq_hz, 4), theme::AMBER)).sense(egui::Sense::click())).clicked() {
                    tune_to = Some(row.freq_hz);
                }
                ui.label(mono(format!("[{}]", row.c.protocol), widgets::protocol_color(&row.c.protocol)));
                ui.label(mono(&row.c.modulation, theme::text_2()));
                ui.label(mono(format!("SNR {:+.1} dB", row.c.snr_db), theme::text_2()));
                if let Some(d) = &row.c.details {
                    ui.add(egui::Label::new(RichText::new(d).font(theme::sans(12.0)).color(theme::text_3())).truncate());
                }
            });
        }
    });
    if let Some(hz) = tune_to {
        app.tune(hz, false);
    }
}

fn calls(app: &mut App, ui: &mut Ui) {
    ui.label(mono(format!("{} call(s) recorded · kept until the server restarts", app.calls.len()), theme::text_3()));
    let dir = app.prefs.record_dir();
    let mut actions: Vec<(u64, &'static str)> = Vec::new();
    let mut tune_to = None;
    egui::ScrollArea::vertical().id_salt("calls").auto_shrink([false, false]).show(ui, |ui| {
        for c in &app.calls {
            egui::Frame::new().fill(theme::surface_2()).corner_radius(egui::CornerRadius::same(4)).inner_margin(egui::Margin::symmetric(8, 6)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(c.started_ms);
                    ui.label(mono(local_hms(t), theme::text_3()));
                    ui.label(mono(&c.protocol, widgets::protocol_color(&c.protocol)));
                    if ui.add(egui::Label::new(mono(crate::tuning::fmt_mhz(c.freq_hz, 5), theme::AMBER)).sense(egui::Sense::click())).clicked() {
                        tune_to = Some(c.freq_hz);
                    }
                    ui.label(mono(format!("{:.1} s", c.duration_s), theme::text_2()));
                    if c.source_id.is_some() || c.target_id.is_some() {
                        ui.label(mono(format!("from {} · to {}", c.source_id.map_or("—".into(), |v| v.to_string()), c.target_id.map_or("—".into(), |v| v.to_string())), theme::text()));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("IQ").on_hover_text("Save the channel I/Q (CS16LE + JSON)").clicked() {
                            actions.push((c.id, "iq"));
                        }
                        if ui.small_button("WAV").on_hover_text("Save the call audio").clicked() {
                            actions.push((c.id, "wav"));
                        }
                        if c.encrypted && !c.decrypted {
                            widgets::chip(ui, Tone::Err, "encrypted — muted");
                        } else if c.peak < 0.002 {
                            widgets::chip(ui, Tone::Warn, "no audio decoded");
                        } else if widgets::action(ui, "▶ play").clicked() {
                            actions.push((c.id, "play"));
                        }
                    });
                });
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
                    let mut chips: Vec<(Tone, String)> = Vec::new();
                    if let Some(p) = &c.digital_protocol {
                        chips.push((Tone::Idle, p.clone()));
                    }
                    if let Some(s) = c.peak_snr_db {
                        chips.push((if s < 8.0 { Tone::Warn } else { Tone::Idle }, format!("SNR {s:.1} dB")));
                    }
                    if let Some(e) = c.freq_error_hz {
                        chips.push((if e.abs() >= 1000.0 { Tone::Warn } else { Tone::Idle }, format!("tune error {e:+.0} Hz")));
                    }
                    for t in [&c.tone, &c.mdc].into_iter().flatten() {
                        chips.push((Tone::Idle, t.clone()));
                    }
                    if let Some(cc) = c.color_code {
                        chips.push((Tone::Idle, format!("CC {cc}")));
                    }
                    if let Some(s) = c.slot {
                        chips.push((Tone::Idle, format!("slot {s}")));
                    }
                    if let Some(g) = c.group {
                        chips.push((Tone::Idle, if g { "group call".into() } else { "private call".into() }));
                    }
                    if let Some(a) = &c.talker_alias {
                        chips.push((Tone::Idle, format!("alias {a}")));
                    }
                    if let Some(m) = &c.manufacturer {
                        chips.push((Tone::Idle, format!("MFID {m}")));
                    }
                    if c.emergency {
                        chips.push((Tone::Err, "EMERGENCY".into()));
                    }
                    if c.encrypted {
                        chips.push(if c.decrypted { (Tone::Ok, "encrypted · decrypted".into()) } else { (Tone::Err, "encrypted · no key".into()) });
                    }
                    if let Some(a) = &c.algorithm {
                        chips.push((Tone::Idle, format!("ALG {a}")));
                    }
                    if let Some(k) = c.key_id {
                        chips.push((Tone::Idle, format!("KEY 0x{k:04X}")));
                    }
                    if let Some(e) = c.error_pct {
                        chips.push((if e >= 20 { Tone::Err } else if e >= 8 { Tone::Warn } else { Tone::Idle }, format!("voice errors ~{e}%")));
                    }
                    if let Some(v) = c.voiced_fraction {
                        chips.push((Tone::Idle, format!("voiced {:.0}%", v * 100.0)));
                    }
                    chips.push((Tone::Idle, format!("audio {} Hz · peak {:.1}%", c.rate_hz, c.peak * 100.0)));
                    chips.push((Tone::Idle, format!("IQ CS16LE {} sps · {} samples", c.iq_rate_hz, c.iq_samples)));
                    for (t, s) in chips {
                        widgets::chip(ui, t, &s);
                    }
                });
            });
            ui.add_space(4.0);
        }
    });
    if let Some(hz) = tune_to {
        app.tune(hz, false);
    }
    for (id, what) in actions {
        let Some(l) = &app.link else { continue };
        let c = app.calls.iter().find(|c| c.id == id).cloned().unwrap_or_default();
        match what {
            "play" => l.fetch(&format!("/api/sdr/calls/{id}/audio.wav"), Fetch::CallAudio(id)),
            "wav" => l.download(&format!("/api/sdr/calls/{id}/audio.wav"), dir.join(format!("usdr-call-{id}-{:.5}MHz.wav", c.freq_hz / 1e6)), None),
            _ => {
                let meta = serde_json::to_value(&c).unwrap_or_default();
                let mut meta = json!({ "source": "recorded call", "format": "cs16le", "freqHz": c.freq_hz, "iqRateHz": c.iq_rate_hz, "iqSamples": c.iq_samples, "call": meta });
                if let Some(m) = meta.as_object_mut() {
                    m.insert("startedMs".into(), json!(c.started_ms));
                }
                l.download(
                    &format!("/api/sdr/calls/{id}/raw.iq"),
                    dir.join(format!("usdr-call-{id}-{:.5}MHz-{}sps-cs16le.iq", c.freq_hz / 1e6, c.iq_rate_hz)),
                    Some(meta),
                );
            }
        }
    }
}

fn scan(app: &mut App, ui: &mut Ui) {
    let st = app.status.scan.clone();
    let running = app.status.mode == Mode::Scan;
    ui.horizontal(|ui| {
        match &st {
            Some(s) => {
                let tone = match s.state.as_str() {
                    "locked" => Tone::Ok,
                    "testing" | "paused" => Tone::Warn,
                    _ => Tone::Idle,
                };
                let msg = s.message.clone().map(|m| format!(" · {m}")).unwrap_or_default();
                widgets::chip(ui, tone, &format!("{}{msg}", s.state));
                ui.label(mono(
                    format!("{} · {} MHz · hits {} · skipped {}", s.progress, crate::tuning::fmt_mhz(s.freq_hz, 4), s.hits, s.skipped),
                    theme::text_2(),
                ));
            }
            None => {
                widgets::chip(ui, Tone::Idle, "idle");
            }
        }
    });
    ui.add_space(4.0);
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_width(360.0);
            let f = &mut app.scan_form;
            let mut touched = false;
            egui::Grid::new("scanform").num_columns(2).spacing(vec2(10.0, 4.0)).show(ui, |ui| {
                let mut from = f.start_hz / 1e6;
                let mut to = f.stop_hz / 1e6;
                ui.label(mono("FROM / TO MHz", theme::text_3()));
                ui.horizontal(|ui| {
                    touched |= ui.add(egui::DragValue::new(&mut from).range(1.0..=2200.0).speed(0.01).fixed_decimals(4)).changed();
                    touched |= ui.add(egui::DragValue::new(&mut to).range(1.0..=2200.0).speed(0.01).fixed_decimals(4)).changed();
                });
                f.start_hz = (from * 1e6).round();
                f.stop_hz = (to * 1e6).round();
                ui.end_row();
                ui.label(mono("MODES", theme::text_3()));
                ui.horizontal(|ui| {
                    for m in ["am", "nfm", "p25", "dmr"] {
                        let mut on = f.modes.iter().any(|x| x == m);
                        if ui.checkbox(&mut on, m.to_uppercase()).changed() {
                            touched = true;
                            f.modes.retain(|x| x != m);
                            if on {
                                f.modes.push(m.into());
                            }
                        }
                    }
                });
                ui.end_row();
                ui.label(mono("SQUELCH", theme::text_3()));
                touched |= ui.add(egui::Slider::new(&mut f.threshold_db, 4.0..=20.0).step_by(0.5).suffix(" dB")).changed();
                ui.end_row();
                ui.label(mono("RESUME AFTER", theme::text_3()));
                touched |= ui.add(egui::Slider::new(&mut f.resume_delay_s, 0.5..=30.0).suffix(" s")).changed();
                ui.end_row();
                for (key, label, lo, hi, def) in [
                    ("slots", "PARALLEL", 1.0, 8.0, 4.0),
                    ("retest_s", "RE-TEST SKIPPED s", 5.0, 600.0, 60.0),
                    ("control_skip_s", "PARK CONTROL CH s", 30.0, 14_400.0, 1800.0),
                    ("persistent_after", "NOISE: PARK AFTER", 0.0, 10.0, 3.0),
                    ("persistent_skip_s", "… FOR s", 300.0, 604_800.0, 86_400.0),
                ] {
                    let mut v = f.other.get(key).and_then(|v| v.as_f64()).unwrap_or(def);
                    ui.label(mono(label, theme::text_3()));
                    if ui.add(egui::DragValue::new(&mut v).range(lo..=hi).speed(1.0).fixed_decimals(0)).changed() {
                        touched = true;
                        f.other.insert(key.into(), json!(v.round() as u64));
                    }
                    ui.end_row();
                }
            });
            if touched {
                app.scan_touched = true;
            }
            ui.horizontal(|ui| {
                if widgets::primary(ui, if running { "Apply" } else { "Start scan" }).clicked() {
                    let cfg = serde_json::to_value(&app.scan_form).unwrap_or_default();
                    app.post("/api/sdr/scan/config", cfg);
                    app.scan_touched = false;
                    if !running {
                        app.set_mode(Mode::Scan);
                    }
                }
                if running {
                    if widgets::action(ui, "STOP").clicked() {
                        let m = app.prefs.last_voice_mode;
                        app.set_mode(m);
                    }
                    let paused = st.as_ref().is_some_and(|s| s.state == "paused");
                    let (lbl, act) = if paused { ("RESUME", "resume") } else { ("PAUSE", "pause") };
                    for (l, a) in [(lbl, act), ("SKIP", "skip"), ("FORGET SKIPS", "forget")] {
                        if widgets::action(ui, l).clicked()
                            && let Some(link) = &app.link
                        {
                            link.post_each("/api/sdr/scan/control", json!({ "action": a }));
                        }
                    }
                }
            });
        });
        ui.separator();
        ui.vertical(|ui| {
            let skips = st.as_ref().map(|s| s.skip_list.clone()).unwrap_or_default();
            ui.label(mono(if skips.is_empty() { "nothing parked".into() } else { format!("{} parked", skips.len()) }, theme::text_3()));
            egui::ScrollArea::vertical().id_salt("skips").auto_shrink([false, false]).show(ui, |ui| {
                for s in skips {
                    ui.horizontal(|ui| {
                        ui.label(mono(crate::tuning::fmt_mhz(s.freq_hz, 4), theme::AMBER));
                        let c = if s.reason.contains("persistent") { theme::ERR } else { theme::text_2() };
                        ui.label(mono(&s.reason, c));
                        if s.fails > 1 {
                            ui.label(mono(format!("×{}", s.fails), theme::text_3()));
                        }
                        let left = if s.expires_in_s >= 90 { format!("{} min", s.expires_in_s / 60) } else { format!("{} s", s.expires_in_s) };
                        ui.label(mono(left, theme::text_3()));
                        if ui.small_button("re-test").clicked()
                            && let Some(l) = &app.link
                        {
                            l.post_each("/api/sdr/scan/control", json!({ "action": "unskip", "freq_hz": s.freq_hz }));
                        }
                    });
                }
            });
        });
    });
}
