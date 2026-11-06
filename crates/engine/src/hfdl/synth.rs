//! A transmitter built from the specification, an HF channel (two paths,
//! Watterson fading, white noise) and PDU builders, for the tests.

use super::fec::{conv_encode, fcs, unpack_lsb_first};
use super::phy::{
    CARRIER_HZ, DATA_LEN, MODES, PREKEY_LEN, SCRAMBLE_LEN, SYMBOL_RATE, T_REPEATS, Tables,
    gray_to_index, interleave_map, rrc,
};
use num_complex::Complex64;
use std::f64::consts::PI;

/// xorshift64*, with Gaussian deviates by Box-Muller.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn gauss(&mut self) -> f64 {
        let u = self.uniform().max(1e-300);
        let v = self.uniform();
        (-2.0 * u.ln()).sqrt() * (2.0 * PI * v).cos()
    }
    pub fn cgauss(&mut self) -> Complex64 {
        Complex64::new(self.gauss(), self.gauss()) * std::f64::consts::FRAC_1_SQRT_2
    }
}

/// The symbols of one burst, prekey first, for `user` bytes (zero padded).
pub fn burst_symbols(mode_idx: usize, user: &[u8]) -> Vec<Complex64> {
    let tables = Tables::new();
    let mode = MODES[mode_idx];
    let mut bits = unpack_lsb_first(user);
    assert!(bits.len() <= mode.info_bits(), "PDU too long for the mode");
    bits.resize(mode.info_bits(), 0);
    let coded = conv_encode(&bits);
    let coded: Vec<u8> = if mode.repeat == 2 {
        coded.iter().flat_map(|&c| [c, c]).collect()
    } else {
        coded
    };
    let map = interleave_map(&mode);
    let sent: Vec<u8> = map.iter().map(|&i| coded[i as usize]).collect();
    let m = 1usize << mode.bits_per_symbol;
    let mut out = Vec::with_capacity(PREKEY_LEN + mode.burst_symbols());
    out.extend(std::iter::repeat_n(Complex64::new(-1.0, 0.0), PREKEY_LEN));
    let bp = |s: f32| Complex64::new(s as f64, 0.0);
    out.extend(tables.a.iter().chain(tables.a.iter()).map(|&s| bp(s)));
    out.extend(tables.m[mode_idx].iter().map(|&s| bp(s)));
    for _ in 0..T_REPEATS {
        out.extend(tables.t.iter().map(|&s| bp(s)));
    }
    for (n, chips) in sent.chunks_exact(mode.bits_per_symbol).enumerate() {
        let label = chips.iter().fold(0u8, |acc, &c| (acc << 1) | c);
        let k = gray_to_index(label) as usize;
        let mut s = Complex64::from_polar(1.0, 2.0 * PI * k as f64 / m as f64);
        if tables.scramble[n % SCRAMBLE_LEN] == 1 {
            s = -s;
        }
        out.push(s);
        if n % DATA_LEN == DATA_LEN - 1 {
            out.extend(tables.t.iter().map(|&s| bp(s)));
        }
    }
    assert_eq!(out.len(), PREKEY_LEN + mode.burst_symbols());
    out
}

/// A path of the channel: its delay and mean power share.
#[derive(Clone, Copy)]
pub struct Path {
    pub delay_ms: f64,
    pub gain: f64,
}

#[derive(Clone)]
pub struct Channel {
    pub fs: f64,
    pub freq_offset_hz: f64,
    /// Sample clock error at the receiver: positive runs the symbols long.
    pub clock_ppm: f64,
    pub paths: Vec<Path>,
    /// Watterson Doppler spread (twice the Gaussian spectrum's sigma), Hz;
    /// 0 for fixed paths.
    pub spread_hz: f64,
    /// Signal to noise in 3 kHz, dB; `None` for no noise.
    pub snr_db: Option<f64>,
    /// Signal amplitude (the audio's level).
    pub level: f64,
}

