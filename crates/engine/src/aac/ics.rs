//! One individual channel stream: ics_info, section data, scalefactors,
//! pulse data, TNS side information and the Huffman-coded spectrum, inverse
//! quantized into a spectrum ready for stereo processing (ISO/IEC 14496-3
//! 4.4.2.7, 4.6.1 to 4.6.3).
//!
//! Spectra are kept deinterleaved: window `w` of an eight-short frame sits
//! at `w * short_len`. The bitstream interleaves the windows of a group band
//! by band, so decoding writes each codeword straight to its window.

use super::bits::BitReader;
use super::codebooks::{SCALEFACTOR, SPECTRUM};
use super::filterbank::EIGHT_SHORT;
use super::huffman::Huffman;
use super::tables::BandTables;
use super::tns::Tns;
use super::AacError;
use std::sync::OnceLock;

pub(super) const ZERO_HCB: u8 = 0;
pub(super) const ESC_HCB: u8 = 11;
pub(super) const NOISE_HCB: u8 = 13;
pub(super) const INTENSITY_HCB2: u8 = 14;
pub(super) const INTENSITY_HCB: u8 = 15;

/// Max bands in any long table, plus room.
pub(super) const MAX_SFB: usize = 64;

/// The virtual codebooks 16 to 31 of error-resilient AAC (VCB11): codebook
/// 11 with these largest magnitudes. With the escape lengths they set, they
/// give the longest codewords in `hcr::MAX_CW_LEN`, which the tests check.
pub(super) const VCB11_LAV: [i32; 16] = [15, 31, 47, 63, 95, 127, 159, 191, 223, 255, 319, 383, 511, 767, 1023, 2047];

/// A codebook that codes spectral lines: 1 to 11, or a virtual codebook.
pub(super) fn is_spectral(cb: u8) -> bool {
    (1..=ESC_HCB).contains(&cb) || (16..=31).contains(&cb)
}

