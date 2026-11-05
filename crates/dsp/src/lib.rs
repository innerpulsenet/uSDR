//! Spectrum estimation and the narrowband tuning chain feeding the decoders.

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::f32::consts::PI;
use std::sync::Arc;

/// Analysis windows, and the trade they represent.
///
/// A window buys sidelobe suppression with main-lobe width. Sidelobes are
/// leakage: a strong signal smeared across the bins around it, which raises
/// the floor a weak neighbour has to stand out of. Main-lobe width is the
/// opposite concern — a narrowband signal spread over more bins has a lower
/// peak, and every detector here works from peak height over a local floor.
///
/// So the choice is not free in either direction, and which one wins depends
/// on whether the weak signal you care about has a loud neighbour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Window {
    /// First sidelobe -31 dB, noise bandwidth 1.50 bins. Kept for the bench
    /// that chose against it: the comparison is the justification for the
    /// default, and it is worth being able to re-run rather than trust.
    #[allow(dead_code)]
    Hann,
    /// Four-term Blackman-Harris: sidelobes -92 dB, noise bandwidth 2.00
    /// bins. Sixty dB less leakage, at 1.25 dB of narrowband sensitivity.
    BlackmanHarris,
}

impl Window {
    fn coeffs(self, size: usize) -> Vec<f32> {
        match self {
            Window::Hann => (0..size)
                .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / size as f32).cos())
                .collect(),
            Window::BlackmanHarris => {
                const A: [f32; 4] = [0.358_75, 0.488_29, 0.141_28, 0.011_68];
                (0..size)
                    .map(|i| {
                        let x = 2.0 * PI * i as f32 / size as f32;
                        A[0] - A[1] * x.cos() + A[2] * (2.0 * x).cos() - A[3] * (3.0 * x).cos()
                    })
                    .collect()
            }
        }
    }
}

/// Welch-style averaged periodogram, fftshifted so bin 0 is the lowest frequency.
pub struct Spectrum {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Sum of the window coefficients. Constant per size, so it is cached
    /// instead of being re-summed on every call.
    win_sum: f32,
    size: usize,
    buf: Vec<Complex32>,
    pending: Vec<Complex32>,
    /// Segment accumulator, reused across calls: this method runs once per
    /// radio block, and a fresh `vec![0; size]` each time was ~125
    /// allocations a second at the default span.
    acc: Vec<f32>,
    /// FFT scratch, so `Fft::process` does not allocate one per segment.
    scratch: Vec<Complex32>,
    /// Streaming full-duty Welch state behind `ingest_welch` / `take_dbfs`.
    welch: Welch,
}

/// Largest stride the late-take guard will escalate to. Past this a missing
/// `take_dbfs` is a caller bug, and a bounded blind spot is better than an
/// overflowing one.
const MAX_STRIDE: usize = 1 << 16;

/// Accumulators for the streaming estimator. Kept apart from `pending`/`acc`
/// so the snapshot path (`ingest` + `power_dbfs`) and the full-duty path do
/// not disturb each other if a caller mixes them.
struct Welch {
    /// Samples of the segment being assembled; always `< size` between calls.
    pending: Vec<Complex32>,
    /// Samples still to discard before the next segment starts (stride > 1).
    skip: usize,
    /// Per-bin sum of |X|² over the segments since the last take (unshifted).
    sum: Vec<f32>,
    /// Per-bin maximum of |X|² over the same segments (unshifted).
    peak: Vec<f32>,
    /// Segments folded into `sum`/`peak` since the last take.
    nseg: usize,
    /// Samples ingested since the last take, and in the interval before it:
    /// the latter is the forecast the segment budget is planned against.
    seen: usize,
    last_seen: usize,
    /// Segment budget per take; 0 = unlimited.
    cap: usize,
    /// Current hop and stride: the next segment starts `hop * stride`
    /// samples after the previous one.
    hop: usize,
    stride: usize,
    /// Late-take guard: once `nseg` reaches `next_escalate` the segment rate
    /// halves, and the next threshold is `escalate_step` further on (the step
    /// itself halves each time, so a missed take costs at most ~2x cap).
    next_escalate: usize,
    escalate_step: usize,
}

impl Welch {
    fn new(size: usize) -> Self {
        let mut w = Self {
            pending: Vec::with_capacity(size),
            skip: 0,
            sum: vec![0.0; size],
            peak: vec![0.0; size],
            nseg: 0,
            seen: 0,
            last_seen: 0,
            cap: 0,
            hop: 0,
            stride: 1,
            next_escalate: 0,
            escalate_step: 0,
        };
        w.plan(size);
        w
    }

    /// Choose hop and stride for the coming interval from the length of the
    /// last one, so a capped budget is spread across the whole interval
    /// instead of being spent on its first few milliseconds.
    fn plan(&mut self, size: usize) {
        let half = (size / 2).max(1);
        self.hop = half;
        self.stride = 1;
        self.next_escalate = self.cap;
        self.escalate_step = (self.cap / 2).max(1);
        if self.cap > 0 && self.last_seen > 0 && self.last_seen / half > self.cap {
            // Overlap would overrun the budget: drop it first, since a 50%
            // overlapped segment adds little independent information.
            self.hop = size;
            let slots = self.last_seen / size;
            if slots > self.cap {
                // Still over: process every `stride`-th segment so the
                // budget samples the whole interval evenly.
                self.stride = slots.div_ceil(self.cap).min(MAX_STRIDE);
            }
        }
        // A skip left by a late-take escalation must not blind the new
        // interval for longer than the new plan itself would.
        let advance = self.hop * self.stride;
        self.skip = self.skip.min(advance.saturating_sub(size));
    }

    /// Late-take guard: halve the segment rate (first by dropping overlap,
    /// then by doubling the stride).
    fn escalate(&mut self, size: usize) {
        if self.hop < size {
            self.hop = size;
        } else {
            self.stride = (self.stride * 2).min(MAX_STRIDE);
        }
        self.next_escalate = self.next_escalate.saturating_add(self.escalate_step);
        self.escalate_step = (self.escalate_step / 2).max(1);
    }

    fn reset_acc(&mut self) {
        self.sum.fill(0.0);
        self.peak.fill(0.0);
        self.nseg = 0;
    }
}

/// fftshift `src` into `out` as dB of `src * scale`.
fn shift_db(src: &[f32], out: &mut Vec<f32>, scale: f32) {
    let n = src.len();
    out.clear();
    out.resize(n, 0.0);
    let half = n / 2;
    for (i, o) in out.iter_mut().enumerate() {
        *o = 10.0 * (src[(i + half) % n] * scale + 1e-20).log10();
    }
}

#[allow(dead_code)]
impl Spectrum {
    pub fn new(size: usize) -> Self {
        Self::with_window(size, Window::BlackmanHarris)
    }

    pub fn with_window(size: usize, win: Window) -> Self {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(size);
        let window = win.coeffs(size);
        let win_sum: f32 = window.iter().sum();
        let scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        Self {
            fft,
            win_sum,
            window,
            size,
            buf: vec![Complex32::new(0.0, 0.0); size],
            pending: Vec::with_capacity(size * 2),
            acc: vec![0.0f32; size],
            scratch,
            welch: Welch::new(size),
        }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// The analysis window coefficients.
    pub fn window(&self) -> &[f32] {
        &self.window
    }

    /// Average the periodogram over every whole segment available, returning dB.
    ///
    /// Samples are carried across calls so the FFT can be larger than one IQ
    /// block — that is what makes high-resolution views possible. If a full
    /// segment is not yet available `out` is left untouched.
    ///
    /// Segments hop by half an FFT so successive estimates overlap; that
    /// Welch average is what keeps the noise floor from boiling.
    pub fn power_db(&mut self, input: &[Complex32], out: &mut Vec<f32>) {
        let Some(nseg) = self.accumulate(input) else {
            if out.is_empty() {
                out.resize(self.size, -140.0);
            }
            return;
        };
        let scale = 1.0 / (nseg as f32 * self.size as f32);
        self.emit(out, scale);
    }

    /// Feed samples without computing a periodogram.
    ///
    /// Display only needs a snapshot at frame rate; ingesting every radio
    /// block keeps the pending window current without 30 FFTs per block.
    pub fn ingest(&mut self, input: &[Complex32]) {
        if input.is_empty() {
            return;
        }
        self.pending.extend_from_slice(input);
        let cap = self.size.saturating_mul(3).max(self.size);
        if self.pending.len() > cap {
            let extra = self.pending.len() - cap;
            self.pending.drain(..extra);
        }
    }

    /// Drop buffered and accumulated display samples — after a break in the
    /// sample timeline the pending Welch window would otherwise average
    /// across a hole that never existed on the air.
    ///
    /// Also resets the streaming (`ingest_welch`) accumulators, its partial
    /// segment, and the interval forecast the segment cap plans against — a
    /// break usually means a retune or rate change, so the old forecast is
    /// no better than none.
    pub fn clear(&mut self) {
        self.buf.fill(Complex32::new(0.0, 0.0));
        self.pending.clear();
        let w = &mut self.welch;
        w.pending.clear();
        w.skip = 0;
        w.reset_acc();
        w.seen = 0;
        w.last_seen = 0;
        w.plan(self.size);
    }

    /// Streaming, full-duty Welch estimate: window and FFT every complete
    /// segment as samples arrive, instead of a snapshot of the newest few.
    ///
    /// Segments hop by `size / 2` and the incomplete tail is carried to the
    /// next call, so block boundaries are invisible: feeding a signal in one
    /// call or in arbitrary pieces gives identical results. At most
    /// `size - 1` samples are held between calls — nothing is buffered per
    /// interval. Each segment adds its |X|² into a per-bin sum and a per-bin
    /// maximum, read and reset by [`take_dbfs`](Self::take_dbfs).
    ///
    /// The cost is one FFT per `size / 2` samples unless a budget is set with
    /// [`set_max_segments_per_take`](Self::set_max_segments_per_take).
    /// Allocation-free once constructed.
    pub fn ingest_welch(&mut self, mut input: &[Complex32]) {
        let n = self.size;
        self.welch.seen = self.welch.seen.saturating_add(input.len());
        while !input.is_empty() {
            let w = &mut self.welch;
            if w.skip > 0 {
                let k = w.skip.min(input.len());
                w.skip -= k;
                input = &input[k..];
                continue;
            }
            let k = (n - w.pending.len()).min(input.len());
            w.pending.extend_from_slice(&input[..k]);
            input = &input[k..];
            if w.pending.len() == n {
                self.welch_segment();
            }
        }
    }

    /// Emit the streaming estimate accumulated since the last take, then
    /// reset it. Returns the number of segments it covers.
    ///
    /// `avg_out` receives the averaged periodogram and `peak_out`, if given,
    /// the per-bin maximum over the individual segments — a burst too short
    /// to lift the average much still reaches full height there. Both are in
    /// calibrated dBFS with the same reference and fftshift as
    /// [`power_dbfs`](Self::power_dbfs) (0 dBFS = full-scale complex
    /// exponential). On noise alone the peak trace sits above the average by
    /// roughly the expected maximum of `n` exponentials: about 8 dB at 400
    /// segments.
    ///
    /// When no segment has completed since the last take, the outputs are
    /// left exactly as they were (so the caller keeps showing the previous
    /// frame) — except that an empty vector is filled with -140 dBFS, as
    /// `power_dbfs` does — and 0 is returned. The pending samples and the
    /// interval forecast are kept in that case, so a slow stream is not
    /// penalized for being polled often.
    pub fn take_dbfs(&mut self, avg_out: &mut Vec<f32>, peak_out: Option<&mut Vec<f32>>) -> usize {
        let nseg = self.welch.nseg;
        if nseg == 0 {
            if avg_out.is_empty() {
                avg_out.resize(self.size, -140.0);
            }
            if let Some(p) = peak_out {
                if p.is_empty() {
                    p.resize(self.size, -140.0);
                }
            }
            return 0;
        }
        let reference = 1.0 / (self.win_sum * self.win_sum);
        shift_db(&self.welch.sum, avg_out, reference / nseg as f32);
        if let Some(p) = peak_out {
            shift_db(&self.welch.peak, p, reference);
        }
        let w = &mut self.welch;
        w.reset_acc();
        w.last_seen = w.seen;
        w.seen = 0;
        w.plan(self.size);
        nseg
    }

    /// Bound the streaming estimator's cost: at most about `n` FFTs per
    /// [`take_dbfs`](Self::take_dbfs) interval (0 = unlimited, the default).
    ///
    /// The budget is planned from the length of the previous interval, so
    /// the processed segments are spread across the whole interval rather
    /// than spent on its start: if 50%-overlapped segments would exceed `n`,
    /// overlap is dropped (hop = size); if non-overlapped segments still
    /// would, only every `k`-th segment is processed, with `k` chosen so the
    /// count fits. A burst anywhere in the interval that lasts at least
    /// `(k + 1) * size` samples is therefore guaranteed to land in a whole
    /// processed segment, and shorter ones still have a proportional chance.
    ///
    /// If the cap is reached anyway — the first interval, which has no
    /// forecast, or a take that came late — the rate is halved on the spot
    /// (first by dropping overlap, then by doubling the stride), and halved
    /// again at each further `n/2`, `n/4`, ... segments. One interval
    /// therefore never costs more than about `2 * n` segments however late
    /// the take, while still sampling the remainder of the interval.
    pub fn set_max_segments_per_take(&mut self, n: usize) {
        self.welch.cap = n;
        self.welch.plan(self.size);
        // Respect a budget already partly spent in this interval.
        if n > 0 && self.welch.nseg >= n {
            let size = self.size;
            self.welch.escalate(size);
        }
    }

    /// Window, FFT and fold in the full segment held in `welch.pending`, then
    /// advance to the next segment start.
    fn welch_segment(&mut self) {
        let n = self.size;
        let Self {
            fft,
            window,
            buf,
            scratch,
            welch: w,
            ..
        } = self;
        for ((b, &x), &c) in buf.iter_mut().zip(w.pending.iter()).zip(window.iter()) {
            *b = x * c;
        }
        fft.process_with_scratch(buf, scratch);
        for ((s, p), b) in w.sum.iter_mut().zip(w.peak.iter_mut()).zip(buf.iter()) {
            let v = b.norm_sqr();
            *s += v;
            *p = p.max(v);
        }
        w.nseg += 1;
        if w.cap > 0 && w.nseg >= w.next_escalate {
            w.escalate(n);
        }
        let advance = w.hop * w.stride;
        if advance < n {
            w.pending.copy_within(advance.., 0);
            w.pending.truncate(n - advance);
        } else {
            w.pending.clear();
            w.skip = advance - n;
        }
    }

    /// Average periodogram returning calibrated dBFS (0 dBFS = full scale CW tone).
    pub fn power_dbfs(&mut self, input: &[Complex32], out: &mut Vec<f32>) {
        let Some(nseg) = self.accumulate(input) else {
            if out.is_empty() {
                out.resize(self.size, -140.0);
            }
            return;
        };
        // 0 dBFS is a full-scale complex exponential, whose power after the
        // window is win_sum²; the two methods differ only in that reference.
        let scale = 1.0 / (nseg as f32 * (self.win_sum * self.win_sum));
        self.emit(out, scale);
    }

    /// Window and FFT every whole segment available. Returns the segment count,
    /// or `None` when no full segment has arrived yet.
    ///
    /// Samples are carried across calls so the FFT can be larger than one IQ
    /// block; segments hop by half an FFT so successive estimates overlap —
    /// that Welch average keeps the noise floor from boiling.
    fn accumulate(&mut self, input: &[Complex32]) -> Option<usize> {
        self.pending.extend_from_slice(input);
        let hop = (self.size / 2).max(1);
        let nseg = if self.pending.len() >= self.size {
            1 + (self.pending.len() - self.size) / hop
        } else {
            0
        };
        if nseg == 0 {
            return None;
        }
        for v in self.acc.iter_mut() {
            *v = 0.0;
        }
        for s in 0..nseg {
            let start = s * hop;
            let seg = &self.pending[start..start + self.size];
            for i in 0..self.size {
                self.buf[i] = seg[i] * self.window[i];
            }
            self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
            for i in 0..self.size {
                self.acc[i] += self.buf[i].norm_sqr();
            }
        }
        let consumed = nseg * hop;
        if consumed > 0 {
            self.pending.drain(..consumed.min(self.pending.len()));
        }
        Some(nseg)
    }

    /// fftshift the accumulated periodogram into dB with `scale`.
    fn emit(&mut self, out: &mut Vec<f32>, scale: f32) {
        shift_db(&self.acc, out, scale);
    }
}

/// Per-bin minimum-statistics noise estimate. Falls promptly when a quiet
/// observation arrives but rises slowly enough that traffic cannot become its
/// own reference level.
///
/// Seeding mirrors the classifier's RF floor: the first second of frames
/// establishes a per-bin running MINIMUM and the floor reports below it, so a
/// burst at boot cannot pin the bins high and then take minutes to decay.
pub struct NoiseFloor {
    bins: Vec<f32>,
    /// Per-bin running minimum over the seeding window.
    seed_min: Vec<f32>,
    /// Frames collected toward the seed window; `SEED_FRAMES` ends seeding.
    seed_blocks: u32,
}

impl NoiseFloor {
    /// Frames collected before the floor trusts its slow-rise EMA — the same
    /// one-second window the classifier's RF floor uses.
    const SEED_FRAMES: u32 = 25;
    /// While seeding, report this far below the running minimum so real
    /// traffic is never treated as noise against a floor that is still
    /// collecting. Tightens to [`Self::FLOOR_MARGIN_DB`] once the window
    /// closes.
    const SEED_MARGIN_DB: f32 = 6.0;
    /// Margin held below the tracked minimum once seeded.
    const FLOOR_MARGIN_DB: f32 = 3.0;

