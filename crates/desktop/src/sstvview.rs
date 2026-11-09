//! Slow-scan television pictures: the color rows the server sends (0x07
//! frames) gathered into pictures, the newest drawn as it arrives with the
//! session's others in a strip under it, and saved as PNG beside the
//! recordings.

use crate::app::App;
use crate::theme::{self, alpha};
use crate::widgets::{self, Tone};
use egui::{CornerRadius, RichText, Sense, Stroke, Ui, pos2, vec2};
use serde_json::json;
use std::collections::VecDeque;
use std::time::SystemTime;
use usdr_proto::{ImageLine, Mode};

/// Pictures kept this session, oldest dropped first.
const KEEP: usize = 24;
/// Rows a picture may hold; the tallest SSTV mode has 616.
const MAX_ROWS: usize = 800;

pub struct Picture {
    pub id: u32,
    pub width: usize,
    /// Three bytes a pixel, row after row; rows not received are gray.
    rgb: Vec<u8>,
    pub rows: usize,
    pub started: SystemTime,
    pub freq_hz: f64,
    pub mode: String,
    texture: Option<egui::TextureHandle>,
    /// The texture's height: the rows grown into by doubling.
    tex_rows: usize,
    /// The first row changed since the texture was last brought up to
    /// date; MAX when none has.
    dirty_from: usize,
    pub saved: Option<std::path::PathBuf>,
}

impl Picture {
    fn new(id: u32, width: usize, freq_hz: f64) -> Self {
        Picture { id, width, rgb: Vec::new(), rows: 0, started: SystemTime::now(), freq_hz, mode: String::new(), texture: None, tex_rows: 0, dirty_from: usize::MAX, saved: None }
    }

    fn push(&mut self, line: usize, rgb: &[u8]) {
        if rgb.len() != 3 * self.width || line >= MAX_ROWS {
            return;
        }
        // Rows skipped on the way are gray, and changed with this one.
        self.dirty_from = self.dirty_from.min(line.min(self.rows));
        if line >= self.rows {
            self.rgb.resize(3 * self.width * (line + 1), 96);
            self.rows = line + 1;
        }
        self.rgb[3 * self.width * line..3 * self.width * (line + 1)].copy_from_slice(rgb);
    }

    /// Bring the texture up to the rows received: grow it by doubling, and
    /// upload only the rows that changed otherwise. Uploading the whole
    /// picture for every row was a megabyte or two a row by the end of a
    /// tall one. The row under the last is a copy of it, so the filtered
    /// edge looks as it did when the texture ended there.
    fn texture(&mut self, ctx: &egui::Context) -> Option<&egui::TextureHandle> {
        if self.rows == 0 {
            return None;
        }
        let opts = egui::TextureOptions::LINEAR;
        let row = 3 * self.width;
        // Rows `from..rows` and the copy of the last after them, up to `to`.
        let image = |from: usize, to: usize| {
            let mut px = self.rgb[row * from..row * self.rows].to_vec();
            for _ in self.rows..to {
                px.extend_from_slice(&self.rgb[row * (self.rows - 1)..row * self.rows]);
            }
            egui::ColorImage::from_rgb([self.width, to - from], &px)
        };
        if self.texture.is_none() || self.rows > self.tex_rows {
            self.tex_rows = self.rows.next_power_of_two().max(64);
            let img = image(0, self.tex_rows);
            match &mut self.texture {
                Some(t) => t.set(img, opts),
                None => self.texture = Some(ctx.load_texture(format!("sstv-{}", self.id), img, opts)),
            }
        } else if self.dirty_from < self.rows {
            let img = image(self.dirty_from, (self.rows + 1).min(self.tex_rows));
            if let Some(t) = &mut self.texture {
                t.set_partial([0, self.dirty_from], img, opts);
            }
        }
        self.dirty_from = usize::MAX;
        self.texture.as_ref()
    }

    /// The part of the texture the picture fills.
    fn uv(&self) -> egui::Rect {
        egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, self.rows as f32 / self.tex_rows.max(1) as f32))
    }

    fn file_stem(&self) -> String {
        let secs = self.started.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let (d, t) = crate::app::utc_date_time(secs);
        let mode = self.mode.replace(' ', "");
        format!("sstv-{d}-{t}-{mode}-{:.3}MHz", self.freq_hz / 1e6)
    }
}

