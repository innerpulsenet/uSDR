//! The AIS workspace in the dock: every vessel heard, on a chart around
//! this station and in a table beside it.
//!
//! A vessel is put together from all its reports, newest first: where it
//! is and how it moves from its position reports, its name, call sign,
//! type and voyage from its static ones. The chart is a plot around this
//! station (its grid square's center) or, with no station set, around the
//! vessels: range rings in nautical miles and north up, each ship a hull
//! pointing along its heading with a line to where it will be in six
//! minutes. There is no coastline: the one the app carries (1:110m) is
//! miles out at this scale, and a wrong shore is worse than none.

use crate::app::{App, local_hms};
use crate::theme::{self, alpha};
use crate::widgets;
use egui::{Align2, Color32, CornerRadius, Rect, RichText, Sense, Shape, Stroke, Ui, pos2, vec2};
use std::time::SystemTime;

/// One vessel (or base station, aid or aircraft), from all its reports.
#[derive(Clone, Default)]
struct Vessel {
    mmsi: String,
    name: Option<String>,
    callsign: Option<String>,
    kind: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    sog: Option<f32>,
    cog: Option<f32>,
    heading: Option<f32>,
    status: Option<String>,
    destination: Option<String>,
    length: Option<String>,
    /// Message types heard, to tell a class B, an aid or a base station.
    types: Vec<u8>,
    last: Option<SystemTime>,
    reports: usize,
}

impl Vessel {
    fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.mmsi.clone())
    }
    /// What it is, by what it sends: an aid, a base station, a SAR
    /// aircraft, a class B set, else a ship.
    fn role(&self) -> Role {
        if self.types.contains(&21) {
            Role::Aid
        } else if self.types.contains(&4) {
            Role::Base
        } else if self.types.contains(&9) {
            Role::Aircraft
        } else if self.types.iter().any(|t| matches!(t, 18 | 19 | 24)) {
            Role::ClassB
        } else {
            Role::Ship
        }
    }
    fn age_s(&self) -> f32 {
        self.last.and_then(|t| t.elapsed().ok()).map_or(f32::INFINITY, |e| e.as_secs_f32())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Ship,
    ClassB,
    Aid,
    Base,
    Aircraft,
}

impl Role {
    fn color(self) -> Color32 {
        match self {
            Role::Ship => widgets::protocol_color("AIS"),
            Role::ClassB => theme::VIOLET,
            Role::Aid => theme::AMBER,
            Role::Base => theme::text_2(),
            Role::Aircraft => theme::WARN,
        }
    }
    fn word(self) -> &'static str {
        match self {
            Role::Ship => "class A",
            Role::ClassB => "class B",
            Role::Aid => "aid to navigation",
            Role::Base => "base station",
            Role::Aircraft => "SAR aircraft",
        }
    }
}

/// A vessel's name, from whichever of its reports carried one.
pub fn name_of(app: &App, mmsi: &str) -> Option<String> {
    app.decodes.iter().filter(|d| d.ev.protocol == "AIS" && d.ev.fields.get("mmsi").is_some_and(|m| m == mmsi)).find_map(|d| d.ev.fields.get("name").cloned())
}

/// Every vessel in the log, most recently heard first.
fn vessels(app: &App) -> Vec<Vessel> {
    let mut out: Vec<Vessel> = Vec::new();
    let mut at: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for d in app.decodes.iter().filter(|d| d.ev.protocol == "AIS") {
        let f = &d.ev.fields;
        let Some(mmsi) = f.get("mmsi").cloned() else { continue };
        let i = *at.entry(mmsi.clone()).or_insert_with(|| {
            out.push(Vessel { mmsi: mmsi.clone(), last: Some(d.at), ..Default::default() });
            out.len() - 1
        });
        let v = &mut out[i];
        v.reports += 1;
        let num = |k: &str| f.get(k).and_then(|s| s.parse::<f64>().ok());
        let text = |k: &str| f.get(k).cloned();
        // Newest first: a field is taken from the newest report that has it.
        if v.lat.is_none()
            && let (Some(la), Some(lo)) = (num("lat"), num("lon"))
        {
            (v.lat, v.lon) = (Some(la), Some(lo));
            v.sog = v.sog.or(num("sog").map(|x| x as f32));
            v.cog = v.cog.or(num("cog").map(|x| x as f32));
            v.heading = v.heading.or(num("heading").map(|x| x as f32));
        }
        v.name = v.name.take().or_else(|| text("name"));
        v.callsign = v.callsign.take().or_else(|| text("callsign"));
        v.kind = v.kind.take().or_else(|| text("shipKind"));
        v.status = v.status.take().or_else(|| text("status"));
        v.destination = v.destination.take().or_else(|| text("destination"));
        v.length = v.length.take().or_else(|| text("length"));
        if let Some(t) = f.get("type").and_then(|t| t.parse::<u8>().ok())
            && !v.types.contains(&t)
        {
            v.types.push(t);
        }
    }
    out
}

