//! SBR bitstream syntax (ISO/IEC 14496-3 4.4.2.8, 4.5.2.8): the header,
//! the time/frequency grid, delta-coded envelopes and noise floors,
//! inverse-filtering modes, sinusoid flags and the extension that carries
//! parametric stereo.
//!
//! Envelope and noise values come out absolute (delta decoding undone) but
//! still quantized; dequantization needs both channels of a coupled pair
//! and happens in the HF adjuster.

use super::super::bits::BitReader;
use super::super::AacError;
use super::freq::FreqTables;
use super::tables::books;
use super::Header;
use super::super::huffman::Huffman;

pub(super) const FIXFIX: u8 = 0;
pub(super) const FIXVAR: u8 = 1;
pub(super) const VARFIX: u8 = 2;
pub(super) const VARVAR: u8 = 3;

const EXTENSION_ID_PS: u32 = 2;

pub(super) fn parse_header(br: &mut BitReader) -> Header {
    let mut h = Header {
        amp_res: br.read(1) as u8,
        start_freq: br.read(4) as u8,
        stop_freq: br.read(4) as u8,
        xover_band: br.read(3) as u8,
        ..Header::default()
    };
    br.skip(2);
    let extra1 = br.bit();
    let extra2 = br.bit();
    if extra1 {
        h.freq_scale = br.read(2) as u8;
        h.alter_scale = br.read(1) as u8;
        h.noise_bands = br.read(2) as u8;
    }
    if extra2 {
        h.limiter_bands = br.read(2) as u8;
        h.limiter_gains = br.read(2) as u8;
        h.interpol_freq = br.bit();
        h.smoothing_mode = br.bit();
    }
    h
}

/// One channel's SBR data for one frame.
#[derive(Clone, Debug)]
pub(super) struct ChannelData {
    pub frame_class: u8,
    pub num_env: usize,
    pub num_noise: usize,
    /// Envelope borders in SBR time slots (2 QMF slots each).
    pub t_env: [usize; 6],
    pub t_noise: [usize; 3],
    pub freq_res: [u8; 5],
    /// 0: 1.5 dB steps, 1: 3 dB steps (this frame).
    pub amp_res: u8,
    /// The envelope that starts at a transient, or -1.
    pub l_a: i32,
    pub invf: [u8; 5],
    /// Quantized envelope [envelope][band], absolute.
    pub env: [[i32; 48]; 5],
    /// Quantized noise floor [noise envelope][band], absolute.
    pub noise: [[i32; 5]; 2],
    pub add_harmonic_flag: bool,
    pub add_harmonic: [bool; 48],
}

impl Default for ChannelData {
    fn default() -> Self {
        Self {
            frame_class: 0,
            num_env: 0,
            num_noise: 0,
            t_env: [0; 6],
            t_noise: [0; 3],
            freq_res: [0; 5],
            amp_res: 0,
            l_a: -1,
            invf: [0; 5],
            env: [[0; 48]; 5],
            noise: [[0; 5]; 2],
            add_harmonic_flag: false,
            add_harmonic: [false; 48],
        }
    }
}

/// What delta decoding in time refers to: the previous frame's last
/// envelope and noise floor.
#[derive(Clone, Copy, Debug)]
pub(super) struct DeltaRef {
    pub env: [i32; 48],
    pub freq_res: u8,
    pub noise: [i32; 5],
}

impl Default for DeltaRef {
    fn default() -> Self {
        Self { env: [0; 48], freq_res: 0, noise: [0; 5] }
    }
}

/// Everything one sbr_data() carries.
#[derive(Clone)]
pub(super) struct FrameData {
    pub coupling: bool,
    pub ch: Vec<ChannelData>,
    /// Bit range of a PS extension payload, if one came.
    pub ps: Option<(usize, usize)>,
}

fn ceil_log2(n: usize) -> u32 {
    // Bits for a pointer into 0..=n.
    [0, 1, 2, 2, 3, 3][n]
}

