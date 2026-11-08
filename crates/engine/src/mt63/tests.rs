//! Tests of the MT63 receiver against a transmitter built from fldigi's
//! transmit path, in the same fixed-point-free arithmetic.

use super::rx::{CARR_SEPAR, CARRIERS, SEPAR, WINDOW, tx_setup, walsh};
use super::tables::{LONG_INTERLEAVE, SHORT_INTERLEAVE, SYMBOL_SHAPE};
use super::*;
use crate::rtty::testsig::{Rng, add_noise, chunks, levenshtein};
use num_complex::Complex64;
use rustfft::FftPlanner;
use std::f64::consts::TAU;

const FS: f32 = 47_904.0;

/// `dspWalshInvTrans`.
fn walsh_inv(d: &mut [f32]) {
    let len = d.len();
    let mut step = len / 2;
    while step > 0 {
        let mut ptr = 0;
        while ptr < len {
            for p in ptr..ptr + step {
                let (a, b) = (d[p], d[p + step]);
                d[p] = a - b;
                d[p + step] = a + b;
            }
            ptr += 2 * step;
        }
        step /= 2;
    }
}

/// `MT63tx` as a closed form: the baseband is generated directly (each
/// carrier a windowed complex exponential at its FFT bin), which is what
/// fldigi's inverse FFT and overlap-add compute.
struct Tx {
    bw: u32,
    first: i64,
    carr_phase: [usize; CARRIERS],
    phase_corr: [usize; CARRIERS],
    pipe: Vec<bool>,
    size: usize,
    ptr: usize,
    patt: [usize; CARRIERS],
    interleave: usize,
    win: Vec<Complex64>,
    over: Vec<Complex64>,
    /// `[I,Q]` output at 8000 / rate Hz.
    out: Vec<Complex64>,
    fft_plan: std::sync::Arc<dyn rustfft::Fft<f64>>,
    rng: Rng,
}

impl Tx {
    fn new(bw: u32, long: bool, center: f32, seed: u64) -> Self {
        let (_ti, _tq, rate) = tx_setup(bw, center);
        let first = ((center as f64 - bw as f64 / 2.0) * WINDOW as f64 * rate as f64 / 8000.0 + 0.5)
            .floor() as i64;
        let mask = WINDOW - 1;
        let mut carr_phase = [0usize; CARRIERS];
        let (mut step, mut p) = (0usize, 0usize);
        for c in carr_phase.iter_mut() {
            *c = p;
            step += 1;
            p = (p + step) & mask;
        }
        let incr = (SEPAR * CARR_SEPAR) & mask;
        let mut phase_corr = [0usize; CARRIERS];
        let mut p = ((SEPAR as i64 * first) & mask as i64) as usize;
        for c in phase_corr.iter_mut() {
            *c = p;
            p = (p + incr) & mask;
        }
        let (interleave, pattern) = if long {
            (64, &LONG_INTERLEAVE)
        } else {
            (32, &SHORT_INTERLEAVE)
        };
        let size = interleave * CARRIERS;
        let mut rng = Rng::new(seed);
        let pipe: Vec<bool> = (0..size).map(|_| rng.uniform() < 0.5).collect();
        let mut patt = [0usize; CARRIERS];
        let mut p = 0;
        for i in 0..CARRIERS {
            patt[i] = p * CARRIERS;
            p += pattern[i];
            if p >= interleave {
                p -= interleave;
            }
        }
        let z = Complex64::new(0.0, 0.0);
        Self {
            bw,
            first,
            carr_phase,
            phase_corr,
            pipe,
            size,
            ptr: 0,
            patt,
            interleave,
            win: vec![z; 2 * WINDOW],
            over: vec![z; 2 * WINDOW],
            out: Vec::new(),
            fft_plan: FftPlanner::new().plan_fft_inverse(WINDOW),
            rng,
        }
    }

    fn interleave(&self) -> usize {
        self.interleave
    }

