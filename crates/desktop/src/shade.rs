//! Shade: the window rolled up to one strip that can stay above the others.
//! It keeps what is worth glancing at while doing something else: the
//! frequency, the mode, the S-meter and the latest thing decoded. F9 or a
//! double-click on the wordmark rolls it up; F9, the UNROLL key or a
//! double-click on the strip rolls it back to the size it was.

use crate::app::App;
use crate::theme::{self, alpha};
use crate::widgets;
use egui::{Align2, CornerRadius, Rect, Sense, Stroke, Ui, ViewportCommand, pos2, vec2};

/// The strip's height, points.
pub const STRIP_H: f32 = 56.0;

/// How the window was before it rolled up.
pub struct Restore {
    size: egui::Vec2,
    maximized: bool,
}

impl App {
    /// Roll the window up to the strip, or back down to its old size.
    pub fn toggle_shade(&mut self, ctx: &egui::Context) {
        match self.shade.take() {
            Some(r) => {
                ctx.send_viewport_cmd(ViewportCommand::WindowLevel(egui::WindowLevel::Normal));
                ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(vec2(1100.0, 700.0)));
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(r.size));
                if r.maximized {
                    ctx.send_viewport_cmd(ViewportCommand::Maximized(true));
                }
            }
            None => {
                let (size, maximized) = ctx.input(|i| {
                    let v = i.viewport();
                    (v.inner_rect.map_or(vec2(1600.0, 980.0), |r| r.size()), v.maximized.unwrap_or(false))
                });
                if self.fullscreen {
                    self.fullscreen = false;
                    ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
                }
                if maximized {
                    ctx.send_viewport_cmd(ViewportCommand::Maximized(false));
                }
                ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(vec2(640.0, STRIP_H)));
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(vec2(size.x.clamp(860.0, 1120.0), STRIP_H)));
                ctx.send_viewport_cmd(ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
                self.shade = Some(Restore { size, maximized });
            }
        }
    }
}

/// The strip itself, filling the window.
pub fn strip(app: &mut App, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    egui::CentralPanel::default()
        .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 6)))
        .show(ui, |ui| {
            let full = ui.max_rect().expand2(vec2(12.0, 6.0));
            theme::paint_plate(ui.painter(), full, theme::plate());
            // The strip is the window's handle: drag it to move the window,
            // double-click it to roll back down. Anything on it takes its
            // own clicks first.
            let bg = ui.interact(full, ui.id().with("shade-bg"), Sense::click_and_drag());
            if bg.drag_started() {
                ctx.send_viewport_cmd(ViewportCommand::StartDrag);
            }
            if bg.double_clicked() {
                app.toggle_shade(&ctx);
                return;
            }
            // The receiver's edge light along the top.
            let b_on = app.active_b && app.status.rx2.is_some();
            let tint = if b_on { theme::VIOLET } else { theme::AMBER };
            theme::edge_light(ui.painter(), full, tint, 1.0);
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                crate::brand::mark_static(ui.painter(), r);
                let live = app.have_status && app.link.is_some();
                if !live {
                    widgets::chip(ui, widgets::Tone::Idle, "not connected");
                } else {
                    frequency(app, ui, tint, b_on);
                    signal(app, ui, b_on);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if widgets::action_small(ui, "UNROLL").on_hover_text("Back to the full window (F9, or double-click the strip)").clicked() {
                        app.toggle_shade(&ctx);
                    }
                    if live {
                        let muted = if b_on { app.prefs.muted_b } else { app.prefs.muted };
                        if widgets::chip_key(ui, "MUTE", muted, theme::ERR).on_hover_text("Mute the receiver being driven (M)").clicked() {
                            if b_on {
                                app.prefs.muted_b = !app.prefs.muted_b;
                                app.player_b.set_muted(app.prefs.muted_b);
                            } else {
                                app.prefs.muted = !app.prefs.muted;
                                app.player.set_muted(app.prefs.muted);
                            }
                        }
                        latest(app, ui);
                    }
                });
            });
        });
}

