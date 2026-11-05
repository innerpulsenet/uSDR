//! The HF data views besides FT8's slot table: a teleprinter for RTTY, a
//! signal browser for PSK31 (every signal in the passband, a row each), a
//! message board for NAVTEX, and a summary of radiofax pictures that opens
//! their viewer. Each reads the decode log the rest of the dock reads, so
//! nothing is kept twice.

use crate::app::{App, local_hms};
use crate::theme::{self, alpha};
use crate::widgets::{self, Tone};
use egui::{RichText, Sense, Stroke, Ui, pos2, vec2};
use serde_json::json;
use usdr_proto::Mode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HfView {
    Slots,
    Rtty,
    Psk,
    Navtex,
    Fax,
}

/// The view a chip asks for, or else the one the mode decodes.
pub fn view_for(app: &App) -> HfView {
    match app.decode_chip {
        Some("RTTY") => HfView::Rtty,
        Some("PSK31") => HfView::Psk,
        Some("NAVTEX") => HfView::Navtex,
        Some("WEFAX") => HfView::Fax,
        Some(_) => HfView::Slots,
        None => match app.status.mode {
            Mode::Rtty => HfView::Rtty,
            Mode::Psk31 => HfView::Psk,
            Mode::Navtex => HfView::Navtex,
            Mode::Wefax => HfView::Fax,
            _ => HfView::Slots,
        },
    }
}

pub fn show(app: &mut App, ui: &mut Ui, v: HfView) {
    match v {
        HfView::Rtty => rtty(app, ui),
        HfView::Psk => psk(app, ui),
        HfView::Navtex => navtex(app, ui),
        HfView::Fax => fax(app, ui),
        HfView::Slots => {}
    }
}

fn mono(t: impl Into<String>, c: egui::Color32) -> RichText {
    RichText::new(t).font(theme::mono(11.0)).color(c)
}

/// The decoder's lock, where it hears the signal, and how well, on one line.
fn reading_line(app: &App, ui: &mut Ui, mode: Mode, idle: &str) {
    let on = app.status.mode == mode;
    let r = app.status.digital.clone().unwrap_or_default();
    if !on {
        widgets::chip(ui, Tone::Idle, &format!("NOT IN {}", mode.label()));
        return;
    }
    if r.locked {
        widgets::chip(ui, Tone::Live, "COPYING");
    } else {
        widgets::chip(ui, Tone::Idle, idle);
    }
    if let Some(hz) = r.center_hz {
        ui.label(mono(format!("{hz:.0} Hz"), theme::AMBER));
    }
    if let Some(snr) = r.snr_db {
        ui.label(mono(format!("{snr:+.0} dB"), theme::text_2()));
    }
}

/// Text lines of one protocol, oldest first, with when each arrived.
fn lines<'a>(app: &'a App, protocol: &'a str, keep: impl Fn(&usdr_proto::DecodeEvent) -> bool + 'a) -> impl Iterator<Item = &'a crate::app::DecodeRow> + 'a {
    app.decodes.iter().rev().filter(move |d| d.ev.protocol == protocol && keep(&d.ev))
}

/// A teleprinter page: the lines in arrival order, the newest at the bottom
/// and kept in view unless the operator scrolls back.
fn teleprinter<'a>(ui: &mut Ui, id: &str, rows: impl Iterator<Item = &'a crate::app::DecodeRow>, empty: &str) {
    let rows: Vec<&crate::app::DecodeRow> = rows.collect();
    egui::Frame::new().fill(alpha(theme::ink(), 0.7)).stroke(Stroke::new(1.0, theme::line())).corner_radius(egui::CornerRadius::same(4)).inner_margin(egui::Margin::symmetric(8, 6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height(ui.available_height());
        if rows.is_empty() {
            ui.label(RichText::new(empty).font(theme::sans(12.0)).color(theme::text_3()));
            return;
        }
        egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]).stick_to_bottom(true).show_rows(ui, 17.0, rows.len(), |ui, range| {
            for d in &rows[range] {
                ui.horizontal(|ui| {
                    ui.set_min_height(17.0);
                    ui.label(mono(local_hms(d.at), theme::text_3()));
                    ui.add(egui::Label::new(RichText::new(&d.ev.summary).font(theme::mono(12.0)).color(theme::text())).wrap());
                });
            }
        });
    });
}

