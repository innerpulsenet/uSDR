//! The operator's station, and what the server's spot reporter is doing
//! with it. The server keeps both; a client edits the station through
//! `POST /api/station` and hears both on the socket as a `station` message.

use serde::{Deserialize, Serialize};

/// Who is listening, and where, as the spots the server reports carry it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Station {
    /// The operator's callsign, upper case. Empty until one is set.
    pub callsign: String,
    /// The Maidenhead locator: 4, 6 or 8 characters, as [`normalize_grid`]
    /// writes it.
    pub grid: String,
    /// The receiving antenna, free text, shown on the reporter's map.
    pub antenna: String,
    /// FT8 and FT4 decodes go to PSKReporter.
    pub report_spots: bool,
}

impl Station {
    /// A callsign and a locator that both pass: what reporting needs.
    pub fn complete(&self) -> bool {
        normalize_callsign(&self.callsign).is_ok() && normalize_grid(&self.grid).is_ok()
    }
}

/// Where the spot reporter is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReporterState {
    /// Reporting is turned off.
    #[default]
    Off,
    /// Turned on, but the station has no valid callsign or locator yet.
    Incomplete,
    /// Collecting spots; the next datagram goes at `next_send`.
    Waiting,
    /// The last datagram could not be sent; `last_error` says why.
    Error,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReporterStatus {
    pub state: ReporterState,
    /// Spots sent since the server started, and the datagrams they went in.
    pub spots_sent: u64,
    pub datagrams: u64,
    /// Spots waiting for the next datagram.
    pub queued: usize,
    /// Distinct stations reported in the last hour.
    pub heard_hour: usize,
    /// Unix seconds of the last datagram sent and of the next one due.
    pub last_sent: Option<u64>,
    pub next_send: Option<u64>,
    pub last_error: Option<String>,
}

/// A callsign as reports carry it: 3 to 12 characters of letters, digits
/// and `/`, with at least one of each of letter and digit, upper case.
pub fn normalize_callsign(s: &str) -> Result<String, &'static str> {
    let c = s.trim().to_ascii_uppercase();
    if c.is_empty() {
        return Err("no callsign");
    }
    if !(3..=12).contains(&c.len()) {
        return Err("a callsign is 3 to 12 characters");
    }
    if !c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '/') {
        return Err("letters, digits and / only");
    }
    if c.starts_with('/') || c.ends_with('/') || c.contains("//") {
        return Err("a / goes between parts");
    }
    if !c.chars().any(|ch| ch.is_ascii_digit()) || !c.chars().any(|ch| ch.is_ascii_alphabetic()) {
        return Err("a callsign has letters and a digit");
    }
    Ok(c)
}

/// A Maidenhead locator written the usual way: field letters upper case,
/// subsquare letters lower case (`FM19`, `FM19la`, `FM19la52`).
pub fn normalize_grid(s: &str) -> Result<String, &'static str> {
    let g: Vec<char> = s.trim().chars().collect();
    if g.is_empty() {
        return Err("no locator");
    }
    if !matches!(g.len(), 4 | 6 | 8) {
        return Err("a locator is 4, 6 or 8 characters");
    }
    let mut out = String::with_capacity(g.len());
    for (i, ch) in g.iter().enumerate() {
        let ok = match i {
            0 | 1 => ch.to_ascii_uppercase().is_ascii_uppercase() && ('A'..='R').contains(&ch.to_ascii_uppercase()),
            2 | 3 | 6 | 7 => ch.is_ascii_digit(),
            _ => ('a'..='x').contains(&ch.to_ascii_lowercase()),
        };
        if !ok {
            return Err(match i {
                0 | 1 => "the field is two letters A to R",
                4 | 5 => "the subsquare is two letters A to X",
                _ => "the square is two digits",
            });
        }
        out.push(if i < 2 { ch.to_ascii_uppercase() } else { ch.to_ascii_lowercase() });
    }
    Ok(out)
}

/// The center of a locator's square, as (latitude, longitude) in degrees.
pub fn grid_center(grid: &str) -> Option<(f64, f64)> {
    let g: Vec<u8> = normalize_grid(grid).ok()?.into_bytes();
    let mut lon = -180.0 + f64::from(g[0] - b'A') * 20.0 + f64::from(g[2] - b'0') * 2.0;
    let mut lat = -90.0 + f64::from(g[1] - b'A') * 10.0 + f64::from(g[3] - b'0');
    let (mut w, mut h) = (2.0, 1.0);
    if g.len() >= 6 {
        w /= 24.0;
        h /= 24.0;
        lon += f64::from(g[4] - b'a') * w;
        lat += f64::from(g[5] - b'a') * h;
    }
    if g.len() == 8 {
        w /= 10.0;
        h /= 10.0;
        lon += f64::from(g[6] - b'0') * w;
        lat += f64::from(g[7] - b'0') * h;
    }
    Some((lat + h / 2.0, lon + w / 2.0))
}

/// The great-circle path between two points (latitude, longitude in
/// degrees): its length in kilometers and the initial bearing from the
/// first, degrees clockwise from true north.
pub fn path(from: (f64, f64), to: (f64, f64)) -> (f64, f64) {
    let (p1, l1) = (from.0.to_radians(), from.1.to_radians());
    let (p2, l2) = (to.0.to_radians(), to.1.to_radians());
    let dl = l2 - l1;
    let a = ((p2 - p1) / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    let km = 2.0 * 6_371.0 * a.sqrt().min(1.0).asin();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    (km, y.atan2(x).to_degrees().rem_euclid(360.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callsigns() {
        for (inp, out) in [("k1abc", "K1ABC"), (" VE3/W1AW ", "VE3/W1AW"), ("W1AW/P", "W1AW/P"), ("2E0XYZ", "2E0XYZ")] {
            assert_eq!(normalize_callsign(inp).as_deref(), Ok(out), "{inp}");
        }
        for bad in ["", "K1", "ABCDEF", "123456", "/K1ABC", "K1ABC/", "K1 ABC", "K1ABC-9", "VERYLONGCALL1"] {
            assert!(normalize_callsign(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn grids() {
        for (inp, out) in [("fm19", "FM19"), ("FM19LA", "FM19la"), ("fm19la52", "FM19la52"), ("RR99xx", "RR99xx")] {
            assert_eq!(normalize_grid(inp).as_deref(), Ok(out), "{inp}");
        }
        for bad in ["", "FM1", "FM19L", "SS00", "FM1A", "FM19ya", "FM19la5", "RR73x"] {
            assert!(normalize_grid(bad).is_err(), "{bad}");
        }
        // FN31pr is Newington, Connecticut: 41.7 N, 72.7 W.
        let (lat, lon) = grid_center("FN31pr").unwrap();
        assert!((lat - 41.729).abs() < 0.03 && (lon + 72.708).abs() < 0.05, "{lat} {lon}");
        let (lat, lon) = grid_center("JJ00").unwrap();
        assert_eq!((lat, lon), (0.5, 1.0));
    }

    #[test]
    fn paths() {
        // Connecticut to London: about 5,400 km, setting out to the
        // northeast.
        let (km, az) = path(grid_center("FN31pr").unwrap(), grid_center("IO91wm").unwrap());
        assert!((km - 5_430.0).abs() < 60.0, "{km}");
        assert!((az - 51.0).abs() < 3.0, "{az}");
        let (km, _) = path((10.0, 20.0), (10.0, 20.0));
        assert!(km.abs() < 1e-6);
    }
}
