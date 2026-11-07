use super::assemble::{Assembler, Js8Message};
use super::varicode::{self, COMMANDS};
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
/// at `rate`, measured in 2500 Hz.
fn amplitude(snr_db: f32, rate: f32) -> f32 {
    let noise_2500 = 2500.0 / (rate / 2.0);
    (2.0 * noise_2500 * 10f32.powf(snr_db / 10.0)).sqrt()
}

// The test encoder: frames packed from JS8Call's layouts, independently of
// the decoder's unpacking.

fn to_payload(bits: &[bool]) -> [u8; 9] {
    assert_eq!(bits.len(), 72);
    let mut p = [0u8; 9];
    for (i, &b) in bits.iter().enumerate() {
        p[i / 8] |= u8::from(b) << (7 - i % 8);
    }
    p
}

fn push(bits: &mut Vec<bool>, v: u64, n: usize) {
    bits.extend((0..n).rev().map(|i| (v >> i) & 1 == 1));
}

/// [3 type][50 call][16 extra][3 more].
fn compound_layout(ty: u64, call: &str, extra: u16, more: u8) -> [u8; 9] {
    let mut b = Vec::new();
    push(&mut b, ty, 3);
    push(&mut b, varicode::pack_alphanumeric50(call), 50);
    push(&mut b, (extra >> 5) as u64, 11);
    push(&mut b, (extra & 31) as u64, 5);
    push(&mut b, more as u64, 3);
    to_payload(&b)
}

fn heartbeat(call: &str, grid: &str, cq: Option<u8>) -> [u8; 9] {
    let mut extra = varicode::pack_grid(grid);
    if cq.is_some() {
        extra |= 1 << 15;
    }
    compound_layout(0, call, extra, cq.unwrap_or(0))
}

fn compound(call: &str, grid: &str) -> [u8; 9] {
    compound_layout(1, call, varicode::pack_grid(grid), 0)
}

fn command(name: &str) -> u8 {
    COMMANDS.iter().position(|c| *c == name).unwrap_or_else(|| panic!("no command {name:?}")) as u8
}

fn directed(from: &str, to: &str, cmd: &str, num: Option<i32>) -> [u8; 9] {
    let (f, pf) = varicode::pack_callsign(from).unwrap();
    let (t, pt) = varicode::pack_callsign(to).unwrap();
    let mut b = Vec::new();
    push(&mut b, 3, 3);
    push(&mut b, f as u64, 28);
    push(&mut b, t as u64, 28);
    push(&mut b, command(cmd) as u64, 5);
    push(&mut b, pf as u64, 1);
    push(&mut b, pt as u64, 1);
    push(&mut b, num.map_or(0, |n| (n.clamp(-30, 31) + 31) as u64), 6);
    to_payload(&b)
}

/// Text into data frames as JS8Call cuts it: dictionary codes while they
/// fit under 72 bits, then a zero and ones. Fast, Turbo and Slow flag the
/// frame as data; Normal still sends the two-bit prefix of the first
/// releases.
fn data_frames(speed: Js8Speed, text: &str) -> Vec<([u8; 9], u8)> {
    let fast = speed != Js8Speed::Normal;
    let mut out = Vec::new();
    let mut rest = text.to_string();
    while !rest.is_empty() {
        let mut bits: Vec<bool> = if fast { vec![] } else { vec![true, true] };
        let mut used = 0;
        for (code, n) in jsc::compress(&rest) {
            if bits.len() + code.len() >= 72 {
                break;
            }
            bits.extend(code);
            used += n;
        }
        assert!(used > 0, "{rest:?} does not compress");
        let pad = 72 - bits.len();
        bits.extend((0..pad).map(|i| i > 0));
        out.push((to_payload(&bits), if fast { 4 } else { 0 }));
        rest = rest[used.min(rest.len())..].to_string();
    }
    out
}

/// Flags for a run of frames: first on the first, last on the last.
fn transmission(frames: Vec<([u8; 9], u8)>) -> Vec<([u8; 9], u8)> {
    let n = frames.len();
    frames.into_iter().enumerate().map(|(i, (p, f))| (p, f | u8::from(i == 0) | if i + 1 == n { 2 } else { 0 })).collect()
}

/// A directed message with a body, the body closed by its checksum when
/// the command calls for one.
fn directed_message(speed: Js8Speed, from: &str, to: &str, cmd: &str, body: &str) -> Vec<([u8; 9], u8)> {
    let mut frames = vec![(directed(from, to, cmd, None), 0)];
    let c = command(cmd);
    let body = if varicode::checksummed(c) { format!("{body} {}", varicode::checksum16(body)) } else { body.to_string() };
    frames.extend(data_frames(speed, &body));
    transmission(frames)
}

