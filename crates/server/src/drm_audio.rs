//! DRM's audio on its way to the listener: decoded a super frame (400 ms)
//! at a time at the codec's rate (12, 24 or 48 kHz), and needed a block at
//! a time at the channel's rate (about 48 kHz, not a round number). This
//! resamples with a windowed-sinc interpolator and holds the result in a
//! buffer that starts playing once it holds enough to ride over the gaps
//! between super frames, and pads with silence (never repeats) when the
//! signal fails.

use std::collections::VecDeque;

/// Taps a side of the interpolating kernel, and the kernel's phases.
const HALF: usize = 8;
const PHASES: usize = 64;

pub struct DrmAudio {
    in_rate: f64,
    out_rate: f64,
    /// Input samples per output sample.
    step: f64,
    /// Where the next output sample falls, in input samples from the
    /// front of `hist`.
    pos: f64,
    hist: VecDeque<f32>,
    kernel: Vec<[f32; 2 * HALF]>,
    fifo: VecDeque<f32>,
    playing: bool,
    /// Samples to hold before playing: a super frame and a half.
    prebuffer: usize,
}

impl DrmAudio {
    pub fn new(out_rate: f64) -> Self {
        let mut a = DrmAudio { in_rate: 0.0, out_rate, step: 1.0, pos: 0.0, hist: VecDeque::new(), kernel: Vec::new(), fifo: VecDeque::new(), playing: false, prebuffer: (out_rate * 0.6) as usize };
        a.set_rate(48_000.0);
        a
    }

    fn set_rate(&mut self, in_rate: f64) {
        if (in_rate - self.in_rate).abs() < 0.5 {
            return;
        }
        self.in_rate = in_rate;
        self.step = in_rate / self.out_rate;
        // Cut off just under the lower of the two Nyquists.
        let fc = 0.45 * (1.0f64).min(self.out_rate / in_rate);
        self.kernel = (0..PHASES)
            .map(|p| {
                let frac = p as f64 / PHASES as f64;
                let mut k = [0f32; 2 * HALF];
                let mut sum = 0.0;
                for (i, v) in k.iter_mut().enumerate() {
                    let x = i as f64 - (HALF as f64 - 1.0) - frac;
                    let sinc = if x.abs() < 1e-9 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x) };
                    // Blackman over the kernel's span.
                    let t = (x + HALF as f64) / (2.0 * HALF as f64);
                    let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * t).cos() + 0.08 * (4.0 * std::f64::consts::PI * t).cos();
                    *v = (sinc * w) as f32;
                    sum += sinc * w;
                }
                // Unity gain at DC for every phase.
                for v in &mut k {
                    *v /= sum as f32;
                }
                k
            })
            .collect();
        self.hist.clear();
        // The first output with a full history under the kernel.
        self.pos = (HALF - 1) as f64;
    }

    /// Mono PCM at `rate`, as the codec produced it.
    pub fn push(&mut self, pcm: &[f32], rate: f64) {
        self.set_rate(rate);
        self.hist.extend(pcm.iter().copied());
        // Output while the kernel has its taps on both sides.
        while self.pos + HALF as f64 + 1.0 < self.hist.len() as f64 {
            let base = self.pos.floor() as usize;
            let frac = self.pos - base as f64;
            let ph = ((frac * PHASES as f64) as usize).min(PHASES - 1);
            let k = &self.kernel[ph];
            let mut acc = 0f32;
            for (i, c) in k.iter().enumerate() {
                let j = base + i;
                acc += c * self.hist.get(j.wrapping_sub(HALF - 1)).copied().unwrap_or(0.0);
            }
            self.fifo.push_back(acc);
            self.pos += self.step;
        }
        // Drop what the kernel has passed.
        let keep_from = (self.pos.floor() as usize).saturating_sub(HALF);
        for _ in 0..keep_from.min(self.hist.len()) {
            self.hist.pop_front();
        }
        self.pos -= keep_from as f64;
        // A buffer that has grown past two seconds is behind: catch up.
        let max = (self.out_rate * 2.0) as usize;
        while self.fifo.len() > max {
            self.fifo.pop_front();
        }
    }

    /// The next `n` samples for the listener: what is buffered, then
    /// silence; nothing until the buffer has filled once.
    pub fn take(&mut self, n: usize, out: &mut Vec<f32>) {
        out.clear();
        if !self.playing && self.fifo.len() >= self.prebuffer {
            self.playing = true;
        }
        if self.playing {
            let k = n.min(self.fifo.len());
            out.extend(self.fifo.drain(..k));
            if k < n {
                // Ran dry: fill again before playing on.
                self.playing = false;
            }
        }
        out.resize(n, 0.0);
    }

    pub fn reset(&mut self) {
        self.fifo.clear();
        self.hist.clear();
        self.pos = (HALF - 1) as f64;
        self.playing = false;
    }

    /// Seconds of audio buffered.
    pub fn buffered_s(&self) -> f64 {
        self.fifo.len() as f64 / self.out_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1 kHz tone at 24 kHz comes out at 47 904 Hz as the same tone, at
    /// the same level, with the right number of samples.
    #[test]
    fn a_tone_keeps_its_pitch_and_level() {
        let out_rate = 47_904.0;
        let mut a = DrmAudio::new(out_rate);
        let tone: Vec<f32> = (0..24_000 * 2).map(|i| (2.0 * std::f32::consts::PI * 1_000.0 * i as f32 / 24_000.0).sin() * 0.5).collect();
        for c in tone.chunks(9_600) {
            a.push(c, 24_000.0);
        }
        let mut got = Vec::new();
        let mut block = Vec::new();
        for _ in 0..80 {
            a.take(1_000, &mut block);
            got.extend_from_slice(&block);
        }
        // Past the prebuffer's silence, a whole second of tone.
        let start = got.iter().position(|v| v.abs() > 0.01).expect("tone");
        let seg = &got[start + 2_000..start + 2_000 + 40_000];
        let rms = (seg.iter().map(|v| v * v).sum::<f32>() / seg.len() as f32).sqrt();
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.01, "{rms}");
        // Its frequency from the zero crossings.
        let crossings = seg.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count() as f64;
        let hz = crossings / (seg.len() as f64 / out_rate);
        assert!((hz - 1_000.0).abs() < 3.0, "{hz}");
    }

    #[test]
    fn it_waits_to_fill_and_pads_with_silence() {
        let mut a = DrmAudio::new(48_000.0);
        let mut out = Vec::new();
        a.push(&vec![0.3; 4_800], 48_000.0);
        a.take(480, &mut out);
        assert!(out.iter().all(|v| *v == 0.0), "not yet full");
        a.push(&vec![0.3; 48_000], 48_000.0);
        a.take(480, &mut out);
        assert!(out.iter().all(|v| (*v - 0.3).abs() < 0.01));
    }
}
