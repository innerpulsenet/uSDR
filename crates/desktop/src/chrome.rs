//! The window's own frame, for the two options that replace the system's:
//! an integrated title bar (the masthead is the title bar: it moves the
//! window, a double-click on it maximizes, and it carries the minimize,
//! maximize and close keys), and on Windows 11 the Mica backdrop showing
//! through the chrome. Both are read once at startup.

use crate::theme::{self, alpha};
use egui::{CornerRadius, Pos2, Rect, ResizeDirection, Sense, Stroke, Ui, ViewportCommand, pos2, vec2};

/// What the window was opened with.
#[derive(Clone, Copy, Default)]
pub struct Chrome {
    /// No system title bar or frame: the app draws its own.
    pub own_titlebar: bool,
    /// Mica is behind the window, and the chrome lets it through.
    pub mica: bool,
}

impl Chrome {
    /// The window's plate: solid, or with Mica, translucent over it.
    pub fn plate_fill(&self) -> egui::Color32 {
        if self.mica { alpha(theme::plate(), 0.62) } else { theme::plate() }
    }
}

/// Put Mica behind the window, where Windows 11 has it. Returns whether
/// it took; anywhere else it is never asked for.
#[cfg(windows)]
pub fn apply_mica(cc: &eframe::CreationContext<'_>) -> bool {
    match window_vibrancy::apply_mica(cc, Some(true)) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("Mica backdrop: {e}");
            false
        }
    }
}

#[cfg(not(windows))]
pub fn apply_mica(_cc: &eframe::CreationContext<'_>) -> bool {
    false
}

/// The masthead as the title bar: dragging its empty parts moves the
/// window and a double-click maximizes or restores it. Call before laying
/// out the masthead's contents, which take their own clicks first.
pub fn title_drag(ui: &mut Ui, rect: Rect) {
    let resp = ui.interact(rect, ui.id().with("titlebar"), Sense::click_and_drag());
    let ctx = ui.ctx();
    if resp.drag_started() {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }
    if resp.double_clicked() {
        let max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        ctx.send_viewport_cmd(ViewportCommand::Maximized(!max));
    }
}

/// Minimize, maximize and close, right to left from the masthead's end.
pub fn window_keys(ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    let max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
    if window_key(ui, Glyph::Close, "Close", true).clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }
    if window_key(ui, if max { Glyph::Restore } else { Glyph::Maximize }, if max { "Restore" } else { "Maximize" }, false).clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Maximized(!max));
    }
    if window_key(ui, Glyph::Minimize, "Minimize", false).clicked() {
        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Glyph {
    Minimize,
    Maximize,
    Restore,
    Close,
}

/// A window key: a drawn glyph on a plate that lights on hover, ember for
/// close (the one destructive key).
fn window_key(ui: &mut Ui, glyph: Glyph, tip: &str, danger: bool) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(30.0, 26.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tip));
    let (_, label) = crate::widgets::paint_key(ui, &resp, r, CornerRadius::same(theme::RADIUS), false, theme::AMBER);
    let h = crate::anim::ease_bool(ui.ctx(), resp.id.with("danger"), danger && resp.hovered(), crate::anim::HOVER_S);
    let p = ui.painter();
    if h > 0.0 {
        p.rect_filled(r, CornerRadius::same(theme::RADIUS), alpha(theme::ACTION, 0.9 * h));
    }
    let c = theme::lerp(label, egui::Color32::WHITE, h);
    let s = Stroke::new(1.2, c);
    let m = r.center();
    let d = 4.5;
    match glyph {
        Glyph::Minimize => {
            p.line_segment([pos2(m.x - d, m.y + 3.0), pos2(m.x + d, m.y + 3.0)], s);
        }
        Glyph::Maximize => {
            p.rect_stroke(Rect::from_center_size(m, vec2(2.0 * d, 2.0 * d)), CornerRadius::same(1), s, egui::StrokeKind::Middle);
        }
        Glyph::Restore => {
            let back = Rect::from_center_size(m + vec2(1.5, -1.5), vec2(2.0 * d - 2.0, 2.0 * d - 2.0));
            let front = Rect::from_center_size(m + vec2(-1.5, 1.5), vec2(2.0 * d - 2.0, 2.0 * d - 2.0));
            p.line_segment([pos2(back.left(), front.top()), back.left_top()], s);
            p.line_segment([back.left_top(), back.right_top()], s);
            p.line_segment([back.right_top(), back.right_bottom()], s);
            p.line_segment([back.right_bottom(), pos2(front.right(), back.bottom())], s);
            p.rect_stroke(front, CornerRadius::same(1), s, egui::StrokeKind::Middle);
        }
        Glyph::Close => {
            p.line_segment([m + vec2(-d, -d), m + vec2(d, d)], s);
            p.line_segment([m + vec2(d, -d), m + vec2(-d, d)], s);
        }
    }
    resp.on_hover_text(tip)
}

