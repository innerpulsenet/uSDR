//! The receive-frequency readout: ten digits, GHz to Hz, in a recessed
//! well. Below 1 GHz the GHz digit and its gap are left out, so 95.1 MHz
//! reads `095.100 000`, not `0 095.100 000`.
//!
//! Every digit is a dial. The wheel over a digit turns it; a click on its
//! upper half adds one and on its lower half takes one away; arrow keys turn
//! the selected digit and move the selection. Leading zeros are dimmed down
//! to the MHz digit, and the Hz group is set smaller, as on a radio's face.
//! A double-click, Space, or typing a digit opens entry, which reads `146.52`,
//! `146520`, `146520000`, `146.520.000` and `7.2m` alike.

use crate::theme;
use egui::{Align2, Color32, CornerRadius, Pos2, Rect, Sense, Shape, Stroke, Ui, pos2, vec2};


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
    /// The digits as last drawn, to see which ones change.
    shown: Option<String>,
    /// A digit rolling over: when it started, the glyph leaving, and which
    /// way (up for a higher frequency).
    roll: [Option<(f64, char, i8)>; DIGITS],
    /// The tint of the well's edge: the receiver the readout belongs to.
    pub tint: Option<Color32>,
    /// The status line inside the glass, over the digits; empty for none.
    pub annun: Annunciators,
    /// The display's height, the digits centered in what the strip leaves;
    /// 0 for just what they need.
    pub well_h: f32,
}

/// The LCD's annunciators: which receiver, its mode, width and gain at the
/// left, and flags at the right that are lit when on and ghosts when off.
#[derive(Default, Clone)]
pub struct Annunciators {
    pub rx: &'static str,
    pub mode: String,
    pub bw: String,
    pub gain: String,
    /// Name, whether lit, and its lit color.
    pub flags: Vec<(&'static str, bool, Color32)>,
}

impl Default for Vfo {
    fn default() -> Self {
        // The 10 kHz digit.
        Self { selected: 5, entry: None, focus_entry: false, scroll_accum: 0.0, open_entry_request: false, cells: Vec::new(), shown: None, roll: [None; DIGITS], tint: None, annun: Annunciators::default(), well_h: 0.0 }
    }
}

/// How long a digit takes to roll to its new value.
const ROLL_S: f64 = 0.11;

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

