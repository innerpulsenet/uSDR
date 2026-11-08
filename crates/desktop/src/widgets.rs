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
    applet_opened(ui, id, title, true, extra, add)
}

/// An applet that starts folded until the operator opens it.
pub fn applet_folded<R>(ui: &mut Ui, id: &str, title: &str, extra: impl FnOnce(&mut Ui), add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    applet_opened(ui, id, title, false, extra, add)
}

fn applet_opened<R>(ui: &mut Ui, id: &str, title: &str, first_open: bool, extra: impl FnOnce(&mut Ui), add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    let open_id = ui.make_persistent_id(("applet", id));
    let mut open = ui.data_mut(|d| *d.get_persisted_mut_or_insert_with(open_id, || first_open));
    // A section engraved on the plate, a groove cut under it.
    let frame = egui::Frame::new().inner_margin(egui::Margin::symmetric(0, 4));
    let shown = frame
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let t = fold(ui, open_id, open);
                let (r, resp) = ui.allocate_exact_size(vec2(12.0, 14.0), Sense::click());
                let c = r.center();
                let rot = t * std::f32::consts::FRAC_PI_2;
                let pts = [vec2(-3.0, -4.0), vec2(3.0, 0.0), vec2(-3.0, 4.0)]
                    .map(|v| c + egui::emath::Rot2::from_angle(rot) * v);
                ui.painter().add(egui::Shape::convex_polygon(pts.to_vec(), theme::text_3(), Stroke::NONE));
                let label = theme::engraved(ui, &title.to_uppercase(), theme::mono(10.5), theme::text_2(), 1.2, Sense::click());
                if resp.clicked() || label.clicked() {
                    open = !open;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| extra(ui));
            });
            // The body folds and unfolds with the chevron: it is laid out at
            // its full height and shown through a window that grows with
            // the same animation, so what is hidden cannot be hit either.
            let lin = ui.ctx().animate_bool_with_time(open_id.with("a"), open, FOLD_S);
            let t = fold(ui, open_id, open);
            if lin <= 0.001 {
                return None;
            }
            ui.add_space(4.0 * t);
            let full = ui.available_rect_before_wrap();
            let natural_id = open_id.with("h");
            let natural: f32 = ui.data(|d| d.get_temp(natural_id).unwrap_or(0.0));
            let settled = lin >= 0.999;
            let shown_h = if settled { f32::INFINITY } else { natural * t };
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(full.min, vec2(full.width(), f32::INFINITY))));
            // Inside the parent's clip on every side: a scrolled side panel
            // puts the body's top above the panel, and a clip that started
            // there painted the card over the bar.
            child.set_clip_rect(Rect::from_min_max(full.min, pos2(full.right(), full.top() + shown_h)).intersect(ui.clip_rect()));
            let r = add(&mut child);
            let h = child.min_rect().height();
            ui.data_mut(|d| d.insert_temp(natural_id, h));
            ui.allocate_space(vec2(full.width(), if settled { h } else { h * t }));
            if !settled {
                crate::anim::more(ui.ctx());
            }
            Some(r)
        });
    let r = shown.response.rect;
    theme::groove_h(ui.painter(), r.left(), r.right(), r.bottom() + 2.0);
    let out = shown.inner;
    ui.data_mut(|d| d.insert_persisted(open_id, open));
    out
}

/// How long an applet takes to open or fold, seconds.
const FOLD_S: f32 = 0.26;

/// An applet's fold, 0 shut to 1 open, on a spring: opening, it runs a
/// few percent past open and settles back; folding, it eases shut. Snaps
/// with reduced motion.
fn fold(ui: &Ui, id: egui::Id, open: bool) -> f32 {
    let lin = ui.ctx().animate_bool_with_time(id.with("a"), open, if reduced(ui) { 0.0 } else { FOLD_S });
    if open { spring_out(lin) } else { lin * lin * (3.0 - 2.0 * lin) }
}

/// Ease out past the end and back: an underdamped spring's first swing,
/// with an overshoot of about 3 percent.
pub fn spring_out(t: f32) -> f32 {
    let s = 0.7;
    let u = t - 1.0;
    1.0 + (s + 1.0) * u * u * u + s * u * u
}

