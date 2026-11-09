//! The innerpulse grounds, the ember signal and uSDR's own accent.
//!
//! Two grounds, from the brand's KDE themes. **Carbon** (the default) is the
//! brand's brushed steel over cool carbon blacks: steel text, one brushed
//! plate with glass let into it, lit from above. **Ink** is the warm product
//! ground.
//! uSDR belongs to the firmware & RF family, whose accent on either is amber:
//! the VFO, the passband, the trace. Ember stays reserved for the mark,
//! focus, the one filled action and the *live* state (recording) — never a
//! warning, and never a series.

use egui::{Color32, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// How far a disabled control dims. Half fell near 2:1 against the plate.
pub const DISABLED_ALPHA: f32 = 0.65;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ground {
    #[default]
    Carbon,
    Ink,
    /// Carbon in red alone, for operating in the dark with dark-adapted
    /// eyes: every color the app draws is turned to its brightness in red.
    Night,
}

impl Ground {
    pub const ALL: [Ground; 3] = [Ground::Carbon, Ground::Ink, Ground::Night];

    pub fn label(self) -> &'static str {
        match self {
            Ground::Carbon => "Carbon — brushed steel on cool blacks",
            Ground::Ink => "Ink — warm product ground",
            Ground::Night => "Night — red only, for dark-adapted eyes",
        }
    }
}

struct Palette {
    ground: Color32,
    s: [Color32; 4],
    text: [Color32; 3],
    line: [Color32; 2],
    /// The window's one plate.
    plate: Color32,
    /// A key's smoked glass: at rest (top, foot), then held down (top, foot).
    key: [Color32; 4],
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
    plate: rgb(0x262a30),
    key: [rgb(0x1b1e23), rgb(0x0e1013), rgb(0x0c0d10), rgb(0x14161a)],
};

// Ink is visibly warmer than Carbon, not a shade off it: its surfaces step
// up through umber rather than slate, and its hairlines are warm and a
// touch stronger, since it has no brushing to give the plate a grain.
const INK_P: Palette = Palette {
    ground: rgb(0x0a0908),
    s: [rgb(0x15110c), rgb(0x1d1812), rgb(0x282119), rgb(0x362d23)],
    text: [rgb(0xf0eae0), rgb(0xb8ae9f), rgb(0x8f8678)],
    line: [hairline(255, 236, 214, 0.11), hairline(255, 236, 214, 0.20)],
    plate: rgb(0x211b14),
    key: [rgb(0x221c15), rgb(0x13100b), rgb(0x110d09), rgb(0x1a150f)],
};

// Night is Carbon's palette, its ground a hair off Carbon's so whatever
// caches colors by the ground (the waterfall) redraws on the switch.
const NIGHT_P: Palette = Palette { ground: rgb(0x0b0c10), ..CARBON };

static GROUND: AtomicU8 = AtomicU8::new(0);

fn pal() -> &'static Palette {
    match GROUND.load(Ordering::Relaxed) {
        1 => &INK_P,
        2 => &NIGHT_P,
        _ => &CARBON,
    }
}

/// The ground's materials: Night is built on Carbon and reports it, so it
/// keeps the brushed plate and the steel.
pub fn ground() -> Ground {
    if GROUND.load(Ordering::Relaxed) == 1 { Ground::Ink } else { Ground::Carbon }
}

/// Whether everything is being drawn in red alone.
pub fn night() -> bool {
    GROUND.load(Ordering::Relaxed) == 2
}