/// Nautical miles east and north of `c`, on a plane: fine at a few
/// tens of miles.
fn enu(c: (f64, f64), lat: f64, lon: f64) -> (f32, f32) {
    let x = (lon - c.1) * c.0.to_radians().cos() * 60.0;
    let y = (lat - c.0) * 60.0;
    (x as f32, y as f32)
}

/// Range and true bearing from `c`, in nm and degrees.
fn range_bearing(c: (f64, f64), lat: f64, lon: f64) -> (f32, f32) {
    let (x, y) = enu(c, lat, lon);
    ((x * x + y * y).sqrt(), (x.atan2(y).to_degrees() + 360.0) % 360.0)
}

/// A ring spacing that gives two to five rings out to `r` nm.
fn ring_step(r: f32) -> f32 {
    [0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0].into_iter().find(|s| r / s <= 5.0).unwrap_or(200.0)
}

pub fn show(app: &mut App, ui: &mut Ui) {
    let all = vessels(app);
    let here = app.station.as_ref().and_then(|(st, _)| usdr_proto::station::grid_center(&st.grid));
    let placed: Vec<&Vessel> = all.iter().filter(|v| v.lat.is_some()).collect();
    let center = here.or_else(|| {
        (!placed.is_empty()).then(|| {
            let n = placed.len() as f64;
            (placed.iter().map(|v| v.lat.unwrap()).sum::<f64>() / n, placed.iter().map(|v| v.lon.unwrap()).sum::<f64>() / n)
        })
    });
    let sel_id = egui::Id::new("ais-selected");
    let mut selected: Option<String> = ui.data(|d| d.get_temp(sel_id));

    // The head line: what is heard, and whether the span holds the channels.
    let span = app.status.rate_hz.max(1.0);
    let hears = (161.96e6..=162.04e6).contains(&app.status.freq_hz) || (app.status.freq_hz - 162.0e6).abs() < span * 0.45 - 40e3;
    let recent = all.iter().filter(|v| v.age_s() < 600.0).count();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        ui.label(RichText::new(format!("{} vessels · {} in the last 10 min · {} reports", all.len(), recent, all.iter().map(|v| v.reports).sum::<usize>())).font(theme::mono(11.0)).color(theme::text_2()));
        if !hears {
            ui.label(RichText::new("the span does not reach 161.975–162.025 MHz").font(theme::mono(11.0)).color(theme::WARN));
            if widgets::chip_key(ui, "TUNE 162.000", false, theme::CYAN).on_hover_text("Put both AIS channels in the span").clicked() {
                app.tune(162.0e6, false);
            }
        }
    });
    ui.add_space(4.0);

    let avail = ui.available_size();
    let chart_w = (avail.x * 0.5).clamp(260.0, avail.y.max(260.0) * 1.6).min(avail.x - 300.0).max(200.0);
    ui.horizontal_top(|ui| {
        let (rect, resp) = ui.allocate_exact_size(vec2(chart_w, avail.y), Sense::click());
        chart(ui, rect, &resp, &all, center, here.is_some(), &mut selected);
        ui.add_space(8.0);
        ui.vertical(|ui| table(ui, &all, here.or(center), &mut selected));
    });
    ui.data_mut(|d| match &selected {
        Some(s) => {
            d.insert_temp(sel_id, s.clone());
        }
        None => d.remove::<String>(sel_id),
    });
}