fn rtty(app: &mut App, ui: &mut Ui) {
    let c = app.status.digital_cfg.clone().unwrap_or_default();
    let on = app.status.mode == Mode::Rtty;
    ui.horizontal_wrapped(|ui| {
        reading_line(app, ui, Mode::Rtty, "LISTENING FOR A TONE PAIR");
        let r = app.status.digital.clone().unwrap_or_default();
        if on && let Some(rev) = r.reverse {
            ui.label(mono(if rev { "reversed" } else { "normal" }, theme::text_3()));
        }
        ui.add_space(8.0);
        ui.add_enabled_ui(on, |ui| {
            let presets = [(45.45f32, 170.0f32, "45.45 / 170"), (50.0, 450.0, "50 / 450"), (75.0, 850.0, "75 / 850")];
            let cur = presets.iter().position(|(b, s, _)| (b - c.rtty_baud).abs() < 0.01 && (s - c.rtty_shift_hz).abs() < 0.5);
            let labels: Vec<&str> = presets.iter().map(|p| p.2).collect();
            if let Some(i) = widgets::segmented(ui, "rtty-preset", &labels, cur, widgets::protocol_color("RTTY")) {
                app.post("/api/sdr/digital", json!({ "rtty_baud": presets[i].0, "rtty_shift_hz": presets[i].1 }));
            }
            ui.label(mono("BAUD / SHIFT", theme::text_3()));
            let pol = [None, Some(false), Some(true)];
            let cur = pol.iter().position(|p| *p == c.rtty_reverse);
            if let Some(i) = widgets::segmented(ui, "rtty-pol", &["AUTO", "NORMAL", "REVERSE"], cur, widgets::protocol_color("RTTY")) {
                app.post("/api/sdr/digital", json!({ "rtty_reverse": pol[i] }));
            }
        });
    });
    ui.add_space(4.0);
    teleprinter(ui, "rtty-text", lines(app, "RTTY", |_| true), "RTTY text appears here as it is copied. The decoder finds the tone pair anywhere in the passband; BANDS lists where to listen.");
}

fn navtex(app: &mut App, ui: &mut Ui) {
    ui.horizontal_wrapped(|ui| {
        reading_line(app, ui, Mode::Navtex, "LISTENING");
        let r = app.status.digital.clone().unwrap_or_default();
        if r.in_message {
            widgets::chip(ui, Tone::Warn, "MESSAGE ARRIVING");
        }
        if let Some(e) = r.error_rate {
            ui.label(mono(format!("{:.0}% errors", e * 100.0), theme::text_3()));
        }
    });
    ui.add_space(4.0);
    let messages: Vec<usize> = app.decodes.iter().enumerate().filter(|(_, d)| d.ev.protocol == "NAVTEX" && d.ev.kind.starts_with("message")).map(|(i, _)| i).collect();
    let avail = ui.available_size();
    ui.horizontal_top(|ui| {
        // Messages, newest first, opened in place.
        let list_w = (avail.x * 0.62).max(240.0);
        ui.allocate_ui_with_layout(vec2(list_w, avail.y), egui::Layout::top_down(egui::Align::Min), |ui| {
            if messages.is_empty() {
                ui.label(RichText::new("Messages appear here from ZCZC to NNNN, each with its station, subject and number. Tune 517 kHz (BANDS) for the international channel; stations send on a schedule, every four hours each.").font(theme::sans(12.0)).color(theme::text_3()));
                return;
            }
            egui::ScrollArea::vertical().id_salt("navtex-msgs").auto_shrink([false, false]).show(ui, |ui| {
                let mut toggle = None;
                for &i in &messages {
                    let d = &app.decodes[i];
                    let f = |k: &str| d.ev.fields.get(k).cloned().unwrap_or_default();
                    let complete = f("complete") != "no";
                    let resp = ui
                        .horizontal(|ui| {
                            ui.label(RichText::new(f("id")).font(theme::mono_bold(12.0)).color(widgets::protocol_color("NAVTEX")));
                            ui.label(mono(f("subjectName"), theme::text()));
                            ui.label(mono(local_hms(d.at), theme::text_3()));
                            if !complete {
                                ui.label(mono("cut off", theme::WARN));
                            }
                            if let Ok(e) = f("errorRate").parse::<f32>()
                                && e > 0.0
                            {
                                ui.label(mono(format!("{:.0}% errors", e * 100.0), theme::text_3()));
                            }
                        })
                        .response
                        .interact(Sense::click());
                    if resp.clicked() {
                        toggle = Some(i);
                    }
                    if d.open {
                        egui::Frame::new().fill(alpha(theme::ink(), 0.6)).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(&d.ev.summary).font(theme::mono(11.5)).color(theme::text())).wrap());
                        });
                    } else {
                        let first = d.ev.summary.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                        ui.add(egui::Label::new(mono(first, theme::text_2())).truncate());
                    }
                    ui.add_space(3.0);
                }
                if let Some(i) = toggle {
                    app.decodes[i].open = !app.decodes[i].open;
                }
            });
        });
        // The running text beside, as it arrives.
        ui.vertical(|ui| {
            ui.label(mono("AS IT ARRIVES", theme::text_3()));
            teleprinter(ui, "navtex-live", lines(app, "NAVTEX", |e| e.kind == "text"), "The text being received, line by line.");
        });
    });
}

