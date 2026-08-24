//! NXDN SACCH/FACCH/CAC/SCCH channel coding and Layer-3 field extraction.

use super::{Frame, System};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlMessage {
    pub channel: String,
    pub message_type: u8,
    pub message_name: String,
    pub ran: Option<u8>,
    pub source_id: Option<u16>,
    pub target_id: Option<u16>,
    pub group: Option<bool>,
    pub channel_number: Option<u16>,
    pub location_id: Option<u32>,
    pub site_code: Option<u8>,
    pub service_options: Option<u8>,
    pub emergency: bool,
    pub cipher_type: Option<u8>,
    pub key_id: Option<u8>,
    pub valid: bool,
    pub raw: String,
}

pub fn decode_control(frame: &Frame) -> Vec<ControlMessage> {
    let bits = dibits_to_bits(&frame.payload);
    if bits.len() < 364 {
        return Vec::new();
    }
    let mut messages = Vec::new();
    if frame.system == System::TypeD {
        if let Some(decoded) = decode_sacch(&bits[16..76], true) {
            messages.push(parse_scch(&decoded, frame.outbound));
        }
    } else {
        if matches!(frame.lich, 0x01 | 0x05)
            && let Some(decoded) = decode_cac(&bits[16..316])
        {
            messages.push(parse_layer3("CAC", &decoded[8..155], None));
        }
        let facch = match frame.lich {
            0x32 | 0x33 | 0x52 | 0x53 => 1,
            0x34 | 0x35 | 0x54 | 0x55 => 2,
            0x20 | 0x21 | 0x30 | 0x31 | 0x40 | 0x41 | 0x50 | 0x51 => 3,
            _ => 0,
        };
        if facch & 1 != 0
            && let Some(decoded) = decode_facch(&bits[76..220])
        {
            messages.push(parse_layer3("FACCH1-A", &decoded[..80], None));
        }
        if facch & 2 != 0
            && let Some(decoded) = decode_facch(&bits[220..364])
        {
            messages.push(parse_layer3("FACCH1-B", &decoded[..80], None));
        }
        // A non-superframe SACCH carries a complete short Layer-3 message.
        if matches!(frame.lich, 0x20 | 0x21 | 0x40 | 0x41)
            && let Some(decoded) = decode_sacch(&bits[16..76], false)
        {
            let ran = Some(read(&decoded, 2, 6) as u8);
            messages.push(parse_layer3("SACCH", &decoded[8..26], ran));
        }
    }
    messages
}

fn decode_sacch(bits: &[u8], scch: bool) -> Option<Vec<u8>> {
    let decoded = convolutional(bits, 12, 5, &[1, 1, 1, 1, 1, 0], 7)?;
    let decoded = decoded.get(..32)?.to_vec();
    if scch {
        (crc7(&decoded[..25]) == read(&decoded, 25, 7) as u8).then_some(decoded)
    } else {
        (crc6(&decoded[..26]) == read(&decoded, 26, 6) as u8).then_some(decoded)
    }
}

fn decode_facch(bits: &[u8]) -> Option<Vec<u8>> {
    let decoded = convolutional(bits, 16, 9, &[1, 0, 1, 1], 8)?;
    let decoded = decoded.get(..92)?.to_vec();
    (crc12(&decoded[..80]) == read(&decoded, 80, 12) as u16).then_some(decoded)
}

fn decode_cac(bits: &[u8]) -> Option<Vec<u8>> {
    let decoded = convolutional(bits, 12, 25, &[1, 1, 1, 0, 1, 1, 1, 1, 1, 1, 1, 0, 1, 1], 8)?;
    let decoded = decoded.get(..171)?.to_vec();
    (crc16_cac(&decoded) == 0).then_some(decoded)
}

