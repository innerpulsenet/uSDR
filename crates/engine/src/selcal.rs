//! SELCAL (ARINC 714), the selective call that rings a flight crew on an
//! HF voice channel, read from the sideband audio.
//!
//! A ground station sends four letters as two pairs of simultaneous tones:
//! one pair for about a second, about 0.2 s of nothing, the second pair for
//! about a second. There are sixteen letters, each a fixed tone, the tones
//! a ratio of about 1.109 apart from 312.6 to 1479.1 Hz.
//!
//! The receiver takes the audio to 8 kHz and, every 25 ms, the spectrum of
//! the last 100 ms, which resolves the two closest tones (34 Hz apart). An
//! HF voice channel is often a few tens of hertz off, and every tone is
//! off by the same amount, so each spectrum is read at every offset within
//! [`MAX_OFFSET_HZ`]. A frame is a pair when two letters hold most of the
//! band's power between them and no third comes near. A call is a pair
//! that lasts as long as a SELCAL pair lasts, a gap as long as the gap,
//! and a second, different pair at the same offset; speech and carriers
//! that happen to touch two tones do not keep to that timing.
//!
//! A channel more than about 30 Hz off can read one letter too high on the
//! lowest letters, where the tones are almost evenly spaced.

use crate::mfsk::resample::Resampler;
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

const RATE: f32 = 8_000.0;
/// Spectrum window: 100 ms.
const WINDOW: usize = 800;
/// A new spectrum every 25 ms.
const HOP: usize = 200;
const HOP_S: f32 = HOP as f32 / RATE;
/// FFT length: bins about 2 Hz apart.
const FFT_LEN: usize = 4096;

/// The tone of each letter (ARINC 714). I, N and O are not used.
pub const TONES: [(char, f32); 16] = [
    ('A', 312.6),
    ('B', 346.7),
    ('C', 384.6),
    ('D', 426.6),
    ('E', 473.2),
    ('F', 524.8),
    ('G', 582.1),
    ('H', 645.7),
    ('J', 716.1),
    ('K', 794.3),
    ('L', 881.0),
    ('M', 977.2),
    ('P', 1083.9),
    ('Q', 1202.3),
    ('R', 1333.5),
    ('S', 1479.1),
];

/// How far off the channel may be.
const MAX_OFFSET_HZ: f32 = 40.0;
const OFFSET_STEP_HZ: f32 = 5.0;
/// A farther offset replaces a nearer one only if it fits this much better.
const FARTHER_BETTER: f32 = 1.15;
/// Both pairs of a call must sit this close to one offset.
const OFFSET_AGREE_HZ: f32 = 10.0;
/// The two tones' share of the power from 250 to 1600 Hz.
const PAIR_SHARE_MIN: f32 = 0.45;
/// The weaker of the two against the stronger: a fade can part them, but
/// not by more than this (about 10 dB).
const BALANCE_MIN: f32 = 0.1;
/// The third letter against the weaker of the pair.
const THIRD_MAX: f32 = 0.2;
/// How long a pair lasts, in seconds. Nominally 1 s, plus the window's
/// smear; the shortest allows for a fade clipping one end.
const PAIR_MIN_S: f32 = 0.6;
const PAIR_MAX_S: f32 = 1.5;
/// The gap between the pairs: nominally 0.2 s, narrowed by the window.
const GAP_MIN_S: f32 = 0.05;
const GAP_MAX_S: f32 = 0.5;
/// A one-frame dropout inside a pair does not end it.
const DROPOUT_FRAMES: u32 = 3;
/// The same code again within this long is the same call repeated, which
/// is logged as a repeat rather than a new call.
const REPEAT_S: f32 = 10.0;
/// The reading shows a pair as heard for this long after the last frame.
const LIVE_HOLD_S: f32 = 0.3;

#[derive(Clone, Debug, PartialEq)]
pub enum SelcalEvent {
    Call {
        /// "AB-CD".
        code: String,
        offset_hz: f32,
        snr_db: f32,
        /// A repeat of the call just logged.
        repeat: bool,
    },
}

