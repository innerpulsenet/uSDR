//! The panafall: panadapter over waterfall, one widget, one frequency axis.
//!
//! The panadapter draws the server's Welch-averaged trace with a little
//! visual smoothing (it rises fast and falls slowly, so a burst shows at full
//! level and the floor stops flickering), a fill whose heat follows level, a
//! decaying peak hold, the server's max hold, the passband with its slice
//! flag, decode spots, spurs and scan marks. The waterfall keeps its history
//! in dB, so a palette or contrast change recolors all of it, not just the
//! rows that arrive afterwards; it scrolls on the frame clock with a
//! sub-row offset, so it moves smoothly at any refresh rate.

use crate::colormap::Colormap;
use crate::theme::{self, alpha};
use egui::{
    Align2, Color32, CornerRadius, Mesh, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, pos2, vec2,
};
use std::collections::VecDeque;
use std::time::Instant;
use usdr_proto::{FftFrame, Mode, Status};

/// What the operator did to the display; the app turns these into requests.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// Tune the receiver. `lock` recenters the span on it.
    Tune { hz: f64, lock: bool },
    /// Move the span center, leaving the receiver where it is.
    Center(f64),
    Zoom(f64),
    StoreMemory(f64),
    CopyFreq(f64),
    /// New passband edges, Hz from the carrier.
    Filter(f32, f32),
    /// Make receiver A (0) or B (1) the one the gestures drive.
    Select(u8),
    /// Put receiver B here.
    PlaceB(f64),
}

/// Receiver B as the display draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct RxB {
    pub freq_hz: f64,
    pub mode: Mode,
    pub bandwidth_hz: f64,
    pub offset_hz: f64,
    pub active: bool,
    /// Channel power over its passband, dBFS, measured from the spectrum.
    pub level: Option<f32>,
    pub note: Option<String>,
}

/// Display preferences the panafall reads each frame.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct View {
    pub palette: Colormap,
    pub peak_hold: bool,
    pub fill: bool,
    pub auto_range: bool,
    pub floor_db: f32,
    pub ceiling_db: f32,
    pub wf_peak: bool,
    /// Waterfall black level, dB relative to the noise floor when auto.
    pub wf_black: f32,
    pub wf_auto_black: bool,
    /// dB from black to the top of the palette.
    pub wf_range: f32,
    pub split: f32,
    pub sql_mark: Option<f32>,
    pub show_peaks: bool,
}

impl Default for View {
    fn default() -> Self {
        Self {
            palette: Colormap::Ember,
            peak_hold: true,
            fill: true,
            auto_range: true,
            floor_db: -110.0,
            ceiling_db: -20.0,
            wf_peak: false,
            wf_black: -3.0,
            wf_auto_black: true,
            wf_range: 55.0,
            split: 0.42,
            sql_mark: None,
            show_peaks: true,
        }
    }
}

const WF_ROWS: usize = 768;
const WF_MAX_COLS: usize = 4096;

pub struct Panafall {
    smooth: Vec<f32>,
    peak: Vec<f32>,
    max_hold: Vec<f32>,
    pub center_hz: f64,
    pub span_hz: f64,
    /// Animated display range.
    min_db: f32,
    max_db: f32,
    target_min: f32,
    target_max: f32,
    /// Noise floor estimate (15th percentile), smoothed.
    pub floor_est: f32,
    wf_shift_db: f32,
    // Waterfall: history in dB, newest at `head`, rows going back in time.
    wf_cols: usize,
    wf_db: Vec<f32>,
    wf_time: Vec<f64>,
    wf_head: usize,
    wf_count: usize,
    wf_tex: Option<egui::TextureHandle>,
    wf_dirty_all: bool,
    wf_new_rows: VecDeque<usize>,
    wf_levels: (f32, f32, Colormap),
    /// The auto black level in use; moves only when the floor has moved
    /// 1.5 dB from it, so a floor wandering across a rounding boundary does
    /// not recolor the whole history every frame.
    auto_black: f32,
    /// Level (0..1 of the range) to palette index, with the tone curve baked in.
    tone: Vec<u8>,
    row_interval: f64,
    last_row_at: f64,
    // Interaction
    drag_origin: Option<(f64, f32)>,
    drag_offset_px: f32,
    sql_drag: bool,
    db_drag: Option<(f32, f32, f32)>,
    split_drag: bool,
    scroll_accum: f32,
    menu_hz: f64,
    pub hover_hz: Option<f64>,
    pub last_frame: Option<Instant>,
    /// Where the panadapter, waterfall, frequency axis and dB scale were
    /// drawn last frame.
    pub regions: Option<[Rect; 4]>,
    /// The filter the operator may set, with its edges now, Hz from the
    /// carrier; `None` where the mode or the server fixes it.
    pub filter: Option<(usdr_proto::FilterRange, f32, f32)>,
    edge_drag: Option<EdgeDrag>,
    /// The second receiver, when one is listening.
    pub rx_b: Option<RxB>,
    /// Where each receiver's flag was drawn last frame (A, B), for clicks.
    flags: [Option<Rect>; 2],
}

/// A passband edge being dragged, or (Ctrl) the whole passband shifted.
#[derive(Clone, Copy)]
enum EdgeDrag {
    Low,
    High,
    Shift { from_hz: f64, lo: f32, hi: f32 },
}

/// New edges for a drag that has reached `rel` Hz from the carrier.
fn drag_edges(d: EdgeDrag, r: &usdr_proto::FilterRange, lo: f32, hi: f32, rel: f32) -> (f32, f32) {
    let min_w = r.min_width_hz;
    if r.symmetric {
        let half = rel.abs().clamp(min_w / 2.0, r.max_width_hz / 2.0);
        return (-half, half);
    }
    let lim = r.max_edge_hz;
    match d {
        EdgeDrag::Low => (rel.clamp(-lim, hi - min_w), hi),
        EdgeDrag::High => (lo, rel.clamp(lo + min_w, lim)),
        EdgeDrag::Shift { from_hz, lo, hi } => {
            let d = rel - from_hz as f32;
            let d = d.clamp(-lim - lo, lim - hi);
            (lo + d, hi + d)
        }
    }
}

impl Default for Panafall {
    fn default() -> Self {
        Self {
            smooth: Vec::new(),
            peak: Vec::new(),
            max_hold: Vec::new(),
            center_hz: 0.0,
            span_hz: 0.0,
            min_db: -110.0,
            max_db: -20.0,
            target_min: -110.0,
            target_max: -20.0,
            floor_est: -100.0,
            wf_shift_db: 0.0,
            wf_cols: 0,
            wf_db: Vec::new(),
            wf_time: Vec::new(),
            wf_head: 0,
            wf_count: 0,
            wf_tex: None,
            wf_dirty_all: true,
            wf_new_rows: VecDeque::new(),
            wf_levels: (0.0, 0.0, Colormap::Ember),
            auto_black: f32::NAN,
            tone: (0..1024).map(|i| ((i as f32 / 1023.0).powf(0.86) * 255.0).round() as u8).collect(),
            row_interval: 0.04,
            last_row_at: 0.0,
            drag_origin: None,
            drag_offset_px: 0.0,
            sql_drag: false,
            db_drag: None,
            split_drag: false,
            scroll_accum: 0.0,
            menu_hz: 0.0,
            hover_hz: None,
            last_frame: None,
            regions: None,
            filter: None,
            edge_drag: None,
            rx_b: None,
            flags: [None, None],
        }
    }
}

/// Fill under a trace with a vertical fade, as one mesh.
pub fn fill_under(painter: &egui::Painter, pts: &[Pos2], base_y: f32, color: Color32, top_alpha: f32) {
    if pts.len() < 2 {
        return;
    }
    let mut mesh = Mesh::default();
    for p in pts {
        let i = mesh.vertices.len() as u32;
        mesh.colored_vertex(*p, alpha(color, top_alpha));
        mesh.colored_vertex(pos2(p.x, base_y), alpha(color, 0.0));
        if i >= 2 {
            mesh.add_triangle(i - 2, i - 1, i);
            mesh.add_triangle(i - 1, i + 1, i);
        }
    }
    painter.add(Shape::mesh(mesh));
}

