//! Time the receive chain's stages on synthetic 6 MS/s IQ:
//! `cargo run --release -p scannerd-dsp --example bench_chain`.
use num_complex::Complex32;
use scannerd_dsp::{DecodeChain, FrontEnd, HalfbandChain, Nco, Spectrum};
use std::time::Instant;

fn main() {
    let fs = 6.0e6f64;
    let block = 120_000usize; // 20 ms, as the device thread delivers
    let secs = 5usize;
    let mut seed = 0x1234_5678u32;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as f32 / u32::MAX as f32 - 0.5
    };
    let blocks: Vec<Vec<Complex32>> = (0..8)
        .map(|b| {
            (0..block)
                .map(|i| {
                    let t = (b * block + i) as f32 / fs as f32;
                    let ph = 2.0 * std::f32::consts::PI * 750e3 * t;
                    Complex32::new(0.3 * ph.cos() + 0.01 * rnd(), 0.3 * ph.sin() + 0.01 * rnd())
                })
                .collect()
        })
        .collect();
    let n = secs * 50;
    let time = |name: &str, f: &mut dyn FnMut(&mut Vec<Complex32>)| {
        let mut work = blocks[0].clone();
        let t0 = Instant::now();
        for i in 0..n {
            work.clear();
            work.extend_from_slice(&blocks[i % blocks.len()]);
            f(&mut work);
        }
        let ms = t0.elapsed().as_secs_f64() * 1e3 / secs as f64;
        println!("{name:<28} {ms:7.1} ms per second of signal ({:.1}% of a core)", ms / 10.0);
    };
    time("copy only", &mut |_w| {});
    let mut fe = FrontEnd::without_blanker(fs);
    time("front end (dc, iq image)", &mut |w| fe.process(w));
    // A front end with a little gain and phase imbalance keeps the image
    // corrector running, as a real tuner does: its two transforms per block
    // never go idle.
    let mut fe = FrontEnd::without_blanker(fs);
    time("front end, corrector on", &mut |w| {
        for s in w.iter_mut() {
            *s = Complex32::new(s.re, s.im * 1.02 + s.re * 0.01);
        }
        fe.process(w)
    });
    for size in [1024usize, 4096] {
        let mut sp = Spectrum::new(size);
        let mut avg = Vec::new();
        time(&format!("spectrum welch {size}"), &mut |w| {
            sp.ingest_welch(w);
            sp.take_dbfs(&mut avg, None);
        });
    }
    let mut nco = Nco::new();
    nco.set_freq(750e3, fs);
    let mut mixed = Vec::new();
    time("nco mix", &mut |w| nco.mix(w, &mut mixed));
    for (name, bw, rate) in [("chain nfm 12.5k @48k", 12_500.0f32, 48_000.0), ("chain wfm 180k @261k", 180_000.0, 260_870.0)] {
        let mut ch = DecodeChain::new(fs, bw, rate);
        ch.set_offset(750e3);
        let mut out = Vec::new();
        time(name, &mut |w| ch.process(w, &mut out));
    }
    // Wideband decoder channels as the server cuts them from the span: each
    // mixed and halved seven times, twelve of them on every delivery, each
    // with scratch buffers of its own. The whole delivery at once, and in
    // tiles that keep one channel's scratch in the cache.
    for tile in [usize::MAX, 16_384, 8_192, 4_096] {
        struct Channel {
            nco: Nco,
            chain: HalfbandChain,
            mixed: Vec<Complex32>,
            part: Vec<Complex32>,
            out: Vec<Complex32>,
        }
        let mut channels: Vec<Channel> = (0..12)
            .map(|k| {
                let mut nco = Nco::new();
                nco.set_freq(-2.5e6 + 400e3 * k as f64, fs);
                Channel { nco, chain: HalfbandChain::new(fs, 6_000.0, 7), mixed: Vec::new(), part: Vec::new(), out: Vec::new() }
            })
            .collect();
        let name = if tile == usize::MAX { "12 wide channels, whole".to_string() } else { format!("12 wide channels, {tile}") };
        time(&name, &mut |w| {
            for c in &mut channels {
                c.out.clear();
                for chunk in w.chunks(tile.min(w.len().max(1))) {
                    c.nco.mix(chunk, &mut c.mixed);
                    c.chain.process(&c.mixed, &mut c.part);
                    c.out.extend_from_slice(&c.part);
                }
            }
        });
    }
}
