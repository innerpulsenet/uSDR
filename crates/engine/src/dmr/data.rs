//! DMR link-control, privacy-indicator, and packet-header decoding.
//!
//! These burst types share the BPTC(196,96) channel code used by CSBK.
//! Link control protects nine payload octets with RS(12,9); the other
//! headers use the masked DMR CRC-CCITT.

use super::csbk::{crc_ccitt, decode_bptc, read_bits};
use super::fec::hamming16114_decode;
use super::framer::{Burst, BurstKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkControl {
    pub protected: bool,
    pub group: bool,
    pub opcode: u8,
    pub opcode_name: String,
    pub feature_id: u8,
    pub manufacturer: String,
    pub service_options: u8,
    pub emergency: bool,
    pub encrypted: bool,
    pub broadcast: bool,
    pub open_voice: bool,
    pub priority: u8,
    pub target_id: u32,
    pub source_id: u32,
    pub capacity_plus_rest_lsn: Option<u8>,
    pub talker_alias: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub position_error_m: Option<u32>,
    pub corrected_bits: u32,
    pub raw: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivacyHeader {
    pub algorithm_id: u8,
    pub algorithm_name: String,
    pub feature_id: u8,
    pub manufacturer: String,
    pub key_id: u8,
    pub message_indicator: String,
    pub target_id: u32,
    pub corrected_bits: u32,
    pub raw: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataHeader {
    pub group: bool,
    pub response_requested: bool,
    pub format: u8,
    pub format_name: String,
    pub service_access_point: u8,
    pub service_name: String,
    pub target_id: u32,
    pub source_id: u32,
    pub blocks_to_follow: Option<u8>,
    pub confirmed: bool,
    pub udt: Option<UdtHeader>,
    pub corrected_bits: u32,
    pub raw: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UdtHeader {
    pub format: u8,
    pub format_name: String,
    pub pad_nibbles: u8,
    pub appended_blocks: u8,
    pub supplementary: bool,
    pub protected: bool,
    pub opcode: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataApplication {
    pub kind: String,
    pub summary: String,
    pub fields: BTreeMap<String, String>,
}

/// One coded data block following a header. The blocks are what actually
/// carry the message; the header only says how many are coming and what they
/// are for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataBlock {
    /// Payload octets this block contributes, already stripped of the
    /// per-block serial number and CRC when the header said "confirmed".
    pub payload: Vec<u8>,
    /// Block sequence number, present only on confirmed blocks.
    pub serial: Option<u8>,
    /// Whether the confirmed block's own CRC-9 checked out. `None` when the
    /// block carries no CRC to check.
    pub block_crc_ok: Option<bool>,
    pub rate: String,
    pub corrected_bits: u32,
    pub raw: String,
}

/// A reassembled data message: the header, its blocks, and whatever the
/// payload turned out to be.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataMessage {
    pub header: DataHeader,
    pub blocks: usize,
    /// True when the trailing CRC-32 over the reassembled payload matched.
    pub crc_ok: bool,
    /// Decoded text when the payload is a recognisable short message.
    pub text: Option<String>,
    /// How the text was recovered — "UTF-16LE", "ASCII", or absent.
    pub encoding: Option<String>,
    pub payload: Vec<u8>,
    pub payload_hex: String,
    pub application: Option<DataApplication>,
}

/// Unified Single Block Data. Service type zero is the standardized compact
/// Location Information Protocol response used by DMR radios.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedSingleBlockData {
    pub service_type: u8,
    pub service_name: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub position_error_m: Option<u32>,
    pub speed_kph: Option<f64>,
    pub direction_degrees: Option<f64>,
    pub time_elapsed: Option<String>,
    pub reason: Option<u8>,
    pub source_hash: Option<u8>,
    pub corrected_bits: u32,
    pub raw: String,
}

/// Complete application-layer data recovered from a traffic channel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
pub enum DecodedData {
    Message(DataMessage),
    UnifiedSingleBlock(UnifiedSingleBlockData),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DataPdu {
    LinkControl(LinkControl),
    PrivacyHeader(PrivacyHeader),
    DataHeader(DataHeader),
    DataBlock(DataBlock),
    UnifiedSingleBlockData(UnifiedSingleBlockData),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataError {
    Unsupported,
    Fec,
    Checksum { raw: String },
}

/// Collects the four 32-bit fragments carried by voice bursts B-E and
/// decodes the BPTC(128,77)-protected Full Link Control. Kept per timeslot by
/// callers so simultaneous repeater conversations cannot contaminate it.
#[derive(Clone, Debug, Default)]
pub struct EmbeddedLcAssembler {
    fragments: [Option<[u8; 32]>; 4],
    alias_bits: Vec<u8>,
    alias_char_bits: usize,
    alias_length: usize,
}

impl EmbeddedLcAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.fragments = [None; 4];
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn push(&mut self, burst: &Burst) -> Option<LinkControl> {
        let (emb, voice_idx) = match &burst.kind {
            BurstKind::Voice {
                emb: Some(emb),
                voice_idx,
                ..
            } => (emb, *voice_idx),
            _ => return None,
        };
        if voice_idx == 1 || emb.lcss == 1 {
            self.reset();
        }
        if !(1..=4).contains(&voice_idx) {
            return None;
        }
        let mut fragment = [0u8; 32];
        for (i, dibit) in burst.dibits[58..74].iter().copied().enumerate() {
            fragment[i * 2] = dibit >> 1;
            fragment[i * 2 + 1] = dibit & 1;
        }
        self.fragments[usize::from(voice_idx - 1)] = Some(fragment);
        if self.fragments.iter().any(Option::is_none) {
            return None;
        }
        let mut coded = [0u8; 128];
        for (i, fragment) in self.fragments.iter().flatten().enumerate() {
            coded[i * 32..i * 32 + 32].copy_from_slice(fragment);
        }
        self.reset();
        let mut lc = decode_embedded_lc(&coded).ok()?;
        self.observe_alias(&mut lc);
        Some(lc)
    }

    fn observe_alias(&mut self, lc: &mut LinkControl) {
        let bytes = match hex_bytes(&lc.raw) {
            Some(bytes) if bytes.len() == 9 => bytes,
            _ => return,
        };
        let opcode = if lc.feature_id == 0x10 && (0x14..=0x17).contains(&lc.opcode) {
            lc.opcode - 0x10
        } else {
            lc.opcode
        };
        let bits = bytes_to_bits(&bytes);
        if opcode == 0x04 {
            let format = read_bits_slice(&bits, 16, 2) as usize;
            self.alias_char_bits = match format {
                0 => 7,
                3 => 16,
                _ => 8,
            };
            self.alias_length = read_bits_slice(&bits, 18, 5) as usize;
            self.alias_bits.clear();
            self.alias_bits
                .extend_from_slice(if self.alias_char_bits == 7 {
                    &bits[23..72]
                } else {
                    &bits[24..72]
                });
        } else if (0x05..=0x07).contains(&opcode) && self.alias_char_bits != 0 {
            let block = usize::from(opcode - 0x05);
            let base = if self.alias_char_bits == 7 { 49 } else { 48 } + block * 56;
            if self.alias_bits.len() < base + 56 {
                self.alias_bits.resize(base + 56, 0);
            }
            self.alias_bits[base..base + 56].copy_from_slice(&bits[16..72]);
        }
        lc.talker_alias = decode_alias(&self.alias_bits, self.alias_char_bits, self.alias_length);
    }
}

fn decode_alias(bits: &[u8], char_bits: usize, length: usize) -> Option<String> {
    if char_bits == 0 || length == 0 || bits.len() < char_bits.min(7) {
        return None;
    }
    let available = (bits.len() / char_bits).min(length);
    if available == 0 {
        return None;
    }
    let text = if char_bits == 16 {
        let units: Vec<u16> = (0..available)
            .map(|i| read_bits_slice(bits, i * 16, 16) as u16)
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        (0..available)
            .map(|i| read_bits_slice(bits, i * char_bits, char_bits) as u8 as char)
            .collect()
    };
    let text = text.trim_matches(['\0', ' ']).to_string();
    (!text.is_empty()).then_some(text)
}

fn hex_bytes(raw: &str) -> Option<Vec<u8>> {
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    (0..raw.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&raw[i..i + 2], 16).ok())
        .collect()
}

fn read_bits_slice(bits: &[u8], start: usize, width: usize) -> u32 {
    bits.get(start..start + width)
        .unwrap_or_default()
        .iter()
        .fold(0, |value, &bit| (value << 1) | u32::from(bit))
}

fn decode_embedded_lc(coded: &[u8; 128]) -> Result<LinkControl, DataError> {
    let mut matrix = [[0u8; 16]; 8];
    for column in 0..16 {
        for row in 0..8 {
            matrix[row][column] = coded[column * 8 + row];
        }
    }
    let mut corrected_bits = 0;
    for row in matrix.iter_mut().take(7) {
        corrected_bits += hamming16114_decode(row).ok_or(DataError::Fec)?;
    }
    for column in 0..16 {
        let parity = matrix
            .iter()
            .take(7)
            .map(|row| row[column])
            .fold(0, |a, b| a ^ b);
        if parity != matrix[7][column] {
            return Err(DataError::Fec);
        }
    }
    let mut bits = Vec::with_capacity(77);
    for row in matrix.iter().take(2) {
        bits.extend_from_slice(&row[..11]);
    }
    for row in matrix.iter().take(7).skip(2) {
        bits.extend_from_slice(&row[..10]);
    }
    for row in matrix.iter().take(7).skip(2) {
        bits.push(row[10]);
    }
    let stored_crc = bits[72..].iter().fold(0u8, |value, bit| (value << 1) | bit);
    let bytes = bits_to_vec(&bits[..72]);
    if embedded_crc5(&bytes) != stored_crc {
        return Err(DataError::Checksum { raw: hex(&bytes) });
    }
    let bytes: [u8; 9] = bytes.try_into().map_err(|_| DataError::Fec)?;
    Ok(link_control_fields(&bytes, corrected_bits, hex(&bytes)))
}

fn embedded_crc5(bytes: &[u8]) -> u8 {
    (bytes.iter().map(|&byte| u32::from(byte)).sum::<u32>() % 31) as u8
}

pub fn decode_data_pdu(burst: &Burst) -> Result<DataPdu, DataError> {
    let data_type = match &burst.kind {
        BurstKind::Data {
            slot_type: Some(slot_type),
            ..
        } => slot_type.data_type,
        _ => return Err(DataError::Unsupported),
    };
    let (bits, corrected_bits) = decode_bptc(&burst.bptc_payload_bits()).ok_or(DataError::Fec)?;
    let bytes = to_bytes(&bits);
    let raw = hex(&bytes);
    match data_type {
        0 => decode_privacy(bytes, corrected_bits, raw).map(DataPdu::PrivacyHeader),
        1 => decode_link_control(bytes, corrected_bits, raw, 0x96_96_96).map(DataPdu::LinkControl),
        2 => decode_link_control(bytes, corrected_bits, raw, 0x99_99_99).map(DataPdu::LinkControl),
        6 => decode_data_header(bits, bytes, corrected_bits, raw).map(DataPdu::DataHeader),
        // Confirmed and unconfirmed packet blocks. The header that preceded
        // these says how many are coming; `DataAssembler` puts them back
        // together and decides whether the first two octets are DBSN/CRC-9.
        7 => Ok(DataPdu::DataBlock(decode_rate_half_block(
            bytes,
            corrected_bits,
            raw,
        ))),
        8 => decode_rate_three_quarter_block(burst).map(DataPdu::DataBlock),
        10 => Ok(DataPdu::DataBlock(decode_full_rate_block(burst))),
        11 => decode_unified_single_block(bits, bytes, corrected_bits, raw)
            .map(DataPdu::UnifiedSingleBlockData),
        _ => Err(DataError::Unsupported),
    }
}

fn decode_unified_single_block(
    bits: [u8; 96],
    bytes: [u8; 12],
    corrected_bits: u32,
    raw: String,
) -> Result<UnifiedSingleBlockData, DataError> {
    // ETSI transmits the one's-complement mask used by the DSD family as
    // 0x3333; crc_ccitt() has the opposite final convention, hence 0xCCCC.
    if u16::from_be_bytes([bytes[10], bytes[11]]) != crc_ccitt(&bytes[..10]) ^ 0xcccc {
        return Err(DataError::Checksum { raw });
    }
    let service_type = read_bits(&bits, 0, 4) as u8;
    let mut pdu = UnifiedSingleBlockData {
        service_type,
        service_name: if service_type == 0 {
            "Location Information Protocol".into()
        } else if service_type > 8 {
            "manufacturer specific".into()
        } else {
            "reserved".into()
        },
        latitude: None,
        longitude: None,
        position_error_m: None,
        speed_kph: None,
        direction_degrees: None,
        time_elapsed: None,
        reason: None,
        source_hash: None,
        corrected_bits,
        raw,
    };
    if service_type != 0 {
        return Ok(pdu);
    }

    let elapsed = read_bits(&bits, 6, 2) as u8;
    let lon_raw = read_bits(&bits, 9, 24);
    let lat_raw = read_bits(&bits, 34, 23);
    let latitude = if bits[33] != 0 {
        -f64::from(0x80_0001u32.saturating_sub(lat_raw)) * 180.0 / 2f64.powi(24)
    } else {
        f64::from(lat_raw) * 180.0 / 2f64.powi(24)
    };
    let longitude = if bits[8] != 0 {
        -f64::from(0x100_0001u32.saturating_sub(lon_raw)) * 360.0 / 2f64.powi(25)
    } else {
        f64::from(lon_raw) * 360.0 / 2f64.powi(25)
    };
    let position_error = read_bits(&bits, 57, 2) as u8;
    let velocity = read_bits(&bits, 59, 7) as u8;
    pdu.latitude = (latitude.abs() < 90.0).then_some(latitude);
    pdu.longitude = (longitude.abs() < 180.0).then_some(longitude);
    pdu.position_error_m = Some(2 * 10u32.pow(u32::from(position_error)));
    pdu.speed_kph = Some(if velocity <= 28 {
        f64::from(velocity)
    } else {
        16.0 * 1.038f64.powf(f64::from(velocity) - 13.0)
    });
    pdu.direction_degrees = Some(f64::from(read_bits(&bits, 66, 4)) * 22.5);
    pdu.time_elapsed = Some(
        match elapsed {
            0 => "less than 5 seconds",
            1 => "less than 5 minutes",
            2 => "less than 30 minutes",
            _ => "unknown",
        }
        .into(),
    );
    pdu.reason = Some(read_bits(&bits, 70, 3) as u8);
    pdu.source_hash = Some(read_bits(&bits, 73, 8) as u8);
    Ok(pdu)
}

fn decode_link_control(
    mut bytes: [u8; 12],
    corrected_bits: u32,
    raw: String,
    mask: u32,
) -> Result<LinkControl, DataError> {
    bytes[9] ^= (mask >> 16) as u8;
    bytes[10] ^= (mask >> 8) as u8;
    bytes[11] ^= mask as u8;
    if rs129_parity(&bytes[..9]).as_slice() != &bytes[9..12] {
        return Err(DataError::Checksum { raw });
    }
    Ok(link_control_fields(&bytes[..9], corrected_bits, raw))
}

fn link_control_fields(bytes: &[u8], corrected_bits: u32, raw: String) -> LinkControl {
    let opcode = bytes[0] & 0x3f;
    let feature_id = bytes[1];
    let service_options = bytes[2];
    let target_id = u32::from_be_bytes([0, bytes[3], bytes[4], bytes[5]]);
    let source_id = u32::from_be_bytes([0, bytes[6], bytes[7], bytes[8]]);
    let capacity_plus = feature_id == 0x10 && matches!(opcode, 0x04 | 0x07);
    let embedded_gps = feature_id == 0 && opcode == 0x08 && bytes[0] & 0x80 == 0;
    let (latitude, longitude, position_error_m) = if embedded_gps {
        let bits = bytes_to_bits(bytes);
        let lat_raw = read_bits_slice(&bits, 49, 23);
        let lon_raw = read_bits_slice(&bits, 24, 24);
        let latitude = if bits[48] != 0 {
            -f64::from(0x80_0001u32.saturating_sub(lat_raw)) * 180.0 / 2f64.powi(24)
        } else {
            f64::from(lat_raw) * 180.0 / 2f64.powi(24)
        };
        let longitude = if bits[23] != 0 {
            -f64::from(0x100_0001u32.saturating_sub(lon_raw)) * 360.0 / 2f64.powi(25)
        } else {
            f64::from(lon_raw) * 360.0 / 2f64.powi(25)
        };
        let error = read_bits_slice(&bits, 20, 3);
        (
            (latitude.abs() < 90.0).then_some(latitude),
            (longitude.abs() < 180.0).then_some(longitude),
            (error != 7).then_some(2 * 10u32.pow(error)),
        )
    } else {
        (None, None, None)
    };
    LinkControl {
        protected: bytes[0] & 0x80 != 0,
        group: bytes[0] & 0x40 != 0 || opcode == 0,
        opcode,
        opcode_name: link_control_name(opcode, feature_id).into(),
        feature_id,
        manufacturer: manufacturer(feature_id).into(),
        service_options,
        emergency: service_options & 0x80 != 0,
        encrypted: service_options & 0x40 != 0,
        broadcast: service_options & 0x08 != 0,
        open_voice: service_options & 0x04 != 0,
        priority: service_options & 0x03,
        target_id,
        source_id: if capacity_plus {
            u32::from(u16::from_be_bytes([bytes[7], bytes[8]]))
        } else {
            source_id
        },
        capacity_plus_rest_lsn: capacity_plus.then_some(bytes[6] & 0x0f),
        talker_alias: None,
        latitude,
        longitude,
        position_error_m,
        corrected_bits,
        raw,
    }
}

fn decode_privacy(
    bytes: [u8; 12],
    corrected_bits: u32,
    raw: String,
) -> Result<PrivacyHeader, DataError> {
    if u16::from_be_bytes([bytes[10], bytes[11]]) != crc_ccitt(&bytes[..10]) ^ 0x9696 {
        return Err(DataError::Checksum { raw });
    }
    let feature_id = bytes[1];
    let mi_len = if feature_id == 0x68 { 5 } else { 4 };
    // ETSI-compatible implementations appear both as 0x0N and 0x2N on air.
    // Keep one canonical ID so key lookup and telemetry agree.
    let algorithm_id = if feature_id == 0x10 && matches!(bytes[0] & 0x07, 1 | 2 | 4 | 5) {
        0x20 | (bytes[0] & 0x07)
    } else {
        bytes[0]
    };
    Ok(PrivacyHeader {
        algorithm_id,
        algorithm_name: privacy_algorithm(bytes[0], feature_id).into(),
        feature_id,
        manufacturer: manufacturer(feature_id).into(),
        key_id: bytes[2],
        message_indicator: hex(&bytes[3..3 + mi_len]),
        target_id: u32::from_be_bytes([0, bytes[7], bytes[8], bytes[9]]),
        corrected_bits,
        raw,
    })
}

fn decode_data_header(
    bits: [u8; 96],
    bytes: [u8; 12],
    corrected_bits: u32,
    raw: String,
) -> Result<DataHeader, DataError> {
    if u16::from_be_bytes([bytes[10], bytes[11]]) != crc_ccitt(&bytes[..10]) ^ 0x3333 {
        return Err(DataError::Checksum { raw });
    }
    let format = read_bits(&bits, 4, 4) as u8;
    let sap = read_bits(&bits, 8, 4) as u8;
    let udt = (format == 0).then(|| {
        let udt_format = read_bits(&bits, 12, 4) as u8;
        let mut appended_blocks = read_bits(&bits, 70, 2) as u8 + 1;
        // ETSI reserves UAB=3 for NMEA; deployed Motorola radios use it for
        // the two-block long form, matching DSD-FME's live-sample handling.
        if udt_format == 5 && appended_blocks == 3 {
            appended_blocks = 2;
        }
        UdtHeader {
            format: udt_format,
            format_name: udt_format_name(udt_format).into(),
            pad_nibbles: read_bits(&bits, 64, 5) as u8,
            appended_blocks,
            supplementary: bits[72] != 0,
            protected: bits[73] != 0,
            opcode: read_bits(&bits, 74, 6) as u8,
        }
    });
    Ok(DataHeader {
        group: bits[0] != 0,
        response_requested: bits[1] != 0,
        format,
        format_name: data_format_name(format).into(),
        service_access_point: sap,
        service_name: service_name(sap).into(),
        target_id: read_bits(&bits, 16, 24),
        source_id: read_bits(&bits, 40, 24),
        blocks_to_follow: udt
            .as_ref()
            .map(|header| header.appended_blocks)
            .or_else(|| matches!(format, 2 | 3).then(|| read_bits(&bits, 65, 7) as u8)),
        confirmed: format == 3 || (matches!(format, 0 | 13 | 14) && bits[1] != 0),
        udt,
        corrected_bits,
        raw,
    })
}

/// A rate-1/2 data block carries twelve octets. On a *confirmed* message the
/// first two hold a 7-bit block serial number and a 9-bit CRC over the rest,
/// leaving ten octets of payload; on an unconfirmed one all twelve are
/// payload. Which it is comes from the header, not the block, so both
/// readings are produced and the assembler picks.
fn decode_rate_half_block(bytes: [u8; 12], corrected_bits: u32, raw: String) -> DataBlock {
    let (serial, crc_ok) = confirmed_block_fields(&bytes, 0x0f0);
    DataBlock {
        payload: bytes.to_vec(),
        serial: Some(serial),
        block_crc_ok: Some(crc_ok),
        rate: "1/2".into(),
        corrected_bits,
        raw,
    }
}

fn decode_rate_three_quarter_block(burst: &Burst) -> Result<DataBlock, DataError> {
    let bytes = trellis_three_quarter(&burst.data_payload_dibits()).ok_or(DataError::Fec)?;
    let (serial, crc_ok) = confirmed_block_fields(&bytes, 0x1ff);
    Ok(DataBlock {
        payload: bytes.to_vec(),
        serial: Some(serial),
        block_crc_ok: Some(crc_ok),
        rate: "3/4".into(),
        corrected_bits: 0,
        raw: hex(&bytes),
    })
}

fn decode_full_rate_block(burst: &Burst) -> DataBlock {
    let bits = burst.bptc_payload_bits();
    // Four reserved bits split the two 96-bit uncoded halves.
    let mut useful = Vec::with_capacity(192);
    useful.extend_from_slice(&bits[..96]);
    useful.extend_from_slice(&bits[100..]);
    let bytes = bits_to_vec(&useful);
    let (serial, crc_ok) = confirmed_block_fields(&bytes, 0x10f);
    DataBlock {
        payload: bytes.clone(),
        serial: Some(serial),
        block_crc_ok: Some(crc_ok),
        rate: "1".into(),
        corrected_bits: 0,
        raw: hex(&bytes),
    }
}

/// ETSI rate-3/4 8-state trellis. The air carries 49 dibit pairs: 48 input
/// tribits plus a zero tail transition. A hard-decision Viterbi search still
/// corrects constellation errors by choosing the minimum-distance path.
pub(crate) fn trellis_three_quarter(input: &[u8; 98]) -> Option<[u8; 18]> {
    const INTERLEAVE: [usize; 98] = [
        0, 1, 8, 9, 16, 17, 24, 25, 32, 33, 40, 41, 48, 49, 56, 57, 64, 65, 72, 73, 80, 81, 88, 89,
        96, 97, 2, 3, 10, 11, 18, 19, 26, 27, 34, 35, 42, 43, 50, 51, 58, 59, 66, 67, 74, 75, 82,
        83, 90, 91, 4, 5, 12, 13, 20, 21, 28, 29, 36, 37, 44, 45, 52, 53, 60, 61, 68, 69, 76, 77,
        84, 85, 92, 93, 6, 7, 14, 15, 22, 23, 30, 31, 38, 39, 46, 47, 54, 55, 62, 63, 70, 71, 78,
        79, 86, 87, 94, 95,
    ];
    const X: [[i8; 8]; 8] = [
        [1, -3, -3, 1, 3, -1, -1, 3],
        [-3, 1, 3, -1, -1, 3, 1, -3],
        [-1, 3, 3, -1, -3, 1, 1, -3],
        [3, -1, -3, 1, 1, -3, -1, 3],
        [-3, 1, 1, -3, -1, 3, 3, -1],
        [1, -3, -1, 3, 3, -1, -3, 1],
        [3, -1, -1, 3, 1, -3, -3, 1],
        [-1, 3, 1, -3, -3, 1, 3, -1],
    ];
    const Y: [[i8; 8]; 8] = [
        [-1, 3, -1, 3, -3, 1, -3, 1],
        [-1, 3, -3, 1, -3, 1, -1, 3],
        [-1, 3, -1, 3, -3, 1, -3, 1],
        [-1, 3, -3, 1, -3, 1, -1, 3],
        [-3, 1, -3, 1, -1, 3, -1, 3],
        [-3, 1, -1, 3, -1, 3, -3, 1],
        [-3, 1, -3, 1, -1, 3, -1, 3],
        [-3, 1, -1, 3, -1, 3, -3, 1],
    ];
    fn coordinate(two_bits: u8) -> i8 {
        [1, 3, -1, -3][usize::from(two_bits & 3)]
    }
    fn distance(nibble: u8, x: i8, y: i8) -> u32 {
        u32::from(coordinate(nibble >> 2).abs_diff(x)) + u32::from(coordinate(nibble).abs_diff(y))
    }

    let mut deinterleaved = [0u8; 98];
    for i in 0..98 {
        deinterleaved[INTERLEAVE[i]] = input[i];
    }
    let symbols: [u8; 49] =
        std::array::from_fn(|i| (deinterleaved[i * 2] << 2) | deinterleaved[i * 2 + 1]);
    let inf = u32::MAX / 4;
    let mut metrics = [inf; 8];
    metrics[0] = 0;
    let mut previous = [[0u8; 8]; 48];
    for step in 0..48 {
        let mut next = [inf; 8];
        for state in 0..8 {
            if metrics[state] == inf {
                continue;
            }
            for input_value in 0..8 {
                let metric = metrics[state]
                    + distance(symbols[step], X[state][input_value], Y[state][input_value]);
                if metric < next[input_value] {
                    next[input_value] = metric;
                    previous[step][input_value] = state as u8;
                }
            }
        }
        metrics = next;
    }
    let (mut state, _) = (0..8)
        .map(|state| {
            let metric = metrics[state] + distance(symbols[48], X[state][0], Y[state][0]);
            (state, metric)
        })
        .min_by_key(|(_, metric)| *metric)?;
    let mut tribits = [0u8; 48];
    for step in (0..48).rev() {
        tribits[step] = state as u8;
        state = usize::from(previous[step][state]);
    }
    let mut out = [0u8; 18];
    for (i, value) in tribits.into_iter().enumerate() {
        for bit in 0..3 {
            let position = i * 3 + bit;
            out[position / 8] |= ((value >> (2 - bit)) & 1) << (7 - position % 8);
        }
    }
    Some(out)
}

fn confirmed_block_fields(bytes: &[u8], crc_mask: u16) -> (u8, bool) {
    if bytes.len() < 2 {
        return (0, false);
    }
    let serial = bytes[0] >> 1;
    let stored = (((u16::from(bytes[0]) & 1) << 8) | u16::from(bytes[1])) ^ crc_mask;
    let bits = bytes_to_bits(bytes);
    let mut check = Vec::with_capacity(bits.len() - 9);
    check.extend_from_slice(&bits[16..]);
    check.extend_from_slice(&bits[..7]);
    (serial, crc9_bits(&check) == stored)
}

/// CRC-9 over a confirmed data block's payload, per ETSI TS 102 361-1 B.3.
/// Polynomial x^9 + x^6 + x^4 + x^3 + 1, seeded to all ones and inverted.
fn crc9_bits(payload: &[u8]) -> u16 {
    let mut crc: u16 = 0x1FF;
    for &input in payload {
        let feedback = ((crc >> 8) & 1) ^ u16::from(input & 1);
        crc = (crc << 1) & 0x1FF;
        if feedback != 0 {
            crc ^= 0x059;
        }
    }
    crc ^ 0x1FF
}

fn bytes_to_bits(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .flat_map(|byte| (0..8).rev().map(move |bit| (byte >> bit) & 1))
        .collect()
}

fn bits_to_vec(bits: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; bits.len().div_ceil(8)];
    for (i, &bit) in bits.iter().enumerate() {
        out[i / 8] |= (bit & 1) << (7 - i % 8);
    }
    out
}

/// Reassembles a data message from the header that announced it and the
/// blocks that follow.
///
/// A DMR data header on its own tells you a message happened, who to and from,
/// and how many blocks are coming — but not what it said. The payload lives
/// entirely in those blocks, so a decoder that stops at the header shows the
/// envelope and throws away the letter. This keeps the envelope open until
/// the blocks arrive.
#[derive(Debug, Default)]
pub struct DataAssembler {
    pending: Option<Pending>,
}

#[derive(Debug)]
struct Pending {
    header: DataHeader,
    remaining: u8,
    payload: Vec<u8>,
    blocks: usize,
}

impl DataAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.pending = None;
    }

    /// True while blocks are still expected for an open header.
    pub fn is_open(&self) -> bool {
        self.pending.is_some()
    }

    /// Start collecting for a header. Any half-built message is dropped: a new
    /// header means the old one will never be completed.
    pub fn begin(&mut self, header: DataHeader) {
        match header.blocks_to_follow {
            Some(n) if n > 0 => {
                self.pending = Some(Pending {
                    header,
                    remaining: n,
                    payload: Vec::new(),
                    blocks: 0,
                });
            }
            _ => self.pending = None,
        }
    }

    /// Offer a block. Returns the message once the last one has arrived.
    pub fn push(&mut self, block: &DataBlock) -> Option<DataMessage> {
        let pending = self.pending.as_mut()?;
        if pending.header.confirmed && block.block_crc_ok == Some(false) {
            self.pending = None;
            return None;
        }
        // Confirmed messages spend the first two octets of every block on a
        // serial number and CRC; unconfirmed ones use all twelve for payload.
        let payload = if pending.header.confirmed {
            block.payload.get(2..).unwrap_or_default()
        } else {
            block.payload.as_slice()
        };
        pending.payload.extend_from_slice(payload);
        pending.blocks += 1;
        pending.remaining = pending.remaining.saturating_sub(1);
        if pending.remaining > 0 {
            return None;
        }
        let done = self.pending.take()?;
        Some(finish(done))
    }
}