    pub fn new() -> Self {
        Self {
            bins: Vec::new(),
            seed_min: Vec::new(),
            seed_blocks: 0,
        }
    }

    pub fn update<'a>(&'a mut self, power_db: &[f32]) -> &'a [f32] {
        if self.bins.len() != power_db.len() {
            // First frame (or a size change): seed from the running-minimum
            // path rather than copying the observation in. Copying would let
            // a burst in frame one pin the floor at the burst level, exactly
            // what the seeding window exists to prevent; reporting below the
            // first observation is provisional but conservative.
            self.bins = power_db.to_vec();
            self.seed_min = power_db.to_vec();
            self.seed_blocks = 0;
            let margin = Self::SEED_MARGIN_DB;
            for (floor, seed) in self.bins.iter_mut().zip(&mut self.seed_min) {
                *floor = *seed - margin;
            }
        } else if self.seed_blocks < Self::SEED_FRAMES {
            // Seeding: track the per-bin running minimum and report below it.
            self.seed_blocks += 1;
            let margin = if self.seed_blocks == Self::SEED_FRAMES {
                Self::FLOOR_MARGIN_DB
            } else {
                Self::SEED_MARGIN_DB
            };
            for ((floor, seed), &x) in self
                .bins
                .iter_mut()
                .zip(&mut self.seed_min)
                .zip(power_db)
            {
                if x < *seed {
                    *seed = x;
                }
                *floor = *seed - margin;
            }
        } else {
            for (floor, &x) in self.bins.iter_mut().zip(power_db) {
                let a = if x < *floor { 0.25 } else { 0.002 };
                *floor += a * (x - *floor);
            }
        }
        &self.bins
    }
}

/// Frequency-domain smooth. `taps` 1 = none, 3 = `1,2,1`, 5 = `1,4,6,4,1`.
/// Local maxima are kept so carriers stay sharp while the floor calms down.
pub fn smooth_bins(src: &[f32], taps: usize, out: &mut Vec<f32>) {
    out.clear();
    out.resize(src.len(), -140.0);
    if src.is_empty() {
        return;
    }
    if taps <= 1 || src.len() < 3 {
        out.copy_from_slice(src);
        return;
    }
    let w: &[f32] = if taps >= 5 {
        &[1.0, 4.0, 6.0, 4.0, 1.0]
    } else {
        &[1.0, 2.0, 1.0]
    };
    let r = w.len() / 2;
    for i in 0..src.len() {
        let mut acc = 0.0;
        let mut ww = 0.0;
        for (k, &wk) in w.iter().enumerate() {
            let j = i as isize + k as isize - r as isize;
            if j >= 0 && (j as usize) < src.len() {
                acc += src[j as usize] * wk;
                ww += wk;
            }
        }
        out[i] = acc / ww.max(1e-6);
    }
    for i in 1..src.len().saturating_sub(1) {
        if src[i] >= src[i - 1] && src[i] >= src[i + 1] {
            out[i] = src[i];
        }
    }
}

/// A unit phasor advanced by a fixed angle per sample.
///
/// A complex multiply where `sin_cos` would otherwise be called per sample.
/// The scouts mix the whole span buffer once per candidate frequency, so that
/// call was costing a quarter of a second per scout pass on a busy band —
/// most of the budget between passes, spent on trigonometry.
///
/// A repeated multiply loses magnitude to rounding, so it is renormalised
/// often enough that the drift never reaches the signal: a first-order
/// correction every 1024 samples, which costs one multiply per sample amortised.
pub(crate) struct Rotator {
    cur: Complex32,
    step: Complex32,
    n: u32,
}

impl Rotator {
    pub(crate) fn new(rad_per_sample: f32) -> Self {
        let (sin, cos) = rad_per_sample.sin_cos();
        Self {
            cur: Complex32::new(1.0, 0.0),
            step: Complex32::new(cos, sin),
            n: 0,
        }
    }

    #[inline]
    pub(crate) fn next(&mut self) -> Complex32 {
        let v = self.cur;
        self.cur *= self.step;
        self.n += 1;
        if self.n >= 1024 {
            self.n = 0;
            // Newton step towards |cur| = 1, cheaper than a square root.
            let k = 1.5 - 0.5 * self.cur.norm_sqr();
            self.cur *= k;
        }
        v
    }
}

/// Mix `iq` down by `hz` and decimate by `decim` for the scouts and the
/// signature classifier. The decimation window is triangular (two boxcars
/// back to back), giving a sinc² response — roughly double the stopband dB
/// of a plain block average for one extra accumulator — so a strong
/// neighbour elsewhere in the span aliases far less into the analysis audio.
pub fn mix_decim(iq: &[Complex32], fs: f32, hz: f32, decim: usize) -> Vec<Complex32> {
    let mut osc = Rotator::new(-2.0 * PI * hz / fs);
    let mut out = Vec::with_capacity(iq.len() / decim.max(1) + 1);
    if decim <= 1 {
        for &s in iq {
            out.push(s * osc.next());
        }
        return out;
    }
    let d = decim as f32;
    // Each sample feeds the falling half of the window ending this block and
    // the rising half of the one ending next block; the halves sum to D+1.
    let norm = 1.0 / (d + 1.0);
    let mut fall = Complex32::new(0.0, 0.0);
    let mut rise = Complex32::new(0.0, 0.0);
    let mut r = 0usize;
    for &s in iq {
        let m = s * osc.next();
        fall += m * (1.0 - r as f32 / d);
        rise += m * ((r + 1) as f32 / d);
        r += 1;
        if r == decim {
            out.push(fall * norm);
            fall = rise;
            rise = Complex32::new(0.0, 0.0);
            r = 0;
        }
    }
    out
}

/// Numerically controlled oscillator used to shift a signal of interest to 0 Hz.
///
/// The phase advances by repeated complex multiplication rather than by a
/// `sin_cos` per sample, for the same reason [`Rotator`] exists: a
/// transcendental call on every sample of a 2 MS/s stream is not a rounding
/// error in the budget, it *is* the budget. Measured on one pager channel it
/// was 39 % of the whole decode chain, and a span of forty channels pays it
/// forty times over.
///
/// Repeated multiplication loses magnitude to rounding, so the phasor is
/// renormalised on a first-order Newton step every 1024 samples — one
/// multiply per sample amortised. That schedule is counted in *samples*, not
/// blocks, which is what keeps the output identical however the caller
/// happens to chunk its input.
pub struct Nco {
    /// Four phasors, one per sample position mod 4, each stepping four
    /// samples at a time. One phasor stepping one sample makes every
    /// multiply wait on the last; four independent ones keep the multiplier
    /// busy, which halved the mixer's cost at 6 MS/s.
    lanes: [Complex32; 4],
    /// Which lane the next sample takes.
    pos: usize,
    inc: Complex32,
    inc4: Complex32,
    since_norm: u32,
}

impl Nco {
    pub fn new() -> Self {
        let one = Complex32::new(1.0, 0.0);
        Self { lanes: [one; 4], pos: 0, inc: one, inc4: one, since_norm: 0 }
    }

    /// Change the rotation rate, keeping the phase where it is — a retune
    /// should not also step the signal's phase.
    pub fn set_freq(&mut self, hz: f64, fs: f64) {
        let step = -2.0 * std::f64::consts::PI * hz / fs;
        let (sin, cos) = step.sin_cos();
        self.inc = Complex32::new(cos as f32, sin as f32);
        let (sin4, cos4) = (4.0 * step).sin_cos();
        self.inc4 = Complex32::new(cos4 as f32, sin4 as f32);
        self.respread(self.lanes[self.pos]);
    }

    /// Restart the phase at DC while keeping the rotation rate — for a break
    /// in the sample timeline, where continuing the old phase is meaningless.
    pub fn reset_phase(&mut self) {
        self.pos = 0;
        self.respread(Complex32::new(1.0, 0.0));
        self.since_norm = 0;
    }

    /// Lay the lanes out from the phase of the next sample.
    fn respread(&mut self, next: Complex32) {
        let mut p = next;
        for k in 0..4 {
            self.lanes[(self.pos + k) % 4] = p;
            p *= self.inc;
        }
    }

    fn renormalise(&mut self) {
        // Newton step toward |cur| = 1, cheaper than a square root.
        for l in &mut self.lanes {
            *l *= 1.5 - 0.5 * l.norm_sqr();
        }
    }

    fn one(&mut self, x: Complex32) -> Complex32 {
        let y = x * self.lanes[self.pos];
        self.lanes[self.pos] *= self.inc4;
        self.pos = (self.pos + 1) % 4;
        self.since_norm += 1;
        if self.since_norm >= 1024 {
            self.since_norm = 0;
            self.renormalise();
        }
        y
    }

    pub fn mix(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        // Every element is written below, so only a length change needs
        // filling: clearing first zeroed the whole block just to overwrite it.
        out.resize(input.len(), Complex32::new(0.0, 0.0));
        let head = ((4 - self.pos) % 4).min(input.len());
        for i in 0..head {
            out[i] = self.one(input[i]);
        }
        let body = (input.len() - head) / 4 * 4;
        // Lane-aligned from here, and the renormalising count moves with the
        // lanes (both start at zero), so it lands on a chunk boundary: the
        // output is the same however the caller chunks its input.
        for (x, y) in input[head..head + body].chunks_exact(4).zip(out[head..head + body].chunks_exact_mut(4)) {
            for l in 0..4 {
                y[l] = x[l] * self.lanes[l];
                self.lanes[l] *= self.inc4;
            }
            self.since_norm += 4;
            if self.since_norm >= 1024 {
                self.since_norm = 0;
                self.renormalise();
            }
        }
        for i in head + body..input.len() {
            out[i] = self.one(input[i]);
        }
    }
}

/// Windowed-sinc FIR lowpass that only evaluates the samples it keeps.
pub struct DecimFir {
    taps: Vec<f32>,
    /// The taps for the direct path, each written twice (once for I, once
    /// for Q) and zero-padded to a whole number of 8-lane chunks, so the
    /// inner product runs over the interleaved samples as plain f32 lanes.
    taps2: Vec<f32>,
    buf: Vec<Complex32>,
    decim: usize,
    fft: Option<Arc<dyn Fft<f32>>>,
    ifft: Option<Arc<dyn Fft<f32>>>,
    response: Vec<Complex32>,
    work: Vec<Complex32>,
    /// Transform scratch, so a frame does not allocate one per transform.
    scratch: Vec<Complex32>,
    overlap: Vec<Complex32>,
    fft_len: usize,
    /// Running phase of the kept-sample grid: increments once per filtered
    /// sample, kept outputs land where it wraps. (Was `fft_phase`.)
    phase: usize,
    /// Absolute filtered-sample count since startup, against `warmup`.
    g: u64,
    /// Samples of startup transient the FIR swallows before emitting:
    /// taps - 1, re-armed on every tap redesign. (Was the `fft_warm` counter.)
    warmup: u64,
}

#[allow(dead_code)]
impl DecimFir {
    pub fn new(cutoff_hz: f32, fs: f32, decim: usize, ntaps: usize) -> Self {
        let ntaps = ntaps | 1; // force odd so there is a center tap
        let mut this = Self {
            taps: lowpass_taps(cutoff_hz, fs, ntaps),
            taps2: Vec::new(),
            buf: Vec::with_capacity(ntaps * 4),
            decim,
            fft: None,
            ifft: None,
            response: Vec::new(),
            work: Vec::new(),
            scratch: Vec::new(),
            overlap: Vec::new(),
            fft_len: 0,
            phase: 0,
            g: 0,
            warmup: 0,
        };
        this.rebuild_fft();
        this
    }

    pub fn set_cutoff(&mut self, cutoff_hz: f32, fs: f32) {
        let n = self.taps.len();
        self.taps = lowpass_taps(cutoff_hz, fs, n);
        self.rebuild_fft();
    }

    /// Redesign at a new length as well as a new cutoff. The sample history
    /// is kept, so changing the filter does not click.
    pub fn set_taps(&mut self, cutoff_hz: f32, fs: f32, ntaps: usize) {
        self.taps = lowpass_taps(cutoff_hz, fs, ntaps | 1);
        self.rebuild_fft();
    }

    pub fn decim(&self) -> usize {
        self.decim
    }

