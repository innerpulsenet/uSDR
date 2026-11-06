//! Builds the HE-AAC and 960-sample test fixtures from the AAC-LC ones.
//!
//! There is no HE-AAC encoder on hand, but ffmpeg's decoder picks up SBR
//! (and, on a mono stream, parametric stereo) implicitly from fill elements
//! in a plain LC stream. So the HE-AAC fixtures are the ffmpeg-encoded LC
//! fixtures with synthetic SBR payloads written into each frame: random but
//! valid side information, varied frame to frame over every frame class,
//! both frequency resolutions and amplitude steps, time and frequency delta
//! coding, coupling, inverse-filtering modes, sinusoids, header changes and
//! resets. ffmpeg then decodes them to the reference PCM, offline.
//!
//! The 960-sample fixture rests on a property of the band tables: below the
//! last band, the 960 tables are the 1024 ones, so an LC frame whose
//! max_sfb stays under the last band parses identically as a 960 frame. The
//! 48 kHz LC stream, rewrapped in LATM with frameLengthFlag set, is then a
//! valid (if odd-sounding) 960-sample stream that ffmpeg decodes too.
//!
//! Run `cargo test -p scannerd-engine --lib aac::fixtures -- --ignored` to
//! rewrite the streams, then decode each with ffmpeg as the integration
//! test's module comment says.

#![allow(clippy::too_many_arguments, clippy::type_complexity)]

use super::sbr::freq::FreqTables;
use super::sbr::tables::data as sbrt;
use super::sbr::Header;
use super::{AacConfig, AacDecoder};

pub(super) struct BitWriter {
    pub bytes: Vec<u8>,
    pub bits: usize,
}

impl BitWriter {
    pub fn new() -> Self {
        Self { bytes: Vec::new(), bits: 0 }
    }

    pub fn put(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if (v >> i) & 1 == 1 {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
    }

    pub fn put_bits_from(&mut self, src: &[u8], nbits: usize) {
        for i in 0..nbits {
            self.put(u32::from((src[i / 8] >> (7 - i % 8)) & 1), 1);
        }
    }

    pub fn align(&mut self) {
        while !self.bits.is_multiple_of(8) {
            self.put(0, 1);
        }
    }
}

pub(super) struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
    pub fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + self.below((hi - lo + 1) as usize) as i32
    }
}

/// ADTS frames: (header bytes, payload).
pub(super) fn adts_split(data: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 7 <= data.len() {
        let h = &data[i..i + 7];
        let len = (usize::from(h[3] & 3) << 11) | (usize::from(h[4]) << 3) | usize::from(h[5] >> 5);
        let hdr = if h[1] & 1 == 1 { 7 } else { 9 };
        out.push((data[i..i + 7].to_vec(), data[i + hdr..i + len].to_vec()));
        i += len;
    }
    out
}

/// One ADTS frame from a template header and a new payload.
pub(super) fn adts_frame(template: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut h = template[..7].to_vec();
    h[1] |= 1; // no CRC
    let len = payload.len() + 7;
    h[3] = (h[3] & !3) | ((len >> 11) & 3) as u8;
    h[4] = (len >> 3) as u8;
    h[5] = (((len & 7) as u8) << 5) | 0x1F;
    h[6] = 0xFC;
    h.extend_from_slice(payload);
    h
}

const NTS: usize = 16;
const FIXFIX: u8 = 0;
const FIXVAR: u8 = 1;
const VARFIX: u8 = 2;
const VARVAR: u8 = 3;

#[derive(Clone)]
struct Grid {
    class: u8,
    num_env: usize,
    t_env: [usize; 6],
    freq_res: [u8; 5],
    pointer: usize,
    var0: usize,
    var1: usize,
    rel0: Vec<usize>,
    rel1: Vec<usize>,
    amp_res: u8,
}

fn ceil_log2(n: usize) -> u32 {
    [0, 1, 2, 2, 3, 3][n]
}

