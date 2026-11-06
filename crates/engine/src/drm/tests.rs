//! Tests: the tables against the standard's own derived numbers, the
//! coding blocks one by one, and the whole receiver against the test
//! transmitter through noise, frequency and clock offsets and fading.

use super::audio::{self, TextAssembler, TextUpdate};
use super::cellmap::CellMap;
use super::coding::{self, CellDeinterleaver, CellInterleaver, Mlc, Viterbi};
use super::resample::Resampler;
use super::tables::{self, MODES, Mode};
use super::tx::{Rng, Tx, TxConfig};
use super::*;

// ---------------------------------------------------------------------------
// Tables against ES 201 980 V4.2.1.

/// Tables 41 to 44 (NMUX and cell loss) and 25 (NSDC).
#[test]
fn cell_counts_match_the_standard() {
    // (mode, so, NMUX, NL, NSDC)
    let want = [
        (Mode::A, 0, 1259, 1, 167),
        (Mode::A, 1, 1422, 2, 190),
        (Mode::A, 2, 2632, 1, 359),
        (Mode::A, 3, 2959, 0, 405),
        (Mode::A, 4, 5464, 2, 754),
        (Mode::A, 5, 6118, 0, 846),
        (Mode::B, 0, 966, 2, 130),
        (Mode::B, 1, 1110, 0, 150),
        (Mode::B, 2, 2051, 0, 282),
        (Mode::B, 3, 2337, 2, 322),
        (Mode::B, 4, 4249, 0, 588),
        (Mode::B, 5, 4774, 1, 662),
        (Mode::C, 3, 1844, 0, 288),
        (Mode::C, 5, 3867, 2, 607),
        (Mode::D, 3, 1226, 1, 152),
        (Mode::D, 5, 2606, 1, 332),
    ];
    for (mode, so, nmux, nl, nsdc) in want {
        let m = CellMap::new(mode, so).unwrap();
        assert_eq!((m.n_mux, m.n_dummy, m.n_sdc()), (nmux, nl, nsdc), "mode {mode:?} SO {so}");
        assert_eq!(m.fac.len(), tables::FAC_CELLS, "FAC cells, mode {mode:?} SO {so}");
    }
}

/// Annex J: bits per multiplex frame (EEP) and per SDC block, and table 21:
/// the SDC data field.
#[test]
fn bit_counts_match_annex_j() {
    // (mode, so, [64-QAM PL 0..3], [16-QAM PL 0, 1], [SDC 16, 4])
    type Row = (Mode, u8, [usize; 4], [usize; 2], [usize; 2]);
    let want: [Row; 16] = [
        (Mode::A, 0, [3757, 4509, 5322, 5898], [2505, 3131], [321, 161]),
        (Mode::A, 1, [4248, 5096, 6018, 6664], [2832, 3540], [366, 184]),
        (Mode::A, 2, [7878, 9450, 11157, 12364], [5250, 6565], [705, 353]),
        (Mode::A, 3, [8857, 10628, 12547, 13908], [5904, 7381], [798, 399]),
        (Mode::A, 4, [16374, 19646, 23193, 25704], [10914, 13645], [1494, 748]),
        (Mode::A, 5, [18336, 21998, 25976, 28788], [12222, 15280], [1680, 840]),
        (Mode::B, 0, [2880, 3456, 4080, 4520], [1920, 2400], [246, 124]),
        (Mode::B, 1, [3312, 3972, 4692, 5196], [2208, 2760], [288, 144]),
        (Mode::B, 2, [6133, 7361, 8688, 9630], [4089, 5111], [552, 276]),
        (Mode::B, 3, [6991, 8390, 9900, 10980], [4662, 5826], [630, 316]),
        (Mode::B, 4, [12727, 15272, 18026, 19980], [8484, 10606], [1164, 582]),
        (Mode::B, 5, [14304, 17162, 20264, 22456], [9534, 11920], [1311, 656]),
        (Mode::C, 3, [5514, 6615, 7808, 8654], [3675, 4595], [564, 282]),
        (Mode::C, 5, [11581, 13898, 16406, 18188], [7722, 9651], [1200, 601]),
        (Mode::D, 3, [3660, 4391, 5185, 5746], [2439, 3050], [291, 146]),
        (Mode::D, 5, [7800, 9359, 11050, 12242], [5199, 6500], [651, 326]),
    ];
    // Table 21: SDC data field bytes, (SDC mode 0, SDC mode 1).
    let field: [(Mode, u8, usize, usize); 16] = [
        (Mode::A, 0, 37, 17),
        (Mode::A, 1, 43, 20),
        (Mode::A, 2, 85, 41),
        (Mode::A, 3, 97, 47),
        (Mode::A, 4, 184, 91),
        (Mode::A, 5, 207, 102),
        (Mode::B, 0, 28, 13),
        (Mode::B, 1, 33, 15),
        (Mode::B, 2, 66, 32),
        (Mode::B, 3, 76, 37),
        (Mode::B, 4, 143, 70),
        (Mode::B, 5, 161, 79),
        (Mode::C, 3, 68, 32),
        (Mode::C, 5, 147, 72),
        (Mode::D, 3, 33, 15),
        (Mode::D, 5, 78, 38),
    ];
    for ((mode, so, q64, q16, sdc), (_, _, f16, f4)) in want.into_iter().zip(field) {
        let m = CellMap::new(mode, so).unwrap();
        for (pl, &l) in q64.iter().enumerate() {
            assert_eq!(Mlc::msc(m.n_mux, 3, 0, pl as u8, 0).unwrap().bits(), l, "64-QAM PL{pl} mode {mode:?} SO {so}");
        }
        for (pl, &l) in q16.iter().enumerate() {
            assert_eq!(Mlc::msc(m.n_mux, 2, 0, pl as u8, 0).unwrap().bits(), l, "16-QAM PL{pl} mode {mode:?} SO {so}");
        }
        let s16 = Mlc::sdc(m.n_sdc(), true).unwrap().bits();
        let s4 = Mlc::sdc(m.n_sdc(), false).unwrap().bits();
        assert_eq!((s16, s4), (sdc[0], sdc[1]), "SDC bits mode {mode:?} SO {so}");
        assert_eq!(((s16 - 20) / 8, (s4 - 20) / 8), (f16, f4), "SDC field mode {mode:?} SO {so}");
    }
}

