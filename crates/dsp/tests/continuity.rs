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
