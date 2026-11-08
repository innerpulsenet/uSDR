use super::*;
use std::f64::consts::{PI, TAU};

struct Rng(u64);
impl Rng {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        let u = self.uniform().max(1e-12);
        let v = self.uniform();
        (-2.0 * u.ln()).sqrt() * (TAU * v).cos()
    }
}

/// Complementary-error-function approximation good enough for the pulse.
fn erf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.5 * x.abs());
    let r = t * (-x * x - 1.26551223 + t * (1.00002368 + t * (0.37409196 + t * (0.09678418 + t * (-0.18628806 + t * (0.27886807 + t * (-1.13520398 + t * (1.48851587 + t * (-0.82215223 + t * 0.17087277))))))))).exp();
    if x >= 0.0 {
        1.0 - r
    } else {
        r - 1.0
    }
}

/// `gfsk_pulse(2.0, t)`.
fn pulse(t: f64) -> f64 {
    let c = PI * (2.0 / 2f64.ln()).sqrt();
    0.5 * (erf(c * 2.0 * (t + 0.5)) - erf(c * 2.0 * (t - 0.5)))
}

/// The instantaneous phase step of a frame, from `gen_fst4wave.f90`.
fn frame_phase(tones: &[u8; 160], nsps: usize, f0: f64, fs: f64, df_total: f64) -> (Vec<f64>, Vec<f64>) {
    let n = 160 * nsps;
    let mut dphi = vec![0f64; (160 + 2) * nsps];
    let peak = TAU / nsps as f64;
    let p: Vec<f64> = (0..3 * nsps).map(|i| pulse((i as f64 + 1.0 - 1.5 * nsps as f64) / nsps as f64)).collect();
    for (j, &t) in tones.iter().enumerate() {
        for (i, v) in p.iter().enumerate() {
            dphi[j * nsps + i] += peak * v * t as f64;
        }
    }
    let tsym = nsps as f64 / fs;
    let shift = TAU * (f0 - 1.5 / tsym) / fs;
    let mut phi = 0f64;
    let (mut re, mut im) = (Vec::with_capacity(n), Vec::with_capacity(n));
    for j in nsps..(160 + 1) * nsps {
        let i = (j - nsps) as f64;
        // A linear drift across the frame, for the fit test.
        let dr = TAU * df_total * i / n as f64 / fs;
        re.push(phi.cos());
        im.push(phi.sin());
        phi += dphi[j] + shift + dr;
    }
    let ramp = (nsps / 4).max(1);
    for k in 0..ramp {
        let w = (1.0 - (TAU * k as f64 / (nsps as f64 / 2.0)).cos()) / 2.0;
        re[k] *= w;
        im[k] *= w;
    }
    for k in 0..=ramp {
        let w = (1.0 + (TAU * k as f64 / (nsps as f64 / 2.0)).cos()) / 2.0;
        let i = n - 1 - ramp + k;
        re[i] *= w;
        im[i] *= w;
    }
    (re, im)
}

/// `text` as real audio at `fs` (`period` long, signal at unit amplitude
/// starting `dt` late of nominal), with white noise so the signal sits at
/// `snr_db` in 2500 Hz.
fn audio(text: &str, period: Period, f: f64, dt: f64, snr_db: f64, seed: u64) -> Vec<f32> {
    let fs = 12_000.0;
    let n = period.secs() as usize * 12_000;
    let tones = channel_tones(text).unwrap();
    let (_, im) = frame_phase(&tones, period.nsps() as usize, f, fs, 0.0);
    let snr = 10f64.powf(snr_db / 10.0);
    // Signal amplitude a: power a^2/2. Noise of unit variance has 2500/6000
    // of its power in 2500 Hz.
    let a = (2.0 * 2500.0 / 6000.0 * snr).sqrt();
    let mut rng = Rng(seed | 1);
    let mut out: Vec<f32> = (0..n).map(|_| rng.gauss() as f32).collect();
    let k = ((dt + 1.0) * fs).round() as usize;
    for (i, v) in im.iter().enumerate().take(n.saturating_sub(k)) {
        out[i + k] += (a * v) as f32;
    }
    out
}

