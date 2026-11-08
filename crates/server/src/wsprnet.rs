//! WSPR spots uploaded to WSPRnet, as the operator's station.
//!
//! Each decode goes as one form post to `wsprnet.org/post/`, the interface
//! WSJT-X uses (`function=wspr`, mode 2 for two-minute WSPR), over plain
//! HTTP as WSJT-X does. The spots wait in a queue while WSPRnet cannot be
//! reached and go when it can again, oldest first. Reporting shares the
//! station's switch, callsign and locator with PSKReporter, and nothing is
//! sent while either is missing.

use crate::pskreporter::{Reporter, unbracket};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::Receiver;
use usdr_proto::{Station, normalize_callsign, normalize_grid};

const HOST: &str = "wsprnet.org";
const PATH: &str = "/post/";
const TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait after a failed upload before trying again.
const RETRY: Duration = Duration::from_secs(60);
/// Spots older than this are dropped rather than uploaded late.
const KEEP_S: u64 = 6 * 3600;

/// One WSPR decode, as WSPRnet takes it.
#[derive(Clone, Debug, PartialEq)]
pub struct WsprSpot {
    pub call: String,
    pub grid: Option<String>,
    /// The dial, and the signal's own frequency (dial plus audio offset).
    pub dial_hz: f64,
    pub freq_hz: f64,
    pub snr_db: i32,
    pub dt_s: f32,
    pub drift_hz: i32,
    pub dbm: i32,
    /// Unix seconds of the slot's start.
    pub slot: u64,
}

/// The spot a WSPR decode makes, when its sender is known.
pub fn spot_of(dial_hz: f64, ev: &scannerd_engine::DecodeEvent) -> Option<WsprSpot> {
    if ev.protocol != "WSPR" || !ev.valid {
        return None;
    }
    let f = |k: &str| ev.fields.get(k);
    let call = normalize_callsign(unbracket(f("call")?)?).ok()?;
    let df: f64 = f("df")?.parse().ok()?;
    if !(0.0..5_000.0).contains(&df) || !(100_000.0..4.0e9).contains(&dial_hz) {
        return None;
    }
    Some(WsprSpot {
        call,
        grid: f("grid").and_then(|g| normalize_grid(g).ok()),
        dial_hz,
        freq_hz: dial_hz + df,
        snr_db: f("snr")?.parse::<f32>().ok()?.round() as i32,
        dt_s: f("dt").and_then(|v| v.parse().ok()).unwrap_or(0.0),
        drift_hz: f("drift").and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.0).round() as i32,
        dbm: f("dbm")?.parse().ok()?,
        slot: f("slotMs")?.parse::<u64>().ok()? / 1000,
    })
}

/// Year, month and day of a day count since 1970-01-01 (proleptic
/// Gregorian), after Howard Hinnant's civil_from_days.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The form WSJT-X posts for one spot.
pub fn form(station: &Station, s: &WsprSpot) -> String {
    let (y, m, d) = civil((s.slot / 86_400) as i64);
    let secs = s.slot % 86_400;
    let pairs: [(&str, String); 15] = [
        ("function", "wspr".into()),
        ("rcall", station.callsign.clone()),
        ("rgrid", station.grid.clone()),
        ("rqrg", format!("{:.6}", s.dial_hz / 1e6)),
        ("date", format!("{:02}{m:02}{d:02}", y % 100)),
        ("time", format!("{:02}{:02}", secs / 3600, (secs / 60) % 60)),
        ("sig", s.snr_db.to_string()),
        ("dt", format!("{:.1}", s.dt_s)),
        ("drift", s.drift_hz.to_string()),
        ("tqrg", format!("{:.6}", s.freq_hz / 1e6)),
        ("tcall", s.call.clone()),
        ("tgrid", s.grid.clone().unwrap_or_default()),
        ("dbm", s.dbm.to_string()),
        ("version", format!("uSDR {}", env!("CARGO_PKG_VERSION"))),
        ("mode", "2".into()),
    ];
    pairs.iter().map(|(k, v)| format!("{k}={}", urlencode(v))).collect::<Vec<_>>().join("&")
}

