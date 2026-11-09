//! The radiofax viewer: weather charts and satellite pictures from HF fax
//! stations, drawn row by row as the server decodes them.
//!
//! The rows arrive as 0x06 frames (see `usdr_proto::FAX_LINE_MAGIC`); the
//! server's `WEFAX` decode events say when a picture starts and ends. Each
//! picture lives in one texture that grows by doubling, and a new row is
//! uploaded alone, so a 1809-pixel row costs 7 KB of upload rather than the
//! whole picture again. Pictures stay for the session, in a filmstrip, and
//! a finished one is saved as a PNG when the operator asks for that.

use crate::app::App;
use crate::theme::{self, alpha};
use crate::widgets::{self, Tone};
use egui::{Align2, CornerRadius, RichText, Sense, Stroke, Ui, pos2, vec2};
use serde_json::json;
use std::collections::VecDeque;
use std::time::SystemTime;
use usdr_proto::{DigitalCfg, FaxLine, Mode};

/// Pictures kept for the session; the oldest goes first.
const KEEP: usize = 16;
/// A picture longer than this is cut off: a transmission is at most about
/// 1300 rows at 120 LPM, and a lost stop tone must not grow one forever.
const MAX_ROWS: usize = 4000;

pub struct Picture {
    pub id: u32,
    pub width: usize,
    /// Gray pixels, row after row.
    pub pixels: Vec<u8>,
    pub rows: usize,
    pub started: SystemTime,
    /// The dial it was received on.
    pub freq_hz: f64,
    pub done: bool,
    /// Rotation applied when drawing and saving, in pixels, so a picture
    /// caught without its phasing lines can be lined up by hand.
    pub shift: i32,
    texture: Option<egui::TextureHandle>,
    /// Rows the texture has room for, and rows uploaded so far.
    tex_rows: usize,
    uploaded: usize,
    pub saved: Option<std::path::PathBuf>,
}

impl Picture {
    fn new(id: u32, width: usize, freq_hz: f64) -> Self {
        Picture { id, width, pixels: Vec::new(), rows: 0, started: SystemTime::now(), freq_hz, done: false, shift: 0, texture: None, tex_rows: 0, uploaded: 0, saved: None }
    }

    fn push(&mut self, line: u32, px: &[u8]) {
        if px.len() != self.width || self.rows >= MAX_ROWS {
            return;
        }
        // A row that was lost on the way leaves a gray row, so the picture
        // keeps its geometry.
        while self.rows < line as usize && self.rows < MAX_ROWS {
            self.pixels.extend(std::iter::repeat_n(128u8, self.width));
            self.rows += 1;
        }
        if (line as usize) < self.rows {
            return;
        }
        self.pixels.extend_from_slice(px);
        self.rows += 1;
    }

    /// Bring the texture up to the rows received: grow it by doubling, and
    /// upload only the new rows otherwise.
    fn texture(&mut self, ctx: &egui::Context) -> Option<&egui::TextureHandle> {
        if self.rows == 0 {
            return None;
        }
        let opts = egui::TextureOptions { magnification: egui::TextureFilter::Linear, minification: egui::TextureFilter::Linear, ..Default::default() };
        let gray = |rows: std::ops::Range<usize>, pixels: &[u8], width: usize, pad_to: usize| {
            let mut img = egui::ColorImage::filled([width, pad_to.max(rows.len())], egui::Color32::BLACK);
            for (i, p) in pixels[rows.start * width..rows.end * width].iter().enumerate() {
                img.pixels[i] = egui::Color32::from_gray(*p);
            }
            img
        };
        if self.texture.is_none() || self.rows > self.tex_rows {
            self.tex_rows = self.rows.next_power_of_two().max(256);
            let img = gray(0..self.rows, &self.pixels, self.width, self.tex_rows);
            match &mut self.texture {
                Some(t) => t.set(img, opts),
                None => self.texture = Some(ctx.load_texture(format!("fax-{}", self.id), img, opts)),
            }
            self.uploaded = self.rows;
        } else if self.uploaded < self.rows {
            let img = gray(self.uploaded..self.rows, &self.pixels, self.width, 0);
            if let Some(t) = &mut self.texture {
                t.set_partial([0, self.uploaded], img, opts);
            }
            self.uploaded = self.rows;
        }
        self.texture.as_ref()
    }

