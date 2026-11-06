//! Round trips through the DRM error-resilient path: LC frames this decoder
//! verifies against ffmpeg are parsed, their quantized spectra and side
//! information re-encoded as DRM frames (VCB11 sections, HCR spectral data,
//! the DRM element order and CRC, SBR bit-reversed at the end), and the DRM
//! decode must equal the LC decode of the same spectra bit for bit.
//!
//! The 48 kHz stereo fixture is the source: its frames parse as 960-sample
//! frames (see `fixtures::latm_960`), and it has short windows with
//! grouping, TNS, M/S and intensity stereo, and codebook 11 bands that turn
//! into virtual codebooks. A test encoder shares my reading of HCR with the
//! decoder, so these runs check the decoder's bookkeeping and that both ends
//! agree; that the reading matches real DRM is what the off-air frames in
//! `tests/aac_drm.rs` check.

use super::codebooks::{SCALEFACTOR, SPECTRUM};
use super::drm::crc8;
use super::filterbank::EIGHT_SHORT;
use super::fixtures::{BitWriter, SbrGen, adts_split, with_fill};
use super::hcr::{self, MAX_CW_LEN, tuple_len};
use super::ics::{
    ESC_HCB, INTENSITY_HCB, INTENSITY_HCB2, IcsInfo, MAX_SFB, VCB11_LAV, is_spectral, real_book,
};
use super::tables::BandTables;
use super::tns::Tns;
use super::{AacConfig, AacDecoder, DrmAacConfig, DrmAudioMode};

/// One parsed channel stream, as the encoders need it.
#[derive(Clone)]
struct Channel {
    info: IcsInfo,
    band_type: [[u8; MAX_SFB]; 8],
    sf: [[i32; MAX_SFB]; 8],
    global_gain: i32,
    tns: Option<Tns>,
    quant: Vec<i32>,
}

/// One frame's worth: one or two channels and the pair's M/S info.
struct Frame {
    /// The pair shares one window (DRM stereo can carry nothing else).
    common: bool,
    ch: Vec<Channel>,
    ms_mask: u32,
    ms_used: [[bool; MAX_SFB]; 8],
}

/// The fixture's frames as parsed with 960-sample tables.
fn source_frames(n: usize) -> Vec<Frame> {
    let data = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/aac/music_stereo48.aac"),
    )
    .unwrap();
    let mut dec = AacDecoder::new(AacConfig {
        core_sample_rate_hz: 48000,
        channels: 2,
        frame_len: 960,
        sbr: false,
        ps: false,
    });
    adts_split(&data)
        .into_iter()
        .take(n)
        .map(|(_, payload)| {
            dec.parse_elements(&payload).unwrap();
            let (common, ms_mask, ms_used) = dec.last_cpe;
            let ch = dec
                .ics
                .iter()
                .map(|i| Channel {
                    info: i.info,
                    band_type: i.band_type,
                    sf: i.sf,
                    global_gain: i.global_gain,
                    tns: i.tns.clone(),
                    quant: i.quant.clone(),
                })
                .collect();
            Frame {
                common,
                ch,
                ms_mask,
                ms_used,
            }
        })
        .collect()
}

/// The spectral codeword (Huffman word, signs, escapes) for one tuple.
fn tuple_bits(cb: u8, v: &[i32]) -> Vec<bool> {
    let real = real_book(cb);
    let (unsigned, lav) = match real {
        1 | 2 => (false, 1),
        3 | 4 => (true, 2),
        5 | 6 => (false, 4),
        7 | 8 => (true, 7),
        9 | 10 => (true, 12),
        _ => (true, 16),
    };
    let modulo = if unsigned { lav + 1 } else { 2 * lav + 1 };
    let mut sym = 0;
    for &x in v {
        let c = if unsigned { x.abs().min(lav) } else { x + lav };
        sym = sym * modulo + c;
    }
    let (code, len) = SPECTRUM[usize::from(real) - 1][sym as usize];
    let mut out: Vec<bool> = (0..len).rev().map(|i| (code >> i) & 1 == 1).collect();
    if unsigned {
        out.extend(v.iter().filter(|&&x| x != 0).map(|&x| x < 0));
        if real == ESC_HCB {
            for &x in v {
                let a = x.abs();
                if a >= 16 {
                    let n = 31 - (a as u32).leading_zeros(); // floor(log2)
                    out.extend(std::iter::repeat_n(true, (n - 4) as usize));
                    out.push(false);
                    out.extend((0..n).rev().map(|i| ((a - (1 << n)) >> i) & 1 == 1));
                }
            }
        }
    }
    out
}

