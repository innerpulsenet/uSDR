//! A native AAC decoder: AAC-LC (ISO/IEC 14496-3 subpart 4) and SBR
//! (HE-AAC v1), for both frame lengths, 1024 and the 960 of DRM and DAB+.
//!
//! The decoder takes one raw_data_block() at a time, with no ADTS or LATM
//! header: the transport (ADTS, LATM, a DRM super frame) is the caller's to
//! strip, and the stream parameters it carries arrive once, in `AacConfig`.
//!
//! The chain, per frame:
//!
//! 1. Parse every syntax element up to ID_END: SCE and CPE are decoded
//!    (sections, scalefactors, Huffman spectrum, pulses, inverse
//!    quantization, noise substitution, M/S and intensity stereo, TNS); a
//!    fill element after one carries that element's SBR data; DSE and PCE are
//!    skipped. The core's state does not change until the whole frame has
//!    parsed, so a corrupt frame leaves the next good one to decode normally.
//! 2. IMDCT, windowing and overlap-add, per channel, in 16-bit sample units.
//! 3. With SBR, the core output goes through a 32-band QMF analysis, the
//!    high band is regenerated from the low one and shaped to the
//!    transmitted envelope, and a 64-band QMF synthesis runs at twice the
//!    core rate.
//!
//! Verified against ffmpeg's decoder (see `tests/aac_fixtures.rs`): LC at
//! both frame lengths and HE-AAC agree to float rounding.
//!
//! Not implemented: parametric stereo, MPEG-4's and DRM's (a PS stream
//! plays its mono core on both channels), AAC Main and LTP prediction, SSR
//! gain control, coupling channel elements, RVLC (DRM does not use it) and
//! xHE-AAC/USAC. Streams that need the missing tools return
//! `AacError::Unsupported`.
//!
//! DRM's AAC frames are not raw_data_block()s but error-resilient AAC-LC
//! (HCR, VCB11, their own element order and CRC, SBR bit-reversed at the
//! frame end): `AacDecoder::new_drm` and `decode_drm` take those, and the
//! `drm` module documents exactly what the receiver passes.
//!
//! Lost frames (`conceal`) repeat the last spectrum at falling gain, through
//! the same filterbank so the overlap stays continuous, and go quiet after a
//! few; SBR repeats its last envelope, scaled with the core. Nothing ever
//! plays noise in place of a lost frame.

// Index loops here follow the standard's equations term for term; the
// iterator forms would hide which index is which.
#![allow(clippy::needless_range_loop)]

mod bits;
mod codebooks;
mod drm;
#[cfg(test)]
mod er_tests;
mod filterbank;
#[cfg(test)]
mod fixtures;
mod hcr;
mod huffman;
mod ics;
mod ps;
mod sbr;
mod tables;
mod tns;

pub use drm::{DrmAacConfig, DrmAudioMode};

use bits::BitReader;
use filterbank::{FilterBank, Overlap, EIGHT_SHORT, LONG_START, LONG_STOP, ONLY_LONG};
use ics::{Ics, IcsInfo, INTENSITY_HCB, INTENSITY_HCB2, MAX_SFB, NOISE_HCB};
use tables::BandTables;

