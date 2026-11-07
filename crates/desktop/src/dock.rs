//! The dock under the display: the decode log with its inspector, the
//! classifier log, the calls heard, and the voice scanner.

use crate::app::{App, DockTab, Playing, local_hms};
use crate::net::Fetch;
use crate::theme::{self, alpha};
use crate::widgets::{self, Tone};
use egui::{RichText, Ui, vec2};
use serde_json::json;
use crate::decoders::Family;
use usdr_proto::Mode;

fn mono(t: impl Into<String>, c: egui::Color32) -> RichText {
    RichText::new(t).font(theme::mono(11.0)).color(c)
}

pub fn dock(app: &mut App, ui: &mut Ui) {
    // The scope: always in view beside the logs.
    if app.prefs.show_scope {
        // The logs keep at least 390 px beside the scope, so their four tabs
        // and their counts never run under its title.
        let scope_max = (ui.available_width() - 390.0).clamp(240.0, 800.0);
        let resp = egui::Panel::right("dock-scope")
            .resizable(true)
            .default_size(app.prefs.scope_width.min(scope_max))
            .size_range(240.0..=scope_max)
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 10, right: 0, top: 0, bottom: 0 }))
            .show(ui, |ui| scope(app, ui));
        app.prefs.scope_width = resp.response.rect.width();
    }
    let row = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let tabs = [
            (DockTab::Decodes, "Decodes", Some(app.decodes.len())),
            (DockTab::Classifier, "Classifier", Some(app.class_log.len())),
            (DockTab::Calls, "Last heard", Some(app.calls.len())),
            (DockTab::Scan, "Scan", None),
        ];
        let mut under: Option<egui::Rect> = None;
        for (t, label, count) in tabs {
            let on = app.dock_tab == t;
            let r = tab(ui, label, count, on);
            if on {
                under = Some(r.rect);
            }
            if r.clicked() {
                app.dock_tab = t;
            }
        }
        // The open log's keys at the row's far end, where the tabs leave
        // room: a row of the dock's height given back to the log.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            match app.dock_tab {
                // Below the menu's own width the keys ride the family row.
                DockTab::Decodes => {
                    let room = ui.available_width() - 12.0;
                    app.decode_keys_below = room < 56.0;
                    if !app.decode_keys_below {
                        decode_keys(app, ui, room);
                    }
                }
                DockTab::Classifier => {
                    if widgets::action_small(ui, "CLEAR").clicked() {
                        app.class_log.clear();
                    }
                }
                _ => {}
            }
        });
        // The amber underline slides to the tab that was chosen.
        if let Some(rr) = under {
            let ctx = ui.ctx().clone();
            let rm = app.reduce_motion();
            let ease = |id: &str, v: f32| if rm { v } else { ctx.animate_value_with_time(egui::Id::new(("dock-tab", id)), v, 0.15) };
            let (l, r) = (ease("l", rr.left() + 4.0), ease("r", rr.right() - 4.0));
            ui.painter().rect_filled(egui::Rect::from_min_max(egui::pos2(l, rr.bottom() - 1.0), egui::pos2(r, rr.bottom() + 1.0)), egui::CornerRadius::same(1), theme::AMBER);
        }
    });
    // A rule the width of the logs under the tabs, the underline riding on it.
    let y = row.response.rect.bottom();
    let x1 = ui.max_rect().right();
    let p = ui.painter();
    p.line_segment([egui::pos2(row.response.rect.left(), y), egui::pos2(x1, y)], egui::Stroke::new(1.0, theme::line()));
    ui.add_space(6.0);
    match app.dock_tab {
        DockTab::Decodes => decodes(app, ui),
        DockTab::Classifier => classifier(app, ui),
        DockTab::Calls => calls(app, ui),
        DockTab::Scan => scan(app, ui),
    }
}