/// The chart: rings, north, this station, and each vessel with its motion.
fn chart(ui: &mut Ui, rect: Rect, resp: &egui::Response, all: &[Vessel], center: Option<(f64, f64)>, is_home: bool, selected: &mut Option<String>) {
    let p = ui.painter_at(rect.expand(2.0));
    crate::vfo::lcd_back(&p, rect, widgets::protocol_color("AIS"));
    let plot = rect.shrink(10.0);
    let Some(c) = center else {
        p.text(plot.center(), Align2::CENTER_CENTER, "No positions heard yet", theme::sans(12.0), theme::text_3());
        crate::vfo::lcd_glass(&p, rect);
        return;
    };
    // Out to the farthest vessel heard in the last hour, or the zoom the
    // wheel set.
    let zoom_id = egui::Id::new("ais-zoom");
    let mut zoom: f32 = ui.data(|d| d.get_temp(zoom_id)).unwrap_or(1.0);
    if resp.hovered() {
        let n = widgets::wheel(ui);
        if n != 0.0 {
            widgets::eat_scroll(ui);
            zoom = (zoom * 1.25f32.powf(-n)).clamp(0.05, 8.0);
        }
    }
    if resp.double_clicked() {
        zoom = 1.0;
    }
    ui.data_mut(|d| d.insert_temp(zoom_id, zoom));
    let far = all.iter().filter(|v| v.age_s() < 3600.0).filter_map(|v| Some(range_bearing(c, v.lat?, v.lon?).0)).fold(1.0f32, f32::max);
    let r_nm = far * 1.15 * zoom;
    let px = plot.width().min(plot.height()) / 2.0 / r_nm;
    let o = plot.center();
    let at = |lat: f64, lon: f64| {
        let (x, y) = enu(c, lat, lon);
        pos2(o.x + x * px, o.y - y * px)
    };
    let cp = p.with_clip_rect(plot);
    // Rings, labeled along the north-east diagonal.
    let step = ring_step(r_nm);
    let mut k = 1;
    while step * k as f32 <= r_nm * 1.5 {
        let rr = step * k as f32 * px;
        cp.circle_stroke(o, rr, Stroke::new(1.0, alpha(theme::text_3(), 0.22)));
        let lp = o + vec2(rr * 0.707, -rr * 0.707);
        let t = if step < 1.0 { format!("{} nm", step * k as f32) } else { format!("{:.0} nm", step * k as f32) };
        cp.text(lp + vec2(3.0, -2.0), Align2::LEFT_BOTTOM, t, theme::mono(9.0), alpha(theme::text_3(), 0.8));
        k += 1;
    }
    cp.line_segment([pos2(o.x, plot.top()), pos2(o.x, plot.bottom())], Stroke::new(1.0, alpha(theme::text_3(), 0.10)));
    cp.line_segment([pos2(plot.left(), o.y), pos2(plot.right(), o.y)], Stroke::new(1.0, alpha(theme::text_3(), 0.10)));
    // North.
    let n0 = pos2(plot.right() - 14.0, plot.top() + 20.0);
    p.add(Shape::convex_polygon(vec![n0 + vec2(0.0, -10.0), n0 + vec2(5.0, 4.0), n0 + vec2(0.0, 1.0), n0 + vec2(-5.0, 4.0)], theme::text_2(), Stroke::NONE));
    p.text(n0 + vec2(0.0, 6.0), Align2::CENTER_TOP, "N", theme::mono_bold(9.0), theme::text_2());
    // This station.
    if is_home {
        cp.line_segment([o + vec2(-6.0, 0.0), o + vec2(6.0, 0.0)], Stroke::new(1.5, theme::AMBER));
        cp.line_segment([o + vec2(0.0, -6.0), o + vec2(0.0, 6.0)], Stroke::new(1.5, theme::AMBER));
    }
    // The vessels: oldest first, so the newest draw on top.
    // Every mark's place first, so no name is set over another's mark.
    let mut labels: Vec<Rect> = all.iter().filter_map(|v| Some(Rect::from_center_size(at(v.lat?, v.lon?), vec2(12.0, 12.0)))).collect();
    let mut hovered: Option<&Vessel> = None;
    let pointer = resp.hover_pos();
    let mut best = f32::INFINITY;
    for v in all.iter().rev() {
        let (Some(lat), Some(lon)) = (v.lat, v.lon) else { continue };
        let pos = at(lat, lon);
        if !plot.expand(6.0).contains(pos) {
            continue;
        }
        let age = v.age_s();
        // Bright for ten minutes, fading to a ghost by an hour.
        let fade = if age < 600.0 { 1.0 } else { (1.0 - (age - 600.0) / 3000.0).clamp(0.25, 1.0) };
        let role = v.role();
        let col = alpha(role.color(), fade);
        let sel = selected.as_deref() == Some(v.mmsi.as_str());
        // Where it will be in six minutes, along its course.
        if let (Some(s), Some(cg)) = (v.sog, v.cog)
            && s > 0.5
            && role != Role::Aid
            && role != Role::Base
        {
            let d = s / 10.0 * px;
            let a = cg.to_radians();
            cp.line_segment([pos, pos + vec2(a.sin() * d, -a.cos() * d)], Stroke::new(1.0, alpha(col, 0.7)));
        }
        match role {
            Role::Aid => {
                cp.add(Shape::convex_polygon(vec![pos + vec2(0.0, -5.0), pos + vec2(5.0, 0.0), pos + vec2(0.0, 5.0), pos + vec2(-5.0, 0.0)], Color32::TRANSPARENT, Stroke::new(1.5, col)));
            }
            Role::Base => {
                cp.rect_stroke(Rect::from_center_size(pos, vec2(8.0, 8.0)), CornerRadius::ZERO, Stroke::new(1.5, col), egui::StrokeKind::Middle);
            }
            _ => match v.heading.or(v.cog.filter(|_| v.sog.is_some_and(|s| s > 0.5))) {
                // A hull: a long triangle pointing where the bow is.
                Some(h) => {
                    let a = h.to_radians();
                    let (f, r) = (vec2(a.sin(), -a.cos()), vec2(a.cos(), a.sin()));
                    let s = if role == Role::ClassB { 5.0 } else { 7.0 };
                    let pts = [pos + f * s * 1.4, pos - f * s * 0.8 + r * s * 0.6, pos - f * s * 0.5, pos - f * s * 0.8 - r * s * 0.6];
                    cp.add(Shape::convex_polygon(vec![pts[0], pts[1], pts[2]], alpha(col, 0.85), Stroke::NONE));
                    cp.add(Shape::convex_polygon(vec![pts[0], pts[2], pts[3]], alpha(col, 0.85), Stroke::NONE));
                }
                None => {
                    cp.circle_filled(pos, 3.5, alpha(col, 0.85));
                }
            },
        }
        if sel {
            cp.circle_stroke(pos, 11.0, Stroke::new(1.5, theme::text()));
        }
        // A name where it fits clear of the others.
        if fade > 0.5 || sel {
            let g = cp.layout_no_wrap(v.label(), theme::mono(9.5), col);
            let r = Rect::from_min_size(pos + vec2(9.0, -g.size().y / 2.0), g.size());
            if plot.contains_rect(r) && !labels.iter().any(|o| o.intersects(r.expand(2.0))) {
                cp.galley(r.min, g, alpha(theme::text_2(), fade));
                labels.push(r);
            }
        }
        if let Some(pp) = pointer {
            let d = pp.distance(pos);
            if d < 12.0 && d < best {
                best = d;
                hovered = Some(v);
            }
        }
    }
    crate::vfo::lcd_glass(&p, rect);
    p.text(rect.left_bottom() + vec2(8.0, -6.0), Align2::LEFT_BOTTOM, if is_home { "around this station · no coastline · wheel to zoom" } else { "around the vessels · set a station for range and bearing" }, theme::mono(9.0), theme::text_3());
    if let Some(v) = hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        resp.clone().on_hover_text_at_pointer(detail(v, center));
        if resp.clicked() {
            *selected = Some(v.mmsi.clone());
        }
    } else if resp.clicked() {
        *selected = None;
    }
}