/// Post one form; the body of WSPRnet's answer when it says 200.
async fn post(body: &str) -> Result<String, String> {
    let exchange = async {
        let mut sock = tokio::net::TcpStream::connect((HOST, 80)).await.map_err(|e| format!("connecting to {HOST}: {e}"))?;
        let req = format!(
            "POST {PATH} HTTP/1.1\r\nHost: {HOST}\r\nUser-Agent: uSDR/{}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            env!("CARGO_PKG_VERSION"),
            body.len()
        );
        sock.write_all(req.as_bytes()).await.map_err(|e| format!("sending to {HOST}: {e}"))?;
        let mut resp = Vec::new();
        sock.read_to_end(&mut resp).await.map_err(|e| format!("reading from {HOST}: {e}"))?;
        let resp = String::from_utf8_lossy(&resp).into_owned();
        let status = resp.split_whitespace().nth(1).unwrap_or("");
        if status != "200" {
            return Err(format!("{HOST} answered {}", resp.lines().next().unwrap_or("nothing")));
        }
        Ok(resp.split_once("\r\n\r\n").map_or(String::new(), |(_, b)| b.trim().to_string()))
    };
    tokio::time::timeout(TIMEOUT, exchange).await.map_err(|_| format!("{HOST} did not answer in {}s", TIMEOUT.as_secs()))?
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Upload the spots as they come, forever.
pub async fn run(reporter: Arc<Reporter>, mut spots: Receiver<WsprSpot>) {
    let mut queue: VecDeque<WsprSpot> = VecDeque::new();
    let mut first = true;
    loop {
        if queue.is_empty() {
            match spots.recv().await {
                Some(s) => queue.push_back(s),
                None => return,
            }
        }
        while let Ok(s) = spots.try_recv() {
            queue.push_back(s);
        }
        let now = unix_now();
        queue.retain(|s| now.saturating_sub(s.slot) < KEEP_S);
        let station = reporter.station();
        let Some(spot) = queue.front().cloned() else { continue };
        if !reporter.reports(&spot.call) {
            // Turned off, or the station changed to one that is not ready:
            // what was heard before is not sent under it.
            queue.clear();
            continue;
        }
        match post(&form(&station, &spot)).await {
            Ok(body) => {
                if first {
                    eprintln!("wsprnet: first spot uploaded ({} {}), answer: {}", spot.call, spot.freq_hz / 1e6, body.chars().take(120).collect::<String>());
                    first = false;
                }
                queue.pop_front();
                reporter.wsprnet_result(1, None);
            }
            Err(e) => {
                eprintln!("wsprnet: {e}");
                reporter.wsprnet_result(0, Some(e));
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wspr(call: &str, grid: Option<&str>) -> scannerd_engine::DecodeEvent {
        let mut fields = std::collections::BTreeMap::new();
        for (k, v) in [("call", call), ("df", "1496"), ("snr", "-21"), ("dt", "0.4"), ("drift", "-1.0"), ("dbm", "37"), ("slotMs", "1762524120000")] {
            fields.insert(k.into(), v.into());
        }
        if let Some(g) = grid {
            fields.insert("grid".into(), g.into());
        }
        scannerd_engine::DecodeEvent { protocol: "WSPR".into(), kind: "type1".into(), summary: String::new(), valid: true, fields }
    }

    #[test]
    fn the_form_is_what_wsjtx_posts() {
        let st = Station { callsign: "N1DQ".into(), grid: "FN42hn".into(), antenna: String::new(), report_spots: true };
        let s = spot_of(14_095_600.0, &wspr("K1ABC", Some("FN42"))).unwrap();
        // 1762524120 is 2025-11-07 14:02:00 UTC.
        assert_eq!(
            form(&st, &s),
            "function=wspr&rcall=N1DQ&rgrid=FN42hn&rqrg=14.095600&date=251107&time=1402&sig=-21&dt=0.4&drift=-1&tqrg=14.097096&tcall=K1ABC&tgrid=FN42&dbm=37&version=uSDR%20"
                .to_string()
                + env!("CARGO_PKG_VERSION")
                + "&mode=2"
        );
    }

    #[test]
    fn senders_must_be_known() {
        assert!(spot_of(14_095_600.0, &wspr("<...>", None)).is_none(), "an unresolved hash");
        assert_eq!(spot_of(14_095_600.0, &wspr("<PJ4/K1ABC>", Some("FK52ud"))).unwrap().call, "PJ4/K1ABC");
        assert_eq!(spot_of(14_095_600.0, &wspr("K1ABC", Some("XX99"))).unwrap().grid, None);
    }

    #[test]
    fn calendar() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(11_016), (2000, 2, 29));
        assert_eq!(civil(20_399), (2025, 11, 7));
    }
}
