use super::*;
use crate::rtty::testsig::{Rng, add_noise, chunks, levenshtein, rayleigh};

/// An MFSK transmitter laid out as fldigi's: the encoder, the interleaver,
/// four coded bits to a symbol (first bit highest), Gray coded onto 16
/// phase-continuous tones, and the picture sequence after a `Pic:` header.
/// Durations are counted at 8 kHz and stretched by the transmitter's
/// clock error.
struct Tx {
    mode: MfskMode,
    enc: Encoder,
    inlv: Interleaver<u8>,
    shreg: u32,
    state: usize,
    /// (frequency offset from the center in Hz, duration in 8 kHz samples)
    segs: Vec<(f64, f64)>,
}

impl Tx {
    fn new(mode: MfskMode) -> Self {
        Self {
            mode,
            enc: Encoder::default(),
            inlv: Interleaver::new(BITS, mode.depth(), true, 0),
            shreg: 0,
            state: 0,
            segs: Vec::new(),
        }
    }

    fn spacing(&self) -> f64 {
        RATE / self.mode.symbol_len() as f64
    }

    fn bit(&mut self, b: bool) {
        let (c0, c1) = self.enc.push(b);
        for c in [c0, c1] {
            self.shreg = (self.shreg << 1) | c as u32;
            self.state += 1;
            if self.state == BITS {
                let mut s: Vec<u8> = (0..BITS)
                    .map(|i| ((self.shreg >> (BITS - 1 - i)) & 1) as u8)
                    .collect();
                self.inlv.symbols(&mut s);
                let v = s.iter().fold(0usize, |a, b| (a << 1) | *b as usize);
                let tone = v ^ (v >> 1);
                let f = (tone as f64 - 7.5) * self.spacing();
                self.segs.push((f, self.mode.symbol_len() as f64));
                self.shreg = 0;
                self.state = 0;
            }
        }
    }

    fn char(&mut self, c: u8) {
        for b in varicode::VARICODE[c as usize].bytes() {
            self.bit(b == b'1');
        }
    }

    fn flush(&mut self) {
        self.bit(true);
        for _ in 0..self.mode.flush_bits() {
            self.bit(false);
        }
        // A part-filled symbol is dropped.
        self.shreg = 0;
        self.state = 0;
    }

    fn text(&mut self, t: &str) {
        for c in t.bytes() {
            self.char(c);
        }
    }

    /// The preamble fldigi sends: a third of the flush length in zeros.
    fn preamble(&mut self) {
        for _ in 0..self.mode.flush_bits() / 3 {
            self.bit(false);
        }
        self.text("\r\x02\r");
    }

    fn end(&mut self) {
        self.text("\r\x04\r");
        self.flush();
    }

    /// `Pic:` header, flush, lead-in, pixels (row by row, and red, green,
    /// blue within a row for color), flush.
    fn picture(&mut self, w: u32, h: u32, color: bool, px: &[u8]) {
        self.text(&format!("Pic:{}x{}{};", w, h, if color { "C" } else { "" }));
        self.flush();
        let bw = self.mode.bandwidth_hz() as f64;
        self.segs.push((-bw / 2.0, PROLOGUE));
        let w = w as usize;
        for row in 0..h as usize {
            for c in 0..if color { 3 } else { 1 } {
                for col in 0..w {
                    let v = if color {
                        px[(row * w + col) * 3 + c]
                    } else {
                        px[row * w + col]
                    };
                    self.segs.push((bw * (v as f64 - 128.0) / 256.0, 8.0));
                }
            }
        }
        self.flush();
    }

    /// The audio at `fs`, the center at `center`, the clock off by `ppm`,
    /// with a second of silence either side. Mean power that of a sine of
    /// `amp`.
    fn audio(&self, fs: f32, center: f64, ppm: f64, amp: f32) -> Vec<f32> {
        let scale = (1.0 + ppm * 1e-6) / RATE;
        let lead = fs as usize;
        let total_s: f64 = self.segs.iter().map(|s| s.1).sum::<f64>() * scale;
        let n = lead + (total_s * fs as f64) as usize + lead;
        let mut out = vec![0.0f32; n];
        let mut ph = 0.0f64;
        let mut seg = 0;
        let mut seg_end = self.segs[0].1 * scale;
        for (i, v) in out.iter_mut().enumerate().skip(lead) {
            let t = (i - lead) as f64 / fs as f64;
            while t >= seg_end && seg + 1 < self.segs.len() {
                seg += 1;
                seg_end += self.segs[seg].1 * scale;
            }
            if t >= seg_end {
                break;
            }
            *v = amp * ph.cos() as f32;
            ph = (ph + TAU * (center + self.segs[seg].0) / fs as f64) % TAU;
        }
        out
    }
}

