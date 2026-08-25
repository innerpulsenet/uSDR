//! Weak-signal regression tests: the decoders must keep working as SNR
//! drops, and the audio auto-notch must kill a stable tone.

use scannerd_engine::autonotch::AutoNotch;
use scannerd_engine::pocsag::PocsagDecoder;

/// Minimal POCSAG burst: preamble + sync + one address + two message words.
fn test_burst(fs: f32) -> Vec<f32> {
    const SYNC: u32 = 0x7CD2_15D8;
    const GEN: u32 = 0b111_0110_1001;
    fn syndrome(mut v: u32) -> u32 {
        for i in (10..31).rev() {
            if v >> i & 1 == 1 {
                v ^= GEN << (i - 10);
            }
        }
        v & 0x3FF
    }
    fn codeword(data20: u32, message: bool) -> u32 {
        let bch_data = (data20 << 11 | u32::from(message) << 31) >> 1;
        let bch = bch_data | syndrome(bch_data);
        bch << 1 | (bch.count_ones() & 1)
    }
    let addr = (0x12345 << 3) | 2;
    fn ascii_word(s: &str) -> u32 {
        let mut bits: Vec<bool> = Vec::new();
        for c in s.bytes() {
            for j in 0..7 {
                bits.push(c >> j & 1 == 1);
            }
        }
        while bits.len() < 20 {
            bits.push(false);
        }
        let mut w = 0u32;
        for (i, b) in bits.iter().take(20).enumerate() {
            w |= u32::from(*b) << i;
        }
        w
    }
    let words = [
        SYNC,
        codeword(addr, false),
        codeword(ascii_word("HI"), true),
        codeword(ascii_word("73"), true),
    ];
    let mut bits: Vec<bool> = (0..576).map(|i| i % 2 == 0).collect();
    for w in words {
        for i in (0..32).rev() {
            bits.push(w >> i & 1 == 1);
        }
    }
    let spb = fs / 1200.0;
    let mut out = Vec::new();
    for (k, &b) in bits.iter().enumerate() {
        let hz = if b { 4500.0 } else { -4500.0 };
        let n = (((k + 1) as f32 * spb).round() as usize)
            .saturating_sub((k as f32 * spb).round() as usize)
            .max(1);
        out.extend(std::iter::repeat_n(hz, n));
    }
    // Soften edges like a real transmitter's IF filter.
    let mut smooth = vec![0.0f32; out.len()];
    const W: usize = 16;
    for i in 0..out.len() {
        let lo = i.saturating_sub(W);
        let acc: f32 = out[lo..=i].iter().sum();
        smooth[i] = acc / (i - lo + 1) as f32;
    }
    smooth
}

#[test]
fn pocsag_decodes_with_noise_at_moderate_snr() {
    let fs = 48_000.0f32;
    let burst = test_burst(fs);
    let mut seed = 0x1234_5678u32;
    let mut rng = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as f32 / u32::MAX as f32 - 0.5
    };
    let noise_amp = 1_500.0; // ~1/3 of full deviation: rough but decodable
    let noisy: Vec<f32> = burst.iter().map(|&x| x + noise_amp * rng()).collect();

    let mut dec = PocsagDecoder::new(f64::from(fs), 1200);
    for _ in 0..2 {
        dec.process(&noisy);
    }
    assert!(
        dec.diagnostics().syncs_1200 > 0,
        "no sync at moderate noise; bits={}",
        dec.diagnostics().bits_1200
    );
}

#[test]
fn autonotch_kills_a_stable_tone() {
    let fs = 48_000.0f64;
    let mut notch = AutoNotch::new(fs);
    let tone_hz = 800.0f32;
    let step = std::f32::consts::TAU * tone_hz / fs as f32;
    let mut phase = 0.0f32;
    let converge = (fs * 2.5) as usize;
    let measure_n = (fs * 0.5) as usize;
    // Reference: mean |sin| of a unit sine is 2/π.
    let before_ref = std::f64::consts::FRAC_2_PI;
    let mut after_sum = 0.0f64;
    let total = converge + measure_n;
    for i in 0..total {
        phase += step;
        let s = phase.sin();
        let mut buf = [s];
        notch.process(&mut buf);
        if i >= converge {
            after_sum += buf[0].abs() as f64;
        }
    }
    let mean_after = after_sum / measure_n as f64;
    let ratio = mean_after / before_ref.max(1e-9);
    assert!(
        ratio < 0.25,
        "tone only reduced to {ratio:.2} of original after convergence"
    );
}
