//! JS8: keyboard-to-keyboard messaging on FT8's signal, decoded natively.
//!
//! JS8Call took FT8 as its first release stood (79 symbols of 8-FSK with
//! three 7×7 Costas syncs, and a (174, 87) LDPC code over 75 message bits
//! and a 12-bit CRC) and put a messaging layer on top. Each frame carries 72
//! bits plus three flag bits: whether it opens or closes a transmission,
//! and whether it is plain data. A transmission is as many consecutive
//! frames as its text needs, all from one audio frequency, so frames are
//! joined back into messages by where they sit in the passband.
//!
//! Four speeds share that frame: Normal (15 s slots, 6.25 Hz tones), Fast
//! (10 s, 10 Hz), Turbo (6 s, 20 Hz) and Slow (30 s, 3.125 Hz). Normal keeps
//! FT8's original sync pattern; the others use three different ones.
//!
//! The 72 bits are a heartbeat or CQ, a callsign too long for the standard
//! packing (a "compound"), a directed command between two stations, or
//! text: Huffman-coded or, in every frame sent since JS8Call 2.2, compressed
//! against a 262 144-word dictionary ("JSC"). Only frames whose LDPC code
//! and CRC both check are reported, and text only as it decoded.
//!
//! The demodulator follows this crate's FT8 decoder (an oversampled
//! waterfall, sync search, max-log soft bits, then a finer coherent pass for
//! what is left) and shares its belief propagation. The protocol details
//! (sync arrays, timing, packing, dictionary) come from reading JS8Call by
//! Jordan Sherer, KN4CRD.

mod assemble;
mod code;
mod demod;
mod jsc;
mod stream;
mod tables;
mod varicode;

pub use assemble::{Js8Event, Js8Message};
pub use stream::{Js8Decoder, Js8Status};
pub use varicode::FrameContent;

/// The rate everything inside the decoder runs at.
pub const RATE_HZ: f32 = 12_000.0;

/// Audio searched for signals unless the caller says otherwise, and the
/// furthest a search may reach, as for FT8.
pub const F_MIN_HZ: f32 = 100.0;
pub const F_MAX_HZ: f32 = 3_000.0;
pub const F_LIMIT_HZ: f32 = 5_000.0;

/// The submodes. Each is one frame in one slot of its own length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Js8Speed {
    /// 15 s slots, 6.25 Hz tones: the default.
    Normal,
    /// 10 s slots, 10 Hz tones.
    Fast,
    /// 6 s slots, 20 Hz tones.
    Turbo,
    /// 30 s slots, 3.125 Hz tones.
    Slow,
}

impl Js8Speed {
    pub const ALL: [Js8Speed; 4] = [Js8Speed::Normal, Js8Speed::Fast, Js8Speed::Turbo, Js8Speed::Slow];

    pub fn label(self) -> &'static str {
        match self {
            Js8Speed::Normal => "normal",
            Js8Speed::Fast => "fast",
            Js8Speed::Turbo => "turbo",
            Js8Speed::Slow => "slow",
        }
    }

    pub fn from_label(s: &str) -> Option<Js8Speed> {
        Js8Speed::ALL.into_iter().find(|v| v.label().eq_ignore_ascii_case(s))
    }

    /// Slot length; slots start on multiples of it in UTC.
    pub fn slot_secs(self) -> f64 {
        match self {
            Js8Speed::Normal => 15.0,
            Js8Speed::Fast => 10.0,
            Js8Speed::Turbo => 6.0,
            Js8Speed::Slow => 30.0,
        }
    }

    /// Samples per symbol at [`RATE_HZ`].
    pub fn symbol_samples(self) -> usize {
        match self {
            Js8Speed::Normal => 1920,
            Js8Speed::Fast => 1200,
            Js8Speed::Turbo => 600,
            Js8Speed::Slow => 3840,
        }
    }

    pub fn symbol_period(self) -> f32 {
        self.symbol_samples() as f32 / RATE_HZ
    }

    /// Spacing of the tones, which is also the symbol rate.
    pub fn tone_spacing_hz(self) -> f32 {
        RATE_HZ / self.symbol_samples() as f32
    }

    /// The eight tones' span.
    pub fn bandwidth_hz(self) -> f32 {
        8.0 * self.tone_spacing_hz()
    }

    /// Where in its slot a transmission nominally starts; DT is measured
    /// from here.
    pub fn start_delay_s(self) -> f32 {
        match self {
            Js8Speed::Normal | Js8Speed::Slow => 0.5,
            Js8Speed::Fast => 0.2,
            Js8Speed::Turbo => 0.1,
        }
    }

    /// The Costas arrays at symbols 0, 36 and 72. Normal kept the first
    /// FT8's; the faster and slower speeds were given their own so that a
    /// receiver set to one does not sync on another.
    pub fn costas(self) -> [[u8; 7]; 3] {
        match self {
            Js8Speed::Normal => [[4, 2, 5, 6, 1, 3, 0]; 3],
            _ => [[0, 6, 2, 3, 5, 4, 1], [1, 5, 0, 2, 3, 6, 4], [2, 5, 0, 6, 4, 1, 3]],
        }
    }

    /// How far apart two frames may be heard and still be one station's
    /// transmission, as JS8Call groups them.
    pub fn join_hz(self) -> f32 {
        match self {
            Js8Speed::Normal | Js8Speed::Slow => 10.0,
            Js8Speed::Fast => 16.0,
            Js8Speed::Turbo => 32.0,
        }
    }
}

