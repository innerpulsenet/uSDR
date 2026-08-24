//! 978 MHz UAT ADS-B (GA traffic). FIS-B uplink is out of scope for now.
//!
//! Sample at 2.083334 MS/s (two samples per 1.041667 Mbaud GFSK symbol).
//! Downlink sync is 36 bits `0xEACDDA4E2`. Short frames are RS(30,18);
//! long frames are RS(48,34). Position is 24-bit absolute lat/lon, not CPR.

use crate::rs::Rs256;
use num_complex::Complex32;
use std::sync::OnceLock;

pub const SAMPLE_RATE: f64 = 2_083_334.0;
pub const FREQ_HZ: f64 = 978_000_000.0;
pub const BIT_RATE: f64 = 1_041_667.0;

const SYNC_BITS: usize = 36;
const ADSB_SYNC: u64 = 0x0EACD_DA4E2;
const SYNC_MASK: u64 = (1u64 << SYNC_BITS) - 1;
const MAX_SYNC_ERR: u32 = 4;

const SHORT_DATA: usize = 18;
const SHORT_BYTES: usize = 30;
const LONG_DATA: usize = 34;
const LONG_BYTES: usize = 48;

static RS_SHORT: OnceLock<Rs256> = OnceLock::new();
static RS_LONG: OnceLock<Rs256> = OnceLock::new();

fn rs_short() -> &'static Rs256 {
    RS_SHORT.get_or_init(|| Rs256::new(12, 225))
}
fn rs_long() -> &'static Rs256 {
    RS_LONG.get_or_init(|| Rs256::new(14, 207))
}

#[derive(Clone, Debug, PartialEq)]
pub struct UatAircraft {
    pub icao: u32,
    pub callsign: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub alt_ft: Option<i32>,
    pub speed_kt: Option<f32>,
    pub heading_deg: Option<f32>,
}

fn popcount_le(mut x: u64, n: u32) -> bool {
    let mut err = 0u32;
    while x != 0 {
        x &= x - 1;
        err += 1;
        if err > n {
            return false;
        }
    }
    true
}

fn fuzzy_sync(word: u64, expected: u64) -> bool {
    popcount_le(word ^ expected, MAX_SYNC_ERR)
}

fn dphi(a: Complex32, b: Complex32) -> f32 {
    let p = b * a.conj();
    if p.norm_sqr() <= 0.0 { 0.0 } else { p.arg() }
}

fn demod_bytes(iq: &[Complex32], start: usize, bytes: usize, center: f32) -> Option<Vec<u8>> {
    // 2 samples/bit, 8 bits/byte.
    let need = start + bytes * 16;
    if iq.len() < need {
        return None;
    }
    let mut out = vec![0u8; bytes];
    for b in 0..bytes {
        let mut v = 0u8;
        for k in 0..8 {
            let i = start + b * 16 + k * 2;
            if dphi(iq[i], iq[i + 1]) > center {
                v |= 0x80 >> k;
            }
        }
        out[b] = v;
    }
    Some(out)
}

fn sync_center(iq: &[Complex32], start: usize, pattern: u64) -> Option<f32> {
    if iq.len() < start + SYNC_BITS * 2 + 1 {
        return None;
    }
    let mut z_sum = 0.0f32;
    let mut o_sum = 0.0f32;
    let mut z_n = 0u32;
    let mut o_n = 0u32;
    for i in 0..SYNC_BITS {
        let dp = dphi(iq[start + i * 2], iq[start + i * 2 + 1]);
        if pattern & (1u64 << (35 - i)) != 0 {
            o_sum += dp;
            o_n += 1;
        } else {
            z_sum += dp;
            z_n += 1;
        }
    }
    if z_n == 0 || o_n == 0 {
        return None;
    }
    let center = (o_sum / o_n as f32 + z_sum / z_n as f32) * 0.5;
    let mut err = 0u32;
    for i in 0..SYNC_BITS {
        let dp = dphi(iq[start + i * 2], iq[start + i * 2 + 1]);
        let bit = dp >= center;
        let want = pattern & (1u64 << (35 - i)) != 0;
        if bit != want {
            err += 1;
        }
    }
    if err <= MAX_SYNC_ERR {
        Some(center)
    } else {
        None
    }
}

