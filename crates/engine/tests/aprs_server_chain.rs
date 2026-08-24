//! The DATA-mode APRS chain, exercised the way the server drives it.
//!
//! `run_aprs` conditions the IQ with a `FrontEnd`, mixes the 100 kHz LO
//! offset out with a `DecodeChain`, discriminates it, and feeds `AprsDecoder`
//! continuously in 16384-sample blocks. Everything below matches that.

use num_complex::Complex32;
use scannerd_dsp::DecodeChain;
use scannerd_engine::aprs::{self, AprsDecoder};
use scannerd_engine::nbfm::NbfmDemod;

const FS_IN: f64 = 960_000.0;
const IF_HZ: f64 = 100_000.0;
// Mirrors `data::APRS_BANDWIDTH_HZ`.
const BANDWIDTH: f32 = scannerd_engine::nbfm::NBFM_BANDWIDTH_HZ as f32;
const AUDIO_RATE: f64 = 16_000.0;
const BLOCK: usize = 16_384;
const DEV_HZ: f64 = 3_000.0;

struct Lcg(u32);
impl Lcg {
    fn uniform(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 16) as i16 as f32 / f32::from(i16::MAX)
    }
    /// Approximately Gaussian by the central limit theorem.
    fn gauss(&mut self) -> f32 {
        (0..4).map(|_| self.uniform()).sum::<f32>() * 0.5
    }
}

/// A packet at `amp`, `err_hz` off channel, between stretches of idle noise.
///
/// Idle means no carrier at all, which is what an FM discriminator turns into
/// full-scale noise -- the real state of 144.390 between bursts.
fn burst_on_idle_channel(amp: f32, noise: f32, err_hz: f64) -> Vec<Complex32> {
    let mut rng = Lcg(0x1234_5678);
    let frame = aprs::build_ui("APRS", "N1TEST-9", "!4134.62N/07324.48W-hello");
    let mut phase = 0.0f64;
    let mut t_audio = 0.0f64;
    let burst = aprs::modulate_burst(
        &frame,
        FS_IN,
        IF_HZ + err_hz,
        amp,
        DEV_HZ,
        &mut phase,
        &mut t_audio,
    );

    let quiet = FS_IN as usize / 10;
    let mut iq: Vec<Complex32> = (0..quiet)
        .map(|_| Complex32::new(noise * rng.gauss(), noise * rng.gauss()))
        .collect();
    iq.extend(
        burst
            .iter()
            .map(|c| Complex32::new(c.re + noise * rng.gauss(), c.im + noise * rng.gauss())),
    );
    iq.extend((0..quiet).map(|_| Complex32::new(noise * rng.gauss(), noise * rng.gauss())));
    iq
}

/// Decode with an explicit channel width, to compare filter settings.
fn decode_bw(iq: &[Complex32], bandwidth: f32) -> usize {
    let mut cleanup = scannerd_dsp::FrontEnd::without_blanker(FS_IN);
    let mut chain = DecodeChain::new(FS_IN, bandwidth, AUDIO_RATE);
    chain.set_offset(IF_HZ);
    let fs = chain.fs_out();
    let mut nbfm = NbfmDemod::new(fs);
    let mut decoder = AprsDecoder::new(fs);
    let (mut clean, mut iq_ch, mut disc) = (Vec::new(), Vec::new(), Vec::new());
    let mut n = 0;
    for block in iq.chunks(BLOCK) {
        clean.clear();
        clean.extend_from_slice(block);
        cleanup.process(&mut clean);
        iq_ch.clear();
        chain.process(&clean, &mut iq_ch);
        disc.clear();
        nbfm.discriminator_hz(&iq_ch, &mut disc);
        n += decoder.process(&disc).len();
    }
    n
}

fn burst_seeded(amp: f32, noise: f32, seed: u32) -> Vec<Complex32> {
    let mut rng = Lcg(seed);
    let frame = aprs::build_ui("APRS", "N1TEST-9", "!4134.62N/07324.48W-hello");
    let mut phase = 0.0f64;
    let mut t_audio = 0.0f64;
    let burst = aprs::modulate_burst(&frame, FS_IN, IF_HZ, amp, DEV_HZ, &mut phase, &mut t_audio);
    let quiet = FS_IN as usize / 20;
    let mut iq: Vec<Complex32> = (0..quiet)
        .map(|_| Complex32::new(noise * rng.gauss(), noise * rng.gauss()))
        .collect();
    iq.extend(
        burst
            .iter()
            .map(|c| Complex32::new(c.re + noise * rng.gauss(), c.im + noise * rng.gauss())),
    );
    iq.extend((0..quiet).map(|_| Complex32::new(noise * rng.gauss(), noise * rng.gauss())));
    iq
}

