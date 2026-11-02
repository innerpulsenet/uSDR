//! The receive-frequency readout: ten digits, GHz to Hz.
//!
//! Every digit is a dial. The wheel over a digit turns it; a click on its
//! upper half adds one and on its lower half takes one away; arrow keys turn
//! the selected digit and move the selection. Leading zeros are dimmed down
//! to the MHz digit, and the Hz group is set smaller, as on a radio's face.
//! A double-click, Space, or typing a digit opens entry, which reads `146.52`,
//! `146520`, `146520000`, `146.520.000` and `7.2m` alike.

use crate::theme;
use egui::{Align2, CornerRadius, Rect, Sense, Stroke, Ui, pos2, vec2};


pub enum VfoAct {
    /// Turn by this many Hz, recentering the span (a dial step).
    Step(f64),
    /// Go to this frequency (typed).
    Set(f64),
}

pub struct Vfo {
    pub selected: usize,
    entry: Option<String>,
    focus_entry: bool,
    scroll_accum: f32,
    pub open_entry_request: bool,
    /// Where each digit was drawn last frame, GHz first.
    pub cells: Vec<Rect>,
}

impl Default for Vfo {
    fn default() -> Self {
        // The 10 kHz digit.
        Self { selected: 5, entry: None, focus_entry: false, scroll_accum: 0.0, open_entry_request: false, cells: Vec::new() }
    }
}

const DIGITS: usize = 10;

fn weight(i: usize) -> f64 {
    10f64.powi((DIGITS - 1 - i) as i32)
}

impl Vfo {
    pub fn editing(&self) -> bool {
        self.entry.is_some()
    }

    pub fn open_entry(&mut self, hz: f64, initial: Option<String>) {
        self.entry = Some(initial.unwrap_or_else(|| format!("{:.6}", hz / 1e6)));
        self.focus_entry = true;
    }

    pub fn ui(&mut self, ui: &mut Ui, hz: f64, height: f32, acts: &mut Vec<VfoAct>) {
        if self.open_entry_request {
            self.open_entry_request = false;
            self.open_entry(hz, None);
        }
        if let Some(text) = &mut self.entry {
            let mut done = None;
            let resp = ui.add_sized(
                [ui.available_width().min(380.0), height],
                egui::TextEdit::singleline(text)
                    .font(theme::mono_bold(height * 0.62))
                    .hint_text("146.52 · 146520k · 7.2m")
                    .text_color(theme::text()),
            );
            if self.focus_entry {
                resp.request_focus();
                self.focus_entry = false;
            }
            if resp.lost_focus() {
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    done = Some(crate::tuning::parse_freq(text));
                } else {
                    done = Some(None);
                }
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                done = Some(None);
            }
            if let Some(result) = done {
                self.entry = None;
                if let Some(v) = result {
                    acts.push(VfoAct::Set(v));
                }
            }
            return;
        }