/// 1-2-5 grid steps.
fn nice_step(range: f64, target_lines: f64) -> f64 {
    let raw = range / target_lines.max(1.0);
    let mag = 10f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let m = if norm < 1.5 {
        1.0
    } else if norm < 3.5 {
        2.0
    } else if norm < 7.5 {
        5.0
    } else {
        10.0
    };
    m * mag
}

fn percentile(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() {
        return -100.0;
    }
    sorted[((sorted.len() as f32 * p) as usize).min(sorted.len() - 1)]
}

impl Panafall {
    fn x_for(&self, hz: f64, r: Rect) -> f32 {
        let lo = self.center_hz - self.span_hz / 2.0;
        r.left() + ((hz - lo) / self.span_hz.max(1.0)) as f32 * r.width() + self.drag_offset_px
    }
    fn hz_for(&self, x: f32, r: Rect) -> f64 {
        let lo = self.center_hz - self.span_hz / 2.0;
        lo + ((x - self.drag_offset_px - r.left()) / r.width().max(1.0)) as f64 * self.span_hz
    }
    fn y_for(&self, db: f32, r: Rect) -> f32 {
        let t = (db - self.min_db) / (self.max_db - self.min_db).max(1.0);
        (r.bottom() - t * r.height()).clamp(r.top(), r.bottom())
    }

    pub fn range(&self) -> (f32, f32) {
        (self.min_db, self.max_db)
    }

    /// Take a new frame: smoothing, holds, auto range and a waterfall row.
    pub fn push(&mut self, f: &FftFrame, view: &View, now: f64) {
        let n = f.pwr.len();
        if n < 2 {
            return;
        }
        let axis_moved = (f.center_hz - self.center_hz).abs() > 1.0;
        let span_changed = (f.rate_hz - self.span_hz).abs() > 1.0;
        if self.smooth.len() != n || span_changed {
            self.smooth = f.pwr.clone();
            self.peak = f.pwr.clone();
        } else if axis_moved {
            // Same span, new center: carry the holds and history across by
            // the bins they moved, so a retune slides rather than blanks.
            let shift = ((f.center_hz - self.center_hz) / f.rate_hz * (n - 1) as f64).round() as isize;
            shift_vec(&mut self.smooth, shift, -200.0);
            shift_vec(&mut self.peak, shift, -200.0);
            self.shift_waterfall(shift as f64 / (n - 1) as f64);
        }
        self.center_hz = f.center_hz;
        self.span_hz = f.rate_hz;
        for (s, &v) in self.smooth.iter_mut().zip(&f.pwr) {
            if *s < -190.0 {
                *s = v;
            }
            *s += (v - *s) * if v > *s { 0.6 } else { 0.22 };
        }
        for (p, &s) in self.peak.iter_mut().zip(&self.smooth) {
            *p = s.max(*p - 0.25);
        }
        self.max_hold = f.max_hold.clone();

        // Auto range: floor just under the 15th percentile, ceiling over the
        // strongest signals, eased so the scale glides instead of jumping.
        let stride = (n / 64).max(1);
        let mut s: Vec<f32> = f.pwr.iter().step_by(stride).copied().collect();
        s.sort_by(|a, b| a.total_cmp(b));
        let p15 = percentile(&s, 0.15);
        let p98 = percentile(&s, 0.98);
        self.floor_est += (p15 - self.floor_est) * 0.08;
        if view.auto_range {
            self.target_min = (p15 - 6.0).clamp(-140.0, -30.0);
            self.target_max = (self.target_min + 30.0).max((p98 + 14.0).max(p15 + 45.0).min(5.0));
        } else {
            self.target_min = view.floor_db;
            self.target_max = view.ceiling_db.max(view.floor_db + 10.0);
        }

        // Waterfall row, from the burst-peak array when WF PEAK is on. The
        // peak array runs hotter than the average; its median excess is
        // taken off so the floor stays where the average put it.
        let row_src: &[f32] = if view.wf_peak && f.wf_peak.len() == n {
            let mut diffs: Vec<f32> = (0..n)
                .step_by((n / 128).max(1))
                .map(|i| f.wf_peak[i] - f.pwr[i])
                .collect();
            diffs.sort_by(|a, b| a.total_cmp(b));
            let med = percentile(&diffs, 0.5);
            self.wf_shift_db += (med - self.wf_shift_db) * 0.1;
            &f.wf_peak
        } else {
            self.wf_shift_db = 0.0;
            &f.pwr
        };
        let cols = n.min(WF_MAX_COLS);
        if cols != self.wf_cols {
            self.wf_cols = cols;
            self.wf_db = vec![-200.0; cols * WF_ROWS];
            self.wf_time = vec![0.0; WF_ROWS];
            self.wf_count = 0;
            self.wf_tex = None;
            self.wf_dirty_all = true;
        }
        self.wf_head = (self.wf_head + WF_ROWS - 1) % WF_ROWS;
        let row = &mut self.wf_db[self.wf_head * cols..(self.wf_head + 1) * cols];
        for (c, out) in row.iter_mut().enumerate() {
            // Each column keeps the strongest bin it covers.
            let a = c * n / cols;
            let b = ((c + 1) * n / cols).max(a + 1);
            *out = row_src[a..b].iter().copied().fold(f32::MIN, f32::max) - self.wf_shift_db;
        }
        self.wf_time[self.wf_head] = now;
        self.wf_count = (self.wf_count + 1).min(WF_ROWS);
        self.wf_new_rows.push_back(self.wf_head);
        if self.wf_new_rows.len() > 64 {
            self.wf_dirty_all = true;
            self.wf_new_rows.clear();
        }
        if self.last_row_at > 0.0 {
            let dt = (now - self.last_row_at).clamp(0.005, 0.5);
            self.row_interval += (dt - self.row_interval) * 0.05;
        }
        self.last_row_at = now;
        self.last_frame = Some(Instant::now());
    }

    fn shift_waterfall(&mut self, frac: f64) {
        let cols = self.wf_cols;
        if cols == 0 {
            return;
        }
        let shift = (frac * cols as f64).round() as isize;
        if shift.unsigned_abs() >= cols {
            self.wf_db.fill(-200.0);
        } else {
            for r in 0..WF_ROWS {
                shift_vec(&mut self.wf_db[r * cols..(r + 1) * cols], shift, -200.0);
            }
        }
        self.wf_dirty_all = true;
    }

    /// Black and white points of the waterfall, dB.
    fn wf_levels(&mut self, view: &View) -> (f32, f32) {
        let black = if view.wf_auto_black {
            let want = self.floor_est + view.wf_black;
            if !(self.auto_black - want).abs().le(&1.5) {
                self.auto_black = want.round();
            }
            self.auto_black
        } else {
            view.wf_black.round()
        };
        (black, black + view.wf_range.max(6.0))
    }

    fn color_row(&self, row: usize, black: f32, top: f32, lut: &[Color32; 256], out: &mut Vec<Color32>) {
        let cols = self.wf_cols;
        let scale = 1023.0 / (top - black).max(1.0);
        for &v in &self.wf_db[row * cols..(row + 1) * cols] {
            if v < -190.0 {
                out.push(theme::ink());
                continue;
            }
            // The tone table lifts weak signals off the floor.
            let k = ((v - black) * scale).clamp(0.0, 1023.0) as usize;
            out.push(lut[self.tone[k] as usize]);
        }
    }

    fn upload(&mut self, ctx: &egui::Context, view: &View) {
        if self.wf_cols == 0 {
            return;
        }
        let (black, top) = self.wf_levels(view);
        let want = (black, top, view.palette);
        if want != self.wf_levels {
            self.wf_levels = want;
            self.wf_dirty_all = true;
        }
        let lut = view.palette.lut();
        let opts = egui::TextureOptions {
            magnification: egui::TextureFilter::Linear,
            minification: egui::TextureFilter::Linear,
            ..Default::default()
        };
        if self.wf_dirty_all || self.wf_tex.is_none() {
            let mut px = Vec::with_capacity(self.wf_cols * WF_ROWS);
            for r in 0..WF_ROWS {
                self.color_row(r, black, top, lut, &mut px);
            }
            let img = egui::ColorImage::new([self.wf_cols, WF_ROWS], px);
            match &mut self.wf_tex {
                Some(t) => t.set(img, opts),
                None => self.wf_tex = Some(ctx.load_texture("waterfall", img, opts)),
            }
            self.wf_dirty_all = false;
            self.wf_new_rows.clear();
        } else {
            while let Some(r) = self.wf_new_rows.pop_front() {
                let mut px = Vec::with_capacity(self.wf_cols);
                self.color_row(r, black, top, lut, &mut px);
                if let Some(t) = &mut self.wf_tex {
                    t.set_partial([0, r], egui::ColorImage::new([self.wf_cols, 1], px), opts);
                }
            }
        }
    }