fn finish(p: Pending) -> DataMessage {
    if p.header.udt.is_some() {
        return finish_udt(p);
    }
    let (body, crc_ok) = split_payload(&p.payload);
    let (text, encoding) = decode_short_message(body);
    let application = decode_data_application(body, &p.header);
    DataMessage {
        header: p.header,
        blocks: p.blocks,
        crc_ok,
        text,
        encoding,
        payload: body.to_vec(),
        payload_hex: hex(body),
        application,
    }
}

fn finish_udt(p: Pending) -> DataMessage {
    let split = p.payload.len().saturating_sub(2);
    let (body, crc_bytes) = p.payload.split_at(split);
    let crc_ok = crc_bytes.len() == 2
        && u16::from_be_bytes([crc_bytes[0], crc_bytes[1]]) == crc_ccitt(body) ^ 0xffff;
    let (text, encoding, application) = decode_udt(body, p.header.udt.as_ref());
    DataMessage {
        header: p.header,
        blocks: p.blocks,
        crc_ok,
        text,
        encoding,
        payload: body.to_vec(),
        payload_hex: hex(body),
        application,
    }
}

fn decode_udt(
    body: &[u8],
    header: Option<&UdtHeader>,
) -> (Option<String>, Option<String>, Option<DataApplication>) {
    let Some(header) = header else {
        return (None, None, None);
    };
    let useful_bits = body
        .len()
        .saturating_mul(8)
        .saturating_sub(usize::from(header.pad_nibbles) * 4);
    let bits = bytes_to_bits(body);
    let mut fields = BTreeMap::new();
    fields.insert("format".into(), header.format_name.clone());
    fields.insert("opcode".into(), header.opcode.to_string());
    let mut text = None;
    let mut encoding = None;
    match header.format {
        0x01 => {
            let addresses: Vec<String> = (0..useful_bits / 24)
                .filter_map(|i| bits.get(i * 24..i * 24 + 24))
                .map(|chunk| read_bits_slice(chunk, 0, 24).to_string())
                .collect();
            fields.insert("addresses".into(), addresses.join(", "));
        }
        0x02 => {
            let value: String = (0..useful_bits / 4)
                .map(|i| match read_bits_slice(&bits, i * 4, 4) as u8 {
                    n @ 0..=9 => char::from(b'0' + n),
                    10 => '*',
                    11 => '#',
                    15 => ' ',
                    n => char::from(b'A' + n - 12),
                })
                .collect::<String>()
                .trim()
                .to_string();
            text = (!value.is_empty()).then_some(value);
            encoding = Some("BCD".into());
        }
        0x03 => {
            let value: String = (0..useful_bits / 7)
                .map(|i| read_bits_slice(&bits, i * 7, 7) as u8 as char)
                .collect::<String>()
                .trim_matches(['\0', ' '])
                .to_string();
            text = (!value.is_empty()).then_some(value);
            encoding = Some("ISO-7".into());
        }
        0x04 => {
            let bytes = &body[..useful_bits / 8];
            let value = String::from_utf8_lossy(bytes)
                .trim_matches(['\0', ' '])
                .to_string();
            text = (!value.is_empty()).then_some(value);
            encoding = Some("ISO-8".into());
        }
        0x06 if body.len() >= 4 => {
            fields.insert(
                "address".into(),
                if header.appended_blocks == 1 {
                    format!("{}.{}.{}.{}", body[0], body[1], body[2], body[3])
                } else {
                    body.chunks(2)
                        .take(8)
                        .map(|c| format!("{:02x}{:02x}", c[0], c.get(1).copied().unwrap_or(0)))
                        .collect::<Vec<_>>()
                        .join(":")
                },
            );
        }
        0x07 => {
            let units: Vec<u16> = body[..useful_bits / 8]
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            let value = String::from_utf16_lossy(&units)
                .trim_matches(['\0', ' '])
                .to_string();
            text = (!value.is_empty()).then_some(value);
            encoding = Some("UTF-16BE".into());
        }
        0x0a if body.len() >= 4 => {
            fields.insert(
                "address".into(),
                u32::from_be_bytes([0, body[1], body[2], body[3]]).to_string(),
            );
            let units: Vec<u16> = body[4..useful_bits / 8]
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            let value = String::from_utf16_lossy(&units)
                .trim_matches(['\0', ' '])
                .to_string();
            text = (!value.is_empty()).then_some(value);
            encoding = Some("UTF-16BE".into());
        }
        0x0b => {
            if let Some(lip) = decode_lip_fields(&bits) {
                fields.extend(lip);
            }
        }
        _ => {}
    }
    if let Some(value) = &text {
        fields.insert("text".into(), value.clone());
    }
    let summary = text.clone().unwrap_or_else(|| header.format_name.clone());
    (
        text,
        encoding,
        Some(DataApplication {
            kind: "UDT".into(),
            summary,
            fields,
        }),
    )
}

