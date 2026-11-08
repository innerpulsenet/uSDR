use super::*;
use crate::rtty::testsig::{Rng, add_noise, chunks, levenshtein};

const TEXT: &str =
    "CQ CQ CQ DE W1AW W1AW W1AW PSE K. THE QUICK BROWN FOX JUMPS OVER THE LAZY DOG 0123456789 ";
const SHORT: &str = "CQ CQ DE W1AW W1AW W1AW K";
const FS: f32 = 47_904.0;

#[derive(Clone, Copy, PartialEq)]
enum TxKind {
    Domino,
    DominoFec,
    Thor,
}

fn mfsk_code(c: u8) -> &'static str {
    varicode::VARICODE[c as usize]
}

/// Data bits to nibbles, as fldigi's `sendchar` packs them.
struct Coder {
    enc: Encoder,
    inlv: Interleaver<u8>,
    shreg: u8,
    n: u8,
}

impl Coder {
    fn new(depth: usize) -> Self {
        Self {
            enc: Encoder::default(),
            inlv: Interleaver::new(4, depth, true, 0),
            shreg: 0,
            n: 0,
        }
    }

    fn code(&mut self, bits: &str, out: &mut Vec<u8>) {
        for b in bits.bytes() {
            let (a, c) = self.enc.push(b == b'1');
            for bit in [a, c] {
                self.shreg = (self.shreg << 1) | bit as u8;
                self.n += 1;
                if self.n == 4 {
                    let n = self.shreg;
                    let mut s = [(n >> 3) & 1, (n >> 2) & 1, (n >> 1) & 1, n & 1];
                    self.inlv.symbols(&mut s);
                    out.push(s[0] << 3 | s[1] << 2 | s[2] << 1 | s[3]);
                    self.n = 0;
                    self.shreg = 0;
                }
            }
        }
    }
}

fn domino_send(c: u8, secondary: bool, out: &mut Vec<u8>) {
    let code = &DOMINO_CODES[c as usize + if secondary { 256 } else { 0 }];
    out.push(code[0]);
    for &n in &code[1..] {
        if n & 8 == 0 {
            break;
        }
        out.push(n);
    }
}

/// The nibbles `tx_process` would send for `text`.
fn nibbles(kind: TxKind, speed: IfkSpeed, text: &str) -> Vec<u8> {
    let mut v = Vec::new();
    let micro = speed == IfkSpeed::Micro;
    match kind {
        TxKind::Domino => {
            domino_send(0, true, &mut v);
            domino_send(b'\r', false, &mut v);
            if !micro {
                domino_send(2, false, &mut v);
                domino_send(b'\r', false, &mut v);
            }
            for b in text.bytes() {
                domino_send(b, false, &mut v);
            }
            domino_send(b'\r', false, &mut v);
            if !micro {
                domino_send(4, false, &mut v);
                domino_send(b'\r', false, &mut v);
            }
            for _ in 0..4 {
                domino_send(0, true, &mut v);
            }
        }
        TxKind::DominoFec => {
            let mut coder = Coder::new(4);
            coder.code(mfsk_code(0), &mut v);
            for b in "\r".bytes().chain(text.bytes()).chain("\r".bytes()) {
                coder.code(mfsk_code(b), &mut v);
            }
            // Idles enough to carry the last characters through the decoder.
            for _ in 0..10 {
                coder.code(mfsk_code(0), &mut v);
            }
        }
        TxKind::Thor => {
            let depth = if micro { 4 } else { 10 };
            v.extend(std::iter::repeat_n(0u8, 16));
            let mut coder = Coder::new(depth);
            coder.code(mfsk_code(0), &mut v);
            coder.code(mfsk_code(b'\r'), &mut v);
            if !micro {
                coder.code(mfsk_code(2), &mut v);
                coder.code(mfsk_code(b'\r'), &mut v);
            }
            for b in text.bytes() {
                coder.code(mfsk_code(b), &mut v);
            }
            coder.code(mfsk_code(b'\r'), &mut v);
            if !micro {
                coder.code(mfsk_code(4), &mut v);
                coder.code(mfsk_code(b'\r'), &mut v);
            }
            // fldigi's own flush, plus enough idles that the interleaver
            // (30 symbols) brings the end of the text out.
            for _ in 0..12 {
                coder.code(mfsk_code(0), &mut v);
            }
        }
    }
    v
}