/// A dock tab: its name and, when it keeps a log, a pill with the count.
fn tab(ui: &mut Ui, label: &str, count: Option<usize>, on: bool) -> egui::Response {
    let p = ui.painter().clone();
    let g = p.layout_no_wrap(label.to_string(), theme::sans_semibold(12.0), egui::Color32::PLACEHOLDER);
    let badge = count.map(|n| p.layout_no_wrap(n.to_string(), theme::mono_bold(10.0), egui::Color32::PLACEHOLDER));
    // The pill holds three digits whatever the count, so a log growing past
    // 9 or 99 does not move the tabs after it.
    let pill_w = |b: &std::sync::Arc<egui::Galley>| b.size().x.max(p.layout_no_wrap("999".into(), theme::mono_bold(10.0), egui::Color32::PLACEHOLDER).size().x) + 10.0;
    let bw = badge.as_ref().map_or(0.0, |b| pill_w(b) + 6.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + bw + 16.0, 26.0), egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, label));
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.12);
    let c = if on { theme::text() } else { theme::lerp(theme::text_2(), theme::text(), hover) };
    let y = rect.center().y - 1.0;
    p.galley(egui::pos2(rect.left() + 8.0, y - g.size().y / 2.0), g.clone(), c);
    if let (Some(b), Some(n)) = (badge, count) {
        let pill = egui::Rect::from_min_size(egui::pos2(rect.left() + 8.0 + g.size().x + 6.0, y - 8.0), vec2(pill_w(&b), 16.0));
        let (fill, text) = if n > 0 && on { (alpha(theme::AMBER, 0.22), theme::AMBER) } else if n > 0 { (theme::surface_3(), theme::text_2()) } else { (theme::surface_2(), theme::text_3()) };
        p.rect(pill, egui::CornerRadius::same(8), fill, egui::Stroke::new(1.0, if n > 0 && on { alpha(theme::AMBER, 0.5) } else { theme::line() }), egui::StrokeKind::Inside);
        p.galley(pill.center() - b.size() / 2.0, b, text);
    }
    resp
}

/// A fixed-width column of a log row, so the columns after it line up.
fn cell(ui: &mut Ui, w: f32, t: RichText, sense: egui::Sense) -> egui::Response {
    ui.allocate_ui_with_layout(vec2(w, 18.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_width(w);
        ui.add(egui::Label::new(t).truncate().sense(sense))
    })
    .inner
}