fn put_bits(w: &mut BitWriter, bits: &[bool]) {
    for &b in bits {
        w.put(u32::from(b), 1);
    }
}

/// Codebook 11 bands moved to the smallest virtual codebook that holds
/// their values and codewords.
fn with_vcb11(c: &Channel, bands: &BandTables) -> Channel {
    let mut c = c.clone();
    let offsets = bands.offsets(c.info.short()).to_vec();
    let win_len = usize::from(*offsets.last().unwrap());
    let mut win = 0;
    for g in 0..c.info.groups {
        let glen = usize::from(c.info.group_len[g]);
        for b in 0..c.info.max_sfb {
            if c.band_type[g][b] != ESC_HCB {
                continue;
            }
            let mut peak = 0;
            let mut longest = 0;
            for w in win..win + glen {
                for k in (usize::from(offsets[b])..usize::from(offsets[b + 1])).step_by(2) {
                    let v = &c.quant[w * win_len + k..w * win_len + k + 2];
                    peak = peak.max(v[0].abs()).max(v[1].abs());
                    longest = longest.max(tuple_bits(ESC_HCB, v).len());
                }
            }
            if let Some(i) = (0..16)
                .find(|&i| VCB11_LAV[i] >= peak && usize::from(MAX_CW_LEN[16 + i]) >= longest)
            {
                c.band_type[g][b] = 16 + i as u8;
            }
        }
        win += glen;
    }
    c
}

/// Codewords in bitstream (LC) order or HCR order.
fn codewords(c: &Channel, bands: &BandTables, hcr_order: bool) -> Vec<(u8, Vec<bool>)> {
    if hcr_order {
        return hcr::codeword_order(&c.info, &c.band_type, bands)
            .into_iter()
            .map(|(cb, at)| (cb, tuple_bits(cb, &c.quant[at..at + tuple_len(cb)])))
            .collect();
    }
    let offsets = bands.offsets(c.info.short());
    let win_len = usize::from(*offsets.last().unwrap());
    let mut out = Vec::new();
    let mut win = 0;
    for g in 0..c.info.groups {
        let glen = usize::from(c.info.group_len[g]);
        for b in 0..c.info.max_sfb {
            let cb = c.band_type[g][b];
            if !is_spectral(cb) {
                continue;
            }
            for w in win..win + glen {
                for k in
                    (usize::from(offsets[b])..usize::from(offsets[b + 1])).step_by(tuple_len(cb))
                {
                    let at = w * win_len + k;
                    out.push((cb, tuple_bits(cb, &c.quant[at..at + tuple_len(cb)])));
                }
            }
        }
        win += glen;
    }
    out
}

/// The encoder half of HCR: priority codewords at the segment starts, the
/// rest in sets, odd sets from the far end of each segment backwards.
fn hcr_arrange(cws: &[(u8, Vec<bool>)], longest: usize) -> Vec<bool> {
    let len: usize = cws.iter().map(|c| c.1.len()).sum();
    let order: Vec<(u8, usize)> = cws.iter().map(|c| (c.0, 0)).collect();
    let mut segs = hcr::segments(&order, len, longest);
    let mut buf = vec![false; len];
    for (s, seg) in segs.iter_mut().enumerate() {
        let bits = &cws[s].1;
        buf[seg.0..seg.0 + bits.len()].copy_from_slice(bits);
        seg.0 += bits.len();
    }
    let n_seg = segs.len();
    let rest = &cws[n_seg..];
    let mut placed = vec![0usize; rest.len()];
    for set in 0..rest.len().div_ceil(n_seg) {
        let backward = set % 2 == 0;
        for trial in 0..n_seg {
            for c in 0..n_seg {
                let idx = set * n_seg + c;
                if idx >= rest.len() {
                    break;
                }
                let seg = &mut segs[(trial + c) % n_seg];
                let bits = &rest[idx].1;
                let n = (bits.len() - placed[idx]).min(seg.1 - seg.0);
                for &b in &bits[placed[idx]..placed[idx] + n] {
                    if backward {
                        seg.1 -= 1;
                        buf[seg.1] = b;
                    } else {
                        buf[seg.0] = b;
                        seg.0 += 1;
                    }
                }
                placed[idx] += n;
            }
        }
    }
    assert!(
        placed.iter().zip(rest).all(|(p, c)| *p == c.1.len()),
        "HCR could not place every codeword"
    );
    buf
}

