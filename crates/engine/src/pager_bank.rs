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
/// How long the bank sits on one channel before stepping.
///
/// Arriving mid-frame, the decoder must first wait out the remainder (up to
/// 1.9 s) before the next sync word, then needs 1.76 s of data after it: a
/// full sync-to-pages pass can take ~3.7 s from an unlucky arrival — and a
/// cold-started decoder's references also need the first moments on channel
/// to settle, which eats the first sync after arrival. 5 s covers settle +
/// wait + sync + data with margin; 2 s caught nothing at all, which looked
/// exactly like "decodes nothing".
pub const DWELL_MS: u64 = 5_000;

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
    /// Fresh decode state: timing loops, slicer references and any
    /// partially-captured frame are stale after minutes away.
    fn reset_decoders(&mut self) {
        self.flex = FlexDecoder::new(PAGER_CHANNEL_RATE);
        self.pocsag = [
            PocsagDecoder::new(PAGER_CHANNEL_RATE, 512),
            PocsagDecoder::new(PAGER_CHANNEL_RATE, 1200),
            PocsagDecoder::new(PAGER_CHANNEL_RATE, 2400),
        ];
        self.disc.reset();
    }

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
        // Snap the first channel onto the 25 kHz grid relative to ZERO, not
        // to -half_band: real paging carriers sit on 25 kHz multiples of the
        // tuned frequency (the operator tunes a channel centre). A grid built
        // from -480 kHz stepped +25 lands every channel exactly 5 kHz off
        // every real carrier — POCSAG's brute-force sync shrugged it off while
        // FLEX silently never locked.
        let start = (-(half_band_hz / PAGER_CHANNEL_HZ)).ceil() * PAGER_CHANNEL_HZ;
        let mut off = start;
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
        // Reset the channel we are leaving: its decoder holds half a frame of
        // stale symbol timing that would otherwise greet the next visit (over
        // a minute away) as if no time had passed.
        if let Some(ch) = self.channels.get_mut(self.live) {
            ch.reset_decoders();
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flex::{tests::discriminator_frame_at, A_1600_2};

    /// The whole point of PAGER: a FLEX carrier sitting 50 kHz off the span
    /// centre must survive the NCO mix, box decimation and discriminator and
    /// still decode. This is the path that silently produced zero FLEX pages
    /// on air while PACKET (direct discriminator at 48 kHz) worked.
    #[test]
    fn a_flex_carrier_50khz_off_centre_decodes_through_the_bank() {
        const SPAN: f64 = 2_048_000.0;
        const OFFSET_HZ: f64 = 50_000.0;

        // One frame of 1600/2 audio at its native rate, then FM-modulate it
        // onto an IF of +OFFSET with ±4800 Hz deviation.
        // Two back-to-back frames: on air the decoder always starts cold
        // mid-stream, so the first sync may be eaten by reference settling.
        // The second frame is what a real visit can actually catch.
        let one = discriminator_frame_at(96_000, A_1600_2, 0, 3, 27, 456_789, "BANK TEST");
        let mut disc = one.clone();
        disc.extend(one);
        let fs = SPAN;
        let mut iq = Vec::with_capacity(disc.len() * (SPAN / 96_000.0) as usize);
        let mut phase = 0.0f64;
        for &hz in &disc {
            // Upsample by repetition: the box decimator's response dominates
            // anyway; what matters is the frequency placement.
            let steps = (SPAN / 96_000.0).round() as usize;
            for _ in 0..steps {
                phase += std::f64::consts::TAU * (OFFSET_HZ + f64::from(hz)) / fs;
                if phase >= std::f64::consts::TAU {
                    phase -= std::f64::consts::TAU;
                }
                iq.push(Complex32::new(phase.cos() as f32, phase.sin() as f32));
            }
        }

        let mut bank = PagerBank::new(SPAN, 480_000.0);
        // +50 kHz must land exactly ON a bank channel: the grid is snapped to
        // 25 kHz multiples of zero. This assertion is the regression guard —
        // before the snap, every channel sat 5 kHz off every real carrier.
        let idx = bank
            .channels
            .iter()
            .position(|c| c.offset_hz == OFFSET_HZ)
            .expect("50 kHz must be a bank channel");
        bank.live = idx;

        // Feed the whole frame in one process call (well under one dwell).
        let ms_per_block = (iq.len() as f64 / SPAN * 1000.0) as u64;
        let mut saw_page = false;
        for block in iq.chunks(65_536) {
            let (flex_msgs, _pocsag, _audio) = bank.process(block, ms_per_block);
            if flex_msgs
                .iter()
                .any(|m| m.text == "BANK TEST")
            {
                saw_page = true;
            }
        }
        if !saw_page {
            // Dump what the live channel's discriminator actually produced
            // versus the source waveform, so a failure says WHY.
            let ch = &bank.channels[idx];
            let got = &ch.disc_buf;
            let n = got.len().min(disc.len());
            let (mut sg, mut sr) = (0f64, 0f64);
            for i in 0..n {
                sg += f64::from(disc[i]);
                sr += f64::from(got[i]);
            }
            panic!(
                "no page. src len {} bank len {}; src mean {:.0} Hz, bank mean {:.0} Hz; \
                 first src {:?} first bank {:?}",
                disc.len(), got.len(), sg / n as f64, sr / n as f64,
                &disc[..8.min(disc.len())], &got[..8.min(got.len())]
            );
        }
    }

    /// Pure-tone probe: a carrier at +50k+1000 Hz must read 1000 Hz on the
    /// channel discriminator after the mix and decimation.
    #[test]
    fn channel_discriminator_reports_tone_frequency() {
        const SPAN: f64 = 2_048_000.0;
        let mut bank = PagerBank::new(SPAN, 480_000.0);
        let idx = bank.channels.iter().position(|c| c.offset_hz == 50_000.0).unwrap();
        bank.live = idx;

        // 100 ms of tone at 51 kHz, amplitude 0.5.
        let mut iq = Vec::new();
        let mut ph = 0.0f64;
        for _ in 0..(0.1 * SPAN) as usize {
            ph += std::f64::consts::TAU * 51_000.0 / SPAN;
            if ph >= std::f64::consts::TAU { ph -= std::f64::consts::TAU; }
            iq.push(Complex32::new((ph.cos() * 0.5) as f32, (ph.sin() * 0.5) as f32));
        }
        let mut total = 0usize;
        let mut sum = 0f64;
        for block in iq.chunks(65_536) {
            let (_f, _p, audio) = bank.process(block, 32);
            for &v in audio.iter() {
                sum += f64::from(v);
                total += 1;
            }
        }
        let mean = sum / total.max(1) as f64;
        // An FM discriminator reads a carrier's offset from centre as DC:
        // the tone sits at 51 kHz on a 50 kHz channel, so ~+1000 Hz is right.
        println!("tone probe: n={} mean_hz={:.1}", total, mean);
        assert!(
            (mean - 1000.0).abs() < 150.0,
            "disc read {:.1} Hz for a tone 1 kHz off-channel centre",
            mean
        );
    }
}
