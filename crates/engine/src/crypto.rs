//! Optional keyed voice decryption.
//!
//! Startup keys come from a private key file; channel-specific keys may also
//! be updated through the dedicated write-only web form. This module has no
//! serde support: keys cannot become ordinary settings, status, or call data.

use aes::{Aes128, Aes256};
use cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use des::Des;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Protocol {
    P25,
    Dmr,
}

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
struct Secret(Vec<u8>);

#[derive(Default)]
pub struct KeyStore {
    keys: HashMap<(Protocol, u8, u32), Secret>,
}

impl KeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a key addressed by the exact over-the-air algorithm and key ID.
    pub fn insert(
        &mut self,
        protocol: Protocol,
        algorithm: u8,
        key_id: u32,
        key: Vec<u8>,
    ) -> Result<(), &'static str> {
        let key = Secret(key);
        if !valid_key_length(protocol, algorithm, key.0.len()) {
            return Err("wrong key length for voice algorithm");
        }
        self.keys.insert((protocol, algorithm, key_id), key);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

static KEYS: OnceLock<Arc<KeyStore>> = OnceLock::new();

/// Install the process key store once. Callers should do this before receiver
/// threads start; replacement at runtime is intentionally forbidden.
pub fn install_keys(keys: KeyStore) -> Result<(), &'static str> {
    KEYS.set(Arc::new(keys))
        .map_err(|_| "voice key store is already installed")
}

// Browser-configured keys override the startup key file only on their channel.
static CHANNEL_KEYS: OnceLock<RwLock<HashMap<u64, (u8, u16, Secret)>>> = OnceLock::new();

pub fn set_p25_channel_key(
    frequency_hz: u64,
    algorithm: u8,
    key_id: u16,
    key: Vec<u8>,
) -> Result<(), &'static str> {
    let key = Secret(key);
    if !valid_key_length(Protocol::P25, algorithm, key.0.len()) {
        return Err("wrong key length for P25 algorithm");
    }
    CHANNEL_KEYS
        .get_or_init(Default::default)
        .write()
        .expect("channel keys")
        .insert(frequency_hz, (algorithm, key_id, key));
    Ok(())
}

pub fn remove_p25_channel_key(frequency_hz: u64) {
    if let Some(keys) = CHANNEL_KEYS.get() {
        keys.write().expect("channel keys").remove(&frequency_hz);
    }
}

fn valid_key_length(protocol: Protocol, algorithm: u8, length: usize) -> bool {
    match (protocol, algorithm) {
        (Protocol::P25, 0x81) | (Protocol::Dmr, 0x22) => length == 8,
        (Protocol::P25, 0x84) | (Protocol::Dmr, 0x25) => length == 32,
        (Protocol::P25, 0x89) | (Protocol::Dmr, 0x24) => length == 16,
        (Protocol::P25, 0xaa) | (Protocol::Dmr, 0x21) => length == 5,
        // Headerless vendor variants use the configured talkgroup as key ID.
        (Protocol::Dmr, 0x01) => length == 2, // Motorola Basic Privacy mask
        (Protocol::Dmr, 0x02) => length == 5, // Hytera Enhanced Privacy
        (Protocol::Dmr, 0x12) => matches!(length, 5 | 16 | 32), // Hytera basic repeating key
        (Protocol::Dmr, 0x37) => length == 32, // Kirisun universal privacy
        (Protocol::Dmr, 0xe1) => length == 16, // TYT EP static AES-derived mask
        _ => false,
    }
}

/// Per-call stream state. The 49 payload bits of each AMBE frame consume a
/// 56-bit stream word; the seven padding bits are discarded as prescribed by
/// the P25/DMR voice mapping.
pub struct VoiceDecryptor {
    stream: Stream,
    current: u8,
    bits_left: u8,
    payload_bits: usize,
    skip_bits: usize,
    phase1_position: Option<usize>,
}

impl VoiceDecryptor {
    pub fn for_call(
        protocol: Protocol,
        algorithm: u8,
        key_id: u16,
        message_indicator: &[u8],
    ) -> Option<Self> {
        Self::for_call_on_channel(None, protocol, algorithm, key_id, message_indicator)
    }