fn correct_adsb(buf: &mut [u8]) -> Option<usize> {
    // Try long, then short. dump978: long if MDB type != 0; short if type == 0.
    if buf.len() < LONG_BYTES {
        return None;
    }
    let mut long = [0u8; LONG_BYTES];
    long.copy_from_slice(&buf[..LONG_BYTES]);
    if let Ok(n) = rs_long().decode(&mut long)
        && n <= 7
        && (long[0] >> 3) != 0
    {
        buf[..LONG_BYTES].copy_from_slice(&long);
        return Some(LONG_DATA);
    }
    let mut short = [0u8; SHORT_BYTES];
    short.copy_from_slice(&buf[..SHORT_BYTES]);
    if let Ok(n) = rs_short().decode(&mut short)
        && n <= 6
        && (short[0] >> 3) == 0
    {
        buf[..SHORT_BYTES].copy_from_slice(&short);
        return Some(SHORT_DATA);
    }
    None
}

/// Demodulate UAT ADS-B downlink frames from 2.083334 MS/s IQ.
pub fn demod_iq(iq: &[Complex32]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if iq.len() < (SYNC_BITS + LONG_BYTES * 8) * 2 + 4 {
        return out;
    }
    let mut sync0 = 0u64;
    let mut sync1 = 0u64;
    let last = iq.len() / 2 - (SYNC_BITS + LONG_BYTES * 8) - 2;
    let mut bit = 0usize;
    while bit < last {
        let i = bit * 2;
        if i + 2 >= iq.len() {
            break;
        }
        sync0 = ((sync0 << 1) | u64::from(dphi(iq[i], iq[i + 1]) > 0.0)) & SYNC_MASK;
        if i + 2 < iq.len() {
            sync1 = ((sync1 << 1) | u64::from(dphi(iq[i + 1], iq[i + 2]) > 0.0)) & SYNC_MASK;
        }
        if bit < SYNC_BITS {
            bit += 1;
            continue;
        }
        let (hit, shift) = if fuzzy_sync(sync0, ADSB_SYNC) {
            (true, 0)
        } else if fuzzy_sync(sync1, ADSB_SYNC) {
            (true, 1)
        } else {
            (false, 0)
        };
        if hit {
            let start = (bit + 1 - SYNC_BITS) * 2 + shift;
            if let Some(center) = sync_center(iq, start, ADSB_SYNC)
                && let Some(mut bytes) = demod_bytes(iq, start + SYNC_BITS * 2, LONG_BYTES, center)
                && let Some(data_len) = correct_adsb(&mut bytes)
            {
                bytes.truncate(data_len);
                out.push(bytes);
                bit += SYNC_BITS + data_len * 8;
                continue;
            }
        }
        bit += 1;
    }
    out
}

const BASE40: &[u8; 40] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ  ..";

pub fn decode_adsb(frame: &[u8]) -> Option<UatAircraft> {
    if frame.len() < 17 {
        return None;
    }
    let mdb_type = (frame[0] >> 3) & 0x1f;
    let aq = frame[0] & 0x07;
    // ICAO via ADS-B (0) or TIS-B (2). Vehicles/beacons still have an address.
    let icao = u32::from_be_bytes([0, frame[1], frame[2], frame[3]]);
    if icao == 0 {
        return None;
    }
    let _ = aq;

    let mut ac = UatAircraft {
        icao,
        callsign: None,
        lat: None,
        lon: None,
        alt_ft: None,
        speed_kt: None,
        heading_deg: None,
    };

    // State vector on types 0–10 except we still try SV on the documented set.
    decode_sv(frame, &mut ac);
    if matches!(mdb_type, 1 | 3) && frame.len() >= 27 {
        ac.callsign = decode_callsign(frame);
    }
    Some(ac)
}