    pub fn ui(&mut self, ui: &mut Ui, hz: f64, height: f32, reduce_motion: bool, acts: &mut Vec<VfoAct>) {
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
        // Under 1 GHz the GHz digit is always a zero; it and its gap go.
        let no_ghz = units < 1_000_000_000;
        if no_ghz && self.selected == 0 {
            self.selected = 1;
        }
        // Seven-segment digits, drawn rather than set in type: taller than a
        // font's figures in the same width, with the unlit segments faintly
        // there, as on a radio's LCD.
        let big = Seg { h: height * 0.76, w: height * 0.36 };
        let small = Seg { h: height * 0.54, w: height * 0.255 };
        let pitch = |g: &Seg| g.w + height * 0.075;
        let (dw, sw) = (pitch(&big), pitch(&small));
        // `1 090.000 000`: the decimal point after the MHz digits, and a
        // thin gap at each thousands group either side of it.
        let point = dw * 0.40;
        let gap = dw * 0.28;
        let widths: Vec<f32> = (0..DIGITS).map(|i| if i >= 7 { sw } else { dw }).collect();
        let shown_w: f32 = if no_ghz { widths[1..].iter().sum::<f32>() + gap } else { widths.iter().sum::<f32>() + 2.0 * gap };
        let strip = !self.annun.rx.is_empty();
        let unit_font = theme::mono_bold(height * 0.24);
        // With a strip the unit rides in it; without, after the digits.
        let unit_w = if strip { 0.0 } else { ui.painter().layout_no_wrap("MHz".into(), unit_font.clone(), theme::text_3()).size().x + 10.0 };
        let strip_h = if strip { 17.0 } else { 0.0 };
        // The glass sits in a bezel, the digits inset from its edges.
        let (pad_x, pad_y) = (14.0, 5.0);
        let natural_h = height + 2.0 * pad_y + strip_h;
        let outer_h = self.well_h.max(natural_h);
        let total = shown_w + point + unit_w + 2.0 * pad_x;
        let (outer, resp) = ui.allocate_exact_size(vec2(total, outer_h), Sense::click());
        // Named for the receiver it reads, its label now being inside it.
        if strip {
            let name = format!("RECEIVER {}", self.annun.rx);
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &name));
        }
        let tint = self.tint.unwrap_or(theme::AMBER);
        lcd_back(ui.painter(), outer, tint);
        // The digits' own band: under the strip, centered in what is left.
        let spare = outer_h - natural_h;
        let rect = Rect::from_min_size(pos2(outer.left() + pad_x, outer.top() + pad_y + strip_h + spare * 0.55), vec2(total - 2.0 * pad_x, height));
        let painter = ui.painter_at(outer.expand(2.0));
        let first_sig = s.find(|c: char| c != '0').unwrap_or(DIGITS - 1).min(3);
        let now = ui.input(|i| i.time);
        // Which digits changed since the last draw, and which way: each
        // rolls over like an odometer wheel.
        if let Some(prev) = &self.shown
            && prev != &s
            && !reduce_motion
        {
            let up = units > prev.parse::<u64>().unwrap_or(units);
            for (i, (a, b)) in prev.chars().zip(s.chars()).enumerate() {
                if a != b {
                    self.roll[i] = Some((now, a, if up { 1 } else { -1 }));
                }
            }
        }
        self.shown = Some(s.clone());
        // The lit segments: the receiver's tint washed toward white, so the
        // figures read first and the color second; the Hz group a step down.
        let lit = theme::lerp(theme::text(), tint, 0.28);
        let lit_hz = theme::lerp(theme::text_2(), tint, 0.22);
        let baseline = rect.bottom() - height * 0.10;
        let mut x = rect.left();
        let mut hovered_digit = None;
        let mut rolling = false;
        self.cells.clear();
        let pointer = resp.hover_pos();
        for (i, ch) in s.chars().enumerate() {
            if i == 0 && no_ghz {
                // Kept as an empty cell, so a digit's index is its weight.
                self.cells.push(Rect::from_min_size(pos2(x, rect.top()), vec2(0.0, rect.height())));
                continue;
            }
            if i == 1 && !no_ghz {
                x += gap;
            } else if i == 4 {
                // The decimal point: a segment-sized square on the baseline.
                let t = big.t();
                let c = pos2(x + point / 2.0 - t * 0.2, baseline - t / 2.0);
                let dot = Rect::from_center_size(c, vec2(t * 1.05, t * 1.05));
                painter.rect_filled(dot.expand(t * 0.35), CornerRadius::same(2), theme::alpha(lit, 0.10));
                painter.rect_filled(dot, CornerRadius::same(1), lit);
                x += point;
            } else if i == 7 {
                x += gap;
            }
            let w = widths[i];
            let cell = Rect::from_min_max(pos2(x, rect.top()), pos2(x + w, rect.bottom()));
            self.cells.push(cell);
            let hot = pointer.is_some_and(|p| cell.contains(p));
            if hot {
                hovered_digit = Some(i);
                painter.rect_filled(cell.expand2(vec2(1.0, -1.0)), CornerRadius::same(3), theme::alpha(tint, 0.09));
            }
            let lead = i < first_sig;
            let g = if i >= 7 { &small } else { &big };
            let color = if i >= 7 { lit_hz } else { lit };
            // Each digit centered in its cell, on the shared baseline.
            let origin = pos2(cell.center().x - g.w / 2.0, baseline - g.h);
            let digit = |p: &egui::Painter, o: Pos2, c: char| g.paint(p, o, if lead { None } else { c.to_digit(10) }, color);
            let roll = self.roll[i].filter(|(t0, _, _)| !reduce_motion && now - t0 < ROLL_S);
            match roll {
                Some((t0, old, dir)) => {
                    let t = ((now - t0) / ROLL_S) as f32;
                    let e = 1.0 - (1.0 - t).powi(3);
                    let shift = height * 0.62 * dir as f32;
                    let clip = Rect::from_min_max(pos2(cell.left() - 2.0, rect.top() - 2.0), pos2(cell.right() + 2.0, rect.bottom() + 2.0));
                    let cp = painter.with_clip_rect(clip);
                    // The ghost stays put; only the lit figures roll.
                    g.paint(&cp, origin, None, color);
                    g.paint_lit(&cp, origin - vec2(0.0, e * shift), old.to_digit(10), color);
                    g.paint_lit(&cp, origin + vec2(0.0, (1.0 - e) * shift), ch.to_digit(10), color);
                    rolling = true;
                }
                None => {
                    self.roll[i] = None;
                    digit(&painter, origin, ch);
                }
            }
            if i == self.selected {
                painter.rect_filled(
                    Rect::from_min_max(pos2(cell.left() + 1.0, rect.bottom() + 1.0), pos2(cell.right() - 1.0, rect.bottom() + 3.0)),
                    CornerRadius::same(1),
                    tint,
                );
            }
            x += w;
        }
        if strip {
            self.paint_strip(&painter, outer, pad_x, tint, lit, lit_hz);
        } else {
            painter.text(pos2(x + 10.0, baseline), Align2::LEFT_BOTTOM, "MHz", unit_font, theme::alpha(lit_hz, 0.75));
        }
        lcd_glass(&painter, outer);
        if rolling {
            crate::anim::more(ui.ctx());
        }

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

    /// The annunciator strip across the top of the glass: the receiver's
    /// letter as an inverse block, its mode, width and gain, and at the
    /// right the flags and the unit.
    fn paint_strip(&self, p: &egui::Painter, outer: Rect, pad_x: f32, tint: Color32, lit: Color32, lit_hz: Color32) {
        let a = &self.annun;
        let y = outer.top() + 5.0 + 7.0;
        let f = theme::mono_bold(10.0);
        let mut x = outer.left() + pad_x - 2.0;
        // The receiver: a block of lit segment with the letter cut out.
        let g = p.layout_no_wrap(format!("RX {}", a.rx), f.clone(), Color32::PLACEHOLDER);
        let block = Rect::from_min_size(pos2(x, y - 7.0), vec2(g.size().x + 8.0, 14.0));
        p.rect_filled(block.expand(1.5), CornerRadius::same(3), theme::alpha(tint, 0.12));
        p.rect_filled(block, CornerRadius::same(2), theme::alpha(tint, 0.85));
        p.galley(pos2(block.left() + 4.0, y - g.size().y / 2.0), g, theme::lerp(theme::ink(), Color32::BLACK, 0.4));
        x = block.right() + 8.0;
        // The unit's room at the right, which the words never reach into.
        let unit_w = p.layout_no_wrap("MHz".into(), f.clone(), lit_hz).size().x;
        let limit = outer.right() - pad_x + 2.0 - unit_w - 8.0;
        let word = |t: &str, c: Color32, x: &mut f32| {
            if t.is_empty() {
                return;
            }
            let g = p.layout_no_wrap(t.to_string(), f.clone(), c);
            if *x + g.size().x > limit {
                return;
            }
            p.galley(pos2(*x, y - g.size().y / 2.0), g.clone(), c);
            *x += g.size().x + 9.0;
        };
        word(&a.mode, lit, &mut x);
        word(&a.bw, lit_hz, &mut x);
        word(&a.gain, lit_hz, &mut x);
        let left_end = x;
        // From the right: the unit, then each flag, lit or a ghost.
        let mut r = outer.right() - pad_x + 2.0;
        let unit = p.layout_no_wrap("MHz".into(), f.clone(), lit_hz);
        p.galley(pos2(r - unit.size().x, y - unit.size().y / 2.0), unit.clone(), theme::alpha(lit_hz, 0.8));
        r -= unit.size().x + 10.0;
        for (name, on, c) in a.flags.iter().rev() {
            let g = p.layout_no_wrap(name.to_string(), theme::mono_bold(9.0), Color32::PLACEHOLDER);
            let w = g.size().x;
            if r - w < left_end {
                break;
            }
            let col = if *on { *c } else { theme::alpha(lit, 0.08) };
            if *on {
                let glow = Rect::from_min_size(pos2(r - w, y - g.size().y / 2.0), g.size()).expand2(vec2(3.0, 1.0));
                p.rect_filled(glow, CornerRadius::same(2), theme::alpha(*c, 0.12));
            }
            p.galley(pos2(r - w, y - g.size().y / 2.0), g, col);
            r -= w + 8.0;
        }
        // A hairline under the strip, as the glass's printed rule.
        let rule_y = outer.top() + 5.0 + 17.0 - 1.5;
        p.line_segment([pos2(outer.left() + pad_x - 2.0, rule_y), pos2(outer.right() - pad_x + 2.0, rule_y)], Stroke::new(1.0, theme::alpha(lit, 0.07)));
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
pub fn tune_strip(ui: &mut Ui, hz: f64, step: f64, width: f32, height: f32) -> Option<f64> {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click_and_drag());
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

    let hovered = resp.hovered() || resp.dragged();
    paint_wheel(ui, rect, hz, st, offset, hovered);
    resp.on_hover_text("Drag like a dial, or wheel · Shift for a tenth of the step");
    (out != 0.0).then_some(out)
}


