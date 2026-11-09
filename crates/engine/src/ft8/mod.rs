//! FT8 and FT4: the weak-signal HF data modes, decoded natively.
//!
//! Both send a 77-bit message, protected by a 14-bit CRC and a (174, 91)
//! LDPC code, as Gaussian-shaped FSK with Costas-array syncs: FT8 as 79
//! symbols of 8 tones 6.25 Hz apart in a 15 s slot, FT4 as 105 symbols of
//! 4 tones 20.8 Hz apart in a 7.5 s slot. Slots start on the UTC clock, so
//! everything heard in one is decoded together once it ends.
//!
//! The protocol tables and the overall structure (oversampled waterfall,
//! sync search, max-log soft bits, belief propagation) follow ft8_lib by
//! Karlis Goba (MIT) and the WSJT-X protocol description by Franke, Somerville
//! and Taylor.

mod deep;
pub(crate) mod demod;
pub(crate) mod ldpc;
mod osd;
mod pack;
mod stream;
mod subtract;
mod tables;

pub use pack::CallHashes;
pub use stream::SlotDecoder;

/// The rate everything inside the decoder runs at.
pub const RATE_HZ: f32 = 12_000.0;

/// Audio searched for signals unless the caller says otherwise
/// ([`decode_slot_in`]), and the furthest a search may reach: 5 kHz is the
/// widest sideband filter, under the 6 kHz Nyquist of the decoder's rate.
pub const F_MIN_HZ: f32 = 100.0;
pub const F_MAX_HZ: f32 = 3_000.0;
pub const F_LIMIT_HZ: f32 = 5_000.0;
/// Sync candidates tried per slot, and the weakest sync worth trying, in
/// half-dB of contrast against the neighboring bins.
const MAX_CANDIDATES: usize = 300;
const MIN_SCORE: f32 = 6.0;
const LDPC_ITERATIONS: usize = 50;
/// The variance soft bits are scaled to (ft8_lib's figure).
const LLR_SPREAD: f32 = 24.0;
/// Candidates each search's second, finer pass may try, strongest first.
const DEEP_CANDIDATES: usize = 120;
/// The time a decode's finer passes may take when nobody says otherwise:
/// the examples and tests. The live decoders are given theirs by the pool
/// that runs them ([`stream`]), from how much of the slot is left.
const DEEP_BUDGET: std::time::Duration = std::time::Duration::from_millis(2_500);
/// Searches of one slot, each after the last one's decodes are taken out.
/// On 147 slots recorded off 20, 17 and 15 m (`examples/ft8_corpus`): two
/// searches with propagation alone decoded 3712; with the aided passes,
/// three searches 4325 and four 4394, and nothing at all with the same
/// slots played backwards. The soft bits' scale made no difference between
/// 14 and 40.
const SEARCHES: usize = 4;
/// The share of the budget by which each search's finer pass must be done.
/// One deadline for the lot let the first search spend it all, and the
/// search after subtraction, where the weak signals have just been
/// uncovered, got none on exactly the busy slots that needed it.
const SEARCH_SHARE: [f32; SEARCHES] = [0.3, 0.6, 0.85, 1.0];

/// How hard a slot is searched. The default is what the live decoders run;
/// [`Options::plain`] is the decoder before the aided passes, for comparing.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Time for the finer passes of every search together.
    pub budget: std::time::Duration,
    /// Searches, each after the last one's decodes are subtracted.
    pub searches: usize,
    /// Each search has its own share of the budget.
    pub split_budget: bool,
    /// Leave out sync candidates that repeat a stronger one. Off: on 147
    /// slots recorded off 20, 17 and 15 m it decoded 1% fewer (4348 against
    /// 4394). The repeats are the same signal at other alignments of the
    /// oversampled grid, and one of them often decodes where the
    /// best-scoring one fails.
    pub distinct: bool,
    /// Try failed candidates as CQs, with a CQ's fixed bits given.
    pub ap_cq: bool,
    /// Run the ordered-statistics decoder where propagation fails.
    pub osd: bool,
    pub osd_max_checks: usize,
    pub osd_max_hard: usize,
    /// The variance the soft bits are scaled to before decoding: how sure
    /// of itself the decoder takes the demodulator to be.
    pub llr_spread: f32,
}