/// `text` cut to fit `max_w` in `font`, with an ellipsis where it was cut.
pub fn ellipsize(painter: &egui::Painter, text: &str, font: egui::FontId, max_w: f32) -> String {
    let width = |s: &str| painter.layout_no_wrap(s.to_owned(), font.clone(), Color32::PLACEHOLDER).size().x;
    if width(text) <= max_w {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut n = chars.len();
    while n > 0 {
        n -= 1;
        let s: String = chars[..n].iter().collect::<String>().trim_end().to_owned() + "…";
        if width(&s) <= max_w {
            return s;
        }
    }
    "…".into()
}

/// A chip that opens a menu: a `chip_key` with a chevron, so every menu on
/// the bars looks like the chips beside it and says it opens.
pub fn menu_chip<R>(ui: &mut Ui, label: &str, tally: Color32, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    menu_chip_fit(ui, label, &[], tally, add)
}

/// A menu chip as wide as the widest of `labels` (and `label`), so a label
/// that changes with its setting does not move the chips after it.
pub fn menu_chip_fit<R>(ui: &mut Ui, label: &str, labels: &[&str], tally: Color32, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    menu_chip_lit(ui, label, labels, tally, false, add)
}

/// A menu chip that stays lit while what it chooses among is in use, as a
/// `chip_key` is while its choice is.
pub fn menu_chip_lit<R>(ui: &mut Ui, label: &str, labels: &[&str], tally: Color32, lit: bool, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    let galley = ui.painter().layout_no_wrap(format!("{label} ▾"), theme::mono_bold(10.5), Color32::PLACEHOLDER);
    let widest = labels
        .iter()
        .map(|l| ui.painter().layout_no_wrap(format!("{l} ▾"), theme::mono_bold(10.5), Color32::PLACEHOLDER).size().x)
        .fold(galley.size().x, f32::max);
    let (rect, resp) = ui.allocate_exact_size(vec2(widest + 14.0, KEY_H_SMALL), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    let (press, c) = paint_key(ui, &resp, rect, CornerRadius::same(3), open || lit, tally);
    ui.painter().galley(rect.center() - galley.size() / 2.0 + vec2(0.0, press), galley, c);
    egui::Popup::menu(&resp).show(add).map(|r| r.inner)
}

/// A key `width` wide that opens a menu and says so with its glyph alone.
pub fn menu_glyph<R>(ui: &mut Ui, glyph: &str, width: f32, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    let galley = ui.painter().layout_no_wrap(glyph.to_string(), key_font(), Color32::PLACEHOLDER);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, KEY_H_SMALL), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, glyph));
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    let (press, c) = paint_key(ui, &resp, rect, CornerRadius::same(3), open, theme::AMBER);
    ui.painter().galley(rect.center() - galley.size() / 2.0 + vec2(0.0, press), galley, c);
    egui::Popup::menu(&resp).show(add).map(|r| r.inner)
}

/// A selector: the chosen value in mono with a chevron, opening a menu of
/// the choices. The house take on a combo box.
pub fn selector<R>(ui: &mut Ui, id: &str, text: &str, color: Color32, width: f32, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    let font = theme::mono(11.0);
    let w = if width > 0.0 { width } else { ui.painter().layout_no_wrap(text.to_owned(), font.clone(), Color32::PLACEHOLDER).size().x + 34.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 24.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, text));
    let (press, _) = paint_key(ui, &resp, rect, CornerRadius::same(theme::RADIUS), false, theme::AMBER);
    let p = ui.painter();
    let shown = ellipsize(p, text, font.clone(), w - 30.0);
    p.text(pos2(rect.left() + 8.0, rect.center().y + press), Align2::LEFT_CENTER, shown, font, color);
    p.text(pos2(rect.right() - 7.0, rect.center().y), Align2::RIGHT_CENTER, "▾", theme::mono(10.0), theme::text_3());
    egui::Popup::menu(&resp).id(egui::Id::new(("selector", id))).width(w.max(160.0)).show(add).map(|r| r.inner)
}

/// A display window of backlit glass the panel's width, `add` laid out in
/// it: as tall as what it holds last frame (a frame whose height moved is
/// laid out again before it is shown).
pub fn glass<R>(ui: &mut Ui, id: egui::Id, tint: Color32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let h_id = id.with("glass-h");
    let h: f32 = ui.data(|d| d.get_temp(h_id)).unwrap_or(40.0);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, h + 12.0), Sense::hover());
    crate::vfo::lcd_back(ui.painter(), rect, tint);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(9.0, 6.0))).layout(egui::Layout::top_down(egui::Align::Min)));
    let r = add(&mut child);
    let used = child.min_rect().height();
    if (used - h).abs() > 0.5 {
        ui.data_mut(|d| d.insert_temp(h_id, used));
        ui.ctx().request_discard("a glass window changed height");
    }
    crate::vfo::lcd_glass(ui.painter(), rect);
    r
}

/// A window let into the plate over the room left in `ui`, `add` laid out
/// in it.
pub fn window<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let r = ui.available_rect_before_wrap();
    theme::well(ui.painter(), r, None);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r.shrink2(vec2(8.0, 6.0))).layout(egui::Layout::top_down(egui::Align::Min)));
    let out = add(&mut child);
    ui.advance_cursor_after_rect(r);
    out
}

