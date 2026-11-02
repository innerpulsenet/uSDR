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
    /// A green CRT phosphor.
    Green,
}

impl Colormap {
    pub const ALL: [Colormap; 7] = [
        Colormap::Ember,
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
            Colormap::Green => "Green",
        }
    }

    fn stops(self) -> &'static [(f32, [u8; 3])] {
        match self {
            Colormap::Ember => &[
                (0.00, [0x0a, 0x09, 0x08]),
                (0.18, [0x1d, 0x0b, 0x0a]),
                (0.38, [0x6e, 0x12, 0x10]),
                (0.56, [0xc4, 0x24, 0x1a]),
                (0.70, [0xf4, 0x3a, 0x2a]),
                (0.84, [0xff, 0x9a, 0x3c]),
                (0.94, [0xff, 0xd5, 0x8a]),
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
}