impl Default for Options {
    fn default() -> Self {
        Options { budget: DEEP_BUDGET, searches: SEARCHES, split_budget: true, distinct: false, ap_cq: true, osd: true, osd_max_checks: 40, osd_max_hard: 30, llr_spread: LLR_SPREAD }
    }
}

impl Options {
    /// Two searches inside one 600 ms deadline, propagation only.
    pub fn plain() -> Self {
        Options { budget: std::time::Duration::from_millis(600), searches: 2, split_budget: false, distinct: false, ap_cq: false, osd: false, osd_max_checks: 0, osd_max_hard: 0, llr_spread: LLR_SPREAD }
    }

    pub fn with_budget(self, budget: std::time::Duration) -> Self {
        Options { budget, ..self }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    Ft8,
    Ft4,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Protocol::Ft8 => "FT8",
            Protocol::Ft4 => "FT4",
        }
    }

    pub fn slot_secs(self) -> f64 {
        match self {
            Protocol::Ft8 => 15.0,
            Protocol::Ft4 => 7.5,
        }
    }

    /// Where in its slot a transmission nominally starts; DT is measured
    /// from here.
    pub fn nominal_start_s(self) -> f32 {
        0.5
    }

    /// Spacing of the tones, which is also the symbol rate.
    pub fn tone_spacing_hz(self) -> f32 {
        1.0 / self.symbol_period()
    }
}

/// One transmission decoded from a slot.
#[derive(Clone, Debug, PartialEq)]
pub struct Decode {
    pub protocol: Protocol,
    /// Signal to noise in 2500 Hz, as WSJT-X reports it.
    pub snr_db: f32,
    /// Start relative to the nominal 0.5 s into the slot.
    pub dt_s: f32,
    /// Audio frequency of the lowest tone.
    pub freq_hz: f32,
    /// The message as WSJT-X prints it, e.g. "CQ K1ABC FN42".
    pub text: String,
    pub message: Message,
    pub ldpc_errors: u32,
    /// How it was decoded: "bp" from the waterfall's soft bits, "deep" from
    /// the matched filter's, "ap" as a CQ with its fixed bits given, "osd"
    /// by the ordered-statistics decoder.
    pub method: &'static str,
}

/// What a message says, field by field.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Message {
    /// "standard", "nonstandard", "free text", "telemetry", "dxpedition",
    /// or a contest exchange ("eu vhf", "field day", "rtty contest",
    /// "wwrof contest") whose fields are not unpacked.
    pub kind: &'static str,
    /// Who the message is for: a call, or CQ / CQ DX / QRZ / DE.
    pub to: Option<String>,
    /// Who sent it.
    pub de: Option<String>,
    /// Grid, report, RRR, RR73 or 73.
    pub extra: Option<String>,
    /// The 4-character Maidenhead square, when the extra is one.
    pub grid: Option<String>,
    pub is_cq: bool,
}

/// A decode in the making: payload, LDPC errors, where it was found (the
/// second pass's own refined frequency and start when that pass found it),
/// and which search found it, the first or the one after subtraction.
struct Found {
    payload: [u8; 10],
    errors: usize,
    freq: f32,
    start: f32,
    search: usize,
    /// SNR, frequency and start, once measured for the subtraction.
    measured: Option<(f32, f32, f32)>,
    method: &'static str,
}

/// Decode one slot of audio at [`RATE_HZ`], starting at the slot boundary.
/// Calls heard in full are added to `hashes`, and hashed calls are looked
/// up in it, including those heard later in the same slot.
pub fn decode_slot(protocol: Protocol, audio: &[f32], hashes: &mut CallHashes) -> Vec<Decode> {
    decode_slot_in(protocol, audio, hashes, F_MIN_HZ, F_MAX_HZ)
}

/// [`decode_slot`] searching `f_min..f_max` Hz of the audio for signals'
/// lowest tones: the passband the receiver's filter lets through.
pub fn decode_slot_in(protocol: Protocol, audio: &[f32], hashes: &mut CallHashes, f_min: f32, f_max: f32) -> Vec<Decode> {
    decode_slot_with(protocol, audio, hashes, f_min, f_max, &Options::default())
}

/// [`decode_slot_in`] with the finer passes given `budget`: the sweeps that
/// compare decoders give it all the time it wants, so a busy test machine
/// does not change what decodes.
#[cfg(test)]
fn decode_slot_budget(protocol: Protocol, audio: &[f32], hashes: &mut CallHashes, f_min: f32, f_max: f32, budget: std::time::Duration) -> Vec<Decode> {
    decode_slot_with(protocol, audio, hashes, f_min, f_max, &Options::default().with_budget(budget))
}

