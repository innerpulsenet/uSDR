//! The views of the HF utility, time and broadcast modes: SITOR and
//! PACTOR as teleprinters, DSC calls, HFDL aircraft and ground stations,
//! the STANAG modems with their constellation, the WWV time code as a
//! clock and the minute's bits, and DRM's service with its text and
//! constellation. Each reads the decode log and the decoder's reading the
//! rest of the dock reads.
//!
//! The decoders say what they found in their events' fields; a view picks
//! out the ones it lays out and shows the rest when a row is opened, so a
//! field a decoder adds later is seen without a change here.

use crate::app::{App, DecodeRow, local_hms};
use crate::theme::{self, alpha};
use crate::widgets::{self, Tone};
use egui::{RichText, Sense, Stroke, Ui, pos2, vec2};
use serde_json::json;
use usdr_proto::{DigitalReading, Mode};

fn mono(t: impl Into<String>, c: egui::Color32) -> RichText {
    RichText::new(t).font(theme::mono(11.0)).color(c)
}

fn reading(app: &App) -> DigitalReading {
    app.status.digital.clone().unwrap_or_default()
}

/// The decoder's lock, what it found, where and how well, and its other
/// live numbers, on one wrapped line.
pub fn reading_strip(app: &App, ui: &mut Ui, mode: Mode, idle: &str) {
    let r = reading(app);
    if app.status.mode != mode {
        widgets::chip(ui, Tone::Idle, &format!("NOT IN {}", mode.label()));
        return;
    }
    if r.locked {
        widgets::chip(ui, Tone::Live, r.variant.as_deref().map(str::to_uppercase).as_deref().unwrap_or("COPYING"));
    } else {
        widgets::chip(ui, Tone::Idle, idle);
    }
    if let Some(hz) = r.center_hz {
        ui.label(mono(format!("{hz:.0} Hz"), theme::AMBER));
    }
    if let Some(snr) = r.snr_db {
        ui.label(mono(format!("{snr:+.0} dB"), theme::text_2()));
    }
    for f in &r.facts {
        let c = match f.tone.as_str() {
            "ok" => theme::OK,
            "warn" => theme::WARN,
            _ => theme::text_2(),
        };
        ui.label(mono(&f.label, theme::text_3()));
        ui.label(mono(&f.value, c));
    }
}

/// A decode log's rows of one protocol, newest first.
fn rows<'a>(app: &'a App, protocol: &'a str) -> impl Iterator<Item = (usize, &'a DecodeRow)> + 'a {
    app.decodes.iter().enumerate().filter(move |(_, d)| d.ev.protocol.to_ascii_uppercase().contains(protocol))
}

/// Messages a row each, newest first: when, what kind, the summary, and a
/// few of its fields as columns; click one to see every field.
fn message_list(app: &mut App, ui: &mut Ui, protocol: &str, columns: &[(&str, &str, f32)], empty: &str) {
    let idx: Vec<usize> = rows(app, protocol).map(|(i, _)| i).take(500).collect();
    if idx.is_empty() {
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(empty).font(theme::sans(12.0)).color(theme::text_3())).wrap());
        return;
    }
    let tally = widgets::protocol_color(protocol);
    let mut toggle = None;
    egui::ScrollArea::vertical().id_salt(("msgs", protocol)).auto_shrink([false, false]).show(ui, |ui| {
        for &i in &idx {
            let d = &app.decodes[i];
            let resp = ui
                .horizontal(|ui| {
                    ui.set_min_height(18.0);
                    ui.label(mono(local_hms(d.at), theme::text_3()));
                    ui.add_sized(vec2(86.0, 16.0), egui::Label::new(RichText::new(d.ev.kind.to_uppercase()).font(theme::mono_bold(10.0)).color(if d.ev.valid { tally } else { theme::WARN })).truncate());
                    for (key, _, w) in columns {
                        let v = derived(&d.ev, key).unwrap_or_else(|| d.ev.fields.get(*key).cloned().unwrap_or_default());
                        ui.add_sized(vec2(*w, 16.0), egui::Label::new(mono(v, theme::text_2())).truncate());
                    }
                    ui.add(egui::Label::new(RichText::new(d.ev.summary.lines().next().unwrap_or("")).font(theme::mono(11.5)).color(theme::text())).truncate());
                })
                .response
                .interact(Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(resp.rect, egui::CornerRadius::same(3), alpha(tally, 0.06));
            }
            if resp.clicked() {
                toggle = Some(i);
            }
            if d.open {
                egui::Frame::new().fill(alpha(theme::ink(), 0.6)).inner_margin(egui::Margin::symmetric(10, 4)).show(ui, |ui| {
                    if d.ev.summary.contains('\n') {
                        ui.add(egui::Label::new(RichText::new(&d.ev.summary).font(theme::mono(11.5)).color(theme::text())).wrap());
                        ui.add_space(4.0);
                    }
                    // What its identifiers stand for, then every field.
                    let words = translations(&d.ev);
                    if !words.is_empty() {
                        egui::Grid::new(("words", i)).num_columns(2).spacing(vec2(12.0, 1.0)).show(ui, |ui| {
                            for (k, v, link) in &words {
                                ui.label(mono(*k, theme::text_3()));
                                ui.horizontal(|ui| {
                                    ui.label(mono(v, theme::CYAN));
                                    if let Some(url) = link
                                        && widgets::action_small(ui, "LOOK UP ↗").on_hover_text(url.as_str()).clicked()
                                    {
                                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                                    }
                                });
                                ui.end_row();
                            }
                        });
                        ui.add_space(4.0);
                    }
                    egui::Grid::new(("fields", i)).num_columns(2).spacing(vec2(12.0, 1.0)).show(ui, |ui| {
                        for (k, v) in &d.ev.fields {
                            ui.label(mono(k, theme::text_3()));
                            ui.add(egui::Label::new(mono(v, theme::text())).wrap());
                            ui.end_row();
                        }
                    });
                });
            }
        }
    });
    if let Some(i) = toggle {
        app.decodes[i].open = !app.decodes[i].open;
    }
}