fn gen_grid(rng: &mut Rng, lead: usize, header_amp_res: u8) -> Grid {
    loop {
        let class = if lead > 0 { [VARFIX, VARVAR][rng.below(2)] } else { rng.below(4) as u8 };
        let mut g = Grid {
            class,
            num_env: 1,
            t_env: [0; 6],
            freq_res: [0; 5],
            pointer: 0,
            var0: 0,
            var1: 0,
            rel0: Vec::new(),
            rel1: Vec::new(),
            amp_res: header_amp_res,
        };
        let rel = |rng: &mut Rng| 2 * rng.below(4) + 2;
        match class {
            FIXFIX => {
                g.num_env = [1, 2, 4][rng.below(3)];
                g.t_env[g.num_env] = NTS;
                let step = (NTS + g.num_env / 2) / g.num_env;
                for l in 1..g.num_env {
                    g.t_env[l] = g.t_env[l - 1] + step;
                }
                if g.num_env == 1 {
                    g.amp_res = 0;
                }
            }
            FIXVAR => {
                g.var1 = rng.below(4);
                g.rel1 = (0..rng.below(4)).map(|_| rel(rng)).collect();
                g.num_env = g.rel1.len() + 1;
            }
            VARFIX => {
                g.var0 = lead;
                g.rel0 = (0..rng.below(4)).map(|_| rel(rng)).collect();
                g.num_env = g.rel0.len() + 1;
            }
            _ => {
                g.var0 = lead;
                g.var1 = rng.below(4);
                let n0 = rng.below(3);
                let n1 = rng.below(3);
                g.rel0 = (0..n0).map(|_| rel(rng)).collect();
                g.rel1 = (0..n1).map(|_| rel(rng)).collect();
                g.num_env = n0 + n1 + 1;
            }
        }
        if class != FIXFIX {
            let n = g.num_env;
            g.t_env[0] = g.var0;
            g.t_env[n] = if class == FIXVAR || class == VARVAR { NTS + g.var1 } else { NTS };
            let mut ok = true;
            for (i, r) in g.rel0.iter().enumerate() {
                g.t_env[i + 1] = g.t_env[i] + r;
            }
            for (i, r) in g.rel1.iter().enumerate() {
                if g.t_env[n - i] < *r {
                    ok = false;
                    break;
                }
                g.t_env[n - 1 - i] = g.t_env[n - i] - r;
            }
            if !ok || (1..=n).any(|l| g.t_env[l - 1] >= g.t_env[l]) {
                continue;
            }
        }
        g.pointer = rng.below(g.num_env + 1);
        if class == FIXFIX {
            let r = rng.below(2) as u8;
            g.freq_res[..g.num_env].fill(r);
        } else {
            for l in 0..g.num_env {
                g.freq_res[l] = rng.below(2) as u8;
            }
        }
        return g;
    }
}

fn write_grid(w: &mut BitWriter, g: &Grid) {
    w.put(u32::from(g.class), 2);
    match g.class {
        FIXFIX => {
            w.put(g.num_env.trailing_zeros(), 2);
            w.put(u32::from(g.freq_res[0]), 1);
        }
        FIXVAR => {
            w.put(g.var1 as u32, 2);
            w.put(g.rel1.len() as u32, 2);
            for r in &g.rel1 {
                w.put(((r - 2) / 2) as u32, 2);
            }
            w.put(g.pointer as u32, ceil_log2(g.num_env));
            for i in 0..g.num_env {
                w.put(u32::from(g.freq_res[g.num_env - 1 - i]), 1);
            }
        }
        VARFIX => {
            w.put(g.var0 as u32, 2);
            w.put(g.rel0.len() as u32, 2);
            for r in &g.rel0 {
                w.put(((r - 2) / 2) as u32, 2);
            }
            w.put(g.pointer as u32, ceil_log2(g.num_env));
            for l in 0..g.num_env {
                w.put(u32::from(g.freq_res[l]), 1);
            }
        }
        _ => {
            w.put(g.var0 as u32, 2);
            w.put(g.var1 as u32, 2);
            w.put(g.rel0.len() as u32, 2);
            w.put(g.rel1.len() as u32, 2);
            for r in g.rel0.iter().chain(&g.rel1) {
                w.put(((r - 2) / 2) as u32, 2);
            }
            w.put(g.pointer as u32, ceil_log2(g.num_env));
            for l in 0..g.num_env {
                w.put(u32::from(g.freq_res[l]), 1);
            }
        }
    }
}

fn num_noise(g: &Grid) -> usize {
    if g.num_env > 1 { 2 } else { 1 }
}

/// Values the next frame's time deltas refer to, in coded units.
#[derive(Clone)]
struct Prev {
    env: Vec<i32>,
    res: u8,
    noise: Vec<i32>,
    /// Invalid after a reset or a coupling change.
    valid: bool,
    end: usize,
}

