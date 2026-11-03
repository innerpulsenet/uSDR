//! The speaker: server audio into the sound card, at whatever rate it has.
//!
//! Audio arrives at the mode's rate (51.2 kHz for WFM, 16 or 48 kHz for
//! voice) and is resampled to the card's through a windowed-sinc
//! interpolator, so the conversion adds nothing audible. The server's clock
//! and the sound card's never quite agree either, so the resampler steers its ratio by
//! the queue depth: a queue running long plays a hair faster, one running
//! short a hair slower, never more than half a percent — far under what an
//! ear notices, and enough to hold the delay steady for hours. A queue that
//! empties waits to refill rather than stuttering; one that backs up past
//! half a second drops to the present, because this is a live monitor.
//!
//! Broadcast FM arrives in stereo when the station sends it; every frame is
//! queued as L, R pairs (mono as the same sample twice) and each channel
//! goes through the same interpolator, so a station that blends to mono and
//! back plays on without a seam. Balance scales the two channels; with the
//! A/B split on, each receiver folds to mono so its whole program is in its
//! own ear.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// How far ahead of the speaker the queue aims to run.
const TARGET_S: f64 = 0.12;
/// Past this the queue is cut back to the target.
const CEILING_S: f64 = 0.5;

#[derive(Default)]
struct Shared {
    /// L, R pairs at `in_rate`.
    queue: VecDeque<[f32; 2]>,
    in_rate: f64,
    out_rate: f64,
    /// Fractional read position into the queue's front.
    phase: f64,
    priming: bool,
    volume: f32,
    muted: bool,
    /// Left/right balance, −1…1.
    pan: f32,
    /// Play stereo as mono (L+R)/2: the A/B split, where a receiver's one
    /// ear must carry the whole program.
    fold: bool,
    /// The stereo decoder's reading from the newest frame that carried one.
    stereo: Option<(std::time::Instant, usdr_proto::StereoReading)>,
    /// Output level for the meter: a decaying peak and the RMS of the last
    /// callback, linear 0…1.
    level_peak: f32,
    level_rms: f32,
    underruns: u64,
    trimmed: u64,
    /// Smoothed ratio correction, for display.
    drift_ppm: f64,
    /// A recording being played back in place of the live audio.
    clip: Option<Clip>,
    /// The interpolator for `in_rate` to `out_rate`, built off the audio
    /// thread when either changes.
    sinc: Option<Sinc>,
}

struct Clip {
    samples: Vec<f32>,
    rate: f64,
    pos: f64,
    sinc: Sinc,
}

/// Taps in the interpolator: half before the point it reads, half after.
const TAPS: usize = 48;
/// Fractional positions tabulated; reads in between blend the two nearest.
const PHASES: usize = 256;
/// Kaiser window shape: about 90 dB down outside the passband.
const KAISER_BETA: f64 = 8.6;

/// A windowed-sinc interpolator for one pair of rates: flat through the
/// audio, its cutoff under both Nyquists, so a rate change neither folds
/// treble back as harshness nor leaves images of a 16 kHz voice stream in
/// the top octave. (A four-point Hermite left products 17–32 dB under the
/// signal, worst on broadcast treble.)
struct Sinc {
    rates: (u64, u64),
    /// `PHASES + 1` rows of `TAPS`, one per fractional position.
    table: Vec<f32>,
}

