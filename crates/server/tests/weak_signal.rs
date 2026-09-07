//! Weak-signal regression tests: the decoders must keep working as SNR
//! drops, and the audio auto-notch must kill a stable tone.
//!
//! The POCSAG fixtures assert exact payload (capcode + text), not just sync
//! detection: a decoder that locks sync and emits garbage must fail here.
//! The burst builder mirrors the engine's proven `burst_audio` encoding
//! (MSB-first alpha packing, idle-filled batches, ±4.5 kHz square wave)
//! with an added IF-filter smoothing pass, and noise is injected on the
//! discriminator-Hz waveform the decoder actually consumes.

use scannerd_engine::autonotch::AutoNotch;
use scannerd_engine::pocsag::{PocsagDecoder, PocsagMessage};

const SYNC: u32 = 0x7CD2_15D8;
const IDLE: u32 = 0x7A89_C197;
/// BCH(31,21) generator x^10+x^9+x^8+x^6+x^5+x^3+1.
const GEN: u32 = 0b111_0110_1001;
const WORDS_PER_BATCH: usize = 16;

fn syndrome(mut v: u32) -> u32 {
    for i in (10..31).rev() {
        if v >> i & 1 == 1 {
            v ^= GEN << (i - 10);
        }
    }
    v & 0x3FF
}

/// Build a 32-bit codeword: 20 data bits (30..11) + BCH + even parity.
fn codeword(data20: u32, message: bool) -> u32 {
    let bch_data = (data20 << 11 | u32::from(message) << 31) >> 1;
    let bch = bch_data | syndrome(bch_data);
    bch << 1 | (bch.count_ones() & 1)
}

/// 7-bit ASCII string → message words, groups MSB-first as the decoder
/// expects (matches the engine's round-trip test helper).
fn alpha_words(s: &str) -> Vec<u32> {
    let mut bits = Vec::new();
    for c in s.bytes() {
        for j in 0..7 {
            bits.push(c >> j & 1 == 1);
        }
    }
    bits.chunks(20)
        .map(|g| {
            g.iter()
                .enumerate()
                .fold(0u32, |v, (j, &b)| v | u32::from(b) << (19 - j))
        })
        .collect()
}

/// Preamble + sync + one batch holding the address (18-bit `addr18`,
/// function bits, frame `frame`) followed by the message words; the rest of
/// the batch is idle. Rendered as a ±4.5 kHz square, then softened with a
/// 16-tap moving average like a real transmitter's IF filter.
fn burst_audio(fs: f32, baud: u32, addr18: u32, function: u8, frame: usize, msg: &[u32]) -> Vec<f32> {
    let addr_word = codeword(addr18 << 2 | u32::from(function), false);
    let mut batch = vec![IDLE; WORDS_PER_BATCH];
    batch[frame * 2] = addr_word;
    let mut msg_idx = 0;
    for i in (frame * 2 + 1)..WORDS_PER_BATCH {
        if msg_idx < msg.len() {
            batch[i] = codeword(msg[msg_idx], true);
            msg_idx += 1;
        }
    }
    let mut all_words = vec![SYNC];
    all_words.extend(batch);

    let mut bits = Vec::new();
    for i in 0..600 {
        bits.push(i % 2 == 0);
    }
    for w in all_words {
        for i in (0..32).rev() {
            bits.push(w >> i & 1 == 1);
        }
    }
    let spb = fs / baud as f32;
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

/// Deterministic xorshift noise scaled to `amp`.
fn add_noise(sig: &[f32], amp: f32, seed: u32) -> Vec<f32> {
    let mut s = seed;
    let mut rng = move || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s as f32 / u32::MAX as f32 - 0.5
    };
    sig.iter().map(|&x| x + amp * rng()).collect()
}

fn decode_all(fs: f32, baud: u32, audio: &[f32]) -> (Vec<PocsagMessage>, PocsagDecoder) {
    let mut dec = PocsagDecoder::new(f64::from(fs), baud);
    // One pass per burst, then a quiet tail so the final batch flushes.
    // Feeding the same burst twice would decode it twice — that is correct
    // decoder behavior, not junk, but it muddies exact-count assertions.
    let mut msgs = dec.process(audio);
    msgs.extend(dec.process(&[0.0f32; 2048]));
    (msgs, dec)
}

/// The fixture burst itself must round-trip to the exact intended page:
/// capcode, function, text, and completeness. Guards the fixture builder —
/// if this fails, the noisy tests below prove nothing.
#[test]
fn pocsag_fixture_round_trips_to_exact_payload() {
    let fs = 48_000.0f32;
    let burst = burst_audio(fs, 1200, 0x12345, 3, 2, &alpha_words("HI73"));
    let (msgs, _dec) = decode_all(fs, 1200, &burst);

    let want_capcode = (0x12345u32 << 3) | 2;
    let mine: Vec<&PocsagMessage> = msgs.iter().filter(|m| m.capcode == want_capcode).collect();
    assert_eq!(mine.len(), 1, "expected exactly one page, got {msgs:#?}");
    let m = mine[0];
    assert_eq!(m.text, "HI73", "payload text mismatch: {m:#?}");
    assert_eq!(m.function, 3);
    assert_eq!(m.baud, 1200);
    assert!(!m.partial, "clean burst must decode complete: {m:#?}");
    // No junk beyond our page.
    assert_eq!(
        msgs.len(),
        1,
        "clean burst produced unexpected extra messages: {msgs:#?}"
    );
}

/// Moderate discriminator noise: the exact page must still come through —
/// same capcode, same text — and nothing else may be invented.
#[test]
fn pocsag_exact_payload_survives_moderate_noise() {
    let fs = 48_000.0f32;
    let burst = burst_audio(fs, 1200, 0x12345, 3, 2, &alpha_words("HI73"));
    // ~1/3 of full deviation: rough but decodable.
    let noisy = add_noise(&burst, 1_500.0, 0x1234_5678);
    let (msgs, dec) = decode_all(fs, 1200, &noisy);

    assert!(
        dec.diagnostics().syncs_1200 > 0,
        "no sync at moderate noise; bits={}",
        dec.diagnostics().bits_1200
    );
    let want_capcode = (0x12345u32 << 3) | 2;
    let mine: Vec<&PocsagMessage> = msgs.iter().filter(|m| m.capcode == want_capcode).collect();
    assert!(
        mine.iter().any(|m| m.text == "HI73"),
        "exact page not recovered under noise: {msgs:#?}"
    );
    // Junk control: noise must not manufacture unrelated pages.
    let junk: Vec<&PocsagMessage> = msgs.iter().filter(|m| m.capcode != want_capcode).collect();
    assert!(
        junk.is_empty(),
        "noise produced {} junk message(s): {junk:#?}",
        junk.len()
    );
}

/// Pure-noise control: uniform ±4.5 kHz-band noise with no burst must yield
/// no syncs and no messages at all.
#[test]
fn pocsag_pure_noise_yields_no_pages() {
    let fs = 48_000.0f32;
    let burst = burst_audio(fs, 1200, 0x12345, 3, 2, &alpha_words("HI73"));
    let noise_only = add_noise(&vec![0.0f32; burst.len()], 4_500.0, 0x99AA_BBCC);
    let (msgs, dec) = decode_all(fs, 1200, &noise_only);
    assert_eq!(dec.diagnostics().syncs_1200, 0, "false syncs on pure noise");
    assert!(msgs.is_empty(), "pure noise produced pages: {msgs:#?}");
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