const TEXT: &str =
    "CQ CQ CQ de W1AW W1AW pse k. The quick brown fox jumps over the lazy dog 0123456789.";

fn send_text(mode: MfskMode, text: &str) -> Tx {
    let mut tx = Tx::new(mode);
    tx.preamble();
    tx.text(text);
    tx.end();
    tx
}

fn run(dec: &mut MfskDecoder, x: &[f32]) -> (String, Vec<MfskEvent>) {
    let mut all = Vec::new();
    chunks(x, |c| {
        dec.push(c);
        all.extend(dec.take());
    });
    dec.flush();
    all.extend(dec.take());
    let mut s = String::new();
    for e in &all {
        if let MfskEvent::Text { text, .. } = e {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(text);
        }
    }
    (s, all)
}

/// Character error rate, blanks aside (lines are cut at 80 characters).
fn cer(sent: &str, got: &str) -> f32 {
    let a: String = sent.split_whitespace().collect();
    let b: String = got.split_whitespace().collect();
    levenshtein(&a, &b) as f32 / a.chars().count() as f32
}

#[test]
fn clean_copy_each_mode_found_by_search() {
    let fs = 47_904.0;
    for (mode, center) in [
        (MfskMode::Mfsk16, 1000.0),
        (MfskMode::Mfsk32, 1500.0),
        (MfskMode::Mfsk64, 1700.0),
    ] {
        let x = send_text(mode, TEXT).audio(fs, center, 0.0, 0.3);
        let mut dec = MfskDecoder::new(
            fs,
            MfskConfig {
                mode,
                center_hz: None,
            },
        );
        let (got, _) = run(&mut dec, &x);
        assert!(cer(TEXT, &got) < 0.02, "{}: {got:?}", mode.name());
        let st = dec.status();
        assert!(
            st.center_hz
                .is_some_and(|c| (c as f64 - center).abs() < 2.0),
            "{st:?}"
        );
    }
}

#[test]
fn offset_and_clock_error() {
    let fs = 47_904.0;
    for (off, ppm) in [(50.0, 100.0), (-50.0, -100.0)] {
        let mode = MfskMode::Mfsk32;
        let mut rng = Rng::new(3);
        let mut x = send_text(mode, &TEXT.repeat(2)).audio(fs, 1500.0 + off, ppm, 0.3);
        add_noise(&mut x, 0.3, 0.0, fs, &mut rng);
        let mut dec = MfskDecoder::new(
            fs,
            MfskConfig {
                mode,
                center_hz: Some(1500.0),
            },
        );
        let (got, _) = run(&mut dec, &x);
        assert!(
            cer(&TEXT.repeat(2), &got) < 0.02,
            "{off} Hz {ppm} ppm: {got:?}"
        );
        let st = dec.status();
        assert!(
            st.center_hz
                .is_some_and(|c| (c as f64 - 1500.0 - off).abs() < 1.0),
            "{st:?}"
        );
        let clock = st.clock_ppm.expect("clock measured");
        assert!((clock as f64 - ppm).abs() < 40.0, "{ppm}: {clock}");
    }
}

fn sweep(mode: MfskMode, fading: bool, snrs: &[f32], text: &str) -> Vec<(f32, f32)> {
    let fs = 47_904.0;
    snrs.iter()
        .map(|&snr| {
            let mut worst = 0.0f32;
            for seed in 1..=2u64 {
                let mut rng = Rng::new(seed * 77);
                let mut x = send_text(mode, text).audio(fs, 1234.0, 40.0, 0.3);
                if fading {
                    let g = rayleigh(x.len(), fs, 1.0, &mut rng);
                    for (v, g) in x.iter_mut().zip(g) {
                        *v *= g.norm();
                    }
                }
                add_noise(&mut x, 0.3, snr, fs, &mut rng);
                let mut dec = MfskDecoder::new(
                    fs,
                    MfskConfig {
                        mode,
                        center_hz: None,
                    },
                );
                let (got, _) = run(&mut dec, &x);
                worst = worst.max(cer(text, &got));
            }
            (snr, worst)
        })
        .collect()
}