    /// The picture as a PNG-ready gray image, rotated by the shift.
    fn gray_image(&self) -> image::GrayImage {
        let w = self.width;
        let s = self.shift.rem_euclid(w.max(1) as i32) as usize;
        let mut out = Vec::with_capacity(self.pixels.len());
        for r in 0..self.rows {
            let row = &self.pixels[r * w..(r + 1) * w];
            out.extend_from_slice(&row[s..]);
            out.extend_from_slice(&row[..s]);
        }
        image::GrayImage::from_raw(w as u32, self.rows as u32, out).expect("rows fill the picture")
    }

    /// Where a chart's left edge is, for a picture caught without its
    /// phasing lines: the middle of the widest band of columns that are
    /// white in nearly every row, which is the margin a chart is drawn
    /// inside. `None` when no column is clearly whiter than the picture.
    pub fn margin_shift(&self) -> Option<i32> {
        let w = self.width;
        if w == 0 || self.rows < 40 {
            return None;
        }
        // Each column's share of rows that are white there, over the rows
        // received (a sample of rows is plenty).
        let step = (self.rows / 400).max(1);
        let mut white = vec![0u32; w];
        let mut n = 0u32;
        for r in (0..self.rows).step_by(step) {
            for (c, p) in self.pixels[r * w..(r + 1) * w].iter().enumerate() {
                white[c] += u32::from(*p > 160);
            }
            n += 1;
        }
        let frac: Vec<f32> = white.iter().map(|&v| v as f32 / n as f32).collect();
        // Smooth over 1% of the width, so a stray dark pixel does not split
        // the margin, then take the longest circular run above the line.
        let k = (w / 100).max(1);
        let smooth: Vec<f32> = (0..w).map(|c| (0..k).map(|d| frac[(c + w - k / 2 + d) % w]).sum::<f32>() / k as f32).collect();
        let mean = smooth.iter().sum::<f32>() / w as f32;
        let line = (mean + (1.0 - mean) * 0.6).max(0.85);
        let (mut best, mut best_at, mut run) = (0usize, 0usize, 0usize);
        for c in 0..2 * w {
            if smooth[c % w] >= line {
                run += 1;
                if run > best && run <= w {
                    best = run;
                    best_at = c;
                }
            } else {
                run = 0;
            }
        }
        // A margin is a few percent of the line; a whole blank picture is not one.
        if best < w / 100 || best > w / 3 {
            return None;
        }
        let mid = (best_at + w - best / 2) % w;
        Some(mid as i32)
    }

    fn file_stem(&self) -> String {
        let secs = self.started.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let (d, t) = crate::app::utc_date_time(secs);
        format!("fax-{d}-{t}-{:.1}kHz", (self.freq_hz + 1_900.0) / 1e3)
    }
}

#[derive(Default)]
pub struct Fax {
    pub open: bool,
    pub pictures: VecDeque<Picture>,
    /// The picture shown; `None` follows the newest.
    pub shown: Option<u32>,
    /// Follow the newest row as it arrives.
    pub follow: bool,
    /// The window was opened for this switch to WEFAX already.
    opened_for_mode: bool,
}

impl Fax {
    pub fn new() -> Self {
        Fax { follow: true, ..Default::default() }
    }

    fn get_or_start(&mut self, id: u32, width: usize, freq_hz: f64) -> &mut Picture {
        if let Some(i) = self.pictures.iter().position(|p| p.id == id) {
            return &mut self.pictures[i];
        }
        // A new picture from the server finishes any it left open.
        for p in self.pictures.iter_mut() {
            p.done = true;
        }
        self.pictures.push_back(Picture::new(id, width, freq_hz));
        while self.pictures.len() > KEEP {
            self.pictures.pop_front();
        }
        self.pictures.back_mut().expect("just pushed")
    }

    pub fn line(&mut self, l: FaxLine, freq_hz: f64) {
        let width = l.pixels.len();
        let p = self.get_or_start(l.image, width, freq_hz);
        if p.width != width {
            return;
        }
        p.push(l.line, &l.pixels);
    }

    /// The server's picture-so-far, fetched when the viewer opens mid-fax.
    pub fn backfill(&mut self, id: u32, width: usize, freq_hz: f64, pixels: &[u8]) {
        if width == 0 {
            return;
        }
        let p = self.get_or_start(id, width, freq_hz);
        if p.rows > 0 || p.width != width {
            return;
        }
        for (i, row) in pixels.chunks_exact(width).enumerate() {
            p.push(i as u32, row);
        }
    }

