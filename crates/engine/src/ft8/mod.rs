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
/// Candidates the second, finer pass may try, strongest first, and the
/// time it may take: a busy band's leftovers are mostly noise, and the slot
/// after this one is already filling.
const DEEP_CANDIDATES: usize = 120;
const DEEP_BUDGET: std::time::Duration = std::time::Duration::from_millis(600);

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
    decode_slot_budget(protocol, audio, hashes, f_min, f_max, DEEP_BUDGET)
}

/// [`decode_slot_in`] with the second pass given `budget` instead of
/// [`DEEP_BUDGET`]: the sweeps that compare decoders give it all the time
/// it wants, so a busy test machine does not change what decodes.
fn decode_slot_budget(protocol: Protocol, audio: &[f32], hashes: &mut CallHashes, f_min: f32, f_max: f32, budget: std::time::Duration) -> Vec<Decode> {
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
    let level: Vec<f32> = audio.iter().map(|x| x * (0.1 / rms)).collect();
    let audio = &level[..];
    let started = std::time::Instant::now();
    let deadline = started.checked_add(budget);
    let mut found: Vec<Found> = Vec::new();
    let Some(bb) = search(protocol, audio, f_min, f_max, 0, &mut found, deadline) else {
        return Vec::new();
    };
    // Every decode taken out of the slot, and the slot searched again for
    // what they covered. On the crowded sweep in the tests (six strong
    // signals a slot, each with a weaker one a few tones over and a little
    // later), the weaker ones went from 0 of 240 decoded to 200 for FT8 and
    // from 70 to 190 for FT4, with nothing false; the slot's decode took
    // about 2.5 times as long, most of it the second search's deep pass,
    // which stays inside the same budget as the first's.
    let mut residual = None;
    if !found.is_empty() {
        let mut rest = audio.to_vec();
        for d in &mut found {
            let tones = demod::tones(protocol, &d.payload);
            let m = deep::measure(&bb, d.freq, d.start, &tones);
            subtract::subtract(protocol, &mut rest, &tones, m.1, m.2, RATE_HZ);
            d.measured = Some(m);
        }
        residual = search(protocol, &rest, f_min, f_max, 1, &mut found, deadline);
    }
    // Once to learn every call sent in full, then again to name the hashes.
    for d in &found {
        pack::unpack(&d.payload, hashes);
    }
    found
        .into_iter()
        .filter_map(|d| {
            let (text, message) = pack::unpack(&d.payload, hashes)?;
            // Measured where it was found: one that only came out once the
            // stronger signals over it were gone is measured without them.
            let (snr_db, freq_hz, start_s) = match d.measured {
                Some(m) => m,
                None => {
                    let bb = if d.search == 0 { &bb } else { residual.as_ref()? };
                    deep::measure(bb, d.freq, d.start, &demod::tones(protocol, &d.payload))
                }
            };
            Some(Decode {
                protocol,
                snr_db: snr_db.clamp(-30.0, 50.0).round(),
                dt_s: start_s - protocol.nominal_start_s(),
                freq_hz,
                text,
                message,
                ldpc_errors: d.errors as u32,
            })
        })
        .collect()
}

/// One search of the slot: sync candidates from the waterfall, each tried
/// from its soft bits, and those that fail tried again by the deep pass
/// until `deadline`. New payloads are added to `found`, marked as found by
/// search number `search`. Returns the slot's baseband, or `None` if the
/// audio is too short to hold a transmission.
fn search(protocol: Protocol, audio: &[f32], f_min: f32, f_max: f32, search: usize, found: &mut Vec<Found>, deadline: Option<std::time::Instant>) -> Option<deep::Baseband> {
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
    for c in wf.candidates(MAX_CANDIDATES, MIN_SCORE) {
        if near(found, &c) {
            continue;
        }
        match demod::decode_candidate(&wf, &c, LDPC_ITERATIONS) {
            Some((payload, errors)) => {
                if !found.iter().any(|d| d.payload == payload) {
                    found.push(Found { payload, errors, freq: wf.freq_hz(&c), start: wf.start_s(&c, RATE_HZ), search, measured: None });
                }
            }
            None => missed.push(c),
        }
    }
    let bb = deep::Baseband::new(protocol, audio, RATE_HZ);
    for c in missed.into_iter().take(DEEP_CANDIDATES) {
        if deadline.is_some_and(|d| std::time::Instant::now() > d) {
            break;
        }
        if near(found, &c) {
            continue;
        }
        if let Some((payload, freq, start)) = deep::decode(&bb, wf.freq_hz(&c), wf.start_s(&c, RATE_HZ), LDPC_ITERATIONS)
            && !found.iter().any(|d| d.payload == payload)
        {
            found.push(Found { payload, errors: 0, freq, start, search, measured: None });
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
