//! The NCDXF/IARU International Beacon Project: 18 beacons on five bands,
//! each sending for 10 seconds on 14.100, 18.110, 21.150, 24.930 and 28.200 MHz
//! in turn, so one three-minute cycle visits every beacon on every band.
//!
//! What a beacon sends (ncdxf.org/beacon): its callsign in Morse at 22 words a
//! minute, then four one-second dashes. The callsign and the first dash go out
//! at 100 W, the other dashes at 10 W, 1 W and 100 mW. 4U1UN starts the cycle
//! on 14.100 MHz at the top of every third minute, and each beacon steps up a
//! band every 10 seconds, so which beacon is on the air is known from the clock
//! alone.
//!
//! The detector is told the band and the pitch the dial puts the carrier at,
//! and is fed upper-sideband audio stamped with the wall clock. For each
//! 10-second slot it looks for the scheduled beacon's own keying: the carrier
//! is found within ±[`SEARCH_HZ`] of the pitch, its envelope is matched against
//! the callsign's Morse at every start up to [`LATE_S`] after the slot opens,
//! and the call counts as heard only when the match stands well clear of what
//! the same matching finds in the noise beside it, and the scheduled call fits
//! better than any of the other seventeen. Then each dash is measured in turn,
//! and the steps heard (1 to 4: down to 100 W, 10 W, 1 W, 100 mW) are the dashes
//! heard in a row from the first.

use num_complex::Complex32;
use rustfft::FftPlanner;
use std::collections::VecDeque;
use std::f32::consts::TAU;

/// The five frequencies, in the order each beacon visits them.
pub const BANDS_HZ: [u64; 5] = [14_100_000, 18_110_000, 21_150_000, 24_930_000, 28_200_000];

pub struct Beacon {
    pub call: &'static str,
    pub place: &'static str,
    /// Maidenhead locator, six characters, as the project lists it.
    pub grid: &'static str,
}

/// In transmit order.
#[rustfmt::skip]
pub const BEACONS: [Beacon; 18] = [
    Beacon { call: "4U1UN", place: "United Nations, New York", grid: "FN30as" },
    Beacon { call: "VE8AT", place: "Inuvik, Canada", grid: "CP38gh" },
    Beacon { call: "W6WX", place: "Mt. Umunhum, California", grid: "CM97bd" },
    Beacon { call: "KH6RS", place: "Maui, Hawaii", grid: "BL10ts" },
    Beacon { call: "ZL6B", place: "Masterton, New Zealand", grid: "RE78tw" },
    Beacon { call: "VK6RBP", place: "Rolystone, Australia", grid: "OF87av" },
    Beacon { call: "JA2IGY", place: "Mt. Asama, Japan", grid: "PM84jk" },
    Beacon { call: "RR9O", place: "Novosibirsk, Russia", grid: "NO14kx" },
    Beacon { call: "VR2B", place: "Hong Kong", grid: "OL72bg" },
    Beacon { call: "4S7B", place: "Colombo, Sri Lanka", grid: "MJ96wv" },
    Beacon { call: "ZS6DN", place: "Pretoria, South Africa", grid: "KG33xi" },
    Beacon { call: "5Z4B", place: "Kikuyu, Kenya", grid: "KI88hr" },
    Beacon { call: "4X6TU", place: "Tel Aviv, Israel", grid: "KM72jb" },
    Beacon { call: "OH2B", place: "Lohja, Finland", grid: "KP20eh" },
    Beacon { call: "CS3B", place: "São Jorge, Madeira", grid: "IM12jt" },
    Beacon { call: "LU4AA", place: "Buenos Aires, Argentina", grid: "GF05tj" },
    Beacon { call: "OA4B", place: "Lima, Peru", grid: "FH17mw" },
    Beacon { call: "YV5B", place: "Caracas, Venezuela", grid: "FK60nd" },
];

/// The band index of a beacon frequency, if it is one.
pub fn band_of(freq_hz: u64) -> Option<usize> {
    BANDS_HZ.iter().position(|&f| f == freq_hz)
}

