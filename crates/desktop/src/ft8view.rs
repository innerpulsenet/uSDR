//! The FT8 and FT4 view: where the receiver is in the UTC slot, the band's
//! activity slot by slot across the audio passband, and the decodes as an
//! operator's log, with the distance and bearing of every station that
//! sent its locator, the calls answering the station's own callsign picked
//! out, and keys to show only CQs or only those.

use crate::app::{App, local_hms};
use crate::theme::{self, alpha};
use crate::widgets;
use egui::{RichText, Ui, vec2};
use usdr_proto::Mode;

fn mono(t: impl Into<String>, c: egui::Color32) -> RichText {
    RichText::new(t).font(theme::mono(11.0)).color(c)
}

/// One decode as the view reads it from the event's fields.
pub struct Row {
    pub i: usize,
    slot: String,
    pub slot_ms: u64,
    snr: Option<f32>,
    dt: Option<f32>,
    df: Option<f32>,
    cq: bool,
    /// Sent to the station's own callsign.
    mine: bool,
    /// Distance (km) and bearing (degrees) to the sender's locator.
    path: Option<(f64, f64)>,
    /// A sign-off: RR73, RRR or 73.
    closing: bool,
}

/// The receiving station's callsign and the center of its locator, once
/// Settings › Station has them.
fn home(app: &App) -> (Option<String>, Option<(f64, f64)>) {
    let Some((st, _)) = &app.station else { return (None, None) };
    let call = (!st.callsign.is_empty()).then(|| st.callsign.clone());
    (call, usdr_proto::station::grid_center(&st.grid))
}

fn row(app: &App, i: usize, call: Option<&str>, here: Option<(f64, f64)>) -> Row {
    let d = &app.decodes[i];
    let f = |k: &str| d.ev.fields.get(k).and_then(|v| v.trim().parse::<f32>().ok());
    let s = |k: &str| d.ev.fields.get(k).map(String::as_str);
    let slot_ms = s("slotMs").and_then(|v| v.parse::<u64>().ok()).unwrap_or_else(|| d.at.duration_since(std::time::SystemTime::UNIX_EPOCH).map(|t| t.as_millis() as u64).unwrap_or(0));
    let slot = s("slot").map(str::to_string).unwrap_or_else(|| local_hms(d.at).replace(':', ""));
    // The callsign as sent may carry a portable suffix or angle brackets
    // (a hashed call); compare the bare call.
    let bare = |c: &str| c.trim_matches(|ch| ch == '<' || ch == '>').split('/').max_by_key(|p| p.len()).unwrap_or("").to_string();
    let mine = call.is_some_and(|me| s("to").is_some_and(|to| bare(to) == bare(me)));
    let path = here.zip(s("grid").and_then(usdr_proto::station::grid_center)).map(|(a, b)| usdr_proto::station::path(a, b));
    let closing = matches!(s("extra"), Some("RR73" | "RRR" | "73"));
    Row { i, slot, slot_ms, snr: f("snr"), dt: f("dt"), df: f("df"), cq: s("cq") == Some("yes") || d.ev.summary.starts_with("CQ "), mine, path, closing }
}

/// Weak to strong: the color a decode's SNR is drawn in.
fn snr_color(snr: f32) -> egui::Color32 {
    let t = ((snr + 22.0) / 24.0).clamp(0.0, 1.0);
    if t < 0.5 { theme::lerp(theme::ERR, theme::AMBER, t * 2.0) } else { theme::lerp(theme::AMBER, theme::OK, (t - 0.5) * 2.0) }
}

fn km(v: f64) -> String {
    if v >= 10_000.0 { format!("{:.0}k", v / 1_000.0) } else { format!("{v:.0}") }
}

#[derive(Clone, Copy, Default)]
struct Filter {
    cq: bool,
    mine: bool,
}

fn cell(ui: &mut Ui, w: f32, t: RichText) {
    ui.allocate_ui_with_layout(vec2(w, 18.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_width(w);
        ui.add(egui::Label::new(t).truncate());
    });
}