/// A selector as a small LCD window: `caption` printed faint, `value` lit,
/// a menu under it on a click. For a setting that is read as much as set.
pub fn lcd_selector<R>(ui: &mut Ui, id: &str, caption: &str, value: &str, tint: Color32, width: f32, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 24.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, format!("{caption} {value}")));
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.12);
    let p = ui.painter_at(rect.expand(3.0));
    crate::vfo::lcd_back(&p, rect, tint);
    let lit = theme::lerp(theme::text(), tint, 0.28);
    let mut x = rect.left() + 8.0;
    if !caption.is_empty() {
        let g = p.layout_no_wrap(caption.to_string(), theme::mono(9.5), alpha(lit, 0.5));
        p.galley(pos2(x, rect.center().y - g.size().y / 2.0), g.clone(), alpha(lit, 0.5));
        x += g.size().x + 6.0;
    }
    let v = ellipsize(&p, value, theme::mono_bold(12.0), rect.right() - 18.0 - x);
    p.text(pos2(x, rect.center().y), Align2::LEFT_CENTER, v, theme::mono_bold(12.0), lit);
    p.text(pos2(rect.right() - 8.0, rect.center().y), Align2::RIGHT_CENTER, "▾", theme::mono(10.0), alpha(lit, 0.45 + 0.4 * hover));
    crate::vfo::lcd_glass(&p, rect);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    egui::Popup::menu(&resp).id(egui::Id::new(("selector", id))).width(width.max(160.0)).show(add).map(|r| r.inner)
}

/// A number the operator drags or types: egui's drag value in the house
/// frame and the measuring mono, at a fixed width so rows do not shift.
pub fn numeric(ui: &mut Ui, value: &mut f64, range: std::ops::RangeInclusive<f64>, speed: f64, decimals: usize, width: f32) -> Response {
    ui.scope(|ui| {
        ui.style_mut().drag_value_text_style = egui::TextStyle::Monospace;
        ui.style_mut().text_styles.insert(egui::TextStyle::Monospace, theme::mono(11.0));
        ui.spacing_mut().interact_size.y = 24.0;
        ui.spacing_mut().button_padding = vec2(8.0, 4.0);
        ui.add_sized([width, 24.0], egui::DragValue::new(value).range(range).speed(speed).fixed_decimals(decimals))
    })
    .inner
}

/// An action key at chip height, for the small verbs inside lists.
pub fn action_small(ui: &mut Ui, label: &str) -> Response {
    action_sized(ui, label, 14.0, KEY_H_SMALL, 3)
}

fn action_sized(ui: &mut Ui, label: &str, pad: f32, height: f32, radius: u8) -> Response {
    let galley = ui.painter().layout_no_wrap(label.to_uppercase(), key_font(), Color32::PLACEHOLDER);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + pad, height), Sense::click());
    let enabled = ui.is_enabled();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    let (press, c) = paint_key(ui, &resp, rect, CornerRadius::same(radius), false, theme::AMBER);
    ui.painter().galley(rect.center() - galley.size() / 2.0 + vec2(0.0, press), galley, c);
    resp
}

/// A key's height at chip size, and the one font every key label is set in.
const KEY_H_SMALL: f32 = 22.0;

pub fn key_font() -> egui::FontId {
    theme::mono_bold(10.5)
}

/// Draw `resp`'s key over `rect`: the one body every key has, its hover,
/// latch and press eased here. Returns how far it is pressed (for its
/// label's offset) and the color the label takes.
pub fn paint_key(ui: &Ui, resp: &Response, rect: Rect, radius: CornerRadius, on: bool, lit: Color32) -> (f32, Color32) {
    let enabled = ui.is_enabled();
    let k = theme::KeyState {
        hover: ui.ctx().animate_bool_with_time(resp.id.with("hv"), enabled && resp.hovered(), 0.12),
        on: ui.ctx().animate_bool_with_time(resp.id.with("on"), on, 0.15),
        press: if enabled { pressed(ui, resp) } else { 0.0 },
        color: theme::on_color(lit),
        focus: resp.has_focus(),
    };
    theme::key_body(ui.painter(), rect, radius, k);
    (k.press.max(k.on * 0.6), theme::key_text(k, enabled))
}

/// A latching key with a tally light. Returns its response; clicked toggles.
pub fn key(ui: &mut Ui, label: &str, on: bool, tally: Color32) -> Response {
    key_sized(ui, label, on, tally, vec2(0.0, 26.0))
}

pub fn key_sized(ui: &mut Ui, label: &str, on: bool, tally: Color32, size: egui::Vec2) -> Response {
    let galley = ui.painter().layout_no_wrap(label.to_string(), key_font(), Color32::PLACEHOLDER);
    let w = if size.x > 0.0 { size.x } else { galley.size().x + 18.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, size.y), Sense::click());
    let enabled = ui.is_enabled();
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, on, label));
    let (press, c) = paint_key(ui, &resp, rect, CornerRadius::same(theme::RADIUS), on, tally);
    ui.painter().galley(rect.center() - galley.size() / 2.0 + vec2(0.0, press), galley, c);
    resp
}

/// A latching key at chip height, for rows of choices (modes, presets)
/// where a full key is too wide.
pub fn chip_key(ui: &mut Ui, label: &str, on: bool, tally: Color32) -> Response {
    chip_key_w(ui, label, on, tally, 0.0)
}

/// A chip key `width` wide (0: as wide as its label), for grids of chips
/// whose edges line up.
pub fn chip_key_w(ui: &mut Ui, label: &str, on: bool, tally: Color32, width: f32) -> Response {
    chip_key_named(ui, label, label, on, tally, width)
}