    /// Draw and handle input. `status` supplies the passband and markers.
    #[allow(clippy::too_many_arguments)]
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        status: &Status,
        frame: Option<&FftFrame>,
        view: &mut View,
        flag: &SliceFlag,
        step_hz: f64,
        reduce_motion: bool,
        acts: &mut Vec<Act>,
    ) -> Response {
        let full = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(full, Sense::click_and_drag());
        let ctx = ui.ctx().clone();
        let now = ui.input(|i| i.time);
        let dt = ui.input(|i| i.stable_dt).min(0.1);

        // Ease the range toward its target.
        let k = if reduce_motion { 1.0 } else { 1.0 - (-dt * 6.0).exp() };
        self.min_db += (self.target_min - self.min_db) * k;
        self.max_db += (self.target_max - self.max_db) * k;
        if (self.min_db - self.target_min).abs() > 0.05 || (self.max_db - self.target_max).abs() > 0.05 {
            crate::anim::more(&ctx);
        }

        let axis_h = 22.0;
        let db_w = 44.0;
        let split_y = full.top() + (full.height() - axis_h) * view.split.clamp(0.15, 0.85);
        let pan = Rect::from_min_max(full.min, pos2(full.right() - db_w, split_y));
        let db_axis = Rect::from_min_max(pos2(full.right() - db_w, full.top()), pos2(full.right(), split_y));
        let axis = Rect::from_min_max(pos2(full.left(), split_y), pos2(full.right(), split_y + axis_h));
        let wf = Rect::from_min_max(pos2(full.left(), split_y + axis_h), pos2(full.right() - db_w, full.bottom()));
        let time_axis = Rect::from_min_max(pos2(full.right() - db_w, split_y + axis_h), full.max);
        let painter = ui.painter_at(full);
        self.regions = Some([pan, wf, axis, db_axis]);

        // ---- backgrounds
        painter.rect_filled(full, CornerRadius::ZERO, theme::ink());
        let bg = Mesh {
            indices: vec![0, 1, 2, 2, 1, 3],
            vertices: vec![
                crate::brand::vtx(pan.left_top(), theme::surface_2()),
                crate::brand::vtx(pan.right_top(), theme::surface_2()),
                crate::brand::vtx(pan.left_bottom(), theme::ink()),
                crate::brand::vtx(pan.right_bottom(), theme::ink()),
            ],
            texture_id: egui::TextureId::default(),
        };
        painter.add(Shape::mesh(bg));

        if self.span_hz <= 0.0 {
            painter.text(
                pan.center(),
                Align2::CENTER_CENTER,
                "waiting for the first spectrum frame",
                theme::sans(13.0),
                theme::text_3(),
            );
            return resp;
        }

        // ---- grid: dB lines and frequency lines
        let db_step = nice_step((self.max_db - self.min_db) as f64, 6.0) as f32;
        let mut db = (self.min_db / db_step).ceil() * db_step;
        while db <= self.max_db {
            let y = self.y_for(db, pan);
            painter.line_segment([pos2(pan.left(), y), pos2(pan.right(), y)], Stroke::new(1.0, theme::line()));
            // Clear of the unit caption at the top of the scale.
            if y < db_axis.top() + 20.0 {
                db += db_step;
                continue;
            }
            painter.text(
                pos2(db_axis.right() - 6.0, y),
                Align2::RIGHT_CENTER,
                format!("{db:.0}"),
                theme::mono(10.5),
                theme::text_3(),
            );
            db += db_step;
        }
        painter.text(
            pos2(db_axis.right() - 6.0, db_axis.top() + 8.0),
            Align2::RIGHT_CENTER,
            "dBFS",
            theme::mono(9.5),
            theme::text_3(),
        );
        let f_step = nice_step(self.span_hz, (pan.width() / 110.0).max(3.0) as f64);
        let lo = self.hz_for(pan.left(), pan);
        let hi = self.hz_for(pan.right(), pan);
        let mut f = (lo / f_step).ceil() * f_step;
        let decimals = if f_step >= 1e6 {
            1
        } else if f_step >= 1e5 {
            2
        } else if f_step >= 1e3 {
            3
        } else {
            4
        };
        painter.rect_filled(axis, CornerRadius::ZERO, theme::surface_1());
        painter.line_segment([axis.left_top(), axis.right_top()], Stroke::new(1.0, theme::line_2()));
        painter.line_segment([axis.left_bottom(), axis.right_bottom()], Stroke::new(1.0, theme::line()));
        while f <= hi {
            let x = self.x_for(f, pan);
            painter.line_segment([pos2(x, pan.top()), pos2(x, pan.bottom())], Stroke::new(1.0, theme::line()));
            painter.line_segment([pos2(x, axis.top()), pos2(x, axis.top() + 4.0)], Stroke::new(1.0, theme::text_3()));
            if x > axis.left() + 30.0 && x < pan.right() - 30.0 {
                painter.text(
                    pos2(x, axis.center().y + 1.0),
                    Align2::CENTER_CENTER,
                    crate::tuning::fmt_mhz(f, decimals),
                    theme::mono(10.5),
                    theme::text_2(),
                );
            }
            f += f_step;
        }

        // ---- waterfall
        self.upload(&ctx, view);
        if let Some(tex) = &self.wf_tex {
            let rows_vis = (wf.height().round() as usize).clamp(1, WF_ROWS);
            let row_px = wf.height() / rows_vis as f32;
            // Smooth scroll: the newest row slides in over one row interval.
            let frac = if reduce_motion {
                0.0
            } else {
                (((now - self.last_row_at) / self.row_interval.max(0.01)) as f32).clamp(0.0, 1.0)
            };
            let painter_wf = ui.painter_at(wf);
            let x0 = self.drag_offset_px;
            let top = wf.top() - (1.0 - frac) * row_px;
            // Rows head, head+1, … wrap at WF_ROWS. Draw in at most two runs.
            let mut drawn = 0usize;
            let mut start = self.wf_head;
            while drawn < rows_vis + 1 {
                let run = (WF_ROWS - start).min(rows_vis + 1 - drawn);
                let y0 = top + drawn as f32 * row_px;
                let rect = Rect::from_min_max(
                    pos2(wf.left() + x0, y0),
                    pos2(wf.right() + x0, y0 + run as f32 * row_px),
                );
                let uv = Rect::from_min_max(
                    pos2(0.0, start as f32 / WF_ROWS as f32),
                    pos2(1.0, (start + run) as f32 / WF_ROWS as f32),
                );
                painter_wf.image(tex.id(), rect, uv, Color32::WHITE);
                drawn += run;
                start = (start + run) % WF_ROWS;
            }
            if !reduce_motion && frac < 1.0 {
                crate::anim::more(&ctx);
            }
            // Time marks, measured from the rows themselves.
            painter.rect_filled(time_axis, CornerRadius::ZERO, theme::surface_1());
            let newest = self.wf_time[self.wf_head];
            let mut next_mark = 5.0;
            for i in 0..rows_vis.min(self.wf_count) {
                let r = (self.wf_head + i) % WF_ROWS;
                let age = newest - self.wf_time[r];
                if age >= next_mark && self.wf_time[r] > 0.0 {
                    let y = top + i as f32 * row_px;
                    painter.line_segment(
                        [pos2(time_axis.left(), y), pos2(time_axis.left() + 4.0, y)],
                        Stroke::new(1.0, theme::text_3()),
                    );
                    painter.text(
                        pos2(time_axis.right() - 6.0, y),
                        Align2::RIGHT_CENTER,
                        format!("−{:.0}s", next_mark),
                        theme::mono(10.0),
                        theme::text_3(),
                    );
                    next_mark += 5.0;
                }
            }
        }

        // ---- the trace, decimated to one point per pixel column (the
        // strongest bin under it, so a narrow carrier never vanishes).
        let n = self.smooth.len();
        let cols = (pan.width().round() as usize).clamp(2, 8192);
        let column = |v: &[f32], c: usize| -> f32 {
            let a = c * n / cols;
            let b = ((c + 1) * n / cols).max(a + 1).min(n);
            v[a.min(n - 1)..b].iter().copied().fold(f32::MIN, f32::max)
        };
        let mut line: Vec<Pos2> = Vec::with_capacity(cols);
        if n >= 2 {
            let mut raw: Vec<f32> = (0..cols).map(|c| column(&self.smooth, c)).collect();
            // A light three-tap smooth for the drawn line only.
            if cols > 3 {
                let src = raw.clone();
                for i in 1..cols - 1 {
                    raw[i] = 0.18 * src[i - 1] + 0.64 * src[i] + 0.18 * src[i + 1];
                }
            }
            for (c, v) in raw.iter().enumerate() {
                let x = pan.left() + c as f32 / (cols - 1) as f32 * pan.width() + self.drag_offset_px;
                line.push(pos2(x, self.y_for(*v, pan)));
            }
            let pan_painter = ui.painter_at(pan);
            if view.fill {
                // Heat: the fill brightens with level, so strong signals glow
                // and the floor stays a quiet wash.
                let mut mesh = Mesh::default();
                for p in &line {
                    let i = mesh.vertices.len() as u32;
                    let h = ((pan.bottom() - p.y) / pan.height()).clamp(0.0, 1.0);
                    let top_c = theme::lerp(theme::alpha(theme::AMBER, 0.10), theme::alpha(theme::AMBER, 0.55), h);
                    mesh.colored_vertex(*p, top_c);
                    mesh.colored_vertex(pos2(p.x, pan.bottom()), alpha(theme::AMBER, 0.0));
                    if i >= 2 {
                        mesh.add_triangle(i - 2, i - 1, i);
                        mesh.add_triangle(i - 1, i + 1, i);
                    }
                }
                pan_painter.add(Shape::mesh(mesh));
            }
            if self.max_hold.len() == n {
                let pts: Vec<Pos2> = (0..cols)
                    .map(|c| {
                        let x = pan.left() + c as f32 / (cols - 1) as f32 * pan.width() + self.drag_offset_px;
                        pos2(x, self.y_for(column(&self.max_hold, c), pan))
                    })
                    .collect();
                pan_painter.add(Shape::line(pts, Stroke::new(1.0, alpha(theme::VIOLET, 0.35))));
            }
            if view.peak_hold && self.peak.len() == n {
                let pts: Vec<Pos2> = (0..cols)
                    .map(|c| {
                        let x = pan.left() + c as f32 / (cols - 1) as f32 * pan.width() + self.drag_offset_px;
                        pos2(x, self.y_for(column(&self.peak, c), pan))
                    })
                    .collect();
                pan_painter.add(Shape::line(pts, Stroke::new(1.0, alpha(theme::CYAN, 0.5))));
            }
            pan_painter.add(Shape::line(line.clone(), Stroke::new(3.2, alpha(theme::AMBER, 0.22))));
            pan_painter.add(Shape::line(line.clone(), Stroke::new(1.25, Color32::from_rgb(0xff, 0xe4, 0xbd))));
        }

        // ---- spurs being notched
        for off in &status.spurs_hz {
            let x = self.x_for(status.freq_hz + off, pan);
            if x < pan.left() || x > pan.right() {
                continue;
            }
            dashed_v(&painter, x, pan.top(), pan.bottom(), alpha(theme::ERR, 0.8));
            painter.text(pos2(x + 3.0, pan.top() + 10.0), Align2::LEFT_CENTER, "NOTCH", theme::mono(9.5), theme::ERR);
        }

        // ---- scan marks
        if let Some(scan) = &status.scan {
            for &hz in &scan.testing {
                let x = self.x_for(hz, pan);
                dashed_v(&painter, x, pan.top(), wf.bottom(), alpha(theme::WARN, 0.75));
            }
            for &hz in &scan.locked {
                let x = self.x_for(hz, pan);
                painter.line_segment([pos2(x, pan.top()), pos2(x, wf.bottom())], Stroke::new(1.5, alpha(theme::OK, 0.7)));
            }
        }
        if let Some(hz) = status.pager_live_hz {
            let x = self.x_for(hz, pan);
            dashed_v(&painter, x, pan.top(), pan.bottom(), alpha(theme::VIOLET, 0.8));
        }

        // ---- passband and carrier
        let inspect = frame.map(|f| f.inspect_hz).filter(|h| *h > 0.0).unwrap_or(status.inspect_hz);
        let ix = self.x_for(inspect, pan);
        let bw = status.bandwidth_hz.max(100.0);
        let pc = inspect + status.passband_offset_hz;
        let px0 = self.x_for(pc - bw / 2.0, pan);
        let px1 = self.x_for(pc + bw / 2.0, pan);
        let wide = (px1 - px0) > pan.width() * 0.25;
        let on_screen = ix >= pan.left() - 2.0 && ix <= pan.right() + 2.0;
        if on_screen {
            let band = Rect::from_min_max(pos2(px0.max(pan.left()), pan.top()), pos2(px1.min(pan.right()), pan.bottom()));
            painter.rect_filled(band, CornerRadius::ZERO, alpha(theme::AMBER, if wide { 0.05 } else { 0.10 }));
            let wf_band = Rect::from_min_max(pos2(px0.max(wf.left()), wf.top()), pos2(px1.min(wf.right()), wf.bottom()));
            painter.rect_filled(wf_band, CornerRadius::ZERO, alpha(theme::AMBER, 0.035));
            if px1 - px0 > 6.0 {
                for x in [px0, px1] {
                    painter.line_segment([pos2(x, pan.top()), pos2(x, pan.bottom())], Stroke::new(1.0, alpha(theme::AMBER, 0.45)));
                    dashed_v(&painter, x, wf.top(), wf.bottom(), alpha(theme::AMBER, 0.22));
                }
            }
            painter.line_segment([pos2(ix, pan.top()), pos2(ix, pan.bottom())], Stroke::new(1.4, theme::AMBER));
            painter.add(Shape::convex_polygon(
                vec![pos2(ix - 6.0, axis.top()), pos2(ix + 6.0, axis.top()), pos2(ix, axis.top() + 7.0)],
                theme::AMBER,
                Stroke::NONE,
            ));
        } else {
            // Off-screen: a chevron at the edge it went past, which brings
            // it back when clicked.
            let left = ix < pan.left();
            let x = if left { pan.left() + 4.0 } else { pan.right() - 4.0 };
            let y = pan.center().y;
            let dir = if left { -1.0 } else { 1.0 };
            painter.add(Shape::convex_polygon(
                vec![pos2(x, y), pos2(x - dir * 9.0, y - 9.0), pos2(x - dir * 9.0, y + 9.0)],
                theme::AMBER,
                Stroke::NONE,
            ));
            let r = Rect::from_center_size(pos2(x - dir * 6.0, y), vec2(22.0, 26.0));
            if ui.interact(r, resp.id.with("offscreen"), Sense::click()).on_hover_text("Bring the receiver back into view").clicked() {
                acts.push(Act::Center(inspect));
            }
        }

        // ---- peaks and decode spots
        if view.show_peaks
            && let Some(f) = frame
        {
            let mut placed: Vec<Rect> = Vec::new();
            for p in &f.peaks {
                let x = self.x_for(p.freq_hz, pan);
                if x < pan.left() || x > pan.right() {
                    continue;
                }
                let near = (p.freq_hz - inspect).abs() < 10e3;
                let c = if near { theme::AMBER } else { theme::text_2() };
                painter.add(Shape::convex_polygon(
                    vec![pos2(x, pan.bottom() - 3.0), pos2(x - 3.5, pan.bottom() - 10.0), pos2(x + 3.5, pan.bottom() - 10.0)],
                    alpha(c, 0.85),
                    Stroke::NONE,
                ));
                if let Some(label) = &p.label {
                    let text = format!("{label} {}", crate::tuning::fmt_mhz(p.freq_hz, 3));
                    let g = painter.layout_no_wrap(text, theme::mono(10.0), theme::text());
                    let w = g.size().x + 10.0;
                    let mut r = Rect::from_min_size(pos2(x + 6.0, pan.bottom() - 30.0), vec2(w, 16.0));
                    if r.right() > pan.right() {
                        r = r.translate(vec2(-w - 12.0, 0.0));
                    }
                    // Stack upward instead of overlapping another spot.
                    let mut guard = 0;
                    while placed.iter().any(|o| o.intersects(r)) && guard < 6 {
                        r = r.translate(vec2(0.0, -19.0));
                        guard += 1;
                    }
                    placed.push(r);
                    painter.rect(r, CornerRadius::same(3), alpha(theme::ink(), 0.88), Stroke::new(1.0, alpha(c, 0.6)), egui::StrokeKind::Inside);
                    painter.galley(pos2(r.left() + 5.0, r.center().y - g.size().y / 2.0), g, theme::text());
                }
            }
        }

        // ---- squelch mark (a visual threshold; the server has no squelch)
        if let Some(sql) = view.sql_mark {
            let y = self.y_for(sql, pan);
            let c = if self.sql_drag { theme::ERR } else { alpha(theme::ERR, 0.7) };
            dashed_h(&painter, pan.left(), pan.right(), y, c);
            let g = painter.layout_no_wrap(format!("MARK {sql:.0} dB"), theme::mono(9.5), theme::ERR);
            let r = Rect::from_min_size(pos2(pan.left() + 6.0, y - 8.0), vec2(g.size().x + 8.0, 15.0));
            painter.rect(r, CornerRadius::same(3), alpha(theme::ink(), 0.9), Stroke::new(1.0, alpha(theme::ERR, 0.6)), egui::StrokeKind::Inside);
            painter.galley(pos2(r.left() + 4.0, r.center().y - g.size().y / 2.0), g, theme::ERR);
        }

        // ---- slice flag at the carrier
        if on_screen {
            let active_a = !self.rx_b.as_ref().is_some_and(|b| b.active);
            self.flags[0] = Some(flag.paint(&painter, pan, ix, line_y_at(&line, ix).unwrap_or(pan.center().y), active_a));
        } else {
            self.flags[0] = None;
        }
        self.flags[1] = None;
        if let Some(b) = self.rx_b.clone() {
            let bx = self.x_for(b.freq_hz, pan);
            if bx >= pan.left() && bx <= pan.right() {
                let c = b.offset_hz + b.freq_hz;
                let x0 = self.x_for(c - b.bandwidth_hz / 2.0, pan);
                let x1 = self.x_for(c + b.bandwidth_hz / 2.0, pan);
                let band = Rect::from_min_max(pos2(x0.max(pan.left()), pan.top()), pos2(x1.min(pan.right()), pan.bottom()));
                painter.rect_filled(band, CornerRadius::ZERO, alpha(theme::VIOLET, if b.active { 0.12 } else { 0.07 }));
                let wf_band = Rect::from_min_max(pos2(x0.max(wf.left()), wf.top()), pos2(x1.min(wf.right()), wf.bottom()));
                painter.rect_filled(wf_band, CornerRadius::ZERO, alpha(theme::VIOLET, 0.035));
                painter.line_segment([pos2(bx, pan.top()), pos2(bx, pan.bottom())], Stroke::new(1.4, theme::VIOLET));
                painter.add(Shape::convex_polygon(
                    vec![pos2(bx - 6.0, axis.top()), pos2(bx + 6.0, axis.top()), pos2(bx, axis.top() + 7.0)],
                    theme::VIOLET,
                    Stroke::NONE,
                ));
                self.flags[1] = Some(paint_b_flag(&painter, pan, bx, &b));
            }
        }

        // ---- interaction
        let pointer = resp.hover_pos();
        let alt = ui.input(|i| i.modifiers.alt);
        let shift = ui.input(|i| i.modifiers.shift);
        let ctrl = ui.input(|i| i.modifiers.command);
        let in_area = |p: Pos2| pan.contains(p) || wf.contains(p) || axis.contains(p);

        // Splitter between panadapter and waterfall.
        let split_r = Rect::from_min_max(pos2(axis.left(), axis.top() - 3.0), pos2(axis.right(), axis.top() + 3.0));
        let split_resp = ui.interact(split_r, resp.id.with("split"), Sense::drag());
        if split_resp.hovered() || split_resp.dragged() {
            ctx.set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
        if split_resp.dragged()
            && let Some(p) = split_resp.interact_pointer_pos()
        {
            view.split = ((p.y - full.top()) / (full.height() - axis_h)).clamp(0.15, 0.85);
            self.split_drag = true;
        }

        // dB axis: drag shifts the range, wheel widens or narrows it.
        let db_resp = ui.interact(db_axis, resp.id.with("db"), Sense::drag());
        if db_resp.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::ResizeVertical);
            let scroll = crate::widgets::wheel(ui);
            if scroll != 0.0 {
                let span = (view.ceiling_db - view.floor_db).max(10.0);
                let s = if scroll > 0.0 { 0.9 } else { 1.1 };
                if view.auto_range {
                    view.floor_db = self.min_db;
                    view.ceiling_db = self.max_db;
                    view.auto_range = false;
                }
                let mid = (view.floor_db + view.ceiling_db) / 2.0;
                let ns = (span * s).clamp(10.0, 160.0);
                view.floor_db = mid - ns / 2.0;
                view.ceiling_db = mid + ns / 2.0;
            }
        }
        if db_resp.drag_started() {
            if view.auto_range {
                view.floor_db = self.min_db;
                view.ceiling_db = self.max_db;
                view.auto_range = false;
            }
            let y0 = ui.input(|i| i.pointer.press_origin()).or(db_resp.interact_pointer_pos()).map_or(0.0, |p| p.y);
            self.db_drag = Some((y0, view.floor_db, view.ceiling_db));
        }
        if let (Some((y0, f0, c0)), Some(p)) = (self.db_drag, db_resp.interact_pointer_pos())
            && db_resp.dragged()
        {
            let per_px = (c0 - f0) / pan.height().max(1.0);
            let d = (p.y - y0) * per_px;
            view.floor_db = f0 + d;
            view.ceiling_db = c0 + d;
        }
        if db_resp.drag_stopped() {
            self.db_drag = None;
        }

        if let Some(p) = pointer.filter(|p| in_area(*p)) {
            let hz = self.hz_for(p.x, pan);
            let landing = if alt { hz.round() } else { crate::tuning::snap(hz, step_hz) };
            self.hover_hz = Some(landing);
            // Hover readout: where a click would land, and the level there.
            let lx = self.x_for(landing, pan);
            painter.line_segment([pos2(lx, pan.top()), pos2(lx, wf.bottom())], Stroke::new(1.0, Color32::from_white_alpha(70)));
            let level = if n >= 2 {
                let bin = (((landing - (self.center_hz - self.span_hz / 2.0)) / self.span_hz) * (n - 1) as f64)
                    .round()
                    .clamp(0.0, (n - 1) as f64) as usize;
                Some(self.smooth[bin])
            } else {
                None
            };
            let text = match level {
                Some(l) if pan.contains(p) => format!("{} MHz · {l:.1} dBFS", crate::tuning::fmt_mhz(landing, 4)),
                _ => format!("{} MHz", crate::tuning::fmt_mhz(landing, 4)),
            };
            let g = painter.layout_no_wrap(text, theme::mono(11.0), theme::text());
            let mut r = Rect::from_min_size(pos2(p.x + 12.0, p.y + 14.0), g.size() + vec2(12.0, 8.0));
            if r.right() > full.right() - 4.0 {
                r = r.translate(vec2(-r.width() - 24.0, 0.0));
            }
            if r.bottom() > full.bottom() - 4.0 {
                r = r.translate(vec2(0.0, -r.height() - 28.0));
            }
            painter.rect(r, CornerRadius::same(4), alpha(theme::surface_3(), 0.94), Stroke::new(1.0, theme::line_2()), egui::StrokeKind::Inside);
            painter.galley(r.min + vec2(6.0, 4.0), g, theme::text());

            // Squelch mark drag.
            let near_sql = view.sql_mark.is_some_and(|s| pan.contains(p) && (self.y_for(s, pan) - p.y).abs() < 6.0);
            if near_sql || self.sql_drag {
                ctx.set_cursor_icon(egui::CursorIcon::ResizeVertical);
            } else if !split_resp.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
            }
            if resp.drag_started() && near_sql {
                self.sql_drag = true;
            }

            // Wheel: tune by the step and recenter (a dial turn); Ctrl zooms.
            let scroll = crate::widgets::wheel(ui);
            if scroll != 0.0 && !db_resp.hovered() {
                self.scroll_accum += scroll;
                let notches = self.scroll_accum.trunc();
                if notches != 0.0 {
                    self.scroll_accum -= notches;
                    if ctrl {
                        let z = (status.zoom.max(1.0) * 2f64.powf(0.25 * notches as f64)).clamp(1.0, 32.0);
                        acts.push(Act::Zoom(z));
                    } else {
                        let step = if alt { self.span_hz / 10.0 } else { step_hz };
                        let base = self.rx_b.as_ref().filter(|b| b.active).map_or(inspect, |b| b.freq_hz);
                        let target = base + notches as f64 * step;
                        let hz = if alt { target.round() } else { crate::tuning::snap(target, step) };
                        acts.push(Act::Tune { hz, lock: true });
                    }
                }
            }
        } else {
            self.hover_hz = None;
        }

        // Passband edges: drag one to set it; Ctrl-drag inside a sideband
        // passband to shift it. The filter applet does the same by numbers.
        if let Some((range, lo, hi)) = self.filter
            && on_screen
        {
            let near = |p: Pos2| -> Option<EdgeDrag> {
                if !pan.contains(p) {
                    return None;
                }
                if (p.x - px0).abs() < 5.0 {
                    Some(EdgeDrag::Low)
                } else if (p.x - px1).abs() < 5.0 {
                    Some(EdgeDrag::High)
                } else if !range.symmetric && p.x > px0 && p.x < px1 && ctrl {
                    Some(EdgeDrag::Shift { from_hz: 0.0, lo, hi })
                } else {
                    None
                }
            };
            if let Some(p) = pointer
                && self.edge_drag.is_none()
                && let Some(kind) = near(p)
            {
                ctx.set_cursor_icon(if matches!(kind, EdgeDrag::Shift { .. }) { egui::CursorIcon::Move } else { egui::CursorIcon::ResizeColumn });
            }
            if resp.drag_started()
                && let Some(p0) = ui.input(|i| i.pointer.press_origin())
                && let Some(kind) = near(p0)
            {
                self.edge_drag = Some(match kind {
                    EdgeDrag::Shift { lo, hi, .. } => EdgeDrag::Shift { from_hz: self.hz_for(p0.x, pan) - inspect, lo, hi },
                    k => k,
                });
            }
            if let Some(d) = self.edge_drag {
                if let Some(p) = resp.interact_pointer_pos()
                    && resp.dragged()
                {
                    let rel = (self.hz_for(p.x, pan) - inspect) as f32;
                    let (nlo, nhi) = drag_edges(d, &range, lo, hi, rel);
                    if (nlo - lo).abs() > 1.0 || (nhi - hi).abs() > 1.0 {
                        acts.push(Act::Filter(nlo.round(), nhi.round()));
                    }
                    painter.text(
                        pos2(p.x, pan.top() + 72.0),
                        Align2::CENTER_BOTTOM,
                        format!("{:+.2}k … {:+.2}k", nlo / 1000.0, nhi / 1000.0),
                        theme::mono(11.0),
                        theme::AMBER,
                    );
                }
                if resp.drag_stopped() || !ui.input(|i| i.pointer.any_down()) {
                    self.edge_drag = None;
                }
            }
        }

        if self.edge_drag.is_some() {
            // The passband has the drag; the display stays put.
        } else if self.sql_drag {
            if let Some(p) = resp.interact_pointer_pos() {
                let t = ((pan.bottom() - p.y) / pan.height()).clamp(0.0, 1.0);
                view.sql_mark = Some((self.min_db + t * (self.max_db - self.min_db)).round());
            }
            if resp.drag_stopped() || !resp.dragged() && !resp.drag_started() {
                self.sql_drag = false;
            }
        } else if !self.split_drag && self.db_drag.is_none() {
            // Drag the display to move the span (or, zoomed in, the receiver
            // the zoomed window follows).
            if resp.drag_started() {
                // From where the button went down, not where egui decided a
                // drag had begun a few pixels later.
                let x = ui
                    .input(|i| i.pointer.press_origin())
                    .or(resp.interact_pointer_pos())
                    .map_or(0.0, |p| p.x);
                self.drag_origin = Some((if status.zoom > 1.01 { inspect } else { self.center_hz }, x));
            }
            if let (Some((origin, x0)), Some(p)) = (self.drag_origin, resp.interact_pointer_pos())
                && resp.dragged()
            {
                self.drag_offset_px = p.x - x0;
                ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                let _ = origin;
            }
            if resp.drag_stopped()
                && let Some((origin, _)) = self.drag_origin.take()
            {
                let d_hz = -(self.drag_offset_px / pan.width().max(1.0)) as f64 * self.span_hz;
                self.drag_offset_px = 0.0;
                if d_hz.abs() > 1.0 {
                    if status.zoom > 1.01 {
                        acts.push(Act::Tune { hz: crate::tuning::snap(origin + d_hz, step_hz), lock: false });
                    } else {
                        acts.push(Act::Center(origin + d_hz));
                    }
                }
            }
        }
        if split_resp.drag_stopped() {
            self.split_drag = false;
        }

        // Clicks.
        let on_flag = resp
            .interact_pointer_pos()
            .and_then(|p| self.flags.iter().position(|f| f.is_some_and(|r| r.contains(p))));
        if let Some(i) = on_flag {
            if resp.clicked() {
                acts.push(Act::Select(i as u8));
            }
            if resp.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        } else if let Some(p) = resp.interact_pointer_pos().filter(|p| in_area(*p)) {
            let hz = self.hz_for(p.x, pan);
            let landing = if alt { hz.round() } else { crate::tuning::snap(hz, step_hz) };
            if resp.middle_clicked() {
                acts.push(Act::PlaceB(landing));
            }
            if resp.double_clicked() {
                acts.push(Act::Tune { hz: landing, lock: true });
            } else if resp.clicked() && !self.sql_drag {
                if shift {
                    acts.push(Act::Center(landing));
                } else {
                    acts.push(Act::Tune { hz: landing, lock: false });
                }
            }
            if resp.secondary_clicked() {
                self.menu_hz = landing;
            }
        }
        let menu_hz = self.menu_hz;
        resp.context_menu(|ui| {
            ui.label(egui::RichText::new(format!("{} MHz", crate::tuning::fmt_mhz(menu_hz, 5))).font(theme::mono(12.0)).color(theme::AMBER));
            ui.separator();
            if ui.button("Tune here").clicked() {
                acts.push(Act::Tune { hz: menu_hz, lock: false });
                ui.close();
            }
            if ui.button("Tune and center").clicked() {
                acts.push(Act::Tune { hz: menu_hz, lock: true });
                ui.close();
            }
            if ui.button("Put receiver B here").clicked() {
                acts.push(Act::PlaceB(menu_hz));
                ui.close();
            }
            if ui.button("Center the span here").clicked() {
                acts.push(Act::Center(menu_hz));
                ui.close();
            }
            if ui.button("Store as a memory").clicked() {
                acts.push(Act::StoreMemory(menu_hz));
                ui.close();
            }
            if ui.button("Copy frequency").clicked() {
                acts.push(Act::CopyFreq(menu_hz));
                ui.close();
            }
            ui.separator();
            ui.checkbox(&mut view.peak_hold, "Peak hold");
            ui.checkbox(&mut view.fill, "Trace fill");
            ui.checkbox(&mut view.show_peaks, "Peak markers and spots");
            ui.checkbox(&mut view.auto_range, "Auto range");
            let mut mark = view.sql_mark.is_some();
            if ui.checkbox(&mut mark, "Squelch mark").changed() {
                view.sql_mark = mark.then_some(self.floor_est + 10.0);
            }
            ui.menu_button("Palette", |ui| {
                for m in Colormap::ALL {
                    if ui.selectable_label(view.palette == m, m.label()).clicked() {
                        view.palette = m;
                        ui.close();
                    }
                }
            });
        });
        resp
    }
}

