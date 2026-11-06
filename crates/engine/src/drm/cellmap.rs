//! Where everything sits in a transmission super frame for one robustness
//! mode and spectrum occupancy: which cells are pilots (and their known
//! values), which carry the FAC, the SDC and the MSC, and the counts that
//! follow (cells per multiplex frame, dummy cells, SDC cells).
//!
//! The cells are laid out as the standard builds them (clause 8): every
//! cell is data to begin with (SDC in the first two or three symbols of the
//! super frame, FAC at the tabulated positions, MSC elsewhere); gain
//! references are placed over them, then time references in the first
//! symbol of each frame and frequency references in every symbol, whose
//! phases take precedence where they meet a gain reference; the carriers
//! around DC are cleared last.

use super::tables::{self, Mode};
use num_complex::Complex32;
use std::f32::consts::PI;

pub const UNUSED: u8 = 0;
pub const MSC: u8 = 1;
pub const SDC: u8 = 2;
pub const FAC: u8 = 3;
pub const PILOT: u8 = 4;

pub struct CellMap {
    pub mode: Mode,
    pub so: u8,
    pub kmin: i32,
    pub ncar: usize,
    /// Symbols per transmission frame.
    pub ns: usize,
    /// Kind of each cell, by super frame symbol (3 ns of them) and carrier
    /// index (k - kmin). The transmitter of the tests maps by it; the
    /// receiver uses the lists below.
    #[cfg(test)]
    pub kind: Vec<u8>,
    /// The known value of each pilot cell, by frame symbol and carrier
    /// index (pilots repeat every frame); zero elsewhere.
    pub pilot: Vec<Complex32>,
    /// The cell lies on the gain reference grid and is a known pilot, by
    /// frame symbol and carrier index. These are the channel estimator's
    /// samples.
    pub grid: Vec<bool>,
    /// FAC cells in transmission order: (frame symbol, carrier index).
    pub fac: Vec<(usize, usize)>,
    /// SDC cells in transmission order: (super frame symbol, carrier index).
    pub sdc: Vec<(usize, usize)>,
    /// MSC carrier indices of each super frame symbol, ascending.
    pub msc: Vec<Vec<u16>>,
    /// Useful MSC cells per multiplex frame, NMUX.
    pub n_mux: usize,
    /// MSC cells per super frame that carry nothing (the dummy cells).
    #[cfg(test)]
    pub n_dummy: usize,
}

fn unit(phase1024: u16, amp: f32) -> Complex32 {
    Complex32::from_polar(amp, 2.0 * PI * phase1024 as f32 / 1024.0)
}

impl CellMap {
    pub fn new(mode: Mode, so: u8) -> Option<CellMap> {
        let (kmin, kmax) = tables::k_range(mode, so)?;
        let ncar = (kmax - kmin + 1) as usize;
        let ns = mode.symbols_per_frame();
        let nsf = 3 * ns;
        let (x, y, k0) = mode.grid();
        let boost = tables::boosted(mode, so);
        let mut kind = vec![MSC; nsf * ncar];
        let mut pilot = vec![Complex32::new(0.0, 0.0); ns * ncar];
        let mut grid = vec![false; ns * ncar];
        let sqrt2 = std::f32::consts::SQRT_2;
        for sym in 0..nsf {
            let s = sym % ns;
            for (i, k) in (kmin..=kmax).enumerate() {
                let c = &mut kind[sym * ncar + i];
                if sym < mode.sdc_symbols() {
                    *c = SDC;
                }
                if tables::fac_cells(mode).iter().any(|&(fs, ks)| fs as usize == s && ks.contains(&k)) {
                    *c = FAC;
                }
                // Gain references: k = k0 + x (s mod y) + x y p.
                if (k - k0 - x * (s as i32 % y)).rem_euclid(x * y) == 0 {
                    *c = PILOT;
                    let amp = if boost.contains(&k) { 2.0 } else { sqrt2 };
                    pilot[s * ncar + i] = unit(tables::gain_phase(mode, s, k), amp);
                    grid[s * ncar + i] = true;
                }
                if s == 0
                    && let Some(&(_, ph)) = tables::time_pilots(mode).iter().find(|p| p.0 == k)
                {
                    *c = PILOT;
                    pilot[s * ncar + i] = unit(ph, sqrt2);
                }
                if let Some((j, &(_, ph))) = tables::freq_pilots(mode).iter().enumerate().find(|p| p.1.0 == k) {
                    *c = PILOT;
                    // Mode D turns its first two frequency references over
                    // on odd symbols to keep them continuous tones.
                    let ph = if mode == Mode::D && j < 2 && s % 2 == 1 { (ph + 512) % 1024 } else { ph };
                    pilot[s * ncar + i] = unit(ph, sqrt2);
                }
                if mode.unused(k) {
                    *c = UNUSED;
                    pilot[s * ncar + i] = Complex32::new(0.0, 0.0);
                    grid[s * ncar + i] = false;
                }
            }
        }
        let mut fac = Vec::new();
        for s in 0..ns {
            for i in 0..ncar {
                if kind[s * ncar + i] == FAC {
                    fac.push((s, i));
                }
            }
        }
        let mut sdc = Vec::new();
        let mut msc = Vec::with_capacity(nsf);
        let mut total = 0;
        for sym in 0..nsf {
            let mut row = Vec::new();
            for i in 0..ncar {
                match kind[sym * ncar + i] {
                    SDC => sdc.push((sym, i)),
                    MSC => row.push(i as u16),
                    _ => {}
                }
            }
            total += row.len();
            msc.push(row);
        }
        let n_mux = total / 3;
        #[cfg(not(test))]
        let _ = kind;
        Some(CellMap {
            mode,
            so,
            kmin,
            ncar,
            ns,
            #[cfg(test)]
            kind,
            pilot,
            grid,
            fac,
            sdc,
            msc,
            n_mux,
            #[cfg(test)]
            n_dummy: total - 3 * n_mux,
        })
    }

    pub fn n_sdc(&self) -> usize {
        self.sdc.len()
    }

    #[cfg(test)]
    pub fn kind_at(&self, sym: usize, i: usize) -> u8 {
        self.kind[sym * self.ncar + i]
    }

    pub fn pilot_at(&self, s: usize, i: usize) -> Complex32 {
        self.pilot[(s % self.ns) * self.ncar + i]
    }

    pub fn on_grid(&self, s: usize, i: usize) -> bool {
        self.grid[(s % self.ns) * self.ncar + i]
    }

    /// FFT bin of carrier index `i` for an FFT of the mode's useful length.
    #[cfg(test)]
    pub fn bin(&self, i: usize) -> usize {
        let n = self.mode.tu() as i32;
        (self.kmin + i as i32).rem_euclid(n) as usize
    }
}