/// A knurled thumbwheel seen edge on, set in a slot: a cylinder lit from
/// above, one ridge per tuning step, turning as the frequency moves, from
/// this wheel or anything else, by a ridge's pitch per step. The amber
/// index at the middle is fixed, as on the radio's case.
fn paint_wheel(ui: &Ui, rect: Rect, hz: f64, st: f64, offset: f32, hovered: bool) {
    let p = ui.painter_at(rect.expand(2.0));
    let cr = CornerRadius::same(4);
    // The slot the wheel turns in.
    p.rect_filled(rect.expand(1.0), CornerRadius::same(5), Color32::from_black_alpha(160));
    let body = rect.shrink2(vec2(2.0, 2.0));
    let (cx, r) = (body.center().x, body.width() / 2.0);
    let steel = theme::lerp(theme::surface_3(), theme::text_2(), 0.18);
    // The cylinder: light at its crown, falling off toward its limbs.
    let mut m = egui::Mesh::default();
    let cols = 32;
    for i in 0..=cols {
        let t = i as f32 / cols as f32;
        let th = (t - 0.5) * std::f32::consts::PI;
        let x = cx + r * th.sin();
        let lit = th.cos().powf(0.8);
        let c = theme::lerp(Color32::from_black_alpha(255), steel, 0.25 + 0.75 * lit);
        m.colored_vertex(pos2(x, body.top()), theme::lerp(c, Color32::WHITE, 0.10 * lit));
        m.colored_vertex(pos2(x, body.bottom()), theme::lerp(c, Color32::BLACK, 0.35));
    }
    for i in 0..cols as u32 {
        let (a, b, c, d) = (2 * i, 2 * i + 1, 2 * i + 2, 2 * i + 3);
        m.add_triangle(a, b, c);
        m.add_triangle(c, b, d);
    }
    p.with_clip_rect(body).add(Shape::mesh(m));
    // The ridges: one per step, a groove's shadow and the lit flank beside
    // it, crowding toward the limbs as the surface turns away.
    let cp = p.with_clip_rect(body.shrink2(vec2(0.0, 1.0)));
    // Three ridges a step, so the knurl reads fine; still a step's turn
    // per STRIP_PX dragged.
    const PER_STEP: f64 = 3.0;
    let pitch = STRIP_PX / PER_STEP as f32 / r;
    let steps = hz / st * PER_STEP;
    let turned = (steps.fract() as f32) * pitch - offset / r;
    let base = steps.floor() as i64;
    let n = (std::f32::consts::FRAC_PI_2 / pitch) as i64 + 2;
    for k in -n..=n {
        let th = k as f32 * pitch - turned;
        if th.abs() >= std::f32::consts::FRAC_PI_2 {
            continue;
        }
        let x = cx + r * th.sin();
        let face = th.cos();
        let decade = (base + k).rem_euclid(10 * PER_STEP as i64) == 0;
        cp.line_segment([pos2(x, body.top() + 2.0), pos2(x, body.bottom() - 2.0)], Stroke::new(1.0, Color32::from_black_alpha((150.0 * face) as u8)));
        let flank = x + 1.0 * face;
        let lite = if decade { 70.0 } else { 38.0 } * face;
        cp.line_segment([pos2(flank, body.top() + 2.0), pos2(flank, body.bottom() - 2.0)], Stroke::new(1.0, Color32::from_white_alpha(lite as u8)));
    }
    // A specular band along the crown, and the shading at top and foot.
    let sheen = Rect::from_min_max(pos2(body.left(), body.top()), pos2(body.right(), body.top() + body.height() * 0.35));
    let mut m = egui::Mesh::default();
    m.colored_vertex(sheen.left_top(), Color32::from_white_alpha(if hovered { 30 } else { 18 }));
    m.colored_vertex(sheen.right_top(), Color32::from_white_alpha(if hovered { 30 } else { 18 }));
    m.colored_vertex(sheen.left_bottom(), Color32::TRANSPARENT);
    m.colored_vertex(sheen.right_bottom(), Color32::TRANSPARENT);
    m.add_triangle(0, 1, 2);
    m.add_triangle(2, 1, 3);
    cp.add(Shape::mesh(m));
    // The limbs fall into the slot's shadow.
    for (x0, x1, a0, a1) in [(body.left(), body.left() + r * 0.35, 200u8, 0u8), (body.right() - r * 0.35, body.right(), 0, 200)] {
        let mut m = egui::Mesh::default();
        m.colored_vertex(pos2(x0, body.top()), Color32::from_black_alpha(a0));
        m.colored_vertex(pos2(x1, body.top()), Color32::from_black_alpha(a1));
        m.colored_vertex(pos2(x0, body.bottom()), Color32::from_black_alpha(a0));
        m.colored_vertex(pos2(x1, body.bottom()), Color32::from_black_alpha(a1));
        m.add_triangle(0, 1, 2);
        m.add_triangle(2, 1, 3);
        p.with_clip_rect(body).add(Shape::mesh(m));
    }
    p.rect_stroke(body, cr, Stroke::new(1.0, Color32::from_black_alpha(180)), egui::StrokeKind::Outside);
    // The index: fixed, amber, notched into the slot top and foot.
    let ix = rect.center().x;
    p.add(Shape::convex_polygon(vec![pos2(ix - 3.5, rect.top() - 1.0), pos2(ix + 3.5, rect.top() - 1.0), pos2(ix, rect.top() + 4.0)], theme::AMBER, Stroke::NONE));
    p.add(Shape::convex_polygon(vec![pos2(ix - 3.5, rect.bottom() + 1.0), pos2(ix, rect.bottom() - 4.0), pos2(ix + 3.5, rect.bottom() + 1.0)], theme::AMBER, Stroke::NONE));
    p.line_segment([pos2(ix, rect.top() + 3.0), pos2(ix, rect.bottom() - 3.0)], Stroke::new(1.0, theme::alpha(theme::AMBER, 0.55)));
}