fn write_info(w: &mut BitWriter, info: &IcsInfo, predictor_bit: bool) {
    w.put(0, 1);
    w.put(u32::from(info.seq), 2);
    w.put(u32::from(info.shape), 1);
    if info.seq == EIGHT_SHORT {
        w.put(info.max_sfb as u32, 4);
        let mut grouping = 0;
        let mut win = 0;
        for g in 0..info.groups {
            for k in 0..usize::from(info.group_len[g]) {
                if win > 0 && k > 0 {
                    grouping |= 1 << (7 - win);
                }
                win += 1;
            }
        }
        w.put(grouping, 7);
    } else {
        w.put(info.max_sfb as u32, 6);
        if predictor_bit {
            w.put(0, 1);
        }
    }
}

fn write_sections(w: &mut BitWriter, c: &Channel, er: bool) {
    let bits = if c.info.short() { 3 } else { 5 };
    let esc = (1u32 << bits) - 1;
    for g in 0..c.info.groups {
        let mut k = 0;
        while k < c.info.max_sfb {
            let cb = c.band_type[g][k];
            w.put(u32::from(cb), if er { 5 } else { 4 });
            if er && (cb == ESC_HCB || cb >= 16) {
                k += 1;
                continue;
            }
            let mut n = 1;
            while k + n < c.info.max_sfb && c.band_type[g][k + n] == cb {
                n += 1;
            }
            let mut left = n as u32;
            while left >= esc {
                w.put(esc, bits);
                left -= esc;
            }
            w.put(left, bits);
            k += n;
        }
    }
}

fn write_scalefactors(w: &mut BitWriter, c: &Channel) {
    let mut sf = c.global_gain;
    let mut is_pos = 0;
    for g in 0..c.info.groups {
        for b in 0..c.info.max_sfb {
            let cb = c.band_type[g][b];
            let delta = if cb == INTENSITY_HCB || cb == INTENSITY_HCB2 {
                let d = c.sf[g][b] - is_pos;
                is_pos = c.sf[g][b];
                d
            } else if is_spectral(cb) {
                let d = c.sf[g][b] - sf;
                sf = c.sf[g][b];
                d
            } else {
                continue;
            };
            let (code, len) = SCALEFACTOR[(delta + 60) as usize];
            w.put(code, u32::from(len));
        }
    }
}

fn write_tns(w: &mut BitWriter, info: &IcsInfo, tns: &Tns) {
    let short = info.short();
    let (n_bits, len_bits, ord_bits) = if short { (1, 4, 3) } else { (2, 6, 5) };
    for win in 0..info.num_windows {
        let n = usize::from(tns.count[win]);
        w.put(n as u32, n_bits);
        if n == 0 {
            continue;
        }
        w.put(u32::from(tns.coef_res[win]), 1);
        for f in &tns.filters[win][..n] {
            w.put(u32::from(f.length()), len_bits);
            w.put(u32::from(f.order()), ord_bits);
            if f.order() > 0 {
                w.put(u32::from(f.downward()), 1);
                w.put(u32::from(f.compress), 1);
                let bits = u32::from(tns.coef_res[win]) + 3 - u32::from(f.compress);
                for &coef in &f.coef[..usize::from(f.order())] {
                    w.put(u32::from(coef), bits);
                }
            }
        }
    }
}

