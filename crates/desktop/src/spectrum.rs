//! The panafall: panadapter over waterfall, one widget, one frequency axis.
//!
//! The panadapter draws the server's Welch-averaged trace with a little
//! visual smoothing (it rises at once and falls slowly, so a burst shows at
//! full level on its first frame and the floor stops flickering), a fill
//! whose heat follows level, a decaying peak hold, the server's max hold, the
//! passband with its slice flag, decode spots, spurs and scan marks.
//!
//! The waterfall paints what was measured: each frame's own row (not the
//! trace's smoothed or averaged one, when the server sends it), every screen
//! column the strongest bin under it, bins drawn as crisp columns when
//! zoomed in, and each row shown the moment it arrives. It keeps its history
//! in dB, so a palette or contrast change recolors all of it, not just the
//! rows that arrive afterwards.

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

/// A look for the panadapter: background, trace, fill, holds, and the
/// waterfall palette that goes with them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Style {
    /// The house look: an amber trace on the ground, ember waterfall.
    Ember,
    /// The classic panadapter: a white trace over a blue fill on deep navy,
    /// and the blue-to-red waterfall.
    Classic,
    /// A pale ice-blue trace over a night-blue pan, and the midnight
    /// waterfall: the contest-radio look.
    Midnight,
    /// Cyan on the ground, the glacier waterfall.
    Glacier,
    /// A green CRT phosphor.
    Phosphor,
    /// An orange trace with a gold core over a plum pan, and the inferno
    /// waterfall: the heat-map look.
    Inferno,
    /// A violet trace over an indigo pan, and the prism waterfall: the
    /// rainbow look.
    #[default]
    Prism,
}

pub struct Look {
    /// Background top and bottom; `None` takes the theme's surfaces.
    pub bg: Option<(Color32, Color32)>,
    pub fill: Color32,
    pub glow: Color32,
    pub core: Color32,
    pub peak: Color32,
    pub max_hold: Color32,
    pub palette: Colormap,
}

impl Style {
    pub const ALL: [Style; 7] = [Style::Ember, Style::Inferno, Style::Prism, Style::Midnight, Style::Classic, Style::Glacier, Style::Phosphor];

    pub fn label(self) -> &'static str {
        match self {
            Style::Ember => "Ember",
            Style::Classic => "Classic",
            Style::Midnight => "Midnight",
            Style::Glacier => "Glacier",
            Style::Phosphor => "Phosphor",
            Style::Inferno => "Inferno",
            Style::Prism => "Prism",
        }
    }

    pub fn look(self) -> Look {
        let rgb = |h: u32| Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8);
        match self {
            Style::Ember => Look { bg: None, fill: theme::AMBER, glow: theme::AMBER, core: rgb(0xffe4bd), peak: theme::CYAN, max_hold: theme::VIOLET, palette: Colormap::Ember },
            Style::Classic => Look {
                bg: Some((rgb(0x0d1a2e), rgb(0x02050b))),
                fill: rgb(0x2f7dff),
                glow: rgb(0x6fb6ff),
                core: rgb(0xf2f8ff),
                peak: rgb(0xffe14a),
                max_hold: rgb(0xff8ad8),
                palette: Colormap::Classic,
            },
            Style::Midnight => Look {
                bg: Some((rgb(0x0b1830), rgb(0x000104))),
                fill: rgb(0x4aa0ff),
                glow: rgb(0x9fd2ff),
                core: rgb(0xf4faff),
                peak: rgb(0xffe85a),
                max_hold: rgb(0xff7a3c),
                palette: Colormap::Midnight,
            },
            Style::Glacier => Look { bg: None, fill: theme::CYAN, glow: theme::CYAN, core: rgb(0xe6fbff), peak: theme::AMBER, max_hold: theme::VIOLET, palette: Colormap::Glacier },
            Style::Phosphor => Look {
                bg: Some((rgb(0x07120a), rgb(0x010402))),
                fill: rgb(0x39ff8a),
                glow: rgb(0x39ff8a),
                core: rgb(0xd2ffe2),
                peak: rgb(0xffe14a),
                max_hold: rgb(0x52e3ff),
                palette: Colormap::Green,
            },
            Style::Inferno => Look {
                bg: Some((rgb(0x1c0a26), rgb(0x06020a))),
                fill: rgb(0xf26b1d),
                glow: rgb(0xffa040),
                core: rgb(0xfff1cc),
                peak: rgb(0xf2a0ff),
                max_hold: rgb(0x8a6cff),
                palette: Colormap::Inferno,
            },
            Style::Prism => Look {
                bg: Some((rgb(0x0c0b2a), rgb(0x020208))),
                fill: rgb(0x8a5cff),
                glow: rgb(0xc39cff),
                core: rgb(0xfbf7ff),
                peak: rgb(0x7dffb0),
                max_hold: rgb(0xffb347),
                palette: Colormap::Prism,
            },
        }
    }
}

/// Display preferences the panafall reads each frame.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct View {
    pub style: Style,
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
    /// The allocation strip along the frequency axis.
    pub band_plan: bool,
}

impl Default for View {
    fn default() -> Self {
        Self {
            style: Style::Prism,
            palette: Colormap::Prism,
            peak_hold: true,
            fill: true,
            auto_range: true,
            floor_db: -110.0,
            ceiling_db: -20.0,
            wf_peak: false,
            wf_black: -3.0,
            wf_auto_black: true,
            wf_range: 65.0,
            split: 0.42,
            sql_mark: None,
            show_peaks: true,
            band_plan: true,
        }
    }
}

const WF_ROWS: usize = 768;
/// The replay history: five minutes of rows at the server's 25 fps, each
/// kept as 1024 columns of 0.75 dB steps from −160 dBFS (about 8 MB).
const ARCH_ROWS: usize = 7_500;
const ARCH_W: usize = 1024;
const ARCH_STEP_DB: f32 = 0.75;
const ARCH_FLOOR_DB: f32 = -160.0;

/// One waterfall row in the replay history, with where and when it was.
struct ArchRow {
    utc: std::time::SystemTime,
    lo_hz: f64,
    hi_hz: f64,
    q: Vec<u8>,
}

/// The waterfall held for replay: how many rows back from the newest the
/// top of the display is, and the picture of that stretch.
pub struct Hold {
    pub(crate) back: usize,
    shown: Option<HeldKey>,
    tex: Option<egui::TextureHandle>,
}

/// What a held picture shows: its top row (counted from the first row ever
/// archived, so it does not change as rows arrive), its size, the levels
/// and palette, and the frequency axis.
type HeldKey = (u64, [usize; 2], (f32, f32, Colormap), u64);
const WF_MAX_COLS: usize = 4096;
/// The waterfall history is kept as hundredths of a dB above −200 dBFS, so
/// one table lookup colors a cell; 0 is the "nothing yet" of an empty row.
const WF_Q_FLOOR_DB: f32 = -200.0;
const WF_Q_PER_DB: f32 = 100.0;

fn wf_quantize(db: f32) -> u16 {
    ((db - WF_Q_FLOOR_DB) * WF_Q_PER_DB).round().clamp(0.0, 65535.0) as u16
}

/// The bins under each of `out_cols` texture columns, as the trace is
/// decimated; empty when there are no more bins than columns.
fn col_bounds(n: usize, out_cols: usize) -> Vec<(usize, usize)> {
    if out_cols >= n {
        return Vec::new();
    }
    (0..out_cols)
        .map(|c| {
            let a = c * n / out_cols;
            (a, ((c + 1) * n / out_cols).max(a + 1).min(n))
        })
        .collect()
}

/// One history row as texture columns: as it is when `bounds` is empty,
/// else each column the strongest bin it covers, so the waterfall and the
/// trace agree on what is there.
fn color_row(q: &[u16], bounds: &[(usize, usize)], lut: &[Color32], out: &mut Vec<Color32>) {
    if bounds.is_empty() {
        out.extend(q.iter().map(|&v| lut[v as usize]));
        return;
    }
    out.extend(bounds.iter().map(|&(a, b)| lut[q[a..b].iter().copied().max().unwrap_or(0) as usize]));
}

/// A palette change in progress: the picture in the old palette, fading
/// out over the new one, with what it takes to keep its newest rows
/// current until it has gone.
struct Fade {
    tex: egui::TextureHandle,
    lut: Vec<Color32>,
    start: f64,
    /// Rows that arrived since the fade began, to color in the old palette.
    fresh: Vec<usize>,
}