pub fn ink() -> Color32 {
    pal().ground
}
/// The plate the whole window is cut from.
pub fn plate() -> Color32 {
    pal().plate
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

/// uSDR's family accent: the VFO, the passband, the live trace.
pub const AMBER: Color32 = rgb(0xffb347);
/// A primary key's amber under the pointer.
pub const AMBER_HOVER: Color32 = rgb(0xffc56e);
/// Secondary data color: peak hold, the second series.
pub const CYAN: Color32 = rgb(0x52e3ff);
pub const VIOLET: Color32 = rgb(0xb38cff);

pub const OK: Color32 = rgb(0x7dffb0);
/// Caution: a held display, drops, a weak decode, DC on the feed. Lemon,
/// a clear step from the amber accent, so a warning never reads as the VFO.
pub const WARN: Color32 = rgb(0xffe45e);
pub const ERR: Color32 = rgb(0xff9a8f);

pub const RADIUS: u8 = 4;
/// The other corner radii, by what they round: a meter cell or a hairline
/// track, a fader cap or a small swatch, a key or a readout plate, (panels
/// and wells take [`RADIUS`]), a floating card of glass, a window.
pub const R_CELL: u8 = 1;
pub const R_CAP: u8 = 2;
pub const R_KEY: u8 = 3;
pub const R_GLASS: u8 = 5;
pub const R_WINDOW: u8 = 6;

/// The lit lip along an edge the light catches, as white alpha: under a
/// groove or along a quiet foot, along a panel's or a key's top, and the
/// hard edge of raised metal.
pub const LIP_SOFT: u8 = 16;
pub const LIP: u8 = 24;
pub const LIP_HARD: u8 = 36;

/// The type scale for everything but the large readouts, which set their
/// own size. Nothing is set smaller than `MICRO`.
pub mod ty {
    /// Captions, axis figures, tags.
    pub const MICRO: f32 = 10.0;
    /// Labels beside controls, chips.
    pub const LABEL: f32 = 10.5;
    /// Keys and small values.
    pub const SMALL: f32 = 11.0;
    /// Values and readouts in the panels.
    pub const VALUE: f32 = 11.5;
    /// Running text.
    pub const BODY: f32 = 12.0;
    /// Running text that leads a card.
    pub const LEDE: f32 = 12.5;
}

pub fn alpha(c: Color32, a: f32) -> Color32 {
    let [r, g, b, _] = c.to_array();
    Color32::from_rgba_unmultiplied(r, g, b, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let [ar, ag, ab, aa] = a.to_array();
    let [br, bg, bb, ba] = b.to_array();
    // Half up by hand: `round` is a library call on a baseline x86-64
    // build, and this runs for every vertex of a shaded trace.
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t + 0.5) as u8;
    Color32::from_rgba_premultiplied(m(ar, br), m(ag, bg), m(ab, bb), m(aa, ba))
}

/// Named families: the body sans, the measuring mono, the display face for
/// headlines and the wordmark face for the product name alone.
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}
/// A named family, its name allocated once: these are asked for hundreds
/// of times a frame, and a clone is a count going up.
macro_rules! named_family {
    ($name:literal) => {{
        static F: std::sync::LazyLock<FontFamily> = std::sync::LazyLock::new(|| FontFamily::Name($name.into()));
        F.clone()
    }};
}
pub fn mono_bold(size: f32) -> FontId {
    FontId::new(size, named_family!("mono-bold"))
}
pub fn sans(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
pub fn sans_semibold(size: f32) -> FontId {
    FontId::new(size, named_family!("sans-600"))
}
pub fn headline(size: f32) -> FontId {
    FontId::new(size, named_family!("sans-700"))
}
pub fn wordmark(size: f32) -> FontId {
    FontId::new(size, named_family!("syne"))
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
    GROUND.store(
        match g {
            Ground::Carbon => 0,
            Ground::Ink => 1,
            Ground::Night => 2,
        },
        Ordering::Relaxed,
    );
    ctx.add_plugin(NightFilter);
    ctx.request_repaint();
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, sans(ty::SMALL)),
            (TextStyle::Body, sans(13.0)),
            (TextStyle::Button, mono(ty::VALUE)),
            (TextStyle::Heading, headline(17.0)),
            (TextStyle::Monospace, mono(ty::BODY)),
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
        v.panel_fill = plate();
        v.window_fill = plate();
        v.extreme_bg_color = ink();
        v.faint_bg_color = surface_2();
        v.code_bg_color = surface_3();
        v.window_stroke = Stroke::new(1.0, line_2());
        v.window_corner_radius = egui::CornerRadius::same(R_WINDOW);
        v.menu_corner_radius = egui::CornerRadius::same(RADIUS);
        v.window_shadow = shadow(Depth::Floating);
        v.popup_shadow = egui::Shadow { blur: 18, offset: [0, 6], ..shadow(Depth::Floating) };
        // What is chosen is amber, in a menu as on a key.
        v.selection.bg_fill = alpha(AMBER, 0.28);
        v.selection.stroke = Stroke::new(1.0, AMBER);
        v.hyperlink_color = AMBER;
        v.warn_fg_color = WARN;
        v.error_fg_color = ERR;
        v.slider_trailing_fill = true;
        // Unavailable keys stay legible (about 3:1 on the panel) while still
        // reading as off; egui's 0.5 left their labels near 2:1.
        v.disabled_alpha = DISABLED_ALPHA;
        v.handle_shape = egui::style::HandleShape::Rect { aspect_ratio: 0.55 };
        v.text_cursor.stroke = Stroke::new(1.5, EMBER);

        // The stock controls wear the keys' glass, flat: a stock button is
        // rare enough not to be worth a mesh, and sits among the real keys.
        let [rest_top, rest_foot, held_top, held_foot] = pal().key;
        let face = lerp(rest_top, rest_foot, 0.5);
        let held = lerp(held_top, held_foot, 0.5);
        let collar = Stroke::new(1.0, Color32::from_black_alpha(190));
        let r = egui::CornerRadius::same(RADIUS);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = surface_2();
        w.noninteractive.weak_bg_fill = surface_2();
        w.noninteractive.bg_stroke = Stroke::new(1.0, line());
        w.noninteractive.fg_stroke = Stroke::new(1.0, text_2());
        w.noninteractive.corner_radius = r;
        w.inactive.bg_fill = face;
        w.inactive.weak_bg_fill = face;
        w.inactive.bg_stroke = collar;
        w.inactive.fg_stroke = Stroke::new(1.0, text());
        w.inactive.corner_radius = r;
        w.hovered.bg_fill = lerp(face, Color32::WHITE, 0.06);
        w.hovered.weak_bg_fill = lerp(face, Color32::WHITE, 0.06);
        w.hovered.bg_stroke = Stroke::new(1.0, line_2());
        w.hovered.fg_stroke = Stroke::new(1.0, text());
        w.hovered.corner_radius = r;
        w.hovered.expansion = 0.0;
        w.active.bg_fill = held;
        w.active.weak_bg_fill = held;
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
        Stroke::new(1.0, Color32::from_white_alpha((LIP as f32 * strength) as u8)),
    );
}