/// A column worked out from the fields rather than read from one: the
/// keys a view's columns name with an `@`.
fn derived(ev: &usdr_proto::DecodeEvent, key: &str) -> Option<String> {
    let f = |k: &str| ev.fields.get(k).cloned().filter(|v| !v.is_empty());
    match key {
        "@from" => f("from").map(|m| crate::lookups::mmsi_line(&m)),
        "@to" => f("to").map(|m| crate::lookups::mmsi_line(&m)).or_else(|| f("area")),
        "@gs" => f("gsId").map(|id| crate::stations_data::HFDL.iter().find(|s| s.call == id).map_or(id.clone(), |s| s.name.split(',').next().unwrap_or(s.name).to_string())),
        "@aircraft" => Some(crate::lookups::describe_hfdl(ev)).filter(|s| *s != ev.summary),
        "@reg" => f("reg").map(|r| {
            let r = r.trim_start_matches('.').to_string();
            match crate::lookups::registration_state(&r) {
                // The state's short name: what comes before any bracket.
                Some(st) => format!("{r} · {}", st.split(" (").next().unwrap_or(st)),
                None => r,
            }
        }),
        "@airline" => f("flight").and_then(|fl| crate::lookups::airline(&fl)).map(|(n, _)| n.to_string()),
        _ => None,
    }
}

/// What a decode's identifiers stand for, as (what, in words, a link to
/// look it up): MMSIs, aircraft addresses and registrations, flight
/// numbers, ACARS labels.
fn translations(ev: &usdr_proto::DecodeEvent) -> Vec<(&'static str, String, Option<String>)> {
    use crate::lookups as l;
    let f = |k: &str| ev.fields.get(k).cloned().filter(|v| !v.is_empty());
    let mut out = Vec::new();
    for (k, what) in [("from", "from"), ("to", "to"), ("distressId", "in distress")] {
        if ev.protocol == "DSC"
            && let Some(m) = f(k).filter(|m| m.len() == 9)
        {
            let line = l::mmsi_line(&m);
            if !line.is_empty() {
                out.push((what, format!("{m} · {line}"), l::mmsi_link(&m)));
            }
        }
    }
    if ev.protocol == "HFDL" {
        if let Some(fl) = f("flight")
            && let Some((name, c)) = l::airline(&fl)
        {
            out.push(("airline", format!("{name} ({c})"), None));
        }
        if let Some(reg) = f("reg")
            && let Some(s) = l::registration_state(reg.trim_start_matches('.'))
        {
            out.push(("registered", s.to_string(), None));
        }
        if let Some(icao) = f("icao") {
            let state = l::icao_state(&icao).map_or(icao.clone(), |s| format!("{icao} · {s}"));
            out.push(("ICAO address", state, l::aircraft_link(&icao)));
        }
        if let Some(lb) = f("label")
            && let Some(m) = l::acars_label(&lb)
        {
            out.push(("label", format!("{lb}: {m}"), None));
        }
        if let Some(id) = f("gsId")
            && let Some(st) = crate::stations_data::HFDL.iter().find(|s| s.call == id)
        {
            out.push(("ground station", format!("{id} · {}", st.name), None));
        }
    }
    out
}

/// The column heads over a message list, lined up with its columns.
fn list_head(ui: &mut Ui, columns: &[(&str, &str, f32)]) {
    ui.horizontal(|ui| {
        let head = |t: &str| RichText::new(t).font(theme::mono(10.0)).color(theme::text_3()).extra_letter_spacing(0.6);
        ui.add_sized(vec2(ui.painter().layout_no_wrap("00:00:00".into(), theme::mono(11.0), theme::text()).size().x, 14.0), egui::Label::new(head("UTC")));
        ui.add_sized(vec2(86.0, 14.0), egui::Label::new(head("KIND")));
        for (_, title, w) in columns {
            ui.add_sized(vec2(*w, 14.0), egui::Label::new(head(title)));
        }
        ui.label(head("MESSAGE"));
    });
}