/// A vessel described in lines, for a hover.
fn detail(v: &Vessel, c: Option<(f64, f64)>) -> String {
    let mut l = vec![format!("{} · MMSI {}", v.label(), v.mmsi)];
    let mut what = vec![v.role().word().to_string()];
    what.extend(v.kind.clone());
    what.extend(v.callsign.clone());
    what.extend(v.length.as_ref().map(|m| format!("{m} m")));
    l.push(what.join(" · "));
    let mut mv = Vec::new();
    mv.extend(v.sog.map(|s| format!("{s:.1} kn")));
    mv.extend(v.cog.map(|c| format!("course {c:.0}°")));
    mv.extend(v.heading.map(|h| format!("heading {h:.0}°")));
    mv.extend(v.status.clone());
    if !mv.is_empty() {
        l.push(mv.join(" · "));
    }
    if let Some(d) = &v.destination {
        l.push(format!("bound for {d}"));
    }
    if let (Some(c), Some(la), Some(lo)) = (c, v.lat, v.lon) {
        let (r, b) = range_bearing(c, la, lo);
        l.push(format!("{la:.4}, {lo:.4} · {r:.1} nm at {b:.0}°"));
    }
    if let Some(t) = v.last {
        l.push(format!("last heard {} · {} reports", local_hms(t), v.reports));
    }
    l.join("\n")
}