    /// A WEFAX decode event: `picture` ends one. Returns the finished
    /// picture's index when it should be saved.
    pub fn event(&mut self, ev: &usdr_proto::DecodeEvent) -> Option<usize> {
        let id: u32 = ev.fields.get("image")?.parse().ok()?;
        match ev.kind.as_str() {
            "picture" => {
                let i = self.pictures.iter().position(|p| p.id == id)?;
                let p = &mut self.pictures[i];
                if p.done && p.saved.is_some() {
                    return None;
                }
                p.done = true;
                (p.rows >= 50).then_some(i)
            }
            _ => None,
        }
    }

    pub fn mode_became(&mut self, m: Mode) {
        if m == Mode::Wefax {
            if !self.opened_for_mode {
                self.open = true;
                self.opened_for_mode = true;
            }
        } else {
            self.opened_for_mode = false;
        }
    }
}

/// Write a picture as a PNG beside the recordings, off the UI thread.
pub fn save(app: &mut App, ctx: &egui::Context, i: usize) {
    let Some(p) = app.fax.pictures.get_mut(i) else { return };
    let dir = app.prefs.record_dir();
    let path = dir.join(format!("{}.png", p.file_stem()));
    let img = p.gray_image();
    p.saved = Some(path.clone());
    let shown = path.display().to_string();
    let _ = std::thread::Builder::new().name("usdr-fax-png".into()).spawn(move || {
        if let Err(e) = std::fs::create_dir_all(&dir).map_err(|e| e.to_string()).and_then(|_| img.save(&path).map_err(|e| e.to_string())) {
            eprintln!("saving {}: {e}", path.display());
        }
    });
    app.toast(ctx, Tone::Ok, format!("Saved {shown}"));
}

pub const SUBCARRIER_HZ: f64 = 1_900.0;

pub fn window(app: &mut App, ctx: &egui::Context) {
    if !app.fax.open {
        return;
    }
    let mut open = true;
    let screen = ctx.content_rect();
    egui::Window::new(RichText::new("Radiofax").font(theme::headline(15.0)))
        .id(egui::Id::new("fax-window"))
        .open(&mut open)
        .collapsible(true)
        .resizable(true)
        .default_size(vec2((screen.width() * 0.55).clamp(520.0, 1100.0), (screen.height() * 0.7).clamp(380.0, 900.0)))
        .min_size(vec2(460.0, 300.0))
        .default_pos(pos2(screen.center().x - screen.width() * 0.27, screen.top() + 80.0))
        .show(ctx, |ui| {
            controls(app, ui, false);
            ui.add_space(6.0);
            let strip_h = if app.fax.pictures.len() > 1 { 74.0 } else { 0.0 };
            let canvas_h = (ui.available_height() - strip_h - 4.0).max(120.0);
            canvas(app, ui, canvas_h);
            if strip_h > 0.0 {
                ui.add_space(4.0);
                filmstrip(app, ui);
            }
        });
    if !open {
        app.fax.open = false;
    }
}

fn cfg(app: &App) -> DigitalCfg {
    app.status.digital_cfg.clone().unwrap_or_default()
}

/// The viewer in the dock: the same picture, controls and filmstrip, the
/// station list left to the dock's own tuning row.
pub fn docked(app: &mut App, ui: &mut Ui) {
    controls(app, ui, true);
    ui.add_space(4.0);
    let strip_h = if app.fax.pictures.len() > 1 { 74.0 } else { 0.0 };
    let canvas_h = (ui.available_height() - strip_h - 4.0).max(80.0);
    canvas(app, ui, canvas_h);
    if strip_h > 0.0 {
        ui.add_space(4.0);
        filmstrip(app, ui);
    }
}