fn decodes(app: &mut App, ui: &mut Ui) {
    // The families, each with what it holds, then the chosen family's own
    // decoders: one row, never wrapped, scrolled sideways where the dock is
    // too narrow for it, so the log below keeps the height it was given.
    let row = egui::ScrollArea::horizontal().id_salt("families").scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden).auto_shrink([false, true]).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            if app.decode_keys_below {
                decode_keys(app, ui, 0.0);
            }
            let mut counts = std::collections::HashMap::new();
            for d in &app.decodes {
                *counts.entry(d.family).or_insert(0usize) += 1;
            }
            // In HF data the families fold into one menu, so the row has
            // room for the mode's own keys and bands.
            if app.decode_family == Some(Family::HfData) {
                let n = counts.get(&Some(Family::HfData)).copied().unwrap_or(0);
                let label = if n > 0 { format!("HF DATA {n}") } else { "HF DATA".to_string() };
                widgets::menu_chip_lit(ui, &label, &[], family_color(Family::HfData), true, |ui| {
                    if ui.button(format!("All decodes · {}", app.decodes.len())).clicked() {
                        app.decode_family = None;
                        app.decode_chip = None;
                        ui.close();
                    }
                    for f in Family::ALL.into_iter().filter(|f| *f != Family::HfData) {
                        let n = counts.get(&Some(f)).copied().unwrap_or(0);
                        if ui.button(format!("{} · {n}", f.label())).clicked() {
                            app.decode_family = Some(f);
                            app.decode_chip = None;
                            app.selected_decode = None;
                            ui.close();
                        }
                    }
                });
            } else {
            if widgets::chip_key(ui, &format!("ALL {}", app.decodes.len()), app.decode_family.is_none(), theme::AMBER).clicked() {
                app.decode_family = None;
                app.decode_chip = None;
            }
            for f in Family::ALL {
                let n = counts.get(&Some(f)).copied().unwrap_or(0);
                let label = if n > 0 { format!("{} {n}", f.label()) } else { f.label().to_string() };
                if widgets::chip_key(ui, &label, app.decode_family == Some(f), family_color(f)).clicked() {
                    app.decode_family = Some(f);
                    app.decode_chip = None;
                    app.selected_decode = None;
                }
            }
            }
            if let Some(f) = app.decode_family {
                // HF data has a decoder for every mode under the HF key; its
                // row keeps those in the log, the mode's own and the one
                // chosen, so it stays one short row.
                let current = app.status.mode.label();
                let list: Vec<&crate::decoders::Decoder> = crate::decoders::in_family(f)
                    .filter(|d| f != Family::HfData || d.chip == current || app.decode_chip == Some(d.chip) || app.decodes.iter().any(|r| r.family == Some(f) && d.takes(&r.ev.protocol.to_ascii_uppercase())))
                    .collect();
                if list.len() > 1 || f == Family::HfData {
                    // A rule between the families and this one's decoders.
                    let (r, _) = ui.allocate_exact_size(vec2(9.0, 18.0), egui::Sense::hover());
                    ui.painter().line_segment([r.center_top(), r.center_bottom()], egui::Stroke::new(1.0, theme::line()));
                }
                if f == Family::HfData {
                    // The receiver's mode has a workspace of its own; ALL and
                    // the protocol keys filter the plain log, whatever the
                    // receiver is on.
                    let mode = app.status.mode;
                    if mode.is_hf_data() {
                        let lit = app.decode_chip.is_none();
                        if widgets::chip_key(ui, &format!("{} ▸", mode.label()), lit, widgets::protocol_color(mode.label())).on_hover_text("The mode's own view, with its controls and stations").clicked() {
                            app.decode_chip = None;
                            app.selected_decode = None;
                        }
                    }
                    let all = app.decode_chip == Some(ALL_HF) || (app.decode_chip.is_none() && !mode.is_hf_data());
                    if widgets::chip_key(ui, "ALL", all, family_color(f)).on_hover_text("Every HF decode in one log, each opened beside it").clicked() {
                        app.decode_chip = Some(ALL_HF);
                    }
                    // The mode's own decodes are its workspace; the other
                    // protocols in the log, a key each.
                    for d in list.into_iter().filter(|d| !(mode.is_hf_data() && d.chip == mode.label())) {
                        if widgets::chip_key(ui, d.chip, app.decode_chip == Some(d.chip), widgets::protocol_color(d.chip)).on_hover_text(format!("{} decodes in the log", d.chip)).clicked() {
                            app.decode_chip = if app.decode_chip == Some(d.chip) { Some(ALL_HF) } else { Some(d.chip) };
                        }
                    }
                    // Where to tune for the receiver's mode.
                    crate::tunestrip::strip(app, ui);
                } else if list.len() > 1 {
                    if widgets::chip_key(ui, "ANY", app.decode_chip.is_none(), family_color(f)).clicked() {
                        app.decode_chip = None;
                    }
                    for d in list {
                        if widgets::chip_key(ui, d.chip, app.decode_chip == Some(d.chip), widgets::protocol_color(d.chip)).clicked() {
                            app.decode_chip = if app.decode_chip == Some(d.chip) { None } else { Some(d.chip) };
                        }
                    }
                }
            }
        });
    });
    // More to the right than the dock shows: the row fades out at its edge.
    let (shown, whole) = (row.inner_rect, row.content_size.x);
    if whole - row.state.offset.x > shown.width() + 1.0 {
        let edge = egui::Rect::from_min_max(egui::pos2(shown.right() - 28.0, shown.top()), shown.max);
        let mut mesh = egui::Mesh::default();
        let (clear, solid) = (alpha(theme::surface_1(), 0.0), theme::surface_1());
        mesh.colored_vertex(edge.left_top(), clear);
        mesh.colored_vertex(edge.right_top(), solid);
        mesh.colored_vertex(edge.right_bottom(), solid);
        mesh.colored_vertex(edge.left_bottom(), clear);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        ui.painter().add(egui::Shape::mesh(mesh));
    }
    ui.add_space(2.0);
    let filter = app.decode_filter.to_lowercase();
    let chip = app.decode_chip.and_then(|c| crate::decoders::DECODERS.iter().find(|d| d.chip == c));
    let family = app.decode_family;
    let visible: Vec<usize> = app
        .decodes
        .iter()
        .enumerate()
        .filter(|(_, d)| family.is_none() || d.family == family)
        .filter(|(_, d)| chip.is_none_or(|c| c.takes(&d.ev.protocol.to_ascii_uppercase())))
        .filter(|(_, d)| filter.is_empty() || d.key.contains(&filter))
        .map(|(i, _)| i)
        .collect();
    // HF data with no key chosen is the receiver's mode's workspace; any
    // key is the log.
    if family == Some(Family::HfData) && app.decode_chip.is_none() && app.status.mode.is_hf_data() {
        hf_data(app, ui, &visible);
        return;
    }
    let total = app.decodes.len();
    let avail = ui.available_size();
    let list_w = if app.selected_decode.is_some() { avail.x * 0.58 } else { avail.x };
    let mut tune_to = None;
    let mut show_all = false;
    ui.horizontal_top(|ui| {
        // Top-down: inside the row above, a plain allocation would lay the
        // frames out side by side.
        ui.allocate_ui_with_layout(vec2(list_w, avail.y), egui::Layout::top_down(egui::Align::Min), |ui| {
            // Fixed columns, sized to the longest protocol and kind in view.
            let char_w = 6.7;
            let proto_w = visible.iter().map(|&i| app.decodes[i].ev.protocol.chars().count() + 3).max().unwrap_or(6).clamp(6, 11) as f32 * char_w;
            let kind_w = visible.iter().map(|&i| app.decodes[i].ev.kind.chars().count()).max().unwrap_or(4).clamp(4, 16) as f32 * char_w;
            let frames = if total == 1 { "frame" } else { "frames" };
            let count = if visible.len() == total { format!("{total} {frames}") } else { format!("{} of {total} {frames}", visible.len()) };
            let family_empty = family.is_some_and(|f| !app.decodes.iter().any(|d| d.family == Some(f)));
            {
                // Column heads over the columns, the count at the end: there
                // from the start, so the first frame does not push the list
                // down a row.
                let head = |t: &str| RichText::new(t).font(theme::mono(10.0)).color(theme::text_3()).extra_letter_spacing(0.6);
                let r = ui.horizontal(|ui| {
                    ui.set_min_height(18.0);
                    ui.label(mono("▸", egui::Color32::TRANSPARENT));
                    cell(ui, 8.0 * char_w, head("TIME"), egui::Sense::hover());
                    cell(ui, 9.0 * char_w, head("FREQ MHz"), egui::Sense::hover());
                    cell(ui, proto_w, head("PROTO"), egui::Sense::hover());
                    cell(ui, kind_w, head("KIND"), egui::Sense::hover());
                    ui.label(head("SUMMARY"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.label(mono(count, theme::text_3())));
                });
                let y = r.response.rect.bottom() + 1.0;
                ui.painter().line_segment([egui::pos2(r.response.rect.left(), y), egui::pos2(r.response.rect.right(), y)], egui::Stroke::new(1.0, theme::line()));
            }
            // Said inside the list, so a short dock scrolls it rather than
            // growing past its panel.
            let empty = if family_empty { family.map(|f| f.empty_text()) } else { (total == 0).then_some(match app.status.mode {
                Mode::Auto => "Frames decoded on receiver A appear here, newest first. AUTO is running every decoder on this channel.",
                Mode::P25 | Mode::Dmr | Mode::Nxdn | Mode::Pager | Mode::Flex | Mode::Packet => "Frames decoded on receiver A appear here, newest first. Click a frame's frequency to tune to it; expand one for its fields.",
                _ => "Frames decoded on receiver A appear here. This mode has no decoder: pick a digital mode, or AUTO to run every decoder on the channel.",
            }) };
            // Scrolled into the log, new frames arriving on top push it down
            // by their height, so the rows being read stay where they are.
            let row_h = 24.0;
            let mut area = egui::ScrollArea::vertical().id_salt("decodes").auto_shrink([false, false]).min_scrolled_height(0.0);
            if app.decode_jump {
                area = area.vertical_scroll_offset(0.0);
                app.decode_jump = false;
            } else if app.decode_scroll > 1.0 && app.decodes_added > 0 {
                area = area.vertical_scroll_offset(app.decode_scroll + app.decodes_added as f32 * row_h);
            }
            app.decodes_added = 0;
            let mut toggle: Option<usize> = None;
            let rm = app.reduce_motion();
            let now = ui.input(|i| i.time);
            let out = area.show(ui, |ui| {
                if let Some(text) = empty {
                    empty_state(ui, text);
                }
                // Frames in the log, every one outside this view: say so, so
                // the tab's count and an empty list never disagree unexplained.
                if visible.is_empty() && total > 0 {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let n = if total == 1 { "1 frame in the log is".to_string() } else { format!("{total} frames in the log are") };
                        ui.label(RichText::new(format!("{n} outside this view.")).font(theme::sans(12.0)).color(theme::text_2()));
                        show_all = widgets::action_small(ui, "SHOW ALL").clicked();
                    });
                }
                for &i in &visible {
                    // Off screen, a row is only its height: a full log costs
                    // what is in view, not four hundred rows of layout.
                    let y0 = ui.cursor().top();
                    let h = app.decodes[i].height;
                    if h > 0.0 && !ui.is_rect_visible(egui::Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), h))) {
                        ui.allocate_space(vec2(ui.available_width(), h));
                        continue;
                    }
                    let born = if rm { f64::NEG_INFINITY } else { *app.decodes[i].born.get_or_insert(now) };
                    let d = &app.decodes[i];
                    let sel = app.selected_decode == Some(i);
                    let resp = ui
                        .horizontal(|ui| {
                            ui.set_min_height(18.0);
                            if ui.add(egui::Label::new(mono(if d.open { "▾" } else { "▸" }, theme::text_3())).sense(egui::Sense::click())).clicked() {
                                toggle = Some(i);
                            }
                            cell(ui, 8.0 * char_w, mono(local_hms(d.at), theme::text_3()), egui::Sense::hover());
                            if cell(ui, 9.0 * char_w, mono(crate::tuning::fmt_mhz(d.freq_hz, 4), theme::AMBER), egui::Sense::click()).on_hover_text("Tune here").clicked() {
                                tune_to = Some(d.freq_hz);
                            }
                            let c = if d.ev.valid { widgets::protocol_color(&d.ev.protocol) } else { theme::ERR };
                            cell(ui, proto_w, mono(format!("[{}]", d.ev.protocol), c), egui::Sense::hover());
                            cell(ui, kind_w, mono(&d.ev.kind, theme::text_2()), egui::Sense::hover());
                            ui.add(egui::Label::new(RichText::new(&d.ev.summary).font(theme::sans(12.0)).color(theme::text())).truncate());
                        })
                        .response;
                    let resp = resp.interact(egui::Sense::click());
                    if sel {
                        ui.painter().rect_filled(resp.rect, egui::CornerRadius::same(3), alpha(theme::AMBER, 0.08));
                    }
                    // A frame new to the log is tinted in its protocol's
                    // color for a moment and fades to the row it is, so an
                    // arrival is seen without anything moving.
                    let t = ((now - born) / 1.4).clamp(0.0, 1.0) as f32;
                    if t < 1.0 {
                        let c = widgets::protocol_color(&d.ev.protocol);
                        ui.painter().rect_filled(resp.rect, egui::CornerRadius::same(3), alpha(c, 0.16 * (1.0 - t).powi(2)));
                        // And a band of light runs once across it, left to
                        // right, in the first half second.
                        let s = ((now - born) / 0.6).clamp(0.0, 1.0) as f32;
                        if s < 1.0 {
                            let e = 1.0 - (1.0 - s).powi(3);
                            let r = resp.rect;
                            let band = 140.0;
                            let x = r.left() - band + (r.width() + 2.0 * band) * e;
                            let hot = alpha(theme::lerp(c, egui::Color32::WHITE, 0.5), 0.20 * (1.0 - s * 0.5));
                            let p = ui.painter_at(r);
                            let mut mesh = egui::Mesh::default();
                            for (dx, col) in [(-band, alpha(c, 0.0)), (0.0, hot), (band * 0.35, alpha(c, 0.0))] {
                                mesh.colored_vertex(egui::pos2(x + dx, r.top()), col);
                                mesh.colored_vertex(egui::pos2(x + dx, r.bottom()), col);
                            }
                            mesh.add_triangle(0, 1, 2);
                            mesh.add_triangle(1, 3, 2);
                            mesh.add_triangle(2, 3, 4);
                            mesh.add_triangle(3, 5, 4);
                            p.add(egui::Shape::mesh(mesh));
                        }
                        crate::anim::more(ui.ctx());
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
                    app.decodes[i].height = (ui.cursor().top() - y0 - ui.spacing().item_spacing.y).max(1.0);
                }
            });
            app.decode_scroll = out.state.offset.y;
            if app.decode_scroll <= 1.0 {
                app.decodes_unseen = 0;
            } else {
                // Reading older frames: say what has arrived above, and offer the way back.
                let n = app.decodes_unseen;
                let text = if n > 0 { format!("↑ {n} new · back to newest") } else { "⏸ scrolled back · back to newest".into() };
                let g = ui.painter().layout_no_wrap(text.clone(), theme::mono_bold(10.5), egui::Color32::PLACEHOLDER);
                let r = egui::Rect::from_center_size(egui::pos2(out.inner_rect.center().x, out.inner_rect.top() + 14.0), g.size() + vec2(18.0, 8.0));
                let b = ui.interact(r, ui.id().with("decode-jump"), egui::Sense::click());
                let fill = if b.hovered() { theme::AMBER } else { alpha(theme::AMBER, 0.88) };
                ui.painter().rect_filled(r, egui::CornerRadius::same(10), fill);
                ui.painter().galley(r.center() - g.size() / 2.0, g, theme::ink());
                if b.clicked() {
                    app.decode_jump = true;
                    app.decodes_unseen = 0;
                }
            }
            if let Some(i) = toggle {
                app.decodes[i].open = !app.decodes[i].open;
            }
        });
        if let Some(i) = app.selected_decode
            && let Some(d) = app.decodes.get(i)
        {
            ui.separator();
            ui.vertical(|ui| {
                egui::ScrollArea::vertical().id_salt("inspector").auto_shrink([false, false]).min_scrolled_height(0.0).show(ui, |ui| {
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
                            if widgets::action_small(ui, "COPY").clicked() {
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
    if show_all {
        app.decode_family = None;
        app.decode_chip = None;
        app.decode_filter.clear();
    }
}

/// What a log will hold, said once while it holds nothing.
fn empty_state(ui: &mut Ui, text: &str) {
    ui.add_space(6.0);
    ui.add(egui::Label::new(RichText::new(text).font(theme::sans(12.0)).color(theme::text_3())).wrap());
}

/// How far through a recording the speaker is, as a slim bar; keeps the
/// frames coming while it moves.
pub fn progress_bar(ui: &mut Ui, p: f32, w: f32) {
    let (r, _) = ui.allocate_exact_size(vec2(w, 4.0), egui::Sense::hover());
    ui.painter().rect_filled(r, egui::CornerRadius::same(2), theme::ink());
    let done = egui::Rect::from_min_size(r.min, vec2(r.width() * p.clamp(0.0, 1.0), r.height()));
    ui.painter().rect_filled(done, egui::CornerRadius::same(2), theme::AMBER);
    crate::anim::more(ui.ctx());
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
    let (title, note) = app.scope.title(mode, kind);
    // Sideband says the filter it is listening through, as set.
    let edges = (app.status.filter_high_hz > app.status.filter_low_hz && mode.is_sideband())
        .then(|| format!("{:.0}–{:.0} Hz", app.status.filter_low_hz.abs().min(app.status.filter_high_hz.abs()), app.status.filter_low_hz.abs().max(app.status.filter_high_hz.abs())));
    let note = note.map(str::to_string).or(edges);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).font(theme::sans_semibold(12.0)).color(theme::text()));
        // Every view the mode offers, named, rather than a click that cycles.
        let views = crate::scope::Scope::views(mode, kind);
        if views.is_empty() {
            if let Some(n) = note {
                ui.label(RichText::new(n).font(theme::mono(10.0)).color(theme::text_3()));
            }
        } else {
            ui.add_space(4.0);
            ui.spacing_mut().item_spacing.x = 3.0;
            let cur = app.scope.sub_for(mode);
            let labels: Vec<&str> = views.iter().map(|v| v.label()).collect();
            if let Some(i) = widgets::segmented(ui, "scope-view", &labels, views.iter().position(|v| *v == cur), theme::AMBER) {
                app.scope.set_view(mode, views[i]);
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let rates = [5.0f32, 10.0, 25.0];
            let cur = rates.iter().position(|hz| (app.prefs.scope_rate_hz - hz).abs() < 0.1);
            if let Some(i) = widgets::segmented(ui, "scope-rate", &["5 Hz", "10 Hz", "25 Hz"], cur, theme::OK) {
                app.prefs.scope_rate_hz = rates[i];
            }
        });
    });
    app.scope.rate_hz = app.prefs.scope_rate_hz;
    // The decoder's symbols, as large as the panel allows, its reading
    // beside them.
    if app.scope.sub_for(mode) == crate::scope::Sub::Constellation {
        let r = app.status.digital.clone().unwrap_or_default();
        let rect = ui.available_rect_before_wrap();
        let size = (rect.height() - 8.0).min(rect.width() * 0.6).max(120.0);
        ui.horizontal_top(|ui| {
            crate::utilviews::constellation(ui, &r.constellation, size, widgets::protocol_color(mode.label()));
            ui.vertical(|ui| {
                ui.label(RichText::new(r.variant.clone().unwrap_or_else(|| "no lock".into())).font(theme::mono_bold(11.0)).color(theme::text()));
                if let Some(snr) = r.snr_db {
                    ui.label(mono(format!("SNR {snr:+.0} dB"), theme::text_2()));
                }
                for f in r.facts.iter().filter(|f| !["●", "○"].contains(&f.value.as_str())) {
                    ui.label(mono(format!("{} {}", f.label, f.value), theme::text_3()));
                }
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new("The last equalized symbols, newest brightest; tight clusters on the points mean a clean copy.").font(theme::sans(10.5)).color(theme::text_3())).wrap());
            });
        });
        ui.advance_cursor_after_rect(rect);
        return;
    }
    let pass = crate::panels::passband_slice(app);
    // FT8/FT4: the last slot's decodes, by who sent them, at their tones.
    let tags: Vec<(f32, String)> = if matches!(mode, Mode::Ft8 | Mode::Ft4) {
        let slot = app.decodes.iter().find(|d| d.ev.protocol == mode.label()).and_then(|d| d.ev.fields.get("slotMs").cloned());
        app.decodes
            .iter()
            .take_while(|d| d.ev.fields.get("slotMs") == slot.as_ref() || d.ev.protocol != mode.label())
            .filter(|d| d.ev.protocol == mode.label())
            .filter_map(|d| {
                let df: f32 = d.ev.fields.get("df")?.parse().ok()?;
                let who = d.ev.fields.get("de").or(d.ev.fields.get("to")).cloned().unwrap_or_else(|| d.ev.summary.chars().take(8).collect());
                // Tone 0 is the reported frequency; the signal's middle is
                // half its width above.
                Some((df + if mode == Mode::Ft4 { 41.7 } else { 25.0 }, who))
            })
            .collect()
    } else {
        Vec::new()
    };
    let rect = ui.available_rect_before_wrap();
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    app.scope.ui(&mut child, app.frame.as_deref(), &app.status, pass.as_ref().map(|(v, r)| (v.as_slice(), *r)), &tags, &app.prefs.view.style.look());
    ui.advance_cursor_after_rect(rect);
}

