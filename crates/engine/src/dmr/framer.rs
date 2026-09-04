//! Burst framing for a DMR channel.
//!
//! The front end yields one frequency estimate per sample; this finds the
//! 24-dibit centre sync (or an EMB) in that stream and hands up whole 132-dibit
//! bursts, each tagged with the timeslot the CACH named.
//!
//! Acquisition scans every sample phase for any of the four 48-bit sync
//! words. Once locked, each following TDMA frame is read at a 144-symbol
//! stride (12-dibit CACH + 132-dibit burst). The stride is `144 * fs / 4800`
//! samples — not a rounded integer — because the analog scanner's 2.048 MS/s
//! decimates to 47627.9 Hz, 9.922 samples/symbol. A whole-sample step slipped
//! a full symbol by the second burst and chopped every live call to 60–120 ms.
//! Voice bursts B–F carry EMB instead of a sync; a decoded EMB keeps the lock
//! without re-acquiring.

use super::fec::{self, Emb, SlotType};
use super::sync::{self, SyncKind};
use super::{
    BURST_DIBITS, CACH_DIBITS, DEV_OUTER_HZ, FRAME_DIBITS, SYMBOL_RATE, dibit_level, fit_symbols,
    samples_per_symbol, slice_at,
};
use num_complex::Complex32;
use scannerd_dsp::OnePole;
use std::f32::consts::TAU;

/// Dibit errors allowed against a known sync during acquisition.
const ACQUIRE_MAX_ERRORS: u8 = 4;
/// Consecutive frames whose centre field is unreadable before lock is dropped.
const LOSS_LIMIT: u32 = 6;

/// Who transmitted the burst, from the sync word that opened it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncSource {
    Bs,
    Ms,
    Unknown,
}

/// What the burst is carrying.
#[derive(Clone, Debug, PartialEq)]
pub enum BurstKind {
    Voice {
        /// Present when the centre field was a voice sync (superframe burst A).
        sync: Option<SyncKind>,
        /// Present when the centre field was an EMB (bursts B–F).
        emb: Option<Emb>,
        /// 0 for the sync burst, 1..=5 for the five EMB bursts that follow.
        voice_idx: u8,
    },
    Data {
        slot_type: Option<SlotType>,
        sync: Option<SyncKind>,
    },
}

/// One decoded TDMA burst.
#[derive(Clone, Debug)]
pub struct Burst {
    /// Timeslot from the CACH TACT, 1 or 2. `None` on simplex / damaged CACH.
    pub slot: Option<u8>,
    pub source: SyncSource,
    pub kind: BurstKind,
    /// The 132 burst dibits, centre field included.
    pub dibits: [u8; BURST_DIBITS],
}

impl Burst {
    /// The 108 payload dibits: 54 before the centre field and 54 after.
    pub fn payload(&self) -> [u8; 108] {
        let mut out = [0u8; 108];
        out[..54].copy_from_slice(&self.dibits[..54]);
        out[54..].copy_from_slice(&self.dibits[78..]);
        out
    }

    /// The 196 information-channel bits from a data burst, excluding the
    /// split Slot Type and centre sync fields, ready for BPTC(196,96).
    pub fn bptc_payload_bits(&self) -> [u8; 196] {
        let mut out = [0u8; 196];
        let mut n = 0;
        for dibit in self.data_payload_dibits() {
            out[n] = (dibit >> 1) & 1;
            out[n + 1] = dibit & 1;
            n += 2;
        }
        out
    }

    /// The 98 coded dibits outside Slot Type and the centre sync. BPTC,
    /// trellis-coded rate-3/4, and uncoded full-rate data all use this same
    /// physical region with different channel coding.
    pub fn data_payload_dibits(&self) -> [u8; 98] {
        let mut out = [0u8; 98];
        out[..49].copy_from_slice(&self.dibits[..49]);
        out[49..].copy_from_slice(&self.dibits[83..]);
        out
    }

    pub fn is_voice(&self) -> bool {
        matches!(self.kind, BurstKind::Voice { .. })
    }

    pub fn color_code(&self) -> Option<u8> {
        match &self.kind {
            BurstKind::Voice { emb: Some(emb), .. } => Some(emb.color_code),
            BurstKind::Data {
                slot_type: Some(st),
                ..
            } => Some(st.color_code),
            _ => None,
        }
    }

    pub fn encrypted(&self) -> bool {
        matches!(
            &self.kind,
            BurstKind::Voice {
                emb: Some(emb),
                ..
            } if emb.pi
        )
    }
}

/// ETSI data-type values we act on.
#[allow(dead_code)] // classified in tests; live path only ends on terminator
pub const DT_VOICE_LC_HEADER: u8 = 1;
pub const DT_TERMINATOR_LC: u8 = 2;
#[allow(dead_code)] // used by synthetic-burst tests
pub const DT_IDLE: u8 = 9;