        let units = (hz.max(0.0).round() as u64).min(9_999_999_999);
        let s = format!("{units:010}");
        let big = theme::mono_bold(height * 0.78);
        let small = theme::mono_bold(height * 0.56);
        let dw = ui.painter().layout_no_wrap("0".into(), big.clone(), theme::text()).size().x;
        let sw = ui.painter().layout_no_wrap("0".into(), small.clone(), theme::text()).size().x;
        let sep = dw * 0.38;
        let widths: Vec<f32> = (0..DIGITS).map(|i| if i >= 7 { sw } else { dw }).collect();
        let total: f32 = widths.iter().sum::<f32>() + 3.0 * sep + 8.0 + 40.0;
        let (rect, resp) = ui.allocate_exact_size(vec2(total, height), Sense::click());
        let painter = ui.painter_at(rect.expand(2.0));
        let first_sig = s.find(|c: char| c != '0').unwrap_or(DIGITS - 1).min(3);
        let mut x = rect.left();
        let mut hovered_digit = None;
        self.cells.clear();
        let pointer = resp.hover_pos();
        for (i, ch) in s.chars().enumerate() {
            if i == 1 || i == 4 || i == 7 {
                let dim = i == 1 && first_sig > 0;
                painter.text(
                    pos2(x + sep / 2.0, rect.bottom() - height * 0.16),
                    Align2::CENTER_BOTTOM,
                    ".",
                    big.clone(),
                    if dim { theme::surface_4() } else { theme::text_3() },
                );
                x += sep;
            }
            let w = widths[i];
            let cell = Rect::from_min_max(pos2(x, rect.top()), pos2(x + w, rect.bottom()));
            self.cells.push(cell);
            let hot = pointer.is_some_and(|p| cell.contains(p));
            if hot {
                hovered_digit = Some(i);
                painter.rect_filled(cell.expand2(vec2(1.0, -2.0)), CornerRadius::same(3), theme::alpha(theme::AMBER, 0.10));
            }
            let lead = i < first_sig;
            let color = if lead {
                theme::surface_4()
            } else if i >= 7 {
                theme::text_2()
            } else {
                theme::text()
            };
            let font = if i >= 7 { small.clone() } else { big.clone() };
            painter.text(pos2(cell.center().x, rect.bottom() - height * 0.12), Align2::CENTER_BOTTOM, ch.to_string(), font, color);
            if i == self.selected {
                painter.rect_filled(
                    Rect::from_min_max(pos2(cell.left() + 1.0, rect.bottom() - 3.0), pos2(cell.right() - 1.0, rect.bottom() - 1.0)),
                    CornerRadius::ZERO,
                    theme::AMBER,
                );
            }
            x += w;
        }
        painter.text(pos2(x + 8.0, rect.bottom() - height * 0.16), Align2::LEFT_BOTTOM, "MHz", theme::mono(height * 0.26), theme::text_3());

        if let Some(i) = hovered_digit {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            let scroll = crate::widgets::wheel(ui);
            if scroll != 0.0 {
                self.selected = i;
                self.scroll_accum += scroll;
                let n = self.scroll_accum.trunc();
                if n != 0.0 {
                    self.scroll_accum -= n;
                    acts.push(VfoAct::Step(n as f64 * weight(i)));
                }
            }
            if resp.clicked()
                && let Some(p) = resp.interact_pointer_pos()
            {
                self.selected = i;
                let up = p.y < rect.center().y;
                acts.push(VfoAct::Step(if up { weight(i) } else { -weight(i) }));
            }
        }
        if resp.double_clicked() {
            // The two clicks before it stepped the digit; undo them.
            acts.clear();
            self.open_entry(hz, None);
        }
        resp.on_hover_text("Wheel or click a digit to turn it · double-click or start typing to enter a frequency");
    }

    pub fn step_selected(&self, dir: f64) -> f64 {
        dir * weight(self.selected)
    }

    pub fn move_selection(&mut self, d: isize) {
        self.selected = (self.selected as isize + d).clamp(0, DIGITS as isize - 1) as usize;
    }
}

/// Pixels of drag per raster step on the tune strip.
const STRIP_PX: f32 = 14.0;

