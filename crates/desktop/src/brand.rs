//! The innerpulse mark and its motion, and the connect screen's splash (six
//! carriers on a panadapter received into a waterfall).
//!
//! The mark is the plate: the beat cut out of a solid ember tile. Geometry and
//! timing follow the brand kit (`scripts/pulse.mjs`). The beat is six bars 22u
//! wide with 4u corners, spread over x = −92…92 in a 220u box, heights
//! 12 · 82.22 · 51.83 · 33.34 · 56.13 · 12 (one damped beat: the strike, then
//! its echo), each from −h − 11 to h + 11. The plate is a 200u square with 26u
//! corners, the bars cut through it at 0.74.
//!
//! The plate never moves; the motion is light. When the mark first appears
//! its slots open once (each grows from the 12u stub to its rest, 0.55 s eased
//! out, the next 0.09 s behind), then every period one pulse of light passes
//! through them left to right: a lamp in each slot whose brightness is the
//! beat's lift over its rest, a sin² bump. Between beats the slots are dark.
//! It moves only from 48 px and never under reduced motion.
//!
//! Under 32 px rendered the plate's slots are too fine to read, so there the
//! mark is the small mark: the six bars in ember on a clear ground (the
//! masthead lockup, the title bar, the strip and the credit).

use crate::theme;
use egui::{Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, pos2, vec2};

pub const HEIGHTS: [f32; 6] = [12.0, 82.2191, 51.8333, 33.3368, 56.1325, 12.0];
const PITCH: f32 = 36.8;
const BAR_W: f32 = 22.0;
const STUB: f32 = 12.0;
/// The beat's lift as it passes a bar, in mark units.
const RISE: f32 = 16.0;
/// The plate: a 200u square with 26u corners, the bars cut at 0.74.
const PLATE: f32 = 200.0;
const PLATE_CORNER: f32 = 26.0;
const CUT: f32 = 0.74;
/// The lamp in a lit slot (`--innerpulse-hot`).
pub const HOT: Color32 = Color32::from_rgb(0xff, 0xd9, 0xc2);

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// The 12u floor with the kit's soft landing between 12 and 30.
fn rest(v: f32) -> f32 {
    if v <= STUB {
        return STUB;
    }
    if v >= 30.0 {
        return v;
    }
    let w = 30.0 - STUB;
    let t = (v - STUB) / w;
    (2.0 * t.powi(3) - 3.0 * t.powi(2) + 1.0) * STUB
        + (-2.0 * t.powi(3) + 3.0 * t.powi(2)) * 30.0
        + (t.powi(3) - t.powi(2)) * w
}

/// How far the slots have opened `t` seconds after the mark appeared.
fn opening(t: f32) -> [f32; 6] {
    let mut h = HEIGHTS;
    for (i, hi) in h.iter_mut().enumerate() {
        let u = (t - 0.09 * i as f32) / 0.55;
        if u < 1.0 {
            *hi = rest(STUB + (HEIGHTS[i] - STUB) * ease_out_cubic(u));
        }
    }
    h
}

/// The beat's lift over each bar's rest at `t` seconds into a beat cycle
/// that repeats every `period`, 0 … RISE.
fn lift(t: f32, period: f32) -> [f32; 6] {
    let mut out = [0.0; 6];
    if period <= 0.0 || t <= 0.0 {
        return out;
    }
    for (i, o) in out.iter_mut().enumerate() {
        let u = ((t % period) - i as f32 * 0.0245 * period) / (0.19 * period);
        if u > 0.0 && u < 1.0 {
            *o = RISE * (std::f32::consts::PI * u).sin().powi(2);
        }
    }
    out
}

/// The beat drawn as height at `t` seconds after it appeared: the opening,
/// then the pulse lifting each bar. For the splash, which plots the beat as a
/// reading; the mark itself shows it as light (`plate_at`).
pub fn heights_at(t: f32, period: f32, moving: bool) -> [f32; 6] {
    if !moving {
        return HEIGHTS;
    }
    let mut h = opening(t);
    let l = lift(t - 1.1, period);
    for i in 0..6 {
        h[i] += l[i];
    }
    h
}