/// Where each mode copies with under 2% character errors, AWGN and flat
/// Rayleigh fading (1 Hz). `--ignored --nocapture` to see it.
#[test]
#[ignore]
fn sensitivity_sweep() {
    let text = TEXT.repeat(2);
    for mode in [MfskMode::Mfsk16, MfskMode::Mfsk32, MfskMode::Mfsk64] {
        let r = sweep(
            mode,
            false,
            &[-6.0, -8.0, -10.0, -12.0, -13.0, -14.0, -15.0],
            &text,
        );
        eprintln!("{} AWGN: {r:?}", mode.name());
        let r = sweep(mode, true, &[0.0, -2.0, -4.0, -6.0, -8.0], &text);
        eprintln!("{} fading: {r:?}", mode.name());
    }
}

#[test]
fn snr_reading() {
    let fs = 47_904.0;
    for mode in [MfskMode::Mfsk16, MfskMode::Mfsk64] {
        for snr in [-8.0f32, 0.0, 10.0] {
            let mut rng = Rng::new(21);
            let mut x = send_text(mode, TEXT).audio(fs, 1500.0, 0.0, 0.3);
            // Stop before the end, while the squelch is open.
            x.truncate(x.len() * 4 / 5);
            add_noise(&mut x, 0.3, snr, fs, &mut rng);
            let mut dec = MfskDecoder::new(
                fs,
                MfskConfig {
                    mode,
                    center_hz: Some(1500.0),
                },
            );
            chunks(&x, |c| dec.push(c));
            let got = dec.status().snr_db.expect("snr");
            eprintln!("{} at {snr} dB reads {got:.1}", mode.name());
            assert!(
                (got - snr).abs() < 2.0,
                "{} at {snr} dB reads {got}",
                mode.name()
            );
        }
    }
}

#[test]
fn copies_weak() {
    let r = sweep(MfskMode::Mfsk32, false, &[-9.0], TEXT);
    assert!(r[0].1 < 0.02, "{r:?}");
}

#[test]
fn noise_prints_nothing() {
    let fs = 47_904.0;
    let mut rng = Rng::new(5);
    let x: Vec<f32> = (0..(90.0 * fs) as usize)
        .map(|_| 0.3 * rng.gauss())
        .collect();
    for mode in [MfskMode::Mfsk16, MfskMode::Mfsk64] {
        let mut dec = MfskDecoder::new(
            fs,
            MfskConfig {
                mode,
                center_hz: Some(1500.0),
            },
        );
        let (got, ev) = run(&mut dec, &x);
        assert!(got.is_empty() && ev.is_empty(), "{}: {ev:?}", mode.name());
        assert!(!dec.status().locked);
    }
}

/// A test card: gradients and bars, so a slant or a shift shows.
fn card(w: u32, h: u32) -> Vec<u8> {
    let mut px = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let r = (255 * x / (w - 1)) as u8;
            let g = (255 * y / (h - 1)) as u8;
            let b = if (x / 4) % 2 == 0 { 230 } else { 20 };
            px.extend_from_slice(&[r, g, b]);
        }
    }
    px
}

fn picture_error(mode: MfskMode, ppm: f64, snr: f32) -> (f32, f32) {
    let fs = 47_904.0;
    let (w, h) = (48, 24);
    let img = card(w, h);
    let mut tx = Tx::new(mode);
    tx.preamble();
    // Radiogram sends minutes of text before a picture; a few lines here
    // give the symbol clock something to learn the transmitter's clock on.
    tx.text(&TEXT.repeat(2));
    tx.text("\rRadiogram test picture follows\r");
    tx.picture(w, h, true, &img);
    tx.text("\rafter the picture 73\r");
    tx.end();
    let mut rng = Rng::new(8);
    let mut x = tx.audio(fs, 1500.0, ppm, 0.3);
    add_noise(&mut x, 0.3, snr, fs, &mut rng);
    let mut dec = MfskDecoder::new(
        fs,
        MfskConfig {
            mode,
            center_hz: None,
        },
    );
    let (text, ev) = run(&mut dec, &x);
    assert!(text.contains("Pic:48x24C;"), "{text:?}");
    assert!(text.contains("after the picture 73"), "{text:?}");
    let pic = ev
        .iter()
        .find_map(|e| match e {
            MfskEvent::Picture {
                width,
                height,
                color,
                pixels,
                complete,
            } => Some((*width, *height, *color, pixels.clone(), *complete)),
            _ => None,
        })
        .expect("picture");
    assert_eq!((pic.0, pic.1, pic.2, pic.4), (w, h, true, true));
    let px = pic.3;
    // Mean error, and the error of the blue bars' edges alone (a shift or
    // slant shows there first).
    let mean = px
        .iter()
        .zip(&img)
        .map(|(a, b)| (*a as f32 - *b as f32).abs())
        .sum::<f32>()
        / px.len() as f32;
    let blue = px
        .iter()
        .zip(&img)
        .skip(2)
        .step_by(3)
        .map(|(a, b)| (*a as f32 - *b as f32).abs())
        .sum::<f32>()
        / (w * h) as f32;
    (mean, blue)
}