fn line_y_at(line: &[Pos2], x: f32) -> Option<f32> {
    line.iter().min_by(|a, b| (a.x - x).abs().total_cmp(&(b.x - x).abs())).map(|p| p.y)
}

fn shift_vec(v: &mut [f32], shift: isize, fill: f32) {
    let n = v.len() as isize;
    if shift == 0 || n == 0 {
        return;
    }
    if shift.abs() >= n {
        v.fill(fill);
        return;
    }
    if shift > 0 {
        let s = shift as usize;
        v.copy_within(s.., 0);
        let len = v.len();
        v[len - s..].fill(fill);
    } else {
        let s = (-shift) as usize;
        let len = v.len();
        v.copy_within(..len - s, s);
        v[..s].fill(fill);
    }
}

pub fn dashed_v(painter: &egui::Painter, x: f32, y0: f32, y1: f32, c: Color32) {
    let mut y = y0;
    while y < y1 {
        painter.line_segment([pos2(x, y), pos2(x, (y + 3.0).min(y1))], Stroke::new(1.0, c));
        y += 6.0;
    }
}

pub fn dashed_h(painter: &egui::Painter, x0: f32, x1: f32, y: f32, c: Color32) {
    let mut x = x0;
    while x < x1 {
        painter.line_segment([pos2(x, y), pos2((x + 4.0).min(x1), y)], Stroke::new(1.2, c));
        x += 8.0;
    }
}