/// The plate at `t` seconds after it appeared: the slots' heights (opening)
/// and each lamp's brightness 0 … 1 (the beat, once the slots are open).
pub fn plate_at(t: f32, period: f32, moving: bool) -> ([f32; 6], [f32; 6]) {
    if !moving {
        return (HEIGHTS, [0.0; 6]);
    }
    // The light starts once the slots are open (about a second).
    (opening(t), lift(t - 1.1, period).map(|l| l / RISE))
}

/// Paint the flat plate filling `rect`'s largest centered square: one ember
/// tile with the six slots cut through it. The slots are dark (the ground
/// shows through); `lit` lights each one's lamp, 0 … 1.
pub fn paint_plate(painter: &Painter, rect: Rect, h: [f32; 6], lit: [f32; 6]) {
    let size = rect.width().min(rect.height());
    let ppp = painter.pixels_per_point();
    if (size * ppp).round() < 48.0 {
        paint_plate_snapped(painter, rect, ppp);
        return;
    }
    let s = size / PLATE;
    let c = rect.center();
    let plate = Rect::from_center_size(c, vec2(size, size));
    painter.rect_filled(plate, CornerRadius::same((PLATE_CORNER * s).round().clamp(0.0, 255.0) as u8), theme::EMBER);
    let k = s * CUT;
    let corner = CornerRadius::same((4.0 * k).round().clamp(0.0, 255.0) as u8);
    for (i, hi) in h.iter().enumerate() {
        let x = -92.0 + PITCH * i as f32;
        let slot = Rect::from_min_max(
            pos2(c.x + (x - BAR_W / 2.0) * k, c.y - (hi + 11.0) * k),
            pos2(c.x + (x + BAR_W / 2.0) * k, c.y + (hi + 11.0) * k),
        );
        painter.rect_filled(slot, corner, theme::ink());
        if lit[i] > 0.0 {
            painter.rect_filled(slot, corner, theme::alpha(HOT, lit[i].min(1.0)));
        }
    }
}

/// The plate at rest on the screen's own pixel grid, as the kit lays it out
/// for 16–48 px (`snappedBars` in pulse.mjs): a whole-pixel pitch, slots half
/// the pitch wide with square ends, whole-pixel heights, so the slots stay
/// sharp where the vector would blur them into the tile.
fn paint_plate_snapped(painter: &Painter, rect: Rect, ppp: f32) {
    let n = (rect.width().min(rect.height()) * ppp).round();
    let at = ((rect.center() - vec2(n, n) / ppp / 2.0) * ppp).round();
    let px = |x: f32, y: f32| pos2(at.x + x, at.y + y) / ppp;
    let corner = (n * PLATE_CORNER / PLATE).round().max(2.0) / ppp;
    painter.rect_filled(Rect::from_min_max(px(0.0, 0.0), px(n, n)), CornerRadius::same(corner.round() as u8), theme::EMBER);
    let pitch = (n * 0.147).floor().max(2.0);
    let w = (pitch / 2.0).ceil();
    let unit = pitch / PITCH;
    let first = n / 2.0 - pitch * 5.0 / 2.0;
    for (j, hj) in HEIGHTS.iter().enumerate() {
        let c = first + pitch * j as f32;
        let cx = if w % 2.0 == 1.0 { c.floor() + 0.5 } else { c.round() };
        let half = (hj * unit + w / 2.0).round().max(w);
        let x = cx - w / 2.0;
        painter.rect_filled(Rect::from_min_max(px(x, n / 2.0 - half), px(x + w, n / 2.0 + half)), 0.0, theme::ink());
    }
}

/// A live mark: the slots open once, then light passes through them every
/// `period` seconds. Holds still under 48 px and under reduced motion.
pub fn mark(ui: &mut egui::Ui, size: f32, period: f32, reduce_motion: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), egui::Sense::hover());
    let now = ui.input(|i| i.time);
    let key = resp.id.with("born");
    let born = ui.ctx().data_mut(|d| *d.get_temp_mut_or_insert_with(key, move || now));
    let age = (now - born) as f32;
    let moving = size >= 48.0 && !reduce_motion;
    if (size * ui.ctx().pixels_per_point()).round() < PLATE_FROM {
        paint_small(ui.painter(), rect, theme::EMBER);
        return resp;
    }
    let (h, lit) = plate_at(age, period, moving);
    paint_plate(ui.painter(), rect, h, lit);
    if moving && (age < 1.2 || period > 0.0) {
        crate::anim::more(ui.ctx());
    }
    resp
}

