//! The masthead's dial scale: the receiver's whole tuning range on a log
//! scale under glass, like the band dial of a shortwave set. Decades are
//! ticked and named, the band plan runs along its foot, the span on screen
//! is a lit window, and a needle stands at each receiver. A click or a drag
//! sets the needle and tunes there when it is let go.

use crate::app::App;
use crate::theme::{self, alpha};
use egui::{Color32, CornerRadius, Rect, Sense, Shape, Stroke, Ui, pos2, vec2};

/// Narrower than this and the scale says too little to be worth the room.
pub const MIN_W: f32 = 180.0;

pub fn show(app: &mut App, ui: &mut Ui, width: f32) {
    let s = &app.status;
    let (lo, hi) = if s.tune_max_hz > s.tune_min_hz { (s.tune_min_hz.max(1e3), s.tune_max_hz) } else { return };
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 24.0), Sense::click_and_drag());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Slider, true, "dial scale"));
    let (llo, lhi) = (lo.log10(), hi.log10());
    let x_of = |hz: f64| rect.left() + ((hz.max(lo).log10() - llo) / (lhi - llo)) as f32 * rect.width();
    let hz_at = |x: f32| 10f64.powf(llo + ((x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * (lhi - llo));
    let tint = theme::AMBER;
    let p = ui.painter_at(rect.expand(3.0));
    crate::vfo::lcd_back(&p, rect, tint);
    let inner = rect.shrink2(vec2(4.0, 2.0));
    let cp = p.with_clip_rect(inner);

    // The band plan along the foot, each band in its kind's color.
    let foot = inner.bottom() - 1.0;
    for b in crate::bandplan::BANDS {
        if b.hi_hz < lo || b.lo_hz > hi {
            continue;
        }
        let (x0, x1) = (x_of(b.lo_hz), x_of(b.hi_hz).max(x_of(b.lo_hz) + 1.5));
        cp.rect_filled(Rect::from_min_max(pos2(x0, foot - 3.0), pos2(x1, foot)), CornerRadius::ZERO, alpha(b.kind.color(), 0.55));
    }

    // Where the needles stand, so no decade's name sits under one.
    let needles: Vec<f32> = std::iter::once(app.inspect_hz()).chain(app.status.rx2.as_ref().map(|b| b.freq_hz)).map(x_of).collect();
    // Decade ticks, named; the 2–9 between them short.
    let text = theme::lerp(theme::text_2(), tint, 0.25);
    let mut d = 10f64.powf(llo.floor());
    let mut last_label = f32::NEG_INFINITY;
    while d <= hi * 1.0001 {
        for k in 1..10 {
            let hz = d * k as f64;
            if hz < lo || hz > hi {
                continue;
            }
            let x = x_of(hz);
            let major = k == 1;
            let h = if major { 9.0 } else if k == 5 { 5.0 } else { 3.0 };
            cp.line_segment([pos2(x, inner.top()), pos2(x, inner.top() + h)], Stroke::new(1.0, alpha(text, if major { 0.65 } else { 0.28 })));
            if major && x - last_label > 34.0 && x + 30.0 < inner.right() {
                let g = cp.layout_no_wrap(decade(hz), theme::mono_bold(crate::theme::ty::MICRO), text);
                let (l, r) = (x + 3.0, x + 3.0 + g.size().x);
                if !needles.iter().any(|n| *n > l - 5.0 && *n < r + 5.0) {
                    cp.galley(pos2(l, inner.top() + 1.0), g, alpha(text, 0.8));
                    last_label = x;
                }
            }
        }
        d *= 10.0;
    }

    // The span on screen: a lit window, at least a hairline wide.
    let pan = &app.pan;
    if pan.span_hz > 0.0 {
        let (x0, x1) = (x_of(pan.center_hz - pan.span_hz / 2.0), x_of(pan.center_hz + pan.span_hz / 2.0));
        let (x0, x1) = if x1 - x0 < 3.0 { ((x0 + x1) / 2.0 - 1.5, (x0 + x1) / 2.0 + 1.5) } else { (x0, x1) };
        let win = Rect::from_min_max(pos2(x0, inner.top()), pos2(x1, inner.bottom()));
        cp.rect_filled(win, CornerRadius::same(crate::theme::R_CELL), alpha(tint, 0.13));
        cp.rect_stroke(win, CornerRadius::same(crate::theme::R_CELL), Stroke::new(1.0, alpha(tint, 0.35)), egui::StrokeKind::Inside);
    }

    // A drag or click sets a needle that follows the pointer; it tunes on
    // release, so a sweep across the scale is one retune, not hundreds.
    // (Kept as a plain f64: egui's store is keyed by type as well as id.)
    let drag_id = ui.id().with("dial-drag");
    let mut preview: Option<f64> = ui.data(|d| d.get_temp::<f64>(drag_id));
    if (resp.dragged() || resp.is_pointer_button_down_on())
        && let Some(pp) = resp.interact_pointer_pos()
    {
        let hz = hz_at(pp.x);
        preview = Some(hz);
        ui.data_mut(|d| d.insert_temp(drag_id, hz));
    }
    if resp.drag_stopped() || resp.clicked() {
        // A click lands on the frame it is let go, where the press may
        // not have been seen: the pointer's own place then.
        let hz = preview.take().or_else(|| resp.interact_pointer_pos().map(|p| hz_at(p.x)));
        if let Some(hz) = hz {
            let step = crate::tuning::mode_raster_hz(app.status.mode, hz).max(if hz < 30e6 { 1e3 } else { 25e3 });
            app.tune(crate::tuning::snap(hz, step), false);
        }
        ui.data_mut(|d| d.remove::<f64>(drag_id));
    }

    // The needles: B in violet where it runs, then A (or the preview).
    let needle = |hz: f64, c: Color32| {
        let x = x_of(hz);
        p.line_segment([pos2(x, rect.top() + 1.0), pos2(x, rect.bottom() - 1.0)], Stroke::new(4.0, alpha(c, 0.16)));
        p.line_segment([pos2(x, rect.top() + 1.0), pos2(x, rect.bottom() - 1.0)], Stroke::new(1.5, c));
        p.add(Shape::convex_polygon(vec![pos2(x - 4.0, rect.top() + 1.0), pos2(x + 4.0, rect.top() + 1.0), pos2(x, rect.top() + 6.0)], c, Stroke::NONE));
    };
    // The preview belongs to whichever receiver a tune would move.
    let b_active = app.active_b && app.status.rx2.is_some();
    if let Some(b) = &app.status.rx2 {
        needle(if b_active { preview.unwrap_or(b.freq_hz) } else { b.freq_hz }, theme::VIOLET);
    }
    let a = app.inspect_hz();
    needle(if b_active { a } else { preview.unwrap_or(a) }, tint);
    crate::vfo::lcd_glass(&p, rect);

    if let Some(hp) = resp.hover_pos() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        let hz = hz_at(hp.x);
        let band = crate::bandplan::BANDS.iter().find(|b| hz >= b.lo_hz && hz <= b.hi_hz).map(|b| format!(" · {}", b.name)).unwrap_or_default();
        resp.on_hover_text_at_pointer(format!("{}{band} · click or drag to tune", crate::tuning::fmt_hz(hz)));
    }
}

/// A decade's name: 10k, 1M, 1G.
fn decade(hz: f64) -> String {
    match hz {
        h if h >= 1e9 => format!("{:.0}G", h / 1e9),
        h if h >= 1e6 => format!("{:.0}M", h / 1e6),
        h if h >= 1e3 => format!("{:.0}k", h / 1e3),
        h => format!("{h:.0}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn decades_are_named_short() {
        assert_eq!(super::decade(10e3), "10k");
        assert_eq!(super::decade(1e6), "1M");
        assert_eq!(super::decade(100e6), "100M");
        assert_eq!(super::decade(1e9), "1G");
    }
}