fn huff(w: &mut BitWriter, table: &[(u32, u8)], v: i32) {
    let lav = (table.len() as i32 - 1) / 2;
    assert!(v.abs() <= lav, "delta {v} beyond the codebook");
    let (code, len) = table[(v + lav) as usize];
    w.put(code, u32::from(len));
}

/// Writes one channel's envelopes and returns its new reference. `balance`
/// marks the second channel of a coupled pair; its values are raw pan
/// steps (the decoder doubles them).
#[allow(clippy::too_many_arguments)]
fn write_envelope(
    w: &mut BitWriter,
    rng: &mut Rng,
    g: &Grid,
    df: &[bool],
    t: &FreqTables,
    prev: &mut Prev,
    balance: bool,
    base: i32,
) {
    let (t_tab, f_tab, start_bits): (&[(u32, u8)], &[(u32, u8)], u32) = match (balance, g.amp_res) {
        (false, 0) => (&sbrt::T_ENV_1_5DB, &sbrt::F_ENV_1_5DB, 7),
        (false, _) => (&sbrt::T_ENV_3_0DB, &sbrt::F_ENV_3_0DB, 6),
        (true, 0) => (&sbrt::T_ENV_BAL_1_5DB, &sbrt::F_ENV_BAL_1_5DB, 6),
        (true, _) => (&sbrt::T_ENV_BAL_3_0DB, &sbrt::F_ENV_BAL_3_0DB, 5),
    };
    let scale = if g.amp_res == 0 { 2 } else { 1 };
    for l in 0..g.num_env {
        let table = t.table(g.freq_res[l]);
        let n = table.len() - 1;
        // A gently falling spectrum with some wobble, in 3 dB units first.
        let vals: Vec<i32> = (0..n)
            .map(|k| {
                if balance {
                    base + rng.range(-2, 2)
                } else {
                    let v = base - (k as i32 * 3) / n.max(1) as i32 + rng.range(-1, 1);
                    scale * v.max(1)
                }
            })
            .collect();
        if !df[l] {
            w.put(vals[0] as u32, start_bits);
            for k in 1..n {
                huff(w, f_tab, vals[k] - vals[k - 1]);
            }
        } else {
            let prev_table = t.table(prev.res);
            for k in 0..n {
                let j = if g.freq_res[l] == prev.res {
                    k
                } else {
                    prev_table.iter().rposition(|&p| p <= table[k]).unwrap_or(0).min(prev_table.len() - 2)
                };
                huff(w, t_tab, vals[k] - prev.env[j]);
            }
        }
        prev.env = vals;
        prev.res = g.freq_res[l];
    }
}

fn write_noise(w: &mut BitWriter, rng: &mut Rng, g: &Grid, df: &[bool], n_q: usize, prev: &mut Prev, balance: bool, base: i32) {
    let (t_tab, f_tab): (&[(u32, u8)], &[(u32, u8)]) = if balance {
        (&sbrt::T_NOISE_BAL_3_0DB, &sbrt::F_ENV_BAL_3_0DB)
    } else {
        (&sbrt::T_NOISE_3_0DB, &sbrt::F_ENV_3_0DB)
    };
    for l in 0..num_noise(g) {
        let vals: Vec<i32> = (0..n_q).map(|_| (base + rng.range(-2, 2)).clamp(0, 30)).collect();
        if !df[l] {
            w.put(vals[0] as u32, 5);
            for k in 1..n_q {
                huff(w, f_tab, vals[k] - vals[k - 1]);
            }
        } else {
            for k in 0..n_q {
                huff(w, t_tab, vals[k] - prev.noise[k]);
            }
        }
        prev.noise = vals;
    }
}

fn gen_header(rng: &mut Rng, rate: u32) -> (Header, FreqTables) {
    loop {
        let h = Header {
            amp_res: rng.below(2) as u8,
            start_freq: rng.range(2, 10) as u8,
            stop_freq: rng.range(3, 13) as u8,
            xover_band: rng.below(3) as u8,
            freq_scale: rng.below(4) as u8,
            alter_scale: rng.below(2) as u8,
            noise_bands: rng.below(4) as u8,
            limiter_bands: rng.below(4) as u8,
            limiter_gains: rng.below(4) as u8,
            interpol_freq: rng.chance(50),
            smoothing_mode: rng.chance(50),
        };
        if let Ok(t) = FreqTables::new(rate, &h)
            && t.n_high() >= 2
        {
            return (h, t);
        }
    }
}

