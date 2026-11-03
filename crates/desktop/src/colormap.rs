//! Waterfall ramps, as 256-entry lookup tables.

use egui::Color32;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Colormap {
    /// Ink through deep red, ember and amber to paper: the house ramp.
    #[default]
    Ember,
    /// The web page's ramp: black, navy, blue, cyan, white.
    Ham,
    /// One amber phosphor, dark to bright.
    Phosphor,
    /// The brand's cyan sequential chart ramp, from ink.
    Glacier,
    /// Perceptually even, for reading levels rather than spotting signals.
    Viridis,
    /// The classic panadapter ramp: black through blue, cyan, green and
    /// yellow to red.
    Classic,
    /// The contest-radio ramp: the floor held black, a sparse navy speckle
    /// of noise, signals rising through blue and ice into yellow, orange
    /// and a white-hot red.
    Midnight,
    /// A green CRT phosphor.
    Green,
    /// Black through plum, magenta and orange to a cream white: the heat
    /// map, perceptually ordered and the most colorful of the quiet ones.
    Inferno,
    /// The whole rainbow, violet to red, over a black floor: every level
    /// its own hue, for picking signals apart by strength at a glance.
    Prism,
}

impl Colormap {
    pub const ALL: [Colormap; 10] = [
        Colormap::Ember,
        Colormap::Inferno,
        Colormap::Prism,
        Colormap::Midnight,
        Colormap::Classic,
        Colormap::Ham,
        Colormap::Phosphor,
        Colormap::Green,
        Colormap::Glacier,
        Colormap::Viridis,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Colormap::Ember => "Ember",
            Colormap::Ham => "HAM",
            Colormap::Phosphor => "Phosphor",
            Colormap::Glacier => "Glacier",
            Colormap::Viridis => "Viridis",
            Colormap::Classic => "Classic",
            Colormap::Midnight => "Midnight",
            Colormap::Green => "Green",
            Colormap::Inferno => "Inferno",
            Colormap::Prism => "Prism",
        }
    }

    /// The color a trace or a readout takes to stand for this palette: its
    /// hot end, short of white.
    pub fn accent(self) -> Color32 {
        self.sample(0.84)
    }

    fn stops(self) -> &'static [(f32, [u8; 3])] {
        match self {
            // The floor stays ink for the first third, so a busy span is
            // not a wall of red; the red, ember and amber are kept for
            // what stands above it.
            Colormap::Ember => &[
                (0.00, [0x0a, 0x09, 0x08]),
                (0.30, [0x1a, 0x0c, 0x0a]),
                (0.48, [0x5a, 0x12, 0x10]),
                (0.62, [0xb0, 0x22, 0x1a]),
                (0.74, [0xf4, 0x3a, 0x2a]),
                (0.86, [0xff, 0x9a, 0x3c]),
                (0.95, [0xff, 0xd5, 0x8a]),
                (1.00, [0xf4, 0xf1, 0xec]),
            ],
            Colormap::Ham => &[
                (0.00, [0x00, 0x00, 0x00]),
                (0.25, [0x00, 0x00, 0x5a]),
                (0.50, [0x00, 0x44, 0xff]),
                (0.75, [0x00, 0xe5, 0xff]),
                (1.00, [0xff, 0xff, 0xff]),
            ],
            Colormap::Phosphor => &[
                (0.00, [0x0a, 0x09, 0x08]),
                (0.30, [0x2b, 0x1a, 0x06]),
                (0.65, [0xb0, 0x6c, 0x16]),
                (0.85, [0xff, 0xb3, 0x47]),
                (1.00, [0xff, 0xec, 0xc4]),
            ],
            Colormap::Glacier => &[
                (0.00, [0x0a, 0x09, 0x08]),
                (0.20, [0x0b, 0x22, 0x2b]),
                (0.40, [0x10, 0x55, 0x69]),
                (0.60, [0x26, 0x9a, 0xbb]),
                (0.80, [0x31, 0xbf, 0xe7]),
                (0.92, [0x8c, 0xdf, 0xfb]),
                (1.00, [0xf0, 0xfb, 0xff]),
            ],
            Colormap::Classic => &[
                (0.00, [0x00, 0x00, 0x04]),
                (0.20, [0x00, 0x14, 0x5c]),
                (0.38, [0x00, 0x4c, 0xc8]),
                (0.52, [0x00, 0xb0, 0xf0]),
                (0.66, [0x10, 0xd8, 0x70]),
                (0.78, [0xf0, 0xe0, 0x10]),
                (0.90, [0xff, 0x70, 0x00]),
                (1.00, [0xff, 0x24, 0x24]),
            ],
            // Black for the first third, so the noise reads as a few navy
            // grains rather than a blue wash; blue only for the weak, and
            // no green: past ice blue a signal turns straight to yellow.
            Colormap::Midnight => &[
                (0.00, [0x00, 0x00, 0x00]),
                (0.34, [0x00, 0x03, 0x10]),
                (0.46, [0x00, 0x18, 0x5a]),
                (0.56, [0x0a, 0x3c, 0xc0]),
                (0.64, [0x2a, 0x7c, 0xf0]),
                (0.71, [0x8a, 0xc8, 0xff]),
                (0.78, [0xff, 0xe8, 0x5a]),
                (0.87, [0xff, 0xa0, 0x20]),
                (0.94, [0xff, 0x3a, 0x18]),
                (1.00, [0xff, 0xf0, 0xe6]),
            ],
            Colormap::Green => &[
                (0.00, [0x01, 0x05, 0x02]),
                (0.25, [0x03, 0x26, 0x10]),
                (0.50, [0x08, 0x70, 0x2e]),
                (0.75, [0x28, 0xe0, 0x68]),
                (0.92, [0xa8, 0xff, 0xc8]),
                (1.00, [0xf2, 0xff, 0xf6]),
            ],
            Colormap::Viridis => &[
                (0.00, [0x44, 0x01, 0x54]),
                (0.25, [0x3b, 0x52, 0x8b]),
                (0.50, [0x21, 0x91, 0x8c]),
                (0.75, [0x5e, 0xc9, 0x62]),
                (1.00, [0xfd, 0xe7, 0x25]),
            ],
            // The floor stays near black for the first fifth, so the noise
            // is a dark plum grain; signals climb through magenta into
            // orange and gold, and only the strongest go cream.
            Colormap::Inferno => &[
                (0.00, [0x00, 0x00, 0x04]),
                (0.20, [0x0d, 0x05, 0x23]),
                (0.34, [0x2f, 0x0a, 0x5a]),
                (0.46, [0x5c, 0x12, 0x6e]),
                (0.56, [0x87, 0x21, 0x6b]),
                (0.66, [0xb2, 0x32, 0x5a]),
                (0.75, [0xd9, 0x45, 0x3c]),
                (0.84, [0xf2, 0x6b, 0x1d]),
                (0.91, [0xfb, 0x9d, 0x0a]),
                (0.96, [0xf8, 0xd3, 0x3b]),
                (1.00, [0xfc, 0xff, 0xa4]),
            ],
            // Black, then the spectrum in order: violet for the weakest,
            // blue and cyan for the modest, green and yellow for the
            // strong, red for the loud, white for what pins the scale.
            Colormap::Prism => &[
                (0.00, [0x00, 0x00, 0x00]),
                (0.18, [0x06, 0x02, 0x1a]),
                (0.30, [0x2a, 0x0a, 0x6e]),
                (0.40, [0x18, 0x30, 0xd0]),
                (0.50, [0x0a, 0x8c, 0xff]),
                (0.58, [0x10, 0xe0, 0xf0]),
                (0.66, [0x38, 0xf0, 0x7a]),
                (0.75, [0xe8, 0xf0, 0x20]),
                (0.84, [0xff, 0x9a, 0x10]),
                (0.92, [0xff, 0x2a, 0x2a]),
                (1.00, [0xff, 0xff, 0xff]),
            ],
        }
    }

    pub fn lut(self) -> &'static [Color32; 256] {
        static LUTS: OnceLock<Vec<[Color32; 256]>> = OnceLock::new();
        let luts = LUTS.get_or_init(|| {
            Colormap::ALL
                .iter()
                .map(|m| {
                    let stops = m.stops();
                    std::array::from_fn(|i| {
                        let t = i as f32 / 255.0;
                        let k = stops
                            .windows(2)
                            .position(|w| t <= w[1].0)
                            .unwrap_or(stops.len() - 2);
                        let (t0, a) = stops[k];
                        let (t1, b) = stops[k + 1];
                        let u = ((t - t0) / (t1 - t0).max(1e-6)).clamp(0.0, 1.0);
                        let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * u).round() as u8;
                        Color32::from_rgb(m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2]))
                    })
                })
                .collect()
        });
        let idx = Colormap::ALL.iter().position(|m| *m == self).unwrap_or(0);
        &luts[idx]
    }

    pub fn sample(self, t: f32) -> Color32 {
        self.lut()[(t.clamp(0.0, 1.0) * 255.0) as usize]
    }

    /// The ramp across `r`, black at the left, as one mesh with a hairline
    /// round it: the swatch every palette and style menu shows.
    pub fn paint_ramp(self, painter: &egui::Painter, r: egui::Rect) {
        let steps = 32;
        let mut mesh = egui::Mesh::default();
        for k in 0..=steps {
            let f = k as f32 / steps as f32;
            let c = self.sample(f);
            let x = r.left() + f * r.width();
            mesh.colored_vertex(egui::pos2(x, r.top()), c);
            mesh.colored_vertex(egui::pos2(x, r.bottom()), c);
            if k > 0 {
                let i = mesh.vertices.len() as u32;
                mesh.add_triangle(i - 4, i - 3, i - 2);
                mesh.add_triangle(i - 3, i - 1, i - 2);
            }
        }
        painter.add(egui::Shape::mesh(mesh));
        painter.rect_stroke(r, egui::CornerRadius::same(2), egui::Stroke::new(1.0, crate::theme::line_2()), egui::StrokeKind::Outside);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_palette_runs_dark_to_bright() {
        for m in Colormap::ALL {
            let lum = |c: Color32| 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
            let (lo, hi) = (lum(m.sample(0.0)), lum(m.sample(1.0)));
            assert!(lo < 90.0, "{} starts at luminance {lo}", m.label());
            // Classic ends on a red, so the top need not be white: only
            // far brighter than the floor.
            assert!(hi > lo + 60.0, "{} ends at luminance {hi}", m.label());
            // The floor stays quiet: the first tenth is nearly black.
            assert!(lum(m.sample(0.08)) < 110.0, "{} floor", m.label());
        }
    }
}