/// A chip key showing `label`, a short form of what it is `name`d for
/// a screen reader and its tooltip.
pub fn chip_key_named(ui: &mut Ui, label: &str, name: &str, on: bool, tally: Color32, width: f32) -> Response {
    let galley = ui.painter().layout_no_wrap(label.to_string(), key_font(), Color32::PLACEHOLDER);
    let w = if width > 0.0 { width } else { galley.size().x + 14.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, KEY_H_SMALL), Sense::click());
    let enabled = ui.is_enabled();
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, on, name));
    let (press, c) = paint_key(ui, &resp, rect, CornerRadius::same(3), on, tally);
    ui.painter().galley(rect.center() - galley.size() / 2.0 + vec2(0.0, press), galley, c);
    resp
}

/// A small outlined key for a header line, 18 px tall: the tally outlines
/// it always, so it reads as something to press, and fills it when `lit`.
/// An LCD's annunciator, for a display's glass: lit, a block of `color`
/// with the word cut out of it; unlit, the word faintly there, as an
/// unpowered segment is. Clickable where the display takes a touch.
pub fn lcd_tag(ui: &mut Ui, text: &str, color: Color32, lit: bool) -> Response {
    let font = theme::mono_bold(10.0);
    let g = ui.painter().layout_no_wrap(text.to_string(), font, Color32::PLACEHOLDER);
    let (r, resp) = ui.allocate_exact_size(vec2(g.size().x + 10.0, 16.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    let p = ui.painter();
    if lit {
        p.rect_filled(r.expand(1.5), CornerRadius::same(3), alpha(color, 0.14));
        p.rect_filled(r, CornerRadius::same(2), alpha(color, 0.88));
        p.galley(r.center() - g.size() / 2.0, g, theme::lerp(theme::ink(), Color32::BLACK, 0.4));
    } else {
        p.rect_stroke(r, CornerRadius::same(2), Stroke::new(1.0, alpha(color, 0.18)), egui::StrokeKind::Inside);
        p.galley(r.center() - g.size() / 2.0, g, alpha(color, 0.38));
    }
    resp
}

/// A chip-height step key with a drawn chevron: `wider` points right.
/// Dimmed and inert when there is no further step.
pub fn step_chip(ui: &mut Ui, wider: bool, enabled: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(20.0, KEY_H_SMALL), if enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, if wider { "wider" } else { "narrower" }));
    let (press, c) = paint_key(ui, &resp, rect, CornerRadius::same(3), false, theme::AMBER);
    let c = if enabled { c } else { theme::text_3() };
    let p = ui.painter();
    let (x, y, d) = (rect.center().x, rect.center().y + press, if wider { 1.0 } else { -1.0 });
    p.line_segment([pos2(x - 2.0 * d, y - 4.0), pos2(x + 2.0 * d, y)], Stroke::new(1.5, c));
    p.line_segment([pos2(x + 2.0 * d, y), pos2(x - 2.0 * d, y + 4.0)], Stroke::new(1.5, c));
    resp
}

/// A plain action key.
pub fn action(ui: &mut Ui, label: &str) -> Response {
    action_sized(ui, label, 20.0, 26.0, theme::RADIUS)
}

