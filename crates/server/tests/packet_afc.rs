//! End-to-end check of the packet path as `run_sdr` drives it: an off-centre
//! POCSAG carrier, the AFC steering the chain onto it, and the external
//! pager bank (the classifier's own being idle) recovering frames.

use num_complex::Complex32;
use scannerd_dsp::DecodeChain;
use scannerd_engine::pocsag::PocsagDecoder;
use scannerd_engine::{Afc, SignalClassifier};

const FS_IN: f64 = 2_048_000.0;
const BLOCK: usize = 16_384;

/// Turn discriminator hertz into complex baseband at `fs`, carrier parked at
/// `offset_hz` from where the receiver will mix.
fn burst_iq(hz_samples: &[f32], fs: f32, offset_hz: f32) -> Vec<Complex32> {
    let mut phase = 0.0f32;
    let step = std::f32::consts::TAU * offset_hz / fs;
    hz_samples
        .iter()
        .map(|&h| {
            phase += std::f32::consts::TAU * h / fs + step;
            Complex32::new(phase.cos(), phase.sin())
        })
        .collect()
}

/// Deterministic PRNG so failures reproduce exactly.
struct Rng(u64);

impl Rng {
    fn next_f32(&mut self) -> f32 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (v >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
    }
    /// Roughly Gaussian, sum of four uniforms.
    fn gauss(&mut self) -> f32 {
        (self.next_f32() + self.next_f32() + self.next_f32() + self.next_f32()) * 0.5
    }
}

/// Add AWGN at `sigma` per component — a real receiver never delivers a
/// signal without a noise floor beneath it, and the classifier's SNR (and
/// therefore the AFC lock gate) is measured against that floor.
fn add_noise(iq: &mut [Complex32], sigma: f32, rng: &mut Rng) {
    for c in iq.iter_mut() {
        c.re += rng.gauss() * sigma;
        c.im += rng.gauss() * sigma;
    }
}