    /// Clear the streaming state — buffered input, overlap tail, and the
    /// kept-sample grid — and re-arm the startup warm-up, without redesigning
    /// the taps or rebuilding the FFT plan. This is the discontinuity reset:
    /// after a break in the sample timeline the buffered history belongs to
    /// samples that will never arrive, so it must not be stitched onto the
    /// new run.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.overlap.fill(Complex32::new(0.0, 0.0));
        self.phase = 0;
        self.g = 0;
        self.warmup = (self.taps.len().saturating_sub(1)) as u64;
    }

    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        if self.fft.is_some() {
            self.process_fft(input, out);
            return;
        }
        out.clear();
        self.buf.extend_from_slice(input);
        // The window covers the padded tap count; the pad taps are zero, so
        // the extra samples it reads contribute nothing.
        let span = self.taps2.len() / 2;
        // SAFETY: `Complex<f32>` is `#[repr(C)]` { re, im }, so a slice of
        // them is the same memory as twice as many f32s.
        let flat: &[f32] = unsafe {
            std::slice::from_raw_parts(self.buf.as_ptr().cast::<f32>(), self.buf.len() * 2)
        };
        let mut i = 0usize;
        while i + span <= self.buf.len() {
            let win = &flat[2 * i..2 * (i + span)];
            let mut acc = [0.0f32; 8];
            for (x, h) in win.chunks_exact(8).zip(self.taps2.chunks_exact(8)) {
                for l in 0..8 {
                    acc[l] += x[l] * h[l];
                }
            }
            out.push(Complex32::new(
                acc[0] + acc[2] + acc[4] + acc[6],
                acc[1] + acc[3] + acc[5] + acc[7],
            ));
            i += self.decim;
        }
        if i > 0 {
            self.buf.drain(..i);
        }
    }

    fn rebuild_fft(&mut self) {
        // Direct or overlap-save, by which costs less per input sample. The
        // direct path computes only the outputs it keeps, `taps / decim`
        // complex multiply-adds per input sample; overlap-save pays two
        // transforms per hop whatever is kept. Measured at 6 MS/s, a
        // 2047-tap stage decimating by 125 costs a quarter as much direct;
        // decimating by 23 (WFM) the transform still wins.
        if self.taps.len() < 64 || self.taps.len() <= DIRECT_TAPS_PER_OUTPUT * self.decim {
            self.fft = None;
            self.ifft = None;
            let n = self.taps.len();
            let padded = n.div_ceil(4) * 4;
            self.taps2 = vec![0.0; padded * 2];
            for (k, &t) in self.taps.iter().enumerate() {
                self.taps2[2 * k] = t;
                self.taps2[2 * k + 1] = t;
            }
            return;
        }
        // A hop of about three filter lengths: two transforms per hop is
        // what overlap-save costs, so a longer hop spreads them over more
        // samples. At 2047 taps an 8192-point transform costs about 30% less
        // per sample than the 4096 one, for 1 ms more latency at 6 MS/s.
        self.fft_len = (self.taps.len() * 4).next_power_of_two();
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(self.fft_len);
        let ifft = planner.plan_fft_inverse(self.fft_len);
        self.response = vec![Complex32::new(0.0, 0.0); self.fft_len];
        for (dst, &tap) in self.response.iter_mut().zip(&self.taps) {
            dst.re = tap;
        }
        fft.process(&mut self.response);
        self.work.resize(self.fft_len, Complex32::new(0.0, 0.0));
        self.scratch.resize(
            fft.get_inplace_scratch_len().max(ifft.get_inplace_scratch_len()),
            Complex32::new(0.0, 0.0),
        );
        self.overlap
            .resize(self.taps.len() - 1, Complex32::new(0.0, 0.0));
        // The overlap is state of the OLD tap count: after a redesign its
        // length meaning changed, so zero it rather than feed a stale tail
        // into the first new frame. The buffered INPUT in `buf`, though, is
        // raw samples that are valid under any tap set — keep it. Clearing it
        // here used to drop up to one hop of real audio mid-stream, an audible
        // gap on every `set_cutoff`/`set_taps`; the redesign's transient is
        // already handled by re-arming `warmup`, and `process_fft` happily
        // drains a `buf` longer than the new hop.
        self.overlap.fill(Complex32::new(0.0, 0.0));
        self.fft = Some(fft);
        self.ifft = Some(ifft);
        self.phase = 0;
        self.g = 0;
        self.warmup = (self.taps.len() - 1) as u64;
    }

    fn process_fft(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        out.clear();
        self.buf.extend_from_slice(input);
        let discard = self.taps.len() - 1;
        let hop = self.fft_len - discard;
        // Walk the buffer with a cursor and drain once at the end. Draining
        // each hop moved the whole remaining buffer every time: a 20 ms block
        // at 8 MS/s is 26 hops, so the copies grew with the square of the
        // block and were most of the chain's memmove.
        let mut at = 0usize;
        while self.buf.len() - at >= hop {
            self.emit_frame(at, hop, out);
            if discard <= hop {
                self.overlap.copy_from_slice(&self.buf[at + hop - discard..at + hop]);
            } else {
                self.overlap.rotate_left(hop);
                self.overlap[discard - hop..].copy_from_slice(&self.buf[at..at + hop]);
            }
            at += hop;
        }
        self.buf.drain(..at);
        // A short input burst can leave less than one FFT hop buffered, which
        // would return an empty output for this call and stall every consumer
        // that expects per-block continuity (APRS bit clocks, FT8 slots).
        // Complete one partial frame with zero padding instead: the missing
        // future samples contribute nothing to the emitted tail of this
        // linear convolution, so what goes out here is exact — the rest of
        // those outputs arrives on later calls with the real samples.
        if !self.buf.is_empty() {
            let consumed = self.buf.len();
            self.emit_frame(0, consumed, out);
            // advance overlap over everything consumed
            if consumed >= discard {
                self.overlap
                    .copy_from_slice(&self.buf[consumed - discard..]);
            } else {
                self.overlap.rotate_left(consumed);
                let start = discard - consumed;
                self.overlap[start..].copy_from_slice(&self.buf);
            }
            self.buf.clear();
        }
    }

    /// Filter one overlap-save frame and push its decimated outputs.
    ///
    /// Only every `decim`-th filtered sample is ever read, so the time-domain
    /// walk visits only those positions: computing all `hop` outputs of the
    /// inverse FFT and throwing most away cost the radio-stage filter
    /// (decim 24–66) most of its multiplies for nothing. The FFT work per
    /// frame is unchanged — the saving is in what is read back out.
    ///
    /// Kept-sample identity matches the original walk exactly: a sample is
    /// emitted when its absolute index `g` (counting every filtered sample
    /// since startup) satisfies `g % decim == 0` and `g >= taps - 1` (the
    /// warm-up that swallows the FIR's transient).
    fn emit_frame(&mut self, at: usize, valid: usize, out: &mut Vec<Complex32>) {
        let discard = self.taps.len() - 1;
        // Only the zero padding past the valid samples needs clearing; the
        // rest is overwritten just below.
        self.work[..discard].copy_from_slice(&self.overlap);
        self.work[discard..discard + valid].copy_from_slice(&self.buf[at..at + valid]);
        self.work[discard + valid..].fill(Complex32::new(0.0, 0.0));
        self.fft.as_ref().unwrap().process_with_scratch(&mut self.work, &mut self.scratch);
        for (x, h) in self.work.iter_mut().zip(&self.response) {
            *x *= *h;
        }
        self.ifft.as_ref().unwrap().process_with_scratch(&mut self.work, &mut self.scratch);
        // Kept-sample identity: emit when `(g - warmup) % decim == 0` past the
        // startup transient. The original walk's `continue` skipped warm
        // samples without advancing its phase counter, so its phase==0 grid
        // is anchored at g == taps - 1 — and in chunked operation each padded
        // frame's walked index maps to output time g - warmup, putting every
        // push on the t % decim == 0 grid this condition reproduces directly.
        // Modular-safe: pos ≡ (warmup - g) mod decim, so every push satisfies
        // (g + pos - warmup) % decim == 0 without any underflowing subtraction.
        let d = self.decim as u64;
        let first = ((self.warmup % d + d - self.g % d) % d) as usize;
        let mut pos = first;
        while pos < valid {
            if self.g + pos as u64 >= self.warmup {
                out.push(self.work[discard + pos] / self.fft_len as f32);
            }
            pos += self.decim;
        }
        self.g += valid as u64;
    }
}

/// A Blackman-windowed sinc moves from passband to stopband in about
/// `5.5 / ntaps` of the sample rate. Every filter length below is derived
/// from that relationship rather than guessed, because a cutoff far narrower
/// than the transition width is not a filter you designed — it is whatever
/// the window happened to give you.
const TRANSITION_K: f32 = 5.5;

fn transition_hz(fs: f32, ntaps: usize) -> f32 {
    TRANSITION_K * fs / ntaps.max(1) as f32
}

/// Shortest odd tap count whose transition is no wider than `want_hz`.
fn taps_for_transition(fs: f32, want_hz: f32, lo: usize, hi: usize) -> usize {
    let n = (TRANSITION_K * fs / want_hz.max(1e-3)).ceil() as usize;
    n.clamp(lo, hi) | 1
}

fn lowpass_taps(cutoff_hz: f32, fs: f32, ntaps: usize) -> Vec<f32> {
    // Normalised-cutoff clamp. The 0.0005 floor exists so a near-DC cutoff
    // cannot produce a degenerate all-zero (or negative-gain) tap set, but it
    // is a FRACTION of fs, not a fixed frequency: at the 192 kHz chain rate it
    // lifts any requested cutoff below 96 Hz up to 96 Hz (an 80 Hz request
    // becomes 96 Hz), while at 8 kHz the same floor is 4 Hz. Callers wanting a
    // specific sub-96 Hz corner at high rates must check against
    // 0.0005 * fs themselves.
    let fc = (cutoff_hz / fs).clamp(0.0005, 0.49);
    let mid = (ntaps / 2) as f32;
    let mut taps = Vec::with_capacity(ntaps);
    for i in 0..ntaps {
        let x = i as f32 - mid;
        let sinc = if x.abs() < 1e-6 {
            2.0 * fc
        } else {
            (2.0 * PI * fc * x).sin() / (PI * x)
        };
        let xw = 2.0 * PI * i as f32 / (ntaps - 1) as f32;
        let w = 0.358_75 - 0.488_29 * xw.cos() + 0.141_28 * (2.0 * xw).cos()
            - 0.011_68 * (3.0 * xw).cos();
        taps.push(sinc * w);
    }
    let sum: f32 = taps.iter().sum();
    if sum.abs() > 1e-12 {
        taps.iter_mut().for_each(|t| *t /= sum);
    }
    taps
}

/// Decimate-by-two halfband FIR.
///
/// A windowed sinc cut at a quarter of the input rate has every even tap at
/// zero except the center one, which is exactly one half. So of its `4m - 1`
/// taps only `m + 1` are distinct: the center, and the `m` odd taps on one
/// side, which the other side mirrors.
///
/// Run polyphase, that structure falls out as two plain streams: with the
/// input split into its even and odd samples, every kept output is a
/// symmetric `2m`-tap FIR over the even stream plus half of one delayed odd
/// sample. Both are contiguous, so the filter is `2m` vectorizable passes of
/// a scalar times a run of floats — rather than `4m - 1` strided complex
/// multiply-adds per output — which is what makes a cascade of these
/// affordable at the radio rate, where a single long filter decimating by
/// 32 is not.
///
/// The transition band is symmetric about a quarter of the input rate: a
/// band of half-width `b` is passed flat and kept clear of aliases when the
/// filter has moved from passband to stopband between `b` and `fs/2 - b`,
/// so the taps a stage needs depend only on how much of the output band it
/// has to protect — see [`halfband_taps`].
pub struct Halfband {
    /// The odd taps on one side of the center, nearest first.
    wing: Vec<f32>,
    /// The `2m` taps applied to the even stream: the wing mirrored, farthest
    /// first, then nearest-first.
    sym: Vec<f32>,
    /// Filter length, `4m - 1`.
    len: usize,
    /// Even-phase samples as interleaved re, im: `2m - 1` of history, then
    /// this call's.
    even: Vec<f32>,
    /// Odd-phase samples the same way: `m` of history, then this call's.
    odd: Vec<f32>,
    /// An input sample waiting for its pair when a block ends on an odd
    /// count, so the phase split is the same however the input is chunked.
    pending: Option<Complex32>,
    acc: Vec<f32>,
}

/// Shortest halfband length (of the form `4m - 1`) whose transition fits
/// between `band_hz` and `fs_in / 2 - band_hz`: the lowest frequency that
/// would fold onto the band when the rate halves.
///
/// Sized on the window's whole main lobe, `8 / N` of the rate, rather than
/// the `5.5 / N` the decode chains use: that figure reaches the stopband's
/// -50 dB point, and a display must not show a ghost of a strong neighbor
/// at the edge of the window, so here the edge lands on the window's floor.
pub fn halfband_taps(fs_in: f32, band_hz: f32) -> usize {
    const MAIN_LOBE_K: f32 = 8.0;
    // Fraction of the input rate between the edge of the protected band and
    // the start of what aliases onto it.
    let tr = (0.5 - 2.0 * band_hz / fs_in).clamp(0.02, 0.5);
    let n = (MAIN_LOBE_K / tr).ceil() as usize;
    // Round up to 4m - 1, at least 7.
    let m = (n + 1).div_ceil(4).max(2);
    4 * m - 1
}

/// Outputs computed per pass over the taps, so the accumulator stays in
/// the first-level cache.
const HALFBAND_TILE: usize = 2048;

impl Halfband {
    /// A halfband filter of `ntaps` taps, rounded up to the next `4m - 1`.
    pub fn new(ntaps: usize) -> Self {
        // 4m - 1 is left as it is; anything else rounds up to the next.
        let m = (ntaps + 1).div_ceil(4).max(2);
        let len = 4 * m - 1;
        let c = (len - 1) / 2;
        let mut wing = Vec::with_capacity(m);
        for j in 0..m {
            let k = (2 * j + 1) as f32;
            // sinc at a quarter of the rate, where the even taps vanish.
            let sinc = (PI * k / 2.0).sin() / (PI * k);
            let i = (c + 2 * j + 1) as f32;
            let xw = 2.0 * PI * i / (len - 1) as f32;
            let w = 0.358_75 - 0.488_29 * xw.cos() + 0.141_28 * (2.0 * xw).cos()
                - 0.011_68 * (3.0 * xw).cos();
            wing.push(sinc * w);
        }
        // Unity gain at DC with the center held at exactly one half, so the
        // filter stays a true halfband after normalisation.
        let s: f32 = wing.iter().sum();
        if s.abs() > 1e-12 {
            let k = 0.25 / s;
            wing.iter_mut().for_each(|t| *t *= k);
        }
        let mut sym: Vec<f32> = wing.iter().rev().copied().collect();
        sym.extend(wing.iter());
        let mut this = Self {
            wing,
            sym,
            len,
            even: Vec::new(),
            odd: Vec::new(),
            pending: None,
            acc: Vec::with_capacity(HALFBAND_TILE * 2),
        };
        this.reset();
        this
    }

    /// Filter length, `4m - 1`.
    pub fn taps(&self) -> usize {
        self.len
    }

    /// Drop the sample history after a break in the timeline.
    pub fn reset(&mut self) {
        let m = self.wing.len();
        self.even.clear();
        self.even.resize((2 * m - 1) * 2, 0.0);
        self.odd.clear();
        self.odd.resize(m * 2, 0.0);
        self.pending = None;
    }

    /// Filter and keep every second sample. Output continuity across calls
    /// is exact: the history and the phase split carry over, so the output
    /// is the same however the input is chunked.
    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        out.clear();
        // Split into phases, pairing a sample the last call left over first.
        let mut rest = input;
        if let Some(p) = self.pending.take() {
            match rest.split_first() {
                Some((&x, tail)) => {
                    self.even.extend_from_slice(&[p.re, p.im]);
                    self.odd.extend_from_slice(&[x.re, x.im]);
                    rest = tail;
                }
                None => {
                    self.pending = Some(p);
                    return;
                }
            }
        }
        let pairs = rest.len() / 2;
        let (e0, o0) = (self.even.len(), self.odd.len());
        self.even.resize(e0 + pairs * 2, 0.0);
        self.odd.resize(o0 + pairs * 2, 0.0);
        for ((pr, e), o) in rest[..pairs * 2]
            .chunks_exact(2)
            .zip(self.even[e0..].chunks_exact_mut(2))
            .zip(self.odd[o0..].chunks_exact_mut(2))
        {
            e[0] = pr[0].re;
            e[1] = pr[0].im;
            o[0] = pr[1].re;
            o[1] = pr[1].im;
        }
        if rest.len() % 2 == 1 {
            self.pending = Some(rest[rest.len() - 1]);
        }
        let m = self.wing.len();
        // One output per new pair: the odd stream holds `m` of history.
        let n = self.odd.len() / 2 - m;
        out.reserve(n);
        let mut n0 = 0usize;
        while n0 < n {
            let cnt = (n - n0).min(HALFBAND_TILE);
            // y[n] = x_odd[n - m] / 2 + Σ_t sym[t] · x_even[n - 2m + 1 + t],
            // in buffer coordinates where the histories sit before index 0.
            self.acc.clear();
            self.acc.extend_from_slice(&self.odd[2 * n0..2 * (n0 + cnt)]);
            for a in &mut self.acc {
                *a *= 0.5;
            }
            // The taps are symmetric, so each pass folds the two samples
            // that share a tap before the one multiply.
            let m2 = self.sym.len();
            for t in 0..m {
                let g = self.sym[t];
                let near = &self.even[2 * (n0 + t)..2 * (n0 + t + cnt)];
                let far = &self.even[2 * (n0 + m2 - 1 - t)..2 * (n0 + m2 - 1 - t + cnt)];
                for ((a, &x), &y) in self.acc.iter_mut().zip(near).zip(far) {
                    *a += g * (x + y);
                }
            }
            out.extend(self.acc.chunks_exact(2).map(|c| Complex32::new(c[0], c[1])));
            n0 += cnt;
        }
        if n > 0 {
            self.even.drain(..2 * n);
            self.odd.drain(..2 * n);
        }
    }
}

/// A cascade of [`Halfband`] stages: decimation by a power of two, each
/// stage sized for the band the final output has to keep clean, so the
/// stages at the high rates are short and only the last one is long.
pub struct HalfbandChain {
    stages: Vec<Halfband>,
    fs_in: f64,
    fs_out: f64,
    a: Vec<Complex32>,
    b: Vec<Complex32>,
}

