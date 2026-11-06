//! AAC decoding checked against ffmpeg's own decode of the same streams.
//!
//! The fixtures in `tests/data/aac/` were made offline with ffmpeg 8.0 and
//! decoded by its native decoder to 16-bit PCM; these tests never run
//! ffmpeg. The sources are synthetic (a flite voice, generated chords and
//! clicks, pink noise), so they carry no license terms.
//!
//! ```text
//! speech_mono24    flite "slt" voice, 24 kHz mono, 32 kb/s, PNS off
//! music_mono24     chord with vibrato and clicks, 24 kHz mono, 40 kb/s, PNS off
//! music_stereo48   different content per side, 48 kHz stereo, 96 kb/s, PNS off
//!                  (M/S and intensity stereo on, as ffmpeg defaults)
//! pns_mono24       pink noise, 24 kHz mono, 24 kb/s, PNS on
//! music_stereo24   tones and clicks, 24 kHz stereo, 48 kb/s (an SBR core)
//! he_mono48        speech_mono24 with synthetic SBR data, 48 kHz out
//! he_stereo48      music_stereo24 with synthetic SBR data (coupled and not)
//! lc960_stereo48   music_stereo48 rewrapped in LATM as 960-sample frames
//! ```
//!
//! The LC streams come from `ffmpeg -i in.wav -c:a aac -b:a 32k -aac_pns 0
//! -f adts out.aac`; the HE-AAC and 960 ones from the generator in
//! `src/aac/fixtures.rs` (its module comment says how they are built and why
//! ffmpeg can decode them). References: `ffmpeg -i x.aac -f s16le -acodec
//! pcm_s16le x.s16` (`-f loas` before `-i` for the LATM one; for he_mono48,
//! which ffmpeg plays as stereo, `-af "pan=mono|c0=c0"`).

use scannerd_engine::aac::{AacConfig, AacDecoder};

const RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

fn read(name: &str) -> Vec<u8> {
    std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/aac/").to_owned() + name).unwrap()
}

/// Splits an ADTS stream into (config, raw_data_block payloads).
fn adts_frames(data: &[u8]) -> (AacConfig, Vec<Vec<u8>>) {
    let mut frames = Vec::new();
    let mut cfg = None;
    let mut i = 0;
    while i + 7 <= data.len() {
        let h = &data[i..i + 7];
        assert_eq!((h[0], h[1] & 0xF6), (0xFF, 0xF0), "ADTS sync at {i}");
        assert_eq!(h[2] >> 6, 1, "AAC-LC");
        let sf = usize::from((h[2] >> 2) & 0xF);
        let channels = ((h[2] & 1) << 2) | (h[3] >> 6);
        let len = (usize::from(h[3] & 3) << 11) | (usize::from(h[4]) << 3) | usize::from(h[5] >> 5);
        assert_eq!(h[6] & 3, 0, "one raw_data_block per ADTS frame");
        let hdr = if h[1] & 1 == 1 { 7 } else { 9 };
        frames.push(data[i + hdr..i + len].to_vec());
        cfg.get_or_insert(AacConfig { core_sample_rate_hz: RATES[sf], channels, frame_len: 1024, sbr: false, ps: false });
        i += len;
    }
    (cfg.expect("at least one frame"), frames)
}

/// Splits the LOAS/LATM fixture (a full StreamMuxConfig in every frame, one
/// AAC-LC payload) into (config, payloads).
fn latm_frames(data: &[u8]) -> (AacConfig, Vec<Vec<u8>>) {
    struct Bits<'a>(&'a [u8], usize);
    impl Bits<'_> {
        fn get(&mut self, n: usize) -> u32 {
            let mut v = 0;
            for _ in 0..n {
                v = (v << 1) | u32::from((self.0[self.1 / 8] >> (7 - self.1 % 8)) & 1);
                self.1 += 1;
            }
            v
        }
    }
    let mut frames = Vec::new();
    let mut cfg = None;
    let mut i = 0;
    while i + 3 <= data.len() {
        assert_eq!(data[i], 0x56, "LOAS sync");
        let len = (usize::from(data[i + 1] & 0x1F) << 8) | usize::from(data[i + 2]);
        let mut b = Bits(&data[i + 3..i + 3 + len], 0);
        assert_eq!(b.get(1), 0, "a StreamMuxConfig in every frame");
        b.get(1 + 1 + 6 + 4 + 3);
        assert_eq!(b.get(5), 2, "AAC-LC");
        let sf = b.get(4) as usize;
        let channels = b.get(4) as u8;
        let frame_len = if b.get(1) == 1 { 960 } else { 1024 };
        b.get(2 + 3 + 8 + 1 + 1);
        let mut payload_len = 0;
        loop {
            let t = b.get(8) as usize;
            payload_len += t;
            if t != 255 {
                break;
            }
        }
        frames.push((0..payload_len).map(|_| b.get(8) as u8).collect());
        cfg.get_or_insert(AacConfig { core_sample_rate_hz: RATES[sf], channels, frame_len, sbr: false, ps: false });
        i += 3 + len;
    }
    (cfg.expect("at least one frame"), frames)
}