/// An LC single channel element of one channel, then (optionally) an SBR
/// fill element.
fn lc_sce(c: &Channel, bands: &BandTables) -> (Vec<u8>, usize) {
    let mut w = BitWriter::new();
    w.put(0, 3); // ID_SCE
    w.put(0, 4);
    w.put(c.global_gain as u32, 8);
    write_info(&mut w, &c.info, true);
    write_sections(&mut w, c, false);
    write_scalefactors(&mut w, c);
    w.put(0, 1); // no pulses
    w.put(u32::from(c.tns.is_some()), 1);
    if let Some(t) = &c.tns {
        write_tns(&mut w, &c.info, t);
    }
    w.put(0, 1); // no gain control
    for (_, bits) in codewords(c, bands, false) {
        put_bits(&mut w, &bits);
    }
    let end = w.bits;
    w.put(7, 3); // ID_END
    w.align();
    (w.bytes, end)
}

/// A DRM frame of one or two channels, `sbr` (the payload as read, in
/// order) stored bit-reversed at the end. Returns (data, crc byte).
fn drm_frame(
    ch: &[Channel],
    ms_mask: u32,
    ms_used: &[[bool; MAX_SFB]; 8],
    bands: &BandTables,
    sbr: Option<&BitWriter>,
) -> (Vec<u8>, u8) {
    let mut w = BitWriter::new();
    let info = ch[0].info;
    write_info(&mut w, &info, false);
    if ch.len() == 2 {
        w.put(ms_mask, 2);
        if ms_mask == 1 {
            for row in ms_used.iter().take(info.groups) {
                for &u in row.iter().take(info.max_sfb) {
                    w.put(u32::from(u), 1);
                }
            }
        }
    }
    let mut spectra = Vec::new();
    for c in ch {
        let cws = codewords(c, bands, true);
        let longest = cws.iter().map(|c| c.1.len()).max().unwrap_or(0);
        let data = if cws.is_empty() {
            Vec::new()
        } else {
            hcr_arrange(&cws, longest)
        };
        w.put(u32::from(c.tns.is_some()), 1);
        w.put(0, 1); // no LTP
        w.put(c.global_gain as u32, 8);
        write_sections(&mut w, c, true);
        write_scalefactors(&mut w, c);
        w.put(data.len() as u32, 14);
        w.put(longest as u32, 6);
        spectra.push(data);
    }
    for c in ch {
        if let Some(t) = &c.tns {
            write_tns(&mut w, &c.info, t);
        }
    }
    let crc = !crc8(&w.bytes, 0, w.bits);
    for s in &spectra {
        put_bits(&mut w, s);
    }
    if let Some(p) = sbr {
        let mut tail = BitWriter::new();
        tail.put(0, 8); // CRC placeholder
        tail.put_bits_from(&p.bytes, p.bits);
        let c = !crc8(&tail.bytes, 8, tail.bits);
        tail.bytes[0] = c;
        let bits: Vec<bool> = (0..tail.bits)
            .map(|i| (tail.bytes[i / 8] >> (7 - i % 8)) & 1 == 1)
            .collect();
        while !(w.bits + bits.len()).is_multiple_of(8) {
            w.put(0, 1);
        }
        for &b in bits.iter().rev() {
            w.put(u32::from(b), 1);
        }
    }
    w.align();
    (w.bytes, crc)
}

