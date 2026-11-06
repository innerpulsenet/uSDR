//! Temporal noise shaping (ISO/IEC 14496-3 4.6.9): an all-pole filter run
//! across frequency over part of the spectrum, which shapes the coding noise
//! in time within the window (a transient keeps its quantization noise
//! close behind it instead of smeared across the block).

use super::bits::BitReader;
use super::ics::IcsInfo;
use super::tables::BandTables;
use super::AacError;
use super::filterbank::EIGHT_SHORT;

const MAX_ORDER_LONG: usize = 12;
const MAX_ORDER_SHORT: usize = 7;

#[derive(Clone, Copy, Default)]
pub(super) struct Filter {
    length: u8,
    order: u8,
    /// Filter runs from high to low frequency.
    downward: bool,
    /// Prediction coefficients a[1..=order] (a[0] = 1 is implicit).
    lpc: [f32; MAX_ORDER_LONG],
    /// As transmitted, for re-encoding in the tests.
    pub(super) compress: u8,
    pub(super) coef: [u8; MAX_ORDER_LONG],
}

#[cfg(test)]
impl Filter {
    pub(super) fn length(&self) -> u8 {
        self.length
    }
    pub(super) fn order(&self) -> u8 {
        self.order
    }
    pub(super) fn downward(&self) -> bool {
        self.downward
    }
}

#[derive(Clone, Default)]
pub(super) struct Tns {
    /// Up to three filters per window, eight windows.
    pub(super) filters: [[Filter; 3]; 8],
    pub(super) count: [u8; 8],
    pub(super) coef_res: [u8; 8],
}

impl Tns {
    pub fn parse(br: &mut BitReader, info: &IcsInfo) -> Result<Self, AacError> {
        let short = info.seq == EIGHT_SHORT;
        let (n_bits, len_bits, ord_bits, max_order) =
            if short { (1, 4, 3, MAX_ORDER_SHORT) } else { (2, 6, 5, MAX_ORDER_LONG) };
        let mut tns = Tns::default();
        for w in 0..info.num_windows {
            let n = br.read(n_bits) as usize;
            tns.count[w] = n as u8;
            if n == 0 {
                continue;
            }
            let coef_res = br.read(1);
            tns.coef_res[w] = coef_res as u8;
            for f in 0..n {
                let filt = &mut tns.filters[w][f];
                filt.length = br.read(len_bits) as u8;
                let order = br.read(ord_bits) as usize;
                if order > max_order {
                    return Err(AacError::Invalid("TNS filter order too high"));
                }
                filt.order = order as u8;
                if order == 0 {
                    continue;
                }
                filt.downward = br.bit();
                let compress = br.read(1);
                filt.compress = compress as u8;
                let bits = coef_res + 3 - compress;
                // Reflection coefficients, inverse-quantized on an arcsine scale.
                let res_bits = coef_res + 3;
                let half = f64::from(1u32 << (res_bits - 1));
                let iqfac = (half - 0.5) / std::f64::consts::FRAC_PI_2;
                let iqfac_m = (half + 0.5) / std::f64::consts::FRAC_PI_2;
                let mut refl = [0f64; MAX_ORDER_LONG];
                for (i, r) in refl.iter_mut().take(order).enumerate() {
                    let raw = br.read(bits) as i32;
                    filt.coef[i] = raw as u8;
                    let v = if raw >= 1 << (bits - 1) { raw - (1 << bits) } else { raw };
                    let v = f64::from(v);
                    *r = (v / if v >= 0.0 { iqfac } else { iqfac_m }).sin();
                }
                // Step-up recursion from reflection to direct-form coefficients.
                let mut a = [0f64; MAX_ORDER_LONG + 1];
                a[0] = 1.0;
                for m in 1..=order {
                    let k = refl[m - 1];
                    let prev = a;
                    for i in 1..m {
                        a[i] = prev[i] + k * prev[m - i];
                    }
                    a[m] = k;
                }
                for i in 0..order {
                    filt.lpc[i] = a[i + 1] as f32;
                }
            }
        }
        Ok(tns)
    }

    /// Filters `spec` (one channel, deinterleaved windows) in place.
    pub fn apply(&self, info: &IcsInfo, bands: &BandTables, spec: &mut [f32]) {
        let short = info.seq == EIGHT_SHORT;
        let offsets = bands.offsets(short);
        let num_swb = bands.num_swb(short);
        let tns_max = if short { bands.tns_max_short } else { bands.tns_max_long };
        let limit = tns_max.min(info.max_sfb);
        let win_len = if short { bands.short[num_swb] as usize } else { bands.long[num_swb] as usize };
        for w in 0..info.num_windows {
            let base = w * win_len;
            let mut bottom = num_swb;
            for filt in &self.filters[w][..usize::from(self.count[w])] {
                let top = bottom;
                bottom = top.saturating_sub(usize::from(filt.length));
                let order = usize::from(filt.order);
                if order == 0 {
                    continue;
                }
                let start = usize::from(offsets[bottom.min(limit)]);
                let end = usize::from(offsets[top.min(limit)]);
                if end <= start {
                    continue;
                }
                let mut state = [0f32; MAX_ORDER_LONG];
                let lpc = &filt.lpc[..order];
                let len = end - start;
                for i in 0..len {
                    let k = base + if filt.downward { end - 1 - i } else { start + i };
                    let mut y = spec[k];
                    for (s, a) in state.iter().zip(lpc) {
                        y -= s * a;
                    }
                    state.copy_within(0..order - 1, 1);
                    state[0] = y;
                    spec[k] = y;
                }
            }
        }
    }
}
