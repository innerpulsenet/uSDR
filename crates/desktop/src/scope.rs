//! The scope: what the demodulator hands the speaker or the slicer.
//!
//! Left, a spectrum — the passband slice of the main display, the audio, or
//! the FM multiplex with its pilot, stereo and RDS bands — over a small
//! waterfall of it. The spectrum is every frame's FFT averaged between
//! updates (Welch, across frames), drawn on a dB axis that follows the
//! signal, with a falling peak-hold above it and the mode's bands on a
//! ruler over the plot, so no label ever sits on the trace. Right, the time domain: an oscilloscope triggered on a
//! rising zero crossing, or for digital modes an eye diagram whose clock
//! phase is searched each frame, with the eye opening and EVM measured from
//! the same strobes.
//!
//! It updates at its own rate (10 Hz unless set otherwise), not the
//! display's: between updates it draws what it last measured, the
//! passband trace is smoothed across updates, and the oscilloscope keeps
//! its last few sweeps fading behind the newest, like a phosphor.

use crate::theme::{self, alpha};
use egui::{Align2, Color32, CornerRadius, Pos2, Rect, Sense, Shape, Stroke, Ui, pos2, vec2};
use std::collections::VecDeque;
use usdr_proto::{FftFrame, Mode, ScopeKind, Status};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Sub {
    #[default]
    Passband,
    Audio,
    Mpx,
    Disc,
}

pub struct Scope {
    pub sub: std::collections::HashMap<Mode, Sub>,
    fft: Option<Fft>,
    /// Power summed per bin over the frames since the spectrum last updated,
    /// and how many windowed segments went in.
    psd_acc: Vec<f32>,
    psd_n: u32,
    /// The averaged spectrum, dB, smoothed across updates.
    spectrum: Vec<f32>,
    /// Each bin's recent maximum, dB, falling at `PEAK_FALL_DB_S`.
    peak: Vec<f32>,
    peak_t: f64,
    /// The dB span drawn, (floor, top), easing after the spectrum.
    range: Option<(f32, f32)>,
    /// What the spectrum is of, so a new mode or view starts it afresh.
    spec_key: Option<(Mode, Sub)>,
    /// Rows of the little waterfall, newest first: each update's average in
    /// dB, unsmoothed, colored against the current range when drawn.
    mini_wf: VecDeque<Vec<f32>>,
    mini_tex: Option<egui::TextureHandle>,
    eye_sweeps: VecDeque<Vec<Vec<f32>>>,
    pub eye_open: f32,
    pub evm: f32,
    pub level_text: String,
    pub time_text: String,
    hist: Vec<f32>,
    /// Recent symbol decisions: the strobed value and the level it fell to.
    strobes: VecDeque<(f32, usize)>,
    /// The discriminator over the last couple of dozen symbols, normalized.
    wave: Vec<f32>,
    levels: Vec<f32>,
    /// Updates a second.
    pub rate_hz: f32,
    last_update: f64,
    /// What the frame the scope last measured said of itself, drawn
    /// until the next update.
    held: Option<(ScopeKind, f32)>,
    /// The mini waterfall's rows or palette changed since its texture was made.
    mini_dirty: bool,
    mini_palette: Option<crate::colormap::Colormap>,
    /// The oscilloscope's recent sweeps, newest first, as (x 0..1, y −1..1).
    osc: VecDeque<Vec<[f32; 2]>>,
    eye_two_level: bool,
    /// What the waveform last measured, to take each new frame exactly once.
    wave_sig: u64,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            sub: Default::default(),
            fft: None,
            psd_acc: Vec::new(),
            psd_n: 0,
            spectrum: Vec::new(),
            peak: Vec::new(),
            peak_t: 0.0,
            range: None,
            spec_key: None,
            mini_wf: VecDeque::new(),
            mini_tex: None,
            eye_sweeps: VecDeque::new(),
            eye_open: 0.0,
            evm: 0.0,
            level_text: String::new(),
            time_text: String::new(),
            hist: vec![0.0; 4],
            strobes: VecDeque::new(),
            wave: Vec::new(),
            levels: Vec::new(),
            rate_hz: 10.0,
            last_update: f64::NEG_INFINITY,
            held: None,
            mini_dirty: true,
            mini_palette: None,
            osc: VecDeque::new(),
            eye_two_level: false,
            wave_sig: 0,
        }
    }
}

const N_FFT: usize = 512;

/// An in-place radix-2 FFT of one power-of-two size. The audio spectrum
/// is 512 points about ten times a second, which needs nothing faster.
struct Fft {
    /// e^(−2πik/n) for k below n/2.
    twiddle: Vec<(f32, f32)>,
    /// Each index with its bits reversed, the order the butterflies want.
    rev: Vec<usize>,
}

