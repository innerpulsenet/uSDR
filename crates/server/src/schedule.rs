//! The monitoring schedule: what the receiver listens to when nobody is using it.
//!
//! There is one tuner and one decoder, so monitoring is time-shared, and this
//! decides the shares. It is off until the operator turns it on. Highest
//! priority first:
//!
//! 1. The operator. Touching the frequency, mode or span holds the schedule for
//!    [`HOLD_S`], with the radio left where they put it.
//! 2. A picture being received is never cut off.
//! 3. Timetabled radiofax charts, which start a minute early for the tone.
//! 4. A regular check on WWV. A stranded tuner hears nothing, which looks
//!    exactly like a quiet band, so a failed check marks the time since the last
//!    good one as unhealthy and stops the hopping until the radio passes again.
//! 5. Everything else, by share of time.
//!
//! Each mode has a share of the air and a dwell long enough to mean something:
//! two minutes of HFDL hears a ground station's squitters, while DSC and SELCAL
//! are so sparse that only long stays say anything. Within a mode the frequency
//! comes from what the station log measured at this hour of the day. A frequency
//! not yet tried scores better than one that has been tried and heard nothing, so
//! the list gets covered, and every fifth pick revisits the least-heard one so a
//! band that has come back is noticed.
//!
//! With wideband channels running, the shares are not used. The tuner holds a
//! slice of spectrum for a while, chosen by what those channels have measured at
//! this hour, and the channels decode every dial inside it. The main decoder is
//! left on the slice's best FT8 dial. It moves only for a radio check, which
//! takes WWV inside the slice when it can, or for a chart no channel can take.
//!
//! A timetabled chart whose frequency a radiofax channel covers inside the held
//! slice does not take the radio: the channel receives it, and the slice is kept
//! until the chart is over, so nothing moves the span under it. A slice choice
//! made shortly before such a chart takes the slice that holds it.
//!
//! Everything is clock-injected, like the scanner, so all of it is testable
//! without hardware or real time.

use crate::sdr::SdrMode;
use crate::stationlog::monitored_mode;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use usdr_proto::ScheduleStatus;

/// How long touching the radio holds the schedule.
pub const HOLD_S: u64 = 30 * 60;
const HEALTH_EVERY_S: u64 = 60 * 60;
/// How soon a radio that failed its check is tested again.
const HEALTH_RETRY_S: u64 = 10 * 60;
/// How long each WWV frequency gets to lock.
const HEALTH_STEP_S: u64 = 75;
/// Start this long before a chart, for its start tone and phasing.
const CHART_LEAD_S: i64 = 60;
/// A chart can still be caught this long after its time.
const CHART_LATE_S: i64 = 120;
/// The most a chart slot lasts if the picture's end is never seen.
const CHART_MAX_S: u64 = 16 * 60;
/// A chart that has not started by this long after its time is not coming.
const CHART_NO_START_S: u64 = 3 * 60;
/// How far a picture in progress may run past its slot.
const PICTURE_OVERRUN_S: u64 = 20 * 60;
/// Every this many picks of a mode, take the least-heard frequency instead.
const EXPLORE_EVERY: u32 = 5;
/// Where the span can sit: 160 to 40 m, 80 to 30 m, 20 to 15 m, 15 to 10 m. The
/// receiver's filters part at 12 MHz, so no slice crosses it.
const SLICE_CENTERS: [u64; 4] = [5_400_000, 6_855_000, 17_607_000, 24_620_000];
/// Below the filter split carries the night, above it the day.
const NIGHT_SLICES: [u64; 2] = [SLICE_CENTERS[0], SLICE_CENTERS[1]];
const DAY_SLICES: [u64; 2] = [SLICE_CENTERS[2], SLICE_CENTERS[3]];
/// How far from center a channel still decodes well, with the 8 MHz span.
const SLICE_REACH_HZ: f64 = 3_680_000.0;
/// The highest audio a channel listens to, above its dial.
const SLICE_TOP_AUDIO_HZ: f64 = 3_100.0;
/// How long the tuner stays on a slice before choosing again.
const SLICE_HOLD_S: u64 = 900;
/// Every this many choices, take the slice visited longest ago, so a band that
/// has come back is noticed.
const SLICE_EXPLORE_EVERY: u32 = 5;
/// Only charts whose title holds all of these are worth the air time.
const CHART_WORDS: [&str; 2] = ["SURFACE", "ANALYSIS"];
/// The radiofax stations in reach of Connecticut worth a slot.
const CHART_STATIONS: [&str; 3] = ["NMF", "NMG", "DDH3"];
/// Radiofax lists its assigned frequency; the dial sits this far below.
const WEFAX_OFFSET_HZ: f64 = 1_900.0;
/// DSC's tones are centered this far above the dial.
const DSC_OFFSET_HZ: f64 = 1_700.0;
/// HFDL ground stations on this side of the world.
const HFDL_STATIONS: [u64; 5] = [3, 4, 7, 11, 17];

const FT8_KHZ: [f64; 10] = [1840.0, 3573.0, 5357.0, 7074.0, 10136.0, 14074.0, 18100.0, 21074.0, 24915.0, 28074.0];
const FT4_KHZ: [f64; 8] = [3575.5, 7047.5, 10140.0, 14080.0, 18104.0, 21140.0, 24919.0, 28180.0];
const WSPR_KHZ: [f64; 10] = [1836.6, 3568.6, 5364.7, 7038.6, 10138.7, 14095.6, 18104.6, 21094.6, 24924.6, 28124.6];
/// The JS8Call defaults.
const JS8_KHZ: [f64; 9] = [1842.0, 3578.0, 7078.0, 10130.0, 14078.0, 18104.0, 21078.0, 24922.0, 28078.0];
const DSC_KHZ: [f64; 6] = [2187.5, 4207.5, 6312.0, 8414.5, 12577.0, 16804.5];
/// The North Atlantic and Caribbean voice families, as the desktop lists them.
const SELCAL_KHZ: [f64; 18] = [
    2887.0, 2899.0, 2962.0, 3016.0, 3476.0, 5550.0, 5598.0, 5616.0, 6577.0, 6622.0, 6628.0, 8825.0, 8831.0, 8846.0, 8864.0, 8906.0, 11396.0, 13306.0,
];
/// Sideband picture modes run upper sideband, so only the bands that use it.
const SSTV_KHZ: [f64; 3] = [14230.0, 21340.0, 28680.0];
/// SSTV's 40 m calling frequency, on the lower sideband: a channel demodulates
/// that, the main receiver's SSTV does not, so it is a channel's only.
const SSTV_LSB_KHZ: [f64; 1] = [7171.0];

struct Plan {
    mode: SdrMode,
    share: f64,
    dwell_s: u64,
}