/// CACH interleave: air-bit index → deinterleaved bit index.
const CACH_INTERLEAVE: [usize; 24] = [
    0, 7, 8, 9, 1, 10, 11, 12, 2, 13, 14, 15, 3, 16, 4, 17, 18, 19, 5, 20, 21, 22, 6, 23,
];

/// A DMR receiver: front end and framer wired together.
pub struct DmrReceiver {
    front: DmrFrontEnd,
    framer: DmrFramer,
    hz: Vec<f32>,
    last_quality_db: f32,
}

impl DmrReceiver {
    pub fn new(fs: f64) -> Self {
        // The boxcar is a matched filter at ~one symbol; its length is the
        // nearest whole sample. Symbol *spacing* is fractional — see DmrFramer.
        let boxcar = samples_per_symbol(fs);
        Self {
            front: DmrFrontEnd::new(fs, boxcar),
            framer: DmrFramer::new(fs),
            hz: Vec::new(),
            last_quality_db: 0.0,
        }
    }

    /// Feed one block of complex baseband; returns any completed bursts.
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<Burst> {
        let mut hz = std::mem::take(&mut self.hz);
        self.front.process(iq, &mut hz);
        let bursts = self.decode(&hz);
        self.hz = hz;
        bursts
    }

    /// Run the receiver over pre-discriminated Hz, for callers that
    /// discriminate a shared filtered buffer once for several consumers.
    pub fn process_hz(&mut self, hz: &[f32]) -> Vec<Burst> {
        let mut boxed = std::mem::take(&mut self.hz);
        self.front.process_hz(hz, &mut boxed);
        let bursts = self.decode(&boxed);
        self.hz = boxed;
        bursts
    }

    /// Quality measurement and framing over discriminated Hz.
    fn decode(&mut self, hz: &[f32]) -> Vec<Burst> {
        let mut signal = 0.0f64;
        let mut error = 0.0f64;
        for &h in hz {
            let ideal = f64::from(dibit_level(slice_at(h, DEV_OUTER_HZ)));
            signal += ideal * ideal;
            let residual = f64::from(h) - ideal;
            error += residual * residual;
        }
        self.last_quality_db = if error > 0.0 {
            (10.0 * (signal / error).max(f64::MIN_POSITIVE).log10()) as f32
        } else {
            0.0
        };
        self.framer.push(hz)
    }

    pub fn locked(&self) -> bool {
        self.framer.locked()
    }

    pub fn quality_db(&self) -> f32 {
        self.last_quality_db
    }

    pub fn offset_hz(&self) -> f32 {
        self.front.offset_hz()
    }
}

/// FM discriminator plus a one-symbol boxcar. Same structure as the Phase 2
/// front end; only the symbol period and the deviations differ.
struct DmrFrontEnd {
    prev: Complex32,
    hz_per_rad: f32,
    buf: Vec<f32>,
    head: usize,
    count: usize,
    sum: f32,
    sps: usize,
    dc: OnePole,
    /// Discriminated Hz scratch for the complex-input path; `process_hz`
    /// callers bring their own discriminator and never fill this.
    disc: Vec<f32>,
}

impl DmrFrontEnd {
    fn new(fs: f64, sps: usize) -> Self {
        Self {
            prev: Complex32::new(0.0, 0.0),
            hz_per_rad: (fs / TAU as f64) as f32,
            buf: vec![0.0; sps],
            head: 0,
            count: 0,
            sum: 0.0,
            sps,
            dc: OnePole::new((fs * 0.04) as f32),
            disc: Vec::new(),
        }
    }

    fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        self.disc.clear();
        self.disc.reserve(iq.len());
        for &x in iq {
            let d = x * self.prev.conj();
            self.prev = x;
            let hz = if d.norm_sqr() > 0.0 {
                d.arg() * self.hz_per_rad
            } else {
                0.0
            };
            self.disc.push(hz);
        }
        let disc = std::mem::take(&mut self.disc);
        self.process_hz(&disc, out);
        self.disc = disc;
    }

    /// Boxcar and DC tracking over pre-discriminated Hz, shared-shape entry
    /// point mirroring [`crate::p25::C4fmFrontEnd::process_hz`].
    fn process_hz(&mut self, hz: &[f32], out: &mut Vec<f32>) {
        out.clear();
        for &sample in hz {
            self.sum -= self.buf[self.head];
            self.buf[self.head] = sample;
            self.sum += sample;
            self.head = (self.head + 1) % self.sps;
            if self.count < self.sps {
                self.count += 1;
            }
            if self.count >= self.sps {
                let acc = self.sum / self.sps as f32;
                out.push(acc - self.dc.process(acc));
            }
        }
    }

    fn offset_hz(&self) -> f32 {
        self.dc.value()
    }
}

/// Per-slot voice-superframe position, so a late EMB can still be numbered.
#[derive(Clone, Copy)]
struct SlotVoice {
    next_idx: u8,
    source: SyncSource,
}