impl HalfbandChain {
    /// `stages` decimate-by-two stages from `fs_in`, every one of them
    /// passing `±band_hz` flat and alias-free.
    pub fn new(fs_in: f64, band_hz: f64, stages: u32) -> Self {
        let mut fs = fs_in;
        let mut v = Vec::with_capacity(stages as usize);
        for _ in 0..stages {
            v.push(Halfband::new(halfband_taps(fs as f32, band_hz as f32)));
            fs /= 2.0;
        }
        Self { stages: v, fs_in, fs_out: fs, a: Vec::new(), b: Vec::new() }
    }

    pub fn fs_in(&self) -> f64 {
        self.fs_in
    }

    pub fn fs_out(&self) -> f64 {
        self.fs_out
    }

    pub fn decimation(&self) -> usize {
        1 << self.stages.len()
    }

    /// Tap count of each stage, first stage first.
    pub fn taps(&self) -> Vec<usize> {
        self.stages.iter().map(Halfband::taps).collect()
    }

    pub fn reset(&mut self) {
        for s in &mut self.stages {
            s.reset();
        }
        self.a.clear();
        self.b.clear();
    }

    /// With no stages the input is copied through unchanged.
    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        match self.stages.len() {
            0 => {
                out.clear();
                out.extend_from_slice(input);
            }
            1 => self.stages[0].process(input, out),
            n => {
                let mut cur = std::mem::take(&mut self.a);
                let mut nxt = std::mem::take(&mut self.b);
                self.stages[0].process(input, &mut cur);
                for st in &mut self.stages[1..n - 1] {
                    st.process(&cur, &mut nxt);
                    std::mem::swap(&mut cur, &mut nxt);
                }
                self.stages[n - 1].process(&cur, out);
                self.a = cur;
                self.b = nxt;
            }
        }
    }
}

/// Radio-rate taps, chosen for response rather than speed. Overlap-save's
/// cost barely depends on the length (the transform dominates), and where the
/// decimation is deep enough for the direct path, 2047 taps is still only
/// about 16 multiply-adds per input sample at 48 kHz out of 6 MS/s.
const RADIO_TAPS: usize = 2047;
/// Up to this many taps per kept output, the direct path is cheaper than
/// overlap-save (measured with `examples/bench_chain.rs`).
const DIRECT_TAPS_PER_OUTPUT: usize = 32;
const AUDIO_TAPS_MIN: usize = 129;
const AUDIO_TAPS_MAX: usize = 4095;

/// NCO + decimating lowpass: takes wideband IQ at the radio rate and produces
/// narrowband complex baseband centred on the cursor at the mode's audio rate.
///
/// The two FIRs have distinct jobs. The radio-rate stage is *only* an
/// anti-alias filter: it passes the whole channel flat and is stopped before
/// `fs_out - bw/2`, the lowest frequency that folds into the channel when
/// decimating. It deliberately does not try to realise the channel width
/// itself — an 80 Hz cutoff at 192 kHz is 35× narrower than 511 taps can
/// resolve there, so asking for it yields the window's own shape, not an
/// 80 Hz filter. The audio-rate stage is the channel filter, and its length
/// is sized from the transition the requested bandwidth actually needs.
pub struct DecodeChain {
    nco: Nco,
    fir: DecimFir,
    audio: DecimFir,
    fs_in: f64,
    fs_out: f64,
    mixed: Vec<Complex32>,
    decimated: Vec<Complex32>,
}

impl DecodeChain {
    /// `target_rate` is the audio rate the mode wants. The achieved rate is
    /// `fs_in / round(fs_in / target_rate)`, so it only lands exactly on the
    /// target when the radio rate is an integer multiple of it — which FT8/FT4
    /// require, and the caller enforces.
    pub fn new(fs_in: f64, bandwidth: f32, target_rate: f64) -> Self {
        let decim = (fs_in / target_rate).round().max(1.0) as usize;
        let fs_out = fs_in / decim as f64;
        let mut chain = Self {
            nco: Nco::new(),
            fir: DecimFir::new(fs_out as f32 * 0.5, fs_in as f32, decim, RADIO_TAPS),
            audio: DecimFir::new(bandwidth / 2.0, fs_out as f32, 1, AUDIO_TAPS_MIN),
            fs_in,
            fs_out,
            mixed: Vec::new(),
            decimated: Vec::new(),
        };
        // One place decides both cutoffs and the audio length.
        chain.set_bandwidth(bandwidth);
        chain
    }

    pub fn fs_out(&self) -> f64 {
        self.fs_out
    }

    pub fn fs_in(&self) -> f64 {
        self.fs_in
    }

    pub fn set_offset(&mut self, hz: f64) {
        self.nco.set_freq(hz, self.fs_in);
    }

    /// Drop all streaming state — NCO phase and both FIR buffers — without
    /// touching the design (offset, bandwidth, taps, FFT plans survive).
    ///
    /// Used when the sample timeline breaks (a lost-delivery gap or an
    /// acquisition epoch boundary): the buffered tail belongs to samples the
    /// consumer will never see again, and emitting it stitched to the new
    /// run would hand framing decoders a waveform that never existed on the
    /// air. After a reset the chain re-arms its warm-up and the first
    /// outputs lag by the usual FIR transient, exactly like a fresh chain.
    pub fn reset(&mut self) {
        // Keep the rotation rate (the design), restart the phase: the sample
        // run being mixed into is over, so the new run starts at DC phase.
        self.nco.reset_phase();
        self.fir.reset();
        self.audio.reset();
        self.mixed.clear();
        self.decimated.clear();
    }

    pub fn set_bandwidth(&mut self, bw: f32) {
        let fs_out = self.fs_out as f32;
        let half = (bw * 0.5).clamp(10.0, fs_out * 0.45);

        // Anti-alias stage. Flat across the channel, stopped by the time
        // the spectrum folds back onto it.
        //
        let tr = transition_hz(self.fs_in as f32, RADIO_TAPS);
        let lo = half + tr * 0.5;
        let hi = (fs_out - half - tr * 0.5).max(lo);
        let radio_cut = (fs_out * 0.5).clamp(lo, hi);
        self.fir.set_cutoff(radio_cut, self.fs_in as f32);

        // Channel stage. FT8/FT4 set the tightest requirement: the chain sits
        // 1600 Hz above the dial and the decoder searches 200–3000 Hz, so the
        // lower skirt has to be flat at 200 Hz and already stopped at 0 Hz —
        // 129 taps could not do that and cost the edges of the waterfall
        // several dB. Ask for a transition that fits inside the setting.
        let want_tr = (0.03 * bw).max(0.01 * fs_out);
        let ntaps = taps_for_transition(fs_out, want_tr, AUDIO_TAPS_MIN, AUDIO_TAPS_MAX);
        self.audio.set_taps(half, fs_out, ntaps);
    }

    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        self.nco.mix(input, &mut self.mixed);
        let mixed = std::mem::take(&mut self.mixed);
        self.fir.process(&mixed, &mut self.decimated);
        self.mixed = mixed;
        self.audio.process(&self.decimated, out);
    }
}

/// One-pole smoother, used for envelopes and AGC-ish level tracking.
#[derive(Clone, Copy)]
pub struct OnePole {
    y: f32,
    a: f32,
}

#[allow(dead_code)]
impl OnePole {
    pub fn new(tau_samples: f32) -> Self {
        Self {
            y: 0.0,
            a: (-1.0 / tau_samples.max(1.0)).exp(),
        }
    }
    pub fn process(&mut self, x: f32) -> f32 {
        self.y = self.a * self.y + (1.0 - self.a) * x;
        self.y
    }
    pub fn value(&self) -> f32 {
        self.y
    }
    /// Forget the accumulated level — after a break in the sample timeline,
    /// the old average describes samples that will never arrive again.
    pub fn reset(&mut self) {
        self.y = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A halfband keeps its band flat, stops what would fold onto it, and
    /// gives the same output however its input is chunked.
    #[test]
    fn halfband_passes_the_band_and_stops_its_alias() {
        let fs = 1.0e6f64;
        let band = 125_000.0f64;
        let taps = halfband_taps(fs as f32, band as f32);
        assert_eq!(taps % 4, 3, "4m - 1 taps: {taps}");
        let tone = |hz: f64, n: usize| -> Vec<Complex32> {
            (0..n)
                .map(|i| {
                    let ph = 2.0 * std::f64::consts::PI * hz * i as f64 / fs;
                    Complex32::new(ph.cos() as f32, ph.sin() as f32)
                })
                .collect()
        };
        let rms = |v: &[Complex32]| (v.iter().map(|c| c.norm_sqr()).sum::<f32>() / v.len() as f32).sqrt();
        let run = |hz: f64| {
            let mut hb = Halfband::new(taps);
            let mut out = Vec::new();
            hb.process(&tone(hz, 20_000), &mut out);
            rms(&out[taps..])
        };
        for hz in [0.0, 60_000.0, band] {
            let g = 20.0 * run(hz).log10();
            assert!(g.abs() < 0.1, "{hz} Hz passes at {g:.3} dB");
        }
        for hz in [fs / 2.0 - band, fs / 2.0 - 60_000.0, fs / 2.0] {
            let g = 20.0 * run(hz).log10();
            assert!(g < -75.0, "{hz} Hz folds onto the band at {g:.1} dB");
        }
        // Chunk invariance: one call or many, the same samples come out.
        let x = tone(30_000.0, 9_001);
        let mut whole = Halfband::new(taps);
        let mut a = Vec::new();
        whole.process(&x, &mut a);
        let mut pieces = Halfband::new(taps);
        let mut b = Vec::new();
        let mut part = Vec::new();
        for chunk in x.chunks(7) {
            pieces.process(chunk, &mut part);
            b.extend_from_slice(&part);
        }
        assert_eq!(a.len(), b.len());
        assert!(a.iter().zip(&b).all(|(p, q)| (p - q).norm() < 1e-6));

        // The cascade sizes its stages for the band at each rate: short at
        // the top, longest at the bottom.
        let chain = HalfbandChain::new(8.0e6, band, 4);
        assert_eq!(chain.fs_out(), 500_000.0);
        assert_eq!(chain.decimation(), 16);
        let t = chain.taps();
        assert!(t.windows(2).all(|w| w[0] <= w[1]), "{t:?}");
        assert!(t[0] <= 19 && t[3] >= 27, "{t:?}");
    }

    #[test]
    fn smooth_bins_keeps_a_peak_and_calms_the_floor() {
        let mut src = vec![-80.0f32; 32];
        src[16] = -20.0;
        src[10] = -70.0;
        src[11] = -68.0;
        src[12] = -71.0;
        let mut out = Vec::new();
        smooth_bins(&src, 5, &mut out);
        assert_eq!(out.len(), src.len());
        assert!((out[16] + 20.0).abs() < 0.01, "peak should be preserved");
        // Neighbours of the floor wiggle are pulled together.
        assert!(out[11] < src[11] + 1.0);
    }

    #[test]
    fn noise_floor_seeds_on_a_running_minimum() {
        // A burst at boot used to pin the floor high: the EMA started from
        // the first frame and took minutes to decay. Seeding must instead
        // report below the minimum seen during the first second.
        let burst: Vec<f32> = vec![-40.0; 64];
        let quiet: Vec<f32> = vec![-90.0; 64];
        let mut nf = NoiseFloor::new();
        // 25 frames of a hot band (a carrier present at startup).
        let mut floor_during_seed = f32::MIN;
        for _ in 0..NoiseFloor::SEED_FRAMES - 1 {
            let f = nf.update(&burst);
            floor_during_seed = floor_during_seed.max(f[0]);
        }
        assert!(
            floor_during_seed <= -40.0 - NoiseFloor::SEED_MARGIN_DB + 0.5,
            "provisional floor {floor_during_seed} should sit well below the burst"
        );
        // A quiet frame immediately after seeds the min and drops the floor.
        let f = nf.update(&quiet)[0];
        assert!(
            f <= -90.0 - NoiseFloor::FLOOR_MARGIN_DB + 0.5,
            "final seed floor {f} should track the running minimum"
        );
        // Once seeded, a low reading still pulls the EMA down promptly.
        let f = nf.update(&quiet)[0];
        assert!(f < -92.0, "post-seed floor {f} should fall onto quiet bins");
    }

    #[test]
    fn decode_chain_still_emits_audio() {
        let mut chain = DecodeChain::new(192_000.0, 400.0, 8000.0);
        chain.set_bandwidth(200.0);
        // A quarter second of IQ, fed in the ~16k blocks the radio delivers.
        let input: Vec<Complex32> = (0..48_000)
            .map(|i| Complex32::new((i as f32 * 0.01).sin() * 0.05, 0.0))
            .collect();
        let mut out = Vec::new();
        let mut total = 0usize;
        for c in input.chunks(16_384) {
            chain.process(c, &mut out);
            total += out.len();
        }
        assert!(total > 0, "audio FIR should emit samples");
        // Both FIRs swallow half their length once, at startup. Past that the
        // chain must keep up with the input or audio would drift behind.
        let expect = input.len() / 24;
        assert!(
            total > expect - 1200 && total <= expect,
            "emitted {total} of an expected ~{expect} samples"
        );
    }

    #[test]
    fn overlap_save_matches_direct_decimation() {
        let input: Vec<_> = (0..20_000)
            .map(|i| Complex32::new((i as f32 * 0.013).sin(), (i as f32 * 0.021).cos()))
            .collect();
        let mut fast = DecimFir::new(3500.0, 192_000.0, 24, 4095);
        let taps = fast.taps.clone();
        let mut got = Vec::new();
        for block in input.chunks(3000) {
            let mut part = Vec::new();
            fast.process(block, &mut part);
            got.extend(part);
        }
        let mut want = Vec::new();
        for i in (0..=input.len() - taps.len()).step_by(24) {
            want.push(
                input[i..i + taps.len()]
                    .iter()
                    .zip(&taps)
                    .map(|(x, h)| *x * *h)
                    .sum::<Complex32>(),
            );
        }
        let err = got
            .iter()
            .zip(&want)
            .map(|(a, b)| (*a - *b).norm())
            .fold(0.0f32, f32::max);
        assert!(err < 1e-3, "overlap-save differs by {err}");
    }

    #[test]
    fn nco_output_does_not_depend_on_chunking() {
        let input: Vec<_> = (0..10_000).map(|i| Complex32::new((i as f32 * 0.01).sin(), 0.5)).collect();
        let run = |sizes: &[usize]| {
            let mut nco = Nco::new();
            nco.set_freq(12_345.0, 1.0e6);
            let (mut all, mut part, mut at, mut k) = (Vec::new(), Vec::new(), 0, 0);
            while at < input.len() {
                let n = sizes[k % sizes.len()].min(input.len() - at);
                nco.mix(&input[at..at + n], &mut part);
                all.extend_from_slice(&part);
                at += n;
                k += 1;
            }
            all
        };
        let whole = run(&[10_000]);
        assert_eq!(whole, run(&[1, 7, 333, 1024, 5]));
        // and it is the rotation asked for
        let want = input[9_999] * Complex32::from_polar(1.0, (-2.0 * std::f64::consts::PI * 12_345.0 * 9_999.0 / 1.0e6) as f32);
        assert!((whole[9_999] - want).norm() < 1e-3, "{:?} vs {want:?}", whole[9_999]);
    }

    #[test]
    fn direct_path_matches_the_convolution() {
        // 2047 taps decimating by 125: the radio stage of every 48 kHz mode.
        let input: Vec<_> = (0..60_000)
            .map(|i| Complex32::new((i as f32 * 0.013).sin(), (i as f32 * 0.021).cos()))
            .collect();
        let mut fir = DecimFir::new(24_000.0, 6.0e6, 125, 2047);
        assert!(fir.fft.is_none(), "a deep decimation should take the direct path");
        let taps = fir.taps.clone();
        let mut got = Vec::new();
        for block in input.chunks(7_001) {
            let mut part = Vec::new();
            fir.process(block, &mut part);
            got.extend(part);
        }
        let want: Vec<Complex32> = (0..=input.len() - taps.len())
            .step_by(125)
            .map(|i| input[i..i + taps.len()].iter().zip(&taps).map(|(x, h)| *x * *h).sum())
            .collect();
        assert!(got.len() + 1 >= want.len() && got.len() <= want.len(), "{} outputs for {}", got.len(), want.len());
        let err = got.iter().zip(&want).map(|(a, b)| (*a - *b).norm()).fold(0.0f32, f32::max);
        assert!(err < 1e-4, "direct path differs by {err}");
    }

    /// Gain of the whole chain at `hz` off centre, relative to DC, in dB.
    fn chain_resp_db(fs_in: f64, bw: f32, target: f64, hz: f64) -> f32 {
        let tone = |hz: f64| -> f32 {
            let mut chain = DecodeChain::new(fs_in, bw, target);
            let n = (fs_in * 0.4) as usize;
            let mut phase = 0.0f64;
            let step = 2.0 * std::f64::consts::PI * hz / fs_in;
            let input: Vec<Complex32> = (0..n)
                .map(|_| {
                    let s = Complex32::from_polar(1.0, phase as f32);
                    phase += step;
                    if phase > std::f64::consts::PI {
                        phase -= 2.0 * std::f64::consts::PI;
                    }
                    s
                })
                .collect();
            let mut out = Vec::new();
            let mut all = Vec::new();
            for c in input.chunks(16384) {
                chain.process(c, &mut out);
                all.extend_from_slice(&out);
            }
            if all.len() < 200 {
                return -200.0;
            }
            let tail = &all[all.len() / 2..];
            let p: f32 = tail.iter().map(|c| c.norm_sqr()).sum::<f32>() / tail.len() as f32;
            10.0 * (p + 1e-30).log10()
        };
        tone(hz) - tone(0.0)
    }

    /// FT8/FT4 put the chain 1600 Hz above the dial and search 200–3000 Hz,
    /// so the passband has to be flat across ±1400 Hz. A filter too soft to
    /// manage that quietly costs the top and bottom of the waterfall several
    /// dB — the stations that were already marginal.
    #[test]
    fn ft8_passband_is_flat_across_the_search_range() {
        for off in [-1400.0, -1200.0, -600.0, 600.0, 1200.0, 1400.0] {
            let r = chain_resp_db(192_000.0, 3000.0, 12_000.0, off);
            assert!(
                r > -1.0,
                "FT8 audio {:.0} Hz is {r:.1} dB down; passband must be flat",
                off + 1600.0
            );
        }
        // ...and still stopped before the real part folds it onto itself.
        let fold = chain_resp_db(192_000.0, 3000.0, 12_000.0, -1750.0);
        assert!(
            fold < -30.0,
            "below the dial should be rejected, got {fold:.1} dB"
        );
    }

    /// A filter setting has to mean what it says: the narrow positions are
    /// the ones that dig a signal out of QRM, and they are also the ones a
    /// fixed tap count cannot resolve.
    #[test]
    fn narrow_filters_are_the_width_they_claim() {
        for bw in [80.0f32, 200.0, 500.0] {
            let edge = chain_resp_db(192_000.0, bw, 8000.0, (bw / 2.0) as f64);
            assert!(
                (-9.0..-3.0).contains(&edge),
                "{bw:.0} Hz filter is {edge:.1} dB at its own edge, so it is not {bw:.0} Hz wide"
            );
            // An octave out must be genuinely gone, not merely rolling off.
            let out = chain_resp_db(192_000.0, bw, 8000.0, bw as f64);
            assert!(
                out < -40.0,
                "{bw:.0} Hz filter passes {out:.1} dB at {bw:.0} Hz off centre"
            );
        }
    }

    /// Everything within half a channel of `fs_out` folds straight onto the
    /// signal when the decimator drops samples, and no later filter can
    /// separate it again.
    #[test]
    fn decimation_does_not_alias_into_the_channel() {
        for hz in [7900.0, 8000.0, 8100.0, 16_000.0] {
            let r = chain_resp_db(192_000.0, 400.0, 8000.0, hz);
            assert!(r < -80.0, "{hz:.0} Hz folds into the channel at {r:.1} dB");
        }
    }

    #[test]
    #[ignore]
    fn bench_fast_convolution() {
        use std::time::Instant;
        let input: Vec<_> = (0..96_000)
            .map(|i| Complex32::new((i as f32 * 0.017).sin(), 0.0))
            .collect();
        let taps = lowpass_taps(100.0, 8000.0, 1101);
        let at = Instant::now();
        let mut direct = Vec::with_capacity(input.len());
        for i in 0..input.len() - taps.len() {
            direct.push(
                input[i..i + taps.len()]
                    .iter()
                    .zip(&taps)
                    .map(|(x, h)| *x * *h)
                    .sum::<Complex32>(),
            );
        }
        let direct_ms = at.elapsed().as_secs_f64() * 1000.0;
        let mut fir = DecimFir::new(100.0, 8000.0, 1, 1101);
        let at = Instant::now();
        let mut out = Vec::new();
        for block in input.chunks(4096) {
            fir.process(block, &mut out);
        }
        let fft_ms = at.elapsed().as_secs_f64() * 1000.0;
        println!(
            "1101-tap convolution: direct {direct_ms:.1} ms, overlap-save {fft_ms:.1} ms ({:.1}x faster)",
            direct_ms / fft_ms
        );
    }
}

/// Basic receiver front-end cleanup, applied to raw IQ before anything else
/// looks at it.
///
/// A quadrature front end has two defects that are not signals and that no
/// amount of care downstream can undo:
///
/// * a **DC offset** — a constant bias on I and Q, which is a carrier sitting
///   exactly on the local oscillator. It is why every candidate picker here
///   has to blank the bins either side of the LO, and so why a real signal
///   that close cannot be seen.
/// * **IQ imbalance** — the two channels differing slightly in gain or in
///   quadrature, which mirrors a copy of every signal about the LO. The image
///   of a strong station is an ordinary-looking weak one, and detection
///   thresholds low enough to find real weak signals are low enough to find
///   those too.
///
/// The driver corrects both where it can (see `radio`), but support varies by
/// device and by SoapySDR backend, so the same corrections are done here as
/// well. Both are cheap, and both are no-ops on IQ that is already clean.
/// Streaming wideband impulse blanker. It deliberately runs before every
/// channel filter, while an RF impulse is still only a few samples wide.
pub struct NoiseBlanker {
    fs: f32,
    /// `fs/4` and the same floored at 32 — the training gate below reads
    /// these per sample at radio rate, so they are hoisted rather than
    /// re-divided two or three million times a second.
    train_q1: usize,
    train_min: usize,
    background: f32,
    pub(crate) level: u8,
    tail: usize,
    window_seen: usize,
    window_blanked: usize,
    last_rate: usize,
    inhibited: bool,
    trained: usize,
    /// Sample offset carried between blocks when the blanker is off, so the
    /// decimated background updates stay evenly spaced.
    level0_phase: usize,
}

impl NoiseBlanker {
    pub fn new(fs: f64) -> Self {
        let q1 = fs as usize / 4;
        Self {
            fs: fs as f32,
            train_q1: q1,
            train_min: q1.max(32),
            background: 1e-3,
            level: 2,
            tail: 0,
            window_seen: 0,
            window_blanked: 0,
            last_rate: 0,
            inhibited: false,
            trained: 0,
            level0_phase: 0,
        }
    }