/// A teleprinter page of one protocol's text, newest at the bottom.
fn teleprinter(app: &App, ui: &mut Ui, protocol: &str, side_field: Option<&str>, empty: &str) {
    let lines: Vec<&DecodeRow> = app.decodes.iter().rev().filter(|d| d.ev.protocol.to_ascii_uppercase().contains(protocol)).collect();
    egui::Frame::new().fill(alpha(theme::ink(), 0.7)).stroke(Stroke::new(1.0, theme::line())).corner_radius(egui::CornerRadius::same(4)).inner_margin(egui::Margin::symmetric(8, 6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height(ui.available_height());
        if lines.is_empty() {
            ui.add(egui::Label::new(RichText::new(empty).font(theme::sans(12.0)).color(theme::text_3())).wrap());
            return;
        }
        egui::ScrollArea::vertical().id_salt(("tp", protocol)).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            for d in lines {
                ui.horizontal(|ui| {
                    ui.label(mono(local_hms(d.at), theme::text_3()));
                    let text = d.ev.summary.as_str();
                    let event = d.ev.kind != "text";
                    if let Some(side) = side_field.and_then(|k| d.ev.fields.get(k)) {
                        ui.add_sized(vec2(70.0, 16.0), egui::Label::new(RichText::new(side).font(theme::mono_bold(10.5)).color(widgets::protocol_color(protocol))).truncate());
                    }
                    let c = if event { theme::text_2() } else { theme::text() };
                    ui.add(egui::Label::new(RichText::new(text).font(if event { theme::mono(11.0) } else { theme::mono(12.0) }).color(c)).wrap());
                });
            }
        });
    });
}

/// Equalized symbols on the I/Q plane: the newest brightest, a ring at
/// unit amplitude, axes through the middle.
pub fn constellation(ui: &mut Ui, pts: &[[f32; 2]], size: f32, tally: egui::Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let p = ui.painter_at(r);
    p.rect_filled(r, egui::CornerRadius::same(4), theme::ink());
    let c = r.center();
    let scale = size * 0.36;
    p.line_segment([pos2(r.left() + 4.0, c.y), pos2(r.right() - 4.0, c.y)], Stroke::new(1.0, theme::line()));
    p.line_segment([pos2(c.x, r.top() + 4.0), pos2(c.x, r.bottom() - 4.0)], Stroke::new(1.0, theme::line()));
    p.circle_stroke(c, scale, Stroke::new(1.0, alpha(theme::text_3(), 0.35)));
    let n = pts.len().max(1) as f32;
    for (k, q) in pts.iter().enumerate() {
        let x = c.x + q[0].clamp(-1.4, 1.4) * scale;
        let y = c.y - q[1].clamp(-1.4, 1.4) * scale;
        let age = k as f32 / n;
        p.circle_filled(pos2(x, y), 1.6, alpha(tally, 0.25 + 0.7 * age));
    }
    if pts.is_empty() {
        p.text(c, egui::Align2::CENTER_CENTER, "no symbols", theme::mono(10.0), theme::text_3());
    }
    p.rect_stroke(r, egui::CornerRadius::same(4), Stroke::new(1.0, theme::line()), egui::StrokeKind::Inside);
}

pub fn sitor(app: &mut App, ui: &mut Ui) {
    let c = app.status.digital_cfg.clone().unwrap_or_default();
    let on = app.status.mode == Mode::Sitor;
    ui.horizontal_wrapped(|ui| {
        reading_strip(app, ui, Mode::Sitor, "LISTENING FOR 100 BAUD");
        ui.add_space(8.0);
        ui.add_enabled_ui(on, |ui| {
            let modes = ["auto", "a", "b"];
            let cur = modes.iter().position(|m| *m == c.sitor_mode);
            if let Some(i) = widgets::segmented(ui, "sitor-mode", &["AUTO", "A · ARQ", "B · FEC"], cur, widgets::protocol_color("SITOR")) {
                app.post("/api/sdr/digital", json!({ "sitor_mode": modes[i] }));
            }
        });
    });
    ui.add_space(4.0);
    teleprinter(app, ui, "SITOR", Some("from"), "SITOR text appears here as it is copied: mode B broadcasts as they run, mode A links a block at a time with each side's traffic marked. The decoder finds the tone pair in the passband and tells A from B by the timing.");
}

pub fn pactor(app: &mut App, ui: &mut Ui) {
    ui.horizontal_wrapped(|ui| {
        reading_strip(app, ui, Mode::Pactor, "LISTENING FOR PACTOR-I");
    });
    ui.add_space(4.0);
    teleprinter(app, ui, "PACTOR", Some("side"), "PACTOR-I links appear here: the connect with both callsigns, then each side's text as its packets check. PACTOR-II, III and IV are proprietary and are not decoded. Amateur PACTOR is found in the automatic digital segments; the band keys above tune them.");
}

