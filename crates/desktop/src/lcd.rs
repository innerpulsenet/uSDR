//! What the receiver's LCD looks like: a handful of faces to choose among,
//! one of them drawn from the waterfall's own palette. Every face is the
//! same black glass; what changes is what is lit on it. A look is a set of
//! roles rather than one tint, because a color display gives its digits, its
//! captions, its readings and its meter different colors.

use crate::colormap::Colormap;
use crate::theme::{self, alpha, lerp};
use egui::Color32;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LcdStyle {
    /// Backlit amber on black, the house look.
    #[default]
    Amber,
    /// The yellow-green of a traditional LCD.
    GreenLcd,
    /// The mint green of a phosphor screen.
    Phosphor,
    /// White figures with sky-blue captions and a cyan meter.
    #[serde(alias = "BlueTft")]
    Ice,
    /// Orange figures, cyan captions, a three-color meter.
    ColorTft,
    /// Colors taken from the waterfall's palette, and following it.
    Waterfall,
}

impl LcdStyle {
    pub const ALL: [LcdStyle; 6] = [LcdStyle::Amber, LcdStyle::GreenLcd, LcdStyle::Phosphor, LcdStyle::Ice, LcdStyle::ColorTft, LcdStyle::Waterfall];

    pub fn label(self) -> &'static str {
        match self {
            LcdStyle::Amber => "AMBER",
            LcdStyle::GreenLcd => "GREEN LCD",
            LcdStyle::Phosphor => "PHOSPHOR",
            LcdStyle::Ice => "ICE",
            LcdStyle::ColorTft => "COLOR",
            LcdStyle::Waterfall => "WATERFALL",
        }
    }
}

/// The colors of one face.
#[derive(Clone, Copy, Debug)]
pub struct Look {
    /// The black glass, and the backlight falling off from its middle.
    pub panel: Color32,
    pub wash: Color32,
    pub digits: Color32,
    /// The Hz group, a step down from the digits.
    pub hz: Color32,
    pub caption: Color32,
    pub value: Color32,
    /// A lit annunciator, and the word cut out of it.
    pub tag: Color32,
    pub tag_text: Color32,
    /// The S-meter: to S9, to +40, past it.
    pub meter: [Color32; 3],
    /// What is drawn rather than read: the passband, the digit marker.
    pub accent: Color32,
    /// The hairlines between zones.
    pub rule: Color32,
}

const fn rgb(h: u32) -> Color32 {
    Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
}

const NEAR_BLACK: Color32 = rgb(0x06080b);