fn parse_grid(br: &mut BitReader, d: &mut ChannelData, header_amp_res: u8, slots: usize) -> Result<(), AacError> {
    d.amp_res = header_amp_res;
    let class = br.read(2) as u8;
    d.frame_class = class;
    let mut pointer = 0usize;
    let num_env;
    match class {
        FIXFIX => {
            num_env = 1usize << br.read(2);
            if num_env > 4 {
                return Err(AacError::Invalid("SBR FIXFIX with more than four envelopes"));
            }
            if num_env == 1 {
                d.amp_res = 0;
            }
            let res = br.read(1) as u8;
            d.freq_res[..num_env].fill(res);
            d.t_env[0] = 0;
            d.t_env[num_env] = slots;
            let step = (slots + num_env / 2) / num_env;
            for l in 1..num_env {
                d.t_env[l] = d.t_env[l - 1] + step;
            }
        }
        FIXVAR => {
            let trail = slots + br.read(2) as usize;
            let n_rel = br.read(2) as usize;
            num_env = n_rel + 1;
            d.t_env[0] = 0;
            d.t_env[num_env] = trail;
            for i in 0..n_rel {
                let rel = 2 * br.read(2) as usize + 2;
                d.t_env[num_env - 1 - i] = d.t_env[num_env - i].saturating_sub(rel);
            }
            pointer = br.read(ceil_log2(num_env)) as usize;
            for i in 0..num_env {
                d.freq_res[num_env - 1 - i] = br.read(1) as u8;
            }
        }
        VARFIX => {
            d.t_env[0] = br.read(2) as usize;
            let n_rel = br.read(2) as usize;
            num_env = n_rel + 1;
            d.t_env[num_env] = slots;
            for i in 0..n_rel {
                d.t_env[i + 1] = d.t_env[i] + 2 * br.read(2) as usize + 2;
            }
            pointer = br.read(ceil_log2(num_env)) as usize;
            for r in d.freq_res.iter_mut().take(num_env) {
                *r = br.read(1) as u8;
            }
        }
        _ => {
            d.t_env[0] = br.read(2) as usize;
            let trail = slots + br.read(2) as usize;
            let n_lead = br.read(2) as usize;
            let n_trail = br.read(2) as usize;
            num_env = n_lead + n_trail + 1;
            if num_env > 5 {
                return Err(AacError::Invalid("SBR VARVAR with more than five envelopes"));
            }
            d.t_env[num_env] = trail;
            for i in 0..n_lead {
                d.t_env[i + 1] = d.t_env[i] + 2 * br.read(2) as usize + 2;
            }
            for i in 0..n_trail {
                let rel = 2 * br.read(2) as usize + 2;
                d.t_env[num_env - 1 - i] = d.t_env[num_env - i].saturating_sub(rel);
            }
            pointer = br.read(ceil_log2(num_env)) as usize;
            for r in d.freq_res.iter_mut().take(num_env) {
                *r = br.read(1) as u8;
            }
        }
    }
    d.num_env = num_env;
    if pointer > num_env + 1 {
        return Err(AacError::Invalid("SBR transient pointer out of range"));
    }
    if (1..=num_env).any(|l| d.t_env[l - 1] >= d.t_env[l]) {
        return Err(AacError::Invalid("SBR envelope borders not increasing"));
    }
    d.num_noise = if num_env > 1 { 2 } else { 1 };
    d.t_noise[0] = d.t_env[0];
    d.t_noise[d.num_noise] = d.t_env[num_env];
    if d.num_noise > 1 {
        let mid = match class {
            FIXFIX => num_env / 2,
            VARFIX => match pointer {
                0 => 1,
                1 => num_env - 1,
                p => p - 1,
            },
            _ => num_env - pointer.saturating_sub(1).max(1),
        };
        d.t_noise[1] = d.t_env[mid];
    }
    d.l_a = match class {
        FIXVAR | VARVAR if pointer > 0 => (num_env + 1 - pointer) as i32,
        VARFIX if pointer > 1 => pointer as i32 - 1,
        _ => -1,
    };
    Ok(())
}