struct DmrFramer {
    buf: Vec<f32>,
    consumed: u64,
    /// Absolute sample index of the next CACH start. Fractional: the analog
    /// scanner's channel rate is not an integer number of samples per DMR
    /// symbol, so a whole-sample stride slips a full symbol by the second
    /// burst and the call dies at ~60–120 ms.
    next_start: Option<f64>,
    /// Absolute sample index acquisition has already searched.
    ///
    /// While unlocked, `acquire` sweeps every position in the retained buffer
    /// and classifies each twice, normal and inverted. The buffer holds two
    /// frames, so on each push it re-examines several thousand positions it
    /// already rejected against the thousand or so that are new. A position's
    /// sync match cannot change once its window has arrived, so rejecting one
    /// is final — until the fit that judged it changes, which only happens
    /// after a successful acquire or a loss of lock, and both reset this.
    acq_scanned_abs: u64,
    losses: u32,
    amp: f32,
    offset: f32,
    inverted: bool,
    /// `fs / 4800`, not necessarily an integer.
    sps: f64,
    /// Voice-superframe counters, indexed by timeslot 0/1. Slot-unknown
    /// traffic uses index 0.
    voice: [Option<SlotVoice>; 2],
    /// Last colour code confirmed by a Slot Type or EMB. Late-entry voice
    /// (EMB with no preceding voice sync) is accepted only when it matches,
    /// so an all-zero QR codeword cannot open a call on silence.
    last_cc: Option<u8>,
}

impl DmrFramer {
    fn new(fs: f64) -> Self {
        Self {
            buf: Vec::new(),
            consumed: 0,
            next_start: None,
            acq_scanned_abs: 0,
            losses: 0,
            amp: DEV_OUTER_HZ,
            offset: 0.0,
            inverted: false,
            sps: fs / SYMBOL_RATE,
            voice: [None, None],
            last_cc: None,
        }
    }

    fn locked(&self) -> bool {
        self.next_start.is_some()
    }

    fn frame_samples(&self) -> f64 {
        FRAME_DIBITS as f64 * self.sps
    }

    /// Linear interpolation so symbol centres can sit between ADC samples.
    fn hz_at(&self, abs: f64) -> f32 {
        let rel = abs - self.consumed as f64;
        if rel < 0.0 {
            return 0.0;
        }
        let i = rel.floor() as usize;
        let frac = (rel - i as f64) as f32;
        let a = match self.buf.get(i) {
            Some(&v) => v,
            None => return 0.0,
        };
        let b = self.buf.get(i + 1).copied().unwrap_or(a);
        a + frac * (b - a)
    }

    fn slice_abs(&self, abs: f64) -> u8 {
        let d = slice_at(self.hz_at(abs) - self.offset, self.amp);
        if self.inverted { d ^ 0b10 } else { d }
    }

    fn push(&mut self, samples: &[f32]) -> Vec<Burst> {
        self.buf.extend_from_slice(samples);
        let mut out = Vec::new();
        let frame_samples = self.frame_samples();

        loop {
            match self.next_start {
                Some(start) => {
                    let rel = start - self.consumed as f64;
                    // +1 so the interpolator can read the sample past the last symbol.
                    let need = rel + frame_samples + 1.0;
                    if need > self.buf.len() as f64 {
                        break;
                    }
                    match self.read_frame(start) {
                        Some(burst) => {
                            self.losses = 0;
                            out.push(burst);
                            // read_frame re-anchors next_start when it sees a
                            // sync; only step blindly if it left the cursor.
                            if self.next_start == Some(start) {
                                self.next_start = Some(start + frame_samples);
                            }
                        }
                        None => {
                            self.losses += 1;
                            if self.losses >= LOSS_LIMIT {
                                // Drop lock and throw away the samples we
                                // already walked so acquire cannot latch
                                // onto the same false peak forever.
                                let skip = (rel + frame_samples).max(0.0).min(self.buf.len() as f64)
                                    as usize;
                                if skip > 0 {
                                    self.buf.drain(..skip);
                                    self.consumed += skip as u64;
                                }
                                self.next_start = None;
                                self.acq_scanned_abs = 0;
                                self.voice = [None, None];
                                self.last_cc = None;
                                self.losses = 0;
                            } else {
                                self.next_start = Some(start + frame_samples);
                            }
                        }
                    }
                }
                None => match self.acquire() {
                    Some(abs_start) => self.next_start = Some(abs_start),
                    None => break,
                },
            }
        }

        let keep = (frame_samples * 2.0 + self.sps).ceil() as usize + 2;
        if self.buf.len() > keep {
            let drop = self.buf.len() - keep;
            let limit = match self.next_start {
                Some(s) => (s - self.consumed as f64).max(0.0) as usize,
                None => drop,
            };
            let drop = drop.min(limit);
            if drop > 0 {
                self.buf.drain(..drop);
                self.consumed += drop as u64;
            }
        }
        out
    }