fn decode_data_application(body: &[u8], header: &DataHeader) -> Option<DataApplication> {
    if header.service_access_point == 4 || body.first().is_some_and(|byte| byte >> 4 == 4) {
        return decode_ipv4_application(body);
    }
    None
}

fn decode_ipv4_application(packet: &[u8]) -> Option<DataApplication> {
    if packet.len() < 20 || packet[0] >> 4 != 4 {
        return None;
    }
    let header_len = usize::from(packet[0] & 0x0f) * 4;
    if header_len < 20 || packet.len() < header_len {
        return None;
    }
    let source = &packet[12..16];
    let target = &packet[16..20];
    let mut fields = BTreeMap::new();
    fields.insert("sourceIp".into(), ipv4(source));
    fields.insert("targetIp".into(), ipv4(target));
    fields.insert("protocol".into(), packet[9].to_string());
    if packet[9] != 17 || packet.len() < header_len + 8 {
        return Some(DataApplication {
            kind: "IPv4".into(),
            summary: format!("{} → {}", ipv4(source), ipv4(target)),
            fields,
        });
    }
    let udp = &packet[header_len..];
    let source_port = u16::from_be_bytes([udp[0], udp[1]]);
    let target_port = u16::from_be_bytes([udp[2], udp[3]]);
    fields.insert("sourcePort".into(), source_port.to_string());
    fields.insert("targetPort".into(), target_port.to_string());
    let payload = &udp[8..];
    let port = if source_port == target_port {
        source_port
    } else {
        target_port
    };
    let kind = match port {
        231 => "Cellocator",
        4001 | 49198 => "LRRP",
        4004 => "XCMP",
        4005 => "ARS",
        4007 => "TMS",
        4008 => "Telemetry",
        4009 => "OTAP",
        4012 => "Battery Management",
        4013 => "Job Ticket Server",
        4069 => "TRBOnet SCADA",
        5007 => "Vertex TMS",
        5016 => "ETSI TMS",
        5017 => "LIP",
        9361 => "P25 Atlas Registration",
        _ => "UDP",
    };
    if matches!(port, 4001 | 49198) {
        fields.extend(decode_lrrp_fields(payload));
    } else if port == 4005 {
        if let Some(id) = printable_text(payload) {
            fields.insert("radioIdentity".into(), id);
        }
    } else if port == 5017 {
        let bits = bytes_to_bits(payload);
        if let Some(lip) = decode_lip_fields(&bits) {
            fields.extend(lip);
        }
    }
    let summary = match (fields.get("latitude"), fields.get("longitude")) {
        (Some(lat), Some(lon)) => format!("{kind} · {lat}, {lon}"),
        _ => format!("{kind} · {} → {}", ipv4(source), ipv4(target)),
    };
    Some(DataApplication {
        kind: kind.into(),
        summary,
        fields,
    })
}