struct Station {
    frames: Vec<([u8; 9], u8)>,
    freq: f32,
    dt: f32,
}

/// Every slot of a band, `slots` long, with `stations` sending their frames
/// one a slot from the first, in noise, at `snr_db`.
fn band(speed: Js8Speed, stations: &[Station], slots: usize, snr_db: f32, seed: u64) -> Vec<Vec<f32>> {
    let len = speed.slot_secs() as f32;
    let a = amplitude(snr_db, RATE_HZ);
    let mut noise = Noise(seed);
    (0..slots)
        .map(|k| {
            let mut audio: Vec<f32> = (0..(len * RATE_HZ) as usize).map(|_| noise.gauss()).collect();
            for s in stations {
                if let Some((p, flags)) = s.frames.get(k) {
                    let wave = synthesize(speed, p, *flags, s.freq, speed.start_delay_s() + s.dt, len);
                    for (x, w) in audio.iter_mut().zip(wave) {
                        *x += a * w;
                    }
                }
            }
            audio
        })
        .collect()
}

/// Decode a band slot by slot as the stream would, and join the frames.
fn run(speed: Js8Speed, slots: &[Vec<f32>]) -> (Vec<Js8Frame>, Vec<Js8Message>) {
    let slot_ms = (speed.slot_secs() * 1000.0) as u64;
    let mut asm = Assembler::default();
    let (mut frames, mut messages) = (Vec::new(), Vec::new());
    for (k, audio) in slots.iter().enumerate() {
        let start = 1_700_000_010_000 / 30_000 * 30_000 + k as u64 * slot_ms;
        let t = std::time::Instant::now();
        let mut got = decode_slot(speed, audio);
        for f in &mut got {
            f.slot_ms = start;
        }
        eprintln!("{} slot {k}: {} frames in {:?}", speed.label(), got.len(), t.elapsed());
        for f in &got {
            eprintln!("  {:+3.0} {:+.2} {:7.1} [{}] {}", f.snr_db, f.dt_s, f.freq_hz, f.flags, f.text());
        }
        messages.extend(asm.slot(speed, start, &got));
        frames.extend(got);
    }
    messages.extend(asm.flush());
    for m in &messages {
        eprintln!("  => {} {:?} {}", m.kind, m.complete, m.text);
    }
    (frames, messages)
}

#[test]
fn a_heartbeat_and_a_cq() {
    let stations = [
        Station { frames: transmission(vec![(heartbeat("KN4CRD", "EM73", None), 0)]), freq: 1_200.0, dt: 0.0 },
        Station { frames: transmission(vec![(heartbeat("VE7/KN4CRD", "CN88", Some(1)), 0)]), freq: 1_900.0, dt: 0.3 },
    ];
    let (frames, msgs) = run(Js8Speed::Normal, &band(Js8Speed::Normal, &stations, 1, -16.0, 1));
    assert_eq!(frames.len(), 2);
    let hb = msgs.iter().find(|m| m.kind == "heartbeat").expect("heartbeat");
    assert_eq!(hb.text, "KN4CRD: @HB HEARTBEAT EM73");
    assert_eq!((hb.from.as_deref(), hb.grid.as_deref(), hb.complete), (Some("KN4CRD"), Some("EM73"), true));
    assert!((hb.freq_hz - 1_200.0).abs() < 1.0 && hb.dt_s.abs() < 0.08, "{} {}", hb.freq_hz, hb.dt_s);
    assert!((hb.snr_db + 16.0).abs() <= 2.0, "snr {}", hb.snr_db);
    let cq = msgs.iter().find(|m| m.kind == "cq").expect("cq");
    assert_eq!(cq.text, "VE7/KN4CRD: @ALLCALL CQ DX CN88");
    assert_eq!(cq.command.as_deref(), Some("CQ DX"));
    assert!((cq.dt_s - 0.3).abs() < 0.08);
}

#[test]
fn multi_frame_free_text_is_joined() {
    let text = "KN4CRD: GOOD MORNING FROM THE SHACK IN GEORGIA, RIG IS AN IC7300 AT 20W INTO A DIPOLE";
    for speed in [Js8Speed::Normal, Js8Speed::Fast] {
        let frames = transmission(data_frames(speed, text));
        assert!(frames.len() >= 3, "{} frames", frames.len());
        let n = frames.len();
        let stations = [Station { frames, freq: 1_000.0, dt: 0.1 }];
        let (got, msgs) = run(speed, &band(speed, &stations, n + 1, -14.0, 7));
        assert_eq!(got.len(), n);
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        let m = &msgs[0];
        assert_eq!((m.kind, m.complete, m.frames), ("message", true, n));
        assert_eq!(m.text, text);
        assert_eq!(m.from.as_deref(), Some("KN4CRD"));
    }
}

