//! The innerpulse grounds, the ember signal and uSDR's own accent.
//!
//! Two grounds, from the brand's KDE themes. **Carbon** (the default) is the
//! brand's brushed steel over cool carbon blacks: steel text, blued-steel
//! selection, glass panels lit from above. **Ink** is the warm product ground.
//! uSDR belongs to the firmware & RF family, whose accent on either is amber:
//! the VFO, the passband, the trace. Ember stays reserved for the mark,
//! focus, the one filled action and the *live* state (recording) — never a
//! warning, and never a series.

use egui::{Color32, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ground {
    #[default]
    Carbon,
    Ink,
}

impl Ground {
    pub fn label(self) -> &'static str {
        match self {
            Ground::Carbon => "Carbon — brushed steel on cool blacks",
            Ground::Ink => "Ink — warm product ground",
        }
    }
}

struct Palette {
    ground: Color32,
    s: [Color32; 4],
    text: [Color32; 3],
    line: [Color32; 2],
    select: Color32,
}

const fn rgb(h: u32) -> Color32 {
    Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
}

/// Light lines as premultiplied white-ish over the ground.
const fn hairline(r: u8, g: u8, b: u8, a: f32) -> Color32 {
    Color32::from_rgba_premultiplied((r as f32 * a) as u8, (g as f32 * a) as u8, (b as f32 * a) as u8, (a * 255.0) as u8)
}

const CARBON: Palette = Palette {
    ground: rgb(0x0b0d10),
    s: [rgb(0x111317), rgb(0x171a1f), rgb(0x1f2329), rgb(0x2b3037)],
    text: [rgb(0xe3e6e9), rgb(0xb9bdc3), rgb(0x8f949b)],
    line: [hairline(227, 230, 233, 0.10), hairline(227, 230, 233, 0.18)],
    select: rgb(0x1a73b8),
};

// Ink is visibly warmer than Carbon, not a shade off it: its surfaces step
// up through umber rather than slate, and its hairlines are warm and a
// touch stronger, since it has no weave to give the panels a grain.
const INK_P: Palette = Palette {
    ground: rgb(0x0a0908),
    s: [rgb(0x15110c), rgb(0x1d1812), rgb(0x282119), rgb(0x362d23)],
    text: [rgb(0xf0eae0), rgb(0xb8ae9f), rgb(0x8f8678)],
    line: [hairline(255, 236, 214, 0.11), hairline(255, 236, 214, 0.20)],
    select: rgb(0x7a521f),
};

static GROUND: AtomicU8 = AtomicU8::new(0);

fn pal() -> &'static Palette {
    if GROUND.load(Ordering::Relaxed) == 1 { &INK_P } else { &CARBON }
}

pub fn ground() -> Ground {
    if GROUND.load(Ordering::Relaxed) == 1 { Ground::Ink } else { Ground::Carbon }
}

pub fn ink() -> Color32 {
    pal().ground
}
pub fn surface_1() -> Color32 {
    pal().s[0]
}
pub fn surface_2() -> Color32 {
    pal().s[1]
}
pub fn surface_3() -> Color32 {
    pal().s[2]
}
/// Gunmetal on Carbon: pressed keys and the fader track.
pub fn surface_4() -> Color32 {
    pal().s[3]
}
pub fn text() -> Color32 {
    pal().text[0]
}
pub fn text_2() -> Color32 {
    pal().text[1]
}
pub fn text_3() -> Color32 {
    pal().text[2]
}
pub fn line() -> Color32 {
    pal().line[0]
}
pub fn line_2() -> Color32 {
    pal().line[1]
}

pub const EMBER: Color32 = rgb(0xf43a2a);
pub const ACTION: Color32 = rgb(0xd12c1e);
pub const ACTION_HOVER: Color32 = rgb(0xbd2417);

/// uSDR's family accent: the VFO, the passband, the live trace.
pub const AMBER: Color32 = rgb(0xffb347);
/// Secondary data color: peak hold, the second series.
pub const CYAN: Color32 = rgb(0x52e3ff);
pub const VIOLET: Color32 = rgb(0xb38cff);

pub const OK: Color32 = rgb(0x7dffb0);
/// Caution: a held display, drops, a weak decode, DC on the feed. Lemon,
/// a clear step from the amber accent, so a warning never reads as the VFO.
pub const WARN: Color32 = rgb(0xffe45e);
pub const ERR: Color32 = rgb(0xff9a8f);