impl Sinc {
    fn new(in_rate: f64, out_rate: f64) -> Self {
        // The passband ends a little under the lower Nyquist, in units of
        // the input's.
        let fc = (out_rate / in_rate.max(1.0)).min(1.0) * 0.90;
        let half = (TAPS / 2) as f64;
        let i0 = |x: f64| {
            let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
            while term > 1e-12 * sum {
                term *= (x / (2.0 * k)).powi(2);
                sum += term;
                k += 1.0;
            }
            sum
        };
        let norm = i0(KAISER_BETA);
        let mut table = Vec::with_capacity((PHASES + 1) * TAPS);
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let row: Vec<f64> = (0..TAPS)
                .map(|k| {
                    // Distance from the point being read, in input samples.
                    let x = k as f64 - (half - 1.0) - frac;
                    let r = x / half;
                    if r.abs() >= 1.0 {
                        return 0.0;
                    }
                    let a = std::f64::consts::PI * fc * x;
                    let sinc = if a.abs() < 1e-9 { 1.0 } else { a.sin() / a };
                    fc * sinc * i0(KAISER_BETA * (1.0 - r * r).sqrt()) / norm
                })
                .collect();
            // Unity gain at DC for every position.
            let sum: f64 = row.iter().sum();
            table.extend(row.iter().map(|v| (v / sum) as f32));
        }
        Sinc { rates: Self::key(in_rate, out_rate), table }
    }

    fn key(in_rate: f64, out_rate: f64) -> (u64, u64) {
        (in_rate.round() as u64, out_rate.round() as u64)
    }

    fn fits(&self, in_rate: f64, out_rate: f64) -> bool {
        self.rates == Self::key(in_rate, out_rate)
    }

    /// The signal `frac` of a sample after tap `TAPS / 2 - 1`, from the
    /// `TAPS` samples `get` returns.
    fn at(&self, get: impl Fn(usize) -> f32, frac: f64) -> f32 {
        let p = (frac.clamp(0.0, 1.0) * PHASES as f64).min(PHASES as f64 - 1e-6);
        let i = p as usize;
        let t = (p - i as f64) as f32;
        let (r0, r1) = (&self.table[i * TAPS..(i + 1) * TAPS], &self.table[(i + 1) * TAPS..(i + 2) * TAPS]);
        let (mut a, mut b) = (0f32, 0f32);
        for k in 0..TAPS {
            let x = get(k);
            a += r0[k] * x;
            b += r1[k] * x;
        }
        a + (b - a) * t
    }
}


pub struct Player {
    shared: Arc<Mutex<Shared>>,
    stream: Mutex<Option<cpal::Stream>>,
    pub device_name: Mutex<String>,
    pub error: Mutex<Option<String>>,
    /// The WAV being written, under a lock of its own: the sound card's
    /// callback never waits on a disk write.
    recorder: Mutex<Option<WavWriter>>,
}

// cpal's Stream is not Send on every host; it is only ever touched from the
// thread that built it or dropped under the mutex.
unsafe impl Send for Player {}
unsafe impl Sync for Player {}

pub struct LevelReading {
    pub peak: f32,
    pub rms: f32,
    pub queue_ms: f32,
    pub drift_ppm: f32,
    pub underruns: u64,
    pub in_rate: f64,
    pub recording: Option<(PathBuf, f32)>,
    /// What the server's stereo decoder said with the audio now arriving;
    /// none when the frames carry no reading or have stopped.
    pub stereo: Option<usdr_proto::StereoReading>,
}