impl Channel {
    pub fn clean(fs: f64) -> Self {
        Self {
            fs,
            freq_offset_hz: 0.0,
            clock_ppm: 0.0,
            paths: vec![Path {
                delay_ms: 0.0,
                gain: 1.0,
            }],
            spread_hz: 0.0,
            snr_db: None,
            level: 0.1,
        }
    }
}

/// Complex Gaussian fading with a Gaussian Doppler spectrum, unit mean
/// power, sampled every `dt` seconds for `n` samples.
fn fading(rng: &mut Rng, spread_hz: f64, n: usize, dt: f64) -> Vec<Complex64> {
    if spread_hz <= 0.0 {
        return vec![Complex64::new(1.0, 0.0); n];
    }
    // White noise through a Gaussian filter whose power response has
    // sigma spread/2; generated at a low rate and interpolated.
    let rate = 50.0;
    let sigma_f = spread_hz / 2.0;
    let sigma_t = 1.0 / (2.0 * PI * std::f64::consts::SQRT_2 * sigma_f);
    let half = (4.0 * sigma_t * rate).ceil() as i64;
    let h: Vec<f64> = (-half..=half)
        .map(|i| (-0.5 * (i as f64 / rate / sigma_t).powi(2)).exp())
        .collect();
    let hn = h.iter().map(|v| v * v).sum::<f64>().sqrt();
    let slow_n = (n as f64 * dt * rate).ceil() as usize + 2;
    let white: Vec<Complex64> = (0..slow_n + h.len()).map(|_| rng.cgauss()).collect();
    let slow: Vec<Complex64> = (0..slow_n)
        .map(|i| {
            h.iter()
                .enumerate()
                .map(|(k, &v)| white[i + k] * v)
                .sum::<Complex64>()
                / hn
        })
        .collect();
    (0..n)
        .map(|i| {
            let t = i as f64 * dt * rate;
            let k = t.floor() as usize;
            let f = t - k as f64;
            slow[k] * (1.0 - f) + slow[k + 1] * f
        })
        .collect()
}

/// Lays bursts into audio. Each burst starts at the given time (seconds).
pub struct Air {
    pub audio: Vec<f32>,
    pub signal_power: f64,
    pub channel: Channel,
    rng: Rng,
    pulse: Vec<f64>,
}

/// Pulse table resolution, points a symbol, and reach, symbols.
const PULSE_RES: usize = 256;
const PULSE_SPAN: usize = 8;

impl Air {
    pub fn new(channel: Channel, seconds: f64, seed: u64) -> Self {
        let n = (seconds * channel.fs) as usize;
        let pulse = (0..=2 * PULSE_SPAN * PULSE_RES)
            .map(|i| rrc((i as f64 / PULSE_RES as f64 - PULSE_SPAN as f64) as f32) as f64)
            .collect();
        Self {
            audio: vec![0.0; n],
            signal_power: 0.0,
            channel,
            rng: Rng::new(seed),
            pulse,
        }
    }

    fn pulse_at(&self, t: f64) -> f64 {
        let x = (t + PULSE_SPAN as f64) * PULSE_RES as f64;
        if x < 0.0 || x >= (2 * PULSE_SPAN * PULSE_RES) as f64 {
            return 0.0;
        }
        let i = x.floor() as usize;
        let f = x - i as f64;
        self.pulse[i] * (1.0 - f) + self.pulse[i + 1] * f
    }

    /// Baseband at time `t` seconds after the burst's first symbol.
    fn baseband(&self, syms: &[Complex64], t: f64, sym_t: f64) -> Complex64 {
        let k = t / sym_t;
        let lo = (k - PULSE_SPAN as f64).ceil().max(0.0) as usize;
        let hi = ((k + PULSE_SPAN as f64).floor() as usize).min(syms.len().saturating_sub(1));
        let mut acc = Complex64::new(0.0, 0.0);
        if lo > hi {
            return acc;
        }
        for (j, s) in syms.iter().enumerate().take(hi + 1).skip(lo) {
            acc += s * self.pulse_at(k - j as f64);
        }
        acc
    }