    pub fn for_call_on_channel(
        frequency_hz: Option<f64>,
        protocol: Protocol,
        algorithm: u8,
        key_id: u16,
        message_indicator: &[u8],
    ) -> Option<Self> {
        let channel_secret = if protocol == Protocol::P25 {
            frequency_hz.and_then(|frequency| {
                CHANNEL_KEYS.get().and_then(|keys| {
                    keys.read()
                        .expect("channel keys")
                        .get(&(frequency.round() as u64))
                        .cloned()
                })
            })
        } else {
            None
        };
        let secret = if let Some((selected_algorithm, selected_id, secret)) = channel_secret {
            if selected_algorithm != algorithm || selected_id != key_id {
                return None;
            }
            secret
        } else {
            KEYS.get()?
                .keys
                .get(&(protocol, algorithm, u32::from(key_id)))?
                .clone()
        };
        let stream = match (protocol, algorithm) {
            (Protocol::P25, 0xaa) => {
                let mut seed = Vec::with_capacity(13);
                seed.extend_from_slice(&secret.0);
                seed.extend_from_slice(message_indicator.get(..8)?);
                Stream::Rc4(Rc4::new(&seed))
            }
            (Protocol::Dmr, 0x21) => {
                let mut seed = Vec::with_capacity(9);
                seed.extend_from_slice(&secret.0);
                seed.extend_from_slice(message_indicator.get(..4)?);
                Stream::Rc4(Rc4::new(&seed))
            }
            (Protocol::Dmr, 0x02) => Stream::Hytera(Hytera::new(&secret.0, message_indicator)?),
            (Protocol::Dmr, 0x37) => Stream::Repeat(Repeat::from_bytes(
                &kirisun_universal(&secret.0, message_indicator)?,
                false,
            )),
            (Protocol::P25, 0x81) | (Protocol::Dmr, 0x22) => {
                let iv = iv64(protocol, message_indicator)?;
                Stream::Des(Ofb::des(&secret.0, iv)?)
            }
            (Protocol::P25, 0x89) | (Protocol::Dmr, 0x24) => {
                let iv = iv128(protocol, message_indicator)?;
                Stream::Aes128(Ofb::aes128(&secret.0, iv)?)
            }
            (Protocol::P25, 0x84) | (Protocol::Dmr, 0x25) => {
                let iv = iv128(protocol, message_indicator)?;
                Stream::Aes256(Ofb::aes256(&secret.0, iv)?)
            }
            _ => return None,
        };
        let continuous_49 = protocol == Protocol::Dmr && matches!(algorithm, 0x02 | 0x37);
        let mut out = Self {
            stream,
            phase1_position: None,
            current: 0,
            bits_left: 0,
            payload_bits: 49,
            skip_bits: if continuous_49 { 0 } else { 7 },
        };
        // Motorola ADP uses RC4-drop[256] in both P25 air interfaces.
        if protocol == Protocol::P25 && algorithm == 0xaa {
            out.discard_octets(256);
        }
        Some(out)
    }

    /// Build a P25 Phase 1 decryptor positioned at an IMBE voice frame in the
    /// current LDU1/LDU2 superframe.  The FDMA mapping consumes eleven whole
    /// octets per 88-bit IMBE information frame.  Block-cipher OFB mappings
    /// also reserve eleven octets between their mandatory first discarded
    /// block and voice frame zero. ADP additionally discards 256 octets.
    pub fn for_p25_phase1(
        algorithm: u8,
        key_id: u16,
        message_indicator: &[u8],
        voice_frame_index: usize,
    ) -> Option<Self> {
        Self::for_p25_phase1_on_channel(
            None,
            algorithm,
            key_id,
            message_indicator,
            voice_frame_index,
        )
    }

    pub fn for_p25_phase1_on_channel(
        frequency_hz: Option<f64>,
        algorithm: u8,
        key_id: u16,
        message_indicator: &[u8],
        voice_frame_index: usize,
    ) -> Option<Self> {
        let mut out = Self::for_call_on_channel(
            frequency_hz,
            Protocol::P25,
            algorithm,
            key_id,
            message_indicator,
        )?;
        if voice_frame_index >= 18 {
            return None;
        }
        // Each LDU has two LSD octets immediately before its ninth voice word.
        let lsd = 2 * (voice_frame_index / 9);
        out.discard_octets(11 + voice_frame_index * 11 + lsd);
        out.phase1_position = Some(voice_frame_index);
        out.payload_bits = 88;
        out.skip_bits = 0;
        Some(out)
    }

    /// Position TDMA voice using its burst index, so missing bursts do not
    /// shift all subsequent voice words.
    pub fn for_p25_phase2(
        algorithm: u8,
        key_id: u16,
        mi: &[u8],
        voice_index: usize,
    ) -> Option<Self> {
        Self::for_p25_phase2_on_channel(None, algorithm, key_id, mi, voice_index)
    }

