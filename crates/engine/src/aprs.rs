//! 1200-baud Bell 202 APRS (AX.25 UI) after NBFM.

use crate::afsk::ToneSlicer;

const MARK: f32 = 1200.0;
const SPACE: f32 = 2200.0;
const BAUD: f32 = 1200.0;
/// Minimum flag preamble before accepting a frame. Four flags cost only 27 ms
/// and are comfortably inside normal APRS TX-delay preambles, while making a
/// random discriminator collision exponentially less likely to arm a lane.
const OPENING_FLAGS: u8 = 4;
/// One lane per sample across a baud at 16 kHz (≈13.3 samples/bit).
const PHASES: usize = 16;

struct Lane {
    slicer: ToneSlicer,
    last_mark: bool,
    ones: u8,
    byte: u8,
    bits: u8,
    in_frame: bool,
    /// Consecutive HDLC flags seen while hunting. APRS transmitters send a
    /// flag preamble; requiring several keeps band-limited discriminator
    /// noise from arming the timing loop on isolated flag-shaped collisions.
    opening_flags: u8,
    /// Decoded bits since the last flag detector hit. Consecutive flags land
    /// eight bits apart; without this, unrelated noise flags would eventually
    /// accumulate to the opening threshold.
    since_flag: u16,
    skip_flag_zero: bool,
    buf: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AprsDiagnostics {
    pub dc_offset_hz: f32,
    pub mark_pwr: f32,
    pub space_pwr: f32,
    pub fcs_ok: u64,
    pub fcs_err: u64,
    pub packets: u64,
    /// Largest bit-clock pull across the lane bank, in ppm. Should sit near
    /// zero on an idle channel: a large idle value means the timing loop is
    /// integrating noise, which leaves every lane unable to decode a burst.
    pub clock_pull_ppm: f32,
}

pub struct AprsDecoder {
    lanes: Vec<Lane>,
    dc_x1: f32,
    dc_y1: f32,
    dc_offset_hz: f32,
    mark_pwr: f32,
    space_pwr: f32,
    fcs_ok: u64,
    fcs_err: u64,
    packets: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AprsPacket {
    pub from: String,
    pub to: String,
    /// Digipeater path in transmission order, e.g. ["WIDE1-1", "WIDE2-2"].
    /// Used-and-spent hops (a digi retransmitting replaces its call with
    /// `VIACALL*`) keep the asterisk, as multimon-ng and every other APRS
    /// client print them.
    pub path: Vec<String>,
    pub text: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

impl AprsDecoder {
    pub fn new(fs: f64) -> Self {
        let n = PHASES.min(
            ToneSlicer::new(fs, MARK, SPACE, BAUD)
                .samples_per_bit()
                .max(1),
        );
        Self {
            lanes: (0..n)
                .map(|d| Lane {
                    slicer: ToneSlicer::new(fs, MARK, SPACE, BAUD).with_delay(d),
                    last_mark: true,
                    ones: 0,
                    byte: 0,
                    bits: 0,
                    in_frame: false,
                    opening_flags: 0,
                    since_flag: u16::MAX,
                    skip_flag_zero: false,
                    buf: Vec::new(),
                })
                .collect(),
            dc_x1: 0.0,
            dc_y1: 0.0,
            dc_offset_hz: 0.0,
            mark_pwr: 0.0,
            space_pwr: 0.0,
            fcs_ok: 0,
            fcs_err: 0,
            packets: 0,
        }
    }

    pub fn reset(&mut self) {
        self.dc_x1 = 0.0;
        self.dc_y1 = 0.0;
        self.dc_offset_hz = 0.0;
        self.mark_pwr = 0.0;
        self.space_pwr = 0.0;
        for lane in &mut self.lanes {
            lane.slicer.reset();
            lane.ones = 0;
            lane.bits = 0;
            lane.in_frame = false;
            lane.opening_flags = 0;
            lane.since_flag = u16::MAX;
            lane.skip_flag_zero = false;
            lane.buf.clear();
            lane.last_mark = true;
        }
    }

    pub fn diagnostics(&self) -> AprsDiagnostics {
        AprsDiagnostics {
            dc_offset_hz: self.dc_offset_hz,
            mark_pwr: self.mark_pwr,
            space_pwr: self.space_pwr,
            fcs_ok: self.fcs_ok,
            fcs_err: self.fcs_err,
            packets: self.packets,
            clock_pull_ppm: self
                .lanes
                .iter()
                .map(|l| l.slicer.clock_pull_ppm().abs() as f32)
                .fold(0.0, f32::max),
        }
    }

    /// Discriminator audio in Hz. Returns completed UI packets.
    pub fn process(&mut self, audio: &[f32]) -> Vec<AprsPacket> {
        let mut out = Vec::new();
        for &x in audio {
            // Single-pole DC blocker (cutoff ~38 Hz at 16 kHz): removes carrier mistuning offset
            let clean_x = x - self.dc_x1 + 0.985 * self.dc_y1;
            self.dc_x1 = x;
            self.dc_y1 = clean_x;
            self.dc_offset_hz += 0.001 * (x - self.dc_offset_hz);

            for lane in &mut self.lanes {
                // Acquire the opening flags on the nominal clock, then let the
                // slicer track the sender across the frame body.
                lane.slicer.set_tracking(lane.in_frame);
                let Some(mark) = lane.slicer.push(clean_x) else {
                    continue;
                };
                let was_in_frame = lane.in_frame;
                self.mark_pwr += 0.02 * (lane.slicer.last_pm - self.mark_pwr);
                self.space_pwr += 0.02 * (lane.slicer.last_ps - self.space_pwr);
                let bit = mark == lane.last_mark;
                lane.last_mark = mark;
                let frame = lane.push_nrzi(bit);
                if was_in_frame && !lane.in_frame {
                    // The frame aborted. Whatever rate the loop learned
                    // belonged to it, so hunt the next one from nominal.
                    lane.slicer.reacquire();
                }
                match frame {
                    Some(Ax25Frame::Packet(p)) => {
                        self.fcs_ok += 1;
                        if !out
                            .iter()
                            .any(|q: &AprsPacket| q.from == p.from && q.text == p.text)
                        {
                            self.packets += 1;
                            out.push(p);
                        }
                    }
                    Some(Ax25Frame::Unsupported) => self.fcs_ok += 1,
                    Some(Ax25Frame::BadFcs) => self.fcs_err += 1,
                    None => {}
                }
            }
        }
        out
    }
}

impl Lane {
    fn push_nrzi(&mut self, bit: bool) -> Option<Ax25Frame> {
        self.since_flag = self.since_flag.saturating_add(1);
        if bit {
            self.ones = self.ones.saturating_add(1);
            if self.ones >= 7 {
                self.abort();
                return None;
            }
            if self.ones == 6 {
                let pkt = if self.in_frame && self.buf.len() >= 17 {
                    Some(parse_ax25(&self.buf))
                } else {
                    None
                };
                self.buf.clear();
                self.bits = 0;
                self.byte = 0;
                self.ones = 0;
                if self.in_frame {
                    // A closing flag is also a legal opening flag for the
                    // next frame, so remain armed across a flag train.
                    self.opening_flags = OPENING_FLAGS;
                } else {
                    self.opening_flags = if self.since_flag <= 9 {
                        self.opening_flags.saturating_add(1)
                    } else {
                        1
                    };
                    self.in_frame = self.opening_flags >= OPENING_FLAGS;
                }
                self.since_flag = 0;
                self.skip_flag_zero = true;
                return pkt;
            }
        } else {
            if self.ones == 5 {
                self.ones = 0;
                return None;
            }
            if self.skip_flag_zero {
                self.skip_flag_zero = false;
                self.ones = 0;
                return None;
            }
            self.ones = 0;
        }
        if !self.in_frame {
            return None;
        }
        if bit {
            self.byte |= 1 << self.bits;
        }
        self.bits += 1;
        if self.bits == 8 {
            self.buf.push(self.byte);
            self.byte = 0;
            self.bits = 0;
            if self.buf.len() > 256 {
                self.abort();
            }
        }
        None
    }

    fn abort(&mut self) {
        self.in_frame = false;
        self.opening_flags = 0;
        self.since_flag = u16::MAX;
        self.buf.clear();
        self.bits = 0;
        self.byte = 0;
        self.ones = 0;
        self.skip_flag_zero = false;
    }
}

#[derive(Debug)]
enum Ax25Frame {
    Packet(AprsPacket),
    Unsupported,
    BadFcs,
}

fn parse_ax25(buf: &[u8]) -> Ax25Frame {
    if buf.len() < 17 {
        return Ax25Frame::Unsupported;
    }
    let data = &buf[..buf.len() - 2];
    let fcs = u16::from_le_bytes([buf[buf.len() - 2], buf[buf.len() - 1]]);
    if crc16(data) != fcs {
        return Ax25Frame::BadFcs;
    }
    if data.len() < 16 {
        return Ax25Frame::Unsupported;
    }
    let to = callsign(&data[0..7]);
    let from = callsign(&data[7..14]);
    let mut i = 14;
    let mut ext = data[13] & 0x01;
    while ext == 0 && i + 7 <= data.len() {
        ext = data[i + 6] & 0x01;
        i += 7;
    }
    if i + 2 > data.len() {
        return Ax25Frame::Unsupported;
    }
    // Addresses between dest+source and the last one are digipeaters, in
    // transmission order. A hop whose command bit (bit 7 of the SSID byte)
    // is set has already repeated this frame — shown with '*'.
    let mut path = Vec::new();
    if i > 14 {
        for hop in data[14..i].chunks_exact(7) {
            let mut call = callsign(hop);
            if hop[6] & 0x80 != 0 {
                call.push('*');
            }
            path.push(call);
        }
    }
    let control = data[i];
    if control & 0xef != 0x03 {
        return Ax25Frame::Unsupported;
    }
    let info = &data[i + 2..];
    let text = String::from_utf8_lossy(info).trim().to_string();
    let (lat, lon) = parse_position(&text);
    Ax25Frame::Packet(AprsPacket {
        from,
        to,
        path,
        text,
        lat,
        lon,
    })
}

fn callsign(b: &[u8]) -> String {
    let mut s = String::new();
    for &c in &b[..6] {
        let ch = (c >> 1) as char;
        if ch != ' ' {
            s.push(ch);
        }
    }
    let ssid = (b[6] >> 1) & 0x0f;
    if ssid != 0 {
        s.push('-');
        s.push_str(&ssid.to_string());
    }
    s
}

fn encode_call(s: &str, last: bool) -> [u8; 7] {
    let mut out = [b' ' << 1; 7];
    let (name, ssid) = s.split_once('-').unwrap_or((s, "0"));
    for (i, c) in name.bytes().take(6).enumerate() {
        out[i] = c << 1;
    }
    let ssid: u8 = ssid.parse().unwrap_or(0);
    out[6] = 0x60 | (ssid << 1) | u8::from(last);
    out
}

#[cfg(test)]
mod chain_tests {
    use super::*;

    /// Digipeater addresses are surfaced in order, with the spent-hop '*'.
    #[test]
    fn digipeater_path_is_parsed_with_repeated_marks() {
        let frame = build_ui_via(
            "APRS",
            "N1TEST-9",
            &["WIDE1-1*", "WIDE2-2"],
            "!4134.62N/07324.48W-hello",
        );
        match parse_ax25(&frame) {
            Ax25Frame::Packet(p) => {
                assert_eq!(p.path, vec!["WIDE1-1*", "WIDE2-2"]);
                assert_eq!(p.from, "N1TEST-9");
                assert!(p.text.contains("hello"));
            }
            other => panic!("expected packet, got {other:?}"),
        }
        // No path: empty, and the frame still parses (end bit on source).
        let frame = build_ui("APRS", "N1TEST", "!4134.62N/07324.48W-x");
        match parse_ax25(&frame) {
            Ax25Frame::Packet(p) => assert!(p.path.is_empty()),
            other => panic!("expected packet, got {other:?}"),
        }
    }

    /// End-to-end regression for the DecodeChain block-stall fix: FM-modulate
    /// a real AX.25 frame at +100 kHz IF, run DecodeChain -> NbfmDemod ->
    /// AprsDecoder in production-sized blocks, require a decode.
    #[test]
    fn decodes_through_fm_chain() {
        use scannerd_dsp::DecodeChain;
        let fs_in = 960_000.0f64;
        let if_hz = 100_000.0f64;
        let frame = build_ui("APRS", "N1TEST-9", "!4134.62N/07324.48W-hello");
        let bits = stuff_nrzi(&frame);
        let marks = nrzi_tones(&bits);

        // True FM: the Bell 202 tone modulates carrier deviation.
        let spb = (fs_in / 1200.0) as usize;
        let dev_hz = 3000.0f64;
        let mut iq = Vec::new();
        let mut phi = 0.0f64;
        let mut t_audio = 0.0f64;
        for _ in 0..240_000 {
            phi += std::f64::consts::TAU * if_hz / fs_in;
            if phi > std::f64::consts::TAU {
                phi -= std::f64::consts::TAU;
            }
            iq.push(num_complex::Complex32::new(
                phi.cos() as f32,
                phi.sin() as f32,
            ));
        }
        for &m in &marks {
            let f = (if m { MARK } else { SPACE }) as f64;
            for _ in 0..spb {
                let dev = dev_hz * (std::f64::consts::TAU * f * t_audio).sin();
                phi += std::f64::consts::TAU * (if_hz + dev) / fs_in;
                if phi > std::f64::consts::TAU {
                    phi -= std::f64::consts::TAU;
                }
                iq.push(num_complex::Complex32::new(
                    phi.cos() as f32,
                    phi.sin() as f32,
                ));
                t_audio += 1.0 / fs_in;
            }
        }

        let mut chain = DecodeChain::new(fs_in, 18_000.0, 16_000.0);
        chain.set_offset(if_hz);
        let fs_out = chain.fs_out();
        let mut nbfm = crate::nbfm::NbfmDemod::new(fs_out);
        let mut aprs = AprsDecoder::new(fs_out);
        let mut iq_ch = Vec::new();
        let mut disc = Vec::new();
        let mut pkts = Vec::new();
        for chunk in iq.chunks(16_384) {
            chain.process(chunk, &mut iq_ch);
            nbfm.discriminator_hz(&iq_ch, &mut disc);
            pkts.extend(aprs.process(&disc));
        }
        assert!(
            pkts.iter().any(|p| p.from.starts_with("N1TEST")),
            "chain path decoded nothing: {:?}",
            aprs.diagnostics()
        );
    }
}

fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &b in data {
        crc ^= u16::from(b);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0x8408;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

fn parse_position(text: &str) -> (Option<f64>, Option<f64>) {
    let bytes = text.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'!' || c == b'=' {
            if let Some(s) = text.get(i + 1..) {
                if s.len() >= 18 {
                    let lat = dm(&s[..7], s.as_bytes()[7]);
                    let lon = dm(&s[9..17], s.as_bytes()[17]);
                    if lat.is_some() && lon.is_some() {
                        return (lat, lon);
                    }
                }
            }
        } else if c == b'@' || c == b'/' {
            // 7-char timestamp (e.g. 220122z or 092345/ or 092345h) followed by lat/lon
            if let Some(s) = text.get(i + 1 + 7..) {
                if s.len() >= 18 {
                    let lat = dm(&s[..7], s.as_bytes()[7]);
                    let lon = dm(&s[9..17], s.as_bytes()[17]);
                    if lat.is_some() && lon.is_some() {
                        return (lat, lon);
                    }
                }
            }
        }
    }
    (None, None)
}

fn dm(s: &str, hemi: u8) -> Option<f64> {
    let (deg_s, min_s) = if s.len() == 7 {
        (&s[..2], &s[2..])
    } else if s.len() == 8 {
        (&s[..3], &s[3..])
    } else {
        return None;
    };
    let deg: f64 = deg_s.parse().ok()?;
    let min: f64 = min_s.parse().ok()?;
    let mut v = deg + min / 60.0;
    if hemi == b'S' || hemi == b'W' {
        v = -v;
    }
    Some(v)
}

/// Build an AX.25 UI frame (addresses + control + PID + info + FCS).
pub fn build_ui(dest: &str, src: &str, info: &str) -> Vec<u8> {
    build_ui_via(dest, src, &[], info)
}

/// `build_ui` with a digipeater path, for tests and tooling. A hop written
/// with a trailing '*' is marked as already-repeated, e.g. `&["WIDE1-1*",
/// "WIDE2-2"]`. The address-field end bit lands on the LAST address overall.
pub fn build_ui_via(dest: &str, src: &str, path: &[&str], info: &str) -> Vec<u8> {
    let mut f = Vec::new();
    let n = path.len();
    // The address-field end bit (bit 0 of the SSID byte) sits on the LAST
    // address: the source when there is no path, else the final hop.
    f.extend_from_slice(&encode_call(dest, false));
    f.extend_from_slice(&encode_call(src, n == 0));
    for (k, hop) in path.iter().enumerate() {
        let repeated = hop.ends_with('*');
        let call = hop.trim_end_matches('*');
        let mut enc = encode_call(call, k + 1 == n);
        if repeated {
            enc[6] |= 0x80;
        }
        f.extend_from_slice(&enc);
    }
    f.push(0x03);
    f.push(0xf0);
    f.extend_from_slice(info.as_bytes());
    let c = crc16(&f);
    f.push(c as u8);
    f.push((c >> 8) as u8);
    f
}

/// Bit-stuff `bytes` between a 16-flag preamble and a 4-flag postamble.
pub fn stuff_nrzi(bytes: &[u8]) -> Vec<bool> {
    let mut bits = Vec::new();
    let mut ones;
    let push = |bits: &mut Vec<bool>, ones: &mut u8, bit: bool| {
        bits.push(bit);
        if bit {
            *ones += 1;
            if *ones == 5 {
                bits.push(false);
                *ones = 0;
            }
        } else {
            *ones = 0;
        }
    };
    for _ in 0..16 {
        for i in 0..8 {
            bits.push((0x7Eu8 >> i) & 1 == 1);
        }
    }
    ones = 0u8;
    for &b in bytes {
        for i in 0..8 {
            push(&mut bits, &mut ones, (b >> i) & 1 == 1);
        }
    }
    for _ in 0..4 {
        for i in 0..8 {
            bits.push((0x7Eu8 >> i) & 1 == 1);
        }
    }
    bits
}

/// NRZI-encode a bit stream into mark/space tone selections.
pub fn nrzi_tones(bits: &[bool]) -> Vec<bool> {
    // mark=true. 0 toggles, 1 holds.
    let mut mark = true;
    bits.iter()
        .map(|&bit| {
            if !bit {
                mark = !mark;
            }
            mark
        })
        .collect()
}

/// FM-modulate an AX.25 UI frame into IQ at `fs`, centred `if_hz` off zero.
///
/// `amp` is the carrier amplitude and `dev_hz` the peak deviation. Returns
/// only the burst; callers splice it into whatever idle channel they want.
pub fn modulate_burst(
    frame: &[u8],
    fs: f64,
    if_hz: f64,
    amp: f32,
    dev_hz: f64,
    phase: &mut f64,
    t_audio: &mut f64,
) -> Vec<num_complex::Complex32> {
    let marks = nrzi_tones(&stuff_nrzi(frame));
    let spb = fs / f64::from(BAUD);
    let mut iq = Vec::new();
    for (k, &m) in marks.iter().enumerate() {
        let f = f64::from(if m { MARK } else { SPACE });
        let a = (k as f64 * spb).round() as usize;
        let b = ((k as f64 + 1.0) * spb).round() as usize;
        for _ in 0..(b - a) {
            let dev = dev_hz * (std::f64::consts::TAU * f * *t_audio).sin();
            *phase += std::f64::consts::TAU * (if_hz + dev) / fs;
            iq.push(num_complex::Complex32::new(
                amp * phase.cos() as f32,
                amp * phase.sin() as f32,
            ));
            *t_audio += 1.0 / fs;
        }
    }
    iq
}

#[cfg(test)]
fn tones_to_audio(marks: &[bool], fs: f32) -> Vec<f32> {
    crate::afsk::marks_to_audio(marks, fs, BAUD, MARK, SPACE, 2500.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_and_callsign_roundtrip_shape() {
        assert_eq!(
            callsign(&[
                b'A' << 1,
                b'P' << 1,
                b'R' << 1,
                b'S' << 1,
                b' ' << 1,
                b' ' << 1,
                0x60
            ]),
            "APRS"
        );
        let (lat, lon) = parse_position("!4134.62N/07324.48W-");
        assert!((lat.unwrap() - 41.577).abs() < 0.01);
        assert!((lon.unwrap() + 73.408).abs() < 0.01);

        let (lat2, lon2) = parse_position("@220122z4100.51N/07339.16W_097/000g000t071");
        assert!((lat2.unwrap() - 41.0085).abs() < 0.01);
        assert!((lon2.unwrap() + 73.6526).abs() < 0.01);
    }

    #[test]
    fn demodulates_a_synthesized_packet() {
        let info = "!4134.62N/07324.48W-hello";
        let frame = build_ui("APRS", "N1TEST-9", info);
        let bits = stuff_nrzi(&frame);
        let marks = nrzi_tones(&bits);
        let audio = tones_to_audio(&marks, 16_000.0);
        let mut dec = AprsDecoder::new(16_000.0);
        let pkts = dec.process(&audio);
        assert!(
            pkts.iter()
                .any(|p| p.from.starts_with("N1TEST") && p.text.contains("hello")),
            "got {pkts:?}"
        );
        let p = pkts.iter().find(|p| p.from.starts_with("N1TEST")).unwrap();
        assert!((p.lat.unwrap() - 41.577).abs() < 0.02);
        assert!((p.lon.unwrap() + 73.408).abs() < 0.02);
    }

    #[test]
    fn an_isolated_flag_does_not_arm_a_noise_frame() {
        let mut lane = Lane {
            slicer: ToneSlicer::new(16_000.0, MARK, SPACE, BAUD),
            last_mark: true,
            ones: 0,
            byte: 0,
            bits: 0,
            in_frame: false,
            opening_flags: 0,
            since_flag: u16::MAX,
            skip_flag_zero: false,
            buf: Vec::new(),
        };
        // HDLC flag, LSB first: 0 111111 0. The flag detector fires on the
        // sixth one; its trailing zero is consumed by skip_flag_zero.
        for bit in [false, true, true, true, true, true, true, false] {
            lane.push_nrzi(bit);
        }
        assert!(
            !lane.in_frame,
            "one flag-shaped noise collision armed a frame"
        );
        for _ in 0..20 {
            lane.push_nrzi(false);
        }
        for _ in 0..OPENING_FLAGS - 1 {
            for bit in [false, true, true, true, true, true, true, false] {
                lane.push_nrzi(bit);
            }
        }
        assert!(!lane.in_frame, "a short flag collision armed a frame");
        for bit in [false, true, true, true, true, true, true, false] {
            lane.push_nrzi(bit);
        }
        assert!(lane.in_frame, "a normal APRS flag preamble did not arm");
    }

    /// A TNC's baud rate is its own crystal's opinion, and an AX.25 frame is
    /// long enough for a small disagreement to become a whole bit. The lane
    /// bank alone covered an unknown *starting* phase and nothing after it,
    /// so a lane held only while accumulated error stayed inside ~1/32 of a
    /// bit — under 100 ppm across a 1000-bit frame. The early/late gate makes
    /// the lane follow the sender instead.
    #[test]
    fn a_transmitter_off_frequency_in_time_still_decodes() {
        let info = "!4134.62N/07324.48W-hello";
        let frame = build_ui("APRS", "N1TEST-9", info);
        let marks = nrzi_tones(&stuff_nrzi(&frame));
        for ppm in [-5_000i32, -2_000, -500, 500, 2_000, 5_000] {
            let air_baud = BAUD * (1.0 + ppm as f32 * 1e-6);
            let audio =
                crate::afsk::marks_to_audio(&marks, 16_000.0, air_baud, MARK, SPACE, 2500.0);
            let mut dec = AprsDecoder::new(16_000.0);
            let pkts = dec.process(&audio);
            assert!(
                pkts.iter()
                    .any(|p| p.from.starts_with("N1TEST") && p.text.contains("hello")),
                "{ppm} ppm baud error: got {pkts:?} ({:?})",
                dec.diagnostics()
            );
        }
    }

    /// The acceptance test for the timing loop, and the one that matters on
    /// air: a packet arriving after the receiver has been sitting in noise.
    ///
    /// A channel is idle almost all the time, so this is the *normal* case,
    /// not an edge case. A loop that integrates the timing detector's output
    /// while there is no signal random-walks its clock away from nominal
    /// within a fraction of a second, and then every burst arrives at a lane
    /// running at the wrong rate. Synthetic tests that start with the packet
    /// never see it; a real receiver sees nothing else.
    #[test]
    fn a_packet_after_seconds_of_receiver_noise_still_decodes() {
        let mut seed = 0x243F_6A88_85A3_08D3u64;
        let mut rng = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 11) as f32 / (1u64 << 53) as f32) * 2.0 - 1.0
        };
        let frame = build_ui("APRS", "N1TEST-9", "!4134.62N/07324.48W-hello");
        let packet = tones_to_audio(&nrzi_tones(&stuff_nrzi(&frame)), 16_000.0);

        for idle_s in [0.5f32, 2.0, 5.0] {
            let mut dec = AprsDecoder::new(16_000.0);
            // An FM discriminator with no carrier swings full scale, randomly.
            let noise: Vec<f32> = (0..(16_000.0 * idle_s) as usize)
                .map(|_| rng() * 3000.0)
                .collect();
            dec.process(&noise);
            let pull = dec.diagnostics().clock_pull_ppm;
            assert!(
                pull.abs() < 2_000.0,
                "{idle_s}s of noise walked the bit clock {pull:.0} ppm off nominal"
            );
            let pkts = dec.process(&packet);
            assert!(
                pkts.iter()
                    .any(|p| p.from.starts_with("N1TEST") && p.text.contains("hello")),
                "after {idle_s}s idle the packet was lost: {:?}",
                dec.diagnostics()
            );
        }
    }

    #[test]
    fn a_half_bit_shift_still_decodes() {
        let info = "!4134.62N/07324.48W-hello";
        let frame = build_ui("APRS", "N1TEST-9", info);
        let audio = tones_to_audio(&nrzi_tones(&stuff_nrzi(&frame)), 16_000.0);
        let mut shifted = vec![0.0f32; 6];
        shifted.extend_from_slice(&audio);
        let mut dec = AprsDecoder::new(16_000.0);
        let pkts = dec.process(&shifted);
        assert!(
            pkts.iter().any(|p| p.from.starts_with("N1TEST")),
            "got {pkts:?}"
        );
    }
}