/// Which beacon is on `band` in the 10-second slot holding `utc_s`.
pub fn on_air(band: usize, utc_s: u64) -> usize {
    ((utc_s % 180) as usize / 10 + 18 - band % 5) % 18
}

/// A beacon heard in one slot.
#[derive(Clone, Debug, PartialEq)]
pub struct Heard {
    pub band: usize,
    /// Index into [`BEACONS`].
    pub beacon: usize,
    /// The slot's start, UTC seconds.
    pub slot_utc: u64,
    /// The callsign's strength over the noise in 2500 Hz, as FT8 and WSPR
    /// report it.
    pub snr_db: f32,
    /// Each dash the same way, `None` past the last one heard.
    pub dashes_db: [Option<f32>; 4],
    /// Dashes heard in a row from the first: 0 is the callsign alone, 1 the
    /// first dash at 100 W, 4 down to the last at 100 mW.
    pub steps: u8,
    /// The carrier's pitch less the expected one.
    pub offset_hz: f32,
    /// How far the callsign started after the slot opened, by this clock.
    pub late_ms: i32,
    /// The match's standing over the noise, in standard deviations.
    pub score: f32,
}

/// Words a minute: a dot is 1.2 s / 22.
const WPM: f32 = 22.0;
/// How far from the expected pitch the carrier may be: the receiver's own
/// error is under 12 Hz at 28 MHz, the rest is room for a beacon's.
const SEARCH_HZ: f32 = 60.0;
/// Baseband rate after the mix: wide enough for the search and the noise
/// references beside it.
const RATE: f32 = 800.0;
/// The envelope's smoothing, a 30 ms Hann window (50 Hz of noise bandwidth,
/// and next to nothing let through from a strong carrier 250 Hz away), and
/// its sample spacing, 5 ms.
const SMOOTH: usize = 24;
const STEP: usize = 4;
/// Where the noise is read, either side of the carrier.
const REF_OFFSET_HZ: f32 = 250.0;
/// The window analyzed around each slot, before its start and after its end,
/// for a clock or a beacon a little off.
const EARLY_S: f32 = 0.5;
const LATE_S: f32 = 1.5;
/// A call is heard at this many standard deviations of the noise's own match.
const CALL_THRESHOLD: f32 = 6.0;
/// A dash is heard at this many standard deviations of a dash's worth of noise.
const DASH_THRESHOLD: f32 = 4.0;
/// A dash is one second; it is measured over its middle, and the next is
/// looked for this soon after one ends.
const DASH_S: f32 = 1.0;
const DASH_MEASURE_S: f32 = 0.8;
const DASH_GAP_MAX_S: f32 = 0.6;

/// Keying of `call` at [`WPM`], one value per envelope sample: 1 key down.
fn keying(call: &str) -> Vec<f32> {
    let dot = 1.2 / WPM * RATE / STEP as f32;
    let mut units: Vec<bool> = Vec::new();
    for (i, ch) in call.chars().enumerate() {
        if i > 0 {
            units.extend([false; 3]);
        }
        let code = crate::cw::pattern_of(ch).unwrap_or("");
        for (j, e) in code.bytes().enumerate() {
            if j > 0 {
                units.push(false);
            }
            units.extend(std::iter::repeat_n(true, if e == b'-' { 3 } else { 1 }));
        }
    }
    let n = (units.len() as f32 * dot).round() as usize;
    (0..n).map(|k| f32::from(u8::from(units[((k as f32 + 0.5) / dot) as usize % units.len()]))).collect()
}

/// The best start of `template` in `env`, and the on-less-off difference there.
fn best_match(env: &[f32], template: &[f32], starts: std::ops::Range<usize>) -> (usize, f32) {
    let on: f32 = template.iter().sum();
    let off = template.len() as f32 - on;
    let mut best = (starts.start, f32::MIN);
    for s in starts {
        let Some(w) = env.get(s..s + template.len()) else { break };
        let (mut a, mut b) = (0.0, 0.0);
        for (x, t) in w.iter().zip(template) {
            if *t > 0.5 { a += x } else { b += x }
        }
        let d = a / on - b / off;
        if d > best.1 {
            best = (s, d);
        }
    }
    best
}