#[test]
fn directed_queries_replies_and_messages() {
    let speed = Js8Speed::Normal;
    let stations = [
        Station { frames: transmission(vec![(directed("KN4CRD", "K0OG", " SNR?", None), 0)]), freq: 700.0, dt: -0.2 },
        Station { frames: transmission(vec![(directed("K0OG", "KN4CRD", " SNR", Some(-12)), 0)]), freq: 1_150.0, dt: 0.4 },
        Station { frames: transmission(vec![(directed("W1AW", "@ALLCALL", " GRID?", None), 0)]), freq: 1_600.0, dt: 0.0 },
        Station { frames: directed_message(speed, "KB1ABC", "G4ABC", " MSG", "SEE YOU ON 40M TONIGHT"), freq: 2_100.0, dt: 0.2 },
    ];
    let slots = band(speed, &stations, 3, -15.0, 11);
    let (_, msgs) = run(speed, &slots);
    let snrq = msgs.iter().find(|m| m.from.as_deref() == Some("KN4CRD")).expect("SNR?");
    assert_eq!((snrq.kind, snrq.to.as_deref(), snrq.command.as_deref()), ("directed", Some("K0OG"), Some("SNR?")));
    assert_eq!(snrq.text, "KN4CRD: K0OG SNR?");
    let rep = msgs.iter().find(|m| m.kind == "snrReply").expect("reply");
    assert_eq!((rep.from.as_deref(), rep.num), (Some("K0OG"), Some(-12)));
    assert_eq!(rep.text, "K0OG: KN4CRD SNR -12");
    let grid = msgs.iter().find(|m| m.to.as_deref() == Some("@ALLCALL")).expect("GRID?");
    assert_eq!(grid.command.as_deref(), Some("GRID?"));
    let msg = msgs.iter().find(|m| m.command.as_deref() == Some("MSG")).expect("MSG");
    assert_eq!((msg.from.as_deref(), msg.to.as_deref(), msg.checksum_ok, msg.complete), (Some("KB1ABC"), Some("G4ABC"), Some(true), true));
    assert_eq!(msg.body, "SEE YOU ON 40M TONIGHT");
    assert_eq!(msg.text, "KB1ABC: G4ABC MSG SEE YOU ON 40M TONIGHT");
    assert_eq!(msgs.len(), 4, "nothing invented");
}

#[test]
fn compound_calls_fill_the_placeholders() {
    // VE7/KN4CRD acknowledges J1Y/P: both compound, so a compound frame and
    // a compound directed frame (JS8Call's third case).
    let mut cd = Vec::new();
    push(&mut cd, 2, 3);
    push(&mut cd, varicode::pack_alphanumeric50("J1Y/P"), 50);
    // A command where a grid would go: 32 410 and up.
    let extra = 32_410 + command(" ACK") as u16;
    push(&mut cd, (extra >> 5) as u64, 11);
    push(&mut cd, (extra & 31) as u64, 5);
    push(&mut cd, 0, 3);
    let frames = transmission(vec![(compound("VE7/KN4CRD", "CN88"), 0), (to_payload(&cd), 0)]);
    // And a plain call answering a compound one through a placeholder.
    let frames2 = transmission(vec![(compound("PJ4/K1ABC", "FK52"), 0), (directed("<....>", "W9XYZ", " ACK", None), 0)]);
    let speed = Js8Speed::Turbo;
    let stations = [Station { frames, freq: 900.0, dt: 0.0 }, Station { frames: frames2, freq: 1_700.0, dt: 0.1 }];
    let (_, msgs) = run(speed, &band(speed, &stations, 2, -10.0, 3));
    let a = msgs.iter().find(|m| m.to.as_deref() == Some("J1Y/P")).expect("compound to compound");
    assert_eq!((a.from.as_deref(), a.command.as_deref(), a.grid.as_deref()), (Some("VE7/KN4CRD"), Some("ACK"), Some("CN88")));
    assert_eq!(a.text, "VE7/KN4CRD: J1Y/P ACK");
    let b = msgs.iter().find(|m| m.to.as_deref() == Some("W9XYZ")).expect("placeholder");
    assert_eq!(b.from.as_deref(), Some("PJ4/K1ABC"));
    assert!(a.complete && b.complete);
}