#[test]
fn color_picture_mfsk32() {
    let (mean, blue) = picture_error(MfskMode::Mfsk32, 0.0, 15.0);
    assert!(mean < 12.0 && blue < 20.0, "{mean} {blue}");
}

#[test]
fn color_picture_mfsk64_with_clock_error() {
    for ppm in [100.0, -100.0] {
        let (mean, blue) = picture_error(MfskMode::Mfsk64, ppm, 15.0);
        assert!(mean < 12.0 && blue < 20.0, "{ppm}: {mean} {blue}");
    }
}

#[test]
fn header_parses() {
    assert_eq!(parse_header("xx Pic:200x150C;"), Some((200, 150, true, 8)));
    assert_eq!(parse_header("Pic:320x240;"), Some((320, 240, false, 8)));
    assert_eq!(parse_header("Pic:100x80Cp4;"), Some((100, 80, true, 4)));
    assert_eq!(parse_header("Pic:100x80p2;"), Some((100, 80, false, 2)));
    assert_eq!(parse_header("Pic:0x80;"), None);
    assert_eq!(parse_header("Pic:12;"), None);
}

#[test]
fn base64_matches() {
    assert_eq!(base64(b"Man"), "TWFu");
    assert_eq!(base64(b"Ma"), "TWE=");
    assert_eq!(base64(b"M"), "TQ==");
}

/// Picture error (mean, and on the blue bars' edges) against SNR, with
/// the transmitter's clock 100 ppm slow. `--ignored --nocapture`.
#[test]
#[ignore]
fn picture_quality() {
    for snr in [30.0, 20.0, 15.0, 10.0] {
        for mode in [MfskMode::Mfsk32, MfskMode::Mfsk64] {
            let r = picture_error(mode, -100.0, snr);
            eprintln!("{} {snr} dB: {r:?}", mode.name());
        }
    }
}

/// Seconds of audio decoded per second of work, each mode. `--ignored
/// --nocapture`.
#[test]
#[ignore]
fn speed() {
    let fs = 47_904.0;
    for mode in [MfskMode::Mfsk16, MfskMode::Mfsk32, MfskMode::Mfsk64] {
        let mut rng = Rng::new(1);
        let mut x = send_text(mode, &TEXT.repeat(3)).audio(fs, 1500.0, 0.0, 0.3);
        add_noise(&mut x, 0.3, 0.0, fs, &mut rng);
        let mut dec = MfskDecoder::new(
            fs,
            MfskConfig {
                mode,
                center_hz: None,
            },
        );
        let t = std::time::Instant::now();
        chunks(&x, |c| dec.push(c));
        let secs = x.len() as f64 / fs as f64;
        eprintln!(
            "{}: {:.0}x real time",
            mode.name(),
            secs / t.elapsed().as_secs_f64()
        );
    }
}

/// Character errors under static crashes (two a second, 20 to 35 dB over
/// the signal), with the blanker and, under `USDR_NO_BLANK`, without:
/// `cargo test --release -p scannerd-engine mfsk::tests::impulse_sweep -- --ignored --nocapture`
#[test]
#[ignore]
fn impulse_sweep() {
    let fs = 47_904.0;
    let text = TEXT.repeat(2);
    for mode in [MfskMode::Mfsk16, MfskMode::Mfsk32] {
        for snr in [0.0f32, -4.0, -8.0, -10.0] {
            for rate in [0.0f32, 2.0, 5.0] {
                let mut total = 0.0;
                for seed in 1..=3u64 {
                    let mut rng = Rng::new(seed * 131);
                    let mut x = send_text(mode, &text).audio(fs, 1234.0, 20.0, 0.3);
                    add_noise(&mut x, 0.3, snr, fs, &mut rng);
                    if rate > 0.0 {
                        crate::rtty::testsig::add_impulses(&mut x, 0.3, fs, rate, &mut rng);
                    }
                    let mut dec = MfskDecoder::new(fs, MfskConfig { mode, center_hz: None });
                    let (got, _) = run(&mut dec, &x);
                    total += cer(&text, &got).min(1.0);
                }
                eprintln!("{} {snr:+} dB, {rate} crashes/s: CER {:.3}", mode.name(), total / 3.0);
            }
        }
    }
}