/// A recessed instrument well: the ground sunk into the panel, a shadow
/// along its top inner edge, a lit lip along its foot, and a hairline that
/// takes the tint of what the well belongs to (amber for A, violet for B).
pub fn well(painter: &egui::Painter, r: egui::Rect, tint: Option<Color32>) {
    let cr = egui::CornerRadius::same(RADIUS);
    painter.rect_stroke(r, cr, Stroke::new(1.0, Color32::from_black_alpha(190)), egui::StrokeKind::Outside);
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
        [r.left_bottom() + egui::vec2(RADIUS as f32, 1.5), r.right_bottom() + egui::vec2(-(RADIUS as f32), 1.5)],
        Stroke::new(1.0, Color32::from_white_alpha(LIP)),
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

/// A lamp's outline: what shape it lights in, so a state reads by shape as
/// well as color.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LampShape {
    Round,
    Triangle,
    Square,
}

fn lamp_outline(shape: LampShape, c: egui::Pos2, r: f32) -> Vec<egui::Pos2> {
    match shape {
        LampShape::Round => (0..24)
            .map(|k| {
                let a = k as f32 / 24.0 * std::f32::consts::TAU;
                c + egui::vec2(a.cos(), a.sin()) * r
            })
            .collect(),
        LampShape::Triangle => {
            let r = r * 1.18;
            vec![c + egui::vec2(0.0, -r), c + egui::vec2(r * 0.95, r * 0.7), c + egui::vec2(-r * 0.95, r * 0.7)]
        }
        LampShape::Square => {
            let r = r * 0.88;
            vec![c + egui::vec2(-r, -r), c + egui::vec2(r, -r), c + egui::vec2(r, r), c + egui::vec2(-r, r)]
        }
    }
}