/// A number column, right aligned so its digits line up.
fn cell_r(ui: &mut Ui, w: f32, t: RichText) {
    ui.allocate_ui_with_layout(vec2(w, 18.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.set_min_width(w);
        ui.add(egui::Label::new(t).truncate());
    });
}

const ROW_H: f32 = 20.0;
const CHAR_W: f32 = 6.7;

pub fn show(app: &mut App, ui: &mut Ui, visible: &[usize], empty: &str) {
    let (call, here) = home(app);
    let all: Vec<Row> = visible.iter().copied().filter(|&i| matches!(app.decodes[i].ev.protocol.as_str(), "FT8" | "FT4")).map(|i| row(app, i, call.as_deref(), here)).collect();
    let fid = egui::Id::new("ft8-filter");
    let mut filter: Filter = ui.data(|d| d.get_temp(fid)).unwrap_or_default();
    if call.is_none() {
        filter.mine = false;
    }
    clock(app, ui, &all, &mut filter, call.is_some());
    ui.data_mut(|d| d.insert_temp(fid, filter));
    let rows: Vec<&Row> = all.iter().filter(|r| (!filter.cq || r.cq) && (!filter.mine || r.mine)).collect();
    ui.add_space(4.0);
    activity(app, ui, &all);
    ui.add_space(4.0);
    let with_path = here.is_some();
    // A decode clicked (here or on the activity strip) opens its station
    // beside the log.
    let picked = app.selected_decode.filter(|i| all.iter().any(|r| r.i == *i));
    match picked {
        Some(i) => {
            let w = ui.available_width();
            let h = ui.available_height();
            let side = (w * 0.36).clamp(260.0, 380.0);
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(vec2(w - side - 8.0, h), egui::Layout::top_down(egui::Align::Min), |ui| table(app, ui, &all, &rows, filter, with_path, empty));
                ui.allocate_ui_with_layout(vec2(side, h), egui::Layout::top_down(egui::Align::Min), |ui| inspector(app, ui, &all, i));
            });
        }
        None => table(app, ui, &all, &rows, filter, with_path, empty),
    }
}