/// The table: one row a vessel, newest heard first.
fn table(ui: &mut Ui, all: &[Vessel], c: Option<(f64, f64)>, selected: &mut Option<String>) {
    let head = |t: &str| RichText::new(t).font(theme::mono(10.0)).color(theme::text_3()).extra_letter_spacing(0.6);
    let cols = [150.0, 120.0, 56.0, 46.0, 90.0];
    let cell = |ui: &mut Ui, w: f32, t: RichText| {
        ui.allocate_ui_with_layout(vec2(w, 18.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_width(w);
            ui.add(egui::Label::new(t).truncate());
        });
    };
    ui.horizontal(|ui| {
        for (t, w) in ["VESSEL", "TYPE", "SOG", "COG", "RANGE"].iter().zip(cols) {
            cell(ui, w, head(t));
        }
        ui.label(head("STATUS · BOUND FOR"));
    });
    let mono = |t: String, c: Color32| RichText::new(t).font(theme::mono(11.0)).color(c);
    egui::ScrollArea::vertical().id_salt("ais-table").auto_shrink([false, false]).show(ui, |ui| {
        if all.is_empty() {
            ui.add(egui::Label::new(RichText::new("Ships, base stations and aids to navigation appear here as their reports are heard. A ship's position comes every 2 to 10 seconds under way, its name every 6 minutes.").font(theme::sans(12.0)).color(theme::text_3())).wrap());
        }
        for v in all {
            let sel = selected.as_deref() == Some(v.mmsi.as_str());
            let fade = if v.age_s() < 600.0 { 1.0 } else { 0.5 };
            let row = ui.horizontal(|ui| {
                let col = v.role().color();
                cell(ui, cols[0], mono(v.label(), alpha(theme::text(), fade)));
                cell(ui, cols[1], mono(v.kind.clone().unwrap_or_else(|| v.role().word().into()), alpha(col, fade)));
                cell(ui, cols[2], mono(v.sog.map_or("—".into(), |s| format!("{s:.1}")), alpha(theme::text_2(), fade)));
                cell(ui, cols[3], mono(v.cog.map_or("—".into(), |s| format!("{s:.0}°")), alpha(theme::text_2(), fade)));
                let rng = match (c, v.lat, v.lon) {
                    (Some(c), Some(la), Some(lo)) => {
                        let (r, b) = range_bearing(c, la, lo);
                        format!("{r:.1} nm {b:03.0}°")
                    }
                    _ => "—".into(),
                };
                cell(ui, cols[4], mono(rng, alpha(theme::text_2(), fade)));
                let mut tail = Vec::new();
                tail.extend(v.status.clone());
                tail.extend(v.destination.as_ref().map(|d| format!("→ {d}")));
                ui.add(egui::Label::new(mono(tail.join(" · "), alpha(theme::text_3(), fade))).truncate());
            });
            let r = row.response.rect;
            if sel {
                ui.painter().rect_filled(r.expand2(vec2(2.0, 1.0)), CornerRadius::same(2), alpha(theme::AMBER, 0.10));
            }
            let resp = ui.interact(r, ui.id().with(("ais-row", &v.mmsi)), Sense::click()).on_hover_text(detail(v, c));
            if resp.clicked() {
                *selected = if sel { None } else { Some(v.mmsi.clone()) };
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rings_are_round_numbers_and_few() {
        assert_eq!(ring_step(0.8), 0.25);
        assert_eq!(ring_step(4.0), 1.0);
        assert_eq!(ring_step(23.0), 5.0);
    }

    #[test]
    fn range_and_bearing_point_the_right_way() {
        let c = (39.0, -76.5);
        let (r, b) = range_bearing(c, 39.1, -76.5);
        assert!((r - 6.0).abs() < 0.05 && b.abs() < 0.5, "{r} {b}");
        let (_, b) = range_bearing(c, 39.0, -76.4);
        assert!((b - 90.0).abs() < 0.5, "{b}");
    }
}