/// A fan from `center` to `outline`, `inner` at the center and `outer` at
/// the edge: a radial gradient in any convex shape.
pub fn radial_fan(painter: &egui::Painter, center: egui::Pos2, outline: &[egui::Pos2], inner: Color32, outer: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(center, inner);
    for p in outline {
        mesh.colored_vertex(*p, outer);
    }
    let n = outline.len() as u32;
    for k in 0..n {
        mesh.add_triangle(0, 1 + k, 1 + (k + 1) % n);
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// An indicator lamp of radius `r`: lit (`lit` 1), a glass bulb glowing in
/// `color` with a halo on the panel and a hot core; dark (0), the same bulb
/// unlit, smoked glass with only its specular highlight, so an off state
/// still reads as a lamp that is off rather than as nothing.
pub fn lamp(painter: &egui::Painter, center: egui::Pos2, r: f32, color: Color32, lit: f32, shape: LampShape) {
    let lit = lit.clamp(0.0, 1.0);
    if lit > 0.01 {
        let halo: Vec<egui::Pos2> = lamp_outline(LampShape::Round, center, r * 2.7);
        radial_fan(painter, center, &halo, alpha(color, 0.32 * lit), alpha(color, 0.0));
    }
    let outline = lamp_outline(shape, center, r);
    let dark_core = lerp(surface_4(), color, 0.22);
    let dark_edge = lerp(ink(), color, 0.10);
    let core = lerp(dark_core, lerp(color, Color32::WHITE, 0.6), lit);
    let edge = lerp(dark_edge, color, lit);
    // The hot center sits a little above middle, where the light falls.
    radial_fan(painter, center + egui::vec2(0.0, -r * 0.15), &outline, core, edge);
    let mut closed = outline.clone();
    closed.push(outline[0]);
    painter.add(egui::Shape::line(closed, Stroke::new(1.0, Color32::from_black_alpha(140))));
    let spec = center + egui::vec2(-r * 0.32, -r * 0.38);
    painter.circle_filled(spec, (r * 0.32).max(0.9), Color32::from_white_alpha((70.0 + 60.0 * (1.0 - lit)) as u8));
}

/// Text stamped into the panel, as if lit from above: the letters' upper
/// walls cast a hairline of shadow and their lower lips catch the light.
pub fn paint_engraved(painter: &egui::Painter, pos: egui::Pos2, galley: std::sync::Arc<egui::Galley>, color: Color32) {
    let lip = if ground() == Ground::Carbon { 30 } else { 20 };
    painter.galley_with_override_text_color(pos + egui::vec2(0.0, 1.0), galley.clone(), Color32::from_white_alpha(lip));
    painter.galley_with_override_text_color(pos + egui::vec2(0.0, -0.7), galley.clone(), Color32::from_black_alpha(150));
    painter.galley_with_override_text_color(pos, galley, color);
}

/// An engraved label: a section's name stamped into the panel, in `font`,
/// letter-spaced by `spacing`. Senses `sense` for labels that also act.
pub fn engraved(ui: &mut egui::Ui, text: &str, font: FontId, color: Color32, spacing: f32, sense: egui::Sense) -> egui::Response {
    let mut job = egui::text::LayoutJob::default();
    job.append(text, 0.0, egui::TextFormat { font_id: font, color: Color32::PLACEHOLDER, extra_letter_spacing: spacing, ..Default::default() });
    let galley = ui.painter().layout_job(job);
    let (rect, resp) = ui.allocate_exact_size(galley.size(), sense);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    paint_engraved(ui.painter(), rect.min, galley, color);
    resp
}

/// Brushed metal: fine horizontal grain, each line its own shade and
/// drifting slowly along its length, as a brushed plate catches the light.
/// A 512 x 64 tile, repeated. Carbon's plate only.
fn brushed(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    if ground() != Ground::Carbon {
        return None;
    }
    let id = egui::Id::new("brushed-metal");
    if let Some(t) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(t);
    }
    let (w, h) = (512usize, 64usize);
    // A small fixed-seed generator: the same plate every run.
    let mut seed = 0x2545_f491_u32;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as f32 / u32::MAX as f32
    };
    let mut px = vec![Color32::TRANSPARENT; w * h];
    for y in 0..h {
        let line = rnd();
        // Knots along the line, smoothed between: the streak's slow drift.
        let knots: Vec<f32> = (0..=16).map(|_| rnd()).collect();
        for x in 0..w {
            let f = x as f32 / w as f32 * 16.0;
            let k = f.floor() as usize;
            let t = f - k as f32;
            let t = t * t * (3.0 - 2.0 * t);
            let drift = knots[k] + (knots[(k + 1) % 16] - knots[k]) * t;
            let grain = rnd();
            let v = 0.45 * line + 0.35 * drift + 0.20 * grain;
            px[y * w + x] = Color32::from_white_alpha((v * 30.0) as u8);
        }
    }
    let tex = ctx.load_texture(
        "brushed-metal",
        egui::ColorImage::new([w, h], px),
        egui::TextureOptions { wrap_mode: egui::TextureWrapMode::Repeat, ..egui::TextureOptions::LINEAR },
    );
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    Some(tex)
}

/// The window's one plate: `fill`, the grain tied to the screen so no panel
/// edge shows a seam, light falling from the top and gone by the foot.
pub fn paint_plate(painter: &egui::Painter, r: egui::Rect, fill: Color32) {
    painter.rect_filled(r, egui::CornerRadius::ZERO, fill);
    if let Some(t) = brushed(painter.ctx()) {
        let uv = egui::Rect::from_min_max(egui::pos2(r.left() / 512.0, r.top() / 64.0), egui::pos2(r.right() / 512.0, r.bottom() / 64.0));
        painter.image(t.id(), r, uv, Color32::WHITE);
    }
    let top = egui::Rect::from_min_max(r.min, egui::pos2(r.right(), r.top() + r.height() * 0.22));
    vgrad(painter, top, Color32::from_white_alpha(13), Color32::TRANSPARENT);
    let foot = egui::Rect::from_min_max(egui::pos2(r.left(), r.bottom() - r.height() * 0.28), r.max);
    vgrad(painter, foot, Color32::TRANSPARENT, Color32::from_black_alpha(56));
}

