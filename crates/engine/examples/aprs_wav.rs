//! Run a PACKET-mode channel capture through the APRS decoder offline: the
//! server's `GET /api/sdr/capture.wav?kind=iq` (16-bit stereo I/Q, the
//! receiver at 0 Hz), through the classifier's discriminator as PACKET mode
//! runs it, and also through a plain one, packets printed from each.
//!
//!     cargo run --release -p scannerd-engine --example aprs_wav <capture.wav>

use num_complex::Complex32;
use scannerd_engine::aprs::AprsDecoder;
use scannerd_engine::classifier::SignalClassifier;

fn main() {
    let path = std::env::args().nth(1).expect("capture.wav");
    let bytes = std::fs::read(&path).expect("read");
    let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap()) as f64;
    let iq: Vec<Complex32> = bytes[44..]
        .chunks_exact(4)
        .map(|c| Complex32::new(i16::from_le_bytes([c[0], c[1]]) as f32, i16::from_le_bytes([c[2], c[3]]) as f32) / 32768.0)
        .collect();
    eprintln!("{:.1} s at {rate} Hz", iq.len() as f64 / rate);
    // `--synth DB`: splice a synthesized packet into the capture at 2 s, its
    // carrier DB above the capture's whole-channel noise power, 3 kHz
    // deviation and 400 Hz off, as the real bursts are.
    let mut iq = iq;
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--synth") {
        let db: f32 = args[i + 1].parse().unwrap();
        let a = (2.0 * rate) as usize;
        let noise = (iq[a..a + rate as usize].iter().map(|z| z.norm_sqr()).sum::<f32>() / rate as f32).sqrt();
        let amp = noise * 10f32.powf(db / 20.0);
        let frame = scannerd_engine::aprs::build_ui("APRS", "N1TEST-9", "!4134.62N/07324.48W-synth");
        let (mut ph, mut ta) = (0.0, 0.0);
        let burst = scannerd_engine::aprs::modulate_burst(&frame, rate, -400.0, amp, 3000.0, &mut ph, &mut ta);
        for (k, b) in burst.iter().enumerate() {
            iq[a + k] += b;
        }
        eprintln!("synth packet at 2.00 s, {db} dB over channel noise, {:.2} s long", burst.len() as f64 / rate);
    }
    let block = (rate / 50.0) as usize;

    // As PACKET mode does: the classifier's monitor discriminator.
    let mut cls = SignalClassifier::new(rate);
    cls.use_external_pagers();
    let mut aprs = AprsDecoder::new(rate);
    let mut aprs_std = AprsDecoder::new(rate);
    let mut seen = std::collections::BTreeMap::<String, usize>::new();
    for (i, c) in iq.chunks(block).enumerate() {
        let r = cls.process(c, 144.39e6);
        if r.active {
            *seen.entry(r.protocol.clone()).or_default() += 1;
        }
        for p in aprs.process(cls.monitor_discriminator_hz()) {
            println!("{:5.2} s  classifier disc: {p:?}", i as f64 * block as f64 / rate);
        }
        for p in aprs_std.process(cls.standard_discriminator_hz()) {
            println!("{:5.2} s  standard disc: {p:?}", i as f64 * block as f64 / rate);
        }
    }
    eprintln!("classifier said: {seen:?}");

    // A plain discriminator over the channel, filtered to ±bw (0: unfiltered).
    for bw in [0.0f64, 10e3, 8e3, 6e3] {
    let mut x = iq.clone();
    if bw > 0.0 {
        use rustfft::FftPlanner;
        let n = x.len();
        let mut fft = FftPlanner::new();
        fft.plan_fft_forward(n).process(&mut x);
        for (k, v) in x.iter_mut().enumerate() {
            let hz = if k <= n / 2 { k as f64 } else { k as f64 - n as f64 } * rate / n as f64;
            if hz.abs() > bw { *v = Complex32::new(0.0, 0.0); }
        }
        fft.plan_fft_inverse(n).process(&mut x);
    }
    let mut aprs = AprsDecoder::new(rate);
    let mut prev = Complex32::new(1.0, 0.0);
    let disc: Vec<f32> = x
        .iter()
        .map(|&z| {
            let d = (z * prev.conj()).arg() * rate as f32 / std::f32::consts::TAU;
            prev = z;
            d
        })
        .collect();
    for (i, c) in disc.chunks(block).enumerate() {
        for p in aprs.process(c) {
            println!("{:5.2} s  plain ±{bw} disc: {p:?}", i as f64 * block as f64 / rate);
        }
    }
    }
}
