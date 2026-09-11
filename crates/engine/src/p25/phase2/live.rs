//! Explicit single-frequency Phase 2 reception configuration.
use super::{DecodedBurst, Phase2Decoder, Phase2Receiver, logical_channel};
use num_complex::Complex32;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug)]
pub struct ReceiveConfig {
    pub frequency_hz: f64,
    pub wacn: u32,
    pub system_id: u16,
    pub nac: u16,
    /// Zero-based logical channel on the air.
    pub slot: u8,
}
impl std::str::FromStr for ReceiveConfig {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let fields: Vec<_> = value.split(',').map(str::trim).collect();
        if fields.len() != 5 {
            return Err("expected MHz,WACN,SYSID,NAC,SLOT (slot 0 or 1)");
        }
        let mhz: f64 = fields[0].parse().map_err(|_| "invalid Phase 2 frequency")?;
        fn number(s: &str) -> Result<u32, &'static str> {
            let (s, radix) = s.strip_prefix("0x").map_or((s, 10), |s| (s, 16));
            u32::from_str_radix(s, radix).map_err(|_| "invalid Phase 2 system number")
        }
        let (wacn, system_id, nac, slot) = (
            number(fields[1])?,
            number(fields[2])?,
            number(fields[3])?,
            number(fields[4])?,
        );
        if !mhz.is_finite()
            || mhz <= 0.0
            || mhz > 10000.0
            || wacn > 0xfffff
            || system_id > 0xfff
            || nac > 0xfff
            || slot > 1
        {
            return Err("Phase 2 frequency or system parameters out of range");
        }
        Ok(Self {
            frequency_hz: mhz * 1e6,
            wacn,
            system_id: system_id as u16,
            nac: nac as u16,
            slot: slot as u8,
        })
    }
}
static CHANNELS: OnceLock<Vec<ReceiveConfig>> = OnceLock::new();
pub fn install_channels(channels: Vec<ReceiveConfig>) -> Result<(), &'static str> {
    for (i, channel) in channels.iter().enumerate() {
        if channels[..i]
            .iter()
            .any(|c| (c.frequency_hz - channel.frequency_hz).abs() < 100.0)
        {
            return Err("configure only one Phase 2 timeslot per frequency");
        }
    }
    CHANNELS
        .set(channels)
        .map_err(|_| "Phase 2 channels already installed")
}

pub(crate) struct LiveReceiver {
    pub config: ReceiveConfig,
    receiver: Phase2Receiver,
    decoder: Phase2Decoder,
}
impl LiveReceiver {
    pub fn configured(frequency_hz: f64, fs: f64) -> Option<Self> {
        let config = *CHANNELS
            .get()?
            .iter()
            .find(|c| (c.frequency_hz - frequency_hz).abs() < 100.0)?;
        Some(Self::new(config, fs))
    }
    pub fn new(config: ReceiveConfig, fs: f64) -> Self {
        Self {
            config,
            receiver: Phase2Receiver::new(fs),
            decoder: Phase2Decoder::new(config.wacn, config.system_id, config.nac)
                .on_channel(config.frequency_hz),
        }
    }
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<DecodedBurst> {
        self.receiver
            .process(iq)
            .iter()
            .filter(|b| logical_channel(b.superframe_slot) == self.config.slot)
            .map(|b| self.decoder.decode(b))
            .collect()
    }
    pub fn locked(&self) -> bool {
        self.receiver.locked()
    }
    pub fn offset_hz(&self) -> f32 {
        self.receiver.offset_hz()
    }
    pub fn end_call(&mut self) {
        self.decoder = Phase2Decoder::new(self.config.wacn, self.config.system_id, self.config.nac)
            .on_channel(self.config.frequency_hz);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_bounded_explicit_system_parameters() {
        let c: ReceiveConfig = "851.0125,0xbee00,0x123,0x293,1".parse().unwrap();
        assert_eq!(c.slot, 1);
        assert_eq!(c.frequency_hz, 851012500.0);
        for bad in [
            "NaN,1,1,1,0",
            "851,0x100000,1,1,0",
            "851,1,1,1,2",
            "851,1,1,1",
        ] {
            assert!(bad.parse::<ReceiveConfig>().is_err());
        }
    }
}

#[cfg(test)]
pub(crate) fn test_iq(config: ReceiveConfig, key_id: u16) -> Vec<Complex32> {
    use super::{PAYLOAD_DIBITS, Scrambler, isch};
    let mut information = [0u8; 16];
    information[0] = 0x84 >> 2;
    information[1] = ((0x84 & 3) << 4) | (key_id >> 12) as u8;
    information[2] = ((key_id >> 6) & 63) as u8;
    information[3] = (key_id & 63) as u8;
    information[4..].fill(1);
    let ess = crate::p25::rs64::encode_for_test(&information, 28);
    let scrambler = Scrambler::new(config.wacn, config.system_id, config.nac);
    let mut dibits = Vec::new();
    for _ in 0..5 {
        for slot in 0..12 {
            let want = [0, 1, -2, -2, 4, 5, -2, -2, 8, 9, -2, -2][slot];
            let code = if want == -2 {
                isch::S_ISCH
            } else {
                let value = (0..128)
                    .find(|v| isch::IschInfo::from_value(*v).checkval() == want)
                    .unwrap();
                isch::codeword_for(value).unwrap()
            };
            dibits.extend((0..20).rev().map(|i| ((code >> (2 * i)) & 3) as u8));
            let mut payload = [0; PAYLOAD_DIBITS];
            let mut put = |at: usize, value: u8| {
                payload[at] = value >> 4;
                payload[at + 1] = (value >> 2) & 3;
                payload[at + 2] = value & 3;
            };
            let voice_id = slot / 2;
            if voice_id < 4 {
                for i in 0..4 {
                    put(74 + 3 * i, ess[voice_id * 4 + i]);
                }
            } else if voice_id == 4 {
                let mut at = 74;
                for i in 0..28 {
                    put(at, ess[16 + i]);
                    at += if i == 15 { 4 } else { 3 };
                }
            }
            for (i, v) in payload.iter_mut().enumerate() {
                *v = scrambler.apply(slot * 180 + 10 + i, *v);
            }
            let duid = if voice_id < 4 {
                0
            } else if voice_id == 4 {
                0x65
            } else {
                0xff
            };
            for (i, at) in [0, 37, 122, 159].into_iter().enumerate() {
                payload[at] = (duid >> (6 - 2 * i)) & 3;
            }
            dibits.extend(payload);
        }
    }
    super::tests::modulate(&dibits, 48_000.0)
}