impl SelcalEvent {
    pub fn kind(&self) -> &'static str {
        "call"
    }

    pub fn summary(&self) -> String {
        let SelcalEvent::Call { code, repeat, .. } = self;
        if *repeat { format!("SELCAL {code} (repeat)") } else { format!("SELCAL {code}") }
    }

    pub fn fields(&self) -> Vec<(&'static str, String)> {
        let SelcalEvent::Call { code, offset_hz, snr_db, repeat } = self;
        vec![
            ("code", code.clone()),
            ("offsetHz", format!("{offset_hz:.0}")),
            ("snrDb", format!("{snr_db:.1}")),
            ("repeat", repeat.to_string()),
        ]
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SelcalStatus {
    /// A pair is sounding now.
    pub locked: bool,
    /// The pair sounding now ("AB").
    pub pair: Option<String>,
    /// The last code heard.
    pub last_code: Option<String>,
    pub offset_hz: Option<f32>,
    pub snr_db: Option<f32>,
    pub calls: u64,
}

/// Two letters (indices, the lower first) heard in one frame.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Frame {
    pair: (usize, usize),
    offset_hz: f32,
    snr_db: f32,
}

/// A pair that has ended.
#[derive(Clone, Copy, Debug)]
struct Heard {
    pair: (usize, usize),
    offset_hz: f32,
    snr_db: f32,
    /// In seconds of audio.
    end: f32,
}

/// The pair sounding now.
#[derive(Clone, Copy, Debug)]
struct Run {
    pair: (usize, usize),
    frames: u32,
    missed: u32,
    offset_sum: f32,
    snr_sum: f32,
    last: f32,
}

pub struct SelcalDecoder {
    resampler: Resampler,
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
    buf: Vec<Complex32>,
    win: Vec<f32>,
    ring: Vec<f32>,
    pos: usize,
    since_hop: usize,
    power: Vec<f32>,
    tmp: Vec<f32>,
    hops: u64,
    run: Option<Run>,
    first: Option<Heard>,
    last_call: Option<(String, f32)>,
    live: Option<(String, f32, f32, f32)>,
    last_code: Option<String>,
    calls: u64,
    events: Vec<SelcalEvent>,
}

fn pair_text(p: (usize, usize)) -> String {
    [TONES[p.0].0, TONES[p.1].0].iter().collect()
}

