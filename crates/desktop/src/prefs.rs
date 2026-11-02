//! What this desktop remembers between runs. Receiver state lives on the
//! server; this is only how this operator likes to look at it.

use crate::spectrum::View;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Memory {
    pub name: String,
    pub freq_hz: f64,
    pub mode: usdr_proto::Mode,
}

/// The web page's eight defaults, offered until the server has a bank.
pub fn default_memories() -> Vec<Memory> {
    use usdr_proto::Mode::*;
    [
        ("AIR", 121_500_000.0, Am),
        ("2M", 146_520_000.0, Nfm),
        ("MARINE", 156_800_000.0, Nfm),
        ("WX", 162_550_000.0, Nfm),
        ("70CM", 446_000_000.0, Nfm),
        ("PAGER", 929_612_500.0, Pager),
        ("TRUNK", 851_012_500.0, P25),
        ("FM BC", 98_100_000.0, Wfm),
    ]
    .into_iter()
    .map(|(n, f, m)| Memory { name: n.into(), freq_hz: f, mode: m })
    .collect()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Motion {
    /// Follow the desktop's animation setting.
    #[default]
    System,
    Full,
    Reduced,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub servers: Vec<String>,
    pub last_server: Option<String>,
    pub auto_connect: bool,
    pub audio_device: Option<String>,
    pub volume: f32,
    pub muted: bool,
    pub pan: f32,
    pub view: View,
    /// A fixed tuning step; `None` follows the mode's raster.
    pub step_hz: Option<f64>,
    pub motion: Motion,
    pub ground: crate::theme::Ground,
    pub record_dir: Option<PathBuf>,
    pub side_width: f32,
    pub dock_height: f32,
    pub show_side: bool,
    pub show_dock: bool,
    pub show_scope: bool,
    /// Keyboard tuning shortcuts; off by default so keys stay with widgets.
    pub shortcuts: bool,
    /// The last mode that was not SCAN, for the scan panel's STOP.
    pub last_voice_mode: usdr_proto::Mode,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            servers: vec!["127.0.0.1:8073".into()],
            last_server: None,
            auto_connect: true,
            audio_device: None,
            volume: 0.8,
            muted: false,
            pan: 0.0,
            view: View::default(),
            step_hz: None,
            motion: Motion::System,
            ground: crate::theme::Ground::Carbon,
            record_dir: None,
            side_width: 318.0,
            dock_height: 230.0,
            show_side: true,
            show_dock: true,
            show_scope: true,
            shortcuts: true,
            last_voice_mode: usdr_proto::Mode::Nfm,
        }
    }
}

pub fn dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("net", "innerpulse", "usdr-desktop")
}

fn path() -> Option<PathBuf> {
    dirs().map(|d| d.config_dir().join("desktop.toml"))
}

impl Prefs {
    pub fn load() -> Prefs {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        match toml::to_string_pretty(self) {
            Ok(t) => {
                if let Err(e) = std::fs::write(&p, t) {
                    eprintln!("saving {}: {e}", p.display());
                }
            }
            Err(e) => eprintln!("serializing preferences: {e}"),
        }
    }

    pub fn record_dir(&self) -> PathBuf {
        self.record_dir.clone().unwrap_or_else(|| {
            directories::UserDirs::new()
                .and_then(|u| u.audio_dir().map(|a| a.to_path_buf()).or_else(|| Some(u.home_dir().join("Music"))))
                .unwrap_or_else(|| PathBuf::from("."))
                .join("uSDR")
        })
    }

    pub fn remember_server(&mut self, s: &str) {
        let s = s.trim().to_string();
        self.servers.retain(|x| x != &s);
        self.servers.insert(0, s.clone());
        self.servers.truncate(8);
        self.last_server = Some(s);
    }
}

/// Whether the desktop asked for less motion. KDE writes an animation speed
/// factor of 0 when animations are off; GNOME has a switch; Windows has a
/// system setting this does not read yet.
pub fn system_reduces_motion() -> bool {
    if let Some(home) = std::env::var_os("HOME") {
        let kde = PathBuf::from(home).join(".config/kdeglobals");
        if let Ok(text) = std::fs::read_to_string(kde) {
            for line in text.lines() {
                if let Some(v) = line.trim().strip_prefix("AnimationDurationFactor=") {
                    return v.trim().parse::<f32>().map(|f| f <= 0.0).unwrap_or(false);
                }
            }
        }
    }
    false
}