fn parse_dtdf(br: &mut BitReader, d: &ChannelData) -> ([bool; 5], [bool; 2]) {
    let mut env = [false; 5];
    let mut noise = [false; 2];
    for e in env.iter_mut().take(d.num_env) {
        *e = br.bit();
    }
    for n in noise.iter_mut().take(d.num_noise) {
        *n = br.bit();
    }
    (env, noise)
}

fn huff(br: &mut BitReader, book: &(Huffman, i32)) -> i32 {
    book.0.decode(br) as i32 - book.1
}

/// Envelope scalefactors, delta-decoded against `prev`. `balance` is the
/// second channel of a coupled pair, coded as a doubled pan value.
fn parse_envelope(
    br: &mut BitReader,
    d: &mut ChannelData,
    df: &[bool; 5],
    t: &FreqTables,
    prev: &DeltaRef,
    balance: bool,
) -> Result<(), AacError> {
    let b = books();
    let (t_book, f_book, start_bits) = match (balance, d.amp_res) {
        (false, 0) => (&b.t_env_15, &b.f_env_15, 7),
        (false, _) => (&b.t_env_30, &b.f_env_30, 6),
        (true, 0) => (&b.t_bal_15, &b.f_bal_15, 6),
        (true, _) => (&b.t_bal_30, &b.f_bal_30, 5),
    };
    let scale = if balance { 2 } else { 1 };
    let mut last = prev.env;
    let mut last_res = prev.freq_res;
    for l in 0..d.num_env {
        let res = d.freq_res[l];
        let table = t.table(res);
        let n = table.len() - 1;
        let mut cur = [0i32; 48];
        if !df[l] {
            cur[0] = scale * br.read(start_bits) as i32;
            for k in 1..n {
                cur[k] = cur[k - 1] + scale * huff(br, f_book);
            }
        } else {
            let prev_table = t.table(last_res);
            for k in 0..n {
                // The previous envelope's band covering this one's start.
                let j = if res == last_res {
                    k
                } else {
                    let f = table[k];
                    prev_table.iter().rposition(|&p| p <= f).unwrap_or(0).min(prev_table.len() - 2)
                };
                cur[k] = last[j] + scale * huff(br, t_book);
            }
        }
        if cur[..n].iter().any(|&v| !(0..=127).contains(&v)) {
            return Err(AacError::Invalid("SBR envelope value out of range"));
        }
        d.env[l] = cur;
        last = cur;
        last_res = res;
    }
    Ok(())
}

fn parse_noise(
    br: &mut BitReader,
    d: &mut ChannelData,
    df: &[bool; 2],
    n_q: usize,
    prev: &DeltaRef,
    balance: bool,
) -> Result<(), AacError> {
    let b = books();
    let (t_book, f_book) = if balance { (&b.t_noise_bal_30, &b.f_bal_30) } else { (&b.t_noise_30, &b.f_env_30) };
    let scale = if balance { 2 } else { 1 };
    let mut last = prev.noise;
    for l in 0..d.num_noise {
        let mut cur = [0i32; 5];
        if !df[l] {
            cur[0] = scale * br.read(5) as i32;
            for k in 1..n_q {
                cur[k] = cur[k - 1] + scale * huff(br, f_book);
            }
        } else {
            for k in 0..n_q {
                cur[k] = last[k] + scale * huff(br, t_book);
            }
        }
        if cur[..n_q].iter().any(|&v| !(0..=30).contains(&v)) {
            return Err(AacError::Invalid("SBR noise floor out of range"));
        }
        d.noise[l] = cur;
        last = cur;
    }
    Ok(())
}

fn parse_harmonics(br: &mut BitReader, d: &mut ChannelData, n_high: usize) {
    d.add_harmonic_flag = br.bit();
    d.add_harmonic = [false; 48];
    if d.add_harmonic_flag {
        for h in d.add_harmonic.iter_mut().take(n_high) {
            *h = br.bit();
        }
    }
}