    /// Off, gentle (6x), normal (5x), aggressive (4x).
    pub fn cycle(&mut self) -> &'static str {
        self.set_level((self.level + 1) % 4)
    }

    /// 0 = off, 1 = gentle (6x), 2 = normal (5x), 3 = aggressive (4x).
    pub fn set_level(&mut self, level: u8) -> &'static str {
        self.level = level.min(3);
        self.label()
    }

    pub fn label(&self) -> &'static str {
        ["off", "gentle", "normal", "aggressive"][self.level as usize]
    }

    pub fn blanks_per_second(&self) -> usize {
        self.last_rate
    }

    fn process(&mut self, iq: &mut [Complex32]) {
        // With the blanker off there is nothing to blank, so the only work
        // left is keeping the background estimate warm for when it is
        // re-enabled — done from a decimated sample so a 2-3 MS/s stream does
        // not pay `norm()` on every one of them.
        if self.level == 0 {
            let stride = ((iq.len() / 64).max(1)) as usize;
            let mut i = self.level0_phase % stride;
            while i < iq.len() {
                let mag = iq[i].norm();
                if self.trained == 0 {
                    self.background = mag.max(1e-6);
                } else {
                    let a = 1.0 - (-1.0 / (0.075 * self.fs * stride as f32)).exp();
                    self.background += (mag - self.background) * a;
                }
                self.window_seen += stride;
                i += stride;
            }
            self.level0_phase = (self.level0_phase + iq.len()) % stride;
            return;
        }
        let a = 1.0 - (-1.0 / (0.075 * self.fs)).exp();
        let threshold = [f32::INFINITY, 6.0, 5.0, 4.0][self.level as usize];
        for i in 0..iq.len() {
            let mag = iq[i].norm();
            if self.trained == 0 {
                self.background = mag.max(1e-6);
            }
            // Clamping keeps a crash from raising the reference used to
            // recognise the rest of that same crash.
            let observed = if self.trained < self.train_q1 {
                mag
            } else {
                mag.min((self.background * 2.5).max(1e-6))
            };
            self.background += (observed - self.background) * a;
            // `level != 0` is implicit: the off path returned above.
            let hot = !self.inhibited
                && self.trained >= self.train_min
                && mag > threshold * self.background.max(1e-6);
            if hot {
                for back in 0..=2 {
                    if let Some(s) = i.checked_sub(back).and_then(|j| iq.get_mut(j)) {
                        if s.norm_sqr() != 0.0 {
                            *s = Complex32::new(0.0, 0.0);
                            self.window_blanked += 1;
                        }
                    }
                }
                self.tail = 3;
            } else if self.tail > 0 {
                // Raised-cosine-like return to full amplitude prevents the
                // blanking edge itself from becoming a broadband click.
                let weight = [1.0, 0.75, 0.25, 0.0][self.tail];
                iq[i] *= weight;
                self.window_blanked += 1;
                self.tail -= 1;
            }
            self.window_seen += 1;
            self.trained = self.trained.saturating_add(1);
            if self.window_seen >= self.fs as usize {
                self.last_rate = self.window_blanked;
                self.inhibited = self.window_blanked * 50 > self.window_seen;
                self.window_seen = 0;
                self.window_blanked = 0;
            }
        }
    }
}

/// Samples per step of the front end's DC tracker (see `FrontEnd::remove_dc`).
const DC_CHUNK: usize = 32;

/// A one-pole of coefficient `a` over one `DC_CHUNK`: the weight each sample
/// of a chunk carries in the estimate at the chunk's end.
fn dc_chunk_weights(a: f64) -> [f32; DC_CHUNK] {
    std::array::from_fn(|j| (a * (1.0 - a).powi((DC_CHUNK - 1 - j) as i32)) as f32)
}

/// The image estimator's bands as runs of the 4096-bin transform: bin `k`
/// (1..4096) sits in band `((k + 2048) % 4096) * 12 / 4096`, which only
/// changes a dozen times along `k`.
/// `buf[k] = orig[k] - a·conj(orig[4096 - k])` for every bin but DC, `a`
/// being bin `k`'s band coefficient.
fn apply_image(buf: &mut [Complex32], orig: &[Complex32], runs: &[(usize, usize, usize)], image: &[Complex32; 12]) {
    let n = orig.len();
    for &(first, end, band) in runs {
        let a = image[band];
        for k in first..end {
            buf[k] = orig[k] - a * orig[n - k].conj();
        }
    }
}

/// Sum in four independent lanes, so the adds pipeline instead of each
/// waiting on the last.
fn lane_sum_c(v: &[Complex32]) -> Complex32 {
    let mut acc = [Complex32::new(0.0, 0.0); 4];
    let chunks = v.chunks_exact(4);
    let rest = chunks.remainder();
    for c in chunks {
        for l in 0..4 {
            acc[l] += c[l];
        }
    }
    rest.iter().fold((acc[0] + acc[1]) + (acc[2] + acc[3]), |s, &x| s + x)
}

fn lane_sum(v: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let chunks = v.chunks_exact(8);
    let rest = chunks.remainder();
    for c in chunks {
        for l in 0..8 {
            acc[l] += c[l];
        }
    }
    let lanes = ((acc[0] + acc[1]) + (acc[2] + acc[3])) + ((acc[4] + acc[5]) + (acc[6] + acc[7]));
    rest.iter().fold(lanes, |s, &x| s + x)
}

fn image_band_runs() -> Vec<(usize, usize, usize)> {
    const N: usize = 4096;
    let band = |k: usize| (((k + N / 2) % N) * 12 / N).min(11);
    let mut runs = Vec::new();
    let mut start = 1;
    for k in 2..=N {
        if k == N || band(k) != band(start) {
            runs.push((start, k, band(start)));
            start = k;
        }
    }
    runs
}

pub struct FrontEnd {
    fs: f64,
    dc: Complex32,
    /// The DC estimate has been seeded from real samples. After a reset (a
    /// retune, a reopen) the tuner's offset is a new value; tracking up to it
    /// from zero through a 2 Hz pole took a few hundred milliseconds, and the
    /// spike showed at the LO for all of them.
    dc_seeded: bool,
    iq_fft: Arc<dyn Fft<f32>>,
    iq_ifft: Arc<dyn Fft<f32>>,
    iq_buf: Vec<Complex32>,
    iq_orig: Vec<Complex32>,
    /// Transform scratch, so neither transform allocates one per block.
    iq_scratch: Vec<Complex32>,
    /// Block staging for the image corrector: the radio delivers arbitrary
    /// block sizes, and `chunks_exact_mut(4096)` alone left a different
    /// remainder uncorrected on every call — whether a sample was corrected
    /// depended on chunk alignment. `carry` holds the sub-block remainder,
    /// `stage` is the aligned scratch it is corrected in.
    carry: Vec<Complex32>,
    stage: Vec<Complex32>,
    /// Blocks skipped while the correction is idle (converged, nothing to
    /// apply). Drives the periodic re-probe that lets a drifting front end
    /// re-open the estimator without a manual reset.
    idle: u32,
    /// Consecutive probe blocks left in the current re-probe run.
    probe: u32,
    /// The image estimator's 12 bands as runs of FFT bins: `(first, end,
    /// band)` over bins 1..4096, each bin's mirror being `4096 - k`. A band
    /// is contiguous in bin order, so the loops walk plain ranges with the
    /// band's sums in registers; looking each bin's band and mirror up in
    /// tables made every accumulate wait on the last one through memory.
    band_runs: Vec<(usize, usize, usize)>,
    /// Per-pair estimator terms for bins 1..=2048: `X[k]·X[4096-k]` and
    /// `|X[k]|² + |X[4096-k]|²` (index 0 unused).
    pair_cross: Vec<Complex32>,
    pair_power: Vec<f32>,
    /// The uncorrected last 4096 samples of the staged input, for correcting
    /// the remainder that does not fill a block (see `correct_images`).
    tail: Vec<Complex32>,
    /// DC tracker weights for one chunk of `DC_CHUNK` samples, see `process`.
    dc_w: [f32; DC_CHUNK],
    /// The DC estimate at the start of the current chunk, the chunk's
    /// partial update, and how far into the chunk the stream is.
    dc_start: Complex32,
    dc_acc: [Complex32; 4],
    dc_pos: usize,
    image: [Complex32; 12],
    /// Samples seen, against the number needed before correcting at all.
    warm: u32,
    settle: u32,
    blanker: NoiseBlanker,
}

impl FrontEnd {
    pub fn new(fs: f64) -> Self {
        Self::with_blanker(fs, 2)
    }

    /// Front end with the impulse blanker disabled.
    ///
    /// The blanker keys on **wideband** magnitude against a 75 ms background
    /// average, so on a quiet band the leading edge of any strong narrowband
    /// burst reads as an impulse: measured here, a 20 dB burst loses its first
    /// 54 ms and a 30 dB burst its first 108 ms. That is fatal to a burst-mode
    /// protocol whose preamble is shorter than that — APRS with a short
    /// TXDELAY decodes reliably without it and not at all with it, and the
    /// packets it destroys are the *strongest* ones.
    ///
    /// Use this wherever a narrowband burst decoder follows. Impulse rejection
    /// for those paths belongs after the channel filter, keyed on the
    /// discriminator rather than on the whole span.
    pub fn without_blanker(fs: f64) -> Self {
        Self::with_blanker(fs, 0)
    }

