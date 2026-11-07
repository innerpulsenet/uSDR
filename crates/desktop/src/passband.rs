//! The filter's passband, drawn and handled under the readout's glass: the
//! audio band around the carrier, the passband as its filter actually
//! falls away (each shape's skirt to scale), the carrier, and the edges.
//! Drag an edge to move it, drag the band to slide it, wheel to widen or
//! narrow it; a symmetric filter's edges move together.

use crate::theme::{self, alpha};
use egui::{Align2, Color32, Rect, Sense, Shape, Stroke, Ui, pos2, vec2};
use usdr_proto::{FilterRange, FilterShape};

/// What a drag took hold of.
#[derive(Clone, Copy, PartialEq)]
enum Grip {
    Low,
    High,
    Band,
}

/// How far the skirt runs past an edge, in Hz: the server's figures for a
/// shaped sideband filter, a small share of the width for the rest.
fn skirt_hz(shape: Option<FilterShape>, width: f32) -> f32 {
    match shape {
        Some(FilterShape::Sharp) => 100.0,
        Some(FilterShape::Normal) => 300.0,
        Some(FilterShape::Soft) => 400.0,
        None => (width * 0.06).max(50.0),
    }
}

/// Draw the passband `lo`..`hi` (Hz from the carrier) and return new edges
/// when the operator moved them. `shape` is `Some` for a shaped filter;
/// `(edges, width)` are read at the glass's right end; `around(width)`
/// gives the edges a width keeps (the wheel's).
#[allow(clippy::too_many_arguments)]
pub fn show(ui: &mut Ui, lo: f32, hi: f32, range: &FilterRange, shape: Option<FilterShape>, tint: Color32, (edges, width): (&str, &str), around: &dyn Fn(f32) -> (f32, f32)) -> Option<(f32, f32)> {
    let (edge_font, width_font) = (theme::mono(9.0), theme::mono_bold(11.0));
    let tw = |t: &str, f: &egui::FontId| ui.painter().layout_no_wrap(t.into(), f.clone(), Color32::PLACEHOLDER).size().x;
    let rw = tw(edges, &edge_font).max(tw(width, &width_font)).max(30.0);
    let w = ui.available_width();
    let (glass, resp) = ui.allocate_exact_size(vec2(w, 24.0), Sense::click_and_drag());
    resp.widget_info(|| egui::WidgetInfo::slider(true, (hi - lo) as f64, "WIDTH"));
    let outer = glass;
    let p = ui.painter_at(outer.expand(3.0));
    crate::vfo::lcd_back(&p, glass, tint);
    // The band to the left; the readout in the glass's right end, apart.
    let read = Rect::from_min_max(pos2(glass.right() - rw - 10.0, glass.top()), glass.max);
    let plot = Rect::from_min_max(pos2(glass.left() + 6.0, glass.top() + 3.0), pos2(read.left() - 4.0, glass.bottom() - 3.0));

    // The axis: the audio band either side of the carrier, or one side of
    // it for a sideband filter.
    let m = range.max_edge_hz.max(hi.abs()).max(lo.abs()).max(1.0);
    let (a0, a1) = if range.symmetric { (-m, m) } else if hi <= 0.0 { (-m, 0.0) } else { (0.0, m) };
    let x_of = |hz: f32| plot.left() + (hz - a0) / (a1 - a0) * plot.width();
    let hz_at = |x: f32| a0 + (x - plot.left()) / plot.width() * (a1 - a0);
    let cp = p.with_clip_rect(plot);

    // Ticks every kHz (every 5 or 10 on a broadcast filter), under the band.
    let step = if a1 - a0 > 60_000.0 { 10_000.0 } else if a1 - a0 > 15_000.0 { 5_000.0 } else { 1_000.0 };
    let mut t = (a0 / step).ceil() * step;
    while t <= a1 {
        let x = x_of(t);
        cp.line_segment([pos2(x, plot.bottom() - 3.0), pos2(x, plot.bottom())], Stroke::new(1.0, alpha(theme::text_3(), 0.5)));
        t += step;
    }

    // The interaction, before the band is drawn, so it draws where it goes.
    let grip_id = resp.id.with("grip");
    let px_hz = (a1 - a0) / plot.width().max(1.0);
    let snap = if range.max_width_hz > 50_000.0 { 1_000.0 } else { 50.0 };
    let near = |x: f32, hz: f32| (x - x_of(hz)).abs() <= 6.0;
    let grip_at = |x: f32| {
        if near(x, lo) && (!near(x, hi) || x < x_of(lo)) {
            Some(Grip::Low)
        } else if near(x, hi) {
            Some(Grip::High)
        } else if x > x_of(lo) && x < x_of(hi) {
            Some(Grip::Band)
        } else {
            None
        }
    };
    let hover = resp.hover_pos().and_then(|p| grip_at(p.x));
    if resp.drag_started()
        && let Some(pp) = resp.interact_pointer_pos()
    {
        let g = grip_at(pp.x).unwrap_or(if pp.x < x_of(lo) { Grip::Low } else { Grip::High });
        ui.data_mut(|d| d.insert_temp(grip_id, (g == Grip::Low) as u8 | ((g == Grip::High) as u8) << 1));
    }
    let held: Option<Grip> = ui.data(|d| d.get_temp::<u8>(grip_id)).map(|b| match b {
        1 => Grip::Low,
        2 => Grip::High,
        _ => Grip::Band,
    });
    if resp.drag_stopped() {
        ui.data_mut(|d| d.remove::<u8>(grip_id));
    }
    let clamp = |l: f32, h: f32| -> (f32, f32) {
        let (mut l, mut h) = ((l / snap).round() * snap, (h / snap).round() * snap);
        // A sideband filter stays on its side of the carrier.
        if !range.symmetric {
            if a0 >= 0.0 {
                l = l.max(0.0);
            } else {
                h = h.min(0.0);
            }
        }
        l = l.max(-range.max_edge_hz);
        h = h.min(range.max_edge_hz);
        (l, h)
    };
    let fits = |l: f32, h: f32| h - l >= range.min_width_hz - 0.5 && h - l <= range.max_width_hz + 0.5;
    let mut out = None;
    if resp.dragged()
        && let (Some(g), Some(pp)) = (held, resp.interact_pointer_pos())
    {
        let at = hz_at(pp.x);
        let (l, h) = match g {
            Grip::Low if range.symmetric => (-(-at).max(0.0), (-at).max(0.0)),
            Grip::High if range.symmetric => (-at.max(0.0), at.max(0.0)),
            Grip::Low => (at, hi),
            Grip::High => (lo, at),
            Grip::Band => {
                let d = resp.drag_delta().x * px_hz;
                (lo + d, hi + d)
            }
        };
        let (l, h) = if g == Grip::Band && !range.symmetric {
            // Sliding keeps the width: stop at the carrier or the far edge.
            let wdt = hi - lo;
            let l = l.clamp(if a0 >= 0.0 { 0.0 } else { -range.max_edge_hz }, if a0 >= 0.0 { range.max_edge_hz - wdt } else { -wdt });
            clamp(l, l + wdt)
        } else if g == Grip::Band {
            (lo, hi)
        } else {
            clamp(l, h)
        };
        if fits(l, h) && ((l - lo).abs() >= 1.0 || (h - hi).abs() >= 1.0) {
            out = Some((l, h));
        }
    }
    if resp.hovered() {
        let n = crate::widgets::wheel(ui);
        if n != 0.0 {
            crate::widgets::eat_scroll(ui);
            let wstep = if range.max_width_hz > 50_000.0 { 5_000.0 } else { 100.0 };
            let target = ((hi - lo) + n.signum() * wstep).clamp(range.min_width_hz, range.max_width_hz);
            let (l, h) = around(target);
            let (l, h) = clamp(l, h);
            if fits(l, h) {
                out = Some((l, h));
            }
        }
        ui.ctx().set_cursor_icon(match hover.or(held) {
            Some(Grip::Low | Grip::High) => egui::CursorIcon::ResizeHorizontal,
            Some(Grip::Band) if !range.symmetric => egui::CursorIcon::Grab,
            _ => egui::CursorIcon::Default,
        });
    }
    let (lo, hi) = out.unwrap_or((lo, hi));

    // The band: flat across the passband, falling away over its skirt.
    let skirt = skirt_hz(shape, hi - lo) / px_hz;
    let (xl, xh) = (x_of(lo), x_of(hi));
    let (top, foot) = (plot.top() + 2.0, plot.bottom() - 1.0);
    // A shape's curve: sharp falls nearly straight down, soft eases out.
    let ease = match shape {
        Some(FilterShape::Sharp) => 0.15,
        Some(FilterShape::Soft) => 0.8,
        _ => 0.45,
    };
    let mut outline = Vec::new();
    let n = 10;
    for i in 0..=n {
        let t = i as f32 / n as f32;
        let fall = t.powf(1.0 + ease * 2.0);
        outline.push(pos2(xl - skirt * t, top + (foot - top) * fall));
    }
    outline.reverse();
    for i in 0..=n {
        let t = i as f32 / n as f32;
        let fall = t.powf(1.0 + ease * 2.0);
        outline.push(pos2(xh + skirt * t, top + (foot - top) * fall));
    }
    // Filled as a fan of thin quads from the foot, so the curve is honored.
    let mut mesh = egui::Mesh::default();
    for pt in &outline {
        mesh.colored_vertex(*pt, alpha(tint, 0.42));
        mesh.colored_vertex(pos2(pt.x, foot), alpha(tint, 0.08));
    }
    for i in 0..outline.len() as u32 - 1 {
        let (a, b, c, d) = (2 * i, 2 * i + 1, 2 * i + 2, 2 * i + 3);
        mesh.add_triangle(a, b, c);
        mesh.add_triangle(c, b, d);
    }
    cp.add(Shape::mesh(mesh));
    cp.add(Shape::line(outline.clone(), Stroke::new(1.25, alpha(tint, 0.95))));
    // The carrier.
    if a0 <= 0.0 && a1 >= 0.0 {
        crate::spectrum::dashed_v(&cp, x_of(0.0), plot.top(), plot.bottom(), alpha(theme::text(), 0.45));
    }
    // The edges, lit when they are the ones under the pointer or in hand.
    for (x, g) in [(xl, Grip::Low), (xh, Grip::High)] {
        let on = held == Some(g) || hover == Some(g);
        cp.line_segment([pos2(x, top - 1.0), pos2(x, foot)], Stroke::new(if on { 2.0 } else { 1.25 }, if on { theme::text() } else { alpha(tint, 0.95) }));
    }
    let lit = theme::lerp(theme::text(), tint, 0.28);
    p.line_segment([pos2(read.left(), glass.top() + 4.0), pos2(read.left(), glass.bottom() - 4.0)], Stroke::new(1.0, alpha(lit, 0.12)));
    p.text(pos2(read.right() - 6.0, glass.top() + 2.0), Align2::RIGHT_TOP, edges, edge_font, alpha(lit, 0.55));
    p.text(pos2(read.right() - 6.0, glass.bottom() - 1.0), Align2::RIGHT_BOTTOM, width, width_font, lit);
    crate::vfo::lcd_glass(&p, glass);
    let tip = if range.symmetric { "Drag an edge to set the width (both move together) · wheel to widen or narrow" } else { "Drag an edge to move it, or the band to slide it · wheel to widen or narrow" };
    resp.on_hover_text(tip);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_shape_has_its_servers_skirt() {
        assert_eq!(skirt_hz(Some(FilterShape::Sharp), 2_400.0), 100.0);
        assert_eq!(skirt_hz(Some(FilterShape::Normal), 2_400.0), 300.0);
        assert_eq!(skirt_hz(Some(FilterShape::Soft), 2_400.0), 400.0);
        assert_eq!(skirt_hz(None, 12_500.0), 750.0);
    }
}