    pub fn for_p25_phase2_on_channel(
        frequency_hz: Option<f64>,
        algorithm: u8,
        key_id: u16,
        mi: &[u8],
        voice_index: usize,
    ) -> Option<Self> {
        if voice_index >= 18 {
            return None;
        }
        let mut out =
            Self::for_call_on_channel(frequency_hz, Protocol::P25, algorithm, key_id, mi)?;
        out.discard_octets(7 * voice_index);
        Some(out)
    }

    /// Headerless DMR Basic Privacy is selected by manufacturer and talkgroup,
    /// not by a Privacy Indicator key ID.
    pub fn for_dmr_basic(feature_id: u8, talkgroup: u32) -> Option<Self> {
        let preferred = match feature_id {
            0x10 => 0x01, // Motorola
            0x68 => 0x12, // Hytera headerless basic variant
            _ => 0,
        };
        let keys = &KEYS.get()?.keys;
        // Cheap-radio families often send only the encrypted service option,
        // with no useful manufacturer discriminator. An explicit per-TG key
        // is therefore also a forced selection. The preferred manufacturer
        // mode wins if both are configured.
        let (algorithm, secret) = [preferred, 0x01, 0x12, 0xe1]
            .into_iter()
            .filter(|algorithm| *algorithm != 0)
            .find_map(|algorithm| {
                keys.get(&(Protocol::Dmr, algorithm, talkgroup))
                    .map(|secret| (algorithm, secret))
            })?;
        let (stream, payload_bits) = match algorithm {
            0x01 => {
                let key = u16::from_be_bytes(secret.0.as_slice().try_into().ok()?);
                let mask = (u64::from(key & 0xff0f) << 32)
                    .wrapping_add(u64::from(key) << 16)
                    .wrapping_add(u64::from(key));
                (Stream::Repeat(Repeat::from_word(mask, 48, true)), 48)
            }
            0xe1 => (
                Stream::Repeat(Repeat::from_bytes(&tyt_ep_mask(&secret.0)?, true)),
                49,
            ),
            _ => (Stream::Repeat(Repeat::from_bytes(&secret.0, false)), 49),
        };
        Some(Self {
            stream,
            phase1_position: None,
            current: 0,
            bits_left: 0,
            payload_bits,
            skip_bits: 0,
        })
    }

    pub fn apply_ambe49(&mut self, frame: &mut [u8; 7]) {
        self.stream.begin_frame();
        for bit in 0..self.payload_bits {
            let byte = bit / 8;
            let shift = 7 - bit % 8;
            frame[byte] ^= self.next_bit() << shift;
        }
        for _ in 0..self.skip_bits {
            let _ = self.next_bit();
        }
        frame[6] &= 0x80;
    }

    /// Apply one P25 Phase 1 keystream word to the FEC-corrected 88 IMBE
    /// information bits.  Encryption is below the IMBE channel-code layer, so
    /// callers must never apply this operation to the 144-bit air codeword.
    pub fn apply_imbe88(&mut self, frame: &mut [u8; 11]) {
        if let Some(position) = self.phase1_position.as_mut() {
            let skip_lsd = *position % 9 == 8;
            *position += 1;
            if skip_lsd {
                self.discard_octets(2);
            }
        }
        self.stream.begin_frame();
        for byte in frame {
            *byte ^= self.next_octet();
        }
    }

    fn discard_octets(&mut self, count: usize) {
        for _ in 0..count {
            let _ = self.next_octet();
        }
    }

    fn next_octet(&mut self) -> u8 {
        if self.bits_left == 0 {
            return self.stream.next_byte();
        }
        let mut out = 0u8;
        for _ in 0..8 {
            out = (out << 1) | self.next_bit();
        }
        out
    }

    fn next_bit(&mut self) -> u8 {
        if self.bits_left == 0 {
            self.current = self.stream.next_byte();
            self.bits_left = 8;
        }
        let bit = self.current >> 7;
        self.current <<= 1;
        self.bits_left -= 1;
        bit
    }
}

fn tyt_ep_mask(user_key: &[u8]) -> Option<[u8; 16]> {
    const STATIC_KEY: [u8; 16] = [
        0x6e, 0x02, 0x8d, 0x8a, 0xca, 0xeb, 0x9b, 0xbe, 0x42, 0x72, 0xfb, 0x82, 0x64, 0x56, 0x31,
        0xfa,
    ];
    let mut register: [u8; 16] = user_key.try_into().ok()?;
    register.reverse();
    let cipher = Aes128::new_from_slice(&STATIC_KEY).ok()?;
    cipher.encrypt_block(GenericArray::from_mut_slice(&mut register));
    Some(register)
}

