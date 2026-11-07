//! WSPR-2: the weak-signal propagation beacon mode, decoded natively.
//!
//! A WSPR transmission carries 50 bits (a call, a locator and a power)
//! under a constraint-length 32, rate 1/2 convolutional code, interleaved,
//! as 162 symbols of 4-FSK 1.4648 Hz apart at 1.4648 baud: 110.6 s that
//! start 1 s into an even UTC minute. Each tone is a sync bit plus twice a
//! data bit, so the sync pattern runs through the whole transmission. The
//! stations of a band share about 200 Hz around 1500 Hz of audio at the
//! standard USB dials, so a slot holds anything from none to forty of them.
//!
//! The audio is brought down to a complex baseband at 375 Hz centered on
//! 1500 Hz. A spectrogram one symbol long, half a symbol apart, is searched
//! for the sync pattern over frequency, start and drift; each candidate is
//! refit on the tones themselves, and its soft bits go to a Fano sequential
//! decoder: from single symbols first, then from runs of two and three
//! symbols added up coherently, which the phase-continuous transmitter
//! allows. What Fano finishes is held to how well the message fits what was
//! received, and a marginal one to decoding the same from slightly moved
//! fits, before it counts. Decoded signals are subtracted and the search
//! runs again for what they hid. The protocol constants (sync vector, polynomials,
//! interleaver, message packing and hash) follow WSJT-X's wsprd by Joe
//! Taylor K1JT and Steven Franke K9AN; the decoder is written from them.

mod demod;
mod fano;
mod pack;
mod stream;

pub use pack::{CallHashes, Message};
pub use stream::{Downconverter, WsprDecoder, WsprStatus};

use fano::FanoParams;
use num_complex::Complex32;

/// The baseband the decoder works at, and the audio frequency it is
/// centered on.
pub const BASEBAND_HZ: f32 = 375.0;
pub const CENTER_HZ: f32 = 1_500.0;
/// Samples per symbol at [`BASEBAND_HZ`].
pub const SPS: usize = 256;
/// A transmission starts 1 s into its slot.
pub const NOMINAL_START: usize = BASEBAND_HZ as usize;
pub const SLOT_MS: u64 = 120_000;
/// Audio kept from each slot: the transmission ends 111.6 s in, plus room
/// for a late clock.
pub const SLOT_KEEP_S: f32 = 115.0;
/// How far either side of [`CENTER_HZ`] signals are searched for: the
/// 200 Hz WSPR window and a little more for a mistuned transmitter.
pub const SEARCH_HZ: f32 = 110.0;

/// Starts searched, in half-symbol spectrogram frames: about -2.4 s to
/// +2.4 s around the nominal start.
const LAGS: std::ops::RangeInclusive<i32> = -4..=10;
/// Total drift across the transmission searched, Hz.
const MAX_DRIFT: f32 = 4.0;
const DRIFT_STEP: f32 = 0.5;
/// Candidates tried per pass, and the weakest coarse sync worth refining
/// and the weakest fine sync worth decoding (fractions of the power in the
/// four tones that follows the sync pattern; noise sits near zero, and
/// refit to its best it reaches 0.1 to 0.2, while signals too weak to decode
/// still show 0.25). Most of a slot's time goes to Fano running out of
/// cycles on noise, so the fine floor is where it is saved.
const MAX_CANDIDATES: usize = 80;
const MIN_COARSE: f32 = 0.08;
const MIN_FINE: f32 = 0.16;
/// Search, decode and subtract this many times.
const PASSES: usize = 2;
/// Fano's bias per code bit. The code rate (0.5) would be the textbook
/// value and WSJT-X uses 0.45; lower lets the decoder follow weaker paths
/// further, worth about a decibel here, and the false paths it then
/// finishes are caught by the checks below.
const BIAS: f32 = 0.3;
const FANO: FanoParams = FanoParams { delta: 4 * fano::SCALE as i32, cycles_per_bit: 10_000 };
/// Symbols scored together, tried in turn until one decodes
/// ([`demod::llrs`]).
const BLOCKS: [usize; 3] = [1, 2, 3];
/// How well a decoded codeword must fit what was received
/// ([`demod::fit_score`], log2 units over the 162 code bits). With a bias
/// this low, Fano now and then finishes a path through noise, or through a
/// real signal too weak to decode, onto a message that unpacks. Measured on
/// synthetic slots: across thousands of paths finished through noise the
/// best single-symbol fit was 51, while real decodes at the edge of
/// decoding fit from 54 up. Block scoring fits every codeword better, noise
/// included, so a block decode must also fit on single symbols, and the two
/// fits together must clear a bar that no false path reached (89.7 at
/// most; real ones from 92).
const MIN_FIT: f32 = 52.5;
const MIN_FIT_SINGLE: f32 = 30.0;
const MIN_FIT_BLOCK: f32 = 94.0;
/// Below this single-symbol fit a decode must also come back, and come
/// back the same, from most of a few slightly moved fits: a real message
/// is still there an eighth of a symbol or 0.05 Hz away, while a path that
/// noise led Fano down rarely is.
const SURE_FIT: f32 = 65.0;
const JIGGLES: [(i32, f32); 4] = [(16, 0.0), (-16, 0.0), (0, 0.05), (0, -0.05)];