/// The one filled call to action in a view.
pub fn primary(ui: &mut Ui, label: &str) -> Response {
    // Amber, the house accent: crimson stays for recording, live and
    // destructive keys, so a SAVE or Connect never reads as a stop.
    let text = Color32::from_rgb(0x1e, 0x14, 0x06);
    let galley = ui.painter().layout_no_wrap(label.to_uppercase(), theme::mono_bold(11.5), text);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 32.0, 32.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.12);
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(theme::RADIUS), theme::lerp(theme::AMBER, theme::AMBER_HOVER, hover));
    let press = pressed(ui, &resp);
    theme::key_face(p, rect, theme::RADIUS, press);
    p.galley(rect.center() - galley.size() / 2.0 + vec2(0.0, press), galley, text);
    if resp.has_focus() {
        p.rect_stroke(rect.expand(1.5), CornerRadius::same(theme::RADIUS + 1), Stroke::new(1.0, theme::EMBER), egui::StrokeKind::Outside);
    }
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
        self.run(ui, false, false)
    }

    /// On one 22 px line: the label, the track, the value.
    pub fn show_compact(self, ui: &mut Ui) -> (bool, bool) {
        self.run(ui, true, false)
    }

    /// The track and its cap alone, its reading shown elsewhere.
    pub fn show_bare(self, ui: &mut Ui) -> (bool, bool) {
        self.run(ui, true, true)
    }

    fn run(self, ui: &mut Ui, compact: bool, bare: bool) -> (bool, bool) {
        let w = ui.available_width();
        let h = if compact { 22.0 } else if self.ticks.is_empty() { 34.0 } else { 46.0 };
        let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click_and_drag());
        let (enabled, value, label) = (self.enabled, *self.value, self.label);
        resp.widget_info(|| egui::WidgetInfo::slider(enabled, value, label));
        let p = ui.painter();
        let a = if self.enabled { 1.0 } else { 0.5 };
        let value_text = (self.text)(*self.value);
        let track = if bare {
            Rect::from_min_max(pos2(rect.left() + 6.0, rect.center().y - 3.0), pos2(rect.right() - 6.0, rect.center().y + 3.0))
        } else if compact {
            let lw = p.layout_no_wrap(self.label.to_string(), theme::mono(10.5), theme::text_3()).size().x;
            // Room for the widest the readout gets over the range, no more,
            // and the value set just after the track's end, so a short
            // reading sits by the thumb rather than across a gap.
            let width_of = |t: String| p.layout_no_wrap(t, theme::mono_bold(11.0), theme::text()).size().x;
            let vw = width_of(value_text.clone()).max(width_of((self.text)(self.min))).max(width_of((self.text)(self.max)));
            p.text(pos2(rect.left(), rect.center().y), Align2::LEFT_CENTER, self.label, theme::mono(10.5), alpha(theme::text_3(), a));
            let track = Rect::from_min_max(pos2(rect.left() + lw + 10.0, rect.center().y - 3.0), pos2(rect.right() - vw - 8.0, rect.center().y + 3.0));
            p.text(pos2(track.right() + 8.0, rect.center().y), Align2::LEFT_CENTER, &value_text, theme::mono_bold(11.0), alpha(self.color, a.max(0.7)));
            track
        } else {
            p.text(rect.left_top(), Align2::LEFT_TOP, self.label, theme::mono(10.5), alpha(theme::text_3(), a));
            p.text(rect.right_top(), Align2::RIGHT_TOP, &value_text, theme::mono_bold(11.5), alpha(self.color, a.max(0.7)));
            Rect::from_min_max(pos2(rect.left() + 4.0, rect.top() + 18.0), pos2(rect.right() - 4.0, rect.top() + 24.0))
        };
        p.rect(track, CornerRadius::same(2), theme::ink(), Stroke::new(1.0, Color32::from_black_alpha(200)), egui::StrokeKind::Outside);
        p.line_segment([pos2(track.left() + 2.0, track.bottom() + 1.5), pos2(track.right() - 2.0, track.bottom() + 1.5)], Stroke::new(1.0, Color32::from_white_alpha(26)));
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
        theme::fader_cap(p, Rect::from_center_size(pos2(fill.right(), track.center().y), vec2(10.0, 16.0)), hover);
        for (f, label) in &self.ticks {
            let x = track.left() + track.width() * f;
            p.line_segment([pos2(x, track.bottom() + 2.0), pos2(x, track.bottom() + 5.0)], Stroke::new(1.0, theme::text_3()));
            let align = if *f <= 0.0 { Align2::LEFT_TOP } else if *f >= 1.0 { Align2::RIGHT_TOP } else { Align2::CENTER_TOP };
            p.text(pos2(x, track.bottom() + 6.0), align, label, theme::mono(10.0), theme::text_3());
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
        // Keyboard: arrows step, Shift for the fine step, while focused.
        if resp.has_focus() {
            let (l, r, fine) = ui.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::ArrowDown),
                    i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::ArrowUp),
                    i.modifiers.shift,
                )
            });
            let st = if fine { self.fine } else { self.step };
            let d = if r { st } else if l { -st } else { 0.0 };
            if d != 0.0 {
                let v = (*self.value + d).clamp(self.min, self.max);
                if (v - *self.value).abs() > 1e-9 {
                    *self.value = v;
                    changed = true;
                }
            }
            p.rect_stroke(rect.expand(1.5), CornerRadius::same(theme::RADIUS), Stroke::new(1.0, theme::EMBER), egui::StrokeKind::Outside);
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

/// An outlined status chip: a glyph and words, never a bare dot. The live
/// chip (recording) breathes: its lamp swells and dims over two seconds,
/// so a recording in progress is seen to be in progress.
pub fn chip(ui: &mut Ui, tone: Tone, text: &str) -> Response {
    let (c, _, _) = tone_look(tone);
    let g = ui.painter().layout_no_wrap(text.to_owned(), theme::mono(10.5), c);
    let lamp_w = 13.0;
    let (rect, resp) = ui.allocate_exact_size(g.size() + vec2(12.0 + lamp_w, 6.0), Sense::hover());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    let p = ui.painter();
    p.rect(rect, CornerRadius::same(3), alpha(c, 0.08), Stroke::new(1.0, alpha(c, 0.5)), egui::StrokeKind::Inside);
    let at = pos2(rect.left() + 6.0 + 4.0, rect.center().y);
    tone_lamp(ui, at, tone);
    p.galley(pos2(rect.left() + 6.0 + lamp_w, rect.top() + 3.0), g, c);
    resp
}

/// A tone's color, its lamp's outline and how brightly it is lit.
fn tone_look(tone: Tone) -> (Color32, theme::LampShape, f32) {
    match tone {
        Tone::Ok => (theme::OK, theme::LampShape::Round, 1.0),
        Tone::Warn => (theme::WARN, theme::LampShape::Triangle, 1.0),
        Tone::Err => (theme::ERR, theme::LampShape::Square, 1.0),
        Tone::Idle => (theme::text_3(), theme::LampShape::Round, 0.0),
        Tone::Live => (theme::EMBER, theme::LampShape::Round, 1.0),
    }
}

