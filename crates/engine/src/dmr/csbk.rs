//! FEC and control-message decoding for DMR CSBK data bursts.
//!
//! Implements the ETSI BPTC(196,96) product code and the masked CSBK
//! CRC-CCITT. Only messages that pass both layers are exposed as decoded
//! Tier III telemetry.

use super::framer::Burst;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Csbk {
    pub last_block: bool,
    pub protected: bool,
    pub opcode: u8,
    pub opcode_name: String,
    pub feature_id: u8,
    pub corrected_bits: u32,
    pub raw: String,
    pub lcn: Option<u16>,
    pub timeslot: Option<u8>,
    pub target_id: Option<u32>,
    pub source_id: Option<u32>,
    pub system_code: Option<u16>,
    pub network_model: Option<String>,
    pub network_id: Option<u16>,
    pub site_id: Option<u16>,
    pub category: Option<String>,
    pub registration_required: Option<bool>,
    pub backoff: Option<u8>,
    pub announcement_type: Option<u8>,
    pub announcement_name: Option<String>,
    pub announced_channels: Vec<u16>,
    pub announced_color_codes: Vec<u8>,
    pub multi_block: bool,
    pub absolute_tx_hz: Option<u64>,
    pub absolute_rx_hz: Option<u64>,
    pub system: Option<String>,
    pub call_type: Option<String>,
    pub rest_lsn: Option<u8>,
    pub adjacent_sites: Vec<u8>,
    /// Active logical slots reconstructed from vendor channel-status CSBKs.
    /// Some vendor formats do not distinguish voice from packet data, so the
    /// activity type intentionally preserves that uncertainty.
    #[serde(default)]
    pub active_slots: Vec<DmrActiveSlot>,
    pub vendor_fields: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DmrActiveSlot {
    pub lsn: u8,
    pub lcn: u16,
    pub timeslot: u8,
    pub target_id: u32,
    pub activity_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CsbkError {
    Fec,
    Crc { raw: String },
}

/// Per-timeslot assembler for standard CSBK and multi-block control (MBC).
/// MBC is how Tier III sends absolute channel definitions when an LCN map is
/// unavailable, so discarding its continuation used to make valid grants
/// permanently unresolvable.
#[derive(Default)]
pub struct ControlDecoder {
    pending: [Option<PendingMbc>; 3],
    capacity_plus: [Option<Vec<u8>>; 3],
}

struct PendingMbc {
    csbk: Csbk,
    continuation: Vec<u8>,
    corrected_bits: u32,
}

impl ControlDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.pending = [None, None, None];
        self.capacity_plus = [None, None, None];
    }

    /// `None` means this was not control signalling, or an MBC is waiting for
    /// its final continuation. A completed CSBK/MBC always carries verified
    /// BPTC and CRC before it is returned.
    pub fn process(&mut self, burst: &Burst) -> Option<Result<Csbk, CsbkError>> {
        let data_type = match &burst.kind {
            super::framer::BurstKind::Data {
                slot_type: Some(slot_type),
                ..
            } => slot_type.data_type,
            _ => return None,
        };
        let slot = usize::from(burst.slot.filter(|slot| matches!(slot, 1 | 2)).unwrap_or(0));
        match data_type {
            3 => {
                let mut csbk = match decode_csbk(burst) {
                    Ok(csbk) => csbk,
                    Err(error) => return Some(Err(error)),
                };
                if csbk.feature_id == 0x10 && csbk.opcode == 0x3e {
                    let (bytes, _) = match bptc_bytes(burst) {
                        Ok(decoded) => decoded,
                        Err(error) => return Some(Err(error)),
                    };
                    let bits = bytes_to_bits(&bytes[..10]);
                    let fragment = read_bits(&bits, 16, 2) as u8;
                    match fragment {
                        2 => {
                            self.capacity_plus[slot] = Some(bits);
                        }
                        0 | 1 => {
                            if let Some(assembled) = self.capacity_plus[slot].as_mut() {
                                assembled.extend_from_slice(&bits[24..80]);
                                if fragment == 1 {
                                    let assembled = self.capacity_plus[slot].take().unwrap();
                                    csbk.active_slots = decode_capacity_plus_activity(&assembled);
                                    csbk.vendor_fields.insert(
                                        "activeSlots".into(),
                                        csbk.active_slots.len().to_string(),
                                    );
                                    csbk.vendor_fields.insert(
                                        "assembledBits".into(),
                                        assembled.len().to_string(),
                                    );
                                }
                            }
                        }
                        3 => {}
                        _ => unreachable!(),
                    }
                }
                Some(Ok(csbk))
            }
            4 => {
                let csbk = match decode_csbk_masked(burst, 0x5555) {
                    Ok(csbk) => csbk,
                    Err(error) => return Some(Err(error)),
                };
                self.pending[slot] = Some(PendingMbc {
                    corrected_bits: csbk.corrected_bits,
                    csbk,
                    continuation: Vec::new(),
                });
                None
            }
            5 => {
                let (bytes, corrected_bits) = match bptc_bytes(burst) {
                    Ok(decoded) => decoded,
                    Err(error) => return Some(Err(error)),
                };
                let Some(pending) = self.pending[slot].as_mut() else {
                    return None;
                };
                pending.corrected_bits += corrected_bits;
                pending.continuation.extend_from_slice(&bytes);
                if bytes[0] & 0x80 == 0 {
                    return None;
                }
                let pending = self.pending[slot].take().expect("pending MBC");
                Some(finish_mbc(pending))
            }
            _ => None,
        }
    }
}

fn bptc_bytes(burst: &Burst) -> Result<([u8; 12], u32), CsbkError> {
    let channel = burst.bptc_payload_bits();
    let (bits, corrected_bits) = decode_bptc(&channel).ok_or(CsbkError::Fec)?;
    let mut bytes = [0u8; 12];
    for (i, &bit) in bits.iter().enumerate() {
        bytes[i / 8] |= bit << (7 - i % 8);
    }
    Ok((bytes, corrected_bits))
}

