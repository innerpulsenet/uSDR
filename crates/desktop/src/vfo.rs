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
}

impl Default for Vfo {
    fn default() -> Self {
        // The 10 kHz digit.
        Self { selected: 5, entry: None, focus_entry: false, scroll_accum: 0.0, open_entry_request: false }
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

/// A thin hairline box with a label, used around readouts.
pub fn readout(ui: &mut Ui, label: &str, value: &str, color: egui::Color32) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        ui.label(egui::RichText::new(label).font(theme::mono(9.5)).color(theme::text_3()));
        ui.label(egui::RichText::new(value).font(theme::mono_bold(12.5)).color(color));
    });
    let _ = Stroke::NONE;
}