/// `v` moved to the middle of the physical pixel it falls in, where a
/// one-pixel line is drawn whole. Anywhere else it is spread over two
/// pixels at half strength, differently from line to line, and a grid
/// shimmers as it moves.
pub fn px(ctx: &egui::Context, v: f32) -> f32 {
    let ppp = ctx.pixels_per_point();
    ((v * ppp).floor() + 0.5) / ppp
}

/// A groove cut across the plate at `y`: a dark line and the lit lip under it.
pub fn groove_h(painter: &egui::Painter, x0: f32, x1: f32, y: f32) {
    let y = y.round();
    painter.line_segment([egui::pos2(x0, y + 0.5), egui::pos2(x1, y + 0.5)], Stroke::new(1.0, Color32::from_black_alpha(158)));
    painter.line_segment([egui::pos2(x0, y + 1.5), egui::pos2(x1, y + 1.5)], Stroke::new(1.0, Color32::from_white_alpha(LIP)));
}

/// A groove cut down the plate at `x`.
pub fn groove_v(painter: &egui::Painter, x: f32, y0: f32, y1: f32) {
    let x = x.round();
    painter.line_segment([egui::pos2(x + 0.5, y0), egui::pos2(x + 0.5, y1)], Stroke::new(1.0, Color32::from_black_alpha(158)));
    painter.line_segment([egui::pos2(x + 1.5, y0), egui::pos2(x + 1.5, y1)], Stroke::new(1.0, Color32::from_white_alpha(LIP)));
}

/// A fader's machined cap, its center line cut across the grip.
pub fn fader_cap(painter: &egui::Painter, r: egui::Rect, hover: f32) {
    let stops = [(0.0, rgb(0x8d939b)), (0.45, rgb(0xe7eaee)), (0.47, rgb(0x4b5057)), (0.55, rgb(0xc4c8ce)), (1.0, rgb(0x777c84))];
    painter.rect_stroke(r, egui::CornerRadius::same(R_CAP), Stroke::new(1.0, Color32::BLACK), egui::StrokeKind::Outside);
    let mut m = egui::Mesh::default();
    for (i, (f, c)) in stops.iter().enumerate() {
        let x = r.left() + r.width() * f;
        let c = lerp(*c, Color32::WHITE, 0.12 * hover);
        m.colored_vertex(egui::pos2(x, r.top()), c);
        m.colored_vertex(egui::pos2(x, r.bottom()), c);
        if i > 0 {
            let k = 2 * i as u32;
            m.add_triangle(k - 2, k - 1, k);
            m.add_triangle(k - 1, k + 1, k);
        }
    }
    painter.add(egui::Shape::mesh(m));
}

/// The filled key's relief over its face, `press` 0 up to 1 down. Up, it
/// stands proud of the plate: a lit top edge and a dark foot under it. Down,
/// it sits in the plate: shade falls into it from the top edge and the foot
/// is gone.
pub fn key_face(painter: &egui::Painter, r: egui::Rect, radius: u8, press: f32) {
    let up = 1.0 - press.clamp(0.0, 1.0);
    let rr = radius as f32;
    if up > 0.0 {
        painter.line_segment(
            [r.left_top() + egui::vec2(rr, 1.5), r.right_top() + egui::vec2(-rr, 1.5)],
            Stroke::new(1.0, Color32::from_white_alpha((LIP_SOFT as f32 * up) as u8)),
        );
        painter.line_segment(
            [r.left_bottom() + egui::vec2(rr, 0.5), r.right_bottom() + egui::vec2(-rr, 0.5)],
            Stroke::new(1.0, Color32::from_black_alpha((110.0 * up) as u8)),
        );
    }
    if press > 0.0 {
        let shade = egui::Rect::from_min_size(r.min + egui::vec2(1.0, 1.0), egui::vec2(r.width() - 2.0, (r.height() * 0.45).min(7.0)));
        let mut mesh = egui::Mesh::default();
        let c = Color32::from_black_alpha((120.0 * press) as u8);
        mesh.colored_vertex(shade.left_top(), c);
        mesh.colored_vertex(shade.right_top(), c);
        mesh.colored_vertex(shade.left_bottom(), Color32::TRANSPARENT);
        mesh.colored_vertex(shade.right_bottom(), Color32::TRANSPARENT);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(2, 1, 3);
        painter.add(egui::Shape::mesh(mesh));
    }
}