/// Stream parameters, from the transport (an AudioSpecificConfig, an ADTS
/// header, or a DRM SDC audio entry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacConfig {
    /// The AAC core's sampling rate: one of the 13 rates AAC defines. DRM
    /// carries 12, 16, 24 or 48 kHz; with SBR the output is twice this.
    pub core_sample_rate_hz: u32,
    /// 1 or 2. A mono core with parametric stereo is 1 here and `ps` set.
    pub channels: u8,
    /// 960 (DRM, DAB+) or 1024 (everything else).
    pub frame_len: u16,
    /// HE-AAC: SBR doubles the output rate.
    pub sbr: bool,
    /// HE-AAC v2 parametric stereo (needs `sbr` and a mono core).
    pub ps: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AacError {
    /// The configuration cannot be decoded (rate, length or channel count).
    Config(&'static str),
    /// The frame ended before its syntax did.
    Truncated,
    /// The frame breaks the syntax or a range the standard sets.
    Invalid(&'static str),
    /// A valid tool or element this decoder does not implement.
    Unsupported(&'static str),
    /// A DRM frame's CRC ("AAC" side information, or "SBR") does not match.
    Crc(&'static str),
}

impl std::fmt::Display for AacError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AacError::Config(s) => write!(f, "AAC config: {s}"),
            AacError::Truncated => write!(f, "AAC frame truncated"),
            AacError::Invalid(s) => write!(f, "invalid AAC frame: {s}"),
            AacError::Unsupported(s) => write!(f, "unsupported AAC feature: {s}"),
            AacError::Crc(s) => write!(f, "DRM {s} CRC mismatch"),
        }
    }
}

impl std::error::Error for AacError {}

const ID_SCE: u32 = 0;
const ID_CPE: u32 = 1;
const ID_CCE: u32 = 2;
const ID_LFE: u32 = 3;
const ID_DSE: u32 = 4;
const ID_PCE: u32 = 5;
const ID_FIL: u32 = 6;
const ID_END: u32 = 7;

const EXT_SBR_DATA: u32 = 13;
const EXT_SBR_DATA_CRC: u32 = 14;

/// Gain per concealed frame, then silence.
const CONCEAL_GAINS: [f32; 4] = [0.7, 0.4, 0.2, 0.0];

/// What parsing a frame's elements found.
struct Parsed {
    /// Channel streams decoded into `AacDecoder::ics` (1 or 2).
    decoded: usize,
    /// They came from a channel pair (for SBR).
    cpe: bool,
    sbr: Option<SbrPayload>,
    /// The noise generator after this frame.
    rng: u32,
    /// Bit position of ID_END.
    #[cfg_attr(not(test), allow(dead_code))]
    end: usize,
}

/// Where a frame's SBR payload sits, to parse once the core has.
struct SbrPayload {
    start: usize,
    bits: usize,
    crc: bool,
}

struct Channel {
    overlap: Overlap,
    /// The last good frame's spectrum and window, for concealment.
    last_spec: Vec<f32>,
    last_info: IcsInfo,
}

pub struct AacDecoder {
    cfg: AacConfig,
    frame_len: usize,
    bands: BandTables,
    fb: FilterBank,
    channels: Vec<Channel>,
    /// Decoded channel streams for the frame being parsed.
    ics: [Ics; 2],
    rng: u32,
    sbr: Option<Box<sbr::Sbr>>,
    /// Channel streams the last good frame carried (1 or 2).
    streams: usize,
    /// Consecutive concealed frames.
    lost: usize,
    /// A configuration that cannot be decoded; every frame reports it.
    bad_config: Option<&'static str>,
    core: [Vec<f32>; 2],
    /// The last channel pair's common_window, ms_mask_present and ms_used
    /// (the tests re-encode frames from them).
    #[cfg_attr(not(test), allow(dead_code))]
    last_cpe: (bool, u32, [[bool; MAX_SFB]; 8]),
    /// DRM frames whose HCR decode left bits or codewords over.
    #[cfg_attr(not(test), allow(dead_code))]
    drm_unclean: usize,
}

impl AacDecoder {
    pub fn new(cfg: AacConfig) -> Self {
        let frame_len = if cfg.frame_len == 960 { 960 } else { 1024 };
        let rate_index = tables::rate_index(cfg.core_sample_rate_hz);
        let mut bad_config = None;
        if rate_index.is_none() {
            bad_config = Some("core sample rate is not an AAC rate");
        } else if cfg.frame_len != 960 && cfg.frame_len != 1024 {
            bad_config = Some("frame length must be 960 or 1024");
        } else if !(1..=2).contains(&cfg.channels) {
            bad_config = Some("1 or 2 channels");
        } else if cfg.ps && (!cfg.sbr || cfg.channels != 1) {
            bad_config = Some("parametric stereo needs SBR and a mono core");
        }
        let rate_index = rate_index.unwrap_or(3);
        let core_channels = usize::from(cfg.channels.clamp(1, 2));
        let sbr = cfg.sbr.then(|| {
            Box::new(sbr::Sbr::new(cfg.core_sample_rate_hz * 2, frame_len, core_channels, cfg.ps))
        });
        Self {
            cfg,
            frame_len,
            bands: BandTables::new(rate_index, frame_len),
            fb: FilterBank::new(frame_len),
            channels: (0..core_channels)
                .map(|_| Channel {
                    overlap: Overlap::new(frame_len),
                    last_spec: vec![0.0; frame_len],
                    last_info: IcsInfo { num_windows: 1, groups: 1, ..Default::default() },
                })
                .collect(),
            ics: [Ics::new(frame_len), Ics::new(frame_len)],
            rng: 0x1f2e_3d4c,
            sbr,
            streams: 1,
            lost: 0,
            bad_config,
            core: [vec![0.0; frame_len], vec![0.0; frame_len]],
            last_cpe: (false, 0, [[false; MAX_SFB]; 8]),
            drm_unclean: 0,
        }
    }

    pub fn output_rate_hz(&self) -> u32 {
        self.cfg.core_sample_rate_hz * if self.cfg.sbr { 2 } else { 1 }
    }

    pub fn output_channels(&self) -> u8 {
        if self.cfg.ps { 2 } else { self.cfg.channels }
    }

    /// Samples per channel in each `decode` or `conceal` result.
    pub fn output_frame_len(&self) -> usize {
        self.frame_len * if self.cfg.sbr { 2 } else { 1 }
    }

    /// Decodes one raw_data_block() (no ADTS/LATM header) to interleaved PCM
    /// in [-1, 1] at the output rate. After an error the decoder's state is
    /// as it was before the call, so the next good frame decodes normally;
    /// call `conceal` to fill the gap.
    pub fn decode(&mut self, frame: &[u8]) -> Result<Vec<f32>, AacError> {
        if let Some(e) = self.bad_config {
            return Err(AacError::Config(e));
        }
        let Parsed { decoded, cpe: element_cpe, sbr: sbr_payload, rng, .. } = self.parse_elements(frame)?;

        // SBR parses into a staged copy, so a bad payload is dropped (the
        // frame keeps its last envelope) without failing the core.
        if let (Some(sbr), Some(p)) = (self.sbr.as_mut(), sbr_payload) {
            let mut sbr_br = BitReader::sub(frame, p.start, p.bits);
            // A bad payload leaves the last good SBR data in force.
            let _ = sbr.parse(&mut sbr_br, element_cpe, p.crc);
        }

        self.rng = rng;
        self.lost = 0;
        self.streams = decoded;
        for (ch, c) in self.channels.iter_mut().enumerate() {
            let ics = &self.ics[ch.min(decoded - 1)];
            c.last_spec.copy_from_slice(&ics.spec[..self.frame_len]);
            c.last_info = ics.info;
        }
        Ok(self.synthesize(1.0))
    }

    /// Parses the syntax elements of one frame into `self.ics`.
    fn parse_elements(&mut self, frame: &[u8]) -> Result<Parsed, AacError> {
        let mut br = BitReader::new(frame);
        let mut p = Parsed { decoded: 0, cpe: false, sbr: None, rng: self.rng, end: frame.len() * 8 };
        loop {
            if br.bits_left() < 3 {
                // A frame cut short of ID_END but complete otherwise.
                break;
            }
            let id_pos = br.position();
            let id = br.read(3);
            match id {
                ID_END => {
                    p.end = id_pos;
                    break;
                }
                ID_SCE | ID_LFE => {
                    br.skip(4);
                    if p.decoded == 0 && id == ID_SCE {
                        let ics = &mut self.ics[0];
                        ics.parse(&mut br, &self.bands, None, &mut p.rng)?;
                        if let Some(tns) = &ics.tns {
                            tns.apply(&ics.info, &self.bands, &mut ics.spec);
                        }
                        p.decoded = 1;
                    } else {
                        // A further channel this decoder does not output.
                        let mut spare = Ics::new(self.frame_len);
                        spare.parse(&mut br, &self.bands, None, &mut p.rng)?;
                    }
                }
                ID_CPE => {
                    br.skip(4);
                    if p.decoded != 0 {
                        return Err(AacError::Unsupported("more than one channel element"));
                    }
                    self.parse_cpe(&mut br, &mut p.rng)?;
                    p.decoded = 2;
                    p.cpe = true;
                }
                ID_CCE => return Err(AacError::Unsupported("coupling channel element")),
                ID_DSE => {
                    br.skip(4);
                    let align = br.bit();
                    let mut count = br.read(8) as usize;
                    if count == 255 {
                        count += br.read(8) as usize;
                    }
                    if align {
                        br.byte_align();
                    }
                    br.skip(8 * count);
                }
                ID_PCE => skip_pce(&mut br)?,
                ID_FIL => {
                    let mut count = br.read(4) as usize;
                    if count == 15 {
                        count += br.read(8) as usize - 1;
                    }
                    if count > 0 {
                        let start = br.position();
                        let ext = br.peek(4);
                        if (ext == EXT_SBR_DATA || ext == EXT_SBR_DATA_CRC) && p.decoded > 0 {
                            p.sbr = Some(SbrPayload {
                                start: start + 4,
                                bits: 8 * count - 4,
                                crc: ext == EXT_SBR_DATA_CRC,
                            });
                        }
                        br.skip(8 * count);
                    }
                }
                _ => unreachable!(),
            }
            if br.overrun() {
                return Err(AacError::Truncated);
            }
        }
        if p.decoded == 0 {
            return Err(AacError::Invalid("no audio channel element"));
        }
        Ok(p)
    }

    /// Output for a lost frame: the last spectrum again at a falling gain,
    /// then silence; SBR repeats its last envelope.
    pub fn conceal(&mut self) -> Vec<f32> {
        if self.bad_config.is_some() {
            return vec![0.0; self.output_frame_len() * usize::from(self.output_channels())];
        }
        let gain = CONCEAL_GAINS[self.lost.min(CONCEAL_GAINS.len() - 1)];
        self.lost += 1;
        for (ch, c) in self.channels.iter_mut().enumerate() {
            let mut info = c.last_info;
            // Keep the window sequence one the filterbank can follow: a start
            // window is closed with a stop window carrying nothing new, short
            // frames repeat as short, anything long repeats as long.
            info.seq = match info.seq {
                LONG_START => {
                    c.last_spec.fill(0.0);
                    LONG_STOP
                }
                EIGHT_SHORT => EIGHT_SHORT,
                _ => ONLY_LONG,
            };
            c.last_info = info;
            let ics = &mut self.ics[ch];
            ics.info = info;
            for (d, s) in ics.spec.iter_mut().zip(&c.last_spec) {
                *d = s * gain;
            }
        }
        self.streams = self.channels.len();
        if let Some(sbr) = self.sbr.as_mut() {
            sbr.conceal();
        }
        self.synthesize(gain)
    }

    /// Filterbank (and SBR) for the channel streams in `self.ics`.
    fn synthesize(&mut self, sbr_gain: f32) -> Vec<f32> {
        let n = self.frame_len;
        let nch = self.channels.len();
        for ch in 0..nch {
            // A mono element feeding a stereo output plays on both sides.
            let ics = &self.ics[ch.min(self.streams - 1)];
            let c = &mut self.channels[ch];
            self.fb.synthesize(&ics.spec, ics.info.seq, ics.info.shape, &mut c.overlap, &mut self.core[ch]);
        }
        let out_ch = usize::from(self.output_channels());
        // 16-bit units to [-1, 1]; a corrupt frame that still parses can
        // carry absurd gains, and clipping keeps that from the speaker.
        let scale = 1.0 / 32768.0;
        if let Some(sbr) = self.sbr.as_mut() {
            let len = 2 * n;
            let mut out = vec![0.0f32; len * out_ch];
            let mut planes = vec![vec![0.0f32; len]; out_ch];
            sbr.process(&self.core[..nch], &mut planes, sbr_gain);
            for (i, frame) in out.chunks_exact_mut(out_ch).enumerate() {
                for (c, s) in frame.iter_mut().enumerate() {
                    *s = (planes[c][i] * scale).clamp(-1.0, 1.0);
                }
            }
            out
        } else {
            let mut out = vec![0.0f32; n * out_ch];
            for (i, frame) in out.chunks_exact_mut(out_ch).enumerate() {
                for (c, s) in frame.iter_mut().enumerate() {
                    *s = (self.core[c.min(nch - 1)][i] * scale).clamp(-1.0, 1.0);
                }
            }
            out
        }
    }

    /// channel_pair_element() after its tag: shared window info, the M/S
    /// mask, both channel streams, then stereo processing and TNS.
    fn parse_cpe(&mut self, br: &mut BitReader, rng: &mut u32) -> Result<(), AacError> {
        let common = br.bit();
        let mut ms_mask = 0;
        let mut ms_used = [[false; MAX_SFB]; 8];
        let mut info = None;
        if common {
            let i = IcsInfo::parse(br, &self.bands)?;
            ms_mask = br.read(2);
            if ms_mask == 3 {
                return Err(AacError::Invalid("reserved ms_mask_present"));
            }
            for row in ms_used.iter_mut().take(i.groups) {
                for used in row.iter_mut().take(i.max_sfb) {
                    *used = ms_mask == 2 || (ms_mask == 1 && br.bit());
                }
            }
            info = Some(i);
        }
        let [l, r] = &mut self.ics;
        l.parse(br, &self.bands, info, rng)?;
        r.parse(br, &self.bands, info, rng)?;
        if common {
            stereo(&self.bands, ms_mask, &ms_used, l, r);
        }
        self.last_cpe = (common, ms_mask, ms_used);
        for ics in [l, r] {
            if let Some(tns) = &ics.tns {
                tns.apply(&ics.info, &self.bands, &mut ics.spec);
            }
        }
        Ok(())
    }
}

/// M/S and intensity stereo on a common-window pair (4.6.8.1, 4.6.8.2).
fn stereo(bands: &BandTables, ms_mask: u32, ms_used: &[[bool; MAX_SFB]; 8], l: &mut Ics, r: &mut Ics) {
    let info = l.info;
    let offsets = bands.offsets(info.short());
    let win_len = usize::from(*offsets.last().unwrap());
    let mut win = 0;
    for g in 0..info.groups {
        let glen = usize::from(info.group_len[g]);
        for b in 0..info.max_sfb {
            let (lo, hi) = (usize::from(offsets[b]), usize::from(offsets[b + 1]));
            let (lt, rt) = (l.band_type[g][b], r.band_type[g][b]);
            let ms = ms_used[g][b];
            if rt == INTENSITY_HCB || rt == INTENSITY_HCB2 {
                let mut sign = if rt == INTENSITY_HCB { 1.0 } else { -1.0 };
                if ms_mask == 1 && ms {
                    sign = -sign;
                }
                let scale = sign * 0.5f32.powf(0.25 * r.sf[g][b] as f32);
                for w in win..win + glen {
                    let base = w * win_len;
                    for k in base + lo..base + hi {
                        r.spec[k] = l.spec[k] * scale;
                    }
                }
            } else if ms && lt == NOISE_HCB && rt == NOISE_HCB {
                // Both sides noise under M/S: the same noise, at the right
                // channel's energy (4.6.13.3).
                let ratio = 2f32.powf(0.25 * (r.sf[g][b] - l.sf[g][b]) as f32);
                for w in win..win + glen {
                    let base = w * win_len;
                    for k in base + lo..base + hi {
                        r.spec[k] = l.spec[k] * ratio;
                    }
                }
            } else if ms && !(NOISE_HCB..=INTENSITY_HCB).contains(&lt) && !(NOISE_HCB..=INTENSITY_HCB).contains(&rt) {
                for w in win..win + glen {
                    let base = w * win_len;
                    for k in base + lo..base + hi {
                        let (m, s) = (l.spec[k], r.spec[k]);
                        l.spec[k] = m + s;
                        r.spec[k] = m - s;
                    }
                }
            }
        }
        win += glen;
    }
}

/// program_config_element(), read only to get past it.
fn skip_pce(br: &mut BitReader) -> Result<(), AacError> {
    br.skip(4 + 2 + 4);
    let front = br.read(4) as usize;
    let side = br.read(4) as usize;
    let back = br.read(4) as usize;
    let lfe = br.read(2) as usize;
    let assoc = br.read(3) as usize;
    let cc = br.read(4) as usize;
    if br.bit() {
        br.skip(4);
    }
    if br.bit() {
        br.skip(4);
    }
    if br.bit() {
        br.skip(3);
    }
    br.skip(5 * (front + side + back) + 4 * (lfe + assoc) + 5 * cc);
    br.byte_align();
    let comment = br.read(8) as usize;
    br.skip(8 * comment);
    if br.overrun() {
        return Err(AacError::Truncated);
    }
    Ok(())
}
