//! End-to-end tests: synthetic transmitters from the standards, through
//! the HF channel of [`super::sim`], into the decoder as audio at the
//! receiver's real, awkward sample rate.
//!
//! "Hard" below is the CCIR poor channel (two equal Rayleigh paths 2 ms
//! apart, 1 Hz Doppler spread) with the carrier 50 Hz off and the sample
//! clock 100 ppm fast, SNR in 3 kHz. The SNRs the tests ask for come from
//! [`sweep_4285`] and [`sweep_110a`] (run with `--ignored`); for 188-110A
//! they are the minimums of its table XX.

use super::sim::{Channel, Path, Rng, modulate, noise, through};
use super::*;
use std::time::Instant;

/// The live rate: not a nice number.
const FS: f32 = 47_904.0;

fn run(audio: &[f32]) -> (Vec<StanagEvent>, StanagDecoder) {
    let mut d = StanagDecoder::new(FS);
    let mut ev = Vec::new();
    for c in audio.chunks(1000) {
        d.push(c);
        ev.extend(d.take());
    }
    (ev, d)
}

fn hard(snr: f64, seed: u64) -> Channel {
    Channel { offset_hz: 50.0, ppm: 100.0, seed, ..Channel::poor(f64::from(FS), snr) }
}

/// A run of NON-ARQ D_PDUs with plain text payloads, numbered, between
/// stretches of random fill (`lead` bytes first), as a busy link sends.
fn traffic(n: usize, lead: usize, seed: u64) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut rng = Rng(seed);
    let mut bytes: Vec<u8> = (0..lead).map(|_| rng.byte()).collect();
    let mut payloads = Vec::new();
    for i in 0..n {
        let text = format!("MSG {i:03} QTH AT SEA ALL WELL").into_bytes();
        bytes.extend(s5066::non_arq(0x0000_1234, 0x0000_5678, i as u16, &text));
        bytes.extend((0..6).map(|_| rng.byte()));
        payloads.push(text);
    }
    (bytes, payloads)
}

fn dpdus(ev: &[StanagEvent]) -> Vec<&Dpdu> {
    ev.iter()
        .filter_map(|e| match e {
            StanagEvent::Dpdu { dpdu, .. } => Some(dpdu),
            _ => None,
        })
        .collect()
}

/// How many of `want` arrived with their payload CRC good.
fn decoded(ev: &[StanagEvent], want: &[Vec<u8>]) -> usize {
    let got: Vec<&Vec<u8>> = dpdus(ev)
        .into_iter()
        .filter_map(|d| match &d.payload {
            Payload::Ok(p) => Some(p),
            _ => None,
        })
        .collect();
    want.iter().filter(|w| got.contains(w)).count()
}

fn modes(ev: &[StanagEvent]) -> Vec<(u32, &'static str)> {
    ev.iter()
        .filter_map(|e| match e {
            StanagEvent::Mode { data_rate_bps, interleave, .. } => Some((*data_rate_bps, *interleave)),
            _ => None,
        })
        .collect()
}

fn waveforms(ev: &[StanagEvent]) -> Vec<&'static str> {
    ev.iter()
        .filter_map(|e| match e {
            StanagEvent::Waveform { waveform, .. } => Some(*waveform),
            _ => None,
        })
        .collect()
}

struct Outcome {
    decoded: usize,
    events: Vec<StanagEvent>,
    /// Status taken late in the transmission, before the tail.
    status: StanagStatus,
}

/// STANAG 4285 at `mode` for `secs`, `n` D_PDUs after about three seconds
/// of fill (the mode is not known before), and random fill after.
fn s4285_case(bps: u32, order: usize, il: s4285::Interleave, secs: f64, ch: &Channel, n: usize) -> Outcome {
    let lead = (bps as usize * 3 / 8).max(16);
    let (mut bytes, want) = traffic(n, lead, u64::from(bps) + 7);
    let mut rng = Rng(99);
    bytes.extend((0..4000).map(|_| rng.byte()));
    let frames = (secs * 2400.0 / 256.0) as usize;
    let syms = s4285::transmit(s4285::mode(bps, order, il), &bytes, frames);
    let audio = modulate(&syms, 2400.0, ch);
    let cut = audio.len() * 8 / 10;
    let (mut events, mut d) = run(&audio[..cut]);
    let status = d.status();
    d.push(&audio[cut..]);
    events.extend(d.take());
    let out = Outcome { decoded: decoded(&events, &want), events, status };
    eprintln!("4285 {bps} {il:?}: {}/{n} decoded, modes {:?}, snr {:?}", out.decoded, modes(&out.events), out.status.snr_db);
    out
}