    pub fn add_burst(&mut self, start_s: f64, syms: &[Complex64]) {
        let ch = self.channel.clone();
        let fs = ch.fs;
        let scale = 1.0 + ch.clock_ppm * 1e-6;
        let sym_t = scale / SYMBOL_RATE;
        let dur = (syms.len() + 2 * PULSE_SPAN) as f64 * sym_t;
        let n0 = (start_s * fs) as usize;
        let n = ((dur * fs) as usize).min(self.audio.len().saturating_sub(n0));
        let fades: Vec<Vec<Complex64>> = ch
            .paths
            .iter()
            .map(|_| fading(&mut self.rng, ch.spread_hz, n, 1.0 / fs))
            .collect();
        let gsum: f64 = ch.paths.iter().map(|p| p.gain).sum();
        let carrier = (CARRIER_HZ + ch.freq_offset_hz) / scale;
        let mut p_sum = 0.0;
        let mut p_n = 0usize;
        for i in 0..n {
            let t = i as f64 / fs;
            let mut v = Complex64::new(0.0, 0.0);
            let mut plain = Complex64::new(0.0, 0.0);
            for (p, f) in ch.paths.iter().zip(fades.iter()) {
                let b = self.baseband(syms, t - p.delay_ms * 1e-3, sym_t) * (p.gain / gsum).sqrt();
                v += b * f[i];
                plain += b;
            }
            let ph = 2.0 * PI * carrier * (n0 + i) as f64 / fs;
            let c = Complex64::from_polar(ch.level, ph);
            self.audio[n0 + i] += (v * c).re as f32;
            let pl = (plain * c).re;
            p_sum += pl * pl;
            p_n += 1;
        }
        self.signal_power = p_sum / p_n.max(1) as f64;
    }

    /// Adds the noise for the channel's SNR, against the power of the last
    /// burst as sent (before fading).
    pub fn add_noise(&mut self) {
        let Some(snr) = self.channel.snr_db else {
            return;
        };
        let fs = self.channel.fs;
        let n3k = self.signal_power / 10f64.powf(snr / 10.0);
        let sigma = (n3k * (fs / 2.0) / 3000.0).sqrt();
        for s in self.audio.iter_mut() {
            *s += (self.rng.gauss() * sigma) as f32;
        }
    }
}

/// Odd parity in bit 8.
fn odd(b: u8) -> u8 {
    let b = b & 0x7f;
    if b.count_ones().is_multiple_of(2) {
        b | 0x80
    } else {
        b
    }
}

fn with_fcs(mut v: Vec<u8>) -> Vec<u8> {
    let c = fcs(&v);
    v.extend_from_slice(&c.to_le_bytes());
    v
}

/// A squitter from `gs` at frame `index`, slot `offset`, with frequency
/// IDs `freqs` in use.
pub fn squitter(gs: u8, index: u16, offset: u8, freqs: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; 64];
    // SPDU, RLS capable, version 0, ISO 8208 supported.
    b[0] = 0x02 | 0x20;
    b[1] = gs | 0x80;
    b[2] = index as u8;
    b[3] = ((index >> 8) as u8 & 0x0f) | offset << 4;
    for (i, v) in b[4..52].iter_mut().enumerate() {
        *v = (i as u8).wrapping_mul(37);
    }
    b[52] = 0x03;
    let systable: u16 = 0x654;
    b[53] = systable as u8;
    let mask: u32 = freqs.iter().fold(0, |m, &f| m | 1 << f);
    b[54] = ((systable >> 8) as u8 & 0x0f) | ((mask & 0x0f) as u8) << 4;
    b[55] = (mask >> 4) as u8;
    b[56] = (mask >> 12) as u8;
    // Two other stations: 7 (synchronized, freqs 1 and 3), 9 (not, freq 0).
    b[57] = 7 | 0x80;
    b[58] = 0x0a;
    b[59] = 0;
    b[60] = (9 & 0x0f) << 4;
    // Station 9's ID high bits (0), not synchronized, frequency 0.
    b[61] = 0x10;
    b[62] = 0;
    b[63] = 0;
    with_fcs(b)
}