#[test]
fn an_off_centre_pocsag_carrier_is_afc_tracked_and_decoded() {
    let mut chain = DecodeChain::new(FS_IN, 25_000.0, 48_000.0);
    // The operator clicked 2500 Hz below the true carrier.
    const TRUE_OFFSET: f32 = 2_500.0;
    chain.set_offset(-TRUE_OFFSET as f64);
    let fs_out = chain.fs_out();
    let mut classifier = SignalClassifier::new(fs_out);
    classifier.use_external_pagers();
    let mut pocsag = PocsagDecoder::new(fs_out, 1200);
    let mut afc = Afc::new(0.0, 5_000.0);
    let mut last_center_offset = 0.0f32;
    let mut last_reported = 0.0f32;

    // Build one POCSAG batch stream at the chain's output rate.
    // Bit durations must be in INPUT-rate samples; the chain decimates
    // afterwards.
    let audio = pocsag_burst_discriminator(FS_IN as f32);

    // Baseline sanity: the same burst, on-centre and without AFC, must at
    // least produce POCSAG syncs. If it cannot, this test's synthesised
    // codewords are wrong and the AFC assertion would be meaningless.
    {
        let mut chain0 = DecodeChain::new(FS_IN, 25_000.0, 48_000.0);
        chain0.set_offset(0.0);
        let fs0 = chain0.fs_out();
        let mut cls0 = SignalClassifier::new(fs0);
        cls0.use_external_pagers();
        let mut dec0 = PocsagDecoder::new(fs0, 1200);
        let mut baseline_blocks = 0usize;
        let mut total_disc = 0usize;
        let mut last_peak = 0.0f32;
        // Both FIR stages warm up over thousands of outputs; 60 s of lead
        // guarantees the burst lands on a settled chain.
        let lead = vec![0.0f32; FS_IN as usize * 8];
        let iq0 = burst_iq(&[lead, audio.clone()].concat(), FS_IN as f32, 0.0);
        // Radio-sized blocks: 16384 input samples ≈ one USB block.
        for chunk in iq0.chunks(16_384) {
            let mut out = Vec::new();
            chain0.process(chunk, &mut out);
            if out.is_empty() {
                continue;
            }
            let _ = cls0.process(&out, 929_585_000.0);
            let disc = cls0.monitor_discriminator_hz();
            baseline_blocks += disc.len();
            let peak = disc.iter().fold(0.0f32, |a, &v| a.max(v.abs()));
            if peak > last_peak {
                last_peak = peak;
            }
            total_disc += disc.len();
            dec0.process(disc);
            if dec0.diagnostics().syncs_1200 > 0 {
                break;
            }
        }
        {
            let mut praw = PocsagDecoder::new(f64::from(fs_out), 1200);
            for _ in 0..3 {
                praw.process(&audio);
            }
            eprintln!(
                "raw-audio probe: syncs={} bits={}",
                praw.diagnostics().syncs_1200,
                praw.diagnostics().bits_1200
            );
        }
        // Bisect: does a decoder fed the chain output through a bare
        // NbfmDemod (no classifier) find syncs?
        {
            use scannerd_engine::nbfm::NbfmDemod;
            let mut nfm = NbfmDemod::new(f64::from(fs_out));
            let mut pdec = PocsagDecoder::new(f64::from(fs_out), 1200);
            let mut dbuf = Vec::new();
            let mut last_mag = -1.0f32;
            for chunk in iq0.chunks(16_384) {
                let mut out = Vec::new();
                chain0.process(chunk, &mut out);
                if out.is_empty() {
                    continue;
                }
                let mut d2 = Vec::new();
                nfm.discriminator_hz(&out, &mut d2);
                dbuf.extend_from_slice(&d2);
                let peakd = d2.iter().fold(0.0f32, |a, &v| a.max(v.abs()));
                if peakd > 100.0 && last_mag < 0.0 {
                    eprintln!(
                        "disc sample: peak={peakd:.0} head={:?}",
                        &d2[..d2.len().min(16)]
                    );
                    last_mag = 1.0;
                }
            }
            pdec.process(&dbuf);
            eprintln!(
                "bare-nbfm probe: syncs={} bits={}",
                pdec.diagnostics().syncs_1200,
                pdec.diagnostics().bits_1200
            );
            let _ = pdec;
        }
        // Collect the chain's discriminator for one burst and diff its
        // polarity pattern against the raw audio's, block-aligned by peak
        // cross-correlation. Print agreement fraction.
        {
            let mut disc_all: Vec<f32> = Vec::new();
            for chunk in iq0.chunks(16_384) {
                let mut out = Vec::new();
                chain0.process(chunk, &mut out);
                if out.is_empty() {
                    continue;
                }
                let _ = cls0.process(&out, 929_585_000.0);
                disc_all.extend_from_slice(cls0.monitor_discriminator_hz());
            }
            let rms = |v: &[f32]| -> f32 {
                (v.iter().map(|x| x * x).sum::<f32>() / v.len().max(1) as f32).sqrt()
            };
            // Lead occupies most of the front; compare first vs last tenth.
            let tenth = disc_all.len() / 10;
            eprintln!(
                "chain disc rms: lead={:.0} tail={:.0} n={}",
                rms(&disc_all[..tenth]),
                rms(&disc_all[disc_all.len() - tenth..]),
                disc_all.len()
            );
            // And dump 40 consecutive tail values to eyeball the waveform.
            let t = &disc_all[disc_all.len() - 40..];
            eprintln!("tail40: {:?}", t.iter().map(|v| v.round() as i16).collect::<Vec<_>>());
        }
        assert!(
            dec0.diagnostics().syncs_1200 > 0,
            "baseline: {total_disc} samples, peak={last_peak:.0}, bits diag: 512={} 1200={} 2400={}",
            dec0.diagnostics().bits_512,
            dec0.diagnostics().bits_1200,
            dec0.diagnostics().bits_2400
        );
    }

    let mut decoded = false;
    let mut blocks_processed = 0usize;
    let lead = vec![0.0f32; FS_IN as usize * 8];
    let full = [lead, audio.clone()].concat();
    // The monitor gate run_sdr keeps for the inspect path, rated for the
    // chain's output like the real one.
    let mut mon_gate = scannerd_engine::NoiseGate::new(f64::from(fs_out));
    let mut last_snr_db = 0.0f32;
    // A receiver's noise floor: the added noise sets the span floor the
    // SNR is measured over, and the AFC lock gate opens only on a signal
    // well above it — as on a real radio.
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    // σ chosen so the burst clears the 8 dB lock bar (~15 dB span SNR)
    // while the quieted carrier still pulls the gate's noise band under
    // its close threshold — a real marginal-but-decodable receiver.
    let noise_sigma = 0.12f32;
    let mut p_floor = f32::INFINITY;
    // Realistic warm-up: the receiver boots into static before the pager
    // transmits, so the classifier's minimum-statistics floor is set on
    // noise (as it is on a real radio) and the SNR lock bar is meaningful.
    let warmup_samples = (FS_IN as usize) * 2;
    let mut warm_iq = Vec::with_capacity(384);
    for _ in (0..warmup_samples).step_by(384) {
        warm_iq.clear();
        warm_iq.resize(384, Complex32::new(0.0, 0.0));
        add_noise(&mut warm_iq, noise_sigma, &mut rng);
        let mut out = Vec::new();
        chain.process(&warm_iq, &mut out);
        if out.is_empty() {
            continue;
        }
        let _ = classifier.process(&out, 929_585_000.0);
        let mut voice = classifier.monitor_voice().to_vec();
        mon_gate.process(&mut voice, classifier.monitor_noise());
        last_snr_db = 0.0; // static: never locked during warm-up
    }
    'outer: for _repeat in 0..8 {
        let mut iq = burst_iq(&full, FS_IN as f32, TRUE_OFFSET);
        add_noise(&mut iq, noise_sigma, &mut rng);
        // ~380 samples per block at 47.6 kHz, matching the radio's cadence.
        for chunk in iq.chunks(384) {
            let mut out = Vec::new();
            chain.process(chunk, &mut out);
            if out.is_empty() {
                continue;
            }
            // Exactly what run_sdr now does each block: steer on the
            // previous block's lock evidence (squelch-open SNR plus an
            // open monitor gate), then classify, then advance the gate.
            // run_sdr's SNR is span power over a min-statistics noise
            // floor (channel_dbfs - noise_dbfs), not the classifier's,
            // so measure it the same way here.
            let p = out.iter().map(|c| c.norm_sqr()).sum::<f32>() / out.len() as f32;
            p_floor = p_floor.min(p);
            last_snr_db = 10.0 * (p / p_floor.max(1e-12)).log10();
            let locked =
                last_snr_db >= scannerd_engine::squelch::DEFAULT_OPEN_DB && mon_gate.is_open();
            afc.observe(last_center_offset, locked);
            let corr = afc.correction_hz();
            if (corr - last_reported).abs() >= 50.0 {
                chain.set_offset(f64::from(corr));
                last_reported = corr;
            }
            let c = classifier.process(&out, 929_585_000.0);
            last_center_offset = c.center_offset_hz;
            let mut voice = classifier.monitor_voice().to_vec();
            mon_gate.process(&mut voice, classifier.monitor_noise());
            blocks_processed += 1;
            for _msg in pocsag.process(classifier.monitor_discriminator_hz()) {
                decoded = true;
                break 'outer;
            }
        }
    }
    if !decoded {
        panic!(
            "no POCSAG frame in {blocks_processed} blocks; afc ended at {} Hz, last dc {}",
            afc.correction_hz(),
            last_center_offset
        );
    }
    // And the loop must have steered toward the carrier, not away.
    assert!(
        (afc.correction_hz() - TRUE_OFFSET).abs() < 1_000.0,
        "AFC settled at {} Hz, expected near {TRUE_OFFSET}",
        afc.correction_hz()
    );
}

