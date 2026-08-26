//! Wideband paging-channel bank.
//!
//! A pager band is dozens of 25 kHz channels, and FLEX transmits essentially
//! continuously — decoding every channel simultaneously would cost forty
//! decoder banks. This module instead walks one decoder bank across the band:
//! each dwell it mixes one channel out of the recorded span, decimates it to
//! a rate the decoders like, and feeds POCSAG + FLEX. One bank's CPU covers
//! the whole band a page at a time; pages repeat every minute or two, so the
//! sweep loses nothing that matters.

use crate::flex::FlexDecoder;
use crate::nbfm::NbfmDemod;
use crate::pocsag::PocsagDecoder;
use num_complex::Complex32;

/// Channel spacing on US paging bands: FLEX uses 25 kHz allocations.
pub const PAGER_CHANNEL_HZ: f64 = 25_000.0;
/// Decoded sample rate per channel. FLEX 6400 sym/s needs >4 samples/symbol
/// for the timing loop to bite; 96 kS/s gives 15.
pub const PAGER_CHANNEL_RATE: f64 = 96_000.0;
/// How long the bank sits on one channel before stepping. One FLEX frame
/// (1.88 s) plus margin — long enough to catch a sync word, short enough to
/// cover 40 channels inside a typical page-repeat interval.
pub const DWELL_MS: u64 = 2_000;

pub struct PagerChannel {
    /// Offset from the span centre, hertz.
    pub offset_hz: f64,
    nco_phase: f64,
    nco_inc: f64,
    /// CIC-style running decimator: box-average over `every` samples.
    acc_re: f32,
    acc_im: f32,
    count: usize,
    every: usize,
    pub flex: FlexDecoder,
    pub pocsag: [PocsagDecoder; 3],
    disc: NbfmDemod,
    /// Decimated channel IQ accumulated across the caller's block.
    chan_iq: Vec<Complex32>,
    disc_buf: Vec<f32>,
}

impl PagerChannel {
    fn new(offset_hz: f64, span_rate: f64) -> Self {
        let every = (span_rate / PAGER_CHANNEL_RATE).round().max(1.0) as usize;
        Self {
            offset_hz,
            // Mix DOWN by the offset: the channel sits at +offset in the span.
            nco_phase: 0.0,
            nco_inc: -std::f64::consts::TAU * offset_hz / span_rate,
            acc_re: 0.0,
            acc_im: 0.0,
            count: 0,
            every,
            flex: FlexDecoder::new(PAGER_CHANNEL_RATE),
            pocsag: [
                PocsagDecoder::new(PAGER_CHANNEL_RATE, 512),
                PocsagDecoder::new(PAGER_CHANNEL_RATE, 1200),
                PocsagDecoder::new(PAGER_CHANNEL_RATE, 2400),
            ],
            disc: NbfmDemod::with_deviation(PAGER_CHANNEL_RATE, 8_000.0),
            chan_iq: Vec::new(),
            disc_buf: Vec::new(),
        }
    }

    /// Feed one span block; returns decoded flex messages this dwell.
    fn process(
        &mut self,
        iq: &[Complex32],
        out_flex: &mut Vec<crate::flex::FlexMessage>,
        out_pocsag: &mut Vec<crate::pocsag::PocsagMessage>,
        out_audio: &mut Vec<f32>,
    ) {
        for &x in iq {
            let (s, c) = self.nco_phase.sin_cos();
            let mixed = Complex32::new(
                x.re * c as f32 - x.im * s as f32,
                x.re * s as f32 + x.im * c as f32,
            );
            self.nco_phase += self.nco_inc;
            if self.nco_phase >= std::f64::consts::TAU {
                self.nco_phase -= std::f64::consts::TAU;
            } else if self.nco_phase < 0.0 {
                self.nco_phase += std::f64::consts::TAU;
            }
            self.acc_re += mixed.re;
            self.acc_im += mixed.im;
            self.count += 1;
            if self.count == self.every {
                let d = self.every as f32;
                self.chan_iq.push(Complex32::new(self.acc_re / d, self.acc_im / d));
                self.acc_re = 0.0;
                self.acc_im = 0.0;
                self.count = 0;
            }
        }
        // One discriminator pass and one decoder pass per input block.
        if !self.chan_iq.is_empty() {
            self.disc_buf.clear();
            self.disc.discriminator_hz(&self.chan_iq, &mut self.disc_buf);
            self.chan_iq.clear();
            out_audio.extend_from_slice(&self.disc_buf);
            for msg in self.flex.process(&self.disc_buf) {
                out_flex.push(msg);
            }
            for dec in &mut self.pocsag {
                for msg in dec.process(&self.disc_buf) {
                    out_pocsag.push(msg);
                }
            }
        }
    }
}

/// The walking bank: channels across the band plus which one is live.
pub struct PagerBank {
    pub channels: Vec<PagerChannel>,
    live: usize,
    since_step_ms: u64,
}

impl PagerBank {
    /// Channels at 25 kHz spacing covering +/-`half_span_hz` around the tuned
    /// frequency, filtered to those actually inside the recorded span.
    pub fn new(span_rate: f64, half_band_hz: f64) -> Self {
        let mut channels = Vec::new();
        let mut off = -half_band_hz;
        while off <= half_band_hz {
            // Keep only channels fully inside the anti-aliased span.
            if off.abs() + PAGER_CHANNEL_HZ < span_rate * 0.45 {
                channels.push(PagerChannel::new(off, span_rate));
            }
            off += PAGER_CHANNEL_HZ;
        }
        Self {
            channels,
            live: 0,
            since_step_ms: 0,
        }
    }

    /// Whether the current dwell has run past `ms`.
    pub fn since_step_exceeds_ms(&self, ms: u64) -> bool {
        self.since_step_ms >= ms
    }

    /// Step to the next channel. Call on dwell expiry.
    pub fn step(&mut self) {
        if !self.channels.is_empty() {
            self.live = (self.live + 1) % self.channels.len();
        }
        self.since_step_ms = 0;
    }

    pub fn live_offset_hz(&self) -> f64 {
        self.channels.get(self.live).map(|c| c.offset_hz).unwrap_or(0.0)
    }

    /// Feed one span-rate block into the live channel. Returns decoded pages
    /// plus the live channel's discriminator audio, so a caller can monitor
    /// exactly what the decoders are hearing.
    pub fn process(
        &mut self,
        iq: &[Complex32],
        ms_elapsed: u64,
    ) -> (
        Vec<crate::flex::FlexMessage>,
        Vec<crate::pocsag::PocsagMessage>,
        Vec<f32>,
    ) {
        let mut flex = Vec::new();
        let mut pocsag = Vec::new();
        let mut live_audio = Vec::new();
        if let Some(ch) = self.channels.get_mut(self.live) {
            ch.process(iq, &mut flex, &mut pocsag, &mut live_audio);
        }
        self.since_step_ms += ms_elapsed;
        (flex, pocsag, live_audio)
    }
}