/// The log as a table: a decode a row, a rule where each slot begins.
fn table(app: &mut App, ui: &mut Ui, all: &[Row], rows: &[&Row], filter: Filter, with_path: bool, empty: &str) {
    // The mode column only while both modes are in the log.
    let both = all.iter().any(|r| app.decodes[r.i].ev.protocol == "FT4") && all.iter().any(|r| app.decodes[r.i].ev.protocol == "FT8");
    let mut cols: Vec<(&str, f32)> = vec![("UTC", 9.0), ("dB", 4.5), ("DT", 5.0), ("FREQ", 6.0)];
    if both {
        cols.push(("MODE", 5.0));
    }
    cols.push(("TYPE", 9.0));
    let tail: Vec<(&str, f32)> = if with_path { vec![("KM", 6.5), ("AZ", 4.5)] } else { vec![] };
    // One message width for the header and every row: what is left of the
    // view after the fixed columns and the gaps between all the cells.
    let gap = ui.spacing().item_spacing.x;
    let fixed: f32 = cols.iter().chain(tail.iter()).map(|(_, w)| w * CHAR_W).sum();
    let msg_w = (ui.available_width() - fixed - gap * (cols.len() + tail.len()) as f32 - 6.0).max(80.0);
    let head = |t: &str| RichText::new(t).font(theme::mono(10.0)).color(theme::text_3()).extra_letter_spacing(0.6);
    let r = ui.horizontal(|ui| {
        ui.set_min_height(18.0);
        for (t, w) in &cols {
            cell(ui, w * CHAR_W, head(t));
        }
        cell(ui, msg_w, head(if filter.cq { "MESSAGE · CQ ONLY" } else if filter.mine { "MESSAGE · TO YOU" } else { "MESSAGE" }));
        for (t, w) in &tail {
            cell_r(ui, w * CHAR_W, head(t));
        }
    });
    let y = r.response.rect.bottom() + 1.0;
    ui.painter().line_segment([egui::pos2(r.response.rect.left(), y), egui::pos2(r.response.rect.right(), y)], egui::Stroke::new(1.0, theme::line()));
    if rows.is_empty() {
        let text = if all.is_empty() { empty } else if filter.mine { "Nothing sent to your callsign in the log yet." } else { "No CQs in the log yet." };
        ui.add_space(6.0);
        ui.add(egui::Label::new(RichText::new(text).font(theme::sans(12.0)).color(theme::text_3())).wrap());
        return;
    }
    let mut select = None;
    egui::ScrollArea::vertical().id_salt("hf-decodes").auto_shrink([false, false]).min_scrolled_height(0.0).show_rows(ui, ROW_H, rows.len(), |ui, range| {
        // Rows exactly ROW_H apart, as show_rows lays them out.
        ui.spacing_mut().item_spacing.y = 0.0;
        for k in range {
            let row = rows[k];
            let d = &app.decodes[row.i];
            // A rule where a slot begins, so each slot's decodes read as one.
            let new_slot = k == 0 || rows[k - 1].slot_ms != row.slot_ms;
            let resp = ui
                .horizontal(|ui| {
                    ui.set_min_height(ROW_H);
                    let slot_text = if new_slot && row.slot.len() >= 6 { format!("{}:{}:{}", &row.slot[..2], &row.slot[2..4], &row.slot[4..6]) } else { String::new() };
                    cell(ui, 9.0 * CHAR_W, mono(slot_text, theme::text_3()));
                    cell(ui, 4.5 * CHAR_W, mono(row.snr.map_or("—".into(), |v| format!("{v:+.0}")), row.snr.map_or(theme::text_3(), snr_color)));
                    cell(ui, 5.0 * CHAR_W, mono(row.dt.map_or("—".into(), |v| format!("{v:+.1}")), theme::text_2()));
                    cell(ui, 6.0 * CHAR_W, mono(row.df.map_or("—".into(), |v| format!("{v:.0}")), theme::AMBER));
                    if both {
                        cell(ui, 5.0 * CHAR_W, mono(&d.ev.protocol, widgets::protocol_color(&d.ev.protocol)));
                    }
                    let (badge, bc) = kind_of(&d.ev);
                    ui.allocate_ui_with_layout(vec2(9.0 * CHAR_W, 18.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.set_min_width(9.0 * CHAR_W);
                        if !badge.is_empty() {
                            badge_label(ui, &badge, bc);
                        }
                    });
                    let msg = RichText::new(&d.ev.summary).font(theme::mono(11.5));
                    let msg = if row.mine {
                        msg.color(theme::AMBER).strong()
                    } else if row.cq {
                        msg.color(theme::OK).strong()
                    } else if row.closing {
                        msg.color(theme::text_2())
                    } else {
                        msg.color(theme::text())
                    };
                    cell(ui, msg_w, msg);
                    if with_path {
                        let (k, a) = row.path.map_or(("".into(), "".into()), |(k, a)| (km(k), format!("{a:.0}°")));
                        cell_r(ui, 6.5 * CHAR_W, mono(k, theme::text_2()));
                        cell_r(ui, 4.5 * CHAR_W, mono(a, theme::text_3()));
                    }
                })
                .response
                .interact(egui::Sense::click());
            if new_slot && k > 0 {
                let y = resp.rect.top();
                ui.painter().line_segment([egui::pos2(resp.rect.left(), y), egui::pos2(resp.rect.right(), y)], egui::Stroke::new(1.0, theme::line()));
            }
            if row.mine {
                ui.painter().rect_filled(resp.rect, egui::CornerRadius::same(3), alpha(theme::AMBER, 0.07));
            }
            if app.selected_decode == Some(row.i) {
                ui.painter().rect_filled(resp.rect, egui::CornerRadius::same(3), alpha(theme::AMBER, 0.1));
            }
            let resp = resp.on_hover_ui(|ui| tip(ui, app, row));
            if resp.clicked() {
                select = Some(if app.selected_decode == Some(row.i) { None } else { Some(row.i) });
            }
        }
    });
    if let Some(s) = select {
        app.selected_decode = s;
    }
}

