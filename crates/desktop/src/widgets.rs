//! The front panel's parts: applet cards, latching keys, precision faders,
//! status chips and segmented selectors.

use crate::theme::{self, alpha};
use egui::{Align2, Color32, CornerRadius, Rect, Response, Sense, Stroke, Ui, pos2, vec2};

/// Wheel movement this frame in notches (lines); positive is wheel-up.
/// Trackpads arrive in points and count 40 to a notch.
pub fn wheel(ui: &Ui) -> f32 {
    ui.input(|i| {
        i.raw
            .events
            .iter()
            .map(|e| match e {
                egui::Event::MouseWheel { unit, delta, .. } => match unit {
                    egui::MouseWheelUnit::Line => delta.y,
                    egui::MouseWheelUnit::Point => delta.y / 40.0,
                    egui::MouseWheelUnit::Page => delta.y * 3.0,
                },
                _ => 0.0,
            })
            .sum()
    })
}

/// Keep a wheel turn that a control used from also scrolling its panel.
pub fn eat_scroll(ui: &Ui) {
    ui.ctx().input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
}

/// An applet: a mono eyebrow over its controls, collapsible by its header.
pub fn applet<R>(ui: &mut Ui, id: &str, title: &str, extra: impl FnOnce(&mut Ui), add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    let open_id = ui.make_persistent_id(("applet", id));
    let mut open = ui.data_mut(|d| *d.get_persisted_mut_or_insert_with(open_id, || true));
    let frame = egui::Frame::new()
        .fill(theme::surface_2())
        .stroke(Stroke::new(1.0, theme::line()))
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(egui::Margin::symmetric(10, 8));
    let out = frame
        .show(ui, |ui| {
            let card = ui.max_rect().expand2(vec2(10.0, 8.0));
            theme::glass_top(ui.painter(), egui::Rect::from_min_size(card.min, vec2(card.width(), 40.0_f32.min(card.height()))));
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let t = ui.ctx().animate_bool_with_time(open_id.with("a"), open, 0.15);
                let (r, resp) = ui.allocate_exact_size(vec2(12.0, 14.0), Sense::click());
                let c = r.center();
                let rot = t * std::f32::consts::FRAC_PI_2;
                let pts = [vec2(-3.0, -4.0), vec2(3.0, 0.0), vec2(-3.0, 4.0)]
                    .map(|v| c + egui::emath::Rot2::from_angle(rot) * v);
                ui.painter().add(egui::Shape::convex_polygon(pts.to_vec(), theme::text_3(), Stroke::NONE));
                let label = ui.add(
                    egui::Label::new(egui::RichText::new(title.to_uppercase()).font(theme::mono(10.5)).color(theme::text_2()).extra_letter_spacing(1.2))
                        .sense(Sense::click()),
                );
                if resp.clicked() || label.clicked() {
                    open = !open;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| extra(ui));
            });
            if open {
                ui.add_space(4.0);
                Some(add(ui))
            } else {
                None
            }
        })
        .inner;
    ui.data_mut(|d| d.insert_persisted(open_id, open));
    out
}

/// A latching key with a tally light. Returns its response; clicked toggles.
pub fn key(ui: &mut Ui, label: &str, on: bool, tally: Color32) -> Response {
    key_sized(ui, label, on, tally, vec2(0.0, 26.0))
}

pub fn key_sized(ui: &mut Ui, label: &str, on: bool, tally: Color32, size: egui::Vec2) -> Response {
    let galley = ui.painter().layout_no_wrap(label.to_string(), theme::sans_semibold(11.5), Color32::PLACEHOLDER);
    let w = if size.x > 0.0 { size.x } else { galley.size().x + 30.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, size.y), Sense::click());
    let t = ui.ctx().animate_bool_with_time(resp.id.with("on"), on, 0.18);
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.12);
    let bg = theme::lerp(theme::surface_3(), theme::surface_4(), hover.max(t * 0.6));
    let p = ui.painter();
    let dim = if ui.is_enabled() { 1.0 } else { 0.4 };
    let tally = alpha(tally, dim);
    p.rect(rect, CornerRadius::same(theme::RADIUS), bg, Stroke::new(1.0, theme::lerp(theme::line(), alpha(tally, 0.55), t)), egui::StrokeKind::Inside);
    let led = pos2(rect.left() + 11.0, rect.center().y);
    if t > 0.01 {
        p.circle_filled(led, 6.0, alpha(tally, 0.18 * t));
    }
    p.circle_filled(led, 3.0, theme::lerp(theme::surface_4().gamma_multiply(1.4), tally, t));
    p.galley(pos2(rect.left() + 21.0, rect.center().y - galley.size().y / 2.0), galley, alpha(theme::lerp(theme::text_2(), theme::text(), t.max(hover)), dim));
    if resp.has_focus() {
        p.rect_stroke(rect.expand(1.5), CornerRadius::same(theme::RADIUS + 1), Stroke::new(1.0, theme::EMBER), egui::StrokeKind::Outside);
    }
    resp
}