/// A slot of baseband directly (no mixing or filter), for the long periods,
/// where the generator can run at 375 Hz with a whole number of samples a
/// symbol. `f` is relative to center.
fn baseband(text: &str, period: Period, f: f64, dt: f64, snr_db: f64, seed: u64, drift: f64) -> Vec<Complex32> {
    assert_ne!(period, Period::S120);
    let fs = 375.0;
    let n = (period.keep_secs() as f64 * fs) as usize;
    let tones = channel_tones(text).unwrap();
    let (re, im) = frame_phase(&tones, period.nsps() as usize / 32, f, fs, drift);
    let snr = 10f64.powf(snr_db / 10.0);
    // Unit-amplitude complex signal: noise per Hz 1/(snr*2500).
    let sigma = (fs / (snr * 2500.0) / 2.0).sqrt();
    let mut rng = Rng(seed | 1);
    let mut out: Vec<Complex32> = (0..n).map(|_| Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32)).collect();
    let k = ((dt + 1.0) * fs).round() as usize;
    for i in 0..re.len().min(n.saturating_sub(k)) {
        out[i + k] += Complex32::new(re[i] as f32, im[i] as f32);
    }
    out
}

fn noise_baseband(period: Period, seed: u64) -> Vec<Complex32> {
    let n = (period.keep_secs() as f64 * 375.0) as usize;
    let mut rng = Rng(seed | 1);
    (0..n).map(|_| Complex32::new(rng.gauss() as f32, rng.gauss() as f32)).collect()
}

fn check_one(d: &[Fst4wDecode], text: &str, period: Period) -> Fst4wDecode {
    assert_eq!(d.len(), 1, "expected one decode, got {d:?}");
    assert_eq!(d[0].text, text);
    assert_eq!(d[0].period, period);
    assert_eq!(d[0].tag, period.tag());
    d[0].clone()
}

#[test]
fn ldpc_tables_are_consistent() {
    let mut rng = Rng(7);
    for _ in 0..200 {
        let payload = (rng.uniform() * (1u64 << 50) as f64) as u64;
        let m = ldpc::with_crc(payload);
        assert_eq!(ldpc::crc24(&m), 0);
        assert_eq!(ldpc::payload(&m), Some(payload));
        let c = ldpc::encode(&m);
        assert_eq!(ldpc::syndrome_weight(&c), 0, "generator and parity checks disagree");
    }
}

#[test]
fn ldpc_round_trip() {
    let mut rng = Rng(11);
    let mut bp = 0;
    let mut osd = 0;
    let trials = 300;
    for t in 0..trials {
        let payload = (rng.uniform() * (1u64 << 50) as f64) as u64;
        let m = ldpc::with_crc(payload);
        let c = ldpc::encode(&m);
        // A decoder gets the channel bits with fairly many flips.
        let sigma = if t % 2 == 0 { 0.7 } else { 0.95 };
        let mut llr = [0f32; ldpc::N];
        for (i, v) in llr.iter_mut().enumerate() {
            let s = if c[i] == 1 { 1.0 } else { -1.0 };
            *v = (2.0 * (s + sigma * rng.gauss()) / (sigma * sigma)) as f32;
        }
        let d = ldpc::decode(&llr);
        let d = d.expect("decodes");
        assert_eq!(ldpc::payload(&d.message), Some(payload));
        if d.bp {
            bp += 1;
        } else {
            osd += 1;
        }
    }
    println!("ldpc: {trials} trials, {bp} by propagation, {osd} by OSD");
    assert!(osd > 0, "the OSD path was not exercised");
}

#[test]
fn message_round_trip() {
    let mut h = CallHashes::default();
    for (text, kind) in [
        ("K1ABC FN42 37", "type1"),
        ("W9XYZ EN37 0", "type1"),
        ("G3ABC IO91 30", "type1"),
        ("PJ4/K1ABC 37", "type2"),
        ("K1ABC/7 20", "type2"),
        ("K1ABC/P 10", "type2"),
    ] {
        let bits = pack::pack(text).expect("packs");
        let m = pack::unpack(bits, &mut h).expect("unpacks");
        assert_eq!(m.text, text);
        assert_eq!(m.kind, kind);
    }
    let bits = pack::pack("<K1ABC> FN42AX").unwrap();
    let m = pack::unpack(bits, &mut h).unwrap();
    assert_eq!(m.kind, "type3");
    assert_eq!(m.grid.as_deref(), Some("FN42AX"));
    assert_eq!(m.power_dbm, None);
    assert_eq!(m.call, "<K1ABC>");
    assert_eq!(m.text, "<K1ABC> FN42AX");
}