/// fldigi's `sendsymbol` and `sendtone`: phase-continuous tones,
/// (previous + 2 + nibble) mod 18.
fn modulate(nib: &[u8], speed: IfkSpeed, fs: f32, ppm: f64, center: f64, amp: f32) -> Vec<f32> {
    let (s, _, rate) = speed.timing();
    let t_sym = s as f64 / rate as f64 * (1.0 + ppm * 1e-6);
    let spacing = speed.spacing_hz() as f64;
    let lead = fs as usize / 2;
    let tail = 2 * fs as usize;
    let body = (nib.len() as f64 * t_sym * fs as f64) as usize;
    let mut out = vec![0f32; lead + body + tail];
    let mut prev = 0usize;
    let (mut ph, mut k, mut next) = (0f64, 0usize, 0f64);
    let mut f = 0f64;
    for i in 0..body {
        let t = i as f64 / fs as f64;
        if t >= next && k < nib.len() {
            let tone = (prev + 2 + nib[k] as usize) % TONES;
            prev = tone;
            f = center + (tone as f64 + 0.5 - 9.0) * spacing;
            k += 1;
            next += t_sym;
        }
        out[lead + i] = amp * ph.cos() as f32;
        ph = (ph + std::f64::consts::TAU * f / fs as f64) % std::f64::consts::TAU;
    }
    out
}

fn tx(kind: TxKind, speed: IfkSpeed, text: &str, fs: f32, ppm: f64, center: f64, amp: f32) -> Vec<f32> {
    modulate(&nibbles(kind, speed, text), speed, fs, ppm, center, amp)
}

fn text_for(speed: IfkSpeed) -> &'static str {
    if speed == IfkSpeed::Micro { SHORT } else { TEXT }
}

fn joined(events: Vec<IfkEvent>) -> String {
    let mut s = String::new();
    for e in events {
        let IfkEvent::Text { text, .. } = e;
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(&text);
    }
    s
}

fn cer(sent: &str, got: &str) -> f32 {
    let a: String = sent.split_whitespace().collect();
    let b: String = got.split_whitespace().collect();
    levenshtein(&a, &b) as f32 / a.chars().count() as f32
}

fn decode(kind: TxKind, speed: IfkSpeed, x: &[f32], fs: f32) -> String {
    match kind {
        TxKind::Thor => {
            let mut d = ThorDecoder::new(fs, speed, 1500.0);
            chunks(x, |c| d.push(c));
            d.flush();
            joined(d.take())
        }
        TxKind::Domino => {
            let mut d = DominoExDecoder::new(fs, speed, 1500.0);
            chunks(x, |c| d.push(c));
            d.flush();
            joined(d.take())
        }
        TxKind::DominoFec => {
            let mut d = DominoExDecoder::new_fec(fs, speed, 1500.0);
            chunks(x, |c| d.push(c));
            d.flush();
            joined(d.take())
        }
    }
}

// -------------------------------------------------------------------------
// Tables.

#[test]
fn domino_codes_are_uniquely_decodable() {
    use std::collections::HashMap;
    let mut seen: HashMap<usize, usize> = HashMap::new();
    for (i, code) in DOMINO_CODES.iter().enumerate() {
        assert!(code[0] & 8 == 0, "{i}: code starts with a continuation nibble");
        let mut len = 1;
        for &n in &code[1..] {
            if n & 8 == 0 {
                break;
            }
            len += 1;
        }
        for &n in &code[len..] {
            assert_eq!(n, 0, "{i}: a short code is filled with zeros");
        }
        if let Some(&j) = seen.get(&domino_value(code)) {
            // fldigi's table gives these two secondary characters one code.
            assert_eq!((j, i), (379, 381), "{i} repeats {j}");
        }
        seen.insert(domino_value(code), i);
    }
}

#[test]
fn domino_stream_decodes_every_primary_byte() {
    // Every code in a row, the top bit of each nibble alone marking where
    // the next starts: the property fldigi's receiver relies on.
    let mut nibs = Vec::new();
    for b in 0..=255u8 {
        domino_send(b, false, &mut nibs);
    }
    domino_send(b' ', false, &mut nibs);
    let mut got = Vec::new();
    let (mut buf, mut n) = ([0u8; 3], 0usize);
    for &c in &nibs {
        if c & 8 == 0 && n > 0 {
            let mut v = 0usize;
            for &x in &buf[..n] {
                v = (v << 4) | x as usize;
            }
            got.push(domino_decode_table()[v] as i32);
            n = 0;
        }
        buf[n] = c;
        n += 1;
    }
    let want: Vec<i32> = (0..=255).collect();
    assert_eq!(got, want);
}

