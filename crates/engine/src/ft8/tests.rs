use super::*;

/// White Gaussian noise of unit variance, reproducible.
struct Noise(u64);

impl Noise {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f32 {
        let (u, v) = (self.uniform(), self.uniform());
        ((-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()) as f32
    }
}

/// Amplitude of a unit sine that sits `snr_db` above unit-variance noise
/// at `rate`, measured in 2500 Hz as WSJT-X does.
fn amplitude(snr_db: f32, rate: f32) -> f32 {
    let noise_2500 = 2500.0 / (rate / 2.0);
    (2.0 * noise_2500 * 10f32.powf(snr_db / 10.0)).sqrt()
}

struct Signal {
    text: &'static str,
    freq: f32,
    dt: f32,
}

fn slot_with(protocol: Protocol, signals: &[Signal], snr_db: f32, seed: u64) -> Vec<f32> {
    let len = protocol.slot_secs() as f32;
    let mut noise = Noise(seed);
    let mut audio: Vec<f32> = (0..(len * RATE_HZ) as usize).map(|_| noise.gauss()).collect();
    let a = amplitude(snr_db, RATE_HZ);
    for s in signals {
        let wave = synthesize(protocol, s.text, s.freq, protocol.nominal_start_s() + s.dt, len).expect("packs");
        for (x, w) in audio.iter_mut().zip(wave) {
            *x += a * w;
        }
    }
    audio
}

fn check_all_decode(protocol: Protocol, signals: &[Signal], snr_db: f32, seed: u64) -> Vec<Decode> {
    let audio = slot_with(protocol, signals, snr_db, seed);
    let t = std::time::Instant::now();
    let decodes = decode_slot(protocol, &audio, &mut CallHashes::default());
    eprintln!("{} slot at {snr_db} dB: {} decodes in {:?}", protocol.label(), decodes.len(), t.elapsed());
    for d in &decodes {
        eprintln!("  {:+3.0} {:+.2} {:6.1} {}", d.snr_db, d.dt_s, d.freq_hz, d.text);
    }
    for s in signals {
        let d = decodes.iter().find(|d| d.text == s.text).unwrap_or_else(|| panic!("{} not decoded", s.text));
        assert!((d.freq_hz - s.freq).abs() <= 3.0, "{}: freq {} vs {}", s.text, d.freq_hz, s.freq);
        assert!((d.dt_s - s.dt).abs() <= 0.1, "{}: dt {} vs {}", s.text, d.dt_s, s.dt);
        assert!((d.snr_db - snr_db).abs() <= 2.0, "{}: snr {} vs {}", s.text, d.snr_db, snr_db);
    }
    assert_eq!(decodes.len(), signals.len(), "nothing invented");
    decodes
}

#[test]
fn several_weak_ft8_signals_all_decode() {
    let signals = [
        Signal { text: "CQ K1ABC FN42", freq: 612.5, dt: -0.3 },
        Signal { text: "K1ABC W9XYZ -11", freq: 1103.0, dt: 0.0 },
        Signal { text: "W9XYZ K1ABC R-09", freq: 1500.0, dt: 0.4 },
        Signal { text: "K1ABC W9XYZ RR73", freq: 2101.6, dt: 0.9 },
        Signal { text: "TNX BOB 73 GL", freq: 2703.1, dt: 1.5 },
    ];
    check_all_decode(Protocol::Ft8, &signals, -15.0, 0x1234_5678_9abc_def1);
}

#[test]
fn a_slot_far_below_full_scale_decodes_the_same() {
    // Sideband audio taken before the AGC: the same weak band, 100 dB down.
    let signals = [Signal { text: "CQ K1ABC FN42", freq: 612.5, dt: 0.0 }, Signal { text: "K1ABC W9XYZ RR73", freq: 2101.6, dt: 0.4 }];
    let audio: Vec<f32> = slot_with(Protocol::Ft8, &signals, -15.0, 0x5eed).iter().map(|x| x * 1e-5).collect();
    let decodes = decode_slot(Protocol::Ft8, &audio, &mut CallHashes::default());
    for s in &signals {
        let d = decodes.iter().find(|d| d.text == s.text).unwrap_or_else(|| panic!("{} not decoded", s.text));
        assert!((d.snr_db + 15.0).abs() <= 2.0, "{}: snr {}", s.text, d.snr_db);
    }
    assert!(decode_slot(Protocol::Ft8, &vec![0.0; 180_000], &mut CallHashes::default()).is_empty());
}

#[test]
fn several_ft4_signals_all_decode() {
    let signals = [
        Signal { text: "CQ K1ABC FN42", freq: 700.0, dt: -0.2 },
        Signal { text: "K1ABC W9XYZ 73", freq: 1210.0, dt: 0.1 },
        Signal { text: "W9XYZ K1ABC R FN42", freq: 1805.0, dt: 0.4 },
        Signal { text: "CQ DX W9XYZ EN37", freq: 2400.0, dt: 0.7 },
    ];
    check_all_decode(Protocol::Ft4, &signals, -12.0, 0x0fed_cba9_8765_4321);
}

#[test]
fn noise_alone_decodes_to_nothing() {
    for p in [Protocol::Ft8, Protocol::Ft4] {
        let audio = slot_with(p, &[], 0.0, 0x5eed);
        assert!(decode_slot(p, &audio, &mut CallHashes::default()).is_empty());
    }
}

#[test]
fn a_hashed_call_heard_in_full_in_the_same_slot_is_named() {
    let signals = [
        Signal { text: "CQ PJ4/K1ABC", freq: 800.0, dt: 0.0 },
        Signal { text: "<PJ4/K1ABC> W9XYZ -07", freq: 1600.0, dt: 0.2 },
    ];
    let audio = slot_with(Protocol::Ft8, &signals, -8.0, 77);
    let decodes = decode_slot(Protocol::Ft8, &audio, &mut CallHashes::default());
    let texts: Vec<&str> = decodes.iter().map(|d| d.text.as_str()).collect();
    assert!(texts.contains(&"CQ PJ4/K1ABC"), "{texts:?}");
    assert!(texts.contains(&"<PJ4/K1ABC> W9XYZ -07"), "{texts:?}");
}

#[test]
fn jittery_stamps_cost_nothing_at_the_top_of_the_passband() {
    // As the server delivers it: ~10 ms blocks, each stamped with when it was
    // processed, ±20 ms of jitter. Steering the clock on every stamp smeared
    // everything above ~500 Hz; the clock now holds still within a slot.
    let rate = 48_000.0f32;
    let p = Protocol::Ft8;
    let slot0: u64 = 1_700_000_010_000;
    let signals = [("CQ K1ABC FN42", 320.0), ("K1ABC W9XYZ -11", 1_510.0), ("W9XYZ K1ABC R-09", 2_240.0), ("CQ DX W9XYZ EN37", 2_810.0)];
    let lead = 2.0f32;
    let total = ((lead + 16.0) * rate) as usize;
    let a = amplitude(-15.0, rate);
    let mut noise = Noise(7);
    let mut audio: Vec<f32> = (0..total).map(|_| noise.gauss()).collect();
    for (text, f) in signals {
        let payload = pack::pack(text, &mut CallHashes::default()).unwrap();
        let wave = demod::gfsk(p, &demod::tones(p, &payload), f, rate);
        let at = ((lead + 0.5) * rate) as usize;
        for (x, w) in audio[at..].iter_mut().zip(wave) {
            *x += a * w;
        }
    }
    let mut dec = SlotDecoder::new(p, rate as f64);
    let start_ms = slot0 as f64 - lead as f64 * 1000.0;
    let mut jitter = Noise(11);
    for (k, block) in audio.chunks(512).enumerate() {
        let end = k * 512 + block.len() - 1;
        let stamp = start_ms + end as f64 / rate as f64 * 1000.0 + (jitter.uniform() - 0.5) * 40.0;
        dec.push(block, stamp as u64);
    }
    let mut got = Vec::new();
    for _ in 0..200 {
        got.extend(dec.take());
        if !got.is_empty() && !dec.busy() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let decodes = &got.first().expect("a slot").1;
    for (text, _) in signals {
        assert!(decodes.iter().any(|d| d.text == text), "{text} not decoded: {:?}", decodes.iter().map(|d| &d.text).collect::<Vec<_>>());
    }
}

/// Stations above 3 kHz: a wide filter's search finds them through the
/// streaming front end, and the default search leaves them alone.
#[test]
fn a_wide_band_search_finds_the_stations_above_3_khz() {
    let rate = 48_000.0f32;
    let p = Protocol::Ft8;
    let slot0: u64 = 1_700_000_010_000;
    let signals = [("CQ K1ABC FN42", 1_200.0), ("K1ABC W9XYZ -11", 3_420.0), ("CQ DX W9XYZ EN37", 4_330.0)];
    let lead = 2.0f32;
    let total = ((lead + 16.0) * rate) as usize;
    let a = amplitude(-14.0, rate);
    let mut noise = Noise(21);
    let mut audio: Vec<f32> = (0..total).map(|_| noise.gauss()).collect();
    for (text, f) in signals {
        let payload = pack::pack(text, &mut CallHashes::default()).unwrap();
        let wave = demod::gfsk(p, &demod::tones(p, &payload), f, rate);
        let at = ((lead + 0.5) * rate) as usize;
        for (x, w) in audio[at..].iter_mut().zip(wave) {
            *x += a * w;
        }
    }
    let run = |band: Option<(f32, f32)>| {
        let mut dec = SlotDecoder::new(p, rate as f64);
        if let Some((lo, hi)) = band {
            dec.set_band(lo, hi);
        }
        let start_ms = slot0 as f64 - lead as f64 * 1000.0;
        for (k, block) in audio.chunks(480).enumerate() {
            let end = k * 480 + block.len() - 1;
            dec.push(block, (start_ms + end as f64 / rate as f64 * 1000.0) as u64);
        }
        let mut got = Vec::new();
        for _ in 0..200 {
            got.extend(dec.take());
            if !got.is_empty() && !dec.busy() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        got.into_iter().flat_map(|(_, d)| d).collect::<Vec<_>>()
    };
    let wide = run(Some((100.0, 4_500.0)));
    eprintln!("wide: {:?}", wide.iter().map(|d| (d.text.clone(), d.freq_hz, d.snr_db)).collect::<Vec<_>>());
    for (text, f) in signals {
        let d = wide.iter().find(|d| d.text == text).unwrap_or_else(|| panic!("{text} not decoded in 100–4500 Hz"));
        assert!((d.freq_hz - f).abs() <= 3.0, "{text}: {} Hz", d.freq_hz);
        // The resampler holds the top of the band flat: SNR as sent.
        assert!((d.snr_db + 14.0).abs() <= 2.0, "{text}: snr {}", d.snr_db);
    }
    let narrow = run(None);
    assert!(narrow.iter().any(|d| d.text == "CQ K1ABC FN42"));
    assert!(narrow.iter().all(|d| d.freq_hz < 3_100.0), "{:?}", narrow.iter().map(|d| d.freq_hz).collect::<Vec<_>>());
}

#[test]
fn the_slot_decoder_cuts_utc_slots_from_a_48k_stream() {
    let rate = 48_000.0f32;
    let p = Protocol::Ft8;
    // A slot boundary: a multiple of 15 s.
    let slot0: u64 = 1_700_000_010_000;
    assert_eq!(slot0 % 15_000, 0);
    let payload = pack::pack("CQ W9XYZ EN37", &mut CallHashes::default()).unwrap();
    let wave = demod::gfsk(p, &demod::tones(p, &payload), 1234.0, rate);
    // Start 3 s before the slot and run 1.5 s past its end.
    let lead = 3.0f32;
    let total = ((lead + 16.5) * rate) as usize;
    let dt = 0.25f32;
    let at = ((lead + 0.5 + dt) * rate) as usize;
    let a = amplitude(-10.0, rate);
    let mut noise = Noise(99);
    let audio: Vec<f32> = (0..total).map(|i| noise.gauss() + if i >= at && i - at < wave.len() { a * wave[i - at] } else { 0.0 }).collect();
    let mut dec = SlotDecoder::new(p, rate as f64);
    let start_ms = slot0 as f64 - lead as f64 * 1000.0;
    let mut jitter = Noise(5);
    for (k, block) in audio.chunks(2048).enumerate() {
        let end = k * 2048 + block.len() - 1;
        let stamp = start_ms + end as f64 / rate as f64 * 1000.0 + (jitter.uniform() - 0.5) * 10.0;
        dec.push(block, stamp as u64);
    }
    let mut got = Vec::new();
    for _ in 0..200 {
        got.extend(dec.take());
        if !got.is_empty() && !dec.busy() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert_eq!(got.len(), 1, "one whole slot; the partial one before it is skipped");
    let (start, decodes) = &got[0];
    assert_eq!(*start, slot0);
    let d = decodes.iter().find(|d| d.text == "CQ W9XYZ EN37").expect("decoded");
    assert!((d.freq_hz - 1234.0).abs() <= 3.0, "freq {}", d.freq_hz);
    assert!((d.dt_s - dt).abs() <= 0.1, "dt {}", d.dt_s);
}

#[test]
fn a_fractional_input_rate_keeps_time() {
    // The chain's rate is rarely a round number.
    let rate = 47_627.91f64;
    let p = Protocol::Ft4;
    let slot0: u64 = 1_700_000_002_500;
    assert_eq!(slot0 % 7_500, 0);
    let payload = pack::pack("K1ABC W9XYZ RR73", &mut CallHashes::default()).unwrap();
    let wave = demod::gfsk(p, &demod::tones(p, &payload), 1500.0, rate as f32);
    let total = (8.5 * rate) as usize;
    let at = (0.5 * rate) as usize;
    let a = amplitude(-6.0, rate as f32);
    let mut noise = Noise(3);
    let audio: Vec<f32> = (0..total).map(|i| noise.gauss() + if i >= at && i - at < wave.len() { a * wave[i - at] } else { 0.0 }).collect();
    let mut dec = SlotDecoder::new(p, rate);
    for (k, block) in audio.chunks(1000).enumerate() {
        let end = k * 1000 + block.len() - 1;
        dec.push(block, slot0 + (end as f64 / rate * 1000.0) as u64);
    }
    let mut got = Vec::new();
    for _ in 0..200 {
        got.extend(dec.take());
        if !got.is_empty() && !dec.busy() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let d = got.iter().flat_map(|(_, d)| d).find(|d| d.text == "K1ABC W9XYZ RR73").expect("decoded");
    assert!(d.dt_s.abs() <= 0.1, "dt {}", d.dt_s);
    assert!((d.freq_hz - 1500.0).abs() <= 3.0, "freq {}", d.freq_hz);
}

/// How weak a signal still decodes, and what a busy band costs:
/// `cargo test --release -p scannerd-engine ft8::tests::sensitivity -- --ignored --nocapture`
#[test]
#[ignore]
fn sensitivity() {
    for p in [Protocol::Ft8, Protocol::Ft4] {
        for snr in [-24.0f32, -22.0, -20.0, -18.0, -16.0, -14.0, -12.0] {
            let mut ok = 0;
            let mut est = Vec::new();
            for seed in 0..20u64 {
                let s = [Signal { text: "K1ABC W9XYZ -11", freq: 1000.0 + seed as f32 * 37.0, dt: (seed % 5) as f32 * 0.1 - 0.2 }];
                let audio = slot_with(p, &s, snr, seed * 7919 + 1);
                let d = decode_slot(p, &audio, &mut CallHashes::default());
                if let Some(d) = d.iter().find(|d| d.text == s[0].text) {
                    ok += 1;
                    est.push(d.snr_db);
                }
            }
            let mean = est.iter().sum::<f32>() / est.len().max(1) as f32;
            eprintln!("{} {snr:+} dB: {ok}/20 decoded, reported {mean:+.1}", p.label());
        }
    }
    // A busy FT8 band: 30 signals.
    const CALLS: [&str; 6] = ["K1ABC", "W9XYZ", "N0CALL", "G4ABC", "JA1XYZ", "VK2DEF"];
    let texts: Vec<String> = (0..30).map(|i| format!("{} {} -{:02}", CALLS[i % 6], CALLS[(i + 1 + i / 6) % 6], 5 + i)).collect();
    let leaked: Vec<&'static str> = texts.into_iter().map(|t| &*Box::leak(t.into_boxed_str())).collect();
    let signals: Vec<Signal> = leaked.iter().enumerate().map(|(i, t)| Signal { text: t, freq: 300.0 + i as f32 * 88.0, dt: (i % 7) as f32 * 0.15 - 0.3 }).collect();
    let audio = slot_with(Protocol::Ft8, &signals, -12.0, 42);
    let t = std::time::Instant::now();
    let d = decode_slot(Protocol::Ft8, &audio, &mut CallHashes::default());
    eprintln!("busy band: {} of {} decoded in {:?}", d.len(), signals.len(), t.elapsed());
}

/// Broadband static crashes: `n` bursts of `ms` milliseconds at `gain`
/// times the noise's own amplitude, spread over the slot.
fn add_crashes(audio: &mut [f32], n: usize, ms: f32, gain: f32, seed: u64) {
    let mut r = Noise(seed ^ 0x9e37_79b9_7f4a_7c15);
    let len = (ms / 1000.0 * RATE_HZ) as usize;
    for _ in 0..n {
        let at = (r.uniform() * (audio.len() - len) as f64) as usize;
        for x in &mut audio[at..at + len] {
            *x += gain * r.gauss();
        }
    }
}

/// A fixed-seed sweep around the threshold, in quiet and under static
/// crashes, counting decodes, false decodes and decode time:
/// `cargo test --release -p scannerd-engine ft8::tests::threshold_sweep -- --ignored --nocapture`
#[test]
#[ignore]
fn threshold_sweep() {
    const TO: [&str; 6] = ["K1ABC", "W9XYZ", "N0AB", "G4ABC", "JA1XYZ", "VK2DEF"];
    const DE: [&str; 5] = ["KA9Q", "DL1ABC", "F5XYZ", "W1AW", "OH2BH"];
    let crashes = [(0usize, 0.0f32, 0.0f32), (4, 15.0, 30.0), (12, 8.0, 60.0)];
    let p = if std::env::var("SWEEP_FT4").is_ok() { Protocol::Ft4 } else { Protocol::Ft8 };
    // FT4's threshold sits some 4 dB above FT8's.
    let shift = if p == Protocol::Ft4 { 4.0 } else { 0.0 };
    let only: Option<usize> = std::env::var("SWEEP_CASE").ok().and_then(|v| v.parse().ok());
    let crashes: Vec<_> = crashes.into_iter().enumerate().filter(|(i, _)| only.is_none_or(|o| o == *i)).map(|(_, c)| c).collect();
    for (n, ms, gain) in crashes {
        let (mut total_ok, mut total_false, mut total_sent) = (0, 0, 0);
        let mut time = std::time::Duration::ZERO;
        for snr in [-24.0f32, -23.0, -22.0, -21.0, -20.0, -19.0, -18.0] {
            let snr = snr + shift;
            let (mut ok, mut bad, mut sent) = (0, 0, 0);
            for slot in 0..12u64 {
                let texts: Vec<String> = (0..5).map(|i| format!("{} {} -{:02}", TO[(i + slot as usize) % 6], DE[i], 5 + i + slot as usize)).collect();
                let leaked: Vec<&'static str> = texts.into_iter().map(|t| &*Box::leak(t.into_boxed_str())).collect();
                let signals: Vec<Signal> = leaked
                    .iter()
                    .enumerate()
                    .map(|(i, t)| Signal { text: t, freq: 400.0 + i as f32 * 480.0 + slot as f32 * 13.7, dt: ((i + slot as usize) % 7) as f32 * 0.15 - 0.3 })
                    .collect();
                let mut audio = slot_with(p, &signals, snr, slot * 7919 + 17);
                if n > 0 {
                    add_crashes(&mut audio, n, ms, gain, slot * 31 + 5);
                }
                let t = std::time::Instant::now();
                let d = decode_slot_budget(p, &audio, &mut CallHashes::default(), F_MIN_HZ, F_MAX_HZ, std::time::Duration::MAX);
                time += t.elapsed();
                sent += signals.len();
                for d in &d {
                    if signals.iter().any(|s| s.text == d.text) {
                        ok += 1
                    } else {
                        bad += 1;
                        eprintln!("  false: {} (sent {:?})", d.text, signals.iter().map(|s| s.text).collect::<Vec<_>>());
                    }
                }
            }
            eprintln!("{} crashes {n}x{ms}ms@{gain}: {snr:+} dB {ok}/{sent} decoded, {bad} false", p.label());
            total_ok += ok;
            total_false += bad;
            total_sent += sent;
        }
        eprintln!("{} crashes {n}x{ms}ms@{gain}: TOTAL {total_ok}/{total_sent}, {total_false} false, {:?} per slot", p.label(), time / 84);
    }
}

/// Slots of noise alone, half of them with crashes: anything that decodes
/// is false.
/// `cargo test --release -p scannerd-engine ft8::tests::false_decodes -- --ignored --nocapture`
#[test]
#[ignore]
fn false_decodes() {
    for p in [Protocol::Ft8, Protocol::Ft4] {
        let mut n = 0;
        let slots = 60u64;
        for seed in 0..slots {
            let mut audio = slot_with(p, &[], 0.0, 1000 + seed);
            if seed % 2 == 1 {
                add_crashes(&mut audio, 4, 15.0, 30.0, seed);
            }
            let d = decode_slot_budget(p, &audio, &mut CallHashes::default(), F_MIN_HZ, F_MAX_HZ, std::time::Duration::MAX);
            for d in &d {
                eprintln!("{} seed {seed}: {}", p.label(), d.text);
            }
            n += d.len();
        }
        eprintln!("{}: {n} false decodes in {slots} slots of noise", p.label());
    }
}