pub fn dsc(app: &mut App, ui: &mut Ui) {
    let cols: &[(&str, &str, f32)] = &[("formatName", "FORMAT", 100.0), ("categoryName", "CATEGORY", 70.0), ("from", "FROM", 80.0), ("@from", "WHO", 130.0), ("to", "TO", 80.0), ("@to", "", 110.0)];
    ui.horizontal_wrapped(|ui| {
        reading_strip(app, ui, Mode::Dsc, "LISTENING FOR CALLS");
    });
    // Distress traffic stands out above the log until it is seen.
    let distress = rows(app, "DSC").find(|(_, d)| d.ev.fields.get("category").is_some_and(|c| c.to_ascii_lowercase().contains("distress")) || d.ev.kind.contains("distress")).map(|(_, d)| (d.at, d.ev.summary.clone()));
    if let Some((at, text)) = distress
        && at.elapsed().map(|e| e.as_secs() < 3_600).unwrap_or(false)
    {
        egui::Frame::new().fill(alpha(theme::ERR, 0.12)).stroke(Stroke::new(1.0, alpha(theme::ERR, 0.6))).corner_radius(egui::CornerRadius::same(4)).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("DISTRESS").font(theme::mono_bold(11.0)).color(theme::ERR));
                ui.label(mono(local_hms(at), theme::text_2()));
                ui.add(egui::Label::new(mono(text, theme::text())).truncate());
            });
        });
    }
    ui.add_space(4.0);
    list_head(ui, cols);
    message_list(app, ui, "DSC", cols, "Digital selective calls appear here: distress and urgency alerts, safety and routine calls between ships and coast stations, each with its MMSIs, category and what it asks. HF DSC keeps 2187.5, 4207.5, 6312, 8414.5, 12577 and 16804.5 kHz; STATIONS tunes them.");
}

pub fn hfdl(app: &mut App, ui: &mut Ui) {
    let cols: &[(&str, &str, f32)] = &[("@gs", "GROUND", 76.0), ("flight", "FLIGHT", 60.0), ("@airline", "AIRLINE", 100.0), ("@reg", "AIRCRAFT", 130.0)];
    ui.horizontal_wrapped(|ui| {
        reading_strip(app, ui, Mode::Hfdl, "LISTENING FOR BURSTS");
    });
    ui.add_space(4.0);
    // Two columns, so a squitter arriving never moves the messages being
    // read: the ground stations heard on the left, the traffic beside.
    let w = ui.available_width();
    let h = ui.available_height();
    let left = (w * 0.34).clamp(250.0, 380.0);
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(vec2(left, h), egui::Layout::top_down(egui::Align::Min), |ui| {
            egui::ScrollArea::vertical().id_salt("hfdl-gs").auto_shrink([false, false]).show(ui, |ui| ground_stations(app, ui));
        });
        ui.allocate_ui_with_layout(vec2(w - left - 8.0, h), egui::Layout::top_down(egui::Align::Min), |ui| {
            list_head(ui, cols);
            message_list(app, ui, "HFDL", cols, "HFDL appears here: the aircraft logged on to each ground station, their ACARS messages and positions. The station list above tunes the ground stations; most are busiest at night on the lower frequencies.");
        });
    });
}