#[test]
fn thor_codes_are_self_delimiting() {
    // Primary (MFSK) and secondary alike: starts 1, ends 00, no 001 inside,
    // so a stream cuts itself at every 001; and no two the same.
    let mut all: Vec<u32> = Vec::new();
    for c in varicode::VARICODE.iter() {
        assert!(c.starts_with('1') && c.ends_with("00") && !c.contains("001"), "{c}");
        all.push(u32::from_str_radix(c, 2).unwrap());
    }
    for &v in THOR_SECONDARY.iter() {
        let s = format!("{v:b}");
        assert!(s.starts_with('1') && s.ends_with("00") && !s.contains("001"), "{s}");
        assert_eq!(s.len(), 12);
        all.push(v as u32);
    }
    let n = all.len();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), n);
}

#[test]
fn thor_stream_decodes_every_primary_byte() {
    let mut bits = String::new();
    for b in 0..=255u8 {
        bits.push_str(mfsk_code(b));
    }
    bits.push_str(mfsk_code(0));
    let mut shreg = 1u32;
    let mut got = Vec::new();
    for b in bits.bytes().skip(1) {
        shreg = (shreg << 1) | (b == b'1') as u32;
        if shreg & 7 == 1 {
            got.push(thor_decode(shreg >> 1));
            shreg = 1;
        }
    }
    assert_eq!(got.len(), 256);
    for (i, d) in got.iter().enumerate() {
        assert_eq!(*d, Decoded::Primary(i as u8), "{i}");
    }
}

// -------------------------------------------------------------------------
// Copy.

fn check_clean(kind: TxKind, speed: IfkSpeed) {
    let text = text_for(speed);
    let x = tx(kind, speed, text, FS, 0.0, 1500.0, 0.3);
    let got = decode(kind, speed, &x, FS);
    assert!(cer(text, &got) < 0.05, "{}: {got:?}", speed.name());
}

#[test]
fn clean_copy_domino() {
    for s in IfkSpeed::ALL {
        check_clean(TxKind::Domino, s);
    }
}

#[test]
fn clean_copy_domino_fec() {
    for s in [IfkSpeed::S8, IfkSpeed::S16, IfkSpeed::S11] {
        check_clean(TxKind::DominoFec, s);
    }
}

#[test]
fn clean_copy_thor() {
    for s in IfkSpeed::ALL {
        check_clean(TxKind::Thor, s);
    }
}

#[test]
fn offset_and_clock_error() {
    for kind in [TxKind::Domino, TxKind::Thor] {
        for s in [IfkSpeed::S4, IfkSpeed::S8, IfkSpeed::S16, IfkSpeed::S22] {
            let mut rng = Rng::new(5);
            let mut x = tx(kind, s, TEXT, FS, 100.0, 1530.0, 0.3);
            add_noise(&mut x, 0.3, 3.0, FS, &mut rng);
            let got = decode(kind, s, &x, FS);
            assert!(cer(TEXT, &got) < 0.05, "{}: {got:?}", s.name());
        }
    }
}

#[test]
fn other_way_round_at_12_khz() {
    let s = IfkSpeed::S16;
    let mut rng = Rng::new(11);
    let mut x = tx(TxKind::Thor, s, TEXT, 12_000.0, -150.0, 1470.0, 0.3);
    add_noise(&mut x, 0.3, 3.0, 12_000.0, &mut rng);
    let mut d = ThorDecoder::new(12_000.0, s, 1500.0);
    chunks(&x, |c| d.push(c));
    d.flush();
    let st = d.status();
    let got = joined(d.take());
    assert!(cer(TEXT, &got) < 0.05, "{got:?}");
    assert!((st.center_hz - 1470.0).abs() < 5.0, "{st:?}");
}

