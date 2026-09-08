//! Multi-device SDR management for scannerd.
//!
//! One [`Device`] owns one dongle and one worker thread; [`DeviceManager`] owns
//! the set of them, keyed by serial. Samples reach consumers through each
//! device's [`Fanout`], never by handing out the device itself.

pub mod device;
pub mod driver_log;
pub mod fanout;

pub use device::{
    Capabilities, Cmd, Device, DeviceConfig, DeviceInfo, Event, GainControl, Role, State, cover_hz,
    cover_hz_offset, enumerate, open,
};
pub use fanout::{ContinuityTracker, Fanout, GapReport, IqBlock};

use anyhow::{Result, bail};
use std::collections::BTreeMap;

/// The set of open devices, keyed by serial.
#[derive(Default)]
pub struct DeviceManager {
    devices: BTreeMap<String, Device>,
}

impl DeviceManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a device and take ownership of it. Fails if that serial is already
    /// open — two owners of one dongle is always a bug, and the driver-level
    /// symptom (a confusing busy error from deep inside libusb) is much harder
    /// to read than this one.
    pub fn add(&mut self, cfg: &DeviceConfig) -> Result<&Device> {
        if self.devices.contains_key(&cfg.serial) {
            bail!("device {} is already open", cfg.serial);
        }
        let dev = device::open(cfg)?;
        Ok(self.devices.entry(cfg.serial.clone()).or_insert(dev))
    }

    pub fn get(&self, serial: &str) -> Option<&Device> {
        self.devices.get(serial)
    }

    pub fn remove(&mut self, serial: &str) -> Option<Device> {
        self.devices.remove(serial)
    }

    pub fn serials(&self) -> impl Iterator<Item = &str> {
        self.devices.keys().map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Device> {
        self.devices.values()
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// The first idle device, for jobs that do not care which dongle they get.
    pub fn first_with_role(&self, role: &Role) -> Option<&Device> {
        self.devices.values().find(|d| &d.role == role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_manager_is_empty() {
        let m = DeviceManager::new();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        assert!(m.get("00000003").is_none());
    }
}