/// What a message is, as a short badge and its color: a CQ (and what kind,
/// "CQ POTA", "CQ DX"), a locator answering one, a signal report, a roger
/// report, or a sign-off.
fn kind_of(ev: &usdr_proto::DecodeEvent) -> (String, egui::Color32) {
    let words: Vec<&str> = ev.summary.split_whitespace().collect();
    let f = |k: &str| ev.fields.get(k).map(String::as_str);
    if words.first() == Some(&"CQ") {
        // "CQ POTA K1ABC FN42": a short word between CQ and the call says
        // what it is calling for.
        let what = words.get(1).filter(|w| words.len() >= 4 && w.len() <= 4 && w.chars().all(|c| c.is_ascii_alphabetic()));
        return (what.map_or("CQ".to_string(), |w| format!("CQ {w}")), theme::OK);
    }
    match f("extra").or(words.get(2).copied()) {
        Some("RR73" | "RRR" | "73") => ("73".into(), theme::VIOLET),
        Some(x) if x.starts_with("R+") || x.starts_with("R-") => ("R-REPORT".into(), theme::AMBER),
        Some(x) if (x.starts_with('+') || x.starts_with('-')) && x[1..].chars().all(|c| c.is_ascii_digit()) => ("REPORT".into(), theme::AMBER),
        _ if f("grid").is_some() => ("GRID".into(), theme::CYAN),
        _ => (String::new(), theme::text_3()),
    }
}

fn badge_label(ui: &mut Ui, text: &str, c: egui::Color32) {
    let g = ui.painter().layout_no_wrap(text.to_string(), theme::mono_bold(9.5), c);
    let (r, _) = ui.allocate_exact_size(g.size() + vec2(8.0, 4.0), egui::Sense::hover());
    ui.painter().rect(r, egui::CornerRadius::same(3), alpha(c, 0.1), egui::Stroke::new(1.0, alpha(c, 0.55)), egui::StrokeKind::Inside);
    ui.painter().galley(r.min + vec2(4.0, 2.0), g, c);
}

