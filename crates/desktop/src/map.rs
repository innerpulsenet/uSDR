//! The map: where the decodes come from. Aircraft heard on HFDL at the
//! positions they report, with their tracks; the HFDL ground stations, lit
//! when heard; ships' DSC positions, a distress alert in red; the FT8, WSPR
//! and JS8 stations at the centers of the locators they send; and the
//! receiving station, with the great-circle path to whatever the pointer
//! is on. Drag to pan, scroll to zoom, click a mark to open its decode.
//!
//! The coastline is Natural Earth's 1:110m (public domain), drawn on an
//! equirectangular grid: simple, and every point where its numbers say.
//! Only positions a decoder decoded are drawn; a locator puts a station
//! somewhere in its square, and the mark says it is a square's center.

use crate::app::{App, DecodeRow, local_hms};
use crate::theme::{self, alpha};
use crate::widgets;
use egui::{Color32, Pos2, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use std::sync::OnceLock;
use std::time::SystemTime;

/// The coastline, as lines of (latitude, longitude) in degrees.
fn coastline() -> &'static [Vec<(f32, f32)>] {
    static LINES: OnceLock<Vec<Vec<(f32, f32)>>> = OnceLock::new();
    LINES.get_or_init(|| {
        let b: &[u8] = include_bytes!("../data/coastline-110m.bin");
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
        let i16_at = |i: usize| i16::from_le_bytes([b[i], b[i + 1]]) as f32 / 100.0;
        let mut out = Vec::new();
        let n = u32_at(0);
        let mut at = 4;
        for _ in 0..n {
            let k = u32_at(at);
            at += 4;
            out.push((0..k).map(|j| (i16_at(at + 4 * j), i16_at(at + 4 * j + 2))).collect());
            at += 4 * k;
        }
        out
    })
}

/// The view: the longitude and latitude at its center, and pixels a degree.
#[derive(Clone, Copy)]
pub struct MapView {
    pub lon: f32,
    pub lat: f32,
    pub zoom: f32,
    pub layers: Layers,
}

#[derive(Clone, Copy)]
pub struct Layers {
    pub aircraft: bool,
    pub ships: bool,
    pub amateur: bool,
    pub ground: bool,
}

impl Default for MapView {
    fn default() -> Self {
        // The North Atlantic, where most of what is heard here is.
        MapView { lon: -40.0, lat: 35.0, zoom: 0.0, layers: Layers { aircraft: true, ships: true, amateur: true, ground: true } }
    }
}

/// What a mark stands for.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Aircraft,
    Ship { distress: bool },
    Station,
    Ground { heard: bool },
    Home,
}

struct Mark {
    lat: f32,
    lon: f32,
    kind: Kind,
    color: Color32,
    label: String,
    detail: String,
    /// The decode it came from, to open on click.
    decode: Option<usize>,
    /// Seconds since it was heard.
    age: f32,
}

fn num(d: &DecodeRow, k: &str) -> Option<f32> {
    d.ev.fields.get(k)?.trim().parse::<f32>().ok().filter(|v| v.is_finite())
}

fn age_s(at: SystemTime) -> f32 {
    at.elapsed().map(|e| e.as_secs_f32()).unwrap_or(0.0)
}