impl Fft {
    fn new(n: usize) -> Fft {
        assert!(n >= 2 && n.is_power_of_two());
        let twiddle = (0..n / 2)
            .map(|k| {
                let a = -std::f64::consts::TAU * k as f64 / n as f64;
                (a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let rev = (0..n).map(|i| i.reverse_bits() >> (usize::BITS - n.trailing_zeros())).collect();
        Fft { twiddle, rev }
    }

    fn process(&self, re: &mut [f32], im: &mut [f32]) {
        let n = self.rev.len();
        for (i, &j) in self.rev.iter().enumerate() {
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut half = 1;
        while half < n {
            let step = n / (2 * half);
            for start in (0..n).step_by(2 * half) {
                for k in 0..half {
                    let (wr, wi) = self.twiddle[k * step];
                    let (a, b) = (start + k, start + k + half);
                    let tr = re[b] * wr - im[b] * wi;
                    let ti = re[b] * wi + im[b] * wr;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            half *= 2;
        }
    }
}
const MINI_W: usize = 256;
const MINI_H: usize = 56;
/// How fast the peak-hold trace falls back to the spectrum.
const PEAK_FALL_DB_S: f32 = 12.0;

fn default_sub(mode: Mode) -> Sub {
    match mode {
        // The multiplex says more about a broadcast station than its
        // passband does: pilot, stereo and RDS, each measured.
        Mode::Wfm => Sub::Mpx,
        Mode::Nfm | Mode::Am => Sub::Passband,
        Mode::Pager => Sub::Disc,
        _ => Sub::Audio,
    }
}

impl Sub {
    pub fn label(self) -> &'static str {
        match self {
            Sub::Passband => "PASSBAND",
            Sub::Audio => "AUDIO",
            Sub::Mpx => "MPX",
            Sub::Disc => "DISC",
        }
    }
}

/// The frequencies the spectrum spans: audio from zero, or the passband
/// either side of the carrier.
struct Span {
    lo: f32,
    hi: f32,
    signed: bool,
}

/// A stretch of the spectrum a mode puts something in, shaded in the plot
/// and named on the ruler above it.
struct Band {
    lo: f32,
    hi: f32,
    name: &'static str,
    tone: Color32,
}

/// One frequency the mode marks: a pilot, a tone.
struct Mark {
    hz: f32,
    name: &'static str,
}

/// What a mode's spectrum carries. The first band is the one whose peak the
/// readout reports.
fn annotations(status: &Status, sub: Sub, from_pass: bool) -> (Vec<Band>, Vec<Mark>) {
    let band = |lo: f32, hi: f32, name, tone| Band { lo, hi, name, tone };
    let mark = |hz: f32, name| Mark { hz, name };
    let (flo, fhi) = (status.filter_low_hz as f32, status.filter_high_hz as f32);
    if from_pass {
        // The passband slice is the filter already.
        return (vec![], vec![]);
    }
    match (status.mode, sub) {
        (Mode::Wfm, Sub::Mpx) => (
            vec![
                band(30.0, 15_000.0, "L+R", theme::CYAN),
                band(23_000.0, 53_000.0, "L−R", theme::VIOLET),
                band(54_600.0, 59_400.0, "RDS", theme::OK),
            ],
            vec![mark(19_000.0, "PILOT")],
        ),
        (Mode::Usb | Mode::Lsb | Mode::Ft8 | Mode::Ft4, _) if fhi > flo => {
            let (a, b) = (flo.abs().min(fhi.abs()), flo.abs().max(fhi.abs()));
            (vec![band(a, b, "PASSBAND", theme::AMBER)], vec![])
        }
        (Mode::Nfm, _) => (vec![band(300.0, 3_000.0, "VOICE", theme::AMBER), band(67.0, 254.0, "CTCSS", theme::VIOLET)], vec![]),
        (Mode::Am, _) if fhi > flo => (vec![band(50.0, (fhi - flo) / 2.0, "AUDIO", theme::AMBER)], vec![]),
        (Mode::Packet, _) => (vec![band(900.0, 2_500.0, "AFSK", theme::AMBER)], vec![mark(1_200.0, "MARK"), mark(2_200.0, "SPACE")]),
        // FLEX's deviation levels, seen as tones in the discriminator.
        (Mode::Pager, Sub::Disc) => (vec![], vec![mark(1_600.0, "±1.6k"), mark(4_800.0, "±4.8k")]),
        _ => (vec![], vec![]),
    }
}

/// A tick label: whole kHz on a kHz step, tenths below it.
fn fmt_khz(hz: f32, step: f32, signed: bool) -> String {
    let k = hz / 1000.0;
    let body = if step >= 1000.0 { format!("{:.0}k", k.abs()) } else { format!("{:.1}k", k.abs()) };
    if signed && hz.abs() > 0.5 { format!("{}{body}", if hz < 0.0 { "−" } else { "+" }) } else if hz.abs() <= 0.5 { "0".into() } else { body }
}

/// A frequency in the readout: Hz under 1 kHz, kHz above.
fn fmt_hz(hz: f32, signed: bool) -> String {
    let sign = if !signed { "" } else if hz < 0.0 { "−" } else { "+" };
    if hz.abs() < 1000.0 { format!("{sign}{:.0} Hz", hz.abs()) } else { format!("{sign}{:.3} kHz", hz.abs() / 1000.0) }
}

impl Scope {
    pub fn sub_for(&self, mode: Mode) -> Sub {
        self.sub.get(&mode).copied().unwrap_or_else(|| default_sub(mode))
    }

    /// The views this mode offers, in the order the selector shows them;
    /// empty where the scope has only the one.
    pub fn views(mode: Mode, kind: ScopeKind) -> &'static [Sub] {
        if kind == ScopeKind::Symbols || mode == Mode::Flex {
            return &[];
        }
        match mode {
            Mode::Wfm => &[Sub::Passband, Sub::Mpx],
            Mode::Nfm | Mode::Am | Mode::Packet | Mode::Ft8 | Mode::Ft4 => &[Sub::Passband, Sub::Audio],
            Mode::Pager => &[Sub::Passband, Sub::Disc],
            _ => &[],
        }
    }

    pub fn set_view(&mut self, mode: Mode, sub: Sub) {
        // The spectrum notices the change and starts afresh.
        self.sub.insert(mode, sub);
    }

    /// The scope's title, and a note on what it shows where there is no
    /// choice of view to say it.
    pub fn title(&self, mode: Mode, kind: ScopeKind) -> (&'static str, Option<&'static str>) {
        let sub = self.sub_for(mode);
        match (mode, kind, sub) {
            (Mode::Flex, _, _) => ("FLEX 2/4-FSK scope", Some("eye · slicer")),
            (_, ScopeKind::Symbols, _) => ("Digital eye", Some("4FSK / C4FM eye · slicer")),
            (Mode::Wfm, _, Sub::Mpx) => ("FM multiplex", None),
            (Mode::Wfm, _, _) => ("WFM passband", None),
            (Mode::Pager, _, Sub::Disc) => ("Pager discriminator", None),
            (_, _, Sub::Passband) => ("Passband", None),
            (Mode::Usb | Mode::Lsb, _, _) => ("Sideband audio", None),
            (Mode::Packet, _, _) => ("AFSK audio", None),
            (Mode::Ft8, _, _) => ("FT8 audio", None),
            (Mode::Ft4, _, _) => ("FT4 audio", None),
            _ => ("Audio", None),
        }
    }

    /// Add a frame's audio to the running average: Hann-windowed segments,
    /// half overlapped, so a frame longer than the FFT is used whole. A
    /// full-scale sine reads 0 dB in its bin.
    fn accumulate(&mut self, s: &[i16]) {
        if s.len() < 64 {
            return;
        }
        let fft = self.fft.get_or_insert_with(|| Fft::new(N_FFT));
        let take = s.len().min(N_FFT);
        let win: Vec<f32> = (0..take).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / (take - 1) as f32).cos()).collect();
        let norm = (win.iter().sum::<f32>() / 2.0 * 32000.0).powi(2);
        if self.psd_acc.len() != N_FFT / 2 {
            self.psd_acc = vec![0.0; N_FFT / 2];
            self.psd_n = 0;
        }
        let hop = (take / 2).max(1);
        let mut start = 0;
        loop {
            let mut re = vec![0.0f32; N_FFT];
            for (i, (r, w)) in re.iter_mut().zip(&win).enumerate() {
                *r = s[start + i] as f32 * w;
            }
            let mut im = vec![0.0; N_FFT];
            fft.process(&mut re, &mut im);
            for (a, (r, i)) in self.psd_acc.iter_mut().zip(re.iter().zip(&im)) {
                *a += (r * r + i * i) / norm;
            }
            self.psd_n += 1;
            if start + take >= s.len() {
                break;
            }
            start = (start + hop).min(s.len() - take);
        }
    }

    /// Close the average gathered since the last update into the spectrum.
    fn settle(&mut self, now: f64) {
        if self.psd_n == 0 {
            return;
        }
        let n = self.psd_n as f32;
        let db: Vec<f32> = self.psd_acc.iter().map(|p| 10.0 * (p / n).max(1e-12).log10()).collect();
        self.psd_acc.fill(0.0);
        self.psd_n = 0;
        self.take_trace(db, 0.5, now);
    }

    /// One update's trace, dB: smoothed into the spectrum, raised into the
    /// peak hold, a row of the little waterfall as it came, and the range
    /// eased toward the floor and the top it shows.
    fn take_trace(&mut self, db: Vec<f32>, keep: f32, now: f64) {
        let dt = (now - self.peak_t).clamp(0.0, 1.0) as f32;
        self.peak_t = now;
        if self.spectrum.len() == db.len() {
            for (a, b) in self.spectrum.iter_mut().zip(&db) {
                *a += (b - *a) * (1.0 - keep);
            }
            for (p, b) in self.peak.iter_mut().zip(&db) {
                *p = (*p - PEAK_FALL_DB_S * dt).max(*b);
            }
        } else {
            self.spectrum = db.clone();
            self.peak = db.clone();
        }
        let mut sorted = self.spectrum.clone();
        sorted.sort_by(f32::total_cmp);
        let floor = ((sorted[sorted.len() * 15 / 100] - 10.0) / 5.0).floor() * 5.0;
        let top = ((self.peak.iter().copied().fold(f32::MIN, f32::max) + 6.0) / 5.0).ceil() * 5.0;
        let top = top.max(floor + 40.0);
        self.range = Some(match self.range {
            Some((f, t)) => (f + (floor - f) * 0.25, t + (top - t) * 0.25),
            None => (floor, top),
        });
        let row = (0..MINI_W)
            .map(|i| {
                let x = i as f32 / (MINI_W - 1) as f32 * (db.len() - 1) as f32;
                let a = x as usize;
                let b = (a + 1).min(db.len() - 1);
                db[a] + (db[b] - db[a]) * (x - a as f32)
            })
            .collect();
        self.mini_wf.push_front(row);
        self.mini_wf.truncate(MINI_H);
        self.mini_dirty = true;
    }

    /// Forget the spectrum, for a new mode or view.
    fn clear_spectrum(&mut self) {
        self.psd_acc.clear();
        self.psd_n = 0;
        self.spectrum.clear();
        self.peak.clear();
        self.range = None;
        self.mini_wf.clear();
        self.mini_dirty = true;
    }

    pub fn ui(
        &mut self,
        ui: &mut Ui,
        f: Option<&FftFrame>,
        status: &Status,
        pass: Option<(&[f32], (f32, f32))>,
        tags: &[(f32, String)],
        look: &crate::spectrum::Look,
    ) {
        let mode = status.mode;
        let palette = look.palette;
        let rect = ui.available_rect_before_wrap();
        let hover = ui.allocate_rect(rect, Sense::hover()).hover_pos();
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(4), theme::ink());
        let Some(live) = f else {
            painter.text(rect.center(), Align2::CENTER_CENTER, "no signal yet", theme::sans(12.0), theme::text_3());
            return;
        };
        let sub = self.sub_for(mode);
        if self.spec_key != Some((mode, sub)) {
            self.spec_key = Some((mode, sub));
            self.clear_spectrum();
        }
        let symbols = live.scope_kind == ScopeKind::Symbols || mode == Mode::Flex;
        let from_pass = sub == Sub::Passband && !symbols;
        // The waveform and the eye take every new frame (the server's 25 a
        // second), and so does the spectrum's average; the rate setting paces
        // only when that average is drawn and the waterfall steps.
        let sig = live.scope.iter().take(64).fold(live.scope.len() as u64, |h, &v| h.wrapping_mul(31).wrapping_add(v as u16 as u64));
        let wave_fresh = sig != self.wave_sig;
        self.wave_sig = sig;
        if wave_fresh && !symbols && !from_pass {
            self.accumulate(&live.scope);
        }
        let now = ui.input(|i| i.time);
        let fresh = now - self.last_update >= 1.0 / f64::from(self.rate_hz.max(1.0)) || self.held.is_none();
        if fresh {
            self.last_update = now;
            self.held = Some((live.scope_kind, live.scope_rate_hz));
            if from_pass {
                if let Some((p, _)) = pass
                    && p.len() > 2
                {
                    self.take_trace(p.to_vec(), 0.55, now);
                }
            } else {
                self.settle(now);
            }
        }
        let Some((_, held_rate)) = self.held else { return };
        let gap = 8.0;
        // Readouts get a strip of their own under the plots, never over a trace.
        let strip = 16.0;
        let plots = Rect::from_min_max(rect.min, pos2(rect.right(), rect.bottom() - strip));
        let left = Rect::from_min_size(plots.min, vec2((plots.width() - gap) * 0.5, plots.height()));
        let right = Rect::from_min_max(pos2(left.right() + gap, plots.top()), plots.max);
        let text_y = rect.bottom() - 1.0;
        if symbols {
            // Digital: the eye on the left, the slicer on the right.
            grid(&painter, left, 4, 4);
            grid(&painter, right, 8, 4);
            self.eye(&painter, left, live, mode, wave_fresh);
            self.slicer(&painter, right, mode);
            painter.text(pos2(left.left() + 4.0, text_y), Align2::LEFT_BOTTOM, &self.level_text, theme::mono(10.0), theme::text_2());
            painter.text(pos2(right.right() - 4.0, text_y), Align2::RIGHT_BOTTOM, &self.time_text, theme::mono(10.0), theme::text_3());
            return;
        }

        // ---- left: spectrum, under its band ruler, over its waterfall
        let span = if from_pass {
            let bw = if status.bandwidth_hz > 0.0 { status.bandwidth_hz as f32 } else { 12_500.0 };
            Span { lo: -bw / 2.0, hi: bw / 2.0, signed: true }
        } else {
            Span { lo: 0.0, hi: held_rate.max(1.0) / 2.0, signed: false }
        };
        let (gutter, ruler_h, axis_h) = (30.0, 14.0, 14.0);
        let ruler = Rect::from_min_max(pos2(left.left() + gutter, left.top()), pos2(left.right(), left.top() + ruler_h));
        // A short dock gives the little waterfall's share to the spectrum,
        // which says more in the height there is.
        let with_mini = left.height() >= 150.0;
        let spec_bottom = if with_mini { left.top() + left.height() * 0.62 - axis_h } else { left.bottom() - axis_h };
        let spec = Rect::from_min_max(pos2(ruler.left(), ruler.bottom() + 2.0), pos2(left.right(), spec_bottom));
        let mini = Rect::from_min_max(pos2(spec.left(), spec.bottom() + axis_h), left.max);
        painter.rect(spec, CornerRadius::same(3), theme::ink(), Stroke::new(1.0, theme::line()), egui::StrokeKind::Inside);
        let x_of = |hz: f32| spec.left() + (hz - span.lo) / (span.hi - span.lo) * spec.width();
        let (bands, marks) = annotations(status, sub, from_pass);
        // Bands shaded faintly in the plot and named on the ruler; marks
        // dashed through the plot, named where the ruler has room.
        // Pills and labels kept apart: a mark's name needs clear ruler, a
        // callsign only clear of other names (it sits on the passband).
        let mut placed: Vec<Rect> = Vec::new();
        let mut names: Vec<Rect> = Vec::new();
        let tagged = !tags.is_empty() && !from_pass;
        for b in &bands {
            let (x0, x1) = (x_of(b.lo).max(spec.left()), x_of(b.hi).min(spec.right()));
            if x1 - x0 < 1.0 {
                continue;
            }
            painter.rect_filled(Rect::from_min_max(pos2(x0, spec.top() + 1.0), pos2(x1, spec.bottom() - 1.0)), CornerRadius::ZERO, alpha(b.tone, 0.06));
            let pill = Rect::from_min_max(pos2(x0, ruler.top() + 2.0), pos2(x1, ruler.bottom() - 1.0));
            painter.rect_filled(pill, CornerRadius::same(2), alpha(b.tone, 0.2));
            painter.line_segment([pos2(x0, pill.bottom()), pos2(x1, pill.bottom())], Stroke::new(1.0, alpha(b.tone, 0.7)));
            placed.push(pill);
            let g = painter.layout_no_wrap(b.name.to_string(), theme::mono(10.0), b.tone);
            if g.size().x + 6.0 <= pill.width() && !tagged {
                names.push(Rect::from_center_size(pill.center(), g.size()));
                painter.galley(pill.center() - g.size() / 2.0, g, b.tone);
            }
        }
        for m in &marks {
            if m.hz <= span.lo || m.hz >= span.hi {
                continue;
            }
            let x = x_of(m.hz);
            crate::spectrum::dashed_v(&painter, x, spec.top() + 1.0, spec.bottom() - 1.0, alpha(theme::AMBER, 0.45));
            let g = painter.layout_no_wrap(m.name.to_string(), theme::mono(10.0), theme::AMBER);
            let r = Rect::from_center_size(pos2(x, ruler.center().y + 0.5), g.size());
            if r.left() >= ruler.left() && r.right() <= ruler.right() && !placed.iter().any(|p| p.intersects(r.expand(2.0))) {
                painter.galley(r.min, g, alpha(theme::AMBER, 0.95));
                placed.push(r);
                names.push(r);
            } else {
                painter.circle_filled(pos2(x, ruler.center().y + 0.5), 2.0, alpha(theme::AMBER, 0.9));
            }
        }
        // What was decoded in the last slot, each at its audio frequency:
        // a tick in the plot, its callsign on the ruler where it fits.
        if tagged {
            let c = crate::widgets::protocol_color(mode.label());
            for (hz, name) in tags {
                if *hz <= span.lo || *hz >= span.hi {
                    continue;
                }
                let x = x_of(*hz);
                painter.line_segment([pos2(x, spec.top() + 1.0), pos2(x, spec.top() + 6.0)], Stroke::new(1.5, c));
                let g = painter.layout_no_wrap(name.clone(), theme::mono(10.0), c);
                let r = Rect::from_center_size(pos2(x, ruler.center().y + 0.5), g.size());
                if r.left() >= ruler.left() && r.right() <= ruler.right() && !names.iter().any(|p| p.intersects(r.expand(2.0))) {
                    painter.rect_filled(r.expand2(vec2(2.0, 0.0)), CornerRadius::same(2), theme::ink());
                    painter.galley(r.min, g, c);
                    names.push(r);
                } else {
                    painter.circle_filled(pos2(x, ruler.center().y + 0.5), 1.8, c);
                }
            }
        }
        // The dB grid, labeled in the gutter.
        let (floor, top) = self.range.unwrap_or((-90.0, -10.0));
        let y_of = |db: f32| spec.bottom() - (db - floor) / (top - floor) * spec.height();
        let step = crate::spectrum::nice_step(f64::from(top - floor), 4.0) as f32;
        let mut db = (floor / step).ceil() * step;
        while db <= top {
            let y = y_of(db);
            if y > spec.top() + 6.0 && y < spec.bottom() - 4.0 {
                painter.line_segment([pos2(spec.left() + 1.0, y), pos2(spec.right() - 1.0, y)], Stroke::new(1.0, theme::line()));
                painter.text(pos2(spec.left() - 4.0, y), Align2::RIGHT_CENTER, format!("{db:.0}"), theme::mono(10.0), theme::text_3());
            }
            db += step;
        }
        painter.text(pos2(spec.left() - 4.0, spec.bottom() + 1.0), Align2::RIGHT_TOP, "dB", theme::mono(10.0), theme::text_3());
        // Frequency ticks on a 1-2-5 step, labeled under the plot.
        let fstep = crate::spectrum::nice_step(f64::from(span.hi - span.lo), (spec.width() / 64.0).clamp(3.0, 8.0) as f64) as f32;
        let mut hz = (span.lo / fstep).ceil() * fstep;
        while hz <= span.hi + 0.5 {
            let x = x_of(hz);
            if x > spec.left() + 1.0 && x < spec.right() - 1.0 {
                painter.line_segment([pos2(x, spec.top() + 1.0), pos2(x, spec.bottom() - 1.0)], Stroke::new(1.0, theme::line()));
            }
            let align = if x <= spec.left() + 1.0 { Align2::LEFT_TOP } else if x >= spec.right() - 12.0 { Align2::RIGHT_TOP } else { Align2::CENTER_TOP };
            painter.text(pos2(x.clamp(spec.left(), spec.right()), spec.bottom() + 1.0), align, fmt_khz(hz, fstep, span.signed), theme::mono(10.0), theme::text_3());
            hz += fstep;
        }
        // Peak hold first, faint, then the spectrum in the display's own style
        // so the scope and the panadapter read as one instrument.
        let bin_x = |i: usize, n: usize| if from_pass { spec.left() + i as f32 / (n - 1).max(1) as f32 * spec.width() } else { spec.left() + i as f32 / n as f32 * spec.width() };
        let to_pts = |v: &[f32]| -> Vec<Pos2> { v.iter().enumerate().map(|(i, d)| pos2(bin_x(i, v.len()), y_of(*d).clamp(spec.top() + 1.0, spec.bottom()))).collect() };
        if self.spectrum.len() > 1 {
            painter.add(Shape::line(to_pts(&self.peak), Stroke::new(1.0, alpha(look.peak, 0.55))));
            let pts = to_pts(&self.spectrum);
            crate::spectrum::fill_under(&painter, &pts, spec.bottom(), look.fill, 0.4);
            painter.add(Shape::line(pts.clone(), Stroke::new(2.6, alpha(look.glow, 0.25))));
            painter.add(Shape::line(pts, Stroke::new(1.1, look.core)));
        }
        // Mini waterfall, one small texture rebuilt when a row arrives,
        // colored against the spectrum's own range.
        if self.mini_dirty || self.mini_palette != Some(palette) || self.mini_tex.is_none() {
            let lut = palette.lut();
            let mut px = Vec::with_capacity(MINI_W * MINI_H);
            for r in 0..MINI_H {
                match self.mini_wf.get(r) {
                    Some(row) => px.extend(row.iter().map(|v| lut[(((v - floor) / (top - floor)).clamp(0.0, 1.0) * 255.0) as usize])),
                    None => px.extend(std::iter::repeat_n(theme::ink(), MINI_W)),
                }
            }
            let img = egui::ColorImage::new([MINI_W, MINI_H], px);
            match &mut self.mini_tex {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.mini_tex = Some(ui.ctx().load_texture("scope-wf", img, egui::TextureOptions::LINEAR)),
            }
            self.mini_dirty = false;
            self.mini_palette = Some(palette);
        }
        if with_mini {
            if let Some(tex) = &self.mini_tex {
                painter.image(tex.id(), mini, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
            painter.rect_stroke(mini, egui::CornerRadius::ZERO, Stroke::new(1.0, theme::line()), egui::StrokeKind::Inside);
            let secs = MINI_H as f32 / self.rate_hz.max(1.0);
            painter.text(pos2(mini.left() - 4.0, mini.top()), Align2::RIGHT_TOP, "now", theme::mono(10.0), theme::text_3());
            painter.text(pos2(mini.left() - 4.0, mini.bottom()), Align2::RIGHT_BOTTOM, format!("−{secs:.0}s"), theme::mono(10.0), theme::text_3());
        }
        // The spectrum's readout: under the cursor while it is over the plot,
        // otherwise what the mode's bands measure.
        let hz_at = |x: f32| span.lo + (x - spec.left()) / spec.width() * (span.hi - span.lo);
        let readout = match hover.filter(|p| spec.expand(1.0).contains(*p) || (with_mini && mini.contains(*p))) {
            Some(p) if !self.spectrum.is_empty() => {
                let hz = hz_at(p.x);
                let x = p.x.clamp(spec.left(), spec.right());
                painter.line_segment([pos2(x, spec.top()), pos2(x, if with_mini { mini.bottom() } else { spec.bottom() })], Stroke::new(1.0, alpha(theme::text(), 0.35)));
                let i = self.bin_of(hz, &span, from_pass);
                format!("{} · {:.1} dB · peak {:.1}", fmt_hz(hz, span.signed), self.spectrum[i], self.peak[i])
            }
            _ => self.measure(status, sub, &span, from_pass),
        };
        // Cut at a field boundary to the left plot's width, so it never runs
        // into the time domain's readout.
        let mut parts: Vec<&str> = readout.split(" · ").collect();
        let fits = |p: &[&str]| painter.layout_no_wrap(p.join(" · "), theme::mono(10.0), Color32::PLACEHOLDER).size().x <= left.width() - 6.0;
        while parts.len() > 1 && !fits(&parts) {
            parts.pop();
        }
        painter.text(pos2(left.left() + 4.0, text_y), Align2::LEFT_BOTTOM, parts.join(" · "), theme::mono(10.0), theme::text_2());

        // ---- right: time domain
        grid(&painter, right, 8, 4);
        self.oscilloscope(&painter, right, live, mode == Mode::Pager && sub == Sub::Disc, wave_fresh);
        painter.text(pos2(right.left() + 4.0, text_y), Align2::LEFT_BOTTOM, &self.level_text, theme::mono(10.0), theme::text_2());
        painter.text(pos2(right.right() - 4.0, text_y), Align2::RIGHT_BOTTOM, &self.time_text, theme::mono(10.0), theme::text_3());
    }

    /// The spectrum bin nearest a frequency on the span.
    fn bin_of(&self, hz: f32, span: &Span, from_pass: bool) -> usize {
        let n = self.spectrum.len();
        let t = (hz - span.lo) / (span.hi - span.lo);
        let i = if from_pass { (t * (n - 1) as f32).round() } else { (t * n as f32).floor() };
        (i.max(0.0) as usize).min(n - 1)
    }

    /// Linear-power mean and max of the averaged spectrum over a band, dB.
    fn band_db(&self, lo: f32, hi: f32, span: &Span, from_pass: bool) -> (f32, f32) {
        let (a, b) = (self.bin_of(lo, span, from_pass), self.bin_of(hi, span, from_pass));
        let v = &self.spectrum[a.min(b)..=b.max(a)];
        let mean = v.iter().map(|d| 10f32.powf(d / 10.0)).sum::<f32>() / v.len() as f32;
        (10.0 * mean.max(1e-12).log10(), v.iter().copied().fold(f32::MIN, f32::max))
    }

    /// What the bands measure, said in the strip under the spectrum.
    fn measure(&self, status: &Status, sub: Sub, span: &Span, from_pass: bool) -> String {
        if self.spectrum.len() < 8 {
            return String::new();
        }
        let rbw = if from_pass { None } else { Some(span.hi * 2.0 / N_FFT as f32) };
        let rbw_text = rbw.map(|r| if r >= 1000.0 { format!("RBW {:.2} kHz", r / 1000.0) } else { format!("RBW {r:.1} Hz") });
        if status.mode == Mode::Wfm && sub == Sub::Mpx && span.hi > 60_000.0 {
            // Each against the multiplex's own floor above RDS, where a
            // station sends nothing.
            let floor = self.band_db(60_500.0, 63_500.0, span, false).0;
            let (_, pilot) = self.band_db(18_700.0, 19_300.0, span, false);
            let (stereo, _) = self.band_db(24_000.0, 52_000.0, span, false);
            let (rds, _) = self.band_db(55_500.0, 58_500.0, span, false);
            // Each over the floor. A band that does not stand out is said
            // to be under 3 dB, which is what was measured: "none" said the
            // station sends none, beside a panel showing its RDS decoded
            // (a subcarrier a decibel over a noisy multiplex still locks).
            let rel = |name: &str, d: f32| if d >= 3.0 { format!("{name} +{d:.0} dB") } else { format!("{name} under 3 dB") };
            return format!("{} · {} · {}", rel("pilot", pilot - floor), rel("L−R", stereo - floor), rel("RDS", rds - floor));
        }
        // Elsewhere, the strongest bin in the mode's passband (or anywhere).
        let (bands, _) = annotations(status, sub, from_pass);
        let (lo, hi) = bands.first().map_or((span.lo, span.hi), |b| (b.lo.max(span.lo), b.hi.min(span.hi)));
        let (a, b) = (self.bin_of(lo, span, from_pass), self.bin_of(hi, span, from_pass));
        let Some(i) = (a..=b).max_by(|&x, &y| self.spectrum[x].total_cmp(&self.spectrum[y])) else {
            return String::new();
        };
        let n = self.spectrum.len();
        let hz = if from_pass { span.lo + i as f32 / (n - 1) as f32 * (span.hi - span.lo) } else { (i as f32 + 0.5) * span.hi / n as f32 };
        let mut out = format!("peak {} · {:.0} dB", fmt_hz(hz, span.signed), self.spectrum[i]);
        if let Some(r) = rbw_text {
            out.push_str(" · ");
            out.push_str(&r);
        }
        out
    }

    fn oscilloscope(&mut self, painter: &egui::Painter, r: Rect, f: &FftFrame, disc: bool, fresh: bool) {
        let s = &f.scope;
        if fresh && s.len() >= 16 {
            self.measure_sweep(f, disc);
        }
        if disc {
            for (lvl, name) in [(0.6, "+3"), (0.2, "+1"), (-0.2, "−1"), (-0.6, "−3")] {
                let y = r.center().y - lvl * (r.height() / 2.0 - 4.0);
                crate::spectrum::dashed_h(painter, r.left(), r.left() + r.width() * 0.76, y, alpha(theme::AMBER, 0.35));
                painter.text(pos2(r.left() + r.width() * 0.77, y), Align2::LEFT_CENTER, name, theme::mono(10.0), alpha(theme::AMBER, 0.8));
            }
        }
        let to_screen = |p: &[f32; 2]| pos2(r.left() + p[0] * r.width(), r.center().y - p[1] * (r.height() / 2.0 - 4.0));
        // Older sweeps first, each fainter, then the newest with its glow.
        let n = self.osc.len();
        for (age, sweep) in self.osc.iter().enumerate().rev() {
            let pts: Vec<Pos2> = sweep.iter().map(to_screen).collect();
            if age == 0 {
                painter.add(Shape::line(pts.clone(), Stroke::new(3.0, alpha(theme::OK, 0.2))));
                painter.add(Shape::line(pts, Stroke::new(1.2, Color32::from_rgb(0xa8, 0xff, 0xcc))));
            } else {
                let a = 0.35 * (1.0 - age as f32 / n.max(1) as f32);
                painter.add(Shape::line(pts, Stroke::new(1.0, alpha(theme::OK, a))));
            }
        }
    }

    /// One oscilloscope sweep from a frame: triggered, normalized, kept.
    fn measure_sweep(&mut self, f: &FftFrame, disc: bool) {
        let s = &f.scope;
        let mut start = 0;
        if s.len() > 128 {
            let lim = (s.len() - 64).min(s.len() * 35 / 100);
            for i in 1..lim {
                if s[i - 1] <= 0 && s[i] > 0 {
                    start = i;
                    break;
                }
            }
        }
        let seg = &s[start..];
        let sweep: Vec<[f32; 2]> = seg
            .iter()
            .enumerate()
            .map(|(i, v)| [i as f32 / (seg.len() - 1).max(1) as f32, (*v as f32 / 32000.0).clamp(-1.2, 1.2)])
            .collect();
        self.osc.push_front(sweep);
        self.osc.truncate(4);
        let rms = (s.iter().map(|v| (*v as f32 / 32768.0).powi(2)).sum::<f32>() / s.len() as f32).sqrt();
        if disc {
            let mean = s.iter().map(|v| *v as f32 / 32000.0).sum::<f32>() / s.len() as f32;
            self.level_text = format!("offset {:+.2} kHz", mean * 8.0);
        } else {
            self.level_text = format!("level {:.0} dB", 20.0 * rms.max(1e-6).log10());
        }
        let ms_div = s.len() as f32 / f.scope_rate_hz.max(1.0) * 1000.0 / 8.0;
        self.time_text = if ms_div >= 1.0 { format!("{ms_div:.1} ms/div") } else { format!("{:.0} µs/div", ms_div * 1000.0) };
    }

    fn eye(&mut self, painter: &egui::Painter, r: Rect, f: &FftFrame, mode: Mode, fresh: bool) {
        if fresh && f.scope.len() >= 64 {
            self.measure_eye(f, mode);
        }
        self.draw_eye(painter, r, mode);
    }

    /// Clock search, sweeps, decisions and the eye's figures from a frame.
    fn measure_eye(&mut self, f: &FftFrame, mode: Mode) {
        let s = &f.scope;
        let rate = if f.scope_rate_hz > 1000.0 { f.scope_rate_hz } else { 48_000.0 };
        let two_level = mode == Mode::Packet || (mode != Mode::Flex && f.symbol_rate_hz > 0.0 && f.symbol_rate_hz <= 1200.0);
        let sym = if f.symbol_rate_hz > 0.0 { f.symbol_rate_hz } else if mode == Mode::Flex { 1600.0 } else { 4800.0 };
        let levels: &[f32] = if two_level { &[0.5, -0.5] } else { &[0.6, 0.2, -0.2, -0.6] };
        let target = if two_level { 0.5 } else { 0.6 };
        let mean = s.iter().map(|v| *v as f32).sum::<f32>() / s.len() as f32;
        let mut x: Vec<f32> = s.iter().map(|v| (*v as f32 - mean) / 32000.0).collect();
        let stride = (x.len() / 64).max(1);
        let p90 = x.iter().step_by(stride).map(|v| v.abs()).sum::<f32>() / (x.len() / stride).max(1) as f32 * 1.35;
        if p90 > 0.08 && p90 < 2.0 {
            for v in &mut x {
                *v *= target / p90;
            }
        }
        // Clock search: try candidate rates (FLEX runs 1600 or 3200 baud) and
        // phases, keep the one whose strobes sit closest to a level.
        let rates: Vec<f32> = if mode == Mode::Flex && (sym == 0.0 || (sym - 1600.0).abs() < 1.0) { vec![1600.0, 3200.0] } else { vec![sym] };
        let mut best = (f32::MAX, 0.0f32, sym);
        for &rs in &rates {
            let sps = (rate / rs).max(2.0);
            let steps = (sps.round() as usize).clamp(4, 12);
            for k in 0..steps {
                let phase = k as f32 * sps / steps as f32;
                let mut err = 0.0;
                let mut cnt = 0;
                let mut t = phase;
                while (t as usize) < x.len().min(512) {
                    let v = x[t as usize];
                    let d = levels.iter().map(|l| (v - l).powi(2)).fold(f32::MAX, f32::min);
                    err += d;
                    cnt += 1;
                    t += sps;
                }
                let e = err / cnt.max(1) as f32;
                if e < best.0 {
                    best = (e, phase, rs);
                }
            }
        }
        let (err, phase, rs) = best;
        let sps = (rate / rs).max(2.0);
        self.evm = ((err.sqrt() / target) * 100.0).min(99.9);
        self.eye_open = (100.0 - 1.8 * self.evm).round().clamp(15.0, 96.0);

        // Sweeps two symbols long, kept a few frames for persistence.
        let span = (2.0 * sps).round() as usize;
        let mut sweeps = Vec::new();
        let mut t = (phase - sps / 2.0).max(0.0);
        while (t as usize) + span < x.len() && sweeps.len() < 48 {
            let a = t as usize;
            sweeps.push(x[a..a + span].to_vec());
            t += sps;
        }
        self.eye_sweeps.push_front(sweeps);
        self.eye_sweeps.truncate(4);
        // Decisions at every strobe, for the slicer and its histogram.
        let mut counts = vec![0f32; levels.len()];
        let mut t = phase;
        while (t as usize) < x.len() {
            let v = x[t as usize];
            let (k, _) = levels.iter().enumerate().map(|(k, l)| (k, (v - l).abs())).fold((0, f32::MAX), |a, b| if b.1 < a.1 { b } else { a });
            counts[k] += 1.0;
            self.strobes.push_back((v, k));
            t += sps;
        }
        while self.strobes.len() > 64 {
            self.strobes.pop_front();
        }
        let total: f32 = counts.iter().sum::<f32>().max(1.0);
        if self.hist.len() != counts.len() {
            self.hist = vec![0.0; counts.len()];
        }
        for (h, c) in self.hist.iter_mut().zip(&counts) {
            *h = *h * 0.75 + (c / total) * 0.25;
        }
        let shown = if mode == Mode::Flex { 16.0 } else { 24.0 };
        let n = ((shown * sps) as usize).min(x.len());
        self.wave = x[x.len() - n..].to_vec();
        self.levels = levels.to_vec();
        self.eye_two_level = two_level;
        self.level_text = format!("eye {:.0}% · EVM {:.1}%", self.eye_open, self.evm);
        self.time_text = format!("{:.1} kBd", rs / 1000.0);
    }

    fn draw_eye(&self, painter: &egui::Painter, r: Rect, mode: Mode) {
        let half = r.height() / 2.0 - 12.0;
        for l in &self.levels {
            let y = r.center().y - l * half;
            painter.line_segment([pos2(r.left(), y), pos2(r.right(), y)], Stroke::new(1.0, alpha(theme::CYAN, 0.18)));
        }
        for xs in [0.25, 0.75] {
            crate::spectrum::dashed_v(painter, r.left() + r.width() * xs, r.top(), r.bottom(), alpha(theme::OK, 0.45));
        }
        for (age, sweeps) in self.eye_sweeps.iter().enumerate().rev() {
            let a = 0.32 * (1.0 - age as f32 / 4.0);
            for sw in sweeps {
                let pts: Vec<Pos2> = sw
                    .iter()
                    .enumerate()
                    .map(|(i, v)| pos2(r.left() + i as f32 / (sw.len() - 1).max(1) as f32 * r.width(), r.center().y - v.clamp(-1.2, 1.2) * half))
                    .collect();
                painter.add(Shape::line(pts, Stroke::new(1.0, alpha(if mode == Mode::Flex { theme::CYAN } else { theme::OK }, a))));
            }
        }
        if self.levels.is_empty() {
            return;
        }
        // Calipers at the first strobe.
        let two_level = self.eye_two_level;
        let open_h = self.eye_open / 100.0 * if two_level { 0.7 } else { 0.38 } * half;
        let cx = r.left() + r.width() * 0.25;
        painter.line_segment([pos2(cx - 6.0, r.center().y - open_h), pos2(cx + 6.0, r.center().y - open_h)], Stroke::new(1.5, theme::AMBER));
        painter.line_segment([pos2(cx - 6.0, r.center().y + open_h), pos2(cx + 6.0, r.center().y + open_h)], Stroke::new(1.5, theme::AMBER));
        painter.text(pos2(cx + 8.0, r.center().y - open_h - 2.0), Align2::LEFT_BOTTOM, format!("EYE {:.0}%", self.eye_open), theme::mono(10.0), theme::AMBER);
    }
}

fn level_color(levels: usize, k: usize) -> Color32 {
    if levels == 2 {
        [theme::OK, theme::ERR][k.min(1)]
    } else {
        [theme::CYAN, theme::OK, theme::AMBER, theme::ERR][k.min(3)]
    }
}

impl Scope {
    /// Where each symbol falls: the decision levels, the discriminator over
    /// the last couple of dozen symbols, the strobes as dots, how often each
    /// level is hit, and the decided symbols as a tape along the bottom.
    fn slicer(&self, painter: &egui::Painter, r: Rect, mode: Mode) {
        let levels = &self.levels;
        if levels.is_empty() {
            return;
        }
        let tape_h = 18.0;
        let area = Rect::from_min_max(r.min, pos2(r.right(), r.bottom() - tape_h));
        let half = area.height() / 2.0 - 10.0;
        let y_of = |v: f32| area.center().y - v.clamp(-1.2, 1.2) * half;
        let plot_r = area.right() - 70.0;
        let n = levels.len();
        for (k, l) in levels.iter().enumerate() {
            let y = y_of(*l);
            let c = level_color(n, k);
            painter.rect_filled(Rect::from_min_max(pos2(area.left(), y - 6.0), pos2(plot_r, y + 6.0)), CornerRadius::ZERO, alpha(c, 0.05));
            crate::spectrum::dashed_h(painter, area.left(), plot_r, y, alpha(c, 0.45));
        }
        if self.wave.len() > 1 {
            let pts: Vec<Pos2> = self
                .wave
                .iter()
                .enumerate()
                .map(|(i, v)| pos2(area.left() + 6.0 + i as f32 / (self.wave.len() - 1) as f32 * (plot_r - area.left() - 12.0), y_of(*v)))
                .collect();
            let c = if mode == Mode::Flex { theme::CYAN } else { theme::OK };
            painter.add(Shape::line(pts.clone(), Stroke::new(2.8, alpha(c, 0.22))));
            painter.add(Shape::line(pts, Stroke::new(1.2, c)));
        }
        let span = (plot_r - area.left() - 60.0).max(20.0);
        for (i, (v, k)) in self.strobes.iter().enumerate() {
            let x = area.left() + 30.0 + (i as f32 * 9.0) % span;
            painter.circle_filled(pos2(x, y_of(*v)), 2.2, alpha(level_color(n, *k), 0.8));
        }
        for (k, (l, h)) in levels.iter().zip(&self.hist).enumerate() {
            let y = y_of(*l);
            let w = h * 58.0;
            painter.rect_filled(Rect::from_min_max(pos2(area.right() - 4.0 - w, y - 3.0), pos2(area.right() - 4.0, y + 3.0)), CornerRadius::same(1), alpha(level_color(n, k), 0.75));
        }
        // The dibit tape.
        let names: &[&str] = if n == 2 { &["1", "0"] } else { &["01", "00", "10", "11"] };
        let tape = Rect::from_min_max(pos2(r.left(), r.bottom() - tape_h), r.max);
        painter.line_segment([tape.left_top(), tape.right_top()], Stroke::new(1.0, theme::line()));
        let fit = ((tape.width() - 12.0) / 20.0) as usize;
        let recent: Vec<&(f32, usize)> = self.strobes.iter().rev().take(fit.min(26)).collect();
        for (i, (_, k)) in recent.iter().rev().enumerate() {
            painter.text(
                pos2(tape.left() + 8.0 + i as f32 * 20.0, tape.center().y),
                Align2::LEFT_CENTER,
                names[(*k).min(n - 1)],
                theme::mono(10.0),
                level_color(n, *k),
            );
        }
    }
}

fn grid(painter: &egui::Painter, r: Rect, cols: usize, rows: usize) {
    painter.rect(r, CornerRadius::same(3), theme::ink(), Stroke::new(1.0, theme::line()), egui::StrokeKind::Inside);
    for i in 1..cols {
        let x = r.left() + r.width() * i as f32 / cols as f32;
        painter.line_segment([pos2(x, r.top()), pos2(x, r.bottom())], Stroke::new(1.0, theme::line()));
    }
    for i in 1..rows {
        let y = r.top() + r.height() * i as f32 / rows as f32;
        painter.line_segment([pos2(r.left(), y), pos2(r.right(), y)], Stroke::new(1.0, theme::line()));
    }
}

#[cfg(test)]
mod tests {
    use super::{Fft, Scope};

    #[test]
    fn the_averaged_spectrum_reads_a_tone_at_its_level() {
        // Half-scale 1 kHz at 8 kS/s, two frames of 1024: four segments.
        let s: Vec<i16> = (0..1024).map(|i| (16000.0 * (std::f32::consts::TAU * 1000.0 * i as f32 / 8000.0).sin()) as i16).collect();
        let mut sc = Scope::default();
        sc.accumulate(&s);
        sc.accumulate(&s);
        assert_eq!(sc.psd_n, 6);
        sc.settle(0.0);
        let top = (0..sc.spectrum.len()).max_by(|&a, &b| sc.spectrum[a].total_cmp(&sc.spectrum[b])).unwrap();
        assert_eq!(top, 64);
        assert!((sc.spectrum[64] + 6.0).abs() < 0.5, "{}", sc.spectrum[64]);
        // The range brackets it, and the peak hold starts at the spectrum.
        let (lo, hi) = sc.range.unwrap();
        assert!(lo < -40.0 && hi > -6.0, "{lo} {hi}");
        assert_eq!(sc.peak, sc.spectrum);
    }

    #[test]
    fn the_peak_hold_falls_back_after_a_burst() {
        let mut sc = Scope::default();
        sc.take_trace(vec![-20.0; 8], 0.5, 0.0);
        sc.take_trace(vec![-80.0; 8], 0.5, 1.0);
        assert!((sc.peak[0] - (-20.0 - super::PEAK_FALL_DB_S)).abs() < 1e-3);
        assert!(sc.spectrum[0] < -40.0);
    }

    #[test]
    fn the_fft_agrees_with_a_plain_dft() {
        for n in [2, 8, 512] {
            let x: Vec<(f32, f32)> = (0..n).map(|i| (((i * 7919) % 101) as f32 / 50.0 - 1.0, ((i * 104_729) % 37) as f32 / 18.0 - 1.0)).collect();
            let (mut re, mut im): (Vec<f32>, Vec<f32>) = x.iter().copied().unzip();
            Fft::new(n).process(&mut re, &mut im);
            for k in 0..n {
                let (mut sr, mut si) = (0.0f64, 0.0f64);
                for (t, &(xr, xi)) in x.iter().enumerate() {
                    let a = -std::f64::consts::TAU * (k * t % n) as f64 / n as f64;
                    sr += xr as f64 * a.cos() - xi as f64 * a.sin();
                    si += xr as f64 * a.sin() + xi as f64 * a.cos();
                }
                let err = ((re[k] as f64 - sr).powi(2) + (im[k] as f64 - si).powi(2)).sqrt();
                assert!(err < 1e-3 * (n as f64).sqrt(), "n {n} bin {k}: got ({}, {}), want ({sr}, {si})", re[k], im[k]);
            }
        }
    }

    #[test]
    fn a_tone_lands_in_its_bin() {
        let n = 512;
        let (mut re, mut im): (Vec<f32>, Vec<f32>) = (0..n).map(|i| ((std::f32::consts::TAU * 37.0 * i as f32 / n as f32).cos(), 0.0)).unzip();
        Fft::new(n).process(&mut re, &mut im);
        let mag: Vec<f32> = re.iter().zip(&im).map(|(r, i)| r.hypot(*i)).collect();
        let top = (0..n / 2).max_by(|&a, &b| mag[a].total_cmp(&mag[b])).unwrap();
        assert_eq!(top, 37);
        assert!((mag[37] - n as f32 / 2.0).abs() < 0.01 * n as f32);
    }
}
