//! A DRM30 transmitter for the tests, built from the standard alongside the
//! receiver: FAC, SDC and MSC with their real channel coding (energy
//! dispersal, multilevel punctured convolutional coding, bit and cell
//! interleaving), OFDM with the pilot patterns, at 48 kHz. It carries one
//! audio service whose audio super frames hold random bytes cut as AAC or
//! xHE-AAC frames, a label, a text message and the time.

use super::audio::text_pieces;
use super::cellmap::{self, CellMap};
use super::coding::{self, CellInterleaver, Mlc};
use super::tables::Mode;
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct TxConfig {
    pub mode: Mode,
    pub so: u8,
    /// MSC levels: 2 for 16-QAM, 3 for 64-QAM.
    pub msc_levels: usize,
    pub pl_a: u8,
    pub pl_b: u8,
    /// Bytes of the audio stream in part A (0 for equal protection).
    pub len_a: usize,
    pub long_interleave: bool,
    pub sdc_qam16: bool,
    pub label: String,
    pub service_id: u32,
    pub text: Option<String>,
    /// AAC core rate code (1: 12 kHz, 3: 24 kHz), or xHE-AAC when `xhe`.
    pub rate_code: u8,
    pub sbr: bool,
    pub audio_mode: u8,
    pub xhe: bool,
    pub seed: u64,
}