/// The card at the carrier: what the receiver is on and what it hears.
#[derive(Default, Clone)]
pub struct SliceFlag {
    pub freq_hz: f64,
    pub mode: Mode,
    pub bandwidth_hz: f64,
    /// Smoothed meter position 0..1 and the S-unit label.
    pub meter: f32,
    pub s_label: String,
    pub snr_db: Option<f32>,
    pub protocol: Option<String>,
    pub recording: bool,
}

impl SliceFlag {
    fn paint(&self, painter: &egui::Painter, pan: Rect, x: f32, trace_y: f32, active: bool) -> Rect {
        let w = 168.0;
        let h = 62.0;
        let right_side = x + 10.0 + w < pan.right() - 4.0;
        let left = if right_side { x + 10.0 } else { x - 10.0 - w };
        // Keep the card clear of the trace where there is room above it.
        let top = (pan.top() + 8.0).max((trace_y - h - 14.0).min(pan.top() + 8.0));
        let r = Rect::from_min_size(pos2(left, top), vec2(w, h));
        painter.rect(r, CornerRadius::same(4), alpha(theme::surface_2(), 0.93), Stroke::new(1.0, alpha(theme::AMBER, 0.45)), egui::StrokeKind::Inside);
        theme::glass(painter, r);
        // The index tick: the active receiver.
        if active {
            painter.rect_filled(Rect::from_min_size(r.left_top(), vec2(16.0, 2.0)), CornerRadius::ZERO, theme::EMBER);
        }
        painter.text(r.right_bottom() + vec2(-8.0, -6.0), Align2::RIGHT_BOTTOM, "A", theme::mono_bold(10.0), theme::text_3());
        let anchor_x = if right_side { r.left() } else { r.right() };
        painter.line_segment([pos2(x, r.top() + 14.0), pos2(anchor_x, r.top() + 14.0)], Stroke::new(1.0, alpha(theme::AMBER, 0.5)));
        painter.text(r.left_top() + vec2(8.0, 7.0), Align2::LEFT_TOP, crate::tuning::fmt_mhz(self.freq_hz, 4), theme::mono_bold(14.0), theme::text());
        let chip = format!("{} · {}", self.mode.label(), crate::tuning::fmt_short_hz(self.bandwidth_hz));
        painter.text(r.right_top() + vec2(-8.0, 9.0), Align2::RIGHT_TOP, chip, theme::mono(10.0), theme::AMBER);
        // Mini S-meter.
        let bar = Rect::from_min_size(r.left_top() + vec2(8.0, 30.0), vec2(w - 64.0, 6.0));
        painter.rect_filled(bar, CornerRadius::same(1), theme::surface_4());
        let lit = Rect::from_min_size(bar.min, vec2(bar.width() * self.meter.clamp(0.0, 1.0), bar.height()));
        let c = if self.meter > 0.556 { theme::WARN } else { theme::OK };
        painter.rect_filled(lit, CornerRadius::same(1), c);
        painter.text(pos2(r.right() - 8.0, bar.center().y), Align2::RIGHT_CENTER, &self.s_label, theme::mono_bold(11.0), theme::text());
        let bottom = match (&self.protocol, self.snr_db) {
            (Some(p), Some(s)) => format!("{p} · SNR {s:.1} dB"),
            (Some(p), None) => p.clone(),
            (None, Some(s)) => format!("SNR {s:.1} dB"),
            _ => String::new(),
        };
        painter.text(r.left_top() + vec2(8.0, 44.0), Align2::LEFT_TOP, bottom, theme::sans(11.0), theme::text_2());
        if self.recording {
            painter.circle_filled(pos2(r.right() - 22.0, r.bottom() - 10.0), 4.0, theme::EMBER);
        }
        r
    }
}

