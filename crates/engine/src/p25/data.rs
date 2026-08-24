//! P25 Phase 1 packet-data header decoding.
//!
//! The first MPDU block uses the same rate-1/2 trellis/interleave and CRC-16
//! protection as a TSBK, but its 80 information bits have a packet header
//! layout rather than a trunking opcode layout.

use super::tsbk::{Convention, TSBK_DIBITS, Tsbk, deinterleave, trellis_decode};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PduHeader {
    pub acknowledge_needed: bool,
    pub inbound: bool,
    pub format: u8,
    pub format_name: String,
    pub service_access_point: u8,
    pub service_name: String,
    pub manufacturer_id: u8,
    pub logical_link_id: u32,
    pub blocks_to_follow: u8,
    pub full_message: bool,
    pub pad_octets: u8,
    pub sequence: u8,
    pub fragment_sequence: u8,
    pub data_offset: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PduMessage {
    pub header: PduHeader,
    pub blocks: usize,
    pub complete: bool,
    pub crc_ok: Option<bool>,
    pub payload: Vec<u8>,
    pub payload_hex: String,
    pub application: Option<String>,
    pub source_ip: Option<String>,
    pub destination_ip: Option<String>,
    pub source_port: Option<u16>,
    pub destination_port: Option<u16>,
    /// Raw manufacturer-specific/trunking blocks, retained even when their
    /// opcode is not yet named so captures can drive future decoders.
    pub vendor_blocks: Vec<String>,
}

pub fn decode_pdu_header(payload: &[u8]) -> Option<PduHeader> {
    if payload.len() < TSBK_DIBITS {
        return None;
    }
    let block = Convention::all()
        .into_iter()
        .filter_map(|convention| Tsbk::decode(payload, convention))
        .find(|block| block.crc_ok)?;
    Some(parse_header(block))
}

/// Reassemble all blocks present in one captured MPDU, including confirmed
/// rate-3/4 delivery with its per-block DBSN/CRC-9 fields.
pub fn decode_pdu(payload: &[u8]) -> Option<PduMessage> {
    let (convention, first) = Convention::all()
        .into_iter()
        .filter_map(|convention| Tsbk::decode(payload, convention).map(|b| (convention, b)))
        .find(|(_, block)| block.crc_ok)?;
    let header = parse_header(first);
    let available = payload.len().saturating_sub(TSBK_DIBITS) / TSBK_DIBITS;
    let wanted = usize::from(header.blocks_to_follow);
    let count = available.min(wanted);
    let confirmed = header.format == 0x16 && header.acknowledge_needed;
    let mut bytes = Vec::with_capacity(count * if confirmed { 16 } else { 12 });
    let mut vendor_blocks = Vec::new();
    let mut block_crc_ok = true;
    let mut expected_serial = None;
    for n in 0..count {
        let start = (n + 1) * TSBK_DIBITS;
        let air: [u8; TSBK_DIBITS] = payload[start..start + TSBK_DIBITS].try_into().ok()?;
        let block = if confirmed {
            let decoded = crate::dmr::trellis_three_quarter(&air)?;
            let serial = decoded[0] >> 1;
            let stored = (u16::from(decoded[0] & 1) << 8) | u16::from(decoded[1]);
            let mut checked = Vec::with_capacity(135);
            for bit in (0..7).rev() {
                checked.push((serial >> bit) & 1);
            }
            checked.extend(
                decoded[2..]
                    .iter()
                    .flat_map(|byte| (0..8).rev().map(move |bit| (byte >> bit) & 1)),
            );
            block_crc_ok &= crc9_p25(&checked) == stored;
            if let Some(previous) = expected_serial {
                block_crc_ok &= serial == previous;
            }
            expected_serial = Some(serial.wrapping_add(1) & 0x7f);
            decoded[2..].to_vec()
        } else {
            decode_half_rate(&air, convention)?.to_vec()
        };
        if matches!(header.service_access_point, 61 | 63) {
            vendor_blocks.push(hex(&block));
        }
        bytes.extend_from_slice(&block);
    }
    let complete = count == wanted;
    let crc_ok = (complete && bytes.len() >= 4).then(|| {
        let split = bytes.len() - 4;
        let stored = u32::from_be_bytes(bytes[split..].try_into().expect("four CRC bytes"));
        block_crc_ok && crc32_mbf(&bytes[..split], split * 8) == stored
    });
    if complete && bytes.len() >= 4 {
        bytes.truncate(bytes.len() - 4);
        let pad = usize::from(header.pad_octets).min(bytes.len());
        bytes.truncate(bytes.len() - pad);
    }
    let (application, source_ip, destination_ip, source_port, destination_port) =
        parse_application(&bytes, header.data_offset);
    Some(PduMessage {
        header,
        blocks: count,
        complete,
        crc_ok,
        payload_hex: hex(&bytes),
        payload: bytes,
        application,
        source_ip,
        destination_ip,
        source_port,
        destination_port,
        vendor_blocks,
    })
}

fn crc9_p25(bits: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &input in bits {
        let feedback = ((crc >> 8) & 1) ^ u16::from(input & 1);
        crc = (crc << 1) & 0x1ff;
        if feedback != 0 {
            crc ^= 0x059;
        }
    }
    crc ^ 0x1ff
}

fn decode_half_rate(dibits: &[u8], convention: Convention) -> Option<[u8; 12]> {
    if dibits.len() < TSBK_DIBITS {
        return None;
    }
    let bits: Vec<u8> = dibits[..TSBK_DIBITS]
        .iter()
        .flat_map(|dibit| [(dibit >> 1) & 1, dibit & 1])
        .collect();
    let decoded = trellis_decode(&deinterleave(&bits, convention.deinterleave_forward));
    let mut out = [0u8; 12];
    for (i, bit) in decoded.iter().take(96).enumerate() {
        out[i / 8] |= bit << (7 - i % 8);
    }
    Some(out)
}

fn crc32_mbf(bytes: &[u8], bits: usize) -> u32 {
    let mut crc = 0u64;
    for i in 0..bits {
        crc <<= 1;
        let bit = (bytes[i / 8] >> (7 - i % 8)) & 1;
        if ((crc >> 32) as u8 ^ bit) & 1 != 0 {
            crc ^= 0x04c1_1db7;
        }
    }
    (crc as u32) ^ u32::MAX
}

fn parse_application(
    payload: &[u8],
    data_offset: u8,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u16>,
    Option<u16>,
) {
    let offset = usize::from(data_offset).min(payload.len());
    let data = &payload[offset..];
    if data.len() < 20 || data[0] >> 4 != 4 {
        return (None, None, None, None, None);
    }
    let ihl = usize::from(data[0] & 0x0f) * 4;
    if ihl < 20 || data.len() < ihl {
        return (
            Some("IPv4 (truncated header)".into()),
            None,
            None,
            None,
            None,
        );
    }
    let src = format!("{}.{}.{}.{}", data[12], data[13], data[14], data[15]);
    let dst = format!("{}.{}.{}.{}", data[16], data[17], data[18], data[19]);
    if data[9] == 17 && data.len() >= ihl + 8 {
        let sport = u16::from_be_bytes([data[ihl], data[ihl + 1]]);
        let dport = u16::from_be_bytes([data[ihl + 2], data[ihl + 3]]);
        (
            Some("IPv4/UDP".into()),
            Some(src),
            Some(dst),
            Some(sport),
            Some(dport),
        )
    } else {
        (
            Some(format!("IPv4 protocol {}", data[9])),
            Some(src),
            Some(dst),
            None,
            None,
        )
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

fn parse_header(block: Tsbk) -> PduHeader {
    let args = block.args.to_be_bytes();
    let format = block.opcode & 0x1f;
    let sap = block.mfid & 0x3f;
    PduHeader {
        acknowledge_needed: block.protected,
        inbound: block.opcode & 0x20 == 0,
        format,
        format_name: format_name(format).into(),
        service_access_point: sap,
        service_name: sap_name(sap).into(),
        manufacturer_id: args[0],
        logical_link_id: u32::from_be_bytes([0, args[1], args[2], args[3]]),
        blocks_to_follow: args[4] & 0x7f,
        full_message: args[4] & 0x80 != 0,
        pad_octets: args[5] & 0x1f,
        sequence: (args[6] >> 4) & 0x07,
        fragment_sequence: args[6] & 0x0f,
        data_offset: args[7] & 0x3f,
    }
}

fn format_name(format: u8) -> &'static str {
    match format {
        0x03 => "response",
        0x15 => "unconfirmed delivery",
        0x16 => "confirmed delivery",
        0x17 => "alternate multi-block trunking",
        _ => "packet data",
    }
}

fn sap_name(sap: u8) -> &'static str {
    match sap {
        0 => "user data",
        1 => "encrypted user data",
        2 => "circuit data",
        3 => "circuit data control",
        4 => "packet data",
        5 => "ARP",
        6 => "SNDCP control",
        15 => "packet-data scan preamble",
        29 => "packet-data encryption support",
        31 => "extended address",
        32 => "registration and authorization",
        40 => "unencrypted key management",
        41 => "encrypted key management",
        48 => "location service",
        61 => "trunking control",
        63 => "encrypted trunking control",
        _ => "unknown SAP",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p25::tsbk::encode_test_block;

    #[test]
    fn packet_header_fields_follow_the_cai_bit_layout() {
        let block = Tsbk {
            last: false,
            protected: true,
            opcode: 0x20 | 0x16,
            mfid: 0x04,
            args: u64::from_be_bytes([0x90, 0x12, 0x34, 0x56, 0x83, 0x05, 0x27, 0x09]),
            crc_ok: true,
        };
        let header = parse_header(block);
        assert!(header.acknowledge_needed);
        assert!(!header.inbound);
        assert_eq!(header.format_name, "confirmed delivery");
        assert_eq!(header.service_name, "packet data");
        assert_eq!(header.logical_link_id, 0x123456);
        assert_eq!(header.blocks_to_follow, 3);
        assert!(header.full_message);
        assert_eq!(header.pad_octets, 5);
        assert_eq!(header.sequence, 2);
        assert_eq!(header.fragment_sequence, 7);
        assert_eq!(header.data_offset, 9);
    }

    #[test]
    fn an_unconfirmed_block_reassembles_and_checks_crc32() {
        let header = [
            0x15, 0x04, 0x90, 0x12, 0x34, 0x56, 0x01, 0x00, 0x10, 0x00, 0, 0,
        ];
        let mut data = [0u8; 12];
        data[..8].copy_from_slice(b"P25-DATA");
        let crc = crc32_mbf(&data[..8], 64);
        data[8..].copy_from_slice(&crc.to_be_bytes());
        let mut dibits = encode_test_block(header, true);
        dibits.extend(encode_test_block(data, false));

        let message = decode_pdu(&dibits).expect("packet");
        assert!(message.complete);
        assert_eq!(message.crc_ok, Some(true));
        assert_eq!(message.payload, b"P25-DATA");
        assert_eq!(message.header.logical_link_id, 0x123456);
    }

    #[test]
    fn a_confirmed_rate_three_quarter_block_checks_both_crcs() {
        let header = [
            0x56, 0x04, 0x90, 0x12, 0x34, 0x56, 0x01, 0x00, 0x10, 0x00, 0, 0,
        ];
        // Independently encoded 8-state trellis vector: DBSN 0, CRC-9 0x1C4,
        // twelve payload octets and a valid message CRC-32.
        let air = [
            0, 2, 0, 3, 3, 1, 3, 3, 2, 2, 2, 1, 2, 3, 2, 2, 1, 2, 0, 3, 1, 3, 3, 0, 3, 2, 0, 2, 1,
            1, 2, 3, 3, 3, 2, 2, 0, 1, 1, 1, 1, 1, 2, 1, 0, 0, 3, 1, 3, 3, 0, 1, 1, 3, 2, 2, 1, 1,
            2, 3, 3, 0, 2, 3, 2, 2, 3, 3, 0, 3, 3, 0, 3, 3, 0, 3, 2, 2, 3, 0, 2, 1, 0, 2, 0, 2, 2,
            2, 1, 2, 3, 3, 1, 0, 2, 3, 0, 0,
        ];
        let mut dibits = encode_test_block(header, true);
        dibits.extend(air);

        let message = decode_pdu(&dibits).expect("confirmed packet");
        assert!(message.complete);
        assert_eq!(message.crc_ok, Some(true));
        assert_eq!(message.payload, b"P25-CONFIRME");
        assert_eq!(message.blocks, 1);
    }
}
