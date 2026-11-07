use super::*;
use std::f64::consts::TAU;

/// White Gaussian noise, reproducible.
struct Noise(u64);

impl Noise {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        let (u, v) = (self.uniform(), self.uniform());
        (-2.0 * u.ln()).sqrt() * (TAU * v).cos()
    }
}

#[derive(Clone)]
struct Tx {
    text: String,
    /// Audio Hz of the tones' center.
    freq: f64,
    /// Total drift across the transmission, Hz.
    drift: f64,
    dt: f64,
}

fn tx(text: &str, freq: f64, drift: f64, dt: f64) -> Tx {
    Tx { text: text.into(), freq, drift, dt }
}

/// The instantaneous frequency of `t` at `s` seconds into its
/// transmission (continuous-phase 4-FSK, drifting linearly).
fn inst_freq(t: &Tx, tones: &[u8; 162], s: f64) -> Option<f64> {
    let sym = 256.0 / 375.0;
    let k = (s / sym).floor();
    if s < 0.0 || k >= 162.0 {
        return None;
    }
    let span = 162.0 * sym;
    Some(t.freq + t.drift * (s / span - 0.5) + (tones[k as usize] as f64 - 1.5) * 375.0 / 256.0)
}

/// A slot of complex baseband at 375 Hz centered on 1500 Hz: unit-power
/// signals in complex noise `snr_db` below them in 2500 Hz.
fn baseband_slot(txs: &[Tx], snr_db: f64, seed: u64) -> Vec<Complex32> {
    let len = (SLOT_KEEP_S * BASEBAND_HZ) as usize;
    let mut noise = Noise(seed);
    let n0 = 1.0 / (10f64.powf(snr_db / 10.0) * 2500.0);
    let sigma = (n0 * BASEBAND_HZ as f64 / 2.0).sqrt();
    let mut z: Vec<(f64, f64)> = (0..len).map(|_| (sigma * noise.gauss(), sigma * noise.gauss())).collect();
    for t in txs {
        let tones = channel_tones(&t.text).expect("packs");
        let mut ph = noise.uniform() * TAU;
        for (n, v) in z.iter_mut().enumerate() {
            let s = n as f64 / BASEBAND_HZ as f64 - 1.0 - t.dt;
            if let Some(f) = inst_freq(t, &tones, s) {
                v.0 += ph.cos();
                v.1 += ph.sin();
                ph += TAU * (f - CENTER_HZ as f64) / BASEBAND_HZ as f64;
            }
        }
    }
    z.into_iter().map(|(a, b)| Complex32::new(a as f32, b as f32)).collect()
}

/// `secs` of real audio at `fs`, unit-variance noise and the signals each
/// `snr_db` above it in 2500 Hz, starting `offset_s` into a slot.
fn audio(txs: &[Tx], snr_db: f64, fs: f64, secs: f64, offset_s: f64, seed: u64) -> Vec<f32> {
    let len = (secs * fs) as usize;
    let mut noise = Noise(seed);
    let mut x: Vec<f64> = (0..len).map(|_| noise.gauss()).collect();
    let amp = (2.0 * 2500.0 / (fs / 2.0) * 10f64.powf(snr_db / 10.0)).sqrt();
    for t in txs {
        let tones = channel_tones(&t.text).expect("packs");
        let mut ph = noise.uniform() * TAU;
        for (n, v) in x.iter_mut().enumerate() {
            let s = offset_s + n as f64 / fs - 1.0 - t.dt;
            if let Some(f) = inst_freq(t, &tones, s) {
                *v += amp * ph.cos();
                ph += TAU * f / fs;
            }
        }
    }
    x.into_iter().map(|v| v as f32).collect()
}

/// Distinct type 1 messages.
fn messages(n: usize) -> Vec<String> {
    let grids = ["FN42", "JO22", "IO91", "EM73", "DM79", "PM95", "QF56", "KP20", "GG66", "RE78"];
    (0..n)
        .map(|i| {
            let call = format!("{}{}{}{}", ["K", "W", "N", "G", "F", "DL", "VK", "JA"][i % 8], i % 10, (b'A' + (i / 3 % 26) as u8) as char, (b'A' + (i * 7 % 26) as u8) as char);
            format!("{call} {} {}", grids[i % grids.len()], [23, 30, 37, 20, 33, 10][i % 6])
        })
        .collect()
}

