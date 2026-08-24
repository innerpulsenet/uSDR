//! Per-device calibration, persisted between runs.
//!
//! Real TOML through serde, deliberately: hfscan hand-parsed its config and
//! ended up understanding exactly two keys, which is a wart worth not
//! inheriting. This file holds only what must be *measured* rather than chosen —
//! frequency error and which feedline a dongle is on. Systems, channels and
//! talkgroups are a separate concern and get their own file.
//!
//! Copied from `scannerd-store` so this tool does not inherit that crate's
//! SQLite and Opus dependencies. Kept verbatim apart from the config path, so
//! re-syncing it upstream stays a straight copy — which is why the
//! calibration-writing half is allowed to sit here unused: the inspector only
//! ever reads a ppm value.
#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default, rename = "device")]
    pub devices: Vec<DeviceCal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCal {
    pub serial: String,
    /// Tuner reported by the driver, recorded so a swapped dongle is obvious.
    #[serde(default)]
    pub tuner: String,
    /// Measured frequency error, in parts per million.
    #[serde(default)]
    pub ppm: f64,
    /// Group label shared by dongles found to be on the same feedline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub antenna: Option<String>,
}

impl DeviceCal {
    pub fn new(serial: impl Into<String>, tuner: impl Into<String>) -> Self {
        Self {
            serial: serial.into(),
            tuner: tuner.into(),
            ppm: 0.0,
            antenna: None,
        }
    }
}

pub fn path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/usdr/devices.toml"))
}

pub fn load() -> Result<Config> {
    let p = path()?;
    if !p.exists() {
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))
}

pub fn save(cfg: &Config) -> Result<PathBuf> {
    let p = path()?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let text = toml::to_string_pretty(cfg).context("serialising config")?;
    std::fs::write(&p, text).with_context(|| format!("writing {}", p.display()))?;
    Ok(p)
}

impl Config {
    /// Find a device's entry, creating it if this is the first time we have seen
    /// that serial.
    pub fn entry(&mut self, serial: &str, tuner: &str) -> &mut DeviceCal {
        if let Some(i) = self.devices.iter().position(|d| d.serial == serial) {
            // A dongle can be replaced while keeping its serial slot; keep the
            // tuner field honest rather than trusting what was written before.
            self.devices[i].tuner = tuner.to_string();
            return &mut self.devices[i];
        }
        self.devices.push(DeviceCal::new(serial, tuner));
        self.devices.last_mut().expect("just pushed")
    }

    pub fn ppm_for(&self, serial: &str) -> f64 {
        self.devices
            .iter()
            .find(|d| d.serial == serial)
            .map_or(0.0, |d| d.ppm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_device_calibrates_as_zero_rather_than_failing() {
        let c = Config::default();
        assert_eq!(c.ppm_for("00000003"), 0.0);
    }

    #[test]
    fn entries_are_created_once_and_then_updated() {
        let mut c = Config::default();
        c.entry("00000003", "R820T").ppm = 2.54;
        c.entry("00000003", "R820T").antenna = Some("a".into());
        assert_eq!(c.devices.len(), 1);
        assert_eq!(c.ppm_for("00000003"), 2.54);
        assert_eq!(c.devices[0].antenna.as_deref(), Some("a"));
    }

    #[test]
    fn a_replaced_dongle_updates_its_recorded_tuner() {
        let mut c = Config::default();
        c.entry("00000001", "E4000").ppm = 0.8;
        c.entry("00000001", "R820T");
        assert_eq!(c.devices.len(), 1);
        assert_eq!(c.devices[0].tuner, "R820T");
        assert_eq!(
            c.devices[0].ppm, 0.8,
            "calibration survives a tuner correction"
        );
    }

    #[test]
    fn config_survives_a_round_trip() {
        let mut c = Config::default();
        c.entry("00000003", "R820T").ppm = 2.54;
        c.entry("00000003", "R820T").antenna = Some("preamp".into());
        c.entry("00000002", "R820T").ppm = 2.56;
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.devices.len(), 2);
        assert_eq!(back.ppm_for("00000002"), 2.56);
        assert_eq!(back.devices[0].antenna.as_deref(), Some("preamp"));
    }
}