#[test]
fn auto_finds_the_speed_and_the_band() {
    let mut rng = Rng::new(3);
    let s = IfkSpeed::S11;
    let mut x = tx(TxKind::Thor, s, TEXT, FS, 40.0, 2230.0, 0.3);
    add_noise(&mut x, 0.3, 0.0, FS, &mut rng);
    let mut d = ThorDecoder::new_auto(FS, 1500.0);
    chunks(&x, |c| d.push(c));
    d.flush();
    let st = d.status();
    let ev = d.take();
    assert!(!ev.is_empty());
    for e in &ev {
        let IfkEvent::Text { mode, .. } = e;
        assert_eq!(mode, "THOR11", "{e:?}");
    }
    assert!(cer(TEXT, &joined(ev)) < 0.05);
    assert_eq!(st.variant, "THOR11");
    assert!((st.center_hz - 2230.0).abs() < 8.0, "{st:?}");
}

#[test]
fn dominoex_will_not_print_thor() {
    let s = IfkSpeed::S16;
    let mut rng = Rng::new(9);
    let mut x = tx(TxKind::Thor, s, TEXT, FS, 0.0, 1500.0, 0.3);
    add_noise(&mut x, 0.3, 3.0, FS, &mut rng);
    let got = decode(TxKind::Domino, s, &x, FS);
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn copy_at_minus_5_db_every_speed() {
    for kind in [TxKind::Thor, TxKind::Domino] {
        for s in IfkSpeed::ALL {
            let text = text_for(s);
            let mut rng = Rng::new(21);
            let mut x = tx(kind, s, text, FS, 60.0, 1530.0, 0.3);
            add_noise(&mut x, 0.3, -5.0, FS, &mut rng);
            let got = decode(kind, s, &x, FS);
            let limit = if kind == TxKind::Thor { 0.02 } else { 0.15 };
            assert!(cer(text, &got) <= limit, "{}: {got:?}", s.name());
        }
    }
}

#[test]
fn no_single_character_lines() {
    let mut rng = Rng::new(31);
    let mut x = tx(TxKind::Domino, IfkSpeed::S16, TEXT, FS, 0.0, 1500.0, 0.3);
    add_noise(&mut x, 0.3, 0.0, FS, &mut rng);
    let mut d = DominoExDecoder::new(FS, IfkSpeed::S16, 1500.0);
    chunks(&x, |c| d.push(c));
    d.flush();
    for e in d.take() {
        let IfkEvent::Text { text, .. } = e;
        assert!(text.split_whitespace().collect::<String>().len() >= 2, "{text:?}");
    }
}

/// Worst CER of three seeds at each SNR, 30 Hz off with a 60 ppm clock:
/// `cargo test --release -p scannerd-engine --lib ifk::tests::sweep -- --ignored --nocapture`
#[test]
#[ignore]
fn sweep() {
    for (name, kind) in [("THOR", TxKind::Thor), ("DOMEX", TxKind::Domino)] {
        for s in IfkSpeed::ALL {
            let text = text_for(s);
            let mut line = format!("{name}{}:", s.name());
            let top = if s == IfkSpeed::Micro { -8 } else { -2 };
            for i in 0..8 {
                let snr = (top - 2 * i) as f32;
                let mut worst = 0f32;
                for seed in 1..=3u64 {
                    let mut rng = Rng::new(seed * 73);
                    let mut x = tx(kind, s, text, FS, 60.0, 1530.0, 0.3);
                    add_noise(&mut x, 0.3, snr, FS, &mut rng);
                    worst = worst.max(cer(text, &decode(kind, s, &x, FS)).min(1.0));
                }
                line.push_str(&format!(" {snr:+.0}:{worst:.2}"));
            }
            eprintln!("{line}");
        }
    }
}

// -------------------------------------------------------------------------
// False prints.

fn assert_no_print(x: &[f32], what: &str) {
    let mut d = ThorDecoder::new_auto(FS, 1500.0);
    chunks(x, |c| d.push(c));
    d.flush();
    let got = joined(d.take());
    assert!(got.is_empty(), "Thor on {what}: {got:?}");
    let mut d = DominoExDecoder::new_auto(FS, 1500.0);
    chunks(x, |c| d.push(c));
    d.flush();
    let got = joined(d.take());
    assert!(got.is_empty(), "DominoEX on {what}: {got:?}");
}

fn noise(seconds: f32, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut x = vec![0f32; (seconds * FS) as usize];
    add_noise(&mut x, 0.3, 0.0, FS, &mut rng);
    x
}

/// FT8-like stations: 8-FSK, 6.25 baud, 6.25 Hz apart, 12.64 s out of every
/// 15 s, all in step.
fn add_ft8_like(x: &mut [f32], rng: &mut Rng, stations: usize, amp: f32) {
    let tau = std::f64::consts::TAU;
    let total = x.len();
    for _ in 0..stations {
        let f0 = 600.0 + 1700.0 * rng.uniform();
        let mut ph = rng.uniform() * tau;
        let mut cycle = 0usize;
        loop {
            let start = (cycle as f64 * 15.0 * FS as f64) as usize;
            if start >= total {
                break;
            }
            cycle += 1;
            let len = (12.64 * FS as f64) as usize;
            let (mut k, mut tone) = (usize::MAX, 0usize);
            for j in 0..len.min(total - start) {
                let sym = (j as f64 / (0.16 * FS as f64)) as usize;
                if sym != k {
                    k = sym;
                    tone = (rng.uniform() * 8.0) as usize;
                }
                let f = f0 + tone as f64 * 6.25;
                x[start + j] += amp * ph.cos() as f32;
                ph = (ph + tau * f / FS as f64) % tau;
            }
        }
    }
}

#[test]
fn noise_prints_nothing() {
    assert_no_print(&noise(180.0, 1), "noise");
}

#[test]
fn carriers_and_ft8_print_nothing() {
    let mut x = noise(150.0, 2);
    let mut rng = Rng::new(77);
    let tau = std::f64::consts::TAU;
    for (f, a) in [(910.0, 0.4), (1500.0, 0.7), (1837.5, 0.25), (2100.0, 0.5)] {
        let inc = tau * f / FS as f64;
        for (i, v) in x.iter_mut().enumerate() {
            *v += a * (inc * i as f64).cos() as f32;
        }
    }
    add_ft8_like(&mut x, &mut rng, 8, 0.25);
    assert_no_print(&x, "carriers and FT8");
}

#[test]
fn ft8_alone_prints_nothing() {
    let mut x = noise(150.0, 3);
    let mut rng = Rng::new(4);
    add_ft8_like(&mut x, &mut rng, 6, 0.5);
    assert_no_print(&x, "FT8");
}

/// Long false-print hunt, with what it found on stderr:
/// `cargo test --release -p scannerd-engine --lib ifk::tests::long_noise -- --ignored --nocapture`
#[test]
#[ignore]
fn long_noise() {
    let minutes = 30.0;
    let x = noise(minutes * 60.0, 12345);
    let t0 = std::time::Instant::now();
    let mut d = ThorDecoder::new_auto(FS, 1500.0);
    chunks(&x, |c| d.push(c));
    d.flush();
    let got = joined(d.take());
    eprintln!("Thor auto: {minutes} min noise in {:.1} s, printed {:?}", t0.elapsed().as_secs_f32(), got);
    let t0 = std::time::Instant::now();
    let mut d = DominoExDecoder::new_auto(FS, 1500.0);
    chunks(&x, |c| d.push(c));
    d.flush();
    let got = joined(d.take());
    eprintln!("DominoEX auto: {minutes} min noise in {:.1} s, printed {:?}", t0.elapsed().as_secs_f32(), got);
}

/// Half an hour of carriers and FT8-like stations at several strengths.
#[test]
#[ignore]
fn long_interference() {
    let tau = std::f64::consts::TAU;
    for (seed, amp) in [(1u64, 0.1f32), (2, 0.3), (3, 1.0)] {
        let mut rng = Rng::new(seed);
        let mut x = noise(600.0, seed + 40);
        for (f, a) in [(830.0, 0.2), (1200.0, 0.6), (1701.0, 0.3)] {
            let inc = tau * f / FS as f64;
            for (i, v) in x.iter_mut().enumerate() {
                *v += a * (inc * i as f64).cos() as f32;
            }
        }
        add_ft8_like(&mut x, &mut rng, 12, amp);
        let mut d = ThorDecoder::new_auto(FS, 1500.0);
        chunks(&x, |c| d.push(c));
        d.flush();
        let got = joined(d.take());
        eprintln!("Thor FT8 amp {amp}: printed {:?}", got);
        let mut d = DominoExDecoder::new_auto(FS, 1500.0);
        chunks(&x, |c| d.push(c));
        d.flush();
        let got = joined(d.take());
        eprintln!("DominoEX FT8 amp {amp}: printed {:?}", got);
    }
}