impl Player {
    /// A player with no sound card behind it: it queues and records but
    /// plays nowhere. For tests and for machines without an output.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn silent() -> Arc<Player> {
        Arc::new(Player {
            shared: Arc::new(Mutex::new(Shared {
                volume: 0.8,
                priming: true,
                in_rate: 48_000.0,
                out_rate: 48_000.0,
                ..Default::default()
            })),
            stream: Mutex::new(None),
            device_name: Mutex::new("none".into()),
            error: Mutex::new(None),
            recorder: Mutex::new(None),
        })
    }

    pub fn new(device: Option<&str>) -> Arc<Player> {
        let p = Arc::new(Player {
            shared: Arc::new(Mutex::new(Shared {
                volume: 0.8,
                priming: true,
                in_rate: 48_000.0,
                out_rate: 48_000.0,
                ..Default::default()
            })),
            stream: Mutex::new(None),
            device_name: Mutex::new(String::new()),
            error: Mutex::new(None),
            recorder: Mutex::new(None),
        });
        p.open(device);
        p
    }

    pub fn devices() -> Vec<String> {
        let host = cpal::default_host();
        host.output_devices()
            .map(|it| {
                it.filter_map(|d| d.description().ok().map(|n| n.name().to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// (Re)open the output on the named device, or the default.
    pub fn open(&self, want: Option<&str>) {
        *self.stream.lock().unwrap() = None;
        let host = cpal::default_host();
        let device = want
            .and_then(|w| {
                host.output_devices().ok()?.find(|d| {
                    d.description().map(|n| n.name() == w).unwrap_or(false)
                })
            })
            .or_else(|| host.default_output_device());
        let Some(device) = device else {
            *self.error.lock().unwrap() = Some("no audio output device".into());
            return;
        };
        let name = device
            .description()
            .map(|n| n.name().to_string())
            .unwrap_or_else(|_| "default".into());
        let cfg = match device.default_output_config() {
            Ok(c) => c,
            Err(e) => {
                *self.error.lock().unwrap() = Some(format!("{name}: {e}"));
                return;
            }
        };
        let channels = cfg.channels() as usize;
        let config: cpal::StreamConfig = cfg.config();
        {
            let mut s = self.shared.lock().unwrap();
            s.out_rate = config.sample_rate as f64;
            s.sinc = Some(Sinc::new(s.in_rate, s.out_rate));
        }
        let shared = self.shared.clone();
        let err_name = name.clone();
        let on_err = move |e: cpal::Error| eprintln!("audio output {err_name}: {e}");
        let built = match cfg.sample_format() {
            cpal::SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
                config,
                {
                    // Converted from a buffer kept between callbacks: no
                    // allocating on the audio thread once it is big enough.
                    let mut tmp: Vec<f32> = Vec::new();
                    move |out: &mut [i16], _| {
                        tmp.resize(out.len(), 0.0);
                        fill(&shared, &mut tmp, channels);
                        for (o, s) in out.iter_mut().zip(&tmp) {
                            *o = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        }
                    }
                },
                on_err,
                None,
            ),
            _ => device.build_output_stream::<f32, _, _>(
                config,
                move |out: &mut [f32], _| fill(&shared, out, channels),
                on_err,
                None,
            ),
        };
        match built {
            Ok(stream) => {
                if let Err(e) = stream.play() {
                    *self.error.lock().unwrap() = Some(format!("{name}: {e}"));
                    return;
                }
                *self.stream.lock().unwrap() = Some(stream);
                *self.device_name.lock().unwrap() = name;
                *self.error.lock().unwrap() = None;
            }
            Err(e) => *self.error.lock().unwrap() = Some(format!("{name}: {e}")),
        }
    }

    pub fn set_volume(&self, v: f32) {
        self.shared.lock().unwrap().volume = v;
    }
    pub fn set_muted(&self, m: bool) {
        self.shared.lock().unwrap().muted = m;
    }
    pub fn set_pan(&self, p: f32) {
        self.shared.lock().unwrap().pan = p.clamp(-1.0, 1.0);
    }
    pub fn set_fold(&self, fold: bool) {
        self.shared.lock().unwrap().fold = fold;
    }
    /// Forget what is queued: after a retune the old channel is not worth hearing.
    pub fn flush(&self) {
        let mut s = self.shared.lock().unwrap();
        s.queue.clear();
        s.priming = true;
    }

    /// Play a recording instead of the live audio until it ends.
    pub fn play_clip(&self, samples: Vec<i16>, rate: u32) {
        let rate = rate.max(1) as f64;
        let out_rate = self.shared.lock().unwrap().out_rate;
        let sinc = Sinc::new(rate, out_rate);
        let mut s = self.shared.lock().unwrap();
        s.clip = Some(Clip { samples: samples.iter().map(|&v| v as f32 / 32768.0).collect(), rate, pos: 0.0, sinc });
    }

    pub fn stop_clip(&self) {
        let mut s = self.shared.lock().unwrap();
        s.clip = None;
        s.queue.clear();
        s.priming = true;
    }

    /// Progress through the clip being played, 0..1.
    pub fn clip_progress(&self) -> Option<f32> {
        let s = self.shared.lock().unwrap();
        s.clip.as_ref().map(|c| (c.pos / c.samples.len().max(1) as f64) as f32)
    }

    pub fn start_recording(&self, path: PathBuf) -> std::io::Result<()> {
        let rate = self.shared.lock().unwrap().in_rate.round() as u32;
        *self.recorder.lock().unwrap() = Some(WavWriter::create(path, rate)?);
        Ok(())
    }

    /// Finish the recording and return where it went.
    pub fn stop_recording(&self) -> Option<(PathBuf, f32)> {
        let w = self.recorder.lock().unwrap().take()?;
        let secs = w.seconds();
        let path = w.path.clone();
        w.finish().ok()?;
        Some((path, secs))
    }

    pub fn reading(&self) -> LevelReading {
        let s = self.shared.lock().unwrap();
        let mut r = LevelReading {
            peak: s.level_peak,
            rms: s.level_rms,
            queue_ms: (s.queue.len() as f64 / s.in_rate.max(1.0) * 1000.0) as f32,
            drift_ppm: s.drift_ppm as f32,
            underruns: s.underruns,
            in_rate: s.in_rate,
            recording: None,
            stereo: s.stereo.filter(|(at, _)| at.elapsed().as_secs_f32() < 1.0).map(|(_, r)| r),
        };
        drop(s);
        r.recording = self.recorder.lock().unwrap().as_ref().map(|w| (w.path.clone(), w.seconds()));
        r
    }
}

/// The two receivers' players, each fed its own audio.
pub struct Speakers {
    pub a: Arc<Player>,
    pub b: Arc<Player>,
}

impl crate::net::AudioSink for Speakers {
    fn push(&self, f: &usdr_proto::AudioFrame) {
        if f.rx == 0 { &self.a } else { &self.b }.push_frame(f);
    }
}

impl Player {
    pub fn push_frame(&self, f: &usdr_proto::AudioFrame) {
        self.push(f.rate_hz, f.channels(), &f.samples);
        let mut s = self.shared.lock().unwrap();
        s.stereo = f.stereo.map(|r| (std::time::Instant::now(), r));
    }

    /// Mono (`channels` 1) or interleaved L, R (2).
    pub fn push(&self, rate_hz: u32, channels: usize, samples: &[i16]) {
        let mut rate_changed = None;
        {
            let mut s = self.shared.lock().unwrap();
            if rate_hz > 0 && (s.in_rate - rate_hz as f64).abs() > 0.5 {
                // A mode change: the queued audio is at the old rate.
                s.in_rate = rate_hz as f64;
                s.queue.clear();
                s.priming = true;
                s.phase = 0.0;
                s.sinc = Some(Sinc::new(s.in_rate, s.out_rate));
                rate_changed = Some(rate_hz);
            }
            let pcm = |v: i16| v as f32 / 32768.0;
            if channels == 2 {
                s.queue.extend(samples.chunks_exact(2).map(|p| [pcm(p[0]), pcm(p[1])]));
            } else {
                s.queue.extend(samples.iter().map(|&v| [pcm(v); 2]));
            }
            let ceiling = (s.in_rate * CEILING_S) as usize;
            if s.queue.len() > ceiling {
                let target = (s.in_rate * TARGET_S) as usize;
                let cut = s.queue.len() - target;
                s.queue.drain(..cut);
                s.trimmed += 1;
            }
        }
        // The recording, with the audio lock released: a disk write that
        // stalls holds up this thread, never the sound card.
        let mut rec = self.recorder.lock().unwrap();
        if let Some(w) = rec.as_mut() {
            if let Some(r) = rate_changed {
                w.note_rate_change(r);
            }
            if channels == 2 {
                // Recordings stay mono WAV: the fold of the two.
                let mono: Vec<i16> = samples.chunks_exact(2).map(|p| ((p[0] as i32 + p[1] as i32) / 2) as i16).collect();
                w.write(&mono);
            } else {
                w.write(samples);
            }
        }
    }
}

fn fill(shared: &Mutex<Shared>, out: &mut [f32], channels: usize) {
    let mut s = shared.lock().unwrap();
    let frames = out.len() / channels.max(1);
    let target = s.in_rate * TARGET_S;
    if s.priming && (s.queue.len() as f64) >= target {
        s.priming = false;
    }
    let gain = if s.muted { 0.0 } else { s.volume * s.volume };
    let (gl, gr) = {
        let p = s.pan;
        (gain * (1.0 - p.max(0.0)), gain * (1.0 + p.min(0.0)))
    };
    // Steer the ratio by how far the queue sits from its target.
    let err = (s.queue.len() as f64 - target) / target.max(1.0);
    let correction = (err * 0.005).clamp(-0.005, 0.005);
    if !s.priming {
        s.drift_ppm = s.drift_ppm * 0.98 + correction * 1e6 * 0.02;
    }
    let step = s.in_rate / s.out_rate.max(1.0) * (1.0 + correction);
    let mut peak = 0f32;
    let mut sum = 0f32;
    if let Some(mut clip) = s.clip.take() {
        let step = clip.rate / s.out_rate.max(1.0);
        for f in 0..frames {
            let i = clip.pos as usize;
            let v = if i + 1 < clip.samples.len() {
                let samples = &clip.samples;
                // Taps run from TAPS/2 − 1 before the point to TAPS/2 after;
                // past either end of the clip is silence.
                let get = |k: usize| (i + k).checked_sub(TAPS / 2 - 1).and_then(|j| samples.get(j)).copied().unwrap_or(0.0);
                clip.sinc.at(get, clip.pos - i as f64)
            } else {
                0.0
            };
            clip.pos += step;
            peak = peak.max(v.abs());
            sum += v * v;
            let base = f * channels;
            for c in 0..channels {
                out[base + c] = v * if c == 0 { gl } else if c == 1 { gr } else { 0.0 };
            }
        }
        if (clip.pos as usize) + 1 < clip.samples.len() {
            s.clip = Some(clip);
        } else {
            // Back to live, starting from the present.
            s.queue.clear();
            s.priming = true;
        }
        s.level_peak = peak.max(s.level_peak * 0.92);
        s.level_rms = (sum / frames.max(1) as f32).sqrt();
        return;
    }
    // Built in `push` and `open`; here only if neither has run yet.
    if !s.sinc.as_ref().is_some_and(|k| k.fits(s.in_rate, s.out_rate)) {
        s.sinc = Some(Sinc::new(s.in_rate, s.out_rate));
    }
    let sinc = s.sinc.take().expect("built above");
    let fold = s.fold || channels < 2;
    for f in 0..frames {
        let (l, r) = if s.priming || s.queue.len() < TAPS {
            if !s.priming {
                s.priming = true;
                s.underruns += 1;
            }
            (0.0, 0.0)
        } else {
            let q = &s.queue;
            // The point read sits between q[TAPS/2 − 1] and q[TAPS/2]:
            // half a filter's length of delay, about half a millisecond.
            // One channel's taps when the two agree, which mono always does.
            let l = sinc.at(|k| q[k][0], s.phase);
            let stereo = q.range(..TAPS).any(|p| p[0] != p[1]);
            let r = if stereo { sinc.at(|k| q[k][1], s.phase) } else { l };
            s.phase += step;
            let whole = s.phase.floor() as usize;
            if whole > 0 {
                let n = whole.min(s.queue.len());
                s.queue.drain(..n);
                s.phase -= whole as f64;
            }
            if fold { ((l + r) * 0.5, (l + r) * 0.5) } else { (l, r) }
        };
        peak = peak.max(l.abs()).max(r.abs());
        sum += (l * l + r * r) * 0.5;
        let base = f * channels;
        if channels >= 2 {
            out[base] = l * gl;
            out[base + 1] = r * gr;
            for c in 2..channels {
                out[base + c] = 0.0;
            }
        } else if channels == 1 {
            out[base] = l * gain;
        }
    }
    s.sinc = Some(sinc);
    let rms = (sum / frames.max(1) as f32).sqrt();
    s.level_peak = peak.max(s.level_peak * 0.92);
    s.level_rms = rms;
}

/// 16-bit mono PCM WAV, header patched on close. A rate change mid-recording
/// (the operator switched mode) is noted in a sidecar rather than resampled.
pub struct WavWriter {
    file: std::io::BufWriter<std::fs::File>,
    pub path: PathBuf,
    rate: u32,
    samples: u64,
    rate_changes: Vec<(u64, u32)>,
}

impl WavWriter {
    pub fn create(path: PathBuf, rate: u32) -> std::io::Result<Self> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);
        file.write_all(&wav_header(rate, 0))?;
        Ok(Self { file, path, rate, samples: 0, rate_changes: Vec::new() })
    }
    fn write(&mut self, s: &[i16]) {
        let bytes: Vec<u8> = s.iter().flat_map(|v| v.to_le_bytes()).collect();
        let _ = self.file.write_all(&bytes);
        self.samples += s.len() as u64;
    }
    fn note_rate_change(&mut self, rate: u32) {
        self.rate_changes.push((self.samples, rate));
    }
    pub fn seconds(&self) -> f32 {
        self.samples as f32 / self.rate.max(1) as f32
    }
    fn finish(mut self) -> std::io::Result<()> {
        use std::io::Seek;
        self.file.flush()?;
        let mut f = self.file.into_inner().map_err(|e| e.into_error())?;
        f.seek(std::io::SeekFrom::Start(0))?;
        f.write_all(&wav_header(self.rate, self.samples))?;
        if !self.rate_changes.is_empty() {
            let note = self
                .rate_changes
                .iter()
                .map(|(at, r)| format!("sample {at}: audio rate changed to {r} Hz"))
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(self.path.with_extension("txt"), note)?;
        }
        Ok(())
    }
}

pub fn wav_header(rate: u32, samples: u64) -> [u8; 44] {
    let data = (samples * 2).min(u32::MAX as u64 - 36) as u32;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&1u16.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 2).to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything the resampler adds, as dB under a tone it carries: the
    /// residual against the exact sine at each output instant.
    fn added_db(in_rate: f64, out_rate: f64, hz: f64) -> f64 {
        let sinc = Sinc::new(in_rate, out_rate);
        let x: Vec<f32> = (0..20_000).map(|n| (std::f64::consts::TAU * hz * n as f64 / in_rate).sin() as f32).collect();
        let (mut err, mut sig) = (0f64, 0f64);
        let mut pos = 200.0;
        while pos < 19_000.0 {
            let i = pos as usize;
            let got = sinc.at(|k| x[i + k - (TAPS / 2 - 1)], pos - i as f64) as f64;
            let want = (std::f64::consts::TAU * hz * pos / in_rate).sin();
            err += (got - want).powi(2);
            sig += want * want;
            pos += in_rate / out_rate;
        }
        10.0 * (err / sig).log10()
    }

    #[test]
    fn broadcast_treble_comes_through_clean() {
        // WFM's 51.2 kHz to a 48 kHz card, up to the 15 kHz the server keeps.
        for hz in [1_000.0, 5_000.0, 10_000.0, 12_000.0, 15_000.0] {
            let db = added_db(51_200.0, 48_000.0, hz);
            assert!(db < -80.0, "{hz} Hz: {db:.1} dB added");
        }
    }

    #[test]
    fn voice_upsampled_leaves_no_images() {
        // A 16 kHz voice stream to a 48 kHz card.
        for hz in [300.0, 1_000.0, 3_000.0, 5_000.0] {
            let db = added_db(16_000.0, 48_000.0, hz);
            assert!(db < -80.0, "{hz} Hz: {db:.1} dB added");
        }
    }

    #[test]
    fn the_interpolator_passes_dc_at_every_position() {
        let sinc = Sinc::new(51_200.0, 48_000.0);
        for p in 0..=20 {
            let v = sinc.at(|_| 0.5, p as f64 / 20.0);
            assert!((v - 0.5).abs() < 1e-5, "{v} at {p}/20");
        }
    }

    /// A second of interleaved stereo through `fill` to a two-channel card,
    /// returning the card's L and R.
    fn play_stereo(fold: bool, pan: f32) -> (Vec<f32>, Vec<f32>) {
        let p = Player::silent();
        p.set_volume(1.0);
        p.set_fold(fold);
        p.set_pan(pan);
        // A 1 kHz tone in L only, at 51.2 kHz.
        let lr: Vec<i16> = (0..51_200)
            .flat_map(|n| [((std::f64::consts::TAU * 1_000.0 * n as f64 / 51_200.0).sin() * 16_000.0) as i16, 0])
            .collect();
        let (mut l, mut r) = (Vec::new(), Vec::new());
        let mut out = vec![0f32; 2 * 480];
        for chunk in lr.chunks(2 * 512) {
            p.push(51_200, 2, chunk);
            fill(&p.shared, &mut out, 2);
            l.extend(out.iter().step_by(2));
            r.extend(out.iter().skip(1).step_by(2));
        }
        (l, r)
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn stereo_plays_each_channel_in_its_own_ear() {
        let (l, r) = play_stereo(false, 0.0);
        assert!(rms(&l) > 0.2, "L {}", rms(&l));
        assert!(rms(&r) < 1e-6, "R {}", rms(&r));
        // Balance toward the right turns the left channel down.
        let (l2, _) = play_stereo(false, 0.5);
        assert!((rms(&l2) / rms(&l) - 0.5).abs() < 0.01);
        // The A/B split folds the program into whichever ear it plays in.
        let (l, r) = play_stereo(true, 1.0);
        assert!(rms(&l) < 1e-6, "A in the right ear only");
        assert!(rms(&r) > 0.1, "and it carries the left channel's tone: {}", rms(&r));
    }

    #[test]
    fn steady_queue_holds_its_depth() {
        let shared = Mutex::new(Shared {
            in_rate: 48_076.9,
            out_rate: 48_000.0,
            volume: 1.0,
            priming: true,
            ..Default::default()
        });
        let mut out = vec![0f32; 960];
        // Feed exactly what the server sends per 20 ms and drain 20 ms of
        // card time, for ten simulated seconds.
        let mut carry = 0.0;
        for _ in 0..500 {
            carry += 48_076.9 * 0.02;
            let n = carry as usize;
            carry -= n as f64;
            shared.lock().unwrap().queue.extend(std::iter::repeat_n([0.1; 2], n));
            fill(&shared, &mut out, 1);
        }
        let s = shared.lock().unwrap();
        let ms = s.queue.len() as f64 / s.in_rate * 1000.0;
        assert!((ms - 120.0).abs() < 40.0, "queue drifted to {ms} ms");
        assert_eq!(s.underruns, 0);
    }
}