pub const RADIUS: u8 = 4;

pub fn alpha(c: Color32, a: f32) -> Color32 {
    let [r, g, b, _] = c.to_array();
    Color32::from_rgba_unmultiplied(r, g, b, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let [ar, ag, ab, aa] = a.to_array();
    let [br, bg, bb, ba] = b.to_array();
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(m(ar, br), m(ag, bg), m(ab, bb), m(aa, ba))
}

/// Named families: the body sans, the measuring mono, the display face for
/// headlines and the wordmark face for the product name alone.
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}
pub fn mono_bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("mono-bold".into()))
}
pub fn sans(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
pub fn sans_semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("sans-600".into()))
}
pub fn headline(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("sans-700".into()))
}
pub fn wordmark(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("syne".into()))
}

fn font(defs: &mut FontDefinitions, name: &str, bytes: &'static [u8]) {
    defs.font_data.insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
}

pub fn install(ctx: &egui::Context, g: Ground) {
    let mut defs = FontDefinitions::default();
    font(&mut defs, "instrument-400", include_bytes!("../assets/fonts/instrument-sans-400.ttf"));
    font(&mut defs, "instrument-600", include_bytes!("../assets/fonts/instrument-sans-600.ttf"));
    font(&mut defs, "instrument-700", include_bytes!("../assets/fonts/instrument-sans-700.ttf"));
    font(&mut defs, "jbm-500", include_bytes!("../assets/fonts/jetbrains-mono-500.ttf"));
    font(&mut defs, "jbm-700", include_bytes!("../assets/fonts/jetbrains-mono-700.ttf"));
    font(&mut defs, "syne-800", include_bytes!("../assets/fonts/syne-800.ttf"));

    // egui's own fonts stay behind ours as fallbacks for symbols and emoji.
    let mut fallbacks: Vec<String> = Vec::new();
    for fam in [FontFamily::Proportional, FontFamily::Monospace] {
        for f in defs.families.get(&fam).cloned().unwrap_or_default() {
            if !fallbacks.contains(&f) {
                fallbacks.push(f);
            }
        }
    }
    let with = |first: &str| {
        let mut v = vec![first.to_owned()];
        v.extend(fallbacks.iter().cloned());
        v
    };
    defs.families.insert(FontFamily::Proportional, with("instrument-400"));
    defs.families.insert(FontFamily::Monospace, with("jbm-500"));
    defs.families.insert(FontFamily::Name("sans-600".into()), with("instrument-600"));
    defs.families.insert(FontFamily::Name("sans-700".into()), with("instrument-700"));
    defs.families.insert(FontFamily::Name("mono-bold".into()), with("jbm-700"));
    defs.families.insert(FontFamily::Name("syne".into()), with("syne-800"));
    ctx.set_fonts(defs);
    set_ground(ctx, g);
}