/// Without a system frame, the window's edges resize it: the pointer near
/// an edge or corner shows the resize cursor and a press there hands the
/// drag to the window manager. Not while maximized or full screen.
pub fn resize_edges(ctx: &egui::Context) {
    let (max, full) = ctx.input(|i| (i.viewport().maximized.unwrap_or(false), i.viewport().fullscreen.unwrap_or(false)));
    if max || full {
        return;
    }
    let screen = ctx.viewport_rect();
    let Some(p) = ctx.input(|i| i.pointer.hover_pos()) else { return };
    let Some(dir) = edge_at(screen, p, 5.0) else { return };
    ctx.set_cursor_icon(match dir {
        ResizeDirection::North | ResizeDirection::South => egui::CursorIcon::ResizeVertical,
        ResizeDirection::East | ResizeDirection::West => egui::CursorIcon::ResizeHorizontal,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => egui::CursorIcon::ResizeNwSe,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => egui::CursorIcon::ResizeNeSw,
    });
    if ctx.input(|i| i.pointer.primary_pressed()) {
        ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
    }
}

/// Which edge or corner of `screen` `p` is within `band` of, corners
/// reaching three bands along each side.
fn edge_at(screen: Rect, p: Pos2, band: f32) -> Option<ResizeDirection> {
    let corner = band * 3.0;
    let (l, r, t, b) = (p.x - screen.left() < band, screen.right() - p.x < band, p.y - screen.top() < band, screen.bottom() - p.y < band);
    let (lc, rc, tc, bc) = (p.x - screen.left() < corner, screen.right() - p.x < corner, p.y - screen.top() < corner, screen.bottom() - p.y < corner);
    use ResizeDirection::*;
    Some(match () {
        _ if (t && lc) || (l && tc) => NorthWest,
        _ if (t && rc) || (r && tc) => NorthEast,
        _ if (b && lc) || (l && bc) => SouthWest,
        _ if (b && rc) || (r && bc) => SouthEast,
        _ if t => North,
        _ if b => South,
        _ if l => West,
        _ if r => East,
        _ => return None,
    })
}

/// A hairline around the whole window, where the system frame would be.
pub fn outline(ctx: &egui::Context) {
    let r = ctx.viewport_rect();
    let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("window-outline")));
    p.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0, theme::line_2()), egui::StrokeKind::Inside);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_and_corners_resize_their_way() {
        let s = Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 800.0));
        assert!(matches!(edge_at(s, pos2(500.0, 2.0), 5.0), Some(ResizeDirection::North)));
        assert!(matches!(edge_at(s, pos2(998.0, 400.0), 5.0), Some(ResizeDirection::East)));
        assert!(matches!(edge_at(s, pos2(2.0, 798.0), 5.0), Some(ResizeDirection::SouthWest)));
        assert!(matches!(edge_at(s, pos2(10.0, 2.0), 5.0), Some(ResizeDirection::NorthWest)));
        assert!(edge_at(s, pos2(500.0, 400.0), 5.0).is_none());
    }
}