/// The signal browser: each PSK31 signal in the passband on its own row,
/// at its frequency, with the last of what it sent. Click one to read all
/// of it.
fn psk(app: &mut App, ui: &mut Ui) {
    let on = app.status.mode == Mode::Psk31;
    let c = app.status.digital_cfg.clone().unwrap_or_default();
    let reading = app.status.digital.clone().unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        let n = reading.channels.iter().filter(|c| c.active).count();
        if on {
            widgets::chip(ui, if n > 0 { Tone::Live } else { Tone::Idle }, &if n > 0 { format!("{n} SIGNALS") } else { "LISTENING".into() });
        } else {
            widgets::chip(ui, Tone::Idle, "NOT IN PSK31");
        }
        ui.add_enabled_ui(on, |ui| {
            let bauds = [31.25f32, 62.5];
            if let Some(i) = widgets::segmented(ui, "psk-baud", &["BPSK31", "BPSK63"], bauds.iter().position(|b| *b == c.psk_baud), widgets::protocol_color("PSK")) {
                app.post("/api/sdr/digital", json!({ "psk_baud": bauds[i] }));
            }
        });
        let picked: Option<u32> = ui.data(|d| d.get_temp(egui::Id::new("psk-pick")));
        if picked.is_some() && widgets::action_small(ui, "ALL SIGNALS").clicked() {
            ui.data_mut(|d| d.remove::<u32>(egui::Id::new("psk-pick")));
        }
    });
    ui.add_space(4.0);
    let picked: Option<u32> = ui.data(|d| d.get_temp(egui::Id::new("psk-pick")));
    if let Some(ch) = picked {
        let key = ch.to_string();
        teleprinter(ui, "psk-one", lines(app, "PSK31", move |e| e.fields.get("channel") == Some(&key)), "Nothing from this signal yet.");
        return;
    }
    // One row per channel: what the server tracks now, plus any heard in
    // the log that has since gone quiet, by audio frequency.
    let mut rows: Vec<(u32, f32, Option<f32>, bool)> = reading.channels.iter().map(|c| (c.id, c.audio_hz, Some(c.snr_db), c.active)).collect();
    for d in app.decodes.iter().filter(|d| d.ev.protocol == "PSK31").take(400) {
        let Some(id) = d.ev.fields.get("channel").and_then(|v| v.parse::<u32>().ok()) else { continue };
        if !rows.iter().any(|r| r.0 == id) {
            let hz = d.ev.fields.get("df").and_then(|v| v.parse().ok()).unwrap_or(0.0);
            rows.push((id, hz, None, false));
        }
    }
    rows.sort_by(|a, b| a.1.total_cmp(&b.1));
    if rows.is_empty() {
        ui.label(RichText::new("Each PSK31 signal in the passband gets a row here, at its audio frequency, with what it is sending. 14.070 MHz is the busiest; BANDS lists the others.").font(theme::sans(12.0)).color(theme::text_3()));
        return;
    }
    let mut pick = None;
    egui::ScrollArea::vertical().id_salt("psk-rows").auto_shrink([false, false]).show(ui, |ui| {
        for (id, hz, snr, active) in rows {
            // The newest text of this signal, last characters first to go.
            let key = id.to_string();
            let mut tail = String::new();
            for d in app.decodes.iter().filter(|d| d.ev.protocol == "PSK31" && d.ev.fields.get("channel") == Some(&key)).take(6) {
                tail = if tail.is_empty() { d.ev.summary.clone() } else { format!("{} · {tail}", d.ev.summary) };
                if tail.len() > 240 {
                    break;
                }
            }
            let resp = ui
                .horizontal(|ui| {
                    ui.set_min_height(20.0);
                    let (r, _) = ui.allocate_exact_size(vec2(8.0, 18.0), Sense::hover());
                    theme::lamp(ui.painter(), r.center(), 3.0, widgets::protocol_color("PSK"), if active { 1.0 } else { 0.0 }, theme::LampShape::Round);
                    ui.add_sized(vec2(56.0, 18.0), egui::Label::new(mono(format!("{hz:.0}"), theme::AMBER)));
                    ui.add_sized(vec2(40.0, 18.0), egui::Label::new(mono(snr.map_or("—".into(), |s| format!("{s:+.0}")), theme::text_2())));
                    // The end of the text is the newest; it is what is kept
                    // when the row is too narrow.
                    let w = ui.available_width();
                    let room = (w / 7.0) as usize;
                    let shown: String = if tail.chars().count() > room { tail.chars().skip(tail.chars().count() - room).collect() } else { tail.clone() };
                    ui.add(egui::Label::new(RichText::new(shown).font(theme::mono(12.0)).color(if active { theme::text() } else { theme::text_2() })).truncate());
                })
                .response
                .interact(Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(resp.rect, egui::CornerRadius::same(3), alpha(theme::AMBER, 0.06));
            }
            if resp.on_hover_text("Read all of this signal").clicked() {
                pick = Some(id);
            }
        }
    });
    if let Some(id) = pick {
        ui.data_mut(|d| d.insert_temp(egui::Id::new("psk-pick"), id));
    }
}