fn decode_sv(frame: &[u8], ac: &mut UatAircraft) {
    if frame.len() < 17 {
        return;
    }
    let nic = frame[11] & 0x0f;
    let raw_lat =
        (u32::from(frame[4]) << 15) | (u32::from(frame[5]) << 7) | (u32::from(frame[6]) >> 1);
    let raw_lon = ((u32::from(frame[6]) & 0x01) << 23)
        | (u32::from(frame[7]) << 15)
        | (u32::from(frame[8]) << 7)
        | (u32::from(frame[9]) >> 1);
    if nic != 0 || raw_lat != 0 || raw_lon != 0 {
        let mut lat = raw_lat as f64 * 360.0 / 16_777_216.0;
        if lat > 90.0 {
            lat -= 180.0;
        }
        let mut lon = raw_lon as f64 * 360.0 / 16_777_216.0;
        if lon > 180.0 {
            lon -= 360.0;
        }
        ac.lat = Some(lat);
        ac.lon = Some(lon);
    }
    let raw_alt = (u32::from(frame[10]) << 4) | (u32::from(frame[11] >> 4));
    if raw_alt != 0 {
        ac.alt_ft = Some((raw_alt as i32 - 1) * 25 - 1000);
    }
    let ag = (frame[12] >> 6) & 0x03;
    if ag == 0 || ag == 1 {
        // subsonic / supersonic
        let mut raw_ns = ((u32::from(frame[12]) & 0x1f) << 6) | (u32::from(frame[13] >> 2));
        let mut ns = 0.0f32;
        let mut ew = 0.0f32;
        let mut ok = false;
        if raw_ns & 0x3ff != 0 {
            ns = (raw_ns & 0x3ff) as f32 - 1.0;
            if raw_ns & 0x400 != 0 {
                ns = -ns;
            }
            if ag == 1 {
                ns *= 4.0;
            }
            ok = true;
        }
        raw_ns = ((u32::from(frame[13]) & 0x03) << 9)
            | (u32::from(frame[14]) << 1)
            | (u32::from(frame[15] >> 7));
        if raw_ns & 0x3ff != 0 {
            ew = (raw_ns & 0x3ff) as f32 - 1.0;
            if raw_ns & 0x400 != 0 {
                ew = -ew;
            }
            if ag == 1 {
                ew *= 4.0;
            }
            ok = true;
        }
        if ok {
            ac.speed_kt = Some((ns * ns + ew * ew).sqrt());
            if ns != 0.0 || ew != 0.0 {
                let h = (360.0 + 90.0 - ns.atan2(ew).to_degrees()) % 360.0;
                ac.heading_deg = Some(if h < 0.0 { h + 360.0 } else { h });
            }
        }
    } else if ag == 2 {
        let raw_gs = ((u32::from(frame[12]) & 0x1f) << 6) | (u32::from(frame[13] >> 2));
        if raw_gs != 0 {
            ac.speed_kt = Some((raw_gs & 0x3ff) as f32 - 1.0);
        }
        let raw_track = ((u32::from(frame[13]) & 0x03) << 9)
            | (u32::from(frame[14]) << 1)
            | (u32::from(frame[15] >> 7));
        if (raw_track & 0x0600) != 0 {
            ac.heading_deg = Some((raw_track & 0x1ff) as f32 * 360.0 / 512.0);
        }
    }
}

