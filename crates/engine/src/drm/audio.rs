//! The audio stream of a service, one logical frame (400 ms) at a time:
//! the text message in its last four bytes, and the audio super frame in
//! the rest, cut into the codec's access units (ES 201 980 clauses 5.3.1,
//! 5.4.1, 6.5).
//!
//! **AAC** (table 10, with the unequal protection layout of earlier
//! versions, which reduces to table 10 when part A is empty): a header of
//! frame borders (12 bits each, byte offsets into the payload, 4 padding
//! bits when there are ten frames), then for each frame its share of the
//! higher protected bytes and its CRC byte, then the rest of each frame.
//! There are 5 frames (12 kHz core) or 10 (24 kHz) of 960 samples. Each
//! frame's 8-bit CRC covers only the first parts of the error-robust frame
//! (mono1 and mono2, or stereo1 to stereo5), whose end only an AAC parser
//! can find, so it is handed on for the AAC decoder to check.
//!
//! **xHE-AAC** (clause 5.3.1.1): audio frames run on across super frames;
//! a 2-byte header (border count, bit reservoir level, CRC-8) and a
//! directory at the end (one 16-bit entry per frame starting here, in
//! reverse order) mark where frames start. Each frame ends in its own
//! CRC-16, checked here.
//!
//! **Text message** (clause 6.5): four bytes per logical frame; four 0xFF
//! bytes start a segment of header, up to 16 bytes of UTF-8 and a CRC-16.
//! Up to eight segments make the message.

use super::coding::{self, Bits};

/// The source coding of an audio service (SDC type 9, audio coding field).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum AudioCodec {
    /// MPEG-4 AAC (ER AAC LC, 960 transform), optionally with SBR and PS.
    Aac,
    /// MPEG-D USAC (xHE-AAC).
    XheAac,
    /// Opus is not a DRM codec; the variant exists for experimental
    /// streams some software signals, but nothing here produces it.
    Opus,
    /// Code 01 in ES 201 980 V2/V3 (reserved since V4).
    Celp,
    /// Code 10 in ES 201 980 V2/V3 (reserved since V4).
    Hvxc,
}

impl AudioCodec {
    pub fn from_code(code: u8) -> AudioCodec {
        match code & 3 {
            0 => AudioCodec::Aac,
            1 => AudioCodec::Celp,
            2 => AudioCodec::Hvxc,
            _ => AudioCodec::XheAac,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            AudioCodec::Aac => "AAC",
            AudioCodec::XheAac => "xHE-AAC",
            AudioCodec::Opus => "Opus",
            AudioCodec::Celp => "CELP",
            AudioCodec::Hvxc => "HVXC",
        }
    }
}

/// One codec access unit cut from an audio super frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioFrame {
    /// AAC: one raw error-robust AAC frame (aac_scalable_main_element of
    /// ISO/IEC 14496-3, with any SBR data at its end read backwards), not
    /// including the CRC byte. xHE-AAC: one USAC access unit (UsacFrame),
    /// its trailing CRC-16 removed.
    pub data: Vec<u8>,
    /// AAC only: the frame's aac_crc_bits, a CRC-8 (G8, preset to ones,
    /// inverted, annex D) over mono1+mono2 or stereo1..stereo5 of the
    /// frame, which the AAC decoder checks once it has parsed that far
    /// (`super::coding::crc8_bits` computes it). `None` for xHE-AAC.
    pub crc: Option<u8>,
    /// AAC: the super frame's frame borders were consistent, so these are
    /// the frame's bytes; the CRC above is still to be checked. xHE-AAC:
    /// the frame's own CRC-16 matched.
    pub ok: bool,
}