fn finish_mbc(mut pending: PendingMbc) -> Result<Csbk, CsbkError> {
    let raw = pending
        .continuation
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    if pending.continuation.len() < 12 {
        return Err(CsbkError::Crc { raw });
    }
    let payload_len = pending.continuation.len() - 2;
    let stored = u16::from_be_bytes([
        pending.continuation[payload_len],
        pending.continuation[payload_len + 1],
    ]);
    if stored != crc_ccitt(&pending.continuation[..payload_len]) ^ 0xffff {
        return Err(CsbkError::Crc { raw });
    }

    pending.csbk.multi_block = true;
    pending.csbk.corrected_bits = pending.corrected_bits;
    pending.csbk.raw.push_str(&raw);
    let bits: Vec<u8> = pending
        .continuation
        .iter()
        .flat_map(|byte| (0..8).rev().map(move |bit| (byte >> bit) & 1))
        .collect();
    // Absolute Channel Parameters (CDEF type 0), ETSI TS 102 361-4
    // 7.2.19.7.1. Frequencies are integer MHz plus 125-Hz steps. These do not
    // only accompany grants whose header LPCN is 0xFFF: C_BCAST type 5 uses
    // the same continuation to announce a logical-channel/frequency
    // relationship. Restricting this to absolute grants discarded the exact
    // mappings that a Tier III control channel normally broadcasts.
    let absolute_grant = pending.csbk.lcn == Some(0x0fff);
    let absolute_announcement =
        pending.csbk.opcode == 0x28 && matches!(pending.csbk.announcement_type, Some(2 | 5 | 6));
    if (absolute_grant || absolute_announcement) && bits.len() >= 80 && read_bits(&bits, 16, 4) == 0
    {
        let defined_lcn = read_bits(&bits, 22, 12) as u16;
        // A type-5 header may carry the logical channel being related while
        // the continuation repeats it as its physical-channel number. Prefer
        // the explicit header relationship when present, but fall back to the
        // continuation for 0xFFF absolute grants and adjacent-site messages.
        let lcn = pending
            .csbk
            .announced_channels
            .iter()
            .copied()
            .find(|channel| !matches!(*channel, 0 | 0x0fff))
            .unwrap_or(defined_lcn);
        let tx_hz = u64::from(read_bits(&bits, 34, 10)) * 1_000_000
            + u64::from(read_bits(&bits, 44, 13)) * 125;
        let rx_hz = u64::from(read_bits(&bits, 57, 10)) * 1_000_000
            + u64::from(read_bits(&bits, 67, 13)) * 125;
        pending.csbk.lcn = Some(lcn);
        pending.csbk.absolute_tx_hz = (tx_hz != 0).then_some(tx_hz);
        pending.csbk.absolute_rx_hz = (rx_hz != 0).then_some(rx_hz);
        if !pending.csbk.announced_channels.contains(&lcn) {
            pending.csbk.announced_channels.push(lcn);
        }
        pending
            .csbk
            .announced_color_codes
            .push(read_bits(&bits, 12, 4) as u8);
        pending
            .csbk
            .vendor_fields
            .insert("absoluteTxHz".into(), tx_hz.to_string());
        pending
            .csbk
            .vendor_fields
            .insert("absoluteRxHz".into(), rx_hz.to_string());
    }
    Ok(pending.csbk)
}

pub fn decode_csbk(burst: &Burst) -> Result<Csbk, CsbkError> {
    decode_csbk_masked(burst, 0x5a5a)
}