    fn read_frame(&mut self, start: f64) -> Option<Burst> {
        let slot = decode_cach(
            &(0..CACH_DIBITS)
                .map(|j| self.slice_abs(start + j as f64 * self.sps))
                .collect::<Vec<_>>(),
        );
        let burst_start = start + CACH_DIBITS as f64 * self.sps;
        let mut dibits = [0u8; BURST_DIBITS];
        for (j, d) in dibits.iter_mut().enumerate() {
            *d = self.slice_abs(burst_start + j as f64 * self.sps);
        }
        let mut centre = [0u8; sync::SYNC_DIBITS];
        centre.copy_from_slice(&dibits[54..78]);

        let classified =
            sync::classify(&centre, false).filter(|&(_, err)| err <= ACQUIRE_MAX_ERRORS);
        if let Some((kind, _)) = classified {
            self.follow_sync(burst_start, kind);
            // Re-time from this sync the way P25 does: a 3 ppm clock is
            // invisible over a burst, but the analog chain's non-integer
            // sps would otherwise accumulate.
            let off = self.sync_sample_offset(burst_start, kind);
            self.next_start = Some(start + self.frame_samples() + off);
        }

        let kind = classify_burst(
            &dibits,
            classified.map(|(k, _)| k),
            slot,
            &mut self.voice,
            self.last_cc,
        )?;
        // Only a Slot Type (Golay-protected, and never the all-zero word
        // on a live repeater idle) may confirm the colour code. An EMB of
        // zeros is a legal QR codeword (CC 0) and must not arm late entry.
        if let BurstKind::Data {
            slot_type: Some(st),
            ..
        } = &kind
        {
            self.last_cc = Some(st.color_code);
        }
        let source = match &kind {
            BurstKind::Voice { sync: Some(s), .. } | BurstKind::Data { sync: Some(s), .. } => {
                s.source()
            }
            BurstKind::Voice { .. } | BurstKind::Data { .. } => slot
                .and_then(|s| self.voice.get(slot_index(s)))
                .and_then(|v| v.as_ref())
                .map(|v| v.source)
                .unwrap_or(SyncSource::Unknown),
        };

        Some(Burst {
            slot,
            source,
            kind,
            dibits,
        })
    }

    fn follow_sync(&mut self, burst_start: f64, kind: SyncKind) {
        let word = sync::PATTERNS
            .iter()
            .find(|&&(_, k)| k == kind)
            .map(|&(w, _)| w)
            .unwrap();
        let mut samples = [0.0f32; sync::SYNC_DIBITS];
        let mut ideal = [0.0f32; sync::SYNC_DIBITS];
        for j in 0..sync::SYNC_DIBITS {
            samples[j] = self.hz_at(burst_start + (54 + j) as f64 * self.sps);
            let level = dibit_level(sync::dibits(word)[j]);
            ideal[j] = if self.inverted { -level } else { level };
        }
        let (amp, offset) = fit_symbols(&samples, &ideal);
        self.amp = 0.7 * self.amp + 0.3 * amp;
        self.offset = 0.7 * self.offset + 0.3 * offset;
    }

    /// Best sample-phase offset of a known sync, in samples (can be negative).
    fn sync_sample_offset(&self, burst_start: f64, _kind: SyncKind) -> f64 {
        let sync_abs = burst_start + 54.0 * self.sps;
        let mut best_err = u8::MAX;
        let mut best_off = 0.0f64;
        let limit = (self.sps * 0.6).max(1.0);
        let mut off = -limit;
        while off <= limit {
            let mut centre = [0u8; sync::SYNC_DIBITS];
            for j in 0..sync::SYNC_DIBITS {
                centre[j] = self.slice_abs(sync_abs + off + j as f64 * self.sps);
            }
            if let Some((_, err)) = sync::classify(&centre, false) {
                if err < best_err {
                    best_err = err;
                    best_off = off;
                }
            }
            off += 0.25;
        }
        best_off
    }