/// Cut an AAC audio super frame (`asf`, the logical frame without any text
/// message bytes) of which `len_a` bytes are in the higher protected part.
/// `frames` is 5 or 10. A header whose borders do not add up gives
/// `frames` empty frames marked bad, so the decoder can conceal them.
pub fn split_aac(asf: &[u8], len_a: usize, frames: usize) -> Vec<AudioFrame> {
    let bad = || vec![AudioFrame { data: Vec::new(), crc: None, ok: false }; frames];
    let header = if frames == 10 { 14 } else { (frames - 1) * 12 / 8 };
    if asf.len() < header + frames {
        return bad();
    }
    let payload = asf.len() - header - frames;
    let bits = coding::unpack(&asf[..header]);
    let mut b = Bits::new(&bits);
    let mut lens = Vec::with_capacity(frames);
    let mut prev = 0usize;
    for _ in 0..frames - 1 {
        let mut border = b.take(12) as usize;
        // Borders past 4095 bytes are sent modulo 4096 (table 11 note 2).
        while border < prev {
            border += 4096;
        }
        lens.push(border - prev);
        prev = border;
    }
    if prev >= payload || lens.contains(&0) {
        return bad();
    }
    lens.push(payload - prev);
    let hp = if len_a > header + frames { (len_a - header - frames) / frames } else { 0 };
    if lens.iter().any(|&l| l < hp) || header + frames * (hp + 1) > asf.len() {
        return bad();
    }
    let mut out: Vec<AudioFrame> =
        lens.iter().map(|&l| AudioFrame { data: Vec::with_capacity(l), crc: None, ok: true }).collect();
    let mut pos = header;
    for f in out.iter_mut() {
        f.data.extend_from_slice(&asf[pos..pos + hp]);
        pos += hp;
        f.crc = Some(asf[pos]);
        pos += 1;
    }
    for (f, &l) in out.iter_mut().zip(lens.iter()) {
        f.data.extend_from_slice(&asf[pos..pos + l - hp]);
        pos += l - hp;
    }
    out
}

/// Builds xHE-AAC access units from successive audio super frames.
#[derive(Default)]
pub struct XheAssembler {
    /// Payload bytes from the start of the frame in progress (or, with no
    /// frame start known, the last two bytes of the last payload).
    pending: Vec<u8>,
    /// `pending` starts at a frame border.
    started: bool,
}

impl XheAssembler {
    pub fn reset(&mut self) {
        self.pending.clear();
        self.started = false;
    }

    /// Feed one audio super frame; returns the frames it completes. A super
    /// frame that cannot be read loses the frame in progress.
    pub fn push(&mut self, asf: &[u8]) -> Vec<AudioFrame> {
        let mut out = Vec::new();
        if asf.len() < 4 {
            self.reset();
            return out;
        }
        let header_ok = coding::crc8(&asf[..1]) == asf[1];
        let mut count = (asf[0] >> 4) as usize;
        if !header_ok {
            // The directory repeats the count in every entry; the last two
            // bytes are always an entry when there is one.
            count = (asf[asf.len() - 1] & 15) as usize;
        }
        let dir = asf.len().saturating_sub(2 * count);
        if dir < 2 {
            self.reset();
            return out;
        }
        let payload = &asf[2..dir];
        let mut borders = Vec::with_capacity(count);
        for i in 0..count {
            // Entries are in reverse order: the first border is the last.
            let e = &asf[asf.len() - 2 * (i + 1)..asf.len() - 2 * i];
            let index = ((e[0] as usize) << 4) | (e[1] as usize >> 4);
            if (e[1] & 15) as usize != count {
                self.reset();
                return out;
            }
            borders.push(index);
        }
        if !header_ok && count == 0 {
            self.reset();
            return out;
        }
        // Positions relative to the start of `pending`, which ends where
        // this payload begins.
        let base = self.pending.len();
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(payload);
        let mut last: Option<usize> = if self.started { Some(0) } else { None };
        for &bidx in &borders {
            let pos = match bidx {
                0xFFE => base.checked_sub(2),
                0xFFF => base.checked_sub(1),
                i if i < payload.len() => Some(base + i),
                _ => None,
            };
            let Some(pos) = pos else {
                last = None;
                continue;
            };
            if let Some(l) = last
                && pos > l + 2
            {
                let unit = &data[l..pos];
                let (au, crc) = unit.split_at(unit.len() - 2);
                let ok = coding::crc16(au) == u16::from_be_bytes([crc[0], crc[1]]);
                out.push(AudioFrame { data: au.to_vec(), crc: None, ok });
            }
            last = Some(pos);
        }
        match last {
            Some(l) => {
                self.pending = data[l..].to_vec();
                self.started = true;
            }
            None => {
                // Keep the last two bytes in case the next super frame's
                // first border points back into them.
                self.pending = data[data.len().saturating_sub(2)..].to_vec();
                self.started = false;
            }
        }
        out
    }
}