/// One transmission decoded from a slot.
#[derive(Clone, Debug, PartialEq)]
pub struct WsprDecode {
    /// Signal to noise in 2500 Hz, as WSJT-X reports it.
    pub snr_db: f32,
    /// Start relative to the nominal 1 s into the slot.
    pub dt_s: f32,
    /// Audio frequency of the center of the four tones, mid-transmission.
    pub freq_hz: f32,
    /// How fast the frequency moved, Hz per minute.
    pub drift_hz_per_min: f32,
    /// The sender: plain, compound, or for type 3 the hashed call in angle
    /// brackets ("<...>" when not heard in full yet).
    pub call: String,
    pub grid: Option<String>,
    pub power_dbm: i8,
    /// The message as WSJT-X prints it, e.g. "K1ABC FN42 37".
    pub text: String,
    /// "type1", "type2" or "type3".
    pub kind: &'static str,
}

/// A decode in the making: the bits, where the signal was, and its SNR.
struct Found {
    bits: u64,
    fit: demod::Fit,
    snr_db: f32,
}

/// Decode a fitted candidate: single symbols, then blocks of two and three,
/// each held to the fit its scoring calls for, and a marginal decode held
/// to coming back the same from slightly moved fits.
fn decode_candidate(z: &[Complex32], fit: &demod::Fit, refs: &demod::Refs) -> Option<u64> {
    let t = demod::tone_correlations(z, fit, refs);
    let single = demod::llrs(&t, fit, 1);
    for block in BLOCKS {
        let Some((bits, llr)) = demod::decode(&t, fit, block, BIAS, FANO) else { continue };
        let sent = demod::tones(bits);
        let fit_1 = demod::fit_score(&single, &sent);
        let passes = if block == 1 { fit_1 >= MIN_FIT } else { fit_1 >= MIN_FIT_SINGLE && fit_1 + demod::fit_score(&llr, &sent) >= MIN_FIT_BLOCK };
        if !passes {
            continue;
        }
        if fit_1 < SURE_FIT {
            let (mut agree, mut differ) = (0, 0);
            for (ds, df) in JIGGLES {
                let moved = demod::Fit { start: fit.start + ds, freq: fit.freq + df, ..*fit };
                let mt = demod::tone_correlations(z, &moved, refs);
                match demod::decode(&mt, &moved, block, BIAS, FANO) {
                    Some((j, _)) if j == bits => agree += 1,
                    Some(_) => differ += 1,
                    None => {}
                }
            }
            if agree <= differ {
                continue;
            }
        }
        return Some(bits);
    }
    None
}