    /// Scan every sample phase for a sync; returns the absolute CACH start
    /// of the matching frame (12 dibits before the burst).
    fn acquire(&mut self) -> Option<f64> {
        let sync_at = (CACH_DIBITS + 54) as f64 * self.sps;
        let sync_span = sync::SYNC_DIBITS as f64 * self.sps;
        if (self.buf.len() as f64) < self.frame_samples() + 1.0 {
            return None;
        }
        let last_start = (self.buf.len() as f64 - sync_at - sync_span - 1.0)
            .floor()
            .max(0.0) as usize;
        // Resume where the previous sweep stopped rather than re-testing the
        // whole retained tail.
        let first_start = self.acq_scanned_abs.saturating_sub(self.consumed) as usize;
        let mut best: Option<(u8, usize, bool)> = None;
        for start in first_start..=last_start {
            let mut centre = [0u8; sync::SYNC_DIBITS];
            for j in 0..sync::SYNC_DIBITS {
                let abs = self.consumed as f64 + start as f64 + sync_at + j as f64 * self.sps;
                centre[j] = slice_at(self.hz_at(abs) - self.offset, self.amp);
            }
            // Pack the observation once; both polarity tests are then XOR +
            // popcount against the same word.
            let got = sync::pack_dibits(&centre);
            for inverted in [false, true] {
                if let Some((_, err)) = sync::classify_packed(got, inverted) {
                    if err <= ACQUIRE_MAX_ERRORS
                        && best.is_none_or(|(best_err, _, _)| err < best_err)
                    {
                        best = Some((err, start, inverted));
                    }
                }
            }
        }
        // Everything up to here has now been judged against the current fit.
        self.acq_scanned_abs = self.consumed + last_start as u64 + 1;
        let (_, start, inverted) = best?;
        // A new fit is about to be made, so earlier rejections no longer hold.
        self.acq_scanned_abs = 0;
        self.inverted = inverted;

        // Fit scale and offset on the winning sync.
        let mut samples = [0.0f32; sync::SYNC_DIBITS];
        let mut centre = [0u8; sync::SYNC_DIBITS];
        for j in 0..sync::SYNC_DIBITS {
            let abs = self.consumed as f64 + start as f64 + sync_at + j as f64 * self.sps;
            samples[j] = self.hz_at(abs);
            centre[j] = slice_at(samples[j] - self.offset, self.amp);
        }
        if let Some((kind, _)) = sync::classify(&centre, inverted) {
            let word = sync::PATTERNS
                .iter()
                .find(|&&(_, k)| k == kind)
                .map(|&(w, _)| w)
                .unwrap();
            let mut ideal = [0.0f32; sync::SYNC_DIBITS];
            for (j, slot) in ideal.iter_mut().enumerate() {
                let level = dibit_level(sync::dibits(word)[j]);
                *slot = if inverted { -level } else { level };
            }
            let (amp, offset) = fit_symbols(&samples, &ideal);
            self.amp = 0.7 * self.amp + 0.3 * amp;
            self.offset = 0.7 * self.offset + 0.3 * offset;
        }

        if start > 0 {
            self.buf.drain(..start);
            self.consumed += start as u64;
        }
        Some(self.consumed as f64)
    }
}

fn slot_index(slot: u8) -> usize {
    if slot >= 2 { 1 } else { 0 }
}

fn classify_burst(
    dibits: &[u8; BURST_DIBITS],
    sync: Option<SyncKind>,
    slot: Option<u8>,
    voice: &mut [Option<SlotVoice>; 2],
    last_cc: Option<u8>,
) -> Option<BurstKind> {
    let vi = slot.map(slot_index).unwrap_or(0);
    if let Some(kind) = sync {
        if kind.is_voice() {
            voice[vi] = Some(SlotVoice {
                next_idx: 1,
                source: kind.source(),
            });
            return Some(BurstKind::Voice {
                sync: Some(kind),
                emb: None,
                voice_idx: 0,
            });
        }
        // A data sync names this timeslot as not-in-voice. Only clear the
        // superframe we *know* this burst belongs to: a CACH-less idle on
        // the other slot used to wipe slot 1's just-opened superframe, so
        // bursts B–F never decoded and every call died after burst A.
        if slot.is_some() {
            voice[vi] = None;
        }
        let slot_type = decode_slot_type(dibits);
        return Some(BurstKind::Data {
            slot_type,
            sync: Some(kind),
        });
    }

    let mut centre_bits = [0u8; 48];
    for (i, &d) in dibits[54..78].iter().enumerate() {
        centre_bits[i * 2] = (d >> 1) & 1;
        centre_bits[i * 2 + 1] = d & 1;
    }
    let mut emb_bits = [0u8; 16];
    emb_bits[..8].copy_from_slice(&centre_bits[..8]);
    emb_bits[8..].copy_from_slice(&centre_bits[40..48]);
    let emb = fec::emb_decode(emb_bits);

    if let Some(emb) = emb {
        // All-zero is a legal QR codeword (CC 0). Silence slices to zero,
        // so an open superframe would otherwise hear five fake voice bursts
        // and never hang. Reject it unless a Slot Type already named CC 0.
        let raw_zero = emb_bits.iter().all(|&b| b == 0);
        if raw_zero && last_cc != Some(0) {
            return None;
        }
        let cc_ok = last_cc.is_none_or(|cc| cc == emb.color_code);
        if let Some(state) = voice[vi] {
            if (1..=5).contains(&state.next_idx) && cc_ok {
                let idx = state.next_idx;
                if let Some(s) = &mut voice[vi] {
                    s.next_idx += 1;
                    if s.next_idx > 5 {
                        voice[vi] = None;
                    }
                }
                return Some(BurstKind::Voice {
                    sync: None,
                    emb: Some(emb),
                    voice_idx: idx,
                });
            }
        } else if last_cc == Some(emb.color_code) {
            // Late entry: a Slot Type on this frequency already named this
            // colour code (typically the other slot's idle).
            voice[vi] = Some(SlotVoice {
                next_idx: 2,
                source: SyncSource::Unknown,
            });
            return Some(BurstKind::Voice {
                sync: None,
                emb: Some(emb),
                voice_idx: 1,
            });
        }
    }

    None
}