/// The Huffman codebook a codebook decodes with.
pub(super) fn real_book(cb: u8) -> u8 {
    if cb >= 16 { ESC_HCB } else { cb }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct IcsInfo {
    pub seq: u8,
    pub shape: u8,
    pub max_sfb: usize,
    pub num_windows: usize,
    pub groups: usize,
    pub group_len: [u8; 8],
}

impl IcsInfo {
    pub fn parse(br: &mut BitReader, bands: &BandTables) -> Result<Self, AacError> {
        if br.bit() {
            return Err(AacError::Invalid("ics_reserved_bit set"));
        }
        let seq = br.read(2) as u8;
        let shape = br.read(1) as u8;
        let mut info = IcsInfo { seq, shape, ..Default::default() };
        if seq == EIGHT_SHORT {
            info.max_sfb = br.read(4) as usize;
            info.set_grouping(br.read(7));
        } else {
            info.max_sfb = br.read(6) as usize;
            info.num_windows = 1;
            info.groups = 1;
            info.group_len[0] = 1;
            if br.bit() {
                // Prediction exists only in AAC Main and LTP, not in LC.
                return Err(AacError::Unsupported("predictor data (AAC Main/LTP)"));
            }
        }
        if info.max_sfb > bands.num_swb(seq == EIGHT_SHORT) {
            return Err(AacError::Invalid("max_sfb beyond the band table"));
        }
        Ok(info)
    }

    pub fn short(&self) -> bool {
        self.seq == EIGHT_SHORT
    }

    /// The window info of a DRM frame: ics_info() without the predictor
    /// bit, which error-resilient AAC-LC does not carry.
    pub fn parse_drm(br: &mut BitReader, bands: &BandTables) -> Result<Self, AacError> {
        if br.bit() {
            return Err(AacError::Invalid("ics_reserved_bit set"));
        }
        let seq = br.read(2) as u8;
        let shape = br.read(1) as u8;
        let mut info = IcsInfo { seq, shape, num_windows: 1, groups: 1, ..Default::default() };
        info.group_len[0] = 1;
        if seq == EIGHT_SHORT {
            info.max_sfb = br.read(4) as usize;
            info.set_grouping(br.read(7));
        } else {
            info.max_sfb = br.read(6) as usize;
        }
        if info.max_sfb > bands.num_swb(seq == EIGHT_SHORT) {
            return Err(AacError::Invalid("max_sfb beyond the band table"));
        }
        Ok(info)
    }

    fn set_grouping(&mut self, grouping: u32) {
        self.num_windows = 8;
        self.groups = 1;
        self.group_len = [0; 8];
        self.group_len[0] = 1;
        for i in 0..7 {
            if grouping & (1 << (6 - i)) != 0 {
                self.group_len[self.groups - 1] += 1;
            } else {
                self.group_len[self.groups] = 1;
                self.groups += 1;
            }
        }
    }
}

/// How one codebook's symbols unpack into spectral values.
struct Book {
    huff: Huffman,
    dim: usize,
    unsigned: bool,
    /// Values per symbol, `dim` of them each.
    values: Vec<i8>,
}

pub(super) struct Codebooks {
    scalefactor: Huffman,
    spectrum: Vec<Book>,
}

impl Codebooks {
    pub fn get() -> &'static Codebooks {
        static BOOKS: OnceLock<Codebooks> = OnceLock::new();
        BOOKS.get_or_init(|| {
            // (dimension, unsigned, largest absolute value) for books 1 to 11.
            const SHAPE: [(usize, bool, i32); 11] = [
                (4, false, 1),
                (4, false, 1),
                (4, true, 2),
                (4, true, 2),
                (2, false, 4),
                (2, false, 4),
                (2, true, 7),
                (2, true, 7),
                (2, true, 12),
                (2, true, 12),
                (2, true, 16),
            ];
            let spectrum = SHAPE
                .iter()
                .zip(SPECTRUM.iter())
                .map(|(&(dim, unsigned, lav), codes)| {
                    let modulo = if unsigned { lav + 1 } else { 2 * lav + 1 };
                    let offset = if unsigned { 0 } else { lav };
                    let mut values = Vec::with_capacity(codes.len() * dim);
                    for sym in 0..codes.len() as i32 {
                        let mut rest = sym;
                        let mut v = [0i8; 4];
                        for d in (0..dim).rev() {
                            v[d] = (rest % modulo - offset) as i8;
                            rest /= modulo;
                        }
                        values.extend_from_slice(&v[..dim]);
                    }
                    Book { huff: Huffman::new(codes), dim, unsigned, values }
                })
                .collect();
            Codebooks { scalefactor: Huffman::new(&SCALEFACTOR), spectrum }
        })
    }

    pub fn scalefactor_delta(&self, br: &mut BitReader) -> i32 {
        self.scalefactor.decode(br) as i32 - 60
    }

    /// One spectral codeword of codebook `cb` (1 to 11, or 16 to 31): the
    /// Huffman word, then sign bits and escapes. A pair beyond a virtual
    /// codebook's limit is zeroed rather than trusted.
    pub fn read_tuple(&self, cb: u8, br: &mut BitReader) -> Result<[i32; 4], AacError> {
        let real = real_book(cb);
        let book = &self.spectrum[usize::from(real) - 1];
        let sym = book.huff.decode(br);
        let mut out = [0i32; 4];
        for (o, &v) in out.iter_mut().zip(&book.values[sym * book.dim..(sym + 1) * book.dim]) {
            *o = i32::from(v);
        }
        if book.unsigned {
            for q in out.iter_mut().take(book.dim) {
                if *q != 0 && br.bit() {
                    *q = -*q;
                }
            }
            if real == ESC_HCB {
                for q in out.iter_mut().take(2) {
                    if q.abs() == 16 {
                        let mut n = 4;
                        while br.bit() {
                            n += 1;
                            if n > 12 {
                                return Err(AacError::Invalid("escape sequence too long"));
                            }
                        }
                        let v = (1 << n) + br.read(n) as i32;
                        *q = if *q < 0 { -v } else { v };
                    }
                }
                if cb >= 16 {
                    let lav = VCB11_LAV[usize::from(cb - 16)];
                    if out[0].abs() > lav || out[1].abs() > lav {
                        out = [0; 4];
                    }
                }
            }
        }
        Ok(out)
    }
}