/// Rendered size, in pixels, from which the mark is the plate; below it the
/// plate's slots are about a pixel wide and it reads as a red square, so the
/// mark is the small mark (`PLATE.plateFrom` in the kit).
const PLATE_FROM: f32 = 32.0;

/// The mark at rest, by its rendered size: the plate from 32 px, the small
/// mark below (the title bar, the strip and the credit).
pub fn mark_static(painter: &Painter, rect: Rect) {
    let size = rect.width().min(rect.height());
    if (size * painter.pixels_per_point()).round() < PLATE_FROM {
        paint_small(painter, rect, theme::EMBER);
    } else {
        paint_plate(painter, rect, HEIGHTS, [0.0; 6]);
    }
}

/// The small mark: the beat itself, the six bars in one color on a clear
/// ground, on the 220u box that fits `rect`. No tile, box or outline. The
/// pitch and bar width land on whole pixels, so each bar stays a crisp
/// column of its own; the ends sit where the geometry puts them.
pub fn paint_small(painter: &Painter, rect: Rect, color: Color32) {
    let ppp = painter.pixels_per_point();
    let s = rect.width().min(rect.height()) / 220.0 * ppp; // pixels per mark unit
    let pitch = (PITCH * s).round().max(2.0);
    let w = (BAR_W * s).round().clamp(1.0, pitch - 1.0);
    let c = pos2(rect.center().x * ppp, rect.center().y * ppp);
    let left = (c.x - (5.0 * pitch + w) / 2.0).round();
    let corner = if w >= 4.0 { (4.0 * s).round().min(w / 2.0) } else { 0.0 };
    for (i, hi) in HEIGHTS.iter().enumerate() {
        let x = left + pitch * i as f32;
        let half = (hi + 11.0) * s;
        let r = Rect::from_min_max(pos2(x, (c.y - half).round()), pos2(x + w, (c.y + half).round()));
        let r = Rect::from_min_max(pos2(r.min.x / ppp, r.min.y / ppp), pos2(r.max.x / ppp, r.max.y / ppp));
        painter.rect_filled(r, CornerRadius::same((corner / ppp).round() as u8), color);
    }
}

/// Syne's cap height, as a fraction of the font size.
const SYNE_CAP: f32 = 0.65;
/// The strike's full height in the 220u box: (82.22 + 11) · 2.
const STRIKE_SPAN: f32 = (82.2191 + 11.0) * 2.0;

/// The small lockup, for a name set under about 40 px: the bare bars, the
/// strike running from the top of the name's capitals to its baseline so the
/// mark is exactly as tall as the name, then the name in steel, 0.3 of the
/// font size apart.
pub fn lockup(ui: &mut egui::Ui, name: &str, size: f32, sense: egui::Sense) -> egui::Response {
    let font = theme::wordmark(size);
    let galley = ui.painter().layout_no_wrap(name.to_owned(), font.clone(), Color32::PLACEHOLDER);
    let base = galley
        .rows
        .first()
        .and_then(|r| r.row.glyphs.first().map(|g| r.pos.y + g.pos.y))
        .unwrap_or(galley.size().y * 0.8);
    let cap = (SYNE_CAP * size).round();
    // The 220u box whose strike spans the capitals; the bars take 206u of
    // its width.
    let boxed = cap * 220.0 / STRIKE_SPAN;
    let mark_w = boxed * 206.0 / 220.0;
    let gap = (0.3 * size).round();
    let (r, resp) = ui.allocate_exact_size(vec2(mark_w + gap + galley.size().x, galley.size().y), sense);
    let mid = pos2(r.left() + mark_w / 2.0, r.top() + base - cap / 2.0);
    paint_small(ui.painter(), Rect::from_center_size(mid, vec2(boxed, boxed)), theme::EMBER);
    theme::steel_text(ui.painter(), pos2(r.left() + mark_w + gap, r.top()), egui::Align2::LEFT_TOP, name, font);
    resp
}

// ---------- the receive splash ----------