/// The ground stations heard in the last quarter hour, from their
/// squitters: when, and the frequencies each says it has on. A squitter
/// names its frequencies by their place in the station's list in the
/// system table, so they read in kilohertz only while the table here is
/// the one on the air.
fn ground_stations(app: &mut App, ui: &mut Ui) {
    let mut heard: Vec<(u32, std::time::SystemTime, Vec<u8>, Option<u32>)> = Vec::new();
    for (_, d) in rows(app, "HFDL").filter(|(_, d)| d.ev.kind == "squitter") {
        if d.at.elapsed().map(|e| e.as_secs() > 900).unwrap_or(true) {
            break;
        }
        let Some(id) = d.ev.fields.get("gsId").and_then(|v| v.parse::<u32>().ok()) else { continue };
        if heard.iter().any(|h| h.0 == id) {
            continue;
        }
        let ids = d.ev.fields.get("freqIds").map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect()).unwrap_or_default();
        let table = d.ev.fields.get("systableVersion").and_then(|v| v.parse().ok());
        heard.push((id, d.at, ids, table));
    }
    if heard.is_empty() {
        ui.label(mono("GROUND STATIONS HEARD", theme::text_3()));
        ui.add(egui::Label::new(RichText::new("Each ground station sends a squitter every 32 seconds, naming the frequencies it has on. They are listed here as they are heard.").font(theme::sans(11.0)).color(theme::text_3())).wrap());
        return;
    }
    let table_here = crate::stations_data::HFDL_TABLE_VERSION;
    let stale = heard.iter().filter_map(|h| h.3).find(|v| *v != table_here);
    ui.horizontal_wrapped(|ui| {
        ui.label(mono("GROUND STATIONS HEARD", theme::text_3()));
        if let Some(v) = stale {
            ui.label(mono(format!("the air's system table is version {v}, this list {table_here}: frequencies may have moved"), theme::WARN));
        }
    });
    let tally = widgets::protocol_color("HFDL");
    let mut pick = None;
    for (id, at, ids, _) in &heard {
        let st = crate::stations_data::HFDL.iter().find(|s| s.call == id.to_string());
        // A station on two lines: who and when, then its frequencies.
        ui.horizontal(|ui| {
            ui.add_sized(vec2(22.0, 16.0), egui::Label::new(RichText::new(id.to_string()).font(theme::mono_bold(11.0)).color(tally)));
            ui.add(egui::Label::new(mono(st.map_or("not in the table", |s| s.name), theme::text())).truncate());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(mono(format!("{} s ago", at.elapsed().map(|e| e.as_secs()).unwrap_or(0)), theme::text_3()));
            });
        });
        ui.horizontal_wrapped(|ui| {
            ui.add_space(26.0);
            ui.spacing_mut().item_spacing.x = 4.0;
            for i in ids {
                match st.and_then(|s| s.freqs_khz.get(*i as usize)) {
                    Some(khz) if stale.is_none() => {
                        let here = (app.inspect_hz() - khz * 1e3).abs() < 50.0;
                        if widgets::chip_key(ui, &format!("{khz:.0}"), here, tally).on_hover_text("On the air now; click to tune").clicked() {
                            pick = Some(*khz);
                        }
                    }
                    _ => {
                        ui.label(mono(format!("#{i}"), theme::text_3()));
                    }
                }
            }
        });
        ui.add_space(3.0);
    }
    if let Some(k) = pick {
        crate::directory::tune(app, &crate::stations::HFDL, k);
    }
}

pub fn stanag(app: &mut App, ui: &mut Ui) {
    let cols: &[(&str, &str, f32)] = &[("waveform", "WAVEFORM", 130.0), ("dataRateBps", "BPS", 44.0), ("dpduTypeName", "D_PDU", 90.0)];
    ui.horizontal_wrapped(|ui| reading_strip(app, ui, Mode::Stanag, "LOOKING FOR A SERIAL TONE"));
    ui.add_space(2.0);
    ui.add(egui::Label::new(RichText::new("Identifies STANAG 4285 and MIL-STD-188-110 serial tones, demodulates them and reads STANAG 5066 headers where the data is sent in the clear. Most military traffic is encrypted: its waveform, rate and addresses are what can be read. The constellation is in the Scope panel.").font(theme::sans(11.0)).color(theme::text_3())).wrap());
    ui.add_space(4.0);
    list_head(ui, cols);
    message_list(app, ui, "STANAG", cols, "Waveforms and decoded headers appear here as they are found.");

}

/// 2G ALE: soundings and calls, a row each, with who called whom; the
/// words they were built from open under a row.
pub fn ale(app: &mut App, ui: &mut Ui) {
    let cols: &[(&str, &str, f32)] = &[("caller", "FROM", 90.0), ("called", "TO", 90.0), ("conclusion", "END", 44.0)];
    ui.horizontal_wrapped(|ui| reading_strip(app, ui, Mode::Ale, "LISTENING FOR ALE"));
    ui.add_space(4.0);
    list_head(ui, cols);
    message_list(app, ui, "2G ALE", cols, "ALE soundings and calls appear here: a station's sounding names it (TIS or TWAS), a call names both ends. The amateur HFN channels in the station list sound around the clock; 14109 kHz is the busiest by day.");
}

/// JS8Call: whole messages a row each (frames joined), who to whom and
/// what; heartbeats and directed queries marked by kind.
pub fn js8(app: &mut App, ui: &mut Ui) {
    let cols: &[(&str, &str, f32)] = &[("speed", "SPEED", 50.0), ("df", "Hz", 40.0), ("snr", "dB", 30.0), ("from", "FROM", 70.0), ("to", "TO", 70.0)];
    ui.horizontal_wrapped(|ui| reading_strip(app, ui, Mode::Js8, "LISTENING"));
    ui.add_space(4.0);
    list_head(ui, cols);
    message_list(app, ui, "JS8", cols, "JS8Call messages appear here a whole transmission at a time: heartbeats, CQs, directed queries and replies, and free text. The band keys above tune JS8Call's dials; 7.078 and 14.078 MHz are the busiest.");
}