/// An ACARS downlink block (SOH to DEL) with parity and block check.
pub fn acars_block(reg: &str, label: &str, msg_no: &str, flight: &str, text: &str) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    body.push(b'2');
    body.extend(format!("{reg:.>7}").bytes());
    body.push(0x15);
    body.extend(label.bytes());
    body.push(b'3');
    body.push(0x02);
    body.extend(msg_no.bytes());
    body.extend(format!("{flight:<6}").bytes());
    body.extend(text.bytes());
    body.push(0x03);
    let mut body: Vec<u8> = body.into_iter().map(odd).collect();
    // ARINC 618's block check: the same reflected CRC, zero start, sent
    // low byte first.
    let crc = super::fec::acars_residue(&body);
    body.extend_from_slice(&crc.to_le_bytes());
    body.push(0x7f);
    let mut out = vec![0x01];
    out.extend(body);
    out
}

/// A downlink MPDU from aircraft `aid` to `gs` with the given LPDUs (each
/// without its check sequence, which is added).
pub fn downlink_mpdu(gs: u8, aid: u8, lpdus: &[Vec<u8>]) -> Vec<u8> {
    let lp: Vec<Vec<u8>> = lpdus.iter().map(|l| with_fcs(l.clone())).collect();
    let mid = 0x01 | 0x02 | ((lp.len() as u8 & 0x0f) << 2);
    let mut h = vec![mid, gs, aid, 0x00, 0x00, 0x00];
    h[0] = if (h[0] & 0x7f).count_ones().is_multiple_of(2) {
        h[0] | 0x80
    } else {
        h[0]
    };
    h.extend(lp.iter().map(|l| (l.len() - 1) as u8));
    let mut out = with_fcs(h);
    for l in lp {
        out.extend(l);
    }
    out
}

/// An uplink MPDU from `gs` to aircraft `aid`.
pub fn uplink_mpdu(gs: u8, aid: u8, lpdus: &[Vec<u8>]) -> Vec<u8> {
    let lp: Vec<Vec<u8>> = lpdus.iter().map(|l| with_fcs(l.clone())).collect();
    // M set, T clear (uplink), one aircraft address field.
    let mut h = vec![0x01, gs, aid, (lp.len() as u8) << 4 | 0x02];
    h.extend(lp.iter().map(|l| (l.len() - 1) as u8));
    let mut out = with_fcs(h);
    for l in lp {
        out.extend(l);
    }
    out
}

/// An ICAO address as the LPDUs carry it.
pub fn icao_bytes(a: u32) -> [u8; 3] {
    [
        ((a >> 16) as u8).reverse_bits(),
        ((a >> 8) as u8).reverse_bits(),
        (a as u8).reverse_bits(),
    ]
}

/// A 20-bit coordinate field.
pub fn coord_field(deg: f64) -> u32 {
    ((deg / 180.0 * 0x7ffff as f64).round() as i32 as u32) & 0xfffff
}

/// A performance data HFNPDU (47 bytes).
pub fn performance_data(
    flight: &str,
    lat: f64,
    lon: f64,
    utc_s: u32,
    gs: u8,
    freq_id: u8,
) -> Vec<u8> {
    let mut h = vec![0u8; 47];
    h[0] = 0xff;
    h[1] = 0xd1;
    h[2..8].copy_from_slice(format!("{flight:<6}").as_bytes());
    let la = coord_field(lat);
    let lo = coord_field(lon);
    h[8] = la as u8;
    h[9] = (la >> 8) as u8;
    h[10] = ((la >> 16) as u8 & 0x0f) | ((lo & 0x0f) as u8) << 4;
    h[11] = (lo >> 4) as u8;
    h[12] = (lo >> 12) as u8;
    let t = (utc_s / 2) as u16;
    h[13..15].copy_from_slice(&t.to_le_bytes());
    h[15] = 1;
    h[17] = gs;
    h[18] = freq_id;
    h
}