/// The connect screen's backdrop: a panadapter whose six carriers sit where
/// the mark's bars do, and a waterfall the carriers write into as they come
/// up. It is decoration, so it carries no scale and no numbers.
pub struct Splash {
    rows: Vec<[f32; 160]>,
    tex: Option<egui::TextureHandle>,
    seed: u64,
    last_row: f64,
    born: Option<f64>,
}

const SPLASH_W: usize = 160;
const SPLASH_H: usize = 96;

impl Default for Splash {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            tex: None,
            seed: 0x9e37_79b9_7f4a_7c15,
            last_row: 0.0,
            born: None,
        }
    }
}

impl Splash {
    fn noise(&mut self) -> f32 {
        // xorshift: cheap and good enough for a noise floor.
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        (self.seed >> 40) as f32 / (1u64 << 24) as f32
    }

    /// One spectrum line at `age`: a noise floor and six carriers whose
    /// levels follow the mark's heights as it draws in and beats.
    fn line(&mut self, age: f32, moving: bool) -> [f32; SPLASH_W] {
        let h = heights_at(age, 4.5, moving);
        let mut out = [0f32; SPLASH_W];
        for (x, v) in out.iter_mut().enumerate() {
            let n = self.noise();
            *v = 0.10 + 0.07 * n;
            let fx = x as f32 / (SPLASH_W - 1) as f32 * 220.0 - 110.0;
            for (i, hi) in h.iter().enumerate() {
                let cx = -92.0 + PITCH * i as f32;
                let d = (fx - cx) / 5.5;
                *v += (hi / 98.0) * 0.85 * (-d * d).exp();
            }
        }
        out
    }

    pub fn paint(&mut self, ui: &mut egui::Ui, rect: Rect, reduce_motion: bool) {
        let now = ui.input(|i| i.time);
        let born = *self.born.get_or_insert(now);
        let age = (now - born) as f32;
        let moving = !reduce_motion;
        if self.rows.is_empty() {
            for _ in 0..SPLASH_H {
                let l = self.line(0.0, false);
                self.rows.push(l.map(|v| v * 0.35));
            }
        }
        if moving && now - self.last_row > 1.0 / 30.0 {
            self.last_row = now;
            let l = self.line(age, true);
            self.rows.insert(0, l);
            self.rows.truncate(SPLASH_H);
        }
        let mut px = Vec::with_capacity(SPLASH_W * SPLASH_H);
        for row in &self.rows {
            for &v in row.iter() {
                px.push(crate::colormap::Colormap::Ember.sample(v));
            }
        }
        let img = egui::ColorImage::new([SPLASH_W, SPLASH_H], px);
        let tex = self.tex.get_or_insert_with(|| {
            ui.ctx()
                .load_texture("splash", img.clone(), egui::TextureOptions::LINEAR)
        });
        tex.set(img, egui::TextureOptions::LINEAR);

        let painter = ui.painter_at(rect);
        let split = rect.top() + rect.height() * 0.42;
        let pan = Rect::from_min_max(rect.min, pos2(rect.right(), split));
        let wf = Rect::from_min_max(pos2(rect.left(), split), rect.max);
        painter.image(
            tex.id(),
            wf,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::from_white_alpha(150),
        );
        // Fade the waterfall into the ground at the bottom.
        let fade = egui::Mesh {
            indices: vec![0, 1, 2, 2, 1, 3],
            vertices: vec![
                vtx(wf.left_top(), Color32::TRANSPARENT),
                vtx(wf.right_top(), Color32::TRANSPARENT),
                vtx(wf.left_bottom(), theme::ink()),
                vtx(wf.right_bottom(), theme::ink()),
            ],
            texture_id: egui::TextureId::default(),
        };
        painter.add(Shape::mesh(fade));

        // The newest line as a trace over the panadapter.
        let line = self.rows.first().copied().unwrap_or([0.0; SPLASH_W]);
        let pts: Vec<Pos2> = line
            .iter()
            .enumerate()
            .map(|(i, v)| {
                pos2(
                    pan.left() + i as f32 / (SPLASH_W - 1) as f32 * pan.width(),
                    pan.bottom() - v.min(1.0) * pan.height() * 0.9,
                )
            })
            .collect();
        crate::spectrum::fill_under(&painter, &pts, pan.bottom(), theme::AMBER, 0.22);
        painter.add(Shape::line(pts, Stroke::new(1.4, theme::alpha(theme::AMBER, 0.85))));
        // The panadapter settles into the waterfall through a short shade
        // rather than a hard rule.
        let shade = egui::Mesh {
            indices: vec![0, 1, 2, 2, 1, 3],
            vertices: vec![
                vtx(pos2(rect.left(), split), theme::alpha(theme::ink(), 0.85)),
                vtx(pos2(rect.right(), split), theme::alpha(theme::ink(), 0.85)),
                vtx(pos2(rect.left(), split + 22.0), Color32::TRANSPARENT),
                vtx(pos2(rect.right(), split + 22.0), Color32::TRANSPARENT),
            ],
            texture_id: egui::TextureId::default(),
        };
        painter.add(Shape::mesh(shade));
        if moving {
            crate::anim::more(ui.ctx());
        }
    }
}