fn decode_callsign(frame: &[u8]) -> Option<String> {
    if frame.len() < 23 {
        return None;
    }
    let mut cs = [0u8; 8];
    let mut v = (u16::from(frame[17]) << 8) | u16::from(frame[18]);
    cs[0] = BASE40[(v / 40 % 40) as usize];
    cs[1] = BASE40[(v % 40) as usize];
    v = (u16::from(frame[19]) << 8) | u16::from(frame[20]);
    cs[2] = BASE40[(v / 1600 % 40) as usize];
    cs[3] = BASE40[(v / 40 % 40) as usize];
    cs[4] = BASE40[(v % 40) as usize];
    v = (u16::from(frame[21]) << 8) | u16::from(frame[22]);
    cs[5] = BASE40[(v / 1600 % 40) as usize];
    cs[6] = BASE40[(v / 40 % 40) as usize];
    cs[7] = BASE40[(v % 40) as usize];
    // first char of v/1600 for bytes 17-18 was emitter category, not callsign[0]
    // dump978: v = frame[17]<<8|frame[18]; cat = v/1600%40; cs[0]=v/40%40; cs[1]=v%40
    v = (u16::from(frame[17]) << 8) | u16::from(frame[18]);
    cs[0] = BASE40[(v / 40 % 40) as usize];
    cs[1] = BASE40[(v % 40) as usize];
    let s = String::from_utf8_lossy(&cs)
        .trim_end_matches(|c: char| c == ' ' || c == '.')
        .trim()
        .to_string();
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_scale_new_england() {
        // 41.577 N, 73.408 W packed the UAT way.
        let lat = 41.577_f64;
        let lon = -73.408_f64;
        let raw_lat = (lat / 360.0 * 16_777_216.0).round() as u32;
        let raw_lon = (((lon + 360.0) / 360.0) * 16_777_216.0).round() as u32;
        let mut frame = [0u8; 18];
        frame[1] = 0xAB;
        frame[2] = 0xCD;
        frame[3] = 0xEF;
        frame[4] = (raw_lat >> 15) as u8;
        frame[5] = (raw_lat >> 7) as u8;
        frame[6] = ((raw_lat << 1) as u8) | ((raw_lon >> 23) as u8 & 1);
        frame[7] = (raw_lon >> 15) as u8;
        frame[8] = (raw_lon >> 7) as u8;
        frame[9] = (raw_lon << 1) as u8;
        frame[11] = 0x01; // nic != 0
        let ac = decode_adsb(&frame).unwrap();
        assert_eq!(ac.icao, 0xABCDEF);
        let (la, lo) = (ac.lat.unwrap(), ac.lon.unwrap());
        assert!((la - lat).abs() < 0.01, "{la} {lo}");
        assert!((lo - lon).abs() < 0.01, "{la} {lo}");
    }

    #[test]
    fn callsign_base40() {
        // Build bytes 17-22 from "N123AB" using dump978 packing.
        let alphabet = BASE40;
        let idx = |c: u8| alphabet.iter().position(|&x| x == c).unwrap() as u16;
        let mut frame = [0u8; 34];
        frame[0] = 1 << 3; // type 1 has MS
        frame[1] = 0x00;
        frame[2] = 0x11;
        frame[3] = 0x22;
        frame[11] = 1;
        let chars = b"N123AB  ";
        // bytes 17-18: cat, cs0, cs1 — cat 0
        // dump978: cat = v/1600%40, cs0 = v/40%40, cs1 = v%40
        // so v = cat*1600 + cs0*40 + cs1
        let v0 = idx(chars[0]) * 40 + idx(chars[1]);
        frame[17] = (v0 >> 8) as u8;
        frame[18] = v0 as u8;
        let v1 = idx(chars[2]) * 1600 + idx(chars[3]) * 40 + idx(chars[4]);
        frame[19] = (v1 >> 8) as u8;
        frame[20] = v1 as u8;
        let v2 = idx(chars[5]) * 1600 + idx(chars[6]) * 40 + idx(chars[7]);
        frame[21] = (v2 >> 8) as u8;
        frame[22] = v2 as u8;
        let ac = decode_adsb(&frame).unwrap();
        assert_eq!(ac.callsign.as_deref(), Some("N123AB"));
    }
}