fn controls(app: &mut App, ui: &mut Ui, docked: bool) {
    let on = app.status.mode == Mode::Wefax;
    let reading = app.status.digital.clone().unwrap_or_default();
    let c = cfg(app);
    ui.horizontal_wrapped(|ui| {
        let state = reading.fax_state.as_deref().unwrap_or("idle");
        let (tone, text) = match (on, state) {
            (false, _) => (Tone::Idle, "NOT IN WEFAX".to_string()),
            (true, "receiving") => (Tone::Live, format!("RECEIVING · ROW {}", reading.fax_lines)),
            (true, "phasing") => (Tone::Warn, "PHASING".to_string()),
            (true, "starting") => (Tone::Warn, "START TONE".to_string()),
            _ => (Tone::Idle, if c.fax_auto_start { "WAITING FOR A START TONE".to_string() } else { "IDLE".to_string() }),
        };
        widgets::chip(ui, tone, &text);
        if let Some(snr) = reading.snr_db.filter(|_| on) {
            ui.label(RichText::new(format!("{snr:.0} dB")).font(theme::mono(11.0)).color(theme::text_3()));
        }
        ui.add_space(6.0);
        crate::widgets::enabled_or(ui, on, "Switch the receiver to this mode to use these", |ui| {
            let lpms = [60u16, 90, 120, 240];
            if let Some(i) = widgets::segmented(ui, "fax-lpm", &["60", "90", "120", "240"], lpms.iter().position(|l| *l == c.fax_lpm), theme::CYAN) {
                app.post("/api/sdr/digital", json!({ "fax_lpm": lpms[i] }));
            }
            ui.label(RichText::new("LPM").font(theme::mono(10.0)).color(theme::text_3()));
            let iocs = [576u16, 288];
            if let Some(i) = widgets::segmented(ui, "fax-ioc", &["576", "288"], iocs.iter().position(|v| *v == c.fax_ioc), theme::CYAN) {
                app.post("/api/sdr/digital", json!({ "fax_ioc": iocs[i] }));
            }
            ui.label(RichText::new("IOC").font(theme::mono(10.0)).color(theme::text_3()));
            if widgets::chip_key(ui, "AUTO", c.fax_auto_start, theme::CYAN).on_hover_text("Begin a picture on the station's start tone").clicked() {
                app.post("/api/sdr/digital", json!({ "fax_auto_start": !c.fax_auto_start }));
            }
            let receiving = matches!(state, "receiving" | "phasing");
            if !receiving && widgets::action_small(ui, "START").on_hover_text("Begin a picture now, for a transmission already under way").clicked() {
                app.post("/api/sdr/digital", json!({ "fax_start": true }));
            }
            if receiving && widgets::action_small(ui, "STOP").clicked() {
                app.post("/api/sdr/digital", json!({ "fax_stop": true }));
            }
        });
        if !docked {
            crate::directory::menu(app, ui, &crate::stations::WEFAX, theme::CYAN);
        }
        crate::directory::schedule_menu(app, ui, &crate::stations::WEFAX, theme::CYAN);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if docked && widgets::action_small(ui, "POP OUT ↗").on_hover_text("The viewer in its own window, as large as you like").clicked() {
                app.fax.open = true;
            }
            let idx = shown_index(app);
            if let Some(i) = idx
                && app.fax.pictures[i].rows > 0
                && widgets::action_small(ui, "SAVE PNG").clicked()
            {
                save(app, &ui.ctx().clone(), i);
            }
        });
    });
    // Slant: the operator trims a picture that leans; the server applies it
    // to the rows it decodes from now on.
    ui.horizontal(|ui| {
        crate::widgets::enabled_or(ui, on, "Switch the receiver to this mode to use these", |ui| {
            ui.label(RichText::new("SLANT").font(theme::mono(10.0)).color(theme::text_3()));
            let mut ppm = c.fax_slant_ppm;
            let r = ui.add(egui::Slider::new(&mut ppm, -200.0..=200.0).step_by(1.0).suffix(" ppm").show_value(true));
            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                app.post("/api/sdr/digital", json!({ "fax_slant_ppm": ppm }));
            }
            if ppm != 0.0 && widgets::action_small(ui, "0").on_hover_text("No slant trim").clicked() {
                app.post("/api/sdr/digital", json!({ "fax_slant_ppm": 0.0 }));
            }
        });
        if let Some(i) = shown_index(app) {
            ui.add_space(10.0);
            ui.label(RichText::new("Shift-drag the picture to move its left edge").font(theme::sans(11.0)).color(theme::text_3()));
            let pic = &mut app.fax.pictures[i];
            if pic.rows >= 40
                && widgets::action_small(ui, "FIND EDGE").on_hover_text("Line the picture up on the chart's white margin").clicked()
                && let Some(s) = pic.margin_shift()
            {
                pic.shift = s;
            }
            if pic.shift != 0 && widgets::action_small(ui, "RESET EDGE").clicked() {
                pic.shift = 0;
            }
        }
    });
}

fn shown_index(app: &App) -> Option<usize> {
    match app.fax.shown {
        Some(id) => app.fax.pictures.iter().position(|p| p.id == id),
        None => app.fax.pictures.len().checked_sub(1),
    }
}

