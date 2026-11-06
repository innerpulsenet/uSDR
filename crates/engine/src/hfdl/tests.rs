use super::fec::{conv_encode, fcs, pack_lsb_first, unpack_lsb_first, viterbi};
use super::pdu::parse_burst;
use super::phy::{MODES, Tables, gray_to_index, index_to_gray, interleave_map};
use super::synth::*;
use super::*;

/// The live front end's rate.
const FS: f64 = 47_904.0;

#[test]
fn hfdl_tables_match_the_spec() {
    let t = Tables::new();
    assert_eq!(t.a.len(), 127);
    assert_eq!(t.scramble.len(), 120);
    // 131BC... starts 0001 0011 0001.
    assert_eq!(&t.scramble[..12], &[0, 0, 0, 1, 0, 0, 1, 1, 0, 0, 0, 1]);
    // T is 000100110101111.
    let tb: Vec<u8> = t.t.iter().map(|&s| (s < 0.0) as u8).collect();
    assert_eq!(tb, [0, 0, 0, 1, 0, 0, 1, 1, 0, 1, 0, 1, 1, 1, 1]);
    // Table 4-7: 8-PSK 010 is 135 degrees, 110 is 180, 100 is 315.
    assert_eq!(gray_to_index(0b010), 3);
    assert_eq!(gray_to_index(0b110), 4);
    assert_eq!(gray_to_index(0b100), 7);
    for k in 0..8 {
        assert_eq!(gray_to_index(index_to_gray(k)), k);
    }
}

#[test]
fn hfdl_interleaver_matches_the_spec_example() {
    // 1200 bps, 1.8 s: N = 108. The second chip sent comes from row 1,
    // column 91, the third from row 2, column 74.
    let mode = MODES[2];
    assert_eq!(mode.columns(), 108);
    let map = interleave_map(&mode);
    assert_eq!(map[0], 0);
    assert_eq!(map[1], 40 * 91 + 9);
    assert_eq!(map[2], 40 * 74 + 18);
    for mode in MODES {
        let map = interleave_map(&mode);
        let mut seen = vec![false; map.len()];
        for &i in &map {
            assert!(!seen[i as usize]);
            seen[i as usize] = true;
        }
    }
    assert_eq!(MODES[0].columns(), 54);
    assert_eq!(MODES[3].columns(), 162);
    assert_eq!(MODES[7].columns(), 378);
    assert_eq!(MODES[4].columns(), 126);
}

#[test]
fn hfdl_viterbi_corrects_errors() {
    let mut rng = Rng::new(7);
    let bits: Vec<u8> = (0..600).map(|_| (rng.next_u64() & 1) as u8).collect();
    let coded = conv_encode(&bits);
    let mut llr: Vec<f32> = coded
        .iter()
        .map(|&c| if c == 0 { 4.0 } else { -4.0 })
        .collect();
    for i in (5..llr.len()).step_by(23) {
        llr[i] = -llr[i];
    }
    assert_eq!(viterbi(&llr), bits);
    assert_eq!(unpack_lsb_first(&pack_lsb_first(&bits)), bits);
}

/// The decoder alone in Gaussian noise: textbook K=7 performance.
#[test]
fn hfdl_viterbi_ber() {
    let mut rng = Rng::new(5);
    for ebn0_db in [1.0f64, 2.0, 3.0, 4.0] {
        let esn0 = 10f64.powf((ebn0_db - 3.0) / 10.0);
        let sigma = (1.0 / (2.0 * esn0)).sqrt();
        let mut errs = 0;
        let mut tot = 0;
        for _ in 0..50 {
            let bits: Vec<u8> = (0..1000).map(|_| (rng.next_u64() & 1) as u8).collect();
            let llr: Vec<f32> = conv_encode(&bits)
                .iter()
                .map(|&c| {
                    let r = if c == 0 { 1.0 } else { -1.0 } + rng.gauss() * sigma;
                    (2.0 * r / (sigma * sigma)) as f32
                })
                .collect();
            let out = viterbi(&llr);
            errs += out.iter().zip(bits.iter()).filter(|(a, b)| a != b).count();
            tot += bits.len();
        }
        let ber = errs as f64 / tot as f64;
        eprintln!("Eb/N0 {ebn0_db} dB: BER {ber:.2e}");
        if ebn0_db >= 4.0 {
            assert!(ber < 1e-4, "BER {ber} at {ebn0_db} dB");
        }
        if ebn0_db >= 3.0 {
            assert!(ber < 2e-3, "BER {ber} at {ebn0_db} dB");
        }
    }
}