fn decode_csbk_masked(burst: &Burst, crc_mask: u16) -> Result<Csbk, CsbkError> {
    let channel = burst.bptc_payload_bits();
    let (bits, corrected_bits) = decode_bptc(&channel).ok_or(CsbkError::Fec)?;
    let mut bytes = [0u8; 12];
    for (i, &bit) in bits.iter().enumerate() {
        bytes[i / 8] |= bit << (7 - i % 8);
    }
    let raw = bytes.iter().map(|b| format!("{b:02X}")).collect::<String>();
    let stored = u16::from_be_bytes([bytes[10], bytes[11]]);
    if stored != crc_ccitt(&bytes[..10]) ^ crc_mask {
        return Err(CsbkError::Crc { raw });
    }
    let opcode = bytes[0] & 0x3f;
    let feature_id = bytes[1];
    let grant = (0x30..=0x38).contains(&opcode);
    let mut lcn = grant.then(|| read_bits(&bits, 16, 12) as u16);
    let mut timeslot = grant.then(|| read_bits(&bits, 28, 1) as u8 + 1);
    let mut target_id = grant.then(|| read_bits(&bits, 32, 24));
    let mut source_id = grant.then(|| read_bits(&bits, 56, 24));
    let mut system = (grant
        || matches!(
            opcode,
            0x19 | 0x1c | 0x1e..=0x21 | 0x26 | 0x28 | 0x2a | 0x2e | 0x2f | 0x39
        ))
    .then(|| "DMR Tier III".to_string());
    let mut call_type = grant.then(|| tier_three_call_type(opcode).to_string());
    let mut rest_lsn = None;
    let mut adjacent_sites = Vec::new();
    let mut vendor_fields = BTreeMap::new();
    let mut system_code = matches!(opcode, 0x19 | 0x28).then(|| read_bits(&bits, 40, 14) as u16);
    let mut network_model = None;
    let mut network_id = None;
    let mut site_id = None;
    let mut category = None;
    let mut registration_required = None;
    let mut backoff = None;
    let mut announcement_type = None;
    let mut announcement_name = None;
    let mut announced_channels = Vec::new();
    let mut announced_color_codes = Vec::new();
    let mut active_slots = Vec::new();

    if opcode == 0x19 {
        call_type = Some("system parameters".into());
        registration_required = Some(bits[35] != 0);
        backoff = Some(read_bits(&bits, 36, 4) as u8);
        category = Some(
            match read_bits(&bits, 54, 2) {
                1 => "A",
                2 => "B",
                3 => "AB",
                _ => "reserved",
            }
            .into(),
        );
        if let Some(code) = system_code {
            let identity = decode_system_code(code);
            network_model = Some(identity.0.into());
            network_id = Some(identity.1);
            site_id = Some(identity.2);
            vendor_fields.insert("rawSystemCode".into(), format!("0x{code:04X}"));
        }
        vendor_fields.insert(
            "documentVersion".into(),
            read_bits(&bits, 19, 3).to_string(),
        );
        vendor_fields.insert("activeConnection".into(), (bits[23] != 0).to_string());
        vendor_fields.insert(
            "serviceFunction".into(),
            read_bits(&bits, 29, 2).to_string(),
        );
    } else if opcode == 0x28 {
        let kind = read_bits(&bits, 16, 5) as u8;
        announcement_type = Some(kind);
        announcement_name = Some(announcement_type_name(kind).into());
        call_type = announcement_name.clone();
        registration_required = Some(bits[35] != 0);
        backoff = Some(read_bits(&bits, 36, 4) as u8);
        match kind {
            0 => {
                announced_color_codes.push(read_bits(&bits, 25, 4) as u8);
                announced_color_codes.push(read_bits(&bits, 29, 4) as u8);
                announced_channels.push(read_bits(&bits, 56, 12) as u16);
                announced_channels.push(read_bits(&bits, 68, 12) as u16);
                vendor_fields.insert("channel1Withdraw".into(), (bits[33] != 0).to_string());
                vendor_fields.insert("channel2Withdraw".into(), (bits[34] != 0).to_string());
            }
            2 | 6 => {
                // Vote-now and adjacent-site announcements carry the other
                // site's system code in BPARMS1, not the common SYS field.
                let adjacent_code = read_bits(&bits, 21, 14) as u16;
                let identity = decode_system_code(adjacent_code);
                network_model = Some(identity.0.into());
                network_id = Some(identity.1);
                site_id = Some(identity.2);
                system_code = Some(adjacent_code);
                announced_channels.push(read_bits(&bits, 68, 12) as u16);
                vendor_fields.insert("online".into(), (bits[57] != 0).to_string());
                vendor_fields.insert(
                    "confirmedPriority".into(),
                    read_bits(&bits, 58, 3).to_string(),
                );
                vendor_fields.insert("activePriority".into(), read_bits(&bits, 61, 3).to_string());
            }
            5 => announced_channels.push(read_bits(&bits, 68, 12) as u16),
            7 => {
                vendor_fields.insert("hibernate".into(), (bits[57] != 0).to_string());
                vendor_fields.insert("talkgroupRegistration".into(), (bits[72] != 0).to_string());
            }
            _ => {}
        }
        announced_channels.retain(|&channel| channel != 0 && channel != 0x0fff);
    }

    if matches!(
        opcode,
        0x04 | 0x05 | 0x1c | 0x1e..=0x23 | 0x26 | 0x2a | 0x2e | 0x2f | 0x3d
    ) {
        target_id = Some(read_bits(&bits, 32, 24));
        source_id = Some(read_bits(&bits, 56, 24));
    }
    match opcode {
        0x04 => call_type = Some("unit-to-unit voice request".into()),
        0x05 => call_type = Some("unit-to-unit voice answer".into()),
        0x1c => {
            let service_kind = read_bits(&bits, 28, 4) as u8;
            call_type = Some(format!("{} request", tier_three_service_name(service_kind)));
            vendor_fields.insert(
                "serviceOptions".into(),
                format!("0x{:02X}", read_bits(&bits, 16, 7)),
            );
            vendor_fields.insert("serviceKind".into(), service_kind.to_string());
            vendor_fields.insert("group".into(), (bits[25] != 0).to_string());
            vendor_fields.insert("ambientListening".into(), (bits[24] != 0).to_string());
            vendor_fields.insert("blocksToFollow".into(), read_bits(&bits, 26, 2).to_string());
        }
        0x20..=0x23 => {
            call_type = Some(
                match opcode {
                    0x20 => "outbound control acknowledgement",
                    0x21 => "inbound control acknowledgement",
                    0x22 => "outbound payload acknowledgement",
                    _ => "inbound payload acknowledgement",
                }
                .into(),
            );
            vendor_fields.insert(
                "responseInfo".into(),
                format!("0x{:02X}", read_bits(&bits, 16, 7)),
            );
            vendor_fields.insert(
                "reasonCode".into(),
                format!("0x{:02X}", read_bits(&bits, 23, 8)),
            );
        }
        0x26 => call_type = Some("negative acknowledgement".into()),
        0x2a => {
            call_type = Some(
                if read_bits(&bits, 28, 3) == 0 {
                    "disconnect"
                } else {
                    "payload maintenance"
                }
                .into(),
            );
            vendor_fields.insert(
                "maintenanceKind".into(),
                read_bits(&bits, 28, 3).to_string(),
            );
        }
        0x2e => call_type = Some("clear traffic channel".into()),
        0x2f => {
            let kind = read_bits(&bits, 28, 3) as u8;
            call_type = Some(
                match kind {
                    0 => "disable target PTT",
                    1 => "enable target PTT",
                    2 => "call hangtime",
                    3 => "enable one mobile PTT",
                    _ => "protect",
                }
                .into(),
            );
            vendor_fields.insert("group".into(), (bits[31] != 0).to_string());
        }
        0x3d => {
            call_type = Some("preamble".into());
            vendor_fields.insert(
                "content".into(),
                if bits[16] == 0 { "CSBK" } else { "data" }.into(),
            );
            vendor_fields.insert("group".into(), (bits[17] != 0).to_string());
            vendor_fields.insert("blocks".into(), read_bits(&bits, 24, 8).to_string());
        }
        _ => {}
    }

    if feature_id == 0x10
        && matches!(opcode, 0x19 | 0x1c | 0x1e..=0x39)
        && !matches!(opcode, 0x3a | 0x3b | 0x3e)
    {
        system = Some("Motorola Capacity Max".into());
    }

    match (feature_id, opcode) {
        (0x06, 0x01) => {
            system = Some("Motorola Connect Plus".into());
            adjacent_sites = bytes[2..7]
                .iter()
                .map(|value| value & 0x3f)
                .filter(|&value| value != 0)
                .collect();
        }
        (0x06, 0x03) => {
            system = Some("Motorola Connect Plus".into());
            source_id = Some(u32::from_be_bytes([0, bytes[2], bytes[3], bytes[4]]));
            target_id = Some(u32::from_be_bytes([0, bytes[5], bytes[6], bytes[7]]));
            lcn = Some(u16::from(bytes[8] >> 4));
            timeslot = Some(((bytes[8] >> 3) & 1) + 1);
            call_type = Some(
                match bytes[9] {
                    2 => "group voice grant",
                    3 => "private voice grant",
                    _ => "voice grant",
                }
                .into(),
            );
            vendor_fields.insert("options".into(), format!("0x{:02X}", bytes[9]));
        }
        (0x06, 0x06) => {
            system = Some("Motorola Connect Plus".into());
            target_id = Some(u32::from_be_bytes([0, bytes[2], bytes[3], bytes[4]]));
            lcn = Some(u16::from(bytes[5] >> 4));
            timeslot = Some(((bytes[5] >> 3) & 1) + 1);
            call_type = Some("data grant".into());
        }
        (0x06, 0x0c) => {
            system = Some("Motorola Connect Plus".into());
            target_id = Some(u32::from_be_bytes([0, bytes[2], bytes[3], bytes[4]]));
            call_type = Some("slot termination".into());
        }
        (0x10, 0x3a) => {
            system = Some("Motorola Capacity Plus".into());
            call_type = Some("channel status".into());
            rest_lsn = Some((read_bits(&bits, 20, 4) as u8).max(1));
            vendor_fields.insert("fragment".into(), read_bits(&bits, 16, 2).to_string());
        }
        (0x10, 0x3b) => {
            system = Some("Motorola Capacity Plus".into());
            call_type = Some("adjacent sites".into());
            for index in 0..6 {
                let site = read_bits(&bits, 32 + index * 8, 4) as u8;
                let rest = read_bits(&bits, 36 + index * 8, 4) as u8;
                if site != 0 {
                    adjacent_sites.push(site);
                    vendor_fields.insert(format!("site{site}RestLsn"), rest.to_string());
                }
            }
        }
        (0x10, 0x3e) => {
            system = Some("Motorola Capacity Plus".into());
            call_type = Some("channel status".into());
            rest_lsn = Some(read_bits(&bits, 20, 4) as u8);
            vendor_fields.insert("fragment".into(), read_bits(&bits, 16, 2).to_string());
            vendor_fields.insert(
                "statusSlot".into(),
                (read_bits(&bits, 18, 1) + 1).to_string(),
            );
            vendor_fields.insert("activeBank".into(), format!("0x{:02X}", bytes[3]));
            // A single-block Capacity Plus status contains an activity bitmap
            // followed by one-byte target IDs, then (when room permits) the
            // second bitmap and its targets. Group voice and data share this
            // representation, so retain the ambiguity.
            let fragment = read_bits(&bits, 16, 2) as u8;
            if fragment == 3 {
                active_slots = decode_capacity_plus_activity(&bits[..80]);
                vendor_fields.insert("activeSlots".into(), active_slots.len().to_string());
            }
        }
        (0x68, 0x0a) => {
            system = Some("Hytera XPT".into());
            call_type = Some("site status".into());
            let sequence = read_bits(&bits, 0, 2) as u8;
            let free_lcn = read_bits(&bits, 16, 4) as u8;
            lcn = Some(u16::from(free_lcn));
            vendor_fields.insert("sequence".into(), sequence.to_string());
            for index in 0..6 {
                let status = read_bits(&bits, 20 + index * 2, 2);
                let target = read_bits(&bits, 32 + index * 8, 8);
                let lsn = usize::from(sequence) * 6 + index + 1;
                vendor_fields.insert(
                    format!("lsn{lsn}"),
                    format!("status {status} target {target}"),
                );
                if target != 0 && matches!(status, 2 | 3) {
                    active_slots.push(active_slot(
                        lsn,
                        target,
                        if status == 2 {
                            "private voice or data"
                        } else {
                            "group voice or data"
                        },
                    ));
                }
            }
        }
        (0x68, 0x0b) => {
            system = Some("Hytera XPT".into());
            call_type = Some("adjacent site".into());
            vendor_fields.insert("sequence".into(), read_bits(&bits, 16, 4).to_string());
            vendor_fields.insert("payload".into(), hex(&bytes[3..10]));
        }
        _ => {}
    }

    // Unit-to-unit service requests carry the pair the call is between.
    if matches!(opcode, 0x04 | 0x05) {
        target_id = Some(read_bits(&bits, 16, 24));
        source_id = Some(read_bits(&bits, 40, 24));
        system = system.or_else(|| "DMR Tier III".to_string().into());
        call_type = Some(
            if opcode == 0x04 {
                "unit-to-unit voice request"
            } else {
                "unit-to-unit voice answer"
            }
            .to_string(),
        );
    }
    // Motorola's unidentified data-channel CSBK: keep the payload where it can
    // be read, since that is all anyone has on it.
    if (feature_id, opcode) == (0x10, 0x29) {
        system = Some("Motorola Capacity Max".into());
        vendor_fields.insert("payloadHex".into(), hex(&bytes[2..10]));
        vendor_fields.insert("interpretation".into(), "unconfirmed — payload only".into());
    }

    // Nothing in the table matched, but the frame passed CRC, so it is a real
    // CSBK this build has no name for. Every Tier III CSBK carries its 64-bit
    // payload in the same place, and most put a 24-bit target then a 24-bit
    // source at the front of it. Surface both — labelled as candidates,
    // because that layout is a convention of the common opcodes and not a
    // guarantee for one we cannot identify. If they match real radio IDs on
    // the system, the layout is confirmed; if they do not, that is evidence
    // too, and either beats an opaque UNKNOWN.
    if opcode_name(opcode, feature_id) == "UNKNOWN_CSBK" {
        vendor_fields.insert("payloadHex".into(), hex(&bytes[2..10]));
        vendor_fields.insert(
            "candidateTargetId".into(),
            read_bits(&bits, 16, 24).to_string(),
        );
        vendor_fields.insert(
            "candidateSourceId".into(),
            read_bits(&bits, 40, 24).to_string(),
        );
        vendor_fields.insert("lastBlock".into(), (bytes[0] & 0x80 != 0).to_string());
        vendor_fields.insert("protectFlag".into(), (bytes[0] & 0x40 != 0).to_string());
    }

    Ok(Csbk {
        last_block: bytes[0] & 0x80 != 0,
        protected: bytes[0] & 0x40 != 0,
        opcode,
        opcode_name: match opcode_name(opcode, feature_id) {
            // An unnamed opcode is still a CRC-valid frame off the air, so say
            // which one it was. "UNKNOWN_CSBK" on its own gives the operator
            // nothing to report and nothing to look up.
            "UNKNOWN_CSBK" => format!("UNKNOWN_CSBK op=0x{opcode:02X} mfid=0x{feature_id:02X}"),
            named => named.to_string(),
        },
        feature_id,
        corrected_bits,
        raw,
        lcn,
        timeslot,
        target_id,
        source_id,
        system_code,
        network_model,
        network_id,
        site_id,
        category,
        registration_required,
        backoff,
        announcement_type,
        announcement_name,
        announced_channels,
        announced_color_codes,
        multi_block: false,
        absolute_tx_hz: None,
        absolute_rx_hz: None,
        system,
        call_type,
        rest_lsn,
        adjacent_sites,
        active_slots,
        vendor_fields,
    })
}