/// The tuning strip: a scale of raster steps under a fixed hairline, as on
/// a radio's dial. Drag it like a dial (right lowers the frequency), or
/// wheel over it; Shift for a tenth of the step. Returns how far to tune.
pub fn tune_strip(ui: &mut Ui, hz: f64, step: f64, width: f32) -> Option<f64> {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 40.0), Sense::click_and_drag());
    let id = resp.id.with("offset");
    let mut offset: f32 = ui.data(|d| d.get_temp(id).unwrap_or(0.0));
    let fine = ui.input(|i| i.modifiers.shift);
    let st = if fine { (step / 10.0).round().max(100.0) } else { step };
    let mut out = 0.0;
    if resp.dragged() {
        offset += resp.drag_delta().x;
        while offset.abs() >= STRIP_PX {
            let dir = offset.signum();
            offset -= dir * STRIP_PX;
            out -= dir as f64 * st;
        }
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    } else {
        offset = 0.0;
    }
    if resp.hovered() {
        let w = crate::widgets::wheel(ui);
        if w != 0.0 {
            crate::widgets::eat_scroll(ui);
            out += w.round() as f64 * st;
        }
    }
    ui.data_mut(|d| d.insert_temp(id, offset));

    let p = ui.painter_at(rect);
    p.rect(rect, egui::CornerRadius::same(theme::RADIUS), theme::ink(), Stroke::new(1.0, theme::line()), egui::StrokeKind::Inside);
    // The scale moves under the hairline as the dial turns.
    let frac = ((hz / st) - (hz / st).round()) as f32 * STRIP_PX;
    let c = rect.center().x - frac + offset;
    let n = (rect.width() / STRIP_PX / 2.0) as i64 + 2;
    let base = (hz / st).round() as i64;
    let decimals = if st >= 100_000.0 { 1 } else if st >= 10_000.0 { 2 } else if st >= 1_000.0 { 3 } else { 4 };
    for k in -n..=n {
        let x = c + k as f32 * STRIP_PX;
        if x < rect.left() + 2.0 || x > rect.right() - 2.0 {
            continue;
        }
        let idx = base + k;
        let (len, col) = if idx % 10 == 0 {
            (14.0, theme::text_2())
        } else if idx % 5 == 0 {
            (10.0, theme::text_3())
        } else {
            (5.0, theme::surface_4())
        };
        p.line_segment([pos2(x, rect.top() + 3.0), pos2(x, rect.top() + 3.0 + len)], Stroke::new(1.0, col));
        let clear = (x - rect.center().x).abs() > 30.0 && x > rect.left() + 26.0 && x < rect.right() - 26.0;
        if idx % 5 == 0 && clear {
            p.text(
                pos2(x, rect.bottom() - 4.0),
                Align2::CENTER_BOTTOM,
                format!("{:.*}", decimals, idx as f64 * st / 1e6),
                theme::mono(9.5),
                theme::text_3(),
            );
        }
    }
    // The hairline: where the receiver is.
    p.line_segment([pos2(rect.center().x, rect.top() + 1.0), pos2(rect.center().x, rect.bottom() - 1.0)], Stroke::new(1.5, theme::AMBER));
    // Fade the ends into the well.
    for (x0, x1, a0, a1) in [(rect.left(), rect.left() + 28.0, 1.0, 0.0), (rect.right() - 28.0, rect.right(), 0.0, 1.0)] {
        let mut m = egui::Mesh::default();
        m.colored_vertex(pos2(x0, rect.top()), theme::alpha(theme::ink(), a0));
        m.colored_vertex(pos2(x1, rect.top()), theme::alpha(theme::ink(), a1));
        m.colored_vertex(pos2(x0, rect.bottom()), theme::alpha(theme::ink(), a0));
        m.colored_vertex(pos2(x1, rect.bottom()), theme::alpha(theme::ink(), a1));
        m.add_triangle(0, 1, 2);
        m.add_triangle(2, 1, 3);
        p.add(egui::Shape::mesh(m));
    }
    resp.on_hover_text("Drag like a dial, or wheel · Shift for a tenth of the step");
    (out != 0.0).then_some(out)
}