#[test]
fn longest_codewords_match_the_codebooks_and_vcb11_limits() {
    for cb in (1..=11u8).chain(16..=31) {
        let lav = if cb >= 16 {
            VCB11_LAV[usize::from(cb - 16)]
        } else {
            [0, 1, 1, 2, 2, 4, 4, 7, 7, 12, 12, 8191][usize::from(cb)]
        };
        let signed = matches!(cb, 1 | 2 | 5 | 6);
        let dim = tuple_len(cb);
        // Every tuple of values up to the limit (for escape books, the
        // limit itself stands for every escaped value, which all code
        // longest at the top of their escape length).
        let reps: Vec<i32> = if real_book(cb) == ESC_HCB {
            let mut r: Vec<i32> = (0..16).collect();
            let mut v = 16;
            while v <= lav {
                let top = ((v * 2) - 1).min(lav);
                r.push(top);
                v *= 2;
            }
            r
        } else if signed {
            (-lav..=lav).collect()
        } else {
            (0..=lav).collect()
        };
        let mut longest = 0;
        let mut tuple = vec![0i32; dim];
        let total = reps.len().pow(dim as u32);
        for n in 0..total {
            let mut k = n;
            for t in tuple.iter_mut() {
                *t = reps[k % reps.len()];
                k /= reps.len();
                if !signed && *t != 0 {
                    // Sign bits cost one per nonzero value; use negatives.
                    *t = -*t;
                }
            }
            longest = longest.max(tuple_bits(cb, &tuple).len());
        }
        // The table's figure for the unsigned quad books is the longest
        // Huffman word plus four sign bits, one more than any actual
        // codeword; segment widths follow the table.
        let slack = usize::from(matches!(cb, 3 | 4));
        assert_eq!(
            longest + slack,
            usize::from(MAX_CW_LEN[usize::from(cb)]),
            "codebook {cb}"
        );
    }
}

#[test]
fn hcr_round_trips_every_frame_of_the_fixture() {
    let bands = BandTables::new(3, 960);
    let mut virtual_bands = 0;
    for (n, f) in source_frames(20).iter().enumerate() {
        for c in &f.ch {
            let c = with_vcb11(c, &bands);
            virtual_bands += c.band_type.iter().flatten().filter(|&&b| b >= 16).count();
            let cws = codewords(&c, &bands, true);
            if cws.is_empty() {
                continue;
            }
            let longest = cws.iter().map(|x| x.1.len()).max().unwrap();
            let bits = hcr_arrange(&cws, longest);
            let mut w = BitWriter::new();
            put_bits(&mut w, &bits);
            w.put(0, 8);
            let order = hcr::codeword_order(&c.info, &c.band_type, &bands);
            let mut quant = vec![0i32; 1024];
            hcr::decode(&w.bytes, 0, bits.len(), longest, &order, &mut quant).unwrap();
            assert_eq!(&quant[..960], &c.quant[..960], "frame {n}");
        }
    }
    assert!(virtual_bands > 0, "no band exercised VCB11");
}

#[test]
fn drm_stereo_frames_decode_exactly_as_the_lc_frames() {
    let bands = BandTables::new(3, 960);
    // Frames with a transient on one side only have separate windows, which
    // DRM stereo cannot carry: both decoders get the others only.
    let frames = source_frames(20);
    assert!(
        frames.iter().filter(|f| f.common).count() >= 8,
        "too few common-window frames"
    );
    assert!(
        frames.iter().any(|f| f.ms_mask != 0),
        "no M/S in the source"
    );
    assert!(
        frames.iter().any(|f| f.ch[1]
            .band_type
            .iter()
            .flatten()
            .any(|&b| b == INTENSITY_HCB)),
        "no intensity"
    );
    let data = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/aac/music_stereo48.aac"),
    )
    .unwrap();
    let mut lc = AacDecoder::new(AacConfig {
        core_sample_rate_hz: 48000,
        channels: 2,
        frame_len: 960,
        sbr: false,
        ps: false,
    });
    let mut drm = AacDecoder::new_drm(DrmAacConfig {
        core_sample_rate_hz: 48000,
        sbr: false,
        mode: DrmAudioMode::Stereo,
    });
    for (n, ((_, payload), f)) in adts_split(&data).iter().zip(&frames).enumerate() {
        if !f.common {
            continue;
        }
        let want = lc.decode(payload).unwrap();
        let ch: Vec<Channel> = f.ch.iter().map(|c| with_vcb11(c, &bands)).collect();
        let (frame, crc) = drm_frame(&ch, f.ms_mask, &f.ms_used, &bands, None);
        let got = drm
            .decode_drm(&frame, crc)
            .unwrap_or_else(|e| panic!("frame {n}: {e}"));
        assert_eq!(got, want, "frame {n}");
    }
    assert_eq!(drm.drm_unclean, 0);
}