/// Switch grounds and restyle every widget to match.
pub fn set_ground(ctx: &egui::Context, g: Ground) {
    GROUND.store(if g == Ground::Ink { 1 } else { 0 }, Ordering::Relaxed);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, sans(11.0)),
            (TextStyle::Body, sans(13.0)),
            (TextStyle::Button, sans_semibold(12.5)),
            (TextStyle::Heading, headline(17.0)),
            (TextStyle::Monospace, mono(12.0)),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(6.0, 6.0);
        style.spacing.button_padding = egui::vec2(9.0, 4.0);
        style.spacing.slider_width = 150.0;
        style.spacing.interact_size.y = 24.0;
        style.animation_time = 0.16;

        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.override_text_color = None;
        v.panel_fill = surface_1();
        v.window_fill = surface_2();
        v.extreme_bg_color = ink();
        v.faint_bg_color = surface_2();
        v.code_bg_color = surface_3();
        v.window_stroke = Stroke::new(1.0, line_2());
        v.window_corner_radius = egui::CornerRadius::same(6);
        v.menu_corner_radius = egui::CornerRadius::same(RADIUS);
        v.window_shadow = egui::Shadow { offset: [0, 10], blur: 28, spread: 0, color: Color32::from_black_alpha(160) };
        v.popup_shadow = egui::Shadow { offset: [0, 6], blur: 16, spread: 0, color: Color32::from_black_alpha(150) };
        v.selection.bg_fill = if g == Ground::Carbon { alpha(pal().select, 0.85) } else { alpha(AMBER, 0.28) };
        v.selection.stroke = Stroke::new(1.0, if g == Ground::Carbon { Color32::WHITE } else { AMBER });
        v.hyperlink_color = AMBER;
        v.warn_fg_color = WARN;
        v.error_fg_color = ERR;
        v.slider_trailing_fill = true;
        // Unavailable keys stay legible (about 3:1 on the panel) while still
        // reading as off; egui's 0.5 left their labels near 2:1.
        v.disabled_alpha = 0.65;
        v.handle_shape = egui::style::HandleShape::Rect { aspect_ratio: 0.55 };
        v.text_cursor.stroke = Stroke::new(1.5, EMBER);

        let r = egui::CornerRadius::same(RADIUS);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = surface_2();
        w.noninteractive.weak_bg_fill = surface_2();
        w.noninteractive.bg_stroke = Stroke::new(1.0, line());
        w.noninteractive.fg_stroke = Stroke::new(1.0, text_2());
        w.noninteractive.corner_radius = r;
        w.inactive.bg_fill = surface_3();
        w.inactive.weak_bg_fill = surface_3();
        w.inactive.bg_stroke = Stroke::new(1.0, line());
        w.inactive.fg_stroke = Stroke::new(1.0, text());
        w.inactive.corner_radius = r;
        w.hovered.bg_fill = surface_4();
        w.hovered.weak_bg_fill = surface_4();
        w.hovered.bg_stroke = Stroke::new(1.0, line_2());
        w.hovered.fg_stroke = Stroke::new(1.0, text());
        w.hovered.corner_radius = r;
        w.hovered.expansion = 0.0;
        w.active.bg_fill = surface_4();
        w.active.weak_bg_fill = surface_4();
        w.active.bg_stroke = Stroke::new(1.0, AMBER);
        w.active.fg_stroke = Stroke::new(1.0, text());
        w.active.corner_radius = r;
        w.active.expansion = 0.0;
        w.open = w.hovered;
    });
}

/// Glass, lit from above: a sheen through the top edge, a one-pixel
/// highlight lip, a faint shade at the foot. Painted over a card's fill.
pub fn glass(painter: &egui::Painter, r: egui::Rect) {
    glass_top(painter, r);
    painter.line_segment(
        [r.left_bottom() + egui::vec2(RADIUS as f32, -0.5), r.right_bottom() + egui::vec2(-(RADIUS as f32), -0.5)],
        Stroke::new(1.0, Color32::from_black_alpha(70)),
    );
}

/// The sheen and the highlight lip alone, for cards whose foot is not known
/// while they are being laid out.
pub fn glass_top(painter: &egui::Painter, r: egui::Rect) {
    let strength = if ground() == Ground::Carbon { 1.0 } else { 0.6 };
    let sheen_h = (r.height() * 0.35).min(28.0);
    let top = egui::Rect::from_min_size(r.min, egui::vec2(r.width(), sheen_h));
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(top.left_top(), Color32::from_white_alpha((10.0 * strength) as u8));
    mesh.colored_vertex(top.right_top(), Color32::from_white_alpha((10.0 * strength) as u8));
    mesh.colored_vertex(top.left_bottom(), Color32::TRANSPARENT);
    mesh.colored_vertex(top.right_bottom(), Color32::TRANSPARENT);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(2, 1, 3);
    painter.add(egui::Shape::mesh(mesh));
    painter.line_segment(
        [r.left_top() + egui::vec2(RADIUS as f32, 0.5), r.right_top() + egui::vec2(-(RADIUS as f32), 0.5)],
        Stroke::new(1.0, Color32::from_white_alpha((26.0 * strength) as u8)),
    );
}

/// A recessed instrument well: the ground sunk into the panel, a shadow
/// along its top inner edge, a lit lip along its foot, and a hairline that
/// takes the tint of what the well belongs to (amber for A, violet for B).
pub fn well(painter: &egui::Painter, r: egui::Rect, tint: Option<Color32>) {
    let cr = egui::CornerRadius::same(RADIUS);
    painter.rect_filled(r, cr, ink());
    let shade = egui::Rect::from_min_size(r.min, egui::vec2(r.width(), (r.height() * 0.4).min(7.0)));
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(shade.left_top(), Color32::from_black_alpha(110));
    mesh.colored_vertex(shade.right_top(), Color32::from_black_alpha(110));
    mesh.colored_vertex(shade.left_bottom(), Color32::TRANSPARENT);
    mesh.colored_vertex(shade.right_bottom(), Color32::TRANSPARENT);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(2, 1, 3);
    painter.add(egui::Shape::mesh(mesh));
    painter.line_segment(
        [r.left_bottom() + egui::vec2(RADIUS as f32, 0.5), r.right_bottom() + egui::vec2(-(RADIUS as f32), 0.5)],
        Stroke::new(1.0, Color32::from_white_alpha(14)),
    );
    let edge = tint.map_or(line(), |t| alpha(t, 0.30));
    painter.rect_stroke(r, cr, Stroke::new(1.0, edge), egui::StrokeKind::Inside);
}

