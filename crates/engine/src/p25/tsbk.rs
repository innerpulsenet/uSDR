//! TSBK: the trunking signalling blocks a control channel is made of.
//!
//! A TSDU carries one to three Trunking Signalling Blocks. Each is 96 bits —
//! 80 of message and a 16-bit CRC — protected by a rate-½ trellis code that
//! expands 48 dibits to 98, and interleaved across the block so a burst of
//! symbol errors is spread thin before the Viterbi decoder sees it.
//!
//! Two details cannot be derived from the signal or from first principles: which
//! direction the interleave table runs, and exactly how the CRC is framed. Both
//! are settled the same way the NID's bit order was — by computing every
//! candidate and letting a live capture say which one produces valid blocks.
//! [`Convention`] exists for that, and once a capture has spoken there is one
//! right answer and the rest are dead weight.

/// Dibits on the air per TSBK.
pub const TSBK_DIBITS: usize = 98;
/// Bits on the air per TSBK.
pub const TSBK_BITS: usize = TSBK_DIBITS * 2;
/// Decoded bits per TSBK: 80 of message plus a 16-bit CRC.
pub const TSBK_DECODED_BITS: usize = 96;

/// The rate-½ trellis constellation: `TABLE[state][input]` is the 4-bit symbol
/// transmitted, and the next state is the input dibit itself.
const CONSTELLATION: [[u8; 4]; 4] = [
    [0x2, 0xC, 0x1, 0xF],
    [0xE, 0x0, 0xD, 0x3],
    [0x9, 0x7, 0xA, 0x4],
    [0x5, 0xB, 0x6, 0x8],
];

/// Which way round the interleave table and CRC framing go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Convention {
    /// True when the schedule lists, for each output position, where to read.
    pub deinterleave_forward: bool,
    /// True when the CRC register starts at all ones rather than zero.
    pub crc_init_ones: bool,
}

impl Default for Convention {
    fn default() -> Self {
        Self {
            deinterleave_forward: true,
            crc_init_ones: false,
        }
    }
}

impl Convention {
    /// Every candidate, for a capture to choose between.
    pub fn all() -> [Convention; 4] {
        [
            Convention {
                deinterleave_forward: true,
                crc_init_ones: false,
            },
            Convention {
                deinterleave_forward: true,
                crc_init_ones: true,
            },
            Convention {
                deinterleave_forward: false,
                crc_init_ones: false,
            },
            Convention {
                deinterleave_forward: false,
                crc_init_ones: true,
            },
        ]
    }

    pub fn label(&self) -> String {
        format!(
            "{}/{}",
            if self.deinterleave_forward {
                "fwd"
            } else {
                "rev"
            },
            if self.crc_init_ones { "init1" } else { "init0" }
        )
    }
}

/// The block interleave schedule: 4-bit groups taken at strides of 52.
///
/// Generated rather than transcribed, because 196 hand-copied numbers is 196
/// chances to make a silent mistake — and the generator is checked to be a
/// permutation, which a transcription could not be.
pub fn interleave_schedule() -> [usize; TSBK_BITS] {
    let mut out = [0usize; TSBK_BITS];
    let mut n = 0;
    for r in 0..13 {
        for (bi, base) in [0usize, 52, 100, 148].iter().enumerate() {
            if bi > 0 && r >= 12 {
                continue;
            }
            for k in 0..4 {
                out[n] = base + 4 * r + k;
                n += 1;
            }
        }
    }
    debug_assert_eq!(n, TSBK_BITS);
    out
}

/// Undo the block interleave.
pub fn deinterleave(bits: &[u8], forward: bool) -> Vec<u8> {
    let sched = interleave_schedule();
    let mut out = vec![0u8; TSBK_BITS];
    for i in 0..TSBK_BITS {
        if forward {
            out[i] = bits[sched[i]];
        } else {
            out[sched[i]] = bits[i];
        }
    }
    out
}

fn deinterleave_soft(p: &[f32], forward: bool) -> Vec<f32> {
    let sched = interleave_schedule();
    let mut out = vec![0.0f32; TSBK_BITS];
    for i in 0..TSBK_BITS {
        if forward {
            out[i] = p[sched[i]];
        } else {
            out[sched[i]] = p[i];
        }
    }
    out
}

/// Viterbi decode of the rate-½ trellis code: 196 bits in, 96 out.
///
/// Four states, one per possible previous dibit, and 49 steps — 48 of data plus
/// the flush the encoder appends. Branch metrics are Hamming distances between
/// the received nibble and the constellation symbol each transition would have
/// produced.
pub fn trellis_decode(bits: &[u8]) -> Vec<u8> {
    let steps = TSBK_BITS / 4; // 49 nibbles
    let mut cost = [u32::MAX; 4];
    cost[0] = 0; // the encoder starts in state zero
    let mut back = vec![[0u8; 4]; steps];

    for step in 0..steps {
        let mut nibble = 0u8;
        for k in 0..4 {
            nibble = (nibble << 1) | (bits[step * 4 + k] & 1);
        }
        let mut next = [u32::MAX; 4];
        for state in 0..4usize {
            if cost[state] == u32::MAX {
                continue;
            }
            for input in 0..4usize {
                let expect = CONSTELLATION[state][input];
                let metric = (expect ^ nibble).count_ones();
                let total = cost[state] + metric;
                // The next state is the input dibit, which is what makes the
                // traceback yield the message directly.
                if total < next[input] {
                    next[input] = total;
                    back[step][input] = state as u8;
                }
            }
        }
        cost = next;
    }

    // The flush leaves the encoder in state zero, so that is where to start.
    let mut state = 0usize;
    if cost[0] == u32::MAX {
        state = cost
            .iter()
            .enumerate()
            .min_by_key(|(_, c)| **c)
            .map(|(i, _)| i)
            .unwrap_or(0);
    }
    let mut dibits = vec![0u8; steps];
    for step in (0..steps).rev() {
        dibits[step] = state as u8;
        state = back[step][state] as usize;
    }

    // Drop the flush dibit; the remaining 48 carry the 96 message bits.
    let mut out = Vec::with_capacity(TSBK_DECODED_BITS);
    for &d in dibits.iter().take(48) {
        out.push((d >> 1) & 1);
        out.push(d & 1);
    }
    out
}