/// |q|^(4/3) for every quantized magnitude the escape code can carry, plus
/// headroom for pulses.
fn pow43() -> &'static [f32] {
    static T: OnceLock<Vec<f32>> = OnceLock::new();
    T.get_or_init(|| (0..8192 + 16).map(|i| (i as f64).powf(4.0 / 3.0) as f32).collect())
}

#[derive(Clone, Copy, Default)]
struct Pulse {
    count: usize,
    start_sfb: usize,
    offset: [u8; 4],
    amp: [u8; 4],
}

/// A parsed channel stream, its spectrum inverse quantized.
pub(super) struct Ics {
    pub info: IcsInfo,
    /// Codebook per [group][band].
    pub band_type: [[u8; MAX_SFB]; 8],
    /// Scalefactor, intensity position or noise energy per [group][band].
    pub sf: [[i32; MAX_SFB]; 8],
    pub tns: Option<Tns>,
    pub global_gain: i32,
    /// Quantized lines (deinterleaved), kept for re-encoding in the tests.
    pub quant: Vec<i32>,
    pub spec: Vec<f32>,
}

impl Ics {
    pub fn new(frame_len: usize) -> Self {
        Self {
            info: IcsInfo::default(),
            band_type: [[0; MAX_SFB]; 8],
            sf: [[0; MAX_SFB]; 8],
            tns: None,
            global_gain: 0,
            quant: vec![0; 1024],
            spec: vec![0.0; frame_len],
        }
    }

    /// individual_channel_stream(): `common` carries the CPE's shared
    /// ics_info when common_window is set.
    pub fn parse(
        &mut self,
        br: &mut BitReader,
        bands: &BandTables,
        common: Option<IcsInfo>,
        rng: &mut u32,
    ) -> Result<(), AacError> {
        let global_gain = br.read(8) as i32;
        self.global_gain = global_gain;
        self.info = match common {
            Some(info) => info,
            None => IcsInfo::parse(br, bands)?,
        };
        self.parse_sections(br, false)?;
        self.parse_scalefactors(br, global_gain)?;
        let pulse = if br.bit() {
            if self.info.short() {
                return Err(AacError::Invalid("pulse data in a short-window frame"));
            }
            let mut p = Pulse { count: br.read(2) as usize + 1, start_sfb: br.read(6) as usize, ..Default::default() };
            if p.start_sfb >= bands.num_swb(false) {
                return Err(AacError::Invalid("pulse start band beyond the table"));
            }
            for i in 0..p.count {
                p.offset[i] = br.read(5) as u8;
                p.amp[i] = br.read(4) as u8;
            }
            Some(p)
        } else {
            None
        };
        self.tns = if br.bit() { Some(Tns::parse(br, &self.info)?) } else { None };
        if br.bit() {
            return Err(AacError::Unsupported("gain control (AAC SSR)"));
        }
        if br.overrun() {
            return Err(AacError::Truncated);
        }
        self.parse_spectrum(br, bands, pulse, rng)
    }

    /// section_data(); `er` is DRM's aacSectionDataResilienceFlag: 5-bit
    /// codebooks with VCB11, and codebook 11 and the virtual codebooks
    /// always one band per section, without a length.
    fn parse_sections(&mut self, br: &mut BitReader, er: bool) -> Result<(), AacError> {
        let bits = if self.info.short() { 3 } else { 5 };
        let esc = (1 << bits) - 1;
        for g in 0..self.info.groups {
            let mut k = 0;
            while k < self.info.max_sfb {
                let cb = br.read(if er { 5 } else { 4 }) as u8;
                if cb == 12 {
                    return Err(AacError::Invalid("reserved codebook 12"));
                }
                if er && cb == NOISE_HCB {
                    return Err(AacError::Invalid("noise substitution in a DRM frame"));
                }
                let mut len = 0;
                if er && (cb == ESC_HCB || cb >= 16) {
                    if br.overrun() {
                        return Err(AacError::Truncated);
                    }
                    self.band_type[g][k] = cb;
                    k += 1;
                    continue;
                }
                loop {
                    let incr = br.read(bits) as usize;
                    len += incr;
                    if incr != esc {
                        break;
                    }
                    if br.overrun() {
                        return Err(AacError::Truncated);
                    }
                }
                if k + len > self.info.max_sfb {
                    return Err(AacError::Invalid("section runs past max_sfb"));
                }
                if br.overrun() {
                    return Err(AacError::Truncated);
                }
                self.band_type[g][k..k + len].fill(cb);
                k += len;
            }
        }
        Ok(())
    }