/// The color a key is lit in, whatever its caller asked for: amber is "on"
/// across the app. Violet stays receiver B's, lemon a caution's (DC on the
/// feed) and ember the live state's, because those say something amber can't.
pub fn on_color(asked: Color32) -> Color32 {
    if asked == VIOLET || asked == WARN || asked == EMBER { asked } else { AMBER }
}

/// What is lit on the LCD: the receiver's color, a little toward white.
pub fn lcd_lit(tint: Color32) -> Color32 {
    lerp(tint, Color32::WHITE, 0.18)
}

/// A vertical gradient over `r`, `top` to `bottom`.
pub fn vgrad(painter: &egui::Painter, r: egui::Rect, top: Color32, bottom: Color32) {
    let mut m = egui::Mesh::default();
    m.colored_vertex(r.left_top(), top);
    m.colored_vertex(r.right_top(), top);
    m.colored_vertex(r.left_bottom(), bottom);
    m.colored_vertex(r.right_bottom(), bottom);
    m.add_triangle(0, 1, 2);
    m.add_triangle(2, 1, 3);
    painter.add(egui::Shape::mesh(m));
}

/// How a key stands this frame: each of the three runs 0 to 1.
#[derive(Clone, Copy)]
pub struct KeyState {
    pub hover: f32,
    pub on: f32,
    pub press: f32,
    /// What lights it, as [`on_color`] gives it.
    pub color: Color32,
    pub focus: bool,
}

/// Every key in the app: a cap of smoked glass set in a dark collar, lit
/// from behind when latched, as a small piece of the LCD. A held key goes
/// darker and its label sinks; a lit one takes the backlight's color through
/// the glass, brightest at its middle.
pub fn key_body(painter: &egui::Painter, r: egui::Rect, radius: egui::CornerRadius, k: KeyState) {
    let [rest_top, rest_foot, held_top, held_foot] = pal().key;
    let on = k.on.clamp(0.0, 1.0);
    let press = k.press.clamp(0.0, 1.0);
    let lift = 0.06 * k.hover * (1.0 - on.max(press));
    let top = lerp(lerp(lerp(rest_top, held_top, press), lerp(rest_top, k.color, 0.07), on), Color32::WHITE, lift);
    let foot = lerp(lerp(lerp(rest_foot, held_foot, press), lerp(held_top, k.color, 0.05), on), Color32::WHITE, lift);
    let inset = (radius.nw as f32).max(2.0);
    // The cap and its collar as one shape, the collar's lit lip under it.
    painter.rect(r, radius, lerp(top, foot, 0.5), Stroke::new(1.0, Color32::from_black_alpha(215)), egui::StrokeKind::Outside);
    painter.line_segment(
        [r.left_bottom() + egui::vec2(inset, 1.5), r.right_bottom() + egui::vec2(-inset, 1.5)],
        Stroke::new(1.0, Color32::from_white_alpha(LIP)),
    );
    // The glass in one mesh: the sheen across its upper half is in the top
    // band's colors. Every key on the window draws this every frame.
    let glass = r.shrink(1.0);
    let mid = glass.center().y;
    let mut m = egui::Mesh::default();
    for (y, c) in [(glass.top(), lerp(top, Color32::WHITE, 0.035)), (mid, lerp(top, foot, 0.5)), (glass.bottom(), foot)] {
        m.colored_vertex(egui::pos2(glass.left(), y), c);
        m.colored_vertex(egui::pos2(glass.right(), y), c);
    }
    for i in [0, 2] {
        m.add_triangle(i, i + 1, i + 2);
        m.add_triangle(i + 2, i + 1, i + 3);
    }
    painter.add(egui::Shape::mesh(m));
    if on > 0.0 {
        let (c, x0, x1, y0, y1) = (glass.center(), glass.left(), glass.right(), glass.top(), glass.bottom());
        let edge = [egui::pos2(x0, y0), egui::pos2(c.x, y0), egui::pos2(x1, y0), egui::pos2(x1, c.y), egui::pos2(x1, y1), egui::pos2(c.x, y1), egui::pos2(x0, y1), egui::pos2(x0, c.y)];
        radial_fan(painter, c, &edge, alpha(k.color, 0.34 * on), alpha(k.color, 0.03 * on));
        painter.rect_stroke(r, radius, Stroke::new(1.0, alpha(k.color, 0.42 * on)), egui::StrokeKind::Inside);
    }
    painter.line_segment(
        [r.left_top() + egui::vec2(inset, 0.5), r.right_top() + egui::vec2(-inset, 0.5)],
        Stroke::new(1.0, Color32::from_white_alpha(LIP_HARD)),
    );
    if k.focus {
        painter.rect_stroke(r.expand(1.5), egui::CornerRadius::same(RADIUS + 1), Stroke::new(1.0, EMBER), egui::StrokeKind::Outside);
    }
}