fn canvas(app: &mut App, ui: &mut Ui, h: f32) {
    let w = ui.available_width();
    let (outer, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click_and_drag());
    let p = ui.painter_at(outer);
    p.rect_filled(outer, CornerRadius::same(4), theme::ink());
    let Some(i) = shown_index(app) else {
        empty(ui, &p, outer);
        return;
    };
    let ctx = ui.ctx().clone();
    let live = i + 1 == app.fax.pictures.len() && !app.fax.pictures[i].done;
    let pic = &mut app.fax.pictures[i];
    let (rows, width, tex_rows) = (pic.rows, pic.width, pic.tex_rows.max(1));
    // Shift-drag turns the picture sideways around its own width.
    if resp.dragged() && ui.input(|i| i.modifiers.shift) {
        let scale = outer.width() / width as f32;
        pic.shift = (pic.shift - (resp.drag_delta().x / scale).round() as i32).rem_euclid(width as i32);
    }
    let shift = pic.shift.rem_euclid(width.max(1) as i32) as f32;
    let Some(tex) = pic.texture(&ctx).cloned() else {
        empty(ui, &p, outer);
        return;
    };
    // Fit to the width, with square pixels; a long picture scrolls, and
    // the newest row stays in view while following.
    let scale = outer.width() / width as f32;
    let full_h = rows as f32 * scale;
    let scroll_id = egui::Id::new(("fax-scroll", app.fax.pictures[i].id));
    let mut offset: f32 = ui.data(|d| d.get_temp(scroll_id)).unwrap_or(0.0);
    let max_off = (full_h - outer.height()).max(0.0);
    if resp.hovered() {
        let wheel = ui.input(|i| i.smooth_scroll_delta.y);
        if wheel != 0.0 {
            offset = (offset - wheel).clamp(0.0, max_off);
            app.fax.follow = offset >= max_off - 1.0;
        }
    }
    if resp.dragged() && !ui.input(|i| i.modifiers.shift) {
        offset = (offset - resp.drag_delta().y).clamp(0.0, max_off);
        app.fax.follow = offset >= max_off - 1.0;
    }
    if app.fax.follow && live {
        offset = max_off;
    }
    offset = offset.clamp(0.0, max_off);
    ui.data_mut(|d| d.insert_temp(scroll_id, offset));
    let top = outer.top() - offset;
    let v = rows as f32 / tex_rows as f32;
    // Two pieces, the right part of each row first, for the shift.
    let s = shift / width as f32;
    let left_w = (1.0 - s) * outer.width();
    let mesh_rect = |x0: f32, x1: f32, u0: f32, u1: f32| {
        let r = egui::Rect::from_min_max(pos2(x0, top), pos2(x1, top + full_h));
        (r, egui::Rect::from_min_max(pos2(u0, 0.0), pos2(u1, v)))
    };
    for (r, uv) in [mesh_rect(outer.left(), outer.left() + left_w, s, 1.0), mesh_rect(outer.left() + left_w, outer.right(), 0.0, s)] {
        if r.width() > 0.5 {
            p.image(tex.id(), r, uv, egui::Color32::WHITE);
        }
    }
    // The write head: a lit rule under the newest row while it arrives.
    if live {
        let y = (top + full_h).min(outer.bottom() - 1.0);
        let rm = app.reduce_motion();
        let glow = if rm { 0.8 } else { 0.55 + 0.45 * (ui.input(|i| i.time) * 3.0).sin().abs() as f32 };
        p.line_segment([pos2(outer.left(), y), pos2(outer.right(), y)], Stroke::new(2.0, alpha(theme::AMBER, glow)));
        p.rect_filled(egui::Rect::from_min_max(pos2(outer.left(), y - 10.0), pos2(outer.right(), y)), CornerRadius::ZERO, alpha(theme::AMBER, 0.05 * glow));
        if !rm {
            crate::anim::after(ui.ctx(), std::time::Duration::from_millis(120));
        }
    }
    // The scroll position, when the picture is longer than the view.
    if max_off > 0.0 {
        let track = egui::Rect::from_min_max(pos2(outer.right() - 5.0, outer.top() + 2.0), pos2(outer.right() - 2.0, outer.bottom() - 2.0));
        let frac = outer.height() / full_h;
        let th = (track.height() * frac).max(16.0);
        let ty = track.top() + (track.height() - th) * (offset / max_off);
        p.rect_filled(egui::Rect::from_min_size(pos2(track.left(), ty), vec2(track.width(), th)), CornerRadius::same(2), alpha(theme::text_2(), 0.5));
    }
    p.rect_stroke(outer, CornerRadius::same(4), Stroke::new(1.0, theme::line()), egui::StrokeKind::Inside);
    let pic = &app.fax.pictures[i];
    let secs = pic.started.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let label = format!(
        "{:02}:{:02} UTC · {:.1} kHz · {} rows{}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        (pic.freq_hz + SUBCARRIER_HZ) / 1e3,
        pic.rows,
        if pic.saved.is_some() { " · saved" } else { "" }
    );
    let g = p.layout_no_wrap(label, theme::mono(10.5), theme::text());
    let tag = egui::Rect::from_min_size(outer.left_top() + vec2(8.0, 8.0), g.size() + vec2(10.0, 6.0));
    p.rect_filled(tag, CornerRadius::same(3), alpha(theme::ink(), 0.78));
    p.galley(tag.min + vec2(5.0, 3.0), g, theme::text());
    if !app.fax.follow && live && widgets::action_small(&mut ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(outer.right_bottom() - vec2(120.0, 30.0), vec2(112.0, 24.0)))), "NEWEST ROW").clicked() {
        app.fax.follow = true;
    }
}