#[test]
fn several_stations_in_one_slot_all_decode() {
    let speed = Js8Speed::Normal;
    let stations: Vec<Station> = [
        ("KN4CRD", "EM73", 512.5, -0.4),
        ("K0OG", "EN34", 803.0, 0.0),
        ("W1AW", "FN31", 1_091.0, 0.6),
        ("G4ABC", "IO91", 1_404.6, 1.1),
        ("VK2DEF", "QF56", 1_870.0, -1.0),
        ("JA1XYZ", "PM95", 2_390.0, 0.3),
    ]
    .iter()
    .map(|&(call, grid, freq, dt)| Station { frames: transmission(vec![(heartbeat(call, grid, None), 0)]), freq, dt })
    .collect();
    let (frames, msgs) = run(speed, &band(speed, &stations, 1, -17.0, 0xfeed));
    for s in &stations {
        let f = frames.iter().find(|f| (f.freq_hz - s.freq).abs() < 2.0).unwrap_or_else(|| panic!("nothing at {}", s.freq));
        assert!((f.dt_s - s.dt).abs() < 0.08, "dt {} vs {}", f.dt_s, s.dt);
        assert!((f.snr_db + 17.0).abs() <= 2.0, "snr {}", f.snr_db);
    }
    assert_eq!(frames.len(), stations.len(), "nothing invented");
    assert_eq!(msgs.len(), stations.len());
}

#[test]
fn every_speed_decodes_its_own_frames_only() {
    let p = directed("KN4CRD", "K0OG", " ACK", None);
    for speed in Js8Speed::ALL {
        let stations = [Station { frames: transmission(vec![(p, 0)]), freq: 1_300.0, dt: 0.05 }];
        let slots = band(speed, &stations, 1, -14.0, 99);
        let got = decode_slot(speed, &slots[0]);
        assert_eq!(got.len(), 1, "{}", speed.label());
        assert_eq!(got[0].text(), "KN4CRD: K0OG ACK");
        // A receiver set to another speed hears nothing in it.
        for other in Js8Speed::ALL.into_iter().filter(|o| *o != speed) {
            let n = (other.slot_secs() as f32 * RATE_HZ) as usize;
            let audio: Vec<f32> = slots[0].iter().copied().cycle().take(n).collect();
            assert!(decode_slot(other, &audio).is_empty(), "{} heard as {}", speed.label(), other.label());
        }
    }
}

#[test]
fn noise_alone_decodes_to_nothing() {
    for speed in Js8Speed::ALL {
        for seed in 0..3 {
            let slots = band(speed, &[], 1, 0.0, 0x5eed + seed);
            let got = decode_slot(speed, &slots[0]);
            assert!(got.is_empty(), "{}: {:?}", speed.label(), got.iter().map(|f| f.text()).collect::<Vec<_>>());
        }
    }
}

#[test]
fn a_huffman_frame_from_the_first_releases() {
    // [1 data][0 Huffman] "HELLO 73", then the zero-and-ones pad.
    let code = |c: char| match c {
        'H' => "00011",
        'E' => "100",
        'L' => "110011",
        'O' => "11111",
        ' ' => "01",
        '7' => "11101011",
        '3' => "0000101",
        _ => unreachable!(),
    };
    let mut bits = vec![true, false];
    for c in "HELLO 73".chars() {
        bits.extend(code(c).bytes().map(|b| b == b'1'));
    }
    let pad = 72 - bits.len();
    bits.extend((0..pad).map(|i| i > 0));
    let content = varicode::unpack(&to_payload(&bits), 3);
    assert_eq!(content, FrameContent::Data { text: "HELLO 73".into(), compressed: false });
}

#[test]
fn a_missed_frame_leaves_the_message_incomplete() {
    let speed = Js8Speed::Fast;
    let text = "THIS MESSAGE LOSES ITS MIDDLE FRAME TO A STATIC CRASH ON THE BAND TODAY";
    let frames = transmission(data_frames(speed, text));
    assert!(frames.len() >= 3);
    let n = frames.len();
    let stations = [Station { frames, freq: 1_500.0, dt: 0.0 }];
    let mut slots = band(speed, &stations, n, -12.0, 5);
    // The middle frame is lost: its slot holds only noise.
    slots[1] = band(speed, &[], 1, 0.0, 6).remove(0);
    let (_, msgs) = run(speed, &slots);
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    let m = &msgs[0];
    assert!(!m.complete && m.missed == 1, "{m:?}");
    assert!(m.text.contains(" … "), "{}", m.text);
}