/// One seven-segment digit's size: its height and width, before the slant.
struct Seg {
    h: f32,
    w: f32,
}

/// Segments a–g lit for each figure, a the top, then clockwise, g the middle.
const SEGMENTS: [u8; 10] = [0b0111111, 0b0000110, 0b1011011, 0b1001111, 0b1100110, 0b1101101, 0b1111101, 0b0000111, 0b1111111, 0b1101111];

/// The forward slant of an LCD's figures, as a run over rise.
const SLANT: f32 = 0.07;

impl Seg {
    /// The stroke of a segment.
    fn t(&self) -> f32 {
        (self.h * 0.12).max(2.0)
    }

    /// The seven segments as hexagons, slanted about the baseline, in
    /// order a–g, with `o` the digit's top-left before the slant.
    fn shapes(&self, o: Pos2) -> [[Pos2; 6]; 7] {
        let (w, h, t) = (self.w, self.h, self.t());
        let g = t * 0.16;
        let half = t / 2.0;
        let horiz = |y: f32| {
            let (a, b) = (half + g, w - half - g);
            [pos2(a, y), pos2(a + half, y - half), pos2(b - half, y - half), pos2(b, y), pos2(b - half, y + half), pos2(a + half, y + half)]
        };
        let vert = |x: f32, y0: f32, y1: f32| {
            let (a, b) = (y0 + g, y1 - g);
            [pos2(x, a), pos2(x + half, a + half), pos2(x + half, b - half), pos2(x, b), pos2(x - half, b - half), pos2(x - half, a + half)]
        };
        let mid = h / 2.0;
        let raw = [
            horiz(half),
            vert(w - half, half, mid),
            vert(w - half, mid, h - half),
            horiz(h - half),
            vert(half, mid, h - half),
            vert(half, half, mid),
            horiz(mid),
        ];
        raw.map(|seg| seg.map(|p| pos2(o.x + p.x + (h - p.y) * SLANT, o.y + p.y)))
    }