fn check(decodes: &[WsprDecode], txs: &[Tx], snr_db: f64) {
    for d in decodes {
        eprintln!("  {:+3.0} {:+.1} {:7.1} {:+.1} {}", d.snr_db, d.dt_s, d.freq_hz, d.drift_hz_per_min, d.text);
    }
    for t in txs {
        let d = decodes.iter().find(|d| d.text == t.text).unwrap_or_else(|| panic!("{} not decoded", t.text));
        assert!((d.freq_hz as f64 - t.freq).abs() <= 0.3, "{}: freq {} vs {}", t.text, d.freq_hz, t.freq);
        assert!((d.dt_s as f64 - t.dt).abs() <= 0.15, "{}: dt {} vs {}", t.text, d.dt_s, t.dt);
        let rate = t.drift * 60.0 / 110.592;
        assert!((d.drift_hz_per_min as f64 - rate).abs() <= 0.4, "{}: drift {} vs {}", t.text, d.drift_hz_per_min, rate);
        assert!((d.snr_db as f64 - snr_db).abs() <= 1.5, "{}: snr {} vs {}", t.text, d.snr_db, snr_db);
    }
    assert_eq!(decodes.len(), txs.len(), "nothing invented");
}

#[test]
fn lookup3_matches_reference() {
    // The test vectors published with lookup3.c.
    assert_eq!(pack::lookup3(b"", 0), 0xdead_beef);
    assert_eq!(pack::lookup3(b"Four score and seven years ago", 0), 0x1777_0551);
    assert_eq!(pack::lookup3(b"Four score and seven years ago", 1), 0xcd62_8161);
}

#[test]
fn messages_round_trip() {
    let mut hashes = CallHashes::default();
    for text in ["K1ABC FN42 37", "G4ABC IO91 0", "VK2XYZ QF56 60", "PJ4/K1ABC 37", "K1ABC/P 23", "K1ABC/7 10", "K1ABC/12 33", "F/G4ABC 7"] {
        let bits = pack::pack(text).unwrap_or_else(|| panic!("{text} packs"));
        assert!(bits < 1 << 50);
        let m = pack::unpack(bits, &mut hashes).unwrap_or_else(|| panic!("{text} unpacks"));
        assert_eq!(m.text, text);
    }
    let bits = pack::pack("<PJ4/K1ABC> FN42AX 37").expect("type 3 packs");
    let m = pack::unpack(bits, &mut hashes).expect("unpacks");
    assert_eq!(m.text, "<PJ4/K1ABC> FN42AX 37");
    assert_eq!(m.kind, "type3");
    let m = pack::unpack(bits, &mut CallHashes::default()).expect("unpacks");
    assert_eq!(m.text, "<...> FN42AX 37");
    assert!(pack::pack("K1ABC FN42 38").is_none(), "power must end in 0, 3 or 7");
}

#[test]
fn several_signals_decode() {
    let texts = messages(6);
    let txs: Vec<Tx> = texts
        .iter()
        .enumerate()
        .map(|(i, m)| tx(m, 1420.0 + 31.0 * i as f64 + 0.37 * i as f64, [-3.0, 0.0, 2.5, 1.0, -1.5, 3.5][i], [-1.5, 0.0, 0.7, 1.8, -0.4, 0.2][i]))
        .collect();
    let z = baseband_slot(&txs, -24.0, 7);
    let t = std::time::Instant::now();
    let d = decode_baseband(&z, &mut CallHashes::default());
    eprintln!("{} decodes in {:?}", d.len(), t.elapsed());
    check(&d, &txs, -24.0);
}

#[test]
fn noise_decodes_nothing() {
    for seed in 1..=4 {
        let z = baseband_slot(&[], 0.0, 1000 + seed);
        let d = decode_baseband(&z, &mut CallHashes::default());
        assert!(d.is_empty(), "noise decoded as {:?}", d.iter().map(|d| &d.text).collect::<Vec<_>>());
    }
}

/// Eight signals a slot, two slots, at -28 dB: WSJT-X's decoding threshold.
#[test]
fn decodes_at_minus_28() {
    let (mut ok, mut all) = (0, 0);
    for slot in 0..2u64 {
        let txs = sweep_slot(slot);
        let z = baseband_slot(&txs, -28.0, 500 + slot);
        let d = decode_baseband(&z, &mut CallHashes::default());
        all += txs.len();
        ok += txs.iter().filter(|t| d.iter().any(|d| d.text == t.text)).count();
        assert!(d.iter().all(|d| txs.iter().any(|t| t.text == d.text)), "nothing invented");
    }
    assert!(ok * 10 >= all * 9, "{ok}/{all} decoded at -28 dB");
}

