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
//! How far ahead of the speaker the queue runs is the delay the operator
//! hears, so it is no longer one figure for every link: it is what this
//! link needs. The longest gap between arrivals lately, a callback of the
//! card's, and a margin, never under 60 ms: about 70 ms on a wired LAN
//! where the old fixed 120 ms was mostly idle, and more than 120 on a link
//! that needs it. An underrun raises it for a while.
//!
//! Broadcast FM arrives in stereo when the station sends it; every frame is
//! queued as L, R pairs (mono as the same sample twice) and each channel
//! goes through the same interpolator, so a station that blends to mono and
//! back plays on without a seam. Balance scales the two channels; with the
//! A/B split on, each receiver folds to mono so its whole program is in its
//! own ear.
//!
//! AF gain runs past unity, to +12 dB, for the weak signal the server's AGC
//! cannot lift further. A look-ahead limiter on the card's output keeps
//! that headroom from ever clipping: it sees each peak 2 ms before it plays
//! and has the gain down by the time it arrives, then lets it back up over
//! a fifth of a second.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// How far ahead of the speaker the queue aims to run before the link has
/// been measured (and where nothing measures it: a test, a clip).
const TARGET_S: f64 = 0.12;
/// The measured target's limits and its margin over what was measured.
const TARGET_MIN_S: f64 = 0.060;
const TARGET_MAX_S: f64 = 0.250;
const TARGET_MARGIN_S: f64 = 0.015;
/// The longest gap is remembered with a half-life of about 20 s of frames,
/// and an underrun's penalty with one of about a minute.
const GAP_KEEP: f64 = 0.9995;
const PENALTY_KEEP: f64 = 0.9998;
/// Each underrun adds this to the target, up to three of them.
const PENALTY_S: f64 = 0.030;
/// The time constant of the mean queue depth the ratio is steered by.
const STEER_MEAN_S: f64 = 2.0;
/// Past this the queue is cut back to the target.
const CEILING_S: f64 = 0.5;
/// The limiter's look-ahead, release time constant, and the level nothing
/// it passes ever exceeds.
const LIMIT_LOOK_S: f64 = 0.002;
const LIMIT_RELEASE_S: f64 = 0.200;
const LIMIT_CEILING: f32 = 0.97;

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
    /// The link, as measured: the longest gap between arrivals lately, the
    /// card's longest callback, and what underruns have added. `target_s`
    /// is the queue depth they call for; zero until a frame has arrived.
    last_push: Option<std::time::Instant>,
    gap_peak_s: f64,
    callback_s: f64,
    penalty_s: f64,
    target_s: f64,
    /// The queue's depth as the ratio is steered by it: its mean over
    /// `STEER_MEAN_S`, seconds. Zero while there is none (priming, a cut).
    depth_s: f64,
    /// Smoothed ratio correction, for display.
    drift_ppm: f64,
    /// A recording being played back in place of the live audio.
    clip: Option<Clip>,
    /// The interpolator for `in_rate` to `out_rate`, built off the audio
    /// thread when either changes.
    sinc: Option<Sinc>,
    /// The output limiter, built for `out_rate`.
    limiter: Option<Limiter>,
}

struct Clip {
    samples: Vec<f32>,
    rate: f64,
    pos: f64,
    sinc: Sinc,
}

impl Shared {
    /// The queue depth to hold, seconds.
    fn target(&self) -> f64 {
        if self.target_s > 0.0 { self.target_s } else { TARGET_S }
    }