/// Hard-decision K=5, rate-1/2 Viterbi with NXDN block deinterleave and puncture.
fn convolutional(
    received: &[u8],
    columns: usize,
    rows: usize,
    puncture: &[u8],
    offset: usize,
) -> Option<Vec<u8>> {
    if received.len() != columns * rows {
        return None;
    }
    let mut deinterleaved = vec![0u8; received.len()];
    for column in 0..columns {
        for row in 0..rows {
            deinterleaved[column + columns * row] = received[column * rows + row] & 1;
        }
    }
    let mut encoded: Vec<Option<u8>> = Vec::new();
    let mut input = 0usize;
    let mut p = 0usize;
    while input < deinterleaved.len() {
        if puncture[p] != 0 {
            encoded.push(Some(deinterleaved[input]));
            input += 1;
        } else {
            encoded.push(None);
        }
        p = (p + 1) % puncture.len();
    }
    if encoded.len() % 2 != 0 {
        encoded.push(None);
    }
    let steps = encoded.len() / 2;
    let mut metrics = [0u32; 16];
    let mut history = vec![0u16; steps];
    const COST0: [u8; 8] = [0, 0, 0, 0, 1, 1, 1, 1];
    const COST1: [u8; 8] = [0, 1, 1, 0, 0, 1, 1, 0];
    for step in 0..steps {
        let observations = [encoded[2 * step], encoded[2 * step + 1]];
        let branch = |a: u8, b: u8| -> u32 {
            observations[0].map_or(1, |got| if got == a { 0 } else { 2 })
                + observations[1].map_or(1, |got| if got == b { 0 } else { 2 })
        };
        let mut next = [0u32; 16];
        for i in 0..8usize {
            let metric = branch(COST0[i], COST1[i]);
            let m0 = metrics[i] + metric;
            let m1 = metrics[i + 8] + (4 - metric);
            let m2 = metrics[i] + (4 - metric);
            let m3 = metrics[i + 8] + metric;
            let even = 2 * i;
            let odd = even + 1;
            if m0 >= m1 {
                history[step] |= 1 << even;
                next[even] = m1;
            } else {
                next[even] = m0;
            }
            if m2 >= m3 {
                history[step] |= 1 << odd;
                next[odd] = m3;
            } else {
                next[odd] = m2;
            }
        }
        metrics = next;
    }
    // This is the reference K=5 chainback convention. The four-bit position
    // lead is the encoder memory; per-channel offsets below remove it.
    let mut state = 0u8;
    let mut bit_position = steps + 4;
    let mut shifted = vec![0u8; steps + 8];
    for step in (0..steps).rev() {
        bit_position -= 1;
        let decision = history[step] & (1 << (state >> 4));
        state >>= 1;
        if decision != 0 {
            state |= 0x80;
            shifted[bit_position] = 1;
        }
    }
    shifted.get(offset..).map(ToOwned::to_owned)
}

fn parse_layer3(channel: &str, bits: &[u8], ran: Option<u8>) -> ControlMessage {
    let message_type = read(bits, 2, 6) as u8;
    let mut message = ControlMessage {
        channel: channel.into(),
        message_type,
        message_name: message_name(message_type).into(),
        ran,
        source_id: None,
        target_id: None,
        group: None,
        channel_number: None,
        location_id: None,
        site_code: None,
        service_options: None,
        emergency: false,
        cipher_type: None,
        key_id: None,
        valid: true,
        raw: hex(bits),
    };
    match message_type {
        0x01 | 0x07 | 0x08 | 0x11 if bits.len() >= 64 => {
            let options = read(bits, 8, 8) as u8;
            let call_type = read(bits, 16, 3) as u8;
            message.service_options = Some(options);
            message.emergency = options & 0x80 != 0;
            message.source_id = Some(read(bits, 24, 16) as u16);
            message.target_id = Some(read(bits, 40, 16) as u16);
            message.group = Some(call_type == 0 || call_type == 1);
            message.cipher_type = Some(read(bits, 56, 2) as u8);
            message.key_id = Some(read(bits, 58, 6) as u8);
        }
        0x04 | 0x05 | 0x0d | 0x0e if bits.len() >= 72 => {
            let options = read(bits, 8, 8) as u8;
            let call_type = read(bits, 16, 3) as u8;
            message.service_options = Some(options);
            message.emergency = options & 0x80 != 0;
            message.source_id = Some(read(bits, 24, 16) as u16);
            message.target_id = Some(read(bits, 40, 16) as u16);
            message.group = Some(call_type == 0 || call_type == 1);
            message.channel_number = Some(read(bits, 62, 10) as u16);
        }
        0x18 | 0x19 if bits.len() >= 32 => {
            let location = read(bits, 8, 24);
            message.location_id = Some(location);
            message.site_code = Some((location & 0xff) as u8);
        }
        _ => {}
    }
    message
}