    /// The digit with its unlit segments as a ghost; `None` draws the ghost
    /// alone, for a leading zero.
    fn paint(&self, p: &egui::Painter, o: Pos2, d: Option<u32>, color: Color32) {
        let ghost = theme::alpha(color, 0.07);
        let mask = d.map_or(0, |d| SEGMENTS[d as usize % 10]);
        for (k, seg) in self.shapes(o).into_iter().enumerate() {
            if mask & (1 << k) == 0 {
                p.add(Shape::convex_polygon(seg.to_vec(), ghost, Stroke::NONE));
            }
        }
        self.paint_lit(p, o, d, color);
    }

    /// The lit segments alone, each with a soft halo, as backlit glass
    /// spreads it.
    fn paint_lit(&self, p: &egui::Painter, o: Pos2, d: Option<u32>, color: Color32) {
        let Some(d) = d else { return };
        let mask = SEGMENTS[d as usize % 10];
        let shapes = self.shapes(o);
        let halo = Stroke::new(self.t() * 0.9, theme::alpha(color, 0.10));
        for (k, seg) in shapes.iter().enumerate() {
            if mask & (1 << k) != 0 {
                p.add(Shape::convex_polygon(seg.to_vec(), Color32::TRANSPARENT, halo));
            }
        }
        for (k, seg) in shapes.iter().enumerate() {
            if mask & (1 << k) != 0 {
                p.add(Shape::convex_polygon(seg.to_vec(), color, Stroke::NONE));
            }
        }
    }
}