pub fn vtx(pos: Pos2, color: Color32) -> egui::epaint::Vertex {
    egui::epaint::Vertex {
        pos,
        uv: egui::epaint::WHITE_UV,
        color,
    }
}

/// The quiet credit: the small mark at 14 px, static, and the word, muted.
pub fn credit(ui: &mut egui::Ui) {
    let galley = ui.painter().layout_no_wrap(
        "innerpulse".into(),
        theme::mono(crate::theme::ty::SMALL),
        theme::text_3(),
    );
    let (rect, resp) = ui.allocate_exact_size(
        vec2(14.0 + 5.0 + galley.size().x, galley.size().y.max(14.0)),
        egui::Sense::click(),
    );
    let mark = Rect::from_center_size(pos2(rect.left() + 7.0, rect.center().y), vec2(14.0, 14.0));
    mark_static(ui.painter(), mark);
    let color = if resp.hovered() { theme::text_2() } else { theme::text_3() };
    ui.painter().text(
        pos2(mark.right() + 5.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        "innerpulse",
        theme::mono(crate::theme::ty::SMALL),
        color,
    );
    if resp.on_hover_text("An innerpulse project").clicked() {
        ui.ctx().open_url(egui::OpenUrl::new_tab("https://www.innerpulse.net/portfolio/#p-usdr"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mark where the window draws it: the small mark for the credit
    /// (14), the title bar (18) and the strip (20); the plate from 32, the
    /// reconnect (48) and connect (56) marks, two larger, a lit frame of the
    /// beat; and the masthead's small lockup; on the plate.
    /// `USDR_QA_DIR=<dir> cargo test -p usdr-desktop qa_renders_the_marks --
    /// --ignored`.
    #[test]
    #[ignore]
    fn qa_renders_the_marks() {
        let Some(dir) = std::env::var_os("USDR_QA_DIR").map(std::path::PathBuf::from) else { return };
        let sizes = [14.0, 18.0, 20.0, 32.0, 48.0, 56.0, 96.0, 160.0];
        let mut ready = false;
        let mut h = egui_kittest::Harness::builder().with_size(vec2(920.0, 240.0)).wgpu().build_ui(move |ui| {
            if !ready {
                crate::theme::install(ui.ctx(), crate::theme::Ground::Carbon);
                ready = true;
                return;
            }
            crate::theme::paint_plate(ui.painter(), ui.max_rect(), crate::theme::plate());
            let mut x = ui.max_rect().left() + 16.0;
            let y = ui.max_rect().top() + 16.0;
            for s in sizes {
                mark_static(ui.painter(), Rect::from_min_size(pos2(x, y), vec2(s, s)));
                x += s + 16.0;
            }
            // One frame of the beat, the light in the strike and the next slot.
            let (hh, lit) = plate_at(1.1 + 0.35 * 4.5 * 0.19 + 0.2, 4.5, true);
            paint_plate(ui.painter(), Rect::from_min_size(pos2(x, y), vec2(160.0, 160.0)), hh, lit);
            ui.put(Rect::from_min_size(pos2(ui.max_rect().left() + 16.0, y + 180.0), vec2(300.0, 30.0)), |ui: &mut egui::Ui| {
                ui.horizontal_centered(|ui| lockup(ui, "uSDR", 20.0, egui::Sense::hover())).inner
            });
        });
        h.run_steps(3);
        h.render().expect("render").save(dir.join("marks.png")).expect("save");
    }
}