#[test]
fn compound_and_hashed_calls() {
    let mut hashes = CallHashes::default();
    // A type 3 message before its sender has been heard in full: the
    // locator and power are known, the call is not.
    let early = [tx("<PJ4/K1ABC> FN42AX 37", 1460.0, 0.0, 0.0)];
    let d = decode_baseband(&baseband_slot(&early, -22.0, 31), &mut hashes);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].text, "<...> FN42AX 37");
    assert_eq!(d[0].call, "<...>");
    // The partners: a compound call (type 2) and a plain one (type 1) whose
    // own type 3 rides in the same slot.
    let first = [tx("PJ4/K1ABC 37", 1440.0, 0.5, 0.3), tx("G4XYZ IO91 23", 1520.0, -1.0, -0.2), tx("<G4XYZ> IO91WM 23", 1580.0, 0.0, 0.5)];
    let d = decode_baseband(&baseband_slot(&first, -22.0, 32), &mut hashes);
    let texts: Vec<&str> = d.iter().map(|d| d.text.as_str()).collect();
    assert_eq!(texts, ["PJ4/K1ABC 37", "G4XYZ IO91 23", "<G4XYZ> IO91WM 23"]);
    let compound = &d[0];
    assert_eq!((compound.kind, compound.call.as_str(), compound.grid.as_deref(), compound.power_dbm), ("type2", "PJ4/K1ABC", None, 37));
    let hashed = &d[2];
    assert_eq!((hashed.kind, hashed.call.as_str(), hashed.grid.as_deref(), hashed.power_dbm), ("type3", "<G4XYZ>", Some("IO91WM"), 23));
    // Next slot, the type 3 alone now names its sender.
    let second = [tx("<PJ4/K1ABC> FN42AX 37", 1490.0, 0.0, 0.0)];
    let d = decode_baseband(&baseband_slot(&second, -22.0, 33), &mut hashes);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].text, "<PJ4/K1ABC> FN42AX 37");
    assert_eq!(d[0].grid.as_deref(), Some("FN42AX"));
}

