//! SBR frequency band tables (ISO/IEC 14496-3 4.6.18.3.2) and the patch
//! layout of the HF generator (4.6.18.6.3), all derived from the header.
//!
//! Every table is in QMF subbands of the 64-band synthesis. The master table
//! spans k0 to k2; the high-resolution table is its top from the crossover
//! band kx, the low-resolution table every other band of that, the noise
//! table a coarser cut again, and the limiter table the low table merged
//! with the patch borders and thinned to the requested bands per octave.

use super::super::AacError;
use super::Header;

/// Start-band offsets by output rate (table 4.82); rows for 16, 22.05, 24,
/// 32, 44.1 to 64, and above 64 kHz.
const START_OFFSET: [[i8; 16]; 6] = [
    [-8, -7, -6, -5, -4, -3, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7],
    [-5, -4, -3, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13],
    [-5, -3, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13, 16],
    [-6, -4, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13, 16],
    [-4, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13, 16, 20],
    [-2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13, 16, 20, 24],
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FreqTables {
    pub k0: usize,
    pub k2: usize,
    /// First SBR subband (the crossover) and the number of SBR subbands.
    pub kx: usize,
    pub m: usize,
    pub master: Vec<usize>,
    pub high: Vec<usize>,
    pub low: Vec<usize>,
    pub noise: Vec<usize>,
    pub limiter: Vec<usize>,
    /// (first source subband, subband count) per patch.
    pub patches: Vec<(usize, usize)>,
}

impl FreqTables {
    pub fn n_high(&self) -> usize {
        self.high.len() - 1
    }
    pub fn n_q(&self) -> usize {
        self.noise.len() - 1
    }
    /// The envelope band table for a frequency resolution (0 low, 1 high).
    pub fn table(&self, res: u8) -> &[usize] {
        if res == 1 { &self.high } else { &self.low }
    }
}

fn nint(x: f32) -> i32 {
    x.round_ties_even() as i32
}

/// `n` band widths from `start` to `stop` on a logarithmic scale.
fn log_bands(start: usize, stop: usize, n: usize) -> Vec<usize> {
    let base = (stop as f32 / start as f32).powf(1.0 / n as f32);
    let mut prod = start as f32;
    let mut prev = start as i32;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n.saturating_sub(1) {
        prod *= base;
        let present = nint(prod);
        out.push((present - prev).max(0) as usize);
        prev = present;
    }
    out.push((stop as i32 - prev).max(0) as usize);
    out
}

fn cumulative(start: usize, widths: &[usize]) -> Result<Vec<usize>, AacError> {
    let mut t = vec![start];
    for &w in widths {
        if w == 0 {
            return Err(AacError::Invalid("SBR band of zero width"));
        }
        t.push(t.last().unwrap() + w);
    }
    Ok(t)
}

impl FreqTables {
    /// Tables for an SBR output rate and header. Fails on a header the
    /// standard does not allow at this rate.
    pub fn new(rate: u32, h: &Header) -> Result<Self, AacError> {
        let row = match rate {
            16000 => 0,
            22050 => 1,
            24000 => 2,
            32000 => 3,
            44100 | 48000 | 64000 => 4,
            r if r >= 88200 => 5,
            _ => return Err(AacError::Config("no SBR at this sample rate")),
        };
        let div = |hz: u32| ((hz * 128 + rate / 2) / rate) as i32;
        let start_min = div(if rate < 32000 { 3000 } else if rate < 64000 { 4000 } else { 5000 });
        let k0 = start_min + i32::from(START_OFFSET[row][usize::from(h.start_freq)]);
        let k2 = match h.stop_freq {
            14 => 2 * k0,
            15 => 3 * k0,
            s => {
                let stop_min = div(if rate < 32000 { 6000 } else if rate < 64000 { 8000 } else { 10000 });
                let mut dk = log_bands(stop_min as usize, 64, 13);
                dk.sort_unstable();
                stop_min + dk[..usize::from(s)].iter().sum::<usize>() as i32
            }
        }
        .min(64);
        if k0 < 1 || k2 <= k0 {
            return Err(AacError::Invalid("SBR start/stop bands out of order"));
        }
        let (k0, k2) = (k0 as usize, k2 as usize);
        let max_span = if rate <= 32000 { 48 } else if rate == 44100 { 35 } else { 32 };
        if k2 - k0 > max_span {
            return Err(AacError::Invalid("SBR range too wide for the sample rate"));
        }

        let master = if h.freq_scale == 0 {
            // Linear spacing, one or two subbands per band.
            let dk = usize::from(h.alter_scale) + 1;
            let n = if dk == 1 { (k2 - k0) / 2 * 2 } else { (k2 - k0 + 2) / 4 * 2 };
            if n == 0 {
                return Err(AacError::Invalid("SBR master table empty"));
            }
            let mut widths = vec![dk as i32; n];
            let diff = (k2 - k0) as i32 - n as i32 * dk as i32;
            if diff < 0 {
                widths[0] -= 1;
                if diff < -1 && n > 1 {
                    widths[1] -= 1;
                }
            } else if diff > 0 {
                widths[n - 1] += 1;
            }
            let widths: Vec<usize> = widths.iter().map(|&w| w.max(0) as usize).collect();
            cumulative(k0, &widths)?
        } else {
            // Logarithmic, in one region or two (above 2 k0 the bands per
            // octave may be warped down).
            let half_bands = [6.0f32, 5.0, 4.0][usize::from(h.freq_scale) - 1];
            let two_regions = 49 * k2 > 110 * k0;
            let k1 = if two_regions { 2 * k0 } else { k2 };
            let n0 = 2 * nint(half_bands * (k1 as f32 / k0 as f32).log2());
            if n0 <= 0 {
                return Err(AacError::Invalid("SBR master table empty"));
            }
            let mut w0 = log_bands(k0, k1, n0 as usize);
            w0.sort_unstable();
            let mut master = cumulative(k0, &w0)?;
            if two_regions {
                let warp = if h.alter_scale == 1 { 1.0 / 1.3 } else { 1.0 };
                let n1 = 2 * nint(half_bands * warp * (k2 as f32 / k1 as f32).log2());
                if n1 <= 0 {
                    return Err(AacError::Invalid("SBR master table empty"));
                }
                let n1 = n1 as usize;
                let mut w1 = log_bands(k1, k2, n1);
                let max0 = *w0.last().unwrap();
                let min1 = *w1.iter().min().unwrap();
                if min1 < max0 {
                    // Keep the bands from narrowing across the region border.
                    w1.sort_unstable();
                    let change = (max0 - w1[0]).min((w1[n1 - 1] - w1[0]) / 2);
                    w1[0] += change;
                    w1[n1 - 1] -= change;
                }
                w1.sort_unstable();
                let upper = cumulative(k1, &w1)?;
                master.extend_from_slice(&upper[1..]);
            }
            master
        };

        let n_master = master.len() - 1;
        let xover = usize::from(h.xover_band);
        if xover >= n_master {
            return Err(AacError::Invalid("SBR crossover above the master table"));
        }
        let high = master[xover..].to_vec();
        let kx = high[0];
        let m = k2 - kx;
        if kx > 32 || kx + m > 64 || m > 48 {
            return Err(AacError::Invalid("SBR range outside the QMF bank"));
        }
        let n_high = high.len() - 1;
        let odd = n_high & 1;
        let n_low = n_high / 2 + odd;
        let mut low = vec![high[0]];
        for i in 1..=n_low {
            low.push(high[2 * i - odd]);
        }
        let n_q = if h.noise_bands == 0 {
            1
        } else {
            nint(f32::from(h.noise_bands) * (k2 as f32 / kx as f32).log2()).max(1) as usize
        };
        if n_q > 5 {
            return Err(AacError::Invalid("more than five SBR noise bands"));
        }
        let mut noise = vec![low[0]];
        let mut i = 0;
        for k in 1..=n_q {
            i += (n_low - i) / (n_q + 1 - k);
            noise.push(low[i]);
        }

        let mut t = FreqTables { k0, k2, kx, m, master, high, low, noise, limiter: Vec::new(), patches: Vec::new() };
        t.make_patches(rate)?;
        t.make_limiter(h.limiter_bands);
        Ok(t)
    }

    /// Which low-band subbands are copied up to which high bands.
    fn make_patches(&mut self, rate: u32) -> Result<(), AacError> {
        let goal = ((2_048_000 + rate / 2) / rate) as usize;
        let (k0, kx, m) = (self.k0, self.kx, self.m);
        let n_master = self.master.len() - 1;
        let mut msb = k0;
        let mut usb = kx;
        let mut k = if goal < kx + m { self.master.iter().position(|&f| f >= goal).unwrap_or(n_master) } else { n_master };
        self.patches.clear();
        let mut sb;
        let mut last = None;
        loop {
            if last == Some((k, msb)) {
                return Err(AacError::Invalid("SBR patch construction does not converge"));
            }
            last = Some((k, msb));
            let mut j = k + 1;
            let mut odd;
            loop {
                j -= 1;
                sb = self.master[j];
                odd = (sb + k0) & 1;
                if j == 0 || sb + odd <= k0 - 1 + msb {
                    break;
                }
            }
            let count = sb.saturating_sub(usb);
            if count > 0 {
                if k0 < odd + count {
                    return Err(AacError::Invalid("SBR patch reaches below subband 0"));
                }
                self.patches.push((k0 - odd - count, count));
                usb = sb;
                msb = sb;
            } else {
                msb = kx;
            }
            if self.master[k] < sb + 3 {
                k = n_master;
            }
            if sb == kx + m {
                break;
            }
            if self.patches.len() > 5 {
                return Err(AacError::Invalid("more than five SBR patches"));
            }
        }
        if self.patches.len() > 1 && self.patches.last().unwrap().1 < 3 {
            self.patches.pop();
        }
        if self.patches.is_empty() {
            return Err(AacError::Invalid("no SBR patches"));
        }
        Ok(())
    }

    /// The limiter bands: the low table plus the patch borders, with bands
    /// narrower than the target bands per octave merged away.
    pub fn make_limiter(&mut self, limiter_bands: u8) {
        if limiter_bands == 0 {
            self.limiter = vec![self.low[0], *self.low.last().unwrap()];
            return;
        }
        let per_octave = [1.2f32, 2.0, 3.0][usize::from(limiter_bands) - 1];
        let mut borders = vec![self.kx];
        for &(_, n) in &self.patches {
            borders.push(borders.last().unwrap() + n);
        }
        let mut t: Vec<usize> = self.low.clone();
        t.extend_from_slice(&borders[1..borders.len() - 1]);
        t.sort_unstable();
        let is_border = |v: usize| borders.contains(&v);
        let mut k = 1;
        while k < t.len() {
            let octaves = (t[k] as f32 / t[k - 1] as f32).log2();
            if octaves * per_octave < 0.49 {
                if t[k] == t[k - 1] {
                    t.remove(k);
                    continue;
                }
                if is_border(t[k]) {
                    if is_border(t[k - 1]) {
                        k += 1;
                    } else {
                        t.remove(k - 1);
                    }
                    continue;
                }
                t.remove(k);
                continue;
            }
            k += 1;
        }
        self.limiter = t;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(start: u8, stop: u8, xover: u8) -> Header {
        Header { start_freq: start, stop_freq: stop, xover_band: xover, ..Header::default() }
    }

    #[test]
    fn tables_nest_and_patches_tile_the_high_band() {
        for rate in [24000u32, 32000, 44100, 48000] {
            for start in 0..16u8 {
                for stop in 0..16u8 {
                    for freq_scale in 0..4u8 {
                        for alter in 0..2u8 {
                            let mut h = header(start, stop, 0);
                            h.freq_scale = freq_scale;
                            h.alter_scale = alter;
                            let Ok(t) = FreqTables::new(rate, &h) else { continue };
                            assert!(t.master.windows(2).all(|w| w[0] < w[1]));
                            assert_eq!((t.master[0], *t.master.last().unwrap()), (t.k0, t.k2));
                            assert_eq!(*t.high.last().unwrap(), t.k2);
                            assert_eq!(*t.low.last().unwrap(), t.k2);
                            assert_eq!(*t.noise.last().unwrap(), t.k2);
                            assert!(t.low.iter().all(|f| t.high.contains(f)));
                            assert!(t.noise.iter().all(|f| t.low.contains(f)));
                            // Patches copy from below kx up to exactly kx + M
                            // (the last one may be dropped if tiny).
                            let total: usize = t.patches.iter().map(|p| p.1).sum();
                            assert!(total <= t.m && total + 2 >= t.m, "{rate} {start} {stop}: {total} of {}", t.m);
                            for &(src, n) in &t.patches {
                                assert!(src + n <= t.kx, "patch source above kx");
                            }
                            // The limiter may stop short of k2 where a short
                            // last patch was dropped (nothing is generated there).
                            assert_eq!(t.limiter[0], t.kx);
                            assert!(t.limiter.windows(2).all(|w| w[0] < w[1]));
                            assert!(*t.limiter.last().unwrap() <= t.k2);
                        }
                    }
                }
            }
        }
    }

    /// Worked by hand through 4.6.18.3.2 and 4.6.18.6.3: 48 kHz output,
    /// bs_start_freq 5, bs_stop_freq 14 (k2 = 2 k0), linear bands.
    #[test]
    fn worked_example_linear_48k() {
        let h = Header { start_freq: 5, stop_freq: 14, freq_scale: 0, alter_scale: 0, ..Header::default() };
        let t = FreqTables::new(48000, &h).unwrap();
        // startMin = NINT(4000 * 128 / 48000) = 11, offset[5] = +2.
        assert_eq!((t.k0, t.k2), (13, 26));
        // dk = 1, numBands = 2 INT(13 / 2) = 12, one subband left over for
        // the top band.
        let mut master: Vec<usize> = (13..=24).collect();
        master.push(26);
        assert_eq!(t.master, master);
        assert_eq!((t.kx, t.m), (13, 13));
        assert_eq!(t.low, vec![13, 15, 17, 19, 21, 23, 26]);
        // N_Q = NINT(2 log2(26 / 13)) = 2.
        assert_eq!(t.noise, vec![13, 19, 26]);
        // goalSb = NINT(2.048e6 / 48000) = 43 is above k2, so patching starts
        // from the top of the master table: 11 subbands from 1 (sb = 24, odd),
        // then 2 more, which is under 3 and dropped.
        assert_eq!(t.patches, vec![(1, 11)]);
        // Two limiter bands per octave: 15, 19 and 23 merge away.
        assert_eq!(t.limiter, vec![13, 17, 21, 26]);
    }
}