#[test]
fn clean_120_audio() {
    let text = "K1ABC FN42 37";
    let a = audio(text, Period::S120, 1531.4, 0.3, -20.0, 1);
    let mut h = CallHashes::default();
    let t0 = std::time::Instant::now();
    let d = decode_audio(&a, 12_000.0, Period::S120, &mut h);
    println!("120 s decode took {:?}", t0.elapsed());
    let x = check_one(&d, text, Period::S120);
    println!("120 s: snr {}, dt {}, f {}, drift {}", x.snr_db, x.dt_s, x.freq_hz, x.drift_hz_per_min);
    assert!((x.freq_hz - 1531.4).abs() < 0.2, "freq {}", x.freq_hz);
    assert!((x.dt_s - 0.3).abs() < 0.15, "dt {}", x.dt_s);
    assert!((x.snr_db + 20.0).abs() < 2.0, "snr {}", x.snr_db);
    assert_eq!(x.call, "K1ABC");
    assert_eq!(x.grid.as_deref(), Some("FN42"));
    assert_eq!(x.power_dbm, Some(37));
}

#[test]
fn clean_300_baseband() {
    clean_long(Period::S300);
}

#[test]
#[ignore = "slow"]
fn clean_900_baseband() {
    clean_long(Period::S900);
}

#[test]
#[ignore = "slow"]
fn clean_1800_baseband() {
    clean_long(Period::S1800);
}

fn clean_long(p: Period) {
    let text = "K1ABC FN42 37";
    let z = baseband(text, p, 31.4, 0.3, -25.0, 2, 0.0);
    let mut h = CallHashes::default();
    let t0 = std::time::Instant::now();
    let d = decode_baseband(&z, p, &mut h);
    let x = check_one(&d, text, p);
    println!("{}: {:?}; snr {}, dt {}, f {}", p.tag(), t0.elapsed(), x.snr_db, x.dt_s, x.freq_hz);
    assert!((x.freq_hz - 1531.4).abs() < 0.5 * p.baud().max(0.1), "freq {}", x.freq_hz);
    assert!((x.dt_s - 0.3).abs() < 0.5, "dt {}", x.dt_s);
}

#[test]
fn type2_and_type3_in_one_slot_named() {
    let mut h = CallHashes::default();
    let a: Vec<f32> = audio("K1ABC FN42 37", Period::S120, 1450.0, 0.0, -20.0, 3)
        .iter()
        .zip(audio("<K1ABC> FN42AX", Period::S120, 1550.0, 0.0, -20.0, 4).iter())
        .map(|(a, b)| a + b)
        .collect();
    let d = decode_audio(&a, 12_000.0, Period::S120, &mut h);
    assert_eq!(d.len(), 2, "{d:?}");
    let t3 = d.iter().find(|x| x.kind == "type3").unwrap();
    assert_eq!(t3.call, "<K1ABC>");
    assert_eq!(t3.grid.as_deref(), Some("FN42AX"));
}

#[test]
fn low_snr_120() {
    // Near the mode's advertised limit of about -33 dB for this period.
    let text = "K1ABC FN42 37";
    let mut hit = 0;
    let trials = 24;
    let snr = -32.0;
    for seed in 0..trials {
        let a = audio(text, Period::S120, 1500.0 + (seed as f64 % 5.0) * 7.3 - 15.0, 0.1, snr, 1000 + seed);
        let mut h = CallHashes::default();
        let d = decode_audio(&a, 12_000.0, Period::S120, &mut h);
        if d.iter().any(|x| x.text == text) {
            hit += 1;
        }
    }
    println!("-32 dB, 120 s: {hit}/{trials} decoded");
    assert!(hit >= 12, "{hit}/{trials} at {snr} dB");
}

#[test]
fn pure_noise_prints_nothing() {
    let mut h = CallHashes::default();
    let t0 = std::time::Instant::now();
    let mut n = 0;
    let trials = 500;
    for seed in 0..trials {
        let z = noise_baseband(Period::S120, 7_000 + seed);
        let d = decode_baseband(&z, Period::S120, &mut h);
        n += d.len();
        if !d.is_empty() {
            println!("false decode, seed {seed}: {d:?}");
        }
    }
    let each = t0.elapsed() / trials as u32;
    println!("noise: {n} decodes in {trials} slots, {each:?} each");
    assert_eq!(n, 0);
}

