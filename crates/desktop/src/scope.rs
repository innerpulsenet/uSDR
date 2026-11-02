//! The scope: what the demodulator hands the speaker or the slicer.
//!
//! Left, a spectrum — the passband slice of the main display, the audio, or
//! the FM multiplex with its pilot, stereo and RDS marks — over a small
//! waterfall of it. Right, the time domain: an oscilloscope triggered on a
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
use num_complex::Complex32;
use std::collections::VecDeque;
use usdr_proto::{FftFrame, Mode, ScopeKind};

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
    fft: Option<std::sync::Arc<dyn rustfft::Fft<f32>>>,
    spectrum: Vec<f32>,
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
    /// The frame the scope last measured, drawn until the next update.
    held: Option<FftFrame>,
    /// The passband slice, normalized and smoothed across updates.
    pass_smooth: Vec<f32>,
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
            spectrum: Vec::new(),
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
            pass_smooth: Vec::new(),
            osc: VecDeque::new(),
            eye_two_level: false,
            wave_sig: 0,
        }
    }
}

const N_FFT: usize = 512;
const MINI_W: usize = 160;
const MINI_H: usize = 48;

fn default_sub(mode: Mode) -> Sub {
    match mode {
        Mode::Wfm | Mode::Nfm | Mode::Am => Sub::Passband,
        Mode::Pager => Sub::Disc,
        _ => Sub::Audio,
    }
}

impl Scope {
    pub fn sub_for(&self, mode: Mode) -> Sub {
        self.sub.get(&mode).copied().unwrap_or_else(|| default_sub(mode))
    }

    /// The other view this mode offers, if any.
    pub fn toggle(&mut self, mode: Mode) {
        let next = match (mode, self.sub_for(mode)) {
            (Mode::Wfm, Sub::Passband) => Sub::Mpx,
            (Mode::Wfm, _) => Sub::Passband,
            (Mode::Nfm | Mode::Am | Mode::Packet, Sub::Passband) => Sub::Audio,
            (Mode::Nfm | Mode::Am | Mode::Packet, _) => Sub::Passband,
            (Mode::Pager, Sub::Passband) => Sub::Disc,
            (Mode::Pager, _) => Sub::Passband,
            (_, s) => s,
        };
        self.sub.insert(mode, next);
        self.mini_wf.clear();
    }

    pub fn title(&self, mode: Mode, kind: ScopeKind) -> (&'static str, String) {
        let sub = self.sub_for(mode);
        match (mode, kind, sub) {
            (Mode::Flex, _, _) => ("FLEX 2/4-FSK scope", "eye · slicer".into()),
            (_, ScopeKind::Symbols, _) => ("Digital eye", "4FSK / C4FM eye".into()),
            (Mode::Wfm, _, Sub::Mpx) => ("FM multiplex", "MPX ⇄ passband".into()),
            (Mode::Wfm, _, _) => ("WFM passband", "passband ⇄ MPX".into()),
            (Mode::Pager, _, Sub::Disc) => ("Pager discriminator", "disc ⇄ passband".into()),
            (_, _, Sub::Passband) => ("Passband", "passband ⇄ audio".into()),
            (Mode::Usb | Mode::Lsb, _, _) => ("Sideband audio", "300–2800 Hz".into()),
            (Mode::Packet, _, _) => ("AFSK audio", "audio ⇄ passband".into()),
            _ => ("Audio", "audio ⇄ passband".into()),
        }
    }