/// What the classifier hears right now, above its log.
fn signal_details(app: &App, ui: &mut Ui) {
    let (c, word) = app.shown_class();
    if let Some(c) = c.filter(|_| word != "IDLE") {
        // Three lines, each cut to the dock rather than wrapped mid-field:
        // what it is, what was measured, and the decoder's own note.
        ui.horizontal(|ui| {
            ui.label(RichText::new(&c.protocol).font(theme::sans_semibold(13.0)).color(widgets::protocol_color(&c.protocol)));
            ui.add(egui::Label::new(mono(format!("{} · {:>3.0}%", c.modulation, c.confidence * 100.0), theme::text_2())).truncate());
        });
        ui.add(egui::Label::new(mono(format!(
            // Padded so each field keeps its place while the figures move.
            "SNR {:>+5.1} dB · RF {:>4.0} dBFS · dev peak {:>4.1}k rms {:>4.1}k · DC {:>+5.0} Hz",
            c.snr_db,
            c.rf_dbfs,
            c.peak_dev_hz / 1000.0,
            c.rms_dev_hz / 1000.0,
            c.center_offset_hz
        ), theme::text_2())).truncate());
        if let Some(d) = &c.details {
            ui.add(egui::Label::new(RichText::new(d).font(theme::sans(11.5)).color(theme::text_3())).truncate());
        }
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
    ui.label(mono(format!("{} detections", app.class_log.len()), theme::text_3()));
    let mut tune_to = None;
    if app.class_log.is_empty() {
        egui::ScrollArea::vertical().id_salt("class").auto_shrink([false, false]).min_scrolled_height(0.0).show(ui, |ui| {
            empty_state(ui, "What the classifier hears on receiver A is logged here: a row when the protocol or channel changes, or every three seconds while a signal holds.");
        });
        return;
    }
    egui::ScrollArea::vertical().id_salt("class").auto_shrink([false, false]).min_scrolled_height(0.0).show_rows(ui, 20.0, app.class_log.len(), |ui, range| {
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
    let progress = app.player.clip_progress();
    egui::ScrollArea::vertical().id_salt("calls").auto_shrink([false, false]).min_scrolled_height(0.0).show(ui, |ui| {
        if app.calls.is_empty() {
            empty_state(ui, "Finished voice calls are kept here with their audio, the most recent sixteen, so a call that went past can be played again. P25, DMR and NFM calls arrive once the receiver, or the scanner, hears one.");
        }
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
                        if widgets::action_small(ui, "IQ").on_hover_text("Save the channel I/Q (CS16LE + JSON)").clicked() {
                            actions.push((c.id, "iq"));
                        }
                        if widgets::action_small(ui, "WAV").on_hover_text("Save the call audio").clicked() {
                            actions.push((c.id, "wav"));
                        }
                        if c.encrypted && !c.decrypted {
                            widgets::chip(ui, Tone::Err, "encrypted — muted");
                        } else if c.peak < 0.002 {
                            widgets::chip(ui, Tone::Warn, "no audio decoded");
                        } else if let (Some(Playing::Call(id)), Some(p)) = (app.playing, progress)
                            && id == c.id
                        {
                            // Playing: the way back to live sits on the call itself.
                            if widgets::action(ui, "■ stop").on_hover_text("Stop and return to the live audio").clicked() {
                                actions.push((c.id, "stop"));
                            }
                            progress_bar(ui, p, 90.0);
                        } else if widgets::action(ui, "▶ play").on_hover_text("Play in place of the live audio").clicked() {
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
        if what == "stop" {
            app.player.stop_clip();
            app.playing = None;
            continue;
        }
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
        // The form scrolls, so a short dock never hides START.
        egui::ScrollArea::vertical().id_salt("scanform-scroll").max_width(380.0).auto_shrink([true, false]).min_scrolled_height(0.0).show(ui, |ui| ui.vertical(|ui| {
            ui.set_width(360.0);
            let f = &mut app.scan_form;
            let mut touched = false;
            egui::Grid::new("scanform").num_columns(2).spacing(vec2(10.0, 4.0)).show(ui, |ui| {
                let mut from = f.start_hz / 1e6;
                let mut to = f.stop_hz / 1e6;
                ui.label(mono("FROM / TO MHz", theme::text_3()));
                ui.horizontal(|ui| {
                    touched |= widgets::numeric(ui, &mut from, 1.0..=2200.0, 0.01, 4, 86.0).changed();
                    touched |= widgets::numeric(ui, &mut to, 1.0..=2200.0, 0.01, 4, 86.0).changed();
                });
                f.start_hz = (from * 1e6).round();
                f.stop_hz = (to * 1e6).round();
                ui.end_row();
                ui.label(mono("MODES", theme::text_3()));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    for m in ["am", "nfm", "p25", "dmr"] {
                        let on = f.modes.iter().any(|x| x == m);
                        if widgets::chip_key(ui, &m.to_uppercase(), on, theme::OK).clicked() {
                            touched = true;
                            f.modes.retain(|x| x != m);
                            if !on {
                                f.modes.push(m.into());
                            }
                        }
                    }
                });
                ui.end_row();
                // The house faders, as everywhere else, rather than egui's own sliders.
                ui.label(mono("SQUELCH", theme::text_3()));
                let mut v = f.threshold_db as f64;
                let ch = widgets::Fader { value: &mut v, min: 4.0, max: 20.0, step: 0.5, fine: 0.5, label: "", text: |v| format!("{v:.1} dB"), color: theme::OK, enabled: true, ticks: vec![] }.show_compact(ui).0;
                if ch {
                    f.threshold_db = v as f32;
                    touched = true;
                }
                ui.end_row();
                ui.label(mono("RESUME AFTER", theme::text_3()));
                let mut v = f.resume_delay_s as f64;
                let ch = widgets::Fader { value: &mut v, min: 0.5, max: 30.0, step: 0.5, fine: 0.1, label: "", text: |v| format!("{v:.1} s"), color: theme::OK, enabled: true, ticks: vec![] }.show_compact(ui).0;
                if ch {
                    f.resume_delay_s = v as f32;
                    touched = true;
                }
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
                    if widgets::numeric(ui, &mut v, lo..=hi, 1.0, 0, 76.0).changed() {
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
        }));
        ui.separator();
        ui.vertical(|ui| {
            let skips = st.as_ref().map(|s| s.skip_list.clone()).unwrap_or_default();
            ui.label(mono(if skips.is_empty() { "nothing parked".into() } else { format!("{} parked", skips.len()) }, theme::text_3()));
            egui::ScrollArea::vertical().id_salt("skips").auto_shrink([false, false]).min_scrolled_height(0.0).show(ui, |ui| {
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
                        if widgets::action_small(ui, "RE-TEST").clicked()
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

/// The decode log's keys on the tab row, laid right to left: the menu,
/// CLEAR (what is in view: one family's, or the whole log), the filter.
/// Where the tabs leave less `room` than they need, the filter and then
/// CLEAR fold into the menu rather than run over a tab.
fn decode_keys(app: &mut App, ui: &mut Ui, room: f32) {
    let (filter_out, clear_out) = (room >= 230.0, room >= 100.0);
    let filtering = !app.decode_filter.is_empty();
    // The menu lights while a filter it holds is narrowing the log.
    let tally = if filtering && !filter_out { theme::AMBER } else { theme::text_2() };
    widgets::menu_chip(ui, "···", tally, |ui| {
        if !filter_out {
            ui.add(egui::TextEdit::singleline(&mut app.decode_filter).hint_text("filter decodes").desired_width(180.0));
        }
        if !clear_out && ui.button("Clear what is shown").clicked() {
            clear_shown(app);
            ui.close();
        }
        if ui.button("Expand all").clicked() {
            app.decodes.iter_mut().for_each(|d| (d.open, d.height) = (true, 0.0));
            ui.close();
        }
        if ui.button("Collapse all").clicked() {
            app.decodes.iter_mut().for_each(|d| (d.open, d.height) = (false, 0.0));
            ui.close();
        }
    });
    if clear_out && widgets::action_small(ui, "CLEAR").clicked() {
        clear_shown(app);
    }
    if filter_out {
        let w = (room - 110.0).clamp(90.0, 170.0);
        ui.add(egui::TextEdit::singleline(&mut app.decode_filter).hint_text("filter decodes").desired_width(w))
            .on_hover_text("Protocol, kind or any text in the summary");
    }
}

/// Clear what the log shows: one family's decodes, or all of them.
fn clear_shown(app: &mut App) {
    match app.decode_family {
        Some(f) => app.decodes.retain(|d| d.family != Some(f)),
        None => app.decodes.clear(),
    }
    app.selected_decode = None;
}

/// A family's tally: the color of its first decoder.
fn family_color(f: Family) -> egui::Color32 {
    crate::decoders::in_family(f).next().map_or(theme::AMBER, |d| widgets::protocol_color(d.chip))
}

/// The HF data key that shows every HF decode as a plain log.
pub const ALL_HF: &str = "ALL HF";

/// The HF data view: a text mode's own view, or FT8's slot table.
fn hf_data(app: &mut App, ui: &mut Ui, visible: &[usize]) {
    match crate::hfviews::view_for(app) {
        crate::hfviews::HfView::Slots => crate::ft8view::show(app, ui, visible, Family::HfData.empty_text()),
        v => crate::hfviews::show(app, ui, v),
    }
}