fn mean(x: &[f32]) -> f32 {
    x.iter().sum::<f32>() / x.len().max(1) as f32
}

fn sd(x: &[f32]) -> f32 {
    let m = mean(x);
    (x.iter().map(|v| (v - m) * (v - m)).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

pub struct BeaconDetector {
    band: usize,
    fs: f32,
    pitch_hz: f32,
    phase: f32,
    /// The decimator: the mean of each `factor` samples. The channel's own
    /// filter has already taken away whatever would fold onto the carrier.
    factor: usize,
    acc: Complex32,
    fill: usize,
    /// Baseband at [`RATE`], and the wall-clock time of its first sample.
    buf: VecDeque<Complex32>,
    buf_ms: f64,
    /// The next slot to analyze, UTC seconds.
    next_slot: Option<u64>,
    templates: Vec<Vec<f32>>,
    heard: Vec<Heard>,
}

impl BeaconDetector {
    /// `band` indexes [`BANDS_HZ`]; the dial puts the carrier at `pitch_hz`.
    pub fn new(fs: f32, band: usize, pitch_hz: f32) -> Self {
        let factor = (fs / RATE).round().max(1.0) as usize;
        BeaconDetector {
            band,
            fs,
            pitch_hz,
            phase: 0.0,
            factor,
            acc: Complex32::default(),
            fill: 0,
            buf: VecDeque::new(),
            buf_ms: 0.0,
            next_slot: None,
            templates: BEACONS.iter().map(|b| keying(b.call)).collect(),
            heard: Vec::new(),
        }
    }

    fn rate(&self) -> f64 {
        f64::from(self.fs) / self.factor as f64
    }

    /// Audio whose first sample was taken at `t0_ms` on the wall clock.
    pub fn push(&mut self, audio: &[f32], t0_ms: u64) {
        let step = TAU * self.pitch_hz / self.fs;
        let first_out = self.buf_ms + self.buf.len() as f64 * 1_000.0 / self.rate();
        // The first output sample this block makes, timed from the block.
        let expect = t0_ms as f64 + (self.factor - self.fill) as f64 * 1_000.0 / f64::from(self.fs);
        if self.buf.is_empty() || (expect - first_out).abs() > 250.0 {
            // A gap or a jump: start again from here.
            if !self.buf.is_empty() {
                self.buf.clear();
                self.next_slot = None;
            }
            self.buf_ms = expect;
        }
        for &x in audio {
            let (s, c) = self.phase.sin_cos();
            self.phase += step;
            if self.phase > TAU {
                self.phase -= TAU;
            }
            self.acc += Complex32::new(x * c, -x * s);
            self.fill += 1;
            if self.fill == self.factor {
                self.buf.push_back(self.acc / self.factor as f32);
                self.acc = Complex32::default();
                self.fill = 0;
            }
        }
        self.analyze_ready();
    }

    /// Beacons heard since the last call.
    pub fn take(&mut self) -> Vec<Heard> {
        std::mem::take(&mut self.heard)
    }

    fn analyze_ready(&mut self) {
        let rate = self.rate();
        let per_ms = rate / 1_000.0;
        let end_ms = self.buf_ms + self.buf.len() as f64 / per_ms;
        let first_full = ((self.buf_ms / 1_000.0 + f64::from(EARLY_S)) / 10.0).ceil() as u64 * 10;
        let mut slot = *self.next_slot.get_or_insert(first_full);
        while (slot as f64 + 10.0 + f64::from(LATE_S)) * 1_000.0 <= end_ms {
            let from = ((slot as f64 - f64::from(EARLY_S)) * 1_000.0 - self.buf_ms) * per_ms;
            let to = ((slot as f64 + 10.0 + f64::from(LATE_S)) * 1_000.0 - self.buf_ms) * per_ms;
            if from >= 0.0 {
                let z: Vec<Complex32> = self.buf.range(from as usize..(to as usize).min(self.buf.len())).copied().collect();
                if let Some(h) = self.analyze(&z, slot) {
                    self.heard.push(h);
                }
            }
            slot += 10;
        }
        self.next_slot = Some(slot);
        // Keep from the next slot's early margin on.
        let keep_ms = (slot as f64 - f64::from(EARLY_S)) * 1_000.0;
        let drop = (((keep_ms - self.buf_ms) * per_ms).floor().max(0.0) as usize).min(self.buf.len());
        self.buf.drain(..drop);
        self.buf_ms += drop as f64 / per_ms;
    }

    /// Power envelope of `z` mixed down by `hz`, one value per [`STEP`].
    fn envelope(z: &[Complex32], hz: f32, rate: f32) -> Vec<f32> {
        let w = TAU * hz / rate;
        let window: Vec<f32> = (0..SMOOTH).map(|k| 0.5 - 0.5 * (TAU * (k as f32 + 0.5) / SMOOTH as f32).cos()).collect();
        let total: f32 = window.iter().sum();
        let mixed: Vec<Complex32> = z.iter().enumerate().map(|(n, v)| v * Complex32::from_polar(1.0, -w * n as f32)).collect();
        mixed
            .windows(SMOOTH)
            .step_by(STEP)
            .map(|span| (span.iter().zip(&window).map(|(v, k)| v * k).sum::<Complex32>() / total).norm_sqr())
            .collect()
    }

    /// The envelope's noise bandwidth: a Hann window's is 1.5 bins.
    fn noise_bandwidth(rate: f32) -> f32 {
        1.5 * rate / SMOOTH as f32
    }

    fn analyze(&self, z: &[Complex32], slot: u64) -> Option<Heard> {
        let rate = self.rate() as f32;
        // The carrier: the strongest point of the slot's spectrum near the pitch.
        let n = z.len().next_power_of_two();
        let mut spec: Vec<Complex32> = z.to_vec();
        spec.resize(n, Complex32::default());
        FftPlanner::new().plan_fft_forward(n).process(&mut spec);
        let bin_hz = rate / n as f32;
        let reach = (SEARCH_HZ / bin_hz) as i64;
        let smooth = (1.5 / bin_hz).ceil() as i64;
        let power = |k: i64| spec[k.rem_euclid(n as i64) as usize].norm_sqr();
        let (mut best_k, mut best_p) = (0i64, f32::MIN);
        for k in -reach..=reach {
            let p: f32 = (k - smooth..=k + smooth).map(power).sum();
            if p > best_p {
                (best_k, best_p) = (k, p);
            }
        }
        let offset_hz = best_k as f32 * bin_hz;

        let env = Self::envelope(z, offset_hz, rate);
        let refs = [Self::envelope(z, offset_hz - REF_OFFSET_HZ, rate), Self::envelope(z, offset_hz + REF_OFFSET_HZ, rate)];
        // Noise power per envelope sample: the references' lower quartile,
        // which neither a stray carrier nor a strong beacon's keying
        // sidebands much move, scaled to the mean of noise alone (an
        // exponential's quartile is ln(4/3) of its mean).
        let mut all: Vec<f32> = refs.iter().flatten().copied().collect();
        if all.len() < 100 {
            return None;
        }
        all.sort_by(f32::total_cmp);
        let noise = all[all.len() / 4] / (4.0f32 / 3.0).ln();
        if noise <= 0.0 {
            return None;
        }
        let q: Vec<f32> = env.iter().map(|p| p / noise).collect();
        let qr: Vec<Vec<f32>> = refs.iter().map(|r| r.iter().map(|p| p / noise).collect()).collect();

        let per_s = rate / STEP as f32;
        let starts = 0..((EARLY_S + LATE_S) * per_s) as usize;
        let own = &self.templates[on_air(self.band, slot)];
        let (at, d) = best_match(&q, own, starts.clone());
        // The same match run through the noise beside the carrier, at every
        // start, says what a difference that large means.
        let null: Vec<f32> = qr
            .iter()
            .flat_map(|r| {
                starts.clone().filter_map(|s| r.get(s..s + own.len())).map(|w| {
                    let on: f32 = own.iter().sum();
                    let (a, b) = w.iter().zip(own).fold((0.0, 0.0), |(a, b), (x, t)| if *t > 0.5 { (a + x, b) } else { (a, b + x) });
                    a / on - b / (own.len() as f32 - on)
                })
            })
            .collect();
        let spread = sd(&null).max(1e-6);
        let score = (d - mean(&null)) / spread;
        if score < CALL_THRESHOLD {
            return None;
        }
        let beacon = on_air(self.band, slot);
        // Seventeen other calls: the scheduled one has to fit best.
        for (i, t) in self.templates.iter().enumerate() {
            if i != beacon && best_match(&q, t, starts.clone()).1 >= d {
                return None;
            }
        }

        // To the 2500 Hz the amateur modes report in.
        let to_db = |s: f32| 10.0 * (s.max(1e-4) * Self::noise_bandwidth(rate) / 2_500.0).log10();
        let on: Vec<f32> = q[at..at + own.len()].iter().zip(own).filter(|(_, t)| **t > 0.5).map(|(x, _)| *x).collect();
        let snr_db = to_db(mean(&on) - 1.0);

        // The dashes, each looked for just after the last one ended.
        let dash = (DASH_S * per_s) as usize;
        let measure = (DASH_MEASURE_S * per_s) as usize;
        let edge = (dash - measure) / 2;
        let dash_noise: Vec<f32> = qr.iter().flat_map(|r| r.chunks_exact(measure).map(mean)).collect();
        let dash_sd = sd(&dash_noise).max(1e-6);
        let mut dashes_db = [None; 4];
        let mut steps = 0;
        let mut from = at + own.len();
        for slot_db in &mut dashes_db {
            let window = from..from + (DASH_GAP_MAX_S * per_s) as usize;
            let Some((start, level)) = window.filter_map(|s| q.get(s + edge..s + edge + measure).map(|w| (s, mean(w) - 1.0))).max_by(|a, b| a.1.total_cmp(&b.1)) else {
                break;
            };
            if level < DASH_THRESHOLD * dash_sd {
                break;
            }
            *slot_db = Some(to_db(level));
            steps += 1;
            from = start + dash;
        }
        // An envelope sample stands for the window that starts at it.
        let late_ms = ((at as f32 / per_s - EARLY_S + SMOOTH as f32 / 2.0 / rate) * 1_000.0).round() as i32;
        Some(Heard { band: self.band, beacon, slot_utc: slot, snr_db, dashes_db, steps, offset_hz, late_ms, score })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f32 = 12_000.0;

    /// A simple, repeatable noise source.
    struct Noise(u64);
    impl Noise {
        fn gauss(&mut self) -> f32 {
            let mut u = || {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                (self.0 >> 11) as f32 / (1u64 << 53) as f32
            };
            let (a, b) = (u().max(1e-9), u());
            (-2.0 * a.ln()).sqrt() * (TAU * b).cos()
        }
    }

    /// `secs` of audio from `t0` (UTC seconds) with the scheduled beacon on
    /// `band` keyed at `snr_db` (2500 Hz), callsign starting `late` seconds
    /// into its slot, dashes `gap` apart, at `pitch` Hz; `call` overrides
    /// which callsign is keyed.
    fn audio(band: usize, t0: u64, secs: u64, snr_db: f32, late: f32, gap: f32, pitch: f32, call: Option<&str>, seed: u64) -> Vec<f32> {
        let n = (secs as f32 * FS) as usize;
        let mut noise = Noise(seed);
        // Noise of unit power per sample has 1/(FS/2) per Hz of the real band.
        let n0 = 1.0 / (FS / 2.0);
        let amp = (2.0 * 10f32.powf(snr_db / 10.0) * n0 * 2_500.0).sqrt();
        let mut x: Vec<f32> = (0..n).map(|_| noise.gauss()).collect();
        let dot = 1.2 / WPM;
        for slot in (t0..t0 + secs).step_by(10) {
            let name = call.unwrap_or(BEACONS[on_air(band, slot)].call);
            let mut key: Vec<(f32, f32, f32)> = Vec::new();
            let mut t = (slot - t0) as f32 + late;
            for (i, ch) in name.chars().enumerate() {
                if i > 0 {
                    t += 2.0 * dot;
                }
                for e in crate::cw::pattern_of(ch).unwrap().bytes() {
                    let len = if e == b'-' { 3.0 * dot } else { dot };
                    key.push((t, t + len, 1.0));
                    t += len + dot;
                }
            }
            t += 2.0 * dot;
            for level in [1.0, 0.1, 0.01, 0.001] {
                key.push((t, t + 1.0, level));
                t += 1.0 + gap;
            }
            for (a, b, level) in key {
                let (i0, i1) = (((a * FS) as usize).min(n), ((b * FS) as usize).min(n));
                // 5 ms raised-cosine edges, as a transmitter shapes its keying.
                let edge = 0.005 * FS;
                for (i, v) in x[i0..i1].iter_mut().enumerate() {
                    let k = i0 + i;
                    let from_edge = (i as f32).min((i1 - i0 - i) as f32);
                    let shape = if from_edge < edge { 0.5 - 0.5 * (std::f32::consts::PI * from_edge / edge).cos() } else { 1.0 };
                    *v += shape * amp * level.sqrt() * (TAU * pitch * k as f32 / FS).sin();
                }
            }
        }
        x
    }

    fn run(band: usize, t0: u64, x: &[f32]) -> Vec<Heard> {
        let mut d = BeaconDetector::new(FS, band, 700.0);
        let block = 1_200;
        let mut out = Vec::new();
        for (i, b) in x.chunks(block).enumerate() {
            d.push(b, t0 * 1_000 + (i * block) as u64 * 1_000 / FS as u64);
            out.extend(d.take());
        }
        out
    }

    #[test]
    fn the_schedule_starts_with_4u1un_on_14_mhz() {
        assert_eq!(BEACONS[on_air(0, 0)].call, "4U1UN");
        assert_eq!(BEACONS[on_air(1, 10)].call, "4U1UN");
        assert_eq!(BEACONS[on_air(4, 40)].call, "4U1UN");
        assert_eq!(BEACONS[on_air(0, 170)].call, "YV5B");
        assert_eq!(BEACONS[on_air(4, 0)].call, "CS3B");
        assert_eq!(BEACONS[on_air(0, 180 + 10)].call, "VE8AT");
        assert_eq!(band_of(24_930_000), Some(3));
    }

    #[test]
    fn a_strong_beacon_is_heard_down_to_its_last_step() {
        let t0 = 1_794_360_000;
        let x = audio(0, t0, 60, 25.0, 0.2, 0.2, 707.0, None, 1);
        let heard = run(0, t0, &x);
        assert!(heard.len() >= 4, "{heard:?}");
        for h in &heard {
            assert_eq!(h.beacon, on_air(0, h.slot_utc));
            assert!((h.snr_db - 25.0).abs() < 2.5, "{h:?}");
            assert_eq!(h.steps, 4, "{h:?}");
            assert!((h.offset_hz - 7.0).abs() < 1.0, "{h:?}");
            assert!((h.late_ms - 200).abs() <= 15, "{h:?}");
            let d = h.dashes_db;
            assert!((d[1].unwrap() - (d[0].unwrap() - 10.0)).abs() < 2.0, "{h:?}");
        }
    }

    #[test]
    fn a_weak_one_is_heard_at_full_power_only() {
        let t0 = 1_794_360_000;
        let x = audio(2, t0, 60, -8.0, 0.4, 0.3, 690.0, None, 2);
        let heard = run(2, t0, &x);
        assert!(heard.len() >= 4, "{heard:?}");
        assert!(heard.iter().all(|h| (1..=2).contains(&h.steps)), "{heard:?}");
    }

    #[test]
    fn noise_and_the_wrong_callsign_are_not_heard() {
        let t0 = 1_794_360_000;
        let x = audio(1, t0, 1_800, -100.0, 0.2, 0.2, 700.0, None, 3);
        assert!(run(1, t0, &x).is_empty());
        // A beacon off its schedule (or another station keying a call) is not
        // the beacon the clock says is on.
        let x = audio(1, t0, 60, 20.0, 0.2, 0.2, 700.0, Some("OH2B"), 4);
        let heard = run(1, t0, &x);
        assert!(heard.iter().all(|h| BEACONS[h.beacon].call == "OH2B"), "{heard:?}");
        assert!(heard.len() <= 1);
    }
}