#[test]
fn hfdl_fcs_is_the_hdlc_crc() {
    // CRC-16/X-25 check value.
    assert_eq!(fcs(b"123456789"), 0x906e);
}

fn squitter_of(ev: &HfdlEvent) -> Option<&Squitter> {
    match &ev.pdu {
        HfdlPdu::Squitter(s) => Some(s),
        _ => None,
    }
}

#[test]
fn hfdl_parses_a_squitter() {
    let b = squitter(3, 150, 3, &[0, 2, 5]);
    let p = parse_burst(&b).expect("squitter");
    let HfdlPdu::Squitter(s) = &p.pdus[0] else {
        panic!()
    };
    assert_eq!(s.gs_id, 3);
    assert!(s.utc_sync);
    assert_eq!(s.frame_index, 150);
    assert_eq!(s.frame_offset, 3);
    // The spec's example: frame 150, slot 3 is 01:20:07.
    assert_eq!(s.utc_s.map(|u| u as u32), Some(4807));
    assert_eq!(s.stations[0].freq_ids, vec![0, 2, 5]);
    assert_eq!(s.stations[1].gs_id, 7);
    assert_eq!(s.stations[1].freq_ids, vec![1, 3]);
    assert_eq!(s.stations[2].gs_id, 9);
    assert!(!s.stations[2].utc_sync);
    assert_eq!(s.stations[2].freq_ids, vec![0]);
    assert_eq!(s.systable_version, 0x654);
    assert!(s.rls && s.iso8208);
    // A flipped bit fails the check.
    let mut bad = b.clone();
    bad[10] ^= 4;
    assert!(parse_burst(&bad).is_none());
}

fn acars_lpdu(text: &str, msg_no: &str) -> Vec<u8> {
    let mut l = vec![0x0d, 0xff, 0xff];
    l.extend(acars_block("N123AB", "H1", msg_no, "UA0123", text));
    l
}