fn decode_cach(dibits: &[u8]) -> Option<u8> {
    if dibits.len() < CACH_DIBITS {
        return None;
    }
    let mut cach = [0u8; 24];
    for i in 0..CACH_DIBITS {
        cach[CACH_INTERLEAVE[i * 2]] = (dibits[i] >> 1) & 1;
        cach[CACH_INTERLEAVE[i * 2 + 1]] = dibits[i] & 1;
    }
    let mut tact = [0u8; 7];
    tact.copy_from_slice(&cach[..7]);
    if !fec::hamming74_decode(&mut tact) {
        return None;
    }
    // TACT bit 1 is TC: 0 → timeslot 1, 1 → timeslot 2.
    Some(tact[1] + 1)
}

fn decode_slot_type(dibits: &[u8; BURST_DIBITS]) -> Option<SlotType> {
    let mut bits = [0u8; 20];
    // Five dibits either side of the centre field.
    for i in 0..5 {
        bits[i * 2] = (dibits[49 + i] >> 1) & 1;
        bits[i * 2 + 1] = dibits[49 + i] & 1;
    }
    for i in 0..5 {
        bits[10 + i * 2] = (dibits[78 + i] >> 1) & 1;
        bits[10 + i * 2 + 1] = dibits[78 + i] & 1;
    }
    fec::slot_type_decode(bits)
}

/// Build a CACH whose TACT names `slot` (1 or 2).
#[cfg(test)]
pub fn encode_cach(slot: u8) -> [u8; CACH_DIBITS] {
    let tact = fec::hamming74_encode(&[0, slot.saturating_sub(1) & 1, 0, 1]);
    let mut bits = [0u8; 24];
    bits[..7].copy_from_slice(&tact);
    let mut dibits = [0u8; CACH_DIBITS];
    for i in 0..CACH_DIBITS {
        dibits[i] = (bits[CACH_INTERLEAVE[i * 2]] << 1) | bits[CACH_INTERLEAVE[i * 2 + 1]];
    }
    dibits
}

/// A voice burst B–F: 54 payload + EMB + 54 payload.
#[cfg(test)]
pub fn encode_voice_emb_burst(payload: &[u8; 108], cc: u8) -> [u8; BURST_DIBITS] {
    let emb = fec::emb_encode(cc, false, 0b01);
    let mut burst = [0u8; BURST_DIBITS];
    burst[..54].copy_from_slice(&payload[..54]);
    burst[78..].copy_from_slice(&payload[54..]);
    let mut bits = [0u8; 48];
    bits[..8].copy_from_slice(&emb[..8]);
    bits[40..].copy_from_slice(&emb[8..]);
    for i in 0..24 {
        burst[54 + i] = (bits[i * 2] << 1) | bits[i * 2 + 1];
    }
    burst
}

/// A voice burst: 54 payload + sync + 54 payload.
#[cfg(test)]
pub fn encode_voice_burst(payload: &[u8; 108], kind: SyncKind) -> [u8; BURST_DIBITS] {
    let word = sync::PATTERNS
        .iter()
        .find(|&&(_, k)| k == kind)
        .map(|&(w, _)| w)
        .unwrap();
    let mut burst = [0u8; BURST_DIBITS];
    burst[..54].copy_from_slice(&payload[..54]);
    burst[54..78].copy_from_slice(&sync::dibits(word));
    burst[78..].copy_from_slice(&payload[54..]);
    burst
}

/// A data burst: info + slot-type + data sync + slot-type + info.
#[cfg(test)]
pub fn encode_data_burst(cc: u8, data_type: u8, kind: SyncKind) -> [u8; BURST_DIBITS] {
    let word = sync::PATTERNS
        .iter()
        .find(|&&(_, k)| k == kind)
        .map(|&(w, _)| w)
        .unwrap();
    let st = fec::slot_type_encode(cc, data_type);
    let mut burst = [0u8; BURST_DIBITS];
    burst[54..78].copy_from_slice(&sync::dibits(word));
    for i in 0..5 {
        burst[49 + i] = (st[i * 2] << 1) | st[i * 2 + 1];
        burst[78 + i] = (st[10 + i * 2] << 1) | st[10 + i * 2 + 1];
    }
    burst
}