    fn encode(&mut self, code: u8) -> [bool; CARRIERS] {
        let code = (code & 0x7f) as usize;
        let mut walsh = [0f32; CARRIERS];
        if code < CARRIERS {
            walsh[code] = 1.0;
        } else {
            walsh[code - CARRIERS] = -1.0;
        }
        walsh_inv(&mut walsh);
        for i in 0..CARRIERS {
            self.pipe[self.ptr + i] = walsh[i] < 0.0;
        }
        let mut out = [false; CARRIERS];
        for i in 0..CARRIERS {
            let mut k = self.ptr + self.patt[i];
            if k >= self.size {
                k -= self.size;
            }
            out[i] = self.pipe[k + i];
        }
        self.ptr += CARRIERS;
        if self.ptr >= self.size {
            self.ptr -= self.size;
        }
        out
    }

    /// A symbol from carrier bits: bit set is no flip.
    fn send(&mut self, bits: &[bool; CARRIERS]) {
        let mask = WINDOW - 1;
        for i in 0..CARRIERS {
            let flip = if bits[i] { 0 } else { WINDOW / 2 };
            self.carr_phase[i] = (self.carr_phase[i] + self.phase_corr[i] + flip) & mask;
        }
        self.process_vect();
    }

    fn send_char(&mut self, code: u8) {
        let bits = self.encode(code);
        self.send(&bits);
    }

    fn send_jam(&mut self) {
        let mask = WINDOW - 1;
        for i in 0..CARRIERS {
            let turn = if self.rng.uniform() < 0.5 {
                WINDOW / 4
            } else {
                3 * WINDOW / 4
            };
            self.carr_phase[i] = (self.carr_phase[i] + self.phase_corr[i] + turn) & mask;
        }
        self.process_vect();
    }

    fn process_vect(&mut self) {
        let z = Complex64::new(0.0, 0.0);
        self.win.iter_mut().for_each(|v| *v = z);
        let mask = (WINDOW - 1) as i64;
        let amp = 4.0 / CARRIERS as f64;
        let mut c = self.first;
        for i in 0..CARRIERS {
            let ph = TAU * self.carr_phase[i] as f64 / WINDOW as f64;
            let bin = (c & mask) as usize + WINDOW * (i & 1);
            self.win[bin] = Complex64::new(amp * ph.cos(), amp * ph.sin());
            c += CARR_SEPAR as i64;
        }
        let (a, b) = self.win.split_at_mut(WINDOW);
        self.fft_plan.process(a);
        self.fft_plan.process(b);
        // Overlap-add of the two halves, `SEPAR / 2` apart.
        let dist = SEPAR / 2;
        for half in 0..2 {
            for i in 0..WINDOW {
                self.over[i] += self.win[half * WINDOW + i] * SYMBOL_SHAPE[i] as f64;
            }
            let (head, rest) = self.over.split_at(dist);
            self.out.extend_from_slice(head);
            let mut next = rest.to_vec();
            next.resize(2 * WINDOW, z);
            self.over = next;
        }
    }
}

/// The transmission as audio: the opening interleave of nulls, the text, the
/// closing nulls and jam, a `lead_s` silence before and `tail_s` after,
/// shifted in frequency by `offset_hz`, clock error `ppm`, mean power that
/// of a sine of `amp`. Mirrors fldigi's `mt63::tx_process`.
fn mt63_tx(
    text: &[u8],
    cfg: Mt63Config,
    offset_hz: f64,
    ppm: f64,
    amp: f32,
    fs: f32,
    seed: u64,
) -> Vec<f32> {
    let mut tx = Tx::new(cfg.bandwidth_hz, cfg.long, cfg.center_hz, seed);
    for _ in 0..tx.interleave() {
        tx.send_char(0);
    }
    for &c in text {
        if c > 127 && cfg.eight_bit {
            tx.send_char(127);
            tx.send_char(c & 127);
        } else {
            tx.send_char(c);
        }
    }
    for _ in 0..tx.interleave() {
        tx.send_char(0);
    }
    tx.send_jam();

    let (ti, tq, rate) = tx_setup(tx.bw, cfg.center_hz);
    let fc = 8000.0 / rate as f64;
    // Frequency offset at baseband, as a real radio's offset appears.
    let base: Vec<Complex64> = tx
        .out
        .iter()
        .enumerate()
        .map(|(n, z)| {
            let t = n as f64 / fc;
            z * Complex64::from_polar(1.0, TAU * offset_hz * t)
        })
        .collect();
    // The quadrature combiner of `dspQuadrComb`.
    let mut out8 = vec![0f32; base.len() * rate + ti.len()];
    for (i, z) in base.iter().enumerate() {
        let at = i * rate;
        for t in 0..ti.len() {
            out8[at + t] += (z.re as f32) * ti[t] + (z.im as f32) * tq[t];
        }
    }
    // Normalize the keyed part.
    let p = out8.iter().map(|v| v * v).sum::<f32>() / out8.len() as f32;
    let g = (0.5 * amp * amp / p).sqrt();
    out8.iter_mut().for_each(|v| *v *= g);
    let mut lead = vec![0f32; (RATE * 0.5) as usize];
    lead.extend_from_slice(&out8);
    lead.resize(lead.len() + (RATE * 16.0) as usize, 0.0);

    let fs_out = fs * (1.0 + ppm * 1e-6) as f32;
    let mut r = Resampler::new(RATE, fs_out, 3000.0, 3800.0);
    let mut y = Vec::with_capacity(lead.len() * 7);
    r.process(&lead, &mut y);
    y
}