/// A vertical gradient over `r`, `top` to `bottom`.
fn vgrad(p: &egui::Painter, r: Rect, top: Color32, bottom: Color32) {
    let mut m = egui::Mesh::default();
    m.colored_vertex(r.left_top(), top);
    m.colored_vertex(r.right_top(), top);
    m.colored_vertex(r.left_bottom(), bottom);
    m.colored_vertex(r.right_bottom(), bottom);
    m.add_triangle(0, 1, 2);
    m.add_triangle(2, 1, 3);
    p.add(Shape::mesh(m));
}

/// The display behind the digits: a bezel cut into the panel, and a panel
/// of dark glass lit from behind in the receiver's color, brightest at its
/// middle as a backlight falls off toward the frame.
pub fn lcd_back(p: &egui::Painter, r: Rect, tint: Color32) {
    let cr = CornerRadius::same(5);
    // The bezel: a dark lip outside the glass and a catch of light below.
    let bezel = r.expand(2.0);
    p.rect_filled(bezel, CornerRadius::same(7), Color32::from_black_alpha(150));
    p.line_segment([bezel.left_bottom() + vec2(6.0, 0.5), bezel.right_bottom() + vec2(-6.0, 0.5)], Stroke::new(1.0, Color32::from_white_alpha(16)));
    let base = theme::lerp(theme::ink(), Color32::BLACK, 0.35);
    p.rect_filled(r, cr, base);
    // The backlight: a wash of the tint, an ellipse fading from the middle.
    let mut m = egui::Mesh::default();
    let c = r.center();
    m.colored_vertex(c, theme::alpha(tint, 0.11));
    let n = 40;
    for k in 0..=n {
        let a = k as f32 / n as f32 * std::f32::consts::TAU;
        m.colored_vertex(pos2(c.x + a.cos() * r.width() * 0.56, c.y + a.sin() * r.height() * 0.95), Color32::TRANSPARENT);
    }
    for k in 1..=n as u32 {
        m.add_triangle(0, k, k + 1);
    }
    p.with_clip_rect(r.shrink(1.0)).add(Shape::mesh(m));
}

