//! Audio at whatever rate the sound card runs (47 904 Hz live, not a round
//! number) brought to exactly 8 kHz, the rate MFSK, Olivia and Feld Hell
//! were defined at. Their symbol lengths are then whole numbers of samples
//! and their tones fall on FFT bins, as the modes' designers intended.
//!
//! A windowed-sinc interpolator evaluated at each output instant: the
//! kernel is tabulated at a fine fractional step, so any ratio works and
//! costs one dot product per output sample.

/// Phases the kernel is tabulated at, per input sample. The nearest one is
/// used; the timing error that leaves (1/1024 sample) is far below the
/// noise of anything these decoders hear.
const PHASES: usize = 512;

pub(crate) struct Resampler {
    /// Input samples per output sample.
    step: f64,
    half: usize,
    /// `PHASES + 1` rows of `2 * half` taps.
    table: Vec<f32>,
    /// Input not yet consumed, oldest first.
    buf: Vec<f32>,
    /// Position of the next output, in input samples from `buf[0]`.
    pos: f64,
}

impl Resampler {
    /// From `fs_in` to `fs_out`, passing `pass_hz` flat and stopping
    /// everything above `stop_hz` (which may fold back no lower than
    /// `fs_out - stop_hz`).
    pub(crate) fn new(fs_in: f32, fs_out: f32, pass_hz: f32, stop_hz: f32) -> Self {
        let fs_in = fs_in as f64;
        let fs_out = fs_out as f64;
        let stop = (stop_hz as f64).min(fs_in / 2.0);
        let pass = (pass_hz as f64).min(stop * 0.9);
        let cutoff = 0.5 * (pass + stop) / fs_in;
        // Blackman: about 5.5 / taps of transition, 74 dB down.
        let width = ((stop - pass) / fs_in).max(1e-3);
        let half = ((5.5 / width) / 2.0).ceil().max(4.0) as usize;
        let len = 2 * half;
        let mut table = vec![0.0f32; (PHASES + 1) * len];
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let row = &mut table[p * len..(p + 1) * len];
            let mut sum = 0.0;
            for (j, v) in row.iter_mut().enumerate() {
                // Tap j multiplies input sample (i - half + 1 + j) for an
                // output at i + frac.
                let x = frac + half as f64 - 1.0 - j as f64;
                let sinc = if x.abs() < 1e-12 {
                    2.0 * cutoff
                } else {
                    (2.0 * std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * x)
                };
                let u = (x / half as f64 + 1.0) * 0.5;
                let w = if (0.0..=1.0).contains(&u) {
                    0.42 - 0.5 * (std::f64::consts::TAU * u).cos()
                        + 0.08 * (2.0 * std::f64::consts::TAU * u).cos()
                } else {
                    0.0
                };
                *v = (sinc * w) as f32;
                sum += sinc * w;
            }
            // Unity gain at DC for every phase, so no phase ripples the level.
            for v in row.iter_mut() {
                *v /= sum as f32;
            }
        }
        Self {
            step: fs_in / fs_out,
            half,
            table,
            buf: Vec::with_capacity(4096),
            pos: (half - 1) as f64,
        }
    }

    /// Resample `input`, appending to `out`.
    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.buf.extend_from_slice(input);
        let len = 2 * self.half;
        loop {
            let i = self.pos.floor();
            let start = i as usize + 1 - self.half;
            if start + len > self.buf.len() {
                break;
            }
            let p = (((self.pos - i) * PHASES as f64).round() as usize).min(PHASES);
            let taps = &self.table[p * len..(p + 1) * len];
            let x = &self.buf[start..start + len];
            out.push(taps.iter().zip(x).map(|(a, b)| a * b).sum());
            self.pos += self.step;
        }
        // Drop what no later output needs.
        let keep_from = (self.pos.floor() as usize + 1).saturating_sub(self.half);
        if keep_from > 4096 {
            self.buf.drain(..keep_from);
            self.pos -= keep_from as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_keeps_its_frequency_and_level() {
        let fs = 47_904.0f32;
        let mut r = Resampler::new(fs, 8000.0, 3000.0, 5000.0);
        let f = 1234.5f64;
        let x: Vec<f32> = (0..fs as usize * 2)
            .map(|i| (std::f64::consts::TAU * f * i as f64 / fs as f64).sin() as f32)
            .collect();
        let mut y = Vec::new();
        for c in x.chunks(777) {
            r.process(c, &mut y);
        }
        assert!((y.len() as i64 - 16_000).abs() < 80, "{}", y.len());
        // Compare against the ideal tone at the output rate, past the start:
        // output n is the input at n / 8000 s plus the first output's offset.
        let lead = (r.half as f64 - 1.0) / fs as f64;
        let mut err = 0.0f64;
        let mut pow = 0.0f64;
        for (n, v) in y.iter().enumerate().skip(400).take(8000) {
            let t = n as f64 / 8000.0 + lead;
            let ideal = (std::f64::consts::TAU * f * t).sin();
            err += (*v as f64 - ideal).powi(2);
            pow += ideal * ideal;
        }
        let snr = 10.0 * (pow / err).log10();
        assert!(snr > 50.0, "{snr}");
    }

    #[test]
    fn alias_above_the_stop_band_is_gone() {
        let fs = 47_904.0f32;
        let mut r = Resampler::new(fs, 8000.0, 3000.0, 5000.0);
        // 6 kHz would fold to 2 kHz.
        let x: Vec<f32> = (0..fs as usize)
            .map(|i| (std::f64::consts::TAU * 6000.0 * i as f64 / fs as f64).sin() as f32)
            .collect();
        let mut y = Vec::new();
        r.process(&x, &mut y);
        let p = y[400..].iter().map(|v| v * v).sum::<f32>() / (y.len() - 400) as f32;
        assert!(10.0 * p.log10() < -60.0, "{}", 10.0 * p.log10());
    }
}