    /// `blanker_level`: 0 = off, 1 = gentle, 2 = normal, 3 = aggressive.
    pub fn with_blanker(fs: f64, blanker_level: u8) -> Self {
        let mut planner = FftPlanner::new();
        let mut blanker = NoiseBlanker::new(fs);
        blanker.set_level(blanker_level);
        let iq_fft = planner.plan_fft_forward(4096);
        let iq_ifft = planner.plan_fft_inverse(4096);
        Self {
            fs,
            dc: Complex32::new(0.0, 0.0),
            dc_seeded: false,
            iq_scratch: vec![
                Complex32::new(0.0, 0.0);
                iq_fft.get_inplace_scratch_len().max(iq_ifft.get_inplace_scratch_len())
            ],
            iq_fft,
            iq_ifft,
            iq_buf: vec![Complex32::new(0.0, 0.0); 4096],
            iq_orig: vec![Complex32::new(0.0, 0.0); 4096],
            carry: Vec::with_capacity(4095),
            stage: Vec::new(),
            idle: 0,
            probe: 0,
            band_runs: image_band_runs(),
            tail: vec![Complex32::new(0.0, 0.0); 4096],
            pair_cross: vec![Complex32::new(0.0, 0.0); 2049],
            pair_power: vec![0.0; 2049],
            // ~2 Hz corner. Slow enough that it is a bias estimate rather
            // than a filter, so it cannot touch a signal even a few tens of
            // hertz off the LO — the region worth recovering in the first place.
            dc_w: dc_chunk_weights(1.0 - (-2.0 * std::f64::consts::PI * 2.0 / fs).exp()),
            dc_start: Complex32::new(0.0, 0.0),
            dc_acc: [Complex32::new(0.0, 0.0); 4],
            dc_pos: 0,
            image: [Complex32::new(0.0, 0.0); 12],
            warm: 0,
            // Two time constants of the estimator above.
            settle: (2.0 * fs) as u32,
            blanker,
        }
    }

    /// Forget everything learned — after a retune or a rate change, when the
    /// front end is being asked a different question.
    ///
    /// The blanker *setting* is a configuration choice, not something learned,
    /// so it survives; only its background estimate is discarded.
    pub fn reset(&mut self) {
        let level = self.blanker.level;
        *self = FrontEnd::with_blanker(self.fs, level);
    }

    /// The correction currently being applied, for display: DC magnitude and
    /// image rejection in dB (higher is better; ~100 means nothing to correct).
    pub fn status(&self) -> (f32, f32) {
        let err = self.image.iter().map(|v| v.norm()).fold(0.0f32, f32::max);
        let rej = if err > 1e-6 {
            -20.0 * err.log10()
        } else {
            100.0
        };
        (self.dc.norm(), rej.min(100.0))
    }

    /// 0 = off, 1 = gentle, 2 = normal, 3 = aggressive.
    pub fn set_blanker(&mut self, level: u8) {
        self.blanker.set_level(level);
    }

    pub fn cycle_blanker(&mut self) -> &'static str {
        self.blanker.cycle()
    }

    pub fn blanker_status(&self) -> (&'static str, usize) {
        (self.blanker.label(), self.blanker.blanks_per_second())
    }

    pub fn process(&mut self, iq: &mut [Complex32]) {
        // Start the tracker on the first block's mean instead of at zero, so
        // the offset is removed from the first block after a reset.
        if !self.dc_seeded && !iq.is_empty() {
            let sum = iq.iter().fold(Complex32::new(0.0, 0.0), |a, &s| a + s);
            self.dc = sum / iq.len() as f32;
            self.dc_start = self.dc;
            self.dc_seeded = true;
        }
        self.remove_dc(iq);

        self.warm = self.warm.saturating_add(iq.len() as u32);
        self.correct_images(iq);
        self.blanker.process(iq);
    }

    /// DC offset: track the mean with the ~2 Hz one-pole and subtract it.
    ///
    /// The pole is stepped once per `DC_CHUNK` samples rather than once per
    /// sample: `dc_w` holds the one-pole's impulse response over a chunk, so
    /// the estimate at each chunk boundary is the per-sample recursion's to
    /// rounding, and each sample has the estimate from its chunk's start
    /// taken off. Inside a chunk the per-sample estimate moves by about
    /// `a * DC_CHUNK` of the signal (a ~1e-4 relative change even at
    /// 250 kS/s), far under the noise. The per-sample recursion was a
    /// dependency chain of three float operations per sample — the largest
    /// single cost in the front end — where this vectorizes. The chunk grid
    /// counts samples, not calls, and every path sums in the same lanes, so
    /// the output does not depend on how the caller chunks its input.
    fn remove_dc(&mut self, iq: &mut [Complex32]) {
        let mut rest = &mut iq[..];
        // Finish a chunk left open by the last call.
        while self.dc_pos != 0 && !rest.is_empty() {
            let (s, tail) = rest.split_first_mut().unwrap();
            self.dc_one(s);
            rest = tail;
        }
        let whole = rest.len() / DC_CHUNK * DC_CHUNK;
        let (body, tail) = rest.split_at_mut(whole);
        for chunk in body.chunks_exact_mut(DC_CHUNK) {
            let d = self.dc_start;
            let mut acc = [Complex32::new(0.0, 0.0); 4];
            for (s4, w4) in chunk.chunks_exact_mut(4).zip(self.dc_w.chunks_exact(4)) {
                for l in 0..4 {
                    let v = s4[l] - d;
                    s4[l] = v;
                    acc[l] += v * w4[l];
                }
            }
            self.dc_start = d + ((acc[0] + acc[1]) + (acc[2] + acc[3]));
        }
        for s in tail {
            self.dc_one(s);
        }
        self.dc = self.dc_start;
    }

    #[inline]
    fn dc_one(&mut self, s: &mut Complex32) {
        let v = *s - self.dc_start;
        *s = v;
        self.dc_acc[self.dc_pos % 4] += v * self.dc_w[self.dc_pos];
        self.dc_pos += 1;
        if self.dc_pos == DC_CHUNK {
            let acc = self.dc_acc;
            self.dc_start += (acc[0] + acc[1]) + (acc[2] + acc[3]);
            self.dc_acc = [Complex32::new(0.0, 0.0); 4];
            self.dc_pos = 0;
        }
    }

    fn correct_images(&mut self, iq: &mut [Complex32]) {
        /// FFT/block length of the corrector.
        const N: usize = 4096;
        const K: usize = 12;
        // The estimator re-probe: while the correction sits idle (converged
        // and quiet) it still watches the spectrum at a slow duty cycle, so a
        // front end whose gain/phase balance DRIFTS — temperature, retune,
        // a different antenna — reopens the estimator and re-converges
        // without anyone calling `reset`. A probe is a RUN of consecutive
        // blocks, long enough that even a large image crosses the 1e-3
        // activity threshold within it (alpha per block is 0.01–0.2); once
        // any band's coefficient exceeds that the normal active condition
        // takes over and full-rate convergence proceeds as at startup. A
        // clean front end costs one run per ~250 blocks: 16 transforms out
        // of 500.
        const IDLE_PROBE_BLOCKS: u32 = 250;
        const PROBE_RUN_BLOCKS: u32 = 8;
        // Staging: process contiguous 4096-sample blocks out of a carry
        // buffer, so the estimator sees every sample in whole blocks no
        // matter how the radio chunks its deliveries. The remainder past the
        // last whole block goes out now, corrected through the transform of
        // the 4096 samples ending with it (see below), and is carried into
        // the next call's first block for the estimator.
        self.stage.clear();
        self.stage.extend_from_slice(&self.carry);
        let prev_carry = self.stage.len();
        self.stage.extend_from_slice(iq);
        let complete = self.stage.len() / N;
        let remainder = self.stage.len() - complete * N;
        self.carry.clear();
        self.carry
            .extend_from_slice(&self.stage[complete * N..]);
        if complete > 0 && remainder > 0 {
            let len = self.stage.len();
            self.tail.copy_from_slice(&self.stage[len - N..]);
        }
        let mut applied = false;
        for block in self.stage.chunks_exact_mut(N) {
            // Nothing worth correcting: skip both transforms. A dongle whose
            // driver already corrects the image reports ~100 dB of rejection,
            // and paying two 4096-pt FFTs per 4096 samples to apply a zero
            // was several percent of a core.
            //
            // The circular-FFT seam, documented rather than rebuilt: the
            // per-bin correction `X[k] - a·conj(X[mirror])` is exact in the
            // frequency domain, but applying it by one forward FFT, a
            // bin-wise fix, and one inverse FFT treats the 4096-sample block
            // as periodic — circular convolution, no overlap management.
            // The mirror term is a time-domain convolution of the block with
            // the band image response, so its circular wrap corrupts the
            // first taps-1 samples of each block. It is bounded: the image
            // coefficient |a| < 0.25 per band and the effective image
            // response spans only a few taps, so the seam contributes error
            // on the order of 0.25 × (few taps)/4096 of block power, i.e.
            // below -30 dB, at the leading edge only. A 50% overlap-save
            // scheme would double the transform count for a fix confined to
            // those samples; the estimator would also re-converge around
            // whatever the overlap returns. If the seam ever becomes
            // measurable (a test failing within a few hundred samples of a
            // block edge is the signature), stage two half-blocks with
            // overlap-save there.
            let active = self.warm < self.settle || !self.image.iter().all(|v| v.norm() < 1e-3);
            if !active && self.probe == 0 {
                self.idle += 1;
                if self.idle >= IDLE_PROBE_BLOCKS {
                    self.idle = 0;
                    self.probe = PROBE_RUN_BLOCKS;
                } else {
                    applied = false;
                    continue;
                }
            }
            if !active {
                // Mid-probe and still nothing to correct: spend one probe
                // block, and stop early only if the run ends up clean.
                self.probe -= 1;
            } else {
                self.probe = 0;
                self.idle = 0;
            }
            self.iq_buf.copy_from_slice(block);
            self.iq_fft.process_with_scratch(&mut self.iq_buf, &mut self.iq_scratch);
            self.iq_orig.copy_from_slice(&self.iq_buf);
            let mut cross = [Complex32::new(0.0, 0.0); K];
            let mut paired = [0.0f32; K];
            // Bin k and its mirror N - k give the same product and the same
            // pair power, so each pair is computed once, for k in 1..=N/2,
            // and every band sums the pairs it holds over a plain range:
            // bins below N/2 forward, bins above it through their mirrors.
            let x = &self.iq_orig;
            for k in 1..=N / 2 {
                let (a, b) = (x[k], x[N - k]);
                self.pair_cross[k] = a * b;
                self.pair_power[k] = a.norm_sqr() + b.norm_sqr();
            }
            for &(first, end, band) in &self.band_runs {
                let range = if first >= N / 2 { N + 1 - end..N + 1 - first } else { first..end };
                cross[band] += lane_sum_c(&self.pair_cross[range.clone()]);
                paired[band] += lane_sum(&self.pair_power[range]);
            }
            let alpha = (N as f32 / (self.fs as f32 * 2.0)).clamp(0.002, 0.2);
            let mut estimate = [None; K];
            let max_power = paired.iter().copied().fold(0.0f32, f32::max);
            for b in 0..K {
                if paired[b] > (max_power * 1e-3).max(1e-12) {
                    let v = cross[b] / paired[b];
                    if v.norm() < 0.25 {
                        estimate[b] = Some(v);
                    }
                }
            }
            for b in 0..K {
                if let Some(mut target) = estimate[b] {
                    let mut weight = 1.0;
                    for neighbour in [b.checked_sub(1), (b + 1 < K).then_some(b + 1)]
                        .into_iter()
                        .flatten()
                    {
                        if let Some(v) = estimate[neighbour] {
                            target += v * 0.25;
                            weight += 0.25;
                        }
                    }
                    target /= weight;
                    self.image[b] += (target - self.image[b]) * alpha;
                }
            }
            applied = self.warm >= self.settle;
            if applied {
                apply_image(&mut self.iq_buf, &self.iq_orig, &self.band_runs, &self.image);
                self.iq_ifft.process_with_scratch(&mut self.iq_buf, &mut self.iq_scratch);
                for (dst, src) in block.iter_mut().zip(&self.iq_buf) {
                    *dst = *src / N as f32;
                }
            }
        }
        // The remainder: correct it now rather than send it out as it came.
        // It used to leave uncorrected — up to 4095 samples at the end of
        // every delivery whose length is not a multiple of 4096 (a 20 ms
        // block at 8 MS/s leaves 256 more each time) — and only its carried
        // copy was corrected, then thrown away. The transform of the 4096
        // samples ending with it puts the remainder at the block's trailing
        // edge, clear of the leading-edge seam, and uses the coefficients as
        // they stand without updating them: the estimator meets these
        // samples in the next call's first block.
        if applied && remainder > 0 {
            self.iq_buf.copy_from_slice(&self.tail);
            self.iq_fft.process_with_scratch(&mut self.iq_buf, &mut self.iq_scratch);
            self.iq_orig.copy_from_slice(&self.iq_buf);
            apply_image(&mut self.iq_buf, &self.iq_orig, &self.band_runs, &self.image);
            self.iq_ifft.process_with_scratch(&mut self.iq_buf, &mut self.iq_scratch);
            let len = self.stage.len();
            for (dst, src) in self.stage[len - remainder..].iter_mut().zip(&self.iq_buf[N - remainder..]) {
                *dst = *src / N as f32;
            }
        }
        // Copy the corrected samples back over the caller's buffer. The
        // caller's samples occupy `stage[prev_carry..]` (the leading carry
        // has been consumed); blocks that were skipped (idle or warm-up) are
        // already identical to the staged input, so the straight copy is
        // correct for them too.
        iq.copy_from_slice(&self.stage[prev_carry..]);
    }
}

#[cfg(test)]
pub(crate) mod frontend_tests {
    use super::*;

    pub(crate) fn noise(rng: &mut u32) -> f32 {
        *rng ^= *rng << 13;
        *rng ^= *rng >> 17;
        *rng ^= *rng << 5;
        (*rng as f32 / u32::MAX as f32) - 0.5
    }

    /// Power in a narrow band around `hz`, by direct correlation.
    pub(crate) fn power_at(iq: &[Complex32], fs: f32, hz: f32) -> f32 {
        let n = iq.len().min(200_000);
        let start = iq.len() - n;
        let mut osc = Rotator::new(-2.0 * PI * hz / fs);
        let mut acc = Complex32::new(0.0, 0.0);
        for &s in &iq[start..] {
            acc += s * osc.next();
        }
        (acc / n as f32).norm_sqr()
    }

    /// A band of noise with a signal on it, a DC offset, and a gain and
    /// quadrature error between I and Q — an ordinary uncorrected front end.
    pub(crate) fn dirty_iq(
        fs: f32,
        sig_hz: f32,
        dc: Complex32,
        gain_err: f32,
        phase_err: f32,
    ) -> Vec<Complex32> {
        let mut rng = 0x1234_5678u32;
        // Long enough to cover the front end's settle time several times over.
        (0..(fs * 10.0) as usize)
            .map(|i| {
                let ph = 2.0 * PI * sig_hz * i as f32 / fs;
                let clean = Complex32::from_polar(0.5, ph)
                    + Complex32::new(noise(&mut rng), noise(&mut rng)) * 0.05;
                // Gain and quadrature error on Q, then the DC bias.
                let q = clean.im * gain_err + clean.re * phase_err;
                Complex32::new(clean.re, q) + dc
            })
            .collect()
    }

    /// Run a long stretch of dirty IQ through in blocks, as the app does,
    /// and return the settled tail of the output. Feeding the corrected
    /// output back in instead would measure a feedback loop, not the
    /// correction.
    pub(crate) fn run_frontend(iq: &[Complex32], fs: f32) -> Vec<Complex32> {
        let mut fe = FrontEnd::new(fs as f64);
        let mut out = Vec::with_capacity(iq.len());
        for chunk in iq.chunks(16_384) {
            let mut buf = chunk.to_vec();
            fe.process(&mut buf);
            out.extend_from_slice(&buf);
        }
        // The last quarter, by which time the estimators have long settled.
        out.split_off(out.len() * 3 / 4)
    }