/// The streaming decoder at the live sample rate: audio pushed in blocks
/// stamped with a jittery clock, starting a few seconds before an even
/// minute, decoded once the slot's audio is in.
#[test]
fn stream_cuts_and_decodes_a_slot() {
    let fs = 47_904.0;
    let slot_ms: u64 = 1_700_000_040_000;
    assert_eq!(slot_ms % SLOT_MS, 0);
    let lead = 3.0;
    let txs = [tx("K1ABC FN42 37", 1450.0, 1.0, 0.4), tx("DL1XYZ JO62 23", 1530.0, -2.0, -0.6), tx("VK2AB QF56 30", 1571.0, 0.0, 1.2)];
    let x = audio(&txs, -24.0, fs, lead + 117.0, -lead, 77);
    let mut dec = WsprDecoder::new(fs as f32);
    assert_eq!(dec.status().slot_secs, None);
    let block = 4_790;
    let mut noise = Noise(5);
    let mut midway = None;
    for (i, chunk) in x.chunks(block).enumerate() {
        let t0 = slot_ms as f64 - lead * 1000.0 + (i * block) as f64 * 1000.0 / fs + (noise.uniform() - 0.5) * 20.0;
        dec.push(chunk, t0 as u64);
        if i * block > x.len() / 2 && midway.is_none() {
            midway = Some(dec.status());
        }
    }
    let midway = midway.expect("status taken");
    assert!(midway.collecting && midway.slot_secs.is_some_and(|s| (55.0..61.0).contains(&s)), "{midway:?}");
    let mut got = Vec::new();
    for _ in 0..600 {
        got.extend(dec.take());
        if !got.is_empty() && !dec.status().decoding {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(got.len(), 1, "one slot cut (the 3 s before it is too little to decode)");
    let (start, d) = &got[0];
    assert_eq!(*start, slot_ms);
    check(d, &txs, -24.0);
    let st = dec.status();
    assert_eq!((st.last_count, st.last_slot_ms, st.decoding), (Some(3), Some(slot_ms), false));
}

/// The decoder on whole slots of audio at the live rate.
#[test]
fn decode_audio_matches_baseband() {
    let fs = 47_904.0;
    let txs = [tx("W1AW FN31 37", 1410.0, 0.0, 0.0), tx("JA1XYZ PM95 20", 1597.0, 1.5, -1.0)];
    let x = audio(&txs, -25.0, fs, 116.0, 0.0, 78);
    let d = decode_audio(&x, fs as f32, &mut CallHashes::default());
    check(&d, &txs, -25.0);
}

/// A crowded band: thirty stations 6.5 Hz apart (their spectra touch), at
/// levels from -8 to -26 dB, with assorted starts and drifts, plus their
/// noise, in one slot.
#[test]
fn busy_band() {
    let texts = messages(30);
    let txs: Vec<Tx> = texts
        .iter()
        .enumerate()
        .map(|(i, m)| tx(m, 1403.0 + 6.5 * i as f64, ((i * 7) % 9) as f64 - 4.0, (((i * 5) % 11) as f64 - 5.0) * 0.3))
        .collect();
    let len = (SLOT_KEEP_S * BASEBAND_HZ) as usize;
    let mut z = baseband_slot(&[], -26.0, 41);
    let mut noise = Noise(42);
    for (i, t) in txs.iter().enumerate() {
        // Each station's own level above the -26 dB noise reference.
        let gain = 10f64.powf((((i * 11) % 19) as f64) / 20.0) as f32;
        let one = baseband_slot(std::slice::from_ref(t), 200.0, noise.0 ^ i as u64);
        for (v, s) in z.iter_mut().zip(one).take(len) {
            *v += s * gain;
        }
        noise.uniform();
    }
    let started = std::time::Instant::now();
    let d = decode_baseband(&z, &mut CallHashes::default());
    eprintln!("busy band: {} of {} in {:?}", d.len(), txs.len(), started.elapsed());
    let got = txs.iter().filter(|t| d.iter().any(|d| d.text == t.text)).count();
    for t in &txs {
        if !d.iter().any(|d| d.text == t.text) {
            eprintln!("  missed {} at {:.1}", t.text, t.freq);
        }
    }
    assert!(d.iter().all(|d| txs.iter().any(|t| t.text == d.text)), "nothing invented");
    assert!(got >= 27, "{got} of {}", txs.len());
}

fn sweep_slot(slot: u64) -> Vec<Tx> {
    messages(8)
        .iter()
        .enumerate()
        .map(|(i, m)| tx(m, 1405.0 + 24.0 * i as f64 + slot as f64 * 0.29, (i as f64 - 3.5) * 0.9, (i as f64 - 3.5) * 0.4))
        .collect()
}

/// Decode rate against SNR, eight signals a slot; run with --ignored
/// --nocapture to print the table. WSPR_SNRS, WSPR_SLOTS and WSPR_SEED
/// pick the levels, slots per level, and the noise.
#[test]
#[ignore]
fn sensitivity_sweep() {
    let env = |k: &str| std::env::var(k).ok();
    let snrs: Vec<f64> = env("WSPR_SNRS").map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or(vec![-27.0, -28.0, -29.0, -30.0, -31.0]);
    let slots: u64 = env("WSPR_SLOTS").and_then(|v| v.parse().ok()).unwrap_or(6);
    let seed: u64 = env("WSPR_SEED").and_then(|v| v.parse().ok()).unwrap_or(100);
    for snr in snrs {
        let (mut ok, mut all, mut extra) = (0, 0, 0);
        let mut took = std::time::Duration::ZERO;
        for slot in 0..slots {
            let txs = sweep_slot(slot);
            let z = baseband_slot(&txs, snr, seed + slot);
            let t = std::time::Instant::now();
            let d = decode_baseband(&z, &mut CallHashes::default());
            took += t.elapsed();
            all += txs.len();
            ok += txs.iter().filter(|t| d.iter().any(|d| d.text == t.text)).count();
            extra += d.iter().filter(|d| !txs.iter().any(|t| t.text == d.text)).count();
        }
        eprintln!("{snr:+} dB: {ok}/{all} decoded ({:.0}%), {extra} false, {:?} a slot", 100.0 * ok as f64 / all as f64, took / slots as u32);
    }
}
