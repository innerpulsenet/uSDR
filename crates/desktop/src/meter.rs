//! The S-meter, on the web page's relative scale: S9 is −60 dBFS, six dB an
//! S-unit below it, S1 at −108 and S9+60 at 0 dBFS. It reads dBFS, not dBm —
//! the receiver is not calibrated in absolute power, so the meter does not
//! pretend to be.
//!
//! Ballistics follow an analog needle: 45 ms up, 220 ms down, with a peak
//! marker that holds a second and then falls at 30% of the scale per second.

use crate::theme;
use egui::{CornerRadius, Rect, Sense, Stroke, Ui, pos2, vec2};

pub const S9_DBFS: f32 = -60.0;

/// The SNR's averaging per frame: about a second at the server's 25 frames
/// a second.
const SNR_EASE: f32 = 0.04;
/// Frames between refreshes of the shown SNR: twice a second.
const SNR_EVERY: u32 = 12;
/// A jump this large is a new signal, not noise on the old one.
const SNR_SNAP_DB: f32 = 10.0;

pub fn frac(dbfs: f32) -> f32 {
    ((dbfs + 108.0) / 108.0).clamp(0.0, 1.0)
}

pub fn s_label(dbfs: Option<f32>) -> String {
    let Some(d) = dbfs else { return "S—".into() };
    if d >= S9_DBFS {
        let over = (d - S9_DBFS).round();
        if over < 1.0 { "S9".into() } else { format!("S9+{over:.0}") }
    } else {
        let s = 9.0 + (d - S9_DBFS) / 6.0;
        if s < 1.0 { "S0".into() } else { format!("S{}", s.floor() as i32) }
    }
}

#[derive(Default)]
pub struct Meter {
    /// Exponentially smoothed channel and floor, dBFS.
    pub rf: Option<f32>,
    pub floor: Option<f32>,
    /// SNR as shown, dB: averaged over about a second and refreshed twice a
    /// second, so its tenths can be read. Frame to frame it wanders a decibel
    /// either way, which at 25 updates a second is a blur.
    pub snr: Option<f32>,
    snr_avg: Option<f32>,
    snr_frames: u32,
    pos: f32,
    peak: f32,
    peak_at: f64,
    pub clipping: bool,
}

impl Meter {
    pub fn feed(&mut self, channel_dbfs: f32, noise_dbfs: f32, peak_iq: f32) {
        if !channel_dbfs.is_finite() {
            return;
        }
        let rf = self.rf.get_or_insert(channel_dbfs);
        let k = if channel_dbfs > *rf { 0.85 } else { 0.48 };
        *rf += (channel_dbfs - *rf) * k;
        if noise_dbfs.is_finite() {
            let fl = self.floor.get_or_insert(noise_dbfs);
            *fl += (noise_dbfs - *fl) * 0.25;
            self.feed_snr(channel_dbfs - noise_dbfs);
        }
        self.clipping = peak_iq > 0.98;
    }

    /// One frame's SNR into the shown figure. A change of more than 10 dB
    /// (a retune, a carrier keying up) is taken at once rather than eased in.
    fn feed_snr(&mut self, raw: f32) {
        let avg = match self.snr_avg {
            Some(a) if (raw - a).abs() <= SNR_SNAP_DB => a + (raw - a) * SNR_EASE,
            _ => {
                self.snr_frames = SNR_EVERY;
                raw
            }
        };
        self.snr_avg = Some(avg);
        self.snr_frames += 1;
        if self.snr_frames >= SNR_EVERY {
            self.snr_frames = 0;
            self.snr = Some(avg);
        }
    }

    /// Advance the needle; returns its 0..1 position.
    pub fn tick(&mut self, now: f64, dt: f32, reduce_motion: bool) -> f32 {
        let target = self.rf.map(frac).unwrap_or(0.0);
        if reduce_motion {
            self.pos = target;
        } else {
            let tau = if target > self.pos { 0.045 } else { 0.22 };
            self.pos += (target - self.pos) * (1.0 - (-dt / tau).exp());
        }
        if self.pos >= self.peak {
            self.peak = self.pos;
            self.peak_at = now;
        } else if now - self.peak_at > 1.0 {
            self.peak = (self.peak - 0.3 * dt).max(self.pos);
        }
        self.pos
    }

    /// The S-units and dBFS for the bay's header, each with its color.
    pub fn readout(&self) -> ((String, egui::Color32), (String, egui::Color32)) {
        let over = self.rf.is_some_and(|d| d >= S9_DBFS);
        let s = (s_label(self.rf), if over { theme::AMBER } else { theme::text() });
        let d = if self.clipping {
            ("CLIP".to_string(), theme::ERR)
        } else {
            (self.rf.map_or("— dBFS".into(), |d| format!("{d:.1} dBFS")), theme::text_2())
        };
        (s, d)
    }