fn parse_invf(br: &mut BitReader, d: &mut ChannelData, n_q: usize) {
    for m in d.invf.iter_mut().take(n_q) {
        *m = br.read(2) as u8;
    }
}

/// sbr_data(): one channel (`cpe` false) or a pair.
pub(super) fn parse_data(
    br: &mut BitReader,
    h: &Header,
    t: &FreqTables,
    cpe: bool,
    refs: &[DeltaRef],
    slots: usize,
) -> Result<FrameData, AacError> {
    let n_q = t.n_q();
    let n_high = t.n_high();
    let mut out = FrameData { coupling: false, ch: Vec::new(), ps: None };
    if !cpe {
        if br.bit() {
            br.skip(4);
        }
        let mut d = ChannelData::default();
        parse_grid(br, &mut d, h.amp_res, slots)?;
        let (df_env, df_noise) = parse_dtdf(br, &d);
        parse_invf(br, &mut d, n_q);
        parse_envelope(br, &mut d, &df_env, t, &refs[0], false)?;
        parse_noise(br, &mut d, &df_noise, n_q, &refs[0], false)?;
        parse_harmonics(br, &mut d, n_high);
        out.ch.push(d);
    } else {
        if br.bit() {
            br.skip(8);
        }
        out.coupling = br.bit();
        let mut a = ChannelData::default();
        let mut b = ChannelData::default();
        if out.coupling {
            parse_grid(br, &mut a, h.amp_res, slots)?;
            b = a.clone();
            let (df_env_a, df_noise_a) = parse_dtdf(br, &a);
            let (df_env_b, df_noise_b) = parse_dtdf(br, &b);
            parse_invf(br, &mut a, n_q);
            b.invf = a.invf;
            parse_envelope(br, &mut a, &df_env_a, t, &refs[0], false)?;
            parse_noise(br, &mut a, &df_noise_a, n_q, &refs[0], false)?;
            parse_envelope(br, &mut b, &df_env_b, t, &refs[1], true)?;
            parse_noise(br, &mut b, &df_noise_b, n_q, &refs[1], true)?;
        } else {
            parse_grid(br, &mut a, h.amp_res, slots)?;
            parse_grid(br, &mut b, h.amp_res, slots)?;
            let (df_env_a, df_noise_a) = parse_dtdf(br, &a);
            let (df_env_b, df_noise_b) = parse_dtdf(br, &b);
            parse_invf(br, &mut a, n_q);
            parse_invf(br, &mut b, n_q);
            parse_envelope(br, &mut a, &df_env_a, t, &refs[0], false)?;
            parse_envelope(br, &mut b, &df_env_b, t, &refs[1], false)?;
            parse_noise(br, &mut a, &df_noise_a, n_q, &refs[0], false)?;
            parse_noise(br, &mut b, &df_noise_b, n_q, &refs[1], false)?;
        }
        parse_harmonics(br, &mut a, n_high);
        parse_harmonics(br, &mut b, n_high);
        out.ch.push(a);
        out.ch.push(b);
    }
    if br.bit() {
        // bs_extended_data
        let mut cnt = br.read(4) as usize;
        if cnt == 15 {
            cnt += br.read(8) as usize;
        }
        let mut left = 8 * cnt as isize;
        while left > 7 {
            let id = br.read(2);
            left -= 2;
            if id == EXTENSION_ID_PS && out.ps.is_none() {
                out.ps = Some((br.position(), left as usize));
            }
            // Extensions run to the end of the extended data.
            br.skip(left as usize);
            left = 0;
        }
        br.skip(left.max(0) as usize);
    }
    if br.overrun() {
        return Err(AacError::Truncated);
    }
    Ok(out)
}

/// The reference the next frame's time-delta coding uses.
pub(super) fn delta_ref(d: &ChannelData) -> DeltaRef {
    let last = d.num_env - 1;
    DeltaRef { env: d.env[last], freq_res: d.freq_res[last], noise: d.noise[d.num_noise - 1] }
}