fn active_slot(lsn: usize, target_id: u32, activity_type: &str) -> DmrActiveSlot {
    DmrActiveSlot {
        lsn: lsn as u8,
        lcn: ((lsn + 1) / 2) as u16,
        timeslot: ((lsn - 1) % 2 + 1) as u8,
        target_id,
        activity_type: activity_type.into(),
    }
}

fn bytes_to_bits(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .flat_map(|byte| (0..8).rev().map(move |bit| (byte >> bit) & 1))
        .collect()
}

fn bits_at(bits: &[u8], start: usize, len: usize) -> Option<u32> {
    (start + len <= bits.len()).then(|| read_bits(bits, start, len))
}

/// Decode Motorola Capacity Plus channel-status payload after proprietary
/// fragment reassembly. The first and second banks describe group activity;
/// optional following banks carry 16-bit private/data targets.
fn decode_capacity_plus_activity(bits: &[u8]) -> Vec<DmrActiveSlot> {
    let mut out = Vec::new();
    let Some(bank_one) = bits_at(bits, 24, 8).map(|v| v as u8) else {
        return out;
    };
    let count_one = bank_one.count_ones() as usize;
    let bank_two_pos = 32 + count_one * 8;
    let bank_two = bits_at(bits, bank_two_pos, 8).unwrap_or(0) as u8;
    let mut cursor = 32usize;
    for bit in 0..8 {
        if bank_one & (0x80 >> bit) != 0 {
            if let Some(target) = bits_at(bits, cursor, 8) {
                out.push(active_slot(bit + 1, target, "group voice or data"));
            }
            cursor += 8;
        }
    }
    cursor += 8; // second-bank bitmap sits between the two target lists
    for bit in 0..8 {
        if bank_two & (0x80 >> bit) != 0 {
            if let Some(target) = bits_at(bits, cursor, 8) {
                out.push(active_slot(bit + 9, target, "group voice or data"));
            }
            cursor += 8;
        }
    }

    // Each optional private/data section is flag, bitmap, then one 16-bit
    // target for every set bit. DSD-FME also treats these as ambiguous because
    // the status CSBK itself does not say whether the bearer is voice or data.
    for bank in 0..2 {
        let Some(flag) = bits_at(bits, cursor, 8) else {
            break;
        };
        cursor += 8;
        if flag == 0 {
            continue;
        }
        let Some(mask) = bits_at(bits, cursor, 8).map(|v| v as u8) else {
            break;
        };
        cursor += 8;
        for bit in 0..8 {
            if mask & (0x80 >> bit) != 0 {
                if let Some(target) = bits_at(bits, cursor, 16) {
                    out.push(active_slot(
                        bank * 8 + bit + 1,
                        target,
                        "private voice or data",
                    ));
                }
                cursor += 16;
            }
        }
    }
    out
}