fn write_header(w: &mut BitWriter, h: &Header) {
    w.put(u32::from(h.amp_res), 1);
    w.put(u32::from(h.start_freq), 4);
    w.put(u32::from(h.stop_freq), 4);
    w.put(u32::from(h.xover_band), 3);
    w.put(0, 2);
    w.put(1, 1);
    w.put(1, 1);
    w.put(u32::from(h.freq_scale), 2);
    w.put(u32::from(h.alter_scale), 1);
    w.put(u32::from(h.noise_bands), 2);
    w.put(u32::from(h.limiter_bands), 2);
    w.put(u32::from(h.limiter_gains), 2);
    w.put(u32::from(h.interpol_freq), 1);
    w.put(u32::from(h.smoothing_mode), 1);
}

/// Synthetic SBR side information for a stream, frame by frame.
pub(super) struct SbrGen {
    rng: Rng,
    rate: u32,
    header: Header,
    tables: FreqTables,
    prev: Vec<Prev>,
    coupling_prev: bool,
    frame: usize,
    /// Extra bits for the extended data (PS), written by the caller.
    pub extension: Option<(Vec<u8>, usize)>,
}

impl SbrGen {
    pub fn new(seed: u64, rate: u32, channels: usize) -> Self {
        let mut rng = Rng::new(seed);
        let (header, tables) = gen_header(&mut rng, rate);
        let prev = Prev { env: vec![0; 48], res: 0, noise: vec![0; 5], valid: false, end: NTS };
        Self { rng, rate, header, tables, prev: vec![prev; channels], coupling_prev: false, frame: 0, extension: None }
    }

    /// One sbr_extension_data() after its extension type, for a frame.
    pub fn payload(&mut self, crc: bool) -> BitWriter {
        let mut w = BitWriter::new();
        if crc {
            w.put(0x2A5, 10);
        }
        let f = self.frame;
        self.frame += 1;
        // A fresh header now and then; at frame 6 one that keeps the tables,
        // at frame 11 one that resets them.
        let mut send_header = f.is_multiple_of(5) || self.rng.chance(10);
        if f == 6 {
            let mut h = self.header;
            h.limiter_bands = (h.limiter_bands + 1) % 4;
            h.limiter_gains = (h.limiter_gains + 1) % 4;
            h.interpol_freq = !h.interpol_freq;
            h.amp_res ^= 1;
            self.tables.make_limiter(h.limiter_bands);
            self.header = h;
            send_header = true;
        }
        if f == 11 {
            let (h, t) = gen_header(&mut self.rng, self.rate);
            self.header = h;
            self.tables = t;
            for p in &mut self.prev {
                p.valid = false;
            }
            send_header = true;
        }
        w.put(u32::from(send_header), 1);
        if send_header {
            write_header(&mut w, &self.header);
        }
        let n_q = self.tables.n_q();
        let n_high = self.tables.n_high();
        let rng = &mut self.rng;
        let t = &self.tables;
        let amp = self.header.amp_res;
        let nch = self.prev.len();
        w.put(0, 1); // bs_data_extra
        if nch == 1 {
            let g = gen_grid(rng, self.prev[0].end - NTS, amp);
            write_grid(&mut w, &g);
            let p = &mut self.prev[0];
            let df_env: Vec<bool> = (0..g.num_env).map(|l| (l > 0 || p.valid) && rng.chance(50)).collect();
            let df_noise: Vec<bool> = (0..num_noise(&g)).map(|l| (l > 0 || p.valid) && rng.chance(50)).collect();
            for &d in df_env.iter().chain(&df_noise) {
                w.put(u32::from(d), 1);
            }
            for _ in 0..n_q {
                w.put(rng.below(4) as u32, 2);
            }
            write_envelope(&mut w, rng, &g, &df_env, t, p, false, 17);
            write_noise(&mut w, rng, &g, &df_noise, n_q, p, false, 12);
            write_harmonics(&mut w, rng, n_high);
            p.valid = true;
            p.end = g.t_env[g.num_env];
        } else {
            // Coupling needs both channels on the same grid border.
            let same_lead = self.prev[0].end == self.prev[1].end;
            let coupling = same_lead && rng.chance(60);
            w.put(u32::from(coupling), 1);
            if coupling != self.coupling_prev {
                self.prev[1].valid = false;
            }
            self.coupling_prev = coupling;
            let grids: Vec<Grid> = if coupling {
                let g = gen_grid(rng, self.prev[0].end - NTS, amp);
                vec![g.clone(), g]
            } else {
                (0..2).map(|c| gen_grid(rng, self.prev[c].end - NTS, amp)).collect()
            };
            write_grid(&mut w, &grids[0]);
            if !coupling {
                write_grid(&mut w, &grids[1]);
            }
            let mut dfs = Vec::new();
            for c in 0..2 {
                let p = &self.prev[c];
                let g = &grids[c];
                let df_env: Vec<bool> = (0..g.num_env).map(|l| (l > 0 || p.valid) && rng.chance(50)).collect();
                let df_noise: Vec<bool> = (0..num_noise(g)).map(|l| (l > 0 || p.valid) && rng.chance(50)).collect();
                for &d in df_env.iter().chain(&df_noise) {
                    w.put(u32::from(d), 1);
                }
                dfs.push((df_env, df_noise));
            }
            for _ in 0..(if coupling { 1 } else { 2 }) {
                for _ in 0..n_q {
                    w.put(rng.below(4) as u32, 2);
                }
            }
            let (p0, p1) = self.prev.split_at_mut(1);
            let (p0, p1) = (&mut p0[0], &mut p1[0]);
            if coupling {
                // Level, then balance (pan center is 6 raw steps at 3 dB,
                // 12 at 1.5 dB; the noise balance center is 6).
                let center = if grids[0].amp_res == 1 { 6 } else { 12 };
                write_envelope(&mut w, rng, &grids[0], &dfs[0].0, t, p0, false, 16);
                write_noise(&mut w, rng, &grids[0], &dfs[0].1, n_q, p0, false, 12);
                write_envelope(&mut w, rng, &grids[1], &dfs[1].0, t, p1, true, center);
                write_noise(&mut w, rng, &grids[1], &dfs[1].1, n_q, p1, true, 6);
            } else {
                write_envelope(&mut w, rng, &grids[0], &dfs[0].0, t, p0, false, 17);
                write_envelope(&mut w, rng, &grids[1], &dfs[1].0, t, p1, false, 15);
                write_noise(&mut w, rng, &grids[0], &dfs[0].1, n_q, p0, false, 12);
                write_noise(&mut w, rng, &grids[1], &dfs[1].1, n_q, p1, false, 10);
            }
            write_harmonics(&mut w, rng, n_high);
            write_harmonics(&mut w, rng, n_high);
            for (c, p) in [p0, p1].into_iter().enumerate() {
                p.valid = true;
                p.end = grids[c].t_env[grids[c].num_env];
            }
        }
        match self.extension.take() {
            Some((bytes, bits)) => {
                // bs_extended_data with one extension (id in the first bits).
                w.put(1, 1);
                let cnt = bits.div_ceil(8);
                if cnt >= 15 {
                    w.put(15, 4);
                    w.put((cnt - 15) as u32, 8);
                } else {
                    w.put(cnt as u32, 4);
                }
                w.put_bits_from(&bytes, bits);
                for _ in bits..8 * cnt {
                    w.put(0, 1);
                }
            }
            None => w.put(0, 1),
        }
        w
    }
}