/// Decode one slot of baseband at [`BASEBAND_HZ`] centered on
/// [`CENTER_HZ`], starting at the slot boundary. Calls heard in full are
/// added to `hashes`, and type 3 hashes are looked up in it, including
/// those whose call was heard in the same slot.
pub fn decode_baseband(z: &[Complex32], hashes: &mut CallHashes) -> Vec<WsprDecode> {
    let ms = z.iter().map(|v| v.norm_sqr()).sum::<f32>() / z.len().max(1) as f32;
    if !(ms > 0.0 && ms.is_finite()) || z.len() < demod::SPAN / 2 {
        return Vec::new();
    }
    // Brought to unit power: every metric is a ratio, but the spectrogram's
    // sums should not live near the edge of an f32's range.
    let scale = 1.0 / ms.sqrt();
    let mut z: Vec<Complex32> = z.iter().map(|v| v * scale).collect();
    let refs = demod::Refs::new();
    let drifts: Vec<f32> = (0..=(2.0 * MAX_DRIFT / DRIFT_STEP) as i32).map(|i| -MAX_DRIFT + DRIFT_STEP * i as f32).collect();
    let mut found: Vec<Found> = Vec::new();
    for _ in 0..PASSES {
        let spec = demod::Spectrogram::new(&z, SEARCH_HZ);
        let scores = spec.search(SEARCH_HZ, LAGS, &drifts);
        // Local peaks over frequency of the best sync at each.
        let mut cands: Vec<(i32, f32, f32, i32)> = (0..scores.len())
            .filter(|&i| {
                let s = scores[i].1;
                let left = i == 0 || scores[i - 1].1 < s;
                let right = i + 1 == scores.len() || scores[i + 1].1 <= s;
                s >= MIN_COARSE && left && right
            })
            .map(|i| scores[i])
            .collect();
        cands.sort_by(|a, b| b.1.total_cmp(&a.1));
        cands.truncate(MAX_CANDIDATES);
        let mut new: Vec<(Found, [u8; fano::NSYM])> = Vec::new();
        for (bin, _, drift, lag) in cands {
            let fit = demod::Fit { freq: bin as f32 * demod::BIN_HZ, drift, start: lag * (SPS / 2) as i32 };
            // A decoded signal also scores two tones either side of itself,
            // where half its tones line up with the pattern, and what its
            // subtraction leaves behind can be read as a confident, wrong
            // message a tone away. Hidden signals that close to a decoded
            // one, at the same start, are given up for that.
            let near = |f: &demod::Fit| (f.freq - fit.freq).abs() < 3.5 * demod::DF && (f.start - fit.start).abs() < 2 * SPS as i32;
            if new.iter().any(|(f, _)| near(&f.fit)) || found.iter().any(|f| near(&f.fit)) {
                continue;
            }
            let (fit, sync) = demod::refine(&z, fit, &refs);
            if sync < MIN_FINE {
                continue;
            }
            let Some(bits) = decode_candidate(&z, &fit, &refs) else { continue };
            if found.iter().any(|f| f.bits == bits) || new.iter().any(|(f, _)| f.bits == bits) {
                continue;
            }
            let sent = demod::tones(bits);
            let fit = demod::fit_known(&z, fit, &sent);
            let t = demod::tone_correlations(&z, &fit, &refs);
            new.push((Found { bits, fit, snr_db: demod::snr_db(&t, &sent) }, sent));
        }
        if new.is_empty() {
            break;
        }
        for (f, sent) in &new {
            let t = demod::tone_correlations(&z, &f.fit, &refs);
            demod::subtract(&mut z, &f.fit, sent, &t);
        }
        found.extend(new.into_iter().map(|(f, _)| f));
    }
    // Once to learn every call sent in full, then again to name the hashes.
    let messages: Vec<Option<Message>> = found.iter().map(|f| pack::unpack(f.bits, hashes)).collect();
    let mut out: Vec<WsprDecode> = found
        .iter()
        .zip(messages)
        .filter_map(|(f, m)| {
            let m = m?;
            let m = if m.kind == "type3" { pack::unpack(f.bits, hashes)? } else { m };
            Some(WsprDecode {
                snr_db: f.snr_db.clamp(-40.0, 40.0).round(),
                dt_s: (demod::dt_s(f.fit.start) * 10.0).round() / 10.0,
                freq_hz: CENTER_HZ + f.fit.freq,
                drift_hz_per_min: f.fit.drift * 60.0 / (demod::SPAN as f32 / BASEBAND_HZ),
                call: m.call,
                grid: m.grid,
                power_dbm: m.power_dbm,
                text: m.text,
                kind: m.kind,
            })
        })
        .collect();
    out.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));
    out
}

/// Decode one slot of real audio at `fs` Hz that starts at the slot
/// boundary: what [`WsprDecoder`] does with a slot, for audio in hand.
pub fn decode_audio(audio: &[f32], fs: f32, hashes: &mut CallHashes) -> Vec<WsprDecode> {
    let mut dc = Downconverter::new(fs);
    let mut z = Vec::new();
    dc.push(audio, &mut z);
    dc.flush(&mut z);
    let z: Vec<Complex32> = z.into_iter().map(|(_, v)| v).collect();
    decode_baseband(&z, hashes)
}

/// Pack `text` ("K1ABC FN42 37", "PJ4/K1ABC 37", "<K1ABC> FN42AX 37"),
/// encode it, and give the 162 channel tones (0..3); `None` if the text is
/// not a WSPR message.
pub fn channel_tones(text: &str) -> Option<[u8; 162]> {
    pack::pack(text).map(demod::tones)
}

/// The tone spacing and symbol rate, Hz.
pub const TONE_SPACING_HZ: f32 = demod::DF;

#[cfg(test)]
mod tests;