/// A tone's lamp at `at`. The live one breathes, over two seconds.
fn tone_lamp(ui: &Ui, at: egui::Pos2, tone: Tone) {
    let (c, shape, lit) = tone_look(tone);
    let mut glow = lit;
    if tone == Tone::Live && !reduced(ui) {
        let t = ui.input(|i| i.time);
        let breath = 0.5 + 0.5 * (t * std::f64::consts::TAU / 2.0).sin() as f32;
        glow = 0.55 + 0.45 * breath;
        crate::anim::more(ui.ctx());
    }
    // Idle reads as a dark lamp in the text's gray, not as a lit one.
    theme::lamp(ui.painter(), at, 3.6, if tone == Tone::Idle { theme::text_2() } else { c }, glow, shape);
}

/// A lamp on the plate and its word engraved beside it, for a state read
/// at a glance. A tone other than good colors the word too.
pub fn lamp_label(ui: &mut Ui, tone: Tone, text: &str) -> Response {
    let (c, _, _) = tone_look(tone);
    let color = if matches!(tone, Tone::Ok | Tone::Idle) { theme::text_2() } else { c };
    let g = ui.painter().layout_no_wrap(text.to_owned(), theme::mono(10.5), Color32::PLACEHOLDER);
    let (rect, resp) = ui.allocate_exact_size(g.size() + vec2(13.0, 4.0), Sense::hover());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    tone_lamp(ui, pos2(rect.left() + 4.0, rect.center().y), tone);
    theme::paint_engraved(ui.painter(), pos2(rect.left() + 13.0, rect.top() + 2.0), g, color);
    resp
}

/// A word on a small LCD let into the plate, at least `min_w` wide.
pub fn lcd_text(ui: &mut Ui, text: &str, color: Color32, min_w: f32) -> Response {
    let font = theme::mono_bold(11.0);
    let g = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let (rect, resp) = ui.allocate_exact_size(vec2((g.size().x + 16.0).max(min_w), 24.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    let p = ui.painter_at(rect.expand(3.0));
    crate::vfo::lcd_back(&p, rect, theme::AMBER);
    p.galley(rect.center() - g.size() / 2.0, g, color);
    crate::vfo::lcd_glass(&p, rect);
    resp
}

/// A small turning arc, for a wait: on the motion clock, where egui's own
/// spinner asks for a frame every frame.
pub fn spinner(ui: &mut Ui, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let t = ui.input(|i| i.time) as f32;
    let (c, r) = (rect.center(), size / 2.0 - 1.5);
    let a0 = if reduced(ui) { 0.0 } else { t * 4.0 };
    let pts: Vec<egui::Pos2> = (0..=12).map(|k| {
        let a = a0 + k as f32 / 12.0 * std::f32::consts::PI * 1.4;
        pos2(c.x + r * a.cos(), c.y + r * a.sin())
    }).collect();
    ui.painter().add(egui::Shape::line(pts, Stroke::new(1.5, color)));
    if !reduced(ui) {
        crate::anim::more(ui.ctx());
    }
}

/// A menu entry for a waterfall palette or a display style: a ramp of the
/// palette, the color its trace takes when it has one, and the name, lit
/// when it is the one in use. Clicked chooses it.
pub fn swatch_item(ui: &mut Ui, label: &str, palette: crate::colormap::Colormap, trace: Option<Color32>, on: bool) -> Response {
    let font = theme::sans(12.5);
    let g = ui.painter().layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER);
    let dot_w = if trace.is_some() { 14.0 } else { 0.0 };
    let w = ui.available_width().max(dot_w + 56.0 + 8.0 + g.size().x + 16.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 24.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, on, label));
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("hv"), resp.hovered(), 0.12);
    let p = ui.painter();
    if on || hover > 0.0 {
        p.rect_filled(rect, CornerRadius::same(3), if on { alpha(theme::AMBER, 0.14 + 0.06 * hover) } else { alpha(theme::surface_4(), 0.8 * hover) });
    }
    let mut x = rect.left() + 6.0;
    if let Some(c) = trace {
        p.circle_filled(pos2(x + 4.0, rect.center().y), 4.0, c);
        p.circle_stroke(pos2(x + 4.0, rect.center().y), 4.0, Stroke::new(1.0, theme::ink()));
        x += dot_w;
    }
    palette.paint_ramp(p, Rect::from_min_size(pos2(x, rect.center().y - 6.0), vec2(56.0, 12.0)));
    x += 56.0 + 10.0;
    p.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, if on { theme::text() } else { theme::lerp(theme::text_2(), theme::text(), hover) });
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
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
    } else if u.contains("FT8") {
        rgb(0xe879f9)
    } else if u.contains("FT4") {
        rgb(0xa78bfa)
    } else if u.contains("WEFAX") || u.contains("FAX") {
        rgb(0xe2e8f0)
    } else if u.contains("RTTY") {
        rgb(0x93c5fd)
    } else if u.contains("NAVTEX") {
        rgb(0xfdba74)
    } else if u.contains("SITOR") {
        rgb(0xfda4af)
    } else if u.contains("DSC") {
        rgb(0xf87171)
    } else if u.contains("HFDL") {
        rgb(0x7dd3fc)
    } else if u.contains("PACTOR") {
        rgb(0x6ee7b7)
    } else if u == "CW" {
        rgb(0xfcd34d)
    } else if u.contains("OLIVIA") {
        rgb(0x86efac)
    } else if u.contains("DOMINOEX") {
        rgb(0xfca5a5)
    } else if u.contains("THOR") {
        rgb(0xc4b5fd)
    } else if u.contains("MT63") {
        rgb(0x7dd3fc)
    } else if u.contains("CONTESTIA") {
        rgb(0xbef264)
    } else if u.contains("SELCAL") {
        rgb(0xfdba74)
    } else if u.contains("MFSK") {
        rgb(0x67e8f9)
    } else if u.contains("HELL") {
        rgb(0xd6d3d1)
    } else if u.contains("FST4W") {
        rgb(0xf9a8d4)
    } else if u.contains("WSPR") {
        rgb(0xfb7185)
    } else if u.contains("JS8") {
        rgb(0xf0abfc)
    } else if u.contains("SSTV") {
        rgb(0x5eead4)
    } else if u.contains("2G ALE") {
        rgb(0xa5b4fc)
    } else if u.contains("STANAG") || u.contains("188-110") {
        rgb(0x94a3b8)
    } else if u.contains("WWV") {
        rgb(0xfef08a)
    } else if u.contains("DRM") {
        rgb(0xf9a8d4)
    } else if u.contains("PSK") {
        rgb(0xbef264)
    } else if u.contains("RDS") {
        rgb(0xa3e635)
    } else if u.contains("SAME") {
        rgb(0xf472b6)
    } else if u.contains("UNCLASSIFIED") {
        theme::text_3()
    } else {
        theme::CYAN
    }
}