/// WSPR: each slot's beacons, newest first, with the path to each one's
/// locator from the station's own.
pub fn wspr(app: &mut App, ui: &mut Ui) {
    ui.horizontal_wrapped(|ui| reading_strip(app, ui, Mode::Wspr, "WAITING FOR THE SLOT"));
    ui.add_space(4.0);
    let here = app.station.as_ref().and_then(|(st, _)| usdr_proto::station::grid_center(&st.grid));
    let idx: Vec<usize> = rows(app, "WSPR").map(|(i, _)| i).take(400).collect();
    let head = |t: &str| RichText::new(t).font(theme::mono(10.0)).color(theme::text_3()).extra_letter_spacing(0.6);
    let cols = [("UTC", 64.0), ("dB", 34.0), ("Hz", 44.0), ("DRIFT", 50.0), ("CALL", 90.0), ("GRID", 56.0), ("POWER", 76.0), ("KM", 56.0), ("KM/W", 70.0)];
    ui.horizontal(|ui| {
        for (t, w) in cols {
            ui.add_sized(vec2(w, 14.0), egui::Label::new(head(t)));
        }
    });
    if idx.is_empty() {
        ui.add(egui::Label::new(RichText::new("WSPR beacons appear here at the end of each even-minute slot: call, locator and power, with the path from your locator (Settings › Station). The band keys above tune WSJT-X's WSPR dials.").font(theme::sans(12.0)).color(theme::text_3())).wrap());
        return;
    }
    egui::ScrollArea::vertical().id_salt("wspr-rows").auto_shrink([false, false]).show(ui, |ui| {
        for i in idx {
            let d = &app.decodes[i];
            let f = |k: &str| d.ev.fields.get(k).cloned().unwrap_or_default();
            let slot = f("slot");
            let dbm: f64 = f("dbm").parse().unwrap_or(f64::NAN);
            let watts = 10f64.powf((dbm - 30.0) / 10.0);
            let path = here.zip(usdr_proto::station::grid_center(&f("grid"))).map(|(a, b)| usdr_proto::station::path(a, b).0);
            ui.horizontal(|ui| {
                let cells = [
                    (if slot.len() >= 4 { format!("{}:{}", &slot[..2], &slot[2..4]) } else { slot.clone() }, theme::text_3()),
                    (f("snr"), theme::text_2()),
                    (f("df"), theme::AMBER),
                    (f("drift"), theme::text_3()),
                    (f("call"), theme::text()),
                    (f("grid"), theme::CYAN),
                    (if dbm.is_finite() { format!("{dbm:.0} dBm {}", if watts < 1.0 { format!("{:.0} mW", watts * 1e3) } else { format!("{watts:.0} W") }) } else { String::new() }, theme::text_2()),
                    (path.map_or(String::new(), |k| format!("{k:.0}")), theme::text_2()),
                    (path.filter(|_| watts > 0.0).map_or(String::new(), |k| format!("{:.0}", k / watts)), theme::OK),
                ];
                for ((t, c), (_, w)) in cells.into_iter().zip(cols) {
                    ui.add_sized(vec2(w, 16.0), egui::Label::new(mono(t, c)).truncate());
                }
            });
        }
    });
}

pub fn olivia(app: &mut App, ui: &mut Ui) {
    ui.horizontal_wrapped(|ui| reading_strip(app, ui, Mode::Olivia, "TRYING 8/250 · 8/500 · 16/500 · 32/1000 · 16/1000 · 4/250"));
    ui.add_space(4.0);
    teleprinter(app, ui, "OLIVIA", None, "Olivia text appears here as it is copied: the decoder tries the common settings at once around 1500 Hz and keeps the one that decodes. The band keys above tune the Olivia club's calling frequencies.");
}

/// MFSK: the text as it arrives and, beside it, the newest picture a
/// Shortwave Radiogram show sent.
pub fn mfsk(app: &mut App, ui: &mut Ui) {
    ui.horizontal_wrapped(|ui| reading_strip(app, ui, Mode::Mfsk, "LOOKING FOR MFSK"));
    ui.add_space(4.0);
    let w = ui.available_width();
    let h = ui.available_height();
    let has_picture = rows(app, "MFSK").any(|(_, d)| d.ev.kind == "picture");
    let pic_w = if has_picture { (w * 0.4).clamp(220.0, 420.0) } else { 0.0 };
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(vec2(w - pic_w - if has_picture { 8.0 } else { 0.0 }, h), egui::Layout::top_down(egui::Align::Min), |ui| {
            teleprinter(app, ui, "MFSK", None, "MFSK text appears here, and its pictures beside it. Shortwave Radiogram sends both on WRMI; the station list above has its times. The decoder finds an MFSK16, 32 or 64 signal anywhere in 300–2700 Hz.");
        });
        if has_picture {
            ui.allocate_ui_with_layout(vec2(pic_w, h), egui::Layout::top_down(egui::Align::Min), |ui| crate::sstvview::canvas_pub(app, ui, h));
        }
    });
}

/// Columns of Feld Hell kept for the ribbon.
pub const HELL_KEEP: usize = 2_400;

