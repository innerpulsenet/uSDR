//! WAV in and out: the server's rolling captures, I/Q exported as raw CS16LE
//! with a JSON sidecar (the format the server's own call export uses), and
//! I/Q files dropped on the window wrapped back into a WAV for replay.

use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use usdr_proto::Status;

pub struct Wav {
    pub rate: u32,
    pub channels: u16,
    /// Interleaved 16-bit samples.
    pub samples: Vec<i16>,
}

impl Wav {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }
    /// The first channel only (a voice capture is mono anyway).
    pub fn mono(&self) -> Vec<i16> {
        self.samples.iter().step_by(self.channels.max(1) as usize).copied().collect()
    }
}

/// Walk the RIFF chunks rather than assume a 44-byte header.
pub fn parse(b: &[u8]) -> Option<Wav> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return None;
    }
    let mut at = 12;
    let mut rate = 0;
    let mut channels = 1;
    while at + 8 <= b.len() {
        let id = &b[at..at + 4];
        let len = u32::from_le_bytes(b[at + 4..at + 8].try_into().ok()?) as usize;
        let body = &b[at + 8..(at + 8 + len).min(b.len())];
        if id == b"fmt " && body.len() >= 16 {
            channels = u16::from_le_bytes([body[2], body[3]]);
            rate = u32::from_le_bytes(body[4..8].try_into().ok()?);
        } else if id == b"data" {
            let samples = body.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
            return Some(Wav { rate, channels, samples });
        }
        at += 8 + len + (len & 1);
    }
    None
}

pub fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d) = civil(secs / 86_400);
    format!(
        "{y:04}{mo:02}{d:02}-{:02}{:02}{:02}Z",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    )
}

/// Days since the epoch to a calendar date (Howard Hinnant's algorithm).
fn civil(days: u64) -> (i64, u32, u32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The stem the web page uses: `usdr-capture-<time>-<MHz>MHz-<rate>sps`.
pub fn capture_stem(dir: &Path, freq_hz: f64, rate: u32) -> PathBuf {
    dir.join(format!("usdr-capture-{}-{:.5}MHz-{rate}sps", stamp(), freq_hz / 1e6))
}

/// Write a rolling capture. I/Q goes out as raw CS16LE plus a JSON sidecar;
/// voice and discriminator captures stay WAV.
pub fn save_capture(kind: &str, stem: &Path, w: &Wav, freq_hz: f64, st: &Status) -> std::io::Result<PathBuf> {
    let dir = stem.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let stem = capture_stem(dir, freq_hz, w.rate);
    let name = stem.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    match kind {
        "iq" => {
            let iq = dir.join(format!("{name}-cs16le.iq"));
            let mut raw = Vec::with_capacity(w.samples.len() * 2);
            for s in &w.samples {
                raw.extend_from_slice(&s.to_le_bytes());
            }
            std::fs::write(&iq, raw)?;
            let frames = w.frames();
            let dur = frames as f64 / w.rate.max(1) as f64;
            let now_ms = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0);
            let meta = json!({
                "source": "rolling capture",
                "format": "cs16le",
                "freqHz": freq_hz,
                "iqRateHz": w.rate,
                "iqSamples": frames,
                "durationS": (dur * 1000.0).round() / 1000.0,
                "startedMs": (now_ms - dur * 1000.0).round(),
                "mode": st.mode.wire(),
                "bandwidthHz": st.bandwidth_hz,
                "tuner": st.tuner,
                "driver": st.driver,
                "host": st.host,
                "serial": st.serial,
            });
            std::fs::write(dir.join(format!("{name}-cs16le.json")), serde_json::to_string_pretty(&meta).unwrap_or_default())?;
            Ok(iq)
        }
        other => {
            let suffix = if other == "voice" { "voice" } else { "disc" };
            let path = dir.join(format!("{name}-{suffix}.wav"));
            let mut out = Vec::with_capacity(44 + w.samples.len() * 2);
            out.extend_from_slice(&wav_header(w.rate, w.channels, w.samples.len()));
            for s in &w.samples {
                out.extend_from_slice(&s.to_le_bytes());
            }
            std::fs::write(&path, out)?;
            Ok(path)
        }
    }
}

fn wav_header(rate: u32, channels: u16, samples: usize) -> [u8; 44] {
    let data = (samples * 2) as u32;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 2 * channels as u32).to_le_bytes());
    h[32..34].copy_from_slice(&(2 * channels).to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

/// Files dropped on the window, as a replay request: an `.iq` (rate and
/// frequency from its `.json` sidecar or its name) wrapped into a stereo WAV,
/// or a WAV sent as it is.
pub fn replay_body(files: &[PathBuf]) -> Result<(Vec<u8>, Option<f64>), String> {
    let ext = |p: &PathBuf| p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if let Some(iq) = files.iter().find(|p| ext(p) == "iq") {
        let raw = std::fs::read(iq).map_err(|e| e.to_string())?;
        let sidecar = files
            .iter()
            .find(|p| ext(p) == "json")
            .cloned()
            .or_else(|| Some(iq.with_extension("json")).filter(|p| p.exists()));
        let mut rate = None;
        let mut freq = None;
        if let Some(j) = sidecar
            && let Ok(text) = std::fs::read_to_string(j)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        {
            if v.get("format").and_then(|f| f.as_str()).is_some_and(|f| f != "cs16le") {
                return Err("the sidecar says this is not CS16LE".into());
            }
            rate = v.get("iqRateHz").and_then(|r| r.as_u64()).map(|r| r as u32);
            freq = v.get("freqHz").and_then(|r| r.as_f64());
        }
        if rate.is_none() {
            // usdr-…-146.52000MHz-48000sps-cs16le.iq
            let name = iq.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let parts: Vec<&str> = name.split('-').collect();
            for (i, p) in parts.iter().enumerate() {
                if let Some(m) = p.strip_suffix("MHz") {
                    freq = m.parse::<f64>().ok().map(|v| v * 1e6);
                    if let Some(r) = parts.get(i + 1).and_then(|s| s.strip_suffix("sps")) {
                        rate = r.parse().ok();
                    }
                }
            }
        }
        let rate = rate.ok_or("no sample rate: add the .json sidecar or keep the file name")?;
        let mut out = Vec::with_capacity(44 + raw.len());
        out.extend_from_slice(&wav_header(rate, 2, raw.len() / 2));
        out.extend_from_slice(&raw[..raw.len() & !3]);
        return Ok((out, freq));
    }
    let first = files.first().ok_or("nothing dropped")?;
    if ext(first) != "wav" {
        return Err("drop a .iq (with its .json) or an I/Q .wav".into());
    }
    Ok((std::fs::read(first).map_err(|e| e.to_string())?, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chunks_past_a_list() {
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF\0\0\0\0WAVE");
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&8000u32.to_le_bytes());
        b.extend_from_slice(&[0; 8]);
        b.extend_from_slice(b"LIST");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&[1, 2, 3, 0]);
        b.extend_from_slice(b"data");
        b.extend_from_slice(&4u32.to_le_bytes());
        b.extend_from_slice(&(-2i16).to_le_bytes());
        b.extend_from_slice(&5i16.to_le_bytes());
        let w = parse(&b).unwrap();
        assert_eq!((w.rate, w.channels), (8000, 2));
        assert_eq!(w.samples, vec![-2, 5]);
        assert_eq!(w.frames(), 1);
    }

    #[test]
    fn calendar() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_759), (2026, 11, 2));
    }
}