/// FT8-style 8-FSK: 79 symbols of 6.25 Hz at 6.25 baud, random tones.
fn ft8_like(out: &mut [f32], fs: f32, f0: f64, at_s: f64, amp: f64, seed: u64) {
    let mut rng = Rng(seed | 1);
    let sps = (fs as f64 / 6.25) as usize;
    let mut phi = 0f64;
    let start = (at_s * fs as f64) as usize;
    for k in 0..79 {
        let tone = (rng.uniform() * 8.0) as u32 as f64;
        let dp = TAU * (f0 + tone * 6.25) / fs as f64;
        for i in 0..sps {
            if let Some(v) = out.get_mut(start + k * sps + i) {
                *v += (amp * phi.sin()) as f32;
            }
            phi += dp;
        }
    }
}

#[test]
fn interference_prints_nothing() {
    let fs = 12_000.0f32;
    let trials = 8;
    let mut n = 0;
    let t0 = std::time::Instant::now();
    for trial in 0..trials {
        let mut rng = Rng(9_000 + trial);
        let len = 120 * 12_000;
        let mut a: Vec<f32> = (0..len).map(|_| rng.gauss() as f32).collect();
        // Steady carriers across the passband, some on FST4W's own grid of
        // tones, others not.
        for (f, amp) in [(1432.3, 0.3), (1481.7, 0.5), (1503.0, 0.8), (1542.25, 0.2), (1577.9, 0.4)] {
            for (i, v) in a.iter_mut().enumerate() {
                *v += (amp * (TAU * f * i as f64 / fs as f64).sin()) as f32;
            }
        }
        // FT8-like bursts in every 15 s slot, within the passband.
        let mut slot = 0;
        while slot < 8 {
            let mut bursts = 0;
            while bursts < 6 {
                let f0 = 1400.0 + rng.uniform() * 200.0;
                ft8_like(&mut a, fs, f0, 15.0 * slot as f64 + 0.5, 0.4, 100 * trial + slot + bursts);
                bursts += 1;
            }
            slot += 1;
        }
        let mut h = CallHashes::default();
        let d = decode_audio(&a, fs, Period::S120, &mut h);
        n += d.len();
        if !d.is_empty() {
            println!("false decode, trial {trial}: {d:?}");
        }
    }
    println!("interference: {n} decodes in {trials} slots, {:?} each", t0.elapsed() / trials as u32);
    assert_eq!(n, 0);
}

#[test]
fn drift_is_reported() {
    let p = Period::S300;
    for tones in [-0.6, -0.3, 0.0, 0.3, 0.6] {
        let total = tones * p.baud() as f64;
        let z = baseband("K1ABC FN42 37", p, -20.0, 0.0, -25.0, 21, total);
        let mut h = CallHashes::default();
        let d = decode_baseband(&z, p, &mut h);
        let x = check_one(&d, "K1ABC FN42 37", p);
        let expect = total as f32 / (p.frame_secs() / 60.0);
        println!("drift {tones}: {:.4} Hz/min, sent {expect:.4}", x.drift_hz_per_min);
        assert!((x.drift_hz_per_min - expect).abs() < 0.1 * p.baud() / (p.frame_secs() / 60.0));
    }
}

#[test]
fn stream_cuts_slots() {
    let fs = 12_000.0f32;
    let p = Period::S120;
    let t0 = 1_700_000_040_000u64 - (1_700_000_040_000u64 % 120_000) + 240_000; // A slot boundary.
    let text = "K1ABC FN42 37";
    // Two slots: one with the signal, then silence to cut it.
    let mut dec = Fst4wDecoder::with_periods(fs, &[p]);
    let block = 1200usize;
    let mut sent = 0u64;
    let one = audio(text, p, 1510.0, 0.0, -20.0, 5);
    for chunk in one.chunks(block) {
        dec.push(chunk, t0 + sent * 1000 / 12_000);
        sent += chunk.len() as u64;
    }
    let tail = vec![0f32; (130.0 * fs) as usize - one.len() + 4000];
    for chunk in tail.chunks(block) {
        dec.push(chunk, t0 + sent * 1000 / 12_000);
        sent += chunk.len() as u64;
    }
    let mut got = Vec::new();
    for _ in 0..600 {
        got.extend(dec.take());
        if !got.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, t0);
    assert_eq!(got[0].1.len(), 1);
    assert_eq!(got[0].1[0].text, text);
    let s = dec.status();
    assert_eq!(s.last_count, Some(1));
    assert_eq!(s.last_slot_ms, Some(t0));
}