    /// The bargraph across the whole of its bay, in a recessed well: lit
    /// segments green to S9, amber to +40, ember past it; the floor caret,
    /// the peak pip and the scale inside the well under the segments.
    pub fn ui_bar(&mut self, ui: &mut Ui, reduce_motion: bool, mark: Option<f32>) {
        let now = ui.input(|i| i.time);
        let dt = ui.input(|i| i.stable_dt).clamp(0.001, 0.05);
        let pos = self.tick(now, dt, reduce_motion);
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(w, 24.0), Sense::hover());
        let p = ui.painter_at(rect.expand(3.0));
        theme::well(&p, rect, None);
        let bar = Rect::from_min_max(pos2(rect.left() + 6.0, rect.top() + 4.0), pos2(rect.right() - 6.0, rect.top() + 11.0));
        let lit_to = bar.left() + bar.width() * pos;
        let at = |db: f32| bar.left() + bar.width() * frac(db);
        let (s9_x, p40_x) = (at(S9_DBFS), at(-20.0));
        let mut x = bar.left();
        while x < bar.right() - 0.5 {
            let r = Rect::from_min_max(pos2(x, bar.top()), pos2((x + 2.0).min(bar.right()), bar.bottom()));
            let c = if x < s9_x {
                theme::OK
            } else if x < p40_x {
                theme::AMBER
            } else {
                theme::EMBER
            };
            p.rect_filled(r, CornerRadius::ZERO, if x < lit_to { c } else { theme::alpha(c, 0.10) });
            x += 3.0;
        }
        if let Some(fl) = self.floor {
            let fx = at(fl);
            p.line_segment([pos2(fx, bar.top() - 2.0), pos2(fx, bar.bottom() + 2.0)], Stroke::new(1.5, theme::CYAN));
        }
        if self.peak > pos + 0.008 {
            let px = bar.left() + bar.width() * self.peak;
            p.line_segment([pos2(px, bar.top() - 1.0), pos2(px, bar.bottom() + 1.0)], Stroke::new(2.0, if self.clipping { theme::ERR } else { theme::text() }));
        }
        if let Some(m) = mark {
            let mx = at(m);
            let open = self.rf.is_some_and(|r| r >= m);
            let tri = vec![pos2(mx, bar.top()), pos2(mx - 3.5, rect.top() - 1.0), pos2(mx + 3.5, rect.top() - 1.0)];
            if open {
                p.add(egui::Shape::convex_polygon(tri, theme::ERR, Stroke::NONE));
            } else {
                p.add(egui::Shape::closed_line(tri, Stroke::new(1.0, theme::ERR)));
            }
        }
        // The scale under the segments, each label on a tick; a label that
        // would run into the last one drawn is left out, so a narrow meter
        // thins its scale rather than overprinting.
        let mut last_right = f32::NEG_INFINITY;
        for (label, db, hi) in [("S1", -108.0, false), ("3", -96.0, false), ("5", -84.0, false), ("7", -72.0, false), ("S9", -60.0, false), ("+20", -40.0, true), ("+40", -20.0, true), ("+60", 0.0, true)] {
            let tx = at(db);
            // The end labels sit inside the ends; only the inner ones get a tick.
            if db > -108.0 && db < 0.0 {
                p.line_segment([pos2(tx, bar.bottom() + 1.0), pos2(tx, bar.bottom() + 3.0)], Stroke::new(1.0, theme::text_3()));
            }
            let g = p.layout_no_wrap(label.to_string(), theme::mono(10.0), if hi { theme::alpha(theme::AMBER, 0.8) } else { theme::text_3() });
            let x0 = if db <= -108.0 { tx } else if db >= 0.0 { tx - g.size().x } else { tx - g.size().x / 2.0 };
            if x0 < last_right + 4.0 {
                continue;
            }
            last_right = x0 + g.size().x;
            p.galley(pos2(x0, rect.bottom() - g.size().y), g, theme::text_3());
        }
        if !reduce_motion && (pos - self.rf.map(frac).unwrap_or(0.0)).abs() > 0.002 {
            crate::anim::more(ui.ctx());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_snr_holds_still_long_enough_to_read() {
        let mut m = Meter::default();
        // ±1 dB of frame-to-frame scatter around 22 dB.
        let mut shown = Vec::new();
        for i in 0..250 {
            let jitter = if i % 2 == 0 { 1.0 } else { -1.0 } * ((i * 7 % 10) as f32 / 10.0);
            m.feed(-40.0 + jitter, -62.0, 0.1);
            shown.push(m.snr.unwrap());
        }
        // It changes at most twice a second (25 frames a second)...
        let changes = shown.windows(2).filter(|w| w[0] != w[1]).count();
        assert!(changes <= 21, "{changes} changes in 10 s");
        // ...and sits close to the mean once settled.
        assert!(shown[100..].iter().all(|s| (s - 22.0).abs() < 0.4), "{:?}", &shown[100..110]);
        // A retune to a much stronger signal shows at the next refresh.
        m.feed(-20.0, -62.0, 0.1);
        assert!((m.snr.unwrap() - 42.0).abs() < 0.01);
    }

    #[test]
    fn s_units() {
        assert_eq!(s_label(Some(-60.0)), "S9");
        assert_eq!(s_label(Some(-40.0)), "S9+20");
        assert_eq!(s_label(Some(-66.0)), "S8");
        assert_eq!(s_label(Some(-120.0)), "S0");
        assert_eq!(s_label(None), "S—");
    }
}