    fn audio_spectrum(&mut self, s: &[i16]) {
        if s.len() < 64 {
            return;
        }
        let fft = self
            .fft
            .get_or_insert_with(|| rustfft::FftPlanner::new().plan_fft_forward(N_FFT))
            .clone();
        let take = s.len().min(N_FFT);
        let start = s.len() - take;
        let mut buf: Vec<Complex32> = (0..N_FFT)
            .map(|i| {
                if i < take {
                    let w = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / (take - 1) as f32).cos();
                    Complex32::new(s[start + i] as f32 / 32000.0 * w, 0.0)
                } else {
                    Complex32::new(0.0, 0.0)
                }
            })
            .collect();
        fft.process(&mut buf);
        let norm = take as f32 / 4.0;
        let out: Vec<f32> = buf[..N_FFT / 2]
            .iter()
            .map(|c| 20.0 * (c.norm() / norm).max(1e-6).log10())
            .collect();
        if self.spectrum.len() == out.len() {
            for (a, b) in self.spectrum.iter_mut().zip(&out) {
                *a += (b - *a) * 0.5;
            }
        } else {
            self.spectrum = out;
        }
    }

    pub fn ui(
        &mut self,
        ui: &mut Ui,
        f: Option<&FftFrame>,
        mode: Mode,
        pass: Option<(&[f32], (f32, f32))>,
        palette: crate::colormap::Colormap,
    ) {
        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(4), theme::ink());
        let Some(live) = f else {
            painter.text(rect.center(), Align2::CENTER_CENTER, "no signal yet", theme::sans(12.0), theme::text_3());
            return;
        };
        // Measure at the scope's own rate; draw the held measurement between.
        let now = ui.input(|i| i.time);
        let fresh = now - self.last_update >= 1.0 / f64::from(self.rate_hz.max(1.0)) || self.held.is_none();
        if fresh {
            self.last_update = now;
            self.held = Some(live.clone());
            if let Some((p, (lo, hi))) = pass
                && p.len() > 2
            {
                let range = (hi - lo).max(1.0);
                let tr: Vec<f32> = p.iter().map(|v| ((v - lo) / range).clamp(0.0, 1.0)).collect();
                if self.pass_smooth.len() == tr.len() {
                    for (a, b) in self.pass_smooth.iter_mut().zip(&tr) {
                        *a += (b - *a) * 0.45;
                    }
                } else {
                    self.pass_smooth = tr;
                }
            }
        }
        let held = self.held.clone();
        let Some(f) = held.as_ref() else { return };
        // The waveform and the eye take every new frame (the server's 25 a
        // second); the rate setting paces only the spectrum and its waterfall.
        let sig = live.scope.iter().take(64).fold(live.scope.len() as u64, |h, &v| h.wrapping_mul(31).wrapping_add(v as u16 as u64));
        let wave_fresh = sig != self.wave_sig;
        self.wave_sig = sig;
        let gap = 8.0;
        // Readouts get a strip of their own under the plots, never over a trace.
        let strip = 16.0;
        let plots = Rect::from_min_max(rect.min, pos2(rect.right(), rect.bottom() - strip));
        let left = Rect::from_min_size(plots.min, vec2((plots.width() - gap) * 0.5, plots.height()));
        let right = Rect::from_min_max(pos2(left.right() + gap, plots.top()), plots.max);
        let text_y = rect.bottom() - 1.0;
        let sub = self.sub_for(mode);
        let symbols = f.scope_kind == ScopeKind::Symbols || mode == Mode::Flex;
        if symbols {
            // Digital: the eye on the left, the slicer on the right.
            grid(&painter, left, 4, 4);
            grid(&painter, right, 8, 4);
            self.eye(&painter, left, live, mode, wave_fresh);
            self.slicer(&painter, right, mode);
            painter.text(pos2(left.left() + 4.0, text_y), Align2::LEFT_BOTTOM, &self.level_text, theme::mono(10.0), theme::text_2());
            painter.text(pos2(right.right() - 4.0, text_y), Align2::RIGHT_BOTTOM, &self.time_text, theme::mono(10.0), theme::text_3());
            if resp.clicked() {
                self.toggle(mode);
            }
            return;
        }

        // ---- left: spectrum
        let spec = Rect::from_min_max(left.min, pos2(left.right(), left.top() + left.height() * 0.6 - 14.0));
        let mini = Rect::from_min_max(pos2(left.left(), spec.bottom() + 14.0), left.max);
        grid(&painter, spec, 4, 3);
        let (trace, axis): (Vec<f32>, Vec<(f32, String)>) = if sub == Sub::Passband && !symbols {
            if self.pass_smooth.len() > 2 {
                (self.pass_smooth.clone(), vec![(0.0, "−".into()), (0.5, "center".into()), (1.0, "+".into())])
            } else {
                (Vec::new(), Vec::new())
            }
        } else {
            if fresh {
                self.audio_spectrum(&f.scope);
            }
            let max_hz = f.scope_rate_hz.max(1.0) / 2.0;
            let tr: Vec<f32> = self.spectrum.iter().map(|d| ((d + 80.0) / 80.0).clamp(0.0, 1.0)).collect();
            let mut ax: Vec<(f32, String)> = (0..=4)
                .map(|i| (i as f32 / 4.0, format!("{:.1}k", max_hz * i as f32 / 4.0 / 1000.0)))
                .collect();
            let marks: &[(f32, &str)] = match (mode, sub) {
                (Mode::Wfm, Sub::Mpx) => &[(19_000.0, "PILOT"), (38_000.0, "STEREO"), (57_000.0, "RDS")],
                (Mode::Packet, _) => &[(1_200.0, "MARK"), (2_200.0, "SPACE")],
                (Mode::Nfm, _) => &[(250.0, "CTCSS")],
                // FLEX's deviation levels, seen as tones in the discriminator.
                (Mode::Pager, Sub::Disc) => &[(1_600.0, "±1.6k"), (4_800.0, "±4.8k")],
                _ => &[],
            };
            for (hz, name) in marks {
                if *hz < max_hz {
                    let x = spec.left() + hz / max_hz * spec.width();
                    crate::spectrum::dashed_v(&painter, x, spec.top(), spec.bottom(), alpha(theme::WARN, 0.5));
                    painter.text(pos2(x + 3.0, spec.top() + 4.0), Align2::LEFT_TOP, *name, theme::mono(9.0), alpha(theme::WARN, 0.9));
                }
            }
            ax.retain(|(t, _)| *t < 1.01);
            (tr, ax)
        };
        if trace.len() > 1 {
            let pts: Vec<Pos2> = trace
                .iter()
                .enumerate()
                .map(|(i, t)| pos2(spec.left() + i as f32 / (trace.len() - 1) as f32 * spec.width(), spec.bottom() - t * spec.height()))
                .collect();
            crate::spectrum::fill_under(&painter, &pts, spec.bottom(), theme::AMBER, 0.4);
            painter.add(Shape::line(pts.clone(), Stroke::new(2.6, alpha(theme::AMBER, 0.25))));
            painter.add(Shape::line(pts, Stroke::new(1.1, Color32::from_rgb(0xff, 0xe4, 0xbd))));
            // Resampled to a fixed width so rows of any length stack. One row
            // per update, so the little waterfall runs at the scope's rate.
            let row: Vec<f32> = if !fresh { Vec::new() } else { (0..MINI_W)
                .map(|i| {
                    let x = i as f32 / (MINI_W - 1) as f32 * (trace.len() - 1) as f32;
                    let a = x as usize;
                    let b = (a + 1).min(trace.len() - 1);
                    trace[a] + (trace[b] - trace[a]) * (x - a as f32)
                })
                .collect() };
            if fresh {
                self.mini_wf.push_front(row);
                self.mini_wf.truncate(MINI_H);
            }
        }
        for (t, label) in axis {
            let x = spec.left() + t * spec.width();
            let align = if t <= 0.0 { Align2::LEFT_TOP } else if t >= 1.0 { Align2::RIGHT_TOP } else { Align2::CENTER_TOP };
            painter.text(pos2(x, spec.bottom() + 1.0), align, label, theme::mono(9.0), theme::text_3());
        }
        // Mini waterfall, one small texture rebuilt each frame.
        let lut = palette.lut();
        let mut px = Vec::with_capacity(MINI_W * MINI_H);
        for r in 0..MINI_H {
            match self.mini_wf.get(r) {
                Some(row) => px.extend(row.iter().map(|v| lut[(v.clamp(0.0, 1.0) * 255.0) as usize])),
                None => px.extend(std::iter::repeat_n(theme::ink(), MINI_W)),
            }
        }
        let img = egui::ColorImage::new([MINI_W, MINI_H], px);
        let tex = self
            .mini_tex
            .get_or_insert_with(|| ui.ctx().load_texture("scope-wf", img.clone(), egui::TextureOptions::LINEAR));
        tex.set(img, egui::TextureOptions::LINEAR);
        painter.image(tex.id(), mini, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);

        // ---- right: time domain
        grid(&painter, right, 8, 4);
        self.oscilloscope(&painter, right, live, mode == Mode::Pager && sub == Sub::Disc, wave_fresh);
        painter.text(pos2(right.left() + 4.0, text_y), Align2::LEFT_BOTTOM, &self.level_text, theme::mono(10.0), theme::text_2());
        painter.text(pos2(right.right() - 4.0, text_y), Align2::RIGHT_BOTTOM, &self.time_text, theme::mono(10.0), theme::text_3());
        if resp.clicked() {
            self.toggle(mode);
        }
    }

    fn oscilloscope(&mut self, painter: &egui::Painter, r: Rect, f: &FftFrame, disc: bool, fresh: bool) {
        let s = &f.scope;
        if fresh && s.len() >= 16 {
            self.measure_sweep(f, disc);
        }
        if disc {
            for (lvl, name) in [(0.6, "+3"), (0.2, "+1"), (-0.2, "−1"), (-0.6, "−3")] {
                let y = r.center().y - lvl * (r.height() / 2.0 - 4.0);
                crate::spectrum::dashed_h(painter, r.left(), r.left() + r.width() * 0.76, y, alpha(theme::WARN, 0.35));
                painter.text(pos2(r.left() + r.width() * 0.77, y), Align2::LEFT_CENTER, name, theme::mono(9.0), alpha(theme::WARN, 0.8));
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
        painter.text(pos2(cx + 8.0, r.center().y - open_h - 2.0), Align2::LEFT_BOTTOM, format!("EYE {:.0}%", self.eye_open), theme::mono(9.5), theme::AMBER);
    }
}

fn level_color(levels: usize, k: usize) -> Color32 {
    if levels == 2 {
        [theme::OK, theme::ERR][k.min(1)]
    } else {
        [theme::CYAN, theme::OK, theme::WARN, theme::ERR][k.min(3)]
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