#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Modifiers, PointerButton, TouchPhase};
    use egui_kittest::Harness;

    struct T {
        vfo: Vfo,
        hz: f64,
        acts: Vec<VfoAct>,
        ready: bool,
    }

    fn harness(hz: f64) -> Harness<'static, T> {
        let mut h = Harness::builder().with_size(egui::vec2(600.0, 120.0)).build_ui_state(
            |ui, t: &mut T| {
                if !t.ready {
                    crate::theme::install(ui.ctx(), crate::theme::Ground::Carbon);
                    t.ready = true;
                    return;
                }
                let hz = t.hz;
                t.vfo.ui(ui, hz, 46.0, &mut t.acts);
            },
            T { vfo: Vfo::default(), hz, acts: Vec::new(), ready: false },
        );
        h.run_steps(2);
        h
    }

    fn steps(h: &Harness<'static, T>) -> Vec<f64> {
        h.state().acts.iter().map(|a| match a {
            VfoAct::Step(d) => *d,
            VfoAct::Set(v) => *v,
        }).collect()
    }

    #[test]
    fn wheel_turns_the_digit_under_the_pointer() {
        let mut h = harness(146.52e6);
        // Digit 4 is the 1 MHz digit (G MMM: indices 0..3, then 100 kHz at 4).
        let c = h.state().vfo.cells[3].center();
        h.event(Event::PointerMoved(c));
        h.run_steps(1);
        h.event(Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta: egui::vec2(0.0, -1.0), phase: TouchPhase::Move, modifiers: Modifiers::NONE });
        h.run_steps(1);
        assert_eq!(steps(&h), vec![-1e6]);
        assert_eq!(h.state().vfo.selected, 3);
    }

    #[test]
    fn upper_half_adds_and_lower_half_takes_away() {
        let mut h = harness(146.52e6);
        let cell = h.state().vfo.cells[5];
        for (y, _) in [(cell.top() + 6.0, 1), (cell.bottom() - 6.0, -1)] {
            let p = egui::pos2(cell.center().x, y);
            h.event(Event::PointerMoved(p));
            h.event(Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
            h.event(Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
            h.run_steps(1);
            // Let the double-click window lapse between the two clicks.
            h.state_mut().vfo.cells.clear();
            h.run_steps(1);
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
        assert_eq!(steps(&h), vec![10_000.0, -10_000.0]);
    }

    #[test]
    fn typed_entry_sets_the_frequency() {
        let mut h = harness(146.52e6);
        h.state_mut().vfo.open_entry(146.52e6, Some(String::new()));
        h.run_steps(2);
        h.event(Event::Text("7.2m".into()));
        h.run_steps(1);
        h.key_press(egui::Key::Enter);
        h.run_steps(2);
        assert_eq!(steps(&h), vec![7.2e6]);
        assert!(!h.state().vfo.editing());
    }

    #[test]
    fn the_strip_turns_like_a_dial() {
        struct S {
            got: Vec<f64>,
            ready: bool,
        }
        let mut h = Harness::builder().with_size(egui::vec2(300.0, 60.0)).build_ui_state(
            |ui, s: &mut S| {
                if !s.ready {
                    crate::theme::install(ui.ctx(), crate::theme::Ground::Carbon);
                    s.ready = true;
                    return;
                }
                if let Some(d) = tune_strip(ui, 146.52e6, 6_250.0, 220.0) {
                    s.got.push(d);
                }
            },
            S { got: Vec::new(), ready: false },
        );
        h.run_steps(2);
        let start = egui::pos2(110.0, 20.0);
        h.event(Event::PointerMoved(start));
        h.drag_at(start);
        h.run_steps(1);
        // Right by three steps' worth: the frequency goes down three steps.
        for i in 1..=6 {
            h.event(Event::PointerMoved(start + egui::vec2(7.0 * i as f32, 0.0)));
            h.run_steps(1);
        }
        h.drop_at(start + egui::vec2(42.0, 0.0));
        h.run_steps(1);
        assert_eq!(h.state().got.iter().sum::<f64>(), -3.0 * 6_250.0);
    }

    #[test]
    fn escape_cancels_entry() {
        let mut h = harness(146.52e6);
        h.state_mut().vfo.open_entry(146.52e6, None);
        h.run_steps(2);
        h.key_press(egui::Key::Escape);
        h.run_steps(2);
        assert!(steps(&h).is_empty());
        assert!(!h.state().vfo.editing());
    }
}