/// A 188-110A transmission: preamble, `n` D_PDUs, end of message.
fn m110_case(bps: u32, il: m110a::Interleave, ch: &Channel, n: usize) -> Outcome {
    let (mut bytes, want) = traffic(n, 8, u64::from(bps) + 11);
    let mut rng = Rng(98);
    bytes.extend((0..(bps as usize / 16).max(8)).map(|_| rng.byte()));
    let syms = m110a::transmit(m110a::test_mode(bps, il), &bytes);
    let audio = modulate(&syms, 2400.0, ch);
    let cut = audio.len() * 7 / 10;
    let (mut events, mut d) = run(&audio[..cut]);
    let status = d.status();
    d.push(&audio[cut..]);
    events.extend(d.take());
    let out = Outcome { decoded: decoded(&events, &want), events, status };
    eprintln!("110A {bps} {il:?}: {}/{n} decoded, modes {:?}, snr {:?}", out.decoded, modes(&out.events), out.status.snr_db);
    out
}

fn eom(ev: &[StanagEvent]) -> bool {
    ev.iter().any(|e| matches!(e, StanagEvent::EndOfMessage { .. }))
}

#[test]
fn s4285_coded_modes_through_the_hard_channel() {
    use s4285::Interleave::Short;
    // Rate, constellation, SNR, seconds, D_PDUs sent, D_PDUs required.
    let cases = [
        (2400, 8, 25.0, 10.0, 8, 6),
        (1200, 4, 14.0, 10.0, 8, 7),
        (600, 2, 7.0, 10.0, 4, 4),
        (300, 2, 5.0, 10.0, 2, 2),
        (150, 2, 3.0, 20.0, 2, 2),
        (75, 2, 4.0, 20.0, 2, 2),
    ];
    for (bps, order, snr, secs, n, need) in cases {
        let o = s4285_case(bps, order, Short, secs, &hard(snr, 10), n);
        assert!(waveforms(&o.events).contains(&NAME_4285), "{bps}");
        assert_eq!(modes(&o.events).first(), Some(&(bps, "short")), "{bps} bps");
        assert!(o.decoded >= need, "{bps} bps: {} of {n}", o.decoded);
        assert_eq!(o.status.modulation, Some(s4285::modulation(order)));
        assert!(o.status.freq_error_hz.is_some_and(|f| (f - 50.0).abs() < 3.0), "{:?}", o.status.freq_error_hz);
    }
}

#[test]
fn s4285_long_interleave() {
    // Ten seconds before the deinterleaver gives anything.
    let o = s4285_case(600, 2, s4285::Interleave::Long, 26.0, &hard(8.0, 7), 8);
    assert!(modes(&o.events).contains(&(600, "long")));
    assert!(o.decoded >= 7, "{}", o.decoded);
}

#[test]
fn s4285_uncoded_modes_are_named_by_their_dpdus() {
    let fs = f64::from(FS);
    for (bps, order) in [(1200, 2), (2400, 4), (3600, 8)] {
        let ch = Channel { offset_hz: 30.0, ..Channel::moderate(fs, 25.0) };
        let o = s4285_case(bps, order, s4285::Interleave::None, 10.0, &ch, 8);
        assert_eq!(modes(&o.events).last(), Some(&(bps, "none")), "{bps}");
        assert!(o.decoded >= 7, "{bps}: {}", o.decoded);
        assert!(o.events.iter().any(|e| matches!(e, StanagEvent::Mode { coded: false, .. })));
    }
}