/// Mono, with SBR: the left channel of each frame as an LC single channel
/// element with an SBR fill element, and as a DRM frame carrying the same
/// SBR payload bit-reversed at its end, decode alike. Also checks that a
/// corrupted CRC fails the frame and a corrupted SBR CRC drops only SBR.
#[test]
fn drm_mono_sbr_frames_decode_exactly_as_lc_with_fill_elements() {
    let bands = BandTables::new(3, 960);
    let frames = source_frames(16);
    assert!(
        frames.iter().any(|f| f.ch[0].info.short()),
        "no short windows in the source"
    );
    // No TNS: neither this source nor the off-air frames use it, so the TNS
    // placement in DRM frames goes untested.
    // Each side of the pair in turn, as a mono stream.
    for side in 0..2 {
        let cfg = AacConfig {
            core_sample_rate_hz: 48000,
            channels: 1,
            frame_len: 960,
            sbr: true,
            ps: false,
        };
        let mut lc = AacDecoder::new(cfg);
        let mut drm = AacDecoder::new_drm(DrmAacConfig {
            core_sample_rate_hz: 48000,
            sbr: true,
            mode: DrmAudioMode::Mono,
        });
        let mut sg = SbrGen::new(3, 96000, 1);
        for (n, f) in frames.iter().enumerate() {
            let c = &f.ch[side];
            let p = sg.payload(false);
            let (sce, end) = lc_sce(c, &bands);
            let want = lc.decode(&with_fill(&sce, end, &p, false)).unwrap();
            let (frame, crc) = drm_frame(&[with_vcb11(c, &bands)], 0, &f.ms_used, &bands, Some(&p));
            if n == 5 {
                // A bad AAC CRC fails the frame without touching the decoder.
                assert!(matches!(
                    drm.decode_drm(&frame, crc ^ 1),
                    Err(super::AacError::Crc("AAC"))
                ));
            }
            let got = drm
                .decode_drm(&frame, crc)
                .unwrap_or_else(|e| panic!("frame {n}: {e}"));
            assert_eq!(got, want, "side {side} frame {n}");
        }
        assert_eq!(drm.drm_unclean, 0);
    }
}

/// The off-air frames (see `tests/aac_drm.rs`): every frame that decodes
/// must also decode cleanly through HCR (every codeword complete, every bit
/// used), the check the frame CRC cannot give for the spectral data. Files 1
/// and 3 are clean throughout; file 2 has a burst of bit errors (frames 16
/// to 28 fail their CRC) and four frames near it whose HCR is not clean.
#[test]
fn off_air_frames_decode_cleanly_through_hcr() {
    for (name, unclean_allowed) in [("rri9570-1", 0), ("rri9570-2", 4), ("rri9570-3", 0)] {
        let d = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("tests/data/aac/drm/{name}.frames")),
        )
        .unwrap();
        let mut dec = AacDecoder::new_drm(DrmAacConfig {
            core_sample_rate_hz: 12000,
            sbr: false,
            mode: DrmAudioMode::Mono,
        });
        let mut i = 0;
        let (mut ok, mut failed) = (0, 0);
        while i < d.len() {
            i += 8;
            i += 2 + usize::from(u16::from_le_bytes([d[i], d[i + 1]]));
            let n = u16::from_le_bytes([d[i], d[i + 1]]);
            i += 2;
            for _ in 0..n {
                let crc = d[i + 2];
                let len = usize::from(u16::from_le_bytes([d[i + 3], d[i + 4]]));
                i += 5;
                match dec.decode_drm(&d[i..i + len], crc) {
                    Ok(_) => ok += 1,
                    Err(_) => failed += 1,
                }
                i += len;
            }
        }
        assert_eq!(ok + failed, 190, "{name}");
        assert!(
            dec.drm_unclean <= unclean_allowed,
            "{name}: {} unclean HCR decodes",
            dec.drm_unclean
        );
        assert!(failed <= unclean_allowed, "{name}: {failed} frames failed");
    }
}