/// The channel filter must stay matched to Bell 202, not to the 18 kHz it
/// used to be set to.
///
/// `DecodeChain` clamps the requested width to `fs_out * 0.45`, so 18 kHz was
/// really a +/-7.2 kHz channel: 31% more noise power than the signal needs,
/// and wide enough to reach into the neighbouring 15 kHz channels. Measured
/// over a noise sweep that is worth roughly 2 dB, which is the difference
/// between decoding and not for a marginal packet.
#[test]
fn the_narrow_channel_filter_beats_the_old_wide_one() {
    let noise = 1.4f32;
    let (mut narrow, mut wide) = (0, 0);
    for seed in 0..12u32 {
        let iq = burst_seeded(
            0.30,
            noise,
            0x1234_5678u32.wrapping_add(seed.wrapping_mul(2_654_435_761)),
        );
        narrow += usize::from(decode_bw(&iq, BANDWIDTH) >= 1);
        wide += usize::from(decode_bw(&iq, 18_000.0) >= 1);
    }
    assert!(
        narrow > wide,
        "narrow channel ({BANDWIDTH:.0} Hz) recovered {narrow}/12 vs {wide}/12 wide; \
         the filter is no longer buying anything"
    );
}

/// Number of packets the chain recovers from `iq`.
fn decode(iq: &[Complex32]) -> usize {
    let mut cleanup = scannerd_dsp::FrontEnd::without_blanker(FS_IN);
    let mut chain = DecodeChain::new(FS_IN, BANDWIDTH, AUDIO_RATE);
    chain.set_offset(IF_HZ);
    let fs = chain.fs_out();
    assert_eq!(fs, AUDIO_RATE, "960 kS/s must decimate exactly to 16 kHz");
    let mut nbfm = NbfmDemod::new(fs);
    let mut decoder = AprsDecoder::new(fs);
    let (mut clean, mut iq_ch, mut disc) = (Vec::new(), Vec::new(), Vec::new());

    let mut n = 0;
    for block in iq.chunks(BLOCK) {
        clean.clear();
        clean.extend_from_slice(block);
        cleanup.process(&mut clean);
        iq_ch.clear();
        chain.process(&clean, &mut iq_ch);
        disc.clear();
        nbfm.discriminator_hz(&iq_ch, &mut disc);
        n += decoder.process(&disc).len();
    }
    n
}

/// The chain must recover a burst that arrives on an idle, noisy channel.
#[test]
fn a_burst_between_stretches_of_idle_noise_decodes() {
    assert!(
        decode(&burst_on_idle_channel(0.30, 0.05, 0.0)) >= 1,
        "no packet recovered from a clean burst"
    );
}

/// Weak signals and receiver frequency error are the normal case, not the
/// exception: 1 ppm of RTL-SDR error is 144 Hz at 144.390 MHz.
#[test]
fn weak_and_off_frequency_bursts_still_decode() {
    for (amp, err_hz) in [
        (0.10f32, 0.0f64),
        (0.05, 0.0),
        (0.30, 1_000.0),
        (0.30, 2_000.0),
        (0.10, 2_000.0),
    ] {
        assert!(
            decode(&burst_on_idle_channel(amp, 0.05, err_hz)) >= 1,
            "amplitude {amp} at {err_hz} Hz off channel decoded nothing"
        );
    }
}

/// An idle channel must not manufacture packets, however long it runs.
#[test]
fn idle_noise_alone_decodes_nothing() {
    let mut rng = Lcg(0x9e37_79b9);
    let iq: Vec<Complex32> = (0..(2 * FS_IN as usize))
        .map(|_| Complex32::new(0.05 * rng.gauss(), 0.05 * rng.gauss()))
        .collect();
    assert_eq!(decode(&iq), 0, "receiver noise synthesized a packet");
}