#[test]
fn m110a_rates_at_the_standards_snr() {
    use m110a::Interleave::Long;
    // Table XX: 2400 bps at 18 dB, 1200 at 11, 600 at 7, on two fading
    // paths 2 ms apart with 1 Hz spread, long interleave. Here with the
    // carrier 50 Hz off and the clock 100 ppm fast besides.
    for (bps, snr) in [(2400, 18.0), (1200, 11.0), (600, 7.0)] {
        let o = m110_case(bps, Long, &hard(snr, 21), 6);
        assert_eq!(modes(&o.events), vec![(bps, "long (4.8 s)")], "{bps}");
        assert!(o.decoded >= 5, "{bps} bps: {} of 6", o.decoded);
        assert!(eom(&o.events), "{bps}: no end of message");
    }
}

#[test]
fn m110a_short_zero_and_uncoded() {
    use m110a::Interleave::*;
    let fs = f64::from(FS);
    for (bps, il, snr) in [(2400, Short, 22.0), (1200, Short, 14.0), (600, Short, 7.0), (300, Short, 8.0), (150, Short, 6.0)] {
        let o = m110_case(bps, il, &hard(snr, 22), 6);
        assert_eq!(modes(&o.events), vec![(bps, "short (0.6 s)")], "{bps}");
        assert!(o.decoded >= 5, "{bps} bps: {} of 6", o.decoded);
    }
    // The 0.0 s setting is not told apart from 0.6 s in the preamble; it
    // decodes all the same.
    let o = m110_case(600, Zero, &Channel { offset_hz: 50.0, ppm: 100.0, ..Channel::moderate(fs, 12.0) }, 6);
    assert!(o.decoded >= 5, "zero: {}", o.decoded);
    // 4800 bps: uncoded, on a steady channel (table XX asks 17 dB).
    let two = vec![Path { delay_s: 0.0, power: 0.7 }, Path { delay_s: 1e-3, power: 0.3 }];
    let o = m110_case(4800, Short, &Channel { paths: two, snr_db: Some(20.0), offset_hz: -40.0, ..Channel::clean(fs) }, 6);
    assert_eq!(modes(&o.events), vec![(4800, "none")]);
    assert!(o.decoded >= 5, "4800: {}", o.decoded);
}

#[test]
fn identified_at_5_db() {
    let o = s4285_case(600, 2, s4285::Interleave::Short, 8.0, &hard(5.0, 30), 2);
    assert_eq!(waveforms(&o.events).first(), Some(&NAME_4285));
    assert!(o.events.iter().any(|e| matches!(e, StanagEvent::Waveform { modulation: Some("BPSK"), .. })));
    assert!(o.status.snr_db.is_some());
    let o = m110_case(1200, m110a::Interleave::Short, &hard(5.0, 31), 4);
    assert_eq!(waveforms(&o.events).first(), Some(&NAME_110A));
    assert_eq!(modes(&o.events), vec![(1200, "short (0.6 s)")]);
}

#[test]
fn nothing_is_named_in_noise_a_carrier_or_ale() {
    let fs = f64::from(FS);
    let mut audio = noise(fs, 20.0, 5);
    // A steady carrier on 1800 Hz: it correlates with itself at every lag.
    let n = audio.len();
    for (i, a) in audio.iter_mut().enumerate().skip(n / 2) {
        *a += 0.2 * (2.0 * std::f64::consts::PI * 1800.0 * i as f64 / fs).sin() as f32;
    }
    let (ev, d) = run(&audio);
    assert!(ev.is_empty(), "{:?}", ev.iter().map(StanagEvent::summary).collect::<Vec<_>>());
    assert!(!d.status().locked);
    // ALE is named as itself and nothing else.
    let ale = through(&ale::transmit(&ale_words(), fs, 0.0), &Channel { snr_db: Some(10.0), ..Channel::clean(fs) });
    let (ev, _) = run(&ale);
    assert_eq!(waveforms(&ev), vec![NAME_ALE]);
    assert!(dpdus(&ev).is_empty() && modes(&ev).is_empty());
}