/// Feld Hell: the columns as the eye reads them, a ribbon drawn twice, one
/// copy over the other, so text that wanders up or down a row is still
/// whole in one of them.
pub fn hell(app: &mut App, ui: &mut Ui) {
    ui.horizontal_wrapped(|ui| reading_strip(app, ui, Mode::Hell, "LOOKING FOR A HELL CARRIER"));
    ui.add_space(6.0);
    let rows = 14usize;
    let cols = app.hell.len() / rows;
    let w = ui.available_width();
    let px = ((ui.available_height() - 20.0) / (2.0 * rows as f32)).clamp(2.0, 5.0);
    let (r, _) = ui.allocate_exact_size(vec2(w, 2.0 * rows as f32 * px + 8.0), Sense::hover());
    let p = ui.painter_at(r);
    p.rect_filled(r, egui::CornerRadius::same(4), theme::ink());
    if cols == 0 {
        p.text(r.center(), egui::Align2::CENTER_CENTER, "Feld Hell paints here, newest at the right. The band keys above tune the Feld Hell Club's activity ranges.", theme::sans(12.0), theme::text_3());
        return;
    }
    let shown = ((w - 8.0) / px) as usize;
    let start = cols.saturating_sub(shown);
    let ink = theme::text();
    for c in start..cols {
        let x = r.left() + 4.0 + (c - start) as f32 * px;
        for y in 0..rows {
            let v = app.hell[c * rows + y];
            if v < 40 {
                continue;
            }
            let col = alpha(ink, v as f32 / 255.0);
            for copy in 0..2 {
                let top = r.top() + 4.0 + (copy * rows + y) as f32 * px;
                p.rect_filled(egui::Rect::from_min_size(pos2(x, top), vec2(px, px)), egui::CornerRadius::ZERO, col);
            }
        }
    }
}

/// WWV: the time the code says, which station is heard, and the minute's
/// bits as they arrive, with the position markers where the frame puts
/// them.
pub fn wwv(app: &mut App, ui: &mut Ui) {
    let r = reading(app);
    ui.horizontal_wrapped(|ui| {
        reading_strip(app, ui, Mode::Wwv, "LISTENING FOR THE TIME CODE");
    });
    ui.add_space(6.0);
    let last = rows(app, "WWV").find(|(_, d)| d.ev.kind == "minute").map(|(_, d)| (d.at, d.ev.fields.clone(), d.ev.summary.clone()));
    ui.horizontal_top(|ui| {
        // The time read from the air, and how long ago it was read.
        ui.vertical(|ui| {
            ui.set_width(250.0);
            match &last {
                Some((at, f, _)) => {
                    // "2026-11-06T17:52Z": the time large, the date under it.
                    let iso = f.get("utc").cloned().unwrap_or_default();
                    let (date, time) = iso.split_once('T').map(|(d, t)| (d.to_string(), t.trim_end_matches('Z').to_string())).unwrap_or((String::new(), "—".into()));
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(time).font(theme::mono_bold(26.0)).color(theme::text()));
                        ui.label(mono("UTC", theme::text_3()));
                    });
                    let ago = at.elapsed().map(|e| e.as_secs()).unwrap_or(0);
                    let day = f.get("dayOfYear").map(|d| format!(" · day {d}")).unwrap_or_default();
                    ui.label(mono(format!("{date}{day} · read {ago} s ago"), theme::text_3()));
                    ui.add_space(4.0);
                    let rows = [
                        ("station", "STATION", String::new()),
                        ("dut1", "DUT1", " s".to_string()),
                        ("dst", "DST", String::new()),
                        ("leapWarning", "LEAP SECOND", String::new()),
                        ("confidence", "CONFIDENCE", String::new()),
                        ("tickOffsetMs", "TICKS LATE BY", " ms".to_string()),
                    ];
                    for (k, label, unit) in rows {
                        if let Some(v) = f.get(k) {
                            let v = match k {
                                "leapWarning" => if v == "true" { "announced".to_string() } else { "none".to_string() },
                                _ => format!("{v}{unit}"),
                            };
                            let r = ui.horizontal(|ui| {
                                ui.add_sized(vec2(110.0, 14.0), egui::Label::new(mono(label, theme::text_3())));
                                ui.label(mono(v, theme::text_2()));
                            });
                            if k == "tickOffsetMs" {
                                r.response.on_hover_text("When the ticks arrived after the second on this server's clock: the path from Colorado or Kauai, the receiver's own delay, and the clock's error together");
                            }
                        }
                    }
                }
                None => {
                    ui.label(RichText::new("--:--").font(theme::mono_bold(26.0)).color(theme::text_3()));
                    ui.add(egui::Label::new(RichText::new("A minute of the code decodes after a full frame: up to two minutes from tuning. 5, 10 and 15 MHz are the strongest; WWV and WWVH share them.").font(theme::sans(11.5)).color(theme::text_3())).wrap());
                }
            }
        });
        ui.add_space(12.0);
        ui.vertical(|ui| {
            ui.label(mono("THIS MINUTE'S CODE", theme::text_3()));
            bit_strip(ui, &r.bits, ui.available_width().min(720.0));
        });
    });
}