const PLANS: [Plan; 6] = [
    Plan { mode: SdrMode::Ft8, share: 0.30, dwell_s: 120 },
    Plan { mode: SdrMode::Hfdl, share: 0.30, dwell_s: 120 },
    Plan { mode: SdrMode::Dsc, share: 0.15, dwell_s: 600 },
    Plan { mode: SdrMode::Wspr, share: 0.10, dwell_s: 360 },
    Plan { mode: SdrMode::Selcal, share: 0.10, dwell_s: 600 },
    Plan { mode: SdrMode::Sstv, share: 0.05, dwell_s: 480 },
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Target {
    pub mode: SdrMode,
    pub dial_hz: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct ChartFreq {
    dial_hz: u64,
    /// UTC minutes of the day this frequency is on; `None` is always.
    window: Option<(u16, u16)>,
}

#[derive(Clone, Debug, PartialEq)]
struct Chart {
    /// UTC minute of the day.
    minute: u16,
    station: String,
    title: String,
    freqs: Vec<ChartFreq>,
}

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    targets: Vec<Target>,
    charts: Vec<Chart>,
}

fn hhmm(s: &str) -> Option<u16> {
    if s.len() != 4 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: u16 = s.parse().ok()?;
    (v / 100 < 24 && v % 100 < 60).then_some(v / 100 * 60 + v % 100)
}

fn parse_window(s: &str) -> Option<(u16, u16)> {
    let (a, b) = s.split_once('-')?;
    Some((hhmm(a.trim())?, hhmm(b.trim())?))
}

fn in_window(window: Option<(u16, u16)>, minute: u16) -> bool {
    match window {
        None => true,
        Some((a, b)) if a <= b => (a..=b).contains(&minute),
        Some((a, b)) => minute >= a || minute <= b,
    }
}

impl Catalog {
    /// Build the lists from the frequency directory the desktop also uses.
    /// The timetabled chart a picture beginning at `utc` on `dial_hz` should
    /// be, as its station and title.
    pub fn chart_at(&self, dial_hz: u64, utc: u64) -> Option<(&str, &str)> {
        const DAY_MIN: i64 = 1_440;
        const WITHIN_MIN: i64 = 5;
        let minute = (utc % 86_400 / 60) as i64;
        let off = |c: &Chart| {
            let d = (minute - i64::from(c.minute)).rem_euclid(DAY_MIN);
            d.min(DAY_MIN - d)
        };
        self.charts
            .iter()
            .filter(|c| off(c) <= WITHIN_MIN && c.freqs.iter().any(|f| f.dial_hz.abs_diff(dial_hz) <= 100))
            .min_by_key(|c| off(c))
            .map(|c| (c.station.as_str(), c.title.as_str()))
    }

    /// The station whose chart frequencies include a dial.
    pub fn station_of(&self, dial_hz: u64) -> Option<&str> {
        self.charts.iter().find(|c| c.freqs.iter().any(|f| f.dial_hz.abs_diff(dial_hz) <= 100)).map(|c| c.station.as_str())
    }

    /// The catalog from the directory built into the server.
    pub fn builtin() -> Self {
        Self::from_directory(include_str!("../../desktop/data/hf-directory.json")).unwrap_or_else(|e| {
            eprintln!("monitoring schedule has no frequencies: {e:#}");
            Self::default()
        })
    }

    /// Every dial worth a wideband channel: the targets, each chart frequency,
    /// the NCDXF/IARU beacons and lower-sideband SSTV, which only channels run.
    pub fn channel_targets(&self) -> Vec<Target> {
        let mut out = self.targets.clone();
        let charts = self.charts.iter().flat_map(|c| &c.freqs).map(|f| Target { mode: SdrMode::Wefax, dial_hz: f.dial_hz });
        let beacons = scannerd_engine::beacon::BANDS_HZ.iter().map(|f| Target { mode: SdrMode::Beacon, dial_hz: f - crate::wideband::BEACON_PITCH_HZ });
        let sstv = SSTV_LSB_KHZ.iter().map(|k| Target { mode: SdrMode::Sstv, dial_hz: (k * 1e3).round() as u64 });
        for t in charts.chain(beacons).chain(sstv) {
            if !out.contains(&t) {
                out.push(t);
            }
        }
        out
    }

    pub fn from_directory(json: &str) -> Result<Self> {
        let d: Value = serde_json::from_str(json).context("reading the frequency directory")?;
        let mut targets = Vec::new();
        let mut add = |mode: SdrMode, hz: f64| {
            let t = Target { mode, dial_hz: hz.round() as u64 };
            if !targets.contains(&t) {
                targets.push(t);
            }
        };
        for (mode, list) in [
            (SdrMode::Ft8, &FT8_KHZ[..]),
            (SdrMode::Ft4, &FT4_KHZ[..]),
            (SdrMode::Wspr, &WSPR_KHZ[..]),
            (SdrMode::Js8, &JS8_KHZ[..]),
            (SdrMode::Selcal, &SELCAL_KHZ[..]),
            (SdrMode::Sstv, &SSTV_KHZ[..]),
        ] {
            for khz in list {
                add(mode, khz * 1e3);
            }
        }
        for khz in DSC_KHZ {
            add(SdrMode::Dsc, khz * 1e3 - DSC_OFFSET_HZ);
        }
        let empty = Vec::new();
        for gs in d["hfdl"].as_array().unwrap_or(&empty) {
            if gs["gs_id"].as_u64().is_some_and(|id| HFDL_STATIONS.contains(&id)) {
                for khz in gs["freqs_khz"].as_array().unwrap_or(&empty).iter().filter_map(Value::as_f64) {
                    add(SdrMode::Hfdl, khz * 1e3);
                }
            }
        }

        let mut charts = Vec::new();
        for station in d["wefax"].as_array().unwrap_or(&empty) {
            let call = station["call"].as_str().unwrap_or("");
            if !CHART_STATIONS.iter().any(|s| call.starts_with(s)) {
                continue;
            }
            let freqs: Vec<ChartFreq> = station["freq_windows_utc"]
                .as_array()
                .unwrap_or(&empty)
                .iter()
                .filter_map(|w| {
                    Some(ChartFreq { dial_hz: (w["khz"].as_f64()? * 1e3 - WEFAX_OFFSET_HZ).round() as u64, window: w["utc"].as_str().and_then(parse_window) })
                })
                .collect();
            for chart in station["schedule"].as_array().unwrap_or(&empty) {
                let title = chart["title"].as_str().unwrap_or("").to_string();
                let upper = title.to_ascii_uppercase();
                if let Some(minute) = chart["utc"].as_str().and_then(hhmm)
                    && CHART_WORDS.iter().all(|w| upper.contains(w))
                {
                    charts.push(Chart { minute, station: call.split(' ').next().unwrap_or(call).to_string(), title, freqs: freqs.clone() });
                }
            }
        }
        charts.sort_by_key(|c| c.minute);
        Ok(Catalog { targets, charts })
    }
}

/// What the log measured for one frequency in one hour of the day.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Yield {
    pub monitored_s: u64,
    pub observations: u64,
}

pub type Yields = BTreeMap<(String, u64, u8), Yield>;

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Planned,
    Chart { at_utc: u64, started: bool },
    Health { step: usize },
}

#[derive(Clone, Debug, PartialEq)]
struct Slot {
    kind: Kind,
    mode: SdrMode,
    dial_hz: u64,
    end: u64,
    label: String,
}

