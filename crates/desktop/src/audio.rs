//! The speaker: server audio into the sound card, at whatever rate it has.
//!
//! The server's clock and the sound card's never quite agree (WFM arrives at
//! 48 076.9 S/s against a 48 000 card), so the resampler steers its ratio by
//! the queue depth: a queue running long plays a hair faster, one running
//! short a hair slower, never more than half a percent — far under what an
//! ear notices, and enough to hold the delay steady for hours. A queue that
//! empties waits to refill rather than stuttering; one that backs up past
//! half a second drops to the present, because this is a live monitor.

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
    queue: VecDeque<f32>,
    in_rate: f64,
    out_rate: f64,
    /// Fractional read position into the queue's front.
    phase: f64,
    priming: bool,
    volume: f32,
    muted: bool,
    /// Left/right balance, −1…1.
    pan: f32,
    /// Output level for the meter: a decaying peak and the RMS of the last
    /// callback, linear 0…1.
    level_peak: f32,
    level_rms: f32,
    underruns: u64,
    trimmed: u64,
    recorder: Option<WavWriter>,
    /// Smoothed ratio correction, for display.
    drift_ppm: f64,
    /// A recording being played back in place of the live audio.
    clip: Option<Clip>,
}

struct Clip {
    samples: Vec<f32>,
    rate: f64,
    pos: f64,
}

pub struct Player {
    shared: Arc<Mutex<Shared>>,
    stream: Mutex<Option<cpal::Stream>>,
    pub device_name: Mutex<String>,
    pub error: Mutex<Option<String>>,
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
        self.shared.lock().unwrap().out_rate = config.sample_rate as f64;
        let shared = self.shared.clone();
        let err_name = name.clone();
        let on_err = move |e: cpal::Error| eprintln!("audio output {err_name}: {e}");
        let built = match cfg.sample_format() {
            cpal::SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
                config,
                move |out: &mut [i16], _| {
                    let mut tmp = vec![0f32; out.len()];
                    fill(&shared, &mut tmp, channels);
                    for (o, s) in out.iter_mut().zip(tmp) {
                        *o = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
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
    /// Forget what is queued: after a retune the old channel is not worth hearing.
    pub fn flush(&self) {
        let mut s = self.shared.lock().unwrap();
        s.queue.clear();
        s.priming = true;
    }

    /// Play a recording instead of the live audio until it ends.
    pub fn play_clip(&self, samples: Vec<i16>, rate: u32) {
        let mut s = self.shared.lock().unwrap();
        s.clip = Some(Clip {
            samples: samples.iter().map(|&v| v as f32 / 32768.0).collect(),
            rate: rate.max(1) as f64,
            pos: 0.0,
        });
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
        let mut s = self.shared.lock().unwrap();
        let rate = s.in_rate.round() as u32;
        s.recorder = Some(WavWriter::create(path, rate)?);
        Ok(())
    }

    /// Finish the recording and return where it went.
    pub fn stop_recording(&self) -> Option<(PathBuf, f32)> {
        let w = self.shared.lock().unwrap().recorder.take()?;
        let secs = w.seconds();
        let path = w.path.clone();
        w.finish().ok()?;
        Some((path, secs))
    }

    pub fn reading(&self) -> LevelReading {
        let s = self.shared.lock().unwrap();
        LevelReading {
            peak: s.level_peak,
            rms: s.level_rms,
            queue_ms: (s.queue.len() as f64 / s.in_rate.max(1.0) * 1000.0) as f32,
            drift_ppm: s.drift_ppm as f32,
            underruns: s.underruns,
            in_rate: s.in_rate,
            recording: s.recorder.as_ref().map(|w| (w.path.clone(), w.seconds())),
        }
    }
}

impl crate::net::AudioSink for Player {
    fn push(&self, rate_hz: u32, samples: &[i16]) {
        let mut s = self.shared.lock().unwrap();
        if rate_hz > 0 && (s.in_rate - rate_hz as f64).abs() > 0.5 {
            // A mode change: the queued audio is at the old rate.
            s.in_rate = rate_hz as f64;
            s.queue.clear();
            s.priming = true;
            s.phase = 0.0;
            if let Some(w) = s.recorder.as_mut() {
                w.note_rate_change(rate_hz);
            }
        }
        if let Some(w) = s.recorder.as_mut() {
            w.write(samples);
        }
        s.queue.extend(samples.iter().map(|&v| v as f32 / 32768.0));
        let ceiling = (s.in_rate * CEILING_S) as usize;
        if s.queue.len() > ceiling {
            let target = (s.in_rate * TARGET_S) as usize;
            let cut = s.queue.len() - target;
            s.queue.drain(..cut);
            s.trimmed += 1;
        }
    }
}

/// Catmull-Rom between p1 and p2.
fn hermite(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    let c1 = 0.5 * (p2 - p0);
    let c2 = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
    let c3 = 0.5 * (p3 - p0) + 1.5 * (p1 - p2);
    ((c3 * t + c2) * t + c1) * t + p1
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
                let t = (clip.pos - i as f64) as f32;
                clip.samples[i] * (1.0 - t) + clip.samples[i + 1] * t
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
    for f in 0..frames {
        let v = if s.priming || s.queue.len() < 4 {
            if !s.priming {
                s.priming = true;
                s.underruns += 1;
            }
            0.0
        } else {
            let q = &s.queue;
            let t = s.phase as f32;
            let v = hermite(q[0], q[1], q[2], q[3], t);
            s.phase += step;
            let whole = s.phase.floor() as usize;
            if whole > 0 {
                let n = whole.min(s.queue.len());
                s.queue.drain(..n);
                s.phase -= whole as f64;
            }
            v
        };
        peak = peak.max(v.abs());
        sum += v * v;
        let base = f * channels;
        if channels >= 2 {
            out[base] = v * gl;
            out[base + 1] = v * gr;
            for c in 2..channels {
                out[base + c] = 0.0;
            }
        } else if channels == 1 {
            out[base] = v * gain;
        }
    }
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
        for v in s {
            let _ = self.file.write_all(&v.to_le_bytes());
        }
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

    #[test]
    fn hermite_passes_through_its_points() {
        assert_eq!(hermite(0.0, 1.0, 2.0, 3.0, 0.0), 1.0);
        assert!((hermite(0.0, 1.0, 2.0, 3.0, 1.0) - 2.0).abs() < 1e-6);
        assert!((hermite(0.0, 1.0, 2.0, 3.0, 0.5) - 1.5).abs() < 1e-6);
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
            shared.lock().unwrap().queue.extend(std::iter::repeat_n(0.1, n));
            fill(&shared, &mut out, 1);
        }
        let s = shared.lock().unwrap();
        let ms = s.queue.len() as f64 / s.in_rate * 1000.0;
        assert!((ms - 120.0).abs() < 40.0, "queue drifted to {ms} ms");
        assert_eq!(s.underruns, 0);
    }
}