/// The station of a clicked decode: who it is, where, how it was heard,
/// and every decode in the log with its call, oldest first, so both sides
/// of a contact read as one exchange.
fn inspector(app: &mut App, ui: &mut Ui, all: &[Row], i: usize) {
    let d = app.decodes[i].ev.clone();
    let f = |k: &str| d.fields.get(k).cloned();
    // The station is the sender; a CQ's or a reply's "de".
    let call = f("de").or_else(|| d.summary.split_whitespace().nth(1).map(str::to_string)).unwrap_or_default();
    let bare = call.trim_matches(|c| c == '<' || c == '>').to_string();
    let row = all.iter().find(|r| r.i == i);
    egui::Frame::new().fill(alpha(theme::ink(), 0.6)).stroke(egui::Stroke::new(1.0, theme::line())).corner_radius(egui::CornerRadius::same(4)).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height(ui.available_height());
        ui.horizontal(|ui| {
            ui.label(RichText::new(&bare).font(theme::mono_bold(16.0)).color(theme::text()));
            let (b, c) = kind_of(&d);
            if !b.is_empty() {
                badge_label(ui, &b, c);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::action_small(ui, "CLOSE").clicked() {
                    app.selected_decode = None;
                }
            });
        });
        // The station's locator: from this decode, or the last it sent.
        let grid = f("grid").or_else(|| app.decodes.iter().filter(|x| x.ev.fields.get("de").is_some_and(|v| v.trim_matches(|c| c == '<' || c == '>') == bare)).find_map(|x| x.ev.fields.get("grid").cloned()));
        if let Some(g) = &grid {
            let path = home(app).1.zip(usdr_proto::station::grid_center(g)).map(|(a, b)| usdr_proto::station::path(a, b));
            let line = match path {
                Some((k, az)) => format!("{g} · {} km at {az:.0}°", km(k)),
                None => g.clone(),
            };
            ui.label(mono(line, theme::CYAN));
        }
        if let Some(r) = row {
            ui.label(mono(format!("{} Hz · {} dB · {} s", r.df.map_or("—".into(), |v| format!("{v:.0}")), r.snr.map_or("—".into(), |v| format!("{v:+.0}")), r.dt.map_or("—".into(), |v| format!("{v:+.1}"))), theme::text_2()));
        }
        ui.add_space(6.0);
        ui.label(mono(format!("IN THE LOG WITH {bare}"), theme::text_3()));
        let mut hist: Vec<&crate::app::DecodeRow> = app
            .decodes
            .iter()
            .filter(|x| matches!(x.ev.protocol.as_str(), "FT8" | "FT4") && x.ev.summary.split_whitespace().any(|w| w.trim_matches(|c| c == '<' || c == '>') == bare))
            .take(60)
            .collect();
        hist.reverse();
        egui::ScrollArea::vertical().id_salt("ft8-inspector").auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            for x in hist {
                ui.horizontal(|ui| {
                    let slot = x.ev.fields.get("slot").map(|s| if s.len() >= 6 { format!("{}:{}:{}", &s[..2], &s[2..4], &s[4..6]) } else { s.clone() }).unwrap_or_else(|| local_hms(x.at));
                    ui.label(mono(slot, theme::text_3()));
                    ui.label(mono(x.ev.fields.get("snr").map(|v| format!("{v:>3}")).unwrap_or_default(), theme::text_2()));
                    let c = if x.ev.summary.starts_with(&format!("{bare} ")) || x.ev.summary.starts_with(&format!("<{bare}> ")) { theme::AMBER } else { theme::text() };
                    ui.add(egui::Label::new(mono(&x.ev.summary, c)).truncate());
                });
            }
        });
    });
}

/// A decode's details: who to whom, from where, and the decoder's numbers.
fn tip(ui: &mut Ui, app: &App, row: &Row) {
    let d = &app.decodes[row.i];
    let f = |k: &str| d.ev.fields.get(k).cloned();
    ui.label(RichText::new(&d.ev.summary).font(theme::mono_bold(12.0)).color(theme::text()));
    egui::Grid::new(("hf-fields", row.i)).num_columns(2).spacing(vec2(10.0, 1.0)).show(ui, |ui| {
        let mut line = |k: &str, v: String| {
            ui.label(mono(k, theme::text_3()));
            ui.label(mono(v, theme::text()));
            ui.end_row();
        };
        if let Some(de) = f("de") {
            line("from", de);
        }
        if let Some(to) = f("to") {
            line("to", to);
        }
        if let Some(g) = f("grid") {
            line("locator", g);
        }
        if let Some((k, a)) = row.path {
            line("path", format!("{} km at {a:.0}°", km(k)));
        }
        if let (Some(s), Some(dt), Some(df)) = (f("snr"), f("dt"), f("df")) {
            line("heard", format!("{s} dB · {dt} s · {df} Hz"));
        }
        if let Some(e) = f("ldpcErrors") {
            line("ldpc", format!("{e} bits corrected"));
        }
    });
}