/// What the receiver is doing, read fresh each tick.
#[derive(Clone, Copy, Debug, Default)]
pub struct Inputs {
    pub utc: u64,
    /// A picture is being received.
    pub picture_busy: bool,
    /// The WWV decoder is locked.
    pub wwv_locked: bool,
    /// A wideband radiofax channel is in the middle of a chart.
    pub channel_chart_busy: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    /// Decode `mode` at `dial_hz`. With `center_hz` the span sits there (the
    /// wideband plan); without, the tuner moves only if the dial has left the span.
    Hop { mode: SdrMode, dial_hz: u64, center_hz: Option<u64> },
    /// The listening recorded from `since` to `until` was, or was not, on a
    /// working radio.
    MarkHealth { since: u64, until: u64, healthy: bool },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Health {
    #[default]
    Unknown,
    Good,
    Failing,
}

#[derive(Default)]
pub struct Scheduler {
    catalog: Catalog,
    yields: Yields,
    enabled: bool,
    hold_until: u64,
    slot: Option<Slot>,
    /// Scheduled seconds by mode this run, for the shares.
    used: BTreeMap<SdrMode, u64>,
    picks: BTreeMap<SdrMode, u32>,
    /// When each target was last chosen, so a revisit moves down the list
    /// between refreshes of the measurements.
    visited: BTreeMap<(SdrMode, u64), u64>,
    next_health: u64,
    last_check: u64,
    last_good: u64,
    health: Health,
    last_chart: u64,
    /// What wideband channels are decoding, by mode and dial. Empty means none.
    channels: BTreeSet<(SdrMode, u64)>,
    slice: Option<u64>,
    slice_until: u64,
    slice_picks: u32,
    slice_visited: BTreeMap<u64, u64>,
    /// The station's latitude and longitude, for where the sun is.
    home: Option<(f64, f64)>,
    /// The last hop sent, so the same one is not sent again: even onto the
    /// dial it is already on, a hop rebuilds the decoder and loses its slot.
    last_hop: Option<Action>,
    /// A chart a channel is receiving: the slice stays until then, or while
    /// the picture is still coming in.
    chart_pin_until: u64,
    /// Whether that holds now, and what the chart is.
    pinned: bool,
    channel_chart: Option<String>,
}

/// The middle of a Maidenhead square, latitude then longitude.
pub fn grid_center(grid: &str) -> Option<(f64, f64)> {
    let g = grid.trim().to_ascii_uppercase();
    let b = g.as_bytes();
    if b.len() < 4 || !(b'A'..=b'R').contains(&b[0]) || !(b'A'..=b'R').contains(&b[1]) || !b[2].is_ascii_digit() || !b[3].is_ascii_digit() {
        return None;
    }
    let lon = f64::from(b[0] - b'A') * 20.0 - 180.0 + f64::from(b[2] - b'0') * 2.0 + 1.0;
    let lat = f64::from(b[1] - b'A') * 10.0 - 90.0 + f64::from(b[3] - b'0') + 0.5;
    Some((lat, lon))
}

/// Whether the sun is up at a place. Accurate to a fraction of a degree, which is
/// far closer than the ionosphere cares about, and it follows the seasons.
fn sun_up(utc: u64, lat: f64, lon: f64) -> bool {
    let d = utc as f64 / 86_400.0 - 10_957.5;
    let g = (357.529 + 0.985_600_28 * d).to_radians();
    let q = 280.459 + 0.985_647_36 * d;
    let ecliptic = (q + 1.915 * g.sin() + 0.02 * (2.0 * g).sin()).to_radians();
    let tilt = 23.439_f64.to_radians();
    let declination = (tilt.sin() * ecliptic.sin()).asin();
    let ascension = (tilt.cos() * ecliptic.sin()).atan2(ecliptic.cos()).to_degrees();
    let greenwich = (18.697_374_558 + 24.065_709_824_419_08 * d) * 15.0;
    let hour_angle = (lon + greenwich - ascension).to_radians();
    lat.to_radians().sin() * declination.sin() + lat.to_radians().cos() * declination.cos() * hour_angle.cos() > 0.0
}

/// Whether a dial and the audio above it sit in the part of a slice that decodes well.
fn in_slice(center_hz: u64, dial_hz: u64) -> bool {
    let offset = dial_hz as f64 - center_hz as f64;
    offset.abs() < SLICE_REACH_HZ && (offset + SLICE_TOP_AUDIO_HZ).abs() < SLICE_REACH_HZ
}

impl Scheduler {
    pub fn new(catalog: Catalog, enabled: bool) -> Self {
        Scheduler { catalog, enabled, ..Default::default() }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Turn the schedule on or off. Off leaves the radio where it is.
    pub fn set_enabled(&mut self, on: bool, now: u64) {
        if on != self.enabled {
            self.enabled = on;
            self.slot = None;
            self.hold_until = 0;
            self.next_health = 0;
            self.last_check = now;
            self.health = Health::Unknown;
            self.last_hop = None;
        }
    }

    /// The operator touched the radio.
    pub fn operator(&mut self, now: u64) {
        if self.enabled {
            self.hold_until = now + HOLD_S;
            self.slot = None;
            self.last_hop = None;
        }
    }

    /// End a hold early.
    pub fn resume(&mut self) {
        self.hold_until = 0;
    }

    /// Where the station is, to tell its day from its night.
    pub fn set_home(&mut self, home: Option<(f64, f64)>) {
        self.home = home;
    }

    fn is_night(&self, now: u64) -> bool {
        match self.home {
            Some((lat, lon)) => !sun_up(now, lat, lon),
            None => matches!(hour_of(now), 0..=10 | 22 | 23),
        }
    }

    /// Tell the schedule what the wideband channels are decoding.
    pub fn set_channels(&mut self, channels: BTreeSet<(SdrMode, u64)>) {
        self.channels = channels;
    }

    pub fn set_yields(&mut self, yields: Yields) {
        self.yields = yields;
    }

    /// Whether the radio being on this frequency and mode is the schedule's doing.
    pub fn owns(&self, mode: SdrMode, dial_hz: f64) -> bool {
        self.slot.as_ref().is_some_and(|s| s.mode == mode && (s.dial_hz as f64 - dial_hz).abs() < 1.0)
    }

    pub fn tick(&mut self, n: Inputs) -> Vec<Action> {
        let mut out = Vec::new();
        if !self.enabled || n.utc < self.hold_until {
            self.slot = None;
            return out;
        }
        let overrun = n.channel_chart_busy && n.utc < self.chart_pin_until + PICTURE_OVERRUN_S;
        self.pinned = self.chart_pin_until > 0 && (n.utc < self.chart_pin_until || overrun);
        if !self.pinned {
            self.channel_chart = None;
        }
        self.chart_to_channel(n);
        if self.slot.is_some() && self.keep_slot(n, &mut out) {
            return out;
        }
        self.slot = None;
        self.begin_next(n.utc, &mut out);
        out
    }

    /// Whether the current slot carries on.
    fn keep_slot(&mut self, n: Inputs, out: &mut Vec<Action>) -> bool {
        let now = n.utc;
        let due_chart = self.chart_due(now).is_some();
        let slot = self.slot.as_mut().expect("a slot");
        match slot.kind {
            Kind::Health { step } => {
                if n.wwv_locked {
                    self.finish_health(now, true, out);
                    return false;
                }
                if now < slot.end {
                    return true;
                }
                let dials = health_dials(now, self.slice);
                if step + 1 < dials.len() {
                    // Another WWV frequency outside the slice would move the
                    // span from under a chart: try again once the chart is over.
                    if self.pinned && !self.slice.is_some_and(|c| in_slice(c, dials[step + 1])) {
                        self.next_health = self.chart_pin_until;
                        return false;
                    }
                    self.begin_health(now, step + 1, out);
                    return true;
                }
                self.finish_health(now, false, out);
                false
            }
            Kind::Chart { at_utc, ref mut started } => {
                if n.picture_busy {
                    *started = true;
                    return now < slot.end + PICTURE_OVERRUN_S;
                }
                if *started {
                    return false;
                }
                if now >= at_utc + CHART_NO_START_S {
                    return false;
                }
                now < slot.end
            }
            Kind::Planned => {
                if n.picture_busy && now < slot.end + PICTURE_OVERRUN_S {
                    return true;
                }
                now < slot.end && !due_chart
            }
        }
    }

    fn begin_next(&mut self, now: u64, out: &mut Vec<Action>) {
        if self.health == Health::Failing {
            if now >= self.next_health {
                self.begin_health(now, 0, out);
            }
            return;
        }
        if let Some((index, at_utc)) = self.chart_due(now) {
            self.begin_chart(index, at_utc, now, out);
        } else if now >= self.next_health && !self.pinned {
            self.begin_health(now, 0, out);
        } else {
            self.begin_planned(now, out);
        }
    }

    /// The chart to start now and when it is, if one is.
    fn chart_due(&self, now: u64) -> Option<(usize, u64)> {
        let day_start = now - now % 86_400;
        self.catalog.charts.iter().enumerate().find_map(|(i, c)| {
            // Around midnight the chart's time may be on the neighboring day.
            [day_start.wrapping_sub(86_400), day_start, day_start + 86_400].into_iter().find_map(|day| {
                let at = day.wrapping_add(u64::from(c.minute) * 60);
                let until = at as i64 - now as i64;
                ((-CHART_LATE_S..=CHART_LEAD_S).contains(&until) && at != self.last_chart).then_some((i, at))
            })
        })
    }

    /// A chart's frequencies on the air at its time.
    fn chart_options(&self, index: usize) -> Vec<u64> {
        let chart = &self.catalog.charts[index];
        chart.freqs.iter().filter(|f| in_window(f.window, chart.minute)).map(|f| f.dial_hz).collect()
    }

    /// The best of a chart's frequencies that a radiofax channel covers inside
    /// the slice at `center_hz`.
    fn chart_channel(&self, index: usize, center_hz: u64, hour: u8) -> Option<u64> {
        let covered: Vec<u64> =
            self.chart_options(index).into_iter().filter(|d| in_slice(center_hz, *d) && self.channels.contains(&(SdrMode::Wefax, *d))).collect();
        self.best("WEFAX", &covered, SdrMode::Wefax, hour)
    }

    /// A chart due now that a channel in the held slice covers is left to the
    /// channel, and the slice kept for it. A channel in another slice will do
    /// too: the span would have moved for the chart anyway, and this way the
    /// main decoder stays free. While a channel has a chart, one that would
    /// move the span waits its turn: it is passed over.
    fn chart_to_channel(&mut self, n: Inputs) {
        let now = n.utc;
        let Some((index, at_utc)) = self.chart_due(now) else { return };
        let chart = &self.catalog.charts[index];
        let label = format!("{} · {}", chart.station, chart.title);
        let hour = hour_of(now);
        // A chart on the main receiver whose picture is over, or never began,
        // ends on this tick: it holds nothing back. (Charts back to back, the
        // second one's lead minute falls just as the first one's picture ends.)
        let over = |s: &Slot| matches!(s.kind, Kind::Chart { at_utc, started } if !n.picture_busy && (started || now >= at_utc + CHART_NO_START_S));
        // The span is on the slice unless the radio check or a chart has taken
        // it elsewhere.
        let here = self.slice.filter(|c| self.slot.as_ref().is_none_or(|s| over(s) || in_slice(*c, s.dial_hz)));
        // Moving is for a slice the planner holds, never from under a picture,
        // a radio check or another chart, and only to a slice on this side of
        // the split with planned work in it: the decoder's hop there is what
        // moves the span.
        let movable = self.slice.is_some() && !self.pinned && !n.picture_busy && self.slot.as_ref().is_none_or(|s| s.kind == Kind::Planned || over(s));
        let side = if self.is_night(now) { NIGHT_SLICES } else { DAY_SLICES };
        let found = here.and_then(|c| self.chart_channel(index, c, hour).map(|d| (c, d))).or_else(|| {
            movable
                .then(|| {
                    side.iter()
                        .copied()
                        .filter(|c| self.catalog.targets.iter().any(|t| in_slice(*c, t.dial_hz)))
                        .find_map(|c| self.chart_channel(index, c, hour).map(|d| (c, d)))
                })
                .flatten()
        });
        match found {
            Some((center_hz, dial_hz)) => {
                if self.slice != Some(center_hz) {
                    // The planned slot ends, and the next starts in the new slice.
                    self.slice = Some(center_hz);
                    self.slice_visited.insert(center_hz, now);
                    self.slot = None;
                }
                self.last_chart = at_utc;
                self.visited.insert((SdrMode::Wefax, dial_hz), now);
                self.chart_pin_until = at_utc + CHART_MAX_S;
                self.slice_until = self.slice_until.max(self.chart_pin_until);
                self.pinned = true;
                eprintln!("schedule: {label} on the radiofax channel at {:.1} kHz", dial_hz as f64 / 1e3);
                self.channel_chart = Some(format!("chart on a channel: {label}"));
            }
            None if self.pinned => {
                self.last_chart = at_utc;
                eprintln!("schedule: {label} passed over, a channel is receiving a chart");
            }
            None => {}
        }
    }

    fn begin_chart(&mut self, index: usize, at_utc: u64, now: u64, out: &mut Vec<Action>) {
        let chart = self.catalog.charts[index].clone();
        self.last_chart = at_utc;
        let options = self.chart_options(index);
        let hour = hour_of(now);
        // A frequency the span already holds costs the channels nothing, and one
        // in another slice still leaves them most of theirs.
        let tier = |d: u64| match self.slice {
            Some(c) if in_slice(c, d) => 0,
            _ if self.channels.is_empty() || SLICE_CENTERS.iter().any(|c| in_slice(*c, d)) => 1,
            _ => 2,
        };
        let Some(first) = options.iter().map(|d| tier(*d)).min() else { return };
        let options: Vec<u64> = options.into_iter().filter(|d| tier(*d) == first).collect();
        let Some(dial_hz) = self.best("WEFAX", &options, SdrMode::Wefax, hour) else { return };
        self.visited.insert((SdrMode::Wefax, dial_hz), now);
        // Leaving the slice, take the one that also holds the most of the
        // station's other frequencies, so channels there hear copies of it.
        let center = match self.slice {
            Some(c) if in_slice(c, dial_hz) => Some(c),
            _ if self.channels.is_empty() => None,
            _ => SLICE_CENTERS
                .iter()
                .copied()
                .filter(|c| in_slice(*c, dial_hz))
                .max_by_key(|c| (options.iter().filter(|d| in_slice(*c, **d)).count(), std::cmp::Reverse(*c)))
                .or(Some(dial_hz)),
        };
        let label = format!("{} · {}", chart.station, chart.title);
        self.start_at(Kind::Chart { at_utc, started: false }, SdrMode::Wefax, dial_hz, center, at_utc + CHART_MAX_S, label, out);
    }

    fn begin_health(&mut self, now: u64, step: usize, out: &mut Vec<Action>) {
        let dial_hz = health_dials(now, self.slice)[step];
        self.start(Kind::Health { step }, SdrMode::Wwv, dial_hz, now + HEALTH_STEP_S, format!("radio check · WWV {} MHz", dial_hz / 1_000_000), out);
    }

    fn finish_health(&mut self, now: u64, ok: bool, out: &mut Vec<Action>) {
        out.push(Action::MarkHealth { since: self.last_check, until: now, healthy: ok });
        self.last_check = now;
        if ok {
            self.health = Health::Good;
            self.last_good = now;
            self.next_health = now + HEALTH_EVERY_S;
        } else {
            self.health = Health::Failing;
            self.next_health = now + HEALTH_RETRY_S;
        }
    }

    /// The slice to hold now, choosing again when the last has run its time.
    fn choose_slice(&mut self, now: u64) -> Option<u64> {
        if self.channels.is_empty() {
            return None;
        }
        if self.slice.is_some() && (now < self.slice_until || self.pinned) {
            return self.slice;
        }
        self.slice_picks += 1;
        let hour = hour_of(now);
        let side = if self.is_night(now) { NIGHT_SLICES } else { DAY_SLICES };
        let charted = side.iter().copied().find(|c| self.chart_soon_in(*c, now));
        let chosen = if charted.is_some() {
            charted
        } else if self.slice_picks.is_multiple_of(SLICE_EXPLORE_EVERY) {
            side.iter().copied().min_by_key(|c| self.slice_visited.get(c).copied().unwrap_or(0))
        } else {
            side.iter().copied().max_by(|a, b| {
                self.slice_score(*a, hour).total_cmp(&self.slice_score(*b, hour)).then(off_band(*b, hour).cmp(&off_band(*a, hour)))
            })
        };
        self.slice = chosen;
        self.slice_until = now + SLICE_HOLD_S;
        if let Some(c) = chosen {
            self.slice_visited.insert(c, now);
        }
        chosen
    }

    /// Whether a chart starts while a slice chosen now would be held, on a
    /// frequency a channel in that slice covers.
    fn chart_soon_in(&self, center_hz: u64, now: u64) -> bool {
        let day_start = now - now % 86_400;
        self.catalog.charts.iter().enumerate().any(|(i, c)| {
            [day_start, day_start + 86_400].into_iter().any(|day| {
                let lead = (day + u64::from(c.minute) * 60).saturating_sub(CHART_LEAD_S as u64);
                (now..now + SLICE_HOLD_S).contains(&lead) && self.chart_channel(i, center_hz, hour_of(now)).is_some()
            })
        })
    }

    /// Decodes per second a slice's channels have managed around this hour of the day.
    fn slice_score(&self, center_hz: u64, hour: u8) -> f64 {
        self.channels
            .iter()
            .filter(|(_, dial)| in_slice(center_hz, *dial))
            .map(|(mode, dial)| {
                let y = self.around(monitored_mode(*mode).unwrap_or(""), *dial, hour);
                (y.observations as f64 + 0.5) / (y.monitored_s as f64 + 300.0)
            })
            .sum()
    }

    /// The dials for a mode.
    fn targets_for(&self, mode: SdrMode) -> Vec<u64> {
        self.catalog.targets.iter().filter(|t| t.mode == mode).map(|t| t.dial_hz).collect()
    }

    /// The mode furthest behind its share, among those with somewhere to go.
    fn behind(&self) -> Option<&'static Plan> {
        let total: u64 = self.used.values().sum();
        PLANS
            .iter()
            .filter(|p| !self.targets_for(p.mode).is_empty())
            .max_by(|a, b| self.deficit(a, total).total_cmp(&self.deficit(b, total)).then(b.share.total_cmp(&a.share)))
    }