fn decode_lrrp_fields(payload: &[u8]) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let Some(&message_type) = payload.first() else {
        return fields;
    };
    fields.insert("messageType".into(), format!("0x{message_type:02X}"));
    fields.insert(
        "direction".into(),
        if matches!(message_type, 0x05 | 0x09 | 0x0f | 0x14) {
            "request"
        } else if matches!(message_type, 0x07 | 0x0b | 0x0d | 0x11 | 0x15) {
            "response"
        } else {
            "unknown"
        }
        .into(),
    );
    let mut i = if matches!(message_type, 0x07 | 0x0b | 0x0d | 0x11 | 0x15) {
        4
    } else {
        2
    };
    while i < payload.len() {
        match payload[i] {
            0x51 | 0x54 | 0x55 if i + 10 < payload.len() => {
                add_lrrp_position(&mut fields, &payload[i + 1..i + 9]);
                fields.insert(
                    "radiusM".into(),
                    u16::from_be_bytes([payload[i + 9], payload[i + 10]]).to_string(),
                );
                i += 11;
            }
            0x66 if i + 8 < payload.len() => {
                add_lrrp_position(&mut fields, &payload[i + 1..i + 9]);
                i += 9;
            }
            0x69 | 0x6a if i + 9 < payload.len() => {
                add_lrrp_position(&mut fields, &payload[i + 1..i + 9]);
                fields.insert("altitudeM".into(), payload[i + 9].to_string());
                i += 10;
            }
            0x6c | 0x70 if i + 2 < payload.len() => {
                let raw = u16::from_be_bytes([payload[i + 1], payload[i + 2]]);
                fields.insert(
                    "speedKph".into(),
                    format!("{:.2}", f64::from(raw) / 255.0 * 3.6),
                );
                i += 3;
            }
            0x56 if i + 1 < payload.len() => {
                fields.insert(
                    "trackDegrees".into(),
                    (u16::from(payload[i + 1]) * 2).to_string(),
                );
                i += 2;
            }
            _ => i += 1,
        }
    }
    fields
}