fn kirisun_universal(key: &[u8], mi: &[u8]) -> Option<[u8; 126]> {
    let key: [u8; 32] = key.try_into().ok()?;
    let mi: [u8; 4] = mi.get(..4)?.try_into().ok()?;
    let real_key = md2ii(&key, 32)?;
    let mut seeded = Vec::with_capacity(36);
    seeded.extend_from_slice(&mi);
    seeded.extend_from_slice(&real_key[..32]);
    let state_hash = md2ii(&seeded, 8)?;
    let stream_key = md2ii(&seeded, 24)?;
    let frame = u64::from_be_bytes(state_hash[..8].try_into().ok()?);
    Some(kirisun_keystream(&stream_key[..24], frame))
}

fn md2ii(input: &[u8], width: usize) -> Option<Vec<u8>> {
    if width == 0 || width > 32 {
        return None;
    }
    const S: [u8; 256] = [
        13, 199, 11, 67, 237, 193, 164, 77, 115, 184, 141, 222, 73, 38, 147, 36, 150, 87, 21, 104,
        12, 61, 156, 101, 111, 145, 119, 22, 207, 35, 198, 37, 171, 167, 80, 30, 219, 28, 213, 121,
        86, 29, 214, 242, 6, 4, 89, 162, 110, 175, 19, 157, 3, 88, 234, 94, 144, 118, 159, 239,
        100, 17, 182, 173, 238, 68, 16, 79, 132, 54, 163, 52, 9, 58, 57, 55, 229, 192, 170, 226,
        56, 231, 187, 158, 70, 224, 233, 245, 26, 47, 32, 44, 247, 8, 251, 20, 197, 185, 109, 153,
        204, 218, 93, 178, 212, 137, 84, 174, 24, 120, 130, 149, 72, 180, 181, 208, 255, 189, 152,
        18, 143, 176, 60, 249, 27, 227, 128, 139, 243, 253, 59, 123, 172, 108, 211, 96, 138, 10,
        215, 42, 225, 40, 81, 65, 90, 25, 98, 126, 154, 64, 124, 116, 122, 5, 1, 168, 83, 190, 131,
        191, 244, 240, 235, 177, 155, 228, 125, 66, 43, 201, 248, 220, 129, 188, 230, 62, 75, 71,
        78, 34, 31, 216, 254, 136, 91, 114, 106, 46, 217, 196, 92, 151, 209, 133, 51, 236, 33, 252,
        127, 179, 69, 7, 183, 105, 146, 97, 39, 15, 205, 112, 200, 166, 223, 45, 48, 246, 186, 41,
        148, 140, 107, 76, 85, 95, 194, 142, 50, 49, 134, 23, 135, 169, 221, 210, 203, 63, 165, 82,
        161, 202, 53, 14, 206, 232, 103, 102, 195, 117, 250, 99, 0, 74, 160, 241, 2, 113,
    ];
    let mut h1 = vec![0u8; width * 3];
    let mut h2 = vec![0u8; width];
    let mut x1 = 0u8;
    let mut x2 = 0usize;
    let process = |data: &[u8], h1: &mut [u8], h2: &mut [u8], x1: &mut u8, x2: &mut usize| {
        for &value in data {
            h1[*x2 + width] = value;
            h1[*x2 + width * 2] = value ^ h1[*x2];
            h2[*x2] ^= S[usize::from(value ^ *x1)];
            *x1 = h2[*x2];
            *x2 += 1;
            if *x2 == width {
                let mut t = 0u8;
                *x2 = 0;
                for round in 0..width + 2 {
                    for cell in h1.iter_mut() {
                        *cell ^= S[usize::from(t)];
                        t = *cell;
                    }
                    t = t.wrapping_add(round as u8);
                }
            }
        }
    };
    process(input, &mut h1, &mut h2, &mut x1, &mut x2);
    let pad = vec![(width - x2) as u8; width - x2];
    process(&pad, &mut h1, &mut h2, &mut x1, &mut x2);
    let checksum = h2.clone();
    process(&checksum, &mut h1, &mut h2, &mut x1, &mut x2);
    Some(h1[..width].to_vec())
}