fn fax(app: &mut App, ui: &mut Ui) {
    let r = app.status.digital.clone().unwrap_or_default();
    let on = app.status.mode == Mode::Wefax;
    ui.horizontal_wrapped(|ui| {
        match (on, r.fax_state.as_deref()) {
            (false, _) => widgets::chip(ui, Tone::Idle, "NOT IN WEFAX"),
            (true, Some("receiving")) => widgets::chip(ui, Tone::Live, &format!("RECEIVING · ROW {}", r.fax_lines)),
            (true, Some("phasing")) => widgets::chip(ui, Tone::Warn, "PHASING"),
            (true, Some("starting")) => widgets::chip(ui, Tone::Warn, "START TONE"),
            _ => widgets::chip(ui, Tone::Idle, "WAITING FOR A START TONE"),
        };
        if widgets::primary(ui, if app.fax.open { "VIEWER OPEN" } else { "OPEN VIEWER" }).clicked() {
            app.fax.open = true;
        }
        ui.label(mono(format!("{} pictures this session", app.fax.pictures.len()), theme::text_3()));
    });
    ui.add_space(6.0);
    let rows: Vec<&crate::app::DecodeRow> = app.decodes.iter().filter(|d| d.ev.protocol == "WEFAX").take(60).collect();
    if rows.is_empty() {
        ui.label(RichText::new("Radiofax pictures draw in their own viewer as they arrive. The viewer's STATIONS menu tunes NOAA's and other stations; the dial goes 1.9 kHz below the assigned frequency.").font(theme::sans(12.0)).color(theme::text_3()));
        return;
    }
    egui::ScrollArea::vertical().id_salt("fax-log").auto_shrink([false, false]).show(ui, |ui| {
        for d in rows {
            ui.horizontal(|ui| {
                ui.label(mono(local_hms(d.at), theme::text_3()));
                ui.label(mono(&d.ev.summary, if d.ev.kind == "picture" { theme::text() } else { theme::text_2() }));
            });
        }
    });
    let _ = pos2;
}