#[derive(Default)]
pub struct Sstv {
    pub pictures: VecDeque<Picture>,
    /// The picture shown; the newest when `None`.
    pub shown: Option<u32>,
    /// Modes named by start events, for pictures whose rows come after.
    names: std::collections::HashMap<u32, String>,
}

impl Sstv {
    pub fn line(&mut self, l: ImageLine, freq_hz: f64) {
        if !self.pictures.iter().any(|p| p.id == l.image) {
            let mut p = Picture::new(l.image, l.width as usize, freq_hz);
            p.mode = self.names.remove(&l.image).unwrap_or_default();
            self.pictures.push_back(p);
            while self.pictures.len() > KEEP {
                self.pictures.pop_front();
            }
        }
        if let Some(p) = self.pictures.iter_mut().find(|p| p.id == l.image) {
            p.push(l.line as usize, &l.rgb);
        }
    }

    /// The mode a picture was sent in, from its start event.
    pub fn named(&mut self, id: u32, mode: &str) {
        match self.pictures.iter_mut().find(|p| p.id == id) {
            Some(p) => p.mode = mode.to_string(),
            None => {
                self.names.insert(id, mode.to_string());
            }
        }
    }

    fn shown_index(&self) -> Option<usize> {
        match self.shown {
            Some(id) => self.pictures.iter().position(|p| p.id == id),
            None => self.pictures.len().checked_sub(1),
        }
    }
}

/// The SSTV view: what the decoder is doing, START in a chosen mode for a
/// picture whose VIS was missed, the picture, and the strip of the rest.
pub fn view(app: &mut App, ui: &mut Ui) {
    let on = app.status.mode == Mode::Sstv;
    let r = app.status.digital.clone().unwrap_or_default();
    let c = app.status.digital_cfg.clone().unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        match (on, r.fax_state.as_deref()) {
            (false, _) => widgets::chip(ui, Tone::Idle, "NOT IN SSTV"),
            (true, Some("receiving")) => widgets::chip(ui, Tone::Live, &format!("{} · LINE {}", r.variant.clone().unwrap_or_default().to_uppercase(), r.fax_lines)),
            (true, Some("starting")) => widgets::chip(ui, Tone::Warn, "VIS LEADER"),
            _ => widgets::chip(ui, Tone::Idle, "WAITING FOR A VIS CODE"),
        };
        for f in &r.facts {
            ui.label(RichText::new(format!("{} {}", f.label, f.value)).font(theme::mono(10.5)).color(theme::text_2()));
        }
        crate::widgets::enabled_or(ui, on, "Switch the receiver to this mode to use these", |ui| {
            let receiving = r.fax_state.as_deref() == Some("receiving");
            if receiving {
                if widgets::action_small(ui, "STOP").clicked() {
                    app.post("/api/sdr/digital", json!({ "fax_stop": true }));
                }
            } else {
                widgets::selector(ui, "sstv-mode", &c.sstv_mode, theme::text_2(), 120.0, |ui| {
                    for m in SSTV_MODES {
                        if ui.selectable_label(c.sstv_mode == *m, *m).clicked() {
                            app.post("/api/sdr/digital", json!({ "sstv_mode": m }));
                            ui.close();
                        }
                    }
                });
                if widgets::action_small(ui, "START").on_hover_text("Begin a picture now in the mode chosen, for one whose VIS code was missed").clicked() {
                    app.post("/api/sdr/digital", json!({ "fax_start": true }));
                }
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(i) = app.sstv.shown_index()
                && app.sstv.pictures[i].rows > 0
                && widgets::action_small(ui, "SAVE PNG").clicked()
            {
                save(app, &ui.ctx().clone(), i);
            }
        });
    });
    ui.add_space(4.0);
    let strip_h = if app.sstv.pictures.len() > 1 { 70.0 } else { 0.0 };
    let h = (ui.available_height() - strip_h - 4.0).max(80.0);
    canvas(app, ui, h);
    if strip_h > 0.0 {
        ui.add_space(4.0);
        strip(app, ui);
    }
}

