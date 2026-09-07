// Verify DecodeChain output sample continuity across arbitrary block splits:
// total output length must equal floor(total_input/decim) regardless of
// chunking, and a ramp input must come through with no gaps/dupes.
use scannerd_dsp::DecodeChain;

#[test]
fn output_is_continuous_across_block_sizes() {
    let audio = 16_000.0f64;
    let cases: &[(f32, f64)] = &[
        (12_500.0, 960_000.0), // p25-ish
        (18_000.0, 960_000.0), // aprs
        (400.0, 192_000.0),
    ];
    for &(bw, rate) in cases {
        // build one long input fed whole vs in odd chunks; compare outputs
        let n = 300_000usize;
        let mut iq = Vec::with_capacity(n);
        let mut phi = 0.0f64;
        for _ in 0..n {
            phi += std::f64::consts::TAU * (100_000.0 + 1200.0) / rate;
            if phi > std::f64::consts::TAU {
                phi -= std::f64::consts::TAU;
            }
            iq.push(num_complex::Complex32::new(
                phi.cos() as f32,
                phi.sin() as f32,
            ));
        }
        let mut whole = DecodeChain::new(rate, bw, audio);
        whole.set_offset(100_000.0);
        let mut out_whole = Vec::new();
        whole.process(&iq, &mut out_whole);

        let mut chunked = DecodeChain::new(rate, bw, audio);
        chunked.set_offset(100_000.0);
        let mut out_chunked = Vec::new();
        let mut tmp = Vec::new();
        // deliberately awkward sizes incl. sub-hop bursts
        let mut pos = 0usize;
        let sizes = [273usize, 350, 16384, 91, 4096, 17];
        let mut k = 0;
        while pos < n {
            let take = sizes[k % sizes.len()].min(n - pos);
            chunked.process(&iq[pos..pos + take], &mut tmp);
            out_chunked.extend_from_slice(&tmp);
            pos += take;
            k += 1;
        }
        assert_eq!(
            out_whole.len(),
            out_chunked.len(),
            "bw={} rate={}: whole={} chunked={}",
            bw,
            rate,
            out_whole.len(),
            out_chunked.len()
        );
        // waveforms should match after the filter's group-delay warm-up
        let skip = 200;
        let mut maxdiff = 0.0f32;
        for (a, b) in out_whole[skip..].iter().zip(&out_chunked[skip..]) {
            maxdiff = maxdiff.max((a.re - b.re).abs().max((a.im - b.im).abs()));
        }
        assert!(
            maxdiff < 1e-3,
            "bw={} rate={}: waveform diverges, maxdiff={}",
            bw,
            rate,
            maxdiff
        );
    }
}

/// After `reset()`, a chain that had buffered history from one sample run must
/// behave EXACTLY like a freshly-constructed chain fed the same post-reset
/// input. This is the discontinuity guarantee: when the IQ timeline breaks (a
/// lost delivery or an epoch boundary), the samples buffered before the break
/// must not be stitched onto what comes after — a framing decoder handed that
/// seam would decode a waveform that never existed on the air.
#[test]
fn reset_severs_buffered_history() {
    let rate = 960_000.0f64;
    let bw = 12_500.0f32;
    let audio = 48_000.0f64;

    // Two unrelated signal runs. The first is fed to a chain that will be
    // reset mid-stream; the second is what we compare against.
    let run = |freq: f64, n: usize, seed_phase: f64| -> Vec<num_complex::Complex32> {
        let mut iq = Vec::with_capacity(n);
        let mut phi = seed_phase;
        for _ in 0..n {
            phi += std::f64::consts::TAU * freq / rate;
            if phi > std::f64::consts::TAU {
                phi -= std::f64::consts::TAU;
            }
            iq.push(num_complex::Complex32::new(
                phi.cos() as f32,
                phi.sin() as f32,
            ));
        }
        iq
    };
    let before = run(120_000.0, 40_000, 0.0);
    let after = run(50_000.0, 40_000, 1.7);

    // A: prime a chain, reset it, then feed the second run.
    let mut primed = DecodeChain::new(rate, bw, audio);
    primed.set_offset(120_000.0);
    let mut scratch = Vec::new();
    // Feed in a couple of blocks so real history accumulates in the FIRs.
    primed.process(&before[..20_000], &mut scratch);
    primed.process(&before[20_000..], &mut scratch);
    primed.set_offset(50_000.0);
    primed.reset();
    let mut out_primed = Vec::new();
    primed.process(&after, &mut out_primed);

    // B: a virgin chain fed only the second run.
    let mut fresh = DecodeChain::new(rate, bw, audio);
    fresh.set_offset(50_000.0);
    let mut out_fresh = Vec::new();
    fresh.process(&after, &mut out_fresh);

    assert_eq!(
        out_primed.len(),
        out_fresh.len(),
        "reset chain emitted {} samples, fresh emitted {}",
        out_primed.len(),
        out_fresh.len()
    );
    let mut maxdiff = 0.0f32;
    for (a, b) in out_primed.iter().zip(&out_fresh) {
        maxdiff = maxdiff.max((a.re - b.re).abs().max((a.im - b.im).abs()));
    }
    assert!(
        maxdiff < 1e-4,
        "reset() did not sever buffered history: output diverges from a fresh \
         chain by {maxdiff} — pre-reset samples leaked into the post-reset run"
    );
}