    fn impulse_case(fs: f32, impulses: bool) -> (Vec<Complex32>, Vec<Complex32>) {
        let mut rng = 0x91ab_cdefu32;
        let mut clean = Vec::with_capacity((fs * 3.0) as usize);
        let mut dirty = Vec::with_capacity(clean.capacity());
        for i in 0..clean.capacity() {
            let tone = Complex32::from_polar(0.015, 2.0 * PI * 1500.0 * i as f32 / fs);
            let noise = Complex32::new(noise(&mut rng), noise(&mut rng)) * 0.025;
            clean.push(tone + noise);
            let crash = impulses && i > fs as usize && i % 1800 < 4;
            dirty.push(
                tone + noise
                    + if crash {
                        Complex32::new(8.0, -6.0)
                    } else {
                        Complex32::new(0.0, 0.0)
                    },
            );
        }
        (clean, dirty)
    }

    fn error_power(got: &[Complex32], want: &[Complex32]) -> f32 {
        got.iter()
            .zip(want)
            .skip(got.len() / 3)
            .map(|(a, b)| (*a - *b).norm_sqr())
            .sum::<f32>()
            / (got.len() - got.len() / 3) as f32
    }

    #[test]
    fn noise_blanker_removes_impulses_without_harming_clean_iq() {
        let fs = 48_000.0;
        let (clean, dirty) = impulse_case(fs, true);
        let mut blanked = dirty.clone();
        let mut nb = NoiseBlanker::new(fs as f64);
        for block in blanked.chunks_mut(4096) {
            nb.process(block);
        }
        let improvement =
            10.0 * (error_power(&dirty, &clean) / error_power(&blanked, &clean)).log10();
        assert!(
            improvement >= 10.0,
            "impulse error improved only {improvement:.1} dB"
        );

        let (_, mut untouched) = impulse_case(fs, false);
        let before = power_at(&untouched, fs, 1500.0);
        let mut nb = NoiseBlanker::new(fs as f64);
        for block in untouched.chunks_mut(4096) {
            nb.process(block);
        }
        let harm = (10.0 * (before / power_at(&untouched, fs, 1500.0)).log10()).abs();
        assert!(harm < 0.2, "clean tone changed by {harm:.2} dB");
    }

    #[test]
    #[ignore]
    fn bench_noise_blanker() {
        let fs = 48_000.0;
        let (clean, dirty) = impulse_case(fs, true);
        let mut blanked = dirty.clone();
        let mut nb = NoiseBlanker::new(fs as f64);
        for block in blanked.chunks_mut(4096) {
            nb.process(block);
        }
        let improvement =
            10.0 * (error_power(&dirty, &clean) / error_power(&blanked, &clean)).log10();
        println!(
            "wideband impulse blanker: {improvement:.1} dB error-power improvement, {} blanks/s",
            nb.blanks_per_second()
        );
    }

    /// The DC offset is what forces the LO bins to be blanked, so removing it
    /// is the difference between a signal near the LO being findable and not.
    #[test]
    fn dc_offset_is_removed() {
        let fs = 48_000.0f32;
        let iq = dirty_iq(fs, 400.0, Complex32::new(0.25, -0.18), 1.0, 0.0);
        let before = power_at(&iq, fs, 0.0);
        let after = power_at(&run_frontend(&iq, fs), fs, 0.0);
        let db = 10.0 * (before / after.max(1e-30)).log10();
        assert!(
            db > 40.0,
            "DC only came down {db:.0} dB ({before:.6} -> {after:.9})"
        );
    }

    /// IQ imbalance mirrors every signal about the LO. With detection
    /// thresholds low enough to find real weak signals, the image of a strong
    /// station is an ordinary-looking weak one — a station that is not there.
    #[test]
    fn iq_imbalance_image_is_suppressed() {
        let fs = 48_000.0f32;
        // 6% gain error and 3 degrees of quadrature error: poor, but the sort
        // of thing an uncorrected direct-conversion front end really does.
        let iq = dirty_iq(fs, 400.0, Complex32::new(0.0, 0.0), 1.06, 0.052);
        let before = 10.0 * (power_at(&iq, fs, 400.0) / power_at(&iq, fs, -400.0)).log10();
        let out = run_frontend(&iq, fs);
        let sig = power_at(&out, fs, 400.0);
        let after = 10.0 * (sig / power_at(&out, fs, -400.0)).log10();
        assert!(
            after > before + 15.0,
            "image rejection only improved {:.1} dB ({before:.1} -> {after:.1})",
            after - before
        );
        assert!(
            sig > power_at(&iq, fs, 400.0) * 0.5,
            "the wanted signal was attenuated"
        );
    }

    fn frequency_dependent_case(fs: f32) -> (Vec<Complex32>, [f32; 2]) {
        let hz = [4687.5, 16406.25]; // exact 4096-point bins
        let a = [Complex32::new(0.09, 0.025), Complex32::new(-0.065, 0.045)];
        let iq = (0..(fs * 10.0) as usize)
            .map(|i| {
                let mut x = Complex32::new(0.0, 0.0);
                for j in 0..2 {
                    let s = Complex32::from_polar(0.35, 2.0 * PI * hz[j] * i as f32 / fs);
                    x += s + a[j] * s.conj();
                }
                x
            })
            .collect();
        (iq, hz)
    }

    fn worst_rejection(iq: &[Complex32], fs: f32, hz: [f32; 2]) -> f32 {
        hz.into_iter()
            .map(|f| 10.0 * (power_at(iq, fs, f) / power_at(iq, fs, -f).max(1e-30)).log10())
            .fold(f32::INFINITY, f32::min)
    }

    fn run_subband_only(iq: &[Complex32], fs: f32) -> Vec<Complex32> {
        let mut fe = FrontEnd::new(fs as f64);
        fe.blanker.level = 0;
        let mut out = Vec::new();
        for chunk in iq.chunks(16_384) {
            let mut block = chunk.to_vec();
            fe.process(&mut block);
            out.extend(block);
        }
        out.split_off(out.len() * 3 / 4)
    }

    #[test]
    fn subband_iq_correction_beats_a_scalar_solve() {
        let fs = 48_000.0;
        let (iq, hz) = frequency_dependent_case(fs);
        let scalar_a = Complex32::new(0.0125, 0.035);
        let scalar: Vec<_> = iq.iter().map(|&x| x - scalar_a * x.conj()).collect();
        let scalar_rej = worst_rejection(&scalar, fs, hz);
        let per_band = worst_rejection(&run_subband_only(&iq, fs), fs, hz);
        assert!(
            per_band >= scalar_rej + 15.0,
            "subbands gained only {:.1} dB ({scalar_rej:.1} -> {per_band:.1})",
            per_band - scalar_rej
        );
    }

    /// Deliveries that are not a multiple of the corrector's 4096-sample
    /// block are corrected as fully as aligned ones. The remainder at the
    /// end of each delivery used to go out uncorrected; at 10 000 samples a
    /// delivery that was a fifth of the signal on average, enough to undo
    /// most of the rejection.
    #[test]
    fn unaligned_deliveries_are_corrected_like_aligned_ones() {
        let fs = 48_000.0;
        let (iq, hz) = frequency_dependent_case(fs);
        let aligned = worst_rejection(&run_subband_only(&iq, fs), fs, hz);
        for size in [10_000usize, 4_097, 20_480 + 1_000] {
            let mut fe = FrontEnd::new(fs as f64);
            fe.blanker.level = 0;
            let mut out = Vec::new();
            for chunk in iq.chunks(size) {
                let mut block = chunk.to_vec();
                fe.process(&mut block);
                out.extend(block);
            }
            let tail = out.split_off(out.len() * 3 / 4);
            let got = worst_rejection(&tail, fs, hz);
            assert!(
                got > aligned - 3.0,
                "{size}-sample deliveries: {got:.1} dB of rejection against {aligned:.1} dB aligned"
            );
        }
    }

    /// The chunked DC tracker gives the same output however the input is
    /// split, and follows a step in the offset like the 2 Hz pole it is.
    #[test]
    fn dc_tracker_is_independent_of_chunking() {
        let fs = 48_000.0f32;
        let mut rng = 0x5eed_1234u32;
        let iq: Vec<Complex32> = (0..(fs as usize * 2))
            .map(|i| {
                let dc = if i < fs as usize { Complex32::new(0.1, -0.05) } else { Complex32::new(-0.2, 0.08) };
                dc + Complex32::new(noise(&mut rng), noise(&mut rng)) * 0.01
            })
            .collect();
        let run = |size: usize| {
            let mut fe = FrontEnd::without_blanker(fs as f64);
            let mut out = Vec::new();
            for chunk in iq.chunks(size) {
                let mut b = chunk.to_vec();
                fe.remove_dc(&mut b);
                out.extend(b);
            }
            (out, fe.dc)
        };
        let (a, dc_a) = run(4096);
        for size in [1usize, 31, 33, 1000, 7777] {
            let (b, dc_b) = run(size);
            assert_eq!(dc_a, dc_b, "{size}-sample calls");
            assert!(a == b, "{size}-sample calls changed the output");
        }
        // Against the per-sample recursion it replaces.
        let alpha = 1.0 - (-2.0 * std::f64::consts::PI * 2.0 / f64::from(fs)).exp();
        let mut dc = Complex32::new(0.0, 0.0);
        let mut worst = 0.0f32;
        for (i, (&s, &got)) in iq.iter().zip(&a).enumerate() {
            dc += (s - dc) * alpha as f32;
            if i > 100 {
                worst = worst.max((got - (s - dc)).norm());
            }
        }
        // Inside a chunk the per-sample estimate moves by up to `a` per sample
        // of the residual, which peaks at about 0.32 just after the step.
        let bound = DC_CHUNK as f32 * alpha as f32 * 0.35;
        assert!(worst < bound, "differs from the per-sample tracker by {worst} (bound {bound})");
        assert!((dc_a - dc).norm() < 1e-4, "estimate {dc_a} against {dc}");
    }

    #[test]
    #[ignore]
    fn bench_frequency_dependent_iq() {
        let fs = 48_000.0;
        let (iq, hz) = frequency_dependent_case(fs);
        let scalar_a = Complex32::new(0.0125, 0.035);
        let scalar: Vec<_> = iq.iter().map(|&x| x - scalar_a * x.conj()).collect();
        let scalar_rej = worst_rejection(&scalar, fs, hz);
        let per_band = worst_rejection(&run_subband_only(&iq, fs), fs, hz);
        println!(
            "frequency-dependent IQ: worst-band scalar {scalar_rej:.1} dB, 12-band {per_band:.1} dB ({:.1} dB gain)",
            per_band - scalar_rej
        );
    }

    /// And clean IQ must come out unharmed — the correction has to be a
    /// no-op on a front end that has nothing wrong with it.
    #[test]
    fn clean_iq_is_left_alone() {
        let fs = 48_000.0f32;
        let clean = dirty_iq(fs, 400.0, Complex32::new(0.0, 0.0), 1.0, 0.0);
        let out = run_frontend(&clean, fs);
        let sig = power_at(&out, fs, 400.0);
        let want = power_at(&clean, fs, 400.0);
        assert!(
            (sig / want - 1.0).abs() < 0.05,
            "clean signal changed by {:.1}%",
            (sig / want - 1.0) * 100.0
        );
        let img = 10.0 * (sig / power_at(&out, fs, -400.0)).log10();
        assert!(img > 40.0, "invented an image: {img:.0} dB rejection");
    }
}

#[cfg(test)]
mod frontend_bench {
    use super::frontend_tests::*;
    use super::*;

    #[test]
    #[ignore]
    fn bench_frontend() {
        let fs = 48_000.0f32;
        println!("\n== DC offset removal ==");
        for dc in [0.02f32, 0.10, 0.25] {
            let iq = dirty_iq(fs, 400.0, Complex32::new(dc, -dc * 0.7), 1.0, 0.0);
            let before = power_at(&iq, fs, 0.0);
            let after = power_at(&run_frontend(&iq, fs), fs, 0.0);
            println!(
                "  offset {:.2} of full scale: {:.0} dB down",
                dc,
                10.0 * (before / after.max(1e-30)).log10()
            );
        }

        println!("\n== IQ image rejection (gain error / quadrature error) ==");
        for (g, p) in [
            (1.01f32, 0.009f32),
            (1.03, 0.026),
            (1.06, 0.052),
            (1.12, 0.105),
        ] {
            let iq = dirty_iq(fs, 400.0, Complex32::new(0.0, 0.0), g, p);
            let before = 10.0 * (power_at(&iq, fs, 400.0) / power_at(&iq, fs, -400.0)).log10();
            let out = run_frontend(&iq, fs);
            let after = 10.0 * (power_at(&out, fs, 400.0) / power_at(&out, fs, -400.0)).log10();
            println!(
                "  {:>4.0}% gain, {:>4.1} deg: {:>5.1} dB -> {:>5.1} dB",
                (g - 1.0) * 100.0,
                p.asin().to_degrees(),
                before,
                after
            );
        }
    }
}

#[cfg(test)]
mod frontend_cost {
    use super::*;

    /// The front end runs on every sample of every block, so it has to be
    /// negligible against the 192 kHz stream it sits in front of.
    #[test]
    #[ignore]
    fn bench_frontend_cost() {
        let fs = 192_000.0f64;
        let n = (fs * 4.0) as usize;
        let mut rng = 0x2222_1111u32;
        let mut nz = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            (rng as f32 / u32::MAX as f32) - 0.5
        };
        let iq: Vec<Complex32> = (0..n).map(|_| Complex32::new(nz(), nz())).collect();
        let mut fe = FrontEnd::new(fs);
        let t = std::time::Instant::now();
        for chunk in iq.chunks(16_384) {
            let mut buf = chunk.to_vec();
            fe.process(&mut buf);
        }
        let el = t.elapsed().as_secs_f64();
        println!(
            "  {:.1} ms for {:.1} s of 192 kHz IQ ({:.2}% of real time)",
            el * 1000.0,
            n as f64 / fs,
            el / (n as f64 / fs) * 100.0
        );
    }
}

#[cfg(test)]
mod window_bench {
    use super::*;

    fn noise(rng: &mut u32) -> f32 {
        *rng ^= *rng << 13;
        *rng ^= *rng >> 17;
        *rng ^= *rng << 5;
        (*rng as f32 / u32::MAX as f32) - 0.5
    }

    /// Peak height over the *local* floor, which is what `scout_peaks` works
    /// from: the median of the surrounding bins, excluding the peak's own
    /// skirts. Measuring against the whole spectrum's median instead misses
    /// the entire effect — one strong signal cannot move the median of 8192
    /// bins, but it can certainly raise the floor beside itself.
    fn peak_over_floor(spec: &[f32], fs: f32, hz: f32) -> f32 {
        const NEAR: isize = 3;
        const CTX: isize = 40;
        let n = spec.len() as isize;
        let bin = fs / n as f32;
        let centre = ((hz / bin) + n as f32 / 2.0).round() as isize;
        let at = |i: isize| spec[i.clamp(0, n - 1) as usize];
        let mut peak = f32::MIN;
        for d in -NEAR..=NEAR {
            peak = peak.max(at(centre + d));
        }
        let mut ctx: Vec<f32> = (-CTX..=CTX)
            .filter(|d| d.abs() > NEAR)
            .map(|d| at(centre + d))
            .collect();
        ctx.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        peak - ctx[ctx.len() / 2]
    }

    fn spectrum_of(iq: &[Complex32], win: Window, size: usize) -> Vec<f32> {
        let mut s = Spectrum::with_window(size, win);
        let mut out = Vec::new();
        s.power_db(iq, &mut out);
        out
    }

    #[test]
    fn ingest_then_empty_flush_still_yields_a_spectrum() {
        let n = 1024;
        let iq: Vec<Complex32> = (0..n * 2)
            .map(|i| {
                let p = 2.0 * PI * 10.0 * i as f32 / n as f32;
                Complex32::new(p.cos(), p.sin())
            })
            .collect();
        let mut s = Spectrum::new(n);
        s.ingest(&iq);
        let mut out = Vec::new();
        s.power_dbfs(&[], &mut out);
        assert_eq!(out.len(), n);
        assert!(out.iter().any(|&v| v > -80.0), "flush produced a dead floor");
    }