fn add_lrrp_position(fields: &mut BTreeMap<String, String>, bytes: &[u8]) {
    let lat = u32::from_be_bytes(bytes[..4].try_into().unwrap());
    let lon = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
    let latitude = if lat & 0x8000_0000 != 0 {
        -f64::from(lat & 0x7fff_ffff) * 180.0 / f64::from(u32::MAX)
    } else {
        f64::from(lat) * 180.0 / f64::from(u32::MAX)
    };
    let longitude = if lon & 0x8000_0000 != 0 {
        -f64::from(0x8000_0000u32.saturating_sub(lon & 0x7fff_ffff)) * 360.0 / f64::from(u32::MAX)
    } else {
        f64::from(lon) * 360.0 / f64::from(u32::MAX)
    };
    if latitude.abs() <= 90.0 && longitude.abs() <= 180.0 {
        fields.insert("latitude".into(), format!("{latitude:.6}"));
        fields.insert("longitude".into(), format!("{longitude:.6}"));
    }
}

fn decode_lip_fields(bits: &[u8]) -> Option<BTreeMap<String, String>> {
    if bits.len() < 81 || read_bits_slice(bits, 0, 4) != 0 {
        return None;
    }
    let mut fields = BTreeMap::new();
    let lon_raw = read_bits_slice(bits, 9, 24);
    let lat_raw = read_bits_slice(bits, 34, 23);
    let latitude = if bits[33] != 0 {
        -f64::from(0x80_0001u32.saturating_sub(lat_raw)) * 180.0 / 2f64.powi(24)
    } else {
        f64::from(lat_raw) * 180.0 / 2f64.powi(24)
    };
    let longitude = if bits[8] != 0 {
        -f64::from(0x100_0001u32.saturating_sub(lon_raw)) * 360.0 / 2f64.powi(25)
    } else {
        f64::from(lon_raw) * 360.0 / 2f64.powi(25)
    };
    fields.insert("latitude".into(), format!("{latitude:.6}"));
    fields.insert("longitude".into(), format!("{longitude:.6}"));
    fields.insert(
        "sourceHash".into(),
        read_bits_slice(bits, 73, 8).to_string(),
    );
    Some(fields)
}