/// Table 26: the first 16 bits of the PRBS.
#[test]
fn prbs_matches_table_26() {
    assert_eq!(coding::prbs(16), vec![0, 0, 0, 0, 0, 1, 1, 1, 1, 0, 1, 1, 1, 1, 1, 0]);
}

/// Annex D's CRCs are the common MSB-first, preset-to-ones, inverted
/// forms: CRC-16/GENIBUS and CRC-8/SAE-J1850, whose check values over
/// "123456789" are published.
#[test]
fn crcs_match_published_check_values() {
    assert_eq!(coding::crc16(b"123456789"), 0xD64E);
    assert_eq!(coding::crc8(b"123456789"), 0x4B);
    assert_eq!(coding::crc8_bits(b"123456789", 72), 0x4B);
}

/// Every gain reference phase in the 0..1024 range and the boosted pilots
/// on the grid's edges (table 59's note: the edge carriers are on it).
#[test]
fn pilots_are_consistent() {
    for mode in MODES {
        for &so in mode.occupancies() {
            let m = CellMap::new(mode, so).unwrap();
            let (x, y, k0) = mode.grid();
            for &k in &tables::boosted(mode, so) {
                assert!((m.kmin..m.kmin + m.ncar as i32).contains(&k));
                assert_eq!((k - k0).rem_euclid(x), 0, "boosted {k} on the grid, mode {mode:?}");
            }
            // Each frame symbol has its gain references and all carriers
            // of the grid's class appear within y symbols.
            for i in 0..m.ncar {
                let k = m.kmin + i as i32;
                if (k - k0).rem_euclid(x) == 0 && !mode.unused(k) {
                    assert!((0..y as usize).any(|s| m.on_grid(s, i)), "carrier {k} mode {mode:?}");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Coding blocks.

#[test]
fn viterbi_decodes_every_rate_through_noise() {
    let rates = [
        tables::R1_4,
        tables::R3_10,
        tables::R1_3,
        tables::R4_11,
        tables::R1_2,
        tables::R4_7,
        tables::R3_5,
        tables::R2_3,
        tables::R8_11,
        tables::R3_4,
        tables::R4_5,
        tables::R7_8,
        tables::R8_9,
    ];
    let mut rng = Rng::new(7);
    let mut vit = Viterbi::new();
    for r in rates {
        let n = 600 * r.rx;
        let bits: Vec<u8> = (0..n).map(|_| (rng.next() & 1) as u8).collect();
        let sched = coding::schedule(0, r, n, r, coding::Tail::Pattern(0));
        let code = coding::encode(&bits, &sched);
        // Raw bit error rates of 3 % (low rates) and 1 % (high rates).
        let sigma = if r.value() <= 0.5 { 0.55 } else { 0.43 };
        let soft: Vec<f32> =
            code.iter().map(|&b| (if b == 0 { 1.0 } else { -1.0 } + sigma * rng.gauss()) as f32).collect();
        let raw = soft.iter().zip(&code).filter(|(s, b)| (**s < 0.0) != (**b == 1)).count();
        let out = vit.decode(&soft, &sched);
        let errs = out.iter().zip(&bits).filter(|(a, b)| a != b).count();
        assert!(raw > 0);
        if r.value() <= 0.5 {
            assert_eq!(errs, 0, "rate {}/{}: {errs} errors from {raw} raw", r.rx, r.ry);
        } else {
            assert!(errs <= raw / 10, "rate {}/{}: {errs} errors from {raw} raw", r.rx, r.ry);
        }
    }
}

#[test]
fn multilevel_coding_round_trips() {
    let mut vit = Viterbi::new();
    let mut rng = Rng::new(3);
    let mut check = |mlc: Mlc| {
        let bits: Vec<u8> = (0..mlc.bits()).map(|_| (rng.next() & 1) as u8).collect();
        let cells = mlc.encode(&bits);
        assert_eq!(cells.len(), mlc.cells());
        let w = vec![1.0; cells.len()];
        let (out, ber) = mlc.decode(&cells, &w, &mut vit);
        assert_eq!(out, bits);
        assert_eq!(ber, 0.0);
    };
    check(Mlc::fac());
    let m = CellMap::new(Mode::B, 3).unwrap();
    check(Mlc::sdc(m.n_sdc(), true).unwrap());
    check(Mlc::sdc(m.n_sdc(), false).unwrap());
    for pl in 0..4 {
        check(Mlc::msc(m.n_mux, 3, pl, pl, 0).unwrap());
        // Unequal protection with 100 bytes in part A.
        check(Mlc::msc(m.n_mux, 3, 0, pl, 100).unwrap());
    }
    for pl in 0..2 {
        check(Mlc::msc(m.n_mux, 2, pl, pl, 0).unwrap());
        check(Mlc::msc(m.n_mux, 2, 0, 1, 60).unwrap());
    }
}

#[test]
fn cell_interleaving_round_trips() {
    for long in [false, true] {
        let n = 2337;
        let mut tx = CellInterleaver::new(n, long, |_| Complex32::new(0.0, 0.0));
        let mut rx = CellDeinterleaver::new(n, long);
        let frames: Vec<Vec<Complex32>> =
            (0..10).map(|f| (0..n).map(|i| Complex32::new(f as f32, i as f32)).collect()).collect();
        let mut got = Vec::new();
        for f in &frames {
            let c = tx.push(f.clone());
            if let Some((z, _)) = rx.push(&c, &vec![1.0; n]) {
                got.push(z);
            }
        }
        let delay = if long { 4 } else { 0 };
        assert_eq!(got.len(), 10 - delay);
        for (i, g) in got.iter().enumerate() {
            assert_eq!(g, &frames[i], "frame {i} long {long}");
        }
    }
}

#[test]
fn aac_super_frame_splits() {
    for (frames, len_a) in [(5, 0), (10, 0), (5, 40)] {
        let header = if frames == 10 { 14 } else { 6 };
        let mut rng = Rng::new(frames as u64);
        let lens: Vec<usize> = (0..frames).map(|_| 20 + (rng.next() % 30) as usize).collect();
        let units: Vec<Vec<u8>> = lens.iter().map(|&l| (0..l).map(|_| rng.byte()).collect()).collect();
        let hp = if len_a > 0 { (len_a - header - frames) / frames } else { 0 };
        let mut hb = Vec::new();
        let mut pos = 0;
        for &l in &lens[..frames - 1] {
            pos += l;
            for i in (0..12).rev() {
                hb.push(((pos >> i) & 1) as u8);
            }
        }
        hb.resize(header * 8, 0);
        let mut asf = coding::pack(&hb);
        for u in &units {
            asf.extend_from_slice(&u[..hp]);
            asf.push(0x5A);
        }
        for u in &units {
            asf.extend_from_slice(&u[hp..]);
        }
        let out = audio::split_aac(&asf, len_a, frames);
        assert_eq!(out.len(), frames);
        for (o, u) in out.iter().zip(&units) {
            assert!(o.ok);
            assert_eq!(&o.data, u);
            assert_eq!(o.crc, Some(0x5A));
        }
        // A corrupt header is caught.
        asf[0] ^= 0xFF;
        asf[1] ^= 0xFF;
        let bad = audio::split_aac(&asf, len_a, frames);
        assert!(bad.iter().all(|f| !f.ok) || bad.iter().zip(&units).any(|(o, u)| &o.data != u));
    }
}

#[test]
fn text_message_reassembles() {
    let text = "DRM text: héllo, a message long enough for three segments!";
    let mut asm = TextAssembler::default();
    let mut got = None;
    let pieces = audio::text_pieces(text, 1);
    for _ in 0..3 {
        for p in &pieces {
            if let Some(TextUpdate::Message(t)) = asm.push(p) {
                got = Some(t);
            }
        }
        // Zero pieces between repetitions must not upset anything.
        asm.push(&[0; 4]);
    }
    assert_eq!(got.as_deref(), Some(text));
    assert!(asm.bad == 0 && asm.ok > 0);
}

// ---------------------------------------------------------------------------
// The channel.

pub struct Channel {
    pub fs: f64,
    pub ppm: f64,
    pub freq_hz: f64,
    pub snr_db: Option<f64>,
    /// (delay s, power gain, Doppler spread Hz) of each path.
    pub paths: Vec<(f64, f64, f64)>,
    pub seed: u64,
}

impl Channel {
    pub fn clean(fs: f64) -> Self {
        Channel { fs, ppm: 0.0, freq_hz: 0.0, snr_db: None, paths: vec![(0.0, 1.0, 0.0)], seed: 1 }
    }

    /// Pass a 48 kHz transmission through: multipath with Rayleigh fading
    /// (sums of sinusoids), the transmitter's clock off by `ppm`, the
    /// frequency offset, resampling to `fs` and noise at `snr_db` within
    /// `bw_hz`.
    pub fn run(&self, tx: &[Complex32], bw_hz: f64) -> Vec<Complex32> {
        let mut rng = Rng::new(self.seed);
        let mut y = vec![Complex32::new(0.0, 0.0); tx.len()];
        for &(delay, gain, doppler) in &self.paths {
            let d = (delay * tables::FS).round() as usize;
            let amp = gain.sqrt() as f32;
            let sines: Vec<(f64, f64)> = (0..12)
                .map(|_| {
                    (doppler * (std::f64::consts::TAU * rng.uniform()).cos(), std::f64::consts::TAU * rng.uniform())
                })
                .collect();
            // The fading gain is held over 16 samples; the fastest Doppler
            // tested is a few hertz.
            let mut g = Complex32::new(amp, 0.0);
            for n in d..tx.len() {
                if doppler > 0.0 && (n - d).is_multiple_of(16) {
                    let t = n as f64 / tables::FS;
                    g = Complex32::new(0.0, 0.0);
                    for &(f, ph) in &sines {
                        g += Complex32::from_polar(1.0, (std::f64::consts::TAU * f * t + ph) as f32);
                    }
                    g *= amp / (sines.len() as f32).sqrt();
                }
                y[n] += tx[n - d] * g;
            }
        }
        let mut rs = Resampler::new(tables::FS * (1.0 + self.ppm * 1e-6), self.fs, 32);
        let mut out = Vec::new();
        rs.process(&y, &mut out);
        let mut ph = 0.0f64;
        let dph = std::f64::consts::TAU * self.freq_hz / self.fs;
        for v in out.iter_mut() {
            *v *= Complex32::from_polar(1.0, ph as f32);
            ph = (ph + dph) % std::f64::consts::TAU;
        }
        if let Some(snr) = self.snr_db {
            let p: f64 = out.iter().map(|v| v.norm_sqr() as f64).sum::<f64>() / out.len() as f64;
            let sigma = (p / 10f64.powf(snr / 10.0) * self.fs / bw_hz / 2.0).sqrt();
            for v in out.iter_mut() {
                *v += Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32);
            }
        }
        out
    }
}

pub struct Run {
    pub events: Vec<DrmEvent>,
    pub status: DrmStatus,
    pub tx: Tx,
}

/// Transmit `superframes`, pass through `ch`, receive in 20 ms chunks.
pub fn run(cfg: TxConfig, ch: &Channel, superframes: usize) -> Run {
    let mut tx = Tx::new(cfg);
    // A second of silence (noise only) in front.
    let mut sig = vec![Complex32::new(0.0, 0.0); 48_000];
    for _ in 0..superframes {
        sig.extend(tx.superframe());
    }
    let bw = (tx.map.ncar as f64) * tx.cfg.mode.spacing_hz();
    let rx_in = ch.run(&sig, bw);
    let mut rx = DrmReceiver::new(ch.fs as f32);
    let mut events = Vec::new();
    for c in rx_in.chunks((ch.fs * 0.02) as usize) {
        rx.push_iq(c);
        events.extend(rx.take());
    }
    Run { events, status: rx.status(), tx }
}

/// Audio super frames received whose frames all arrived intact (every
/// frame `ok` and byte for byte one that was sent), and all received.
pub fn audio_score(r: &Run) -> (usize, usize) {
    let sent: std::collections::HashSet<&Vec<u8>> = r.tx.sent.iter().flat_map(|s| s.units.iter()).collect();
    let mut good = 0;
    let mut total = 0;
    for e in &r.events {
        if let DrmEvent::AudioSuperFrame { frames, .. } = e {
            total += 1;
            if !frames.is_empty() && frames.iter().all(|f| f.ok && sent.contains(&f.data)) {
                good += 1;
            }
        }
    }
    (good, total)
}

fn label_of(r: &Run) -> Option<String> {
    r.events.iter().rev().find_map(|e| match e {
        DrmEvent::Service(s) => s.label.clone(),
        _ => None,
    })
}

fn text_of(r: &Run) -> Option<String> {
    r.events.iter().rev().find_map(|e| match e {
        DrmEvent::Text { text, .. } => Some(text.clone()),
        _ => None,
    })
}

fn show(r: &Run) {
    for e in &r.events {
        if !matches!(e, DrmEvent::AudioSuperFrame { .. }) {
            println!("{}: {}", e.kind(), e.summary());
        }
    }
    println!("{:?}", r.status);
}

#[test]
fn mode_b_64qam_clean_decodes_everything() {
    let cfg = TxConfig::new(Mode::B, 3, 3);
    let text = cfg.text.clone();
    let mut ch = Channel::clean(47_904.0);
    ch.freq_hz = 123.4;
    ch.ppm = 20.0;
    ch.snr_db = Some(30.0);
    let r = run(cfg, &ch, 12);
    show(&r);
    let (good, total) = audio_score(&r);
    println!("audio {good}/{total}");
    assert_eq!(label_of(&r).as_deref(), Some("uSDR Test"));
    assert_eq!(text_of(&r), text);
    assert!(total >= 10 && good == total, "{good}/{total} audio super frames");
    assert!(r.events.iter().any(|e| matches!(e, DrmEvent::Time { hour: 14, .. })));
    assert!((r.status.freq_offset_hz - 123.4).abs() < 1.0, "offset {}", r.status.freq_offset_hz);
}

fn assert_decodes(name: &str, r: &Run, min_audio: usize) {
    let (good, total) = audio_score(r);
    println!("{}", summary_line(name, r));
    assert_eq!(label_of(r).as_deref(), Some("uSDR Test"), "{name}: label");
    assert!(good >= min_audio && good == total, "{name}: {good}/{total} audio super frames");
}

#[test]
fn mode_a_16qam_through_noise_and_offsets() {
    let cfg = TxConfig::new(Mode::A, 3, 2);
    let ch = Channel { ppm: -50.0, freq_hz: -1234.0, snr_db: Some(15.0), ..Channel::clean(47_904.0) };
    let r = run(cfg, &ch, 10);
    assert_decodes("A 16-QAM", &r, 15);
    assert!((r.status.freq_offset_hz + 1234.0).abs() < 1.0, "offset {}", r.status.freq_offset_hz);
    assert!((r.status.clock_ppm + 50.0).abs() < 10.0, "clock {}", r.status.clock_ppm);
    assert_eq!(r.status.mode, Some('A'));
    assert_eq!(r.status.spectrum_occupancy, Some(3));
    assert_eq!(r.status.msc_qam, Some(16));
}

/// Mode B, the HF workhorse, through two equal Rayleigh paths 2 ms apart
/// with 1 Hz Doppler (each 64-QAM cell faded, notches every 500 Hz
/// sweeping through the band), a transmitter clock 50 ppm fast and noise.
#[test]
fn mode_b_64qam_two_path_fading() {
    let cfg = TxConfig::new(Mode::B, 3, 3);
    let ch = Channel {
        paths: vec![(0.0, 0.5, 1.0), (2e-3, 0.5, 1.0)],
        ppm: 50.0,
        freq_hz: 37.0,
        snr_db: Some(25.0),
        ..Channel::clean(47_904.0)
    };
    let r = run(cfg, &ch, 12);
    assert_decodes("B 64-QAM 2-path", &r, 20);
    let s = &r.status;
    assert!(s.fac_ok_rate > 0.9 && s.sdc_ok_rate > 0.9, "{s:?}");
    // The channel's figures, roughly: an rms delay spread of 1 ms, a
    // Doppler of 1 Hz.
    let spread = s.delay_spread_ms.unwrap();
    let doppler = s.doppler_hz.unwrap();
    assert!((0.6..1.4).contains(&spread), "delay spread {spread}");
    assert!((0.5..2.0).contains(&doppler), "doppler {doppler}");
}

#[test]
fn mode_b_16qam_two_path_fading() {
    let cfg = TxConfig::new(Mode::B, 3, 2);
    let ch = Channel {
        paths: vec![(0.0, 0.5, 1.0), (2e-3, 0.5, 1.0)],
        ppm: -20.0,
        snr_db: Some(18.0),
        seed: 5,
        ..Channel::clean(47_904.0)
    };
    let r = run(cfg, &ch, 12);
    assert_decodes("B 16-QAM 2-path", &r, 20);
}

/// Every robustness mode, and the occupancies that put the whole signal
/// to one side of the nominal frequency (0, 1) or reach 14.6 kHz up (5).
#[test]
fn modes_and_occupancies() {
    for (mode, so, levels) in [(Mode::A, 0, 3), (Mode::B, 1, 2), (Mode::A, 5, 3), (Mode::C, 3, 3), (Mode::D, 5, 2)] {
        let ch = Channel { freq_hz: -250.0, snr_db: Some(30.0), ..Channel::clean(47_904.0) };
        let r = run(TxConfig::new(mode, so, levels), &ch, 8);
        assert_decodes(&format!("{mode:?} SO{so}"), &r, 10);
        assert_eq!(r.status.spectrum_occupancy, Some(so));
        assert_eq!(r.status.mode, Some(mode.letter()));
    }
}

/// xHE-AAC frames (cut across super frames, each with its own CRC), AAC
/// at 24 kHz (ten frames), unequal protection, short interleaving with a
/// 4-QAM SDC.
#[test]
fn audio_and_coding_variants() {
    let b = |l| TxConfig::new(Mode::B, 3, l);
    for (name, cfg) in [
        ("xHE-AAC", TxConfig { xhe: true, rate_code: 4, sbr: false, ..b(3) }),
        ("AAC 24 kHz stereo", TxConfig { rate_code: 3, sbr: false, audio_mode: 2, ..b(3) }),
        ("UEP", TxConfig { len_a: 80, pl_a: 0, pl_b: 2, ..b(3) }),
        ("short, SDC 4-QAM", TxConfig { long_interleave: false, sdc_qam16: false, ..b(2) }),
    ] {
        let r = run(cfg.clone(), &Channel { snr_db: Some(30.0), ..Channel::clean(47_904.0) }, 8);
        assert_decodes(name, &r, 10);
        let last = r.events.iter().rev().find_map(|e| match e {
            DrmEvent::AudioSuperFrame { codec, sample_rate_hz, stereo, frames, audio_config, .. } => {
                Some((*codec, *sample_rate_hz, *stereo, frames.len(), audio_config.clone()))
            }
            _ => None,
        });
        let (codec, rate, stereo, n, conf) = last.unwrap();
        if cfg.xhe {
            assert_eq!((codec, rate, conf.len()), (AudioCodec::XheAac, 24_000, 4));
            assert_eq!(n, 3);
        } else {
            assert_eq!(codec, AudioCodec::Aac);
            assert_eq!(n, if cfg.rate_code == 3 { 10 } else { 5 });
            assert_eq!(rate, if cfg.rate_code == 3 { 24_000 } else { 12_000 });
            assert_eq!(stereo, cfg.audio_mode == 2);
        }
    }
}

/// Noise alone finds nothing; a station that goes away is reported lost,
/// and the next one (another mode) is found.
#[test]
fn noise_loss_and_reacquisition() {
    let mut rng = Rng::new(9);
    let mut noise = move |n: usize| -> Vec<Complex32> {
        (0..n).map(|_| Complex32::new(rng.gauss() as f32 * 0.05, rng.gauss() as f32 * 0.05)).collect()
    };
    let mut rx = DrmReceiver::new(48_000.0);
    let mut events = Vec::new();
    let feed = |rx: &mut DrmReceiver, x: &[Complex32], events: &mut Vec<DrmEvent>| {
        for c in x.chunks(960) {
            rx.push_iq(c);
            events.extend(rx.take());
        }
    };
    feed(&mut rx, &noise(48_000 * 10), &mut events);
    assert!(events.is_empty(), "{events:?}");
    assert!(!rx.status().fac_locked);
    for (mode, levels) in [(Mode::B, 3), (Mode::A, 2)] {
        let mut tx = Tx::new(TxConfig::new(mode, 3, levels));
        let mut sig = Vec::new();
        for _ in 0..6 {
            sig.extend(tx.superframe());
        }
        let n = noise(sig.len());
        let sig: Vec<Complex32> = sig.iter().zip(n).map(|(a, b)| a * 0.3 + b).collect();
        feed(&mut rx, &sig, &mut events);
        feed(&mut rx, &noise(48_000 * 8), &mut events);
    }
    let syncs: Vec<(bool, Option<char>)> = events
        .iter()
        .filter_map(|e| match e {
            DrmEvent::Sync { locked, mode, .. } => Some((*locked, *mode)),
            _ => None,
        })
        .collect();
    assert_eq!(syncs, vec![(true, Some('B')), (false, None), (true, Some('A')), (false, None)]);
}

fn summary_line(name: &str, r: &Run) -> String {
    let (good, total) = audio_score(r);
    format!(
        "{name:<16} audio {good:>2}/{total:<2} label {:?} text {} snr {:?} mer {:?} fac {:.2} sdc {:.2} off {:.1} ppm {:.1} spread {:?} doppler {:?}",
        label_of(r),
        text_of(r).is_some(),
        r.status.snr_db,
        r.status.mer_db,
        r.status.fac_ok_rate,
        r.status.sdc_ok_rate,
        r.status.freq_offset_hz,
        r.status.clock_ppm,
        r.status.delay_spread_ms,
        r.status.doppler_hz,
    )
}

#[test]
#[ignore]
fn explore() {
    let c = || Channel::clean(47_904.0);
    let cases: Vec<(&str, TxConfig, Channel)> = vec![
        ("A SO3 16QAM", TxConfig::new(Mode::A, 3, 2), c()),
        ("B SO0", TxConfig::new(Mode::B, 0, 2), c()),
        ("B SO1", TxConfig::new(Mode::B, 1, 3), c()),
        ("A SO5", TxConfig::new(Mode::A, 5, 3), c()),
        ("C SO3", TxConfig::new(Mode::C, 3, 3), c()),
        ("D SO3", TxConfig::new(Mode::D, 3, 2), c()),
        ("D SO5", TxConfig::new(Mode::D, 5, 3), c()),
        ("B SO3 ppm-50", TxConfig::new(Mode::B, 3, 3), Channel { ppm: -50.0, freq_hz: -1234.0, ..c() }),
        ("B SO3 f-1234", TxConfig::new(Mode::B, 3, 3), Channel { freq_hz: -1234.0, ..c() }),
        ("B SO3 f-600", TxConfig::new(Mode::B, 3, 3), Channel { freq_hz: -600.0, ..c() }),
        ("B SO3 f+1234", TxConfig::new(Mode::B, 3, 3), Channel { freq_hz: 1234.0, ..c() }),
        ("B SO3 p-50only", TxConfig::new(Mode::B, 3, 3), Channel { ppm: -50.0, ..c() }),
        ("B SO3 ppm+50", TxConfig::new(Mode::B, 3, 3), Channel { ppm: 50.0, freq_hz: 777.0, ..c() }),
        (
            "B 2path",
            TxConfig::new(Mode::B, 3, 3),
            Channel { paths: vec![(0.0, 0.5, 1.0), (2e-3, 0.5, 1.0)], snr_db: Some(25.0), ..c() },
        ),
        (
            "B static2path",
            TxConfig::new(Mode::B, 3, 3),
            Channel { paths: vec![(0.0, 0.5, 0.0), (2e-3, 0.5, 0.0)], ..c() },
        ),
        (
            "B static2p-1ms",
            TxConfig::new(Mode::B, 3, 3),
            Channel { paths: vec![(0.0, 0.6, 0.0), (1e-3, 0.4, 0.0)], ..c() },
        ),
        ("B xhe", TxConfig { xhe: true, rate_code: 4, sbr: false, ..TxConfig::new(Mode::B, 3, 3) }, c()),
        ("B aac24 stereo", TxConfig { rate_code: 3, sbr: false, audio_mode: 2, ..TxConfig::new(Mode::B, 3, 3) }, c()),
        ("B uep", TxConfig { len_a: 80, pl_a: 0, pl_b: 2, ..TxConfig::new(Mode::B, 3, 3) }, c()),
        ("B short sdc4", TxConfig { long_interleave: false, sdc_qam16: false, ..TxConfig::new(Mode::B, 3, 2) }, c()),
        (
            "A 2path",
            TxConfig::new(Mode::A, 3, 3),
            Channel { paths: vec![(0.0, 0.6, 0.5), (1e-3, 0.4, 0.5)], snr_db: Some(25.0), ..c() },
        ),
        (
            "D 2path",
            TxConfig::new(Mode::D, 3, 2),
            Channel { paths: vec![(0.0, 0.5, 2.0), (4e-3, 0.5, 2.0)], snr_db: Some(20.0), ..c() },
        ),
        (
            "D static2p4ms",
            TxConfig::new(Mode::D, 3, 2),
            Channel { paths: vec![(0.0, 0.6, 0.0), (4e-3, 0.4, 0.0)], ..c() },
        ),
        (
            "D static2p1ms",
            TxConfig::new(Mode::D, 3, 2),
            Channel { paths: vec![(0.0, 0.6, 0.0), (1e-3, 0.4, 0.0)], ..c() },
        ),
        (
            "C static2p2ms",
            TxConfig::new(Mode::C, 3, 2),
            Channel { paths: vec![(0.0, 0.6, 0.0), (2e-3, 0.4, 0.0)], ..c() },
        ),
        ("B 2pslow", TxConfig::new(Mode::B, 3, 3), Channel { paths: vec![(0.0, 0.5, 0.1), (2e-3, 0.5, 0.1)], ..c() }),
        ("B 2p1ms", TxConfig::new(Mode::B, 3, 3), Channel { paths: vec![(0.0, 0.5, 1.0), (1e-3, 0.5, 1.0)], ..c() }),
        ("B 2p05ms", TxConfig::new(Mode::B, 3, 3), Channel { paths: vec![(0.0, 0.5, 1.0), (0.5e-3, 0.5, 1.0)], ..c() }),
        ("B fade1path", TxConfig::new(Mode::B, 3, 3), Channel { paths: vec![(0.0, 1.0, 1.0)], ..c() }),
        ("acq B16 4dB", TxConfig::new(Mode::B, 3, 2), Channel { snr_db: Some(4.0), freq_hz: 800.0, ..c() }),
        ("acq B16 6dB", TxConfig::new(Mode::B, 3, 2), Channel { snr_db: Some(6.0), freq_hz: 800.0, ..c() }),
        ("acq B16 12dB", TxConfig::new(Mode::B, 3, 2), Channel { snr_db: Some(12.0), freq_hz: 800.0, ..c() }),
        ("acq A16 6dB", TxConfig::new(Mode::A, 3, 2), Channel { snr_db: Some(6.0), freq_hz: -800.0, ..c() }),
        (
            "acq B 2p 12dB",
            TxConfig::new(Mode::B, 3, 2),
            Channel { paths: vec![(0.0, 0.5, 1.0), (2e-3, 0.5, 1.0)], snr_db: Some(12.0), ..c() },
        ),
        (
            "acq B 2p 16dB",
            TxConfig::new(Mode::B, 3, 2),
            Channel { paths: vec![(0.0, 0.5, 1.0), (2e-3, 0.5, 1.0)], snr_db: Some(16.0), ..c() },
        ),
        (
            "B 2path-16",
            TxConfig::new(Mode::B, 3, 2),
            Channel { paths: vec![(0.0, 0.5, 1.0), (2e-3, 0.5, 1.0)], snr_db: Some(25.0), ..c() },
        ),
    ];
    let only = std::env::var("DRM_CASE").ok();
    for (name, cfg, ch) in cases {
        if only.as_ref().is_some_and(|o| !name.contains(o.as_str())) {
            continue;
        }
        let t = std::time::Instant::now();
        let r = run(cfg, &ch, 10);
        println!("{} ({:.2} s)", summary_line(name, &r), t.elapsed().as_secs_f32());
    }
}

/// Steady state decoding rates through noise: `preroll` super frames
/// without noise (acquisition, the interleaver filling), then `measure`
/// super frames at `snr_db` in the signal's bandwidth. Counted between
/// the first and the last super frame the receiver finished in the noisy
/// part (two super frames in, so that nothing of the clean part is left
/// in the cell interleaver): the shares of FAC blocks, SDC blocks and
/// audio super frames (every frame intact) that came through.
fn measure(cfg: TxConfig, ch: &Channel, snr_db: f64, preroll: usize, measure: usize) -> [f64; 3] {
    let mut tx = Tx::new(cfg);
    let mut sig = Vec::new();
    for _ in 0..preroll + measure {
        sig.extend(tx.superframe());
    }
    let bw = (tx.map.ncar as f64) * tx.cfg.mode.spacing_hz();
    let mut x = ch.run(&sig, bw);
    let start = (preroll as f64 * 1.2 * ch.fs) as usize;
    let p: f64 = x[start..].iter().map(|v| v.norm_sqr() as f64).sum::<f64>() / (x.len() - start) as f64;
    let sigma = (p / 10f64.powf(snr_db / 10.0) * ch.fs / bw / 2.0).sqrt();
    let mut rng = Rng::new(ch.seed ^ 0x5EED);
    for v in x[start..].iter_mut() {
        *v += Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32);
    }
    let sent: std::collections::HashSet<Vec<u8>> = tx.sent.iter().flat_map(|s| s.units.iter().cloned()).collect();
    let mut rx = DrmReceiver::new(ch.fs as f32);
    let from = start + (2.4 * ch.fs) as usize;
    let chunk = (ch.fs * 0.02) as usize;
    // (time, cumulative FAC ok, SDC ok, good audio super frames) at each
    // super frame finished after `from`.
    let mut marks: Vec<(f64, u64, u64, u64)> = Vec::new();
    let mut good = 0u64;
    for (i, c) in x.chunks(chunk).enumerate() {
        rx.push_iq(c);
        let t = (i * chunk) as f64 / ch.fs;
        for e in rx.take() {
            match e {
                DrmEvent::AudioSuperFrame { frames, .. } => {
                    if !frames.is_empty() && frames.iter().all(|f| f.ok && sent.contains(&f.data)) {
                        good += 1;
                    }
                }
                DrmEvent::Stats(s) if i * chunk >= from => marks.push((t, s.fac_ok, s.sdc_ok, good)),
                _ => {}
            }
        }
    }
    let end_t = x.len() as f64 / ch.fs;
    let Some(&(t0, f0, s0, a0)) = marks.first() else { return [0.0; 3] };
    let &(t1, f1, s1, a1) = marks.last().unwrap();
    // Super frames from the first mark to the end of the signal: any lost
    // after the last mark count as failures.
    let k = ((end_t - t0) / 1.2).floor().max(1.0);
    let _ = t1;
    [(f1 - f0) as f64 / (3.0 * k), (s1 - s0) as f64 / k, (a1 - a0) as f64 / (3.0 * k)]
}

/// The SNR (in the signal's bandwidth) at which FAC, SDC and MSC decode
/// reliably, per mode and constellation, on a plain channel and through
/// two-path fading, in steady state (see `measure`). Reliable: at least
/// 99 % of FAC blocks, 95 % of SDC blocks and 95 % of audio super frames,
/// over three runs of 15 super frames each. Slow; run it by name in
/// release.
#[test]
#[ignore]
fn snr_table() {
    let configs =
        [("A 16-QAM", Mode::A, 2usize), ("A 64-QAM", Mode::A, 3), ("B 16-QAM", Mode::B, 2), ("B 64-QAM", Mode::B, 3)];
    for (cname, mode, levels) in configs {
        // Two equal Rayleigh paths with 1 Hz Doppler, 2 ms apart (1 ms in
        // mode A, whose guard is 2.7 ms).
        let fading = if mode == Mode::A { 1e-3 } else { 2e-3 };
        for (chname, paths) in [("AWGN", vec![(0.0, 1.0, 0.0)]), ("2-path", vec![(0.0, 0.5, 1.0), (fading, 0.5, 1.0)])]
        {
            let cfg = TxConfig::new(mode, 3, levels);
            let mut found = [None::<f64>; 3];
            let need = [0.99, 0.95, 0.95];
            let mut line = String::new();
            for snr in (0..=34).step_by(2).map(|s| s as f64) {
                let mut rates = [0.0; 3];
                for seed in 1..=3u64 {
                    let ch = Channel {
                        paths: paths.clone(),
                        ppm: 30.0,
                        freq_hz: 200.0,
                        seed: seed * 101,
                        ..Channel::clean(47_904.0)
                    };
                    let r = measure(TxConfig { seed, ..cfg.clone() }, &ch, snr, 4, 15);
                    for (a, b) in rates.iter_mut().zip(r) {
                        *a += b / 3.0;
                    }
                }
                line += &format!(" {snr:.0}:{:.0}/{:.0}/{:.0}", 100.0 * rates[0], 100.0 * rates[1], 100.0 * rates[2]);
                for i in 0..3 {
                    if found[i].is_none() && rates[i] >= need[i] {
                        found[i] = Some(snr);
                    }
                }
                if found.iter().all(|f| f.is_some()) && rates.iter().all(|&r| r >= 0.99) {
                    break;
                }
            }
            let show = |v: Option<f64>| v.map_or("> 34".to_string(), |v| format!("{v:.0}"));
            println!(
                "{cname} {chname:<6}: FAC {} dB, SDC {} dB, MSC {} dB   (FAC/SDC/MSC % by SNR:{line})",
                show(found[0]),
                show(found[1]),
                show(found[2])
            );
        }
    }
}

/// Real-time factor of the receiver alone, mode B 64-QAM, at 47 904 Hz
/// through a fading channel.
#[test]
#[ignore]
fn real_time_factor() {
    for (name, cfg, paths) in [
        ("B SO3 64-QAM", TxConfig::new(Mode::B, 3, 3), vec![(0.0, 0.5, 1.0), (2e-3, 0.5, 1.0)]),
        ("A SO5 64-QAM", TxConfig::new(Mode::A, 5, 3), vec![(0.0, 1.0, 0.0)]),
    ] {
        let mut tx = Tx::new(cfg);
        let mut sig = Vec::new();
        for _ in 0..25 {
            sig.extend(tx.superframe());
        }
        let ch = Channel { paths, snr_db: Some(25.0), ..Channel::clean(47_904.0) };
        let rx_in = ch.run(&sig, 10_000.0);
        let mut rx = DrmReceiver::new(47_904.0);
        let t = std::time::Instant::now();
        let mut n = 0;
        for c in rx_in.chunks(958) {
            rx.push_iq(c);
            n += rx.take().len();
        }
        let secs = rx_in.len() as f64 / 47_904.0;
        let el = t.elapsed().as_secs_f64();
        println!("{name}: {secs:.1} s of signal in {el:.3} s: {:.1} % of one core ({n} events)", 100.0 * el / secs);
    }
    // Searching (noise only) costs the guard correlation of four modes.
    let mut rng = Rng::new(3);
    let noise: Vec<Complex32> =
        (0..47_904 * 30).map(|_| Complex32::new(rng.gauss() as f32, rng.gauss() as f32)).collect();
    let mut rx = DrmReceiver::new(47_904.0);
    let t = std::time::Instant::now();
    for c in noise.chunks(958) {
        rx.push_iq(c);
        rx.take();
    }
    let el = t.elapsed().as_secs_f64();
    println!("searching noise: 30.0 s in {el:.3} s: {:.1} % of one core", 100.0 * el / 30.0);
}

/// The multilevel decoder alone on cells with white noise and the channel
/// known: the SNR per cell at which a multiplex frame comes through
/// whole, as a reference for what the OFDM chain costs.
#[test]
#[ignore]
fn mlc_reference() {
    let m = CellMap::new(Mode::B, 3).unwrap();
    let mut vit = Viterbi::new();
    let mut rng = Rng::new(4);
    for (name, mlc) in [
        ("16-QAM PL0 (R 0.5)", Mlc::msc(m.n_mux, 2, 0, 0, 0).unwrap()),
        ("64-QAM PL1 (R 0.6)", Mlc::msc(m.n_mux, 3, 0, 1, 0).unwrap()),
        ("SDC 16-QAM", Mlc::sdc(m.n_sdc(), true).unwrap()),
        ("FAC", Mlc::fac()),
    ] {
        let mut line = String::new();
        for snr10 in (20..=200).step_by(5) {
            let snr = snr10 as f64 / 10.0;
            let sigma = (0.5 / 10f64.powf(snr / 10.0)).sqrt();
            let mut ok = 0;
            for _ in 0..20 {
                let bits: Vec<u8> = (0..mlc.bits()).map(|_| (rng.next() & 1) as u8).collect();
                let cells: Vec<Complex32> = mlc
                    .encode(&bits)
                    .iter()
                    .map(|c| c + Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32))
                    .collect();
                let (out, _) = mlc.decode(&cells, &vec![1.0; cells.len()], &mut vit);
                if out == bits {
                    ok += 1;
                }
            }
            if ok >= 19 {
                line = format!("{snr:.1} dB");
                break;
            }
        }
        println!("{name}: 95 % of blocks whole at {line}");
    }
}

#[test]
#[ignore]
fn snr_point() {
    let snr: f64 = std::env::var("DRM_SNR").map(|v| v.parse().unwrap()).unwrap_or(18.0);
    let levels: usize = std::env::var("DRM_LEVELS").map(|v| v.parse().unwrap()).unwrap_or(3);
    let fading = std::env::var("DRM_FADE").is_ok();
    let paths = if fading { vec![(0.0, 0.5, 1.0), (2e-3, 0.5, 1.0)] } else { vec![(0.0, 1.0, 0.0)] };
    let mut rates = [0.0; 3];
    for seed in 1..=3u64 {
        let ch =
            Channel { paths: paths.clone(), ppm: 30.0, freq_hz: 200.0, seed: seed * 101, ..Channel::clean(47_904.0) };
        let r = measure(TxConfig { seed, ..TxConfig::new(Mode::B, 3, levels) }, &ch, snr, 4, 15);
        for (a, b) in rates.iter_mut().zip(r) {
            *a += b / 3.0;
        }
    }
    println!(
        "B levels {levels} fading {fading} at {snr} dB: FAC/SDC/MSC {:.2} {:.2} {:.2}",
        rates[0], rates[1], rates[2]
    );
}