fn reference(name: &str) -> Vec<f32> {
    read(&format!("{name}.s16")).chunks_exact(2).map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0).collect()
}

fn decode_frames(name: &str, cfg: AacConfig, frames: &[Vec<u8>]) -> Vec<f32> {
    let mut dec = AacDecoder::new(cfg);
    let mut out = Vec::new();
    for (n, f) in frames.iter().enumerate() {
        let pcm = dec.decode(f).unwrap_or_else(|e| panic!("{name} frame {n}: {e}"));
        assert_eq!(pcm.len(), dec.output_frame_len() * usize::from(dec.output_channels()));
        out.extend(pcm);
    }
    if let Some(dir) = std::env::var_os("AAC_DUMP_DIR") {
        // For comparing against a float reference offline.
        let bytes: Vec<u8> = out.iter().flat_map(|v| v.to_le_bytes()).collect();
        std::fs::write(std::path::Path::new(&dir).join(format!("{name}.f32")), bytes).unwrap();
    }
    out
}

/// SNR of `out` against `reference` in dB, over the whole signal.
fn snr_db(out: &[f32], reference: &[f32]) -> f64 {
    assert_eq!(out.len(), reference.len());
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for (&o, &r) in out.iter().zip(reference) {
        // The reference is 16-bit, so quantize ours the same way first.
        let o = (f64::from(o) * 32768.0).round().clamp(-32768.0, 32767.0) / 32768.0;
        sig += f64::from(r) * f64::from(r);
        err += (o - f64::from(r)).powi(2);
    }
    10.0 * (sig / err.max(1e-30)).log10()
}

fn check(name: &str, cfg: AacConfig, frames: &[Vec<u8>], min_snr: f64) {
    let out = decode_frames(name, cfg, frames);
    let snr = snr_db(&out, &reference(name));
    eprintln!("{name}: {cfg:?}: SNR {snr:.1} dB vs ffmpeg");
    assert!(snr > min_snr, "{name}: SNR {snr:.1} dB");
}

fn check_adts(name: &str, sbr: bool, min_snr: f64) {
    let (mut cfg, frames) = adts_frames(&read(&format!("{name}.aac")));
    cfg.sbr = sbr;
    check(name, cfg, &frames, min_snr);
}

#[test]
fn lc_speech_mono_24k_matches_ffmpeg() {
    check_adts("speech_mono24", false, 90.0);
}

#[test]
fn lc_music_mono_24k_with_short_blocks_matches_ffmpeg() {
    check_adts("music_mono24", false, 90.0);
}

#[test]
fn lc_music_stereo_48k_matches_ffmpeg() {
    check_adts("music_stereo48", false, 90.0);
}

/// Noise substitution: the standard leaves the random generator open; this
/// decoder's (a 32-bit LCG) happens to draw the same sequence as ffmpeg's,
/// so the noise bands match sample for sample and the SNR checks their
/// level and placement exactly.
#[test]
fn lc_pns_matches_ffmpeg() {
    check_adts("pns_mono24", false, 90.0);
}

/// 960-sample frames: IMDCT size, windows, band tables and TNS limits of
/// the DRM/DAB+ frame length, against ffmpeg's 960 decoder.
#[test]
fn lc_960_stereo_48k_matches_ffmpeg() {
    let (cfg, frames) = latm_frames(&read("lc960_stereo48.latm"));
    assert_eq!(cfg.frame_len, 960);
    check("lc960_stereo48", cfg, &frames, 90.0);
}

/// SBR over a speech core: every frame class, both resolutions and
/// amplitude steps, time and frequency deltas, inverse filtering, noise,
/// sinusoids, a header change and a reset. Matches to float rounding.
#[test]
fn he_aac_mono_matches_ffmpeg() {
    check_adts("he_mono48", true, 90.0);
}

/// SBR on a channel pair, coupled and independent frames, over a tonal core
/// with clicks. Pure tones make the inverse filter's covariance nearly
/// singular, where float rounding alone moves the prediction coefficients
/// in either decoder, so a few frames agree only to ~60 dB.
#[test]
fn he_aac_stereo_matches_ffmpeg() {
    check_adts("he_stereo48", true, 70.0);
}