fn printable_text(bytes: &[u8]) -> Option<String> {
    let value: String = bytes
        .iter()
        .copied()
        .filter(|byte| matches!(byte, 0x20..=0x7e))
        .map(char::from)
        .collect();
    (value.len() >= 2).then_some(value)
}

fn ipv4(bytes: &[u8]) -> String {
    format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3])
}

/// Split the reassembled octets into body and trailing CRC-32.
///
/// The last block of a message is zero-padded to a whole block, so the CRC is
/// not simply the last four octets of what arrived. The header does carry a
/// pad-octet count, but its placement differs between the confirmed and
/// unconfirmed layouts and between vendors, and getting it wrong silently
/// corrupts the payload. The CRC itself is a better oracle: try each candidate
/// length and accept the one that checks out. A false match is one in 2^32,
/// which is a far better guarantee than trusting a field we might be reading
/// from the wrong offset.
fn split_payload(payload: &[u8]) -> (&[u8], bool) {
    const MAX_PAD: usize = 12;
    let full = payload.len();
    for pad in 0..=MAX_PAD.min(full.saturating_sub(4)) {
        let end = full - pad;
        let Some(split) = end.checked_sub(4) else {
            continue;
        };
        if split == 0 {
            continue;
        }
        let body = &payload[..split];
        let tail = &payload[split..end];
        let want = u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]);
        if crc32(body) == want {
            return (body, true);
        }
    }
    // Nothing verified. Hand back everything bar the nominal CRC so the hex
    // is still useful, and say plainly that it did not check out.
    (&payload[..full.saturating_sub(4).max(0)], false)
}

/// Recover readable text from a short-message payload.
///
/// DMR text messaging is not one format. Motorola and the ETSI UDT profiles
/// carry UTF-16LE, most commonly behind a four-octet header; plain ASCII shows
/// up from third-party radios. Rather than guess from the SAP — which lies
/// often enough to be useless — both readings are tried and the one that
/// produces sensible printable text wins. If neither does, the caller still
/// has the hex.
fn decode_short_message(body: &[u8]) -> (Option<String>, Option<String>) {
    for skip in [0usize, 4, 6, 8] {
        let Some(rest) = body.get(skip..) else {
            continue;
        };
        if rest.len() < 2 {
            continue;
        }
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        if let Ok(s) = String::from_utf16(&units)
            && is_readable(&s)
        {
            return (Some(trim_text(&s)), Some("UTF-16LE".into()));
        }
        // Big-endian shows up on some ETSI UDT profiles.
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        if let Ok(s) = String::from_utf16(&units)
            && is_readable(&s)
        {
            return (Some(trim_text(&s)), Some("UTF-16BE".into()));
        }
        if let Ok(s) = std::str::from_utf8(rest)
            && is_readable(s)
        {
            return (Some(trim_text(s)), Some("ASCII".into()));
        }
    }
    (None, None)
}

/// Text is only offered when it actually looks like a message.
///
/// "Not a control character" is far too weak a test: arbitrary binary read as
/// UTF-16 produces perfectly valid CJK and private-use characters, so a
/// payload of random octets would be presented as if it were a message. Real
/// DMR short messages are overwhelmingly in the Latin repertoire, so that is
/// what is required — and it is what makes a wrong encoding guess fail
/// loudly instead of quietly.
fn is_readable(s: &str) -> bool {
    let mut good = 0usize;
    let mut total = 0usize;
    for c in s.chars() {
        if c == '\0' {
            continue;
        }
        total += 1;
        let plausible = matches!(c, ' '..='~')
            || matches!(c, '\n' | '\r' | '\t')
            || matches!(c, '\u{a0}'..='\u{ff}');
        if plausible {
            good += 1;
        }
    }
    total >= 2 && good * 10 >= total * 9
}

/// Short messages arrive with a few octets of framing in front — a message
/// number, a reference, sometimes nothing at all — and it decodes to NULs
/// rather than to characters. Trim from both ends so the message itself is
/// what gets shown.
fn trim_text(s: &str) -> String {
    s.trim_matches('\0').trim().to_string()
}

/// CRC-32 as DMR uses it: the standard reflected polynomial, inverted in and
/// out, with the result byte-reversed relative to the usual presentation.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

fn to_bytes(bits: &[u8; 96]) -> [u8; 12] {
    std::array::from_fn(|i| {
        bits[i * 8..i * 8 + 8]
            .iter()
            .fold(0u8, |value, &bit| (value << 1) | bit)
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0;
    while b != 0 {
        if b & 1 != 0 {
            product ^= a;
        }
        let carry = a & 0x80 != 0;
        a <<= 1;
        if carry {
            a ^= 0x1d;
        }
        b >>= 1;
    }
    product
}

fn rs129_parity(data: &[u8]) -> [u8; 3] {
    let mut parity = [0u8; 3];
    for &byte in data {
        let feedback = byte ^ parity[0];
        parity[0] = parity[1] ^ gf_mul(0x0e, feedback);
        parity[1] = parity[2] ^ gf_mul(0x38, feedback);
        parity[2] = gf_mul(0x40, feedback);
    }
    parity
}

fn manufacturer(fid: u8) -> &'static str {
    match fid {
        0x00 => "DMR Association",
        0x06 | 0x10 => "Motorola",
        0x08 | 0x68 => "Hytera",
        0x0a => "Kirisun",
        0x58 => "Tait",
        _ => "Unknown",
    }
}

fn link_control_name(opcode: u8, fid: u8) -> &'static str {
    match (opcode, fid) {
        (0x00, _) => "group voice channel user",
        (0x03, _) => "unit-to-unit voice channel user",
        (0x04, 0x10) => "Capacity Plus group voice",
        (0x07, 0x10) => "Capacity Plus private voice",
        (0x08, 0x00 | 0x68) => "embedded GPS",
        (0x09, 0x68) => "Hytera XPT call alert",
        (0x30, _) => "data terminator",
        _ => "unknown link control",
    }
}

fn privacy_algorithm(algorithm: u8, fid: u8) -> &'static str {
    match (algorithm & 0x07, fid) {
        (1, 0x10) => "RC4",
        (2, 0x10) => "DES",
        (4, 0x10) => "AES-128",
        (5, 0x10) => "AES-256",
        (_, 0x68) => "Hytera Enhanced Privacy",
        (_, 0x0a) => "Kirisun Privacy",
        _ => "vendor privacy",
    }
}

fn data_format_name(format: u8) -> &'static str {
    match format {
        0 => "unified data transport",
        1 => "response",
        2 => "unconfirmed delivery",
        3 => "confirmed delivery",
        13 => "defined short data",
        14 => "raw/status short data",
        15 => "proprietary",
        _ => "reserved",
    }
}

fn udt_format_name(format: u8) -> &'static str {
    match format {
        0x00 => "binary",
        0x01 => "appended addresses",
        0x02 => "BCD",
        0x03 => "ISO-7 text",
        0x04 => "ISO-8 text",
        0x05 => "NMEA location",
        0x06 => "IP address",
        0x07 => "UTF-16BE text",
        0x08 | 0x09 => "manufacturer specific",
        0x0a => "mixed address/UTF-16BE",
        0x0b => "LIP location",
        _ => "reserved",
    }
}