#[test]
fn m110a_joined_after_its_preamble_is_identified() {
    let fs = f64::from(FS);
    let (bytes, _) = traffic(0, 900, 4);
    let syms = m110a::transmit(m110a::test_mode(600, m110a::Interleave::Short), &bytes);
    let ch = Channel { lead_s: 0.0, ..Channel::moderate(fs, 15.0) };
    let audio = modulate(&syms, 2400.0, &ch);
    // Drop the preamble: three 200 ms segments.
    let (ev, d) = run(&audio[(0.65 * fs) as usize..]);
    assert_eq!(waveforms(&ev), vec![NAME_110A]);
    assert!(modes(&ev).is_empty(), "nothing said the mode");
    assert!(d.status().scores.iter().any(|&(w, s)| w == NAME_110A && s > 0.5), "{:?}", d.status().scores);
}

/// An ALE individual call (the called address repeated while it scans,
/// then the caller's) and, after a pause, a sounding.
fn ale_words() -> Vec<(u8, [u8; 3])> {
    let mut w = Vec::new();
    for _ in 0..4 {
        w.push((2, *b"KJ4"));
        w.push((0, *b"XYZ"));
    }
    for _ in 0..2 {
        w.push((5, *b"NAV"));
        w.push((0, *b"Y01"));
    }
    w
}

#[test]
fn ale_calls_and_soundings_decode() {
    let fs = f64::from(FS);
    let mut audio = ale::transmit(&ale_words(), fs, 0.0);
    audio.extend(vec![0.0; (fs * 1.5) as usize]);
    audio.extend(ale::transmit(&[(3, *b"SHP"); 3], fs, 0.0));
    audio.extend(vec![0.0; (fs * 1.5) as usize]);
    for ch in [
        Channel { snr_db: Some(-3.0), offset_hz: -40.0, ..Channel::clean(fs) },
        Channel { offset_hz: 50.0, ..Channel::poor(fs, 6.0) },
    ] {
        let (ev, d) = run(&through(&audio, &ch));
        let calls: Vec<String> = ev.iter().filter(|e| e.kind() == "ale_call").map(StanagEvent::summary).collect();
        assert_eq!(calls, vec!["ALE call KJ4XYZ from NAVY01 (TIS)", "ALE sounding from SHP (TWAS)"]);
        assert_eq!(waveforms(&ev), vec![NAME_ALE]);
        let words: Vec<String> = ev.iter().filter(|e| e.kind() == "ale_word").map(StanagEvent::summary).collect();
        assert!(words.contains(&"ALE TO KJ4".to_string()), "{words:?}");
        let _ = d;
    }
}

#[test]
fn event_fields_are_camel_case() {
    let o = m110_case(600, m110a::Interleave::Short, &Channel::clean(f64::from(FS)), 2);
    let d = o.events.iter().find(|e| e.kind() == "dpdu").expect("a D_PDU");
    let f = d.fields();
    let get = |k: &str| f.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
    assert_eq!(get("dpduTypeName").as_deref(), Some("NON-ARQ DATA"));
    assert_eq!(get("source").as_deref(), Some("0.0.86.120"));
    assert_eq!(get("destination").as_deref(), Some("0.0.18.52"));
    assert_eq!(get("payloadText").as_deref(), Some("MSG 000 QTH AT SEA ALL WELL"));
    assert!(d.summary().contains("NON-ARQ DATA 0.0.86.120 -> 0.0.18.52"), "{}", d.summary());
    for e in &o.events {
        for (k, _) in e.fields() {
            assert!(k.chars().next().unwrap().is_ascii_lowercase() && !k.contains('_'), "{k}");
        }
    }
    let w = o.events.iter().find(|e| e.kind() == "waveform").unwrap();
    assert!(w.summary().starts_with("MIL-STD-188-110A"), "{}", w.summary());
}

#[test]
fn encrypted_payloads_are_reported_by_size() {
    let fs = f64::from(FS);
    let mut rng = Rng(77);
    let secret: Vec<u8> = (0..120).map(|_| rng.byte()).collect();
    let mut bytes = vec![0u8; 8];
    bytes.extend(s5066::non_arq(0x10, 0x20, 3, &secret));
    bytes.extend([0u8; 32]);
    let syms = m110a::transmit(m110a::test_mode(1200, m110a::Interleave::Short), &bytes);
    let (ev, _) = run(&modulate(&syms, 2400.0, &Channel::clean(fs)));
    let d = ev.iter().find(|e| e.kind() == "dpdu").expect("a D_PDU");
    assert!(d.summary().contains("120 bytes, encrypted or compressed (not decodable)"), "{}", d.summary());
    assert!(d.fields().iter().all(|(k, _)| *k != "payloadHex"));
}