/// Sixty cells, a second each, ten to a row: the width tells a 0 (short)
/// from a 1 (half) and a marker (long), as the pulses do on air.
fn bit_strip(ui: &mut Ui, bits: &[u8], w: f32) {
    let cell = (w / 10.0).floor().max(18.0);
    let h = 16.0;
    let (r, _) = ui.allocate_exact_size(vec2(cell * 10.0, 6.0 * (h + 4.0)), Sense::hover());
    let p = ui.painter_at(r);
    for s in 0..60usize {
        let (row, col) = (s / 10, s % 10);
        let x = r.left() + col as f32 * cell;
        let y = r.top() + row as f32 * (h + 4.0);
        let slot = egui::Rect::from_min_size(pos2(x + 1.0, y), vec2(cell - 2.0, h));
        p.rect_filled(slot, egui::CornerRadius::same(2), theme::ink());
        let (frac, c) = match bits.get(s).copied() {
            Some(0) => (0.2, theme::text_2()),
            Some(1) => (0.5, theme::AMBER),
            Some(2) => (0.8, theme::CYAN),
            _ => (0.0, theme::text_3()),
        };
        if frac > 0.0 {
            let pulse = egui::Rect::from_min_size(slot.min + vec2(1.0, 3.0), vec2((slot.width() - 2.0) * frac, h - 6.0));
            p.rect_filled(pulse, egui::CornerRadius::same(1), alpha(c, 0.9));
        }
        if col == 0 {
            p.text(pos2(slot.left() + 2.0, slot.bottom() + 0.5), egui::Align2::LEFT_TOP, format!("{s}"), theme::mono(7.5), theme::text_3());
        }
    }
}

/// DRM: the station's name and service, its text message, how far the
/// receiver has got (time, frequency, frame, FAC, SDC, MSC), and the MSC's
/// constellation.
pub fn drm(app: &mut App, ui: &mut Ui) {
    let c = app.status.digital_cfg.clone().unwrap_or_default();
    let on = app.status.mode == Mode::Drm;
    let tally = widgets::protocol_color("DRM");
    ui.horizontal_wrapped(|ui| {
        reading_strip(app, ui, Mode::Drm, "FINDING THE MULTIPLEX");
    });
    ui.add_space(4.0);
    let service = rows(app, "DRM").find(|(_, d)| d.ev.kind.eq_ignore_ascii_case("service")).map(|(_, d)| d.ev.fields.clone());
    let text = rows(app, "DRM").find(|(_, d)| d.ev.kind.eq_ignore_ascii_case("text")).map(|(_, d)| (d.at, d.ev.fields.get("text").cloned().unwrap_or_else(|| d.ev.summary.clone())));
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            match &service {
                Some(f) => {
                    let name = f.get("label").cloned().unwrap_or_else(|| "(no label sent)".into());
                    ui.label(RichText::new(name).font(theme::headline(20.0)).color(theme::text()));
                    // The SDC's ISO codes, by name where the tables have them.
                    let named = |k: &str, look: fn(&str) -> Option<&'static str>| f.get(k).filter(|v| !v.is_empty()).map(|v| look(v).map_or(v.clone(), str::to_string));
                    let line: Vec<String> = [f.get("codec").cloned(), f.get("audioMode").cloned(), named("languageCode", crate::lookups::language).or_else(|| f.get("language").cloned()), named("countryCode", crate::lookups::country), f.get("programType").cloned()]
                        .into_iter()
                        .flatten()
                        .filter(|v| !v.is_empty() && !v.starts_with("No "))
                        .collect();
                    ui.label(mono(line.join(" · "), theme::text_2()));
                }
                None => {
                    ui.label(RichText::new("No service yet").font(theme::headline(18.0)).color(theme::text_3()));
                }
            }
            if let Some((at, t)) = &text {
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(t).font(theme::sans(13.0)).color(theme::text())).wrap());
                ui.label(mono(format!("text at {}", local_hms(*at)), theme::text_3()));
            }
            ui.add_space(6.0);
            ui.add_enabled_ui(on, |ui| {
                ui.horizontal(|ui| {
                    ui.label(mono("SERVICE", theme::text_3()));
                    if let Some(i) = widgets::segmented(ui, "drm-service", &["1", "2", "3", "4"], Some(c.drm_service as usize), tally) {
                        app.post("/api/sdr/digital", json!({ "drm_service": i }));
                    }
                });
            });
        });
    });
    ui.add_space(4.0);
    let cols: &[(&str, &str, f32)] = &[];
    list_head(ui, cols);
    message_list(app, ui, "DRM", cols, "DRM's signal events, services and text messages appear here. DRM needs a clean signal: about 15 dB for 16-QAM, more for 64-QAM. STATIONS lists broadcasts aimed at or heard in North America.");
}