/// A key's label, in step with its [`KeyState`].
pub fn key_text(k: KeyState, enabled: bool) -> Color32 {
    if !enabled {
        return text_3();
    }
    let off = lerp(text_2(), text(), k.hover);
    lerp(off, lerp(k.color, Color32::WHITE, 0.6), k.on)
}

/// A light along the top edge of `r` in the driven receiver's color: a
/// hairline brightest at `travel` (0 the left end, 1 the right) and fading
/// away from it, with a soft wash falling from it into the panel.
pub fn edge_light(painter: &egui::Painter, r: egui::Rect, color: Color32, travel: f32) {
    let w = r.width();
    let peak = r.left() + w * travel.clamp(0.0, 1.0);
    let xs = [r.left(), peak - w * 0.12, peak, peak + w * 0.5, r.right()];
    let level = |x: f32| -> f32 {
        if x <= peak {
            (1.0 - (peak - x) / (w * 0.12)).clamp(0.0, 1.0)
        } else {
            (1.0 - (x - peak) / (w * 0.5)).clamp(0.0, 1.0)
        }
    };
    let mut cols: Vec<f32> = xs.iter().map(|x| x.clamp(r.left(), r.right())).collect();
    cols.sort_by(f32::total_cmp);
    cols.dedup();
    for (h, a) in [(1.0, 0.85), (10.0, 0.16)] {
        let mut mesh = egui::Mesh::default();
        for x in &cols {
            let i = mesh.vertices.len() as u32;
            mesh.colored_vertex(egui::pos2(*x, r.top()), alpha(color, a * level(*x)));
            mesh.colored_vertex(egui::pos2(*x, r.top() + h), if h > 1.0 { alpha(color, 0.0) } else { alpha(color, a * level(*x)) });
            if i >= 2 {
                mesh.add_triangle(i - 2, i - 1, i);
                mesh.add_triangle(i - 1, i + 1, i);
            }
        }
        painter.add(egui::Shape::mesh(mesh));
    }
}

/// How far a surface stands from the panel. Sunken parts are wells
/// (`well`); raised parts are plates (`plate_shape`), which draw their own
/// short shadow; floating parts are windows, menus and toasts, which cast a
/// long soft one. Overlays drawn on the spectrum float by their frosted glass
/// and rim alone: a shadow there would darken the data around them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Depth {
    Floating,
}

pub fn shadow(d: Depth) -> egui::Shadow {
    match d {
        Depth::Floating => egui::Shadow { offset: [0, 10], blur: 28, spread: 0, color: Color32::from_black_alpha(160) },
    }
}

/// The faceplate's bezel around the display's screen: the plate slopes
/// down into a dark groove at the screen's edge, shaded along its top where
/// the light from above cannot reach and lit along its foot. All of it lies
/// outside `screen`; the display inside is drawn as it is.
pub fn bezel(painter: &egui::Painter, screen: egui::Rect, width: f32) {
    let band = |a: egui::Pos2, b: egui::Pos2, c: egui::Pos2, d: egui::Pos2, inner: Color32| {
        // a-b on the screen's edge, c-d on the plate's.
        let mut mesh = egui::Mesh::default();
        mesh.colored_vertex(a, inner);
        mesh.colored_vertex(b, inner);
        mesh.colored_vertex(c, Color32::TRANSPARENT);
        mesh.colored_vertex(d, Color32::TRANSPARENT);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(2, 1, 3);
        painter.add(egui::Shape::mesh(mesh));
    };
    let o = screen.expand(width);
    let s = screen;
    let dark = Color32::from_black_alpha(120);
    let lit = Color32::from_white_alpha(10);
    band(s.left_top(), s.right_top(), o.left_top(), o.right_top(), dark);
    band(s.left_top(), s.left_bottom(), o.left_top(), o.left_bottom(), Color32::from_black_alpha(80));
    band(s.right_top(), s.right_bottom(), o.right_top(), o.right_bottom(), Color32::from_black_alpha(60));
    band(s.left_bottom(), s.right_bottom(), o.left_bottom(), o.right_bottom(), lit);
    painter.rect_stroke(s, egui::CornerRadius::ZERO, Stroke::new(1.0, Color32::from_black_alpha(200)), egui::StrokeKind::Outside);
    painter.line_segment(
        [s.left_bottom() + egui::vec2(0.0, 1.5), s.right_bottom() + egui::vec2(0.0, 1.5)],
        Stroke::new(1.0, Color32::from_white_alpha(if ground() == Ground::Carbon { LIP } else { LIP_SOFT })),
    );
}

