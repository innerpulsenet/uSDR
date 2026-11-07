//! Taking decoded signals out of the slot, so that a second pass can find
//! the weaker ones they covered.
//!
//! Each decode is rebuilt as a unit complex exponential following its
//! tones, the audio is mixed down by it, and what is left at 0 Hz, smoothed
//! over a third of a second, is the signal's amplitude and phase as fading
//! and a little frequency error move them. That times the reference is the
//! signal as received, and it is subtracted. WSJT-X takes signals out the
//! same way.

use super::Protocol;
use super::demod;
use num_complex::{Complex32, Complex64};

/// Width in samples of each of the two boxcars, one after the other a
/// triangle a third of a second wide at the decoder's rate, that smooth the
/// signal's amplitude: wide enough to average the noise down, narrow
/// enough to follow fading and a fraction of a hertz of error.
const SMOOTH: usize = 2_000;

/// Take the transmission of `tones`, its lowest tone at `f0`, starting
/// `start_s` into the slot, out of `audio` at `rate`.
///
/// The start the decoder found is good to a thirty-second of a symbol, and
/// a few milliseconds off is a phase error that grows with the tone sent,
/// which leaves most of a high tone behind; so the start is refined here
/// first, to the sample, by how well the reference lines up symbol by
/// symbol.
pub fn subtract(protocol: Protocol, audio: &mut [f32], tones: &[u8], f0: f32, start_s: f32, rate: f32) {
    let reference = demod::gfsk_complex(protocol, tones, f0, rate);
    let sps = (rate * protocol.symbol_period()).round() as usize;
    let coarse = (start_s * rate).round() as isize;
    let fit = |at: isize| -> f32 {
        let mut total = 0.0;
        for (k, sym) in reference.chunks(sps).enumerate() {
            let mut acc = Complex32::default();
            for (m, r) in sym.iter().enumerate() {
                let n = at + (k * sps + m) as isize;
                if n >= 0
                    && let Some(&x) = audio.get(n as usize)
                {
                    acc += r.conj() * x;
                }
            }
            total += acc.norm();
        }
        total
    };
    // The decoder's step is 1/32 of a symbol, 60 samples for FT8: a coarse
    // look a step either way, then halving steps toward the best.
    let reach = (sps / 32) as isize;
    let mut step = (reach / 4).max(1);
    let mut best = (f32::MIN, coarse);
    let mut d = -reach;
    while d <= reach {
        let s = fit(coarse + d);
        if s > best.0 {
            best = (s, coarse + d);
        }
        d += step;
    }
    while step > 1 {
        step /= 2;
        let center = best.1;
        for at in [center - step, center + step] {
            let s = fit(at);
            if s > best.0 {
                best = (s, at);
            }
        }
    }
    let at = best.1;
    // The signal's slowly moving amplitude: the audio mixed down by the
    // reference and smoothed, divided by how much of the smoothing window
    // fell on the slot, so the ends are not underestimated.
    let mut mixed = vec![Complex64::default(); reference.len()];
    let mut inside = vec![Complex64::default(); reference.len()];
    for (m, r) in reference.iter().enumerate() {
        let n = at + m as isize;
        if n >= 0
            && let Some(&x) = audio.get(n as usize)
        {
            mixed[m] = Complex64::new(r.re as f64, -r.im as f64) * x as f64;
            inside[m] = Complex64::new(1.0, 0.0);
        }
    }
    let mixed = boxcar(&boxcar(&mixed));
    let inside = boxcar(&boxcar(&inside));
    for (m, r) in reference.iter().enumerate() {
        let n = at + m as isize;
        if n < 0 || n as usize >= audio.len() || inside[m].re <= 1e-9 {
            continue;
        }
        let h = mixed[m] / inside[m].re;
        let r = Complex64::new(r.re as f64, r.im as f64);
        // Real audio mixed down keeps half the signal: the other half went
        // to twice the frequency, which the smoothing removed.
        audio[n as usize] -= (2.0 * (h * r).re) as f32;
    }
}

/// A centered running sum [`SMOOTH`] samples wide, in double precision so
/// the running total does not drift over a slot.
fn boxcar(x: &[Complex64]) -> Vec<Complex64> {
    let mut prefix = Vec::with_capacity(x.len() + 1);
    let mut acc = Complex64::default();
    prefix.push(acc);
    for v in x {
        acc += v;
        prefix.push(acc);
    }
    let half = SMOOTH / 2;
    (0..x.len())
        .map(|i| {
            let lo = i.saturating_sub(half);
            let hi = (i + SMOOTH - half).min(x.len());
            prefix[hi] - prefix[lo]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::{CallHashes, RATE_HZ, pack};
    use super::*;

    #[test]
    fn a_lone_signal_comes_out_by_more_than_twenty_db() {
        for p in [Protocol::Ft8, Protocol::Ft4] {
            let payload = pack::pack("CQ K1ABC FN42", &mut CallHashes::default()).unwrap();
            let tones = demod::tones(p, &payload);
            let (f0, start) = (1234.56, 0.73);
            let wave = demod::gfsk(p, &tones, f0, RATE_HZ);
            let at = (start * RATE_HZ).round() as usize;
            let mut audio = vec![0f32; (p.slot_secs() * RATE_HZ as f64) as usize];
            // Some fading and a phase, as a real signal brings.
            for (i, w) in wave.iter().enumerate() {
                let t = i as f32 / RATE_HZ;
                audio[at + i] = 0.5 * (1.0 + 0.3 * (0.5 * t).sin()) * w;
            }
            let before: f32 = audio.iter().map(|x| x * x).sum();
            // A fifth of a hertz and 2 ms off, as the decoder may report it.
            subtract(p, &mut audio, &tones, f0 + 0.2, start + 0.002, RATE_HZ);
            let after: f32 = audio.iter().map(|x| x * x).sum();
            let db = 10.0 * (after / before).log10();
            assert!(db < -20.0, "{}: {db:.1} dB left", p.label());
        }
    }
}