/// Every mark the log supports, newest first per thing; and the aircraft
/// tracks, oldest point first.
fn gather(app: &App, l: Layers) -> (Vec<Mark>, Vec<(Color32, Vec<(f32, f32)>)>) {
    let mut marks = Vec::new();
    let mut tracks: Vec<(String, Vec<(f32, f32)>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Each vessel's name from the newest of its reports that has one, found
    // in one pass rather than a search of the log for every vessel.
    let mut ship_names: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    if l.ships {
        for d in app.decodes.iter().filter(|d| d.ev.protocol == "AIS") {
            if let (Some(m), Some(n)) = (d.ev.fields.get("mmsi"), d.ev.fields.get("name")) {
                ship_names.entry(m.as_str()).or_insert(n.as_str());
            }
        }
    }
    for (i, d) in app.decodes.iter().enumerate() {
        let p = d.ev.protocol.as_str();
        let age = age_s(d.at);
        if age > 6.0 * 3600.0 {
            continue;
        }
        match p {
            "HFDL" if l.aircraft => {
                let (Some(lat), Some(lon)) = (num(d, "lat"), num(d, "lon")) else { continue };
                let who = d.ev.fields.get("flight").or(d.ev.fields.get("reg")).or(d.ev.fields.get("icao")).cloned().unwrap_or_else(|| format!("ac {}", d.ev.fields.get("aircraftId").cloned().unwrap_or_default()));
                match tracks.iter_mut().find(|t| t.0 == who) {
                    Some(t) => t.1.push((lat, lon)),
                    None => tracks.push((who.clone(), vec![(lat, lon)])),
                }
                if seen.insert(("ac", who.clone())) {
                    let mut detail = format!("{} · {}", local_hms(d.at), crate::lookups::describe_hfdl(&d.ev));
                    if let Some(gs) = d.ev.fields.get("gsId") {
                        detail += &format!(" · via ground station {gs}");
                    }
                    marks.push(Mark { lat, lon, kind: Kind::Aircraft, color: widgets::protocol_color("HFDL"), label: who, detail, decode: Some(i), age });
                }
            }
            "DSC" if l.ships => {
                let (Some(lat), Some(lon)) = (num(d, "lat"), num(d, "lon")) else { continue };
                let distress = d.ev.kind.contains("distress");
                let from = d.ev.fields.get("distressId").or(d.ev.fields.get("from")).cloned().unwrap_or_default();
                if seen.insert(("ship", from.clone())) {
                    let who = crate::lookups::mmsi_line(&from);
                    marks.push(Mark { lat, lon, kind: Kind::Ship { distress }, color: if distress { theme::ERR } else { widgets::protocol_color("DSC") }, label: from, detail: format!("{} · {who} · {}", local_hms(d.at), d.ev.summary), decode: Some(i), age });
                }
            }
            "AIS" if l.ships => {
                let (Some(lat), Some(lon)) = (num(d, "lat"), num(d, "lon")) else { continue };
                let mmsi = d.ev.fields.get("mmsi").cloned().unwrap_or_default();
                if seen.insert(("ship", mmsi.clone())) {
                    // The vessel's name from any of its reports, not only this one.
                    let name = ship_names.get(mmsi.as_str()).map_or_else(|| mmsi.clone(), |n| n.to_string());
                    marks.push(Mark { lat, lon, kind: Kind::Ship { distress: false }, color: widgets::protocol_color("AIS"), label: name, detail: format!("{} · AIS · {}", local_hms(d.at), d.ev.summary), decode: Some(i), age });
                }
            }
            "FT8" | "FT4" | "WSPR" | "FST4W" | "JS8" if l.amateur => {
                let Some(grid) = d.ev.fields.get("grid") else { continue };
                let Some((lat, lon)) = usdr_proto::station::grid_center(grid) else { continue };
                let call = d.ev.fields.get("de").or(d.ev.fields.get("call")).or(d.ev.fields.get("from")).cloned().unwrap_or_default();
                if call.is_empty() || call.starts_with('<') {
                    continue;
                }
                if seen.insert(("stn", call.clone())) {
                    marks.push(Mark { lat: lat as f32, lon: lon as f32, kind: Kind::Station, color: widgets::protocol_color(p), label: call, detail: format!("{} · {p} · {} (the square's center) · {}", local_hms(d.at), grid, d.ev.summary), decode: Some(i), age });
                }
            }
            _ => {}
        }
    }
    if l.ground {
        // The ground stations heard in the last quarter hour, from their
        // squitters.
        let heard: std::collections::HashSet<u32> = app.decodes.iter().filter(|d| d.ev.protocol == "HFDL" && d.ev.kind == "squitter" && age_s(d.at) < 900.0).filter_map(|d| d.ev.fields.get("gsId")?.parse().ok()).collect();
        for &(id, lat, lon) in crate::stations_data::HFDL_POS {
            let name = crate::stations_data::HFDL.iter().find(|s| s.call == id.to_string()).map_or("", |s| s.name);
            let heard = heard.contains(&id);
            marks.push(Mark {
                lat: lat as f32,
                lon: lon as f32,
                kind: Kind::Ground { heard },
                color: if heard { widgets::protocol_color("HFDL") } else { theme::text_3() },
                label: format!("GS {id}"),
                detail: format!("HFDL ground station {id} · {name}{}", if heard { " · heard" } else { "" }),
                decode: None,
                age: 0.0,
            });
        }
    }
    if let Some((st, _)) = &app.station
        && let Some((lat, lon)) = usdr_proto::station::grid_center(&st.grid)
    {
        let label = if st.callsign.is_empty() { "HERE".to_string() } else { st.callsign.clone() };
        marks.push(Mark { lat: lat as f32, lon: lon as f32, kind: Kind::Home, color: theme::AMBER, label, detail: format!("This station · {}", st.grid), decode: None, age: 0.0 });
    }
    let tracks = tracks.into_iter().filter(|t| t.1.len() > 1).map(|(_, mut pts)| {
        pts.reverse();
        (widgets::protocol_color("HFDL"), pts)
    });
    (marks, tracks.collect())
}

/// Points along the great circle from `a` to `b`, (lat, lon) in degrees.
fn great_circle(a: (f64, f64), b: (f64, f64), n: usize) -> Vec<(f32, f32)> {
    let (p1, l1) = (a.0.to_radians(), a.1.to_radians());
    let (p2, l2) = (b.0.to_radians(), b.1.to_radians());
    let to = |p: f64, l: f64| [p.cos() * l.cos(), p.cos() * l.sin(), p.sin()];
    let (u, v) = (to(p1, l1), to(p2, l2));
    let dot = (u[0] * v[0] + u[1] * v[1] + u[2] * v[2]).clamp(-1.0, 1.0);
    let w = dot.acos();
    if w < 1e-6 {
        return vec![(a.0 as f32, a.1 as f32)];
    }
    (0..=n)
        .map(|k| {
            let t = k as f64 / n as f64;
            let (s1, s2) = (((1.0 - t) * w).sin() / w.sin(), (t * w).sin() / w.sin());
            let x = [s1 * u[0] + s2 * v[0], s1 * u[1] + s2 * v[1], s1 * u[2] + s2 * v[2]];
            (x[2].asin().to_degrees() as f32, x[1].atan2(x[0]).to_degrees() as f32)
        })
        .collect()
}

pub fn show(app: &mut App, ui: &mut Ui) {
    let mut v = app.map;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let key = |ui: &mut Ui, label: &str, on: &mut bool, c: Color32| {
            if widgets::chip_key(ui, label, *on, c).clicked() {
                *on = !*on;
            }
        };
        key(ui, "AIRCRAFT", &mut v.layers.aircraft, widgets::protocol_color("HFDL"));
        key(ui, "GROUND STATIONS", &mut v.layers.ground, widgets::protocol_color("HFDL"));
        key(ui, "SHIPS", &mut v.layers.ships, widgets::protocol_color("DSC"));
        key(ui, "AMATEUR", &mut v.layers.amateur, widgets::protocol_color("FT8"));
        if widgets::action_small(ui, "WORLD").clicked() {
            v = MapView { layers: v.layers, ..MapView { lon: 0.0, lat: 20.0, zoom: -1.0, layers: v.layers } };
        }
        if widgets::action_small(ui, "ATLANTIC").clicked() {
            v = MapView { layers: v.layers, ..MapView::default() };
        }
        ui.label(RichText::new("drag to pan · scroll to zoom · click a mark to open its decode").font(theme::mono(crate::theme::ty::MICRO)).color(theme::text_3()));
    });
    ui.add_space(4.0);
    let (marks, tracks) = gather(app, v.layers);
    let rect = ui.available_rect_before_wrap();
    let resp = ui.allocate_rect(rect, Sense::click_and_drag());
    // Pixels a degree: the whole world across the view at zoom 0, twice
    // as much each step in.
    let scale = rect.width() / 360.0 * 2f32.powf(v.zoom) * 1.6;
    if resp.dragged() {
        let d = resp.drag_delta();
        v.lon -= d.x / scale;
        v.lat = (v.lat + d.y / scale).clamp(-80.0, 80.0);
    }
    if resp.hovered() {
        let wheel = ui.input(|i| i.smooth_scroll_delta.y);
        if wheel != 0.0 {
            v.zoom = (v.zoom + wheel / 240.0).clamp(-1.0, 5.0);
        }
    }
    let scale = rect.width() / 360.0 * 2f32.powf(v.zoom) * 1.6;
    // A point's place on screen, the copy of the world nearest the center.
    let at = |lat: f32, lon: f32| -> Pos2 {
        let mut dl = lon - v.lon;
        dl = (dl + 540.0).rem_euclid(360.0) - 180.0;
        pos2(rect.center().x + dl * scale, rect.center().y - (lat - v.lat) * scale)
    };
    let p = ui.painter_at(rect);
    p.rect_filled(rect, egui::CornerRadius::same(crate::theme::RADIUS), theme::ink());
    // The grid: every 30 degrees, the equator a little brighter.
    for k in -6..=6 {
        let lon = k as f32 * 30.0;
        let x = at(0.0, lon).x;
        p.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.0, alpha(theme::line(), 0.6)));
    }
    for k in -2..=2 {
        let lat = k as f32 * 30.0;
        let y = at(lat, v.lon).y;
        p.line_segment([pos2(rect.left(), y), pos2(rect.right(), y)], Stroke::new(1.0, alpha(theme::line(), if k == 0 { 1.0 } else { 0.6 })));
    }
    // The coastline, split where a line wraps across the view's edge.
    let coast = Stroke::new(1.0, alpha(theme::text_3(), 0.8));
    for line in coastline() {
        let mut seg: Vec<Pos2> = Vec::new();
        for &(lat, lon) in line {
            let q = at(lat, lon);
            if let Some(last) = seg.last()
                && (q.x - last.x).abs() > 180.0 * scale
            {
                if seg.len() > 1 {
                    p.add(egui::Shape::line(std::mem::take(&mut seg), coast));
                }
                seg.clear();
            }
            seg.push(q);
        }
        if seg.len() > 1 {
            p.add(egui::Shape::line(seg, coast));
        }
    }
    // Aircraft tracks.
    for (c, pts) in &tracks {
        let line: Vec<Pos2> = pts.iter().map(|&(lat, lon)| at(lat, lon)).collect();
        p.add(egui::Shape::line(line, Stroke::new(1.0, alpha(*c, 0.45))));
    }
    // Labels take their places in turn: home first, then the ground
    // stations; one that would cover another tries the other side, and is
    // left out if both are taken.
    // The summary and the coastline's credit have their corners first, so
    // no station's name is put under either.
    let count = |f: &dyn Fn(&Mark) -> bool| marks.iter().filter(|m| f(m)).count();
    let summary = format!(
        "{} aircraft · {} ships · {} amateur stations · {} ground stations heard",
        count(&|m| m.kind == Kind::Aircraft),
        count(&|m| matches!(m.kind, Kind::Ship { .. })),
        count(&|m| m.kind == Kind::Station),
        count(&|m| m.kind == Kind::Ground { heard: true })
    );
    let summary_g = p.layout_no_wrap(summary, theme::mono(crate::theme::ty::MICRO), theme::text_2());
    let tag = Rect::from_min_size(rect.left_bottom() + vec2(8.0, -summary_g.size().y - 12.0), summary_g.size() + vec2(10.0, 6.0));
    let credit_g = p.layout_no_wrap("coastline: Natural Earth".into(), theme::mono(crate::theme::ty::MICRO), theme::text_3());
    let credit = Rect::from_min_size(rect.right_bottom() - credit_g.size() - vec2(18.0, 12.0), credit_g.size() + vec2(10.0, 6.0));
    let mut taken: Vec<Rect> = vec![tag, credit];
    let mut label = |p: &egui::Painter, q: Pos2, text: &str, font: egui::FontId, c: Color32| {
        let g = p.layout_no_wrap(text.to_string(), font, c);
        for r in [Rect::from_min_size(q + vec2(9.0, -g.size().y / 2.0), g.size()), Rect::from_min_size(q + vec2(-9.0 - g.size().x, -g.size().y / 2.0), g.size()), Rect::from_min_size(q + vec2(-g.size().x / 2.0, -g.size().y - 8.0), g.size())] {
            if !taken.iter().any(|t| t.expand(2.0).intersects(r)) {
                taken.push(r);
                p.galley(r.min, g, c);
                return;
            }
        }
    };
    let mut order: Vec<usize> = (0..marks.len()).collect();
    order.sort_by_key(|&k| match marks[k].kind {
        Kind::Home => 0,
        Kind::Ground { .. } => 1,
        _ => 2,
    });
    // Marks, and the one under the pointer.
    let hover = resp.hover_pos();
    let mut near: Option<(f32, usize)> = None;
    for &k in &order {
        let m = &marks[k];
        let q = at(m.lat, m.lon);
        if !rect.contains(q) {
            continue;
        }
        let fade = (1.0 - m.age / (6.0 * 3600.0)).clamp(0.35, 1.0);
        let c = alpha(m.color, fade);
        match m.kind {
            Kind::Aircraft => {
                let s = 4.0;
                p.add(egui::Shape::convex_polygon(vec![q + vec2(0.0, -s), q + vec2(s * 0.8, s), q + vec2(-s * 0.8, s)], c, Stroke::NONE));
            }
            Kind::Ship { distress } => {
                let s = if distress { 6.0 } else { 4.0 };
                p.add(egui::Shape::convex_polygon(vec![q + vec2(0.0, -s), q + vec2(s, 0.0), q + vec2(0.0, s), q + vec2(-s, 0.0)], c, Stroke::NONE));
                if distress {
                    p.circle_stroke(q, 10.0, Stroke::new(1.5, alpha(theme::ERR, 0.8)));
                }
            }
            Kind::Station => {
                p.circle_filled(q, 2.5, c);
            }
            Kind::Ground { heard } => {
                let r = egui::Rect::from_center_size(q, vec2(7.0, 7.0));
                p.rect_stroke(r, egui::CornerRadius::same(crate::theme::R_CELL), Stroke::new(1.5, c), egui::StrokeKind::Middle);
                if heard {
                    label(&p, q, &m.label, theme::mono(crate::theme::ty::MICRO), c);
                }
            }
            Kind::Home => {
                p.circle_filled(q, 4.0, c);
                p.circle_stroke(q, 7.0, Stroke::new(1.0, c));
                label(&p, q, &m.label, theme::mono_bold(crate::theme::ty::MICRO), c);
            }
        }
        if let Some(h) = hover {
            let d = h.distance(q);
            if d < 9.0 && near.is_none_or(|(b, _)| d < b) {
                near = Some((d, k));
            }
        }
    }
    let home = app.station.as_ref().and_then(|(st, _)| usdr_proto::station::grid_center(&st.grid));
    if let Some((_, k)) = near {
        let m = &marks[k];
        // The path from here, and how far.
        let mut path_text = String::new();
        if let Some(h) = home
            && m.kind != Kind::Home
        {
            let pts = great_circle(h, (m.lat as f64, m.lon as f64), 64);
            let mut seg: Vec<Pos2> = Vec::new();
            for (lat, lon) in pts {
                let q = at(lat, lon);
                if let Some(last) = seg.last()
                    && (q.x - last.x).abs() > 90.0 * scale
                {
                    p.add(egui::Shape::dashed_line(&std::mem::take(&mut seg), Stroke::new(1.0, alpha(theme::AMBER, 0.7)), 5.0, 4.0));
                }
                seg.push(q);
            }
            p.add(egui::Shape::dashed_line(&seg, Stroke::new(1.0, alpha(theme::AMBER, 0.7)), 5.0, 4.0));
            let (km, az) = usdr_proto::station::path(h, (m.lat as f64, m.lon as f64));
            path_text = format!("{km:.0} km at {az:.0}° from here");
        }
        let tip_pos = at(m.lat, m.lon);
        egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), egui::Id::new("map-tip"), egui::PopupAnchor::Position(tip_pos + vec2(10.0, 10.0))).show(|ui| {
            ui.label(RichText::new(&m.label).font(theme::mono_bold(crate::theme::ty::BODY)).color(theme::text()));
            ui.add(egui::Label::new(RichText::new(&m.detail).font(theme::mono(crate::theme::ty::LABEL)).color(theme::text_2())).wrap());
            ui.label(RichText::new(format!("{:.2}°, {:.2}°", m.lat, m.lon)).font(theme::mono(crate::theme::ty::MICRO)).color(theme::text_3()));
            if !path_text.is_empty() {
                ui.label(RichText::new(path_text).font(theme::mono(crate::theme::ty::MICRO)).color(theme::AMBER));
            }
        });
        if resp.clicked()
            && let Some(i) = m.decode
        {
            app.selected_decode = Some(i);
            app.decode_family = crate::decoders::family_of(&app.decodes[i].ev.protocol);
            app.decode_chip = Some(crate::dock::ALL_HF);
            app.dock_tab = crate::app::DockTab::Decodes;
        }
    }
    p.rect_filled(tag, egui::CornerRadius::same(crate::theme::R_KEY), alpha(theme::ink(), 0.8));
    p.galley(tag.min + vec2(5.0, 3.0), summary_g, theme::text_2());
    p.rect_filled(credit, egui::CornerRadius::same(crate::theme::R_KEY), alpha(theme::ink(), 0.8));
    p.galley(credit.min + vec2(5.0, 3.0), credit_g, theme::text_3());
    p.rect_stroke(rect, egui::CornerRadius::same(crate::theme::RADIUS), Stroke::new(1.0, theme::line()), egui::StrokeKind::Inside);
    app.map = v;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_coastline_loads_and_sits_on_the_globe() {
        let lines = coastline();
        assert!(lines.len() > 100);
        assert!(lines.iter().flatten().all(|&(lat, lon)| (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)));
    }

    #[test]
    fn a_great_circle_starts_and_ends_where_asked() {
        let pts = great_circle((41.7, -72.7), (51.5, -0.1), 32);
        let (a, b) = (pts[0], pts[pts.len() - 1]);
        assert!((a.0 - 41.7).abs() < 0.01 && (a.1 + 72.7).abs() < 0.01);
        assert!((b.0 - 51.5).abs() < 0.01 && (b.1 + 0.1).abs() < 0.01);
        // It bows north of the rhumb line, as a great circle to Europe does.
        assert!(pts[16].0 > 50.0);
    }
}