/// A color as Night shows it: its brightness, in red. Premultiplied in and
/// out, so a translucent color stays as translucent; egui's placeholder
/// color passes through for the painter to fill in.
pub fn night_color(c: Color32) -> Color32 {
    if c == Color32::PLACEHOLDER {
        return c;
    }
    let [r, g, b, a] = c.to_array();
    // Rec. 709 luma on the gamma-encoded channels: cheap, and the eye's
    // order of brightness is what has to survive, not exact lightness.
    let l = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
    let red = (l * 1.1).min(a as f32).min(255.0);
    Color32::from_rgba_premultiplied(red as u8, (l * 0.07).min(red) as u8, 0, a)
}

fn night_stroke(s: &mut Stroke) {
    s.color = night_color(s.color);
}

fn night_path_stroke(s: &mut egui::epaint::PathStroke) {
    use egui::epaint::ColorMode;
    s.color = match &s.color {
        ColorMode::Solid(c) => ColorMode::Solid(night_color(*c)),
        ColorMode::UV(f) => {
            let f = f.clone();
            ColorMode::UV(Arc::new(move |r, p| night_color(f(r, p))))
        }
    };
}

fn night_shape(shape: &mut egui::Shape) {
    use egui::Shape;
    match shape {
        Shape::Noop | Shape::Callback(_) => {}
        Shape::Vec(v) => v.iter_mut().for_each(night_shape),
        Shape::Circle(c) => {
            c.fill = night_color(c.fill);
            night_stroke(&mut c.stroke);
        }
        Shape::Ellipse(e) => {
            e.fill = night_color(e.fill);
            night_stroke(&mut e.stroke);
        }
        Shape::LineSegment { stroke, .. } => night_stroke(stroke),
        Shape::Path(p) => {
            p.fill = night_color(p.fill);
            night_path_stroke(&mut p.stroke);
        }
        Shape::Rect(r) => {
            r.fill = night_color(r.fill);
            night_stroke(&mut r.stroke);
        }
        Shape::Text(t) => {
            t.fallback_color = night_color(t.fallback_color);
            t.override_text_color = t.override_text_color.map(night_color);
            night_stroke(&mut t.underline);
            // The galley's own colors are baked into its rows' meshes.
            let g = Arc::make_mut(&mut t.galley);
            for row in &mut g.rows {
                let row = Arc::make_mut(&mut row.row);
                for v in &mut row.visuals.mesh.vertices {
                    v.color = night_color(v.color);
                }
            }
        }
        Shape::Mesh(m) => {
            let m = Arc::make_mut(m);
            for v in &mut m.vertices {
                v.color = night_color(v.color);
            }
        }
        Shape::QuadraticBezier(q) => {
            q.fill = night_color(q.fill);
            night_path_stroke(&mut q.stroke);
        }
        Shape::CubicBezier(c) => {
            c.fill = night_color(c.fill);
            night_path_stroke(&mut c.stroke);
        }
    }
}

/// Turns the whole frame red as it leaves egui, when Night is the ground:
/// one pass over what was drawn, so no part of the app has to know.
struct NightFilter;

impl egui::Plugin for NightFilter {
    fn debug_name(&self) -> &'static str {
        "usdr-night"
    }

    fn output_hook(&mut self, _ctx: &egui::Context, output: &mut egui::FullOutput) {
        if night() {
            for c in &mut output.shapes {
                night_shape(&mut c.shape);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_lit_amber_unless_its_color_says_more() {
        for asked in [AMBER, CYAN, OK, ERR, Color32::from_rgb(1, 2, 3)] {
            assert_eq!(on_color(asked), AMBER);
        }
        for kept in [VIOLET, WARN, EMBER] {
            assert_eq!(on_color(kept), kept);
        }
    }

    #[test]
    fn night_keeps_brightness_order_and_alpha_in_red() {
        let w = night_color(Color32::WHITE);
        let g = night_color(Color32::from_gray(90));
        let [r, gg, b, a] = w.to_array();
        assert!(r > 240 && gg < 30 && b == 0 && a == 255);
        assert!(g.r() < w.r() && g.r() > 60);
        // A translucent color stays as translucent, and valid premultiplied.
        let t = night_color(alpha(AMBER, 0.3));
        assert_eq!(t.a(), alpha(AMBER, 0.3).a());
        assert!(t.r() <= t.a());
        // The painter's placeholder is left for the painter to fill.
        assert_eq!(night_color(Color32::PLACEHOLDER), Color32::PLACEHOLDER);
    }
}