#[test]
fn hfdl_parses_acars_logons_and_positions() {
    let mut logon = vec![0x8f];
    logon.extend(icao_bytes(0xA1B2C3));
    logon.extend(performance_data(
        "UA0123",
        41.25,
        -72.5,
        3600 * 13 + 62,
        3,
        4,
    ));
    let b = downlink_mpdu(3, 0xff, &[logon, acars_lpdu("POS N41150W072300", "M01A")]);
    let p = parse_burst(&b).expect("mpdu");
    assert_eq!(p.lpdu_failed, 0);
    let kinds: Vec<&str> = p
        .pdus
        .iter()
        .map(|x| match x {
            HfdlPdu::Logon(_) => "logon",
            HfdlPdu::Position(_) => "position",
            HfdlPdu::Acars(_) => "acars",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["logon", "position", "acars"]);
    let HfdlPdu::Logon(l) = &p.pdus[0] else {
        panic!()
    };
    assert_eq!(l.icao, 0xA1B2C3);
    assert_eq!(l.kind, LogonKind::Request);
    assert_eq!(l.link.aircraft_id, 0xff);
    let HfdlPdu::Position(pos) = &p.pdus[1] else {
        panic!()
    };
    assert!((pos.lat - 41.25).abs() < 1e-3 && (pos.lon + 72.5).abs() < 1e-3);
    assert_eq!(pos.flight, "UA0123");
    assert_eq!(pos.utc_s, 3600 * 13 + 62);
    assert_eq!(pos.icao, Some(0xA1B2C3));
    let HfdlPdu::Acars(a) = &p.pdus[2] else {
        panic!()
    };
    assert_eq!(a.reg, ".N123AB");
    assert_eq!(a.label, "H1");
    assert_eq!(a.msg_no.as_deref(), Some("M01A"));
    assert_eq!(a.flight.as_deref(), Some("UA0123"));
    assert_eq!(a.text, "POS N41150W072300");
    assert!(a.crc_ok && a.final_block);
    assert_eq!(a.ack, '!');

    // An uplink logon confirm.
    let mut conf = vec![0x9f];
    conf.extend(icao_bytes(0xA1B2C3));
    conf.extend([23, 0, 0, 0]);
    let b = uplink_mpdu(3, 0xff, &[conf]);
    let p = parse_burst(&b).expect("uplink");
    let HfdlPdu::Logon(l) = &p.pdus[0] else {
        panic!()
    };
    assert_eq!(l.kind, LogonKind::Confirm);
    assert_eq!(l.assigned_id, Some(23));
    assert_eq!(l.link.direction, Direction::Uplink);

    // A damaged LPDU is counted, not reported; the good one still is.
    let mut b = downlink_mpdu(
        3,
        12,
        &[acars_lpdu("ONE", "M02A"), acars_lpdu("TWO", "M03A")],
    );
    let n = b.len();
    b[n - 5] ^= 0x10;
    let p = parse_burst(&b).expect("header ok");
    assert_eq!(p.lpdu_failed, 1);
    assert_eq!(p.pdus.len(), 1);
}

/// Slot length, seconds.
const SLOT_S: f64 = 32.0 / 13.0;

/// Sends `n` bursts of `mode`, alternating a squitter and an ACARS MPDU
/// unless `only` says which, through `ch`, and counts the ones decoded
/// exactly. Returns (sent, decoded, decoder).
fn trial(
    mode: usize,
    n: usize,
    ch: Channel,
    seed: u64,
    only: Option<bool>,
) -> (usize, usize, HfdlDecoder) {
    let slot = if MODES[mode].long {
        2.0 * SLOT_S
    } else {
        SLOT_S
    };
    let secs = 0.4 + n as f64 * slot + 1.0;
    let mut air = Air::new(ch.clone(), secs, seed);
    let mut want = Vec::new();
    for i in 0..n {
        let sq = only.unwrap_or(i % 2 == 0);
        let bytes = if sq {
            squitter(5, 100 + i as u16, 0, &[1, 4])
        } else {
            downlink_mpdu(
                5,
                40,
                &[acars_lpdu(&format!("TEST MESSAGE {i:03}"), "M01A")],
            )
        };
        want.push((sq, i));
        air.add_burst(0.4 + i as f64 * slot, &burst_symbols(mode, &bytes));
    }
    air.add_noise();
    let mut dec = HfdlDecoder::new(ch.fs as f32);
    let mut events = Vec::new();
    for chunk in air.audio.chunks(4800) {
        dec.push(chunk);
        events.extend(dec.take());
    }
    let mut got = 0;
    for (sq, i) in want {
        let hit = events.iter().any(|e| {
            if e.burst.rate_bps != MODES[mode].rate_bps
                || e.burst.long_interleaver != MODES[mode].long
            {
                return false;
            }
            if sq {
                squitter_of(e).is_some_and(|s| {
                    s.frame_index == 100 + i as u16 && s.stations[0].freq_ids == [1, 4]
                })
            } else {
                matches!(&e.pdu, HfdlPdu::Acars(a) if a.text == format!("TEST MESSAGE {i:03}"))
            }
        });
        got += hit as usize;
    }
    (n, got, dec)
}

#[test]
fn hfdl_decodes_every_mode_clean() {
    for mode in 0..8 {
        let (n, got, dec) = trial(mode, 2, Channel::clean(FS), 1 + mode as u64, None);
        let st = dec.status();
        assert_eq!(got, n, "mode {mode}: {st:?}");
        assert_eq!(st.crc_failed, 0);
        assert!(st.snr_db.unwrap() > 25.0, "{st:?}");
    }
}

#[test]
fn hfdl_decodes_with_offsets_and_clock_error() {
    for (mode, df, ppm, fs) in [
        (0, 50.0, 100.0, FS),
        (1, -50.0, -100.0, FS),
        (2, 37.0, 100.0, 48_000.0),
        (3, -50.0, 100.0, FS),
        (3, 50.0, -100.0, 44_100.0),
        (7, 50.0, 100.0, FS),
        (6, -45.0, -100.0, FS),
        (1, 20.0, 50.0, 12_000.0),
        (5, -20.0, -50.0, 8_000.0),
    ] {
        let ch = Channel {
            freq_offset_hz: df,
            clock_ppm: ppm,
            snr_db: Some(25.0),
            ..Channel::clean(fs)
        };
        let (n, got, dec) = trial(mode, 2, ch, 11, None);
        let st = dec.status();
        assert_eq!(got, n, "mode {mode} {df} Hz {ppm} ppm: {st:?}");
        let fe = st.freq_error_hz.unwrap();
        assert!((fe - df as f32).abs() < 1.0, "freq error {fe} vs {df}");
    }
}

#[test]
fn hfdl_level_does_not_matter() {
    for level in [1e-4, 1.0] {
        let ch = Channel {
            level,
            snr_db: Some(20.0),
            ..Channel::clean(FS)
        };
        let (n, got, _) = trial(2, 2, ch, 3, None);
        assert_eq!(got, n, "level {level}");
    }
}

#[test]
fn hfdl_noise_alone_decodes_nothing() {
    let mut rng = Rng::new(99);
    let mut dec = HfdlDecoder::new(FS as f32);
    let audio: Vec<f32> = (0..(FS * 30.0) as usize)
        .map(|_| (rng.gauss() * 0.1) as f32)
        .collect();
    for c in audio.chunks(4800) {
        dec.push(c);
    }
    assert!(dec.take().is_empty());
    assert_eq!(dec.status().decoded, 0);
}

fn fading_channel(snr: f64, df: f64, ppm: f64, delay_ms: f64, spread: f64) -> Channel {
    Channel {
        freq_offset_hz: df,
        clock_ppm: ppm,
        paths: vec![
            Path {
                delay_ms: 0.0,
                gain: 1.0,
            },
            Path {
                delay_ms,
                gain: 1.0,
            },
        ],
        spread_hz: spread,
        snr_db: Some(snr),
        ..Channel::clean(FS)
    }
}

/// Share of `n` bursts decoded at each SNR (in 3 kHz) through two equal
/// paths `delay_ms` apart, each fading with `spread` Hz Watterson spread,
/// the carrier 50 Hz and the sample clock 100 ppm off (the signs alternate
/// from one SNR to the next). Fixed seeds, so the result is repeatable.
fn sweep(mode: usize, snrs: &[f64], n: usize, delay_ms: f64, spread: f64) -> Vec<(f64, f64)> {
    snrs.iter()
        .enumerate()
        .map(|(k, &snr)| {
            let sign = if k % 2 == 0 { 1.0 } else { -1.0 };
            let ch = fading_channel(snr, 50.0 * sign, 100.0 * sign, delay_ms, spread);
            let (sent, got, _) = trial(mode, n, ch, 1000 + k as u64 * 17 + mode as u64, None);
            (snr, got as f64 / sent as f64)
        })
        .collect()
}

fn report(name: &str, r: &[(f64, f64)]) {
    let line: Vec<String> = r
        .iter()
        .map(|(s, p)| format!("{s:+.0} dB {:.0}%", p * 100.0))
        .collect();
    eprintln!("{name}: {}", line.join(", "));
}

/// The first SNR from which every point decodes at least 90%.
fn threshold(r: &[(f64, f64)]) -> Option<f64> {
    (0..r.len())
        .find(|&i| r[i..].iter().all(|p| p.1 >= 0.9))
        .map(|i| r[i].0)
}

fn fading_case(mode: usize, snrs: &[f64], want: f64) {
    let m = MODES[mode];
    let r = sweep(mode, snrs, 20, 2.0, 1.0);
    let name = format!(
        "{} bps {} slot, 2 ms 1 Hz",
        m.rate_bps,
        if m.long { "double" } else { "single" }
    );
    report(&name, &r);
    let t = threshold(&r);
    assert!(
        t.is_some_and(|t| t <= want),
        "{name}: 90% from {t:?}, wanted {want} dB"
    );
}

#[test]
fn hfdl_fading_300() {
    fading_case(0, &[-3.0, 0.0, 3.0, 6.0, 9.0], 6.0);
}

#[test]
fn hfdl_fading_600() {
    fading_case(1, &[0.0, 3.0, 6.0, 9.0, 12.0], 9.0);
}

#[test]
fn hfdl_fading_1200() {
    fading_case(2, &[3.0, 6.0, 9.0, 12.0, 15.0], 12.0);
}

#[test]
fn hfdl_fading_1800() {
    fading_case(3, &[12.0, 15.0, 18.0, 21.0, 24.0], 24.0);
}

#[test]
fn hfdl_fading_double_slot() {
    for (mode, snr) in [(4, 6.0), (5, 6.0), (6, 9.0), (7, 18.0)] {
        let r = sweep(mode, &[snr], 10, 2.0, 1.0);
        report(
            &format!("{} bps double slot, 2 ms 1 Hz", MODES[mode].rate_bps),
            &r,
        );
        assert!(r[0].1 >= 0.9, "mode {mode} at {snr} dB: {r:?}");
    }
}

#[test]
fn hfdl_fading_moderate() {
    // CCIR moderate: 1 ms, 0.5 Hz.
    for (mode, snr) in [(0, 3.0), (1, 6.0), (2, 9.0), (3, 18.0)] {
        let r = sweep(mode, &[snr], 10, 1.0, 0.5);
        report(
            &format!("{} bps single slot, 1 ms 0.5 Hz", MODES[mode].rate_bps),
            &r,
        );
        assert!(r[0].1 >= 0.9, "mode {mode} at {snr} dB: {r:?}");
    }
}

#[test]
fn hfdl_noise_thresholds() {
    // White noise only, carrier and clock off.
    for (mode, snr) in [(0, -2.0), (1, 1.0), (2, 4.0), (3, 9.0)] {
        let ch = Channel {
            snr_db: Some(snr),
            freq_offset_hz: -50.0,
            clock_ppm: 100.0,
            ..Channel::clean(FS)
        };
        let (sent, got, dec) = trial(mode, 10, ch, 77 + mode as u64, None);
        eprintln!(
            "{} bps, noise only, {snr:+} dB: {got}/{sent}, estimated {:.1} dB",
            MODES[mode].rate_bps,
            dec.status().snr_db.unwrap_or(f32::NAN)
        );
        assert!(
            got * 10 >= sent * 9,
            "mode {mode} at {snr} dB: {got}/{sent}"
        );
        let est = dec.status().snr_db.unwrap();
        assert!(
            (est - snr as f32).abs() < 1.5,
            "SNR estimate {est} vs {snr}"
        );
    }
}

/// Real-time factor: a frame and a half of a busy frequency (a burst in
/// every slot, all four rates, fading, 12 dB), and a minute of noise.
#[test]
fn hfdl_real_time_factor() {
    let ch = fading_channel(12.0, 30.0, 60.0, 2.0, 1.0);
    let secs = 48.0;
    let mut air = Air::new(ch.clone(), secs + 1.0, 5);
    let mut t = 0.3;
    let mut i = 0;
    while t + 2.0 * SLOT_S < secs {
        let mode = if i % 7 == 6 { 5 } else { i % 4 };
        let bytes = if i % 2 == 0 {
            squitter(5, 100 + i as u16, 0, &[1, 4])
        } else {
            downlink_mpdu(5, 40, &[acars_lpdu(&format!("TEST {i}"), "M01A")])
        };
        air.add_burst(t, &burst_symbols(mode, &bytes));
        t += if MODES[mode].long {
            2.0 * SLOT_S
        } else {
            SLOT_S
        };
        i += 1;
    }
    air.add_noise();
    let mut dec = HfdlDecoder::new(FS as f32);
    let start = std::time::Instant::now();
    for c in air.audio.chunks(4790) {
        dec.push(c);
    }
    let busy = start.elapsed().as_secs_f64() / (air.audio.len() as f64 / FS);
    let st = dec.status();
    let mut rng = Rng::new(3);
    let quiet: Vec<f32> = (0..(FS * 60.0) as usize)
        .map(|_| (rng.gauss() * 0.05) as f32)
        .collect();
    let mut dec2 = HfdlDecoder::new(FS as f32);
    let start = std::time::Instant::now();
    for c in quiet.chunks(4790) {
        dec2.push(c);
    }
    let idle = start.elapsed().as_secs_f64() / 60.0;
    eprintln!(
        "real-time factor: busy {:.4} ({} of {i} bursts decoded), idle {:.4}",
        busy, st.decoded, idle
    );
    if !cfg!(debug_assertions) {
        assert!(busy < 0.1 && idle < 0.1);
    }
    assert!(st.decoded as usize * 10 >= i * 8, "{st:?}");
}

#[test]
fn hfdl_event_api() {
    let burst = BurstInfo {
        rate_bps: 300,
        long_interleaver: false,
        freq_error_hz: 12.3,
        center_hz: 1452.3,
        snr_db: 14.0,
    };
    let p = parse_burst(&squitter(3, 150, 3, &[0, 2, 5])).unwrap();
    let ev = HfdlEvent {
        burst,
        pdu: p.pdus[0].clone(),
    };
    assert_eq!(ev.kind(), "squitter");
    assert_eq!(
        ev.summary(),
        "Squitter · gs 3 · frame 150 slot 3 · 01:20:07 UTC · freqs 0,2,5 · 300 bps"
    );
    let f = ev.fields();
    let get = |k: &str| f.iter().find(|(key, _)| *key == k).map(|(_, v)| v.clone());
    assert_eq!(get("gsId").as_deref(), Some("3"));
    assert_eq!(get("freqError").as_deref(), Some("+12.3"));
    assert_eq!(get("slot").as_deref(), Some("single"));
    assert!(get("slotsRawHex").is_some());

    let b = downlink_mpdu(3, 40, &[acars_lpdu("HELLO", "M07A")]);
    let p = parse_burst(&b).unwrap();
    let ev = HfdlEvent {
        burst,
        pdu: p.pdus[0].clone(),
    };
    assert_eq!(ev.kind(), "acars");
    assert_eq!(
        ev.summary(),
        "ACARS · ac 40 → gs 3 · N123AB · UA0123 · label H1 · HELLO"
    );
    let f = ev.fields();
    for key in [
        "aircraftId",
        "reg",
        "flight",
        "label",
        "text",
        "msgNo",
        "rate",
        "snr",
    ] {
        assert!(f.iter().any(|(k, _)| *k == key), "missing {key}");
    }
}