/// A short alphanumeric POCSAG burst as discriminator hertz, at `fs`.
fn pocsag_burst_discriminator(fs: f32) -> Vec<f32> {
    // Sync word + one address word + two message words, BCH-encoded exactly
    // as the engine does it (pocsag.rs codeword/syndrome).
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
    // capcode 0x12345 function 2, frame 2 → address codeword payload.
    let addr_payload = (0x12345 << 3) | 2;
    let msg_a: u32 = ascii_word("HE");
    let msg_b: u32 = ascii_word("Y ");
    let words = [
        SYNC,
        codeword(addr_payload, false),
        codeword(msg_a, true),
        codeword(msg_b, true),
    ];
    let mut bits: Vec<bool> = (0..576).map(|i| i % 2 == 0).collect(); // preamble
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
    // A real transmitter's IF filter rounds the NRZ edges; without that the
    // ideal square wave rings the receiver's anti-alias filters and the
    // sliced bits pick up intersymbol interference the air never has.
    let mut smooth = vec![0.0f32; out.len()];
    const W: usize = 16;
    for i in 0..out.len() {
        let lo = i.saturating_sub(W);
        let acc: f32 = out[lo..=i].iter().sum();
        smooth[i] = acc / (i - lo + 1) as f32;
    }
    smooth
}

/// Pack ASCII chars into a 20-bit message-word payload, LSB-first per char,
/// matching the engine's alpha_words().
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

#[test]
fn synthesised_codewords_sync_a_bare_decoder() {
    use scannerd_engine::pocsag::PocsagDecoder;
    let fs = 48_000.0f32;
    let audio = pocsag_burst_discriminator(fs);
    let mut dec = PocsagDecoder::new(f64::from(fs), 1200);
    for _ in 0..3 {
        for &x in &audio {
            dec.process(&[x]);
        }
    }
    let d = dec.diagnostics();
    assert!(d.syncs_1200 > 0, "bare decoder found no sync; bits 512={} 1200={} 2400={}", d.bits_512, d.bits_1200, d.bits_2400);
}