/// How far a key is pressed in, 0 to 1: down while the pointer holds it,
/// quick both ways so a click is felt without slowing it.
fn pressed(ui: &Ui, resp: &Response) -> f32 {
    let down = resp.is_pointer_button_down_on();
    if reduced(ui) {
        return if down { 1.0 } else { 0.0 };
    }
    ui.ctx().animate_bool_with_time(resp.id.with("press"), down, 0.05)
}

fn reduce_motion_id() -> egui::Id {
    egui::Id::new("usdr-reduce-motion")
}

/// Tell the widgets whether the operator has asked for less motion.
pub fn set_reduce_motion(ctx: &egui::Context, on: bool) {
    ctx.data_mut(|d| d.insert_temp(reduce_motion_id(), on));
}

fn reduced(ui: &Ui) -> bool {
    ui.data(|d| d.get_temp(reduce_motion_id()).unwrap_or(false))
}

/// The corners of the `i`th key of `n` joined in a row: only the row's two
/// ends are rounded, so they read as one block.
pub fn joined_corners(i: usize, n: usize) -> CornerRadius {
    let r = 3;
    CornerRadius { nw: if i == 0 { r } else { 0 }, sw: if i == 0 { r } else { 0 }, ne: if i + 1 == n { r } else { 0 }, se: if i + 1 == n { r } else { 0 } }
}