fn kirisun_keystream(key: &[u8], mut frame: u64) -> [u8; 126] {
    fn threshold(a: u64, b: u64, c: u64) -> bool {
        [a, b, c].into_iter().filter(|v| (v >> 31) & 1 != 0).count() <= 1
    }
    fn clock(mut value: u64, control: bool, taps: &[u8]) -> u64 {
        if control ^ ((value >> 31) & 1 != 0) {
            let feedback = taps.iter().fold(0u64, |v, tap| v ^ ((value >> tap) & 1));
            value <<= 1;
            value |= feedback;
        }
        value
    }
    const T1: [u8; 28] = [
        0, 3, 5, 9, 10, 11, 12, 17, 18, 28, 33, 34, 35, 36, 37, 39, 42, 43, 44, 46, 47, 49, 50, 57,
        60, 61, 62, 63,
    ];
    const T2: [u8; 34] = [
        0, 3, 5, 8, 9, 10, 12, 13, 15, 17, 19, 20, 21, 22, 24, 27, 30, 31, 33, 34, 35, 36, 37, 40,
        41, 42, 51, 52, 55, 56, 59, 60, 62, 63,
    ];
    const T3: [u8; 42] = [
        1, 2, 4, 5, 6, 7, 8, 9, 10, 14, 15, 16, 17, 18, 22, 23, 25, 26, 27, 28, 29, 31, 32, 34, 35,
        36, 38, 41, 42, 43, 44, 45, 47, 48, 49, 50, 51, 54, 55, 59, 61, 63,
    ];
    let mut a = u64::from_be_bytes(key[..8].try_into().expect("eight bytes"));
    let mut b = u64::from_be_bytes(key[8..16].try_into().expect("eight bytes"));
    let mut c = u64::from_be_bytes(key[16..24].try_into().expect("eight bytes"));
    for _ in 0..64 {
        let control = threshold(a, b, c);
        a = clock(a, control, &T1);
        b = clock(b, control, &T2);
        c = clock(c, control, &T3);
        if frame & 1 != 0 {
            a ^= 1;
            b ^= 1;
            c ^= 1;
        }
        frame >>= 1;
    }
    for _ in 0..384 {
        let control = threshold(a, b, c);
        a = clock(a, control, &T1);
        b = clock(b, control, &T2);
        c = clock(c, control, &T3);
    }
    let mut out = [0u8; 126];
    for bit in 0..1008 {
        let control = threshold(a, b, c);
        a = clock(a, control, &T1);
        b = clock(b, control, &T2);
        c = clock(c, control, &T3);
        out[bit / 8] |= (((a >> 63) ^ (b >> 63) ^ (c >> 63)) as u8 & 1) << (7 - bit % 8);
    }
    out
}

enum Stream {
    Rc4(Rc4),
    Des(Ofb<Des, 8>),
    Aes128(Ofb<Aes128, 16>),
    Aes256(Ofb<Aes256, 16>),
    Repeat(Repeat),
    Hytera(Hytera),
}

impl Stream {
    fn next_byte(&mut self) -> u8 {
        match self {
            Self::Rc4(v) => v.next_byte(),
            Self::Des(v) => v.next_byte(),
            Self::Aes128(v) => v.next_byte(),
            Self::Aes256(v) => v.next_byte(),
            Self::Repeat(v) => v.next_byte(),
            Self::Hytera(v) => v.next_byte(),
        }
    }

    fn begin_frame(&mut self) {
        if let Self::Repeat(v) = self {
            v.begin_frame();
        }
    }
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct Hytera {
    rc4: Rc4,
    key_iv: [u8; 5],
    at: usize,
}

impl Hytera {
    fn new(key: &[u8], mi: &[u8]) -> Option<Self> {
        let key: [u8; 5] = key.try_into().ok()?;
        let mi: [u8; 5] = mi.get(..5)?.try_into().ok()?;
        Some(Self {
            rc4: Rc4::new(&key),
            key_iv: std::array::from_fn(|i| key[i] ^ mi[i]),
            at: 0,
        })
    }

    fn next_byte(&mut self) -> u8 {
        let out = self.rc4.next_byte() ^ self.key_iv[self.at];
        self.at = (self.at + 1) % self.key_iv.len();
        out
    }
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct Repeat {
    bits: Vec<u8>,
    at: usize,
    reset_each_frame: bool,
}

impl Repeat {
    fn from_word(word: u64, count: usize, reset_each_frame: bool) -> Self {
        Self {
            bits: (0..count)
                .map(|bit| ((word >> (count - 1 - bit)) & 1) as u8)
                .collect(),
            at: 0,
            reset_each_frame,
        }
    }