/// A plain action key.
pub fn action(ui: &mut Ui, label: &str) -> Response {
    let galley = ui.painter().layout_no_wrap(label.to_string(), theme::sans_semibold(11.5), Color32::PLACEHOLDER);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 20.0, 26.0), Sense::click());
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.12);
    let p = ui.painter();
    let bg = if resp.is_pointer_button_down_on() { theme::surface_4() } else { theme::lerp(theme::surface_3(), theme::surface_4(), hover) };
    p.rect(rect, CornerRadius::same(theme::RADIUS), bg, Stroke::new(1.0, theme::line_2()), egui::StrokeKind::Inside);
    let c = if ui.is_enabled() { theme::text() } else { theme::text_3() };
    p.galley(rect.center() - galley.size() / 2.0, galley, c);
    resp
}

/// The one filled call to action in a view.
pub fn primary(ui: &mut Ui, label: &str) -> Response {
    let galley = ui.painter().layout_no_wrap(label.to_uppercase(), theme::sans_semibold(12.0), Color32::WHITE);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 32.0, 32.0), Sense::click());
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.12);
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(theme::RADIUS), theme::lerp(theme::ACTION, theme::ACTION_HOVER, hover));
    p.galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
    resp
}

pub struct Fader<'a, F: Fn(f64) -> String> {
    pub value: &'a mut f64,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub fine: f64,
    pub label: &'a str,
    /// The readout, from the value as it stands after this frame's input.
    pub text: F,
    pub color: Color32,
    pub enabled: bool,
    /// Scale labels under the track: (fraction, text).
    pub ticks: Vec<(f32, String)>,
}

impl<F: Fn(f64) -> String> Fader<'_, F> {
    /// Returns (changed, released-after-change).
    pub fn show(self, ui: &mut Ui) -> (bool, bool) {
        let w = ui.available_width();
        let h = if self.ticks.is_empty() { 34.0 } else { 46.0 };
        let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click_and_drag());
        let p = ui.painter();
        let a = if self.enabled { 1.0 } else { 0.5 };
        p.text(rect.left_top(), Align2::LEFT_TOP, self.label, theme::mono(10.0), alpha(theme::text_3(), a));
        p.text(rect.right_top(), Align2::RIGHT_TOP, (self.text)(*self.value), theme::mono_bold(11.5), alpha(self.color, a.max(0.7)));
        let track = Rect::from_min_max(pos2(rect.left() + 4.0, rect.top() + 18.0), pos2(rect.right() - 4.0, rect.top() + 24.0));
        p.rect_filled(track, CornerRadius::same(2), theme::ink());
        p.rect_stroke(track, CornerRadius::same(2), Stroke::new(1.0, theme::line()), egui::StrokeKind::Outside);
        let span = (self.max - self.min).max(1e-9);
        let t = ((*self.value - self.min) / span).clamp(0.0, 1.0) as f32;
        let fill = Rect::from_min_max(track.min, pos2(track.left() + track.width() * t, track.bottom()));
        // Lit fill fades in from the left, like a backlit scale.
        let mut mesh = egui::Mesh::default();
        mesh.colored_vertex(fill.left_top(), alpha(self.color, 0.15 * a));
        mesh.colored_vertex(fill.right_top(), alpha(self.color, 0.85 * a));
        mesh.colored_vertex(fill.left_bottom(), alpha(self.color, 0.15 * a));
        mesh.colored_vertex(fill.right_bottom(), alpha(self.color, 0.85 * a));
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(2, 1, 3);
        p.add(egui::Shape::mesh(mesh));
        let hover = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered() || resp.dragged(), 0.12);
        let thumb = Rect::from_center_size(pos2(fill.right(), track.center().y), vec2(8.0, 16.0 + 2.0 * hover));
        p.rect(thumb, CornerRadius::same(2), theme::lerp(theme::text_2(), theme::text(), hover), Stroke::new(1.0, theme::ink()), egui::StrokeKind::Inside);
        p.line_segment([pos2(thumb.center().x, thumb.top() + 4.0), pos2(thumb.center().x, thumb.bottom() - 4.0)], Stroke::new(1.0, alpha(theme::ink(), 0.7)));
        for (f, label) in &self.ticks {
            let x = track.left() + track.width() * f;
            p.line_segment([pos2(x, track.bottom() + 2.0), pos2(x, track.bottom() + 5.0)], Stroke::new(1.0, theme::text_3()));
            let align = if *f <= 0.0 { Align2::LEFT_TOP } else if *f >= 1.0 { Align2::RIGHT_TOP } else { Align2::CENTER_TOP };
            p.text(pos2(x, track.bottom() + 6.0), align, label, theme::mono(9.0), theme::text_3());
        }
        let mut changed = false;
        if let Some(pos) = resp.interact_pointer_pos()
            && (resp.dragged() || resp.clicked())
        {
            let ft = ((pos.x - track.left()) / track.width()).clamp(0.0, 1.0) as f64;
            let fine = ui.input(|i| i.modifiers.shift);
            let mut v = self.min + ft * span;
            let st = if fine { self.fine } else { self.step };
            if st > 0.0 {
                v = (v / st).round() * st;
            }
            let v = v.clamp(self.min, self.max);
            if (v - *self.value).abs() > 1e-9 {
                *self.value = v;
                changed = true;
            }
        }
        if resp.hovered() {
            let scroll = wheel(ui);
            if scroll != 0.0 {
                eat_scroll(ui);
                let fine = ui.input(|i| i.modifiers.shift);
                let st = if fine { self.fine } else { self.step };
                let v = (*self.value + scroll.signum() as f64 * st).clamp(self.min, self.max);
                if (v - *self.value).abs() > 1e-9 {
                    *self.value = v;
                    changed = true;
                }
            }
        }
        let released = resp.drag_stopped();
        (changed, released)
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    Ok,
    Warn,
    Err,
    Idle,
    Live,
}