    fn deficit(&self, p: &Plan, total: u64) -> f64 {
        p.share * total as f64 - self.used.get(&p.mode).copied().unwrap_or(0) as f64 + p.share
    }

    fn begin_planned(&mut self, now: u64, out: &mut Vec<Action>) {
        if let Some(slice) = self.choose_slice(now) {
            self.park(slice, now, out);
            return;
        }
        let Some(plan) = self.behind() else { return };
        let label = monitored_mode(plan.mode).unwrap_or("");
        let hour = hour_of(now);
        let dials = self.targets_for(plan.mode);
        let n = self.picks.entry(plan.mode).or_insert(0);
        *n += 1;
        let explore = *n % EXPLORE_EVERY == 0;
        let chosen = if explore { self.least_heard(label, &dials, plan.mode, hour) } else { self.best(label, &dials, plan.mode, hour) };
        let Some(dial_hz) = chosen else { return };
        *self.used.entry(plan.mode).or_insert(0) += plan.dwell_s;
        self.visited.insert((plan.mode, dial_hz), now);
        self.start(Kind::Planned, plan.mode, dial_hz, now + plan.dwell_s, format!("{label} {:.1} kHz", dial_hz as f64 / 1e3), out);
    }

    /// Leave the main decoder on the slice's best FT8 dial until the slice is
    /// chosen again. The channels have the rest of it.
    fn park(&mut self, slice: u64, now: u64, out: &mut Vec<Action>) {
        let hour = hour_of(now);
        let inside = |mode: SdrMode| -> Vec<u64> { self.targets_for(mode).into_iter().filter(|d| in_slice(slice, *d)).collect() };
        let (mode, dial_hz) = match self.best("FT8", &inside(SdrMode::Ft8), SdrMode::Ft8, hour) {
            Some(d) => (SdrMode::Ft8, d),
            None => match self.catalog.targets.iter().find(|t| in_slice(slice, t.dial_hz)) {
                Some(t) => (t.mode, t.dial_hz),
                None => return,
            },
        };
        let label = format!("{} {:.1} kHz · slice {:.1} MHz", monitored_mode(mode).unwrap_or(""), dial_hz as f64 / 1e3, slice as f64 / 1e6);
        self.start(Kind::Planned, mode, dial_hz, self.slice_until, label, out);
    }

    /// The measurements for a frequency this hour of the day and on each side.
    fn around(&self, label: &str, dial_hz: u64, hour: u8) -> Yield {
        let mut sum = Yield::default();
        for h in [(hour + 23) % 24, hour, (hour + 1) % 24] {
            if let Some(y) = self.yields.get(&(label.to_string(), dial_hz, h)) {
                sum.monitored_s += y.monitored_s;
                sum.observations += y.observations;
            }
        }
        sum
    }

    fn last_visit(&self, mode: SdrMode, dial_hz: u64) -> u64 {
        self.visited.get(&(mode, dial_hz)).copied().unwrap_or(0)
    }

    /// The most productive frequency, counting one not yet tried as promising.
    fn best(&self, label: &str, dials: &[u64], mode: SdrMode, hour: u8) -> Option<u64> {
        let score = |d: u64| {
            let y = self.around(label, d, hour);
            (y.observations as f64 + 0.5) / (y.monitored_s as f64 + 300.0)
        };
        dials.iter().copied().max_by(|&a, &b| {
            score(a).total_cmp(&score(b)).then(self.last_visit(mode, b).cmp(&self.last_visit(mode, a))).then(off_band(b, hour).cmp(&off_band(a, hour)))
        })
    }

    fn least_heard(&self, label: &str, dials: &[u64], mode: SdrMode, hour: u8) -> Option<u64> {
        dials.iter().copied().min_by_key(|&d| (self.around(label, d, hour).monitored_s, self.last_visit(mode, d), off_band(d, hour)))
    }

    fn start(&mut self, kind: Kind, mode: SdrMode, dial_hz: u64, end: u64, label: String, out: &mut Vec<Action>) {
        self.start_at(kind, mode, dial_hz, self.center_for(dial_hz), end, label, out);
    }

    fn start_at(&mut self, kind: Kind, mode: SdrMode, dial_hz: u64, center_hz: Option<u64>, end: u64, label: String, out: &mut Vec<Action>) {
        let hop = Action::Hop { mode, dial_hz, center_hz };
        if self.last_hop != Some(hop) {
            out.push(hop);
            self.last_hop = Some(hop);
        }
        self.slot = Some(Slot { kind, mode, dial_hz, end, label });
    }

    /// Where the span should sit for a dial: the held slice if it reaches, else
    /// one that does, else the dial itself. `None` when no channels are running.
    fn center_for(&self, dial_hz: u64) -> Option<u64> {
        if self.channels.is_empty() {
            return None;
        }
        Some(match self.slice {
            Some(c) if in_slice(c, dial_hz) => c,
            _ => SLICE_CENTERS.iter().copied().find(|c| in_slice(*c, dial_hz)).unwrap_or(dial_hz),
        })
    }

