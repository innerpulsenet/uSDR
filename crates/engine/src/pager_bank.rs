//! Wideband paging-channel bank.
//!
//! A pager band is dozens of 25 kHz channels, and FLEX transmits essentially
//! continuously — decoding every channel simultaneously would cost forty
//! decoder banks. This module instead walks one decoder bank across the band:
//! each dwell it mixes one channel out of the recorded span, decimates it to
//! a rate the decoders like, and feeds POCSAG + FLEX. One bank's CPU covers
//! the whole band a page at a time; pages repeat every minute or two, so the
//! sweep loses nothing that matters.
//!
//! Two properties of a real dongle shape the design:
//!
//! * **The decimators land where they land.** The channel rate is
//!   `span_rate / round(span_rate / TARGET)` — exactly 96 kS/s only when the
//!   span divides it. Handing the decoders the nominal rate was a 1.6% clock
//!   error at 2.048 MS/s (15 706 ppm — twenty times what the decoders'
//!   timing loops can pull), which decoded nothing at five of the six spans
//!   the UI offers. Everything downstream is therefore clocked from the rate
//!   the decimator really produces, the same rule `DecodeChain::fs_out`
//!   imposes on the inspect path.
//! * **The LO drifts.** An RTL-SDR at 929 MHz is off by several ppm even
//!   after calibration, and the error moves with temperature, so real
//!   carriers sit up to half a raster away from the 25 kHz grid of the
//!   *actual* oscillator. Drift compensation is deliberately NOT a mix
//!   correction here: FLEX idle frames transmit all-zero codewords, which is
//!   sustained single-sided deviation, so any discriminator-DC estimate is
//!   content-biased by up to full deviation and a loop steered by it walks
//!   off and garbles the very frames it meant to help. Instead the channel
//!   filter is wide enough to pass a half-raster-offset carrier plus full
//!   deviation, and the decoders' own DC trackers — which run during the
//!   alternating (DC-free) sync word and freeze for the frame — centre the
//!   signal where it matters. The sync-time DC is reported back as the
//!   honest carrier-offset measurement, so ppm calibration works in this
//!   mode too.

use crate::flex::FlexDecoder;
use crate::nbfm::NbfmDemod;
use crate::pocsag::PocsagDecoder;
use num_complex::Complex32;
use scannerd_dsp::{DecimFir, Nco};

/// Channel spacing on US paging bands: FLEX uses 25 kHz allocations.
pub const PAGER_CHANNEL_HZ: f64 = 25_000.0;
/// Decoded sample rate per channel. FLEX 6400 sym/s needs >4 samples/symbol
/// for the timing loop to bite; 96 kS/s gives 15.
pub const PAGER_CHANNEL_RATE: f64 = 96_000.0;
/// Full-scale reference of the channel discriminator, in Hz. Scope traces
/// and level marks are normalised against this.
pub const PAGER_DEVIATION_HZ: f32 = 8_000.0;
/// Half-width of the per-channel filter, in Hz.
///
/// The lower bound is what a drifted carrier needs: up to half the 25 kHz
/// raster of offset (a carrier between two grid channels) plus full ±4.8 kHz
/// deviation while the decoders' DC reference is still converging. The upper
/// bound is the adjacent channel: its carrier centre must land in the
/// stopband, because the box decimator alone passes it at only −1 dB and the
/// discriminator then captures whichever neighbour is strongest.
const CHANNEL_FILTER_HZ: f32 = 20_000.0;
/// How long FLEX sync evidence mutes the channel's POCSAG decoders: a bit
/// more than one FLEX frame period, so a continuously transmitting channel
/// stays muted while it is on, and a quiet one unmutes within a frame.
const POCSAG_MUTE_MS: u64 = 4_000;
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
    /// Mix-down oscillator. `set_freq(offset_hz, span_rate)` reproduces the
    /// phasor `e^{-j·2π·offset·n/span_rate}` this used to evaluate with a
    /// `sin_cos` per span-rate sample — a transcendental per sample of the
    /// whole 2 MS/s span, which is the budget, not a rounding error in it.
    /// The recurrence keeps phase across calls and renormalises itself.
    nco: Nco,
    /// Span-rate mix output, reused per block.
    mixed: Vec<Complex32>,
    /// CIC-style running decimator: box-average over `every` samples.
    acc_re: f32,
    acc_im: f32,
    count: usize,
    every: usize,
    /// Sample rate this channel really delivers: `span_rate / every`.
    pub rate: f64,
    pub flex: FlexDecoder,
    pub pocsag: [PocsagDecoder; 3],
    disc: NbfmDemod,
    /// Channel selectivity the box average does not provide: a complex
    /// low-pass at the decimated rate, so the adjacent 25 kHz carrier (which
    /// the box passes at −1 dB) is actually stopped before the discriminator
    /// can capture it.
    fir: DecimFir,
    filtered: Vec<Complex32>,
    /// Decimated channel IQ accumulated across the caller's block.
    chan_iq: Vec<Complex32>,
    disc_buf: Vec<f32>,
    /// Milliseconds of POCSAG muting left. While the channel's FLEX decoder
    /// is producing syncs, any POCSAG "page" from the same discriminator is a
    /// false positive: a channel carries one protocol, and POCSAG's 32-bit
    /// hunt with a two-bit budget plus a BCH that happily corrects random
    /// debris is exactly the machinery that invents garbled capcodes out of
    /// FLEX data.
    pocsag_mute_ms: u64,
}