    fn parse_scalefactors(&mut self, br: &mut BitReader, global_gain: i32) -> Result<(), AacError> {
        let books = Codebooks::get();
        let mut sf = global_gain;
        let mut is_pos = 0;
        let mut noise = global_gain - 90;
        let mut first_noise = true;
        for g in 0..self.info.groups {
            for b in 0..self.info.max_sfb {
                self.sf[g][b] = match self.band_type[g][b] {
                    ZERO_HCB => 0,
                    INTENSITY_HCB | INTENSITY_HCB2 => {
                        is_pos += books.scalefactor_delta(br);
                        is_pos
                    }
                    NOISE_HCB => {
                        if first_noise {
                            first_noise = false;
                            noise += br.read(9) as i32 - 256;
                        } else {
                            noise += books.scalefactor_delta(br);
                        }
                        noise
                    }
                    _ => {
                        sf += books.scalefactor_delta(br);
                        if !(0..=255).contains(&sf) {
                            return Err(AacError::Invalid("scalefactor out of range"));
                        }
                        sf
                    }
                };
            }
        }
        if br.overrun() {
            return Err(AacError::Truncated);
        }
        Ok(())
    }

    fn parse_spectrum(
        &mut self,
        br: &mut BitReader,
        bands: &BandTables,
        pulse: Option<Pulse>,
        rng: &mut u32,
    ) -> Result<(), AacError> {
        let books = Codebooks::get();
        let short = self.info.short();
        let offsets = bands.offsets(short);
        let win_len = usize::from(*offsets.last().unwrap());
        let quant = &mut self.quant;
        quant.fill(0);
        let mut win = 0;
        for g in 0..self.info.groups {
            let glen = usize::from(self.info.group_len[g]);
            for b in 0..self.info.max_sfb {
                let cb = self.band_type[g][b];
                if !is_spectral(cb) {
                    continue;
                }
                let dim = super::hcr::tuple_len(cb);
                let (lo, hi) = (usize::from(offsets[b]), usize::from(offsets[b + 1]));
                for w in win..win + glen {
                    let base = w * win_len;
                    for k in (lo..hi).step_by(dim) {
                        let v = books.read_tuple(cb, br)?;
                        quant[base + k..base + k + dim].copy_from_slice(&v[..dim]);
                    }
                    if br.overrun() {
                        return Err(AacError::Truncated);
                    }
                }
            }
            win += glen;
        }

        if let Some(p) = pulse {
            let mut k = usize::from(offsets[p.start_sfb]);
            for i in 0..p.count {
                k += usize::from(p.offset[i]);
                if k >= win_len {
                    return Err(AacError::Invalid("pulse past the spectrum"));
                }
                let amp = i32::from(p.amp[i]);
                quant[k] += if quant[k] > 0 { amp } else { -amp };
            }
        }
        self.dequantize(bands, rng);
        Ok(())
    }

    /// The error-resilient side information of a DRM channel (side_info()
    /// with scal_flag set): global gain, VCB11 section data, scalefactors,
    /// then the HCR lengths. Returns (reordered data bits, longest codeword).
    pub fn parse_er_side(&mut self, br: &mut BitReader, info: IcsInfo) -> Result<(usize, usize), AacError> {
        self.info = info;
        self.tns = None;
        let global_gain = br.read(8) as i32;
        self.global_gain = global_gain;
        self.parse_sections(br, true)?;
        self.parse_scalefactors(br, global_gain)?;
        let len = br.read(14) as usize;
        let longest = (br.read(6) as usize).min(49);
        if br.overrun() {
            return Err(AacError::Truncated);
        }
        Ok((len, longest))
    }