    /// A frame arrived `gap_s` after the one before it.
    ///
    /// The queue has to outlast the longest wait for a frame, plus the
    /// callback that empties it in one go, plus a margin. The whole gap is
    /// counted, not just how late the frame was, which leaves a frame's
    /// length of slack on top. A gap of a second or more is a pause in the
    /// stream (a retune, a reconnect), not jitter.
    fn arrived(&mut self, gap_s: f64) {
        if gap_s < 1.0 {
            self.gap_peak_s = gap_s.max(self.gap_peak_s * GAP_KEEP);
        }
        self.penalty_s *= PENALTY_KEEP;
        self.target_s = (self.gap_peak_s + self.callback_s + TARGET_MARGIN_S + self.penalty_s).clamp(TARGET_MIN_S, TARGET_MAX_S);
    }
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

/// Look-ahead peak limiter on L, R pairs at the card's rate. The output is
/// the input delayed by the look-ahead; the gain is steered by the smallest
/// gain any sample inside that window needs, so it is already down when a
/// peak plays. A hard clamp at the ceiling is the backstop for the last
/// percent the attack curve leaves.
struct Limiter {
    rate: u64,
    look: usize,
    delay: VecDeque<[f32; 2]>,
    /// Sliding minimum of the gain each sample in the window needs:
    /// (sample number, gain), gains increasing front to back.
    need: VecDeque<(u64, f32)>,
    n: u64,
    gain: f32,
    attack: f32,
    release: f32,
}

impl Limiter {
    fn new(out_rate: f64) -> Self {
        let look = ((LIMIT_LOOK_S * out_rate).round() as usize).max(1);
        Limiter {
            rate: out_rate.round() as u64,
            look,
            delay: VecDeque::with_capacity(look + 1),
            need: VecDeque::with_capacity(look + 1),
            n: 0,
            gain: 1.0,
            // Most of the way (98%) to the needed gain within the look-ahead.
            attack: 1.0 - (-4.0 / look as f32).exp(),
            release: 1.0 - (-1.0 / (LIMIT_RELEASE_S * out_rate) as f32).exp(),
        }
    }

    fn fits(&self, out_rate: f64) -> bool {
        self.rate == out_rate.round() as u64
    }

    fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        let peak = l.abs().max(r.abs());
        let want = if peak > LIMIT_CEILING { LIMIT_CEILING / peak } else { 1.0 };
        while self.need.back().is_some_and(|&(_, g)| g >= want) {
            self.need.pop_back();
        }
        self.need.push_back((self.n, want));
        while self.need.front().is_some_and(|&(i, _)| i + (self.look as u64) < self.n) {
            self.need.pop_front();
        }
        self.n += 1;
        let target = self.need.front().map_or(1.0, |&(_, g)| g);
        let k = if target < self.gain { self.attack } else { self.release };
        self.gain += (target - self.gain) * k;
        self.delay.push_back([l, r]);
        if self.delay.len() <= self.look {
            return (0.0, 0.0);
        }
        let [l, r] = self.delay.pop_front().expect("longer than the look-ahead");
        let g = self.gain;
        ((l * g).clamp(-LIMIT_CEILING, LIMIT_CEILING), (r * g).clamp(-LIMIT_CEILING, LIMIT_CEILING))
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
    /// Nothing is meant to play through it: the output is closed.
    idle: std::sync::atomic::AtomicBool,
    /// The output device asked for, to open when it is next wanted.
    device: Mutex<Option<String>>,
    /// Built without a sound card (tests): it never opens one.
    silent: bool,
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
            idle: Default::default(),
            device: Mutex::new(None),
            silent: true,
        })
    }

    pub fn new(device: Option<&str>) -> Arc<Player> {
        let p = Self::unopened(device, false);
        p.open(device);
        p
    }

    /// A player with its output closed until [`Player::set_idle`] wants it.
    pub fn new_idle(device: Option<&str>) -> Arc<Player> {
        Self::unopened(device, true)
    }

    fn unopened(device: Option<&str>, idle: bool) -> Arc<Player> {
        Arc::new(Player {
            shared: Arc::new(Mutex::new(Shared {
                volume: 0.8,
                priming: true,
                in_rate: 48_000.0,
                out_rate: 48_000.0,
                // Until the link has shown what it does, assume gaps of
                // 60 ms: the queue starts near the old fixed depth and
                // comes down as the measurement replaces the guess.
                gap_peak_s: 0.060,
                ..Default::default()
            })),
            stream: Mutex::new(None),
            device_name: Mutex::new(String::new()),
            error: Mutex::new(None),
            recorder: Mutex::new(None),
            idle: idle.into(),
            device: Mutex::new(device.map(str::to_string)),
            silent: false,
        })
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
        *self.device.lock().unwrap() = want.map(str::to_string);
        if self.silent || self.idle.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
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
            // Another card, another callback size.
            s.callback_s = 0.0;
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
    /// Close the output while nothing is meant to play through it (the
    /// second receiver, off), and open it again when something is: an
    /// idle stream still woke an audio thread of its own and the sound
    /// server's every few milliseconds to limit and mix silence. (ALSA
    /// through PipeWire refused to pause one.) Opened again, it starts
    /// from what arrives next. From the UI thread, which builds streams.
    pub fn set_idle(&self, idle: bool) {
        if self.idle.swap(idle, std::sync::atomic::Ordering::Relaxed) == idle {
            return;
        }
        if idle {
            *self.stream.lock().unwrap() = None;
        } else {
            self.flush();
            let want = self.device.lock().unwrap().clone();
            self.open(want.as_deref());
        }
    }

    /// Forget what is queued: after a retune the old channel is not worth hearing.
    pub fn flush(&self) {
        let mut s = self.shared.lock().unwrap();
        s.queue.clear();
        s.priming = true;
        // The wait for the stream that follows is the reconnect's, not the
        // link's: it is not measured as a gap.
        s.last_push = None;
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
            let now = std::time::Instant::now();
            let rate_change = rate_hz > 0 && (s.in_rate - rate_hz as f64).abs() > 0.5;
            // The pause a mode change leaves in the stream is the server
            // switching over, not the link: it is not measured as a gap.
            if let Some(prev) = s.last_push.replace(now)
                && !rate_change
            {
                s.arrived(now.duration_since(prev).as_secs_f64());
            }
            if rate_change {
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
            let ceiling = (s.in_rate * CEILING_S.max(2.0 * s.target())) as usize;
            if s.queue.len() > ceiling {
                let target = (s.in_rate * s.target()) as usize;
                let cut = s.queue.len() - target;
                s.queue.drain(..cut);
                s.trimmed += 1;
                // The mean depth was of a queue that is no longer there.
                s.depth_s = 0.0;
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
    if s.out_rate > 0.0 {
        s.callback_s = s.callback_s.max(frames as f64 / s.out_rate).min(0.1);
    }
    let target = s.in_rate * s.target();
    if s.priming && (s.queue.len() as f64) >= target {
        s.priming = false;
    }
    let gain = if s.muted { 0.0 } else { s.volume * s.volume };
    let (gl, gr) = {
        let p = s.pan;
        (gain * (1.0 - p.max(0.0)), gain * (1.0 + p.min(0.0)))
    };
    // Steer the ratio by how far the queue sits from its target, read
    // through a two-second mean. The depth at the instant of a callback
    // swings by a frame's length with where the last arrival happened to
    // fall, and steering by that swung the ratio at every callback: a tenth
    // of a percent of pitch wobble, forty-odd times a second, which put
    // everything that was not the tone only 35 dB under a 5 kHz tone (28 dB
    // with the queue at 60 ms). The mean moves the ratio by parts per
    // million. It is taken as read at once when there is none yet, and
    // when the queue is far from where it should be, so a queue running
    // dry or backing up is still answered straight away.
    let depth = s.queue.len() as f64 / s.in_rate.max(1.0);
    let target_s = target / s.in_rate.max(1.0);
    if s.priming {
        s.depth_s = 0.0;
    } else if s.depth_s <= 0.0 || depth < 0.25 * target_s || depth > 2.5 * target_s {
        s.depth_s = depth;
    } else {
        let dt = frames as f64 / s.out_rate.max(1.0);
        s.depth_s += (depth - s.depth_s) * (1.0 - (-dt / STEER_MEAN_S).exp());
    }
    let err = if s.depth_s > 0.0 { (s.depth_s - target_s) / target_s.max(1e-3) } else { 0.0 };
    let correction = (err * 0.005).clamp(-0.005, 0.005);
    if !s.priming {
        s.drift_ppm = s.drift_ppm * 0.98 + correction * 1e6 * 0.02;
    }
    let step = s.in_rate / s.out_rate.max(1.0) * (1.0 + correction);
    let mut peak = 0f32;
    let mut sum = 0f32;
    if !s.limiter.as_ref().is_some_and(|k| k.fits(s.out_rate)) {
        s.limiter = Some(Limiter::new(s.out_rate));
    }
    let mut limiter = s.limiter.take().expect("built above");
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
            let (l, r) = if channels >= 2 { limiter.process(v * gl, v * gr) } else { limiter.process(v * gain, v * gain) };
            for c in 0..channels {
                out[base + c] = if c == 0 { l } else if c == 1 { r } else { 0.0 };
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
        s.limiter = Some(limiter);
        return;
    }
    // Built in `push` and `open`; here only if neither has run yet.
    if !s.sinc.as_ref().is_some_and(|k| k.fits(s.in_rate, s.out_rate)) {
        s.sinc = Some(Sinc::new(s.in_rate, s.out_rate));
    }
    let sinc = s.sinc.take().expect("built above");
    let fold = s.fold || channels < 2;
    // The queue read as one slice, and what was played off its front let
    // go once, after the callback, rather than the ring buffer indexed tap
    // by tap (a wrap test on every one) and drained sample by sample. What
    // plays is the same to the bit.
    let s = &mut *s;
    let q = s.queue.make_contiguous();
    let mut read = 0usize;
    for f in 0..frames {
        let (l, r) = if s.priming || q.len() - read < TAPS {
            if !s.priming {
                s.priming = true;
                s.underruns += 1;
                // The queue was too short for this link: longer for a
                // while, on top of whatever the gap that caused it adds.
                s.penalty_s = (s.penalty_s + PENALTY_S).min(3.0 * PENALTY_S);
            }
            (0.0, 0.0)
        } else {
            let w: &[[f32; 2]; TAPS] = q[read..read + TAPS].try_into().expect("TAPS long");
            // The point read sits between w[TAPS/2 − 1] and w[TAPS/2]:
            // half a filter's length of delay, about half a millisecond.
            // One channel's taps when the two agree, which mono always does.
            let l = sinc.at(|k| w[k][0], s.phase);
            let stereo = w.iter().any(|p| p[0] != p[1]);
            let r = if stereo { sinc.at(|k| w[k][1], s.phase) } else { l };
            s.phase += step;
            let whole = s.phase.floor() as usize;
            if whole > 0 {
                read = (read + whole).min(q.len());
                s.phase -= whole as f64;
            }
            if fold { ((l + r) * 0.5, (l + r) * 0.5) } else { (l, r) }
        };
        peak = peak.max(l.abs()).max(r.abs());
        sum += (l * l + r * r) * 0.5;
        let base = f * channels;
        if channels >= 2 {
            (out[base], out[base + 1]) = limiter.process(l * gl, r * gr);
            for c in 2..channels {
                out[base + c] = 0.0;
            }
        } else if channels == 1 {
            out[base] = limiter.process(l * gain, l * gain).0;
        }
    }
    s.queue.drain(..read);
    s.sinc = Some(sinc);
    s.limiter = Some(limiter);
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

    /// Full volume and past it: a near-full-scale tone at +12 dB comes out
    /// at the ceiling and no further, and at unity the limiter is a plain
    /// delay that changes nothing.
    #[test]
    fn gain_past_unity_is_limited_not_clipped() {
        let mut lim = Limiter::new(48_000.0);
        let tone = |n: usize| (std::f64::consts::TAU * 1_000.0 * n as f64 / 48_000.0).sin() as f32 * 0.85;
        let out: Vec<f32> = (0..48_000).map(|n| lim.process(tone(n) * 4.0, tone(n) * 4.0).0).collect();
        let peak = out.iter().fold(0f32, |m, v| m.max(v.abs()));
        let settled = rms(&out[24_000..]);
        assert!(peak <= LIMIT_CEILING, "peaked at {peak}");
        // A sine held at the ceiling, not squared off: its RMS stays near
        // the sine's 0.707 of peak rather than climbing toward a square's 1.
        assert!((settled / LIMIT_CEILING - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.03, "rms {settled}");

        let mut lim = Limiter::new(48_000.0);
        let look = lim.look;
        let out: Vec<f32> = (0..4_800).map(|n| lim.process(tone(n), tone(n)).0).collect();
        for n in look..4_800 {
            assert_eq!(out[n], tone(n - look));
        }
    }

    #[test]
    fn the_queue_runs_as_deep_as_the_link_needs() {
        // A wired LAN: frames every 13 ms, now and then one 35 ms after
        // the last, into a card that takes 21 ms a callback.
        let mut s = Shared { gap_peak_s: 0.060, callback_s: 1024.0 / 48_000.0, ..Default::default() };
        assert_eq!(s.target(), TARGET_S, "the fixed depth until a frame arrives");
        for k in 0..6000 {
            s.arrived(if k % 200 == 0 { 0.035 } else { 0.013 });
        }
        let lan = s.target();
        assert!((0.065..0.080).contains(&lan), "LAN target {lan}");
        // A link that stalls 150 ms at a time gets the depth to ride it out.
        for k in 0..400 {
            s.arrived(if k % 50 == 0 { 0.150 } else { 0.013 });
        }
        assert!(s.target() > 0.170, "bad link target {}", s.target());
        // Never under the floor, however clean, nor over the ceiling.
        let mut clean = Shared { callback_s: 0.005, ..Default::default() };
        clean.arrived(0.010);
        assert_eq!(clean.target(), TARGET_MIN_S);
        clean.arrived(0.9);
        assert_eq!(clean.target(), TARGET_MAX_S);
        // A pause in the stream is not jitter.
        let before = s.gap_peak_s;
        s.arrived(3.0);
        assert!(s.gap_peak_s <= before);
    }

    #[test]
    fn an_output_opened_again_starts_from_what_arrives_next() {
        let p = Player::silent();
        p.set_idle(true);
        p.push(48_000, 1, &[100i16; 4_800]);
        p.set_idle(false);
        let s = p.shared.lock().unwrap();
        assert!(s.queue.is_empty() && s.priming);
        assert!(s.last_push.is_none(), "the wait before it is not the link's");
    }

    #[test]
    fn a_flush_or_a_mode_change_is_not_measured_as_a_gap() {
        let p = Player::silent();
        let frame = vec![0i16; 640];
        p.push(48_000, 1, &frame);
        p.push(48_000, 1, &frame);
        let settled = p.shared.lock().unwrap().gap_peak_s;
        // A reconnect: the stream comes back 300 ms after the flush.
        p.flush();
        std::thread::sleep(std::time::Duration::from_millis(300));
        p.push(48_000, 1, &frame);
        assert_eq!(p.shared.lock().unwrap().gap_peak_s, settled);
        // A mode change: the first frame at the new rate, 300 ms on.
        std::thread::sleep(std::time::Duration::from_millis(300));
        p.push(16_000, 1, &frame);
        assert_eq!(p.shared.lock().unwrap().gap_peak_s, settled);
        // A late frame within a stream is still the link's.
        std::thread::sleep(std::time::Duration::from_millis(300));
        p.push(16_000, 1, &frame);
        assert!(p.shared.lock().unwrap().gap_peak_s >= 0.3);
    }

    #[test]
    fn an_underrun_deepens_the_queue_and_a_measured_target_is_held() {
        let shared = Mutex::new(Shared { in_rate: 48_000.0, out_rate: 48_000.0, volume: 1.0, priming: true, ..Default::default() });
        shared.lock().unwrap().arrived(0.020);
        let target = shared.lock().unwrap().target();
        let mut out = vec![0f32; 960];
        // Ten steady seconds at the measured depth: it holds, with no underrun.
        for _ in 0..500 {
            let mut s = shared.lock().unwrap();
            s.queue.extend(std::iter::repeat_n([0.1; 2], 960));
            s.arrived(0.020);
            drop(s);
            fill(&shared, &mut out, 1);
        }
        {
            let s = shared.lock().unwrap();
            let depth = s.queue.len() as f64 / s.in_rate;
            assert!((depth - target).abs() < 0.03, "queue at {depth} s for a target of {target}");
            assert_eq!(s.underruns, 0);
        }
        // The stream stops: the queue runs dry once, and the target rises.
        for _ in 0..20 {
            fill(&shared, &mut out, 1);
        }
        let mut s = shared.lock().unwrap();
        assert_eq!(s.underruns, 1);
        s.arrived(0.020);
        assert!(s.target() >= target + 0.02, "target {} after an underrun, {target} before", s.target());
    }

    /// A pure tone through the live path as it really runs: frames of
    /// 13.3 ms arriving with jitter, the card pulling 1024 frames at a time.
    /// Returns how far everything that is not the tone sits under it, dB.
    fn tone_purity(target_s: f64, tone_hz: f64) -> f64 {
        let (in_rate, out_rate) = (51_613.0f64, 48_000.0f64);
        let shared = Mutex::new(Shared { in_rate, out_rate, volume: 1.0, priming: true, target_s, ..Default::default() });
        let mut seed = 0x1234_5678u32;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f64 / u32::MAX as f64
        };
        let frame = (in_rate * 106_496.0 / 8e6) as usize;
        let period = 106_496.0 / 8e6;
        let (mut k_arrive, mut t_arrive, mut t_card, mut n_in) = (0u64, 0.0f64, 0.0f64, 0u64);
        let mut out_all: Vec<f32> = Vec::new();
        let mut buf = vec![0f32; 1024];
        while t_card < 12.0 {
            // Arrivals due before this callback: on their 13.3 ms grid, each
            // up to 9 ms late, now and then two together.
            while t_arrive <= t_card {
                let mut s = shared.lock().unwrap();
                for _ in 0..frame {
                    let v = 0.5 * (std::f64::consts::TAU * tone_hz * n_in as f64 / in_rate).sin() as f32;
                    s.queue.push_back([v; 2]);
                    n_in += 1;
                }
                k_arrive += 1;
                t_arrive = k_arrive as f64 * period + rnd() * 0.009;
            }
            fill(&shared, &mut buf, 1);
            out_all.extend_from_slice(&buf);
            t_card += 1024.0 / out_rate;
        }
        assert_eq!(shared.lock().unwrap().underruns, 0);
        // The last five seconds, Hann-windowed: the tone's power within
        // 20 Hz of where it lands, against all the rest.
        let x = &out_all[out_all.len() - 240_000..];
        let n = x.len();
        let w: Vec<f64> = (0..n).map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos()).collect();
        let total: f64 = x.iter().zip(&w).map(|(&v, &w)| (v as f64 * w).powi(2)).sum::<f64>();
        let mut tone = 0.0;
        let df = out_rate / n as f64;
        let k0 = (tone_hz / df).round() as i64;
        for k in k0 - 100..=k0 + 100 {
            let f = k as f64 * df;
            let (mut re, mut im) = (0.0, 0.0);
            for (i, (&v, &w)) in x.iter().zip(&w).enumerate() {
                let ph = std::f64::consts::TAU * f * i as f64 / out_rate;
                re += v as f64 * w * ph.cos();
                im -= v as f64 * w * ph.sin();
            }
            // Twice: the other half of a real tone is at the negative frequency.
            tone += 2.0 * (re * re + im * im) / n as f64;
        }
        10.0 * (tone / (total - tone).max(1e-30)).log10()
    }

    #[test]
    fn steering_the_clock_does_not_wobble_the_pitch() {
        // Steered by the depth at each callback, the ratio swung with the
        // arrivals and the residue under a 10 kHz tone was 28 dB down at a
        // 120 ms queue and 22 dB at 60 ms: broadcast treble with a rough
        // edge. Whatever the queue's depth, it must be far under the tone.
        for target in [0.12, 0.06] {
            for (hz, least) in [(1_000.0, 70.0), (5_000.0, 60.0), (10_000.0, 55.0)] {
                let db = tone_purity(target, hz);
                println!("target {:>3.0} ms tone {:>5.0} Hz: residue {db:.1} dB under the tone", target * 1e3, hz);
                assert!(db > least, "{hz} Hz at a {target} s queue: residue only {db:.1} dB down");
            }
        }
    }

    /// Program that blends between mono and stereo every few frames, with
    /// a stall long enough to run the queue dry, through `fill` at callback
    /// sizes that do not divide the frames. With `wrap` the queue's ring
    /// buffer starts part way round, so it wraps as it fills.
    fn blended_through_fill(wrap: bool) -> (Vec<f32>, u64) {
        let shared = Mutex::new(Shared { in_rate: 51_200.0, out_rate: 48_000.0, volume: 1.0, priming: true, target_s: 0.06, ..Default::default() });
        if wrap {
            let mut s = shared.lock().unwrap();
            s.queue = VecDeque::with_capacity(8_192);
            s.queue.extend(std::iter::repeat_n([0.0; 2], 5_000));
            s.queue.drain(..5_000);
        }
        let mut out_all = Vec::new();
        let mut n_in = 0u64;
        for k in 0..400usize {
            if !(200..215).contains(&k) {
                let mut s = shared.lock().unwrap();
                let right = if (k / 20) % 2 == 0 { 1.0 } else { 0.5 };
                for _ in 0..683 {
                    let v = 0.5 * (std::f64::consts::TAU * 1_000.0 * n_in as f64 / 51_200.0).sin() as f32;
                    s.queue.push_back([v, v * right]);
                    n_in += 1;
                }
            }
            let mut buf = vec![0f32; 2 * if k % 3 == 0 { 600 } else { 660 }];
            fill(&shared, &mut buf, 2);
            out_all.extend_from_slice(&buf);
        }
        let underruns = shared.lock().unwrap().underruns;
        (out_all, underruns)
    }

    #[test]
    fn where_the_queue_wraps_changes_nothing_played() {
        let (straight, u1) = blended_through_fill(false);
        let (wrapped, u2) = blended_through_fill(true);
        assert!(u1 >= 1, "the stall runs the queue dry");
        assert_eq!(u1, u2);
        assert!(straight.iter().zip(&wrapped).all(|(a, b)| a.to_bits() == b.to_bits()));
        // A digest of every sample's bits: the same before and after the
        // queue was read as a slice rather than tap by tap.
        let digest = straight.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, v| (h ^ v.to_bits() as u64).wrapping_mul(0x100_0000_01b3));
        println!("fill digest {digest:016x}, {} samples", straight.len());
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
