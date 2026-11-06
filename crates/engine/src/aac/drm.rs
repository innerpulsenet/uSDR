//! DRM's AAC audio frames (ETSI ES 201 980): error-resilient AAC-LC with
//! 960-sample frames, VCB11 and HCR, and SBR stored at the end of the frame.
//!
//! # What the receiver passes
//!
//! Configuration, from the SDC audio information (type 9) of the service:
//! the core sampling rate in Hz (12000, 16000, 24000 or 48000; mapping the
//! SDC's rate field to Hz is the receiver's), the SBR flag, and the audio
//! mode (mono, parametric stereo, stereo). Only AAC: CELP and HVXC services
//! are not for this decoder.
//!
//! One call to `decode_drm(data, crc)` takes one AAC frame of an audio
//! super frame, as the receiver's `drm::audio::split_aac` cuts it:
//!
//! - `data`: the frame's bytes reassembled, its higher-protected bytes
//!   first, then its lower-protected bytes: one contiguous run of the frame
//!   length the super frame's header gives, without the CRC byte;
//! - `crc`: the frame's `aac_crc_bits` byte as transmitted (stored
//!   inverted). Do not check it: it covers the frame only up to the end of
//!   its side information, a bit position only the decoder finds.
//!
//! A frame whose CRC fails returns
//! `AacError::Crc("AAC")` and changes nothing: call `conceal` for it. A bad
//! SBR CRC only drops that frame's SBR data (the last good data stays in
//! force), as for a bad SBR payload in an LC stream.
//!
//! # Frame layout
//!
//! The bits after the CRC byte, for mono and (bracketed) stereo:
//!
//! ```text
//! ics_info without the predictor bit   (shared by both channels)
//! [ms_mask_present, ms_used]
//! tns_data_present, ltp_data_present, side info (channel 1)
//! [tns_data_present, ltp_data_present, side info (channel 2)]
//! tns_data (1), [tns_data (2)]           <- the CRC covers everything to here
//! HCR spectral data (1), [(2)]
//! ... padding ...
//! SBR data, bit-reversed from the last bit of the frame backwards:
//!   SBR CRC (8 bits, inverted), sbr_extension_data() without its type
//! ```
//!
//! Side info is global gain, section data with 5-bit codebooks (VCB11: the
//! codebook 11 family codes one band per section), scalefactors, and the
//! 14-bit length of the reordered spectral data and 6-bit longest codeword.
//! There is no noise substitution, no pulse data and no prediction. Both
//! CRCs are CRC-8, x^8 + x^4 + x^3 + x^2 + 1, preset to all ones.
//!
//! Checked two ways. Off-air frames from Radio Romania International (12 kHz
//! mono core, no SBR; `tests/aac_drm.rs`) all pass their CRC where the
//! capture was clean, fill their byte length to within padding, and decode
//! through HCR with every codeword complete and every bit used. And LC
//! streams this decoder verifies against ffmpeg, re-encoded into this layout
//! (stereo with M/S and intensity, short windows, VCB11, SBR at the end),
//! decode bit for bit as the LC streams do (`er_tests`). Not exercised by
//! either: TNS in a DRM frame, and SBR in a real DRM broadcast. DRM's
//! parametric stereo is not implemented: a PS service plays its mono core on
//! both channels.

use super::bits::BitReader;
use super::ics::{IcsInfo, MAX_SFB};
use super::tns::Tns;
use super::{stereo, AacConfig, AacDecoder, AacError};

/// The service's audio mode (SDC type 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrmAudioMode {
    Mono,
    /// A mono core with DRM parametric stereo (needs SBR).
    ParametricStereo,
    Stereo,
}

/// What a DRM receiver tells the decoder about an AAC service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrmAacConfig {
    /// The AAC core's sampling rate in Hz (with SBR the output is twice it).
    pub core_sample_rate_hz: u32,
    pub sbr: bool,
    pub mode: DrmAudioMode,
}

/// CRC-8 (x^8 + x^4 + x^3 + x^2 + 1, preset all ones) over bits
/// `start..end` of `data`, MSB first.
pub(super) fn crc8(data: &[u8], start: usize, end: usize) -> u8 {
    let mut r: u8 = 0xFF;
    for i in start..end {
        let bit = (data[i / 8] >> (7 - i % 8)) & 1;
        let feedback = (r >> 7) ^ bit;
        r <<= 1;
        if feedback == 1 {
            r ^= 0x1D;
        }
    }
    r
}