/// Soft-decision Viterbi: `p[i]` is P(bit i = 1) in `0.0..=1.0`.
///
/// Branch metric is the sum of per-bit disagreements, so a half-confident
/// error costs less than a hard flip. On a clean block (p near 0 or 1) this
/// is the Hamming decoder; on a weak control channel it is what keeps a
/// TSBK's CRC passing when two or three symbols sit near a threshold.
pub fn trellis_decode_soft(p: &[f32]) -> Vec<u8> {
    let steps = TSBK_BITS / 4;
    let mut cost = [f32::INFINITY; 4];
    cost[0] = 0.0;
    let mut back = vec![[0u8; 4]; steps];

    for step in 0..steps {
        let mut next = [f32::INFINITY; 4];
        for state in 0..4usize {
            if !cost[state].is_finite() {
                continue;
            }
            for input in 0..4usize {
                let expect = CONSTELLATION[state][input];
                let mut metric = 0.0f32;
                for k in 0..4 {
                    let want = f32::from((expect >> (3 - k)) & 1);
                    let pk = p.get(step * 4 + k).copied().unwrap_or(0.5);
                    metric += if want > 0.5 { 1.0 - pk } else { pk };
                }
                let total = cost[state] + metric;
                if total < next[input] {
                    next[input] = total;
                    back[step][input] = state as u8;
                }
            }
        }
        cost = next;
    }

    let mut state = 0usize;
    if !cost[0].is_finite() {
        state = cost
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .unwrap_or(0);
    }
    let mut dibits = vec![0u8; steps];
    for step in (0..steps).rev() {
        dibits[step] = state as u8;
        state = back[step][state] as usize;
    }

    let mut out = Vec::with_capacity(TSBK_DECODED_BITS);
    for &d in dibits.iter().take(48) {
        out.push((d >> 1) & 1);
        out.push(d & 1);
    }
    out
}