/// Text in brushed steel: bright above the crease, dark through it, and a
/// lighter band below, the chrome masthead's ramp in three cuts. Gunmetal
/// tones on Ink would fight the warm ground, so there it is plain text.
pub fn steel_text(painter: &egui::Painter, pos: egui::Pos2, anchor: egui::Align2, s: &str, font: FontId) -> egui::Rect {
    let galley = painter.layout_no_wrap(s.to_owned(), font, Color32::PLACEHOLDER);
    let rect = anchor.anchor_size(pos, galley.size());
    if ground() != Ground::Carbon {
        painter.galley(rect.min, galley, text());
        return rect;
    }
    // Where the ink of the letters sits inside the galley's line box.
    let ink_top = rect.top() + rect.height() * 0.22;
    let ink_h = rect.height() * 0.62;
    let cuts = [
        (f32::NEG_INFINITY, 0.38, rgb(0xf4f6f8)),
        (0.38, 0.54, rgb(0xcfd3d8)),
        (0.54, 0.62, rgb(0x8b9097)),
        (0.62, 0.84, rgb(0xd5d9de)),
        (0.84, f32::INFINITY, rgb(0x9da2a9)),
    ];
    for (a, b, c) in cuts {
        let y0 = if a.is_finite() { ink_top + ink_h * a } else { rect.top() - 4.0 };
        let y1 = if b.is_finite() { ink_top + ink_h * b } else { rect.bottom() + 4.0 };
        let clip = egui::Rect::from_min_max(egui::pos2(rect.left() - 2.0, y0), egui::pos2(rect.right() + 2.0, y1));
        painter.with_clip_rect(clip.intersect(painter.clip_rect())).galley(rect.min, galley.clone(), c);
    }
    rect
}

/// Steel text as a widget, sized to its letters.
pub fn steel_label(ui: &mut egui::Ui, s: &str, font: FontId) -> egui::Response {
    let size = ui.painter().layout_no_wrap(s.to_owned(), font.clone(), Color32::PLACEHOLDER).size();
    let (r, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    steel_text(ui.painter(), r.min, egui::Align2::LEFT_TOP, s, font);
    resp
}

/// The carbon-fiber weave: a 16 px twill tile, a whisper above the ground,
/// for the masthead and the applet column. Ink has no weave.
pub fn weave(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    if ground() != Ground::Carbon {
        return None;
    }
    let id = egui::Id::new("carbon-weave");
    if let Some(t) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(t);
    }
    let n = 16;
    let mut px = Vec::with_capacity(n * n);
    for y in 0..n {
        for x in 0..n {
            // Two 8 px tows crossing: the band a pixel sits on, and how far
            // across the tow, give a soft rounded highlight.
            let warp = ((x / 4 + y / 4) % 2) == 0;
            let u = if warp { (x % 4) as f32 } else { (y % 4) as f32 };
            let lift = (1.0 - ((u - 1.5) / 2.0).powi(2)).max(0.0);
            let a = if warp { 5.0 + 6.0 * lift } else { 2.0 + 4.0 * lift };
            px.push(Color32::from_white_alpha(a as u8));
        }
    }
    let tex = ctx.load_texture(
        "carbon-weave",
        egui::ColorImage::new([n, n], px),
        egui::TextureOptions { wrap_mode: egui::TextureWrapMode::Repeat, ..egui::TextureOptions::NEAREST },
    );
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    Some(tex)
}

/// Tile the weave over `r`.
pub fn paint_weave(painter: &egui::Painter, r: egui::Rect) {
    if let Some(t) = weave(painter.ctx()) {
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(r.width() / 16.0, r.height() / 16.0));
        painter.image(t.id(), r, uv, Color32::WHITE);
    }
}