fn empty(_ui: &Ui, p: &egui::Painter, r: egui::Rect) {
    let lines = [
        "No pictures yet.",
        "Pick a station under STATIONS: it sets WEFAX and tunes the dial 1.9 kHz below.",
        "A picture begins on the station's start tone; START begins one now.",
    ];
    for (k, l) in lines.iter().enumerate() {
        let (font, c) = if k == 0 { (theme::sans(14.0), theme::text_2()) } else { (theme::sans(12.0), theme::text_3()) };
        p.text(r.center() + vec2(0.0, (k as f32 - 1.0) * 22.0), Align2::CENTER_CENTER, *l, font, c);
    }
}

/// The session's pictures, newest last, the one shown lit.
fn filmstrip(app: &mut App, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    let shown = shown_index(app);
    let n = app.fax.pictures.len();
    let mut pick = None;
    egui::ScrollArea::horizontal().id_salt("fax-strip").auto_shrink([false, true]).stick_to_right(true).show(ui, |ui| {
        ui.horizontal(|ui| {
            for (k, pic) in app.fax.pictures.iter_mut().enumerate() {
                let (r, resp) = ui.allocate_exact_size(vec2(96.0, 66.0), Sense::click());
                let p = ui.painter_at(r);
                p.rect_filled(r, CornerRadius::same(3), theme::ink());
                let (rows, width) = (pic.rows, pic.width);
                if let Some(tex) = pic.texture(&ctx).cloned() {
                    let v = rows as f32 / pic.tex_rows.max(1) as f32;
                    // The top of the picture, where a chart has its title.
                    let aspect = rows as f32 / width as f32;
                    let vis = (r.height() / (r.width() * aspect)).min(1.0);
                    p.image(tex.id(), r.shrink(1.0), egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, v * vis)), egui::Color32::WHITE);
                }
                let on = shown == Some(k);
                let c = if on { theme::AMBER } else if resp.hovered() { theme::text_2() } else { theme::line() };
                p.rect_stroke(r, CornerRadius::same(3), Stroke::new(if on { 2.0 } else { 1.0 }, c), egui::StrokeKind::Inside);
                if k + 1 == n && !pic.done {
                    p.text(r.left_bottom() + vec2(4.0, -3.0), Align2::LEFT_BOTTOM, "LIVE", theme::mono_bold(9.5), theme::EMBER);
                }
                if resp.clicked() {
                    pick = Some(if k + 1 == n { None } else { Some(pic.id) });
                }
            }
        });
    });
    if let Some(s) = pick {
        app.fax.shown = s;
        app.fax.follow = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chart's content with a white margin at columns 700–759: the edge
    /// is found in the middle of the margin, and a blank picture has none.
    #[test]
    fn the_margin_is_found() {
        let w = 1809;
        let mut p = Picture::new(1, w, 0.0);
        let mut seed = 7u32;
        for r in 0..200u32 {
            let row: Vec<u8> = (0..w)
                .map(|c| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    if (700..760).contains(&c) { 250 } else if (seed >> 16) % 3 == 0 { 20 } else { 230 }
                })
                .collect();
            p.push(r, &row);
        }
        let s = p.margin_shift().expect("a margin");
        assert!((s - 730).abs() <= 12, "{s}");
        let mut blank = Picture::new(2, w, 0.0);
        for r in 0..100 {
            blank.push(r, &vec![255u8; w]);
        }
        assert_eq!(blank.margin_shift(), None);
    }
}