    fn state(&self, now: u64) -> &'static str {
        if !self.enabled {
            "off"
        } else if now < self.hold_until {
            "held"
        } else if self.health == Health::Failing && !matches!(self.slot.as_ref().map(|s| &s.kind), Some(Kind::Health { .. })) {
            "failed"
        } else {
            match self.slot.as_ref().map(|s| &s.kind) {
                Some(Kind::Health { .. }) => "checking",
                Some(Kind::Chart { .. }) => "chart",
                Some(Kind::Planned) => "monitoring",
                None => "idle",
            }
        }
    }

    pub fn status(&self, now: u64) -> ScheduleStatus {
        let slot = self.slot.as_ref();
        let day_minute = ((now % 86_400) / 60) as u16;
        let next_chart = self
            .catalog
            .charts
            .iter()
            .min_by_key(|c| (c.minute + 1_440 - day_minute - 1) % 1_440)
            .map(|c| format!("{:02}{:02} UTC · {} · {}", c.minute / 60, c.minute % 60, c.station, c.title))
            .unwrap_or_default();
        let label = slot.map(|s| s.label.clone()).unwrap_or_default();
        ScheduleStatus {
            enabled: self.enabled,
            state: self.state(now).into(),
            label: match (&self.channel_chart, self.pinned && self.enabled && now >= self.hold_until) {
                (Some(chart), true) if label.is_empty() => chart.clone(),
                (Some(chart), true) => format!("{label} · {chart}"),
                _ => label,
            },
            ends_utc: slot.map_or(0, |s| s.end),
            hold_until_utc: if self.enabled && now < self.hold_until { self.hold_until } else { 0 },
            health: match self.health {
                Health::Unknown => "unknown",
                Health::Good => "good",
                Health::Failing => "failing",
            }
            .into(),
            last_good_utc: self.last_good,
            next_chart,
        }
    }
}

fn hour_of(now: u64) -> u8 {
    ((now % 86_400) / 3_600) as u8
}

/// How far a frequency is from where HF is usually best at this hour, in hertz.
/// Only breaks ties, so it decides where to start before anything is measured:
/// without it the first night's picks were the last on each list, the highest
/// and deadest bands.
fn off_band(dial_hz: u64, hour: u8) -> u64 {
    let night = matches!(hour, 0..=10 | 22 | 23);
    dial_hz.abs_diff(if night { 7_000_000 } else { 14_000_000 })
}