pub struct Panafall {
    smooth: Vec<f32>,
    peak: Vec<f32>,
    max_hold: Vec<f32>,
    /// A slower-falling trace drawn faintly under the live one, the way a
    /// phosphor keeps what it just showed.
    after: Vec<f32>,
    /// Where the carrier is drawn: it glides to the receive frequency.
    marker_hz: f64,
    /// Where it was last asked to go, and when: a new frequency pulses the
    /// passband as the marker lands there.
    marker_target: f64,
    tuned_at: f64,
    fade: Option<Fade>,
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
    // Waterfall: history quantized (`wf_quantize`), newest at `head`, rows
    // going back in time.
    wf_cols: usize,
    wf_q: Vec<u16>,
    wf_time: Vec<f64>,
    /// How far the span has moved since the history began, in columns, and
    /// where it was when each row arrived: a retune moves the rows already
    /// drawn by the difference rather than shifting and recoloring them.
    wf_pan: f64,
    wf_row_pan: Vec<f64>,
    wf_head: usize,
    wf_count: usize,
    wf_tex: Option<egui::TextureHandle>,
    /// Columns in the texture: the history's, or the screen's when the
    /// history has more, each then the strongest bin under it.
    wf_tex_cols: usize,
    /// Rows whose texture row is out of date. Only rows on screen are
    /// colored, so a recolor costs what is shown, not the whole history.
    wf_stale: Vec<bool>,
    wf_levels: (f32, f32, Colormap),
    /// Every quantized level to its color at the current levels and palette.
    wf_lut: Vec<Color32>,
    wf_lut_key: Option<(f32, f32, Colormap, Color32)>,
    /// The auto black level in use; moves only when the floor has moved
    /// 1.5 dB from it, so a floor wandering across a rounding boundary does
    /// not recolor the whole history every frame.
    auto_black: f32,
    /// Level (0..1 of the range) to palette index, with the tone curve baked in.
    tone: Vec<u8>,
    row_interval: f64,
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
    archive: VecDeque<ArchRow>,
    /// Rows archived so far: the held picture's rows are counted from the
    /// first, so its key holds still while new rows arrive.
    archived: u64,
    pub hold: Option<Hold>,
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
            after: Vec::new(),
            marker_hz: 0.0,
            marker_target: 0.0,
            tuned_at: f64::NEG_INFINITY,
            fade: None,
            center_hz: 0.0,
            span_hz: 0.0,
            min_db: -110.0,
            max_db: -20.0,
            target_min: -110.0,
            target_max: -20.0,
            floor_est: -100.0,
            wf_shift_db: 0.0,
            wf_cols: 0,
            wf_q: Vec::new(),
            wf_time: Vec::new(),
            wf_pan: 0.0,
            wf_row_pan: Vec::new(),
            wf_head: 0,
            wf_count: 0,
            wf_tex: None,
            wf_tex_cols: 0,
            wf_stale: Vec::new(),
            wf_levels: (0.0, 0.0, Colormap::Ember),
            wf_lut: Vec::new(),
            wf_lut_key: None,
            auto_black: f32::NAN,
            tone: (0..1024).map(|i| ((i as f32 / 1023.0).powf(0.86) * 255.0).round() as u8).collect(),
            row_interval: 0.04,
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
            archive: VecDeque::new(),
            archived: 0,
            hold: None,
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
pub(crate) fn nice_step(range: f64, target_lines: f64) -> f64 {
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
            // A new span or a new width: carry the old trace over, resampled
            // onto the new axis, so the display glides into the new span
            // rather than blinking to a fresh one.
            if self.span_hz > 0.0 && self.smooth.len() >= 2 {
                let old = (self.center_hz, self.span_hz);
                let new = (f.center_hz, f.rate_hz);
                self.smooth = resample(&self.smooth, old, n, new);
                self.peak = resample(&self.peak, old, n, new);
                self.after = resample(&self.after, old, n, new);
            } else {
                self.smooth = f.pwr.clone();
                self.peak = f.pwr.clone();
                self.after = f.pwr.clone();
            }
        } else if axis_moved {
            // Same span, new center: carry the holds and history across by
            // the bins they moved, so a retune slides rather than blanks.
            let shift = ((f.center_hz - self.center_hz) / f.rate_hz * (n - 1) as f64).round() as isize;
            shift_vec(&mut self.smooth, shift, -200.0);
            shift_vec(&mut self.peak, shift, -200.0);
            shift_vec(&mut self.after, shift, -200.0);
            self.wf_pan += (f.center_hz - self.center_hz) / f.rate_hz * self.wf_cols as f64;
        }
        self.center_hz = f.center_hz;
        self.span_hz = f.rate_hz;
        for (s, &v) in self.smooth.iter_mut().zip(&f.pwr) {
            if *s < -190.0 {
                *s = v;
            }
            // Up at once, so nothing waits a frame to show its level; down
            // gently, so the floor does not flicker.
            if v > *s {
                *s = v;
            } else {
                *s += (v - *s) * 0.22;
            }
        }
        for (p, &s) in self.peak.iter_mut().zip(&self.smooth) {
            *p = s.max(*p - 0.25);
        }
        if self.after.len() != n {
            self.after = self.smooth.clone();
        }
        for (a, &s) in self.after.iter_mut().zip(&self.smooth) {
            if *a < -190.0 {
                *a = s;
            }
            *a += (s - *a) * if s > *a { 0.5 } else { 0.06 };
        }
        self.max_hold.clone_from(&f.max_hold);

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