    /// Two competing effects, measured against each other: how far a weak
    /// narrowband signal stands out of the floor on its own, and how far it
    /// stands out with a strong station nearby leaking over it.
    #[test]
    #[ignore]
    fn bench_window_tradeoff() {
        let fs = 192_000.0f32;
        let n = 8192;
        let len = n * 8;
        let weak_hz = 2_000.0f32;

        println!("\n== a weak narrowband signal on its own ==");
        println!("{:>10}{:>12}{:>12}", "level", "Hann", "Blackman-H");
        for amp in [0.02f32, 0.01, 0.005] {
            let mut rng = 0x9111_2222u32;
            let iq: Vec<Complex32> = (0..len)
                .map(|i| {
                    let ph = 2.0 * PI * weak_hz * i as f32 / fs;
                    Complex32::from_polar(amp, ph)
                        + Complex32::new(noise(&mut rng), noise(&mut rng)) * 0.01
                })
                .collect();
            let h = peak_over_floor(&spectrum_of(&iq, Window::Hann, n), fs, weak_hz);
            let b = peak_over_floor(&spectrum_of(&iq, Window::BlackmanHarris, n), fs, weak_hz);
            println!(
                "{:>10}{:>12}{:>12}",
                format!("{amp:.3}"),
                format!("{h:.1} dB"),
                format!("{b:.1} dB")
            );
        }

        println!("\n== the same weak signal with a strong station nearby ==");
        println!("(weak signal at 0.005; strong one 80 dB above it)");
        println!("(under ~4 bins apart the two are not resolved at all, so");
        println!(" those rows measure the strong signal, not the weak one)");
        println!(
            "{:>10}{:>10}{:>12}{:>12}",
            "spacing", "bins", "Hann", "Blackman-H"
        );
        for sep in [25.0f32, 50.0, 100.0, 200.0, 500.0, 2_000.0] {
            let mut rng = 0x9111_2222u32;
            let iq: Vec<Complex32> = (0..len)
                .map(|i| {
                    let t = i as f32 / fs;
                    let weak = Complex32::from_polar(0.005, 2.0 * PI * weak_hz * t);
                    let strong = Complex32::from_polar(50.0, 2.0 * PI * (weak_hz + sep) * t);
                    weak + strong + Complex32::new(noise(&mut rng), noise(&mut rng)) * 0.01
                })
                .collect();
            let h = peak_over_floor(&spectrum_of(&iq, Window::Hann, n), fs, weak_hz);
            let b = peak_over_floor(&spectrum_of(&iq, Window::BlackmanHarris, n), fs, weak_hz);
            println!(
                "{:>10}{:>10}{:>12}{:>12}",
                format!("{sep:.0} Hz"),
                format!("{:.1}", sep / (fs / n as f32)),
                format!("{h:.1} dB"),
                format!("{b:.1} dB")
            );
        }
    }
}

#[cfg(test)]
mod frontend_audit {
    use super::frontend_tests::*;
    use super::*;

    /// SNR of a weak tone measured against the broadband floor around it.
    fn tone_snr_db(iq: &[Complex32], fs: f32, hz: f32) -> f32 {
        let sig = power_at(iq, fs, hz);
        // Floor sampled well away from the tone and its image, avoiding DC.
        let probes = [hz + 3000.0, hz + 5000.0, hz - 4000.0, hz + 7000.0];
        let n: f32 = probes.iter().map(|&f| power_at(iq, fs, f)).sum::<f32>() / probes.len() as f32;
        10.0 * (sig / n.max(1e-30)).log10()
    }

    /// Does the front end cost a weak signal anything?
    ///
    /// Each stage is exercised on its own so a loss can be attributed. The
    /// image corrector is the one under suspicion: it round-trips every 4096
    /// samples through an FFT, edits bins, and inverse-transforms with no
    /// window and no overlap, which is a circular convolution and rings at
    /// the block seams.
    #[test]
    #[ignore]
    fn bench_frontend_weak_signal_cost() {
        let fs = 192_000.0f32;
        let hz = 12_000.0f32;
        for (label, gain_err, phase_err, dc) in [
            (
                "clean input      ",
                1.0f32,
                0.0f32,
                Complex32::new(0.0, 0.0),
            ),
            ("1% gain imbalance", 1.01, 0.005, Complex32::new(0.0, 0.0)),
            (
                "5% gain imbalance",
                1.05,
                0.03,
                Complex32::new(0.01, -0.007),
            ),
        ] {
            let mut rng = 0x1357_9bdfu32;
            let n = (fs * 4.0) as usize;
            // A weak tone in noise, then the receiver's imperfections on top.
            let raw: Vec<Complex32> = (0..n)
                .map(|i| {
                    let ph = 2.0 * PI * hz * i as f32 / fs;
                    let clean = Complex32::from_polar(0.004, ph)
                        + Complex32::new(noise(&mut rng), noise(&mut rng)) * 0.05;
                    let q = clean.im * gain_err + clean.re * phase_err;
                    Complex32::new(clean.re, q) + dc
                })
                .collect();
            let before = tone_snr_db(&raw[raw.len() * 3 / 4..], fs, hz);
            let after = tone_snr_db(&run_frontend(&raw, fs), fs, hz);
            println!(
                "  {label}: {before:6.2} dB in -> {after:6.2} dB out   ({:+.2} dB)",
                after - before
            );
        }
    }

    /// The blanker is on by default at "normal". On a band with no impulse
    /// noise it should be doing nothing at all.
    #[test]
    #[ignore]
    fn bench_blanker_on_quiet_band() {
        let fs = 192_000.0f32;
        let hz = 12_000.0f32;
        let mut rng = 0x2468_ace0u32;
        let n = (fs * 4.0) as usize;
        let raw: Vec<Complex32> = (0..n)
            .map(|i| {
                let ph = 2.0 * PI * hz * i as f32 / fs;
                Complex32::from_polar(0.004, ph)
                    + Complex32::new(noise(&mut rng), noise(&mut rng)) * 0.05
            })
            .collect();
        let mut nb = NoiseBlanker::new(fs as f64);
        let mut out = raw.clone();
        for chunk in out.chunks_mut(16_384) {
            nb.process(chunk);
        }
        let before = tone_snr_db(&raw[raw.len() * 3 / 4..], fs, hz);
        let after = tone_snr_db(&out[out.len() * 3 / 4..], fs, hz);
        println!(
            "  blanker 'normal' on clean noise: {before:6.2} -> {after:6.2} dB  ({:+.2} dB), {} blanks/s",
            after - before,
            nb.blanks_per_second()
        );
    }
}


#[cfg(test)]
mod welch_stream {
    use super::*;

    fn noise(rng: &mut u32) -> f32 {
        *rng ^= *rng << 13;
        *rng ^= *rng >> 17;
        *rng ^= *rng << 5;
        (*rng as f32 / u32::MAX as f32) - 0.5
    }

    /// White noise at about -95 dBFS per bin (size 1024), plus a tone of
    /// `amp` at exact bin `bin` of a `size`-point FFT over `[on, off)`.
    fn burst_iq(len: usize, size: usize, bin: f32, amp: f32, on: usize, off: usize, seed: u32) -> Vec<Complex32> {
        let mut rng = seed;
        (0..len)
            .map(|i| {
                let n = Complex32::new(noise(&mut rng), noise(&mut rng)) * 0.001;
                if (on..off).contains(&i) {
                    let ph = 2.0 * PI * bin * i as f32 / size as f32;
                    n + Complex32::from_polar(amp, ph)
                } else {
                    n
                }
            })
            .collect()
    }

    fn median(v: &[f32]) -> f32 {
        let mut s = v.to_vec();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        s[s.len() / 2]
    }

    fn std_dev(v: &[f32]) -> f32 {
        let m = v.iter().sum::<f32>() / v.len() as f32;
        (v.iter().map(|x| (x - m) * (x - m)).sum::<f32>() / v.len() as f32).sqrt()
    }

    /// The server's pattern on the old path: `ingest` every block, one
    /// `power_dbfs` snapshot at the end of the frame interval.
    fn old_path(iq: &[Complex32], size: usize, block: usize) -> Vec<f32> {
        let mut s = Spectrum::new(size);
        for b in iq.chunks(block) {
            s.ingest(b);
        }
        let mut out = Vec::new();
        s.power_dbfs(&[], &mut out);
        out
    }

    fn new_path(iq: &[Complex32], size: usize, block: usize) -> (Vec<f32>, Vec<f32>, usize) {
        let mut s = Spectrum::new(size);
        for b in iq.chunks(block) {
            s.ingest_welch(b);
        }
        let (mut avg, mut peak) = (Vec::new(), Vec::new());
        let n = s.take_dbfs(&mut avg, Some(&mut peak));
        (avg, peak, n)
    }

    /// (a) A 2 ms burst in the middle of a 40 ms frame at 5 MS/s.
    #[test]
    fn mid_interval_burst_reaches_the_display() {
        let (size, fs) = (1024usize, 5e6f32);
        let len = (0.040 * fs) as usize; // 200,000
        let (on, off) = ((0.019 * fs) as usize, (0.021 * fs) as usize);
        let bin = 205.0;
        let iq = burst_iq(len, size, bin, 0.1, on, off, 0x1234_5678); // -20 dBFS
        let at = bin as usize + size / 2;

        let old = old_path(&iq, size, 8192);
        let (avg, peak, nseg) = new_path(&iq, size, 8192);
        let floor = median(&avg);
        println!(
            "burst bin: old {:.1} dBFS, new avg {:.1}, new peak {:.1}; floor {:.1}; {} segments",
            old[at], avg[at], peak[at], floor, nseg
        );
        assert_eq!(nseg, 1 + (len - size) / (size / 2));
        assert!(old[at] < floor + 6.0, "old path should miss the burst: {:.1}", old[at]);
        assert!((peak[at] + 20.0).abs() < 0.5, "peak should read the burst level: {:.1}", peak[at]);
        // Duty 2/40 is -13 dB; the average is attenuated by about that.
        let duty_db = 10.0 * (2.0f32 / 40.0).log10();
        assert!(
            (avg[at] - (-20.0 + duty_db)).abs() < 1.5,
            "avg should show the burst attenuated by duty: {:.1}",
            avg[at]
        );
        assert!(avg[at] > floor + 40.0);
    }

    /// (b) The floor is far steadier when every sample contributes.
    #[test]
    fn full_duty_floor_is_steadier() {
        let size = 1024;
        let iq = burst_iq(200_000, size, 0.0, 0.0, 0, 0, 0xdead_beef);
        let old = old_path(&iq, size, 8192);
        let (avg, _, nseg) = new_path(&iq, size, 8192);
        let (so, sn) = (std_dev(&old), std_dev(&avg));
        println!("white-noise per-bin std dev: old {so:.3} dB, new {sn:.3} dB ({nseg} segments)");
        assert!(sn < so / 4.0, "old {so:.3} dB vs new {sn:.3} dB");
        // Both estimate the same floor.
        assert!((median(&old) - median(&avg)).abs() < 1.5);
    }

    /// (c) Calibration matches `power_dbfs`: full-scale tone at a bin center.
    #[test]
    fn full_scale_tone_reads_zero_dbfs() {
        let size = 1024;
        let k = 100usize;
        let iq: Vec<Complex32> = (0..size * 20)
            .map(|i| Complex32::from_polar(1.0, 2.0 * PI * k as f32 * i as f32 / size as f32))
            .collect();
        let (avg, peak, _) = new_path(&iq, size, 4096);
        let old = old_path(&iq, size, 4096);
        let at = k + size / 2;
        let argmax = avg.iter().enumerate().max_by(|a, b| a.1.partial_cmp(b.1).unwrap()).unwrap().0;
        assert_eq!(argmax, at);
        for (name, v) in [("avg", avg[at]), ("peak", peak[at]), ("old", old[at])] {
            assert!(v.abs() < 0.05, "{name} reads {v:.3} dBFS");
        }
    }

    /// (d) A cap bounds the work per take and still spreads it over the
    /// whole interval: bursts at either end of a 40 ms frame at 10 MS/s.
    #[test]
    fn capped_budget_is_bounded_and_spread() {
        let (size, fs, cap) = (1024usize, 10e6f32, 100usize);
        let len = (0.040 * fs) as usize; // 400,000: 781 overlapped slots
        let ms = |t: f32| (t * fs / 1000.0) as usize;
        let bin = 300.0;
        let at = bin as usize + size / 2;
        let mut s = Spectrum::new(size);
        s.set_max_segments_per_take(cap);
        let (mut avg, mut peak) = (Vec::new(), Vec::new());
        // Interval 1 has no forecast (late-take guard); 2 and 3 are planned.
        let cases = [(0.5, 2.5), (0.5, 2.5), (37.0, 39.0)];
        for (i, &(t0, t1)) in cases.iter().enumerate() {
            let iq = burst_iq(len, size, bin, 0.1, ms(t0), ms(t1), 0x5151 + i as u32);
            for b in iq.chunks(10_007) {
                s.ingest_welch(b);
            }
            let n = s.take_dbfs(&mut avg, Some(&mut peak));
            println!(
                "interval {}: {n} segments (cap {cap}), burst {t0}-{t1} ms peak {:.1} dBFS, floor {:.1}",
                i + 1,
                peak[at],
                median(&avg)
            );
            assert!(n <= 2 * cap + 32, "guard should bound the first interval: {n}");
            if i > 0 {
                assert!(n <= cap && n >= cap * 3 / 4, "planned interval used {n} of {cap}");
            }
            assert!((peak[at] + 20.0).abs() < 1.0, "burst missed: {:.1}", peak[at]);
            assert!(s.welch.pending.len() < size);
        }
        // A take that never comes: still bounded, and still sampling.
        let iq = burst_iq(len * 10, size, 0.0, 0.0, 0, 0, 77);
        s.ingest_welch(&iq);
        let n = s.take_dbfs(&mut avg, None);
        println!("10x late take: {n} segments");
        assert!(n > cap && n <= 2 * cap + 32, "{n}");
    }

    /// (e) Arbitrary block sizes give exactly the one-shot result.
    #[test]
    fn remainder_carries_across_odd_blocks() {
        let size = 1024;
        let len = 50_000;
        let iq = burst_iq(len, size, 37.5, 0.3, 10_000, 30_000, 99);
        let (avg1, peak1, n1) = new_path(&iq, size, len);

        let mut s = Spectrum::new(size);
        let sizes = [1usize, 7, 333, 1023, 1025, 4097, 2, 511, 512, 513];
        let (mut pos, mut k) = (0, 0);
        while pos < len {
            let end = (pos + sizes[k % sizes.len()]).min(len);
            s.ingest_welch(&iq[pos..end]);
            assert!(s.welch.pending.len() < size);
            pos = end;
            k += 1;
        }
        let (mut avg2, mut peak2) = (Vec::new(), Vec::new());
        let n2 = s.take_dbfs(&mut avg2, Some(&mut peak2));
        assert_eq!(n1, 1 + (len - size) / (size / 2));
        assert_eq!(n1, n2);
        assert_eq!(avg1, avg2);
        assert_eq!(peak1, peak2);

        // Nothing new: outputs untouched, 0 returned. Then clear() resets.
        let before = avg2.clone();
        assert_eq!(s.take_dbfs(&mut avg2, Some(&mut peak2)), 0);
        assert_eq!(avg2, before);
        s.ingest_welch(&iq[..size - 1]);
        s.clear();
        s.ingest_welch(&iq[..1]);
        assert_eq!(s.take_dbfs(&mut avg2, None), 0);
        assert_eq!(s.welch.pending.len(), 1);
    }

    /// Cost per segment of the streaming path, for choosing a cap.
    /// `cargo test -p scannerd-dsp --release -- --ignored --nocapture bench_welch`
    #[test]
    #[ignore]
    fn bench_welch_segment_cost() {
        for size in [1024usize, 2048, 8192] {
            let iq = burst_iq(1 << 22, size, 10.0, 0.1, 0, 1 << 22, 3);
            let mut s = Spectrum::new(size);
            s.ingest_welch(&iq[..1 << 20]); // warm up
            let mut out = Vec::new();
            s.take_dbfs(&mut out, None);
            let mut best = f64::MAX;
            let mut nseg = 0;
            for _ in 0..3 {
                let t = std::time::Instant::now();
                s.ingest_welch(&iq);
                let el = t.elapsed().as_secs_f64();
                nseg = s.take_dbfs(&mut out, None);
                best = best.min(el / nseg as f64);
            }
            let us = best * 1e6;
            let per_s_10m = 10e6 / (size / 2) as f64;
            println!(
                "size {size:>5}: {us:6.2} us/segment; full duty at 10 MS/s = {per_s_10m:.0} seg/s = {:.1}% of a core ({nseg} segs/run)",
                us * per_s_10m / 1e4
            );
        }
    }
}