impl TxConfig {
    pub fn new(mode: Mode, so: u8, msc_levels: usize) -> Self {
        TxConfig {
            mode,
            so,
            msc_levels,
            pl_a: 0,
            pl_b: if msc_levels == 3 { 1 } else { 0 },
            len_a: 0,
            long_interleave: true,
            sdc_qam16: true,
            label: "uSDR Test".to_string(),
            service_id: 0x1234AB,
            text: Some("uSDR DRM test: two text segments".to_string()),
            rate_code: 1,
            sbr: true,
            audio_mode: 0,
            xhe: false,
            seed: 1,
        }
    }
}

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn uniform(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn byte(&mut self) -> u8 {
        (self.next() >> 32) as u8
    }

    pub fn gauss(&mut self) -> f64 {
        let u = self.uniform().max(1e-300);
        let v = self.uniform();
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// What went into one multiplex frame's audio stream.
#[derive(Clone, Debug)]
pub struct SentFrame {
    /// The access units cut from it (AAC: all its frames; xHE-AAC: those
    /// that ended in it).
    pub units: Vec<Vec<u8>>,
}

pub struct Tx {
    pub cfg: TxConfig,
    pub map: CellMap,
    msc: Mlc,
    sdc: Mlc,
    fac: Mlc,
    inter: CellInterleaver,
    rng: Rng,
    pub len_b: usize,
    text: Vec<[u8; 4]>,
    text_pos: usize,
    ifft: Arc<dyn Fft<f32>>,
    /// The audio sent, per multiplex frame, in order.
    pub sent: Vec<SentFrame>,
    xhe_cur: Vec<u8>,
    minute: u32,
}

/// Bits of `v` (MSB first) into `out`.
fn put(out: &mut Vec<u8>, v: u64, n: usize) {
    for i in (0..n).rev() {
        out.push(((v >> i) & 1) as u8);
    }
}

impl Tx {
    pub fn new(cfg: TxConfig) -> Self {
        let map = CellMap::new(cfg.mode, cfg.so).expect("mode and occupancy");
        let msc = Mlc::msc(map.n_mux, cfg.msc_levels, cfg.pl_a, cfg.pl_b, cfg.len_a).expect("MSC coding");
        let sdc = Mlc::sdc(map.n_sdc(), cfg.sdc_qam16).expect("SDC coding");
        let len_b = msc.bits() / 8 - cfg.len_a;
        let text = cfg.text.as_deref().map(|t| text_pieces(t, 0)).unwrap_or_default();
        let ifft = FftPlanner::new().plan_fft_inverse(cfg.mode.tu());
        let (pts, a) = coding::pam(cfg.msc_levels);
        let fill = |i: usize| {
            let h = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32;
            Complex32::new(pts[(h & 7) as usize % pts.len()] * a, pts[(h >> 3 & 7) as usize % pts.len()] * a)
        };
        Tx {
            inter: CellInterleaver::new(map.n_mux, cfg.long_interleave, fill),
            rng: Rng::new(cfg.seed),
            cfg,
            map,
            msc,
            sdc,
            fac: Mlc::fac(),
            len_b,
            text,
            text_pos: 0,
            ifft,
            sent: Vec::new(),
            xhe_cur: Vec::new(),
            minute: 0,
        }
    }

    fn fac_bits(&self, frame: usize) -> Vec<u8> {
        let c = &self.cfg;
        let mut b = Vec::new();
        put(&mut b, 0, 1); // base layer
        put(&mut b, frame as u64, 2); // identity
        put(&mut b, 0, 1); // RM flag
        put(&mut b, c.so as u64, 3);
        put(&mut b, if c.long_interleave { 0 } else { 1 }, 1);
        put(&mut b, if c.msc_levels == 3 { 0 } else { 3 }, 2);
        put(&mut b, if c.sdc_qam16 { 0 } else { 1 }, 1);
        put(&mut b, 4, 4); // one audio service
        put(&mut b, 0, 3);
        put(&mut b, 0, 1);
        put(&mut b, 0, 1);
        put(&mut b, c.service_id as u64, 24);
        put(&mut b, 0, 2); // short id
        put(&mut b, 0, 1);
        put(&mut b, 5, 4); // English
        put(&mut b, 0, 1); // audio
        put(&mut b, 10, 5); // Pop Music
        put(&mut b, 0, 1);
        put(&mut b, 0, 6);
        let crc = coding::crc8(&coding::pack(&b));
        put(&mut b, crc as u64, 8);
        b
    }

    fn entity(out: &mut Vec<u8>, kind: u64, body: &[u8]) {
        // Body bits are 4 + 8 n; the header gives n.
        let n = (body.len() - 4) / 8;
        put(out, n as u64, 7);
        put(out, 0, 1);
        put(out, kind, 4);
        out.extend_from_slice(body);
    }

    fn sdc_bits(&mut self) -> Vec<u8> {
        let c = self.cfg.clone();
        let l = self.sdc.bits();
        let n = (l - 20) / 8;
        let mut ents: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut body = Vec::new();
        put(&mut body, c.pl_a as u64, 2);
        put(&mut body, c.pl_b as u64, 2);
        put(&mut body, c.len_a as u64, 12);
        put(&mut body, self.len_b as u64, 12);
        ents.push((0, body));
        let mut body = Vec::new();
        put(&mut body, 0, 4); // short id 0, stream 0
        put(&mut body, if c.xhe { 3 } else { 0 }, 2);
        put(&mut body, c.sbr as u64, 1);
        put(&mut body, c.audio_mode as u64, 2);
        put(&mut body, c.rate_code as u64, 3);
        put(&mut body, c.text.is_some() as u64, 1);
        put(&mut body, 0, 1);
        put(&mut body, 0, 5);
        put(&mut body, 0, 1);
        if c.xhe {
            // A stand-in for the static configuration bytes.
            put(&mut body, 0xA5C3, 16);
        }
        ents.push((9, body));
        let mut body = Vec::new();
        put(&mut body, 0, 4);
        for &ch in c.label.as_bytes() {
            put(&mut body, ch as u64, 8);
        }
        ents.push((1, body));
        let mut body = Vec::new();
        put(&mut body, 0, 4);
        for &ch in b"engus" {
            put(&mut body, ch as u64, 8);
        }
        let lang = (12, body);
        // 2026-11-06 is MJD 61350.
        let mut body = Vec::new();
        put(&mut body, 61350, 17);
        put(&mut body, 14, 5);
        put(&mut body, (self.minute % 60) as u64, 6);
        let time = (8, body);
        // Small SDCs cannot carry everything every time: the optional
        // entities take turns, as on the air.
        if self.minute.is_multiple_of(2) {
            ents.push(time);
            ents.push(lang);
        } else {
            ents.push(lang);
            ents.push(time);
        }
        let mut field = Vec::new();
        for (i, (kind, body)) in ents.iter().enumerate() {
            if field.len() + 12 + body.len() <= 8 * n {
                Self::entity(&mut field, *kind, body);
            } else {
                assert!(i >= 3, "the multiplex, audio and label fit the SDC");
            }
        }
        assert!(field.len() <= 8 * n, "SDC entities fit the data field");
        field.resize(8 * n, 0);
        let mut bits = vec![0u8; 4];
        bits.extend_from_slice(&field);
        let crc = coding::crc16(&coding::pack(&[vec![0u8; 4], bits.clone()].concat()));
        put(&mut bits, crc as u64, 16);
        bits.resize(l, 0);
        bits
    }

    fn text_piece(&mut self) -> [u8; 4] {
        if self.text.is_empty() {
            return [0; 4];
        }
        let p = self.text[self.text_pos % self.text.len()];
        self.text_pos += 1;
        p
    }

    /// One logical frame of the audio stream.
    fn audio_stream(&mut self) -> Vec<u8> {
        let total = self.cfg.len_a + self.len_b;
        let text = self.cfg.text.is_some();
        let asf_len = total - if text { 4 } else { 0 };
        let mut asf;
        let mut units = Vec::new();
        if self.cfg.xhe {
            // Three frames start in each super frame, never in its last
            // bytes; each ends in a CRC-16 over its access unit.
            let b = 3;
            let p = asf_len - 2 - 2 * b;
            let mut borders = Vec::new();
            for i in 0..b {
                let centre = (2 * i + 1) * p / (2 * b);
                let jitter = (self.rng.uniform() - 0.5) * (p / (3 * b)) as f64;
                borders.push((centre as f64 + jitter) as usize);
            }
            let mut payload = Vec::with_capacity(p);
            let mut next = 0;
            while payload.len() < p {
                let j = payload.len();
                if next < b && j + 2 == borders[next] {
                    let crc = coding::crc16(&self.xhe_cur);
                    payload.extend_from_slice(&crc.to_be_bytes());
                    units.push(std::mem::take(&mut self.xhe_cur));
                    next += 1;
                } else {
                    let v = self.rng.byte();
                    payload.push(v);
                    self.xhe_cur.push(v);
                }
            }
            let count = b as u8;
            asf = vec![count << 4 | 5];
            asf.push(coding::crc8(&asf[..1]));
            asf.extend_from_slice(&payload);
            for i in (0..b).rev() {
                let e = ((borders[i] as u16) << 4) | count as u16;
                asf.extend_from_slice(&e.to_be_bytes());
            }
        } else {
            let frames = if self.cfg.rate_code == 3 { 10 } else { 5 };
            let header = if frames == 10 { 14 } else { 6 };
            let payload = asf_len - header - frames;
            let hp = if self.cfg.len_a > 0 { (self.cfg.len_a - header - frames) / frames } else { 0 };
            // Random frame lengths around the mean.
            let mean = payload / frames;
            let mut lens: Vec<usize> =
                (0..frames - 1).map(|_| mean - mean / 16 + (self.rng.next() % (mean as u64 / 8)) as usize).collect();
            let used: usize = lens.iter().sum();
            lens.push(payload - used);
            for &l in &lens {
                units.push((0..l).map(|_| self.rng.byte()).collect::<Vec<u8>>());
            }
            let mut hb = Vec::new();
            let mut pos = 0;
            for &l in &lens[..frames - 1] {
                pos += l;
                put(&mut hb, (pos % 4096) as u64, 12);
            }
            if frames == 10 {
                put(&mut hb, 0, 4);
            }
            asf = coding::pack(&hb);
            for u in &units {
                asf.extend_from_slice(&u[..hp]);
                asf.push(coding::crc8(&u[..2]));
            }
            for u in &units {
                asf.extend_from_slice(&u[hp..]);
            }
        }
        assert_eq!(asf.len(), asf_len);
        self.sent.push(SentFrame { units });
        if text {
            let p = self.text_piece();
            asf.extend_from_slice(&p);
        }
        asf
    }

    fn mux_cells(&mut self) -> Vec<Complex32> {
        let stream = self.audio_stream();
        // Part A of every stream, then part B (one stream here).
        let mut bits = coding::unpack(&stream);
        bits.resize(self.msc.bits(), 0);
        coding::disperse(&mut bits);
        let cells = self.msc.encode(&bits);
        self.inter.push(cells)
    }

    /// One transmission super frame at 48 kHz, about unit power.
    pub fn superframe(&mut self) -> Vec<Complex32> {
        self.minute += 1;
        let ns = self.map.ns;
        let (nu, ng) = (self.cfg.mode.tu(), self.cfg.mode.tg());
        let mut msc: Vec<Complex32> = Vec::with_capacity(3 * self.map.n_mux + 2);
        for _ in 0..3 {
            let c = self.mux_cells();
            msc.extend(c);
        }
        let a = coding::pam(self.cfg.msc_levels).1;
        if self.map.n_dummy >= 1 {
            msc.push(Complex32::new(a, a));
        }
        if self.map.n_dummy >= 2 {
            msc.push(Complex32::new(a, -a));
        }
        let mut sdc_bits = self.sdc_bits();
        coding::disperse(&mut sdc_bits);
        let sdc = self.sdc.encode(&sdc_bits);
        let map = &self.map;
        let mut out = Vec::with_capacity(3 * ns * (nu + ng));
        let (mut mi, mut si) = (0, 0);
        let scale = 1.0 / (map.ncar as f32).sqrt();
        for f in 0..3 {
            let mut fb = self.fac_bits(f);
            coding::disperse(&mut fb);
            let fac = self.fac.encode(&fb);
            let mut fi = 0;
            for s in 0..ns {
                let sym = f * ns + s;
                let mut x = vec![Complex32::new(0.0, 0.0); nu];
                for i in 0..map.ncar {
                    let v = match map.kind_at(sym, i) {
                        cellmap::PILOT => map.pilot_at(s, i),
                        cellmap::FAC => {
                            fi += 1;
                            fac[fi - 1]
                        }
                        cellmap::SDC => {
                            si += 1;
                            sdc[si - 1]
                        }
                        cellmap::MSC => {
                            mi += 1;
                            msc[mi - 1]
                        }
                        _ => Complex32::new(0.0, 0.0),
                    };
                    x[map.bin(i)] = v * scale;
                }
                self.ifft.process(&mut x);
                out.extend_from_slice(&x[nu - ng..]);
                out.extend_from_slice(&x);
            }
            assert_eq!(fi, fac.len());
        }
        assert_eq!(mi, msc.len());
        assert_eq!(si, sdc.len());
        out
    }
}