#[test]
#[ignore = "sweep, prints a curve"]
fn sensitivity_sweep() {
    let text = "K1ABC FN42 37";
    for (p, snrs) in [
        (Period::S120, vec![-30.0, -31.0, -32.0, -33.0, -34.0]),
        (Period::S300, vec![-34.0, -35.0, -36.0, -37.0, -38.0]),
        (Period::S900, vec![-39.0, -40.0, -41.0, -42.0, -43.0]),
        (Period::S1800, vec![-42.0, -43.0, -44.0, -45.0, -46.0]),
    ] {
        for snr in snrs {
            let mut hit = 0;
            let trials = 40;
            for seed in 0..trials {
                let d = if p == Period::S120 {
                    let a = audio(text, p, 1500.0 + (seed as f64 % 5.0) * 7.3 - 15.0, 0.1, snr, 50_000 + seed);
                    decode_audio(&a, 12_000.0, p, &mut CallHashes::default())
                } else {
                    let z = baseband(text, p, (seed as f64 % 5.0) * 7.3 - 15.0, 0.1, snr, 50_000 + seed, 0.0);
                    decode_baseband(&z, p, &mut CallHashes::default())
                };
                if d.iter().any(|x| x.text == text) {
                    hit += 1;
                }
            }
            println!("{} {snr} dB: {hit}/{trials}", p.tag());
        }
    }
}

#[test]
fn noise_longer_periods() {
    for (p, trials) in [(Period::S300, 50u64), (Period::S900, 8), (Period::S1800, 4)] {
        let mut h = CallHashes::default();
        let mut n = 0;
        let t0 = std::time::Instant::now();
        for seed in 0..trials {
            let d = decode_baseband(&noise_baseband(p, 8_000 + seed), p, &mut h);
            n += d.len();
        }
        println!("{} noise: {n} decodes in {trials} slots, {:?} each", p.tag(), t0.elapsed() / trials as u32);
        assert_eq!(n, 0);
    }
}

/// How often the decoders pass a CRC on random soft bits, and the hard-error
/// counts of what does: the false decode a candidate that got this far costs.
#[test]
#[ignore = "statistics"]
fn crc_false_passes_on_random_llrs() {
    let mut rng = Rng(77);
    let mut passes = 0u32;
    let trials = 200_000u32;
    let (mut valid, mut errs) = (0, Vec::new());
    let mut h = CallHashes::default();
    let t0 = std::time::Instant::now();
    for _ in 0..trials {
        let mut llr = [0f32; ldpc::N];
        for v in llr.iter_mut() {
            *v = (2.83 * rng.gauss()) as f32;
        }
        if let Some(d) = ldpc::decode(&llr) {
            passes += 1;
            errs.push(d.hard_errors);
            if pack::unpack(ldpc::payload(&d.message).unwrap(), &mut h).is_some() {
                valid += 1;
            }
        }
    }
    println!("{passes} CRC passes in {trials} ({valid} unpack); hard errors {errs:?}; {:?} per decode", t0.elapsed() / trials);
}

/// Hard-error counts of real decodes at the edge of decoding.
#[test]
#[ignore = "statistics"]
fn hard_errors_of_edge_decodes() {
    let text = "K1ABC FN42 37";
    let mut errs = Vec::new();
    for seed in 0..60 {
        let a = audio(text, Period::S120, 1500.0 + (seed as f64 % 5.0) * 7.3 - 15.0, 0.1, -33.0, 60_000 + seed);
        for x in decode_audio(&a, 12_000.0, Period::S120, &mut CallHashes::default()) {
            assert_eq!(x.text, text);
            errs.push(x.hard_errors);
        }
    }
    errs.sort();
    println!("{} decodes at -33 dB; hard errors {errs:?}", errs.len());
}