    fn from_bytes(bytes: &[u8], reset_each_frame: bool) -> Self {
        Self {
            bits: bytes
                .iter()
                .flat_map(|byte| (0..8).rev().map(move |bit| (byte >> bit) & 1))
                .collect(),
            at: 0,
            reset_each_frame,
        }
    }

    fn begin_frame(&mut self) {
        if self.reset_each_frame {
            self.at = 0;
        }
    }

    fn next_byte(&mut self) -> u8 {
        let mut out = 0u8;
        for _ in 0..8 {
            out = (out << 1) | self.bits[self.at];
            self.at = (self.at + 1) % self.bits.len();
        }
        out
    }
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct Rc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    fn new(key: &[u8]) -> Self {
        let mut s = [0u8; 256];
        for (i, value) in s.iter_mut().enumerate() {
            *value = i as u8;
        }
        let mut j = 0u8;
        for i in 0..256 {
            j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
            s.swap(i, usize::from(j));
        }
        Self { s, i: 0, j: 0 }
    }

    fn next_byte(&mut self) -> u8 {
        self.i = self.i.wrapping_add(1);
        self.j = self.j.wrapping_add(self.s[usize::from(self.i)]);
        self.s.swap(usize::from(self.i), usize::from(self.j));
        self.s[usize::from(self.s[usize::from(self.i)].wrapping_add(self.s[usize::from(self.j)]))]
    }
}

struct Ofb<C, const N: usize> {
    cipher: C,
    feedback: [u8; N],
    at: usize,
}

impl Ofb<Des, 8> {
    fn des(key: &[u8], iv: [u8; 8]) -> Option<Self> {
        let cipher = Des::new_from_slice(key).ok()?;
        let mut out = Self {
            cipher,
            feedback: iv,
            at: 8,
        };
        out.refill(); // The first OFB block is reserved/discarded on voice.
        out.at = 8;
        Some(out)
    }
}

impl Ofb<Aes128, 16> {
    fn aes128(key: &[u8], iv: [u8; 16]) -> Option<Self> {
        let cipher = Aes128::new_from_slice(key).ok()?;
        let mut out = Self {
            cipher,
            feedback: iv,
            at: 16,
        };
        out.refill();
        out.at = 16;
        Some(out)
    }
}

impl Ofb<Aes256, 16> {
    fn aes256(key: &[u8], iv: [u8; 16]) -> Option<Self> {
        let cipher = Aes256::new_from_slice(key).ok()?;
        let mut out = Self {
            cipher,
            feedback: iv,
            at: 16,
        };
        out.refill();
        out.at = 16;
        Some(out)
    }
}

impl<C: BlockEncrypt, const N: usize> Ofb<C, N> {
    fn refill(&mut self) {
        let block = GenericArray::from_mut_slice(&mut self.feedback);
        self.cipher.encrypt_block(block);
        self.at = 0;
    }