impl PagerChannel {
    /// Fresh decode state: timing loops, slicer references and any
    /// partially-captured frame are stale after minutes away.
    fn reset_decoders(&mut self) {
        self.flex = FlexDecoder::new(self.rate);
        self.pocsag = [
            PocsagDecoder::new(self.rate, 512),
            PocsagDecoder::new(self.rate, 1200),
            PocsagDecoder::new(self.rate, 2400),
        ];
        self.disc.reset();
        self.pocsag_mute_ms = 0;
    }

    fn new(offset_hz: f64, span_rate: f64) -> Self {
        let every = (span_rate / PAGER_CHANNEL_RATE).round().max(1.0) as usize;
        // The rate the decimator will really deliver. Everything downstream
        // is clocked from this; the nominal 96 kHz is only a design target.
        let rate = span_rate / every as f64;
        Self {
            offset_hz,
            // Mix DOWN by the offset: the channel sits at +offset in the span.
            nco: {
                let mut nco = Nco::new();
                nco.set_freq(offset_hz, span_rate);
                nco
            },
            mixed: Vec::new(),
            acc_re: 0.0,
            acc_im: 0.0,
            count: 0,
            every,
            rate,
            flex: FlexDecoder::new(rate),
            pocsag: [
                PocsagDecoder::new(rate, 512),
                PocsagDecoder::new(rate, 1200),
                PocsagDecoder::new(rate, 2400),
            ],
            disc: NbfmDemod::with_deviation(rate, PAGER_DEVIATION_HZ),
            fir: DecimFir::new(
                CHANNEL_FILTER_HZ.min(rate as f32 * 0.45),
                rate as f32,
                1,
                255,
            ),
            filtered: Vec::new(),
            chan_iq: Vec::new(),
            disc_buf: Vec::new(),
            pocsag_mute_ms: 0,
        }
    }