/// Through the streaming front end: a 47 904 Hz stream with jittery
/// stamps, Normal and Turbo at once, a two-frame message on Normal and
/// heartbeats on Turbo.
#[test]
fn the_stream_decodes_two_speeds_at_once() {
    let rate = 47_904.0f32;
    let t0: u64 = 1_700_000_010_000 / 30_000 * 30_000;
    let lead = 2.0f32;
    let secs = 31.5f32;
    let total = ((lead + secs) * rate) as usize;
    // Turbo's symbols are a third as long as Normal's: about 5 dB less
    // sensitive.
    let level = |speed: Js8Speed| amplitude(if speed == Js8Speed::Turbo { -9.0 } else { -14.0 }, rate);
    let mut noise = Noise(17);
    let mut audio: Vec<f32> = (0..total).map(|_| noise.gauss()).collect();
    let mut add = |speed: Js8Speed, slot: usize, (p, flags): ([u8; 9], u8), freq: f32| {
        let tones = demod::tones(speed, &code::encode(&p, flags));
        let wave = demod::fsk(speed, &tones, freq, rate);
        let at = ((lead + slot as f32 * speed.slot_secs() as f32 + speed.start_delay_s()) * rate) as usize;
        let a = level(speed);
        for (x, w) in audio[at..].iter_mut().zip(wave) {
            *x += a * w;
        }
    };
    let msg = directed_message(Js8Speed::Normal, "KN4CRD", "K0OG", " MSG", "HI JOHN");
    assert_eq!(msg.len(), 2);
    for (k, f) in msg.into_iter().enumerate() {
        add(Js8Speed::Normal, k, f, 1_100.0);
    }
    for k in 0..4 {
        add(Js8Speed::Turbo, k, transmission(vec![(heartbeat("W1AW", "FN31", None), 0)])[0], 2_000.0 + 40.0 * k as f32);
    }
    let mut dec = Js8Decoder::new(rate, &[Js8Speed::Turbo, Js8Speed::Normal]);
    let start_ms = t0 as f64 - lead as f64 * 1000.0;
    let mut jitter = Noise(23);
    let mut events = Vec::new();
    for (k, block) in audio.chunks(480).enumerate() {
        let end = k * 480 + block.len() - 1;
        let stamp = start_ms + end as f64 / rate as f64 * 1000.0 + (jitter.uniform() - 0.5) * 30.0;
        dec.push(block, stamp as u64);
        // Audio comes in real time on the air; here it comes as fast as it
        // can, so give the worker the time the air would.
        while dec.status().queued > 1 {
            events.extend(dec.take());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    for _ in 0..400 {
        events.extend(dec.take());
        if !dec.busy() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    events.extend(dec.take());
    for e in &events {
        eprintln!("{}: {}", e.kind(), e.summary());
    }
    let status = dec.status();
    eprintln!("{status:?}");
    let m = events
        .iter()
        .find_map(|e| match e {
            Js8Event::Message(m) if m.command.as_deref() == Some("MSG") => Some(m),
            _ => None,
        })
        .expect("the MSG");
    assert_eq!((m.text.as_str(), m.checksum_ok, m.complete), ("KN4CRD: K0OG MSG HI JOHN", Some(true), true));
    assert_eq!(m.slot_ms, t0);
    assert!((m.freq_hz - 1_100.0).abs() < 1.0);
    let hbs = events.iter().filter(|e| e.kind() == "heartbeat").count();
    assert_eq!(hbs, 4, "a Turbo heartbeat in each of the slots");
    let fields = events.iter().find(|e| e.kind() == "heartbeat").unwrap().fields();
    assert!(fields.contains(&("speed", "turbo".into())) && fields.contains(&("grid", "FN31".into())), "{fields:?}");
    assert_eq!(status.dropped, 0);
}

/// How weak a signal still decodes:
/// `cargo test --release -p scannerd-engine js8::tests::sensitivity -- --ignored --nocapture`
#[test]
#[ignore]
fn sensitivity() {
    let trials = 40u64;
    for speed in Js8Speed::ALL {
        let p = directed("KN4CRD", "K0OG", " SNR?", None);
        for snr in (-28..=-14).step_by(1).map(|v| v as f32) {
            let mut ok = 0;
            let mut est = Vec::new();
            let t = std::time::Instant::now();
            for seed in 0..trials {
                let s = [Station { frames: transmission(vec![(p, 0)]), freq: 800.0 + seed as f32 * 37.0, dt: (seed % 5) as f32 * 0.1 - 0.2 }];
                let slots = band(speed, &s, 1, snr, seed * 7919 + 1);
                let d = decode_slot(speed, &slots[0]);
                if let Some(f) = d.iter().find(|f| f.payload == p) {
                    ok += 1;
                    est.push(f.snr_db);
                }
            }
            let mean = est.iter().sum::<f32>() / est.len().max(1) as f32;
            let per = t.elapsed().as_secs_f64() / trials as f64;
            eprintln!(
                "{:6} {snr:+} dB: {ok}/{trials} decoded, reported {mean:+.1}, {:.0} ms a slot ({:.3} of real time)",
                speed.label(),
                per * 1000.0,
                per / speed.slot_secs()
            );
            if ok == trials && snr > -20.0 {
                break;
            }
        }
    }
}

/// Many noise slots: how often anything at all comes out.
/// `cargo test --release -p scannerd-engine js8::tests::false_decodes -- --ignored --nocapture`
#[test]
#[ignore]
fn false_decodes() {
    for speed in Js8Speed::ALL {
        let mut n = 0;
        let slots = 60;
        for seed in 0..slots {
            let audio = band(speed, &[], 1, 0.0, 1000 + seed).remove(0);
            let got = decode_slot(speed, &audio);
            for f in &got {
                eprintln!("{} seed {seed}: {} hard {} snr {}", speed.label(), f.text(), f.hard_errors, f.snr_db);
            }
            n += got.len();
        }
        eprintln!("{}: {n} false frames in {slots} slots of noise", speed.label());
    }
}

/// Decode a recorded I/Q capture (16-bit stereo WAV, dial at 0 Hz), in
/// 15 s windows every half second since its start time is not known:
/// `JS8_WAV=path cargo test --release -p scannerd-engine js8::tests::capture -- --ignored --nocapture`
#[test]
#[ignore]
fn capture() {
    use rustfft::{FftPlanner, num_complex::Complex32};
    let Ok(path) = std::env::var("JS8_WAV") else { return };
    let bytes = std::fs::read(&path).unwrap();
    let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap()) as f32;
    let mut x: Vec<Complex32> = bytes[44..]
        .chunks_exact(4)
        .map(|c| Complex32::new(i16::from_le_bytes([c[0], c[1]]) as f32, i16::from_le_bytes([c[2], c[3]]) as f32) / 32768.0)
        .collect();
    let n = x.len();
    FftPlanner::new().plan_fft_forward(n).process(&mut x);
    // The upper sideband, +100..+3100 Hz, straight into a 12 kHz spectrum.
    let m = (n as f64 * RATE_HZ as f64 / rate as f64).round() as usize;
    let df = rate / n as f32;
    let mut y = vec![Complex32::default(); m];
    // The upper sideband unless told otherwise (JS8_SIDE=lower for a
    // capture with I and Q swapped). On 7.078 the side below 0 Hz is louder
    // anyway: it holds the FT8 segment at 7.074.
    let bin = |f: i32| ((f as f32 / df).round() as isize).rem_euclid(n as isize) as usize;
    let pos: f32 = (100..3100).map(|f| x[bin(f)].norm_sqr()).sum();
    let neg: f32 = (100..3100).map(|f| x[bin(-f)].norm_sqr()).sum();
    // Bins to move the band by: to look at FT8 four kHz down, say.
    let shift = std::env::var("JS8_SHIFT").ok().and_then(|v| v.parse::<f32>().ok()).map_or(0, |hz| (hz / df).round() as isize);
    let flip = match std::env::var("JS8_SIDE").as_deref() {
        Ok("upper") => false,
        Ok("lower") => true,
        _ => false,
    };
    eprintln!("  band above 0 Hz {:.1} dB over the band below; using the {} side", 10.0 * (pos / neg).log10(), if flip { "lower (conjugated)" } else { "upper" });
    for (k, v) in y.iter_mut().enumerate().take(m / 2) {
        if (100.0..5000.0).contains(&(k as f32 * df)) {
            let k = (k as isize + shift).rem_euclid(n as isize) as usize;
            *v = if flip { x[(n - k) % n].conj() } else { x[k] };
        }
    }
    // Where the energy is, in 25 Hz bands, against the band's median.
    let band = |lo: f32| -> f32 {
        let (a, b) = (((lo / df) as isize).rem_euclid(n as isize) as usize, ((lo + 25.0) / df) as isize);
        (0..(b - (lo / df) as isize)).map(|i| x[(a + i as usize) % n].norm_sqr()).sum::<f32>()
    };
    let mut bands: Vec<(f32, f32)> = (-120..120).map(|i| (i as f32 * 25.0, band(i as f32 * 25.0))).collect();
    let mut sorted: Vec<f32> = bands.iter().map(|b| b.1).collect();
    sorted.sort_by(f32::total_cmp);
    let med = sorted[sorted.len() / 2];
    bands.sort_by(|a, b| b.1.total_cmp(&a.1));
    eprintln!("  loudest 25 Hz bands, dB over median: {:?}", bands.iter().take(12).map(|(f, p)| (*f, (10.0 * (p / med).log10()).round())).collect::<Vec<_>>());
    FftPlanner::new().plan_fft_inverse(m).process(&mut y);
    let audio: Vec<f32> = y.iter().map(|v| 2.0 * v.re / n as f32).collect();
    eprintln!("{path}: {:.1} s at {rate} Hz", audio.len() as f32 / RATE_HZ);
    if std::env::var("JS8_FT8").is_ok() {
        for off in [0.0f32, 1.0, 2.0, 3.0, 4.0, 5.0] {
            let a = (off * RATE_HZ) as usize;
            let d = crate::ft8::decode_slot(crate::ft8::Protocol::Ft8, &audio[a..a + 180_000], &mut crate::ft8::CallHashes::default());
            eprintln!("  FT8 window {off}: {:?}", d.iter().map(|d| (d.freq_hz.round(), d.text.clone())).collect::<Vec<_>>());
        }
        return;
    }
    let mut seen: Vec<[u8; 9]> = Vec::new();
    for speed in Js8Speed::ALL {
        let len = (speed.slot_secs() as f32 * RATE_HZ) as usize;
        // Windows reaching past either end are padded with silence.
        let mut off = -speed.slot_secs() as f32;
        while off < audio.len() as f32 / RATE_HZ {
            let a = (off * RATE_HZ) as isize;
            let w: Vec<f32> = (0..len as isize).map(|i| audio.get((a + i).max(-1) as usize).copied().unwrap_or(0.0)).collect();
            for f in decode_slot_in(speed, &w, 100.0, 4900.0) {
                if seen.contains(&f.payload) {
                    continue;
                }
                seen.push(f.payload);
                let bits: String = (0..72).map(|i| if (f.payload[i / 8] >> (7 - i % 8)) & 1 == 1 { '1' } else { '0' }).collect();
                eprintln!(
                    "  {} start {:5.2} s {:+3.0} dB {:7.1} Hz flags {} raw {} hard {}\n    bits {bits}\n    {:?}\n    {}",
                    speed.label(),
                    f.dt_s + f.speed.start_delay_s() + off,
                    f.snr_db,
                    f.freq_hz,
                    f.flags,
                    f.raw,
                    f.hard_errors,
                    f.content,
                    f.text()
                );
            }
            off += speed.symbol_period() * 3.0;
        }
    }
}


/// The frame at 552 Hz in the 7.078 MHz capture of 2026-11-06 23:36 UTC
/// (js8-7078-1): only its last 5.4 s were recorded, but the code is
/// systematic, so its message bits were read off symbols 45 to 71 and the
/// two cut-off symbols found as the one filling of 64 the CRC accepts.
const CAPTURED_HB_ACK: &str = "011111110010100101111111000011100111110110000111101111011111110100010100";

fn bits_payload(s: &str) -> [u8; 9] {
    to_payload(&s.bytes().map(|b| b == b'1').collect::<Vec<_>>())
}

#[test]
fn a_captured_heartbeat_acknowledgment_unpacks() {
    let p = bits_payload(CAPTURED_HB_ACK);
    let c = varicode::unpack(&p, 3);
    assert_eq!(c, FrameContent::Directed { from: "W0WLY".into(), to: "9A3SWO".into(), cmd: varicode::CMD_HEARTBEAT_SNR, num: Some(-11) });
    assert!(c.implausible_calls().is_empty());
    // And on the air: encoded, sent and decoded back.
    let speed = Js8Speed::Normal;
    let (_, msgs) = run(speed, &band(speed, &[Station { frames: vec![(p, 3)], freq: 552.0, dt: 0.0 }], 1, -12.0, 41));
    assert_eq!(msgs.len(), 1);
    let m = &msgs[0];
    assert_eq!((m.kind, m.text.as_str(), m.num), ("snrReply", "W0WLY: 9A3SWO HEARTBEAT SNR -11", Some(-11)));
    assert!(m.implausible_calls.is_empty());
}

/// What was reported live at 552 Hz on 7.078 MHz, 23:31 UTC: a compound
/// frame "K4KCF" and a compound directed "1XY/LE/6OR HEARTBEAT SNR -16".
/// That is JS8Call's layout for a heartbeat acknowledgment to a compound
/// callsign, and JS8Call prints these bits the same way; the call is what
/// the sending station's software put on the air (an automatic answer to a
/// heartbeat it decoded falsely, most likely). It is reported as sent and
/// flagged as no callsign.
#[test]
fn an_acknowledgment_to_a_callsign_that_cannot_exist_is_flagged() {
    let mut cd = Vec::new();
    push(&mut cd, 2, 3);
    push(&mut cd, varicode::pack_alphanumeric50("1XY/LE/6OR"), 50);
    // HEARTBEAT SNR -16: the SNR flag, the heartbeat flag and -16 + 31,
    // above the 32 410 where commands start.
    let extra = 32_410 + (0x80 | 0x40 | 15);
    push(&mut cd, (extra >> 5) as u64, 11);
    push(&mut cd, (extra & 31) as u64, 5);
    push(&mut cd, 0, 3);
    let frames = transmission(vec![(compound_layout(1, "K4KCF", (1 << 15) - 1, 0), 0), (to_payload(&cd), 0)]);
    let speed = Js8Speed::Normal;
    let (got, msgs) = run(speed, &band(speed, &[Station { frames, freq: 552.0, dt: 0.0 }], 2, -10.0, 43));
    assert_eq!(got[0].text(), "K4KCF: ");
    assert_eq!(got[1].text(), "1XY/LE/6OR HEARTBEAT SNR -16");
    assert!(got[0].content.implausible_calls().is_empty());
    assert_eq!(got[1].content.implausible_calls(), vec!["1XY/LE/6OR".to_string()]);
    assert_eq!(msgs.len(), 1);
    let m = &msgs[0];
    assert_eq!((m.kind, m.text.as_str()), ("snrReply", "K4KCF: 1XY/LE/6OR HEARTBEAT SNR -16"));
    assert_eq!(m.implausible_calls, vec!["1XY/LE/6OR".to_string()]);
    let e = Js8Event::Message(m.clone());
    assert!(e.fields().contains(&("implausibleCall", "1XY/LE/6OR".into())));
    assert!(e.summary().ends_with("[not a callsign: 1XY/LE/6OR]"), "{}", e.summary());
}

#[test]
fn plausible_callsigns() {
    for c in ["W0WLY", "9A3SWO", "K4KCF", "KN4CRD", "K0OG", "3DA0RU", "2E0ABC", "VE7/KN4CRD", "KN4CRD/P", "PJ4/K1ABC", "J1Y/P", "@ALLCALL", "@GROUP/4", "<....>"] {
        assert!(varicode::plausible_call(c), "{c}");
    }
    for c in ["1XY/LE/6OR", "138SDP", "HELLO", "12345", "K/L/M/N", ""] {
        assert!(!varicode::plausible_call(c), "{c}");
    }
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

/// A fixed-seed sweep of Normal-speed frames around the threshold, in quiet
/// and under static crashes, counting decodes, false decodes and time:
/// `cargo test --release -p scannerd-engine js8::tests::threshold_sweep -- --ignored --nocapture`
#[test]
#[ignore]
fn threshold_sweep() {
    let speed = Js8Speed::Normal;
    const FROM: [&str; 5] = ["KN4CRD", "K0OG", "W1AW", "DL1ABC", "VK2DEF"];
    const TO: [&str; 3] = ["KA9Q", "G4ABC", "JA1XYZ"];
    let crashes = [(0usize, 0.0f32, 0.0f32), (4, 15.0, 30.0), (12, 8.0, 60.0)];
    let only: Option<usize> = std::env::var("SWEEP_CASE").ok().and_then(|v| v.parse().ok());
    let crashes: Vec<_> = crashes.into_iter().enumerate().filter(|(i, _)| only.is_none_or(|o| o == *i)).map(|(_, c)| c).collect();
    for (n, ms, gain) in crashes {
        let (mut total_ok, mut total_false, mut total_sent) = (0, 0, 0);
        let mut time = std::time::Duration::ZERO;
        for snr in [-25.0f32, -24.0, -23.0, -22.0, -21.0, -20.0, -19.0] {
            let (mut ok, mut bad, mut sent) = (0, 0, 0);
            for slot in 0..12u64 {
                let stations: Vec<Station> = (0..5)
                    .map(|i| Station {
                        frames: transmission(vec![(directed(FROM[i], TO[(i + slot as usize) % 3], " SNR?", None), 0)]),
                        freq: 400.0 + i as f32 * 480.0 + slot as f32 * 13.7,
                        dt: ((i + slot as usize) % 7) as f32 * 0.15 - 0.3,
                    })
                    .collect();
                let mut audio = band(speed, &stations, 1, snr, slot * 7919 + 17).remove(0);
                if n > 0 {
                    add_crashes(&mut audio, n, ms, gain, slot * 31 + 5);
                }
                let t = std::time::Instant::now();
                let d = demod::decode_slot_budget(speed, &audio, F_MIN_HZ, F_MAX_HZ, std::time::Duration::MAX);
                time += t.elapsed();
                sent += stations.len();
                for f in &d {
                    if stations.iter().any(|s| s.frames[0] == (f.payload, f.flags)) { ok += 1 } else { bad += 1 }
                }
            }
            eprintln!("JS8 crashes {n}x{ms}ms@{gain}: {snr:+} dB {ok}/{sent} decoded, {bad} false");
            total_ok += ok;
            total_false += bad;
            total_sent += sent;
        }
        eprintln!("JS8 crashes {n}x{ms}ms@{gain}: TOTAL {total_ok}/{total_sent}, {total_false} false, {:?} per slot", time / 84);
    }
}