/// An outlined status chip: a glyph and words, never a bare dot.
pub fn chip(ui: &mut Ui, tone: Tone, text: &str) -> Response {
    let (c, glyph) = match tone {
        Tone::Ok => (theme::OK, "●"),
        Tone::Warn => (theme::WARN, "▲"),
        Tone::Err => (theme::ERR, "■"),
        Tone::Idle => (theme::text_3(), "○"),
        Tone::Live => (theme::EMBER, "●"),
    };
    let g = ui.painter().layout_no_wrap(format!("{glyph} {text}"), theme::mono(10.5), c);
    let (rect, resp) = ui.allocate_exact_size(g.size() + vec2(12.0, 6.0), Sense::hover());
    ui.painter().rect(rect, CornerRadius::same(3), alpha(c, 0.08), Stroke::new(1.0, alpha(c, 0.5)), egui::StrokeKind::Inside);
    ui.painter().galley(rect.min + vec2(6.0, 3.0), g, c);
    resp
}

/// A segmented selector; returns the newly picked index.
pub fn segmented(ui: &mut Ui, items: &[&str], selected: usize) -> Option<usize> {
    let mut picked = None;
    let n = items.len().max(1);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 24.0), Sense::hover());
    let p = ui.painter();
    p.rect(rect, CornerRadius::same(theme::RADIUS), theme::ink(), Stroke::new(1.0, theme::line()), egui::StrokeKind::Inside);
    let cw = rect.width() / n as f32;
    let id = ui.id().with(("seg", items.first().copied().unwrap_or("")));
    let x = ui.ctx().animate_value_with_time(id, selected as f32, 0.16);
    let sel = Rect::from_min_size(pos2(rect.left() + x * cw + 2.0, rect.top() + 2.0), vec2(cw - 4.0, rect.height() - 4.0));
    ui.painter().rect_filled(sel, CornerRadius::same(3), theme::surface_4());
    for (i, item) in items.iter().enumerate() {
        let r = Rect::from_min_size(pos2(rect.left() + i as f32 * cw, rect.top()), vec2(cw, rect.height()));
        let resp = ui.interact(r, id.with(i), Sense::click());
        let c = if i == selected { theme::AMBER } else if resp.hovered() { theme::text() } else { theme::text_2() };
        ui.painter().text(r.center(), Align2::CENTER_CENTER, *item, theme::mono(10.5), c);
        if resp.clicked() && i != selected {
            picked = Some(i);
        }
    }
    picked
}

pub fn protocol_color(p: &str) -> Color32 {
    let u = p.to_ascii_uppercase();
    let rgb = |h: u32| Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8);
    if u.contains("P25") {
        rgb(0x4ade80)
    } else if u.contains("DMR") {
        rgb(0x60a5fa)
    } else if u.contains("SMARTNET") || u.contains("SMARTZONE") {
        rgb(0xfb7185)
    } else if u.contains("POCSAG") || u.contains("FLEX") {
        rgb(0xfacc15)
    } else if u.contains("APRS") {
        rgb(0xfb923c)
    } else if u.contains("NXDN") {
        rgb(0x22d3ee)
    } else if u.contains("GMSK") || u.contains("AIS") {
        rgb(0x2dd4bf)
    } else if u.contains("PACKET") || u.contains("TELEMETRY") {
        rgb(0xf59e0b)
    } else if u.contains("ANALOG") {
        rgb(0xc084fc)
    } else if u.contains("SAME") {
        rgb(0xf472b6)
    } else if u.contains("UNCLASSIFIED") {
        theme::text_3()
    } else {
        theme::CYAN
    }
}