    /// The HCR spectral data (`len` bits at the reader's position), then
    /// inverse quantization. Returns whether the HCR decode was clean.
    pub fn parse_hcr(
        &mut self,
        br: &mut BitReader,
        data: &[u8],
        bands: &BandTables,
        len: usize,
        longest: usize,
    ) -> Result<bool, AacError> {
        let order = super::hcr::codeword_order(&self.info, &self.band_type, bands);
        self.quant.fill(0);
        let clean = super::hcr::decode(data, br.position(), len, longest, &order, &mut self.quant)?;
        br.skip(len);
        let mut rng = 0;
        self.dequantize(bands, &mut rng);
        Ok(clean)
    }

    /// Inverse quantization and scaling of `self.quant`; noise substitution.
    fn dequantize(&mut self, bands: &BandTables, rng: &mut u32) {
        let offsets = bands.offsets(self.info.short());
        let win_len = usize::from(*offsets.last().unwrap());
        let quant = &self.quant;
        let pow = pow43();
        self.spec.fill(0.0);
        let mut win = 0;
        for g in 0..self.info.groups {
            let glen = usize::from(self.info.group_len[g]);
            for b in 0..self.info.max_sfb {
                let (lo, hi) = (usize::from(offsets[b]), usize::from(offsets[b + 1]));
                let cb = self.band_type[g][b];
                match cb {
                    ZERO_HCB | INTENSITY_HCB | INTENSITY_HCB2 => {}
                    NOISE_HCB => {
                        let gain = 2f32.powf(0.25 * self.sf[g][b] as f32);
                        for w in win..win + glen {
                            let band = &mut self.spec[w * win_len + lo..w * win_len + hi];
                            let mut energy = 0.0f32;
                            for v in band.iter_mut() {
                                *rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
                                *v = (*rng as i32) as f32;
                                energy += *v * *v;
                            }
                            let scale = gain / energy.sqrt().max(f32::MIN_POSITIVE);
                            for v in band.iter_mut() {
                                *v *= scale;
                            }
                        }
                    }
                    _ => {
                        let gain = 2f32.powf(0.25 * (self.sf[g][b] - 100) as f32);
                        for w in win..win + glen {
                            let base = w * win_len;
                            for k in lo..hi {
                                let q = quant[base + k];
                                let mag = pow[(q.unsigned_abs() as usize).min(pow.len() - 1)] * gain;
                                self.spec[base + k] = if q < 0 { -mag } else { mag };
                            }
                        }
                    }
                }
            }
            win += glen;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::BitWriter;
    use super::*;

    /// A hand-built long-window channel stream with one coded band and two
    /// pulses: one on a zero line (it becomes -amp) and one on a coded line
    /// (its magnitude grows by amp). ffmpeg's encoder never writes pulses,
    /// so this is their only check.
    #[test]
    fn pulses_add_to_the_quantized_lines() {
        let bands = BandTables::new(6, 1024); // 24 kHz: band 0 is lines 0..4
        let mut w = BitWriter::new();
        w.put(100, 8); // global_gain
        w.put(0, 1); // ics_reserved_bit
        w.put(0, 2); // ONLY_LONG
        w.put(0, 1); // sine window
        w.put(1, 6); // max_sfb
        w.put(0, 1); // no prediction
        w.put(1, 4); // section: codebook 1
        w.put(1, 5); // one band
        let (c, l) = SCALEFACTOR[60]; // delta 0: scalefactor 100
        w.put(c, u32::from(l));
        w.put(1, 1); // pulse_data_present
        w.put(1, 2); // two pulses
        w.put(0, 6); // from band 0
        w.put(1, 5); // line 1
        w.put(3, 4); // amp 3
        w.put(1, 5); // line 2
        w.put(2, 4); // amp 2
        w.put(0, 1); // no TNS
        w.put(0, 1); // no gain control
        // Band 0 as one codebook-1 word: (0, 0, 1, 0), index 27*1 + 9*1 + 3*2 + 1.
        let (c, l) = SPECTRUM[0][27 + 9 + 6 + 1];
        w.put(u32::from(c), u32::from(l));
        w.put(0, 8);
        let mut br = BitReader::new(&w.bytes);
        let mut ics = Ics::new(1024);
        let mut rng = 1;
        ics.parse(&mut br, &bands, None, &mut rng).unwrap();
        let p = |q: f32| q.signum() * q.abs().powf(4.0 / 3.0);
        assert_eq!(&ics.spec[..4], &[0.0, p(-3.0), p(3.0), 0.0]);
    }
}