impl AacDecoder {
    /// A decoder for a DRM AAC service; frames go to `decode_drm`.
    pub fn new_drm(cfg: DrmAacConfig) -> Self {
        let channels = if cfg.mode == DrmAudioMode::Stereo { 2 } else { 1 };
        AacDecoder::new(AacConfig {
            core_sample_rate_hz: cfg.core_sample_rate_hz,
            channels,
            frame_len: 960,
            sbr: cfg.sbr,
            ps: cfg.sbr && cfg.mode == DrmAudioMode::ParametricStereo,
        })
    }

    /// Decodes one DRM AAC frame (the module documentation says exactly
    /// which bytes) to interleaved PCM in [-1, 1].
    pub fn decode_drm(&mut self, data: &[u8], crc: u8) -> Result<Vec<f32>, AacError> {
        if let Some(e) = self.bad_config {
            return Err(AacError::Config(e));
        }
        if data.is_empty() {
            return Err(AacError::Truncated);
        }
        let crc = !crc;
        let body = data;
        let nch = self.channels.len();
        let mut br = BitReader::new(body);

        let info = IcsInfo::parse_drm(&mut br, &self.bands)?;
        let mut ms_mask = 0;
        let mut ms_used = [[false; MAX_SFB]; 8];
        if nch == 2 {
            ms_mask = br.read(2);
            if ms_mask == 3 {
                return Err(AacError::Invalid("reserved ms_mask_present"));
            }
            for row in ms_used.iter_mut().take(info.groups) {
                for used in row.iter_mut().take(info.max_sfb) {
                    *used = ms_mask == 2 || (ms_mask == 1 && br.bit());
                }
            }
        }
        let mut lens = [(0, 0); 2];
        let mut tns_present = [false; 2];
        for ch in 0..nch {
            tns_present[ch] = br.bit();
            if br.bit() {
                return Err(AacError::Unsupported("long-term prediction"));
            }
            lens[ch] = self.ics[ch].parse_er_side(&mut br, info)?;
        }
        for ch in 0..nch {
            if tns_present[ch] {
                self.ics[ch].tns = Some(Tns::parse(&mut br, &info)?);
            }
        }
        if br.overrun() {
            return Err(AacError::Truncated);
        }
        if crc8(body, 0, br.position()) != crc {
            return Err(AacError::Crc("AAC"));
        }
        let max_len = if nch == 2 { 6144 } else { 12288 };
        let mut clean = true;
        for ch in 0..nch {
            let (len, longest) = lens[ch];
            clean &= self.ics[ch].parse_hcr(&mut br, body, &self.bands, len.min(max_len), longest)?;
        }
        // The spectral data is outside the CRC; an HCR decode that leaves
        // bits or codewords over is the only sign of errors there. The frame
        // still plays (other decoders do the same).
        if !clean {
            self.drm_unclean += 1;
        }
        if br.overrun() {
            return Err(AacError::Truncated);
        }
        if nch == 2 {
            let [l, r] = &mut self.ics;
            stereo(&self.bands, ms_mask, &ms_used, l, r);
        }
        for ics in self.ics.iter_mut().take(nch) {
            if let Some(tns) = &ics.tns {
                tns.apply(&ics.info, &self.bands, &mut ics.spec);
            }
        }

        // SBR, read from the last bit of the frame backwards.
        if let Some(sbr) = self.sbr.as_mut() {
            let total = body.len() * 8;
            let aac_end = br.position();
            if total >= aac_end + 8 {
                let rev: Vec<u8> = body.iter().rev().map(|b| b.reverse_bits()).collect();
                let mut sbr_br = BitReader::sub(&rev, 0, total - aac_end);
                let sbr_crc = !(sbr_br.read(8) as u8);
                // A bad payload keeps the last good SBR data in force.
                let _ = sbr.parse_checked(&mut sbr_br, nch == 2, |end| crc8(&rev, 8, end) == sbr_crc);
            }
        }

        self.lost = 0;
        self.streams = nch;
        for (ch, c) in self.channels.iter_mut().enumerate() {
            let ics = &self.ics[ch];
            c.last_spec.copy_from_slice(&ics.spec[..self.frame_len]);
            c.last_info = ics.info;
        }
        Ok(self.synthesize(1.0))
    }
}
