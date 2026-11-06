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
mod demod;
mod ldpc;
mod pack;
mod stream;
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

/// A decode in the making: payload, the candidate it came from, LDPC
/// errors, and the second pass's own frequency and start when that pass
/// found it.
type Found = ([u8; 10], demod::Candidate, usize, Option<(f32, f32)>);

/// Decode one slot of audio at [`RATE_HZ`], starting at the slot boundary.
/// Calls heard in full are added to `hashes`, and hashed calls are looked
/// up in it, including those heard later in the same slot.
pub fn decode_slot(protocol: Protocol, audio: &[f32], hashes: &mut CallHashes) -> Vec<Decode> {
    decode_slot_in(protocol, audio, hashes, F_MIN_HZ, F_MAX_HZ)
}

/// [`decode_slot`] searching `f_min..f_max` Hz of the audio for signals'
/// lowest tones: the passband the receiver's filter lets through.
pub fn decode_slot_in(protocol: Protocol, audio: &[f32], hashes: &mut CallHashes, f_min: f32, f_max: f32) -> Vec<Decode> {
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
    let wf = demod::Waterfall::new(protocol, audio, RATE_HZ, f_min, f_max);
    if wf.num_blocks < protocol.symbols() / 2 {
        return Vec::new();
    }
    let started = std::time::Instant::now();
    let mut found: Vec<Found> = Vec::new();
    let mut missed = Vec::new();
    // A strong signal scores at several neighboring offsets; once one of
    // them has decoded, the others would only repeat it.
    let near = |found: &[Found], c: &demod::Candidate| {
        found.iter().any(|(_, f, _, _)| {
            (wf.freq_hz(f) - wf.freq_hz(c)).abs() < 0.6 * protocol.tone_spacing_hz()
                && (wf.start_s(f, RATE_HZ) - wf.start_s(c, RATE_HZ)).abs() < 0.6 * protocol.symbol_period()
        })
    };
    for c in wf.candidates(MAX_CANDIDATES, MIN_SCORE) {
        if near(&found, &c) {
            continue;
        }
        match demod::decode_candidate(&wf, &c, LDPC_ITERATIONS) {
            Some((payload, errors)) => {
                if !found.iter().any(|(p, ..)| *p == payload) {
                    found.push((payload, c, errors, None));
                }
            }
            None => missed.push(c),
        }
    }
    let mut bb = None;
    for c in missed.into_iter().take(DEEP_CANDIDATES) {
        if started.elapsed() > DEEP_BUDGET {
            break;
        }
        if near(&found, &c) {
            continue;
        }
        let bb = bb.get_or_insert_with(|| deep::Baseband::new(protocol, audio, RATE_HZ));
        if let Some((payload, freq, start)) = deep::decode(bb, wf.freq_hz(&c), wf.start_s(&c, RATE_HZ), LDPC_ITERATIONS)
            && !found.iter().any(|(p, ..)| *p == payload)
        {
            found.push((payload, c, 0, Some((freq, start))));
        }
    }
    // Once to learn every call sent in full, then again to name the hashes.
    for (p, ..) in &found {
        pack::unpack(p, hashes);
    }
    let bb = bb.unwrap_or_else(|| deep::Baseband::new(protocol, audio, RATE_HZ));
    found
        .into_iter()
        .filter_map(|(p, c, errors, fine)| {
            let (text, message) = pack::unpack(&p, hashes)?;
            let (f0, t0) = fine.unwrap_or_else(|| (wf.freq_hz(&c), wf.start_s(&c, RATE_HZ)));
            let (snr_db, freq_hz, start_s) = deep::measure(&bb, f0, t0, &demod::tones(protocol, &p));
            Some(Decode {
                protocol,
                snr_db: snr_db.clamp(-30.0, 50.0).round(),
                dt_s: start_s - protocol.nominal_start_s(),
                freq_hz,
                text,
                message,
                ldpc_errors: errors as u32,
            })
        })
        .collect()
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
