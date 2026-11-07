//! Run a serial-tone capture through the STANAG decoder offline: the
//! server's `GET /api/sdr/capture.wav?kind=iq` (16-bit stereo I/Q, the dial
//! at 0 Hz), turned into upper-sideband audio here, and the decoder's
//! events and readings printed as it goes, with how the equalized symbols
//! fall on the phase circle.
//!
//!     cargo run --release -p scannerd-engine --example stanag_wav <capture.wav>

use num_complex::Complex32;
use rustfft::FftPlanner;
use scannerd_engine::stanag::StanagDecoder;

fn main() {
    let path = std::env::args().nth(1).expect("capture.wav");
    let bytes = std::fs::read(&path).expect("read");
    let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap()) as f32;
    let mut iq: Vec<Complex32> = bytes[44..].chunks_exact(4).map(|c| Complex32::new(i16::from_le_bytes([c[0], c[1]]) as f32, i16::from_le_bytes([c[2], c[3]]) as f32) / 32768.0).collect();
    // Upper sideband: keep 0..+3.4 kHz, drop the rest, take the real part.
    let n = iq.len();
    let mut fft = FftPlanner::new();
    fft.plan_fft_forward(n).process(&mut iq);
    for (k, v) in iq.iter_mut().enumerate() {
        let hz = if k <= n / 2 { k as f32 } else { k as f32 - n as f32 } * rate / n as f32;
        if !(100.0..=3_400.0).contains(&hz) {
            *v = Complex32::new(0.0, 0.0);
        }
    }
    fft.plan_fft_inverse(n).process(&mut iq);
    let audio: Vec<f32> = iq.iter().map(|v| 2.0 * v.re / n as f32).collect();
    eprintln!("{} s at {rate} Hz", n as f32 / rate);
    let mut d = StanagDecoder::new(rate);
    let block = (rate / 10.0) as usize;
    for (i, c) in audio.chunks(block).enumerate() {
        d.push(c);
        for e in d.take() {
            println!("{:5.1} s  {}", i as f32 / 10.0, e.summary());
        }
        if i % 20 == 19 {
            let s = d.status();
            let pts = d.constellation();
            // Phase histogram in eighths, and the spread of the magnitudes.
            let mut h = [0u32; 8];
            let mut mag = 0.0;
            for p in &pts {
                h[((p.arg() / (std::f32::consts::PI / 4.0)).round() as i32).rem_euclid(8) as usize] += 1;
                mag += (p.norm() - 1.0).powi(2);
            }
            let rms = (mag / pts.len().max(1) as f32).sqrt();
            println!(
                "{:5.1} s  locked {} {:?} {:?} {:?} bps · snr {:?} · offset {:?} · scores {:?} · phases {:?} · |z|-1 rms {rms:.2}",
                (i + 1) as f32 / 10.0,
                s.locked,
                s.waveform,
                s.modulation,
                s.data_rate_bps,
                s.snr_db.map(|v| v.round()),
                s.freq_error_hz.map(|v| v.round()),
                s.scores.iter().map(|(n, v)| (n.split_whitespace().last().unwrap_or(n), (v * 100.0).round())).collect::<Vec<_>>(),
                h
            );
        }
    }
}