fn write_harmonics(w: &mut BitWriter, rng: &mut Rng, n_high: usize) {
    let on = rng.chance(40);
    w.put(u32::from(on), 1);
    if on {
        for _ in 0..n_high {
            w.put(u32::from(rng.chance(25)), 1);
        }
    }
}

/// Rewrites a raw_data_block with a fill element carrying `sbr` (an
/// sbr_extension_data() without its type) before ID_END.
pub(super) fn with_fill(frame: &[u8], end_bit: usize, sbr: &BitWriter, crc: bool) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.put_bits_from(frame, end_bit);
    let bits = 4 + sbr.bits;
    let count = bits.div_ceil(8);
    w.put(6, 3); // ID_FIL
    if count >= 15 {
        w.put(15, 4);
        w.put((count - 14) as u32, 8);
    } else {
        w.put(count as u32, 4);
    }
    w.put(if crc { 14 } else { 13 }, 4);
    w.put_bits_from(&sbr.bytes, sbr.bits);
    for _ in bits..8 * count {
        w.put(0, 1);
    }
    w.put(7, 3); // ID_END
    w.align();
    w.bytes
}

fn data_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/aac")
}

/// LC frames of a fixture with the bit position of each one's ID_END.
pub(super) fn lc_frames(name: &str, frames: usize) -> Vec<(Vec<u8>, Vec<u8>, usize)> {
    let data = std::fs::read(data_dir().join(format!("{name}.aac"))).unwrap();
    let split = adts_split(&data);
    let h = &split[0].0;
    let rate = super::tables::SAMPLE_RATES[usize::from((h[2] >> 2) & 0xF)];
    let channels = ((h[2] & 1) << 2) | (h[3] >> 6);
    let mut dec = AacDecoder::new(AacConfig { core_sample_rate_hz: rate, channels, frame_len: 1024, sbr: false, ps: false });
    split
        .into_iter()
        .take(frames)
        .map(|(hdr, payload)| {
            let end = dec.parse_elements(&payload).unwrap().end;
            (hdr, payload, end)
        })
        .collect()
}