/// The relative luminance of an opaque color.
fn luminance(c: Color32) -> f32 {
    let f = |v: u8| {
        let s = v as f32 / 255.0;
        if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
}

/// The contrast ratio of two opaque colors.
pub fn contrast(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// `c`, moved toward white just far enough to read at `ratio` on `back`.
fn lifted(c: Color32, back: Color32, ratio: f32) -> Color32 {
    (0..=10).map(|k| lerp(c, Color32::WHITE, k as f32 / 10.0)).find(|x| contrast(*x, back) >= ratio).unwrap_or(Color32::WHITE)
}

/// The glass every face is lit on.
fn panel() -> Color32 {
    lerp(theme::ink(), Color32::BLACK, 0.35)
}

/// A face on the black glass.
fn face(digits: Color32, caption: Color32, value: Color32, tag: Color32, meter: [Color32; 3]) -> Look {
    Look {
        panel: panel(),
        // Just enough of the figures' color to say the glass is lit.
        wash: alpha(digits, 0.06),
        digits,
        hz: lerp(digits, Color32::BLACK, 0.16),
        caption,
        value,
        tag,
        tag_text: NEAR_BLACK,
        meter,
        accent: lerp(tag, digits, 0.25),
        rule: alpha(caption, 0.2),
    }
}

/// The look of `style` for receiver A or B, `palette` being the waterfall's.
/// Night keeps the amber face whatever is chosen, its red being the point.
pub fn look(style: LcdStyle, palette: Colormap, is_b: bool) -> Look {
    let style = if theme::night() { LcdStyle::Amber } else { style };
    let lit_on_glass = |c: Color32| lerp(panel(), c, 0.72);
    let hot_meter = |c: Color32| [c, lerp(c, Color32::WHITE, 0.45), theme::EMBER];
    match style {
        LcdStyle::Amber => {
            let tint = if is_b { theme::VIOLET } else { theme::AMBER };
            let lit = theme::lcd_lit(tint);
            Look { wash: alpha(tint, 0.11), accent: tint, rule: alpha(tint, 0.14), ..face(lit, lit_on_glass(lit), lit, tint, hot_meter(tint)) }
        }
        LcdStyle::GreenLcd => {
            let lit = if is_b { rgb(0x8fe3d0) } else { rgb(0xc4e46a) };
            face(lit, lit_on_glass(lit), lit, lit, hot_meter(lit))
        }
        LcdStyle::Phosphor => {
            let lit = if is_b { rgb(0x7df0ff) } else { rgb(0x7dff9a) };
            face(lit, lit_on_glass(lit), lit, lit, hot_meter(lit))
        }
        LcdStyle::Ice => {
            let (digits, tag) = if is_b { (rgb(0xffe8a0), rgb(0xffc75e)) } else { (rgb(0xf0f6ff), rgb(0x46c8ff)) };
            face(digits, rgb(0x8fc0f0), rgb(0xe4eefb), tag, [rgb(0x4fd8ff), rgb(0xffe45e), rgb(0xff7a66)])
        }
        LcdStyle::ColorTft => {
            let (digits, caption) = if is_b { (rgb(0x63d6ff), rgb(0xffa94d)) } else { (rgb(0xff9d33), rgb(0x63d6ff)) };
            face(digits, caption, rgb(0xf2f5f8), caption, [rgb(0x5ee38f), rgb(0xffc93d), rgb(0xff5a45)])
        }
        LcdStyle::Waterfall => {
            let at = |t: f32| palette.sample(t);
            let back = panel();
            let hot = palette.accent();
            let digits = if is_b { theme::lcd_lit(theme::VIOLET) } else { lifted(lerp(hot, Color32::WHITE, 0.18), back, 7.0) };
            let tag = if is_b { theme::VIOLET } else { lifted(hot, back, 4.5) };
            let caption = lifted(at(0.66), back, 4.5);
            face(digits, caption, lifted(lerp(hot, Color32::WHITE, 0.4), back, 7.0), tag, [lifted(at(0.55), back, 3.0), lifted(at(0.76), back, 3.0), lifted(at(0.93), back, 3.0)])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_look_is_legible_on_every_palette_for_both_receivers() {
        for style in LcdStyle::ALL {
            for palette in Colormap::ALL {
                for is_b in [false, true] {
                    let l = look(style, palette, is_b);
                    let name = format!("{style:?} on {palette:?}, b={is_b}");
                    for (what, c, least) in [("digits", l.digits, 4.5), ("hz", l.hz, 4.5), ("caption", l.caption, 4.5), ("value", l.value, 4.5), ("meter", l.meter[0], 3.0), ("accent", l.accent, 3.0)] {
                        assert!(contrast(c, l.panel) >= least, "{name}: {what} {c:?} is {:.2}", contrast(c, l.panel));
                    }
                    assert!(contrast(l.tag_text, l.tag) >= 4.5, "{name}: the tag's word");
                }
            }
        }
    }

    #[test]
    fn the_waterfall_look_follows_the_palette() {
        let a = look(LcdStyle::Waterfall, Colormap::Glacier, false);
        let b = look(LcdStyle::Waterfall, Colormap::Ember, false);
        assert_ne!(a.digits, b.digits);
        assert_eq!(a.panel, b.panel, "the glass stays black");
    }

    #[test]
    fn receiver_b_reads_apart_from_a() {
        for style in LcdStyle::ALL {
            assert_ne!(look(style, Colormap::Prism, false).digits, look(style, Colormap::Prism, true).digits, "{style:?}");
        }
    }
}