/// What a text message segment did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextUpdate {
    /// A whole message, newly complete or changed.
    Message(String),
    /// The clear display command.
    Clear,
}

/// Reassembles text messages from the four bytes each logical frame
/// carries.
#[derive(Default)]
pub struct TextAssembler {
    seg: Vec<u8>,
    active: bool,
    toggle: Option<u8>,
    segments: [Option<Vec<u8>>; 8],
    count: Option<usize>,
    last: Option<String>,
    /// Segments whose CRC passed, and failed.
    pub ok: u64,
    pub bad: u64,
}

impl TextAssembler {
    pub fn push(&mut self, piece: &[u8]) -> Option<TextUpdate> {
        if piece.len() != 4 {
            return None;
        }
        if piece.iter().all(|&b| b == 0xFF) {
            self.seg.clear();
            self.active = true;
            return None;
        }
        if !self.active {
            return None;
        }
        self.seg.extend_from_slice(piece);
        if self.seg.len() < 2 {
            return None;
        }
        let h0 = self.seg[0];
        let h1 = self.seg[1];
        let command = h0 & 0x10 != 0;
        let body = if command { 0 } else { (h0 & 15) as usize + 1 };
        if self.seg.len() < 4 + body {
            return None;
        }
        self.active = false;
        let crc = u16::from_be_bytes([self.seg[2 + body], self.seg[3 + body]]);
        if coding::crc16(&self.seg[..2 + body]) != crc {
            self.bad += 1;
            return None;
        }
        self.ok += 1;
        let toggle = h0 >> 7;
        let first = h0 & 0x40 != 0;
        let last = h0 & 0x20 != 0;
        if command {
            return if h0 & 15 == 1 {
                self.segments = Default::default();
                self.count = None;
                self.last = None;
                Some(TextUpdate::Clear)
            } else {
                None
            };
        }
        if self.toggle != Some(toggle) {
            self.segments = Default::default();
            self.count = None;
            self.toggle = Some(toggle);
        }
        let n = if first { 0 } else { (h1 >> 4 & 7) as usize };
        if !first && n == 0 {
            // SegNum 0 is reserved.
            return None;
        }
        let bytes = self.seg[2..2 + body].to_vec();
        if self.segments[n].as_ref().is_some_and(|old| *old != bytes) {
            // A new message under the same toggle: start over.
            self.segments = Default::default();
            self.count = None;
        }
        self.segments[n] = Some(bytes);
        if last {
            self.count = Some(n + 1);
        }
        let count = self.count?;
        if self.segments[..count].iter().any(|s| s.is_none()) {
            return None;
        }
        let all: Vec<u8> = self.segments[..count].iter().flatten().flatten().copied().collect();
        let text = String::from_utf8_lossy(&all).to_string();
        if self.last.as_ref() == Some(&text) {
            return None;
        }
        self.last = Some(text.clone());
        Some(TextUpdate::Message(text))
    }
}

/// Text message pieces for a message, as a transmitter sends them: each
/// segment preceded by four 0xFF bytes and padded to whole pieces.
#[cfg(test)]
pub fn text_pieces(text: &str, toggle: u8) -> Vec<[u8; 4]> {
    let bytes = text.as_bytes();
    let chunks: Vec<&[u8]> = bytes.chunks(16).collect();
    let mut out = Vec::new();
    for (i, c) in chunks.iter().enumerate() {
        out.push([0xFF; 4]);
        let first = i == 0;
        let last = i + 1 == chunks.len();
        let h0 = (toggle << 7) | ((first as u8) << 6) | ((last as u8) << 5) | (c.len() as u8 - 1);
        let h1 = if first { 0xF0 } else { (i as u8) << 4 };
        let mut seg = vec![h0, h1];
        seg.extend_from_slice(c);
        let crc = coding::crc16(&seg);
        seg.extend_from_slice(&crc.to_be_bytes());
        while !seg.len().is_multiple_of(4) {
            seg.push(0);
        }
        for p in seg.chunks(4) {
            out.push([p[0], p[1], p[2], p[3]]);
        }
    }
    out
}