/// [`decode_slot_in`], searched as `options` say.
pub fn decode_slot_with(protocol: Protocol, audio: &[f32], hashes: &mut CallHashes, f_min: f32, f_max: f32, options: &Options) -> Vec<Decode> {
    name(protocol, find(protocol, audio, f_min, f_max, options), hashes)
}

/// A payload found in a slot, measured, and how it was decoded; the calls
/// in it are not yet looked up.
pub(crate) struct Heard {
    payload: [u8; 10],
    errors: usize,
    /// SNR, frequency and start.
    measured: (f32, f32, f32),
    method: &'static str,
}

/// Everything in a slot, which is nearly all of a decode's work and needs
/// no table of calls: the live decoders share one table and hold it only
/// for [`name`].
pub(crate) fn find(protocol: Protocol, audio: &[f32], f_min: f32, f_max: f32, options: &Options) -> Vec<Heard> {
    let f_max = f_max.min(F_LIMIT_HZ);
    let f_min = f_min.clamp(0.0, f_max - 100.0);
    // Brought to one level first. The waterfall's dB carries an absolute
    // floor, and audio taken ahead of an AGC can sit a hundred dB down,
    // where that floor buries the band's noise and every weak signal in it;
    // live, only the few strongest stations decoded. Nothing else depends on
    // the level: SNR is measured against the slot's own noise.
    let rms = (audio.iter().map(|x| x * x).sum::<f32>() / audio.len().max(1) as f32).sqrt();
    if !(rms > 0.0 && rms.is_finite()) {
        return Vec::new();
    }
    let mut rest: Vec<f32> = audio.iter().map(|x| x * (0.1 / rms)).collect();
    let started = std::time::Instant::now();
    let searches = options.searches.clamp(1, SEARCHES);
    let deadline = |search: usize| {
        let share = if options.split_budget { SEARCH_SHARE[search + SEARCHES - searches] } else { 1.0 };
        started.checked_add(options.budget.mul_f32(share.min(1.0)))
    };
    let mut found: Vec<Found> = Vec::new();
    // Every decode taken out of the slot, and the slot searched again for
    // what they covered, until a search finds nothing new. On the crowded
    // sweep in the tests (six strong signals a slot, each with a weaker one
    // a few tones over and a little later), one subtraction took the weaker
    // ones from 0 of 240 decoded to 200 for FT8 and from 70 to 190 for FT4,
    // with nothing false.
    for search in 0..searches {
        let before = found.len();
        let Some(bb) = self::search(protocol, &rest, f_min, f_max, search, &mut found, deadline(search), options) else { break };
        // Measured where it was found: one that only came out once the
        // stronger signals over it were gone is measured without them.
        let last = found.len() == before || search + 1 == searches;
        for d in &mut found[before..] {
            let tones = demod::tones(protocol, &d.payload);
            let m = deep::measure(&bb, d.freq, d.start, &tones);
            if !last {
                subtract::subtract(protocol, &mut rest, &tones, m.1, m.2, RATE_HZ);
            }
            d.measured = Some(m);
        }
        if last {
            break;
        }
    }
    found.into_iter().filter_map(|d| Some(Heard { payload: d.payload, errors: d.errors, measured: d.measured?, method: d.method })).collect()
}

/// The messages in what [`find`] heard. Calls sent in full are added to
/// `hashes`, and hashed calls are looked up in it.
pub(crate) fn name(protocol: Protocol, heard: Vec<Heard>, hashes: &mut CallHashes) -> Vec<Decode> {
    // Once to learn every call sent in full, then again to name the hashes.
    for d in &heard {
        pack::unpack(&d.payload, hashes);
    }
    heard
        .into_iter()
        .filter_map(|d| {
            let (text, message) = pack::unpack(&d.payload, hashes)?;
            let (snr_db, freq_hz, start_s) = d.measured;
            Some(Decode {
                protocol,
                snr_db: snr_db.clamp(-30.0, 50.0).round(),
                dt_s: start_s - protocol.nominal_start_s(),
                freq_hz,
                text,
                message,
                ldpc_errors: d.errors as u32,
                method: d.method,
            })
        })
        .collect()
}