/// The modes START can begin a picture in, as the server names them.
const SSTV_MODES: &[&str] = &["Martin M1", "Martin M2", "Scottie S1", "Scottie S2", "Scottie DX", "Robot 36", "Robot 72", "PD 50", "PD 90", "PD 120", "PD 160", "PD 180", "PD 240", "PD 290", "Wraase SC2-180"];

/// The newest picture (SSTV's or MFSK's), for another view to draw.
pub fn canvas_pub(app: &mut App, ui: &mut Ui, h: f32) {
    canvas(app, ui, h);
}

fn canvas(app: &mut App, ui: &mut Ui, h: f32) {
    let w = ui.available_width();
    let (outer, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let p = ui.painter_at(outer);
    p.rect_filled(outer, CornerRadius::same(4), theme::ink());
    let ctx = ui.ctx().clone();
    let Some(i) = app.sstv.shown_index() else {
        p.text(outer.center(), egui::Align2::CENTER_CENTER, "Pictures draw here as they arrive. 14.230 MHz carries the most; the band keys above tune it.", theme::sans(12.0), theme::text_3());
        return;
    };
    let newest = i + 1 == app.sstv.pictures.len();
    let receiving = app.status.digital.as_ref().is_some_and(|d| d.fax_state.as_deref() == Some("receiving"));
    let pic = &mut app.sstv.pictures[i];
    let full_h = 0.75 * pic.width as f32; // SSTV pictures are 4:3 or near it
    let rows = pic.rows;
    let width = pic.width;
    let (label, live) = (format!("{} · {:.3} MHz · {} rows{}", if pic.mode.is_empty() { "SSTV" } else { &pic.mode }, pic.freq_hz / 1e6, rows, if pic.saved.is_some() { " · saved" } else { "" }), receiving && newest);
    if let Some(tex) = pic.texture(&ctx).map(|t| t.id()) {
        let uv = pic.uv();
        // Fit the whole picture (its full height, rows still to come left
        // dark), square pixels.
        let total_rows = (full_h as usize).max(rows) as f32;
        let scale = (outer.width() / width as f32).min(outer.height() / total_rows);
        let size = vec2(width as f32 * scale, rows as f32 * scale);
        let top_left = pos2(outer.center().x - size.x / 2.0, outer.top() + (outer.height() - total_rows * scale) / 2.0);
        let r = egui::Rect::from_min_size(top_left, size);
        p.image(tex, r, uv, egui::Color32::WHITE);
        p.rect_stroke(egui::Rect::from_min_size(top_left, vec2(size.x, total_rows * scale)), CornerRadius::same(2), Stroke::new(1.0, theme::line()), egui::StrokeKind::Outside);
        if live {
            let y = r.bottom();
            p.line_segment([pos2(r.left(), y), pos2(r.right(), y)], Stroke::new(2.0, alpha(theme::AMBER, 0.8)));
        }
    }
    let g = p.layout_no_wrap(label, theme::mono(10.5), theme::text());
    let tag = egui::Rect::from_min_size(outer.left_top() + vec2(8.0, 8.0), g.size() + vec2(10.0, 6.0));
    p.rect_filled(tag, CornerRadius::same(3), alpha(theme::ink(), 0.78));
    p.galley(tag.min + vec2(5.0, 3.0), g, theme::text());
}

fn strip(app: &mut App, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    let shown = app.sstv.shown_index();
    let n = app.sstv.pictures.len();
    let mut pick = None;
    egui::ScrollArea::horizontal().id_salt("sstv-strip").auto_shrink([false, true]).stick_to_right(true).show(ui, |ui| {
        ui.horizontal(|ui| {
            for (k, pic) in app.sstv.pictures.iter_mut().enumerate() {
                let (r, resp) = ui.allocate_exact_size(vec2(84.0, 63.0), Sense::click());
                let p = ui.painter_at(r);
                p.rect_filled(r, CornerRadius::same(3), theme::ink());
                if let Some(tex) = pic.texture(&ctx).map(|t| t.id()) {
                    p.image(tex, r.shrink(1.0), pic.uv(), egui::Color32::WHITE);
                }
                let c = if shown == Some(k) { theme::AMBER } else if resp.hovered() { theme::text_2() } else { theme::line() };
                p.rect_stroke(r, CornerRadius::same(3), Stroke::new(if shown == Some(k) { 2.0 } else { 1.0 }, c), egui::StrokeKind::Inside);
                if resp.on_hover_text(pic.mode.clone()).clicked() {
                    pick = Some(if k + 1 == n { None } else { Some(pic.id) });
                }
            }
        });
    });
    if let Some(s) = pick {
        app.sstv.shown = s;
    }
}

fn save(app: &mut App, ctx: &egui::Context, i: usize) {
    let dir = app.prefs.record_dir();
    let Some(p) = app.sstv.pictures.get_mut(i) else { return };
    let path = dir.join(format!("{}.png", p.file_stem()));
    let Some(img) = image::RgbImage::from_raw(p.width as u32, p.rows as u32, p.rgb.clone()) else { return };
    p.saved = Some(path.clone());
    let shown = path.display().to_string();
    let _ = std::thread::Builder::new().name("usdr-sstv-png".into()).spawn(move || {
        if let Err(e) = std::fs::create_dir_all(&dir).map_err(|e| e.to_string()).and_then(|_| img.save(&path).map_err(|e| e.to_string())) {
            eprintln!("saving {}: {e}", path.display());
        }
    });
    app.toast(ctx, Tone::Ok, format!("Saved {shown}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The texture as the GPU would hold it: every upload since the last
    /// look applied in turn. Returns the bytes uploaded.
    fn replay(ctx: &egui::Context, tex: &mut Vec<egui::Color32>, size: &mut [usize; 2]) -> usize {
        let delta = ctx.tex_manager().write().take_delta();
        let mut bytes = 0;
        for (_, d) in delta.set {
            let egui::ImageData::Color(img) = &d.image;
            bytes += img.pixels.len() * 4;
            match d.pos {
                None => {
                    *size = img.size;
                    *tex = img.pixels.clone();
                }
                Some([x, y]) => {
                    for r in 0..img.size[1] {
                        for c in 0..img.size[0] {
                            tex[(y + r) * size[0] + x + c] = img.pixels[r * img.size[0] + c];
                        }
                    }
                }
            }
        }
        bytes
    }

    #[test]
    fn the_texture_holds_the_picture_and_only_new_rows_go_up() {
        let ctx = egui::Context::default();
        let width = 320;
        let mut pic = Picture::new(1, width, 14.23e6);
        let (mut tex, mut size) = (Vec::new(), [0usize; 2]);
        let line = |n: usize| (0..3 * width).map(|i| ((i * 7 + n * 13) % 251) as u8).collect::<Vec<u8>>();
        let mut late = 0;
        for n in 0..256 {
            // Row 100 is lost on the way, then arrives late, after 120.
            if n != 100 {
                pic.push(n, &line(n));
            }
            if n == 120 {
                pic.push(100, &line(100));
            }
            pic.texture(&ctx);
            let bytes = replay(&ctx, &mut tex, &mut size);
            if n > 64 && !n.is_power_of_two() && n != 120 {
                // A row (two where one was skipped), and the copy of the
                // last under the picture.
                assert!(bytes <= 3 * width * 4, "row {n}: {bytes} bytes");
            }
            if n == 120 {
                late = bytes;
            }
            // Every row as received, and the last repeated under it.
            let want = egui::ColorImage::from_rgb([width, pic.rows], &pic.rgb);
            assert_eq!(&tex[..want.pixels.len()], &want.pixels[..], "after row {n}");
            if pic.rows < size[1] {
                assert_eq!(&tex[pic.rows * width..(pic.rows + 1) * width], &want.pixels[(pic.rows - 1) * width..], "edge after row {n}");
            }
            assert!((pic.uv().max.y * size[1] as f32 - pic.rows as f32).abs() < 1e-3);
        }
        // The late row went up with the ones after it, not the whole picture.
        assert!(late <= 22 * width * 4, "{late}");
    }
}