impl SelcalDecoder {
    pub fn new(fs: f32) -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_LEN);
        let scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let win = (0..WINDOW).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / WINDOW as f32).cos()).collect();
        Self {
            resampler: Resampler::new(fs, RATE, 3000.0, 3800.0),
            fft,
            scratch,
            buf: vec![Complex32::new(0.0, 0.0); FFT_LEN],
            win,
            ring: vec![0.0; WINDOW],
            pos: 0,
            since_hop: 0,
            power: vec![0.0; FFT_LEN / 2],
            tmp: Vec::with_capacity(4096),
            hops: 0,
            run: None,
            first: None,
            last_call: None,
            live: None,
            last_code: None,
            calls: 0,
            events: Vec::new(),
        }
    }

    fn now(&self) -> f32 {
        self.hops as f32 * HOP_S
    }

    pub fn push(&mut self, audio: &[f32]) {
        let mut tmp = std::mem::take(&mut self.tmp);
        tmp.clear();
        self.resampler.process(audio, &mut tmp);
        for &v in &tmp {
            self.ring[self.pos] = v;
            self.pos = (self.pos + 1) % WINDOW;
            self.since_hop += 1;
            if self.since_hop == HOP {
                self.since_hop = 0;
                self.hops += 1;
                let f = self.frame();
                self.step(f);
            }
        }
        self.tmp = tmp;
    }

    /// The largest power within a bin of a frequency: the offsets are read
    /// on a coarser grid than the bins, and a tone falls between.
    fn tone_power(&self, hz: f32) -> f32 {
        let c = (hz * FFT_LEN as f32 / RATE).round() as usize;
        self.power[c - 1].max(self.power[c]).max(self.power[c + 1])
    }

    /// This hop's spectrum, read for a pair at the best offset.
    fn frame(&mut self) -> Option<Frame> {
        for i in 0..WINDOW {
            let s = self.ring[(self.pos + i) % WINDOW];
            self.buf[i] = Complex32::new(s * self.win[i], 0.0);
        }
        self.buf[WINDOW..].fill(Complex32::new(0.0, 0.0));
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        for (p, z) in self.power.iter_mut().zip(&self.buf) {
            *p = z.norm_sqr();
        }
        let bin = RATE / FFT_LEN as f32;
        let lo = (250.0 / bin) as usize;
        let hi = (1600.0 / bin) as usize;
        let total: f32 = self.power[lo..hi].iter().sum();
        if total <= 0.0 {
            return None;
        }
        // A tone's power is spread over the window's main lobe: this many
        // bins' worth, for a Hann window on this zero padding.
        let lobe = 1.5 * FFT_LEN as f32 / WINDOW as f32;

        // Nearest offsets first, and a farther one must fit clearly better.
        // The lowest tones are nearly evenly spaced (A to B is 34 Hz, B to C
        // 38), so A+B read 37 Hz down looks like B+C; a channel is far more
        // often near its dial than that far off.
        let steps = (MAX_OFFSET_HZ / OFFSET_STEP_HZ) as i32;
        let mut best: Option<(f32, Frame)> = None;
        for s in (0..=2 * steps).map(|k| if k % 2 == 0 { -k / 2 } else { (k + 1) / 2 }) {
            let off = s as f32 * OFFSET_STEP_HZ;
            let mut p = [0f32; 16];
            for (v, (_, hz)) in p.iter_mut().zip(TONES.iter()) {
                *v = self.tone_power(hz + off);
            }
            let (mut a, mut b, mut c) = (0usize, usize::MAX, usize::MAX);
            for i in 1..16 {
                if p[i] > p[a] {
                    (c, b, a) = (b, a, i);
                } else if b == usize::MAX || p[i] > p[b] {
                    (c, b) = (b, i);
                } else if c == usize::MAX || p[i] > p[c] {
                    c = i;
                }
            }
            let (p1, p2, p3) = (p[a], p[b], p[c]);
            let share = (p1 + p2) * lobe / total;
            if share < PAIR_SHARE_MIN || p2 < BALANCE_MIN * p1 || p3 > THIRD_MAX * p2 {
                continue;
            }
            let rest = (total - (p1 + p2) * lobe).max(total * 1e-5);
            let noise_per_bin = rest / (hi - lo) as f32;
            // Two tones' power against the noise in 2500 Hz.
            let snr_db = 10.0 * (((p1 + p2) * lobe) / (noise_per_bin * 2500.0 / bin)).log10();
            if best.as_ref().is_none_or(|(s, _)| share > *s * FARTHER_BETTER) {
                best = Some((share, Frame { pair: (a.min(b), a.max(b)), offset_hz: off, snr_db }));
            }
        }
        best.map(|(_, f)| f)
    }

    fn step(&mut self, f: Option<Frame>) {
        let now = self.now();
        if let Some(f) = f {
            self.live = Some((pair_text(f.pair), f.offset_hz, f.snr_db, now));
        }
        match (self.run.as_mut(), f) {
            (Some(r), Some(f)) if r.pair == f.pair => {
                r.frames += 1;
                r.missed = 0;
                r.offset_sum += f.offset_hz;
                r.snr_sum += f.snr_db;
                r.last = now;
            }
            (Some(r), _) if f.is_none() && r.missed < DROPOUT_FRAMES => r.missed += 1,
            _ => {
                self.end_run();
                self.run = f.map(|f| Run { pair: f.pair, frames: 1, missed: 0, offset_sum: f.offset_hz, snr_sum: f.snr_db, last: now });
            }
        }
    }

    /// The pair has stopped: see whether it finishes a call or may start one.
    fn end_run(&mut self) {
        let Some(r) = self.run.take() else { return };
        let dur = r.frames as f32 * HOP_S;
        // The analysis window stretches a tone by most of its own length.
        let dur = (dur - 0.05).max(0.0);
        // A frame or two of false pair at a tone's edge says nothing about
        // the call around it; something too long to be a pair ends it.
        if dur < PAIR_MIN_S {
            return;
        }
        let first = self.first.take();
        if dur > PAIR_MAX_S {
            return;
        }
        let start = r.last - r.frames as f32 * HOP_S;
        let heard = Heard { pair: r.pair, offset_hz: r.offset_sum / r.frames as f32, snr_db: r.snr_sum / r.frames as f32, end: r.last };
        if let Some(a) = first {
            let gap = start - a.end;
            if (GAP_MIN_S..=GAP_MAX_S).contains(&gap) && a.pair != heard.pair && (a.offset_hz - heard.offset_hz).abs() <= OFFSET_AGREE_HZ {
                self.call(a, heard);
                return;
            }
        }
        self.first = Some(heard);
    }

    fn call(&mut self, a: Heard, b: Heard) {
        let code = format!("{}-{}", pair_text(a.pair), pair_text(b.pair));
        let now = b.end;
        let repeat = self.last_call.as_ref().is_some_and(|(c, t)| *c == code && now - *t <= REPEAT_S);
        self.last_call = Some((code.clone(), now));
        self.last_code = Some(code.clone());
        if !repeat {
            self.calls += 1;
        }
        self.events.push(SelcalEvent::Call { code, offset_hz: (a.offset_hz + b.offset_hz) / 2.0, snr_db: a.snr_db.min(b.snr_db), repeat });
    }

    pub fn take(&mut self) -> Vec<SelcalEvent> {
        std::mem::take(&mut self.events)
    }

    /// Finish a pair the audio ended on.
    pub fn flush(&mut self) {
        self.end_run();
    }

    pub fn status(&self) -> SelcalStatus {
        let live = self.live.as_ref().filter(|l| self.now() - l.3 <= LIVE_HOLD_S);
        SelcalStatus {
            locked: live.is_some(),
            pair: live.map(|l| l.0.clone()),
            last_code: self.last_code.clone(),
            offset_hz: live.map(|l| l.1),
            snr_db: live.map(|l| l.2),
            calls: self.calls,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtty::testsig::{Rng, add_noise, chunks};

    const FS: f32 = 47_904.0;

    fn tone_of(c: char) -> f32 {
        TONES.iter().find(|t| t.0 == c).unwrap().1
    }

    /// A pair of tones for a time, appended to `x`.
    fn pair(x: &mut Vec<f32>, a: char, b: char, secs: f32, off: f32, amp: f32) {
        let n = (secs * FS) as usize;
        let (fa, fb) = (tone_of(a) + off, tone_of(b) + off);
        for i in 0..n {
            let t = i as f32 / FS;
            let edge = (t / 0.005).min((secs - t) / 0.005).clamp(0.0, 1.0);
            x.push(amp * edge * ((std::f32::consts::TAU * fa * t).sin() + (std::f32::consts::TAU * fb * t).sin()));
        }
    }

    fn silence(x: &mut Vec<f32>, secs: f32) {
        x.resize(x.len() + (secs * FS) as usize, 0.0);
    }

    fn call(code: &str, off: f32) -> Vec<f32> {
        let c: Vec<char> = code.chars().collect();
        let mut x = Vec::new();
        silence(&mut x, 0.5);
        pair(&mut x, c[0], c[1], 1.0, off, 0.1);
        silence(&mut x, 0.2);
        pair(&mut x, c[2], c[3], 1.0, off, 0.1);
        silence(&mut x, 0.5);
        x
    }

    fn run(x: &[f32]) -> Vec<SelcalEvent> {
        let mut d = SelcalDecoder::new(FS);
        chunks(x, |c| d.push(c));
        d.flush();
        d.take()
    }

    fn code(e: &SelcalEvent) -> &str {
        let SelcalEvent::Call { code, .. } = e;
        code
    }

    #[test]
    fn reads_a_clean_call() {
        let mut x = call("ABCD", 0.0);
        let mut rng = Rng::new(1);
        add_noise(&mut x, 0.1, 20.0, FS, &mut rng);
        let e = run(&x);
        assert_eq!(e.len(), 1, "{e:?}");
        assert_eq!(code(&e[0]), "AB-CD");
    }

    #[test]
    fn reads_every_letter() {
        let letters: Vec<char> = TONES.iter().map(|t| t.0).collect();
        for w in letters.chunks(4) {
            let s: String = w.iter().collect();
            let e = run(&call(&s, 0.0));
            assert_eq!(e.len(), 1, "{s}: {e:?}");
            assert_eq!(code(&e[0]), format!("{}{}-{}{}", w[0], w[1], w[2], w[3]));
        }
    }

    #[test]
    fn reads_a_call_off_frequency() {
        for off in [-35.0, -12.0, 17.0, 33.0] {
            let e = run(&call("HMPS", off));
            assert_eq!(e.len(), 1, "{off}: {e:?}");
            assert_eq!(code(&e[0]), "HM-PS", "{off}");
            let SelcalEvent::Call { offset_hz, .. } = e[0];
            assert!((offset_hz - off).abs() <= 6.0, "{offset_hz} for {off}");
        }
    }

    #[test]
    fn reads_a_call_in_noise() {
        let mut x = call("EJKQ", 8.0);
        let mut rng = Rng::new(5);
        add_noise(&mut x, 0.1, 0.0, FS, &mut rng);
        let e = run(&x);
        assert_eq!(e.len(), 1, "{e:?}");
        assert_eq!(code(&e[0]), "EJ-KQ");
    }

    #[test]
    fn a_repeat_is_marked() {
        let mut x = call("ABCD", 0.0);
        x.extend(call("ABCD", 0.0));
        let e = run(&x);
        assert_eq!(e.len(), 2, "{e:?}");
        assert!(matches!(e[1], SelcalEvent::Call { repeat: true, .. }));
    }

    #[test]
    fn one_pair_alone_is_not_a_call() {
        let mut x = Vec::new();
        silence(&mut x, 0.5);
        pair(&mut x, 'A', 'B', 1.0, 0.0, 0.1);
        silence(&mut x, 2.0);
        assert!(run(&x).is_empty());
    }

    #[test]
    fn steady_tones_are_not_a_call() {
        let mut x = Vec::new();
        pair(&mut x, 'C', 'H', 10.0, 0.0, 0.1);
        assert!(run(&x).is_empty());
    }

    #[test]
    fn pairs_of_the_wrong_length_are_not_a_call() {
        let mut x = Vec::new();
        silence(&mut x, 0.5);
        pair(&mut x, 'A', 'B', 0.3, 0.0, 0.1);
        silence(&mut x, 0.2);
        pair(&mut x, 'C', 'D', 0.3, 0.0, 0.1);
        silence(&mut x, 0.5);
        assert!(run(&x).is_empty());
    }

    /// A voice-like comb, its fundamental wandering: harmonics cross the
    /// tones all the time and never hold a pair for a second.
    #[test]
    fn speech_like_harmonics_are_not_a_call() {
        let n = (120.0 * FS) as usize;
        let mut x = vec![0f32; n];
        let mut ph = 0f32;
        for (i, v) in x.iter_mut().enumerate() {
            let t = i as f32 / FS;
            let f0 = 130.0 + 35.0 * (t * 1.7).sin() + 12.0 * (t * 5.3).sin();
            ph += std::f32::consts::TAU * f0 / FS;
            let env = (0.5 + 0.5 * (t * 3.1).sin()).powi(2);
            *v = 0.02 * env * (1..12).map(|h| (ph * h as f32).sin() / h as f32).sum::<f32>();
        }
        assert!(run(&x).is_empty());
    }

    #[test]
    fn noise_prints_nothing() {
        let mut x = vec![0f32; (600.0 * 8_000.0) as usize];
        let mut rng = Rng::new(9);
        add_noise(&mut x, 0.1, 0.0, 8_000.0, &mut rng);
        let mut d = SelcalDecoder::new(8_000.0);
        chunks(&x, |c| d.push(c));
        d.flush();
        assert!(d.take().is_empty());
    }

    #[test]
    fn status_shows_the_pair_while_it_sounds() {
        let mut x = Vec::new();
        pair(&mut x, 'A', 'D', 0.8, 0.0, 0.1);
        let mut d = SelcalDecoder::new(FS);
        d.push(&x);
        let s = d.status();
        assert!(s.locked, "{s:?}");
        assert_eq!(s.pair.as_deref(), Some("AD"));
    }
}