    fn next_byte(&mut self) -> u8 {
        if self.at == N {
            self.refill();
        }
        let out = self.feedback[self.at];
        self.at += 1;
        out
    }
}

fn iv64(protocol: Protocol, mi: &[u8]) -> Option<[u8; 8]> {
    match protocol {
        Protocol::P25 => mi.get(..8)?.try_into().ok(),
        Protocol::Dmr => {
            let seed: [u8; 4] = mi.get(..4)?.try_into().ok()?;
            Some(expand_dmr_iv(seed)[..8].try_into().ok()?)
        }
    }
}

fn iv128(protocol: Protocol, mi: &[u8]) -> Option<[u8; 16]> {
    match protocol {
        Protocol::P25 => {
            let seed: [u8; 8] = mi.get(..8)?.try_into().ok()?;
            let mut state = u64::from_be_bytes(seed);
            let mut out = [0u8; 16];
            out[..8].copy_from_slice(&seed);
            for bit_at in 64..128 {
                let bit = ((state >> 63)
                    ^ (state >> 61)
                    ^ (state >> 45)
                    ^ (state >> 37)
                    ^ (state >> 26)
                    ^ (state >> 14))
                    & 1;
                state = (state << 1) | bit;
                out[bit_at / 8] |= (bit as u8) << (7 - bit_at % 8);
            }
            Some(out)
        }
        Protocol::Dmr => {
            let seed: [u8; 4] = mi.get(..4)?.try_into().ok()?;
            Some(expand_dmr_iv(seed))
        }
    }
}

/// Predict the next P25 MI when the repeated ESS is damaged.
pub(crate) fn cycle_p25_mi(mi: &mut [u8; 9]) {
    let iv = iv128(Protocol::P25, mi).expect("nine-byte MI");
    mi[..8].copy_from_slice(&iv[8..]);
    mi[8] = 0;
}

fn expand_dmr_iv(seed: [u8; 4]) -> [u8; 16] {
    let mut state = u32::from_be_bytes(seed);
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&seed);
    for bit_at in 32..128 {
        let bit = ((state >> 31) ^ (state >> 21) ^ (state >> 1) ^ state) & 1;
        state = (state << 1) | bit;
        out[bit_at / 8] |= (bit as u8) << (7 - bit_at % 8);
    }
    out
}

#[cfg(test)]
pub(crate) fn install_test_keys() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let mut keys = KeyStore::new();
        for (algorithm, size) in [(0xaa, 5), (0x81, 8), (0x84, 32)] {
            keys.insert(Protocol::P25, algorithm, 0xcafe, (0..size).collect())
                .unwrap();
        }
        install_keys(keys).unwrap();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_keys_override_only_their_frequency_and_update_both_phases() {
        install_test_keys();
        let frequency = 913127253.0;
        let mi = [1; 9];
        let mut baseline = [0; 7];
        VoiceDecryptor::for_p25_phase2(0xaa, 0xcafe, &mi, 8)
            .unwrap()
            .apply_ambe49(&mut baseline);
        set_p25_channel_key(frequency as u64, 0xaa, 0xcafe, vec![0x55; 5]).unwrap();
        let mut scoped = [0; 7];
        VoiceDecryptor::for_p25_phase2_on_channel(Some(frequency), 0xaa, 0xcafe, &mi, 8)
            .unwrap()
            .apply_ambe49(&mut scoped);
        assert_ne!(scoped, baseline);
        assert!(
            VoiceDecryptor::for_p25_phase1_on_channel(Some(frequency), 0x84, 0xcafe, &mi, 0)
                .is_none(),
            "wrong channel key type must not fall back to global key"
        );
        remove_p25_channel_key(frequency as u64);
        let mut restored = [0; 7];
        VoiceDecryptor::for_p25_phase2_on_channel(Some(frequency), 0xaa, 0xcafe, &mi, 8)
            .unwrap()
            .apply_ambe49(&mut restored);
        assert_eq!(restored, baseline);
    }

