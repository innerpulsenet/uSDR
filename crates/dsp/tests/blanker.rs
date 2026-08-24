//! The impulse blanker must not eat the leading edge of a strong narrowband
//! burst. It used to: `FrontEnd::new` ran it unconditionally, and on a quiet
//! band a 20 dB burst lost its first 54 ms — which is longer than a short
//! APRS TXDELAY, so the strongest packets were the ones that never decoded.

use num_complex::Complex32;
use scannerd_dsp::FrontEnd;

/// Quiet noise, then a strong carrier. Returns the IQ and where the burst starts.
fn noise_then_burst(fs: f64, noise_amp: f32, burst_amp: f32) -> (Vec<Complex32>, usize) {
    let mut seed = 0x243F_6A88_85A3_08D3u64;
    let mut rng = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        ((seed >> 11) as f32 / (1u64 << 53) as f32) * 2.0 - 1.0
    };
    let quiet = (fs * 3.0) as usize;
    let burst = (fs * 0.6) as usize;
    let mut iq = Vec::with_capacity(quiet + burst);
    for _ in 0..quiet {
        iq.push(Complex32::new(rng() * noise_amp, rng() * noise_amp));
    }
    let mut phi = 0.0f64;
    for _ in 0..burst {
        phi += std::f64::consts::TAU * 100_000.0 / fs;
        iq.push(Complex32::new(
            phi.cos() as f32 * burst_amp + rng() * noise_amp,
            phi.sin() as f32 * burst_amp + rng() * noise_amp,
        ));
    }
    (iq, quiet)
}

/// Samples zeroed inside the burst, and how far in the last one is (ms).
fn blanking(mut fe: FrontEnd, fs: f64, noise: f32, carrier: f32) -> (f32, f64) {
    let (mut iq, start) = noise_then_burst(fs, noise, carrier);
    for chunk in iq.chunks_mut(16_384) {
        fe.process(chunk);
    }
    let burst = &iq[start..];
    let zeroed = burst.iter().filter(|c| c.norm() < 1e-9).count();
    let last = burst
        .iter()
        .rposition(|c| c.norm() < 1e-9)
        .map(|i| i as f64 / fs * 1000.0)
        .unwrap_or(0.0);
    (zeroed as f32 / burst.len() as f32, last)
}

#[test]
fn without_blanker_leaves_a_strong_burst_intact() {
    let fs = 960_000.0;
    for &snr_db in &[14.0f32, 20.0, 30.0] {
        let noise = 0.01f32;
        let carrier = noise * 10f32.powf(snr_db / 20.0);
        let (fraction, _) = blanking(FrontEnd::without_blanker(fs), fs, noise, carrier);
        assert!(
            fraction == 0.0,
            "{snr_db} dB burst lost {:.2}% of its samples with the blanker off",
            fraction * 100.0
        );
    }
}

/// The behaviour the APRS path is opting out of, kept as the justification for
/// opting out: with the blanker on, the *stronger* the burst the more of its
/// leading edge disappears.
#[test]
fn the_blanker_chews_the_leading_edge_of_a_strong_burst() {
    let fs = 960_000.0;
    let noise = 0.01f32;
    let (weak, _) = blanking(
        FrontEnd::new(fs),
        fs,
        noise,
        noise * 10f32.powf(14.0 / 20.0),
    );
    let (strong, strong_ms) = blanking(
        FrontEnd::new(fs),
        fs,
        noise,
        noise * 10f32.powf(30.0 / 20.0),
    );
    assert!(
        strong > weak,
        "a stronger burst should be blanked harder: weak {weak:.4}, strong {strong:.4}"
    );
    assert!(
        strong_ms > 50.0,
        "expected the damage to reach well past a short TXDELAY, got {strong_ms:.1} ms"
    );
}

/// `reset()` forgets what was learned, not what was configured.
#[test]
fn reset_keeps_the_blanker_setting() {
    let mut fe = FrontEnd::without_blanker(960_000.0);
    assert_eq!(fe.blanker_status().0, "off");
    fe.reset();
    assert_eq!(fe.blanker_status().0, "off");
}