/// One frame decoded from a slot.
#[derive(Clone, Debug, PartialEq)]
pub struct Js8Frame {
    pub speed: Js8Speed,
    /// UNIX ms of the slot's start; zero from [`decode_slot`], which is not
    /// told.
    pub slot_ms: u64,
    /// Signal to noise in 2500 Hz.
    pub snr_db: f32,
    /// Start relative to the speed's nominal start in the slot.
    pub dt_s: f32,
    /// Audio frequency of the lowest tone.
    pub freq_hz: f32,
    /// The three flag bits: 1 first frame, 2 last frame, 4 data.
    pub flags: u8,
    /// The 72 bits, most significant first.
    pub payload: [u8; 9],
    /// The bits as JS8Call writes a frame: 12 characters of its 64-symbol
    /// alphabet.
    pub raw: String,
    pub content: FrameContent,
    /// Codeword bits the soft decisions had wrong.
    pub hard_errors: u32,
}

impl Js8Frame {
    pub fn is_first(&self) -> bool {
        self.flags & 1 != 0
    }

    pub fn is_last(&self) -> bool {
        self.flags & 2 != 0
    }

    /// The frame as JS8Call prints it in its band activity.
    pub fn text(&self) -> String {
        self.content.text()
    }
}

/// Decode one slot of audio at [`RATE_HZ`], starting at the slot boundary,
/// searching the default passband.
pub fn decode_slot(speed: Js8Speed, audio: &[f32]) -> Vec<Js8Frame> {
    decode_slot_in(speed, audio, F_MIN_HZ, F_MAX_HZ)
}

/// [`decode_slot`] searching `f_min..f_max` Hz for signals' lowest tones.
pub fn decode_slot_in(speed: Js8Speed, audio: &[f32], f_min: f32, f_max: f32) -> Vec<Js8Frame> {
    demod::decode_slot(speed, audio, f_min, f_max)
}

/// Encode a frame (72 payload bits and the flag bits) and synthesize it at
/// [`RATE_HZ`] with its lowest tone at `freq_hz`, starting `start_s` into a
/// slot `slot_len_s` long. Unit amplitude, continuous-phase FSK as JS8Call
/// sends it.
pub fn synthesize(speed: Js8Speed, payload: &[u8; 9], flags: u8, freq_hz: f32, start_s: f32, slot_len_s: f32) -> Vec<f32> {
    let tones = demod::tones(speed, &code::encode(payload, flags));
    let wave = demod::fsk(speed, &tones, freq_hz, RATE_HZ);
    let mut out = vec![0f32; (slot_len_s * RATE_HZ) as usize];
    let at = (start_s * RATE_HZ).round() as isize;
    for (i, v) in wave.into_iter().enumerate() {
        let n = at + i as isize;
        if n >= 0 && (n as usize) < out.len() {
            out[n as usize] = v;
        }
    }
    out
}

#[cfg(test)]
mod tests;