        // Waterfall row: the frame's own measurement, before the bin
        // smoothing and AVG the trace gets (an older server sends only the
        // trace's). From the burst-peak array when WF PEAK is on: it runs
        // hotter than the mean, so its median excess is taken off and the
        // floor stays where the mean put it.
        let base: &[f32] = if f.wf_row.len() == n { &f.wf_row } else { &f.pwr };
        let row_src: &[f32] = if view.wf_peak && f.wf_peak.len() == n {
            let mut diffs: Vec<f32> = (0..n)
                .step_by((n / 128).max(1))
                .map(|i| f.wf_peak[i] - base[i])
                .collect();
            diffs.sort_by(|a, b| a.total_cmp(b));
            let med = percentile(&diffs, 0.5);
            self.wf_shift_db += (med - self.wf_shift_db) * 0.1;
            &f.wf_peak
        } else {
            self.wf_shift_db = 0.0;
            base
        };
        let cols = n.min(WF_MAX_COLS);
        if cols != self.wf_cols {
            self.wf_cols = cols;
            self.wf_q = vec![0; cols * WF_ROWS];
            self.wf_time = vec![0.0; WF_ROWS];
            self.wf_pan = 0.0;
            self.wf_row_pan = vec![0.0; WF_ROWS];
            self.wf_stale = vec![true; WF_ROWS];
            self.wf_count = 0;
            self.wf_tex = None;
            self.fade = None;
        }
        self.wf_head = (self.wf_head + WF_ROWS - 1) % WF_ROWS;
        let row = &mut self.wf_q[self.wf_head * cols..(self.wf_head + 1) * cols];
        if cols == n {
            for (out, &v) in row.iter_mut().zip(row_src) {
                *out = wf_quantize(v - self.wf_shift_db);
            }
        } else {
            for (c, out) in row.iter_mut().enumerate() {
                // Each column keeps the strongest bin it covers.
                let a = c * n / cols;
                let b = ((c + 1) * n / cols).max(a + 1);
                *out = wf_quantize(row_src[a..b].iter().copied().fold(f32::MIN, f32::max) - self.wf_shift_db);
            }
        }
        self.wf_time[self.wf_head] = now;
        self.wf_row_pan[self.wf_head] = self.wf_pan;
        self.wf_stale[self.wf_head] = true;
        if let Some(f) = &mut self.fade {
            f.fresh.push(self.wf_head);
        }
        // The replay history: strongest bin per column, quantized, in the
        // buffer of the row about to fall off the end.
        let mut q = match self.archive.len() >= ARCH_ROWS {
            true => self.archive.pop_back().map(|r| r.q).unwrap_or_default(),
            false => Vec::with_capacity(ARCH_W),
        };
        q.clear();
        q.extend((0..ARCH_W).map(|c| {
            let a = c * n / ARCH_W;
            let b = ((c + 1) * n / ARCH_W).max(a + 1).min(n);
            let v = row_src[a.min(n - 1)..b].iter().copied().fold(f32::MIN, f32::max) - self.wf_shift_db;
            ((v - ARCH_FLOOR_DB) / ARCH_STEP_DB).clamp(0.0, 255.0) as u8
        }));
        self.archive.push_front(ArchRow {
            utc: std::time::SystemTime::now(),
            lo_hz: f.center_hz - f.rate_hz / 2.0,
            hi_hz: f.center_hz + f.rate_hz / 2.0,
            q,
        });
        self.archive.truncate(ARCH_ROWS);
        self.archived += 1;
        // Held, the picture stays on the rows it shows while new ones arrive
        // above it: it is a row further back from the newest each time.
        if let Some(h) = &mut self.hold {
            h.back += 1;
        }
        self.wf_count = (self.wf_count + 1).min(WF_ROWS);
        let prev = self.wf_time[(self.wf_head + 1) % WF_ROWS];
        if self.wf_count > 1 && prev > 0.0 {
            let dt = (now - prev).clamp(0.005, 0.5);
            self.row_interval += (dt - self.row_interval) * 0.05;
        }
        self.last_frame = Some(Instant::now());
    }

    pub fn toggle_hold(&mut self) {
        // The live texture is not kept current while held.
        self.fade = None;
        self.hold = match self.hold {
            Some(_) => None,
            None => Some(Hold { back: 0, shown: None, tex: None }),
        };
    }

    /// How far back the held waterfall is, in seconds.
    pub fn held_age(&self) -> Option<f64> {
        let h = self.hold.as_ref()?;
        let newest = self.archive.front()?.utc;
        let row = self.archive.get(h.back)?.utc;
        Some(newest.duration_since(row).map(|d| d.as_secs_f64()).unwrap_or(0.0))
    }

    /// The held waterfall: the archived rows from `back` downward, mapped to
    /// the display's current frequency axis, with UTC times beside them.
    fn draw_held(&mut self, ui: &mut Ui, wf: Rect, time_axis: Rect, view: &View, px_w: usize) {
        let rows_vis = (wf.height().round() as usize).clamp(1, 2048);
        // As live: no more columns than pixels, each the strongest it covers.
        let cols = ARCH_W.min(px_w.max(16));
        let (black, top) = self.wf_levels(view);
        let lut = view.palette.lut();
        let lo = self.center_hz - self.span_hz / 2.0;
        let span = self.span_hz.max(1.0);
        let max_back = self.archive.len().saturating_sub(rows_vis.min(self.archive.len()));
        let Some(h) = self.hold.as_mut() else { return };
        h.back = h.back.min(max_back);
        let key: HeldKey = (
            self.archived - h.back as u64,
            [cols, rows_vis],
            (black, top, view.palette),
            (lo as u64) ^ (span as u64).rotate_left(32),
        );
        if h.shown != Some(key) || h.tex.is_none() {
            let scale = 1023.0 / (top - black).max(1.0);
            let ink = theme::ink();
            // Every archived level to its color, once for the whole picture.
            let colors: [Color32; 256] = std::array::from_fn(|q| {
                let v = q as f32 * ARCH_STEP_DB + ARCH_FLOOR_DB;
                let k = ((v - black) * scale).clamp(0.0, 1023.0) as usize;
                lut[self.tone[k] as usize]
            });
            // Display column to the archived columns it covers, `None`
            // where the row did not reach; rows of one span share it.
            let mut map: Vec<Option<(usize, usize)>> = Vec::with_capacity(cols);
            let mut map_for = (f64::NAN, f64::NAN);
            let mut px = Vec::with_capacity(cols * rows_vis);
            for r in 0..rows_vis {
                match self.archive.get(h.back + r) {
                    Some(row) => {
                        if map_for != (row.lo_hz, row.hi_hz) {
                            map_for = (row.lo_hz, row.hi_hz);
                            let w = (row.hi_hz - row.lo_hz).max(1.0);
                            let at = |hz: f64| (hz - row.lo_hz) / w * ARCH_W as f64;
                            map.clear();
                            map.extend((0..cols).map(|c| {
                                let a = at(lo + c as f64 / cols as f64 * span);
                                let b = at(lo + (c + 1) as f64 / cols as f64 * span);
                                if b <= 0.0 || a >= ARCH_W as f64 {
                                    return None;
                                }
                                let a = a.max(0.0) as usize;
                                Some((a, (b.ceil() as usize).clamp(a + 1, ARCH_W)))
                            }));
                        }
                        px.extend(map.iter().map(|m| match m {
                            Some((a, b)) => colors[row.q[*a..*b].iter().copied().max().unwrap_or(0) as usize],
                            None => ink,
                        }));
                    }
                    None => px.extend(std::iter::repeat_n(ink, cols)),
                }
            }
            let img = egui::ColorImage::new([cols, rows_vis], px);
            let opts = egui::TextureOptions::NEAREST;
            match &mut h.tex {
                Some(t) => t.set(img, opts),
                None => h.tex = Some(ui.ctx().load_texture("waterfall-held", img, opts)),
            }
            h.shown = Some(key);
        }
        let painter = ui.painter_at(wf);
        if let Some(t) = &h.tex {
            painter.image(t.id(), wf, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        }
        // Wall-clock times on the held rows, about every 60 px.
        let p = ui.painter();
        p.rect_filled(time_axis, CornerRadius::ZERO, theme::surface_1());
        let row_px = wf.height() / rows_vis as f32;
        let every = (60.0 / row_px).ceil() as usize;
        for i in (0..rows_vis).step_by(every.max(1)) {
            let Some(row) = self.archive.get(h.back + i) else { break };
            let y = wf.top() + i as f32 * row_px;
            let secs = row.utc.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            p.line_segment([pos2(time_axis.left(), y), pos2(time_axis.left() + 4.0, y)], Stroke::new(1.0, theme::text_3()));
            p.text(
                pos2(time_axis.right() - 7.0, y + 1.0),
                Align2::RIGHT_TOP,
                format!("{:02}:{:02}", (secs / 60) % 60, secs % 60),
                theme::mono(10.0),
                theme::text_2(),
            );
        }
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

    /// The table that colors every quantized level at these levels, in
    /// this palette: below −190 dBFS is "nothing yet", drawn as the ground.
    fn build_lut(&self, black: f32, top: f32, palette: &[Color32; 256], ink: Color32) -> Vec<Color32> {
        let scale = 1023.0 / (top - black).max(1.0);
        (0..=u16::MAX)
            .map(|q| {
                let v = q as f32 / WF_Q_PER_DB + WF_Q_FLOOR_DB;
                if v < -190.0 {
                    return ink;
                }
                // The tone table lifts weak signals off the floor.
                let k = ((v - black) * scale).clamp(0.0, 1023.0) as usize;
                palette[self.tone[k] as usize]
            })
            .collect()
    }

    /// Bring the waterfall texture up to date for the `rows_vis` rows on
    /// screen, `px_w` pixels wide: new rows, and every row after a palette,
    /// level, width or retune.
    fn upload(&mut self, ctx: &egui::Context, view: &View, now: f64, reduce_motion: bool, rows_vis: usize, px_w: usize) {
        if self.wf_cols == 0 {
            return;
        }
        // Never more columns than pixels: the GPU would sample between
        // them and a narrow carrier could fall through. Fewer bins than
        // pixels stay as they are and are drawn as crisp columns.
        let tex_cols = self.wf_cols.min(px_w.max(16));
        if tex_cols != self.wf_tex_cols {
            self.wf_tex_cols = tex_cols;
            self.wf_tex = None;
            self.fade = None;
            self.wf_stale.fill(true);
        }
        let (black, top) = self.wf_levels(view);
        let want = (black, top, view.palette);
        if want != self.wf_levels {
            // A new palette crossfades over the whole history, a quarter of
            // a second, rather than snapping: the picture in the old one
            // fades out over the new. Levels and ranges snap as before,
            // since those are readings.
            if self.wf_levels.2 != view.palette && !reduce_motion && self.wf_tex.is_some() {
                let old = self.wf_tex.take().expect("checked");
                match &mut self.fade {
                    // Changed again mid-fade: the first palette keeps fading.
                    Some(f) => f.start = now,
                    None => {
                        self.fade = Some(Fade { tex: old, lut: std::mem::take(&mut self.wf_lut), start: now, fresh: Vec::new() });
                        self.wf_lut_key = None;
                    }
                }
            }
            self.wf_levels = want;
            self.wf_stale.fill(true);
        }
        if self.fade.as_ref().is_some_and(|f| reduce_motion || now - f.start >= 0.25) {
            self.fade = None;
        }
        let ink = theme::ink();
        let key = (black, top, view.palette, ink);
        if self.wf_lut_key != Some(key) {
            self.wf_lut = self.build_lut(black, top, view.palette.lut(), ink);
            self.wf_lut_key = Some(key);
            self.wf_stale.fill(true);
        }
        let opts = egui::TextureOptions::NEAREST;
        let cols = self.wf_cols;
        let bounds = col_bounds(cols, tex_cols);
        let color = |rows: &[u16], lut: &[Color32], out: &mut Vec<Color32>| {
            for q in rows.chunks_exact(cols) {
                color_row(q, &bounds, lut, out);
            }
        };
        if self.wf_tex.is_none() {
            // A new texture: the rows on screen colored, the rest left as
            // ground until they are shown.
            let mut px = vec![ink; tex_cols * WF_ROWS];
            let mut row = Vec::with_capacity(tex_cols);
            for i in 0..(rows_vis + 1).min(WF_ROWS) {
                let r = (self.wf_head + i) % WF_ROWS;
                row.clear();
                color_row(&self.wf_q[r * cols..(r + 1) * cols], &bounds, &self.wf_lut, &mut row);
                px[r * tex_cols..(r + 1) * tex_cols].copy_from_slice(&row);
                self.wf_stale[r] = false;
            }
            self.wf_tex = Some(ctx.load_texture("waterfall", egui::ColorImage::new([tex_cols, WF_ROWS], px), opts));
        }
        // Stale rows on screen, newest first, sent as runs of adjacent rows.
        let tex = self.wf_tex.as_mut().expect("made above");
        let shown = (rows_vis + 1).min(WF_ROWS);
        let mut i = 0;
        while i < shown {
            let r0 = (self.wf_head + i) % WF_ROWS;
            if !self.wf_stale[r0] {
                i += 1;
                continue;
            }
            let mut r1 = r0;
            while i < shown && r1 < WF_ROWS && self.wf_stale[r1] && (self.wf_head + i) % WF_ROWS == r1 {
                self.wf_stale[r1] = false;
                r1 += 1;
                i += 1;
            }
            let mut px = Vec::with_capacity(tex_cols * (r1 - r0));
            color(&self.wf_q[r0 * cols..r1 * cols], &self.wf_lut, &mut px);
            tex.set_partial([0, r0], egui::ColorImage::new([tex_cols, r1 - r0], px), opts);
        }
        if let Some(f) = &mut self.fade {
            for r in f.fresh.drain(..) {
                let mut px = Vec::with_capacity(tex_cols);
                color(&self.wf_q[r * cols..(r + 1) * cols], &f.lut, &mut px);
                f.tex.set_partial([0, r], egui::ColorImage::new([tex_cols, 1], px), opts);
            }
            crate::anim::more(ctx);
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

        // The frequency axis, with the band plan as a named strip above the
        // labels when it is on.
        let strip_h = if view.band_plan { 13.0 } else { 0.0 };
        let axis_h = 22.0 + strip_h;
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
        let look = view.style.look();
        let (bg_top, bg_bottom) = look.bg.unwrap_or((theme::surface_2(), theme::ink()));
        let bg = Mesh {
            indices: vec![0, 1, 2, 2, 1, 3],
            vertices: vec![
                crate::brand::vtx(pan.left_top(), bg_top),
                crate::brand::vtx(pan.right_top(), bg_top),
                crate::brand::vtx(pan.left_bottom(), bg_bottom),
                crate::brand::vtx(pan.right_bottom(), bg_bottom),
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
            // Clear of the unit caption at the top of the scale, and whole
            // above its foot, where the band plan's strip begins.
            if y < db_axis.top() + 20.0 || y > db_axis.bottom() - 7.0 {
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
            theme::mono(10.0),
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
        // The labels sit in the lower 22 px; the strip, when on, above them.
        let labels_top = axis.bottom() - 22.0;
        while f <= hi {
            let x = self.x_for(f, pan);
            painter.line_segment([pos2(x, pan.top()), pos2(x, pan.bottom())], Stroke::new(1.0, theme::line()));
            painter.line_segment([pos2(x, labels_top), pos2(x, labels_top + 4.0)], Stroke::new(1.0, theme::text_3()));
            if x > axis.left() + 30.0 && x < pan.right() - 30.0 {
                painter.text(
                    pos2(x, labels_top + 12.0),
                    Align2::CENTER_CENTER,
                    crate::tuning::fmt_mhz(f, decimals),
                    theme::mono(10.5),
                    theme::text_2(),
                );
            }
            f += f_step;
        }

        // ---- the band plan: a tinted strip over the labels, each band
        // named where there is room, so a sweep across a band says what it
        // is without a hover.
        if view.band_plan {
            let lo = self.hz_for(pan.left(), pan);
            let hi = self.hz_for(pan.right(), pan);
            let strip = Rect::from_min_max(pos2(axis.left(), axis.top() + 1.0), pos2(pan.right(), labels_top - 1.0));
            painter.line_segment([pos2(strip.left(), labels_top - 0.5), pos2(strip.right(), labels_top - 0.5)], Stroke::new(1.0, theme::line()));
            for b in crate::bandplan::within(lo, hi) {
                let x0 = self.x_for(b.lo_hz, pan).max(pan.left());
                let x1 = self.x_for(b.hi_hz, pan).min(pan.right());
                if x1 <= x0 {
                    continue;
                }
                let c = b.kind.color();
                let r = Rect::from_min_max(pos2(x0, strip.top()), pos2(x1.max(x0 + 2.0), strip.bottom()));
                painter.rect_filled(r, CornerRadius::ZERO, alpha(c, 0.16));
                painter.rect_filled(Rect::from_min_size(r.min, vec2(r.width(), 2.0)), CornerRadius::ZERO, alpha(c, 0.85));
                let g = painter.layout_no_wrap(b.name.to_owned(), theme::mono(10.0), c);
                if g.size().x + 12.0 < r.width() {
                    painter.galley(pos2(r.left() + 6.0, r.center().y + 1.0 - g.size().y / 2.0), g, c);
                }
            }
        }

        // ---- waterfall
        let rows_vis = (wf.height().round() as usize).clamp(1, WF_ROWS);
        let px_w = (wf.width() * ctx.pixels_per_point()).round() as usize;
        if self.hold.is_some() {
            self.draw_held(ui, wf, time_axis, view, px_w);
        } else if let Some(tex) = {
            self.upload(&ctx, view, now, reduce_motion, rows_vis, px_w);
            &self.wf_tex
        } {
            let row_px = wf.height() / rows_vis as f32;
            // The newest row is drawn at the top as soon as it arrives: no
            // slide-in that would hold it back by a row interval, and no
            // sub-row offset that would blend it with its neighbor.
            let painter_wf = ui.painter_at(wf);
            let x0 = self.drag_offset_px;
            let top = wf.top();
            // Rows head, head+1, … wrap at WF_ROWS. Drawn in runs of rows
            // that arrived on the same span, each moved by how far the span
            // has panned since; mid-fade, the old palette's picture over the
            // new one.
            let fade = self.fade.as_ref().map(|f| {
                let t = ((now - f.start) / 0.25).clamp(0.0, 1.0) as f32;
                (f.tex.id(), 1.0 - (1.0 - t).powi(3))
            });
            let col_px = wf.width() as f64 / self.wf_cols.max(1) as f64;
            let shown = (rows_vis + 1).min(WF_ROWS);
            for (id, tint) in std::iter::once((tex.id(), Color32::WHITE))
                .chain(fade.map(|(id, e)| (id, Color32::from_white_alpha(((1.0 - e) * 255.0) as u8))))
            {
                let mut i = 0;
                while i < shown {
                    let start = (self.wf_head + i) % WF_ROWS;
                    let pan_at = self.wf_row_pan[start];
                    let mut run = 1;
                    while i + run < shown && start + run < WF_ROWS && self.wf_row_pan[start + run] == pan_at {
                        run += 1;
                    }
                    let moved = self.wf_pan - pan_at;
                    if moved.abs() < self.wf_cols as f64 {
                        let dx = x0 - (moved * col_px) as f32;
                        let y0 = top + i as f32 * row_px;
                        let rect = Rect::from_min_max(
                            pos2(wf.left() + dx, y0),
                            pos2(wf.right() + dx, y0 + run as f32 * row_px),
                        );
                        let uv = Rect::from_min_max(
                            pos2(0.0, start as f32 / WF_ROWS as f32),
                            pos2(1.0, (start + run) as f32 / WF_ROWS as f32),
                        );
                        painter_wf.image(id, rect, uv, tint);
                    }
                    i += run;
                }
            }
            // Time marks, measured from the rows themselves, spaced for
            // about one label per 70 px whatever the row rate.
            painter.rect_filled(time_axis, CornerRadius::ZERO, theme::surface_1());
            let newest = self.wf_time[self.wf_head];
            let visible_s = rows_vis as f64 * self.row_interval.max(0.005);
            let step_s = nice_step(visible_s, (wf.height() / 70.0).max(1.0) as f64).max(0.5);
            let mut next_mark = step_s;
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
                        pos2(time_axis.right() - 7.0, y),
                        Align2::RIGHT_CENTER,
                        fmt_age(next_mark, step_s, visible_s >= 60.0),
                        theme::mono(10.0),
                        theme::text_3(),
                    );
                    next_mark += step_s;
                }
            }
        }

        // ---- the palette's ramp down the gutter's edge, top level at the
        // top and black at the foot, with the levels on hover.
        {
            let (black, top) = self.wf_levels(view);
            let lut = view.palette.lut();
            let ramp = Rect::from_min_max(pos2(time_axis.right() - 4.0, wf.top() + 2.0), pos2(time_axis.right() - 2.0, wf.bottom() - 2.0));
            if ramp.height() > 8.0 {
                let mut mesh = Mesh::default();
                let steps = 48;
                for k in 0..=steps {
                    let f = k as f32 / steps as f32;
                    let c = lut[self.tone[((1.0 - f) * 1023.0) as usize] as usize];
                    let y = ramp.top() + f * ramp.height();
                    mesh.colored_vertex(pos2(ramp.left(), y), c);
                    mesh.colored_vertex(pos2(ramp.right(), y), c);
                    if k > 0 {
                        let i = mesh.vertices.len() as u32;
                        mesh.add_triangle(i - 4, i - 3, i - 2);
                        mesh.add_triangle(i - 3, i - 1, i - 2);
                    }
                }
                painter.add(Shape::mesh(mesh));
                painter.rect_stroke(ramp.expand(0.5), CornerRadius::same(1), Stroke::new(1.0, theme::line()), egui::StrokeKind::Outside);
                ui.interact(time_axis, resp.id.with("ramp"), Sense::hover())
                    .on_hover_text(format!("Waterfall colors: {black:.0} dBFS and below is the ground, {top:.0} dBFS and above the top of the palette"));
            }
        }

        // ---- replay controls on the waterfall: HOLD when live; when held,
        // how far back, and LIVE (ember: the live state) to return.
        {
            let held = self.held_age();
            let label = if held.is_some() { "LIVE" } else { "HOLD" };
            let g = painter.layout_no_wrap(label.into(), theme::mono_bold(10.5), Color32::PLACEHOLDER);
            let r = Rect::from_min_size(pos2(wf.right() - g.size().x - 22.0, wf.top() + 8.0), g.size() + vec2(14.0, 8.0));
            let b = ui.interact(r, resp.id.with("hold"), Sense::click());
            let hover = b.hovered();
            let (fill, stroke, text_c) = if held.is_some() {
                (theme::ACTION, theme::ACTION, Color32::WHITE)
            } else {
                (alpha(theme::surface_3(), if hover { 0.95 } else { 0.8 }), theme::line_2(), theme::text())
            };
            if let Some(age) = held {
                // Held: a timeline of the whole history beside LIVE, its
                // knob where the display is, to drag or click anywhere along.
                let rows_vis = (wf.height().round() as usize).clamp(1, 2048);
                let max_back = self.archive.len().saturating_sub(rows_vis.min(self.archive.len()));
                let span_s = match (self.archive.front(), self.archive.get(max_back)) {
                    (Some(a), Some(b)) => a.utc.duration_since(b.utc).map(|d| d.as_secs_f64()).unwrap_or(0.0),
                    _ => 0.0,
                };
                let mmss = |t: f64| format!("−{:.0}:{:04.1}", (t / 60.0).floor(), t - (t / 60.0).floor() * 60.0);
                let label = painter.layout_no_wrap(format!("HELD {}", mmss(age)), theme::mono_bold(10.5), theme::WARN);
                let ends = painter.layout_no_wrap(format!("−{:.0}:{:02.0}", (span_s / 60.0).floor(), (span_s % 60.0).floor()), theme::mono(10.0), theme::text_3());
                let track_w = 180.0f32.min(wf.width() * 0.3);
                let track = Rect::from_min_size(pos2(r.left() - 12.0 - track_w, r.center().y - 3.0), vec2(track_w, 6.0));
                let ends_x = track.left() - 8.0 - ends.size().x;
                let label_x = ends_x - 12.0 - label.size().x;
                let plate = Rect::from_min_max(pos2(label_x - 8.0, r.top() - 4.0), pos2(r.right() + 4.0, r.bottom() + 4.0));
                painter.rect_filled(plate, CornerRadius::same(5), alpha(theme::surface_1(), 0.92));
                painter.galley(pos2(label_x, r.center().y - label.size().y / 2.0), label, theme::WARN);
                // A hairline between where the display is and where the history starts.
                painter.line_segment([pos2(ends_x - 6.0, r.top()), pos2(ends_x - 6.0, r.bottom())], Stroke::new(1.0, theme::line_2()));
                painter.galley(pos2(ends_x, r.center().y - ends.size().y / 2.0), ends, theme::text_3());
                painter.rect_filled(track, CornerRadius::same(3), theme::surface_4());
                let back = self.hold.as_ref().map_or(0, |h| h.back.min(max_back));
                let knob_x = if max_back == 0 { track.right() } else { track.right() - back as f32 / max_back as f32 * track.width() };
                painter.rect_filled(Rect::from_min_max(pos2(knob_x, track.top()), track.right_bottom()), CornerRadius::same(3), alpha(theme::WARN, 0.35));
                painter.circle(pos2(knob_x, track.center().y), 6.0, theme::WARN, Stroke::new(1.5, theme::ink()));
                let t = ui.interact(track.expand2(vec2(6.0, 8.0)), resp.id.with("hold-track"), Sense::click_and_drag());
                if let Some(p) = t.interact_pointer_pos().filter(|_| t.dragged() || t.clicked()) {
                    let frac = ((track.right() - p.x) / track.width()).clamp(0.0, 1.0);
                    if let Some(h) = self.hold.as_mut() {
                        h.back = (frac * max_back as f32).round() as usize;
                    }
                }
                t.on_hover_text("Drag or click to move through the held history; the wheel steps a second a notch");
            }
            painter.rect(r, CornerRadius::same(3), fill, Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
            painter.galley(r.min + vec2(7.0, 4.0), g, text_c);
            if b.on_hover_text(if held.is_some() { "Back to the live waterfall (H)" } else { "Hold the waterfall to look back up to five minutes (H)" }).clicked() {
                self.toggle_hold();
            }
        }

        // ---- the trace, decimated to one point per pixel column (the
        // strongest bin under it, so a narrow carrier never vanishes).
        let n = self.smooth.len();
        let cols = (pan.width().round() as usize).clamp(2, 8192);
        // The bins under each column and where it is drawn, worked out once
        // for every trace.
        let bins: Vec<(usize, usize)> = (0..cols)
            .map(|c| {
                let a = c * n / cols;
                let b = ((c + 1) * n / cols).max(a + 1).min(n);
                (a.min(n.saturating_sub(1)), b)
            })
            .collect();
        let xs: Vec<f32> = (0..cols).map(|c| pan.left() + c as f32 / (cols - 1) as f32 * pan.width() + self.drag_offset_px).collect();
        let column = |v: &[f32], c: usize| -> f32 {
            let (a, b) = bins[c];
            v[a..b].iter().copied().fold(f32::MIN, f32::max)
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
            for (&x, v) in xs.iter().zip(&raw) {
                line.push(pos2(x, self.y_for(*v, pan)));
            }
            let pan_painter = ui.painter_at(pan);
            if view.fill {
                // Heat: the fill brightens with level and warms toward the
                // trace's core, so strong signals glow white-hot at their
                // crest and the floor stays a quiet wash.
                let hot = theme::lerp(look.fill, look.core, 0.45);
                let mut mesh = Mesh::default();
                for p in &line {
                    let i = mesh.vertices.len() as u32;
                    let h = ((pan.bottom() - p.y) / pan.height()).clamp(0.0, 1.0);
                    let top_c = theme::lerp(theme::alpha(look.fill, 0.08), theme::alpha(hot, 0.60), h.powf(1.35));
                    mesh.colored_vertex(*p, top_c);
                    mesh.colored_vertex(pos2(p.x, pan.bottom()), alpha(look.fill, 0.0));
                    if i >= 2 {
                        mesh.add_triangle(i - 2, i - 1, i);
                        mesh.add_triangle(i - 1, i + 1, i);
                    }
                }
                pan_painter.add(Shape::mesh(mesh));
            }
            // The afterglow: what the trace showed a moment ago, wide and
            // faint under it. Classic keeps its flat look.
            if view.style != Style::Classic && self.after.len() == n {
                let pts: Vec<Pos2> = xs.iter().enumerate().map(|(c, &x)| pos2(x, self.y_for(column(&self.after, c), pan))).collect();
                pan_painter.add(Shape::line(pts, Stroke::new(4.5, alpha(look.glow, 0.11))));
            }
            if self.max_hold.len() == n {
                let pts: Vec<Pos2> = xs.iter().enumerate().map(|(c, &x)| pos2(x, self.y_for(column(&self.max_hold, c), pan))).collect();
                pan_painter.add(Shape::line(pts, Stroke::new(1.0, alpha(look.max_hold, 0.35))));
            }
            if view.peak_hold && self.peak.len() == n {
                let pts: Vec<Pos2> = xs.iter().enumerate().map(|(c, &x)| pos2(x, self.y_for(column(&self.peak, c), pan))).collect();
                pan_painter.add(Shape::line(pts, Stroke::new(1.0, alpha(look.peak, 0.5))));
            }
            pan_painter.add(Shape::line(line.clone(), Stroke::new(3.2, alpha(look.glow, 0.22))));
            pan_painter.add(Shape::line(line.clone(), Stroke::new(1.25, look.core)));
        }

        // ---- spurs being notched
        for off in &status.spurs_hz {
            let x = self.x_for(status.freq_hz + off, pan);
            if x < pan.left() || x > pan.right() {
                continue;
            }
            dashed_v(&painter, x, pan.top(), pan.bottom(), alpha(theme::ERR, 0.8));
            painter.text(pos2(x + 3.0, pan.top() + 10.0), Align2::LEFT_CENTER, "NOTCH", theme::mono(10.0), theme::ERR);
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
        // A new frequency: the passband and carrier flare as the marker
        // lands and settle over 0.4 s, so a tune is seen to take.
        if (inspect - self.marker_target).abs() > 1.0 {
            if self.marker_target > 0.0 && !reduce_motion {
                self.tuned_at = now;
            }
            self.marker_target = inspect;
        }
        let pulse = {
            let t = ((now - self.tuned_at) / 0.4).clamp(0.0, 1.0) as f32;
            if t < 1.0 {
                crate::anim::more(&ctx);
            }
            (1.0 - t).powi(2)
        };
        // The carrier, passband and flag glide to a new frequency over
        // about 140 ms (the waterfall is panning under them); a jump past
        // most of the span, or reduced motion, snaps.
        if reduce_motion || self.marker_hz <= 0.0 || (self.marker_hz - inspect).abs() > self.span_hz * 0.75 {
            self.marker_hz = inspect;
        } else if (self.marker_hz - inspect).abs() > 1.0 {
            let k = 1.0 - (-(dt as f64) * 22.0).exp();
            self.marker_hz += (inspect - self.marker_hz) * k;
            if (self.marker_hz - inspect).abs() <= 1.0 {
                self.marker_hz = inspect;
            } else {
                crate::anim::more(&ctx);
            }
        }
        let shown_hz = self.marker_hz;
        let ix = self.x_for(shown_hz, pan);
        let bw = status.bandwidth_hz.max(100.0);
        let pc = shown_hz + status.passband_offset_hz;
        let px0 = self.x_for(pc - bw / 2.0, pan);
        let px1 = self.x_for(pc + bw / 2.0, pan);
        let wide = (px1 - px0) > pan.width() * 0.25;
        let on_screen = ix >= pan.left() - 2.0 && ix <= pan.right() + 2.0;
        if on_screen {
            let band = Rect::from_min_max(pos2(px0.max(pan.left()), pan.top()), pos2(px1.min(pan.right()), pan.bottom()));
            let band_a = if wide { 0.05 + 0.10 * pulse } else { 0.10 + 0.22 * pulse };
            painter.rect_filled(band, CornerRadius::ZERO, alpha(theme::AMBER, band_a));
            let wf_band = Rect::from_min_max(pos2(px0.max(wf.left()), wf.top()), pos2(px1.min(wf.right()), wf.bottom()));
            painter.rect_filled(wf_band, CornerRadius::ZERO, alpha(theme::AMBER, 0.035 + 0.08 * pulse));
            if px1 - px0 > 6.0 {
                for x in [px0, px1] {
                    painter.line_segment([pos2(x, pan.top()), pos2(x, pan.bottom())], Stroke::new(1.0, alpha(theme::AMBER, 0.45 + 0.4 * pulse)));
                    dashed_v(&painter, x, wf.top(), wf.bottom(), alpha(theme::AMBER, 0.22));
                }
            }
            // The carrier: a soft glow under a crisp line, both swelling
            // for the moment of a tune.
            painter.line_segment([pos2(ix, pan.top()), pos2(ix, pan.bottom())], Stroke::new(5.0 + 6.0 * pulse, alpha(theme::AMBER, 0.14 + 0.18 * pulse)));
            painter.line_segment([pos2(ix, pan.top()), pos2(ix, pan.bottom())], Stroke::new(1.4 + 0.8 * pulse, theme::AMBER));
            painter.add(Shape::convex_polygon(
                vec![pos2(ix - 6.0, labels_top), pos2(ix + 6.0, labels_top), pos2(ix, labels_top + 7.0)],
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
            let g = painter.layout_no_wrap(format!("MARK {sql:.0} dB"), theme::mono(10.0), theme::ERR);
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
                    vec![pos2(bx - 6.0, labels_top), pos2(bx + 6.0, labels_top), pos2(bx, labels_top + 7.0)],
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
        // Its grip: a pill on the seam, so the seam says it moves.
        {
            let hot = ctx.animate_bool_with_time(resp.id.with("split-hot"), split_resp.hovered() || split_resp.dragged(), 0.12);
            let grip = Rect::from_center_size(pos2(pan.center().x, axis.top()), vec2(40.0 + 8.0 * hot, 4.0));
            painter.rect(grip, CornerRadius::same(2), theme::lerp(theme::surface_4(), theme::AMBER, hot), Stroke::new(1.0, theme::ink()), egui::StrokeKind::Outside);
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
            let mut text = match level {
                Some(l) if pan.contains(p) => format!("{} MHz · {l:.1} dBFS", crate::tuning::fmt_mhz(landing, 4)),
                _ => format!("{} MHz", crate::tuning::fmt_mhz(landing, 4)),
            };
            if view.band_plan
                && let Some(b) = crate::bandplan::within(landing, landing + 1.0).next()
            {
                // The kind only where the name does not already say it.
                let kind = b.kind.name();
                if b.name.to_lowercase().contains(&kind.to_lowercase()) {
                    text.push_str(&format!("\n{}", b.name));
                } else {
                    text.push_str(&format!("\n{} · {kind}", b.name));
                }
            }
            // A dot on the trace where the hover line crosses it, and its
            // level carried across to the dB scale.
            if let Some(l) = level
                && pan.contains(p)
            {
                let y = self.y_for(l, pan);
                painter.line_segment([pos2(lx, y), pos2(db_axis.left(), y)], Stroke::new(1.0, Color32::from_white_alpha(40)));
                painter.circle(pos2(lx, y), 3.0, look.core, Stroke::new(1.0, theme::ink()));
                let g = painter.layout_no_wrap(format!("{l:.0}"), theme::mono_bold(10.0), theme::text());
                let plate = Rect::from_center_size(pos2(db_axis.right() - 6.0 - g.size().x / 2.0, y), g.size() + vec2(8.0, 4.0));
                painter.rect(plate, CornerRadius::same(3), alpha(theme::surface_3(), 0.96), Stroke::new(1.0, theme::line_2()), egui::StrokeKind::Inside);
                painter.galley(plate.min + vec2(4.0, 2.0), g, theme::text());
            }
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
            if scroll != 0.0 && self.hold.is_some() && wf.contains(p) {
                // Held: the wheel walks back through time, a second a notch.
                if let Some(h) = self.hold.as_mut() {
                    let d = (scroll.round() as i64) * 25;
                    h.back = (h.back as i64 + d).max(0) as usize;
                }
            } else if scroll != 0.0 && !db_resp.hovered() {
                self.scroll_accum += scroll;
                let notches = self.scroll_accum.trunc();
                if notches != 0.0 {
                    self.scroll_accum -= notches;
                    if ctrl {
                        // Zoom into what is under the cursor: the zoomed
                        // window follows the receiver, so zooming in from
                        // more than a channel away tunes there first.
                        let z = (status.zoom.max(1.0) * 2f64.powf(0.25 * notches as f64)).clamp(1.0, 32.0);
                        let base = self.rx_b.as_ref().filter(|b| b.active).map_or(inspect, |b| b.freq_hz);
                        if notches > 0.0 && (landing - base).abs() > status.bandwidth_hz.max(1_000.0) {
                            acts.push(Act::Tune { hz: landing, lock: false });
                        }
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
            let hot = pointer.filter(|_| self.edge_drag.is_none()).and_then(near);
            if let Some(kind) = hot {
                ctx.set_cursor_icon(if matches!(kind, EdgeDrag::Shift { .. }) { egui::CursorIcon::Move } else { egui::CursorIcon::ResizeColumn });
            }
            // Grips on the edges, so they say they can be dragged before
            // the pointer finds them; brighter under the pointer or in hand.
            if px1 - px0 > 16.0 {
                let held = |e: EdgeDrag| match (self.edge_drag, e) {
                    (Some(EdgeDrag::Low), EdgeDrag::Low) | (Some(EdgeDrag::High), EdgeDrag::High) => true,
                    (Some(_), _) => false,
                    (None, EdgeDrag::Low) => matches!(hot, Some(EdgeDrag::Low)),
                    (None, _) => matches!(hot, Some(EdgeDrag::High)),
                };
                for (x, e, key) in [(px0, EdgeDrag::Low, "grip-lo"), (px1, EdgeDrag::High, "grip-hi")] {
                    let t = ctx.animate_bool_with_time(resp.id.with(key), held(e), 0.12);
                    let g = Rect::from_center_size(pos2(x, pan.center().y), vec2(4.0, 22.0 + 6.0 * t));
                    if pan.contains_rect(g) && !self.flags[0].is_some_and(|f| f.intersects(g.expand(2.0))) {
                        painter.rect(g, CornerRadius::same(2), alpha(theme::AMBER, 0.55 + 0.45 * t), Stroke::new(1.0, alpha(theme::ink(), 0.8)), egui::StrokeKind::Outside);
                    }
                }
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
            ui.menu_button("Style", |ui| {
                ui.set_min_width(200.0);
                for st in Style::ALL {
                    let look = st.look();
                    if crate::widgets::swatch_item(ui, st.label(), look.palette, Some(look.fill), view.style == st).clicked() {
                        view.style = st;
                        view.palette = look.palette;
                        ui.close();
                    }
                }
            });
            ui.menu_button("Palette", |ui| {
                ui.set_min_width(200.0);
                for m in Colormap::ALL {
                    if crate::widgets::swatch_item(ui, m.label(), m, None, view.palette == m).clicked() {
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

fn shift_vec<T: Copy>(v: &mut [T], shift: isize, fill: T) {
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
    /// A broadcast station's RDS, once its PI is confirmed.
    pub rds: Option<FlagRds>,
}

/// What the flag says of a broadcast station from its RDS.
#[derive(Default, Clone)]
pub struct FlagRds {
    /// Its name, or its call letters when its PS scrolls.
    pub station: String,
    /// Its program type, when sent.
    pub kind: Option<String>,
    /// What is on; a text still arriving carries `RT_GAP`s and its share in.
    pub now: Option<(String, Option<f32>)>,
    /// Block sync held: text from a station whose lock has gone is dimmed.
    pub locked: bool,
}

impl SliceFlag {
    fn paint(&self, painter: &egui::Painter, pan: Rect, x: f32, trace_y: f32, active: bool) -> Rect {
        if let Some(rds) = &self.rds {
            return self.paint_rds(painter, pan, x, trace_y, active, rds);
        }
        let bottom = match (&self.protocol, self.snr_db) {
            (Some(p), Some(s)) => format!("{p} · SNR {s:.1} dB"),
            (Some(p), None) => p.clone(),
            (None, Some(s)) => format!("SNR {s:.1} dB"),
            _ => String::new(),
        };
        // One width whatever it says or whether it is recording (the line is
        // cut to it), so the card's edges, and its left edge when it sits
        // left of the carrier, hold still as the protocol and SNR change.
        let rec_w = if self.recording { 16.0 } else { 0.0 };
        let w = 248.0;
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
        receiver_tab(painter, r, "A", theme::AMBER, active);
        let anchor_x = if right_side { r.left() } else { r.right() };
        painter.line_segment([pos2(x, r.top() + 14.0), pos2(anchor_x, r.top() + 14.0)], Stroke::new(1.0, alpha(theme::AMBER, 0.5)));
        painter.text(r.left_top() + vec2(27.0, 7.0), Align2::LEFT_TOP, crate::tuning::fmt_mhz(self.freq_hz, 4), theme::mono_bold(14.0), theme::text());
        let chip = format!("{} · {}", self.mode.label(), crate::tuning::fmt_short_hz(self.bandwidth_hz));
        painter.text(r.right_top() + vec2(-8.0, 9.0), Align2::RIGHT_TOP, chip, theme::mono(10.0), theme::AMBER);
        // Mini S-meter.
        let bar = Rect::from_min_size(r.left_top() + vec2(8.0, 30.0), vec2(w - 64.0, 6.0));
        mini_meter(painter, bar, self.meter, theme::OK);
        painter.text(pos2(r.right() - 8.0, bar.center().y), Align2::RIGHT_CENTER, &self.s_label, theme::mono_bold(11.0), theme::text());
        // The bottom line is cut to the card, clear of the recording dot.
        let bottom = crate::widgets::ellipsize(painter, &bottom, theme::sans(11.0), w - 16.0 - rec_w);
        painter.text(r.left_top() + vec2(8.0, 44.0), Align2::LEFT_TOP, bottom, theme::sans(11.0), theme::text_2());
        if self.recording {
            painter.circle_filled(pos2(r.right() - 12.0, r.bottom() - 10.0), 4.0, theme::EMBER);
        }
        r
    }
}

impl SliceFlag {
    /// The flag for a broadcast station with RDS: a line for who it is and
    /// its program type with the SNR beside them, and a line for what is on,
    /// a text still arriving shown with its gaps and a bar of how much is in.
    fn paint_rds(&self, painter: &egui::Painter, pan: Rect, x: f32, trace_y: f32, active: bool, rds: &FlagRds) -> Rect {
        let rec_w = if self.recording { 16.0 } else { 0.0 };
        let fade = |c: Color32| if rds.locked { c } else { theme::text_3() };
        let who_font = theme::sans_semibold(12.5);
        let kind = rds.kind.as_deref().map(|k| format!(" · {k}")).unwrap_or_default();
        let snr = self.snr_db.map(|s| format!("SNR {s:.1} dB"));
        // Sized by what it says, within bounds, and by the widest SNR so it
        // does not breathe as the figure changes.
        // One size whatever arrives or whether it is recording (each line is
        // cut to it), so the card does not grow under the pointer as the
        // station's text comes in.
        let w = 296.0;
        let h = 84.0;
        let right_side = x + 10.0 + w < pan.right() - 4.0;
        let left = if right_side { x + 10.0 } else { x - 10.0 - w };
        let top = (pan.top() + 8.0).max((trace_y - h - 14.0).min(pan.top() + 8.0));
        let r = Rect::from_min_size(pos2(left, top), vec2(w, h));
        painter.rect(r, CornerRadius::same(4), alpha(theme::surface_2(), 0.93), Stroke::new(1.0, alpha(theme::AMBER, 0.45)), egui::StrokeKind::Inside);
        theme::glass(painter, r);
        if active {
            painter.rect_filled(Rect::from_min_size(r.left_top(), vec2(16.0, 2.0)), CornerRadius::ZERO, theme::EMBER);
        }
        receiver_tab(painter, r, "A", theme::AMBER, active);
        let anchor_x = if right_side { r.left() } else { r.right() };
        painter.line_segment([pos2(x, r.top() + 14.0), pos2(anchor_x, r.top() + 14.0)], Stroke::new(1.0, alpha(theme::AMBER, 0.5)));
        painter.text(r.left_top() + vec2(27.0, 7.0), Align2::LEFT_TOP, crate::tuning::fmt_mhz(self.freq_hz, 4), theme::mono_bold(14.0), theme::text());
        let chip = format!("{} · {}", self.mode.label(), crate::tuning::fmt_short_hz(self.bandwidth_hz));
        painter.text(r.right_top() + vec2(-8.0, 9.0), Align2::RIGHT_TOP, chip, theme::mono(10.0), theme::AMBER);
        let bar = Rect::from_min_size(r.left_top() + vec2(8.0, 30.0), vec2(w - 64.0, 6.0));
        mini_meter(painter, bar, self.meter, theme::OK);
        painter.text(pos2(r.right() - 8.0, bar.center().y), Align2::RIGHT_CENTER, &self.s_label, theme::mono_bold(11.0), theme::text());
        // Who: the name, the program type after it, the SNR at the right.
        let y_who = r.top() + 50.0;
        let mut right = r.right() - 8.0;
        if let Some(s) = &snr {
            let g = painter.layout_no_wrap(s.clone(), theme::mono(10.0), theme::text_2());
            painter.galley(pos2(right - g.size().x, y_who - g.size().y / 2.0), g.clone(), theme::text_2());
            right -= g.size().x + 10.0;
        }
        let room = right - (r.left() + 8.0);
        let name = crate::widgets::ellipsize(painter, &rds.station, who_font.clone(), room);
        let g = painter.layout_no_wrap(name, who_font, fade(theme::text()));
        let name_w = g.size().x;
        painter.galley(pos2(r.left() + 8.0, y_who - g.size().y / 2.0), g, fade(theme::text()));
        if !kind.is_empty() && name_w + 20.0 < room {
            let k = crate::widgets::ellipsize(painter, &kind, theme::sans(11.0), room - name_w);
            let g = painter.layout_no_wrap(k, theme::sans(11.0), fade(widgets_rds_color()));
            painter.galley(pos2(r.left() + 8.0 + name_w, y_who - g.size().y / 2.0), g, fade(widgets_rds_color()));
        }
        // What is on, cut to the card; a text arriving keeps its gaps.
        if let Some((now, progress)) = &rds.now {
            let y_now = r.top() + 67.0;
            let room = w - 16.0 - rec_w;
            let shown = crate::widgets::ellipsize(painter, now, theme::sans(11.5), room);
            let job = crate::bar::gapped(&shown, theme::sans(11.5), fade(theme::text_2()));
            let g = painter.layout_job(job);
            painter.galley(pos2(r.left() + 8.0, y_now - g.size().y / 2.0), g, theme::text_2());
            if let Some(p) = progress {
                let track = Rect::from_min_size(pos2(r.left() + 8.0, r.bottom() - 5.0), vec2(room, 2.0));
                painter.rect_filled(track, CornerRadius::same(1), alpha(widgets_rds_color(), 0.15));
                painter.rect_filled(Rect::from_min_size(track.min, vec2(room * p.clamp(0.0, 1.0), 2.0)), CornerRadius::same(1), alpha(widgets_rds_color(), 0.8));
            }
        }
        if self.recording {
            painter.circle_filled(pos2(r.right() - 12.0, r.bottom() - 12.0), 4.0, theme::EMBER);
        }
        r
    }
}

/// The color the app gives RDS everywhere it shows.
fn widgets_rds_color() -> Color32 {
    crate::widgets::protocol_color("RDS")
}

/// A flag's S-meter: segments in the receiver's color up to S9 and amber
/// past it, the unlit ones faint, and a tick where S9 is.
fn mini_meter(painter: &egui::Painter, bar: Rect, pos: f32, below: Color32) {
    let s9 = bar.left() + bar.width() * crate::meter::frac(crate::meter::S9_DBFS);
    let lit_to = bar.left() + bar.width() * pos.clamp(0.0, 1.0);
    painter.rect_filled(bar.expand(1.0), CornerRadius::same(1), theme::ink());
    let mut x = bar.left();
    while x < bar.right() - 0.5 {
        let c = if x < s9 { below } else { theme::AMBER };
        let seg = Rect::from_min_max(pos2(x, bar.top()), pos2((x + 2.0).min(bar.right()), bar.bottom()));
        painter.rect_filled(seg, CornerRadius::ZERO, if x < lit_to { c } else { alpha(c, 0.14) });
        x += 3.0;
    }
    painter.line_segment([pos2(s9, bar.top() - 2.0), pos2(s9, bar.bottom() + 2.0)], Stroke::new(1.0, alpha(theme::text(), 0.85)));
}

/// The receiver's letter as a small tab at the top-left of its flag, lit
/// when that receiver is the one being driven.
fn receiver_tab(painter: &egui::Painter, r: Rect, letter: &str, color: Color32, active: bool) {
    let tab = Rect::from_min_size(r.left_top() + vec2(8.0, 8.0), vec2(14.0, 14.0));
    painter.rect_filled(tab, CornerRadius::same(3), alpha(color, if active { 0.9 } else { 0.3 }));
    painter.text(tab.center(), Align2::CENTER_CENTER, letter, theme::mono_bold(10.0), if active { theme::ink() } else { theme::text() });
}

/// A waterfall age for the time axis: m:ss on an axis that reaches a
/// minute, every label alike, else seconds.
fn fmt_age(s: f64, step: f64, minutes: bool) -> String {
    if minutes {
        format!("−{}:{:02}", (s / 60.0).floor(), (s % 60.0).round())
    } else if step < 1.0 {
        format!("−{s:.1}s")
    } else {
        format!("−{s:.0}s")
    }
}

/// `old`, measured over one span, as `n` values over another: linear
/// between bins, and the "nothing yet" sentinel where the new span reaches
/// past the old.
fn resample(old: &[f32], (oc, os): (f64, f64), n: usize, (nc, ns): (f64, f64)) -> Vec<f32> {
    let m = old.len();
    if m < 2 || os <= 0.0 || ns <= 0.0 || n < 2 {
        return vec![-200.0; n];
    }
    let olo = oc - os / 2.0;
    let nlo = nc - ns / 2.0;
    (0..n)
        .map(|i| {
            let hz = nlo + i as f64 / (n - 1) as f64 * ns;
            let x = (hz - olo) / os * (m - 1) as f64;
            if !(0.0..=(m - 1) as f64).contains(&x) {
                return -200.0;
            }
            let a = x.floor() as usize;
            let b = (a + 1).min(m - 1);
            let t = (x - a as f64) as f32;
            old[a] + (old[b] - old[a]) * t
        })
        .collect()
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
    receiver_tab(painter, r, "B", theme::VIOLET, b.active);
    let anchor = if right { r.left() } else { r.right() };
    painter.line_segment([pos2(x, r.top() + 12.0), pos2(anchor, r.top() + 12.0)], Stroke::new(1.0, alpha(theme::VIOLET, 0.5)));
    painter.text(r.left_top() + vec2(27.0, 6.0), Align2::LEFT_TOP, crate::tuning::fmt_mhz(b.freq_hz, 4), theme::mono_bold(12.5), theme::text());
    painter.text(r.right_top() + vec2(-8.0, 7.0), Align2::RIGHT_TOP, b.mode.label(), theme::mono(10.0), theme::VIOLET);
    match (&b.note, b.level) {
        (Some(n), _) => {
            let n = crate::widgets::ellipsize(painter, n, theme::sans(11.0), w - 16.0);
            painter.text(r.left_top() + vec2(8.0, 25.0), Align2::LEFT_TOP, n, theme::sans(11.0), theme::WARN);
        }
        (None, Some(l)) => {
            let bar = Rect::from_min_size(r.left_top() + vec2(8.0, 29.0), vec2(w - 64.0, 5.0));
            mini_meter(painter, bar, crate::meter::frac(l), theme::VIOLET);
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
    fn wheel_tunes_by_the_step_and_recenters() {
        let mut h = harness();
        let r = pan_rect(&h);
        wheel(&mut h, r.center(), 2.0, Modifiers::NONE);
        assert_eq!(h.state().acts, vec![Act::Tune { hz: 146.2e6 + 12_500.0, lock: true }]);
    }

    #[test]
    fn ctrl_wheel_zooms_into_what_is_under_the_cursor() {
        let mut h = harness();
        let r = pan_rect(&h);
        // The center of the span is 146.0, a channel and more from the
        // receiver at 146.2: zooming in tunes there first, then zooms.
        wheel(&mut h, r.center(), 4.0, Modifiers::COMMAND);
        assert_eq!(h.state().acts, vec![Act::Tune { hz: 146.0e6, lock: false }, Act::Zoom(2.0)]);
        // Over the receiver itself, it only zooms; zooming out never tunes.
        h.state_mut().acts.clear();
        wheel(&mut h, pos2(x_of(r, 146.2e6), r.center().y), 4.0, Modifiers::COMMAND);
        wheel(&mut h, r.center(), -4.0, Modifiers::COMMAND);
        assert_eq!(h.state().acts, vec![Act::Zoom(2.0), Act::Zoom(1.0)]);
    }

    #[test]
    fn a_span_change_carries_the_trace_over() {
        let mut pan = Panafall::default();
        let view = View::default();
        let mut pwr = vec![-100.0f32; 1024];
        pwr[512] = -30.0;
        pan.push(&FftFrame { center_hz: 100e6, rate_hz: 2e6, pwr: pwr.clone(), ..Default::default() }, &view, 0.0);
        // Half the span around the same center: the carrier stays in the
        // middle of the carried-over trace rather than the trace restarting.
        let quiet = vec![-100.0f32; 1024];
        pan.push(&FftFrame { center_hz: 100e6, rate_hz: 1e6, pwr: quiet, ..Default::default() }, &view, 0.04);
        assert!(pan.smooth[512] > -60.0, "carried: {}", pan.smooth[512]);
        assert!(pan.smooth[256] < -90.0);
    }

    #[test]
    fn ages_read_as_seconds_then_minutes() {
        assert_eq!(fmt_age(5.0, 5.0, false), "−5s");
        assert_eq!(fmt_age(0.5, 0.5, false), "−0.5s");
        assert_eq!(fmt_age(90.0, 30.0, true), "−1:30");
        // One format down an axis that reaches a minute: not −20s, −40s, −1:00.
        assert_eq!(fmt_age(20.0, 20.0, true), "−0:20");
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
        assert!((c - (146e6 - 0.2e6)).abs() < 5_000.0, "center {c}");
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
    fn a_held_waterfall_scrolls_back_through_time() {
        let mut h = harness();
        // Forty seconds of frames, 25 a second: more than the waterfall shows.
        for i in 0..1000 {
            let mut f = h.state().frame.clone();
            f.pwr[(i / 10) % 1024] = -30.0;
            h.state_mut().pan.push(&f, &View::default(), i as f64 * 0.04);
        }
        h.state_mut().pan.toggle_hold();
        h.run_steps(2);
        assert_eq!(h.state().pan.held_age(), Some(0.0));
        let wf = h.state().pan.regions.unwrap()[1];
        // Five notches up: five seconds back (by row count; the test's rows
        // arrive faster than real time, so the clock age is near zero).
        wheel(&mut h, wf.center(), 5.0, Modifiers::NONE);
        h.run_steps(1);
        assert_eq!(h.state().pan.hold.as_ref().map(|x| x.back), Some(125));
        assert!(h.state().acts.is_empty(), "scrolling the held waterfall never tunes");
        h.state_mut().pan.toggle_hold();
        assert!(h.state().pan.hold.is_none());
    }

    #[test]
    fn a_held_waterfall_stays_on_its_rows_as_new_ones_arrive() {
        let mut h = harness();
        for i in 0..400 {
            let f = h.state().frame.clone();
            h.state_mut().pan.push(&f, &View::default(), i as f64 * 0.04);
        }
        h.state_mut().pan.toggle_hold();
        h.run_steps(2);
        let shown = h.state().pan.hold.as_ref().and_then(|x| x.shown);
        assert!(shown.is_some());
        // Two seconds more of spectrum: the hold is fifty rows further back
        // from the newest, showing the same rows, so nothing is redrawn.
        for i in 0..50 {
            let f = h.state().frame.clone();
            h.state_mut().pan.push(&f, &View::default(), 16.0 + i as f64 * 0.04);
        }
        h.run_steps(2);
        let held = h.state().pan.hold.as_ref().unwrap();
        assert_eq!(held.back, 50);
        assert_eq!(held.shown, shown, "the held picture did not move");
    }

    #[test]
    fn a_one_bin_carrier_survives_a_narrow_waterfall() {
        let lut: Vec<Color32> = (0..=u16::MAX).map(|q| if q > wf_quantize(-60.0) { Color32::RED } else { Color32::BLACK }).collect();
        let mut q = vec![wf_quantize(-100.0); 4096];
        q[2049] = wf_quantize(-30.0);
        // 4096 bins on 1100 pixels: every column is the strongest it covers.
        let mut out = Vec::new();
        color_row(&q, &col_bounds(4096, 1100), &lut, &mut out);
        assert_eq!(out.len(), 1100);
        assert_eq!(out.iter().filter(|c| **c == Color32::RED).count(), 1);
        // Fewer bins than pixels: the bins as they are.
        out.clear();
        color_row(&q[..512], &col_bounds(512, 1100), &lut, &mut out);
        assert_eq!(out.len(), 512);
    }

    #[test]
    fn the_quantized_waterfall_colors_as_the_levels_say() {
        let pan = Panafall::default();
        let (black, top) = (-105.0, -40.0);
        let palette = Colormap::Ember.lut();
        let lut = pan.build_lut(black, top, palette, theme::ink());
        let scale = 1023.0 / (top - black);
        let mut off = 0;
        let mut v = -189.0f32;
        while v < 10.0 {
            // The color the level itself maps to, unquantized.
            let k = ((v - black) * scale).clamp(0.0, 1023.0) as usize;
            if lut[wf_quantize(v) as usize] != palette[pan.tone[k] as usize] {
                off += 1;
            }
            v += 0.37;
        }
        // A hundredth of a dB can tip a level across a step of the tone
        // table, and no more than that.
        assert!(off < 6, "{off} levels colored differently");
        assert_eq!(lut[0], theme::ink(), "an empty row is ground");
    }

    #[test]
    fn a_palette_change_crossfades_and_finishes() {
        let ctx = egui::Context::default();
        let mut pan = Panafall::default();
        let mut view = View::default();
        let mut pwr = vec![-100.0f32; 512];
        pwr[200] = -40.0;
        pan.push(&FftFrame { center_hz: 100e6, rate_hz: 1e6, pwr, ..Default::default() }, &view, 0.0);
        pan.upload(&ctx, &view, 0.0, false, 300, 4096);
        assert!(pan.fade.is_none());
        assert!((0..301).all(|i| !pan.wf_stale[(pan.wf_head + i) % WF_ROWS]), "the rows on screen are colored");
        assert!(pan.wf_stale[(pan.wf_head + 400) % WF_ROWS], "the rest wait until shown");
        view.palette = Colormap::Classic;
        pan.upload(&ctx, &view, 1.0, false, 300, 4096);
        assert!(pan.fade.is_some(), "the old picture fades out over the new");
        pan.upload(&ctx, &view, 1.1, false, 300, 4096);
        assert!(pan.fade.is_some());
        pan.upload(&ctx, &view, 1.3, false, 300, 4096);
        assert!(pan.fade.is_none(), "gone after a quarter of a second");
        // Reduced motion snaps.
        view.palette = Colormap::Ember;
        pan.upload(&ctx, &view, 2.0, true, 300, 4096);
        assert!(pan.fade.is_none());
    }

    #[test]
    fn waterfall_keeps_its_history_across_a_retune() {
        let mut pan = Panafall::default();
        let view = View::default();
        let mut pwr = vec![-100.0f32; 1024];
        pwr[512] = -30.0;
        let f = FftFrame { center_hz: 100e6, rate_hz: 1e6, pwr: pwr.clone(), ..Default::default() };
        pan.push(&f, &view, 0.0);
        // The span moves up a quarter: the carrier's old row is drawn a
        // quarter of the columns to the left instead of being blanked, and
        // the row that arrives on the new span is drawn where it lies.
        let g = FftFrame { center_hz: 100.25e6, ..f };
        pan.push(&g, &view, 0.04);
        let cols = pan.wf_cols;
        let older = (pan.wf_head + 1) % WF_ROWS;
        let row = &pan.wf_q[older * cols..(older + 1) * cols];
        let hot = row.iter().enumerate().max_by_key(|a| *a.1).unwrap().0 as f64;
        let drawn_at = hot - (pan.wf_pan - pan.wf_row_pan[older]);
        assert!((drawn_at - 256.0).abs() <= 2.0, "carrier now at column {drawn_at}");
        assert_eq!(pan.wf_pan, pan.wf_row_pan[pan.wf_head]);
    }

}
