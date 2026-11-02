//! The innerpulse mark, its two motions, and uSDR's take on it: the mark as
//! six carriers on a panadapter, received into a waterfall.
//!
//! Geometry and timing follow the brand kit (`scripts/pulse.mjs`): six bars
//! 22u wide with 4u corners, spread over x = −92…92 in a 220u box, heights
//! 12 · 76.74 · 56.21 · 42 · 82.16 · 12. The mark draws in once (each bar from
//! its 12u stub to its rest, 0.55 s eased out, the next 0.09 s behind) and
//! then beats: every period one pulse passes left to right, each bar rising
//! 16u with a sin² bump. It moves only from 48 px and never under reduced
//! motion.

use crate::theme;
use egui::{Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, pos2, vec2};

pub const HEIGHTS: [f32; 6] = [12.0, 76.74, 56.21, 42.0, 82.16, 12.0];
const PITCH: f32 = 36.8;
const BAR_W: f32 = 22.0;
const STUB: f32 = 12.0;

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

/// Bar heights at `t` seconds after the mark first appeared.
pub fn heights_at(t: f32, period: f32, moving: bool) -> [f32; 6] {
    let mut h = HEIGHTS;
    if !moving {
        return h;
    }
    for (i, hi) in h.iter_mut().enumerate() {
        let local = t - 0.09 * i as f32;
        if local < 0.55 {
            *hi = rest(STUB + (HEIGHTS[i] - STUB) * ease_out_cubic(local / 0.55));
            continue;
        }
        if period > 0.0 {
            let since = t - 1.1; // the beat starts once the reading is taken
            if since > 0.0 {
                let u = ((since % period) - i as f32 * 0.0245 * period) / (0.19 * period);
                if u > 0.0 && u < 1.0 {
                    *hi += 16.0 * (std::f32::consts::PI * u).sin().powi(2);
                }
            }
        }
    }
    h
}

/// Paint the mark centered in `rect` (square-ish), at heights `h`.
pub fn paint_mark(painter: &Painter, rect: Rect, h: [f32; 6], color: Color32) {
    let size = rect.width().min(rect.height());
    let s = size / 220.0;
    let c = rect.center();
    let corner = (4.0 * s).round().clamp(0.0, 255.0) as u8;
    for (i, hi) in h.iter().enumerate() {
        let x = -92.0 + PITCH * i as f32;
        let r = Rect::from_min_max(
            pos2(c.x + (x - BAR_W / 2.0) * s, c.y - (hi + 11.0) * s),
            pos2(c.x + (x + BAR_W / 2.0) * s, c.y + (hi + 11.0) * s),
        );
        painter.rect_filled(r, CornerRadius::same(corner), color);
    }
}

/// A live mark: draws in once, then beats while `moving`.
pub fn mark(ui: &mut egui::Ui, size: f32, period: f32, reduce_motion: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), egui::Sense::hover());
    let now = ui.input(|i| i.time);
    let key = resp.id.with("born");
    let born = ui.ctx().data_mut(|d| *d.get_temp_mut_or_insert_with(key, move || now));
    let age = (ui.input(|i| i.time) - born) as f32;
    let moving = size >= 48.0 && !reduce_motion;
    paint_mark(ui.painter(), rect, heights_at(age, period, moving), theme::EMBER);
    if moving && (age < 1.2 || period > 0.0) {
        crate::anim::more(ui.ctx());
    }
    resp
}

/// The static mark at text height, for the credit and the masthead.
pub fn mark_static(painter: &Painter, rect: Rect) {
    paint_mark(painter, rect, HEIGHTS, theme::EMBER);
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
        painter.line_segment(
            [pos2(rect.left(), split), pos2(rect.right(), split)],
            Stroke::new(1.0, theme::line_2()),
        );
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

/// The quiet credit: a 12 px static mark and the word, muted.
pub fn credit(ui: &mut egui::Ui) {
    let galley = ui.painter().layout_no_wrap(
        "innerpulse".into(),
        theme::mono(11.0),
        theme::text_3(),
    );
    let (rect, resp) = ui.allocate_exact_size(
        vec2(12.0 + 5.0 + galley.size().x, galley.size().y.max(12.0)),
        egui::Sense::click(),
    );
    let mark = Rect::from_center_size(pos2(rect.left() + 6.0, rect.center().y), vec2(12.0, 12.0));
    mark_static(ui.painter(), mark);
    let color = if resp.hovered() { theme::text_2() } else { theme::text_3() };
    ui.painter().text(
        pos2(mark.right() + 5.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        "innerpulse",
        theme::mono(11.0),
        color,
    );
    if resp.on_hover_text("An innerpulse project").clicked() {
        ui.ctx().open_url(egui::OpenUrl::new_tab("https://www.innerpulse.net/portfolio/#p-usdr"));
    }
}