/// The SBR fixtures: a mono and a stereo 24 kHz core, 48 kHz out.
pub(super) fn he_aac_stream(name: &str, frames: usize, seed: u64, channels: usize) -> Vec<u8> {
    let lc = lc_frames(name, frames);
    let mut sbr_gen = SbrGen::new(seed, 48000, channels);
    let mut out = Vec::new();
    for (i, (hdr, payload, end)) in lc.iter().enumerate() {
        let crc = i % 7 == 3;
        let sbr = sbr_gen.payload(crc);
        out.extend(adts_frame(hdr, &with_fill(payload, *end, &sbr, crc)));
    }
    out
}

/// The LC frames of an ADTS stream as LATM/LOAS with frameLengthFlag set.
pub(super) fn latm_960(name: &str, frames: usize) -> Vec<u8> {
    let data = std::fs::read(data_dir().join(format!("{name}.aac"))).unwrap();
    let split = adts_split(&data);
    let h = &split[0].0;
    let sf_index = u32::from((h[2] >> 2) & 0xF);
    let channels = u32::from(((h[2] & 1) << 2) | (h[3] >> 6));
    let mut out = Vec::new();
    for (_, payload) in split.iter().take(frames) {
        let mut w = BitWriter::new();
        w.put(0, 1); // useSameStreamMux = 0: config every frame
        w.put(0, 1); // audioMuxVersion
        w.put(1, 1); // allStreamsSameTimeFraming
        w.put(0, 6); // numSubFrames
        w.put(0, 4); // numProgram
        w.put(0, 3); // numLayer
        w.put(2, 5); // AAC LC
        w.put(sf_index, 4);
        w.put(channels, 4);
        w.put(1, 1); // frameLengthFlag: 960
        w.put(0, 1); // dependsOnCoreCoder
        w.put(0, 1); // extensionFlag
        w.put(0, 3); // frameLengthType
        w.put(0xFF, 8); // latmBufferFullness
        w.put(0, 1); // otherDataPresent
        w.put(0, 1); // crcCheckPresent
        let mut len = payload.len();
        while len >= 255 {
            w.put(255, 8);
            len -= 255;
        }
        w.put(len as u32, 8);
        w.put_bits_from(payload, payload.len() * 8);
        w.align();
        let n = w.bytes.len();
        out.push(0x56);
        out.push(0xE0 | ((n >> 8) as u8 & 0x1F));
        out.push(n as u8);
        out.extend_from_slice(&w.bytes);
    }
    out
}

/// The generator and the parser agree bit for bit on every payload.
#[test]
fn generated_sbr_payloads_parse_back_exactly() {
    use super::bits::BitReader;
    use super::sbr::Sbr;
    for (seed, channels) in [(1u64, 1usize), (2, 2), (3, 1), (4, 2), (5, 2)] {
        let mut sg = SbrGen::new(seed, 48000, channels);
        let mut sbr = Sbr::new(48000, 1024, channels, false);
        for f in 0..40 {
            let crc = f % 7 == 3;
            let w = sg.payload(crc);
            let mut br = BitReader::new(&w.bytes);
            sbr.parse(&mut br, channels == 2, crc).unwrap_or_else(|e| panic!("seed {seed} frame {f}: {e}"));
            assert_eq!(br.position(), w.bits, "seed {seed} frame {f}");
        }
    }
}

#[test]
#[ignore = "rewrites the checked-in fixtures; run by hand, then regenerate the references with ffmpeg"]
fn write_fixtures() {
    let dir = data_dir();
    std::fs::write(dir.join("he_mono48.aac"), he_aac_stream("speech_mono24", 16, 1, 1)).unwrap();
    std::fs::write(dir.join("he_stereo48.aac"), he_aac_stream("music_stereo24", 12, 2, 2)).unwrap();
    std::fs::write(dir.join("lc960_stereo48.latm"), latm_960("music_stereo48", 16)).unwrap();
}