/// Receiver B's flag: smaller than A's, in violet, set below it so the two
/// never cover each other when the receivers are close.
fn paint_b_flag(painter: &egui::Painter, pan: Rect, x: f32, b: &RxB) -> Rect {
    let (w, h) = (150.0, 44.0);
    let right = x + 10.0 + w < pan.right() - 4.0;
    let left = if right { x + 10.0 } else { x - 10.0 - w };
    let r = Rect::from_min_size(pos2(left, pan.top() + 78.0), vec2(w, h));
    painter.rect(r, CornerRadius::same(4), alpha(theme::surface_2(), 0.93), Stroke::new(1.0, alpha(theme::VIOLET, 0.55)), egui::StrokeKind::Inside);
    theme::glass(painter, r);
    if b.active {
        painter.rect_filled(Rect::from_min_size(r.left_top(), vec2(16.0, 2.0)), CornerRadius::ZERO, theme::EMBER);
    }
    let anchor = if right { r.left() } else { r.right() };
    painter.line_segment([pos2(x, r.top() + 12.0), pos2(anchor, r.top() + 12.0)], Stroke::new(1.0, alpha(theme::VIOLET, 0.5)));
    painter.text(r.left_top() + vec2(8.0, 5.0), Align2::LEFT_TOP, crate::tuning::fmt_mhz(b.freq_hz, 4), theme::mono_bold(12.5), theme::text());
    painter.text(r.right_top() + vec2(-8.0, 7.0), Align2::RIGHT_TOP, format!("B · {}", b.mode.label()), theme::mono(10.0), theme::VIOLET);
    match (&b.note, b.level) {
        (Some(n), _) => {
            painter.text(r.left_top() + vec2(8.0, 25.0), Align2::LEFT_TOP, n, theme::sans(11.0), theme::WARN);
        }
        (None, Some(l)) => {
            let bar = Rect::from_min_size(r.left_top() + vec2(8.0, 29.0), vec2(w - 64.0, 5.0));
            painter.rect_filled(bar, CornerRadius::same(1), theme::surface_4());
            let f = crate::meter::frac(l);
            painter.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * f, bar.height())), CornerRadius::same(1), theme::VIOLET);
            painter.text(pos2(r.right() - 8.0, bar.center().y), Align2::RIGHT_CENTER, crate::meter::s_label(Some(l)), theme::mono_bold(10.5), theme::text());
        }
        _ => {}
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Modifiers, PointerButton, TouchPhase};
    use egui_kittest::Harness;

    struct T {
        pan: Panafall,
        view: View,
        status: Status,
        frame: FftFrame,
        acts: Vec<Act>,
        ready: bool,
    }

    /// A 2 MHz span at 146 MHz, NFM, flat noise with one carrier.
    fn harness() -> Harness<'static, T> {
        let mut pwr = vec![-100.0f32; 1024];
        pwr[700] = -40.0;
        let frame = FftFrame { center_hz: 146e6, rate_hz: 2e6, inspect_hz: 146.2e6, pwr, ..Default::default() };
        let status = Status {
            freq_hz: 146e6,
            rate_hz: 2e6,
            inspect_hz: 146.2e6,
            bandwidth_hz: 12_500.0,
            zoom: 1.0,
            mode: usdr_proto::Mode::Nfm,
            ..Default::default()
        };
        let mut pan = Panafall::default();
        pan.push(&frame, &View::default(), 0.0);
        let state = T { pan, view: View::default(), status, frame, acts: Vec::new(), ready: false };
        let mut h = Harness::builder().with_size(egui::vec2(1000.0, 600.0)).build_ui_state(
            |ui, t: &mut T| {
                // The named font families exist from the frame after this.
                if !t.ready {
                    crate::theme::install(ui.ctx(), crate::theme::Ground::Carbon);
                    t.ready = true;
                    return;
                }
                let flag = SliceFlag::default();
                t.pan.ui(ui, &t.status, Some(&t.frame), &mut t.view, &flag, 6_250.0, true, &mut t.acts);
            },
            state,
        );
        h.run_steps(2);
        h
    }

    fn pan_rect(h: &Harness<'static, T>) -> Rect {
        h.state().pan.regions.expect("drawn")[0]
    }

    fn x_of(r: Rect, hz: f64) -> f32 {
        r.left() + ((hz - 145e6) / 2e6) as f32 * r.width()
    }

    fn click(h: &mut Harness<'static, T>, p: Pos2, modifiers: Modifiers) {
        h.event(Event::PointerMoved(p));
        h.event_modifiers(Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: true, modifiers }, modifiers);
        h.event_modifiers(Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: false, modifiers }, modifiers);
        h.run_steps(2);
    }

    fn wheel(h: &mut Harness<'static, T>, p: Pos2, notches: f32, modifiers: Modifiers) {
        h.event(Event::PointerMoved(p));
        h.run_steps(1);
        h.event_modifiers(
            Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta: egui::vec2(0.0, notches), phase: TouchPhase::Move, modifiers },
            modifiers,
        );
        h.run_steps(1);
    }

    #[test]
    fn click_tunes_snapped_to_the_raster() {
        let mut h = harness();
        let r = pan_rect(&h);
        // A little above 145.5 MHz; the NFM raster puts it on 145.500.
        let p = pos2(x_of(r, 145.501e6), r.center().y);
        click(&mut h, p, Modifiers::NONE);
        assert_eq!(h.state().acts.first(), Some(&Act::Tune { hz: 145.5e6, lock: false }));
    }

    #[test]
    fn alt_click_tunes_off_the_raster() {
        let mut h = harness();
        let r = pan_rect(&h);
        click(&mut h, pos2(x_of(r, 145.5031e6), r.center().y), Modifiers::ALT);
        let Some(Act::Tune { hz, lock: false }) = h.state().acts.first().cloned() else { panic!("{:?}", h.state().acts) };
        assert!((hz - 145.5031e6).abs() < 2_500.0 && (hz / 6_250.0).fract() != 0.0);
    }

    #[test]
    fn shift_click_moves_the_span_only() {
        let mut h = harness();
        let r = pan_rect(&h);
        click(&mut h, pos2(x_of(r, 145.75e6), r.center().y), Modifiers::SHIFT);
        assert_eq!(h.state().acts.first(), Some(&Act::Center(145.75e6)));
    }

    #[test]
    fn wheel_tunes_by_the_step_and_recentres() {
        let mut h = harness();
        let r = pan_rect(&h);
        wheel(&mut h, r.center(), 2.0, Modifiers::NONE);
        assert_eq!(h.state().acts, vec![Act::Tune { hz: 146.2e6 + 12_500.0, lock: true }]);
    }

    #[test]
    fn ctrl_wheel_zooms() {
        let mut h = harness();
        let r = pan_rect(&h);
        wheel(&mut h, r.center(), 4.0, Modifiers::COMMAND);
        assert_eq!(h.state().acts, vec![Act::Zoom(2.0)]);
    }

    #[test]
    fn dragging_the_display_pans_the_span() {
        let mut h = harness();
        let r = pan_rect(&h);
        let a = r.center();
        h.event(Event::PointerMoved(a));
        h.drag_at(a);
        h.run_steps(1);
        // A tenth of the width to the right: the span moves down by a tenth.
        for i in 1..=10 {
            h.event(Event::PointerMoved(a + egui::vec2(r.width() * 0.01 * i as f32, 0.0)));
            h.run_steps(1);
        }
        h.drop_at(a + egui::vec2(r.width() * 0.1, 0.0));
        h.run_steps(2);
        let Some(Act::Center(c)) = h.state().acts.last().cloned() else { panic!("{:?}", h.state().acts) };
        assert!((c - (146e6 - 0.2e6)).abs() < 5_000.0, "centre {c}");
    }

    #[test]
    fn wheel_on_the_db_scale_takes_the_range_off_auto() {
        let mut h = harness();
        let db = h.state().pan.regions.unwrap()[3];
        assert!(h.state().view.auto_range);
        wheel(&mut h, db.center(), 1.0, Modifiers::NONE);
        assert!(!h.state().view.auto_range);
        assert!(h.state().acts.is_empty(), "the dB scale never tunes");
    }

    #[test]
    fn edge_drags_respect_the_mode() {
        let sym = usdr_proto::FilterRange { min_width_hz: 2_500.0, max_width_hz: 25_000.0, max_edge_hz: 12_500.0, symmetric: true };
        // Either edge of a symmetric filter sets a centered width.
        assert_eq!(drag_edges(EdgeDrag::High, &sym, -6_250.0, 6_250.0, 4_000.0), (-4_000.0, 4_000.0));
        assert_eq!(drag_edges(EdgeDrag::Low, &sym, -6_250.0, 6_250.0, -100.0), (-1_250.0, 1_250.0));
        let ssb = usdr_proto::FilterRange { min_width_hz: 200.0, max_width_hz: 4_000.0, max_edge_hz: 4_000.0, symmetric: false };
        // A sideband edge moves alone, never past the other less the minimum.
        assert_eq!(drag_edges(EdgeDrag::High, &ssb, 300.0, 2_800.0, 2_000.0), (300.0, 2_000.0));
        assert_eq!(drag_edges(EdgeDrag::High, &ssb, 300.0, 2_800.0, 350.0), (300.0, 500.0));
        // A shift keeps the width and stops at the limit.
        let shift = EdgeDrag::Shift { from_hz: 1_000.0, lo: 300.0, hi: 2_800.0 };
        assert_eq!(drag_edges(shift, &ssb, 300.0, 2_800.0, 1_500.0), (800.0, 3_300.0));
        assert_eq!(drag_edges(shift, &ssb, 300.0, 2_800.0, 9_000.0), (1_500.0, 4_000.0));
    }

    #[test]
    fn dragging_a_passband_edge_sets_the_filter() {
        let mut h = harness();
        let range = usdr_proto::FilterRange { min_width_hz: 2_500.0, max_width_hz: 25_000.0, max_edge_hz: 12_500.0, symmetric: true };
        h.state_mut().status.bandwidth_hz = 100_000.0;
        h.state_mut().pan.filter = Some((range, -50_000.0, 50_000.0));
        h.run_steps(2);
        let r = pan_rect(&h);
        let edge = pos2(x_of(r, 146.25e6), r.center().y);
        h.event(Event::PointerMoved(edge));
        h.drag_at(edge);
        h.run_steps(1);
        for i in 1..=8 {
            h.event(Event::PointerMoved(edge - egui::vec2(4.0 * i as f32, 0.0)));
            h.run_steps(1);
        }
        h.drop_at(edge - egui::vec2(32.0, 0.0));
        h.run_steps(1);
        let acts = &h.state().acts;
        assert!(matches!(acts.last(), Some(Act::Filter(lo, hi)) if *lo < 0.0 && (*lo + *hi).abs() < 1.0), "{acts:?}");
        assert!(!acts.iter().any(|a| matches!(a, Act::Center(_) | Act::Tune { .. })), "an edge drag neither pans nor tunes: {acts:?}");
    }

    #[test]
    fn waterfall_keeps_its_history_across_a_retune() {
        let mut pan = Panafall::default();
        let view = View::default();
        let mut pwr = vec![-100.0f32; 1024];
        pwr[512] = -30.0;
        let f = FftFrame { center_hz: 100e6, rate_hz: 1e6, pwr: pwr.clone(), ..Default::default() };
        pan.push(&f, &view, 0.0);
        // The span moves up a quarter: the carrier's old row slides left by
        // a quarter of the columns instead of the history being blanked.
        let g = FftFrame { center_hz: 100.25e6, ..f };
        pan.push(&g, &view, 0.04);
        let cols = pan.wf_cols;
        let older = (pan.wf_head + 1) % WF_ROWS;
        let row = &pan.wf_db[older * cols..(older + 1) * cols];
        let hot = row.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert!((hot as i64 - 256).abs() <= 2, "carrier now at column {hot}");
    }
}