/// Where the receiver is in the UTC slot, what the newest slot held, and
/// the keys that narrow the log.
fn clock(app: &App, ui: &mut Ui, rows: &[Row], filter: &mut Filter, has_call: bool) {
    let slot = match app.status.mode {
        Mode::Ft8 => 15.0,
        Mode::Ft4 => 7.5,
        _ => {
            ui.label(mono("Not decoding: pick FT8 or FT4 under HF.", theme::text_3()));
            return;
        }
    };
    let now = std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let into = now.rem_euclid(slot);
    let start = now - into;
    let secs = start as u64;
    // Stations take turns: the first and second slot of each pair.
    let even = (start / slot).round() as u64 % 2 == 0;
    let tally = widgets::protocol_color(app.status.mode.label());
    ui.horizontal(|ui| {
        ui.label(mono(format!("{} · {:02}:{:02}:{:04.1}", app.status.mode.label(), (secs / 3600) % 24, (secs / 60) % 60, start.rem_euclid(60.0)), theme::text_2()));
        ui.label(mono(if even { "1ST" } else { "2ND" }, theme::text_3())).on_hover_text("Stations take turns: one sends in the first slot of each pair, the other answers in the second");
        let w = (ui.available_width() * 0.32).clamp(80.0, 320.0);
        let (r, _) = ui.allocate_exact_size(vec2(w, 6.0), egui::Sense::hover());
        let p = ui.painter();
        p.rect_filled(r, egui::CornerRadius::same(3), theme::ink());
        let done = egui::Rect::from_min_size(r.min, vec2(r.width() * (into / slot) as f32, r.height()));
        p.rect_filled(done, egui::CornerRadius::same(3), alpha(tally, 0.8));
        ui.label(mono(format!("decode in {:.1} s", slot - into), theme::text_3()));
        // What the newest slot held: stations, and the farthest one.
        if let Some(newest) = rows.first().map(|r| r.slot_ms) {
            let slot_rows: Vec<&Row> = rows.iter().take_while(|r| r.slot_ms == newest).collect();
            let far = slot_rows.iter().filter_map(|r| r.path.map(|p| p.0)).fold(None, |a: Option<f64>, k| Some(a.map_or(k, |a| a.max(k))));
            let text = match far {
                Some(k) => format!("{} heard · farthest {} km", slot_rows.len(), km(k)),
                None => format!("{} heard", slot_rows.len()),
            };
            ui.label(mono(text, theme::text_2())).on_hover_text("In the newest slot");
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if has_call && widgets::chip_key(ui, "TO YOU", filter.mine, theme::AMBER).on_hover_text("Only the calls sent to your callsign (Settings › Station)").clicked() {
                filter.mine = !filter.mine;
                filter.cq &= !filter.mine;
            }
            if widgets::chip_key(ui, "CQ", filter.cq, theme::OK).on_hover_text("Only stations calling CQ").clicked() {
                filter.cq = !filter.cq;
                filter.mine &= !filter.cq;
            }
        });
    });
    // The bar moves on its own; a few frames a second is plenty.
    crate::anim::after(ui.ctx(), std::time::Duration::from_millis(200));
}