fn decode_system_code(code: u16) -> (&'static str, u16, u16) {
    match code >> 12 {
        0 => ("Tiny", ((code >> 3) & 0x01ff) + 1, (code & 0x0007) + 1),
        1 => ("Small", ((code >> 5) & 0x007f) + 1, (code & 0x001f) + 1),
        2 => ("Large", ((code >> 8) & 0x000f) + 1, (code & 0x00ff) + 1),
        _ => ("Huge", ((code >> 10) & 0x0003) + 1, (code & 0x03ff) + 1),
    }
}

fn announcement_type_name(kind: u8) -> &'static str {
    match kind {
        0 => "announce/withdraw control channels",
        1 => "call timer parameters",
        2 => "vote now",
        3 => "local time",
        4 => "mass registration",
        5 => "channel/frequency relationship",
        6 => "adjacent site",
        7 => "general site parameters",
        0x1e | 0x1f => "manufacturer-specific announcement",
        _ => "reserved announcement",
    }
}

fn tier_three_service_name(kind: u8) -> &'static str {
    match kind {
        0 | 1 => "voice call",
        2 | 3 => "packet data call",
        4 | 5 => "UDT short data call",
        6 => "UDT short data polling",
        7 => "status transport",
        8 => "call diversion",
        9 => "call answer",
        10 => "full-duplex voice call",
        11 => "full-duplex packet data call",
        13 => "supplementary service",
        14 => "registration/authentication",
        15 => "cancel call",
        _ => "reserved service",
    }
}

fn opcode_name(op: u8, feature_id: u8) -> &'static str {
    match (feature_id, op) {
        (0x06, 0x01) => "CONPLUS_ADJACENT",
        (0x06, 0x03) => "CONPLUS_VOICE_GRANT",
        (0x06, 0x06) => "CONPLUS_DATA_GRANT",
        (0x06, 0x0c) => "CONPLUS_SLOT_TERMINATION",
        (0x10, 0x3a) => "CAPPLUS_STATUS",
        (0x10, 0x3b) => "CAPPLUS_ADJACENT",
        (0x10, 0x3e) => "CAPPLUS_CHANNEL_STATUS",
        (0x68, 0x0a) => "XPT_SITE_STATUS",
        (0x68, 0x0b) => "XPT_ADJACENT",
        // Motorola opcode 0x29 carries eight payload bytes that nobody has
        // pinned down. DSD-FME files it under "misc discovered but not
        // uncovered", printing it as a data-channel announcement and dumping
        // the payload; SDRTrunk's guess is a data revert channel. Naming it
        // matches what the operator would see elsewhere, and the payload is
        // surfaced below so the guess can be checked rather than trusted.
        (0x10, 0x29) => "MOTO_DATA_CHANNEL",
        // ETSI TS 102 361-4 opcodes this build previously had no name for.
        (_, 0x04) => "UU_V_REQ",
        (_, 0x05) => "UU_ANS_RSP",
        (_, 0x07) => "CT_CSBK",
        (_, 0x22) => "P_ACKD",
        (_, 0x23) => "P_ACKU",
        (_, 0x19) => "C_ALOHA",
        (_, 0x1c) => "C_AHOY",
        (_, 0x1e) => "C_ACKVIT",
        (_, 0x1f) => "C_RAND",
        (_, 0x20) => "C_ACKD",
        (_, 0x21) => "C_ACKU",
        (_, 0x26) => "C_NACK",
        (_, 0x28) => "C_BCAST",
        (_, 0x2a) => "P_MAINT",
        (_, 0x2e) => "P_CLEAR",
        (_, 0x2f) => "P_PROTECT",
        (_, 0x30) => "PV_GRANT",
        (_, 0x31) => "TV_GRANT",
        (_, 0x32) => "BTV_GRANT",
        (_, 0x33) => "PD_GRANT",
        (_, 0x34) => "TD_GRANT",
        (_, 0x35) => "PV_GRANT_DX",
        (_, 0x36) => "PD_GRANT_DX",
        (_, 0x37) => "PD_GRANT_MULTI",
        (_, 0x38) => "TD_GRANT_MULTI",
        (_, 0x39) => "C_MOVE",
        (_, 0x3d) => "PREAMBLE",
        _ => "UNKNOWN_CSBK",
    }
}