/// Decoding time over audio time, for one core, in release.
#[test]
fn real_time_factor() {
    let fs = f64::from(FS);
    let mut audio = Vec::new();
    // BPSK, where the most modes are tried at once (twelve coded guesses,
    // the long ones' deinterleavers among them), then a 110A message.
    let (bytes, _) = traffic(20, 400, 3);
    let syms = s4285::transmit(s4285::mode(600, 2, s4285::Interleave::Long), &bytes, 280);
    audio.extend(modulate(&syms, 2400.0, &hard(12.0, 3)));
    let (bytes, _) = traffic(20, 8, 4);
    let syms = m110a::transmit(m110a::test_mode(2400, m110a::Interleave::Long), &bytes);
    audio.extend(modulate(&syms, 2400.0, &hard(20.0, 4)));
    let t0 = Instant::now();
    let (ev, _) = run(&audio);
    let rtf = t0.elapsed().as_secs_f64() / (audio.len() as f64 / fs);
    eprintln!("real-time factor {rtf:.4} over {:.0} s of audio", audio.len() as f64 / fs);
    assert!(waveforms(&ev).contains(&NAME_4285) && waveforms(&ev).contains(&NAME_110A));
    assert!(rtf < 0.10, "rtf {rtf}");
}

#[test]
fn m110a_75_bps() {
    use m110a::Interleave::*;
    // Walsh-coded, no probes: table XX asks 2 dB (with long interleave).
    for (il, snr) in [(Short, 2.0), (Long, 0.0)] {
        let o = m110_case(75, il, &hard(snr, 24), 2);
        assert_eq!(modes(&o.events).len(), 1, "{il:?}");
        assert_eq!(o.decoded, 2, "{il:?}");
        assert!(eom(&o.events), "{il:?}");
    }
}

/// Decode rates over SNR for each 4285 mode on the hard channel, three
/// seeds each.
#[test]
#[ignore]
fn sweep_4285() {
    use s4285::Interleave::Short;
    for (bps, order, snrs) in [
        (2400u32, 8usize, [18.0, 22.0, 25.0]),
        (1200, 4, [8.0, 11.0, 14.0]),
        (600, 2, [4.0, 7.0, 10.0]),
        (300, 2, [2.0, 5.0, 8.0]),
        (150, 2, [0.0, 3.0, 6.0]),
        (75, 2, [-2.0, 1.0, 4.0]),
    ] {
        for snr in snrs {
            let (mut got, mut sent) = (0, 0);
            for seed in 0..3 {
                let n = (bps as usize / 150).clamp(2, 8);
                let secs = if bps < 300 { 20.0 } else { 10.0 };
                got += s4285_case(bps, order, Short, secs, &hard(snr, 10 + seed), n).decoded;
                sent += n;
            }
            eprintln!("== 4285 {bps} short at {snr} dB: {got}/{sent}");
        }
    }
}

/// The same for 188-110A, four seeds each.
#[test]
#[ignore]
fn sweep_110a() {
    use m110a::Interleave::*;
    for (bps, il, snrs) in [
        (2400u32, Short, [18.0, 22.0, 26.0]),
        (1200, Short, [8.0, 11.0, 14.0]),
        (600, Short, [4.0, 7.0, 10.0]),
        (300, Short, [2.0, 5.0, 8.0]),
        (2400, Long, [14.0, 18.0, 22.0]),
        (1200, Long, [8.0, 11.0, 14.0]),
        (600, Long, [4.0, 7.0, 10.0]),
        (75, Short, [-1.0, 2.0, 5.0]),
        (75, Long, [-1.0, 2.0, 5.0]),
    ] {
        for snr in snrs {
            let mut got = 0;
            for seed in 0..4 {
                got += m110_case(bps, il, &hard(snr, 20 + seed), 6).decoded;
            }
            eprintln!("== 110A {bps} {il:?} at {snr} dB: {got}/24");
        }
    }
}
