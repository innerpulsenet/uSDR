//! The S-meter, on the web page's relative scale: S9 is −60 dBFS, six dB an
//! S-unit below it, S1 at −108 and S9+60 at 0 dBFS. It reads dBFS, not dBm —
//! the receiver is not calibrated in absolute power, so the meter does not
//! pretend to be.
//!
//! Ballistics follow an analog needle: 45 ms up, 220 ms down, with a peak
//! marker that holds a second and then falls at 30% of the scale per second.

use crate::theme;
use egui::{Align2, CornerRadius, Rect, Sense, Stroke, Ui, pos2, vec2};

pub const S9_DBFS: f32 = -60.0;

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
    pub snr: Option<f32>,
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
            self.snr = Some(channel_dbfs - noise_dbfs);
        }
        self.clipping = peak_iq > 0.98;
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

    /// The meter on two short lines: S-units, the bar and dBFS on one, the
    /// scale under the bar.
    pub fn ui_compact(&mut self, ui: &mut Ui, reduce_motion: bool, mark: Option<f32>) {
        let now = ui.input(|i| i.time);
        let dt = ui.input(|i| i.stable_dt).clamp(0.001, 0.05);
        let pos = self.tick(now, dt, reduce_motion);
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(w, 32.0), Sense::hover());
        let p = ui.painter_at(rect.expand(2.0));
        let s = s_label(self.rf);
        let over = self.rf.is_some_and(|d| d >= S9_DBFS);
        p.text(pos2(rect.left(), rect.top() + 9.0), Align2::LEFT_CENTER, &s, theme::mono_bold(16.0), if over { theme::WARN } else { theme::text() });
        let dbfs = self.rf.map_or("— dBFS".into(), |d| format!("{d:.1} dBFS"));
        let dc = if self.clipping { theme::ERR } else { theme::text_2() };
        p.text(pos2(rect.right(), rect.top() + 9.0), Align2::RIGHT_CENTER, if self.clipping { "CLIP".to_string() } else { dbfs }, theme::mono(11.0), dc);
        let bar = Rect::from_min_max(pos2(rect.left() + 76.0, rect.top() + 4.0), pos2(rect.right() - 82.0, rect.top() + 14.0));
        p.rect_filled(bar, CornerRadius::same(2), theme::ink());
        let lit_to = bar.left() + bar.width() * pos;
        let s9_x = bar.left() + bar.width() * frac(S9_DBFS);
        let mut x = bar.left() + 1.0;
        while x < bar.right() - 1.0 {
            let r = Rect::from_min_max(pos2(x, bar.top() + 2.0), pos2((x + 2.0).min(bar.right() - 1.0), bar.bottom() - 2.0));
            let c = if x < lit_to {
                if x < s9_x { theme::lerp(theme::OK, theme::AMBER, (x - bar.left()) / (s9_x - bar.left()).max(1.0) * 0.4) } else { theme::lerp(theme::AMBER, theme::EMBER, (x - s9_x) / (bar.right() - s9_x).max(1.0)) }
            } else {
                theme::surface_3()
            };
            p.rect_filled(r, CornerRadius::ZERO, c);
            x += 3.0;
        }
        if let Some(fl) = self.floor {
            let fx = bar.left() + bar.width() * frac(fl);
            p.line_segment([pos2(fx, bar.top() - 2.0), pos2(fx, bar.bottom() + 2.0)], Stroke::new(1.5, theme::text_3()));
        }
        if self.peak > pos + 0.008 {
            let px = bar.left() + bar.width() * self.peak;
            p.line_segment([pos2(px, bar.top()), pos2(px, bar.bottom())], Stroke::new(2.0, if self.clipping { theme::ERR } else { theme::text() }));
        }
        if let Some(m) = mark {
            let mx = bar.left() + bar.width() * frac(m);
            let open = self.rf.is_some_and(|r| r >= m);
            let tri = vec![pos2(mx, bar.top()), pos2(mx - 3.5, bar.top() - 5.0), pos2(mx + 3.5, bar.top() - 5.0)];
            if open {
                p.add(egui::Shape::convex_polygon(tri, theme::ERR, Stroke::NONE));
            } else {
                p.add(egui::Shape::closed_line(tri, Stroke::new(1.0, theme::ERR)));
            }
        }
        for (label, db, hi) in [("1", -108.0, false), ("3", -96.0, false), ("5", -84.0, false), ("7", -72.0, false), ("9", -60.0, false), ("+20", -40.0, true), ("+40", -20.0, true), ("+60", 0.0, true)] {
            let tx = bar.left() + bar.width() * frac(db);
            let align = if db <= -108.0 { Align2::LEFT_TOP } else if db >= 0.0 { Align2::RIGHT_TOP } else { Align2::CENTER_TOP };
            p.text(pos2(tx, bar.bottom() + 3.0), align, label, theme::mono(9.0), if hi { theme::WARN } else { theme::text_3() });
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
    fn s_units() {
        assert_eq!(s_label(Some(-60.0)), "S9");
        assert_eq!(s_label(Some(-40.0)), "S9+20");
        assert_eq!(s_label(Some(-66.0)), "S8");
        assert_eq!(s_label(Some(-120.0)), "S0");
        assert_eq!(s_label(None), "S—");
    }
}