/// A frame that fails to parse leaves the decoder as it was: concealing it
/// stays quiet (no burst), and the stream is back on the reference two
/// frames later (the frame after carries the concealed frame's overlap).
fn recovers_after_a_bad_frame(name: &str, sbr: bool) {
    let (mut cfg, frames) = adts_frames(&read(&format!("{name}.aac")));
    cfg.sbr = sbr;
    let reference = reference(name);
    let mut dec = AacDecoder::new(cfg);
    let per_frame = dec.output_frame_len() * usize::from(dec.output_channels());
    let bad = 8;
    let mut out = Vec::new();
    let mut peak_before = 0.0f32;
    for (n, f) in frames.iter().enumerate() {
        if n == bad || n == bad + 1 {
            // Truncated mid-spectrum: the first fails to parse, and the
            // second is simply lost.
            if n == bad {
                assert!(dec.decode(&f[..f.len() / 3]).is_err());
            }
            let pcm = dec.conceal();
            let peak = pcm.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(peak <= peak_before * 1.5 + 1e-3, "concealed peak {peak} after {peak_before}");
            out.extend(pcm);
            continue;
        }
        let pcm = dec.decode(f).unwrap();
        if n < bad {
            peak_before = pcm.iter().fold(peak_before, |m, v| m.max(v.abs()));
        }
        out.extend(pcm);
    }
    // SBR's QMF chain delays the core by most of a frame, so allow it one
    // more frame to come back.
    let settle = bad + 3 + usize::from(sbr);
    let tail = settle * per_frame..out.len();
    let snr = snr_db(&out[tail.clone()], &reference[tail]);
    assert!(snr > 80.0, "{name}: SNR {snr:.1} dB after recovery");
}

#[test]
fn lc_recovers_after_a_bad_frame() {
    recovers_after_a_bad_frame("music_mono24", false);
}

#[test]
fn he_aac_recovers_after_a_bad_frame() {
    recovers_after_a_bad_frame("he_mono48", true);
}

/// Concealing many frames in a row fades to silence and stays there.
#[test]
fn long_loss_fades_to_silence() {
    let (mut cfg, frames) = adts_frames(&read("he_mono48.aac"));
    cfg.sbr = true;
    let mut dec = AacDecoder::new(cfg);
    for f in &frames[..10] {
        dec.decode(f).unwrap();
    }
    let mut last = Vec::new();
    for _ in 0..8 {
        last = dec.conceal();
        assert!(last.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
    }
    let peak = last.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!(peak < 1e-4, "still {peak} after eight lost frames");
}

/// Real-time factor, release builds: `cargo test --release --test
/// aac_fixtures speed -- --ignored --nocapture`.
#[test]
#[ignore = "timing, run by hand in release"]
fn speed() {
    for (name, sbr) in [("he_mono48", true), ("he_stereo48", true), ("music_stereo48", false)] {
        let (mut cfg, frames) = adts_frames(&read(&format!("{name}.aac")));
        cfg.sbr = sbr;
        let mut dec = AacDecoder::new(cfg);
        let per_frame = dec.output_frame_len() as f64 / f64::from(dec.output_rate_hz());
        let reps = 200;
        let t = std::time::Instant::now();
        for _ in 0..reps {
            for f in &frames {
                std::hint::black_box(dec.decode(f).unwrap());
            }
        }
        let audio = per_frame * (reps * frames.len()) as f64;
        let cpu = t.elapsed().as_secs_f64();
        eprintln!("{name}: {:.2}% of one core ({audio:.0} s of audio in {cpu:.2} s)", 100.0 * cpu / audio);
    }
}

/// Corrupt input never panics: every fixture frame with bits flipped or cut
/// short, and random bytes, through LC, SBR and 960 decoders. Each call
/// returns PCM or an error, and the decoder keeps going.
#[test]
fn corrupt_frames_never_panic() {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let streams = [
        ("speech_mono24", false, 1024),
        ("music_stereo48", false, 1024),
        ("he_mono48", true, 1024),
        ("he_stereo48", true, 1024),
        ("music_stereo48", false, 960),
    ];
    for (name, sbr, frame_len) in streams {
        let (mut cfg, frames) = adts_frames(&read(&format!("{name}.aac")));
        cfg.sbr = sbr;
        cfg.frame_len = frame_len;
        let mut dec = AacDecoder::new(cfg);
        let expect = dec.output_frame_len() * usize::from(dec.output_channels());
        for round in 0..20 {
            for f in &frames {
                let mut g = f.clone();
                match round % 4 {
                    0 => {
                        for _ in 0..1 + next() % 8 {
                            let bit = (next() as usize) % (8 * g.len());
                            g[bit / 8] ^= 0x80 >> (bit % 8);
                        }
                    }
                    1 => g.truncate((next() as usize) % g.len()),
                    2 => g.iter_mut().for_each(|b| *b = next() as u8),
                    _ => {}
                }
                match dec.decode(&g) {
                    Ok(pcm) => {
                        assert_eq!(pcm.len(), expect);
                        assert!(pcm.iter().all(|v| v.is_finite()), "{name}: non-finite output");
                    }
                    Err(_) => assert_eq!(dec.conceal().len(), expect),
                }
            }
        }
    }
}

#[test]
fn config_is_reported() {
    let dec = AacDecoder::new(AacConfig { core_sample_rate_hz: 24000, channels: 1, frame_len: 960, sbr: true, ps: false });
    assert_eq!((dec.output_rate_hz(), dec.output_channels(), dec.output_frame_len()), (48000, 1, 1920));
    let mut bad = AacDecoder::new(AacConfig { core_sample_rate_hz: 25000, channels: 1, frame_len: 1024, sbr: false, ps: false });
    assert!(bad.decode(&[0u8; 8]).is_err());
}