fn tier_three_call_type(op: u8) -> &'static str {
    match op {
        0x30 => "PV_GRANT",
        0x31 => "TV_GRANT",
        0x32 => "BTV_GRANT",
        0x33 => "PD_GRANT",
        0x34 => "TD_GRANT",
        0x35 => "PV_GRANT_DX",
        0x36 => "PD_GRANT_DX",
        0x37 => "PD_GRANT_MULTI",
        0x38 => "TD_GRANT_MULTI",
        _ => "control",
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|value| format!("{value:02X}")).collect()
}

pub(super) fn read_bits(bits: &[u8], start: usize, len: usize) -> u32 {
    bits[start..start + len]
        .iter()
        .fold(0, |v, &b| (v << 1) | u32::from(b))
}

pub(super) fn crc_ccitt(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn parity_15(data: &[u8; 11]) -> [u8; 4] {
    let d = |i| data[i];
    [
        d(0) ^ d(1) ^ d(2) ^ d(3) ^ d(5) ^ d(7) ^ d(8),
        d(1) ^ d(2) ^ d(3) ^ d(4) ^ d(6) ^ d(8) ^ d(9),
        d(2) ^ d(3) ^ d(4) ^ d(5) ^ d(7) ^ d(9) ^ d(10),
        d(0) ^ d(1) ^ d(2) ^ d(4) ^ d(6) ^ d(7) ^ d(10),
    ]
}

fn parity_13(data: &[u8; 9]) -> [u8; 4] {
    let d = |i| data[i];
    [
        d(0) ^ d(1) ^ d(3) ^ d(5) ^ d(6),
        d(0) ^ d(1) ^ d(2) ^ d(4) ^ d(6) ^ d(7),
        d(0) ^ d(1) ^ d(2) ^ d(3) ^ d(5) ^ d(7) ^ d(8),
        d(0) ^ d(2) ^ d(4) ^ d(5) ^ d(8),
    ]
}

fn correct<const N: usize>(word: &mut [u8; N], clean: impl Fn(&[u8; N]) -> bool) -> Option<u32> {
    if clean(word) {
        return Some(0);
    }
    for i in 0..N {
        word[i] ^= 1;
        if clean(word) {
            return Some(1);
        }
        word[i] ^= 1;
    }
    None
}

fn clean_row(w: &[u8; 15]) -> bool {
    let data: [u8; 11] = w[..11].try_into().unwrap();
    parity_15(&data).as_slice() == &w[11..]
}
fn clean_col(w: &[u8; 13]) -> bool {
    let data: [u8; 9] = w[..9].try_into().unwrap();
    parity_13(&data).as_slice() == &w[9..]
}

pub(super) fn decode_bptc(channel: &[u8; 196]) -> Option<([u8; 96], u32)> {
    let mut deinterleaved = [0u8; 196];
    for (i, out) in deinterleaved.iter_mut().enumerate() {
        *out = channel[(i * 181) % 196] & 1;
    }
    let mut matrix = [[0u8; 15]; 13];
    for r in 0..13 {
        for c in 0..15 {
            matrix[r][c] = deinterleaved[r * 15 + c + 1];
        }
    }
    let mut corrected = 0;
    for _ in 0..5 {
        let mut changed = 0;
        for c in 0..15 {
            let mut word = std::array::from_fn(|r| matrix[r][c]);
            if let Some(n) = correct(&mut word, clean_col) {
                changed += n;
                corrected += n;
                for r in 0..13 {
                    matrix[r][c] = word[r];
                }
            }
        }
        for row in matrix.iter_mut().take(9) {
            let mut word = *row;
            if let Some(n) = correct(&mut word, clean_row) {
                changed += n;
                corrected += n;
                *row = word;
            }
        }
        if changed == 0 {
            break;
        }
    }
    for c in 0..15 {
        if !clean_col(&std::array::from_fn(|r| matrix[r][c])) {
            return None;
        }
    }
    for row in matrix.iter().take(9) {
        if !clean_row(row) {
            return None;
        }
    }
    let mut info = [0u8; 96];
    let mut i = 0;
    for c in 3..=10 {
        info[i] = matrix[0][c];
        i += 1;
    }
    for row in matrix.iter().take(9).skip(1) {
        for &bit in row.iter().take(11) {
            info[i] = bit;
            i += 1;
        }
    }
    Some((info, corrected))
}

#[cfg(test)]
mod tests {

    /// An opcode the table has no name for still has to report what it was.
    /// Anything less leaves the operator with "UNKNOWN_CSBK" and nothing to
    /// act on — no value to look up and nothing to report.
    #[test]
    fn an_unnamed_opcode_reports_its_identity() {
        assert_eq!(opcode_name(0x2c, 0x00), "UNKNOWN_CSBK");
        let labelled = match opcode_name(0x2c, 0x00) {
            "UNKNOWN_CSBK" => format!("UNKNOWN_CSBK op=0x{:02X} mfid=0x{:02X}", 0x2c, 0x00),
            named => named.to_string(),
        };
        assert_eq!(labelled, "UNKNOWN_CSBK op=0x2C mfid=0x00");
    }

    /// The opcodes added from the ETSI table and from what Motorola systems
    /// are actually seen sending. 0x29 in particular used to be the loudest
    /// unknown on a Capacity Max control channel.
    #[test]
    fn the_opcodes_seen_on_real_systems_are_named() {
        assert_eq!(opcode_name(0x29, 0x10), "MOTO_DATA_CHANNEL");
        assert_eq!(opcode_name(0x04, 0x00), "UU_V_REQ");
        assert_eq!(opcode_name(0x05, 0x00), "UU_ANS_RSP");
        assert_eq!(opcode_name(0x07, 0x00), "CT_CSBK");
        assert_eq!(opcode_name(0x22, 0x00), "P_ACKD");
        assert_eq!(opcode_name(0x23, 0x00), "P_ACKU");
        // Vendor opcodes must still win over the generic table.
        assert_eq!(opcode_name(0x3a, 0x10), "CAPPLUS_STATUS");
        assert_eq!(opcode_name(0x01, 0x06), "CONPLUS_ADJACENT");
    }
    use super::*;
    use crate::dmr::fec::SlotType;
    use crate::dmr::{BurstKind, SyncSource};

    fn encode_bptc(info: &[u8; 96]) -> [u8; 196] {
        let mut matrix = [[0u8; 15]; 13];
        let mut i = 0;
        for c in 3..=10 {
            matrix[0][c] = info[i];
            i += 1;
        }
        for row in matrix.iter_mut().take(9).skip(1) {
            for cell in row.iter_mut().take(11) {
                *cell = info[i];
                i += 1;
            }
        }
        for row in matrix.iter_mut().take(9) {
            let data: [u8; 11] = row[..11].try_into().unwrap();
            row[11..].copy_from_slice(&parity_15(&data));
        }
        for c in 0..15 {
            let data: [u8; 9] = std::array::from_fn(|r| matrix[r][c]);
            let parity = parity_13(&data);
            for r in 9..13 {
                matrix[r][c] = parity[r - 9];
            }
        }
        let mut deinterleaved = [0u8; 196];
        for r in 0..13 {
            for c in 0..15 {
                deinterleaved[r * 15 + c + 1] = matrix[r][c];
            }
        }
        let mut channel = [0u8; 196];
        for i in 0..196 {
            channel[(i * 181) % 196] = deinterleaved[i];
        }
        channel
    }

    fn csbk_burst(payload: [u8; 10]) -> Burst {
        let mut bytes = [0u8; 12];
        bytes[..10].copy_from_slice(&payload);
        let crc = crc_ccitt(&bytes[..10]) ^ 0x5a5a;
        bytes[10..].copy_from_slice(&crc.to_be_bytes());
        bptc_burst(bytes, 3)
    }

    fn bptc_burst(bytes: [u8; 12], data_type: u8) -> Burst {
        let info: [u8; 96] = std::array::from_fn(|i| (bytes[i / 8] >> (7 - i % 8)) & 1);
        let channel = encode_bptc(&info);
        let mut dibits = [0u8; 132];
        for i in 0..49 {
            dibits[i] = channel[i * 2] << 1 | channel[i * 2 + 1];
            dibits[83 + i] = channel[98 + i * 2] << 1 | channel[99 + i * 2];
        }
        Burst {
            slot: Some(1),
            source: SyncSource::Bs,
            kind: BurstKind::Data {
                slot_type: Some(SlotType {
                    color_code: 7,
                    data_type,
                }),
                sync: None,
            },
            dibits,
        }
    }

    fn put_bits(bits: &mut [u8], start: usize, len: usize, value: u64) {
        for i in 0..len {
            bits[start + i] = ((value >> (len - 1 - i)) & 1) as u8;
        }
    }

    #[test]
    fn crc_vector() {
        assert_eq!(crc_ccitt(b"123456789"), 0x31c3);
    }

    #[test]
    fn tier_three_grant_survives_bptc_and_crc() {
        let csbk = decode_csbk(&csbk_burst([
            0xb1, 0x00, 0x12, 0x38, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
        ]))
        .unwrap();
        assert_eq!(csbk.opcode_name, "TV_GRANT");
        assert_eq!(csbk.lcn, Some(0x123));
        assert_eq!(csbk.timeslot, Some(2));
        assert_eq!(csbk.target_id, Some(0x010203));
        assert_eq!(csbk.source_id, Some(0x040506));
    }

    #[test]
    fn aloha_names_the_tier_three_network_and_site() {
        // Small model, raw network 17, raw site 5, category AB.
        let code = 0x1000 | (17 << 5) | 5;
        let mut payload = [0u8; 10];
        payload[0] = 0x19;
        let mut bits = [0u8; 80];
        for (i, bit) in bits.iter_mut().enumerate() {
            *bit = (payload[i / 8] >> (7 - i % 8)) & 1;
        }
        for i in 0..14 {
            bits[40 + i] = ((code >> (13 - i)) & 1) as u8;
        }
        bits[54] = 1;
        bits[55] = 1;
        bits[35] = 1;
        for (i, bit) in bits.iter().enumerate() {
            payload[i / 8] |= bit << (7 - i % 8);
        }
        let csbk = decode_csbk(&csbk_burst(payload)).unwrap();
        assert_eq!(csbk.call_type.as_deref(), Some("system parameters"));
        assert_eq!(csbk.network_model.as_deref(), Some("Small"));
        assert_eq!(csbk.network_id, Some(18));
        assert_eq!(csbk.site_id, Some(6));
        assert_eq!(csbk.category.as_deref(), Some("AB"));
        assert_eq!(csbk.registration_required, Some(true));
    }

    #[test]
    fn broadcast_adjacent_site_exposes_identity_and_lpcn() {
        let adjacent_code = 0x2000 | (3 << 8) | 42;
        let mut bits = [0u8; 80];
        bits[2] = 1; // opcode 0x28
        bits[4] = 1;
        bits[16..21].copy_from_slice(&[0, 0, 1, 1, 0]); // type 6
        for i in 0..14 {
            bits[21 + i] = ((adjacent_code >> (13 - i)) & 1) as u8;
        }
        let channel = 75u16;
        for i in 0..12 {
            bits[68 + i] = ((channel >> (11 - i)) & 1) as u8;
        }
        let mut payload = [0u8; 10];
        for (i, bit) in bits.iter().enumerate() {
            payload[i / 8] |= bit << (7 - i % 8);
        }
        let csbk = decode_csbk(&csbk_burst(payload)).unwrap();
        assert_eq!(csbk.announcement_name.as_deref(), Some("adjacent site"));
        assert_eq!(csbk.network_model.as_deref(), Some("Large"));
        assert_eq!(csbk.network_id, Some(4));
        assert_eq!(csbk.site_id, Some(43));
        assert_eq!(csbk.announced_channels, vec![75]);
    }

    #[test]
    fn mbc_absolute_grant_supplies_frequency_without_an_lcn_map() {
        let mut header = [0u8; 12];
        header[..10].copy_from_slice(&[0x31, 0x00, 0xff, 0xf0, 0x00, 0x12, 0x34, 0x00, 0x56, 0x78]);
        let crc = crc_ccitt(&header[..10]) ^ 0x5555;
        header[10..].copy_from_slice(&crc.to_be_bytes());

        let mut continuation_bits = [0u8; 96];
        continuation_bits[0] = 1; // last block
        put_bits(&mut continuation_bits, 12, 4, 7); // color code
        put_bits(&mut continuation_bits, 16, 4, 0); // absolute channel definition
        put_bits(&mut continuation_bits, 22, 12, 75);
        put_bits(&mut continuation_bits, 34, 10, 451);
        put_bits(&mut continuation_bits, 44, 13, 800); // 100 kHz
        put_bits(&mut continuation_bits, 57, 10, 452);
        put_bits(&mut continuation_bits, 67, 13, 6000); // 750 kHz
        let mut continuation = [0u8; 12];
        for (i, &bit) in continuation_bits.iter().take(80).enumerate() {
            continuation[i / 8] |= bit << (7 - i % 8);
        }
        let crc = crc_ccitt(&continuation[..10]) ^ 0xffff;
        continuation[10..].copy_from_slice(&crc.to_be_bytes());

        let mut decoder = ControlDecoder::new();
        assert!(decoder.process(&bptc_burst(header, 4)).is_none());
        let csbk = decoder
            .process(&bptc_burst(continuation, 5))
            .expect("completed MBC")
            .expect("valid MBC");
        assert!(csbk.multi_block);
        assert_eq!(csbk.lcn, Some(75));
        assert_eq!(csbk.absolute_tx_hz, Some(451_100_000));
        assert_eq!(csbk.absolute_rx_hz, Some(452_750_000));
        assert_eq!(csbk.announced_color_codes, vec![7]);
    }

    #[test]
    fn mbc_channel_frequency_announcement_populates_logical_channel() {
        let mut header_bits = [0u8; 96];
        put_bits(&mut header_bits, 2, 6, 0x28); // C_BCAST
        put_bits(&mut header_bits, 16, 5, 5); // Chan_Freq announcement
        put_bits(&mut header_bits, 68, 12, 147); // header LPCN relationship
        let mut header = [0u8; 12];
        for (i, &bit) in header_bits.iter().take(80).enumerate() {
            header[i / 8] |= bit << (7 - i % 8);
        }
        let crc = crc_ccitt(&header[..10]) ^ 0x5555;
        header[10..].copy_from_slice(&crc.to_be_bytes());

        let mut continuation_bits = [0u8; 96];
        continuation_bits[0] = 1; // last block
        put_bits(&mut continuation_bits, 12, 4, 3); // color code
        put_bits(&mut continuation_bits, 16, 4, 0); // absolute channel definition
        put_bits(&mut continuation_bits, 22, 12, 147);
        put_bits(&mut continuation_bits, 34, 10, 451);
        put_bits(&mut continuation_bits, 44, 13, 800);
        put_bits(&mut continuation_bits, 57, 10, 452);
        put_bits(&mut continuation_bits, 67, 13, 6000);
        let mut continuation = [0u8; 12];
        for (i, &bit) in continuation_bits.iter().take(80).enumerate() {
            continuation[i / 8] |= bit << (7 - i % 8);
        }
        let crc = crc_ccitt(&continuation[..10]) ^ 0xffff;
        continuation[10..].copy_from_slice(&crc.to_be_bytes());

        let mut decoder = ControlDecoder::new();
        assert!(decoder.process(&bptc_burst(header, 4)).is_none());
        let csbk = decoder
            .process(&bptc_burst(continuation, 5))
            .expect("completed MBC")
            .expect("valid MBC");
        assert_eq!(csbk.announcement_type, Some(5));
        assert_eq!(csbk.lcn, Some(147));
        assert_eq!(csbk.announced_channels, vec![147]);
        assert_eq!(csbk.absolute_rx_hz, Some(452_750_000));
    }

    #[test]
    fn xpt_status_exposes_group_and_private_logical_slots() {
        let mut bits = [0u8; 80];
        put_bits(&mut bits, 2, 6, 0x0a); // opcode, sequence zero
        put_bits(&mut bits, 8, 8, 0x68);
        put_bits(&mut bits, 16, 4, 4); // free LCN
        put_bits(&mut bits, 20, 2, 3); // group activity, LSN 1
        put_bits(&mut bits, 22, 2, 2); // private activity, LSN 2
        put_bits(&mut bits, 32, 8, 21);
        put_bits(&mut bits, 40, 8, 37);
        let mut payload = [0u8; 10];
        for (i, &bit) in bits.iter().enumerate() {
            payload[i / 8] |= bit << (7 - i % 8);
        }
        let csbk = decode_csbk(&csbk_burst(payload)).unwrap();
        assert_eq!(csbk.active_slots.len(), 2);
        assert_eq!(
            csbk.active_slots[0],
            active_slot(1, 21, "group voice or data")
        );
        assert_eq!(
            csbk.active_slots[1],
            active_slot(2, 37, "private voice or data")
        );
    }

    #[test]
    fn capacity_plus_single_block_exposes_active_targets() {
        let csbk = decode_csbk(&csbk_burst([0x3e, 0x10, 0xc1, 0x80, 42, 0, 0, 0, 0, 0])).unwrap();
        assert_eq!(csbk.rest_lsn, Some(1));
        assert_eq!(
            csbk.active_slots,
            vec![active_slot(1, 42, "group voice or data")]
        );
    }

    #[test]
    fn capacity_plus_fragments_reassemble_per_timeslot() {
        let mut decoder = ControlDecoder::new();
        let initial = decoder
            .process(&csbk_burst([0x3e, 0x10, 0x81, 0x80, 42, 0x80, 55, 0, 0, 0]))
            .expect("initial status remains visible")
            .unwrap();
        assert!(initial.active_slots.is_empty());

        let final_status = decoder
            .process(&csbk_burst([0x3e, 0x10, 0x41, 0, 0, 0, 0, 0, 0, 0]))
            .expect("final status")
            .unwrap();
        assert_eq!(
            final_status.active_slots,
            vec![
                active_slot(1, 42, "group voice or data"),
                active_slot(9, 55, "group voice or data"),
            ]
        );
        assert_eq!(
            final_status
                .vendor_fields
                .get("assembledBits")
                .map(String::as_str),
            Some("136")
        );
    }
}