/// Modulate dibits as flat-held frequency symbols at the true 4800 baud
/// rate. Each symbol lasts `1/4800` s, which is not a whole number of
/// samples at the analog scanner's channel rate (2_048_000/43 Hz).
#[cfg(test)]
pub fn modulate(dibits: &[u8], fs: f64) -> Vec<Complex32> {
    if dibits.is_empty() || fs <= 0.0 {
        return Vec::new();
    }
    let symbol_s = 1.0 / SYMBOL_RATE;
    let n = ((dibits.len() as f64 * symbol_s * fs).round() as usize).max(1);
    let mut phase = 0.0f64;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / fs;
        let sym = ((t / symbol_s) as usize).min(dibits.len() - 1);
        let hz = f64::from(dibit_level(dibits[sym]));
        phase += TAU as f64 * hz / fs;
        out.push(Complex32::new(phase.cos() as f32, phase.sin() as f32));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dmr::CHANNEL_RATE;
    use crate::dmr::voice::{self, VoiceFrame};

    fn voice_frame(seed: u8) -> VoiceFrame {
        VoiceFrame {
            data: [
                seed,
                seed.wrapping_add(1),
                seed.wrapping_add(2),
                0x40,
                0x00,
                0x00,
                0x80,
            ],
            errors: 0,
            valid: true,
        }
    }

    fn one_voice_frame_dibits() -> Vec<u8> {
        let payload = voice::assemble(&[voice_frame(0x10), voice_frame(0x20), voice_frame(0x30)]);
        let burst = encode_voice_burst(&payload, SyncKind::BsVoice);
        let mut d = encode_cach(1).to_vec();
        d.extend_from_slice(&burst);
        d
    }

    #[test]
    fn cach_round_trips_the_timeslot() {
        for slot in [1u8, 2] {
            assert_eq!(decode_cach(&encode_cach(slot)), Some(slot));
        }
    }

    #[test]
    fn the_front_end_recovers_modulated_dibits() {
        let dibits = [0b01, 0b00, 0b10, 0b11, 0b01, 0b11, 0b00, 0b10];
        let iq = modulate(&dibits, CHANNEL_RATE);
        let sps = samples_per_symbol(CHANNEL_RATE);
        let mut front = DmrFrontEnd::new(CHANNEL_RATE, sps);
        let mut hz = Vec::new();
        front.process(&iq, &mut hz);
        let mut got = Vec::new();
        for k in 1..dibits.len() {
            let idx = k * sps;
            if idx < hz.len() {
                got.push(slice_at(hz[idx], DEV_OUTER_HZ));
            }
        }
        assert_eq!(got, dibits[1..].to_vec());
    }

    #[test]
    fn a_clean_voice_burst_locks_and_names_the_slot() {
        let mut stream = Vec::new();
        // A couple of frames so acquisition has a full window and the
        // tracker can step onto the next one.
        for _ in 0..4 {
            stream.extend(one_voice_frame_dibits());
            // The other timeslot: idle-looking data so the stride stays 144.
            stream.extend(encode_cach(2));
            stream.extend(encode_data_burst(1, DT_IDLE, SyncKind::BsData));
        }
        let iq = modulate(&stream, CHANNEL_RATE);
        let mut rx = DmrReceiver::new(CHANNEL_RATE);
        let bursts = rx.process(&iq);
        let voice: Vec<_> = bursts.iter().filter(|b| b.is_voice()).collect();
        assert!(
            !voice.is_empty(),
            "expected a voice burst, got {} bursts (locked={})",
            bursts.len(),
            rx.locked()
        );
        assert_eq!(voice[0].slot, Some(1));
        assert!(matches!(
            voice[0].kind,
            BurstKind::Voice {
                sync: Some(SyncKind::BsVoice),
                ..
            }
        ));
        let payload = voice[0].payload();
        let frames = voice::extract(&payload);
        assert_eq!(frames[0].data[0], 0x10);
        assert_eq!(frames[1].data[0], 0x20);
        assert_eq!(frames[2].data[0], 0x30);
    }

    /// The classifier discriminates the shared filtered buffer once and feeds
    /// every consumer of it from that pass; the pre-discriminated entry point
    /// must decode the same bursts as the inline-discrimination path.
    #[test]
    fn process_hz_matches_process_exactly() {
        let mut stream = Vec::new();
        for _ in 0..4 {
            stream.extend(one_voice_frame_dibits());
            stream.extend(encode_cach(2));
            stream.extend(encode_data_burst(1, DT_IDLE, SyncKind::BsData));
        }
        let iq = modulate(&stream, CHANNEL_RATE);
        let mut direct = DmrReceiver::new(CHANNEL_RATE);
        let mut via_hz = DmrReceiver::new(CHANNEL_RATE);
        let bursts_direct = direct.process(&iq);

        // The same delay-line discrimination the front end performs inline.
        let hz_per_rad = (CHANNEL_RATE / TAU as f64) as f32;
        let mut prev = Complex32::new(0.0, 0.0);
        let mut hz = Vec::new();
        for &x in &iq {
            let d = x * prev.conj();
            prev = x;
            hz.push(if d.norm_sqr() > 0.0 {
                d.arg() * hz_per_rad
            } else {
                0.0
            });
        }
        let bursts_hz = via_hz.process_hz(&hz);

        assert!(!bursts_direct.is_empty(), "baseline decoded nothing");
        assert_eq!(bursts_direct.len(), bursts_hz.len());
        for (a, b) in bursts_direct.iter().zip(&bursts_hz) {
            assert_eq!(a.slot, b.slot);
            assert_eq!(a.source, b.source);
            assert_eq!(a.dibits, b.dibits);
            assert_eq!(format!("{:?}", a.kind), format!("{:?}", b.kind));
        }
    }

    #[test]
    fn a_data_burst_yields_its_colour_code() {
        let mut stream = encode_cach(2).to_vec();
        stream.extend(encode_data_burst(7, DT_TERMINATOR_LC, SyncKind::BsData));
        // Pad so acquisition has a full frame after the sync.
        stream.extend(encode_cach(1));
        stream.extend(encode_data_burst(7, DT_IDLE, SyncKind::BsData));
        stream.extend(encode_cach(2));
        stream.extend(encode_data_burst(7, DT_IDLE, SyncKind::BsData));
        let iq = modulate(&stream, CHANNEL_RATE);
        let mut rx = DmrReceiver::new(CHANNEL_RATE);
        let bursts = rx.process(&iq);
        let data: Vec<_> = bursts
            .iter()
            .filter(|b| matches!(b.kind, BurstKind::Data { .. }))
            .collect();
        assert!(
            data.iter().any(|b| b.color_code() == Some(7)),
            "expected CC7, got {:?}",
            data.iter().map(|b| b.color_code()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn noise_produces_no_bursts() {
        let iq = vec![Complex32::new(0.01, -0.02); 48_000];
        let mut rx = DmrReceiver::new(CHANNEL_RATE);
        assert!(rx.process(&iq).is_empty());
        assert!(!rx.locked());
    }

    /// The analog scanner's 2.048 MS/s decimates by 43, so the channel
    /// rate is not 10.000 samples/symbol. Integer-stride framing used to
    /// lose lock after burst A on this rate.
    const ANALOG_CHAN_HZ: f64 = 2_048_000.0 / 43.0;

    fn repeater_superframes(n: usize, cc: u8) -> Vec<u8> {
        let payload = voice::assemble(&[voice_frame(0x10), voice_frame(0x20), voice_frame(0x30)]);
        let mut d = Vec::new();
        // A couple of idle frames so a long FIR can settle before voice.
        for _ in 0..2 {
            d.extend(encode_cach(1));
            d.extend(encode_data_burst(cc, DT_IDLE, SyncKind::BsData));
            d.extend(encode_cach(2));
            d.extend(encode_data_burst(cc, DT_IDLE, SyncKind::BsData));
        }
        for _ in 0..n {
            d.extend(encode_cach(1));
            d.extend(encode_voice_burst(&payload, SyncKind::BsVoice));
            d.extend(encode_cach(2));
            d.extend(encode_data_burst(cc, DT_IDLE, SyncKind::BsData));
            for _ in 0..5 {
                d.extend(encode_cach(1));
                d.extend(encode_voice_emb_burst(&payload, cc));
                d.extend(encode_cach(2));
                d.extend(encode_data_burst(cc, DT_IDLE, SyncKind::BsData));
            }
        }
        d
    }

    #[test]
    fn analog_channel_rate_keeps_lock_across_a_superframe() {
        assert!(
            (ANALOG_CHAN_HZ / crate::dmr::SYMBOL_RATE - 10.0).abs() > 0.05,
            "this test is only meaningful when sps is not an integer"
        );
        let iq = modulate(&repeater_superframes(2, 10), ANALOG_CHAN_HZ);
        let mut rx = DmrReceiver::new(ANALOG_CHAN_HZ);
        let bursts = rx.process(&iq);
        let voice: Vec<_> = bursts.iter().filter(|b| b.is_voice()).collect();
        let emb = voice
            .iter()
            .filter(|b| matches!(b.kind, BurstKind::Voice { emb: Some(_), .. }))
            .count();
        assert!(
            voice.len() >= 10,
            "expected a full superframe of voice, got {} voice / {} total (locked={})",
            voice.len(),
            bursts.len(),
            rx.locked()
        );
        assert!(
            emb >= 8,
            "bursts B–F must decode as EMB voice, got {emb} EMB of {} voice",
            voice.len()
        );
        assert!(
            voice.iter().any(|b| b.color_code() == Some(10)),
            "expected CC10 from EMB, got {:?}",
            voice.iter().map(|b| b.color_code()).collect::<Vec<_>>()
        );
    }
}