fn service_name(sap: u8) -> &'static str {
    match sap {
        0 => "UDT data",
        2 => "compressed TCP/IP",
        3 => "compressed UDP/IP",
        4 => "IP packet data",
        5 => "ARP",
        9 => "proprietary extension",
        10 => "short data",
        _ => "reserved",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dmr::framer::{Burst, BurstKind, SyncSource};

    #[test]
    fn unified_single_block_lip_decodes_location() {
        let mut bits = [0u8; 96];
        let put = |bits: &mut [u8], start: usize, width: usize, value: u32| {
            for i in 0..width {
                bits[start + i] = ((value >> (width - 1 - i)) & 1) as u8;
            }
        };
        // Service 0 (LIP), 45 degrees north and 90 degrees east.
        put(&mut bits, 9, 24, 0x80_0000);
        put(&mut bits, 34, 23, 0x40_0000);
        put(&mut bits, 57, 2, 1);
        put(&mut bits, 59, 7, 25);
        put(&mut bits, 66, 4, 4);
        put(&mut bits, 73, 8, 0x5a);
        let mut bytes = to_bytes(&bits);
        let crc = crc_ccitt(&bytes[..10]) ^ 0xcccc;
        bytes[10..].copy_from_slice(&crc.to_be_bytes());
        for (i, byte) in bytes.iter().copied().enumerate() {
            put(&mut bits, i * 8, 8, u32::from(byte));
        }
        let decoded = decode_unified_single_block(bits, bytes, 2, hex(&bytes)).unwrap();
        assert_eq!(decoded.service_type, 0);
        assert!((decoded.latitude.unwrap() - 45.0).abs() < 0.0001);
        assert!((decoded.longitude.unwrap() - 90.0).abs() < 0.0001);
        assert_eq!(decoded.position_error_m, Some(20));
        assert_eq!(decoded.speed_kph, Some(25.0));
        assert_eq!(decoded.direction_degrees, Some(90.0));
        assert_eq!(decoded.source_hash, Some(0x5a));
    }

    /// A header on its own says a message happened; the blocks say what it
    /// was. Reassembly has to survive the confirmed/unconfirmed split, the
    /// trailing CRC-32, and the fact that DMR text is not one encoding.
    #[test]
    fn blocks_reassemble_into_a_readable_short_message() {
        for (confirmed, label) in [(false, "unconfirmed"), (true, "confirmed")] {
            let text = "MEET AT THE NORTH GATE";
            let mut body: Vec<u8> = vec![0x00, 0x00, 0x00, 0x00];
            for unit in text.encode_utf16() {
                body.extend_from_slice(&unit.to_le_bytes());
            }
            let mut payload = body.clone();
            payload.extend_from_slice(&crc32(&body).to_be_bytes());

            let per_block = if confirmed { 10 } else { 12 };
            let blocks: Vec<Vec<u8>> = payload
                .chunks(per_block)
                .map(|c| {
                    let mut v = c.to_vec();
                    v.resize(per_block, 0);
                    v
                })
                .collect();

            let header = DataHeader {
                group: false,
                response_requested: confirmed,
                format: if confirmed { 3 } else { 2 },
                format_name: "test".into(),
                service_access_point: 4,
                service_name: "test".into(),
                target_id: 2001,
                source_id: 1001,
                blocks_to_follow: Some(blocks.len() as u8),
                confirmed,
                udt: None,
                corrected_bits: 0,
                raw: String::new(),
            };

            let mut asm = DataAssembler::new();
            asm.begin(header);
            assert!(asm.is_open());
            let mut done = None;
            for (i, chunk) in blocks.iter().enumerate() {
                let mut octets = Vec::new();
                if confirmed {
                    octets.push((i as u8) << 1);
                    octets.push(0);
                }
                octets.extend_from_slice(chunk);
                done = asm.push(&DataBlock {
                    payload: octets,
                    serial: confirmed.then_some(i as u8),
                    block_crc_ok: None,
                    rate: "1/2".into(),
                    corrected_bits: 0,
                    raw: String::new(),
                });
            }
            let msg = done.unwrap_or_else(|| panic!("{label}: never completed"));
            assert!(
                msg.crc_ok,
                "{label}: CRC-32 over the reassembled payload failed"
            );
            assert_eq!(msg.blocks, blocks.len(), "{label}");
            assert_eq!(msg.text.as_deref(), Some(text), "{label}");
            assert_eq!(msg.encoding.as_deref(), Some("UTF-16LE"), "{label}");
            assert!(!asm.is_open(), "{label}: assembler should have closed");
        }
    }

    /// Binary that happens to parse as text must not be presented as text.
    #[test]
    fn a_binary_payload_is_offered_as_hex_not_as_text() {
        let body: Vec<u8> = (0..24u8).map(|i| i.wrapping_mul(37)).collect();
        let (text, encoding) = decode_short_message(&body);
        assert!(
            text.is_none() && encoding.is_none(),
            "binary was read as {text:?} / {encoding:?}"
        );
    }

    /// A new header abandons whatever was half-collected — the old message is
    /// never going to be completed, and mixing them would invent a payload.
    #[test]
    fn a_new_header_drops_a_half_built_message() {
        let header = |blocks| DataHeader {
            group: false,
            response_requested: false,
            format: 2,
            format_name: "test".into(),
            service_access_point: 4,
            service_name: "test".into(),
            target_id: 1,
            source_id: 2,
            blocks_to_follow: Some(blocks),
            confirmed: false,
            udt: None,
            corrected_bits: 0,
            raw: String::new(),
        };
        let mut asm = DataAssembler::new();
        asm.begin(header(4));
        let block = DataBlock {
            payload: vec![0xAA; 12],
            serial: None,
            block_crc_ok: None,
            rate: "1/2".into(),
            corrected_bits: 0,
            raw: String::new(),
        };
        assert!(asm.push(&block).is_none());
        asm.begin(header(1));
        let msg = asm
            .push(&block)
            .expect("second message completes on its own");
        assert_eq!(
            msg.blocks, 1,
            "the abandoned block leaked into the new message"
        );
    }

    #[test]
    fn multi_block_udt_uses_crc16_and_declared_text_format() {
        let header = DataHeader {
            group: false,
            response_requested: false,
            format: 0,
            format_name: "unified data transport".into(),
            service_access_point: 0,
            service_name: "UDT data".into(),
            target_id: 2001,
            source_id: 1001,
            blocks_to_follow: Some(1),
            confirmed: false,
            udt: Some(UdtHeader {
                format: 4,
                format_name: "ISO-8 text".into(),
                pad_nibbles: 10,
                appended_blocks: 1,
                supplementary: false,
                protected: false,
                opcode: 0,
            }),
            corrected_bits: 0,
            raw: String::new(),
        };
        let mut payload = [0u8; 12];
        payload[..5].copy_from_slice(b"HELLO");
        let crc = crc_ccitt(&payload[..10]) ^ 0xffff;
        payload[10..].copy_from_slice(&crc.to_be_bytes());
        let mut assembler = DataAssembler::new();
        assembler.begin(header);
        let message = assembler
            .push(&DataBlock {
                payload: payload.to_vec(),
                serial: None,
                block_crc_ok: None,
                rate: "1/2".into(),
                corrected_bits: 0,
                raw: hex(&payload),
            })
            .expect("UDT should finish after its declared appended block");
        assert!(message.crc_ok);
        assert_eq!(message.text.as_deref(), Some("HELLO"));
        assert_eq!(message.encoding.as_deref(), Some("ISO-8"));
        assert_eq!(message.application.unwrap().kind, "UDT");
    }

    #[test]
    fn ipv4_udp_lrrp_exposes_position_speed_and_track() {
        let mut packet = vec![0u8; 28];
        packet[0] = 0x45;
        packet[9] = 17;
        packet[12..16].copy_from_slice(&[10, 0, 0, 1]);
        packet[16..20].copy_from_slice(&[10, 0, 0, 2]);
        packet[20..22].copy_from_slice(&4001u16.to_be_bytes());
        packet[22..24].copy_from_slice(&4001u16.to_be_bytes());
        packet.extend_from_slice(&[
            0x0d, 15, 0x22, 0x00, 0x66, 0x40, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x6c, 0x00,
            0xff, 0x56, 45,
        ]);
        let app = decode_ipv4_application(&packet).expect("IPv4/UDP application");
        assert_eq!(app.kind, "LRRP");
        assert_eq!(
            app.fields.get("latitude").map(String::as_str),
            Some("45.000000")
        );
        assert_eq!(
            app.fields.get("longitude").map(String::as_str),
            Some("90.000000")
        );
        assert_eq!(app.fields.get("speedKph").map(String::as_str), Some("3.60"));
        assert_eq!(
            app.fields.get("trackDegrees").map(String::as_str),
            Some("90")
        );
    }

    #[test]
    fn rs_parity_and_link_control_fields_are_checked() {
        let mut bytes = [0u8; 12];
        bytes[..9].copy_from_slice(&[0x00, 0x00, 0xc3, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        let parity = rs129_parity(&bytes[..9]);
        bytes[9] = parity[0] ^ 0x96;
        bytes[10] = parity[1] ^ 0x96;
        bytes[11] = parity[2] ^ 0x96;
        let lc = decode_link_control(bytes, 0, hex(&bytes), 0x96_96_96).unwrap();
        assert_eq!(lc.target_id, 0x010203);
        assert_eq!(lc.source_id, 0x040506);
        assert!(lc.emergency);
        assert!(lc.encrypted);
        assert_eq!(lc.priority, 3);
    }

    #[test]
    fn crc_masked_data_header_fields_are_checked() {
        let mut bytes = [0u8; 12];
        bytes[..10].copy_from_slice(&[0x23, 0xa0, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x85, 0x00]);
        let crc = crc_ccitt(&bytes[..10]) ^ 0x3333;
        bytes[10..].copy_from_slice(&crc.to_be_bytes());
        let bits = std::array::from_fn(|i| (bytes[i / 8] >> (7 - i % 8)) & 1);
        let header = decode_data_header(bits, bytes, 0, hex(&bytes)).unwrap();
        assert_eq!(header.format, 3);
        assert_eq!(header.service_access_point, 10);
        assert_eq!(header.target_id, 0x010203);
        assert_eq!(header.source_id, 0x040506);
        assert!(header.confirmed);
    }

    #[test]
    fn rate_three_quarter_trellis_recovers_a_clean_block() {
        // Independent encoder for the ETSI 8-state constellation. This locks
        // down both transition direction and the unusual 98-dibit interleave;
        // either being reversed can still look plausible on random traffic.
        const INTERLEAVE: [usize; 98] = [
            0, 1, 8, 9, 16, 17, 24, 25, 32, 33, 40, 41, 48, 49, 56, 57, 64, 65, 72, 73, 80, 81, 88,
            89, 96, 97, 2, 3, 10, 11, 18, 19, 26, 27, 34, 35, 42, 43, 50, 51, 58, 59, 66, 67, 74,
            75, 82, 83, 90, 91, 4, 5, 12, 13, 20, 21, 28, 29, 36, 37, 44, 45, 52, 53, 60, 61, 68,
            69, 76, 77, 84, 85, 92, 93, 6, 7, 14, 15, 22, 23, 30, 31, 38, 39, 46, 47, 54, 55, 62,
            63, 70, 71, 78, 79, 86, 87, 94, 95,
        ];
        const X: [[i8; 8]; 8] = [
            [1, -3, -3, 1, 3, -1, -1, 3],
            [-3, 1, 3, -1, -1, 3, 1, -3],
            [-1, 3, 3, -1, -3, 1, 1, -3],
            [3, -1, -3, 1, 1, -3, -1, 3],
            [-3, 1, 1, -3, -1, 3, 3, -1],
            [1, -3, -1, 3, 3, -1, -3, 1],
            [3, -1, -1, 3, 1, -3, -3, 1],
            [-1, 3, 1, -3, -3, 1, 3, -1],
        ];
        const Y: [[i8; 8]; 8] = [
            [-1, 3, -1, 3, -3, 1, -3, 1],
            [-1, 3, -3, 1, -3, 1, -1, 3],
            [-1, 3, -1, 3, -3, 1, -3, 1],
            [-1, 3, -3, 1, -3, 1, -1, 3],
            [-3, 1, -3, 1, -1, 3, -1, 3],
            [-3, 1, -1, 3, -1, 3, -3, 1],
            [-3, 1, -3, 1, -1, 3, -1, 3],
            [-3, 1, -1, 3, -1, 3, -3, 1],
        ];
        fn dibit(value: i8) -> u8 {
            match value {
                1 => 0,
                3 => 1,
                -1 => 2,
                -3 => 3,
                _ => unreachable!(),
            }
        }

        let wanted: [u8; 18] = std::array::from_fn(|i| (i as u8).wrapping_mul(37) ^ 0xa5);
        let bits = bytes_to_bits(&wanted);
        let mut state = 0usize;
        let mut deinterleaved = [0u8; 98];
        for step in 0..48 {
            let input =
                ((bits[step * 3] << 2) | (bits[step * 3 + 1] << 1) | bits[step * 3 + 2]) as usize;
            deinterleaved[step * 2] = dibit(X[state][input]);
            deinterleaved[step * 2 + 1] = dibit(Y[state][input]);
            state = input;
        }
        deinterleaved[96] = dibit(X[state][0]);
        deinterleaved[97] = dibit(Y[state][0]);
        let air = std::array::from_fn(|i| deinterleaved[INTERLEAVE[i]]);
        assert_eq!(trellis_three_quarter(&air), Some(wanted));
    }

    #[test]
    fn full_rate_uses_all_192_payload_bits_around_the_slot_type() {
        let wanted: [u8; 24] = std::array::from_fn(|i| (i as u8).wrapping_mul(19) ^ 0x6d);
        let wanted_bits = bytes_to_bits(&wanted);
        let mut bits = [0u8; 196];
        bits[..96].copy_from_slice(&wanted_bits[..96]);
        bits[100..].copy_from_slice(&wanted_bits[96..]);
        let mut dibits = [0u8; crate::dmr::BURST_DIBITS];
        for i in 0..49 {
            dibits[i] = (bits[i * 2] << 1) | bits[i * 2 + 1];
        }
        for i in 49..98 {
            dibits[i + 34] = (bits[i * 2] << 1) | bits[i * 2 + 1];
        }
        let burst = Burst {
            slot: Some(2),
            source: SyncSource::Bs,
            kind: BurstKind::Data {
                slot_type: None,
                sync: None,
            },
            dibits,
        };
        assert_eq!(decode_full_rate_block(&burst).payload, wanted);
    }

    #[test]
    fn embedded_bptc_link_control_recovers_source_and_target() {
        const H: [[u8; 16]; 5] = [
            [1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0, 0],
            [0, 1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0],
            [0, 0, 1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0],
            [1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0, 1, 0],
            [1, 0, 1, 0, 0, 1, 1, 0, 1, 1, 1, 0, 0, 0, 0, 1],
        ];
        let bytes = [0x00, 0x00, 0x83, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let mut payload = bytes_to_bits(&bytes);
        let crc = embedded_crc5(&bytes);
        payload.extend((0..5).rev().map(|bit| (crc >> bit) & 1));
        let mut matrix = [[0u8; 16]; 8];
        let mut at = 0;
        for row in 0..2 {
            matrix[row][..11].copy_from_slice(&payload[at..at + 11]);
            at += 11;
        }
        for row in 2..7 {
            matrix[row][..10].copy_from_slice(&payload[at..at + 10]);
            at += 10;
        }
        for row in 2..7 {
            matrix[row][10] = payload[at];
            at += 1;
        }
        for row in matrix.iter_mut().take(7) {
            for parity in 0..5 {
                row[11 + parity] = row[..11]
                    .iter()
                    .zip(&H[parity][..11])
                    .fold(0, |value, (&bit, &check)| value ^ (bit & check));
            }
        }
        for column in 0..16 {
            matrix[7][column] = matrix
                .iter()
                .take(7)
                .map(|row| row[column])
                .fold(0, |a, b| a ^ b);
        }
        let coded = std::array::from_fn(|i| matrix[i % 8][i / 8]);
        let lc = decode_embedded_lc(&coded).expect("valid embedded LC");
        assert_eq!(lc.target_id, 0x010203);
        assert_eq!(lc.source_id, 0x040506);
        assert!(lc.emergency);
        assert_eq!(lc.priority, 3);
    }

    #[test]
    fn embedded_talker_alias_assembles_across_link_control_blocks() {
        let header = [0x04, 0x00, 0x56, b'H', b'E', b'L', b'L', b'O', b' '];
        let block = [0x05, 0x00, b'W', b'O', b'R', b'L', b'D', 0, 0];
        let mut assembler = EmbeddedLcAssembler::new();
        let mut first = link_control_fields(&header, 0, hex(&header));
        assembler.observe_alias(&mut first);
        assert_eq!(first.talker_alias.as_deref(), Some("HELLO"));
        let mut second = link_control_fields(&block, 0, hex(&block));
        assembler.observe_alias(&mut second);
        assert_eq!(second.talker_alias.as_deref(), Some("HELLO WORLD"));
    }
}
