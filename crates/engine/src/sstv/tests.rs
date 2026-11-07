//! A synthetic SSTV transmitter, written from the mode timings separately
//! from the receiver's table, sending a test card through tuning error,
//! sample-clock error and white noise.

use super::*;
use std::f64::consts::TAU;

/// The live rate is not a round number.
const FS: f32 = 47_904.0;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn uniform(&mut self) -> f64 {
        ((self.next() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    fn gauss(&mut self) -> f64 {
        let (a, b) = (self.uniform(), self.uniform());
        (-2.0 * a.ln()).sqrt() * (TAU * b).cos()
    }
}

/// Phase-continuous FM tone generator on the sender's own clock: `fs`
/// claimed, `fs * (1 + ppm)` true, so the receiver, told `fs`, sees every
/// line `ppm` long or short.
struct Tx {
    fs_true: f64,
    off: f64,
    amp: f64,
    phase: f64,
    t: f64,
    out: Vec<f32>,
}

impl Tx {
    fn new(ppm: f64, off: f64, amp: f64) -> Self {
        Tx { fs_true: FS as f64 * (1.0 + ppm * 1e-6), off, amp, phase: 0.0, t: 0.0, out: Vec::new() }
    }

    fn tone(&mut self, hz: f64, ms: f64) {
        self.t += ms / 1000.0;
        let step = TAU * (hz + self.off) / self.fs_true;
        while (self.out.len() as f64) < self.t * self.fs_true {
            self.phase += step;
            if self.phase > TAU {
                self.phase -= TAU;
            }
            self.out.push((self.amp * self.phase.sin()) as f32);
        }
    }

    fn silence(&mut self, ms: f64) {
        self.t += ms / 1000.0;
        while (self.out.len() as f64) < self.t * self.fs_true {
            self.out.push(0.0);
        }
    }

    fn scan(&mut self, vals: &[f64], ms: f64) {
        let px = ms / vals.len() as f64;
        for &v in vals {
            self.tone(1500.0 + v.clamp(0.0, 255.0) / 255.0 * 800.0, px);
        }
    }

    fn vis(&mut self, code: u8) {
        self.tone(1900.0, 300.0);
        self.tone(1200.0, 10.0);
        self.tone(1900.0, 300.0);
        self.tone(1200.0, 30.0);
        for i in 0..7 {
            self.tone(if code >> i & 1 == 1 { 1100.0 } else { 1300.0 }, 30.0);
        }
        self.tone(if code.count_ones() % 2 == 1 { 1100.0 } else { 1300.0 }, 30.0);
        self.tone(1200.0, 30.0);
    }
}

/// Test card: color bars over the top two thirds, a gray ramp with black
/// ticks every 40 pixels, then red rising and blue falling across.
fn card(w: usize, h: usize) -> Vec<Vec<[u8; 3]>> {
    const BARS: [[u8; 3]; 8] = [
        [255, 255, 255],
        [255, 255, 0],
        [0, 255, 255],
        [0, 255, 0],
        [255, 0, 255],
        [255, 0, 0],
        [0, 0, 255],
        [0, 0, 0],
    ];
    // Region edges on even rows, so a Robot 36 or PD row pair never
    // straddles two colors.
    let h1 = (h * 2 / 3) & !1;
    let h2 = (h * 5 / 6) & !1;
    (0..h)
        .map(|r| {
            (0..w)
                .map(|c| {
                    let x = (c * 255 / (w - 1)) as u8;
                    if r < h1 {
                        BARS[c * 8 / w]
                    } else if r < h2 {
                        if c % 40 < 4 { [0, 0, 0] } else { [x, x, x] }
                    } else {
                        [x, 128, 255 - x]
                    }
                })
                .collect()
        })
        .collect()
}

fn yuv(p: [u8; 3]) -> [f64; 3] {
    let (r, g, b) = (p[0] as f64, p[1] as f64, p[2] as f64);
    [
        16.0 + (65.738 * r + 129.057 * g + 25.064 * b) / 256.0,
        128.0 + (-37.945 * r - 74.494 * g + 112.439 * b) / 256.0,
        128.0 + (112.439 * r - 94.154 * g - 18.285 * b) / 256.0,
    ]
}

fn comp(row: &[[u8; 3]], i: usize) -> Vec<f64> {
    row.iter().map(|p| p[i] as f64).collect()
}

/// Luminance (0), B-Y (1) or R-Y (2) of a row, or of the mean of two rows.
fn yuv_comp(rows: &[&[[u8; 3]]], i: usize) -> Vec<f64> {
    (0..rows[0].len()).map(|c| rows.iter().map(|r| yuv(r[c])[i]).sum::<f64>() / rows.len() as f64).collect()
}

/// The picture after its VIS, by the published timings.
fn send_picture(tx: &mut Tx, mode: SstvMode, img: &[Vec<[u8; 3]>]) {
    let martin = |tx: &mut Tx, scan: f64| {
        for row in img {
            tx.tone(1200.0, 4.862);
            tx.tone(1500.0, 0.572);
            for i in [1, 2, 0] {
                tx.scan(&comp(row, i), scan);
                tx.tone(1500.0, 0.572);
            }
        }
    };
    let scottie = |tx: &mut Tx, scan: f64| {
        tx.tone(1200.0, 9.0);
        for row in img {
            tx.tone(1500.0, 1.5);
            tx.scan(&comp(row, 1), scan);
            tx.tone(1500.0, 1.5);
            tx.scan(&comp(row, 2), scan);
            tx.tone(1200.0, 9.0);
            tx.tone(1500.0, 1.5);
            tx.scan(&comp(row, 0), scan);
        }
    };
    let pd = |tx: &mut Tx, px: f64| {
        let d = px * img[0].len() as f64;
        for pair in img.chunks(2) {
            let both = [&pair[0][..], &pair[1][..]];
            tx.tone(1200.0, 20.0);
            tx.tone(1500.0, 2.08);
            tx.scan(&yuv_comp(&both[..1], 0), d);
            tx.scan(&yuv_comp(&both, 2), d);
            tx.scan(&yuv_comp(&both, 1), d);
            tx.scan(&yuv_comp(&both[1..], 0), d);
        }
    };
    match mode {
        SstvMode::MartinM1 => martin(tx, 146.432),
        SstvMode::MartinM2 => martin(tx, 73.216),
        SstvMode::ScottieS1 => scottie(tx, 138.24),
        SstvMode::ScottieS2 => scottie(tx, 88.064),
        SstvMode::ScottieDx => scottie(tx, 345.6),
        SstvMode::Robot36 => {
            for (r, row) in img.iter().enumerate() {
                let pair = [&img[r & !1][..], &img[r | 1][..]];
                tx.tone(1200.0, 9.0);
                tx.tone(1500.0, 3.0);
                tx.scan(&yuv_comp(&[row], 0), 88.0);
                if r % 2 == 0 {
                    tx.tone(1500.0, 4.5);
                    tx.tone(1900.0, 1.5);
                    tx.scan(&yuv_comp(&pair, 2), 44.0);
                } else {
                    tx.tone(2300.0, 4.5);
                    tx.tone(1900.0, 1.5);
                    tx.scan(&yuv_comp(&pair, 1), 44.0);
                }
            }
        }
        SstvMode::Robot72 => {
            for row in img {
                tx.tone(1200.0, 9.0);
                tx.tone(1500.0, 3.0);
                tx.scan(&yuv_comp(&[row], 0), 138.0);
                tx.tone(1500.0, 4.5);
                tx.tone(1900.0, 1.5);
                tx.scan(&yuv_comp(&[row], 2), 69.0);
                tx.tone(2300.0, 4.5);
                tx.tone(1900.0, 1.5);
                tx.scan(&yuv_comp(&[row], 1), 69.0);
            }
        }
        SstvMode::Pd50 | SstvMode::Pd180 | SstvMode::Pd290 => pd(tx, 0.286),
        SstvMode::Pd90 => pd(tx, 0.532),
        SstvMode::Pd120 => pd(tx, 0.19),
        SstvMode::Pd160 | SstvMode::Pd240 => pd(tx, 0.382),
        SstvMode::WraaseSc2_180 => {
            for row in img {
                tx.tone(1200.0, 5.5225);
                tx.tone(1500.0, 0.5);
                for i in 0..3 {
                    tx.scan(&comp(row, i), 235.0);
                }
            }
        }
    }
}

/// White noise for `snr_db` in 3 kHz against a sine of amplitude `amp`.
fn add_noise(x: &mut [f32], amp: f64, snr_db: f64, seed: u64) {
    let sigma = (amp * amp / 2.0 / 10f64.powf(snr_db / 10.0) / (3000.0 / (FS as f64 / 2.0))).sqrt();
    let mut rng = Rng(seed | 1);
    for v in x.iter_mut() {
        *v += (sigma * rng.gauss()) as f32;
    }
}

struct Run {
    events: Vec<SstvEvent>,
    /// Decode time over audio time.
    rtf: f64,
    /// The last slant reading while a picture was running.
    slant_ppm: f32,
}

fn decode(x: &[f32], chunk: usize) -> Run {
    let mut d = SstvDecoder::new(FS);
    let mut events = Vec::new();
    let mut slant_ppm = 0.0;
    let t = std::time::Instant::now();
    for c in x.chunks(chunk) {
        d.push(c);
        events.extend(d.take());
        let st = d.status();
        if st.state == SstvState::Receiving {
            slant_ppm = st.slant_ppm;
        }
    }
    let rtf = t.elapsed().as_secs_f64() / (x.len() as f64 / FS as f64);
    Run { events, rtf, slant_ppm }
}

/// A whole transmission: noise, VIS, picture, noise.
fn transmission(mode: SstvMode, ppm: f64, off: f64, snr_db: f64, seed: u64) -> (Vec<f32>, Vec<Vec<[u8; 3]>>) {
    let img = card(mode.width() as usize, mode.height() as usize);
    let amp = 0.3;
    let mut tx = Tx::new(ppm, off, amp);
    tx.silence(700.0);
    tx.vis(mode.vis());
    send_picture(&mut tx, mode, &img);
    tx.silence(2000.0);
    let mut x = tx.out;
    if snr_db.is_finite() {
        add_noise(&mut x, amp, snr_db, seed);
    }
    (x, img)
}

/// Decoded rows by line number.
fn picture(events: &[SstvEvent], w: usize, h: usize) -> Vec<Option<Vec<u8>>> {
    let mut rows = vec![None; h];
    for e in events {
        if let SstvEvent::Line { line, rgb } = e {
            assert_eq!(rgb.len(), w * 3, "row width");
            rows[*line as usize] = Some(rgb.clone());
        }
    }
    rows
}

fn psnr(rows: &[Option<Vec<u8>>], img: &[Vec<[u8; 3]>]) -> f64 {
    let (mut se, mut n) = (0f64, 0f64);
    for (got, want) in rows.iter().zip(img) {
        for (c, p) in want.iter().enumerate() {
            for i in 0..3 {
                let g = got.as_ref().map_or(128.0, |r| r[c * 3 + i] as f64);
                se += (g - p[i] as f64).powi(2);
                n += 1.0;
            }
        }
    }
    10.0 * (255.0f64 * 255.0 / (se / n).max(1e-9)).log10()
}

/// Column shift of a decoded row against its source, by least squares.
fn row_shift(got: &[u8], want: &[[u8; 3]], max: i32) -> i32 {
    let w = want.len() as i32;
    let mut best = (f64::MAX, 0);
    for s in -max..=max {
        let mut se = 0f64;
        for c in max..w - max {
            for i in 0..3 {
                se += (got[(c * 3) as usize + i] as f64 - want[(c + s) as usize][i] as f64).powi(2);
            }
        }
        if se < best.0 {
            best = (se, s);
        }
    }
    best.1
}

struct Outcome {
    vis: Option<u8>,
    mode: Option<SstvMode>,
    complete: bool,
    rows: usize,
    psnr: f64,
    shifts: Vec<i32>,
    rtf: f64,
    slant_ppm: f32,
}

fn check(mode: SstvMode, ppm: f64, off: f64, snr_db: f64, seed: u64) -> Outcome {
    let (x, img) = transmission(mode, ppm, off, snr_db, seed);
    let run = decode(&x, 3001);
    let (w, h) = (mode.width() as usize, mode.height() as usize);
    let rows = picture(&run.events, w, h);
    let (mut vis, mut got_mode, mut complete) = (None, None, false);
    for e in &run.events {
        match e {
            SstvEvent::Start { mode, vis: v, width, height } => {
                assert_eq!((*width, *height), (mode.width(), mode.height()));
                vis = *v;
                got_mode = Some(*mode);
            }
            SstvEvent::End { complete: c, .. } => complete = *c,
            _ => {}
        }
    }
    let shifts = rows.iter().zip(&img).filter_map(|(g, want)| g.as_ref().map(|g| row_shift(g, want, 12))).collect();
    Outcome {
        vis,
        mode: got_mode,
        complete,
        rows: rows.iter().filter(|r| r.is_some()).count(),
        psnr: psnr(&rows, &img),
        shifts,
        rtf: run.rtf,
        slant_ppm: run.slant_ppm,
    }
}

/// The PSNR a mode reaches on the test card with little noise. The bars'
/// edges are what cost it: the scans are faster than the band the
/// discriminator passes, and the YUV modes, Robot 36 most (its color
/// difference scans are 44 ms), double the color difference's blur in red
/// and blue.
fn floor_db(mode: SstvMode) -> f64 {
    match mode {
        SstvMode::Robot36 => 20.0,
        SstvMode::Robot72 => 23.0,
        m if m.spec().chans.iter().any(|c| c.comp == Comp::Y) => 24.0,
        _ => 25.0,
    }
}

/// Every mode, its VIS decoded, through ±50 Hz of tuning error, ±200 ppm of
/// sample-clock error and noise at 30 dB in 3 kHz: the picture complete,
/// close to the source, and every row where it belongs (the slant taken
/// out).
fn mode_test(mode: SstvMode, sign: f64) {
    let o = check(mode, 200.0 * sign, 50.0 * sign, 30.0, mode.vis() as u64 * 7919);
    let worst = o.shifts.iter().map(|s| s.abs()).max().unwrap_or(99);
    eprintln!(
        "{}: vis {:?}, {} rows, PSNR {:.1} dB, worst shift {} px, slant {:.0} ppm, rtf {:.4}",
        mode.name(),
        o.vis,
        o.rows,
        o.psnr,
        worst,
        o.slant_ppm,
        o.rtf
    );
    assert_eq!(o.vis, Some(mode.vis()), "{}: VIS", mode.name());
    assert_eq!(o.mode, Some(mode));
    assert!(o.complete, "{}: incomplete, {} rows", mode.name(), o.rows);
    assert_eq!(o.rows, mode.height() as usize);
    assert!(worst <= 2, "{}: slant, shifts {:?}", mode.name(), o.shifts);
    assert!((o.slant_ppm as f64 - 200.0 * sign).abs() < 20.0, "{}: slant {} ppm", mode.name(), o.slant_ppm);
    assert!(o.psnr > floor_db(mode), "{}: PSNR {:.1} dB", mode.name(), o.psnr);
}

macro_rules! mode_tests {
    ($($name:ident: $mode:expr, $sign:expr;)*) => {
        $(
            #[test]
            fn $name() {
                mode_test($mode, $sign);
            }
        )*
    };
}

mode_tests! {
    martin_m1: SstvMode::MartinM1, 1.0;
    martin_m2: SstvMode::MartinM2, -1.0;
    scottie_s1: SstvMode::ScottieS1, -1.0;
    scottie_s2: SstvMode::ScottieS2, 1.0;
    scottie_dx: SstvMode::ScottieDx, 1.0;
    robot_36: SstvMode::Robot36, 1.0;
    robot_72: SstvMode::Robot72, -1.0;
    pd_50: SstvMode::Pd50, -1.0;
    pd_90: SstvMode::Pd90, 1.0;
    pd_120: SstvMode::Pd120, -1.0;
    pd_160: SstvMode::Pd160, 1.0;
    pd_180: SstvMode::Pd180, 1.0;
    pd_240: SstvMode::Pd240, -1.0;
    pd_290: SstvMode::Pd290, 1.0;
    wraase_sc2_180: SstvMode::WraaseSc2_180, -1.0;
}

#[test]
fn mode_table_matches_published_line_times() {
    let want = [
        (SstvMode::MartinM1, 446.446),
        (SstvMode::MartinM2, 226.798),
        (SstvMode::ScottieS1, 428.22),
        (SstvMode::ScottieS2, 277.692),
        (SstvMode::ScottieDx, 1050.3),
        (SstvMode::Robot36, 150.0),
        (SstvMode::Robot72, 300.0),
        (SstvMode::Pd50, 388.16),
        (SstvMode::Pd90, 703.04),
        (SstvMode::Pd120, 508.48),
        (SstvMode::Pd160, 804.416),
        (SstvMode::Pd180, 754.24),
        (SstvMode::Pd240, 1000.0),
        (SstvMode::Pd290, 937.28),
        (SstvMode::WraaseSc2_180, 711.0225),
    ];
    for (m, ms) in want {
        assert!((m.line_ms() - ms).abs() < 1e-6, "{}: {} ms", m.name(), m.line_ms());
        assert_eq!(SstvMode::from_vis(m.vis()), Some(m));
    }
}

/// Noise alone, at three levels, starts no picture.
#[test]
fn noise_alone_no_picture() {
    let n = (FS * 120.0) as usize;
    for (k, level) in [0.001f64, 0.05, 0.5].into_iter().enumerate() {
        let mut x = vec![0f32; n];
        add_noise(&mut x, level, 0.0, 99 + k as u64);
        let run = decode(&x, 4096);
        eprintln!("noise {level}: rtf {:.4}", run.rtf);
        assert!(run.events.is_empty(), "noise started {:?}", run.events.first());
    }
}

/// The level swinging 40 dB over the picture changes nothing.
#[test]
fn level_does_not_matter() {
    let (x, img) = transmission(SstvMode::Robot36, 0.0, 0.0, f64::INFINITY, 1);
    let steady = psnr(&picture(&decode(&x, 777).events, 320, 240), &img);
    let len = x.len() as f64;
    let swung: Vec<f32> = x
        .iter()
        .enumerate()
        .map(|(i, &v)| v * 10f64.powf(-2.0 * (TAU * i as f64 / len * 3.0).sin().abs()) as f32)
        .collect();
    let p = psnr(&picture(&decode(&swung, 777).events, 320, 240), &img);
    assert!((p - steady).abs() < 0.2, "PSNR {p:.1} against {steady:.1} steady");
}

/// `start_now` on a picture whose VIS was lost, a few seconds in: the rows
/// from there on are placed and colored as with the VIS.
#[test]
fn start_now_without_vis() {
    for mode in [SstvMode::ScottieS1, SstvMode::Robot36, SstvMode::Pd120] {
        let (x, img) = transmission(mode, -150.0, 30.0, 30.0, 5);
        // Cut out everything before the picture's tenth second.
        let cut = (FS * 10.0) as usize;
        let mut d = SstvDecoder::new(FS);
        d.start_now(mode);
        let mut events = d.take();
        // The picture, then silence until the decoder gives up on it.
        for c in x[cut..].chunks(2048).chain(vec![0f32; FS as usize * 8].chunks(2048)) {
            d.push(c);
            events.extend(d.take());
        }
        assert!(matches!(events[0], SstvEvent::Start { vis: None, .. }));
        assert!(matches!(events.last(), Some(SstvEvent::End { complete: false, .. })));
        // Rows are numbered from where reception began; find the offset.
        let got: Vec<&Vec<u8>> =
            events.iter().filter_map(|e| if let SstvEvent::Line { rgb, .. } = e { Some(rgb) } else { None }).collect();
        let t_pic = cut as f64 / FS as f64 - 1.3;
        let first = (t_pic * 1000.0 / mode.line_ms()).floor() as usize * mode.rows_per_line() as usize;
        let mut best = (f64::MIN, 0usize);
        for skip in first.saturating_sub(4)..first + 6 {
            let rows: Vec<Option<Vec<u8>>> =
                (0..img.len()).map(|r| r.checked_sub(skip).and_then(|i| got.get(i)).map(|v| (*v).clone())).collect();
            let want: Vec<Vec<[u8; 3]>> = img
                .iter()
                .enumerate()
                .map(|(r, row)| if rows[r].is_some() { row.clone() } else { vec![[128; 3]; row.len()] })
                .collect();
            let p = psnr(&rows, &want);
            if p > best.0 {
                best = (p, skip);
            }
        }
        eprintln!("{} start_now: {} rows, PSNR {:.1} dB from row {}", mode.name(), got.len(), best.0, best.1);
        assert!(best.0 > floor_db(mode), "{}: PSNR {:.1}", mode.name(), best.0);
        // The remaining rows plus the few after the end, before the loss.
        assert!(got.len() + best.1 >= img.len());
    }
}

/// A realistic weak signal: Martin M1 at 12 dB in 3 kHz.
#[test]
fn weak_signal() {
    let o = check(SstvMode::MartinM1, 120.0, -20.0, 12.0, 11);
    eprintln!("M1 at 12 dB: PSNR {:.1} dB", o.psnr);
    assert_eq!(o.vis, Some(SstvMode::MartinM1.vis()));
    assert!(o.psnr > 20.0, "PSNR {:.1}", o.psnr);
}

/// The SNR sweep behind the numbers in the module notes, every mode with
/// +50 Hz and +200 ppm: the lowest SNR (3 kHz, 1 dB steps) where the VIS
/// still decodes, and where the picture's PSNR is still over 20 dB.
/// `cargo test --release -p scannerd-engine sstv::tests::snr_sweep -- --ignored --nocapture`
#[test]
#[ignore]
fn snr_sweep() {
    let lines: Vec<String> = std::thread::scope(|s| {
        let jobs: Vec<_> = SstvMode::ALL
            .into_iter()
            .map(|mode| {
                s.spawn(move || {
                    let (mut vis_at, mut psnr_at) = (None, None);
                    let mut cells = String::new();
                    for snr in (0..=24).rev() {
                        let o = check(mode, 200.0, 50.0, snr as f64, 3 + snr as u64);
                        if o.vis.is_some() {
                            vis_at = Some(snr);
                        }
                        if o.psnr > 20.0 {
                            psnr_at = Some(snr);
                        }
                        if snr % 4 == 0 {
                            cells += &format!(" {snr:>2}:{:>5.1}", o.psnr);
                        }
                        if vis_at != Some(snr) && psnr_at.is_some_and(|p| p > snr + 2) {
                            break;
                        }
                    }
                    format!(
                        "{:<15} VIS down to {:>3} dB, PSNR > 20 dB down to {:>3} dB |{cells}",
                        mode.name(),
                        vis_at.map_or("-".into(), |v: i32| v.to_string()),
                        psnr_at.map_or("-".into(), |v: i32| v.to_string())
                    )
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });
    for l in lines {
        eprintln!("{l}");
    }
}

/// A picture cut off halfway by the next one's VIS ends there, incomplete,
/// and the next is received whole. The VIS is only known at its stop bit,
/// so the first picture also draws most of the 0.91 s of VIS as its last
/// rows (five of Robot 36's 150 ms lines; the sixth waits on a pulse).
#[test]
fn vis_interrupts_picture() {
    let mode = SstvMode::Robot36;
    let img = card(320, 240);
    let mut tx = Tx::new(80.0, -30.0, 0.3);
    tx.silence(500.0);
    tx.vis(mode.vis());
    send_picture(&mut tx, mode, &img[..120]);
    tx.vis(SstvMode::MartinM2.vis());
    send_picture(&mut tx, SstvMode::MartinM2, &card(320, 256));
    tx.silence(1500.0);
    let mut x = tx.out;
    add_noise(&mut x, 0.3, 25.0, 17);
    let run = decode(&x, 4000);
    let marks: Vec<String> =
        run.events.iter().filter(|e| !matches!(e, SstvEvent::Line { .. })).map(|e| e.summary()).collect();
    assert_eq!(
        marks,
        [
            "Robot 36 320x240 (VIS 8)",
            "picture ended after 125 lines",
            "Martin M2 320x256 (VIS 40)",
            "picture complete, 256 lines"
        ],
        "{marks:?}"
    );
}