/// One choice of several: keys joined in one block, the backlight sliding
/// to the chosen one. It reads as "pick one" where a row of separate keys
/// reads as "switch any". Returns the segment clicked when it is not the one
/// already chosen.
pub fn segmented(ui: &mut Ui, id: &str, labels: &[&str], current: Option<usize>, tally: Color32) -> Option<usize> {
    let id = ui.make_persistent_id(("segmented", id));
    let font = key_font();
    let widths: Vec<f32> = labels.iter().map(|l| ui.painter().layout_no_wrap(l.to_string(), font.clone(), Color32::PLACEHOLDER).size().x + 14.0).collect();
    let total = widths.iter().sum::<f32>();
    let (track, _) = ui.allocate_exact_size(vec2(total, KEY_H_SMALL), Sense::hover());
    let enabled = ui.is_enabled();
    let p = ui.painter().clone();
    let color = theme::on_color(tally);
    let mut x = track.left();
    let cells: Vec<Rect> = widths
        .iter()
        .map(|w| {
            let r = Rect::from_min_size(pos2(x, track.top()), vec2(*w, track.height()));
            x += w;
            r
        })
        .collect();
    // The keys first, each up, so the lit one can slide across them.
    let mut states = Vec::with_capacity(cells.len());
    for (i, (cell, label)) in cells.iter().zip(labels).enumerate() {
        let resp = ui.interact(*cell, id.with(i), if enabled { Sense::click() } else { Sense::hover() });
        let on = current == Some(i);
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, enabled, on, *label));
        let hover = ui.ctx().animate_bool_with_time(resp.id.with("hv"), enabled && resp.hovered() && !on, 0.12);
        let k = theme::KeyState { hover, on: 0.0, press: if enabled { pressed(ui, &resp) } else { 0.0 }, color, focus: resp.has_focus() };
        theme::key_body(&p, cell.shrink2(vec2(0.5, 0.0)), joined_corners(i, cells.len()), k);
        states.push((resp, k));
    }
    if let Some(i) = current.filter(|i| *i < cells.len()) {
        let c = cells[i];
        let t = if reduced(ui) { 0.0 } else { 0.14 };
        let l = ui.ctx().animate_value_with_time(id.with("l"), c.left(), t);
        let r = ui.ctx().animate_value_with_time(id.with("r"), c.right(), t);
        let lit = Rect::from_min_max(pos2(l, c.top()), pos2(r, c.bottom())).shrink2(vec2(0.5, 0.0));
        theme::key_body(&p, lit, joined_corners(i, cells.len()), theme::KeyState { hover: 0.0, on: 1.0, press: 0.0, color, focus: false });
    }
    let mut out = None;
    for (i, ((cell, label), (resp, k))) in cells.iter().zip(labels).zip(&states).enumerate() {
        let on = current == Some(i);
        let c = theme::key_text(theme::KeyState { on: if on { 1.0 } else { 0.0 }, ..*k }, enabled);
        p.text(cell.center() + vec2(0.0, if on { 0.6 } else { k.press }), Align2::CENTER_CENTER, *label, font.clone(), c);
        if resp.clicked() && !on {
            out = Some(i);
        }
    }
    out
}

/// A run of digits that roll like an odometer when they change: each
/// changed character slides out and its new one in, up for a larger
/// number and down for a smaller, over `ROLL_S`. The value shown is always
/// the new one once the roll ends, and at once with reduced motion.
/// Returns where it was drawn.
pub fn rolling_text(painter: &egui::Painter, id: egui::Id, pos: egui::Pos2, anchor: Align2, text: &str, font: egui::FontId, color: Color32) -> Rect {
    const ROLL_S: f64 = 0.12;
    #[derive(Clone, Default)]
    struct Roll {
        shown: String,
        rolls: Vec<Option<(f64, char, f32)>>,
    }
    let ctx = painter.ctx().clone();
    let now = ctx.input(|i| i.time);
    let rm = ctx.data(|d| d.get_temp(reduce_motion_id()).unwrap_or(false));
    let mut st: Roll = ctx.data(|d| d.get_temp(id)).unwrap_or_default();
    let chars: Vec<char> = text.chars().collect();
    if st.rolls.len() != chars.len() {
        st.rolls = vec![None; chars.len()];
    }
    if !st.shown.is_empty() && st.shown != text && st.shown.chars().count() == chars.len() && !rm {
        let num = |s: &str| s.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse::<u64>().unwrap_or(0);
        let dir = if num(text) >= num(&st.shown) { 1.0 } else { -1.0 };
        for (i, (a, b)) in st.shown.chars().zip(chars.iter()).enumerate() {
            if a != *b {
                st.rolls[i] = Some((now, a, dir));
            }
        }
    }
    st.shown = text.to_owned();
    let whole = painter.layout_no_wrap(text.to_owned(), font.clone(), color);
    let rect = anchor.anchor_size(pos, whole.size());
    let mut x = rect.left();
    let mut rolling = false;
    for (i, ch) in chars.iter().enumerate() {
        let g = painter.layout_no_wrap(ch.to_string(), font.clone(), color);
        let w = g.size().x;
        let cell = Rect::from_min_size(pos2(x, rect.top()), vec2(w, rect.height()));
        match st.rolls[i].filter(|(t0, _, _)| now - t0 < ROLL_S && !rm) {
            Some((t0, old, dir)) => {
                let t = ((now - t0) / ROLL_S) as f32;
                let e = 1.0 - (1.0 - t).powi(3);
                let shift = rect.height() * 0.8 * dir;
                let cp = painter.with_clip_rect(cell.expand2(vec2(1.0, 0.0)).intersect(painter.clip_rect()));
                cp.text(cell.left_top() - vec2(0.0, e * shift), Align2::LEFT_TOP, old.to_string(), font.clone(), color);
                cp.galley(cell.left_top() + vec2(0.0, (1.0 - e) * shift), g, color);
                rolling = true;
            }
            None => {
                st.rolls[i] = None;
                painter.galley(cell.min, g, color);
            }
        }
        x += w;
    }
    if rolling {
        crate::anim::more(&ctx);
    }
    ctx.data_mut(|d| d.insert_temp(id, st));
    rect
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_fold_spring_lands_open_after_a_small_overshoot() {
        let peak = (0..=100).map(|i| super::spring_out(i as f32 / 100.0)).fold(0.0, f32::max);
        assert!(peak > 1.0 && peak < 1.03, "{peak}");
        assert!(super::spring_out(0.0).abs() < 1e-6);
        assert!((super::spring_out(1.0) - 1.0).abs() < 1e-6);
    }
}