    #[test]
    fn p25_matches_boatbod_all_voice_words_both_phases() {
        install_test_keys();
        let mi = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0];
        for line in include_str!("../tests/fixtures/op25-voice-keystream.txt").lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            let algorithm = u8::from_str_radix(fields[0], 16).unwrap();
            let expected: Vec<u8> = (0..fields[2].len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&fields[2][i..i + 2], 16).unwrap())
                .collect();
            let mut got = Vec::new();
            if fields[1] == "1" {
                for ldu in [0, 9] {
                    let mut decryptor =
                        VoiceDecryptor::for_p25_phase1(algorithm, 0xcafe, &mi, ldu).unwrap();
                    for _ in 0..9 {
                        let mut word = [0; 11];
                        decryptor.apply_imbe88(&mut word);
                        got.extend(word);
                    }
                }
            } else {
                let mut decryptor =
                    VoiceDecryptor::for_call(Protocol::P25, algorithm, 0xcafe, &mi).unwrap();
                for _ in 0..18 {
                    let mut word = [0; 7];
                    decryptor.apply_ambe49(&mut word);
                    got.extend(word);
                }
            }
            assert_eq!(
                got, expected,
                "algorithm {algorithm:02x}, phase {}",
                fields[1]
            );
            // Re-entry after a lost voice burst must equal the same word in
            // the uninterrupted OP25 sequence, including both LSD boundaries.
            for index in 0..18 {
                if fields[1] == "1" {
                    let mut word = [0; 11];
                    VoiceDecryptor::for_p25_phase1(algorithm, 0xcafe, &mi, index)
                        .unwrap()
                        .apply_imbe88(&mut word);
                    assert_eq!(&word, &expected[index * 11..(index + 1) * 11]);
                } else {
                    let mut word = [0; 7];
                    VoiceDecryptor::for_p25_phase2(algorithm, 0xcafe, &mi, index)
                        .unwrap()
                        .apply_ambe49(&mut word);
                    assert_eq!(&word, &expected[index * 7..(index + 1) * 7]);
                }
            }
            assert!(VoiceDecryptor::for_call(Protocol::P25, algorithm, 0xffff, &mi).is_none());
        }
    }

    #[test]
    fn rc4_matches_the_published_key_plaintext_vector() {
        let mut rc4 = Rc4::new(b"Key");
        let got: Vec<u8> = (0..10).map(|_| rc4.next_byte()).collect();
        assert_eq!(
            got,
            [0xeb, 0x9f, 0x77, 0x81, 0xb7, 0x34, 0xca, 0x72, 0xa7, 0x19]
        );
    }

    #[test]
    fn dmr_iv_expansion_is_stable() {
        assert_eq!(
            expand_dmr_iv([0x12, 0x34, 0x56, 0x78]),
            [
                0x12, 0x34, 0x56, 0x78, 0xb4, 0x51, 0x46, 0x3a, 0x41, 0xd7, 0x89, 0x91, 0xa4, 0x9a,
                0x64, 0x02
            ]
        );
    }

    #[test]
    fn motorola_basic_mask_resets_for_each_ambe_frame() {
        let key = 0x1234u16;
        let mask = (u64::from(key & 0xff0f) << 32)
            .wrapping_add(u64::from(key) << 16)
            .wrapping_add(u64::from(key));
        let make = || VoiceDecryptor {
            stream: Stream::Repeat(Repeat::from_word(mask, 48, true)),
            phase1_position: None,
            current: 0,
            bits_left: 0,
            payload_bits: 48,
            skip_bits: 0,
        };
        let original = [0xa8, 0xb8, 0x08, 0xed, 0x3d, 0x8e, 0x80];
        let mut encrypted = original;
        make().apply_ambe49(&mut encrypted);
        assert_ne!(encrypted, original);
        make().apply_ambe49(&mut encrypted);
        assert_eq!(encrypted, original);
    }

    #[test]
    fn hytera_enhanced_stream_is_continuous_without_dmr_padding_skip() {
        let make = || VoiceDecryptor {
            stream: Stream::Hytera(
                Hytera::new(&[1, 2, 3, 4, 5], &[0xaa, 0xbb, 0xcc, 0xdd, 0xee]).unwrap(),
            ),
            phase1_position: None,
            current: 0,
            bits_left: 0,
            payload_bits: 49,
            skip_bits: 0,
        };
        let originals = [
            [0xa8, 0xb8, 0x08, 0xed, 0x3d, 0x8e, 0x80],
            [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x80],
        ];
        let mut encrypted = originals;
        let mut encryptor = make();
        for frame in &mut encrypted {
            encryptor.apply_ambe49(frame);
        }
        let mut decryptor = make();
        for frame in &mut encrypted {
            decryptor.apply_ambe49(frame);
        }
        assert_eq!(encrypted, originals);
    }

    #[test]
    fn phase_one_imbe_consumes_exactly_eleven_octets_per_frame() {
        let make = || VoiceDecryptor {
            stream: Stream::Repeat(Repeat::from_bytes(&[0x12, 0x34, 0x56], false)),
            phase1_position: None,
            current: 0,
            bits_left: 0,
            payload_bits: 88,
            skip_bits: 0,
        };
        let originals = [[0xa5; 11], [0x5a; 11]];
        let mut encrypted = originals;
        let mut encryptor = make();
        encryptor.apply_imbe88(&mut encrypted[0]);
        encryptor.apply_imbe88(&mut encrypted[1]);
        let mut decryptor = make();
        decryptor.apply_imbe88(&mut encrypted[0]);
        decryptor.apply_imbe88(&mut encrypted[1]);
        assert_eq!(encrypted, originals);
    }

    #[test]
    fn kirisun_universal_matches_an_independent_md2ii_lfsr_vector() {
        let key = [0x33; 32];
        let stream = kirisun_universal(&key, &[0x12, 0x34, 0x56, 0x78]).unwrap();
        assert_eq!(
            &stream[..24],
            &[
                0x6d, 0xde, 0xd4, 0x87, 0x72, 0xbd, 0x8d, 0xb3, 0x34, 0xb4, 0x34, 0x27, 0xb4, 0x73,
                0x9f, 0x75, 0xa6, 0x44, 0xae, 0x8d, 0xd3, 0x53, 0xc2, 0xdd,
            ]
        );
    }

    #[test]
    fn tyt_ep_static_mask_is_stable_and_reversed_keyed() {
        assert_eq!(
            tyt_ep_mask(&[
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ])
            .unwrap(),
            [
                0x89, 0x00, 0x2b, 0x33, 0x5f, 0xf2, 0x81, 0xdd, 0xe1, 0x42, 0x46, 0x97, 0x5d, 0x12,
                0x5f, 0xb7,
            ]
        );
    }
}