fn parse_scch(bits: &[u8], outbound: bool) -> ControlMessage {
    let sf = read(bits, 0, 2) as u8;
    let area = bits[2];
    let repeater = read(bits, 3, 5) as u16;
    let home = read(bits, 8, 5) as u16;
    let id = read(bits, 13, 11) as u16;
    let group = bits[24] == 0;
    let (name, target) = if sf == 0 || sf == 3 {
        let name = match id {
            2046 => "Idle Repeater",
            2045 => "Halt Repeater",
            2044 => "Free Repeater",
            2041 => "Site ID",
            _ => "Busy Repeater / Channel Update",
        };
        (name, (!matches!(id, 2041 | 2044..=2046)).then_some(id))
    } else {
        ("Type-D call continuation", None)
    };
    ControlMessage {
        channel: "SCCH".into(),
        message_type: (u8::from(outbound) << 2) | sf,
        message_name: name.into(),
        ran: Some(area),
        source_id: None,
        target_id: target,
        group: target.map(|_| group),
        channel_number: Some(repeater),
        location_id: None,
        site_code: (id == 2041).then_some(home as u8),
        service_options: None,
        emergency: false,
        cipher_type: None,
        key_id: None,
        valid: true,
        raw: hex(bits),
    }
}

fn message_name(value: u8) -> &'static str {
    match value {
        0x01 => "Voice Call",
        0x03 => "Voice Call IV",
        0x04 => "Voice Call Assignment",
        0x05 => "Voice Call Assignment Duplicate",
        0x07 => "Transmission Release Extended",
        0x08 => "Transmission Release",
        0x0d => "Data Call Assignment Duplicate",
        0x0e => "Data Call Assignment",
        0x11 => "Disconnect",
        0x17 => "Destination ID Information",
        0x18 => "Site Information",
        0x19 => "Service Information",
        0x1a => "Control Channel Information",
        0x1b => "Adjacent Site Information",
        0x3f => "Proprietary",
        _ => "NXDN Layer-3 Message",
    }
}

fn dibits_to_bits(dibits: &[u8]) -> Vec<u8> {
    dibits.iter().flat_map(|d| [(d >> 1) & 1, d & 1]).collect()
}

fn read(bits: &[u8], start: usize, len: usize) -> u32 {
    bits.get(start..start + len)
        .unwrap_or(&[])
        .iter()
        .fold(0, |value, bit| (value << 1) | u32::from(*bit))
}

fn hex(bits: &[u8]) -> String {
    bits.chunks(8)
        .map(|chunk| format!("{:02X}", chunk.iter().fold(0u8, |v, b| (v << 1) | b)))
        .collect()
}

fn crc6(bits: &[u8]) -> u8 {
    let mut s = [1u8; 6];
    for &bit in bits {
        let a = bit ^ s[0];
        s = [a ^ s[1], s[2], s[3], a ^ s[4], a ^ s[5], a];
    }
    s.into_iter().fold(0, |v, bit| (v << 1) | bit)
}

fn crc7(bits: &[u8]) -> u8 {
    let mut s = [1u8; 7];
    for &bit in bits {
        let a = bit ^ s[0];
        s = [s[1], s[2], s[3], a ^ s[4], s[5], s[6], a];
    }
    s.into_iter().fold(0, |v, bit| (v << 1) | bit)
}

fn crc12(bits: &[u8]) -> u16 {
    let mut s = [1u8; 12];
    for &bit in bits {
        let a = bit ^ s[0];
        s = [
            a ^ s[1],
            s[2],
            s[3],
            s[4],
            s[5],
            s[6],
            s[7],
            s[8],
            a ^ s[9],
            a ^ s[10],
            a ^ s[11],
            a,
        ];
    }
    s.into_iter().fold(0, |v, bit| (v << 1) | u16::from(bit))
}

fn crc16_cac(bits: &[u8]) -> u16 {
    let mut crc = 0xc3eeu32;
    for &bit in bits {
        crc = ((crc << 1) | u32::from(bit)) & 0x1ffff;
        if crc & 0x10000 != 0 {
            crc = (crc & 0xffff) ^ 0x1021;
        }
    }
    (crc ^ 0xffff) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_three_call_fields_are_extracted() {
        let mut bits = vec![0u8; 80];
        let put = |bits: &mut [u8], start: usize, len: usize, value: u32| {
            for i in 0..len {
                bits[start + i] = ((value >> (len - 1 - i)) & 1) as u8;
            }
        };
        put(&mut bits, 2, 6, 0x04);
        put(&mut bits, 8, 8, 0x80);
        put(&mut bits, 16, 3, 1);
        put(&mut bits, 24, 16, 123);
        put(&mut bits, 40, 16, 456);
        put(&mut bits, 62, 10, 77);
        let got = parse_layer3("test", &bits, Some(9));
        assert_eq!(got.message_name, "Voice Call Assignment");
        assert_eq!(got.source_id, Some(123));
        assert_eq!(got.target_id, Some(456));
        assert_eq!(got.channel_number, Some(77));
        assert!(got.emergency);
    }
}