/// Three of WWV's frequencies to try, in order: any the slice holds first, as
/// they cost the channels nothing, then the likeliest. The lower bands are the
/// ones open across the American night; 20 and 25 MHz are tried only from a
/// slice that holds them.
fn health_dials(now: u64, slice: Option<u64>) -> Vec<u64> {
    let mut dials = match hour_of(now) {
        0..=10 | 22 | 23 => vec![5_000_000, 10_000_000, 15_000_000],
        _ => vec![10_000_000, 15_000_000, 5_000_000],
    };
    if let Some(c) = slice {
        dials.extend([20_000_000, 25_000_000].into_iter().filter(|d| in_slice(c, *d)));
        dials.sort_by_key(|d| !in_slice(c, *d));
    }
    dials.truncate(3);
    dials
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Midnight UTC.
    const DAY: u64 = 1_794_182_400 - 1_794_182_400 % 86_400;
    /// Noon: nothing in the test catalog is near it.
    const NOON: u64 = DAY + 12 * 3_600;

    fn ft8(khz: u64) -> Target {
        Target { mode: SdrMode::Ft8, dial_hz: khz * 1_000 }
    }

    fn catalog(targets: Vec<Target>, charts: Vec<Chart>) -> Catalog {
        Catalog { targets, charts }
    }

    fn chart(minute: u16, dial_hz: u64) -> Chart {
        Chart { minute, station: "NMF".into(), title: "SURFACE ANALYSIS".into(), freqs: vec![ChartFreq { dial_hz, window: None }] }
    }

    fn at(utc: u64) -> Inputs {
        Inputs { utc, ..Default::default() }
    }

    fn hops(a: &[Action]) -> Vec<(SdrMode, u64)> {
        a.iter().filter_map(|x| if let Action::Hop { mode, dial_hz, .. } = x { Some((*mode, *dial_hz)) } else { None }).collect()
    }

    /// A scheduler already past its first radio check.
    fn running(c: Catalog, now: u64) -> Scheduler {
        let mut s = Scheduler::new(c, true);
        let first = s.tick(at(now));
        assert_eq!(hops(&first)[0].0, SdrMode::Wwv, "the first thing it does is check the radio");
        s.tick(Inputs { utc: now, wwv_locked: true, ..Default::default() });
        s
    }

    #[test]
    fn it_does_nothing_until_turned_on() {
        let mut s = Scheduler::new(catalog(vec![ft8(14074)], vec![]), false);
        assert!(s.tick(at(NOON)).is_empty());
        assert_eq!(s.status(NOON).state, "off");
    }

    #[test]
    fn the_operator_holds_the_schedule_and_it_comes_back() {
        let mut s = running(catalog(vec![ft8(14074)], vec![]), NOON);
        s.operator(NOON + 5);
        assert!(s.tick(at(NOON + 6)).is_empty());
        assert_eq!(s.status(NOON + 6).state, "held");
        assert_eq!(s.status(NOON + 6).hold_until_utc, NOON + 5 + HOLD_S);
        assert!(s.tick(at(NOON + 5 + HOLD_S - 1)).is_empty());
        assert_eq!(hops(&s.tick(at(NOON + 5 + HOLD_S))), vec![(SdrMode::Ft8, 14_074_000)]);
    }

    #[test]
    fn resume_ends_a_hold_at_once() {
        let mut s = running(catalog(vec![ft8(14074)], vec![]), NOON);
        s.operator(NOON);
        s.resume();
        assert_eq!(hops(&s.tick(at(NOON + 1))).len(), 1);
    }

    #[test]
    fn touching_the_radio_when_it_is_off_starts_no_hold() {
        let mut s = Scheduler::new(catalog(vec![ft8(14074)], vec![]), false);
        s.operator(NOON);
        s.set_enabled(true, NOON);
        assert_eq!(hops(&s.tick(at(NOON + 1))).len(), 1);
    }

    #[test]
    fn a_slot_lasts_its_dwell() {
        let mut s = running(catalog(vec![ft8(14074), ft8(7074)], vec![]), NOON);
        assert!(s.tick(at(NOON + 119)).is_empty());
        assert_eq!(hops(&s.tick(at(NOON + 120))).len(), 1);
    }

    #[test]
    fn a_chart_preempts_a_planned_slot_a_minute_early() {
        let mut s = running(catalog(vec![ft8(14074)], vec![chart(12 * 60 + 2, 9_108_100)]), NOON);
        assert!(s.tick(at(NOON + 59)).is_empty(), "too early");
        assert_eq!(hops(&s.tick(at(NOON + 60))), vec![(SdrMode::Wefax, 9_108_100)]);
        assert_eq!(s.status(NOON + 60).state, "chart");
    }

    #[test]
    fn a_picture_in_progress_is_not_cut_off() {
        let mut s = running(catalog(vec![ft8(14074)], vec![chart(12 * 60 + 2, 9_108_100)]), NOON);
        s.tick(at(NOON + 60));
        let end = NOON + 120 + CHART_MAX_S;
        let busy = |utc| Inputs { utc, picture_busy: true, ..Default::default() };
        assert!(s.tick(busy(end + 60)).is_empty(), "past its slot and still receiving");
        // When the picture ends the slot ends, and the next begins.
        assert_eq!(hops(&s.tick(at(end + 61))).len(), 1);
        assert_eq!(s.status(end + 61).state, "monitoring");
    }

    #[test]
    fn a_stuck_picture_does_not_hold_the_radio_forever() {
        let mut s = running(catalog(vec![ft8(14074)], vec![chart(12 * 60 + 2, 9_108_100)]), NOON);
        s.tick(at(NOON + 60));
        let limit = NOON + 120 + CHART_MAX_S + PICTURE_OVERRUN_S;
        assert_eq!(hops(&s.tick(Inputs { utc: limit, picture_busy: true, ..Default::default() })).len(), 1);
    }

    #[test]
    fn a_chart_that_never_starts_is_given_up_and_not_retried() {
        let mut s = running(catalog(vec![ft8(14074)], vec![chart(12 * 60 + 2, 9_108_100)]), NOON);
        s.tick(at(NOON + 60));
        assert!(s.tick(at(NOON + 120 + CHART_NO_START_S - 1)).is_empty());
        assert_eq!(hops(&s.tick(at(NOON + 120 + CHART_NO_START_S))), vec![(SdrMode::Ft8, 14_074_000)]);
    }

    #[test]
    fn a_held_operator_beats_a_picture() {
        let mut s = running(catalog(vec![ft8(14074)], vec![chart(12 * 60 + 2, 9_108_100)]), NOON);
        s.tick(at(NOON + 60));
        s.operator(NOON + 200);
        assert!(s.tick(Inputs { utc: NOON + 201, picture_busy: true, ..Default::default() }).is_empty());
        assert_eq!(s.status(NOON + 201).state, "held");
    }

    #[test]
    fn a_chart_just_before_midnight_is_found_from_the_day_it_is_on() {
        let mut s = running(catalog(vec![ft8(14074)], vec![chart(0, 9_108_100)]), DAY + 86_400 - 200);
        assert!(s.tick(at(DAY + 86_400 - 100)).is_empty());
        assert_eq!(hops(&s.tick(at(DAY + 86_400 - 60))), vec![(SdrMode::Wefax, 9_108_100)]);
    }

    #[test]
    fn a_radio_that_fails_its_check_stops_hopping_and_marks_the_time() {
        let mut s = Scheduler::new(catalog(vec![ft8(14074)], vec![]), true);
        s.last_check = NOON - 50;
        assert_eq!(hops(&s.tick(at(NOON))), vec![(SdrMode::Wwv, 10_000_000)]);
        assert_eq!(hops(&s.tick(at(NOON + HEALTH_STEP_S))), vec![(SdrMode::Wwv, 15_000_000)]);
        assert_eq!(hops(&s.tick(at(NOON + 2 * HEALTH_STEP_S))), vec![(SdrMode::Wwv, 5_000_000)]);
        let done = s.tick(at(NOON + 3 * HEALTH_STEP_S));
        assert!(done.contains(&Action::MarkHealth { since: NOON - 50, until: NOON + 3 * HEALTH_STEP_S, healthy: false }));
        assert!(hops(&done).is_empty());
        assert_eq!(s.status(NOON + 3 * HEALTH_STEP_S).state, "failed");
        assert_eq!(s.status(NOON + 3 * HEALTH_STEP_S).health, "failing");
        assert!(s.tick(at(NOON + 3 * HEALTH_STEP_S + HEALTH_RETRY_S - 1)).is_empty());

        // Tested again, and when it passes the schedule carries on.
        let retry = NOON + 3 * HEALTH_STEP_S + HEALTH_RETRY_S;
        assert_eq!(hops(&s.tick(at(retry))).len(), 1);
        let passed = s.tick(Inputs { utc: retry + 10, wwv_locked: true, ..Default::default() });
        assert!(passed.iter().any(|a| matches!(a, Action::MarkHealth { healthy: true, .. })));
        assert_eq!(hops(&passed), vec![(SdrMode::Ft8, 14_074_000)]);
    }

    #[test]
    fn the_night_checks_the_lower_bands_first() {
        assert_eq!(health_dials(DAY + 3_600, None), [5_000_000, 10_000_000, 15_000_000]);
        assert_eq!(health_dials(NOON, None)[0], 10_000_000);
    }

    #[test]
    fn a_radio_check_uses_wwv_inside_the_slice_first() {
        assert_eq!(health_dials(NOON, Some(17_607_000)), [15_000_000, 20_000_000, 10_000_000]);
        assert_eq!(health_dials(NOON, Some(24_620_000)), [25_000_000, 10_000_000, 15_000_000], "the high slice has only 25 MHz");
        assert_eq!(health_dials(DAY + 3_600, Some(6_855_000)), [5_000_000, 10_000_000, 15_000_000]);
    }

    fn with_channels(targets: Vec<Target>, channels: &[(SdrMode, u64)]) -> Scheduler {
        let mut s = running(catalog(targets, vec![]), NOON);
        s.next_health = u64::MAX;
        s.slot = None;
        s.set_channels(channels.iter().copied().collect());
        s
    }

    fn center_of(a: &[Action]) -> Option<u64> {
        a.iter().find_map(|x| if let Action::Hop { center_hz, .. } = x { *center_hz } else { None })
    }

    #[test]
    fn in_a_slice_the_decoder_stays_on_the_best_ft8_dial() {
        let targets = vec![
            Target { mode: SdrMode::Selcal, dial_hz: 15_000_000 },
            Target { mode: SdrMode::Hfdl, dial_hz: 17_919_000 },
            Target { mode: SdrMode::Ft8, dial_hz: 18_100_000 },
            Target { mode: SdrMode::Ft8, dial_hz: 14_074_000 },
            Target { mode: SdrMode::Ft8, dial_hz: 7_074_000 },
        ];
        let channels: Vec<(SdrMode, u64)> = targets.iter().map(|t| (t.mode, t.dial_hz)).collect();
        let mut s = with_channels(targets, &channels);
        s.yields.insert(("FT8".to_string(), 14_074_000, hour_of(NOON)), Yield { monitored_s: 100, observations: 500 });
        let first = s.tick(at(NOON));
        assert_eq!(hops(&first), vec![(SdrMode::Ft8, 14_074_000)]);
        assert_eq!(center_of(&first), Some(17_607_000));
        // Nothing moves for the rest of the slice's time.
        for t in (NOON + 1..NOON + SLICE_HOLD_S).step_by(30) {
            assert!(s.tick(at(t)).is_empty(), "moved at {}", t - NOON);
        }
    }

    #[test]
    fn choosing_the_same_slice_again_sends_no_hop() {
        let mut s = with_channels(vec![ft8(14074), ft8(28074)], &[(SdrMode::Ft8, 14_074_000), (SdrMode::Ft8, 28_074_000)]);
        for h in 0..24 {
            s.yields.insert(("FT8".to_string(), 14_074_000, h), Yield { monitored_s: 100, observations: 500 });
        }
        assert_eq!(hops(&s.tick(at(NOON))).len(), 1);
        assert!(s.tick(at(NOON + SLICE_HOLD_S)).is_empty(), "same slice, same dial: the decoder keeps its slot");
        assert_eq!(s.slice, Some(17_607_000));
        // After the operator has had the radio, it is put back.
        s.operator(NOON + SLICE_HOLD_S + 1);
        s.resume();
        assert_eq!(hops(&s.tick(at(NOON + SLICE_HOLD_S + 2))), vec![(SdrMode::Ft8, 14_074_000)]);
    }

    #[test]
    fn a_chart_takes_a_frequency_inside_the_slice_when_it_has_one() {
        let freqs = vec![ChartFreq { dial_hz: 12_788_000, window: None }, ChartFreq { dial_hz: 17_144_500, window: None }, ChartFreq { dial_hz: 8_502_000, window: None }];
        let c = Chart { minute: 12 * 60 + 10, station: "NMG".into(), title: "SURFACE ANALYSIS".into(), freqs };
        let mut s = running(catalog(vec![ft8(14074)], vec![c]), NOON);
        s.next_health = u64::MAX;
        s.set_channels([(SdrMode::Ft8, 14_074_000)].into_iter().collect());
        // 12788 has been the best heard, but it is in no slice.
        s.yields.insert(("WEFAX".to_string(), 12_788_000, 12), Yield { monitored_s: 100, observations: 50 });
        s.slot = None;
        s.tick(at(NOON));
        assert_eq!(s.slice, Some(17_607_000));
        let chart = s.tick(at(NOON + 9 * 60));
        assert_eq!(hops(&chart), vec![(SdrMode::Wefax, 17_144_500)]);
        assert_eq!(center_of(&chart), Some(17_607_000), "the span stays where it is");
        // Without the slice's frequency it goes to one in the other slice before one in none.
        let freqs = vec![ChartFreq { dial_hz: 12_788_000, window: None }, ChartFreq { dial_hz: 8_502_000, window: None }];
        let c = Chart { minute: 12 * 60 + 40, station: "NMG".into(), title: "SURFACE ANALYSIS".into(), freqs };
        s.catalog.charts = vec![c];
        s.slot = None;
        let chart = s.tick(at(NOON + 39 * 60));
        assert_eq!(hops(&chart), vec![(SdrMode::Wefax, 8_502_000)]);
        assert!(center_of(&chart).is_some_and(|c| c < 12_000_000));
        // NMF on 6340.5 kHz: the span that also holds 9110, not the first that holds 6340.5.
        let freqs = vec![ChartFreq { dial_hz: 6_338_600, window: None }, ChartFreq { dial_hz: 9_108_100, window: None }];
        s.catalog.charts = vec![Chart { minute: 13 * 60 + 10, station: "NMF".into(), title: "SURFACE ANALYSIS".into(), freqs }];
        s.yields.insert(("WEFAX".to_string(), 6_338_600, 13), Yield { monitored_s: 100, observations: 50 });
        s.slot = None;
        let chart = s.tick(at(NOON + 69 * 60));
        assert_eq!(hops(&chart), vec![(SdrMode::Wefax, 6_338_600)]);
        assert_eq!(center_of(&chart), Some(6_855_000));
    }

    #[test]
    fn the_channel_targets_include_every_chart_frequency() {
        let c = Catalog::builtin();
        let t = c.channel_targets();
        assert!(t.contains(&Target { mode: SdrMode::Wefax, dial_hz: 12_788_000 }));
        assert!(t.contains(&Target { mode: SdrMode::Wefax, dial_hz: 13_880_600 }));
        assert_eq!(t.iter().filter(|x| x.mode == SdrMode::Wefax).count(), 11);
        assert!(t.contains(&Target { mode: SdrMode::Beacon, dial_hz: 14_099_300 }));
        assert!(t.contains(&Target { mode: SdrMode::Sstv, dial_hz: 7_171_000 }));
        assert_eq!(t.len(), c.targets.len() + 11 + 5 + 1);
    }

    #[test]
    fn a_slice_with_nothing_else_to_do_still_gets_a_decoder() {
        let mut s = with_channels(vec![Target { mode: SdrMode::Ft8, dial_hz: 14_074_000 }], &[(SdrMode::Ft8, 14_074_000)]);
        assert_eq!(hops(&s.tick(at(NOON))), vec![(SdrMode::Ft8, 14_074_000)]);
    }

    #[test]
    fn every_so_often_the_longest_unvisited_slice_is_tried() {
        let targets = vec![Target { mode: SdrMode::Ft8, dial_hz: 14_074_000 }, Target { mode: SdrMode::Ft8, dial_hz: 28_074_000 }];
        let mut s = with_channels(targets, &[(SdrMode::Ft8, 14_074_000), (SdrMode::Ft8, 28_074_000)]);
        for h in 0..24 {
            s.yields.insert(("FT8".to_string(), 14_074_000, h), Yield { monitored_s: 100, observations: 500 });
        }
        let mut centers = Vec::new();
        let mut now = NOON;
        for _ in 0..SLICE_EXPLORE_EVERY {
            s.slot = None;
            s.tick(at(now));
            centers.push(s.slice.unwrap());
            now += SLICE_HOLD_S;
        }
        assert_eq!(centers[0], 17_607_000);
        assert_eq!(*centers.last().unwrap(), 24_620_000, "the fifth choice goes looking");
    }

    #[test]
    fn a_chart_outside_the_slice_moves_the_span_and_without_channels_nothing_changes() {
        let mut s = with_channels(vec![Target { mode: SdrMode::Ft8, dial_hz: 14_074_000 }], &[(SdrMode::Ft8, 14_074_000)]);
        s.tick(at(NOON));
        assert_eq!(s.center_for(15_000_000), Some(17_607_000), "in the slice already");
        assert_eq!(s.center_for(9_108_100), Some(6_855_000), "moves for a chart below the split");
        assert_eq!(s.center_for(12_750_000), Some(12_750_000), "in no slice at all");
        let mut plain = running(catalog(vec![Target { mode: SdrMode::Ft8, dial_hz: 7_074_000 }], vec![]), NOON);
        plain.next_health = u64::MAX;
        assert_eq!(center_of(&plain.tick(at(NOON))), None);
    }

    /// NMG's 17146.4 kHz, as a channel in the day slice.
    const NMG_17: u64 = 17_144_500;

    #[test]
    fn a_chart_a_channel_covers_in_the_slice_leaves_the_radio_alone() {
        let targets = vec![ft8(14074), Target { mode: SdrMode::Selcal, dial_hz: 15_000_000 }];
        let mut s = with_channels(targets, &[(SdrMode::Ft8, 14_074_000), (SdrMode::Wefax, NMG_17)]);
        s.catalog.charts = vec![chart(12 * 60 + 30, NMG_17), chart(12 * 60 + 34, 9_108_100)];
        assert_eq!(center_of(&s.tick(at(NOON))), Some(17_607_000));
        s.slot = None;
        // The chart's lead minute: the decoder carries on in the slice.
        let lead = NOON + 29 * 60;
        let a = s.tick(at(lead));
        assert!(hops(&a).is_empty(), "the decoder is already parked in the slice: {a:?}");
        assert_eq!(s.slice, Some(17_607_000));
        assert_eq!(s.status(lead).state, "monitoring");
        assert!(s.status(lead).label.contains("chart on a channel: NMF"), "{}", s.status(lead).label);
        // Past the slice's own hold, a radio check due, and another chart off
        // the slice: none of them moves the span while the chart runs.
        s.next_health = lead;
        s.slot = None;
        let a = s.tick(at(NOON + 34 * 60 - 60));
        assert!(hops(&a).is_empty(), "{a:?}");
        assert_eq!(s.slice, Some(17_607_000));
        // The picture outlasting the timetable keeps the slice too.
        s.slot = None;
        let late = NOON + 30 * 60 + CHART_MAX_S + 60;
        let busy = Inputs { utc: late, channel_chart_busy: true, ..Default::default() };
        let a = s.tick(busy);
        assert!(hops(&a).iter().all(|h| h.0 != SdrMode::Wwv), "{a:?}");
        // Over: the radio check goes ahead.
        s.slot = None;
        assert_eq!(hops(&s.tick(at(late + 1)))[0].0, SdrMode::Wwv);
        assert!(!s.status(late + 1).label.contains("chart"));
    }

    #[test]
    fn a_chart_a_channel_in_another_slice_covers_moves_the_slice_not_the_decoder() {
        let targets = vec![ft8(14074), ft8(24915)];
        let mut s = with_channels(targets, &[(SdrMode::Ft8, 14_074_000), (SdrMode::Ft8, 24_915_000), (SdrMode::Wefax, NMG_17)]);
        s.catalog.charts = vec![chart(12 * 60 + 30, NMG_17)];
        s.tick(at(NOON));
        s.slice = Some(24_620_000);
        s.slice_until = NOON + 3_600;
        // A picture on the main receiver is not cut off for it.
        let lead = NOON + 29 * 60;
        s.slot = None;
        assert_eq!(center_of(&s.tick(at(lead - 30))), Some(24_620_000));
        let busy = s.tick(Inputs { utc: lead, picture_busy: true, ..Default::default() });
        assert!(hops(&busy).is_empty(), "{busy:?}");
        assert_eq!(s.slice, Some(24_620_000));
        // Otherwise the slice moves to the channel and the decoder works there.
        s.last_chart = 0;
        let a = s.tick(at(lead + 1));
        assert_eq!(center_of(&a), Some(17_607_000), "{a:?}");
        assert!(hops(&a).iter().all(|h| h.0 == SdrMode::Ft8), "{a:?}");
        assert!(s.status(lead + 1).label.contains("chart on a channel"));
    }

    #[test]
    fn a_chart_right_after_one_on_the_radio_still_goes_to_its_channel() {
        // DDH3 at 1800 on 13880.6 kHz, outside every slice, then NMG at 1820
        // with a channel in the day slice: the evening of 2026-11-10.
        let mut s = with_channels(vec![ft8(14074)], &[(SdrMode::Ft8, 14_074_000), (SdrMode::Wefax, NMG_17)]);
        s.catalog.charts = vec![chart(12 * 60, 13_880_600), chart(12 * 60 + 20, NMG_17)];
        s.tick(at(NOON - 20 * 60));
        s.slot = None;
        assert_eq!(hops(&s.tick(at(NOON - 60))), vec![(SdrMode::Wefax, 13_880_600)]);
        let busy = |t| Inputs { utc: t, picture_busy: true, ..Default::default() };
        assert!(hops(&s.tick(busy(NOON + 60))).is_empty());
        assert!(hops(&s.tick(busy(NOON + 19 * 60 + 38))).is_empty(), "the second chart waits for the first picture");
        // The first picture ends in the second chart's lead minute.
        let a = s.tick(at(NOON + 19 * 60 + 39));
        assert!(hops(&a).iter().all(|h| h.0 == SdrMode::Ft8), "{a:?}");
        assert_eq!(center_of(&a), Some(17_607_000), "{a:?}");
        assert!(s.status(NOON + 19 * 60 + 39).label.contains("chart on a channel"));
    }

    #[test]
    fn a_chart_no_channel_in_the_slice_covers_takes_the_radio() {
        let mut s = with_channels(vec![ft8(14074)], &[(SdrMode::Ft8, 14_074_000), (SdrMode::Wefax, 9_108_100)]);
        s.catalog.charts = vec![chart(12 * 60 + 30, 9_108_100)];
        s.tick(at(NOON));
        s.slot = None;
        let a = s.tick(at(NOON + 29 * 60));
        assert_eq!(hops(&a), vec![(SdrMode::Wefax, 9_108_100)], "the channel is below the split, the slice above");
        assert_eq!(s.status(NOON + 29 * 60).state, "chart");
    }

    #[test]
    fn a_slice_chosen_just_before_a_chart_is_the_one_holding_it() {
        let night = DAY + 3 * 3_600;
        let channels = [(SdrMode::Ft8, 1_840_000), (SdrMode::Ft8, 7_074_000), (SdrMode::Wefax, 9_108_100)];
        let mut s = with_channels(vec![ft8(1840), ft8(7074)], &channels);
        s.yields.insert(("FT8".to_string(), 1_840_000, 3), Yield { monitored_s: 100, observations: 5_000 });
        assert_eq!(s.choose_slice(night), Some(5_400_000), "160 m has been busy");
        s.slice_until = 0;
        s.catalog.charts = vec![chart(3 * 60 + 20, 9_108_100)];
        assert_eq!(s.choose_slice(night), Some(5_400_000), "the chart is past this hold");
        s.slice_until = 0;
        assert_eq!(s.choose_slice(night + 10 * 60), Some(6_855_000), "9110 kHz is only in the upper night slice");
    }

    #[test]
    fn the_sun_decides_which_side_of_the_split_it_holds() {
        let home = grid_center("FN31").unwrap();
        assert_eq!(home, (41.5, -73.0));
        assert_eq!(grid_center("xx"), None);
        // December: dark before 12 UTC on the US East Coast, light at 17 UTC.
        let december = 1_796_083_200_u64 - (1_796_083_200 % 86_400);
        let targets = vec![Target { mode: SdrMode::Ft8, dial_hz: 7_074_000 }, Target { mode: SdrMode::Ft8, dial_hz: 14_074_000 }];
        let mut s = with_channels(targets, &[(SdrMode::Ft8, 7_074_000), (SdrMode::Ft8, 14_074_000)]);
        s.set_home(Some(home));
        assert!(s.is_night(december + 5 * 3_600));
        assert!(!s.is_night(december + 17 * 3_600));
        assert_eq!(s.choose_slice(december + 5 * 3_600).map(|c| c < 12_000_000), Some(true));
        s.slice_until = 0;
        assert_eq!(s.choose_slice(december + 17 * 3_600).map(|c| c > 12_000_000), Some(true));
    }

    #[test]
    fn no_slice_crosses_the_receivers_filter_split() {
        for c in SLICE_CENTERS {
            let (low, high) = (c as f64 - SLICE_REACH_HZ, c as f64 + SLICE_REACH_HZ);
            assert!(high < 12_000_000.0 || low > 12_000_000.0, "{c} straddles 12 MHz");
        }
    }

    #[test]
    fn modes_get_their_shares() {
        let targets: Vec<Target> = PLANS.iter().map(|p| Target { mode: p.mode, dial_hz: 7_000_000 }).collect();
        let mut s = running(catalog(targets, vec![]), NOON);
        s.next_health = u64::MAX;
        let mut now = NOON;
        let mut spent: BTreeMap<SdrMode, u64> = BTreeMap::new();
        for _ in 0..600 {
            s.slot = None;
            let hop = hops(&s.tick(at(now)))[0];
            let dwell = PLANS.iter().find(|p| p.mode == hop.0).unwrap().dwell_s;
            *spent.entry(hop.0).or_default() += dwell;
            now += dwell;
        }
        let total: u64 = spent.values().sum();
        for p in &PLANS {
            let got = spent[&p.mode] as f64 / total as f64;
            assert!((got - p.share).abs() < 0.03, "{:?}: {got:.3} of the air against a share of {}", p.mode, p.share);
        }
    }

    fn yields(rows: &[(u64, u8, u64, u64)]) -> Yields {
        rows.iter().map(|&(khz, hour, monitored_s, observations)| (("FT8".to_string(), khz * 1_000, hour), Yield { monitored_s, observations })).collect()
    }

    #[test]
    fn the_frequency_comes_from_what_was_heard_at_this_hour() {
        let mut s = running(catalog(vec![ft8(7074), ft8(14074), ft8(21074)], vec![]), NOON);
        s.set_yields(yields(&[(7074, 12, 3_600, 5), (14074, 12, 3_600, 900), (21074, 12, 3_600, 40), (7074, 3, 3_600, 9_000)]));
        s.slot = None;
        assert_eq!(picked(&mut s, NOON + 1), (SdrMode::Ft8, 14_074_000));
    }

    /// Where a tick puts the decoder, whether or not it had to move to get there.
    fn picked(s: &mut Scheduler, now: u64) -> (SdrMode, u64) {
        s.tick(at(now));
        let slot = s.slot.as_ref().expect("a slot");
        (slot.mode, slot.dial_hz)
    }

    #[test]
    fn neighboring_hours_count() {
        let s = {
            let mut s = Scheduler::new(catalog(vec![ft8(14074)], vec![]), true);
            s.set_yields(yields(&[(14074, 11, 100, 1), (14074, 13, 100, 2), (14074, 14, 100, 50), (14074, 12, 100, 4)]));
            s
        };
        assert_eq!(s.around("FT8", 14_074_000, 12), Yield { monitored_s: 300, observations: 7 });
        let wraps = {
            let mut s = Scheduler::new(catalog(vec![], vec![]), true);
            s.set_yields(yields(&[(14074, 23, 10, 1), (14074, 1, 10, 1)]));
            s.around("FT8", 14_074_000, 0)
        };
        assert_eq!(wraps.observations, 2);
    }

    #[test]
    fn an_untried_frequency_beats_one_that_heard_nothing() {
        let mut s = running(catalog(vec![ft8(1840), ft8(28074)], vec![]), NOON);
        s.set_yields(yields(&[(1840, 12, 3_600, 0)]));
        s.slot = None;
        assert_eq!(hops(&s.tick(at(NOON + 1))), vec![(SdrMode::Ft8, 28_074_000)]);
    }

    #[test]
    fn with_nothing_measured_it_starts_on_a_band_that_suits_the_hour() {
        let list = vec![ft8(1840), ft8(7074), ft8(14074), ft8(28074)];
        let mut day = running(catalog(list.clone(), vec![]), NOON);
        day.slot = None;
        // Starting up has just sent it to the best guess; that is the one tried.
        assert_eq!(day.last_visit(SdrMode::Ft8, 14_074_000), NOON);

        let night_time = DAY + 4 * 3_600;
        let night = running(catalog(list, vec![]), night_time);
        assert_eq!(night.last_visit(SdrMode::Ft8, 7_074_000), night_time);
    }

    #[test]
    fn every_fifth_pick_revisits_the_least_heard() {
        let mut s = running(catalog(vec![ft8(1840), ft8(14074)], vec![]), NOON);
        s.set_yields(yields(&[(1840, 12, 200, 0), (14074, 12, 90_000, 90_000)]));
        let mut now = NOON + 1;
        let mut picks = Vec::new();
        // Starting up already made the first pick.
        for _ in 0..4 {
            s.slot = None;
            picks.push(picked(&mut s, now).1);
            now += 200;
        }
        assert_eq!(picks[..3], [14_074_000; 3]);
        assert_eq!(picks[3], 1_840_000);
    }

    #[test]
    fn a_window_that_crosses_midnight_is_read_as_one_stretch() {
        let w = parse_window("2200-0500");
        assert!(in_window(w, 23 * 60) && in_window(w, 60) && !in_window(w, 12 * 60));
        assert_eq!(parse_window("all broadcast times"), None);
        assert!(in_window(None, 0));
        assert_eq!(parse_window("0230-1039"), Some((150, 639)));
    }

    #[test]
    fn a_chart_uses_a_frequency_that_is_on_at_its_time() {
        let freqs = vec![ChartFreq { dial_hz: 4_233_100, window: Some((150, 639)) }, ChartFreq { dial_hz: 12_748_100, window: Some((840, 1_359)) }];
        let c = Chart { minute: 12 * 60 + 2, station: "NMF".into(), title: "x".into(), freqs };
        let mut s = running(catalog(vec![ft8(14074)], vec![c]), NOON);
        let got = hops(&s.tick(at(NOON + 60)));
        assert!(got.is_empty() || got[0].0 != SdrMode::Wefax, "neither frequency is on at 1202: {got:?}");
    }

    #[test]
    fn the_real_directory_gives_a_workable_catalog() {
        let c = Catalog::from_directory(include_str!("../../desktop/data/hf-directory.json")).unwrap();
        assert!(c.targets.iter().all(|t| (100_000..30_000_000).contains(&t.dial_hz)));
        assert!(c.targets.iter().any(|t| t.mode == SdrMode::Hfdl && t.dial_hz == 11_387_000), "Riverhead");
        assert!(c.targets.iter().any(|t| t.mode == SdrMode::Dsc && t.dial_hz == 8_412_800));
        for p in &PLANS {
            assert!(c.targets.iter().any(|t| t.mode == p.mode), "{:?} has nowhere to go", p.mode);
        }
        assert_eq!(c.charts.iter().filter(|x| x.station == "NMF").count(), 12);
        assert_eq!(c.charts.iter().filter(|x| x.station == "NMG").count(), 8);
        assert_eq!(c.charts.iter().filter(|x| x.station == "DDH3").count(), 6);
        assert!(c.charts.iter().all(|x| !x.freqs.is_empty()));
        // At the NMF chart at 0233 the published window picks 4235 and the
        // always-on frequencies, none of the daytime-only ones.
        let nmf = c.charts.iter().find(|x| x.station == "NMF" && x.minute == 153).unwrap();
        let dials: Vec<u64> = nmf.freqs.iter().filter(|f| in_window(f.window, 153)).map(|f| f.dial_hz).collect();
        assert!(dials.contains(&4_233_100), "{dials:?}");
        assert!(!dials.contains(&12_748_100), "{dials:?}");
    }

    #[test]
    fn the_status_says_where_it_is_and_what_is_next() {
        let mut s = running(catalog(vec![ft8(14074)], vec![chart(13 * 60, 9_108_100)]), NOON);
        let st = s.status(NOON);
        assert_eq!((st.state.as_str(), st.health.as_str()), ("monitoring", "good"));
        assert!(st.label.contains("14074.0"), "{}", st.label);
        assert_eq!(st.ends_utc, NOON + 120);
        assert_eq!(st.last_good_utc, NOON);
        assert!(st.next_chart.starts_with("1300 UTC"), "{}", st.next_chart);
        s.set_enabled(false, NOON);
        assert_eq!(s.status(NOON).state, "off");
    }

    #[test]
    fn turning_off_leaves_the_radio_alone() {
        let mut s = running(catalog(vec![ft8(14074)], vec![]), NOON);
        s.set_enabled(false, NOON + 1);
        assert!(s.tick(at(NOON + 500)).is_empty());
    }

    #[test]
    fn it_owns_only_what_it_tuned() {
        let s = running(catalog(vec![ft8(14074)], vec![]), NOON);
        assert!(s.owns(SdrMode::Ft8, 14_074_000.0));
        assert!(!s.owns(SdrMode::Ft8, 14_075_000.0));
        assert!(!s.owns(SdrMode::Usb, 14_074_000.0));
    }
}
