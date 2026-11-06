//! Decode a DRM capture (the server's `GET /api/sdr/capture.wav?kind=iq`,
//! 16-bit stereo I/Q) and print what the receiver finds; with a second
//! argument, write every audio super frame's codec frames to that file for
//! the AAC decoder to work on offline.
//!
//!     cargo run --release -p scannerd-engine --example drm_wav <capture.wav> [frames.bin]
//!
//! frames.bin is a run of records, little endian:
//!     u32 core rate Hz, u8 sbr, u8 stereo, u8 ps, u8 codec (0 AAC, 1 xHE-AAC, 2 other),
//!     u16 audio_config length, the audio_config bytes,
//!     u16 frame count, then per frame: u8 ok, u8 has_crc, u8 crc, u16 length, the bytes.

use num_complex::Complex32;
use scannerd_engine::drm::{AudioCodec, DrmEvent, DrmReceiver};
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("capture.wav");
    let bytes = std::fs::read(path).expect("read");
    // A plain 44-byte RIFF header, as the server writes it.
    let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap()) as f32;
    let data = &bytes[44..];
    let iq: Vec<Complex32> = data.chunks_exact(4).map(|c| Complex32::new(i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0, i16::from_le_bytes([c[2], c[3]]) as f32 / 32768.0)).collect();
    eprintln!("{} samples at {rate} Hz", iq.len());
    let mut out = args.get(2).map(|p| std::fs::File::create(p).expect("create"));
    let mut rx = DrmReceiver::new(rate);
    let (mut superframes, mut frames_ok, mut frames) = (0, 0, 0);
    for block in iq.chunks(4_800) {
        rx.push_iq(block);
        for e in rx.take() {
            match &e {
                DrmEvent::AudioSuperFrame { sample_rate_hz, sbr, stereo, ps, codec, audio_config, frames: f, .. } => {
                    superframes += 1;
                    frames += f.len();
                    frames_ok += f.iter().filter(|x| x.ok).count();
                    if let Some(o) = out.as_mut() {
                        let mut rec = Vec::new();
                        rec.extend_from_slice(&sample_rate_hz.to_le_bytes());
                        rec.extend_from_slice(&[*sbr as u8, *stereo as u8, *ps as u8, match codec {
                            AudioCodec::Aac => 0,
                            AudioCodec::XheAac => 1,
                            _ => 2,
                        }]);
                        rec.extend_from_slice(&(audio_config.len() as u16).to_le_bytes());
                        rec.extend_from_slice(audio_config);
                        rec.extend_from_slice(&(f.len() as u16).to_le_bytes());
                        for fr in f {
                            rec.extend_from_slice(&[fr.ok as u8, fr.crc.is_some() as u8, fr.crc.unwrap_or(0)]);
                            rec.extend_from_slice(&(fr.data.len() as u16).to_le_bytes());
                            rec.extend_from_slice(&fr.data);
                        }
                        o.write_all(&rec).expect("write");
                    }
                }
                DrmEvent::Stats(_) => {}
                _ => println!("{}", e.summary()),
            }
        }
    }
    let s = rx.status();
    println!("{superframes} audio super frames, {frames_ok}/{frames} frames ok; SNR {:?} dB, WMER {:?} dB, FAC {:.0}% SDC {:.0}% MSC {:.0}%", s.snr_db, s.wmer_db, s.fac_ok_rate * 100.0, s.sdc_ok_rate * 100.0, s.msc_ok_rate * 100.0);
}