/// The frequency, rolling as it changes; the wheel over it tunes by the step.
fn frequency(app: &mut App, ui: &mut Ui, tint: egui::Color32, b_on: bool) {
    let hz = app.active_hz();
    let text = crate::tuning::fmt_mhz(hz, 4);
    let font = theme::mono_bold(22.0);
    let size = ui.painter().layout_no_wrap(text.clone(), font.clone(), theme::text()).size();
    let (rect, resp) = ui.allocate_exact_size(vec2(size.x + 46.0, 32.0), Sense::hover());
    theme::well(ui.painter(), rect, Some(tint));
    let tab = Rect::from_center_size(pos2(rect.left() + 13.0, rect.center().y), vec2(14.0, 14.0));
    ui.painter().rect_filled(tab, CornerRadius::same(3), alpha(tint, 0.9));
    ui.painter().text(tab.center(), Align2::CENTER_CENTER, if b_on { "B" } else { "A" }, theme::mono_bold(10.0), theme::ink());
    widgets::rolling_text(ui.painter(), egui::Id::new("shade-freq"), pos2(rect.left() + 26.0, rect.center().y), Align2::LEFT_CENTER, &text, font, theme::text());
    if resp.hovered() {
        let notches = widgets::wheel(ui);
        if notches != 0.0 {
            widgets::eat_scroll(ui);
            let step = app.step_hz();
            let target = crate::tuning::snap(hz + notches.signum() as f64 * step, step);
            app.tune(target, true);
        }
    }
    resp.on_hover_text("Wheel to tune by the step");
    let (mode, bw) = match app.status.rx2.as_ref().filter(|_| b_on) {
        Some(b) => (b.mode, b.bandwidth_hz),
        None => (app.status.mode, app.status.bandwidth_hz),
    };
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(egui::RichText::new(mode.label()).font(theme::mono_bold(11.0)).color(tint));
        ui.label(egui::RichText::new(crate::tuning::fmt_short_hz(bw)).font(theme::mono(10.5)).color(theme::text_3()));
    });
}

/// The S-meter, as the receiver bar draws it, and its reading.
fn signal(app: &mut App, ui: &mut Ui, b_on: bool) {
    let rm = app.reduce_motion();
    let mark = app.prefs.view.sql_mark;
    let meter = if b_on { &mut app.meter_b } else { &mut app.meter };
    let ((s_txt, s_col), _) = meter.readout();
    ui.allocate_ui(vec2(190.0, 26.0), |ui| {
        ui.set_width(190.0);
        meter.ui_bar(ui, rm, mark);
    });
    let g = ui.painter().layout_no_wrap("S9+60".into(), theme::mono_bold(15.0), s_col);
    let (r, _) = ui.allocate_exact_size(g.size(), Sense::hover());
    ui.painter().text(r.left_center(), Align2::LEFT_CENTER, s_txt, theme::mono_bold(15.0), s_col);
}

/// What was heard last: a broadcast station's name and what is on, else
/// the newest decode in the last ten minutes, else what the classifier
/// says. Cut to the room left.
fn latest(app: &App, ui: &mut Ui) {
    let room = ui.available_width() - 8.0;
    if room < 60.0 {
        return;
    }
    let fresh = |at: std::time::SystemTime| at.elapsed().is_ok_and(|d| d.as_secs() < 600);
    let (tag, tag_c, body) = if let Some(r) = app.rds() {
        let now = crate::bar::rds_now(r).map(|(t, _)| t).unwrap_or_default();
        (crate::bar::rds_station(r), widgets::protocol_color("RDS"), now)
    } else if let Some(d) = app.decodes.front().filter(|d| fresh(d.at)) {
        (d.ev.protocol.clone(), widgets::protocol_color(&d.ev.protocol), d.ev.summary.clone())
    } else if let (Some(c), _) = app.shown_class() {
        (c.protocol.clone(), widgets::protocol_color(&c.protocol), String::new())
    } else {
        (String::new(), theme::text_3(), crate::app::mode_idle_text(app.status.mode).to_owned())
    };
    let p = ui.painter().clone();
    let tag = widgets::ellipsize(&p, &tag, theme::mono_bold(11.0), room * 0.4);
    let tag_w = if tag.is_empty() { 0.0 } else { p.layout_no_wrap(tag.clone(), theme::mono_bold(11.0), tag_c).size().x + 10.0 };
    let body = widgets::ellipsize(&p, &body.replace('\n', " "), theme::sans(12.0), (room - tag_w).max(0.0));
    let (rect, _) = ui.allocate_exact_size(vec2(room, 20.0), Sense::hover());
    // Laid right to left: the text starts at the left of what is left.
    let mut x = rect.left();
    if !tag.is_empty() {
        p.text(pos2(x, rect.center().y), Align2::LEFT_CENTER, &tag, theme::mono_bold(11.0), tag_c);
        x += tag_w;
    }
    p.text(pos2(x, rect.center().y), Align2::LEFT_CENTER, body, theme::sans(12.0), theme::text());
    p.line_segment([pos2(rect.left() - 6.0, rect.top()), pos2(rect.left() - 6.0, rect.bottom())], Stroke::new(1.0, theme::line()));
}