/// Whether a payload reached with help is a message worth believing. The
/// aided decoders always land on some codeword, and one in 16,384 of those
/// passes the CRC by chance; nearly all such words unpack to free text,
/// telemetry, or calls that are not calls. So only a standard message
/// between two whole callsigns is kept, and for the CQ attempt only a CQ.
fn plausible(payload: &[u8; 10], method: &str) -> bool {
    let Some((_, m)) = pack::unpack(payload, &mut CallHashes::default()) else { return false };
    let whole = |c: &Option<String>| c.as_deref().is_some_and(|c| !c.starts_with('<') && c.len() >= 3);
    m.kind == "standard" && whole(&m.de) && if method == "ap" { m.is_cq } else { m.is_cq || whole(&m.to) }
}

/// One search of the slot: sync candidates from the waterfall, each tried
/// from its soft bits, and those that fail tried again by the deep pass
/// until `deadline`. New payloads are added to `found`, marked as found by
/// search number `search`. Returns the slot's baseband, or `None` if the
/// audio is too short to hold a transmission.
#[allow(clippy::too_many_arguments)]
fn search(protocol: Protocol, audio: &[f32], f_min: f32, f_max: f32, search: usize, found: &mut Vec<Found>, deadline: Option<std::time::Instant>, options: &Options) -> Option<deep::Baseband> {
    let wf = demod::Waterfall::new(protocol, audio, RATE_HZ, f_min, f_max);
    if wf.num_blocks < protocol.symbols() / 2 {
        return None;
    }
    let mut missed = Vec::new();
    // A strong signal scores at several neighboring offsets; once one of
    // them has decoded, the others would only repeat it. Decodes of an
    // earlier search were taken out, so only this one's count.
    let near = |found: &[Found], c: &demod::Candidate| {
        found.iter().filter(|d| d.search == search).any(|d| {
            (d.freq - wf.freq_hz(c)).abs() < 0.6 * protocol.tone_spacing_hz() && (d.start - wf.start_s(c, RATE_HZ)).abs() < 0.6 * protocol.symbol_period()
        })
    };
    for c in wf.candidates(MAX_CANDIDATES, MIN_SCORE, options.distinct) {
        if near(found, &c) {
            continue;
        }
        match demod::decode_candidate(&wf, &c, LDPC_ITERATIONS, options.llr_spread) {
            Some((payload, errors)) => {
                if !found.iter().any(|d| d.payload == payload) {
                    found.push(Found { payload, errors, freq: wf.freq_hz(&c), start: wf.start_s(&c, RATE_HZ), search, measured: None, method: "bp" });
                }
            }
            None => missed.push(c),
        }
    }
    let bb = deep::Baseband::new(protocol, audio, RATE_HZ);
    let aid = deep::Aid { ap_cq: options.ap_cq, osd: options.osd, osd_max_checks: options.osd_max_checks, osd_max_hard: options.osd_max_hard, llr_spread: options.llr_spread };
    for c in missed.into_iter().take(DEEP_CANDIDATES) {
        if deadline.is_some_and(|d| std::time::Instant::now() > d) {
            break;
        }
        if near(found, &c) {
            continue;
        }
        if let Some((payload, freq, start, method)) = deep::decode(&bb, wf.freq_hz(&c), wf.start_s(&c, RATE_HZ), LDPC_ITERATIONS, &aid)
            && !found.iter().any(|d| d.payload == payload)
            && (method == "deep" || plausible(&payload, method))
        {
            found.push(Found { payload, errors: 0, freq, start, search, measured: None, method });
        }
    }
    Some(bb)
}

/// Pack `text`, encode it, and synthesize the transmission at [`RATE_HZ`]
/// with its lowest tone at `freq_hz`, starting `start_s` into a slot
/// `slot_len_s` long. Unit amplitude; `None` if the text does not pack.
pub fn synthesize(protocol: Protocol, text: &str, freq_hz: f32, start_s: f32, slot_len_s: f32) -> Option<Vec<f32>> {
    let payload = pack::pack(text, &mut CallHashes::default())?;
    let tones = demod::tones(protocol, &payload);
    let wave = demod::gfsk(protocol, &tones, freq_hz, RATE_HZ);
    let mut out = vec![0f32; (slot_len_s * RATE_HZ) as usize];
    let at = (start_s * RATE_HZ).round() as isize;
    for (i, v) in wave.into_iter().enumerate() {
        let n = at + i as isize;
        if n >= 0 && (n as usize) < out.len() {
            out[n as usize] = v;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests;