/// The band's activity: the last few slots, newest on top, each decode a
/// tile at its audio frequency colored by its SNR. Hover for the message,
/// click to find it in the log. A short dock keeps four slots, so the log
/// under it keeps rows.
fn activity(app: &mut App, ui: &mut Ui, rows: &[Row]) {
    let mut slots: Vec<u64> = rows.iter().map(|r| r.slot_ms).collect();
    slots.dedup();
    let room = ui.available_height();
    // A dock too short for the strip and rows of the log keeps the log.
    if room < 150.0 {
        return;
    }
    let lanes = if room < 260.0 { 4usize } else { 8 };
    slots.truncate(lanes);
    let (axis_h, max_hz) = (13.0f32, if app.status.filter_high_hz.abs() > 3_200.0 { app.status.filter_high_hz.abs() as f32 + 100.0 } else { 3_200.0 });
    let lane_h = ((room * 0.3 - axis_h) / lanes as f32).clamp(5.0, 9.0);
    let w = ui.available_width();
    let (outer, _) = ui.allocate_exact_size(vec2(w, lanes as f32 * lane_h + axis_h + 2.0), egui::Sense::hover());
    let plot = egui::Rect::from_min_max(egui::pos2(outer.left() + 44.0, outer.top()), egui::pos2(outer.right() - 4.0, outer.bottom() - axis_h));
    let p = ui.painter().clone();
    p.rect(plot, egui::CornerRadius::same(3), theme::ink(), egui::Stroke::new(1.0, theme::line()), egui::StrokeKind::Inside);
    let x_of = |hz: f32| plot.left() + hz / max_hz * plot.width();
    for k in 1..(max_hz / 500.0) as usize + 1 {
        let hz = k as f32 * 500.0;
        let x = x_of(hz);
        if x < plot.right() - 1.0 {
            p.line_segment([egui::pos2(x, plot.top() + 1.0), egui::pos2(x, plot.bottom() - 1.0)], egui::Stroke::new(1.0, theme::line()));
        }
        let align = if x > plot.right() - 16.0 { egui::Align2::RIGHT_TOP } else { egui::Align2::CENTER_TOP };
        p.text(egui::pos2(x.min(plot.right()), plot.bottom() + 1.0), align, format!("{:.1}k", hz / 1000.0), theme::mono(10.0), theme::text_3());
    }
    p.text(egui::pos2(plot.left() - 4.0, plot.top()), egui::Align2::RIGHT_TOP, "newest", theme::mono(10.0), theme::text_3());
    if plot.height() >= 30.0 {
        p.text(egui::pos2(plot.left() - 4.0, plot.bottom()), egui::Align2::RIGHT_BOTTOM, format!("{} slots", slots.len()), theme::mono(10.0), theme::text_3());
    }
    // The filter's edges, where the decoder can hear.
    let (flo, fhi) = (app.status.filter_low_hz.abs().min(app.status.filter_high_hz.abs()) as f32, app.status.filter_low_hz.abs().max(app.status.filter_high_hz.abs()) as f32);
    if matches!(app.status.mode, Mode::Ft8 | Mode::Ft4) && fhi > flo {
        for hz in [flo, fhi] {
            crate::spectrum::dashed_v(&p, x_of(hz), plot.top() + 1.0, plot.bottom() - 1.0, alpha(theme::AMBER, 0.4));
        }
    }
    let hover = ui.input(|i| i.pointer.hover_pos()).filter(|h| plot.contains(*h));
    let mut tip: Option<usize> = None;
    let clicked = ui.input(|i| i.pointer.primary_clicked());
    for r in rows {
        let Some(lane) = slots.iter().position(|s| *s == r.slot_ms) else { continue };
        let Some(df) = r.df else { continue };
        // FT8 spans 50 Hz of audio, FT4 83.
        let width_hz = if app.decodes[r.i].ev.protocol.contains("FT4") { 83.3 } else { 50.0 };
        let y = plot.top() + 1.0 + lane as f32 * lane_h;
        let tile = egui::Rect::from_min_max(egui::pos2(x_of(df), y + 1.0), egui::pos2(x_of(df + width_hz).max(x_of(df) + 3.0), y + lane_h - 1.0));
        let c = r.snr.map_or(theme::text_3(), snr_color);
        let sel = app.selected_decode == Some(r.i);
        p.rect_filled(tile, egui::CornerRadius::same(2), alpha(c, if lane == 0 { 0.95 } else { 0.75 - lane as f32 * 0.06 }));
        if r.cq || r.mine || sel {
            let ring = if sel { theme::text() } else if r.mine { theme::AMBER } else { theme::OK };
            p.rect_stroke(tile.expand(1.0), egui::CornerRadius::same(2), egui::Stroke::new(1.0, ring), egui::StrokeKind::Outside);
        }
        if hover.is_some_and(|h| tile.expand(1.5).contains(h)) {
            tip = Some(r.i);
        }
    }
    if let Some(i) = tip {
        let d = &app.decodes[i];
        let text = format!("{}  {}", d.ev.summary, d.ev.fields.get("snr").map(|s| format!("{s} dB")).unwrap_or_default());
        egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), egui::Id::new("hf-tile"), egui::PopupAnchor::Pointer).show(|ui| ui.add(egui::Label::new(mono(text, theme::text())).extend()));
        if clicked {
            app.selected_decode = Some(i);
        }
    }
}