const TEXT: &str =
    "CQ CQ CQ DE W1AW W1AW PSE K. THE QUICK BROWN FOX JUMPS OVER THE LAZY DOG 0123456789 \n";

fn run(dec: &mut Mt63Decoder, x: &[f32]) -> String {
    chunks(x, |c| dec.push(c));
    dec.flush();
    let mut s = String::new();
    for e in dec.take() {
        let Mt63Event::Text { text, .. } = e;
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

fn modes() -> Vec<Mt63Config> {
    let mut v = Vec::new();
    for bw in [500, 1000, 2000] {
        for long in [true, false] {
            v.push(Mt63Config::new(bw, long, 1500.0));
        }
    }
    v
}

#[test]
fn walsh_round_trip() {
    for ch in 0..128u8 {
        let mut d = [0f32; CARRIERS];
        if (ch as usize) < CARRIERS {
            d[ch as usize] = 1.0;
        } else {
            d[ch as usize - CARRIERS] = -1.0;
        }
        walsh_inv(&mut d);
        walsh(&mut d);
        let (at, v) = d
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap();
        // The transform pair recovers the sign and the row exactly.
        assert_eq!(at as u8 + if *v < 0.0 { 0 } else { 64 }, ch ^ 64);
        assert!((v.abs() - 64.0).abs() < 1e-3);
    }
}

#[test]
fn tables_are_as_generated() {
    assert_eq!(SYMBOL_SHAPE.len(), 512);
    assert_eq!(SHORT_INTERLEAVE[0], 4);
    assert_eq!(LONG_INTERLEAVE[62], 63);
    assert_eq!(LONG_INTERLEAVE[63], 0);
    assert!(SYMBOL_SHAPE[256] > 0.5, "{}", SYMBOL_SHAPE[256]);
}

#[test]
fn clean_copy_every_mode() {
    for cfg in modes() {
        // "Clean": 40 dB over noise, because digital silence is not a
        // thing a radio's audio ever is.
        let mut rng = Rng::new(7);
        let mut x = mt63_tx(TEXT.as_bytes(), cfg, 0.0, 0.0, 0.3, FS, 7);
        add_noise(&mut x, 0.3, 40.0, FS, &mut rng);
        let mut dec = Mt63Decoder::new(FS, cfg);
        let got = run(&mut dec, &x);
        eprintln!("{}: {got:?}", cfg.label());
        assert!(cer(TEXT, &got) < 0.02, "{}: {got:?}", cfg.label());
    }
}

#[test]
fn clean_copy_at_12_khz() {
    let cfg = Mt63Config::new(1000, true, 1500.0);
    let x = mt63_tx(TEXT.as_bytes(), cfg, 0.0, 0.0, 0.3, 12_000.0, 9);
    let mut dec = Mt63Decoder::new(12_000.0, cfg);
    let got = run(&mut dec, &x);
    assert!(cer(TEXT, &got) < 0.02, "{got:?}");
}

#[test]
fn offset_and_clock_error() {
    for cfg in modes() {
        for (off, ppm) in [(30.0, 100.0), (-30.0, -100.0)] {
            let mut rng = Rng::new(11);
            let mut x = mt63_tx(TEXT.as_bytes(), cfg, off, ppm, 0.3, FS, 5);
            add_noise(&mut x, 0.3, 10.0, FS, &mut rng);
            let mut dec = Mt63Decoder::new(FS, cfg);
            let got = run(&mut dec, &x);
            let st = dec.status();
            eprintln!(
                "{} {off} Hz {ppm} ppm: {got:?} center {}",
                cfg.label(),
                st.center_hz
            );
            assert!(cer(TEXT, &got) < 0.02, "{}: {got:?}", cfg.label());
            assert!((st.center_hz - 1500.0 - off as f32).abs() < 4.0, "{st:?}");
        }
    }
}

/// Per-symbol diagnostics of one setting. `--ignored --nocapture`.
#[test]
#[ignore]
fn trace_one() {
    let v = |k: &str, d: f64| {
        std::env::var(k)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(d)
    };
    let cfg = Mt63Config::new(v("BW", 500.0) as u32, v("LONG", 1.0) > 0.5, 1500.0);
    let mut rng = Rng::new(11);
    let mut x = mt63_tx(
        TEXT.as_bytes(),
        cfg,
        v("OFF", -30.0),
        v("PPM", -100.0),
        0.3,
        FS,
        5,
    );
    if v("SNR", 10.0) < 99.0 {
        add_noise(&mut x, 0.3, v("SNR", 10.0) as f32, FS, &mut rng);
    }
    let mut dec = Mt63Decoder::new(FS, cfg);
    let got = run(&mut dec, &x);
    eprintln!("{got:?}");
    for (i, (d, locked, conf, f, dev)) in dec.chains[0].lanes[0].trace.iter().enumerate() {
        if i % v("EVERY", 8.0) as usize == 0 {
            eprintln!(
                "{i:4} code {:3} raw {:5.2} s {:5.2} ofs {:3} lock {} conf {conf:.3} f {f:6.2} dev {dev:.3}",
                d.code, d.raw, d.smooth, d.offset, *locked as u8
            );
        }
    }
}

/// Settings tried across the signal: `(off Hz, ppm)` of a realistic radio.
const IMPAIR: (f64, f64) = (30.0, 60.0);

/// Worst character error rate over a few seeds. `--ignored --nocapture`.
fn sweep(cfg: Mt63Config, snrs: &[f32]) -> Vec<(f32, f32)> {
    let show = std::env::var("MT63_SHOW").is_ok();
    let long = TEXT.repeat(2);
    snrs.iter()
        .map(|&snr| {
            let mut worst = 0.0f32;
            for seed in 1..=3u64 {
                let mut rng = Rng::new(seed * 101);
                let mut x = mt63_tx(long.as_bytes(), cfg, IMPAIR.0, IMPAIR.1, 0.3, FS, seed);
                add_noise(&mut x, 0.3, snr, FS, &mut rng);
                let mut dec = Mt63Decoder::new(FS, cfg);
                let got = run(&mut dec, &x);
                if show {
                    eprintln!("{} {snr} seed {seed}: {got:?}", cfg.label());
                }
                worst = worst.max(cer(&long, &got).min(1.0));
            }
            (snr, worst)
        })
        .collect()
}

#[test]
#[ignore]
fn sensitivity_sweep() {
    for cfg in modes() {
        let r = sweep(cfg, &[6.0, 3.0, 0.0, -2.0, -4.0, -6.0, -8.0, -10.0, -12.0]);
        eprintln!("{}: {r:?}", cfg.label());
    }
}

/// Worst of three runs, where the sweep showed copy: the bar is the
/// measured floor, a dB or two above where it fell apart.
#[test]
fn copies_weak_signals() {
    for (bw, snr) in [(500, -6.0f32), (1000, -4.0), (2000, -2.0)] {
        let cfg = Mt63Config::new(bw, true, 1500.0);
        let r = sweep(cfg, &[snr]);
        assert!(r[0].1 < 0.02, "{}: {r:?}", cfg.label());
    }
}

/// The SNR reading against the SNR set. Printed, not asserted: it reads
/// low at high SNR, where the keying skirts count as noise (4 to 10 dB low
/// at 20 dB). `--nocapture` to see it.
#[test]
fn snr_reading() {
    for cfg in [
        Mt63Config::new(1000, true, 1500.0),
        Mt63Config::new(2000, false, 1500.0),
        Mt63Config::new(500, true, 1500.0),
    ] {
        for snr in [-3.0f32, 3.0, 10.0, 20.0] {
            let mut rng = Rng::new(21);
            let mut x = mt63_tx(TEXT.as_bytes(), cfg, 0.0, 0.0, 0.3, FS, 3);
            add_noise(&mut x, 0.3, snr, FS, &mut rng);
            let mut dec = Mt63Decoder::new(FS, cfg);
            chunks(&x, |c| dec.push(c));
            eprintln!(
                "{} at {snr} dB reads {:?}",
                cfg.label(),
                dec.status().snr_db
            );
        }
    }
}

#[test]
fn auto_finds_the_mode_and_prints_only_it() {
    for cfg in modes() {
        let mut rng = Rng::new(5);
        let mut x = mt63_tx(TEXT.as_bytes(), cfg, 12.0, 30.0, 0.3, FS, 13);
        add_noise(&mut x, 0.3, 15.0, FS, &mut rng);
        let mut dec = Mt63Decoder::new_auto(FS, 1500.0);
        chunks(&x, |c| dec.push(c));
        dec.flush();
        let mut text = String::new();
        for e in dec.take() {
            let Mt63Event::Text { text: t, mode, .. } = &e;
            assert_eq!(*mode, cfg.label(), "{e:?}");
            text.push_str(t);
            text.push(' ');
        }
        assert!(cer(TEXT, &text) < 0.02, "{}: {text:?}", cfg.label());
        assert_eq!(dec.status().mode, cfg.label());
    }
}

#[test]
fn eight_bit_round_trip() {
    let mut cfg = Mt63Config::new(1000, false, 1500.0);
    cfg.eight_bit = true;
    let sent = "CAF\u{e9} A\u{c9}O \u{fc}BER DIE BR\u{fc}CKE 73";
    let bytes: Vec<u8> = sent.chars().map(|c| c as u32 as u8).collect();
    let x = mt63_tx(&bytes, cfg, 0.0, 0.0, 0.3, FS, 4);
    let mut dec = Mt63Decoder::new(FS, cfg);
    let got = run(&mut dec, &x);
    assert!(got.contains("BR\u{fc}CKE"), "{got:?}");
}

/// Copy text after the fact; the assertions are on what printed.
fn assert_quiet(mut dec: Mt63Decoder, x: &[f32], name: &str) {
    chunks(x, |c| dec.push(c));
    dec.flush();
    let ev = dec.take();
    assert!(ev.is_empty(), "{name}: {ev:?}");
    assert!(!dec.status().locked, "{name}");
}

#[test]
fn noise_prints_nothing() {
    for seed in 1..=2u64 {
        let mut rng = Rng::new(seed * 991);
        let n = (240.0 * FS) as usize;
        let x: Vec<f32> = (0..n).map(|_| 0.3 * rng.gauss()).collect();
        assert_quiet(Mt63Decoder::new_auto(FS, 1500.0), &x, "noise");
    }
}

/// Steady carriers and FT8-like 8-FSK stations, in 15 second slots, over a
/// little noise.
fn interference(secs: f32, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let n = (secs * FS) as usize;
    let mut x = vec![0f32; n];
    for f in [640.0f64, 921.3, 1187.5, 1512.4, 1766.6, 2045.0, 2311.9] {
        let a = 0.12;
        for (i, v) in x.iter_mut().enumerate() {
            *v += a * (TAU * f * i as f64 / FS as f64).sin() as f32;
        }
    }
    // FT8-like: 79 symbols, 6.25 baud, 8 tones 6.25 Hz apart, 15 s slots,
    // several stations each in its own slot pattern.
    let stations = 18;
    for s in 0..stations {
        let f0 = 300.0 + 2300.0 * (s as f64 + rng.uniform()) / stations as f64;
        let amp = 0.08 + 0.3 * rng.uniform() as f32;
        let offs = (rng.uniform() * 2.0) as usize;
        let mut slot = offs;
        while (slot as f32 * 15.0 + 13.0) < secs {
            let start = (slot as f32 * 15.0 * FS) as usize;
            let dt = (0.5 * rng.uniform() as f32 * FS) as usize;
            let sym_len = (FS * 0.16) as usize;
            let mut phase = TAU * rng.uniform();
            for k in 0..79 {
                let tone = (rng.uniform() * 8.0).floor();
                let f = f0 + 6.25 * tone;
                for j in 0..sym_len {
                    let i = start + dt + k * sym_len + j;
                    if i < n {
                        let u = j as f32 / sym_len as f32;
                        let env = (std::f32::consts::PI * u).sin().powf(0.25);
                        x[i] += amp * env * phase.sin() as f32;
                    }
                    phase += TAU * f / FS as f64;
                }
            }
            slot += 2;
        }
    }
    for v in x.iter_mut() {
        *v += 0.02 * rng.gauss();
    }
    x
}

#[test]
fn interference_prints_nothing() {
    for seed in 1..=2u64 {
        let x = interference(180.0, seed * 37);
        assert_quiet(Mt63Decoder::new_auto(FS, 1500.0), &x, "interference");
        for cfg in [
            Mt63Config::new(1000, true, 1500.0),
            Mt63Config::new(500, false, 1500.0),
        ] {
            assert_quiet(Mt63Decoder::new(FS, cfg), &x, "interference, one mode");
        }
    }
}

#[test]
fn no_short_fragments() {
    for cfg in [
        Mt63Config::new(1000, true, 1500.0),
        Mt63Config::new(2000, false, 1500.0),
    ] {
        let mut rng = Rng::new(5);
        let mut x = mt63_tx(TEXT.as_bytes(), cfg, 0.0, 0.0, 0.3, FS, 13);
        add_noise(&mut x, 0.3, 5.0, FS, &mut rng);
        let mut dec = Mt63Decoder::new_auto(FS, 1500.0);
        chunks(&x, |c| dec.push(c));
        dec.flush();
        for e in dec.take() {
            let Mt63Event::Text { text, .. } = &e;
            assert!(text.len() > 2, "{e:?}");
        }
    }
}

/// Seconds of audio decoded per second of work. `--ignored --nocapture`.
#[test]
#[ignore]
fn speed() {
    let x = mt63_tx(TEXT.as_bytes(), Mt63Config::default(), 0.0, 0.0, 0.3, FS, 1);
    let secs = x.len() as f64 / FS as f64;
    for auto in [false, true] {
        let mut dec = if auto {
            Mt63Decoder::new_auto(FS, 1500.0)
        } else {
            Mt63Decoder::new(FS, Mt63Config::default())
        };
        let t = std::time::Instant::now();
        chunks(&x, |c| dec.push(c));
        eprintln!(
            "{}: {:.0}x real time",
            if auto { "auto" } else { "1000L" },
            secs / t.elapsed().as_secs_f64()
        );
    }
}

/// Largest gate statistics any lane reached on non-MT63 audio. `--ignored
/// --nocapture`.
#[test]
#[ignore]
fn gate_margins() {
    let report = |name: &str, x: &[f32]| {
        let mut dec = Mt63Decoder::new_auto(FS, 1500.0);
        chunks(x, |c| dec.push(c));
        dec.flush();
        let printed = dec.take().len();
        for chain in &dec.chains {
            for l in &chain.lanes {
                let mut max_s = 0.0f32;
                let mut run = 0;
                let mut best = 0;
                let mut locked_good = 0;
                let mut last = i32::MIN;
                for (d, locked, _, _, _) in &l.trace {
                    max_s = max_s.max(d.smooth);
                    let good = *locked && d.smooth >= S_OPEN && d.offset == last;
                    last = d.offset;
                    if good {
                        locked_good += 1;
                        run += 1;
                        best = best.max(run);
                    } else {
                        run = 0;
                    }
                }
                eprintln!(
                    "{name} {}: max S {max_s:.2}, longest gate run {best}/{OPEN_RUN}, good symbols {locked_good}, printed {printed}",
                    l.cfg.label()
                );
            }
        }
    };
    let mut rng = Rng::new(3);
    let noise: Vec<f32> = (0..(180.0 * FS) as usize)
        .map(|_| 0.3 * rng.gauss())
        .collect();
    report("noise", &noise);
    report("interference", &interference(180.0, 91));
    let strong: Vec<f32> = interference(180.0, 92).iter().map(|v| v * 3.0).collect();
    report("interference x3", &strong);
}