    /// Feed one span block; returns decoded flex messages this dwell.
    fn process(
        &mut self,
        iq: &[Complex32],
        ms_elapsed: u64,
        out_flex: &mut Vec<crate::flex::FlexMessage>,
        out_pocsag: &mut Vec<crate::pocsag::PocsagMessage>,
        out_audio: &mut Vec<f32>,
    ) {
        self.nco.mix(iq, &mut self.mixed);
        for &m in &self.mixed {
            self.acc_re += m.re;
            self.acc_im += m.im;
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
            self.fir.process(&self.chan_iq, &mut self.filtered);
            self.chan_iq.clear();
            self.disc_buf.clear();
            self.disc.discriminator_hz(&self.filtered, &mut self.disc_buf);
            out_audio.extend_from_slice(&self.disc_buf);
            let flex_syncs_before = {
                let d = self.flex.diagnostics();
                d.syncs_1600 + d.syncs_3200 + d.syncs_6400
            };
            for msg in self.flex.process(&self.disc_buf) {
                out_flex.push(msg);
            }
            let flex_syncs_after = {
                let d = self.flex.diagnostics();
                d.syncs_1600 + d.syncs_3200 + d.syncs_6400
            };
            if flex_syncs_after > flex_syncs_before {
                self.pocsag_mute_ms = POCSAG_MUTE_MS;
                // Anything accumulated before protocol ownership was known
                // came from FLEX, not POCSAG. Clear both partial batches and
                // their diagnostics so they cannot mature into a phantom
                // page after the mute expires.
                self.pocsag = [
                    PocsagDecoder::new(self.rate, 512),
                    PocsagDecoder::new(self.rate, 1200),
                    PocsagDecoder::new(self.rate, 2400),
                ];
            } else {
                self.pocsag_mute_ms = self.pocsag_mute_ms.saturating_sub(ms_elapsed);
            }
            if self.pocsag_mute_ms == 0 {
                for dec in &mut self.pocsag {
                    for msg in dec.process(&self.disc_buf) {
                        out_pocsag.push(msg);
                    }
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
    /// Dwell order: centre of the band first, then alternating outward. The
    /// operator tunes to a signal before switching to PAGER; a sweep that
    /// started at the band edge kept them waiting ~95 s (39 channels × 5 s
    /// dwell divided by two) for the one channel they actually pointed at,
    /// which reads exactly like "decodes nothing".
    order: Vec<usize>,
    order_pos: usize,
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
        // Walk centre-out: the tuned frequency first, then its neighbours,
        // then the rest of the band.
        let mut order = Vec::with_capacity(channels.len());
        if let Some(centre) = channels
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.offset_hz.abs().total_cmp(&b.offset_hz.abs()))
            .map(|(index, _)| index)
        {
            let len = channels.len() as isize;
            let mut low = centre as isize;
            let mut high = centre as isize + 1;
            while order.len() < channels.len() {
                if high < len && (order.len() % 2 == 1 || low < 0) {
                    order.push(high as usize);
                    high += 1;
                } else if low >= 0 {
                    order.push(low as usize);
                    low -= 1;
                } else if high < len {
                    order.push(high as usize);
                    high += 1;
                } else {
                    break;
                }
            }
        }
        let live = order.first().copied().unwrap_or(0);
        Self {
            channels,
            live,
            since_step_ms: 0,
            order,
            order_pos: 0,
        }
    }

    /// The sample rate the live channel really delivers — `span_rate /
    /// round(span_rate / TARGET)`, not the nominal target. Scope traces,
    /// monitor audio and diagnostics must all be labelled with this.
    pub fn channel_rate(&self) -> f64 {
        self.channels
            .first()
            .map(|c| c.rate)
            .unwrap_or(PAGER_CHANNEL_RATE)
    }

    /// The live channel's carrier offset from where the receiver put it, as
    /// measured by its own decoders during the last sync word — the honest
    /// drift figure for a ppm calibration. FLEX reports its sync-time DC
    /// reference; POCSAG lanes report theirs (measured over the alternating
    /// preamble, which is even cleaner). `None` until something has synced.
    pub fn carrier_offset_hz(&self) -> Option<f64> {
        let ch = self.channels.get(self.live)?;
        if let Some(hz) = ch.flex.diagnostics().last_carrier_offset_hz {
            return Some(f64::from(hz));
        }
        ch.pocsag
            .iter()
            .find_map(|d| d.diagnostics().last_carrier_offset_hz)
            .map(f64::from)
    }

    /// Whether the current dwell has run past `ms`.
    pub fn since_step_exceeds_ms(&self, ms: u64) -> bool {
        self.since_step_ms >= ms
    }

    /// Step to the next channel in the centre-out walk. Call on dwell expiry.
    pub fn step(&mut self) {
        // Reset the channel we are leaving: its decoder holds half a frame of
        // stale symbol timing that would otherwise greet the next visit (over
        // a minute away) as if no time had passed.
        if let Some(ch) = self.channels.get_mut(self.live) {
            ch.reset_decoders();
        }
        if !self.order.is_empty() {
            self.order_pos = (self.order_pos + 1) % self.order.len();
            self.live = self.order[self.order_pos];
        }
        self.since_step_ms = 0;
    }

    pub fn live_offset_hz(&self) -> f64 {
        self.channels.get(self.live).map(|c| c.offset_hz).unwrap_or(0.0)
    }

    /// Index of the live channel, and how many there are — for the sweep
    /// position readout.
    pub fn live_index(&self) -> usize {
        self.live
    }

    pub fn len(&self) -> usize {
        self.channels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
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
            ch.process(iq, ms_elapsed, &mut flex, &mut pocsag, &mut live_audio);
        }
        self.since_step_ms += ms_elapsed;
        (flex, pocsag, live_audio)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flex::{tests::discriminator_frame_at, A_1600_2};

    /// FM-modulate a discriminator-hertz signal onto a carrier at span rate.
    ///
    /// The discriminator samples were rendered at `audio_rate`; this resamples
    /// them to the span rate *by real time* (linear interpolation), which is
    /// what a real transmitter and a real sampling clock produce. Repeating
    /// each audio sample `span/audio_rate` times — the old shortcut — makes
    /// the bank's decimation land back on the nominal rate no matter what the
    /// decoders were told, hiding exactly the clock error this test exists to
    /// catch.
    fn fm_at_span(
        disc: &[f32],
        audio_rate: f64,
        span: f64,
        carrier_offset: f64,
        amp: f32,
        phase: &mut f64,
    ) -> Vec<Complex32> {
        let n = (disc.len() as f64 * span / audio_rate).ceil() as usize;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 * audio_rate / span;
            let i0 = t.floor() as usize;
            let frac = (t - t.floor()) as f32;
            let hz = disc[i0.min(disc.len() - 1)]
                + frac * (disc[(i0 + 1).min(disc.len() - 1)] - disc[i0.min(disc.len() - 1)]);
            *phase += std::f64::consts::TAU * (carrier_offset + f64::from(hz)) / span;
            if *phase >= std::f64::consts::TAU {
                *phase -= std::f64::consts::TAU;
            }
            out.push(Complex32::new(
                (*phase).cos() as f32 * amp,
                (*phase).sin() as f32 * amp,
            ));
        }
        out
    }

    /// Back-to-back frames are the honest arrival: on air the decoder always
    /// starts cold mid-stream, the channel filter's startup transient eats
    /// the first sync word it lands on, and the DC reference needs the idle
    /// content of a whole frame to settle. The second frame is the first one
    /// a real visit can catch; the third gives its 1.76 s data section room
    /// to finish inside the test's finite stream.
    fn frames(text: &str, count: usize) -> Vec<f32> {
        let one = discriminator_frame_at(96_000, A_1600_2, 0, 3, 27, 456_789, text);
        let mut disc = Vec::with_capacity(one.len() * count);
        for _ in 0..count {
            disc.extend_from_slice(&one);
        }
        disc
    }

    /// Same, with the capcode chosen per caller so two concurrent
    /// transmissions can be told apart in the assertion.
    fn frames_named(text: &str, capcode: u32, count: usize) -> Vec<f32> {
        let one = discriminator_frame_at(96_000, A_1600_2, 0, 3, 27, capcode, text);
        let mut disc = Vec::with_capacity(one.len() * count);
        for _ in 0..count {
            disc.extend_from_slice(&one);
        }
        disc
    }

    /// Decode every page the bank produces while `iq` is fed through it, with
    /// the bank parked on the channel at `offset_hz`.
    fn decode_at(bank: &mut PagerBank, iq: &[Complex32], span: f64, offset_hz: f64) -> Vec<String> {
        let idx = bank
            .channels
            .iter()
            .position(|c| c.offset_hz == offset_hz)
            .expect("offset must be a bank channel");
        bank.live = idx;
        let ms_per_block = (65_536.0 / span * 1000.0) as u64;
        let mut texts = Vec::new();
        for block in iq.chunks(65_536) {
            let (flex_msgs, _pocsag, _audio) = bank.process(block, ms_per_block);
            texts.extend(flex_msgs.into_iter().map(|m| m.text));
        }
        texts
    }

    /// The whole point of PAGER: a FLEX carrier sitting 50 kHz off the span
    /// centre must survive the NCO mix, decimation, channel filter and
    /// discriminator and still decode. Modulated honestly at the span rate,
    /// so a decoder clocked at the nominal 96 kHz while the decimator
    /// delivers `span/every` cannot decode it — which is what sat on air for
    /// every span but 1.536 MHz.
    #[test]
    fn a_flex_carrier_50khz_off_centre_decodes_through_the_bank() {
        const SPAN: f64 = 2_048_000.0;
        const OFFSET_HZ: f64 = 50_000.0;

        let mut phase = 0.0f64;
        let iq = fm_at_span(&frames("BANK TEST", 3), 96_000.0, SPAN, OFFSET_HZ, 1.0, &mut phase);

        let mut bank = PagerBank::new(SPAN, 480_000.0);
        // The honest modulator puts symbols where the TRUE channel rate says
        // they are; a bank that clocks its decoders at the nominal 96 kHz is
        // 15 706 ppm deaf to them.
        assert!(
            (bank.channel_rate() - 97_523.809_5).abs() < 0.1,
            "channel rate {} should be span/21",
            bank.channel_rate()
        );
        let texts = decode_at(&mut bank, &iq, SPAN, OFFSET_HZ);
        assert!(
            texts.iter().any(|t| t == "BANK TEST"),
            "no page through the bank; texts={texts:?}"
        );
    }

    /// Every span the UI offers must clock its decoders at the rate its own
    /// decimator produces. The nominal 96 kHz is exact for one span in six;
    /// for the rest it is 1-11% wrong, which the decoders' timing loops
    /// (±800 ppm) cannot even begin to pull.
    #[test]
    fn every_span_clocks_its_decoders_at_the_rate_it_really_produces() {
        for span in [
            2_048_000.0f64,
            1_800_000.0,
            1_536_000.0,
            1_024_000.0,
            512_000.0,
            256_000.0,
        ] {
            let bank = PagerBank::new(span, 480_000.0);
            let every = (span / PAGER_CHANNEL_RATE).round();
            assert_eq!(bank.channel_rate(), span / every, "span {span}");
        }
        let awkward = PagerBank::new(2_048_000.0, 480_000.0);
        assert!((awkward.channel_rate() - PAGER_CHANNEL_RATE).abs() > 1000.0);
        let exact = PagerBank::new(1_536_000.0, 480_000.0);
        assert!((exact.channel_rate() - PAGER_CHANNEL_RATE).abs() < 1e-6);
    }

    /// A drifted carrier — off the 25 kHz grid by a few kHz, the normal
    /// state of an RTL-SDR at 929 MHz — must decode, and the bank must be
    /// able to report the drift it rode. The decoders' own DC references do
    /// the centring: they run during the alternating sync word and freeze
    /// for the frame, so content bias cannot poison them.
    #[test]
    fn a_drifted_off_grid_carrier_decodes_and_reports_its_offset() {
        const SPAN: f64 = 2_048_000.0;
        const OFFSET_HZ: f64 = 50_000.0;
        for drift in [2_000.0f64, 4_000.0, 12_500.0] {
            let mut phase = 0.0f64;
            let iq = fm_at_span(
                &frames("DRIFT TEST", 3),
                96_000.0,
                SPAN,
                OFFSET_HZ + drift,
                1.0,
                &mut phase,
            );
            let mut bank = PagerBank::new(SPAN, 480_000.0);
            let texts = decode_at(&mut bank, &iq, SPAN, OFFSET_HZ);
            assert!(
                texts.iter().any(|t| t == "DRIFT TEST"),
                "carrier drifted {drift} Hz off the grid did not decode; texts={texts:?}"
            );
            let measured = bank
                .carrier_offset_hz()
                .unwrap_or(f64::NAN)
                .round();
            assert!(
                (measured - drift).abs() < 1_500.0,
                "carrier offset reported as {measured:.0} Hz, expected near {drift}"
            );
        }
    }

    /// The channel filter is the whole reason a dwell on a quiet channel
    /// next to a busy one stays quiet: without it the box decimator passes
    /// the adjacent 25 kHz carrier at −1 dB and the discriminator captures
    /// whichever neighbour is strongest. A weaker carrier carrying OUR page
    /// must still decode while a stronger neighbour transmits on the next
    /// channel over.
    #[test]
    fn a_strong_adjacent_carrier_does_not_capture_the_live_channel() {
        const SPAN: f64 = 2_048_000.0;
        const OFFSET_HZ: f64 = 50_000.0;

        let ours = frames_named("WANTED PAGE", 100_001, 3);
        let theirs = frames_named("NEIGHBOUR", 100_002, 3);
        let mut phase = 0.0f64;
        let mut neighbour_phase = 0.0f64;
        let a = fm_at_span(&ours, 96_000.0, SPAN, OFFSET_HZ, 0.35, &mut phase);
        let b = fm_at_span(&theirs, 96_000.0, SPAN, OFFSET_HZ + 25_000.0, 1.0, &mut neighbour_phase);
        let iq: Vec<Complex32> = a.iter().zip(b.iter()).map(|(x, y)| x + y).collect();

        let mut bank = PagerBank::new(SPAN, 480_000.0);
        let texts = decode_at(&mut bank, &iq, SPAN, OFFSET_HZ);
        assert!(
            texts.iter().any(|t| t == "WANTED PAGE"),
            "the weaker on-channel carrier lost to the adjacent one; texts={texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t == "NEIGHBOUR"),
            "the adjacent channel leaked through the channel filter"
        );
    }

    /// The sweep starts on the tuned channel and walks outward, so pointing
    /// at a signal and switching to PAGER decodes it on the first dwell
    /// instead of ~95 s later.
    #[test]
    fn the_sweep_starts_centre_and_walks_outward() {
        let mut bank = PagerBank::new(2_048_000.0, 480_000.0);
        assert_eq!(bank.live_offset_hz(), 0.0, "first dwell must be the tune");
        let mut offs = vec![bank.live_offset_hz()];
        for _ in 1..bank.len() {
            bank.step();
            offs.push(bank.live_offset_hz());
        }
        // Every channel visited exactly once...
        assert_eq!(offs.len(), bank.len());
        let mut sorted = offs.clone();
        sorted.sort_by(f64::total_cmp);
        let mut deduped = sorted.clone();
        deduped.dedup();
        assert_eq!(sorted, deduped, "a channel was visited twice: {offs:?}");
        // ...and the walk never leaves a visited channel further out than an
        // unvisited one closer in: |offset| must be non-decreasing over any
        // prefix that still has inward neighbours unvisited. Simpler check:
        // the first five dwells are 0, ±25, ±50 kHz in some order.
        let mut first5: Vec<f64> = offs[..5].to_vec();
        first5.sort_by(f64::total_cmp);
        assert_eq!(first5, [-50_000.0, -25_000.0, 0.0, 25_000.0, 50_000.0]);
    }

    /// All four FLEX air modes must survive the NCO mix, decimation, channel
    /// filter and discriminator — the 4-level modes are the fragile ones on
    /// air, and a bank regression that only ever tests 1600/2 would not see
    /// them break.
    #[test]
    fn every_flex_air_mode_decodes_through_the_bank() {
        const SPAN: f64 = 2_048_000.0;
        const OFFSET_HZ: f64 = 50_000.0;
        for (code, phase) in [
            (crate::flex::A_1600_2, 0usize),
            (crate::flex::A_1600_4, 2),
            (crate::flex::A_3200_2, 2),
            (crate::flex::A_3200_4, 3),
        ] {
            let one = discriminator_frame_at(96_000, code, phase, 3, 27, 456_789, "ALL MODES");
            let mut disc = Vec::with_capacity(one.len() * 3);
            for _ in 0..3 {
                disc.extend_from_slice(&one);
            }
            let mut ph = 0.0f64;
            let iq = fm_at_span(&disc, 96_000.0, SPAN, OFFSET_HZ, 1.0, &mut ph);
            let mut bank = PagerBank::new(SPAN, 480_000.0);
            let texts = decode_at(&mut bank, &iq, SPAN, OFFSET_HZ);
            assert!(
                texts.iter().any(|t| t == "ALL MODES"),
                "mode {code:#06x} phase {phase} did not decode; texts={texts:?}"
            );
        }
    }

    /// While the channel's FLEX decoder is producing syncs, its POCSAG lanes
    /// must not even be fed: whatever they slice out of FLEX traffic is
    /// invention, and feeding them costs CPU besides.
    #[test]
    fn flex_syncs_mute_the_channel_pocsag_lanes() {
        const SPAN: f64 = 2_048_000.0;
        const OFFSET_HZ: f64 = 50_000.0;
        let one = discriminator_frame_at(96_000, A_1600_2, 0, 3, 27, 456_789, "MUTE TEST");
        let mut disc = Vec::with_capacity(one.len() * 3);
        for _ in 0..3 {
            disc.extend_from_slice(&one);
        }
        let mut ph = 0.0f64;
        let iq = fm_at_span(&disc, 96_000.0, SPAN, OFFSET_HZ, 1.0, &mut ph);

        let mut bank = PagerBank::new(SPAN, 480_000.0);
        let idx = bank
            .channels
            .iter()
            .position(|c| c.offset_hz == OFFSET_HZ)
            .unwrap();
        bank.live = idx;
        let ms_per_block = (65_536.0 / SPAN * 1000.0) as u64;
        let mut saw_page = false;
        for block in iq.chunks(65_536) {
            let (flex_msgs, _pocsag, _audio) = bank.process(block, ms_per_block);
            if flex_msgs.iter().any(|m| m.text == "MUTE TEST") {
                saw_page = true;
            }
        }
        assert!(saw_page, "FLEX page did not decode");
        let ch = &bank.channels[idx];
        let fed = ch
            .pocsag
            .iter()
            .map(|d| {
                let diag = d.diagnostics();
                diag.bits_512 + diag.bits_1200 + diag.bits_2400
            })
            .sum::<u64>();
        assert_eq!(
            fed, 0,
            "POCSAG lanes were fed while FLEX owned the channel ({fed} bits)"
        );
    }

    /// Pure-tone probe: a carrier at +50k+1000 Hz must read 1000 Hz on the
    /// channel discriminator after the mix, decimation and filter.
    #[test]
    fn channel_discriminator_reports_tone_frequency() {
        const SPAN: f64 = 2_048_000.0;
        let mut bank = PagerBank::new(SPAN, 480_000.0);
        let idx = bank
            .channels
            .iter()
            .position(|c| c.offset_hz == 50_000.0)
            .unwrap();
        bank.live = idx;

        // 100 ms of tone at 51 kHz, amplitude 0.5.
        let mut iq = Vec::new();
        let mut ph = 0.0f64;
        for _ in 0..(0.1 * SPAN) as usize {
            ph += std::f64::consts::TAU * 51_000.0 / SPAN;
            if ph >= std::f64::consts::TAU {
                ph -= std::f64::consts::TAU;
            }
            iq.push(Complex32::new(
                (ph.cos() * 0.5) as f32,
                (ph.sin() * 0.5) as f32,
            ));
        }
        let mut first_mean: Option<f64> = None;
        for block in iq.chunks(65_536) {
            let (_f, _p, audio) = bank.process(block, 32);
            if first_mean.is_none() && !audio.is_empty() {
                first_mean = Some(audio.iter().sum::<f32>() as f64 / audio.len() as f64);
            }
        }
        // An FM discriminator reads a carrier's offset from centre as DC:
        // the tone sits at 51 kHz on a 50 kHz channel, so ~+1000 Hz is right.
        let mean = first_mean.unwrap_or(0.0);
        assert!(
            (mean - 1000.0).abs() < 150.0,
            "disc read {:.1} Hz for a tone 1 kHz off-channel centre",
            mean
        );
    }
}