/// The glass over the digits: a frosted sheen across the top, a fine row
/// texture like an LCD's cells, a shadow under the bezel's upper lip, and
/// the glass's own edge.
pub fn lcd_glass(p: &egui::Painter, r: Rect) {
    let cp = p.with_clip_rect(r.shrink(1.0));
    // Rows: a faint dark line every third point.
    let mut y = r.top() + 2.0;
    while y < r.bottom() - 1.0 {
        cp.line_segment([pos2(r.left(), y), pos2(r.right(), y)], Stroke::new(1.0, Color32::from_black_alpha(18)));
        y += 3.0;
    }
    // The lip's shadow, then the frost: light caught in the upper half,
    // gone by the middle, and a brighter band where the glass curves.
    vgrad(&cp, Rect::from_min_size(r.min, vec2(r.width(), 6.0)), Color32::from_black_alpha(120), Color32::TRANSPARENT);
    vgrad(&cp, Rect::from_min_max(pos2(r.left(), r.top() + 1.0), pos2(r.right(), r.center().y)), Color32::from_white_alpha(16), Color32::TRANSPARENT);
    // A slanted sheen across the left of the glass.
    let w = r.width();
    let sheen = [pos2(r.left() + w * 0.08, r.top()), pos2(r.left() + w * 0.22, r.top()), pos2(r.left() + w * 0.12, r.bottom()), pos2(r.left() - w * 0.02, r.bottom())];
    let mut m = egui::Mesh::default();
    m.colored_vertex(sheen[0], Color32::from_white_alpha(9));
    m.colored_vertex(sheen[1], Color32::from_white_alpha(9));
    m.colored_vertex(sheen[2], Color32::TRANSPARENT);
    m.colored_vertex(sheen[3], Color32::TRANSPARENT);
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    cp.add(Shape::mesh(m));
    let cr = CornerRadius::same(5);
    p.rect_stroke(r, cr, Stroke::new(1.0, Color32::from_white_alpha(22)), egui::StrokeKind::Inside);
    p.line_segment([r.left_top() + vec2(5.0, 1.5), r.right_top() + vec2(-5.0, 1.5)], Stroke::new(1.0, Color32::from_white_alpha(26)));
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
                t.vfo.ui(ui, hz, 46.0, true, &mut t.acts);
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
                if let Some(d) = tune_strip(ui, 146.52e6, 6_250.0, 220.0, 40.0) {
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