/// Soft bits of one C4FM symbol: `(P(msb=1), P(lsb=1))`.
///
/// The Gray map is `01`=+outer, `00`=+inner, `10`=−inner, `11`=−outer, so
/// the MSB is the sign and the LSB is |f| ≷ midpoint.
fn dibit_soft(hz: f32, outer: f32) -> (f32, f32) {
    let mid = outer * (super::DEV_INNER_HZ + super::DEV_OUTER_HZ) / (2.0 * super::DEV_OUTER_HZ);
    let t = (0.25 * outer).max(1.0);
    let p_msb = sigmoid(-hz / t);
    let p_lsb = sigmoid((hz.abs() - mid) / t);
    (p_msb, p_lsb)
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// CRC-16 over `bits`, with the P25 polynomial x^16 + x^12 + x^5 + 1.
pub fn crc16(bits: &[u8], init_ones: bool) -> u16 {
    const POLY: u32 = 0x1021;
    let mut crc: u32 = if init_ones { 0xFFFF } else { 0 };
    // Feed the message followed by sixteen zeros, which is the remainder of
    // m(x)·x^16 modulo the generator.
    for &b in bits.iter().chain([0u8; 16].iter()) {
        crc = (crc << 1) | u32::from(b & 1);
        if crc & 0x1_0000 != 0 {
            crc = (crc & 0xFFFF) ^ POLY;
        }
    }
    (crc as u16) ^ 0xFFFF
}

/// Test-vector encoder shared with the packet-data regression suite.
#[cfg(test)]
pub(crate) fn encode_test_block(mut bytes: [u8; 12], header_crc: bool) -> Vec<u8> {
    if header_crc {
        let bits: Vec<u8> = bytes[..10]
            .iter()
            .flat_map(|byte| (0..8).rev().map(move |bit| (byte >> bit) & 1))
            .collect();
        bytes[10..].copy_from_slice(&crc16(&bits, false).to_be_bytes());
    }
    let message: Vec<u8> = bytes
        .iter()
        .flat_map(|byte| (0..8).rev().map(move |bit| (byte >> bit) & 1))
        .collect();
    let mut state = 0usize;
    let mut coded = Vec::with_capacity(TSBK_BITS);
    for dibit in message
        .chunks_exact(2)
        .map(|bits| (bits[0] << 1) | bits[1])
        .chain(std::iter::once(0))
    {
        let nibble = CONSTELLATION[state][usize::from(dibit)];
        for bit in (0..4).rev() {
            coded.push((nibble >> bit) & 1);
        }
        state = usize::from(dibit);
    }
    let interleaved = deinterleave(&coded, false);
    interleaved
        .chunks_exact(2)
        .map(|bits| (bits[0] << 1) | bits[1])
        .collect()
}

/// One decoded signalling block.
#[derive(Clone, Debug, PartialEq)]
pub struct Tsbk {
    /// Set on the final block of a TSDU.
    pub last: bool,
    pub protected: bool,
    pub opcode: u8,
    pub mfid: u8,
    /// The 64-bit argument field, opcode-dependent.
    pub args: u64,
    pub crc_ok: bool,
}

impl Tsbk {
    /// Decode one block from its 98 on-air dibits.
    pub fn decode(dibits: &[u8], conv: Convention) -> Option<Tsbk> {
        if dibits.len() < TSBK_DIBITS {
            return None;
        }
        let hz: Vec<f32> = dibits[..TSBK_DIBITS]
            .iter()
            .map(|&d| super::dibit_level(d))
            .collect();
        Self::decode_soft(&hz, super::DEV_OUTER_HZ, conv)
    }

    /// Decode one block from the 98 frequency samples, using soft metrics.
    ///
    /// `amp` is the fitted outer deviation so the soft bit probabilities
    /// sit on the same scale as the discriminator.
    pub fn decode_soft(hz: &[f32], amp: f32, conv: Convention) -> Option<Tsbk> {
        if hz.len() < TSBK_DIBITS {
            return None;
        }
        let mut soft = Vec::with_capacity(TSBK_BITS);
        for &s in &hz[..TSBK_DIBITS] {
            let (msb, lsb) = dibit_soft(s, amp);
            soft.push(msb);
            soft.push(lsb);
        }
        let deint = deinterleave_soft(&soft, conv.deinterleave_forward);
        let msg = trellis_decode_soft(&deint);

        let word = |from: usize, len: usize| -> u64 {
            let mut v = 0u64;
            for k in 0..len {
                v = (v << 1) | u64::from(msg[from + k]);
            }
            v
        };
        let crc_rx = word(80, 16) as u16;
        let crc_calc = crc16(&msg[..80], conv.crc_init_ones);

        Some(Tsbk {
            last: msg[0] == 1,
            protected: msg[1] == 1,
            opcode: word(2, 6) as u8,
            mfid: word(8, 8) as u8,
            args: word(16, 64),
            crc_ok: crc_rx == crc_calc,
        })
    }

    /// The 16-bit channel field: a band-plan identifier and a channel number.
    pub fn split_channel(ch: u16) -> (u8, u16) {
        ((ch >> 12) as u8, ch & 0x0FFF)
    }

    /// What this block is saying, for the opcodes a scanner cares about.
    pub fn event(&self) -> TsbkEvent {
        // Manufacturer-specific blocks reuse the opcode space, so anything with
        // a non-standard MFID is left alone rather than misread.
        if self.mfid != 0 {
            let a = self.args;
            if self.mfid == 0x90 && self.opcode == 0x02 {
                return TsbkEvent::GroupVoiceGrant {
                    service_options: ((a >> 56) & 0xff) as u8,
                    channel: ((a >> 40) & 0xffff) as u16,
                    talkgroup: ((a >> 24) & 0xffff) as u16,
                    source: (a & 0xff_ffff) as u32,
                };
            }
            if self.mfid == 0x90 && self.opcode == 0x03 {
                return TsbkEvent::GroupVoiceUpdate {
                    first: (((a >> 48) & 0xffff) as u16, ((a >> 32) & 0xffff) as u16),
                    second: Some((((a >> 16) & 0xffff) as u16, (a & 0xffff) as u16)),
                };
            }
            if self.mfid == 0xa4 && self.opcode == 0x30 {
                return TsbkEvent::HarrisRegroupEncryption {
                    options: ((a >> 56) & 0xff) as u8,
                    supergroup: ((a >> 40) & 0xffff) as u16,
                    key_id: ((a >> 24) & 0xffff) as u16,
                    address: (a & 0xff_ffff) as u32,
                };
            }
            return TsbkEvent::Other {
                opcode: self.opcode,
            };
        }
        let a = self.args;
        match self.opcode {
            // Group Voice Channel Grant: options, channel, group, source.
            0x00 => TsbkEvent::GroupVoiceGrant {
                service_options: ((a >> 56) & 0xFF) as u8,
                channel: ((a >> 40) & 0xFFFF) as u16,
                talkgroup: ((a >> 24) & 0xFFFF) as u16,
                source: (a & 0xFF_FFFF) as u32,
            },
            // Grant Update carries two independent (channel, group) pairs.
            0x02 => TsbkEvent::GroupVoiceUpdate {
                first: (((a >> 48) & 0xFFFF) as u16, ((a >> 32) & 0xFFFF) as u16),
                second: {
                    let ch = ((a >> 16) & 0xFFFF) as u16;
                    let tg = (a & 0xFFFF) as u16;
                    // The second slot is padded out when unused.
                    (tg != 0).then_some((ch, tg))
                },
            },
            // Explicit update: separate transmit and receive channels.
            0x03 => TsbkEvent::GroupVoiceUpdate {
                first: (((a >> 32) & 0xFFFF) as u16, (a & 0xFFFF) as u16),
                second: None,
            },
            // Individual Unit-to-Unit Voice Grant / Update
            0x04 | 0x06 => TsbkEvent::IndividualVoiceGrant {
                channel: ((a >> 40) & 0xFFFF) as u16,
                target: ((a >> 24) & 0xFFFF) as u32,
                source: (a & 0xFF_FFFF) as u32,
            },
            // Group Affiliation Response: result, announced group, selected
            // group and the target subscriber. This used to be mislabeled as
            // a patch message, which made ordinary affiliations look like
            // dynamic regrouping in downstream telemetry.
            0x28 => TsbkEvent::GroupAffiliation {
                result: ((a >> 56) & 0x03) as u8,
                announced_group: ((a >> 40) & 0xFFFF) as u16,
                group: ((a >> 24) & 0xFFFF) as u16,
                target: (a & 0xFF_FFFF) as u32,
            },
            // Secondary Control Channel Broadcast, explicit form. The second
            // channel field is the paired uplink, so only the downlink is a
            // control-channel candidate.
            0x29 => TsbkEvent::SecondaryControlChannel {
                rfss_id: ((a >> 56) & 0xFF) as u8,
                site_id: ((a >> 48) & 0xFF) as u8,
                first: (((a >> 32) & 0xFFFF) as u16, Some((a & 0xFF) as u8)),
                second: None,
            },
            // Secondary Control Channel Broadcast, implicit form: two
            // independent downlink channel/service-class pairs.
            0x39 => TsbkEvent::SecondaryControlChannel {
                rfss_id: ((a >> 56) & 0xFF) as u8,
                site_id: ((a >> 48) & 0xFF) as u8,
                first: (((a >> 32) & 0xFFFF) as u16, Some(((a >> 24) & 0xFF) as u8)),
                second: Some((((a >> 8) & 0xFFFF) as u16, Some((a & 0xFF) as u8))),
            },
            // Unit registration and deregistration responses are useful site
            // activity even though they do not represent voice calls.
            0x2C => TsbkEvent::UnitRegistration {
                result: ((a >> 60) & 0x03) as u8,
                sysid: ((a >> 48) & 0x0FFF) as u16,
                system_unit_id: ((a >> 24) & 0xFF_FFFF) as u32,
                source: (a & 0xFF_FFFF) as u32,
            },
            0x2F => TsbkEvent::UnitDeregistration {
                wacn: ((a >> 36) & 0xF_FFFF) as u32,
                sysid: ((a >> 24) & 0x0FFF) as u16,
                source: (a & 0xFF_FFFF) as u32,
            },
            // RFSS Site Status Broadcast for the site carrying this control
            // channel. Its field layout is distinct from adjacent-site status.
            0x3A => TsbkEvent::RfssSiteStatus {
                sysid: ((a >> 40) & 0x0FFF) as u16,
                rfss_id: ((a >> 32) & 0xFF) as u8,
                site_id: ((a >> 24) & 0xFF) as u8,
                channel: ((a >> 8) & 0xFFFF) as u16,
                service_class: (a & 0xFF) as u8,
            },
            // Adjacent Site Status Broadcast.
            0x3C => TsbkEvent::AdjacentSiteStatus {
                rfss_id: ((a >> 32) & 0xFF) as u8,
                site_id: ((a >> 24) & 0xFF) as u8,
                channel: ((a >> 8) & 0xFFFF) as u16,
                service_class: (a & 0xFF) as u8,
            },
            // Network Status Broadcast: carries the WACN and system ID a Phase 2
            // voice channel needs to seed its descrambler. Layout of the 64-bit
            // args: LRA(8), WACN(20), SYSID(12), channel(16), service class(8).
            0x3B => TsbkEvent::NetStatus {
                wacn: ((a >> 36) & 0xF_FFFF) as u32,
                sysid: ((a >> 24) & 0xFFF) as u16,
                channel: ((a >> 8) & 0xFFFF) as u16,
                service_class: (a & 0xFF) as u8,
            },
            // Band plan announcements. All three share a layout except for the
            // four bits after the identifier, which this does not depend on.
            0x33 | 0x34 | 0x3D => TsbkEvent::IdenUpdate {
                id: ((a >> 60) & 0xF) as u8,
                // Base frequency counts in 5 Hz units, spacing in 125 Hz.
                base_hz: ((a & 0xFFFF_FFFF) as f64) * 5.0,
                spacing_hz: (((a >> 32) & 0x3FF) as f64) * 125.0,
                tdma: self.opcode == 0x33,
            },
            _ => TsbkEvent::Other {
                opcode: self.opcode,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TsbkEvent {
    /// A talkgroup was granted a voice channel — someone is about to talk.
    GroupVoiceGrant {
        /// Raw service-options octet: emergency, encryption and priority.
        service_options: u8,
        channel: u16,
        talkgroup: u16,
        source: u32,
    },
    /// An individual radio was granted a unit-to-unit voice channel.
    IndividualVoiceGrant {
        channel: u16,
        target: u32,
        source: u32,
    },
    /// Harris working-group regroup/patch encryption command (MFID A4).
    HarrisRegroupEncryption {
        options: u8,
        supergroup: u16,
        key_id: u16,
        address: u32,
    },
    /// A subscriber's group-affiliation response.
    GroupAffiliation {
        result: u8,
        announced_group: u16,
        group: u16,
        target: u32,
    },
    /// Secondary control channel candidate frequency announcement.
    SecondaryControlChannel {
        rfss_id: u8,
        site_id: u8,
        first: (u16, Option<u8>),
        second: Option<(u16, Option<u8>)>,
    },
    /// Status for the site carrying the current control channel.
    RfssSiteStatus {
        sysid: u16,
        rfss_id: u8,
        site_id: u8,
        channel: u16,
        service_class: u8,
    },
    /// Adjacent-site status announcement.
    AdjacentSiteStatus {
        rfss_id: u8,
        site_id: u8,
        channel: u16,
        service_class: u8,
    },
    UnitRegistration {
        result: u8,
        sysid: u16,
        system_unit_id: u32,
        source: u32,
    },
    UnitDeregistration {
        wacn: u32,
        sysid: u16,
        source: u32,
    },
    /// A reminder that talkgroups are still up on their channels.
    GroupVoiceUpdate {
        first: (u16, u16),
        second: Option<(u16, u16)>,
    },
    /// The network identity: WACN and system ID, which seed the Phase 2
    /// voice-channel descrambler.
    NetStatus {
        wacn: u32,
        sysid: u16,
        channel: u16,
        service_class: u8,
    },
    /// A band plan entry: how to turn channel numbers into frequencies.
    IdenUpdate {
        id: u8,
        base_hz: f64,
        spacing_hz: f64,
        /// True for the TDMA variant, which a Phase II system announces.
        tdma: bool,
    },
    Other {
        opcode: u8,
    },
}

/// Timeslots each TDMA carrier is shared between. Two, for P25 Phase II.
const SLOTS_PER_TDMA_CARRIER: u16 = 2;

/// The band plan a site announces, and the only way to turn a grant's channel
/// number into a frequency to tune.
///
/// Without this a grant says "talkgroup 401 is on channel 0x1234", which is not
/// something a receiver can act on. Sites repeat these announcements
/// continuously, so the plan fills in within seconds of locking.
#[derive(Clone, Debug, Default)]
pub struct ChannelPlan {
    entries: std::collections::BTreeMap<u8, (f64, f64, bool)>,
}

impl ChannelPlan {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, event: &TsbkEvent) {
        if let TsbkEvent::IdenUpdate {
            id,
            base_hz,
            spacing_hz,
            tdma,
        } = event
        {
            // A spacing of zero would map every channel to the base frequency.
            if *spacing_hz > 0.0 && *base_hz > 0.0 {
                self.entries.insert(*id, (*base_hz, *spacing_hz, *tdma));
            }
        }
    }

    /// Frequency for a 16-bit channel field, if its band plan is known.
    ///
    /// On a **TDMA band the channel number counts timeslots, not carriers**, so
    /// it must be divided by the slots per carrier before the spacing is
    /// applied. Treating a Phase II channel as FDMA puts every voice frequency
    /// wrong — measured against the air, grants pointed at 851.5375 MHz where
    /// the carrier was actually at 851.2750, and the same factor of two held
    /// across every channel checked.
    pub fn frequency(&self, channel: u16) -> Option<f64> {
        let (id, number) = Tsbk::split_channel(channel);
        let (base, spacing, tdma) = self.entries.get(&id)?;
        let carrier = if *tdma {
            number / SLOTS_PER_TDMA_CARRIER
        } else {
            number
        };
        Some(base + f64::from(carrier) * spacing)
    }

    /// Which timeslot a TDMA channel refers to, or `None` on an FDMA band.
    pub fn slot(&self, channel: u16) -> Option<u8> {
        let (id, number) = Tsbk::split_channel(channel);
        let (_, _, tdma) = self.entries.get(&id)?;
        tdma.then(|| (number % SLOTS_PER_TDMA_CARRIER) as u8)
    }

    /// Whether a channel's band was announced as TDMA.
    ///
    /// A Phase II system announces both kinds. A TDMA voice channel carries no
    /// C4FM at all, so failing to decode one is expected rather than a fault.
    pub fn is_tdma(&self, channel: u16) -> Option<bool> {
        let (id, _) = Tsbk::split_channel(channel);
        self.entries.get(&id).map(|(_, _, t)| *t)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (u8, f64, f64, bool)> + '_ {
        self.entries.iter().map(|(id, (b, s, t))| (*id, *b, *s, *t))
    }
}

/// The standard's name for an opcode, for display.
pub fn opcode_name(op: u8) -> &'static str {
    match op {
        0x00 => "GRP_V_CH_GRANT",
        0x02 => "GRP_V_CH_GRANT_UPDT",
        0x03 => "GRP_V_CH_GRANT_UPDT_EXP",
        0x04 => "UU_V_CH_GRANT",
        0x05 => "UU_ANS_REQ",
        0x06 => "UU_V_CH_GRANT_UPDT",
        0x08 => "TELE_INT_CH_GRANT",
        0x09 => "TELE_INT_CH_GRANT_UPDT",
        0x0A => "TELE_INT_ANS_REQ",
        0x14 => "SNDCP_CH_GRANT",
        0x15 => "SNDCP_DATA_PAGE_REQ",
        0x16 => "SNDCP_DATA_CH_ANN_EXP",
        0x18 => "STS_UPDT",
        0x1A => "STS_Q",
        0x1C => "MSG_UPDT",
        0x1F => "CALL_ALRT",
        0x20 => "ACK_RSP_FNE",
        0x21 => "QUE_RSP",
        0x24 => "EXT_FNCT_CMD",
        0x27 => "DENY_RSP",
        0x28 => "GRP_AFF_RSP",
        0x29 => "SCCB_EXP",
        0x2A => "GRP_AFF_Q",
        0x2B => "LOC_REG_RSP",
        0x2C => "U_REG_RSP",
        0x2D => "U_REG_CMD",
        0x2F => "U_DE_REG_ACK",
        0x30 => "SYNC_BCST",
        0x33 => "IDEN_UP_TDMA",
        0x34 => "IDEN_UP_VU",
        0x36 => "ROAM_ADDR_CMD",
        0x38 => "SYS_SRV_BCST",
        0x39 => "SCCB",
        0x3A => "RFSS_STS_BCST",
        0x3B => "NET_STS_BCST",
        0x3C => "ADJ_STS_BCST",
        0x3D => "IDEN_UP",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schedule must be a permutation, which is what makes deinterleaving
    /// invertible at all.
    #[test]
    fn the_interleave_schedule_is_a_permutation() {
        let s = interleave_schedule();
        let mut seen = [false; TSBK_BITS];
        for &v in &s {
            assert!(v < TSBK_BITS, "index {v} out of range");
            assert!(!seen[v], "index {v} appears twice");
            seen[v] = true;
        }
        assert!(seen.iter().all(|&v| v));
    }

    #[test]
    fn deinterleaving_inverts_itself() {
        let bits: Vec<u8> = (0..TSBK_BITS).map(|i| (i % 3 == 0) as u8).collect();
        let there = deinterleave(&bits, true);
        let back = deinterleave(&there, false);
        assert_eq!(back, bits);
    }

    /// Encode with the constellation table, then decode: the round trip proves
    /// the Viterbi traceback recovers the message it was given.
    fn trellis_encode(msg: &[u8]) -> Vec<u8> {
        let mut state = 0usize;
        let mut out = Vec::with_capacity(TSBK_BITS);
        let mut dibits: Vec<u8> = msg.chunks(2).map(|c| (c[0] << 1) | c[1]).collect();
        dibits.push(0); // flush back to state zero
        for &d in &dibits {
            let nibble = CONSTELLATION[state][d as usize];
            for k in (0..4).rev() {
                out.push((nibble >> k) & 1);
            }
            state = d as usize;
        }
        out
    }

    fn message(seed: u32) -> Vec<u8> {
        let mut s = seed;
        (0..TSBK_DECODED_BITS)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                ((s >> 20) & 1) as u8
            })
            .collect()
    }

    #[test]
    fn the_trellis_round_trips_a_clean_block() {
        for seed in [1u32, 7, 4242] {
            let msg = message(seed);
            let coded = trellis_encode(&msg);
            assert_eq!(coded.len(), TSBK_BITS);
            assert_eq!(trellis_decode(&coded), msg, "seed {seed}");
        }
    }

    /// The code exists to survive symbol errors, so it must.
    #[test]
    fn the_trellis_corrects_scattered_errors() {
        let msg = message(11);
        let mut coded = trellis_encode(&msg);
        for k in [5usize, 41, 97, 150] {
            coded[k] ^= 1;
        }
        assert_eq!(trellis_decode(&coded), msg);
    }

    #[test]
    fn soft_metrics_agree_with_the_hard_decoder_on_a_clean_block() {
        let msg = message(19);
        let coded = trellis_encode(&msg);
        let soft: Vec<f32> = coded
            .iter()
            .map(|&b| if b == 1 { 0.95 } else { 0.05 })
            .collect();
        assert_eq!(trellis_decode_soft(&soft), msg);
    }

    /// A couple of symbols sitting near a decision boundary should still
    /// decode when the rest of the block is clean — the case hard slicing
    /// loses and soft metrics exist for.
    #[test]
    fn soft_metrics_survive_symbols_near_a_threshold() {
        let conv = Convention::default();
        let mut msg = vec![0u8; TSBK_DECODED_BITS];
        msg[0] = 1;
        let crc = crc16(&msg[..80], conv.crc_init_ones);
        for k in 0..16 {
            msg[80 + k] = ((crc >> (15 - k)) & 1) as u8;
        }
        let coded = trellis_encode(&msg);
        let interleaved = deinterleave(&coded, false);
        let mut hz: Vec<f32> = interleaved
            .chunks(2)
            .map(|c| crate::p25::dibit_level((c[0] << 1) | c[1]))
            .collect();
        // Pull two symbols halfway toward their neighbour.
        hz[3] *= 0.4;
        hz[17] *= 0.4;
        let tsbk = Tsbk::decode_soft(&hz, crate::p25::DEV_OUTER_HZ, conv).expect("decode");
        assert!(
            tsbk.crc_ok,
            "soft decode lost a block that was only barely faded"
        );
    }

    #[test]
    fn the_crc_detects_a_flipped_bit() {
        let msg = message(3);
        let good = crc16(&msg[..80], false);
        let mut bad = msg.clone();
        bad[17] ^= 1;
        assert_ne!(crc16(&bad[..80], false), good);
        // The two initialisations must genuinely differ, or offering both is
        // pointless.
        assert_ne!(crc16(&msg[..80], true), good);
    }

    /// A whole block through interleave, trellis and CRC, as it travels.
    #[test]
    fn a_block_survives_the_full_round_trip() {
        let conv = Convention::default();
        // Build a grant: opcode 0x00, MFID 0, known talkgroup and source.
        let mut msg = vec![0u8; TSBK_DECODED_BITS];
        msg[0] = 1; // last block
        let opcode: u8 = 0x00;
        for k in 0..6 {
            msg[2 + k] = (opcode >> (5 - k)) & 1;
        }
        let args: u64 = (0xC3u64 << 56) | (0x1234u64 << 40) | (0x0426u64 << 24) | 0x00_1F41;
        for k in 0..64 {
            msg[16 + k] = ((args >> (63 - k)) & 1) as u8;
        }
        let crc = crc16(&msg[..80], conv.crc_init_ones);
        for k in 0..16 {
            msg[80 + k] = ((crc >> (15 - k)) & 1) as u8;
        }

        let coded = trellis_encode(&msg);
        let interleaved = deinterleave(&coded, false);
        let dibits: Vec<u8> = interleaved.chunks(2).map(|c| (c[0] << 1) | c[1]).collect();

        let tsbk = Tsbk::decode(&dibits, conv).expect("decode");
        assert!(tsbk.crc_ok, "CRC failed on a block we built ourselves");
        assert!(tsbk.last);
        assert_eq!(tsbk.opcode, 0x00);
        assert_eq!(tsbk.mfid, 0);
        assert_eq!(
            tsbk.event(),
            TsbkEvent::GroupVoiceGrant {
                service_options: 0xC3,
                channel: 0x1234,
                talkgroup: 0x0426,
                source: 0x1F41,
            }
        );
        assert_eq!(Tsbk::split_channel(0x1234), (1, 0x234));
    }

    #[test]
    fn manufacturer_blocks_are_not_misread_as_grants() {
        let t = Tsbk {
            last: true,
            protected: false,
            opcode: 0x00,
            mfid: 0x90,
            args: 0,
            crc_ok: true,
        };
        assert_eq!(t.event(), TsbkEvent::Other { opcode: 0x00 });
    }

    #[test]
    fn motorola_regroup_grants_and_harris_encryption_commands_decode() {
        let moto = Tsbk {
            last: true,
            protected: false,
            opcode: 0x02,
            mfid: 0x90,
            args: (0x43u64 << 56) | (0x1234u64 << 40) | (0x4567u64 << 24) | 0x12_3456,
            crc_ok: true,
        };
        assert_eq!(
            moto.event(),
            TsbkEvent::GroupVoiceGrant {
                service_options: 0x43,
                channel: 0x1234,
                talkgroup: 0x4567,
                source: 0x12_3456,
            }
        );
        let harris = Tsbk {
            last: true,
            protected: false,
            opcode: 0x30,
            mfid: 0xa4,
            args: (0xa1u64 << 56) | (0x2345u64 << 40) | (0x6789u64 << 24) | 0xab_cdef,
            crc_ok: true,
        };
        assert_eq!(
            harris.event(),
            TsbkEvent::HarrisRegroupEncryption {
                options: 0xa1,
                supergroup: 0x2345,
                key_id: 0x6789,
                address: 0xab_cdef,
            }
        );
    }

    /// The band plan is what makes a grant actionable, so the arithmetic that
    /// turns a channel number into a frequency has to be right.
    #[test]
    fn the_band_plan_turns_channels_into_frequencies() {
        // A 700 MHz entry: base 851.00625 MHz in 5 Hz units, 6.25 kHz spacing
        // in 125 Hz units, identifier 1.
        let base_units = (851_006_250f64 / 5.0) as u64;
        let spacing_units = (6_250f64 / 125.0) as u64;
        let args = (1u64 << 60) | (spacing_units << 32) | base_units;
        let t = Tsbk {
            last: true,
            protected: false,
            opcode: 0x3D,
            mfid: 0,
            args,
            crc_ok: true,
        };
        let ev = t.event();
        let mut plan = ChannelPlan::new();
        plan.observe(&ev);
        assert_eq!(plan.len(), 1);
        // This band is FDMA, so numbers count carriers directly.
        // Channel 0x1000 is identifier 1, number 0 — the base itself.
        let f0 = plan.frequency(0x1000).unwrap();
        assert!((f0 - 851_006_250.0).abs() < 1.0, "got {f0}");
        // Number 16 is sixteen steps up.
        let f16 = plan.frequency(0x1010).unwrap();
        assert!(
            (f16 - (851_006_250.0 + 16.0 * 6250.0)).abs() < 1.0,
            "got {f16}"
        );
        // An identifier never announced has no answer, rather than a wrong one.
        assert!(plan.frequency(0x7000).is_none());
    }

    /// The regression that only an off-air measurement could have found: TDMA
    /// channel numbers count slots, so two consecutive numbers share a carrier.
    ///
    /// The expected values are the carriers actually measured on CLMRN band 2 —
    /// 851.2750, 851.6375 and 852.1375 MHz — where the FDMA reading had pointed
    /// at 851.5375 and beyond and found nothing.
    ///
    /// Consecutive channel numbers map to the *same* frequency and differ only
    /// by slot, which is what sharing a carrier means. Each carrier showed up
    /// across two adjacent 12.5 kHz analysis bins because it is itself 12.5 kHz
    /// wide, which briefly looked like two carriers.
    #[test]
    fn tdma_channel_numbers_count_slots_not_carriers() {
        let mut plan = ChannelPlan::new();
        plan.observe(&TsbkEvent::IdenUpdate {
            id: 2,
            base_hz: 851_012_500.0,
            spacing_hz: 12_500.0,
            tdma: true,
        });
        for (channel_number, want_hz, want_slot) in [
            (42u16, 851_275_000.0, 0u8),
            (43, 851_275_000.0, 1),
            (100, 851_637_500.0, 0),
            (101, 851_637_500.0, 1),
            (180, 852_137_500.0, 0),
        ] {
            let ch = (2u16 << 12) | channel_number;
            let got = plan.frequency(ch).unwrap();
            assert!(
                (got - want_hz).abs() < 1.0,
                "channel {channel_number}: got {got:.0} Hz, expected {want_hz:.0}"
            );
            assert_eq!(plan.slot(ch), Some(want_slot));
        }
    }

    /// An FDMA band must keep counting carriers, or the fix breaks Phase 1.
    #[test]
    fn fdma_channel_numbers_still_count_carriers() {
        let mut plan = ChannelPlan::new();
        plan.observe(&TsbkEvent::IdenUpdate {
            id: 1,
            base_hz: 851_006_250.0,
            spacing_hz: 6_250.0,
            tdma: false,
        });
        let ch = (1u16 << 12) | 16;
        assert!((plan.frequency(ch).unwrap() - (851_006_250.0 + 16.0 * 6250.0)).abs() < 1.0);
        assert_eq!(plan.slot(ch), None);
    }

    #[test]
    fn new_tsbk_opcodes_decode_events() {
        // Individual Grant (0x04)
        let t_ind = Tsbk {
            last: true,
            protected: false,
            opcode: 0x04,
            mfid: 0,
            args: (0x1234u64 << 40) | (0x0000_0111u64 << 24) | 0x0000_0222,
            crc_ok: true,
        };
        assert_eq!(
            t_ind.event(),
            TsbkEvent::IndividualVoiceGrant {
                channel: 0x1234,
                target: 0x111,
                source: 0x222
            }
        );

        // 0x08 is telephone interconnect, not a special emergency grant.
        let t_emg = Tsbk {
            last: true,
            protected: false,
            opcode: 0x08,
            mfid: 0,
            args: (0x5678u64 << 40) | (0x03E8u64 << 24) | 0x0000_0999,
            crc_ok: true,
        };
        assert_eq!(t_emg.event(), TsbkEvent::Other { opcode: 0x08 });

        // 0x18 is a status update, not a patch mapping.
        let t_patch = Tsbk {
            last: true,
            protected: false,
            opcode: 0x18,
            mfid: 0,
            args: (0x07D0u64 << 40) | (0x07D1u64 << 24),
            crc_ok: true,
        };
        assert_eq!(t_patch.event(), TsbkEvent::Other { opcode: 0x18 });

        // Secondary CC (0x29)
        let t_scc = Tsbk {
            last: true,
            protected: false,
            opcode: 0x29,
            mfid: 0,
            args: (0x01u64 << 56) | (0x02u64 << 48) | (0x1000u64 << 32) | 0xA5,
            crc_ok: true,
        };
        assert_eq!(
            t_scc.event(),
            TsbkEvent::SecondaryControlChannel {
                rfss_id: 1,
                site_id: 2,
                first: (0x1000, Some(0xA5)),
                second: None,
            }
        );
    }

    /// A malformed announcement must not poison the plan: a zero spacing would
    /// map every channel in that band to the same frequency.
    #[test]
    fn a_degenerate_band_plan_entry_is_ignored() {
        let mut plan = ChannelPlan::new();
        plan.observe(&TsbkEvent::IdenUpdate {
            id: 2,
            base_hz: 851_000_000.0,
            spacing_hz: 0.0,
            tdma: false,
        });
        assert!(plan.is_empty());
    }

    #[test]
    fn site_and_unit_messages_keep_their_telemetry_fields() {
        let rfss = Tsbk {
            last: true,
            protected: false,
            opcode: 0x3A,
            mfid: 0,
            args: (0x321u64 << 40) | (3u64 << 32) | (7u64 << 24) | (0x1234u64 << 8) | 0xA5,
            crc_ok: true,
        };
        assert_eq!(
            rfss.event(),
            TsbkEvent::RfssSiteStatus {
                sysid: 0x321,
                rfss_id: 3,
                site_id: 7,
                channel: 0x1234,
                service_class: 0xA5,
            }
        );

        let net = Tsbk {
            last: true,
            protected: false,
            opcode: 0x3B,
            mfid: 0,
            args: (0xABCDEu64 << 36) | (0x321u64 << 24) | (0x2345u64 << 8) | 0x91,
            crc_ok: true,
        };
        assert_eq!(
            net.event(),
            TsbkEvent::NetStatus {
                wacn: 0xABCDE,
                sysid: 0x321,
                channel: 0x2345,
                service_class: 0x91,
            }
        );

        let registration = Tsbk {
            last: true,
            protected: false,
            opcode: 0x2C,
            mfid: 0,
            args: (2u64 << 60) | (0x321u64 << 48) | (0x123456u64 << 24) | 0x654321,
            crc_ok: true,
        };
        assert_eq!(
            registration.event(),
            TsbkEvent::UnitRegistration {
                result: 2,
                sysid: 0x321,
                system_unit_id: 0x123456,
                source: 0x654321,
            }
        );
    }

    #[test]
    fn a_grant_update_carries_two_pairs() {
        let t = Tsbk {
            last: true,
            protected: false,
            opcode: 0x02,
            mfid: 0,
            args: (0x1234u64 << 48) | (0x0191u64 << 32) | (0x1235u64 << 16) | 0x0192,
            crc_ok: true,
        };
        assert_eq!(
            t.event(),
            TsbkEvent::GroupVoiceUpdate {
                first: (0x1234, 0x0191),
                second: Some((0x1235, 0x0192)),
            }
        );
    }

    #[test]
    fn an_unused_second_pair_is_dropped() {
        let t = Tsbk {
            last: true,
            protected: false,
            opcode: 0x02,
            mfid: 0,
            args: (0x1234u64 << 48) | (0x0191u64 << 32),
            crc_ok: true,
        };
        match t.event() {
            TsbkEvent::GroupVoiceUpdate { second, .. } => assert!(second.is_none()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn opcodes_have_names() {
        assert_eq!(opcode_name(0x00), "GRP_V_CH_GRANT");
        assert_eq!(opcode_name(0x3D), "IDEN_UP");
        assert_eq!(opcode_name(0xFE), "?");
    }
}
