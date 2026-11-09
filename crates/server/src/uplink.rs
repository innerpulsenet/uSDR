//! The station log, sent on to the public site's backend.
//!
//! It runs only when `uplink.toml` sits beside the settings, and that file holds
//! the address and the token. Each batch goes once a minute, gzipped, and the
//! cursors move only when the backend has said yes, so an outage just means a
//! bigger batch afterward. Row ids make a repeat harmless.
//!
//! Nothing identifies the station: a batch carries a generic id and a
//! 4-character grid. Rows are read through a column list that leaves out
//! `private_json`, and the mode and frequency limits here are the first of two;
//! the backend applies them again.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::stationlog::StationLog;

const EVERY: Duration = Duration::from_secs(60);
const LONGEST_WAIT: Duration = Duration::from_secs(15 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
/// Rows of each kind in one batch. A long outage catches up over several.
pub const BATCH_ROWS: u32 = 4_000;
const STATION_ID: &str = "hf1";

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// For example `https://example.net/api/ingest`.
    pub url: String,
    pub token: String,
}

/// The configuration, or `None` when this daemon does not upload.
pub fn load(path: &Path) -> Result<Option<Config>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let config: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if !config.url.starts_with("https://") || config.token.len() < 32 {
        bail!("{} needs an https url and a token of at least 32 characters", path.display());
    }
    Ok(Some(config))
}

/// The first four characters of a locator, the most this station says of where
/// it is. Anything that is not a real square says nothing.
pub fn public_grid(grid: &str) -> Option<String> {
    let g = grid.trim().as_bytes();
    let real = g.len() >= 4 && (b'A'..=b'R').contains(&g[0].to_ascii_uppercase()) && (b'A'..=b'R').contains(&g[1].to_ascii_uppercase()) && g[2].is_ascii_digit() && g[3].is_ascii_digit();
    real.then(|| grid.trim()[..4].to_ascii_uppercase())
}

fn gzip(json: &Value) -> Result<Vec<u8>> {
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(json.to_string().as_bytes())?;
    Ok(gz.finish()?)
}

/// One round: the rows, then one picture. `post` delivers a body to a path
/// under the ingest address, with its content type and any extra headers, and
/// returns an error for anything but success. True when there is more to send.
pub fn send_once(log: &StationLog, grid: Option<&str>, now: u64, post: &mut dyn FnMut(&str, &str, &BTreeMap<&str, String>, Vec<u8>) -> Result<()>) -> Result<bool> {
    let batch = log.uplink_batch(now, BATCH_ROWS)?;
    let full = batch.full;
    if !batch.is_empty() || batch.moved() {
        let body = json!({
            "station": STATION_ID,
            "grid": grid,
            "sent_utc": now,
            "observations": batch.observations,
            "listening": batch.listening,
            "gaps": batch.gaps,
        });
        let mut headers = BTreeMap::new();
        headers.insert("content-encoding", "gzip".to_string());
        post("", "application/json", &headers, gzip(&body)?).context("sending rows")?;
        log.uplink_advance(batch.cursors);
    }
    let mut more = full;
    if let Some(picture) = log.next_picture()? {
        let png = std::fs::read(&picture.path).with_context(|| format!("reading {}", picture.path.display()))?;
        let mut headers = BTreeMap::new();
        headers.insert("x-hf-picture", picture.meta.to_string());
        headers.insert("x-hf-station", STATION_ID.to_string());
        post("/picture", "image/png", &headers, png).context("sending a picture")?;
        log.picture_uploaded(picture.id, now);
        more = true;
    }
    Ok(more)
}

fn deliver(agent: &ureq::Agent, config: &Config, path: &str, content_type: &str, headers: &BTreeMap<&str, String>, body: Vec<u8>) -> Result<()> {
    let url = format!("{}{path}", config.url);
    let mut request = agent.post(&url).header("authorization", format!("Bearer {}", config.token)).header("content-type", content_type);
    for (name, value) in headers {
        request = request.header(*name, value.as_str());
    }
    request.send(&body[..]).map(drop).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Send until the daemon stops, slowing down while the backend is unreachable.
pub async fn run(log: Arc<StationLog>, config: Config, grid: impl Fn() -> String + Send + Sync + 'static) {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(HTTP_TIMEOUT)).build().into();
    let grid = Arc::new(grid);
    let mut wait = EVERY;
    loop {
        tokio::time::sleep(wait).await;
        let (log, config, agent, grid) = (Arc::clone(&log), config.clone(), agent.clone(), Arc::clone(&grid));
        let round = tokio::task::spawn_blocking(move || {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let grid = public_grid(&grid());
            send_once(&log, grid.as_deref(), now, &mut |path, kind, headers, body| deliver(&agent, &config, path, kind, headers, body))
        })
        .await;
        wait = match round {
            Ok(Ok(true)) => Duration::from_secs(2),
            Ok(Ok(false)) => EVERY,
            Ok(Err(e)) => {
                eprintln!("uplink: {e:#}");
                (wait * 2).clamp(EVERY, LONGEST_WAIT)
            }
            Err(e) => {
                eprintln!("uplink: {e}");
                LONGEST_WAIT
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grid_goes_out_at_four_characters_at_most() {
        assert_eq!(public_grid("fn20xk").as_deref(), Some("FN20"));
        assert_eq!(public_grid(" FN20 ").as_deref(), Some("FN20"));
        assert_eq!(public_grid("FN2"), None);
        assert_eq!(public_grid("ZZ99"), None);
        assert_eq!(public_grid(""), None);
    }

    #[test]
    fn a_missing_file_means_off_and_a_weak_one_is_refused() {
        let dir = std::env::temp_dir().join(format!("usdr-uplink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("uplink.toml");
        let _ = std::fs::remove_file(&path);
        assert!(load(&path).unwrap().is_none());
        std::fs::write(&path, "url = \"http://example.net\"\ntoken = \"0123456789012345678901234567890123\"\n").unwrap();
        assert!(load(&path).is_err(), "not over plain HTTP");
        std::fs::write(&path, "url = \"https://example.net\"\ntoken = \"short\"\n").unwrap();
        assert!(load(&path).is_err(), "not with a short token");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
